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

/// Full name of the config-reporting tool, installed from a `.mur.zip` file and so in the `local`
/// scope; the capsule fixture invokes it by its bare part.
const TOOL_NAME: &str = "local/config-echo";
/// Second artifact name the capsule fixture also invokes, installed from the same component, so
/// the scoping scenario can declare two entries that differ only in their `config:` block.
const SECOND_TOOL_NAME: &str = "local/config-echo-b";
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

/// Install the config-reporting tool into `project`'s own artifact store, under the full `name`;
/// the zip is packed under its bare part.
fn install_config_tool(fixture: &Path, project: &Path, name: &str) {
    let name = murmur_artifact::tool_name_of(name);
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
    cmd.env("HOME", home.path());
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
/// the rule — before any work folder exists, which is what proves the refusal precedes
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
            "'{config}' must be refused before any work folder is created"
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
            "name: {CAPSULE_NAME}\nversion: 0.0.1\nartifacts:\n  - name: local/notes-skill\n    \
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

// ── Native tools ────────────────────────────────────────────────────────────────────
//
// A native tool is dispatched by the agent loop, never by a script capsule's `tool-registry`
// invoke, so these scenarios run an agent capsule on a scripted model: each tool is called once,
// in entry order, and what it reported is read back from the tool result the model was sent.

const DRIVER: &str = "local/murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.0";

/// The retired inert-config warning code, spelled so the tree's only literal of it is the
/// retirement comment in `security_warnings.rs`.
const RETIRED_INERT_CONFIG_CODE: &str = concat!("W-SEC-", "015");

/// A native tool's script: drain stdin with a builtin, run `body`, which sets `$v`, then print
/// `$v` between `<<<` and `>>>` as the result's summary, escaped for a JSON string. Builtins only:
/// on the Landlock tiers a native tool can execute nothing `shell.allow` does not name.
fn native_script(body: &str) -> String {
    format!(
        "#!/bin/bash\nwhile read -r _; do :; done\n{body}\nv=${{v//\\\\/\\\\\\\\}}\nv=${{v//\\\"/\\\\\\\"}}\n\
         printf '{{\"status\":\"passed\",\"summary\":\"<<<%s>>>\"}}' \"$v\"\n"
    )
}

/// Reports `MURMUR_ARTIFACT_CONFIG`, or `absent`.
const ECHO_CONFIG: &str = "v=${MURMUR_ARTIFACT_CONFIG:-absent}";

/// Reports `<MURMUR_ARTIFACT_CONFIG>|<MURMUR_STATE_DIR>`, each `absent` when unset.
const ECHO_BOTH: &str = "v=\"${MURMUR_ARTIFACT_CONFIG:-absent}|${MURMUR_STATE_DIR:-absent}\"";

/// Appends a line to `$MURMUR_STATE_DIR/runs` and reports `<line count> <store>`.
const COUNT_RUNS: &str = "if printf 'run\\n' >> \"$MURMUR_STATE_DIR/runs\"; then\n  n=0\n  \
                          while read -r _; do n=$((n+1)); done < \"$MURMUR_STATE_DIR/runs\"\n  \
                          v=\"$n $MURMUR_STATE_DIR\"\nelse\n  v=append-failed\nfi";

/// Writes `$MURMUR_STATE_DIR/secret` and reports whether it could.
const WRITE_SECRET: &str = "if printf 'the-secret\\n' > \"$MURMUR_STATE_DIR/secret\"; then \
                            v=wrote; else v=write-failed; fi";

/// `capabilities.state` on an artifact entry, with the capsule-name default.
const STATE: &str = "\n      state: {}";

/// The capsule-wide `containment: sealed` line.
const SEALED: &str = "  containment: sealed\n";

/// An artifact entry with `capabilities_yaml` spliced in under `capabilities:`.
fn entry_with_capabilities(
    name: &str,
    config_yaml: Option<&str>,
    capabilities_yaml: Option<&str>,
) -> String {
    let mut entry = artifact_entry(name, config_yaml);
    if let Some(capabilities) = capabilities_yaml {
        entry.push_str(&format!("    capabilities:{capabilities}\n"));
    }
    entry
}

/// Whether this host reaches `sealed`, printing a `[SKIP-HOST]` line when it does not.
fn host_reaches_sealed(test: &str) -> bool {
    let (decision, shortfall) =
        capsule_runtime::detect_containment_decision(murmur_artifact::ContainmentClass::Sealed);
    if decision.achieved() == murmur_artifact::ContainmentClass::Sealed {
        return true;
    }
    eprintln!(
        "[SKIP-HOST] {test}: this host cannot back sealed ({})",
        shortfall
            .map(|shortfall| shortfall.reason())
            .unwrap_or_else(|| format!("it reaches {}", decision.achieved()))
    );
    false
}

fn end_turn_response() -> String {
    serde_json::json!({
        "id": "msg_end",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": "done"}],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {"input_tokens": 1, "output_tokens": 1},
    })
    .to_string()
}

/// An agent capsule named [`CAPSULE_NAME`] on a scripted model, declaring `entries` beside its
/// driver, with `capabilities_yaml` (indented two spaces) added to its capsule-wide block.
struct NativeCapsule {
    home: TempDir,
    project: TempDir,
    fixture: TempDir,
    entries: Vec<String>,
    capabilities_yaml: String,
}

impl NativeCapsule {
    /// Writes the manifest and installs the driver and one native tool per `(name, body)`.
    fn new(entries: Vec<String>, capabilities_yaml: &str, tools: &[(&str, &str)]) -> Self {
        let capsule = Self {
            home: tempfile::tempdir().unwrap(),
            project: tempfile::tempdir().unwrap(),
            fixture: tempfile::tempdir().unwrap(),
            entries,
            capabilities_yaml: capabilities_yaml.to_string(),
        };
        // An endpoint nothing listens on, so `mur install` finds a project; every run rewrites it.
        capsule.write_manifest("http://127.0.0.1:9");
        let driver = common::create_driver_artifact(
            capsule.fixture.path(),
            DRIVER,
            DRIVER_VERSION,
            &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
        );
        common::install_artifact_to_project_with_home(
            capsule.project.path(),
            &capsule.home,
            &driver,
            &["--no-precompile"],
        )
        .success();
        for (name, body) in tools {
            let artifact = common::create_native_artifact(
                capsule.fixture.path(),
                name,
                TOOL_VERSION,
                &native_script(body),
                Some("Reports what its own entry handed it."),
                None,
            );
            common::install_artifact_to_project_with_home(
                capsule.project.path(),
                &capsule.home,
                &artifact,
                &[],
            )
            .success();
        }
        capsule
    }

    fn manifest(&self) -> PathBuf {
        self.project.path().join("murmur.yaml")
    }

    fn write_manifest(&self, endpoint: &str) {
        fs::write(
            self.manifest(),
            format!(
                "name: {CAPSULE_NAME}\nversion: 0.0.1\nartifacts:\n  - name: {DRIVER}\n    version: \
                 {DRIVER_VERSION}\n    runtime: driver\n    gateway:\n      endpoint: {endpoint}\n      \
                 api_key: test-key\n{}capabilities:\n  network:\n    allow:\n      - {endpoint}\n{}\
                 inference:\n  transport: http\n  model: test-model\n  driver:\n    artifact: {DRIVER}\n",
                self.entries.concat(),
                self.capabilities_yaml,
            ),
        )
        .unwrap();
    }

    /// One `mur run --task` calling each tool in `tools` once, in order, with `env` set on `mur`
    /// itself; required to finish `ok`. Returns what each tool reported, then `mur`'s stderr.
    fn run(&self, tools: &[&str], env: &[(&str, &str)]) -> (Vec<String>, String) {
        let mut responses: Vec<String> = tools
            .iter()
            .enumerate()
            .map(|(index, tool)| {
                common::tool_use_response(&format!("call_{index}"), tool, serde_json::json!({}))
            })
            .collect();
        responses.push(end_turn_response());
        let server = common::ScriptedServer::start(responses);
        self.write_manifest(&server.endpoint);

        let mut cmd = Command::cargo_bin("mur").unwrap();
        cmd.env("HOME", self.home.path())
            .current_dir(self.project.path());
        for (key, value) in env {
            cmd.env(key, value);
        }
        let output = cmd
            .args([
                "run",
                "--manifest",
                self.manifest().to_str().unwrap(),
                "--task",
                "Run each tool once.",
                "--verbose",
            ])
            .timeout(std::time::Duration::from_secs(300))
            .assert()
            .success()
            .stdout(predicate::str::contains("status:  ok"))
            .get_output()
            .clone();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

        let requests = server.requests();
        let reported = (0..tools.len())
            .map(|index| {
                let id = format!("call_{index}");
                let text = common::tool_result_text(&requests, &id)
                    .unwrap_or_else(|| panic!("no tool result for {id}; stderr: {stderr}"));
                let start = text
                    .find("<<<")
                    .unwrap_or_else(|| panic!("{id} reported nothing: {text}"));
                let end = text
                    .rfind(">>>")
                    .unwrap_or_else(|| panic!("{id} reported nothing: {text}"));
                text[start + 3..end].to_string()
            })
            .collect();
        (reported, stderr)
    }

    fn store(&self) -> PathBuf {
        self.home.path().join(".murmur/state").join(CAPSULE_NAME)
    }
}

/// The bare tool name the model calls `name` by.
fn tool(name: &str) -> &str {
    murmur_artifact::tool_name_of(name)
}

fn reads_its_config_block(test: &str, capabilities_yaml: &str) {
    let capsule = NativeCapsule::new(
        vec![artifact_entry(TOOL_NAME, Some(SAMPLE_CONFIG))],
        capabilities_yaml,
        &[(TOOL_NAME, ECHO_CONFIG)],
    );

    let (reported, stderr) = capsule.run(&[tool(TOOL_NAME)], &[]);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&reported[0]).unwrap_or_else(|err| panic!(
            "{test}: the tool must observe JSON, got {:?}: {err}",
            reported[0]
        )),
        serde_json::json!({
            "types": {"utterance": {"schema": {"required": ["text"], "type": "object"}}},
            "read_recent": {"default": 20, "max": 100},
        })
    );
    assert!(!stderr.contains(RETIRED_INERT_CONFIG_CODE), "{stderr}");
}

