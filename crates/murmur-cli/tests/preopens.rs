//! The filesystem surface each artifact is preopened into, as `mur run --explain-scope` and
//! `mur doctor` report it.
//!
//! Every test here drives the real `mur` binary, because the claim is about what an operator can
//! read before launching anything: an in-process assertion about a `ScopeReport` cannot tell you
//! whether the three surfaces are distinguishable in the output a person actually sees.
//!
//! `--explain-scope` returns ahead of every side effect, so most of these need only a
//! `murmur.yaml` and no installed artifact at all. The two that need one say so.

#[path = "common/mod.rs"]
mod common;

use std::{fs, io::Write, path::Path};

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;
use zip::{
    write::{FileOptions, SimpleFileOptions},
    CompressionMethod, ZipWriter,
};

const CAPSULE_NAME: &str = "preopen-capsule";
const ARTIFACT_VERSION: &str = "0.1.0";

/// One artifact entry. `capabilities_yaml` is spliced in under `capabilities:`, so `None`
/// produces an entry with no `capabilities:` block at all — the undeclared case, not an empty
/// block.
fn artifact_entry(name: &str, runtime: &str, capabilities_yaml: Option<&str>) -> String {
    let capabilities = capabilities_yaml
        .map(|yaml| format!("    capabilities:{yaml}\n"))
        .unwrap_or_default();
    format!(
        "  - name: {name}\n    version: {ARTIFACT_VERSION}\n    runtime: {runtime}\n{capabilities}"
    )
}

/// A project declaring `entries`, with `capsule_yaml` spliced in above them for the capsule-wide
/// blocks. The capsule bytes are a bare module header, enough to be discovered and read.
fn project(dir: &Path, capsule_yaml: &str, entries: &[String]) {
    fs::write(
        dir.join("murmur.yaml"),
        format!(
            "name: {CAPSULE_NAME}\nversion: 0.0.1\n{capsule_yaml}artifacts:\n{}",
            entries.concat()
        ),
    )
    .unwrap();
    fs::write(dir.join("capsule.wasm"), b"\0asm\x01\0\0\0").unwrap();
}

fn mur_run(home: &TempDir, project_dir: &Path, args: &[&str]) -> assert_cmd::assert::Assert {
    Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .current_dir(project_dir)
        .arg("run")
        .arg("--manifest")
        .arg("murmur.yaml")
        .args(args)
        .assert()
}

fn explain_scope_stdout(home: &TempDir, project_dir: &Path) -> String {
    let output = mur_run(home, project_dir, &["--explain-scope"])
        .success()
        .get_output()
        .stdout
        .clone();
    String::from_utf8(output).unwrap()
}

fn explain_scope_json(home: &TempDir, project_dir: &Path) -> serde_json::Value {
    let output = mur_run(home, project_dir, &["--explain-scope", "--json"])
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).expect("--explain-scope --json emits one JSON object")
}

/// Install a WASM artifact into `project`'s own store under `name`. The bytes are never
/// instantiated by `mur doctor`, which resolves and hashes them and nothing more.
fn install_wasm_artifact(fixture: &Path, project_dir: &Path, name: &str, runtime: &str) {
    let artifact = fixture.join(format!("{name}-{ARTIFACT_VERSION}.mur.zip"));
    let mut zip = ZipWriter::new(fs::File::create(&artifact).unwrap());
    let options: SimpleFileOptions =
        FileOptions::default().compression_method(CompressionMethod::Deflated);

    zip.start_file("murmur.yaml", options).unwrap();
    writeln!(zip, "name: {name}").unwrap();
    writeln!(zip, "version: {ARTIFACT_VERSION}").unwrap();
    writeln!(zip, "runtime: {runtime}").unwrap();

    let entry = if runtime == "hook" {
        "hook.wasm"
    } else {
        "tool.wasm"
    };
    zip.start_file(entry, options).unwrap();
    zip.write_all(b"\0asm\x01\0\0\0").unwrap();
    zip.finish().unwrap();

    common::install_artifact_to_project(project_dir, &artifact).success();
}

