use std::{fs, path::Path, sync::LazyLock};

use murmur_artifact::{ArtifactRuntime, PACKED_MANIFEST_ENTRY};
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
    /// session holds for its whole life is reproducible, not a snapshot of directory order.
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
