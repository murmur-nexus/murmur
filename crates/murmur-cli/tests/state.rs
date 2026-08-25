//! Durable, capsule-scoped state stores: `capabilities.state` on an artifact entry, backed by
//! `$HOME/.murmur/state/<store>/` and mounted into the guest as a second preopen named `state`.
//!
//! Every test here drives the real `mur` binary against a real Wasmtime guest, because the whole
//! point of the capability is a host path outside the session workdir: an in-process assertion
//! about a `WasiCtx` cannot tell you what a guest's `std::fs::write("state/notes.jsonl", ..)`
//! actually resolves against, and that resolution is the mechanism.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;
use tempfile::TempDir;
use zip::{
    write::{FileOptions, SimpleFileOptions},
    CompressionMethod, ZipWriter,
};

const TOOL_NAME: &str = "state-writer";
const TOOL_VERSION: &str = "0.1.0";
const CAPSULE_NAME: &str = "state-capsule";

/// The store `capabilities.state` opens, given a capsule and a declared (or defaulted) name.
fn store_dir(home: &TempDir, store: &str) -> PathBuf {
    home.path().join(".murmur/state").join(store)
}

/// Lines the tool has appended to its store across every launch so far.
fn notes_lines(home: &TempDir, store: &str) -> usize {
    let notes = store_dir(home, store).join("notes.jsonl");
    fs::read_to_string(&notes)
        .unwrap_or_else(|err| panic!("reading {}: {err}", notes.display()))
        .lines()
        .filter(|line| !line.is_empty())
        .count()
}

/// A project whose capsule invokes `state-writer` and publishes the line count it reported.
///
/// `state_yaml` is spliced in under the tool entry's `capabilities:`, so passing `None` produces
/// an entry with no `capabilities:` block at all — the default-deny baseline, not an empty block.
fn state_project(project: &Path, capsule_name: &str, state_yaml: Option<&str>) -> PathBuf {
    state_project_with_capsule(
        project,
        capsule_name,
        "capsule-state-writer.wasm",
        state_yaml,
    )
}

fn state_project_with_capsule(
    project: &Path,
    capsule_name: &str,
    capsule_fixture: &str,
    state_yaml: Option<&str>,
) -> PathBuf {
    let capabilities = state_yaml
        .map(|yaml| format!("    capabilities:\n      state:{yaml}\n"))
        .unwrap_or_default();

    fs::write(
        project.join("murmur.yaml"),
        format!(
            "name: {capsule_name}\nversion: 0.0.1\nartifacts:\n  - name: {TOOL_NAME}\n    \
             version: {TOOL_VERSION}\n    runtime: tool\n{capabilities}"
        ),
    )
    .unwrap();

    fs::copy(
        fixture_component(capsule_fixture),
        project.join("capsule.wasm"),
    )
    .unwrap();

    project.join("murmur.yaml")
}