/// The wide default, read off the diagnostic: a tool entry that declares no `capabilities:` block
/// keeps the whole accessible workdir, and the line says which declaration is absent.
#[test]
fn an_undeclared_tool_reads_as_the_whole_workdir() {
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    project(dir.path(), "", &[artifact_entry("notes-tool", "tool", None)]);

    let stdout = explain_scope_stdout(&home, dir.path());
    assert!(
        stdout.contains(
            "- notes-tool (tool): the whole accessible workdir — no capabilities.filesystem.scope \
             declared"
        ),
        "expected the whole-workdir line, got:\n{stdout}"
    );
    // The list is rendered under the grants, beside the capsule-wide scope it qualifies.
    assert!(stdout.contains("preopens:"), "got:\n{stdout}");
    assert!(
        !dir.path().join("workdir").exists(),
        "--explain-scope must not create a workdir"
    );
}

/// A declared per-artifact scope is the narrowing that reaches a guest, and the report names it.
#[test]
fn a_scoped_tool_reads_as_a_subtree() {
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    project(
        dir.path(),
        "",
        &[artifact_entry(
            "notes-tool",
            "tool",
            Some("\n      filesystem:\n        scope: cache"),
        )],
    );

    let stdout = explain_scope_stdout(&home, dir.path());
    assert!(
        stdout.contains(
            "- notes-tool (tool): one subtree of the accessible workdir — \
             capabilities.filesystem.scope: cache"
        ),
        "expected the scoped line, got:\n{stdout}"
    );
}

/// The asymmetry between the two roles, readable from one report: three entries, three visibly
/// different lines. An operator who cannot tell them apart cannot use the diagnostic.
#[test]
fn the_three_surfaces_render_as_three_different_lines() {
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    project(
        dir.path(),
        "",
        &[
            artifact_entry("notes-tool", "tool", None),
            artifact_entry(
                "cache-tool",
                "tool",
                Some("\n      filesystem:\n        scope: cache"),
            ),
            artifact_entry("telemetry-hook", "hook", None),
        ],
    );

    let stdout = explain_scope_stdout(&home, dir.path());
    let lines: Vec<&str> = stdout
        .lines()
        .map(str::trim)
        .filter(|line| {
            line.starts_with("- notes-tool")
                || line.starts_with("- cache-tool")
                || line.starts_with("- telemetry-hook")
        })
        .collect();

    assert_eq!(lines.len(), 3, "expected three preopen lines, got:\n{stdout}");
    assert!(
        lines[2].contains("nothing preopened — no capabilities.filesystem.scope declared"),
        "an ungranted hook must read as nothing preopened, got: {}",
        lines[2]
    );
    // Distinct wording per surface, not one phrase with a path appended.
    assert_eq!(
        lines.iter().collect::<std::collections::HashSet<_>>().len(),
        3,
        "the three surfaces must be distinguishable: {lines:?}"
    );
}

/// The wire shape: one element per guest-bearing artifact, four keys, `scope` `null` when the
/// entry declared none, and one of exactly three surface names.
#[test]
fn the_json_report_carries_a_stable_preopen_shape() {
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    project(
        dir.path(),
        "",
        &[
            artifact_entry("notes-tool", "tool", None),
            artifact_entry(
                "cache-tool",
                "driver",
                Some("\n      filesystem:\n        scope: cache"),
            ),
            artifact_entry("telemetry-hook", "hook", None),
            artifact_entry("notes-skill", "skill", None),
        ],
    );

    assert_eq!(
        explain_scope_json(&home, dir.path())["preopens"],
        serde_json::json!([
            {
                "artifact": "notes-tool",
                "role": "tool",
                "scope": null,
                "surface": "whole-workdir",
            },
            {
                "artifact": "cache-tool",
                "role": "driver",
                "scope": "cache",
                "surface": "scoped-subtree",
            },
            {
                "artifact": "telemetry-hook",
                "role": "hook",
                "scope": null,
                "surface": "nothing",
            },
        ])
    );
}

