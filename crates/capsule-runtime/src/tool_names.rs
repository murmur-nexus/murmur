//! The tool name of an artifact inside one staged session.
//!
//! A capsule declares artifacts by full name, `<scope>/<name>`, and a model is offered each by its
//! tool name, the bare part: the tool it calls, its `<runtime dir>/tools/<name>/` directory and
//! every key the session dispatches on. Staging refuses two artifacts that would share a tool name,
//! then renames the request once ([`localize_artifact_names`]) so the rest of the session works in
//! tool names alone. What leaves the session — the store, the lock, trace identity, `manage.*` and
//! roost requests — goes back to the full name through the map that rename returns.

use std::collections::HashMap;

use murmur_artifact::tool_name_of;

use crate::errors::RuntimeError;
use crate::types::StageRequest;

/// Refuses the first two declared names that share a tool name.
///
/// `names` are the names as declared: full names, and the bare names of `source:` entries. Called
/// from `stage_session` ahead of every resolve, and from `mur run`'s pre-flight beside
/// [`crate::runtime::check_no_reserved_tool_names`].
pub fn check_unique_tool_names<'a, I>(names: I) -> Result<(), RuntimeError>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut seen: HashMap<&str, &str> = HashMap::new();
    for name in names {
        let tool = tool_name_of(name);
        if let Some(first) = seen.insert(tool, name) {
            return Err(RuntimeError::DuplicateToolName {
                first: first.to_string(),
                second: name.to_string(),
                tool: tool.to_string(),
            });
        }
    }
    Ok(())
}

/// Rename every artifact reference in `request` to its tool name — the artifact entries, the tool
/// allowlist, and the inference block's driver, alternate drivers and system-prompt artifact — and
/// return each tool name's full name. Run after [`check_unique_tool_names`], which makes the map
/// one-to-one.
pub(crate) fn localize_artifact_names(request: &mut StageRequest) -> HashMap<String, String> {
    let mut full_names = HashMap::with_capacity(request.artifacts.len());
    for artifact in &mut request.artifacts {
        let tool = tool_name_of(&artifact.name).to_string();
        full_names.insert(tool.clone(), std::mem::replace(&mut artifact.name, tool));
    }
    request.allowlisted_tools = request
        .allowlisted_tools
        .iter()
        .map(|name| tool_name_of(name).to_string())
        .collect();
    if let Some(inference) = request.inference.as_mut() {
        if let Some(driver) = inference.driver.as_mut() {
            driver.artifact = tool_name_of(&driver.artifact).to_string();
        }
        for alternate in &mut inference.alternates {
            alternate.driver = tool_name_of(&alternate.driver).to_string();
        }
        if let Some(artifact) = inference.system_prompt_artifact.as_mut() {
            *artifact = tool_name_of(artifact).to_string();
        }
    }
    full_names
}

/// The `capabilities.spawn.allow` entry a `delegate-task` call or a plan's `capsule` step names by
/// its bare part. A name no entry has is returned as given, so mur-roost's referee refuses it in
/// its own words; the manifest parser keeps the bare parts of the entries unique.
pub fn spawn_target(spawn_allow: &[String], capsule: &str) -> String {
    spawn_allow
        .iter()
        .find(|entry| tool_name_of(entry) == capsule)
        .cloned()
        .unwrap_or_else(|| capsule.to_string())
}

/// The full name of the artifact a session knows as `tool`, or `tool` itself for one with no full
/// name: a `source:` skill, or a runtime-provided tool.
pub(crate) fn full_name_of<'a>(full_names: &'a HashMap<String, String>, tool: &'a str) -> &'a str {
    full_names.get(tool).map_or(tool, String::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_tool_names_refuses_two_scopes_with_one_name() {
        let error = check_unique_tool_names(["murmur/tool-editor", "ana/tool-editor"]).unwrap_err();
        let RuntimeError::DuplicateToolName {
            first,
            second,
            tool,
        } = &error
        else {
            panic!("expected DuplicateToolName, got {error:?}");
        };
        assert_eq!(first, "murmur/tool-editor");
        assert_eq!(second, "ana/tool-editor");
        assert_eq!(tool, "tool-editor");
        let message = error.to_string();
        for part in ["murmur/tool-editor", "ana/tool-editor", "'tool-editor'"] {
            assert!(message.contains(part), "{message}");
        }
    }

    #[test]
    fn unique_tool_names_refuses_a_source_skill_beside_a_resolved_artifact() {
        let error = check_unique_tool_names(["tool-editor", "murmur/tool-editor"]).unwrap_err();
        assert!(matches!(error, RuntimeError::DuplicateToolName { .. }));
    }

    #[test]
    fn delegate_task_spawn_target_maps_a_bare_name_to_its_entry() {
        let allow = vec!["local/worker".to_string(), "ana/reviewer".to_string()];
        assert_eq!(spawn_target(&allow, "worker"), "local/worker");
        assert_eq!(spawn_target(&allow, "reviewer"), "ana/reviewer");
        assert_eq!(spawn_target(&allow, "stranger"), "stranger");
    }

    #[test]
    fn unique_tool_names_accepts_distinct_tool_names() {
        check_unique_tool_names(["murmur/tool-editor", "ana/tool-x", "local-skill"]).unwrap();
    }
}
