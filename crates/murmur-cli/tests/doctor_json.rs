//! `mur doctor --json`: one document on stdout, the text report on stderr, and the `Process
//! harness` block of a `transport: process` capsule, against real stores in temp directories and
//! the fixture process driver driving its fake harness.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use assert_cmd::Command;
use murmur_artifact::LockOrigin;
use serde_json::Value;
use tempfile::TempDir;

const DRIVER: &str = "local/fixture-process-driver";
const DRIVER_VERSION: &str = "0.1.0";

/// The credential-shaped value the fake harness prints under the `logged-out` profile.
const HARNESS_SECRET: &str = "fixture-secret-0d1e2f";

/// `mur doctor`, with `--json` when `json`, under `home`, from `cwd`, with `env` set.
fn doctor(home: &Path, cwd: &Path, json: bool, env: &[(&str, &str)]) -> Output {
    let mut command = Command::cargo_bin("mur").unwrap();
    command
        .env("HOME", home)
        .env_remove("MURMUR_ROOST_URL")
        .current_dir(cwd)
        .timeout(Duration::from_secs(180))
        .arg("doctor");
    if json {
        command.arg("--json");
    }
    for (name, value) in env {
        command.env(name, value);
    }
    command.output().unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The one JSON value stdout holds, failing when it holds anything else.
fn document(output: &Output) -> Value {
    let text = stdout(output);
    let mut values = serde_json::Deserializer::from_str(&text).into_iter::<Value>();
    let value = values
        .next()
        .unwrap_or_else(|| {
            panic!(
                "stdout holds no JSON value:\n{text}\nstderr:\n{}",
                stderr(output)
            )
        })
        .unwrap_or_else(|error| panic!("stdout is not JSON ({error}):\n{text}"));
    assert!(
        values.next().is_none(),
        "stdout holds more than one value:\n{text}"
    );
    assert!(text.ends_with("}\n"), "{text:?}");
    value
}

/// The document's invariants: every finding has a code, the counts are the findings', the verdict
/// follows the errors, and the process exited with `exit_code`.
fn assert_consistent(output: &Output, report: &Value) {
    let findings = report["findings"].as_array().expect("findings is an array");
    let count = |severity: &str| {
        findings
            .iter()
            .filter(|finding| finding["severity"] == severity)
            .count() as u64
    };
    assert_eq!(report["errors"], count("error"), "{report:#}");
    assert_eq!(report["warnings"], count("warning"), "{report:#}");
    for finding in findings {
        let code = finding["code"].as_str().expect("every finding has a code");
        assert!(
            code.starts_with("E-") || code.starts_with("W-"),
            "{finding}"
        );
        assert!(finding["block"].is_string(), "{finding}");
        assert!(finding["message"].is_string(), "{finding}");
        assert!(finding["fix"].is_string(), "{finding}");
        assert!(
            finding["subject"].is_string() || finding["subject"].is_null(),
            "{finding}"
        );
    }
    let refused = report["errors"].as_u64().unwrap() > 0;
    assert_eq!(
        report["verdict"],
        if refused { "refused" } else { "ready" },
        "{report:#}"
    );
    assert_eq!(report["exit_code"], i64::from(refused), "{report:#}");
    assert_eq!(
        output.status.code(),
        Some(i32::from(refused)),
        "{report:#}\nstderr:\n{}",
        stderr(output)
    );
}

fn write_manifest(project: &Path, body: &str) {
    fs::write(
        project.join("murmur.yaml"),
        format!("name: doctor-json\nversion: 0.0.1\n{body}"),
    )
    .unwrap();
}

/// A project declaring `local/pulled-skill@0.1.0`, installed and pinned in `murmur.lock` from a
/// runtime pull by session `ses_skill`, and, with `missing`, `local/absent-tool@0.1.0`, which no
/// store holds.
fn pulled_and_missing(home: &TempDir, project: &Path, missing: bool) {
    let staging = TempDir::new().unwrap();
    let skill = common::create_skill_artifact(
        staging.path(),
        "local/pulled-skill",
        "0.1.0",
        "# guidance\n",
    );
    common::publish_local(home, &skill).success();
    let skill_entry = "  - name: local/pulled-skill\n    version: 0.1.0\n    runtime: skill\n";
    write_manifest(project, &format!("artifacts:\n{skill_entry}"));
    Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .current_dir(project)
        .args(["install", "local/pulled-skill@0.1.0"])
        .assert()
        .success();
    let lock_path = project.join("murmur.lock");
    let mut lock = murmur_artifact::read_lockfile(&lock_path).unwrap();
    for entry in &mut lock.artifacts {
        entry.origin = LockOrigin::Runtime {
            session: "ses_skill".to_string(),
        };
    }
    murmur_artifact::write_lockfile_atomic(&lock_path, &lock).unwrap();
    if missing {
        write_manifest(
            project,
            &format!(
                "artifacts:\n  - name: local/absent-tool\n    version: 0.1.0\n    runtime: tool\n\
                 {skill_entry}"
            ),
        );
    }
}

/// S1: a missing artifact is the first refusal, and the runtime-pulled pin is a `W-REG-004`
/// warning after it; the text report is on stderr.
#[test]
fn a_missing_artifact_and_a_warning_are_refused() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    pulled_and_missing(&home, project.path(), true);

    let output = doctor(home.path(), project.path(), true, &[]);
    let report = document(&output);
    assert_consistent(&output, &report);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(report["verdict"], "refused");
    assert_eq!(report["exit_code"], 1);
    assert_eq!(report["errors"], 1, "{report:#}");
    assert_eq!(report["warnings"], 1, "{report:#}");
    let findings = report["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 2, "{report:#}");
    assert_eq!(findings[0]["severity"], "error");
    assert_eq!(findings[0]["code"], "E-RUN-008");
    assert_eq!(findings[0]["block"], "artifacts");
    assert_eq!(findings[0]["subject"], "local/absent-tool@0.1.0");
    assert_eq!(
        findings[0]["message"],
        "local/absent-tool@0.1.0 \u{2014} missing"
    );
    assert_eq!(findings[0]["fix"], "mur install local/absent-tool@0.1.0");
    assert_eq!(findings[1]["severity"], "warning");
    assert_eq!(findings[1]["code"], "W-REG-004");
    assert_eq!(findings[1]["block"], "artifacts");
    assert_eq!(findings[1]["subject"], "local/pulled-skill@0.1.0");
    assert_eq!(
        findings[1]["message"],
        "local/pulled-skill@0.1.0 \u{2014} pulled at runtime by session ses_skill"
    );
    assert_eq!(findings[1]["fix"], "mur install local/pulled-skill@0.1.0");

    let text = stderr(&output);
    assert!(text.contains("Checking "), "{text}");
    assert!(
        text.contains("1 check passed, 1 error found, 1 warning."),
        "{text}"
    );
    assert!(
        text.contains("First refusal: local/absent-tool@0.1.0 \u{2014} missing"),
        "{text}"
    );
}

/// S2: nothing found is `ready`, exit `0`, no findings, and the text report still ends
/// `All checks passed.` on stderr.
#[test]
fn a_clean_project_is_ready() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_manifest(project.path(), "artifacts: []\n");

    let output = doctor(home.path(), project.path(), true, &[]);
    let report = document(&output);
    assert_consistent(&output, &report);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(report["verdict"], "ready");
    assert_eq!(report["exit_code"], 0);
    assert_eq!(report["errors"], 0);
    assert_eq!(report["warnings"], 0);
    assert_eq!(report["findings"], serde_json::json!([]));
    assert!(
        stderr(&output).ends_with("All checks passed.\n"),
        "{}",
        stderr(&output)
    );
}

/// The text form's tally numbers and its `Fix:` lines, in order.
fn text_tally(text: &str) -> (Vec<u64>, Vec<String>) {
    let tally = text
        .lines()
        .find(|line| line.contains(" passed, "))
        .unwrap_or_else(|| panic!("no tally in:\n{text}"));
    let numbers = tally
        .split(|c: char| !c.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .map(|part| part.parse().unwrap())
        .collect();
    let fixes = text
        .lines()
        .filter_map(|line| line.strip_prefix("Fix: "))
        .map(str::to_string)
        .collect();
    (numbers, fixes)
}

/// S3: the document's findings are the text run's `Fix:` lines, in order, and its counts are the
/// tally's; a warning alone leaves the verdict `ready`.
#[test]
fn the_json_findings_are_the_fix_lines_in_order() {
    for missing in [true, false] {
        let home = TempDir::new().unwrap();
        let project = TempDir::new().unwrap();
        pulled_and_missing(&home, project.path(), missing);

        let text_run = doctor(home.path(), project.path(), false, &[]);
        let (numbers, fixes) = text_tally(&stdout(&text_run));
        let json_run = doctor(home.path(), project.path(), true, &[]);
        let report = document(&json_run);
        assert_consistent(&json_run, &report);
        assert_eq!(text_run.status.code(), json_run.status.code());
        let json_fixes: Vec<String> = report["findings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|finding| finding["fix"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(json_fixes, fixes, "{report:#}");
        let expected = if missing {
            vec![1, 1, 1]
        } else {
            vec![1, 0, 1]
        };
        assert_eq!(numbers, expected);
        assert_eq!(
            vec![
                report["checks_passed"].as_u64().unwrap(),
                report["errors"].as_u64().unwrap(),
                report["warnings"].as_u64().unwrap()
            ],
            expected
        );
        if !missing {
            assert_eq!(report["verdict"], "ready");
            assert_eq!(json_run.status.code(), Some(0));
        }
        // Every line the text form prints on stdout is on stderr under `--json`, in order, among
        // the lines both forms print on stderr.
        let json_stderr = stderr(&json_run);
        let mut remaining = json_stderr.lines();
        for line in stdout(&text_run).lines() {
            assert!(
                remaining.any(|candidate| candidate == line),
                "{line:?} is missing from, or out of order in, the --json stderr:\n{json_stderr}"
            );
        }
    }
}

/// A `transport: process` project whose driver is the fixture `wasm`, published to `home`'s
/// global store, with `inference.command` naming `command`.
struct ProcessProject {
    home: TempDir,
    project: TempDir,
    /// Holds the fake harness copy.
    _bin: TempDir,
}

impl ProcessProject {
    /// The default fixture build and a copy of the fake harness.
    fn new() -> Self {
        Self::with_driver("process-driver/tool/process-driver.wasm", None)
    }

    /// `wasm` (relative to the fixtures folder), and `command` as `inference.command`, or a fresh
    /// copy of the fake harness when `None`.
    fn with_driver(wasm: &str, command: Option<&Path>) -> Self {
        let home = TempDir::new().unwrap();
        let project = TempDir::new().unwrap();
        let bin = TempDir::new().unwrap();
        let staging = TempDir::new().unwrap();
        let artifact = common::create_driver_artifact_with_auth(
            staging.path(),
            DRIVER,
            DRIVER_VERSION,
            &common::fixture_path(wasm),
            "",
        );
        common::publish_local(&home, &artifact).success();
        let command = match command {
            Some(command) => command.to_path_buf(),
            None => fake_harness(bin.path()),
        };
        write_manifest(
            project.path(),
            &format!(
                "artifacts:\n  - name: {DRIVER}\n    version: {DRIVER_VERSION}\n    runtime: driver\n\
                 capabilities:\n  env:\n    allow: [HOME, FIXTURE_HARNESS_PROFILE]\n\
                 inference:\n  transport: process\n  driver:\n    artifact: {DRIVER}\n  \
                 command: {}\n",
                command.display()
            ),
        );
        Self {
            home,
            project,
            _bin: bin,
        }
    }

    fn doctor(&self, json: bool, profile: &str) -> Output {
        doctor(
            self.home.path(),
            self.project.path(),
            json,
            &[("FIXTURE_HARNESS_PROFILE", profile)],
        )
    }
}

/// A copy of the fixture's fake harness in `dir`, mode 0755.
fn fake_harness(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("fixture-cli");
    fs::copy(common::fixture_path("process-driver/fake-harness"), &path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// The findings of `block` in `report`.
fn block_findings<'a>(report: &'a Value, block: &str) -> Vec<&'a Value> {
    report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|finding| finding["block"] == block)
        .collect()
}

/// S4: a harness binary that does not resolve is staging's `E-RUN-006`, and no version or login
/// is asked.
#[test]
fn a_harness_that_does_not_resolve_is_refused() {
    let missing_dir = TempDir::new().unwrap();
    let missing = missing_dir.path().join("no-such-harness");
    let capsule =
        ProcessProject::with_driver("process-driver/tool/process-driver.wasm", Some(&missing));

    let output = capsule.doctor(true, "happy");
    let report = document(&output);
    assert_consistent(&output, &report);
    assert_eq!(report["verdict"], "refused");
    let harness = block_findings(&report, "harness");
    assert_eq!(harness.len(), 1, "{report:#}");
    assert_eq!(harness[0]["code"], "E-RUN-006");
    assert_eq!(harness[0]["severity"], "error");
    assert_eq!(harness[0]["subject"], missing.to_str().unwrap());
    assert!(
        harness[0]["fix"]
            .as_str()
            .unwrap()
            .contains("point inference.command at the executable"),
        "{report:#}"
    );
    let text = stderr(&output);
    assert!(
        text.contains(&format!(
            "Process harness ({DRIVER}@{DRIVER_VERSION}, harness fixture-harness):\n  \u{2717}  error[E-RUN-006]: "
        )),
        "{text}"
    );
    assert!(!text.contains("  login    "), "{text}");
    assert!(!text.contains("  version  "), "{text}");
}

/// S5: a harness the driver reads as logged out is `E-RUN-064`, and nothing the harness printed
/// reaches either output form.
#[test]
fn a_logged_out_harness_is_refused() {
    let capsule = ProcessProject::new();

    let output = capsule.doctor(true, "logged-out");
    let report = document(&output);
    assert_consistent(&output, &report);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(report["verdict"], "refused");
    let harness = block_findings(&report, "harness");
    assert_eq!(harness.len(), 1, "{report:#}");
    assert_eq!(harness[0]["code"], "E-RUN-064");
    assert_eq!(harness[0]["subject"], "fixture-harness");
    let message = "fixture-harness is not logged in for this user: fixture-harness is not logged \
                   in for this user; run fixture-cli auth login";
    assert_eq!(harness[0]["message"], message);
    assert_eq!(
        report["findings"][0]["code"], "E-RUN-064",
        "the login refusal is the first refusal: {report:#}"
    );

    let text_run = capsule.doctor(false, "logged-out");
    assert_eq!(text_run.status.code(), Some(1));
    let text = stdout(&text_run);
    assert!(
        text.contains(&format!(
            "Process harness ({DRIVER}@{DRIVER_VERSION}, harness fixture-harness):\n"
        )),
        "{text}"
    );
    assert!(text.contains("  \u{2713}  binary   "), "{text}");
    assert!(
        text.contains("  \u{2713}  version  fixture-harness 1.0.0, tested\n"),
        "{text}"
    );
    assert!(
        text.contains(&format!("  \u{2717}  error[E-RUN-064]: {message}\n")),
        "{text}"
    );
    assert!(
        text.contains(&format!("First refusal: error[E-RUN-064]: {message}")),
        "{text}"
    );
    for stream in [
        stdout(&output),
        stderr(&output),
        stdout(&text_run),
        stderr(&text_run),
    ] {
        assert!(!stream.contains(HARNESS_SECRET), "{stream}");
    }
}

/// S6: a logged-in harness at a tested version adds nothing to the tally.
#[test]
fn login_logged_in_adds_no_finding() {
    let capsule = ProcessProject::new();
    let output = capsule.doctor(true, "happy");
    let report = document(&output);
    assert_consistent(&output, &report);
    assert!(block_findings(&report, "harness").is_empty(), "{report:#}");
    let text = stderr(&output);
    assert!(text.contains("  \u{2713}  login    logged in\n"), "{text}");
    assert!(
        text.contains("  \u{2713}  version  fixture-harness 1.0.0, tested\n"),
        "{text}"
    );
}

/// S6: a driver that names no login command is a `W-RUN-013` warning, and the verdict stays
/// `ready`.
#[test]
fn login_not_checked_without_a_login_command_is_a_warning() {
    let capsule = ProcessProject::with_driver(
        "process-driver/tool/process-driver-no-login-check.wasm",
        None,
    );
    let output = capsule.doctor(true, "happy");
    let report = document(&output);
    assert_consistent(&output, &report);
    assert_eq!(report["verdict"], "ready", "{report:#}");
    assert_eq!(output.status.code(), Some(0));
    let harness = block_findings(&report, "harness");
    assert_eq!(harness.len(), 1, "{report:#}");
    assert_eq!(harness[0]["code"], "W-RUN-013");
    assert_eq!(harness[0]["severity"], "warning");
    assert_eq!(
        harness[0]["message"],
        "login not checked \u{2014} fixture-harness has no way to report its login without a turn"
    );
    assert_eq!(
        harness[0]["fix"],
        "check by hand that the harness is logged in as this user on this host"
    );
}

/// S6: an answer the driver cannot read is a `W-RUN-013` warning.
#[test]
fn login_unreadable_is_a_warning() {
    let capsule = ProcessProject::new();
    let output = capsule.doctor(true, "login-unreadable");
    let report = document(&output);
    assert_consistent(&output, &report);
    assert_eq!(report["verdict"], "ready", "{report:#}");
    assert_eq!(output.status.code(), Some(0));
    let harness = block_findings(&report, "harness");
    assert_eq!(harness.len(), 1, "{report:#}");
    assert_eq!(harness[0]["code"], "W-RUN-013");
    assert_eq!(
        harness[0]["message"],
        "login not checked \u{2014} fixture-harness printed no loggedIn field"
    );
}

/// S6: an untested harness version is the `W-RUN-002` warning `mur run` prints, beside a login
/// check that passes.
#[test]
fn login_checked_beside_an_untested_version_warning() {
    let capsule = ProcessProject::new();
    let output = capsule.doctor(true, "old-version");
    let report = document(&output);
    assert_consistent(&output, &report);
    assert_eq!(report["verdict"], "ready", "{report:#}");
    assert_eq!(output.status.code(), Some(0));
    let harness = block_findings(&report, "harness");
    assert_eq!(harness.len(), 1, "{report:#}");
    assert_eq!(harness[0]["code"], "W-RUN-002");
    assert_eq!(harness[0]["subject"], "fixture-harness");
    assert_eq!(
        harness[0]["message"],
        "the harness reports version 'fixture-harness 0.9.0', which this process driver was not \
         tested against (tested against 1.0.0)"
    );
    assert!(
        stderr(&output).contains("  \u{2713}  login    logged in\n"),
        "{}",
        stderr(&output)
    );
}

/// An installed driver built against `murmur:driver@0.4.0` fails the checklist, and the harness
/// block says the driver did not pass it rather than that it is missing, adding no finding of its
/// own.
#[test]
fn a_level_4_driver_fails_the_checklist_and_the_harness_is_not_checked() {
    let capsule =
        ProcessProject::with_driver("process-driver-v4/tool/process-driver-v4.wasm", None);
    let output = capsule.doctor(true, "happy");
    let report = document(&output);
    assert_consistent(&output, &report);
    assert_eq!(report["verdict"], "refused");
    let findings = report["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 1, "{report:#}");
    assert_eq!(findings[0]["code"], "E-WIT-LEVEL-001");
    assert_eq!(findings[0]["block"], "artifacts");
    assert!(block_findings(&report, "harness").is_empty(), "{report:#}");
    let text = stderr(&output);
    assert!(
        text.contains(&format!(
            "Process harness: not checked, driver {DRIVER} did not pass the checklist\n"
        )),
        "{text}"
    );
}

/// A driver declared with `source:` is not loaded, and the harness block says so.
#[test]
fn a_local_source_driver_is_not_checked() {
    let capsule = ProcessProject::new();
    let manifest = capsule.project.path().join("murmur.yaml");
    // A `source:` entry is named by its bare name and declares `local_source: true`.
    let body = fs::read_to_string(&manifest)
        .unwrap()
        .replace(DRIVER, "fixture-process-driver")
        .replace(
            "    runtime: driver\n",
            "    runtime: driver\n    source: ./driver\n    local_source: true\n",
        );
    fs::write(&manifest, body).unwrap();
    fs::create_dir(capsule.project.path().join("driver")).unwrap();
    let output = capsule.doctor(false, "happy");
    let text = stdout(&output);
    assert!(
        text.contains("  fixture-process-driver@local   local source\n"),
        "{text}\nstderr:\n{}",
        stderr(&output)
    );
    assert!(
        text.contains(
            "Process harness: not checked, driver fixture-process-driver is a local source\n"
        ),
        "{text}"
    );
}

/// S8: no project is still one document: `refused`, the error's code, block `project`.
#[test]
fn no_project_is_one_refused_document() {
    let home = TempDir::new().unwrap();
    let empty = TempDir::new().unwrap();
    let output = doctor(home.path(), empty.path(), true, &[]);
    let report = document(&output);
    assert_consistent(&output, &report);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(report["verdict"], "refused");
    let findings = report["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 1, "{report:#}");
    assert_eq!(findings[0]["code"], "E-IO-001");
    assert_eq!(findings[0]["block"], "project");
    assert_eq!(findings[0]["subject"], Value::Null);
    assert!(
        findings[0]["fix"]
            .as_str()
            .unwrap()
            .contains("run `mur doctor` from inside a project directory"),
        "{report:#}"
    );
    assert!(
        stderr(&output).contains("error[E-IO-001]"),
        "{}",
        stderr(&output)
    );
}

/// S8: a formation directory whose roster is refused gives the roster refusal under block
/// `roster`.
#[test]
fn formation_with_a_refused_roster_is_one_refused_document() {
    let home = TempDir::new().unwrap();
    let formation = TempDir::new().unwrap();
    fs::write(
        formation.path().join("roster.yaml"),
        "roster_version: 1\nmembers:\n  - name: solo\n    capsule: solo\n    version: 1.0.0\n    \
         entry: true\n",
    )
    .unwrap();
    let output = doctor(home.path(), formation.path(), true, &[]);
    let report = document(&output);
    assert_consistent(&output, &report);
    assert_eq!(output.status.code(), Some(1));
    let findings = report["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 1, "{report:#}");
    assert!(
        findings[0]["code"].as_str().unwrap().starts_with("E-ROS-"),
        "{report:#}"
    );
    assert_eq!(findings[0]["block"], "roster");
    assert!(
        stderr(&output).contains(&format!(
            "First refusal: error[{}]",
            findings[0]["code"].as_str().unwrap()
        )),
        "{}",
        stderr(&output)
    );
}