/// The key is never skipped: a capsule whose only artifacts are skills reports `[]`, so a consumer
/// can tell "this runtime predates the field" from "this capsule has no guest artifacts".
#[test]
fn a_skill_only_capsule_reports_an_empty_preopen_array() {
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    project(
        dir.path(),
        "",
        &[artifact_entry("notes-skill", "skill", None)],
    );

    assert_eq!(
        explain_scope_json(&home, dir.path())["preopens"],
        serde_json::json!([])
    );
    assert!(explain_scope_stdout(&home, dir.path()).contains("preopens: <none>"));
}

/// A diagnostic that describes a launch refuses whatever the launch refuses. Both commands fail
/// with the same code on the same manifest line, rather than one printing a clean report.
#[test]
fn an_escaping_scope_refuses_the_report_and_the_run_alike() {
    let home = TempDir::new().unwrap();
    let fixture = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    project(
        dir.path(),
        "",
        &[artifact_entry(
            "notes-tool",
            "tool",
            Some("\n      filesystem:\n        scope: ../escape"),
        )],
    );
    install_wasm_artifact(fixture.path(), dir.path(), "notes-tool", "wasm");

    for args in [&["--explain-scope"][..], &[][..]] {
        mur_run(&home, dir.path(), args)
            .failure()
            .stderr(predicate::str::contains("error[E-CAP-002]"))
            .stderr(predicate::str::contains("../escape"));
    }

    assert!(
        !dir.path().join("workdir").exists(),
        "a refused scope must be caught before any workdir is created"
    );
}

/// The report describes the resolved preopen, not the declaration: a capsule-wide
/// `capabilities.filesystem.scope` is joined into no guest preopen root, so a tool with no
/// per-artifact block still holds the whole accessible workdir.
#[test]
fn a_capsule_wide_scope_does_not_narrow_a_tools_preopen() {
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    project(
        dir.path(),
        "capabilities:\n  filesystem:\n    scope: repo\n",
        &[artifact_entry("notes-tool", "tool", None)],
    );

    let report = explain_scope_json(&home, dir.path());
    assert_eq!(report["filesystem_scope"], "repo");
    assert_eq!(
        report["preopens"],
        serde_json::json!([
            {
                "artifact": "notes-tool",
                "role": "tool",
                "scope": null,
                "surface": "whole-workdir",
            },
        ])
    );
    assert!(explain_scope_stdout(&home, dir.path())
        .contains("- notes-tool (tool): the whole accessible workdir"));
}

/// `mur doctor` prints the same lines from the same resolver, and its verdict is untouched: a wide
/// preopen is reported, never counted as a failure and never turned into a fix line.
#[test]
fn doctor_reports_the_same_preopens_without_changing_its_verdict() {
    let home = TempDir::new().unwrap();
    let fixture = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    project(
        dir.path(),
        "",
        &[
            artifact_entry("notes-tool", "tool", None),
            artifact_entry("telemetry-hook", "hook", None),
        ],
    );
    install_wasm_artifact(fixture.path(), dir.path(), "notes-tool", "wasm");
    install_wasm_artifact(fixture.path(), dir.path(), "telemetry-hook", "hook");

    let assertion = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .current_dir(dir.path())
        .arg("doctor")
        .assert()
        .success();
    let stdout = String::from_utf8(assertion.get_output().stdout.clone()).unwrap();

    assert!(stdout.contains("Filesystem preopens"), "got:\n{stdout}");
    assert!(
        stdout.contains(
            "- notes-tool (tool): the whole accessible workdir — no capabilities.filesystem.scope \
             declared"
        ),
        "got:\n{stdout}"
    );
    assert!(
        stdout.contains(
            "- telemetry-hook (hook): nothing preopened — no capabilities.filesystem.scope declared"
        ),
        "got:\n{stdout}"
    );
    // The verdict is the artifact checklist's alone: two present artifacts, no fix lines.
    assert!(stdout.contains("All checks passed."), "got:\n{stdout}");
    assert!(!stdout.contains("Fix:"), "got:\n{stdout}");
}