/// A native tool reads its entry's block from `MURMUR_ARTIFACT_CONFIG` as the same JSON a WASM
/// tool reads, with no warning.
#[test]
fn a_native_tool_reads_its_config_block_as_json() {
    const TEST: &str = "a_native_tool_reads_its_config_block_as_json";
    if capsule_runtime::skip_without_egress_namespace(TEST) {
        return;
    }
    reads_its_config_block(TEST, "");
}

/// The same under `sealed`, where the variable crosses into the composed root.
#[test]
fn a_native_tool_reads_its_config_block_as_json_when_sealed() {
    const TEST: &str = "a_native_tool_reads_its_config_block_as_json_when_sealed";
    if capsule_runtime::skip_without_egress_namespace(TEST) || !host_reaches_sealed(TEST) {
        return;
    }
    reads_its_config_block(TEST, SEALED);
}

/// Neither runtime-owned name comes from the host: with both in `mur run`'s environment and in
/// `shell.baseline_env`, the configured tool sees its own block and no store, and the unconfigured
/// one sees neither variable.
#[test]
fn a_native_tool_without_config_gets_no_variable() {
    const TEST: &str = "a_native_tool_without_config_gets_no_variable";
    if capsule_runtime::skip_without_egress_namespace(TEST) {
        return;
    }
    let capsule = NativeCapsule::new(
        vec![
            artifact_entry(TOOL_NAME, Some("\n      who: manifest")),
            artifact_entry(SECOND_TOOL_NAME, None),
        ],
        "  shell:\n    allow: [true]\n    baseline_env: [MURMUR_ARTIFACT_CONFIG, MURMUR_STATE_DIR]\n",
        &[(TOOL_NAME, ECHO_BOTH), (SECOND_TOOL_NAME, ECHO_BOTH)],
    );

    let (reported, _) = capsule.run(
        &[tool(TOOL_NAME), tool(SECOND_TOOL_NAME)],
        &[
            ("MURMUR_ARTIFACT_CONFIG", r#"{"who":"host"}"#),
            ("MURMUR_STATE_DIR", "/host/state"),
        ],
    );

    assert_eq!(reported[0], r#"{"who":"manifest"}|absent"#);
    assert_eq!(reported[1], "absent|absent");
}

/// A block the channel cannot carry refuses the launch on a native entry exactly as on a WASM
/// one, before any work folder exists.
#[test]
fn a_malformed_config_on_a_native_tool_refuses_the_launch() {
    let home = tempfile::tempdir().unwrap();
    let fixture = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    // A well-formed manifest first, so `mur install` can find the project root.
    one_tool_project(project.path(), Some("\n      who: a"));
    let artifact = common::create_native_artifact(
        fixture.path(),
        TOOL_NAME,
        TOOL_VERSION,
        &native_script(ECHO_CONFIG),
        None,
        None,
    );
    common::install_artifact_to_project(project.path(), &artifact).success();

    let manifest = one_tool_project(project.path(), Some(" 7"));
    common::run_capsule(&home, &manifest)
        .failure()
        .stderr(predicate::str::contains("error[E-CAP-010]"))
        .stderr(predicate::str::contains(TOOL_NAME))
        .stderr(predicate::str::contains("must be a mapping"));
    assert!(
        !project.path().join("workdir").exists(),
        "the refusal must come before any work folder is created"
    );
}

fn store_outlives_the_session(test: &str, capabilities_yaml: &str) {
    let capsule = NativeCapsule::new(
        vec![entry_with_capabilities(TOOL_NAME, None, Some(STATE))],
        capabilities_yaml,
        &[(TOOL_NAME, COUNT_RUNS)],
    );
    let store = capsule.store();

    let (first, _) = capsule.run(&[tool(TOOL_NAME)], &[]);
    assert_eq!(first[0], format!("1 {}", store.display()), "{test}");
    let (second, _) = capsule.run(&[tool(TOOL_NAME)], &[]);
    assert_eq!(
        second[0],
        format!("2 {}", store.display()),
        "{test}: the store must outlive the session"
    );
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        fs::metadata(&store).unwrap().permissions().mode() & 0o777,
        0o700,
        "{test}"
    );

    assert_eq!(
        common::explain_scope_json(&capsule.home, &capsule.manifest())["state_stores"],
        serde_json::json!([{
            "artifact": TOOL_NAME,
            "store": CAPSULE_NAME,
            "host_path": store.display().to_string(),
        }]),
        "{test}"
    );
}

/// `capabilities.state` on a native tool's entry hands it `~/.murmur/state/<capsule>/` as
/// `MURMUR_STATE_DIR`, and what it writes there is there in the next session.
#[test]
fn a_native_tools_store_outlives_the_session() {
    const TEST: &str = "a_native_tools_store_outlives_the_session";
    if capsule_runtime::skip_without_egress_namespace(TEST) {
        return;
    }
    store_outlives_the_session(TEST, "");
}

/// The same under `sealed`, where the store is bound into the composed root.
#[test]
fn a_native_tools_store_outlives_the_session_when_sealed() {
    const TEST: &str = "a_native_tools_store_outlives_the_session_when_sealed";
    if capsule_runtime::skip_without_egress_namespace(TEST) || !host_reaches_sealed(TEST) {
        return;
    }
    store_outlives_the_session(TEST, SEALED);
}

fn store_reaches_the_declaring_tool_alone(test: &str, capabilities_yaml: &str) {
    // The second tool's script names the store's absolute path, which depends on the scratch
    // home, so the capsule is built in two steps: the home first, then the scripts.
    let probe = NativeCapsule::new(Vec::new(), capabilities_yaml, &[]);
    let secret = probe.store().join("secret");
    let read_secret = format!(
        "if {{ read -r line < '{}'; }} 2>/dev/null; then v=\"read:$line\"; else v=denied; fi",
        secret.display()
    );
    let capsule = NativeCapsule {
        entries: vec![
            entry_with_capabilities(TOOL_NAME, None, Some(STATE)),
            artifact_entry(SECOND_TOOL_NAME, None),
        ],
        ..probe
    };
    capsule.write_manifest("http://127.0.0.1:9");
    for (name, body) in [
        (TOOL_NAME, WRITE_SECRET),
        (SECOND_TOOL_NAME, read_secret.as_str()),
    ] {
        let artifact = common::create_native_artifact(
            capsule.fixture.path(),
            name,
            TOOL_VERSION,
            &native_script(body),
            Some("Reports what its own entry handed it."),
            None,
        );
        common::install_artifact_to_project_with_home(
            capsule.project.path(),
            &capsule.home,
            &artifact,
            &[],
        )
        .success();
    }

    let tier = common::explain_scope_json(&capsule.home, &capsule.manifest())["enforcement_tier"]
        .as_str()
        .unwrap()
        .to_string();
    let (reported, _) = capsule.run(&[tool(TOOL_NAME), tool(SECOND_TOOL_NAME)], &[]);
    assert_eq!(reported[0], "wrote", "{test} at {tier}");
    assert_eq!(
        fs::read_to_string(&secret).unwrap(),
        "the-secret\n",
        "{test} at {tier}"
    );
    match tier.as_str() {
        "landlock+seccomp" | "mountns+pivot_root+landlock+seccomp" => assert_eq!(
            reported[1], "denied",
            "{test}: at {tier} an undeclared native tool must not read the store"
        ),
        "seccomp-only" | "none" => assert_eq!(
            reported[1], "read:the-secret",
            "{test}: at {tier} nothing is scoped, so the store is reachable by path"
        ),
        other => panic!("{test}: unexpected enforcement tier {other}"),
    }
}

/// The store is the declaring tool's alone: on a Landlock tier another native tool pointed at its
/// path cannot read it; on a tier with no filesystem scope it can, as documented.
#[test]
fn the_store_reaches_the_declaring_native_tool_alone() {
    const TEST: &str = "the_store_reaches_the_declaring_native_tool_alone";
    if capsule_runtime::skip_without_egress_namespace(TEST) {
        return;
    }
    store_reaches_the_declaring_tool_alone(TEST, "");
}

/// The same under `sealed`, where the undeclared tool's composed root has no store in it at all.
#[test]
fn the_store_reaches_the_declaring_native_tool_alone_when_sealed() {
    const TEST: &str = "the_store_reaches_the_declaring_native_tool_alone_when_sealed";
    if capsule_runtime::skip_without_egress_namespace(TEST) || !host_reaches_sealed(TEST) {
        return;
    }
    store_reaches_the_declaring_tool_alone(TEST, SEALED);
}

/// `capabilities.state` is the one sub-block applied to a native tool: alone it prints no
/// `W-SEC-008`, and beside another sub-block the warning names that sub-block and says `state` is
/// the one that applies. Neither prints the retired inert-config warning.
#[test]
fn native_tool_capabilities_warnings() {
    const TEST: &str = "native_tool_capabilities_warnings";
    if capsule_runtime::skip_without_egress_namespace(TEST) {
        return;
    }
    for (capabilities, warns) in [
        (STATE, false),
        (
            "\n      state: {}\n      network:\n        allow: [https://example.com]",
            true,
        ),
    ] {
        let capsule = NativeCapsule::new(
            vec![entry_with_capabilities(TOOL_NAME, None, Some(capabilities))],
            "",
            &[(TOOL_NAME, ECHO_CONFIG)],
        );
        let (_, stderr) = capsule.run(&[tool(TOOL_NAME)], &[]);
        assert!(!stderr.contains(RETIRED_INERT_CONFIG_CODE), "{stderr}");
        let warning = stderr
            .lines()
            .find(|line| line.contains("warning[W-SEC-008]"));
        if !warns {
            assert_eq!(warning, None, "{stderr}");
            continue;
        }
        let warning = warning.unwrap_or_else(|| panic!("W-SEC-008 must fire: {stderr}"));
        assert!(
            warning.contains(&format!("artifact '{TOOL_NAME}'")),
            "{warning}"
        );
        assert!(warning.contains("capabilities.network"), "{warning}");
        assert!(
            warning.contains("capabilities.state is the one sub-block that applies"),
            "{warning}"
        );
    }
}
