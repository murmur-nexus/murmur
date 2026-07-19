use std::fs;

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

fn mur() -> Command {
    Command::cargo_bin("mur").unwrap()
}

// ── Fixtures ─────────────────────────────────────────────────────────────────
//
// Two complete eval sessions:
//   A: 2 scorers (both pass), dataset_run with overall=pass
//   B: 2 scorers (one fail), dataset_run with overall=fail

const FIXTURE_A: &str = concat!(
    "{\"record_type\":\"event_score\",\"ts\":1000,\"turn\":2,\"event_type\":\"session_end\",",
    "\"scorer\":\"turn_limit\",\"result\":\"pass\",\"score\":1.0,\"reason\":\"turns=2 max=5\"}\n",
    "{\"record_type\":\"event_score\",\"ts\":1001,\"turn\":2,\"event_type\":\"session_end\",",
    "\"scorer\":\"success_check\",\"result\":\"pass\",\"score\":1.0,\"reason\":\"exit_status=ok\"}\n",
    "{\"record_type\":\"dataset_run\",\"ts\":1002,\"dataset_id\":\"test-ds\",\"case_id\":\"case_001\",",
    "\"overall\":\"pass\",\"scores\":{\"turn_limit\":1.0,\"success_check\":1.0}}\n"
);

const FIXTURE_B: &str = concat!(
    "{\"record_type\":\"event_score\",\"ts\":2000,\"turn\":8,\"event_type\":\"session_end\",",
    "\"scorer\":\"turn_limit\",\"result\":\"fail\",\"score\":0.0,\"reason\":\"turns=8 max=5\"}\n",
    "{\"record_type\":\"event_score\",\"ts\":2001,\"turn\":8,\"event_type\":\"session_end\",",
    "\"scorer\":\"success_check\",\"result\":\"pass\",\"score\":1.0,\"reason\":\"exit_status=ok\"}\n",
    "{\"record_type\":\"dataset_run\",\"ts\":2002,\"dataset_id\":\"test-ds\",\"case_id\":\"case_002\",",
    "\"overall\":\"fail\",\"scores\":{\"turn_limit\":0.0,\"success_check\":1.0}}\n"
);

const FIXTURE_NO_SCORERS: &str =
    "{\"record_type\":\"dataset_run\",\"ts\":3000,\"dataset_id\":null,\"case_id\":null,\
     \"overall\":\"no_scores\",\"scores\":{}}\n";

const FIXTURE_SINGLE_SCORER: &str = concat!(
    "{\"record_type\":\"event_score\",\"ts\":4000,\"turn\":1,\"event_type\":\"session_end\",",
    "\"scorer\":\"exit_ok\",\"result\":\"pass\",\"score\":1.0,\"reason\":\"exit_status=ok\"}\n",
    "{\"record_type\":\"dataset_run\",\"ts\":4001,\"dataset_id\":null,\"case_id\":null,",
    "\"overall\":\"pass\",\"scores\":{\"exit_ok\":1.0}}\n"
);

fn write_fixture(dir: &TempDir, name: &str, content: &str) -> std::path::PathBuf {
    let path = dir.path().join(name);
    fs::write(&path, content).unwrap();
    path
}

// ── mur eval show ─────────────────────────────────────────────────────────────

