//! What `mur run --explain-scope`, `mur doctor` and a launch say about the routes a capsule runs
//! code on and about its cgroup scope, driven through the real binary.
//!
//! The cgroup cases build a host whose cgroup hierarchy is read-only, which an unprivileged user
//! can do with `unshare`: a bind of `/sys/fs/cgroup` over itself, remounted read-only inside a
//! fresh user and mount namespace. There every `mkdir` of a child cgroup fails with `EROFS`, so a
//! capsule that runs subprocesses cannot get its scope and a launch is refused with `E-RUN-012`.
//! The construction runs inside a transient `systemd-run --user --scope -p Delegate=yes` scope, so
//! the delegated controllers are there and only the read-only mount refuses. A host where the
//! construction cannot be built prints `[SKIP-HOST]` and the case returns.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use serde_json::{json, Value};
use tempfile::TempDir;

const DRIVER_NAME: &str = "local/murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.7";
const TOOL_NAME: &str = "local/jsonl-line-count";
const TOOL_VERSION: &str = "0.1.0";
const PROCESS_DRIVER_NAME: &str = "local/process-driver";

/// The setup `sh -c` runs before it `exec`s the program: `/sys/fs/cgroup` bound over itself and
/// remounted read-only.
const READ_ONLY_CGROUP: &str = "mount --bind /sys/fs/cgroup /sys/fs/cgroup && \
     mount -o remount,bind,ro /sys/fs/cgroup && exec \"$0\" \"$@\"";

fn mur_path() -> PathBuf {
    assert_cmd::cargo::cargo_bin("mur")
}

