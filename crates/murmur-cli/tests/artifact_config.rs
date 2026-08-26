//! Operator-authored per-artifact configuration: `config:` on an artifact entry, lowered to JSON
//! and delivered to that artifact alone as `MURMUR_ARTIFACT_CONFIG`.
//!
//! Every test here drives the real `mur` binary against a real Wasmtime guest, because the whole
//! point of the channel is what one guest's environment holds and another's does not: an
//! in-process assertion about a `WasiCtx` cannot tell you what a tool's `std::env::var` actually
//! returns, and that resolution is the mechanism.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;
use zip::{
    write::{FileOptions, SimpleFileOptions},
    CompressionMethod, ZipWriter,
};

const TOOL_NAME: &str = "config-echo";
/// Second artifact name the capsule fixture also invokes, installed from the same component, so
/// the scoping scenario can declare two entries that differ only in their `config:` block.
const SECOND_TOOL_NAME: &str = "config-echo-b";
const TOOL_VERSION: &str = "0.1.0";
const CAPSULE_NAME: &str = "config-capsule";

/// The block the happy path declares, and the JSON it must arrive as.
const SAMPLE_CONFIG: &str = "
      types:
        utterance:
          schema: { type: object, required: [text] }
      read_recent: { default: 20, max: 100 }";

/// One artifact entry, with `config_yaml` spliced in under `config:`. `None` produces an entry
/// with no `config:` key at all — the default, not an empty block.
fn artifact_entry(name: &str, config_yaml: Option<&str>) -> String {
    let config = config_yaml
        .map(|yaml| format!("    config:{yaml}\n"))
        .unwrap_or_default();
    format!("  - name: {name}\n    version: {TOOL_VERSION}\n    runtime: tool\n{config}")
}

/// A project whose capsule invokes the config-reporting tools and publishes what they reported.
fn config_project(project: &Path, entries: &[String]) -> PathBuf {
    fs::write(
        project.join("murmur.yaml"),
        format!(
            "name: {CAPSULE_NAME}\nversion: 0.0.1\nartifacts:\n{}",
            entries.concat()
        ),
    )
    .unwrap();

    fs::copy(
        fixture_component("capsule-config-echo.wasm"),
        project.join("capsule.wasm"),
    )
    .unwrap();

    project.join("murmur.yaml")
}

/// The single-tool shape every scenario but the scoping one uses.
fn one_tool_project(project: &Path, config_yaml: Option<&str>) -> PathBuf {
    config_project(project, &[artifact_entry(TOOL_NAME, config_yaml)])
}

/// Install the config-reporting tool into `project`'s own artifact store, under `name`.
fn install_config_tool(fixture: &Path, project: &Path, name: &str) {
    let artifact = fixture.join(format!("{name}-{TOOL_VERSION}.mur.zip"));
    let mut zip = ZipWriter::new(fs::File::create(&artifact).unwrap());
    let options: SimpleFileOptions =
        FileOptions::default().compression_method(CompressionMethod::Deflated);

    zip.start_file("murmur.yaml", options).unwrap();
    writeln!(zip, "name: {name}").unwrap();
    writeln!(zip, "version: {TOOL_VERSION}").unwrap();
    writeln!(zip, "runtime: wasm").unwrap();

    zip.start_file("tool.wasm", options).unwrap();
    zip.write_all(&fs::read(fixture_component("config-echo.wasm")).unwrap())
        .unwrap();
    zip.finish().unwrap();

    common::install_artifact_to_project(project, &artifact).success();
}

fn fixture_component(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("run")
        .join("components")
        .join(name)
}

/// What the capsule published for one tool: the `MURMUR_ARTIFACT_CONFIG` that tool observed, or
/// the literal `absent` when it observed none.
fn published(stdout: &str, file: &str) -> String {
    let workdir = common::parse_workdir_from_stdout(stdout);
    fs::read_to_string(workdir.join("out").join(file)).unwrap()
}

fn run_and_read(home: &TempDir, manifest: &Path) -> String {
    let stdout = common::run_capsule(home, manifest)
        .success()
        .get_output()
        .stdout
        .clone();
    published(&String::from_utf8(stdout).unwrap(), "result.txt")
}

fn explain_scope(home: &TempDir, manifest: &Path) -> Command {
    let mut cmd = Command::cargo_bin("mur").unwrap();
    cmd.env("HOME", home.path()).env_remove("NEXUS_API_KEY");
    cmd.args(["run", "--manifest", manifest.to_str().unwrap()]);
    cmd.arg("--explain-scope");
    cmd
}