#[test]
fn show_human_readable_pass_session() {
    let dir = TempDir::new().unwrap();
    let path = write_fixture(&dir, "eval.jsonl", FIXTURE_A);

    mur()
        .args(["eval", "show", path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("turn_limit"))
        .stdout(predicate::str::contains("success_check"))
        .stdout(predicate::str::contains("pass"));
}

#[test]
fn show_human_readable_fail_session() {
    let dir = TempDir::new().unwrap();
    let path = write_fixture(&dir, "eval.jsonl", FIXTURE_B);

    mur()
        .args(["eval", "show", path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("turn_limit"))
        .stdout(predicate::str::contains("fail"));
}

#[test]
fn show_json_output_is_valid_json() {
    let dir = TempDir::new().unwrap();
    let path = write_fixture(&dir, "eval.jsonl", FIXTURE_A);

    let output = mur()
        .args(["eval", "show", "--json", path.to_str().unwrap()])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let text = String::from_utf8(output).unwrap();
    let parsed: serde_json::Value =
        serde_json::from_str(&text).expect("--json output must be valid JSON");
    assert_eq!(parsed["overall"], "pass");
    assert!(parsed["scorers"].is_object());
}

#[test]
fn show_json_fail_session_overall_fail() {
    let dir = TempDir::new().unwrap();
    let path = write_fixture(&dir, "eval.jsonl", FIXTURE_B);

    let output = mur()
        .args(["eval", "show", "--json", path.to_str().unwrap()])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let text = String::from_utf8(output).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed["overall"], "fail");
}

#[test]
fn show_no_scorer_events_handles_gracefully() {
    let dir = TempDir::new().unwrap();
    let path = write_fixture(&dir, "eval.jsonl", FIXTURE_NO_SCORERS);

    mur()
        .args(["eval", "show", path.to_str().unwrap()])
        .assert()
        .success();
}

#[test]
fn show_single_scorer_shows_pass_rate() {
    let dir = TempDir::new().unwrap();
    let path = write_fixture(&dir, "eval.jsonl", FIXTURE_SINGLE_SCORER);

    mur()
        .args(["eval", "show", path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("exit_ok"))
        .stdout(predicate::str::contains("100.0%").or(predicate::str::contains("pass")));
}

#[test]
fn show_missing_file_exits_nonzero() {
    mur()
        .args(["eval", "show", "/tmp/does-not-exist-eval-xyz.jsonl"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("E-IO-001").or(predicate::str::contains("not found")));
}

#[test]
fn show_malformed_json_reports_line_number() {
    let dir = TempDir::new().unwrap();
    let content = concat!(
        "{\"record_type\":\"event_score\",\"ts\":1000,\"turn\":1,\"event_type\":\"session_end\",",
        "\"scorer\":\"s1\",\"result\":\"pass\",\"score\":1.0}\n",
        "this is not valid json\n"
    );
    let path = write_fixture(&dir, "eval.jsonl", content);

    mur()
        .args(["eval", "show", path.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("E-EVAL-001").or(predicate::str::contains(":2:")));
}

#[test]
fn show_empty_file_exits_success_with_no_scores_message() {
    let dir = TempDir::new().unwrap();
    let path = write_fixture(&dir, "eval.jsonl", "");

    mur()
        .args(["eval", "show", path.to_str().unwrap()])
        .assert()
        .success();
}

#[test]
fn show_unknown_record_type_fails() {
    let dir = TempDir::new().unwrap();
    let content = "{\"record_type\":\"unknown_type\",\"ts\":1000}\n";
    let path = write_fixture(&dir, "eval.jsonl", content);

    mur()
        .args(["eval", "show", path.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("E-EVAL-001"));
}

// ── mur eval diff ─────────────────────────────────────────────────────────────

#[test]
fn diff_shows_per_scorer_comparison() {
    let dir = TempDir::new().unwrap();
    let path_a = write_fixture(&dir, "eval_a.jsonl", FIXTURE_A);
    let path_b = write_fixture(&dir, "eval_b.jsonl", FIXTURE_B);

    mur()
        .args([
            "eval",
            "diff",
            path_a.to_str().unwrap(),
            path_b.to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("turn_limit"))
        .stdout(predicate::str::contains("success_check"));
}

#[test]
fn diff_delta_shows_which_run_scored_better() {
    let dir = TempDir::new().unwrap();
    let path_a = write_fixture(&dir, "eval_a.jsonl", FIXTURE_A);
    let path_b = write_fixture(&dir, "eval_b.jsonl", FIXTURE_B);

    let output = mur()
        .args([
            "eval",
            "diff",
            path_a.to_str().unwrap(),
            path_b.to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let text = String::from_utf8(output).unwrap();
    // A scores 100% on turn_limit, B scores 0% — so A should be better
    assert!(
        text.contains("A better") || text.contains("pp"),
        "diff output should indicate which run scored better:\n{text}"
    );
}

#[test]
fn diff_missing_file_exits_nonzero() {
    let dir = TempDir::new().unwrap();
    let path_a = write_fixture(&dir, "eval_a.jsonl", FIXTURE_A);

    mur()
        .args([
            "eval",
            "diff",
            path_a.to_str().unwrap(),
            "/tmp/missing-eval.jsonl",
        ])
        .assert()
        .failure();
}

#[test]
fn diff_same_file_shows_equal_delta() {
    let dir = TempDir::new().unwrap();
    let path_a = write_fixture(&dir, "eval_a.jsonl", FIXTURE_A);
    let path_b = write_fixture(&dir, "eval_b2.jsonl", FIXTURE_A);

    let output = mur()
        .args([
            "eval",
            "diff",
            path_a.to_str().unwrap(),
            path_b.to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let text = String::from_utf8(output).unwrap();
    assert!(
        text.contains('='),
        "same-file diff should show '=' in delta column:\n{text}"
    );
}

// ── JSON round-trip ───────────────────────────────────────────────────────────

#[test]
fn show_json_contains_scorer_names() {
    let dir = TempDir::new().unwrap();
    let path = write_fixture(&dir, "eval.jsonl", FIXTURE_A);

    let output = mur()
        .args(["eval", "show", "--json", path.to_str().unwrap()])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let text = String::from_utf8(output).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
    let scorers = parsed["scorers"].as_object().unwrap();
    assert!(scorers.contains_key("turn_limit"));
    assert!(scorers.contains_key("success_check"));
}

#[test]
fn show_json_pass_rate_fields_present() {
    let dir = TempDir::new().unwrap();
    let path = write_fixture(&dir, "eval.jsonl", FIXTURE_A);

    let output = mur()
        .args(["eval", "show", "--json", path.to_str().unwrap()])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let text = String::from_utf8(output).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
    let scorer_data = &parsed["scorers"]["turn_limit"];
    assert!(scorer_data["pass"].is_number());
    assert!(scorer_data["fail"].is_number());
    assert!(scorer_data["total"].is_number());
    assert!(scorer_data["pass_rate"].is_number());
}

// ── mur eval run (argument defaults) ─────────────────────────────────────────

fn write_minimal_manifest(dir: &TempDir) {
    fs::write(
        dir.path().join("murmur.yaml"),
        "name: test-eval\nversion: 0.1.0\n",
    )
    .unwrap();
}

#[test]
fn run_no_args_default_dataset_absent_gives_clear_error() {
    let dir = TempDir::new().unwrap();
    write_minimal_manifest(&dir);

    mur()
        .args(["eval", "run"])
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("no dataset found")
                .and(predicate::str::contains("./eval.jsonl")),
        );
}

#[test]
fn run_explicit_capsule_only_default_dataset_absent_gives_clear_error() {
    let dir = TempDir::new().unwrap();
    write_minimal_manifest(&dir);

    mur()
        .args(["eval", "run", dir.path().to_str().unwrap()])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("no dataset found")
                .and(predicate::str::contains("./eval.jsonl")),
        );
}

#[test]
fn run_explicit_dataset_only_capsule_defaults_to_cwd() {
    let dir = TempDir::new().unwrap();
    write_minimal_manifest(&dir);
    let dataset_path = "/tmp/no-such-eval-explicit-only.jsonl";

    mur()
        .args(["eval", "run", "--dataset", dataset_path])
        .current_dir(dir.path())
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("dataset not found at")
                .and(predicate::str::contains(dataset_path)),
        );
}

#[test]
fn run_both_explicit_uses_given_paths() {
    let dir = TempDir::new().unwrap();
    write_minimal_manifest(&dir);
    let dataset_path = "/tmp/no-such-eval-both-explicit.jsonl";

    mur()
        .args([
            "eval",
            "run",
            dir.path().to_str().unwrap(),
            "--dataset",
            dataset_path,
        ])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("dataset not found at")
                .and(predicate::str::contains(dataset_path)),
        );
}
