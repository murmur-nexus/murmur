//! `wit_contracts` in stored artifact metadata, end to end through `mur publish` and `mur list`.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;
use tempfile::TempDir;
use zip::{write::SimpleFileOptions, ZipWriter};

// ── Fixtures ──────────────────────────────────────────────────────────────────

fn fixture_bytes(relative: &str) -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(relative);
    fs::read(&path).unwrap_or_else(|err| panic!("missing fixture {}: {err}", path.display()))
}

/// A `.mur.zip` holding `murmur.yaml` plus the given entries.
fn pack(
    dir: &Path,
    name: &str,
    version: &str,
    runtime: &str,
    execution: &str,
    files: &[(&str, &[u8])],
) -> PathBuf {
    let artifact_path = dir.join(format!("{name}-{version}.mur.zip"));
    let mut zip = ZipWriter::new(fs::File::create(&artifact_path).unwrap());
    let options = SimpleFileOptions::default();

    zip.start_file("murmur.yaml", options).unwrap();
    writeln!(zip, "name: {name}").unwrap();
    writeln!(zip, "version: {version}").unwrap();
    writeln!(zip, "runtime: {runtime}").unwrap();
    writeln!(zip, "execution: {execution}").unwrap();

    for (entry, bytes) in files {
        zip.start_file(*entry, options).unwrap();
        zip.write_all(bytes).unwrap();
    }

    zip.finish().unwrap();
    artifact_path
}

fn publish(home: &TempDir, artifact_path: &Path) -> assert_cmd::assert::Assert {
    Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .args(["publish", artifact_path.to_str().unwrap()])
        .assert()
}

fn list(home: &TempDir, args: &[&str]) -> assert_cmd::assert::Assert {
    let mut cmd = Command::cargo_bin("mur").unwrap();
    cmd.env("HOME", home.path()).arg("list");
    for arg in args {
        cmd.arg(arg);
    }
    cmd.assert()
}

fn raw_metadata(home: &TempDir, name: &str, version: &str) -> String {
    fs::read_to_string(home.path().join(format!(
        ".murmur/artifacts/{name}/{version}/{name}-{version}.meta.json"
    )))
    .unwrap()
}

fn contracts(home: &TempDir, name: &str, version: &str) -> Value {
    let parsed: Value = serde_json::from_str(&raw_metadata(home, name, version)).unwrap();
    parsed["meta"]["wit_contracts"].clone()
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item.as_str().unwrap().to_string())
        .collect()
}

// ── Extraction ────────────────────────────────────────────────────────────────

#[test]
fn a_published_tool_records_the_interface_it_exports() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let artifact = pack(
        work.path(),
        "contract-tool",
        "0.1.0",
        "tool",
        "wasm",
        &[("tool.wasm", &fixture_bytes("run/components/echo-tool.wasm"))],
    );

    publish(&home, &artifact).success();

    let contracts = contracts(&home, "contract-tool", "0.1.0");
    assert!(strings(&contracts["exports"]).contains(&"murmur:tool/run@0.1.0".to_string()));
}

#[test]
fn a_capsule_records_both_directions_sorted_and_deduplicated() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let artifact = pack(
        work.path(),
        "contract-capsule",
        "0.1.0",
        "driver",
        "wasm",
        &[(
            "capsule.wasm",
            &fixture_bytes("graduation/capsule/capsule.wasm"),
        )],
    );

    publish(&home, &artifact).success();

    let contracts = contracts(&home, "contract-capsule", "0.1.0");
    let exports = strings(&contracts["exports"]);
    let imports = strings(&contracts["imports"]);

    assert!(exports.contains(&"murmur:capsule/run@0.1.0".to_string()));
    assert!(imports.contains(&"murmur:tool-registry/invoke@0.1.0".to_string()));

    for names in [&exports, &imports] {
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(names, &sorted);
        assert!(names.iter().all(|name| name.contains('/')));
    }
}

