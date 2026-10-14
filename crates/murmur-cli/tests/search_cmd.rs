#[path = "common/mod.rs"]
mod common;

use assert_cmd::Command;
use murmur_artifact::{ArtifactMeta, LocalRegistry, Registry, RuntimeType};
use predicates::prelude::*;
use tempfile::TempDir;

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Write a minimal valid artifacts-index.json to a temp file.
/// Returns the path as a String for use with --index.
fn write_index(dir: &TempDir, entries: &[serde_json::Value]) -> String {
    let index = serde_json::json!({
        "schema_version": "1",
        "updated_at": "2026-06-07T00:00:00Z",
        "artifacts": entries
    });
    let path = dir.path().join("artifacts-index.json");
    std::fs::write(&path, serde_json::to_string_pretty(&index).unwrap()).unwrap();
    path.to_str().unwrap().to_string()
}

fn index_json(entries: &[serde_json::Value]) -> String {
    serde_json::json!({
        "schema_version": "1",
        "updated_at": "2026-06-07T00:00:00Z",
        "artifacts": entries
    })
    .to_string()
}

fn tool_entry(name: &str) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "version": "1.0.0",
        "runtime": "tool",
        "description": format!("The {} tool.", name),
        "tags": [name],
        "platforms": ["darwin-aarch64"]
    })
}

fn publish_local(home: &TempDir, name: &str) {
    let root = home.path().join(".murmur/artifacts");
    let reg = LocalRegistry::new(root);
    let meta = ArtifactMeta {
        name: name.to_string(),
        version: "1.0.0".to_string(),
        runtime: RuntimeType::Native,
        artifact_runtime: "native".to_string(),
        platforms: vec![("darwin".to_string(), "aarch64".to_string())],
        description: None,
        tags: vec![],
        wit_contracts: None,
    };
    reg.publish(meta, b"fake").unwrap();
}

fn mur_search(home: &TempDir, args: &[&str]) -> assert_cmd::assert::Assert {
    mur_search_in(home, home.path(), args)
}

/// `mur search` run from `cwd`, for an index path given relative to it.
fn mur_search_in(
    home: &TempDir,
    cwd: &std::path::Path,
    args: &[&str],
) -> assert_cmd::assert::Assert {
    let mut cmd = Command::cargo_bin("mur").unwrap();
    cmd.env("HOME", home.path()).current_dir(cwd).arg("search");
    for arg in args {
        cmd.arg(arg);
    }
    cmd.assert()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// Absolute index path: query matches → result row printed.
#[test]
fn search_index_query_matches_returns_row() {
    let dir = TempDir::new().unwrap();
    let idx = write_index(&dir, &[tool_entry("murmur-tool-git")]);
    mur_search(&dir, &["git", "--index", &idx])
        .success()
        .stdout(predicate::str::contains("murmur-tool-git"));
}

/// Absolute index path: query doesn't match → "No results found", exits 0.
#[test]
fn search_index_no_match_prints_no_results() {
    let dir = TempDir::new().unwrap();
    let idx = write_index(&dir, &[tool_entry("murmur-tool-git")]);
    mur_search(&dir, &["zzznomatch", "--index", &idx])
        .success()
        .stdout(predicate::str::contains("No results found"));
}

/// Local scan: populated store → artifact appears in results.
#[test]
fn search_local_populated_returns_artifact() {
    let home = TempDir::new().unwrap();
    publish_local(&home, "murmur-tool-editor");
    mur_search(&home, &["editor", "--index", "local"])
        .success()
        .stdout(predicate::str::contains("murmur-tool-editor"));
}

/// Local scan: empty store → "No results found", exits 0, no network call.
#[test]
fn search_local_empty_returns_no_results() {
    let home = TempDir::new().unwrap();
    mur_search(&home, &["git", "--index", "local"])
        .success()
        .stdout(predicate::str::contains("No results found"));
}

/// Wrong schema_version → exits non-zero with a descriptive error.
#[test]
fn search_wrong_schema_version_fails_gracefully() {
    let dir = TempDir::new().unwrap();
    let bad = serde_json::json!({
        "schema_version": "99",
        "updated_at": "2026-06-07T00:00:00Z",
        "artifacts": []
    });
    let path = dir.path().join("bad.json");
    std::fs::write(&path, serde_json::to_string(&bad).unwrap()).unwrap();
    let path_str = path.to_str().unwrap();
    mur_search(&TempDir::new().unwrap(), &["git", "--index", path_str])
        .failure()
        .stderr(predicate::str::contains("schema version"));
}

/// --limit caps the number of result rows.
#[test]
fn search_limit_caps_results() {
    let dir = TempDir::new().unwrap();
    let idx = write_index(
        &dir,
        &[
            tool_entry("murmur-tool-alpha"),
            tool_entry("murmur-tool-beta"),
            tool_entry("murmur-tool-gamma"),
        ],
    );
    // "murmur" matches all three; limit to 1
    let output = mur_search(&dir, &["murmur", "--index", &idx, "--limit", "1"])
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8_lossy(&output);
    // Header line + 1 result = 2 lines (ignore trailing newline)
    let data_lines: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(
        data_lines.len(),
        2,
        "expected header + 1 result row, got: {stdout}"
    );
}

/// Network failure: unreachable URL → exits non-zero, error names the URL.
#[test]
fn search_network_error_exits_nonzero_with_url_in_message() {
    let home = TempDir::new().unwrap();
    let bad_url = "https://this-does-not-exist.invalid/artifacts-index.json";
    mur_search(&home, &["git", "--index", bad_url])
        .failure()
        .stderr(predicate::str::contains(bad_url));
}

/// A relative index path is read from disk against the working directory, never fetched.
#[test]
fn search_relative_index_path_is_read_from_disk() {
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    write_index(&dir, &[tool_entry("murmur-tool-git")]);
    for relative in ["./artifacts-index.json", "artifacts-index.json"] {
        mur_search_in(&home, dir.path(), &["git", "--index", relative])
            .success()
            .stdout(predicate::str::contains("murmur-tool-git"));
    }
}

/// A missing index file fails with E-IO-003 naming the path.
#[test]
fn search_missing_index_file_names_the_path() {
    let home = TempDir::new().unwrap();
    mur_search(&home, &["git", "--index", "./no-such-index.json"])
        .failure()
        .stderr(predicate::str::contains("E-IO-003"))
        .stderr(predicate::str::contains("./no-such-index.json"));
}

/// An http:// index is fetched.
#[test]
fn search_http_index_is_fetched() {
    let home = TempDir::new().unwrap();
    let server = common::ScriptedServer::start(vec![index_json(&[tool_entry("murmur-tool-git")])]);
    let url = format!("{}/artifacts-index.json", server.endpoint);
    mur_search(&home, &["git", "--index", &url])
        .success()
        .stdout(predicate::str::contains("murmur-tool-git"));
    assert_eq!(server.requests().len(), 1);
}

/// With no --index, `registry.index_url` in ~/.murmur/config.yaml is fetched.
#[test]
fn search_without_index_fetches_the_configured_index_url() {
    let home = TempDir::new().unwrap();
    let server =
        common::ScriptedServer::start(vec![index_json(&[tool_entry("murmur-tool-editor")])]);
    let config_dir = home.path().join(".murmur");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.yaml"),
        format!(
            "registry:\n  index_url: {}/artifacts-index.json\n",
            server.endpoint
        ),
    )
    .unwrap();
    mur_search(&home, &["editor"])
        .success()
        .stdout(predicate::str::contains("murmur-tool-editor"));
    assert_eq!(server.requests().len(), 1);
}