/// Install the `state-writer` tool into `project`'s own artifact store.
fn install_state_tool(fixture: &Path, project: &Path) {
    let artifact = fixture.join(format!("{TOOL_NAME}-{TOOL_VERSION}.mur.zip"));
    let mut zip = ZipWriter::new(fs::File::create(&artifact).unwrap());
    let options: SimpleFileOptions =
        FileOptions::default().compression_method(CompressionMethod::Deflated);

    zip.start_file("murmur.yaml", options).unwrap();
    writeln!(zip, "name: {TOOL_NAME}").unwrap();
    writeln!(zip, "version: {TOOL_VERSION}").unwrap();
    writeln!(zip, "runtime: wasm").unwrap();

    zip.start_file("tool.wasm", options).unwrap();
    zip.write_all(&fs::read(fixture_component("state-writer.wasm")).unwrap())
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

/// What the capsule wrote into its session workdir — the tool's reported line count, or the
/// failure it reported instead of trapping.
fn capsule_result(stdout: &str) -> String {
    let marker = "workdir: ";
    let start = stdout
        .find(marker)
        .unwrap_or_else(|| panic!("missing '{marker}' in stdout: {stdout}"));
    let workdir = PathBuf::from(
        stdout[start + marker.len()..]
            .lines()
            .next()
            .unwrap_or_default()
            .trim(),
    );
    fs::read_to_string(workdir.join("out/result.txt")).unwrap()
}

fn run_and_read(home: &TempDir, manifest: &Path) -> String {
    let stdout = common::run_capsule(home, manifest)
        .success()
        .get_output()
        .stdout
        .clone();
    capsule_result(&String::from_utf8(stdout).unwrap())
}

fn explain_scope(home: &TempDir, manifest: &Path, json: bool) -> Command {
    let mut cmd = Command::cargo_bin("mur").unwrap();
    cmd.env("HOME", home.path()).env_remove("NEXUS_API_KEY");
    cmd.args(["run", "--manifest", manifest.to_str().unwrap()]);
    if json {
        cmd.arg("--json");
    }
    cmd.arg("--explain-scope");
    cmd
}

fn explain_scope_json(home: &TempDir, manifest: &Path) -> Value {
    let stdout = explain_scope(home, manifest, true)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&stdout).expect("--explain-scope --json emits one JSON object")
}

/// The whole point of the capability: two launches, two different session workdirs, one store.
///
/// Neither run passes `--workdir`, so each gets a fresh `<manifest_dir>/workdir/<session_id>` and
/// the two share no directory at all. A count that reaches `2` can only have come from state that
/// outlived the first session.
#[test]
fn a_granted_tool_reads_back_what_an_earlier_session_wrote() {
    use std::os::unix::fs::PermissionsExt;

    let home = tempfile::tempdir().unwrap();
    let fixture = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let manifest = state_project(project.path(), CAPSULE_NAME, Some(" {}"));
    install_state_tool(fixture.path(), project.path());

    assert_eq!(run_and_read(&home, &manifest), "1");
    assert_eq!(notes_lines(&home, CAPSULE_NAME), 1);

    assert_eq!(run_and_read(&home, &manifest), "2");
    assert_eq!(notes_lines(&home, CAPSULE_NAME), 2);

    // A store holds one capsule's private working set, so both directories it needs are
    // owner-only — the root as well as the store, since a readable root leaks store names.
    for dir in [
        home.path().join(".murmur/state"),
        store_dir(&home, CAPSULE_NAME),
    ] {
        let mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode,
            0o700,
            "{} must be 0700, got {mode:04o}",
            dir.display()
        );
    }
}

/// Absent declaration, absent behaviour change: the run still succeeds, the guest's write fails,
/// and no part of the state tree is brought into existence.
#[test]
fn an_undeclared_tool_gets_no_store_and_no_directory_is_created() {
    let home = tempfile::tempdir().unwrap();
    let fixture = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let manifest = state_project(project.path(), CAPSULE_NAME, None);
    install_state_tool(fixture.path(), project.path());

    // The fixture reports its failure rather than trapping, so the assertion is on the outcome
    // it reported: without the second preopen there is no `state` path to open.
    assert!(
        run_and_read(&home, &manifest).starts_with("state-denied:"),
        "an undeclared tool must not reach a store"
    );
    assert!(
        !home.path().join(".murmur/state").exists(),
        "default-deny must create nothing at all"
    );

    explain_scope(&home, &manifest, false)
        .assert()
        .success()
        .stdout(predicate::str::contains("state stores: <none>"));
    assert_eq!(
        explain_scope_json(&home, &manifest)["state_stores"],
        serde_json::json!([])
    );
}

