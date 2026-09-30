use std::{fs, path::Path, sync::LazyLock};

use murmur_artifact::{ArtifactRuntime, ToolRefresh, PACKED_MANIFEST_ENTRY};
use serde_json::{json, Value};

const DEFAULT_INPUT_SCHEMA: &str = r#"{"type":"object","properties":{}}"#;

static DEFAULT_SCHEMA: LazyLock<Value> =
    LazyLock::new(|| serde_json::from_str(DEFAULT_INPUT_SCHEMA).unwrap_or_else(|_| json!({})));

/// Build the tool inventory sent to the model each turn.
///
/// `system_prompt_artifact`: when set, the skill with this name is excluded from the inventory
/// because it is already injected as the system prompt — listing it as a callable tool would
/// cause double-injection and waste context.
pub(crate) fn build_tool_inventory(
    workdir: &Path,
    system_prompt_artifact: Option<&str>,
) -> Vec<Value> {
    let tools_dir = workdir.join("tools");
    let mut tools = Vec::new();

    let entries = match fs::read_dir(&tools_dir) {
        Ok(e) => e,
        Err(_) => return tools,
    };

    let mut entries: Vec<_> = entries.flatten().collect();
    // Sorted, not in `read_dir` order, because of prompt caching: the serialized tool array is
    // part of the prefix every provider matches its cache on, so a reordered array changes the
    // prefix and invalidates the cache entry for the whole session. Directory order is
    // filesystem-dependent and unstable across launches; the sort is what makes the same tool
    // set render the same bytes twice.
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };

        let manifest_path = path.join(PACKED_MANIFEST_ENTRY);
        let manifest_content = match fs::read_to_string(&manifest_path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let value: serde_yaml::Value = match serde_yaml::from_str(&manifest_content) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Skip artifacts that are not LLM-visible (e.g. runtime: driver, hook).
        let runtime_str = value
            .get("runtime")
            .and_then(|v| v.as_str())
            .unwrap_or("wasm");
        let runtime = match runtime_str {
            "driver" => ArtifactRuntime::Driver,
            "hook" => ArtifactRuntime::Hook,
            "skill" => ArtifactRuntime::Skill,
            _ => ArtifactRuntime::Tool, // covers "tool", legacy "wasm"/"native"
        };
        if !runtime.is_llm_visible() {
            continue;
        }

        // Skip the skill that is already bound as the system prompt.
        if matches!(runtime, ArtifactRuntime::Skill)
            && system_prompt_artifact == Some(name.as_str())
        {
            continue;
        }

        // Description: manifest field, or fall back to the first non-empty line of skill.md.
        let description = {
            let from_manifest = value
                .get("description")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);

            from_manifest.unwrap_or_else(|| {
                if matches!(runtime, ArtifactRuntime::Skill) {
                    let skill_path = path.join("skill.md");
                    fs::read_to_string(&skill_path)
                        .unwrap_or_default()
                        .lines()
                        .find(|l| !l.trim().is_empty())
                        .map(str::trim)
                        .unwrap_or_default()
                        .to_string()
                } else {
                    String::new()
                }
            })
        };

        // Skills take no input payload — use empty schema. Tools keep their declared schema.
        let parameters = if matches!(runtime, ArtifactRuntime::Skill) {
            DEFAULT_SCHEMA.clone()
        } else {
            crate::tool_annotations::declared_input_schema(&value)
                .unwrap_or_else(|| DEFAULT_SCHEMA.clone())
        };

        let mut tool = json!({
            "name": name,
            "parameters": parameters,
        });

        if !description.trim().is_empty() {
            tool["description"] = Value::String(description);
        }

        tools.push(tool);
    }

    tools
}

/// The tool array the http agent loop sends the model, and the session's installed generation
/// it was built at.
///
/// The array is the head of the prefix every provider matches its prompt cache on, so it is
/// held byte-for-byte across calls and rebuilt only at the boundary `inference.tool_refresh`
/// names, once `CapsuleStoreState::installed_generation` has moved past the held one.
pub(crate) struct HeldInventory {
    tools: Vec<Value>,
    generation: u64,
}

/// Why a held tool array was rebuilt before an inference call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefreshReason {
    /// `inference.tool_refresh: immediate`, on a call no compaction had already made a full
    /// cache miss: this call pays the miss for the refresh alone.
    Immediate,
    /// A compaction committed a replacement context since the previous call, whatever the
    /// declared trigger: the message history was already a full miss.
    Compaction,
}

impl RefreshReason {
    /// The value `tools_refreshed.trigger` carries.
    pub(crate) fn wire_name(self) -> &'static str {
        match self {
            Self::Immediate => ToolRefresh::Immediate.wire_name(),
            Self::Compaction => ToolRefresh::Compaction.wire_name(),
        }
    }
}

