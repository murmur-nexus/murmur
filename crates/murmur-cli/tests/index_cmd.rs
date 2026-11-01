//! `mur index`: the schema 2 artifact index built from a directory of real `.mur.zip` files,
//! checked against the published JSON Schema, the files' own digests and the store's record of
//! the same payloads.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::time::{Duration, SystemTime};

use assert_cmd::Command;
use murmur_artifact::ArtifactIndex;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use zip::write::SimpleFileOptions;
use zip::ZipWriter;

/// A hook component exporting `murmur:hook/lifecycle` at a version no `mur` serves. The extractor
/// reads only the import and export sections, so the component needs no functions.
const UNSERVED_HOOK_WAT: &str = r#"
    (component
      (instance $i)
      (export "murmur:hook/lifecycle@0.0.1" (instance $i)))
"#;

const UNSERVED_HOOK_INTERFACE: &str = "murmur:hook/lifecycle@0.0.1";

fn fixture_bytes(relative: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(relative);
    fs::read(&path).unwrap_or_else(|err| panic!("missing fixture {}: {err}", path.display()))
}

fn schema_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/content/reference/artifact-index.schema.json")
}

/// Write `<dir>/<file>`, a zip holding `manifest` as `murmur.yaml` plus `entries`.
fn pack(dir: &Path, file: &str, manifest: &str, entries: &[(&str, &[u8])]) -> PathBuf {
    let path = dir.join(file);
    let mut zip = ZipWriter::new(fs::File::create(&path).unwrap());
    let options = SimpleFileOptions::default();
    zip.start_file("murmur.yaml", options).unwrap();
    zip.write_all(manifest.as_bytes()).unwrap();
    for (name, bytes) in entries {
        zip.start_file(*name, options).unwrap();
        zip.write_all(bytes).unwrap();
    }
    zip.finish().unwrap();
    path
}

fn pack_echo_tool(dir: &Path, version: &str) -> PathBuf {
    pack(
        dir,
        &format!("echo-tool-{version}.mur.zip"),
        &format!(
            "name: echo-tool\nversion: {version}\nruntime: tool\ndescription: >\n  Echo the input \
             back. Nothing else happens.\n"
        ),
        &[(
            "capsule.wasm",
            &fixture_bytes("run/components/echo-tool.wasm"),
        )],
    )
}

fn pack_native(dir: &Path, name: &str, version: &str, platform: &str) -> PathBuf {
    pack(
        dir,
        &format!("{name}-{version}-{platform}.mur.zip"),
        &format!("name: {name}\nversion: {version}\nruntime: tool\nimplementation: native\n"),
        &[(&format!("bin/{name}"), b"#!/bin/sh\necho native\n")],
    )
}

/// A release directory: an echo tool at two versions, a hook exporting an interface no `mur`
/// serves, a native tool with two platform payloads, a skill, and two entries `mur index` skips.
fn release_dir() -> TempDir {
    let dir = TempDir::new().unwrap();
    pack_echo_tool(dir.path(), "0.1.0");
    pack_echo_tool(dir.path(), "0.2.0");
    pack(
        dir.path(),
        "hook-old-0.3.0.mur.zip",
        "name: hook-old\nversion: 0.3.0\nruntime: hook\n",
        &[("hook.wasm", &wat::parse_str(UNSERVED_HOOK_WAT).unwrap())],
    );
    pack_native(dir.path(), "tool-native", "1.0.0", "linux-x86_64");
    pack_native(dir.path(), "tool-native", "1.0.0", "darwin-aarch64");
    pack(
        dir.path(),
        "skill-notes-0.1.0.mur.zip",
        "name: skill-notes\nversion: 0.1.0\nruntime: skill\ndescription: Take notes!\n",
        &[("skill.md", b"# Notes\n")],
    );
    fs::write(dir.path().join("notes.txt"), "not an artifact").unwrap();
    fs::create_dir(dir.path().join("sub")).unwrap();
    pack_echo_tool(&dir.path().join("sub"), "9.9.9");
    dir
}

fn mur(home: &TempDir, cwd: &Path, args: &[&str]) -> assert_cmd::assert::Assert {
    Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .current_dir(cwd)
        .args(args)
        .assert()
}