/// `store:` names the directory; the capsule name is only the default, and a declared name must
/// leave no trace of the default beside it.
#[test]
fn an_explicit_store_name_is_the_only_directory_created() {
    let home = tempfile::tempdir().unwrap();
    let fixture = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let manifest = state_project(project.path(), CAPSULE_NAME, Some("\n        store: shey"));
    install_state_tool(fixture.path(), project.path());

    assert_eq!(run_and_read(&home, &manifest), "1");
    assert_eq!(notes_lines(&home, "shey"), 1);
    assert!(
        !store_dir(&home, CAPSULE_NAME).exists(),
        "the capsule-named default must not be created alongside a declared store"
    );
}

/// Two capsules, one `HOME`, the same tool artifact declaring the same capability: each gets its
/// own store, and neither can see the other's line. A workdir-keyed store would have made this an
/// undeclared sharing channel; a capsule-keyed one makes it a non-event.
#[test]
fn two_capsules_sharing_a_home_never_share_a_store() {
    let home = tempfile::tempdir().unwrap();
    let fixture = tempfile::tempdir().unwrap();
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();

    let first_manifest = state_project(first.path(), "capsule-one", Some(" {}"));
    install_state_tool(fixture.path(), first.path());
    let second_manifest = state_project(second.path(), "capsule-two", Some(" {}"));
    install_state_tool(fixture.path(), second.path());

    assert_eq!(run_and_read(&home, &first_manifest), "1");
    assert_eq!(run_and_read(&home, &second_manifest), "1");

    assert_eq!(notes_lines(&home, "capsule-one"), 1);
    assert_eq!(notes_lines(&home, "capsule-two"), 1);
}

/// A capsule component runs on the capsule ceiling and holds no artifact grant, so it holds no
/// descriptor naming a store — not by the guest path a granted artifact uses, and not by walking
/// out of the workdir preopen towards where the store lives on the host.
///
/// Asserted while a store for the same capsule is live on disk, so a pass cannot come from the
/// store simply not existing yet.
#[test]
fn a_capsule_cannot_reach_its_own_tools_store() {
    let home = tempfile::tempdir().unwrap();
    let fixture = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    // First launch: an ordinary capsule, purely to bring the store into existence.
    let manifest = state_project(project.path(), CAPSULE_NAME, Some(" {}"));
    install_state_tool(fixture.path(), project.path());
    assert_eq!(run_and_read(&home, &manifest), "1");

    // Second launch: same manifest, same live store, a capsule that probes for it.
    let manifest = state_project_with_capsule(
        project.path(),
        CAPSULE_NAME,
        "capsule-state-probe.wasm",
        Some(" {}"),
    );
    assert_eq!(
        run_and_read(&home, &manifest),
        "blocked blocked",
        "neither the `state` guest path nor a traversal out of the workdir may resolve"
    );
    assert!(
        !store_dir(&home, CAPSULE_NAME).join("probe.txt").exists(),
        "no probe may land in the store"
    );
    // The tool's own line is still the only thing there.
    assert_eq!(notes_lines(&home, CAPSULE_NAME), 1);
}

/// A store name is one path segment. Each malformed value refuses the launch by name, before any
/// registry pull, workdir creation or component instantiation — and leaves nothing behind.
#[test]
fn a_malformed_store_name_refuses_the_launch() {
    let home = tempfile::tempdir().unwrap();
    let fixture = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    // A well-formed manifest first, so `mur install` can find the project root; each iteration
    // then rewrites only the store name.
    state_project(project.path(), CAPSULE_NAME, Some(" {}"));
    install_state_tool(fixture.path(), project.path());

    for store in ["../escape", "/abs/path", "a/b", ""] {
        let manifest = state_project(
            project.path(),
            CAPSULE_NAME,
            Some(&format!("\n        store: \"{store}\"")),
        );

        common::run_capsule(&home, &manifest)
            .failure()
            .stderr(predicate::str::contains("error[E-CAP-009]"))
            .stderr(predicate::str::contains(format!("'{store}'")));

        // The same refusal reaches the diagnostic: a bad name is caught by `--explain-scope` too,
        // so an operator checking their manifest is not told it is fine and then refused a launch.
        explain_scope(&home, &manifest, false)
            .assert()
            .failure()
            .stderr(predicate::str::contains("error[E-CAP-009]"));

        assert!(
            !home.path().join(".murmur/state").exists(),
            "'{store}' must create nothing under the state root"
        );
    }
}

