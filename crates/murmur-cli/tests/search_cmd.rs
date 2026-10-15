#[path = "common/mod.rs"]
mod common;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use capsule_runtime::SERVED_WIT_PACKAGES;
use murmur_artifact::{
    current_platform, ArtifactMeta, LocalRegistry, Registry, RuntimeType, SUPPORTED_PLATFORMS,
};
use predicates::prelude::*;
use serde_json::{json, Value};
use tempfile::TempDir;
use zip::write::SimpleFileOptions;
use zip::ZipWriter;

// ── Helpers ───────────────────────────────────────────────────────────────────

const SCHEMA_1_HINT: &str = "hint: this is a schema 1 artifact index, which this mur does not \
                             read; its publisher regenerates it with `mur index <dir> --repo \
                             <owner>/<repo> --release <tag>`";

fn served(package: &str) -> &'static str {
    SERVED_WIT_PACKAGES
        .iter()
        .find(|(served_package, _)| *served_package == package)
        .unwrap_or_else(|| panic!("{package} is not served"))
        .1
}

/// A platform other than this host's.
fn other_platform() -> &'static str {
    SUPPORTED_PLATFORMS
        .iter()
        .find(|p| **p != current_platform())
        .unwrap()
}

fn asset(name: &str, version: &str, platform: Option<&str>) -> Value {
    match platform {
        Some(platform) => json!({
            "file": format!("{name}-{version}-{platform}.mur.zip"),
            "platform": platform,
            "sha256": "0".repeat(64),
        }),
        None => json!({ "file": format!("{name}-{version}.mur.zip"), "sha256": "0".repeat(64) }),
    }
}

fn version(name: &str, version: &str, interfaces: &[&str], platforms: &[Option<&str>]) -> Value {
    json!({
        "version": version,
        "release": format!("v{version}"),
        "interfaces": interfaces,
        "assets": platforms.iter().map(|p| asset(name, version, *p)).collect::<Vec<_>>(),
    })
}

fn tool_entry(name: &str) -> Value {
    json!({
        "name": name,
        "runtime": "tool",
        "description": format!("The {name} tool."),
        "tags": ["tool", name],
        "versions": [version(name, "1.0.0", &[], &[None])],
    })
}

fn index_json(entries: &[Value]) -> String {
    let mut entries = entries.to_vec();
    entries.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    serde_json::to_string_pretty(&json!({
        "schema_version": 2,
        "scope": "murmur",
        "artifacts": entries,
    }))
    .unwrap()
}

/// Write a schema 2 `artifacts-index.json` into `dir` and return its path.
fn write_index(dir: &TempDir, entries: &[Value]) -> String {
    let path = dir.path().join("artifacts-index.json");
    fs::write(&path, index_json(entries)).unwrap();
    path.to_str().unwrap().to_string()
}

/// An index exercising the fit rule: a tool whose newest version names an interface version no
/// `mur` serves, a native tool whose newest version has no payload for this host, a hook naming
/// a package this `mur` does not serve, and a skill that fits everywhere.
fn fit_index_entries() -> Vec<Value> {
    let tool_run = format!("murmur:tool/run@{}", served("murmur:tool"));
    vec![
        json!({
            "name": "tool-editor",
            "runtime": "tool",
            "description": "Read, write and patch files in the workdir.",
            "tags": ["tool", "editor"],
            "versions": [
                version("tool-editor", "0.8.0", &["murmur:tool/run@9.9.9"], &[None]),
                version("tool-editor", "0.7.0", &[&tool_run], &[None]),
            ],
        }),
        json!({
            "name": "tool-git",
            "runtime": "tool",
            "description": "Structured git interface.",
            "tags": ["tool", "git"],
            "versions": [
                version("tool-git", "0.3.0", &[], &[Some(other_platform())]),
                version("tool-git", "0.2.0", &[], &[Some(current_platform())]),
            ],
        }),
        json!({
            "name": "hook-old",
            "runtime": "hook",
            "tags": ["hook", "old"],
            "versions": [version("hook-old", "1.0.0", &["murmur:nowhere/x@1.0.0"], &[None])],
        }),
        json!({
            "name": "skill-x",
            "runtime": "skill",
            "description": "A skill.",
            "tags": ["skill", "x"],
            "versions": [version("skill-x", "0.4.0", &[], &[None])],
        }),
    ]
}