#[test]
fn a_retired_interface_version_is_recorded_rather_than_refused() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let component = wat::parse_str(
        r#"
        (component
          (core module $m (func (export "on-event")))
          (core instance $i (instantiate $m))
          (func $on_event (canon lift (core func $i "on-event")))
          (instance $iface (export "on-event" (func $on_event)))
          (export "murmur:hook/lifecycle@0.5.0" (instance $iface))
        )
        "#,
    )
    .unwrap();
    let artifact = pack(
        work.path(),
        "retired-hook",
        "0.1.0",
        "hook",
        "wasm",
        &[("hook.wasm", &component)],
    );

    publish(&home, &artifact)
        .success()
        .stderr(predicate::str::is_empty());

    let contracts = contracts(&home, "retired-hook", "0.1.0");
    assert_eq!(
        strings(&contracts["exports"]),
        vec!["murmur:hook/lifecycle@0.5.0"]
    );
}

#[test]
fn payloads_with_no_component_omit_the_key() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();

    let cases: Vec<(&str, PathBuf)> = vec![
        (
            "contract-skill",
            pack(
                work.path(),
                "contract-skill",
                "0.1.0",
                "skill",
                "static",
                &[("skill.md", b"# guidance")],
            ),
        ),
        (
            "contract-native",
            pack(
                work.path(),
                "contract-native",
                "0.1.0",
                "tool",
                "native",
                &[("bin/contract-native", b"\x7fELF")],
            ),
        ),
        (
            "contract-stub",
            pack(
                work.path(),
                "contract-stub",
                "0.1.0",
                "tool",
                "wasm",
                &[("tool.wasm", b"\0asm")],
            ),
        ),
        (
            "contract-garbage",
            pack(
                work.path(),
                "contract-garbage",
                "0.1.0",
                "tool",
                "wasm",
                &[("tool.wasm", &fixture_bytes("happy/tool.wasm"))],
            ),
        ),
        (
            "contract-module",
            pack(
                work.path(),
                "contract-module",
                "0.1.0",
                "tool",
                "wasm",
                &[(
                    "tool.wasm",
                    &wat::parse_str(r#"(module (func (export "f")))"#).unwrap(),
                )],
            ),
        ),
    ];

    for (name, artifact) in cases {
        publish(&home, &artifact).success();
        let raw = raw_metadata(&home, name, "0.1.0");
        assert!(
            !raw.contains("wit_contracts"),
            "{name} recorded a wit_contracts key: {raw}"
        );
    }
}

// ── mur list --contract ───────────────────────────────────────────────────────

fn store_with_three_artifacts() -> TempDir {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();

    publish(
        &home,
        &pack(
            work.path(),
            "contract-tool",
            "0.1.0",
            "tool",
            "wasm",
            &[("tool.wasm", &fixture_bytes("run/components/echo-tool.wasm"))],
        ),
    )
    .success();
    publish(
        &home,
        &pack(
            work.path(),
            "contract-capsule",
            "0.1.0",
            "driver",
            "wasm",
            &[(
                "capsule.wasm",
                &fixture_bytes("graduation/capsule/capsule.wasm"),
            )],
        ),
    )
    .success();
    publish(
        &home,
        &pack(
            work.path(),
            "contract-skill",
            "0.1.0",
            "skill",
            "static",
            &[("skill.md", b"# guidance")],
        ),
    )
    .success();

    home
}

#[test]
fn contract_filter_selects_artifacts_touching_the_package_in_either_direction() {
    let home = store_with_three_artifacts();

    list(&home, &["-g", "--contract", "murmur:tool"])
        .success()
        .stdout(predicate::str::contains("CONTRACTS"))
        .stdout(predicate::str::contains("contract-tool"))
        .stdout(predicate::str::contains("murmur:tool/run@0.1.0"))
        .stdout(predicate::str::contains("contract-capsule"))
        .stdout(predicate::str::contains(
            "murmur:tool-registry/invoke@0.1.0",
        ))
        .stdout(predicate::str::contains("contract-skill").not());
}

#[test]
fn contract_filter_with_no_match_prints_the_no_artifacts_message() {
    let home = store_with_three_artifacts();

    list(&home, &["-g", "--contract", "murmur:hook"])
        .success()
        .stdout(predicate::str::contains("No artifacts found."))
        .stdout(predicate::str::contains("CONTRACTS").not());
}

#[test]
fn the_default_listing_carries_no_contracts_column() {
    let home = store_with_three_artifacts();

    list(&home, &["-g"])
        .success()
        .stdout(predicate::str::contains("NAME"))
        .stdout(predicate::str::contains("PLATFORMS"))
        .stdout(predicate::str::contains("CONTRACTS").not());
}