/// A well-formed declaration whose store cannot be made on this host refuses by the same code,
/// naming the store and the path — a host problem, reported as one, not as a bad manifest.
#[test]
fn an_unusable_state_root_refuses_the_launch() {
    let home = tempfile::tempdir().unwrap();
    let fixture = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let manifest = state_project(project.path(), CAPSULE_NAME, Some(" {}"));
    install_state_tool(fixture.path(), project.path());

    // A regular file where the state root would go: `create_dir_all` cannot proceed.
    fs::create_dir_all(home.path().join(".murmur")).unwrap();
    fs::write(home.path().join(".murmur/state"), b"not a directory").unwrap();

    common::run_capsule(&home, &manifest)
        .failure()
        .stderr(predicate::str::contains("error[E-CAP-009]"))
        .stderr(predicate::str::contains(CAPSULE_NAME))
        // The hint points at the store and at `~/.murmur/state/`, never at the containment
        // ladder: a state store is not a containment shortfall and no floor change makes one
        // appear, so neither remedy the ladder offers may be suggested here.
        .stderr(predicate::str::contains("~/.murmur/state/"))
        .stderr(predicate::str::contains("--containment").not())
        .stderr(predicate::str::contains("capabilities.containment").not());
}

/// The grant is reported in all three places, identically, and reporting it moves nothing else.
#[test]
fn the_grant_is_reported_and_reporting_it_changes_nothing_else() {
    let home = tempfile::tempdir().unwrap();
    let fixture = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let manifest = state_project(project.path(), CAPSULE_NAME, Some("\n        store: shey"));
    install_state_tool(fixture.path(), project.path());

    let expected_path = store_dir(&home, "shey").display().to_string();
    let declared = explain_scope_json(&home, &manifest);
    assert_eq!(
        declared["state_stores"],
        serde_json::json!([{
            "artifact": TOOL_NAME,
            "store": "shey",
            "host_path": expected_path,
        }])
    );

    explain_scope(&home, &manifest, false)
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "{TOOL_NAME}: shey -> {expected_path}"
        )));

    // `--explain-scope` is read-only. Printing a host path must not be what brings it into being.
    assert!(
        !home.path().join(".murmur/state").exists(),
        "a diagnostic must create no directory"
    );

    // The launch itself agrees with the diagnostic: same store, same host path. The third place
    // it is reported is `session_start.effective_grants`, which carries the whole `ScopeReport`
    // verbatim; that identity is asserted against a populated `state_stores` in
    // `capsule-runtime`'s `session_start_records_the_whole_scope_report_as_effective_grants`. This
    // capsule declares no `inference:` block, so it runs no agent loop and opens no `trace.jsonl`
    // of its own to read here.
    assert_eq!(run_and_read(&home, &manifest), "1");
    assert_eq!(notes_lines(&home, "shey"), 1);

    // Same manifest with the `state:` block deleted: every containment field is identical, so
    // declaring a store is proven to move nothing but its own key.
    let undeclared = explain_scope_json(&home, &state_project(project.path(), CAPSULE_NAME, None));
    assert_eq!(undeclared["state_stores"], serde_json::json!([]));
    for field in [
        "declared_containment",
        "achieved_containment",
        "floor_met",
        "enforcement_tier",
    ] {
        assert_eq!(
            declared[field], undeclared[field],
            "declaring state moved '{field}'"
        );
    }
}