fn publish_meta(home: &TempDir, name: &str) {
    let root = home.path().join(".murmur/artifacts");
    let reg = LocalRegistry::new(root);
    let meta = ArtifactMeta {
        name: murmur_artifact::ArtifactName::parse(name).unwrap(),
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

/// Pack `wasm` as a hook named `name` into `dir` and `mur publish` it into `home`'s global store.
fn publish_hook(home: &TempDir, dir: &Path, name: &str, version: &str, wasm: &[u8]) {
    let path = dir.join(format!("{name}-{version}.mur.zip"));
    let mut zip = ZipWriter::new(fs::File::create(&path).unwrap());
    zip.start_file("murmur.yaml", SimpleFileOptions::default())
        .unwrap();
    write!(zip, "name: {name}\nversion: {version}\nruntime: hook\n").unwrap();
    zip.start_file("hook.wasm", SimpleFileOptions::default())
        .unwrap();
    zip.write_all(wasm).unwrap();
    zip.finish().unwrap();
    Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .args(["publish", path.to_str().unwrap()])
        .assert()
        .success();
}

fn sidecar(home: &TempDir, name: &str, version: &str) -> PathBuf {
    home.path().join(format!(
        ".murmur/artifacts/local/{name}/{version}/{name}-{version}.meta.json"
    ))
}

fn mur_search(home: &TempDir, args: &[&str]) -> assert_cmd::assert::Assert {
    mur_search_in(home, home.path(), args)
}

/// `mur search` run from `cwd`, for an index path given relative to it.
fn mur_search_in(home: &TempDir, cwd: &Path, args: &[&str]) -> assert_cmd::assert::Assert {
    let mut cmd = Command::cargo_bin("mur").unwrap();
    cmd.env("HOME", home.path()).current_dir(cwd).arg("search");
    for arg in args {
        cmd.arg(arg);
    }
    cmd.assert()
}

fn stdout_of(assert: &assert_cmd::assert::Assert) -> String {
    String::from_utf8_lossy(&assert.get_output().stdout).into_owned()
}

fn stderr_of(assert: &assert_cmd::assert::Assert) -> String {
    String::from_utf8_lossy(&assert.get_output().stderr).into_owned()
}

/// `(NAME, VERSION)` of every table row, header excluded; the table ends at the first blank line.
fn rows(stdout: &str) -> Vec<(String, String)> {
    stdout
        .lines()
        .skip(1)
        .take_while(|line| !line.trim().is_empty())
        .map(|line| {
            let mut cells = line.split_whitespace();
            (
                cells.next().unwrap().to_string(),
                cells.next().unwrap().to_string(),
            )
        })
        .collect()
}

/// The lines after the table.
fn notes(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .skip_while(|line| !line.trim().is_empty())
        .filter(|line| !line.trim().is_empty())
        .map(str::to_string)
        .collect()
}

fn pairs(expected: &[(&str, &str)]) -> Vec<(String, String)> {
    expected
        .iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect()
}

fn write_config_index_url(home: &TempDir, url: &str) {
    let config_dir = home.path().join(".murmur");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.yaml"),
        format!("registry:\n  index_url: {url}\n"),
    )
    .unwrap();
}

// ── Query and table ───────────────────────────────────────────────────────────

/// Absolute index path: query matches → result row printed under the full name.
#[test]
fn search_index_query_matches_returns_row() {
    let dir = TempDir::new().unwrap();
    let idx = write_index(&dir, &[tool_entry("tool-git")]);
    mur_search(&dir, &["git", "--index", &idx])
        .success()
        .stdout(predicate::str::contains("murmur/tool-git  1.0.0"));
}

/// Absolute index path: query doesn't match → "No results found", exits 0.
#[test]
fn search_index_no_match_prints_no_results() {
    let dir = TempDir::new().unwrap();
    let idx = write_index(&dir, &[tool_entry("tool-git")]);
    mur_search(&dir, &["zzznomatch", "--index", &idx])
        .success()
        .stdout(predicate::str::contains("No results found"));
}

/// Each row shows the newest version that fits this mur, and a note under the table says why
/// the newest version is not the one shown.
#[test]
fn search_shows_the_newest_version_that_fits_this_mur() {
    let dir = TempDir::new().unwrap();
    let idx = write_index(&dir, &fit_index_entries());
    let stdout = stdout_of(&mur_search(&dir, &["", "--index", &idx]).success());
    assert_eq!(
        rows(&stdout),
        pairs(&[
            ("murmur/hook-old", "\u{2014}"),
            ("murmur/skill-x", "0.4.0"),
            ("murmur/tool-editor", "0.7.0"),
            ("murmur/tool-git", "0.2.0"),
        ]),
        "{stdout}"
    );
    assert_eq!(
        notes(&stdout),
        [
            "murmur/hook-old: 1.0.0 needs murmur:nowhere/x@1.0.0, this mur serves no version of \
             murmur:nowhere"
                .to_string(),
            format!(
                "murmur/tool-editor: 0.8.0 needs murmur:tool/run@9.9.9, this mur serves \
                 murmur:tool@{}",
                served("murmur:tool")
            ),
            format!(
                "murmur/tool-git: 0.3.0 has no payload for {}",
                current_platform()
            ),
        ],
        "{stdout}"
    );
}

#[test]
fn search_matches_full_name_description_and_tags() {
    let dir = TempDir::new().unwrap();
    let idx = write_index(&dir, &fit_index_entries());
    for (query, expected) in [
        ("MURMUR/TOOL", vec!["murmur/tool-editor", "murmur/tool-git"]),
        ("patch files", vec!["murmur/tool-editor"]),
        ("old", vec!["murmur/hook-old"]),
    ] {
        let stdout = stdout_of(&mur_search(&dir, &[query, "--index", &idx]).success());
        let names: Vec<String> = rows(&stdout).into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, expected, "{query}: {stdout}");
    }
}