/// A rebuild that changed the bytes the next call sends. The new array is
/// [`HeldInventory::tools`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InventoryRefresh {
    pub(crate) reason: RefreshReason,
    /// Names offered now and not before, sorted.
    pub(crate) added: Vec<String>,
    /// Names offered before and not now, sorted.
    pub(crate) removed: Vec<String>,
    /// Every name the rebuilt array offers, in array order, which is sorted.
    pub(crate) offered: Vec<String>,
}

impl HeldInventory {
    /// The launch build: [`build_tool_inventory`] over `workdir`, held as of `generation`.
    pub(crate) fn build(
        workdir: &Path,
        system_prompt_artifact: Option<&str>,
        generation: u64,
    ) -> Self {
        Self {
            tools: build_tool_inventory(workdir, system_prompt_artifact),
            generation,
        }
    }

    /// The array the next inference call sends.
    pub(crate) fn tools(&self) -> &[Value] {
        &self.tools
    }

    /// The installed generation [`Self::tools`] reflects.
    #[cfg(test)]
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    /// The pre-call decision: rebuild the array when `current_generation` has moved past the
    /// held one and either `trigger` is [`ToolRefresh::Immediate`] or `compaction_committed`
    /// says a compaction replaced the context since the previous call.
    ///
    /// An unchanged generation reads nothing from disk. A change the trigger does not release
    /// stays pending: the held generation is not advanced, so the next eligible call still sees
    /// it. A rebuild that serializes to the held bytes advances the held generation and returns
    /// `None`, because the provider sees no change.
    pub(crate) fn refresh_before_call(
        &mut self,
        workdir: &Path,
        system_prompt_artifact: Option<&str>,
        trigger: ToolRefresh,
        current_generation: u64,
        compaction_committed: bool,
    ) -> Option<InventoryRefresh> {
        if current_generation == self.generation {
            return None;
        }
        let reason = if compaction_committed {
            RefreshReason::Compaction
        } else if trigger == ToolRefresh::Immediate {
            RefreshReason::Immediate
        } else {
            return None;
        };

        let rebuilt = build_tool_inventory(workdir, system_prompt_artifact);
        self.generation = current_generation;
        if serialized(&rebuilt) == serialized(&self.tools) {
            return None;
        }

        let before = tool_names(&self.tools);
        let offered = tool_names(&rebuilt);
        let mut added: Vec<String> = offered
            .iter()
            .filter(|name| !before.contains(name))
            .cloned()
            .collect();
        let mut removed: Vec<String> = before
            .iter()
            .filter(|name| !offered.contains(name))
            .cloned()
            .collect();
        added.sort();
        removed.sort();
        self.tools = rebuilt;
        Some(InventoryRefresh {
            reason,
            added,
            removed,
            offered,
        })
    }
}

/// The exact bytes the array contributes to a payload's `tools` member.
fn serialized(tools: &[Value]) -> String {
    serde_json::to_string(tools).unwrap_or_default()
}

fn tool_names(tools: &[Value]) -> Vec<String> {
    tools
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_tool(tools_dir: &Path, name: &str) {
        let dir = tools_dir.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(PACKED_MANIFEST_ENTRY),
            format!("name: {name}\nversion: 1.0.0\nruntime: tool\n"),
        )
        .unwrap();
    }

    /// Tool order is sorted regardless of the order the directories were created in, because
    /// the serialized tool array is part of the cached prompt prefix — see the sort's comment.
    #[test]
    fn tool_inventory_is_sorted_regardless_of_creation_order() {
        let workdir = tempfile::tempdir().unwrap();
        let tools_dir = workdir.path().join("tools");
        for name in ["zeta", "alpha", "mid"] {
            write_tool(&tools_dir, name);
        }

        let names: Vec<String> = build_tool_inventory(workdir.path(), None)
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();

        assert_eq!(names, vec!["alpha", "mid", "zeta"]);
    }

    /// Two builds over the same unchanged workdir produce byte-identical JSON: the array a
    /// session holds between refreshes is reproducible, not a snapshot of directory order.
    #[test]
    fn tool_inventory_is_stable_across_repeated_builds() {
        let workdir = tempfile::tempdir().unwrap();
        let tools_dir = workdir.path().join("tools");
        for name in ["zeta", "alpha", "mid"] {
            write_tool(&tools_dir, name);
        }

        let first = serde_json::to_string(&build_tool_inventory(workdir.path(), None)).unwrap();
        let second = serde_json::to_string(&build_tool_inventory(workdir.path(), None)).unwrap();

        assert_eq!(first, second);
    }
}
