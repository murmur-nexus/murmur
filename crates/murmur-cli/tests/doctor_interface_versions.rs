//! `mur doctor`'s `Interface versions` check: every installed artifact naming a `murmur:`
//! interface version this `mur` does not serve, against a real store under a temporary `HOME`.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use capsule_runtime::SERVED_WIT_PACKAGES;
use common::{create_hook_artifact, create_skill_artifact, create_tool_artifact, publish_local};
use serde_json::Value;
use tempfile::TempDir;

const HOOK: &str = "stale-hook";
const HOOK_VERSION: &str = "0.3.0";

/// A hook component built against a retired `murmur:hook` and a retired `murmur:runtime`. The
/// extractor reads only the import and export sections, so the component needs no functions.
const STALE_HOOK_WAT: &str = r#"
    (component
      (import "murmur:runtime/inference@0.3.0" (instance))
      (instance $i)
      (export "murmur:hook/lifecycle@0.8.0" (instance $i)))
"#;

fn served(package: &str) -> &'static str {
    SERVED_WIT_PACKAGES
        .iter()
        .find(|(served_package, _)| *served_package == package)
        .unwrap_or_else(|| panic!("{package} is not served"))
        .1
}

fn publish_component(home: &TempDir, name: &str, version: &str, wat: &str) {
    let work = TempDir::new().unwrap();
    let wasm_path = work.path().join("hook.wasm");
    fs::write(&wasm_path, wat::parse_str(wat).unwrap()).unwrap();
    publish_local(
        home,
        &create_hook_artifact(work.path(), name, version, &wasm_path),
    )
    .success();
}

fn publish_stale_hook(home: &TempDir) {
    publish_component(home, HOOK, HOOK_VERSION, STALE_HOOK_WAT);
}

fn sidecar_path(home: &TempDir, name: &str, version: &str) -> PathBuf {
    home.path().join(format!(
        ".murmur/artifacts/{name}/{version}/{name}-{version}.meta.json"
    ))
}

/// Rewrite the global-store sidecar of `name@version`, passing its `meta` object to `edit`.
fn edit_sidecar(home: &TempDir, name: &str, version: &str, edit: impl FnOnce(&mut Value)) {
    let path = sidecar_path(home, name, version);
    let mut sidecar: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    edit(&mut sidecar["meta"]);
    fs::write(&path, serde_json::to_string_pretty(&sidecar).unwrap()).unwrap();
}

fn create_project(project_dir: &Path, artifacts_yaml: &str) {
    fs::write(
        project_dir.join("murmur.yaml"),
        format!("name: doctor-fixture\nversion: 0.0.1\nartifacts:{artifacts_yaml}\n"),
    )
    .unwrap();
}

fn mur_doctor(home: &TempDir, project_dir: &Path) -> assert_cmd::assert::Assert {
    Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .current_dir(project_dir)
        .arg("doctor")
        .assert()
}

fn stdout_of(assert: &assert_cmd::assert::Assert) -> String {
    String::from_utf8(assert.get_output().stdout.clone()).unwrap()
}

fn stderr_of(assert: &assert_cmd::assert::Assert) -> String {
    String::from_utf8(assert.get_output().stderr.clone()).unwrap()
}

/// The two `Interface versions` lines the stale hook produces in the global store.
fn assert_stale_hook_lines(stdout: &str) {
    let hook_ref = format!("{HOOK}@{HOOK_VERSION}");
    assert!(stdout.contains("Interface versions"), "{stdout}");
    let export_line = stdout
        .lines()
        .find(|line| line.contains("exports murmur:hook/lifecycle@0.8.0"))
        .unwrap_or_else(|| panic!("no export line: {stdout}"));
    assert!(export_line.contains(&hook_ref), "{export_line}");
    assert!(export_line.contains("global"), "{export_line}");
    assert!(
        export_line.contains(&format!(
            "this mur serves murmur:hook@{}",
            served("murmur:hook")
        )),
        "{export_line}"
    );
    let import_line = stdout
        .lines()
        .find(|line| line.contains("imports murmur:runtime/inference@0.3.0"))
        .unwrap_or_else(|| panic!("no import line: {stdout}"));
    assert!(import_line.contains(&hook_ref), "{import_line}");
    assert!(import_line.contains("global"), "{import_line}");
    assert!(
        import_line.contains(&format!(
            "this mur serves murmur:runtime@{}",
            served("murmur:runtime")
        )),
        "{import_line}"
    );
    assert!(stdout.contains("W-REG-003"), "{stdout}");
}

/// The warning-tier verdict for the stale hook: one warning, a `Fix:` naming a release built
/// against the served versions, and no `Fix:` that reinstalls the stale version.
fn assert_stale_hook_warning(stdout: &str) {
    assert!(stdout.contains("1 warning"), "{stdout}");
    assert!(stdout.contains("0 errors found"), "{stdout}");
    assert!(
        stdout.contains(&format!(
            "Fix: mur install -g {HOOK}@<a release built against murmur:hook@{}, murmur:runtime@{}>",
            served("murmur:hook"),
            served("murmur:runtime")
        )),
        "{stdout}"
    );
    let stale_target = format!("{HOOK}@{HOOK_VERSION}");
    for line in stdout.lines().filter(|line| line.starts_with("Fix:")) {
        assert!(
            !line.contains(&stale_target),
            "a Fix reinstalls the stale build: {line}"
        );
    }
}

#[test]
fn an_undeclared_stale_artifact_is_a_warning() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    create_project(project.path(), " []");
    publish_stale_hook(&home);

    let assert = mur_doctor(&home, project.path()).code(0);
    let stdout = stdout_of(&assert);
    assert_stale_hook_lines(&stdout);
    assert_stale_hook_warning(&stdout);
}