/// --limit caps the number of result rows.
#[test]
fn search_limit_caps_results() {
    let dir = TempDir::new().unwrap();
    let idx = write_index(
        &dir,
        &[
            tool_entry("tool-alpha"),
            tool_entry("tool-beta"),
            tool_entry("tool-gamma"),
        ],
    );
    let stdout = stdout_of(&mur_search(&dir, &["tool", "--index", &idx, "--limit", "1"]).success());
    assert_eq!(rows(&stdout), pairs(&[("murmur/tool-alpha", "1.0.0")]));
    assert_eq!(stdout.lines().count(), 2, "{stdout}");
}

/// A relative index path is read from disk against the working directory, never fetched.
#[test]
fn search_relative_index_path_is_read_from_disk() {
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    write_index(&dir, &[tool_entry("tool-git")]);
    for relative in ["./artifacts-index.json", "artifacts-index.json"] {
        mur_search_in(&home, dir.path(), &["git", "--index", relative])
            .success()
            .stdout(predicate::str::contains("murmur/tool-git"));
    }
}

/// An http:// index is fetched, and prints what the same file prints.
#[test]
fn search_http_index_is_fetched() {
    let home = TempDir::new().unwrap();
    let file = write_index(&home, &fit_index_entries());
    let from_file = stdout_of(&mur_search(&home, &["", "--index", &file]).success());
    let server = common::ScriptedServer::start(vec![index_json(&fit_index_entries())]);
    let url = format!("{}/artifacts-index.json", server.endpoint);
    let from_url = stdout_of(&mur_search(&home, &["", "--index", &url]).success());
    assert_eq!(from_url, from_file);
    assert_eq!(server.requests().len(), 1);
}

/// With no --index, `registry.index_url` in ~/.murmur/config.yaml is fetched.
#[test]
fn search_without_index_fetches_the_configured_index_url() {
    let home = TempDir::new().unwrap();
    let server = common::ScriptedServer::start(vec![index_json(&[tool_entry("tool-editor")])]);
    write_config_index_url(&home, &format!("{}/artifacts-index.json", server.endpoint));
    mur_search(&home, &["editor"])
        .success()
        .stdout(predicate::str::contains("murmur/tool-editor"));
    assert_eq!(server.requests().len(), 1);
}

// ── Indexes this mur does not read ────────────────────────────────────────────

const SCHEMA_1: &str = r#"{
  "schema_version": "1",
  "updated_at": "2026-06-07T00:00:00Z",
  "artifacts": [{"name": "murmur-tool-git", "version": "1.0.0", "runtime": "tool", "tags": [], "platforms": []}]
}"#;

fn assert_schema_1_refused(assert: assert_cmd::assert::Assert, source: &str) {
    let stderr = stderr_of(&assert.failure());
    assert!(
        stderr.contains(&format!(
            "error[E-IDX-002]: artifact index {source} is a schema 1 index"
        )),
        "{stderr}"
    );
    assert!(stderr.contains(SCHEMA_1_HINT), "{stderr}");
}

#[test]
fn a_schema_1_index_is_refused_with_a_hint_from_a_file() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("old.json");
    fs::write(&path, SCHEMA_1).unwrap();
    let path = path.to_str().unwrap();
    assert_schema_1_refused(mur_search(&dir, &["git", "--index", path]), path);
}

#[test]
fn a_schema_1_index_is_refused_with_a_hint_over_http() {
    let home = TempDir::new().unwrap();
    let server = common::ScriptedServer::start(vec![SCHEMA_1.to_string()]);
    let url = format!("{}/artifacts-index.json", server.endpoint);
    assert_schema_1_refused(mur_search(&home, &["git", "--index", &url]), &url);
}