/// A home directory that cannot be resolved refuses a launch only when something declared a
/// store, so the refusal is attributable to the declaration rather than to the environment. Both
/// halves are asserted together, because either alone would be consistent with the other cause.
///
/// A *relative* `HOME` is the reachable form of an unresolvable one: a durable store resolved
/// against it would depend on the process's working directory, so it is refused, while the config
/// and artifact-store paths `mur run` needs unconditionally still resolve.
#[test]
fn an_unresolvable_home_refuses_only_a_capsule_that_declared_a_store() {
    let cwd = tempfile::tempdir().unwrap();
    let fixture = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let declared = state_project(project.path(), CAPSULE_NAME, Some(" {}"));
    install_state_tool(fixture.path(), project.path());

    // Run from a scratch directory: whatever `mur` does resolve against a relative `HOME` lands
    // there rather than wherever the test binary happens to be invoked from.
    let mut refused = Command::cargo_bin("mur").unwrap();
    refused
        .current_dir(cwd.path())
        .env("HOME", "relative-home")
        .env_remove("NEXUS_API_KEY")
        .args(["run", "--manifest", declared.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("error[E-CAP-009]"))
        .stderr(predicate::str::contains(CAPSULE_NAME))
        .stderr(predicate::str::contains(
            "the home directory could not be resolved",
        ));

    // Same host, same unresolvable home, no declaration: nothing looks a home directory up on
    // behalf of a store that was never asked for.
    let undeclared = state_project(project.path(), CAPSULE_NAME, None);
    let mut launched = Command::cargo_bin("mur").unwrap();
    launched
        .current_dir(cwd.path())
        .env("HOME", "relative-home")
        .env_remove("NEXUS_API_KEY")
        .args(["run", "--manifest", undeclared.to_str().unwrap()])
        .assert()
        .success();
}

/// A capsule-wide `capabilities.state` reaches nothing — the capsule's own guest is built with no
/// artifact grant — so it launches, warns, and creates no directory.
#[test]
fn a_capsule_wide_declaration_warns_and_grants_nothing() {
    let home = tempfile::tempdir().unwrap();
    let fixture = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    fs::write(
        project.path().join("murmur.yaml"),
        format!(
            "name: {CAPSULE_NAME}\nversion: 0.0.1\nartifacts:\n  - name: {TOOL_NAME}\n    \
             version: {TOOL_VERSION}\n    runtime: tool\ncapabilities:\n  state:\n    store: \
             capsule-wide\n"
        ),
    )
    .unwrap();
    fs::copy(
        fixture_component("capsule-state-writer.wasm"),
        project.path().join("capsule.wasm"),
    )
    .unwrap();
    install_state_tool(fixture.path(), project.path());

    common::run_capsule(&home, &project.path().join("murmur.yaml"))
        .success()
        .stderr(predicate::str::contains("warning[W-SEC-014]"))
        .stderr(predicate::str::contains("capsule-wide capabilities.state"))
        .stderr(predicate::str::contains("granted per artifact"));

    assert!(
        !home.path().join(".murmur/state").exists(),
        "a declaration nothing reads must create nothing"
    );
}

/// The other declaration that would grant nothing is `capabilities:` on a `runtime: skill` entry
/// — and the manifest parser already refuses it outright, which is strictly stronger than a
/// warning. Asserted here so the refusal is not quietly weakened into one later.
#[test]
fn state_on_a_skill_entry_is_refused_at_parse_time() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    fs::write(
        project.path().join("murmur.yaml"),
        format!(
            "name: {CAPSULE_NAME}\nversion: 0.0.1\nartifacts:\n  - name: notes-skill\n    \
             version: 1.0.0\n    runtime: skill\n    capabilities:\n      state: {{}}\n"
        ),
    )
    .unwrap();
    fs::copy(
        fixture_component("capsule-state-writer.wasm"),
        project.path().join("capsule.wasm"),
    )
    .unwrap();

    common::run_capsule(&home, &project.path().join("murmur.yaml"))
        .failure()
        .stderr(predicate::str::contains("notes-skill"))
        .stderr(predicate::str::contains("runtime: skill"));

    assert!(!home.path().join(".murmur/state").exists());
}