fn mur_index(cwd: &Path, args: &[&str]) -> assert_cmd::assert::Assert {
    let home = TempDir::new().unwrap();
    let mut full = vec!["index"];
    full.extend_from_slice(args);
    mur(&home, cwd, &full)
}

fn stdout_of(assert: &assert_cmd::assert::Assert) -> String {
    String::from_utf8_lossy(&assert.get_output().stdout).into_owned()
}

fn stderr_of(assert: &assert_cmd::assert::Assert) -> String {
    String::from_utf8_lossy(&assert.get_output().stderr).into_owned()
}

fn sha256_of(path: &Path) -> String {
    Sha256::digest(fs::read(path).unwrap())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn entries_of(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Validate `index` against the published JSON Schema with Python's `jsonschema`, when the host
/// has it.
fn assert_matches_published_schema(index: &Path) {
    let has_validator = StdCommand::new("python3")
        .args(["-I", "-c", "import jsonschema"])
        .output()
        .is_ok_and(|out| out.status.success());
    if !has_validator {
        eprintln!("[SKIP] schema validation: python3 with jsonschema is not installed");
        return;
    }
    let out = StdCommand::new("python3")
        .args([
            "-I",
            "-c",
            "import json,sys,jsonschema; s=json.load(open(sys.argv[1])); \
             jsonschema.Draft202012Validator.check_schema(s); \
             jsonschema.Draft202012Validator(s).validate(json.load(open(sys.argv[2])))",
        ])
        .arg(schema_path())
        .arg(index)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{} does not validate: {}",
        index.display(),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn artifact<'a>(index: &'a Value, name: &str) -> &'a Value {
    index["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == name)
        .unwrap_or_else(|| panic!("{name} not in {index}"))
}

// ── Indexing a release directory ─────────────────────────────────────────────

#[test]
fn indexes_every_artifact_in_the_directory_from_the_files_themselves() {
    let dir = release_dir();
    let out_dir = TempDir::new().unwrap();
    let assert = mur_index(
        out_dir.path(),
        &[
            dir.path().to_str().unwrap(),
            "--repo",
            "ana/my-artifacts",
            "--release",
            "v1.0.0",
            "-o",
            "out.json",
        ],
    )
    .success();
    assert_eq!(
        stdout_of(&assert),
        "Indexed 4 artifacts (5 versions) for scope ana into out.json\n"
    );

    let out = out_dir.path().join("out.json");
    assert_matches_published_schema(&out);
    let text = fs::read_to_string(&out).unwrap();
    ArtifactIndex::from_json(&text).unwrap();
    assert!(!text.contains("updated_at"), "{text}");
    assert!(!text.contains("wasi:"), "{text}");

    let index: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(index["schema_version"], json!(2));
    assert_eq!(index["scope"], "ana");
    let names: Vec<&str> = index["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["echo-tool", "hook-old", "skill-notes", "tool-native"]
    );

    for a in index["artifacts"].as_array().unwrap() {
        for v in a["versions"].as_array().unwrap() {
            assert_eq!(v["release"], "v1.0.0");
            for asset in v["assets"].as_array().unwrap() {
                let file = asset["file"].as_str().unwrap();
                assert_eq!(asset["sha256"], sha256_of(&dir.path().join(file)), "{file}");
            }
        }
    }

    let echo = artifact(&index, "echo-tool");
    assert_eq!(echo["runtime"], "tool");
    assert_eq!(echo["description"], "Echo the input back.");
    assert_eq!(echo["tags"], json!(["tool", "echo"]));
    let versions: Vec<&str> = echo["versions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["version"].as_str().unwrap())
        .collect();
    assert_eq!(versions, ["0.2.0", "0.1.0"]);
    assert_eq!(
        echo["versions"][0]["assets"],
        json!([{ "file": "echo-tool-0.2.0.mur.zip", "sha256": sha256_of(&dir.path().join("echo-tool-0.2.0.mur.zip")) }])
    );

    // The same `murmur:` names `mur publish` records for the same payload.
    let home = TempDir::new().unwrap();
    mur(
        &home,
        home.path(),
        &[
            "publish",
            dir.path().join("echo-tool-0.2.0.mur.zip").to_str().unwrap(),
        ],
    )
    .success();
    let sidecar: Value = serde_json::from_str(
        &fs::read_to_string(
            home.path()
                .join(".murmur/artifacts/local/echo-tool/0.2.0/echo-tool-0.2.0.meta.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let contracts = &sidecar["meta"]["wit_contracts"];
    let mut recorded: Vec<&str> = contracts["exports"]
        .as_array()
        .unwrap()
        .iter()
        .chain(contracts["imports"].as_array().unwrap())
        .filter_map(|n| n.as_str())
        .filter(|n| n.starts_with("murmur:"))
        .collect();
    recorded.sort();
    recorded.dedup();
    assert_eq!(recorded, ["murmur:tool/run@0.2.0"]);
    assert_eq!(echo["versions"][0]["interfaces"], json!(recorded));

    let hook = artifact(&index, "hook-old");
    assert_eq!(hook["tags"], json!(["hook", "old"]));
    assert!(hook.get("description").is_none());
    assert_eq!(
        hook["versions"][0]["interfaces"],
        json!([UNSERVED_HOOK_INTERFACE])
    );

    let native = artifact(&index, "tool-native");
    assert_eq!(native["versions"].as_array().unwrap().len(), 1);
    assert_eq!(native["versions"][0]["interfaces"], json!([]));

    // None of these files packs a label, so each is derived from its interfaces: the newest level
    // they fit, or none for the hook that fits no level.
    let served = capsule_runtime::served_wit_level().level;
    for name in ["echo-tool", "skill-notes", "tool-native"] {
        for v in artifact(&index, name)["versions"].as_array().unwrap() {
            assert_eq!(v["built_at_level"], json!(served), "{name}: {v}");
        }
    }
    assert!(
        hook["versions"][0].get("built_at_level").is_none(),
        "{hook}"
    );
    let platforms: Vec<&str> = native["versions"][0]["assets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["platform"].as_str().unwrap())
        .collect();
    assert_eq!(platforms, ["darwin-aarch64", "linux-x86_64"]);

    let skill = artifact(&index, "skill-notes");
    assert_eq!(skill["description"], "Take notes!");
    assert_eq!(skill["tags"], json!(["skill", "notes"]));
    assert_eq!(skill["versions"][0]["interfaces"], json!([]));
    assert!(skill["versions"][0]["assets"][0].get("platform").is_none());
}

// ── Determinism ───────────────────────────────────────────────────────────────

#[test]
fn indexing_the_same_files_gives_the_same_bytes() {
    let dir = release_dir();
    let out = TempDir::new().unwrap();
    let args = |output: &str| {
        vec![
            dir.path().to_str().unwrap().to_string(),
            "--repo".into(),
            "ana/my-artifacts".into(),
            "--release".into(),
            "v1.0.0".into(),
            "-o".into(),
            output.to_string(),
        ]
    };
    let run = |args: Vec<String>| {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        mur_index(out.path(), &args).success();
    };
    run(args("a.json"));
    run(args("b.json"));
    let a = fs::read(out.path().join("a.json")).unwrap();
    assert_eq!(a, fs::read(out.path().join("b.json")).unwrap());

    // A copy written in reverse order, every file with another modification time.
    let copy = TempDir::new().unwrap();
    let mut files = entries_of(dir.path());
    files.reverse();
    let when = SystemTime::now() - Duration::from_secs(86_400 * 400);
    for file in files.iter().filter(|f| f.ends_with(".mur.zip")) {
        fs::copy(dir.path().join(file), copy.path().join(file)).unwrap();
        fs::File::options()
            .write(true)
            .open(copy.path().join(file))
            .unwrap()
            .set_modified(when)
            .unwrap();
    }
    mur_index(
        out.path(),
        &[
            copy.path().to_str().unwrap(),
            "--repo",
            "ana/my-artifacts",
            "--release",
            "v1.0.0",
            "-o",
            "c.json",
        ],
    )
    .success();
    assert_eq!(a, fs::read(out.path().join("c.json")).unwrap());

    // Merging the same directory over its own index, in place.
    let assert = mur_index(
        out.path(),
        &[
            dir.path().to_str().unwrap(),
            "--repo",
            "ana/my-artifacts",
            "--release",
            "v1.0.0",
            "--merge",
            "a.json",
            "-o",
            "a.json",
        ],
    )
    .success();
    assert!(
        stdout_of(&assert).ends_with("; 0 versions kept from a.json, 0 replaced\n"),
        "{}",
        stdout_of(&assert)
    );
    assert_eq!(a, fs::read(out.path().join("a.json")).unwrap());
    assert_eq!(entries_of(out.path()), ["a.json", "b.json", "c.json"]);
}

// ── Files that cannot be indexed ──────────────────────────────────────────────

/// Run `mur index` over `dir` into an `out.json` that already exists, and assert it is refused
/// with `code`, every `needle` on stderr, and `out.json` untouched with nothing left beside it.
fn assert_refused(dir: &Path, extra: &[&str], code: &str, needles: &[&str]) {
    let out = TempDir::new().unwrap();
    let before = b"{\"existing\": true}\n";
    fs::write(out.path().join("out.json"), before).unwrap();
    let mut args = vec![
        dir.to_str().unwrap(),
        "--repo",
        "ana/my-artifacts",
        "--release",
        "v1.0.0",
        "-o",
        "out.json",
    ];
    args.extend_from_slice(extra);
    let assert = mur_index(out.path(), &args).failure();
    let stderr = stderr_of(&assert);
    assert!(stderr.contains(&format!("error[{code}]")), "{stderr}");
    for needle in needles {
        assert!(stderr.contains(needle), "missing {needle:?} in {stderr}");
    }
    assert_eq!(fs::read(out.path().join("out.json")).unwrap(), before);
    assert_eq!(entries_of(out.path()), ["out.json"]);
}

#[test]
fn a_file_that_cannot_be_indexed_refuses_the_directory() {
    let native = "runtime: tool\nimplementation: native\n";
    let cases: Vec<(&str, String, &str)> = vec![
        (
            "tool-a-0.2.0.mur.zip",
            "name: tool-a\nversion: 0.1.0\nruntime: tool\n".into(),
            "does not match its packed name 'tool-a' and version '0.1.0'",
        ),
        (
            "tool-b-0.1.0.mur.zip",
            "name: tool-a\nversion: 0.1.0\nruntime: tool\n".into(),
            "does not match its packed name 'tool-a'",
        ),
        (
            "tool-a-0.1.0.mur.zip",
            format!("name: tool-a\nversion: 0.1.0\n{native}"),
            "a native payload names its platform",
        ),
        (
            "tool-a-0.1.0-linux-x86_64.mur.zip",
            "name: tool-a\nversion: 0.1.0\nruntime: tool\n".into(),
            "a wasm payload runs on every platform; name it tool-a-0.1.0.mur.zip",
        ),
        (
            "tool-a-banana.mur.zip",
            "name: tool-a\nversion: banana\nruntime: tool\n".into(),
            "packed version 'banana' is not a semantic version",
        ),
        (
            "tool-a-latest.mur.zip",
            "name: tool-a\nversion: latest\nruntime: tool\n".into(),
            "packed version 'latest' is reserved",
        ),
    ];
    for (file, manifest, reason) in cases {
        let dir = TempDir::new().unwrap();
        pack(dir.path(), file, &manifest, &[]);
        assert_refused(dir.path(), &[], "E-IDX-001", &[file, reason]);
    }

    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("tool-a-0.1.0.mur.zip"), b"").unwrap();
    assert_refused(
        dir.path(),
        &[],
        "E-IDX-001",
        &["tool-a-0.1.0.mur.zip", "is not a zip archive"],
    );

    let dir = TempDir::new().unwrap();
    let mut zip =
        ZipWriter::new(fs::File::create(dir.path().join("tool-a-0.1.0.mur.zip")).unwrap());
    zip.start_file("README.md", SimpleFileOptions::default())
        .unwrap();
    zip.write_all(b"no manifest").unwrap();
    zip.finish().unwrap();
    assert_refused(
        dir.path(),
        &[],
        "E-IDX-001",
        &["tool-a-0.1.0.mur.zip", "holds no packed murmur.yaml"],
    );

    // Two payloads of one version whose packed runtime differs.
    let dir = TempDir::new().unwrap();
    pack(
        dir.path(),
        "tool-a-0.1.0-darwin-aarch64.mur.zip",
        "name: tool-a\nversion: 0.1.0\nruntime: tool\nexecution: native\n",
        &[],
    );
    pack(
        dir.path(),
        "tool-a-0.1.0-linux-x86_64.mur.zip",
        "name: tool-a\nversion: 0.1.0\nruntime: hook\nexecution: native\n",
        &[],
    );
    assert_refused(
        dir.path(),
        &[],
        "E-IDX-001",
        &[
            "tool-a-0.1.0-linux-x86_64.mur.zip",
            "tool-a-0.1.0-darwin-aarch64.mur.zip",
            "runtime 'hook'",
        ],
    );

    let empty = TempDir::new().unwrap();
    assert_refused(empty.path(), &[], "E-IDX-001", &["holds no .mur.zip file"]);
    fs::write(empty.path().join("a.txt"), "x").unwrap();
    fs::write(empty.path().join("b.txt"), "y").unwrap();
    assert_refused(empty.path(), &[], "E-IDX-001", &["holds no .mur.zip file"]);
}

// ── a wasm payload must carry a readable component ────────────────────────────

/// Index a directory holding one `runtime: tool` payload, `tool-w-0.1.0.mur.zip`, packing
/// `entries`, and expect `E-IDX-001` naming the file and `reason`.
fn assert_component_refused(entries: &[(&str, &[u8])], env: Option<(&str, &str)>, reason: &str) {
    let dir = TempDir::new().unwrap();
    pack(
        dir.path(),
        "tool-w-0.1.0.mur.zip",
        "name: tool-w\nversion: 0.1.0\nruntime: tool\n",
        entries,
    );
    let out = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let mut command = Command::cargo_bin("mur").unwrap();
    command
        .env("HOME", home.path())
        .current_dir(out.path())
        .args([
            "index",
            dir.path().to_str().unwrap(),
            "--repo",
            "ana/artifacts",
            "--release",
            "v1",
            "--output",
            "artifacts-index.json",
        ]);
    if let Some((key, value)) = env {
        command.env(key, value);
    }
    let assert = command.assert().code(1);
    let stderr = stderr_of(&assert);
    for needle in [
        "error[E-IDX-001]",
        "tool-w-0.1.0.mur.zip",
        "its wasm payload cannot be read as a component: ",
        reason,
    ] {
        assert!(stderr.contains(needle), "missing {needle:?} in {stderr}");
    }
    assert_eq!(entries_of(out.path()), Vec::<String>::new());
}

#[test]
fn a_wasm_payload_with_no_wasm_entry_is_refused() {
    assert_component_refused(
        &[("README.md", b"no component")],
        None,
        "it has no root .wasm entry (expected capsule.wasm or one root *.wasm)",
    );
}

#[test]
fn a_wasm_payload_whose_wasm_does_not_parse_is_refused() {
    assert_component_refused(
        &[("capsule.wasm", b"not wasm")],
        None,
        "its root .wasm entry capsule.wasm does not parse as a component: ",
    );
}

#[test]
fn a_wasm_payload_holding_a_core_module_is_refused() {
    assert_component_refused(
        &[("capsule.wasm", &wat::parse_str("(module)").unwrap())],
        None,
        "its root .wasm entry capsule.wasm is a core module, not a component",
    );
}

#[test]
fn a_wasm_payload_with_ambiguous_root_entries_is_refused() {
    let echo = fixture_bytes("run/components/echo-tool.wasm");
    assert_component_refused(
        &[("a.wasm", &echo), ("b.wasm", &echo)],
        None,
        "its root .wasm entries are ambiguous: a.wasm, b.wasm, and none is capsule.wasm",
    );
}

#[test]
fn a_wasm_payload_over_the_decompression_cap_is_refused() {
    assert_component_refused(
        &[(
            "capsule.wasm",
            &fixture_bytes("run/components/echo-tool.wasm"),
        )],
        Some(("MURMUR_MAX_ARTIFACT_DECOMPRESSED_BYTES", "64")),
        "its root .wasm entry capsule.wasm is over the 64-byte decompression cap",
    );
}

/// Only a wasm payload is expected to carry a component: a skill and a native payload index with
/// no interfaces.
#[test]
fn a_skill_or_native_payload_without_wasm_still_indexes() {
    let dir = TempDir::new().unwrap();
    pack(
        dir.path(),
        "skill-notes-0.1.0.mur.zip",
        "name: skill-notes\nversion: 0.1.0\nruntime: skill\n",
        &[("skill.md", b"# Notes\n")],
    );
    pack_native(dir.path(), "tool-native", "1.0.0", "linux-x86_64");
    let out = TempDir::new().unwrap();
    mur_index(
        out.path(),
        &[
            dir.path().to_str().unwrap(),
            "--repo",
            "ana/artifacts",
            "--release",
            "v1",
            "--output",
            "artifacts-index.json",
        ],
    )
    .success();
    let index: Value =
        serde_json::from_slice(&fs::read(out.path().join("artifacts-index.json")).unwrap())
            .unwrap();
    let artifacts = index["artifacts"].as_array().unwrap();
    assert_eq!(artifacts.len(), 2);
    for artifact in artifacts {
        assert_eq!(
            artifact["versions"][0]["interfaces"],
            json!([]),
            "{artifact}"
        );
    }
}

// ── --repo and --release ──────────────────────────────────────────────────────

#[test]
fn the_scope_comes_from_the_repository_by_the_install_rule() {
    let dir = TempDir::new().unwrap();
    pack_echo_tool(dir.path(), "0.1.0");
    for (repo, scope) in [
        ("ana/my-artifacts", "ana"),
        ("Ana/my-artifacts", "ana"),
        ("murmur-nexus/default-artifacts", "murmur"),
    ] {
        let out = TempDir::new().unwrap();
        mur_index(
            out.path(),
            &[
                dir.path().to_str().unwrap(),
                "--repo",
                repo,
                "--release",
                "v1",
            ],
        )
        .success();
        let index: Value = serde_json::from_str(
            &fs::read_to_string(out.path().join("artifacts-index.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(index["scope"], scope, "{repo}");
    }
}

#[test]
fn an_unusable_repo_or_release_is_refused() {
    let dir = TempDir::new().unwrap();
    pack_echo_tool(dir.path(), "0.1.0");
    for (repo, reason) in [
        ("local/x", "reserved scope"),
        ("murmur/x", "reserved scope"),
        ("ana", "is not a GitHub repository written <owner>/<repo>"),
        (
            "ana/x/y",
            "is not a GitHub repository written <owner>/<repo>",
        ),
        ("an_a/x", "is not a valid artifact scope"),
    ] {
        let out = TempDir::new().unwrap();
        let assert = mur_index(
            out.path(),
            &[
                dir.path().to_str().unwrap(),
                "--repo",
                repo,
                "--release",
                "v1",
            ],
        )
        .failure();
        let stderr = stderr_of(&assert);
        assert!(
            stderr.contains("error[E-IDX-003]")
                && stderr.contains(&format!("'{repo}'"))
                && stderr.contains(reason),
            "{repo}: {stderr}"
        );
        assert!(entries_of(out.path()).is_empty());
    }
    for (release, reason) in [
        ("", "is empty"),
        ("v 1", "contains whitespace or a control character"),
    ] {
        let out = TempDir::new().unwrap();
        let assert = mur_index(
            out.path(),
            &[
                dir.path().to_str().unwrap(),
                "--repo",
                "ana/r",
                "--release",
                release,
            ],
        )
        .failure();
        let stderr = stderr_of(&assert);
        assert!(
            stderr.contains("error[E-IDX-003]")
                && stderr.contains(&format!("--release '{release}'"))
                && stderr.contains(reason),
            "{release:?}: {stderr}"
        );
        assert!(entries_of(out.path()).is_empty());
    }
}

#[test]
fn repo_and_release_are_required() {
    let dir = TempDir::new().unwrap();
    for args in [
        vec![dir.path().to_str().unwrap(), "--release", "v1"],
        vec![dir.path().to_str().unwrap(), "--repo", "ana/r"],
    ] {
        mur_index(dir.path(), &args).code(2);
    }
}

/// A packed `murmur-build.json` is written as it is, whatever the interfaces would derive; files of
/// one version that pack different labels are refused.
#[test]
fn a_packed_label_is_written_and_two_labels_of_one_version_are_refused() {
    let dir = TempDir::new().unwrap();
    pack(
        dir.path(),
        "hook-old-0.3.0.mur.zip",
        "name: hook-old\nversion: 0.3.0\nruntime: hook\n",
        &[
            ("hook.wasm", &wat::parse_str(UNSERVED_HOOK_WAT).unwrap()),
            ("murmur-build.json", b"{\"built_at_level\":1}\n"),
        ],
    );
    let out = TempDir::new().unwrap();
    mur_index(
        out.path(),
        &[
            dir.path().to_str().unwrap(),
            "--repo",
            "ana/r",
            "--release",
            "v1",
            "-o",
            "out.json",
        ],
    )
    .success();
    let path = out.path().join("out.json");
    assert_matches_published_schema(&path);
    let index: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        artifact(&index, "hook-old")["versions"][0]["built_at_level"],
        json!(1)
    );

    let dir = TempDir::new().unwrap();
    let manifest = "name: tool-n\nversion: 1.0.0\nruntime: tool\nimplementation: native\n";
    for (platform, label) in [("darwin-aarch64", "1"), ("linux-x86_64", "2")] {
        pack(
            dir.path(),
            &format!("tool-n-1.0.0-{platform}.mur.zip"),
            manifest,
            &[(
                "murmur-build.json",
                format!("{{\"built_at_level\":{label}}}").as_bytes(),
            )],
        );
    }
    let assert = mur_index(
        out.path(),
        &[
            dir.path().to_str().unwrap(),
            "--repo",
            "ana/r",
            "--release",
            "v1",
            "-o",
            "two.json",
        ],
    )
    .failure();
    let stderr = stderr_of(&assert);
    assert!(
        stderr.contains("error[E-IDX-001]")
            && stderr.contains("tool-n-1.0.0-linux-x86_64.mur.zip")
            && stderr.contains("tool-n-1.0.0-darwin-aarch64.mur.zip")
            && stderr.contains("its packed murmur-build.json sets built_at_level 2"),
        "{stderr}"
    );
    assert!(!out.path().join("two.json").exists());
}

// ── --merge ───────────────────────────────────────────────────────────────────

fn asset(file: &str, fill: char) -> Value {
    json!({ "file": file, "sha256": fill.to_string().repeat(64) })
}

/// The earlier index: `tool-gone` 1.0.0, and `tool-x` 0.2.0 plus 0.1.0 carrying a release of
/// its own and a `built_at_level`.
fn old_index(scope: &str) -> Value {
    json!({
        "schema_version": 2,
        "scope": scope,
        "artifacts": [
            {
                "name": "tool-gone",
                "runtime": "tool",
                "tags": ["tool", "gone"],
                "versions": [
                    { "version": "1.0.0", "release": "v0.1.0", "interfaces": [],
                      "assets": [asset("tool-gone-1.0.0.mur.zip", 'e')] }
                ]
            },
            {
                "name": "tool-x",
                "runtime": "tool",
                "tags": ["tool", "x"],
                "versions": [
                    { "version": "0.2.0", "release": "v0.2.0", "interfaces": [],
                      "assets": [asset("tool-x-0.2.0.mur.zip", 'b')] },
                    { "version": "0.1.0", "release": "v0.1.0", "built_at_level": 1,
                      "interfaces": ["murmur:tool/run@0.1.0"],
                      "assets": [asset("tool-x-0.1.0.mur.zip", 'a')] }
                ]
            }
        ]
    })
}

fn merge_dir() -> TempDir {
    let dir = TempDir::new().unwrap();
    for version in ["0.2.0", "0.3.0"] {
        pack(
            dir.path(),
            &format!("tool-x-{version}.mur.zip"),
            &format!("name: tool-x\nversion: {version}\nruntime: tool\n"),
            &[(
                "capsule.wasm",
                &fixture_bytes("run/components/echo-tool.wasm"),
            )],
        );
    }
    dir
}

#[test]
fn merge_keeps_every_version_the_directory_does_not_hold() {
    let dir = merge_dir();
    let out = TempDir::new().unwrap();
    let old = old_index("ana");
    fs::write(
        out.path().join("old.json"),
        serde_json::to_string_pretty(&old).unwrap(),
    )
    .unwrap();
    let assert = mur_index(
        out.path(),
        &[
            dir.path().to_str().unwrap(),
            "--repo",
            "ana/r",
            "--release",
            "v0.3.0",
            "--merge",
            "old.json",
            "-o",
            "new.json",
        ],
    )
    .success();
    assert_eq!(
        stdout_of(&assert),
        "Indexed 2 artifacts (4 versions) for scope ana into new.json; 2 versions kept from \
         old.json, 1 replaced\n"
    );
    let path = out.path().join("new.json");
    assert_matches_published_schema(&path);
    let new: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    // Kept verbatim, but for the derived built-at level a version recording none is given.
    let mut gone = artifact(&old, "tool-gone").clone();
    gone["versions"][0]["built_at_level"] = json!(capsule_runtime::served_wit_level().level);
    assert_eq!(artifact(&new, "tool-gone"), &gone);
    let versions = artifact(&new, "tool-x")["versions"].as_array().unwrap();
    let listed: Vec<(&str, &str)> = versions
        .iter()
        .map(|v| {
            (
                v["version"].as_str().unwrap(),
                v["release"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        listed,
        [
            ("0.3.0", "v0.3.0"),
            ("0.2.0", "v0.3.0"),
            ("0.1.0", "v0.1.0")
        ]
    );
    assert_eq!(versions[2], artifact(&old, "tool-x")["versions"][1]);
    assert_eq!(
        versions[1]["assets"][0]["sha256"],
        sha256_of(&dir.path().join("tool-x-0.2.0.mur.zip"))
    );
}

#[test]
fn merge_refuses_an_index_it_cannot_use() {
    let dir = merge_dir();

    let schema_1 = TempDir::new().unwrap();
    let old = schema_1.path().join("old.json");
    fs::write(
        &old,
        r#"{"schema_version": "1", "updated_at": "2026-06-07T00:00:00Z", "artifacts": []}"#,
    )
    .unwrap();
    assert_merge_refused(
        dir.path(),
        &old,
        "E-IDX-002",
        &[
            "is a schema 1 index",
            "hint: this is a schema 1 artifact index",
            "mur index <dir> --repo <owner>/<repo> --release <tag>",
        ],
    );

    let bob = TempDir::new().unwrap();
    let old = bob.path().join("old.json");
    fs::write(&old, old_index("bob").to_string()).unwrap();
    assert_merge_refused(dir.path(), &old, "E-IDX-003", &["'bob'", "'ana'"]);

    let missing = TempDir::new().unwrap();
    let old = missing.path().join("no-such.json");
    assert_merge_refused(dir.path(), &old, "E-IO-003", &[old.to_str().unwrap()]);
}

fn assert_merge_refused(dir: &Path, merge: &Path, code: &str, needles: &[&str]) {
    let out = TempDir::new().unwrap();
    let assert = mur_index(
        out.path(),
        &[
            dir.to_str().unwrap(),
            "--repo",
            "ana/r",
            "--release",
            "v0.3.0",
            "--merge",
            merge.to_str().unwrap(),
            "-o",
            "new.json",
        ],
    )
    .failure();
    let stderr = stderr_of(&assert);
    assert!(stderr.contains(&format!("error[{code}]")), "{stderr}");
    for needle in needles {
        assert!(stderr.contains(needle), "missing {needle:?} in {stderr}");
    }
    assert!(entries_of(out.path()).is_empty());
}