/// The whole point of the channel: an operator writes a block in their own manifest and the tool
/// reads it as JSON — integers still integers, sequences still sequences, nested mappings still
/// objects. Compared as a `serde_json::Value` so the assertion is about the data and not about
/// key order.
#[test]
fn a_configured_tool_reads_its_block_as_json() {
    let home = tempfile::tempdir().unwrap();
    let fixture = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let manifest = one_tool_project(project.path(), Some(SAMPLE_CONFIG));
    install_config_tool(fixture.path(), project.path(), TOOL_NAME);

    let observed = run_and_read(&home, &manifest);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&observed)
            .unwrap_or_else(|err| panic!("the tool must observe JSON, got {observed:?}: {err}")),
        serde_json::json!({
            "types": {"utterance": {"schema": {"required": ["text"], "type": "object"}}},
            "read_recent": {"default": 20, "max": 100},
        })
    );
}

/// Scoping, both halves at once: two entries declaring their own blocks, and neither tool can see
/// the other's. A session-wide variable would make both reads identical.
#[test]
fn each_tool_sees_only_its_own_config() {
    let home = tempfile::tempdir().unwrap();
    let fixture = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let manifest = config_project(
        project.path(),
        &[
            artifact_entry(TOOL_NAME, Some("\n      who: a")),
            artifact_entry(SECOND_TOOL_NAME, Some("\n      who: b")),
        ],
    );
    install_config_tool(fixture.path(), project.path(), TOOL_NAME);
    install_config_tool(fixture.path(), project.path(), SECOND_TOOL_NAME);

    let stdout = common::run_capsule(&home, &manifest)
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(stdout).unwrap();

    assert_eq!(published(&stdout, "result.txt"), r#"{"who":"a"}"#);
    assert_eq!(published(&stdout, "result-b.txt"), r#"{"who":"b"}"#);
}

/// Config declared on one entry leaves the other with no variable at all — `absent`, not an empty
/// JSON object. The distinction is what lets an artifact tell "not configured" from "configured
/// with nothing".
#[test]
fn an_undeclared_entry_beside_a_declared_one_reports_absent() {
    let home = tempfile::tempdir().unwrap();
    let fixture = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let manifest = config_project(
        project.path(),
        &[
            artifact_entry(TOOL_NAME, Some("\n      who: a")),
            artifact_entry(SECOND_TOOL_NAME, None),
        ],
    );
    install_config_tool(fixture.path(), project.path(), TOOL_NAME);
    install_config_tool(fixture.path(), project.path(), SECOND_TOOL_NAME);

    let stdout = common::run_capsule(&home, &manifest)
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(stdout).unwrap();

    assert_eq!(published(&stdout, "result.txt"), r#"{"who":"a"}"#);
    assert_eq!(published(&stdout, "result-b.txt"), "absent");
}

/// Absent declaration, absent behaviour change: the run succeeds, the tool observes nothing, and
/// the diagnostic reports an empty list in both renderings.
#[test]
fn an_entry_declaring_nothing_behaves_exactly_as_before() {
    let home = tempfile::tempdir().unwrap();
    let fixture = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let manifest = one_tool_project(project.path(), None);
    install_config_tool(fixture.path(), project.path(), TOOL_NAME);

    assert_eq!(run_and_read(&home, &manifest), "absent");

    explain_scope(&home, &manifest)
        .assert()
        .success()
        .stdout(predicate::str::contains("artifact config: <none>"));
    assert_eq!(
        common::explain_scope_json(&home, &manifest)["configured_artifacts"],
        serde_json::json!([])
    );
}

/// Five shapes the channel cannot carry, each refused by the same code, naming the artifact and
/// the rule — before any session workdir exists, which is what proves the refusal precedes
/// staging rather than following it.
#[test]
fn a_malformed_config_refuses_the_launch_and_leaves_no_workdir() {
    let home = tempfile::tempdir().unwrap();
    let fixture = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    // A well-formed manifest first, so `mur install` can find the project root; each iteration
    // then rewrites only the `config:` block.
    one_tool_project(project.path(), Some("\n      who: a"));
    install_config_tool(fixture.path(), project.path(), TOOL_NAME);

    let oversized = format!("\n      blob: {}", "x".repeat(70_000));
    for (config, rule) in [
        (" 7", "must be a mapping"),
        (" [a, b]", "must be a mapping"),
        ("", "must be a mapping"),
        ("\n      1: x", "must be a string"),
        (oversized.as_str(), "over the 65536-byte limit"),
    ] {
        let manifest = one_tool_project(project.path(), Some(config));

        common::run_capsule(&home, &manifest)
            .failure()
            .stderr(predicate::str::contains("error[E-CAP-010]"))
            .stderr(predicate::str::contains(TOOL_NAME))
            .stderr(predicate::str::contains(rule));

        assert!(
            !project.path().join("workdir").exists(),
            "'{config}' must be refused before any session workdir is created"
        );
    }

    // The same refusal reaches the diagnostic: a report that describes a launch refuses whatever
    // the launch refuses, rather than telling an operator their manifest is fine.
    let manifest = one_tool_project(project.path(), Some(" 7"));
    explain_scope(&home, &manifest)
        .assert()
        .failure()
        .stderr(predicate::str::contains("error[E-CAP-010]"));
    assert!(!project.path().join("workdir").exists());
}

/// The oversized case names the actual size as well as the cap, so the operator knows how far
/// over they are rather than only that they are over.
#[test]
fn an_oversized_config_names_the_size_it_serialized_to() {
    let home = tempfile::tempdir().unwrap();
    let fixture = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    one_tool_project(project.path(), Some("\n      who: a"));
    install_config_tool(fixture.path(), project.path(), TOOL_NAME);
    let manifest = one_tool_project(
        project.path(),
        Some(&format!("\n      blob: {}", "x".repeat(70_000))),
    );

    common::run_capsule(&home, &manifest)
        .failure()
        .stderr(predicate::str::contains("error[E-CAP-010]"))
        .stderr(predicate::str::contains("70011 bytes"))
        .stderr(predicate::str::contains("65536"));
}

/// A skill runs no component and holds no grant, so nothing would deliver a block to one. The
/// manifest parser refuses it outright, which is strictly stronger than a warning.
#[test]
fn config_on_a_skill_entry_is_refused_at_parse_time() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    fs::write(
        project.path().join("murmur.yaml"),
        format!(
            "name: {CAPSULE_NAME}\nversion: 0.0.1\nartifacts:\n  - name: notes-skill\n    \
             version: 1.0.0\n    runtime: skill\n    config:\n      who: me\n"
        ),
    )
    .unwrap();
    fs::copy(
        fixture_component("capsule-config-echo.wasm"),
        project.path().join("capsule.wasm"),
    )
    .unwrap();

    common::run_capsule(&home, &project.path().join("murmur.yaml"))
        .failure()
        .stderr(predicate::str::contains("notes-skill"))
        .stderr(predicate::str::contains("runtime: skill"))
        .stderr(predicate::str::contains("runtime: hook"));
}

/// Config is delivered on one artifact's own grant, so a capsule-wide block would reach nothing.
/// Refused with a message pointing at the artifact entry, never silently ignored.
#[test]
fn a_capsule_wide_config_block_is_refused_at_parse_time() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    fs::write(
        project.path().join("murmur.yaml"),
        format!(
            "name: {CAPSULE_NAME}\nversion: 0.0.1\nconfig:\n  who: capsule\nartifacts:\n  - name: \
             {TOOL_NAME}\n    version: {TOOL_VERSION}\n    runtime: tool\n"
        ),
    )
    .unwrap();
    fs::copy(
        fixture_component("capsule-config-echo.wasm"),
        project.path().join("capsule.wasm"),
    )
    .unwrap();

    common::run_capsule(&home, &project.path().join("murmur.yaml"))
        .failure()
        .stderr(predicate::str::contains("declared per artifact"));
}

/// The declaration is reported by name in both renderings, nothing from inside it reaches either,
/// and declaring it moves no other field of the report.
#[test]
fn the_declaration_is_reported_by_name_and_moves_nothing_else() {
    let home = tempfile::tempdir().unwrap();
    let fixture = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let manifest = one_tool_project(project.path(), Some(SAMPLE_CONFIG));
    install_config_tool(fixture.path(), project.path(), TOOL_NAME);

    let declared = common::explain_scope_json(&home, &manifest);
    assert_eq!(
        declared["configured_artifacts"],
        serde_json::json!([TOOL_NAME])
    );
    // `read_recent` is a key only this manifest's block carries, so its absence is evidence that
    // nothing from inside the block reached the report.
    assert!(
        !declared.to_string().contains("read_recent"),
        "the report must carry no value from inside a config block: {declared}"
    );

    let rendered = explain_scope(&home, &manifest)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let rendered = String::from_utf8(rendered).unwrap();
    assert!(
        rendered.contains("artifact config:") && rendered.contains(&format!("- {TOOL_NAME}")),
        "the rendering must name the configured artifact: {rendered}"
    );
    assert!(
        !rendered.contains("read_recent"),
        "the rendering must carry no value from inside a config block: {rendered}"
    );

    // Same manifest with the block deleted: every containment field is identical, so declaring
    // config is proven to move nothing but its own key.
    let undeclared = common::explain_scope_json(&home, &one_tool_project(project.path(), None));
    assert_eq!(undeclared["configured_artifacts"], serde_json::json!([]));
    for field in [
        "declared_containment",
        "achieved_containment",
        "floor_met",
        "enforcement_tier",
    ] {
        assert_eq!(
            declared[field], undeclared[field],
            "declaring config moved '{field}'"
        );
    }
}

/// `MURMUR_ARTIFACT_CONFIG` is runtime-owned: allowlisting the name cannot make a host value
/// reach a guest, whether or not that guest's entry also declares a block. Both halves are
/// asserted together, because either alone would be consistent with the other cause.
#[test]
fn the_host_value_never_reaches_a_guest_however_a_manifest_allowlists_it() {
    let home = tempfile::tempdir().unwrap();
    let fixture = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let allowlist = "\n    capabilities:\n      env:\n        allow:\n          \
                     - MURMUR_ARTIFACT_CONFIG";
    let entry = |name: &str, config: Option<&str>| {
        let mut entry = artifact_entry(name, config);
        entry.push_str(allowlist);
        entry.push('\n');
        entry
    };
    let manifest = config_project(
        project.path(),
        &[
            entry(TOOL_NAME, Some("\n      who: manifest")),
            entry(SECOND_TOOL_NAME, None),
        ],
    );
    install_config_tool(fixture.path(), project.path(), TOOL_NAME);
    install_config_tool(fixture.path(), project.path(), SECOND_TOOL_NAME);

    let stdout = common::run_capsule_with_env(
        &home,
        &manifest,
        &[("MURMUR_ARTIFACT_CONFIG", r#"{"who":"host"}"#)],
    )
    .success()
    .get_output()
    .stdout
    .clone();
    let stdout = String::from_utf8(stdout).unwrap();

    assert_eq!(published(&stdout, "result.txt"), r#"{"who":"manifest"}"#);
    assert_eq!(
        published(&stdout, "result-b.txt"),
        "absent",
        "an allowlisted name must not pull the host value into an unconfigured guest"
    );
}

/// A native tool is a host subprocess under the capsule-wide shell environment, which is not
/// per-artifact — a `config:` block there is inert. Warned rather than refused, on the same terms
/// `capabilities:` on a native tool is, and the launch still succeeds.
///
/// Config is delivered only in the per-artifact WASI environment built at tool dispatch, and a
/// native tool never reaches that path at all: it is a host subprocess, so there is no per-artifact
/// environment for the runtime to write into. What the capsule publishes therefore carries nothing
/// from inside the block, which is asserted below rather than assumed.
#[test]
fn config_on_a_native_tool_warns_and_is_not_delivered() {
    let home = tempfile::tempdir().unwrap();
    let fixture = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let script = "#!/bin/sh\nprintf '{\"summary\":\"%s\",\"status\":\"passed\"}' \
                  \"${MURMUR_ARTIFACT_CONFIG:-absent}\"\n";
    let artifact = common::create_native_artifact(
        fixture.path(),
        TOOL_NAME,
        TOOL_VERSION,
        script,
        Some("Reports the artifact config it was handed."),
        None,
    );

    let manifest = one_tool_project(project.path(), Some("\n      who: a"));
    common::install_artifact_to_project(project.path(), &artifact).success();

    let assertion = common::run_capsule(&home, &manifest)
        .success()
        .stderr(predicate::str::contains("warning[W-SEC-015]"))
        .stderr(predicate::str::contains(TOOL_NAME))
        .stderr(predicate::str::contains("host subprocess"));

    let stdout = String::from_utf8(assertion.get_output().stdout.clone()).unwrap();
    let observed = published(&stdout, "result.txt");
    assert!(
        !observed.contains("who"),
        "nothing from inside the block may come back from a native tool: {observed}"
    );

    // The declaration is still reported — it is what the operator wrote — and the warning is what
    // says it reaches nothing.
    assert_eq!(
        common::explain_scope_json(&home, &manifest)["configured_artifacts"],
        serde_json::json!([TOOL_NAME])
    );
}