/// What one `mur` invocation printed, and its exit code.
struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Run {
    fn of(output: Output) -> Self {
        Self {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    fn all(&self) -> String {
        format!(
            "--- exit {:?} ---\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.code, self.stdout, self.stderr
        )
    }

    fn json(&self) -> Value {
        serde_json::from_str(self.stdout.trim()).unwrap_or_else(|error| {
            panic!(
                "--explain-scope --json printed no JSON ({error}):\n{}",
                self.all()
            )
        })
    }
}

/// `program args…` in `cwd` with `HOME` set, inside the read-only cgroup construction when
/// `read_only_cgroup`.
///
/// The construction runs inside a transient systemd scope with every controller delegated, so the
/// delegated base `mur` finds there carries memory, pids, cpu and io whatever the test harness's
/// own cgroup has, and the first call to fail is the `mkdir` of a child cgroup.
fn command(
    read_only_cgroup: bool,
    home: &Path,
    cwd: &Path,
    program: &Path,
    args: &[&str],
) -> Command {
    let mut command = if read_only_cgroup {
        let mut command = Command::new("systemd-run");
        command
            .args([
                "--user",
                "--scope",
                "--quiet",
                "--collect",
                "-p",
                "Delegate=yes",
                "--",
                "unshare",
                "-Urm",
                "sh",
                "-c",
                READ_ONLY_CGROUP,
            ])
            .arg(program)
            .args(args);
        command
    } else {
        let mut command = Command::new(program);
        command.args(args);
        command
    };
    command.env("HOME", home).current_dir(cwd);
    command
}

fn mur(read_only_cgroup: bool, home: &Path, cwd: &Path, args: &[&str]) -> Run {
    Run::of(
        command(read_only_cgroup, home, cwd, &mur_path(), args)
            .output()
            .unwrap(),
    )
}

/// `None` when the construction works here — it runs, and a child cgroup of this process's own
/// cgroup cannot be created inside it, with `EROFS` — else why it does not.
fn read_only_cgroup_unavailable() -> Option<String> {
    if !cfg!(target_os = "linux") {
        return Some("cgroups are Linux-only".to_string());
    }
    let script = "d=/sys/fs/cgroup$(sed -n 's/^0:://p' /proc/self/cgroup)/murmur-ro-probe-$$; \
                  if mkdir \"$d\" 2>&1; then rmdir \"$d\"; echo WRITABLE; fi";
    let output = command(
        true,
        Path::new("/"),
        Path::new("/"),
        Path::new("sh"),
        &["-c", script],
    )
    .output();
    match output {
        Err(error) => Some(format!("systemd-run or unshare could not be run: {error}")),
        Ok(output) => {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            if text.contains("Read-only file system") {
                None
            } else {
                Some(format!(
                    "the read-only /sys/fs/cgroup construction did not refuse a mkdir with EROFS: \
                     {}",
                    text.trim()
                ))
            }
        }
    }
}

fn skip_without_read_only_cgroup(test_name: &str) -> bool {
    match read_only_cgroup_unavailable() {
        Some(why) => {
            eprintln!("[SKIP-HOST] {test_name}: {why}");
            true
        }
        None => false,
    }
}

/// A script capsule declaring `capabilities` (YAML under the key, may be empty).
fn script_project(capabilities: &str) -> TempDir {
    let project = TempDir::new().unwrap();
    fs::write(
        project.path().join("murmur.yaml"),
        format!("name: cgroup-scope-capsule\nversion: 0.1.0\n{capabilities}"),
    )
    .unwrap();
    fs::copy(
        common::fixture_path("run/components/capsule-allowlisted.wasm"),
        project.path().join("capsule.wasm"),
    )
    .unwrap();
    project
}

const SHELL_CAPABILITIES: &str = "capabilities:\n  shell:\n    allow:\n      - bash\n";

/// The `cgroup scope:` line of a text report or of doctor's stdout.
fn cgroup_line(stdout: &str) -> &str {
    stdout
        .lines()
        .find(|line| line.trim_start().starts_with("cgroup scope:"))
        .unwrap_or_else(|| panic!("no `cgroup scope:` line in:\n{stdout}"))
}

/// What each step of [`IN_ONE_SCOPE`] printed: `<name>.out`, `<name>.err` and `<name>.code`.
fn step(out: &Path, name: &str) -> Run {
    let read = |suffix: &str| fs::read_to_string(out.join(format!("{name}.{suffix}"))).unwrap();
    Run {
        code: read("code").trim().parse().ok(),
        stdout: read("out"),
        stderr: read("err"),
    }
}

/// The four invocations, run one after another by one shell inside one construction, so they
/// share one cgroup and a reason naming its path names the same path in each.
const IN_ONE_SCOPE: &str = r#"
run() { name=$1; shift; "$MUR" "$@" > "$OUT/$name.out" 2> "$OUT/$name.err"; echo $? > "$OUT/$name.code"; }
run explain-json run --manifest murmur.yaml --explain-scope --json
run explain-text run --manifest murmur.yaml --explain-scope
run doctor doctor
run launch run --manifest murmur.yaml
"#;

/// A capsule that runs subprocesses, on a host whose cgroup hierarchy is read-only: the report
/// predicts the `E-RUN-012` refusal with the errno and the reason the refusal prints, the class
/// and the floor are untouched, `mur doctor` fails the check, and the launch is refused.
#[test]
fn a_read_only_cgroup_hierarchy_is_reported_as_the_refusal_it_causes() {
    let test = "a_read_only_cgroup_hierarchy_is_reported_as_the_refusal_it_causes";
    if skip_without_read_only_cgroup(test) {
        return;
    }
    let home = TempDir::new().unwrap();
    let project = script_project(SHELL_CAPABILITIES);
    let cwd = project.path();
    let out = TempDir::new().unwrap();
    let status = command(
        true,
        home.path(),
        cwd,
        Path::new("sh"),
        &["-c", IN_ONE_SCOPE],
    )
    .env("MUR", mur_path())
    .env("OUT", out.path())
    .status()
    .unwrap();
    assert!(
        status.success(),
        "the construction's shell failed: {status}"
    );

    let refused = step(out.path(), "explain-json");
    assert_eq!(refused.code, Some(0), "{}", refused.all());
    let report = refused.json();
    let scope = &report["cgroup_scope"];
    assert_eq!(scope["status"], "unavailable", "{report}");
    assert_eq!(scope["errno"], "EROFS", "{report}");
    let reason = scope["reason"]
        .as_str()
        .expect("an unavailable scope names its reason");
    assert!(reason.contains("Read-only file system"), "{reason}");

    // The class a session's commands would run under is the same as outside: a refused capsule is
    // carried by `cgroup_scope`, not by a lowered class.
    let outside = mur(
        false,
        home.path(),
        cwd,
        &[
            "run",
            "--manifest",
            "murmur.yaml",
            "--explain-scope",
            "--json",
        ],
    );
    assert_eq!(outside.code, Some(0), "{}", outside.all());
    let outside_report = outside.json();
    for key in [
        "achieved_containment",
        "floor_met",
        "enforcement_tier",
        "host_containment",
    ] {
        assert_eq!(report[key], outside_report[key], "{key}: {report}");
    }

    let text = step(out.path(), "explain-text");
    assert_eq!(text.code, Some(0), "{}", text.all());
    assert_eq!(
        cgroup_line(&text.stdout),
        format!("  cgroup scope: unavailable (EROFS) \u{2014} {reason}"),
        "{}",
        text.all()
    );
    assert!(text.stdout.contains("  floor met: yes\n"), "{}", text.all());
    assert!(
        text.stdout.contains(
            "This is a report only — `mur run` without --explain-scope would refuse to launch \
             here with E-RUN-012"
        ),
        "{}",
        text.all()
    );

    let doctor = step(out.path(), "doctor");
    assert_eq!(
        cgroup_line(&doctor.stdout),
        format!("  cgroup scope: unavailable (EROFS) \u{2014} {reason}"),
        "{}",
        doctor.all()
    );
    assert!(
        doctor
            .stdout
            .contains("  refused: `mur run` refuses this capsule here with E-RUN-012"),
        "{}",
        doctor.all()
    );
    assert!(
        !doctor.stdout.contains("All checks passed."),
        "{}",
        doctor.all()
    );
    assert!(
        doctor
            .stdout
            .lines()
            .any(|line| line.starts_with("Fix: give `mur` a delegated cgroup v2 scope")),
        "{}",
        doctor.all()
    );
    assert_eq!(doctor.code, Some(1), "{}", doctor.all());

    let launch = step(out.path(), "launch");
    assert_ne!(launch.code, Some(0), "{}", launch.all());
    assert!(
        launch.stderr.contains("error[E-RUN-012]"),
        "{}",
        launch.all()
    );
    assert!(
        launch.stderr.contains(&format!("({reason})")),
        "the refusal must carry the reason the report printed, {reason:?}:\n{}",
        launch.all()
    );
}

/// Outside the construction, on a host that delegates, the same capsule's scope is `delegated`
/// and no footer prints.
#[test]
fn a_delegated_host_reports_the_scope_delegated() {
    let test = "a_delegated_host_reports_the_scope_delegated";
    if !cfg!(target_os = "linux") || !common::cgroup_delegation_available() {
        eprintln!("[SKIP-HOST] {test}: this host cannot delegate a cgroup v2 scope");
        return;
    }
    let home = TempDir::new().unwrap();
    let project = script_project(SHELL_CAPABILITIES);
    let cwd = project.path();

    let report = mur(
        false,
        home.path(),
        cwd,
        &[
            "run",
            "--manifest",
            "murmur.yaml",
            "--explain-scope",
            "--json",
        ],
    )
    .json();
    assert_eq!(
        report["cgroup_scope"],
        json!({"status": "delegated"}),
        "{report}"
    );

    let text = mur(
        false,
        home.path(),
        cwd,
        &["run", "--manifest", "murmur.yaml", "--explain-scope"],
    );
    assert_eq!(cgroup_line(&text.stdout), "  cgroup scope: delegated");
    assert!(
        !text.stdout.contains("This is a report only"),
        "{}",
        text.all()
    );

    let doctor = mur(false, home.path(), cwd, &["doctor"]);
    assert_eq!(
        cgroup_line(&doctor.stdout),
        "  cgroup scope: delegated",
        "{}",
        doctor.all()
    );
    assert!(!doctor.stdout.contains("E-RUN-012"), "{}", doctor.all());
}

/// A capsule that runs no subprocess needs no scope: inside the construction its report says
/// `not-required` with no footer, and doctor prints the host's answer without failing on it.
#[test]
fn a_capsule_with_no_subprocess_route_is_not_refused_for_a_read_only_hierarchy() {
    let test = "a_capsule_with_no_subprocess_route_is_not_refused_for_a_read_only_hierarchy";
    if skip_without_read_only_cgroup(test) {
        return;
    }
    let home = TempDir::new().unwrap();
    let project = script_project("");
    let cwd = project.path();

    let report = mur(
        true,
        home.path(),
        cwd,
        &[
            "run",
            "--manifest",
            "murmur.yaml",
            "--explain-scope",
            "--json",
        ],
    )
    .json();
    assert_eq!(report["cgroup_scope"]["status"], "not-required", "{report}");
    assert!(report["cgroup_scope"].get("errno").is_none(), "{report}");

    let text = mur(
        true,
        home.path(),
        cwd,
        &["run", "--manifest", "murmur.yaml", "--explain-scope"],
    );
    assert!(
        cgroup_line(&text.stdout).starts_with("  cgroup scope: not-required"),
        "{}",
        text.all()
    );
    assert!(
        !text.stdout.contains("This is a report only"),
        "{}",
        text.all()
    );

    let doctor = mur(true, home.path(), cwd, &["doctor"]);
    assert!(
        cgroup_line(&doctor.stdout).starts_with("  cgroup scope: unavailable (EROFS)"),
        "{}",
        doctor.all()
    );
    assert!(
        doctor
            .stdout
            .contains("  this capsule runs no subprocess, so a launch creates no scope for it"),
        "{}",
        doctor.all()
    );
    assert!(
        !doctor
            .stdout
            .lines()
            .any(|line| line.starts_with("Fix:") && line.contains("cgroup")),
        "{}",
        doctor.all()
    );
    assert!(!doctor.stdout.contains("refused: "), "{}", doctor.all());
}

/// An agent capsule with a WASM driver and a WASM tool, at `containment`, its driver pointed at
/// `endpoint`.
fn agent_project(containment: &str, endpoint: &str) -> TempDir {
    let project = TempDir::new().unwrap();
    fs::write(
        project.path().join("murmur.yaml"),
        format!(
            "name: route-capsule\nversion: 0.1.0\nartifacts:\n  - name: {DRIVER_NAME}\n    \
             version: {DRIVER_VERSION}\n    runtime: driver\n    gateway:\n      endpoint: \
             {endpoint}\n      api_key: test-key\n  - name: {TOOL_NAME}\n    version: \
             {TOOL_VERSION}\n    runtime: tool\ncapabilities:\n  containment: {containment}\n  \
             network:\n    allow:\n      - {endpoint}\ninference:\n  transport: http\n  model: \
             test-model\n  driver:\n    artifact: {DRIVER_NAME}\n"
        ),
    )
    .unwrap();
    project
}

fn publish_agent_artifacts(home: &TempDir) {
    let artifacts = TempDir::new().unwrap();
    let driver = common::create_driver_artifact(
        artifacts.path(),
        DRIVER_NAME,
        DRIVER_VERSION,
        &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(home, &driver).success();
    let tool = common::create_tool_artifact(
        artifacts.path(),
        TOOL_NAME,
        TOOL_VERSION,
        &common::fixture_path("graduation/tool/jsonl-line-count.wasm"),
    );
    common::publish_local(home, &tool).success();
}

fn end_turn_response() -> String {
    json!({
        "id": "msg_et",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": "done"}],
        "stop_reason": "end_turn",
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": 8, "output_tokens": 3},
    })
    .to_string()
}

fn uncovered_warnings(stderr: &str) -> Vec<&str> {
    stderr
        .lines()
        .filter(|line| line.contains("warning[W-SEC-034]"))
        .collect()
}

/// A `sealed` capsule whose driver and tool are WASM runs both in guest workers under the class:
/// neither `mur doctor` nor its launch prints `W-SEC-034`, and the routes say the tier holds them.
/// An `advisory` capsule gets none either.
#[test]
fn a_sealed_wasm_capsule_gets_no_uncovered_route_warning() {
    let test = "a_sealed_wasm_capsule_gets_no_uncovered_route_warning";
    let home = TempDir::new().unwrap();
    publish_agent_artifacts(&home);

    let project = agent_project("sealed", "http://127.0.0.1:9");
    let doctor = mur(false, home.path(), project.path(), &["doctor"]);
    assert!(
        uncovered_warnings(&doctor.stderr).is_empty(),
        "{}",
        doctor.all()
    );
    assert!(doctor.stdout.contains("\nRoutes\n"), "{}", doctor.all());

    let advisory = agent_project("advisory", "http://127.0.0.1:9");
    let doctor = mur(false, home.path(), advisory.path(), &["doctor"]);
    assert!(
        uncovered_warnings(&doctor.stderr).is_empty(),
        "{}",
        doctor.all()
    );

    let report = mur(
        false,
        home.path(),
        project.path(),
        &[
            "run",
            "--manifest",
            "murmur.yaml",
            "--explain-scope",
            "--json",
        ],
    )
    .json();
    let routes = report["routes"].as_array().expect("routes");
    for name in ["wasm-tools", "drivers"] {
        let row = routes
            .iter()
            .find(|row| row["route"] == name)
            .unwrap_or_else(|| panic!("no {name} row: {report}"));
        assert_eq!(row["present"], true, "{row}");
        assert_eq!(row["under_class"], true, "{row}");
        assert_eq!(row["mechanism"], report["enforcement_tier"], "{row}");
        assert!(row["note"].is_null(), "{row}");
    }
    if report["floor_met"] != true {
        eprintln!(
            "[SKIP-HOST] {test}: this host does not reach sealed ({}), so the launch half is not \
             run",
            report["shortfall_reason"]
        );
        return;
    }

    for (containment, expected) in [("sealed", 0), ("advisory", 0)] {
        let server = common::ScriptedServer::start(vec![end_turn_response()]);
        let project = agent_project(containment, &server.endpoint);
        let run = mur(
            false,
            home.path(),
            project.path(),
            &[
                "run",
                "--manifest",
                "murmur.yaml",
                "--task",
                "Report and stop.",
            ],
        );
        assert_eq!(run.code, Some(0), "{containment}: {}", run.all());
        assert_eq!(
            uncovered_warnings(&run.stderr).len(),
            expected,
            "{containment}: {}",
            run.all()
        );
        // The project directory is canonical in the records group's mirror, as the runtime
        // resolves it.
        let workdir_root = project.path().canonicalize().unwrap().join("workdir");
        let log = common::newest_session_records(home.path(), &workdir_root)
            .and_then(|records| fs::read_to_string(records.join("logs").join("bootstrap.log")).ok())
            .unwrap_or_default();
        assert_eq!(
            log.matches("warning[W-SEC-034]").count(),
            expected,
            "{containment}: bootstrap log was:\n{log}\n{}",
            run.all()
        );
    }
}

/// `inference.transport: process` puts the harness and its own tools on the table, under the
/// class at its installed tier; the process driver runs in the driver worker under the class too,
/// so at `scoped` doctor has no `W-SEC-034` to give.
#[test]
fn a_process_transport_capsule_reports_the_harness_routes() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    fs::write(
        project.path().join("murmur.yaml"),
        format!(
            "name: harness-capsule\nversion: 0.1.0\nartifacts:\n  - name: {PROCESS_DRIVER_NAME}\n    \
             version: 0.1.0\n    runtime: driver\ncapabilities:\n  containment: scoped\n\
             inference:\n  transport: process\n  model: test-model\n  driver:\n    artifact: \
             {PROCESS_DRIVER_NAME}\n"
        ),
    )
    .unwrap();

    let explain = mur(
        false,
        home.path(),
        project.path(),
        &[
            "run",
            "--manifest",
            "murmur.yaml",
            "--explain-scope",
            "--json",
        ],
    );
    let report = explain.json();
    let routes = report["routes"].as_array().expect("routes");
    for name in ["process-harness", "harness-tools"] {
        let row = routes
            .iter()
            .find(|row| row["route"] == name)
            .unwrap_or_else(|| panic!("no {name} row: {report}"));
        assert_eq!(row["present"], true, "{row}");
        assert_eq!(row["under_class"], true, "{row}");
        assert_eq!(row["mechanism"], report["enforcement_tier"], "{row}");
        assert_ne!(row["mechanism"], "host-process", "{row}");
        assert!(row["note"].is_null(), "{row}");
    }

    let doctor = mur(false, home.path(), project.path(), &["doctor"]);
    let warnings = uncovered_warnings(&doctor.stderr);
    assert!(warnings.is_empty(), "{}", doctor.all());
}

/// A capsule whose only route is a WASM tool and a driver (no `shell.allow`, no `spawn.allow`)
/// runs them in guest worker processes, so it needs a cgroup scope exactly as a shell capsule
/// does: inside the read-only construction its report says the scope is required and refused,
/// and its launch is refused with `E-RUN-012` before any session folder exists, so before any
/// worker could be started (workers start once the work folder and records exist).
#[test]
fn worker_only_capsule_needs_cgroup() {
    let test = "worker_only_capsule_needs_cgroup";
    if skip_without_read_only_cgroup(test) {
        return;
    }
    if capsule_runtime::skip_without_egress_namespace(test) {
        return;
    }
    let home = TempDir::new().unwrap();
    publish_agent_artifacts(&home);
    let project = agent_project("scoped", "http://127.0.0.1:9");

    let report = mur(
        true,
        home.path(),
        project.path(),
        &[
            "run",
            "--manifest",
            "murmur.yaml",
            "--explain-scope",
            "--json",
        ],
    )
    .json();
    assert_ne!(report["cgroup_scope"]["status"], "not-required", "{report}");

    let launch = mur(
        true,
        home.path(),
        project.path(),
        &[
            "run",
            "--manifest",
            "murmur.yaml",
            "--task",
            "Report and stop.",
        ],
    );
    assert_ne!(launch.code, Some(0), "{}", launch.all());
    assert!(
        launch.stderr.contains("error[E-RUN-012]"),
        "{}",
        launch.all()
    );
    assert!(
        !project.path().join("workdir").exists(),
        "a refused launch creates no work folder: {}",
        launch.all()
    );
    for folder in ["sessions", "records"] {
        let path = home.path().join(".murmur").join(folder);
        let entries = fs::read_dir(&path)
            .map(|entries| entries.count())
            .unwrap_or(0);
        assert_eq!(
            entries,
            0,
            "a refused launch creates nothing under {}: {}",
            path.display(),
            launch.all()
        );
    }
}