#[test]
fn a_schema_1_index_is_refused_with_a_hint_from_the_configured_url() {
    let home = TempDir::new().unwrap();
    let server = common::ScriptedServer::start(vec![SCHEMA_1.to_string()]);
    let url = format!("{}/artifacts-index.json", server.endpoint);
    write_config_index_url(&home, &url);
    assert_schema_1_refused(mur_search(&home, &["git"]), &url);
}

#[test]
fn another_schema_version_is_refused_by_number() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("bad.json");
    fs::write(&path, r#"{"schema_version": 99, "artifacts": []}"#).unwrap();
    let path = path.to_str().unwrap();
    mur_search(&dir, &["git", "--index", path])
        .failure()
        .stderr(predicate::str::contains(format!(
            "error[E-IDX-002]: artifact index {path} has schema_version 99"
        )));
}

#[test]
fn an_invalid_schema_2_index_names_the_json_location() {
    let dir = TempDir::new().unwrap();
    let mut value: Value = serde_json::from_str(&index_json(&[tool_entry("tool-git")])).unwrap();
    value["artifacts"][0]["versions"][0]
        .as_object_mut()
        .unwrap()
        .remove("assets");
    let path = dir.path().join("bad.json");
    fs::write(&path, value.to_string()).unwrap();
    mur_search(&dir, &["git", "--index", path.to_str().unwrap()])
        .failure()
        .stderr(predicate::str::contains("error[E-IDX-002]"))
        .stderr(predicate::str::contains(
            "artifacts[0].versions[0].assets is missing",
        ));
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

/// Network failure: unreachable URL → E-IO-003 naming the URL.
#[test]
fn search_network_error_exits_nonzero_with_url_in_message() {
    let home = TempDir::new().unwrap();
    let bad_url = "https://this-does-not-exist.invalid/artifacts-index.json";
    mur_search(&home, &["git", "--index", bad_url])
        .failure()
        .stderr(predicate::str::contains("E-IO-003"))
        .stderr(predicate::str::contains(bad_url));
}

// ── --index local ─────────────────────────────────────────────────────────────

/// Local scan: populated store → artifact appears in results, under its full name.
#[test]
fn search_local_populated_returns_artifact() {
    let home = TempDir::new().unwrap();
    publish_meta(&home, "local/tool-editor");
    mur_search(&home, &["editor", "--index", "local"])
        .success()
        .stdout(predicate::str::contains("local/tool-editor"));
}

/// Local scan: empty store → "No results found", exits 0, no network call.
#[test]
fn search_local_empty_returns_no_results() {
    let home = TempDir::new().unwrap();
    mur_search(&home, &["git", "--index", "local"])
        .success()
        .stdout(predicate::str::contains("No results found"));
}

/// The global store's artifacts fit by the same rule: a current hook is listed at its version,
/// a stale one at `—` with a note, including when its sidecar has lost its recorded contracts.
#[test]
fn search_local_applies_the_fit_rule_to_the_global_store() {
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let hook = served("murmur:hook");
    let current = format!(
        "(component (instance $i) (export \"murmur:hook/lifecycle@{hook}\" (instance $i)))"
    );
    let stale = "(component (instance $i) (export \"murmur:hook/lifecycle@0.0.1\" (instance $i)))";
    publish_hook(
        &home,
        work.path(),
        "hook-now",
        "0.2.0",
        &wat::parse_str(&current).unwrap(),
    );
    publish_hook(
        &home,
        work.path(),
        "hook-stale",
        "0.3.0",
        &wat::parse_str(stale).unwrap(),
    );
    publish_hook(
        &home,
        work.path(),
        "hook-bare",
        "0.4.0",
        &wat::parse_str(stale).unwrap(),
    );

    let path = sidecar(&home, "hook-bare", "0.4.0");
    let mut value: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert!(value["meta"]
        .as_object_mut()
        .unwrap()
        .remove("wit_contracts")
        .is_some());
    fs::write(&path, value.to_string()).unwrap();

    let stdout = stdout_of(&mur_search(&home, &["hook", "--index", "local"]).success());
    assert_eq!(
        rows(&stdout),
        pairs(&[
            ("local/hook-bare", "\u{2014}"),
            ("local/hook-now", "0.2.0"),
            ("local/hook-stale", "\u{2014}"),
        ]),
        "{stdout}"
    );
    let line = |name: &str, version: &str| {
        format!(
            "local/{name}: {version} needs murmur:hook/lifecycle@0.0.1, this mur serves \
             murmur:hook@{hook}"
        )
    };
    assert_eq!(
        notes(&stdout),
        [line("hook-bare", "0.4.0"), line("hook-stale", "0.3.0")],
        "{stdout}"
    );
    assert!(value["meta"].get("wit_contracts").is_none());
    let after: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert!(
        after["meta"].get("wit_contracts").is_none(),
        "search wrote the sidecar"
    );
}