#[test]
fn current_artifacts_print_nothing() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    create_project(project.path(), " []");
    let echo_tool =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/run/components/echo-tool.wasm");
    publish_local(
        &home,
        &create_tool_artifact(work.path(), "current-tool", "0.1.0", &echo_tool),
    )
    .success();
    publish_local(
        &home,
        &create_skill_artifact(work.path(), "current-skill", "0.1.0", "# guidance"),
    )
    .success();

    let assert = mur_doctor(&home, project.path()).code(0);
    let stdout = stdout_of(&assert);
    let stderr = stderr_of(&assert);
    assert!(stdout.contains("All checks passed."), "{stdout}");
    for stream in [&stdout, &stderr] {
        assert!(!stream.contains("Interface versions"), "{stream}");
        assert!(!stream.contains("W-REG-003"), "{stream}");
    }
}

#[test]
fn a_declared_stale_artifact_is_an_error() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    create_project(
        project.path(),
        &format!("\n  - name: {HOOK}\n    version: {HOOK_VERSION}\n    runtime: hook"),
    );
    publish_stale_hook(&home);

    let assert = mur_doctor(&home, project.path()).code(1);
    let stdout = stdout_of(&assert);
    let failed = stdout
        .lines()
        .find(|line| line.contains('\u{2717}') && line.contains("murmur:hook/lifecycle@0.8.0"))
        .unwrap_or_else(|| panic!("no failing checklist line: {stdout}"));
    assert!(
        failed.contains(&format!("{HOOK}@{HOOK_VERSION}")),
        "{failed}"
    );
    assert!(
        failed.contains(&format!(
            "this mur serves murmur:hook@{}",
            served("murmur:hook")
        )),
        "{failed}"
    );
    assert!(failed.contains("(+1 more)"), "{failed}");
    assert!(
        stdout.contains("0 checks passed, 1 error found."),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "Fix: {HOOK}: pin a release built against murmur:hook@{}, murmur:runtime@{} in murmur.yaml, then run mur install",
            served("murmur:hook"),
            served("murmur:runtime")
        )),
        "{stdout}"
    );
    assert!(!stdout.contains("Fix: mur install"), "{stdout}");
}

#[test]
fn a_package_this_mur_does_not_serve_is_named() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    create_project(project.path(), " []");
    publish_component(
        &home,
        "odd-hook",
        "0.1.0",
        r#"
        (component
          (import "murmur:nonexistent/thing@0.1.0" (instance))
          (instance $i)
          (export "murmur:hook/lifecycle@0.9.0" (instance $i)))
        "#,
    );

    let assert = mur_doctor(&home, project.path()).code(0);
    let stdout = stdout_of(&assert);
    assert!(
        stdout.contains(
            "imports murmur:nonexistent/thing@0.1.0 \u{2014} this mur serves no version of murmur:nonexistent"
        ),
        "{stdout}"
    );
    assert!(!stdout.contains("murmur:hook/lifecycle"), "{stdout}");
    assert!(stdout.contains("1 warning"), "{stdout}");
    assert!(
        stdout
            .contains("Fix: mur install -g odd-hook@<a release built without murmur:nonexistent>"),
        "{stdout}"
    );
}

#[test]
fn a_sidecar_without_recorded_contracts_is_read_from_the_payload_and_left_untouched() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    create_project(project.path(), " []");
    publish_stale_hook(&home);
    edit_sidecar(&home, HOOK, HOOK_VERSION, |meta| {
        meta.as_object_mut().unwrap().remove("wit_contracts");
    });
    let before = fs::read(sidecar_path(&home, HOOK, HOOK_VERSION)).unwrap();
    assert!(!String::from_utf8_lossy(&before).contains("wit_contracts"));

    let assert = mur_doctor(&home, project.path()).code(0);
    let stdout = stdout_of(&assert);
    assert_stale_hook_lines(&stdout);
    assert_stale_hook_warning(&stdout);

    assert_eq!(
        fs::read(sidecar_path(&home, HOOK, HOOK_VERSION)).unwrap(),
        before
    );
}

/// The recorded value is what the check reads whenever the key is present, so a sidecar naming
/// current interfaces silences a payload that would not.
#[test]
fn recorded_contracts_win_over_the_payload() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    create_project(project.path(), " []");
    publish_stale_hook(&home);
    edit_sidecar(&home, HOOK, HOOK_VERSION, |meta| {
        meta["wit_contracts"] = serde_json::json!({
            "exports": [format!("murmur:hook/lifecycle@{}", served("murmur:hook"))],
            "imports": [format!("murmur:runtime/inference@{}", served("murmur:runtime"))],
        });
    });

    let assert = mur_doctor(&home, project.path()).code(0);
    let stdout = stdout_of(&assert);
    assert!(!stdout.contains("Interface versions"), "{stdout}");
    assert!(stdout.contains("All checks passed."), "{stdout}");
}

#[test]
fn an_unreadable_store_is_named_and_does_not_fail_doctor() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    create_project(project.path(), " []");
    publish_stale_hook(&home);
    fs::write(sidecar_path(&home, HOOK, HOOK_VERSION), "{ not json").unwrap();

    let assert = mur_doctor(&home, project.path()).code(0);
    let stdout = stdout_of(&assert);
    assert!(stdout.contains("Interface versions"), "{stdout}");
    assert!(
        stdout.contains("  not checked (global store): "),
        "{stdout}"
    );
    assert!(stdout.contains("All checks passed."), "{stdout}");
}
