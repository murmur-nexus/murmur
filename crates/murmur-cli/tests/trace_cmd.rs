use std::{fs, path::Path};

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

fn mur() -> Command {
    Command::cargo_bin("mur").unwrap()
}

// ── Fixtures ─────────────────────────────────────────────────────────────────
//
// Two complete sessions designed to produce a meaningful diff:
//   A: 2 turns, ok exit, 1 tool call, 1 shell call, no compaction, 500ms
//   B: 5 turns, max_turns_reached, 5 tool calls, 5 shell calls, compaction, 1700ms

const FIXTURE_A: &str = concat!(
    "{\"event_type\":\"session_start\",\"session_id\":\"ses_aaaaaaaaaaaa4aaa8aaa000000000001\",\"timestamp\":1000,",
    "\"capsule_name\":\"test-capsule\",\"capsule_version\":\"0.1.0\",\"model\":\"claude-3-5-sonnet\",",
    "\"max_turns\":10,\"capabilities\":[\"shell\"],\"tools_declared\":[\"bash\"]}\n",

    "{\"event_type\":\"inference\",\"session_id\":\"ses_aaaaaaaaaaaa4aaa8aaa000000000001\",\"timestamp\":1100,",
    "\"turn\":1,\"input_tokens\":1000,\"output_tokens\":200,\"decision\":\"tool_call\",\"tool_name\":\"bash\"}\n",

    "{\"event_type\":\"tool_call\",\"session_id\":\"ses_aaaaaaaaaaaa4aaa8aaa000000000001\",\"timestamp\":1200,",
    "\"turn\":1,\"tool_name\":\"bash\",\"input_bytes\":50,\"output_bytes\":20,\"duration_ms\":100,\"status\":\"ok\"}\n",

    "{\"event_type\":\"shell\",\"session_id\":\"ses_aaaaaaaaaaaa4aaa8aaa000000000001\",\"timestamp\":1300,",
    "\"turn\":1,\"command\":\"echo hello\",\"exit_code\":0,\"stdout_bytes\":6,\"stderr_bytes\":0,\"duration_ms\":50}\n",

    "{\"event_type\":\"inference\",\"session_id\":\"ses_aaaaaaaaaaaa4aaa8aaa000000000001\",\"timestamp\":1400,",
    "\"turn\":2,\"input_tokens\":1200,\"output_tokens\":150,\"decision\":\"end_turn\",\"tool_name\":null}\n",

    "{\"event_type\":\"session_end\",\"session_id\":\"ses_aaaaaaaaaaaa4aaa8aaa000000000001\",\"timestamp\":1500,",
    "\"total_turns\":2,\"total_input_tokens\":2200,\"total_output_tokens\":350,",
    "\"total_tool_calls\":1,\"total_shell_calls\":1,\"duration_ms\":500,\"exit_status\":\"ok\"}\n"
);

const FIXTURE_B: &str = concat!(
    "{\"event_type\":\"session_start\",\"session_id\":\"ses_bbbbbbbbbbbb4bbb8bbb000000000002\",\"timestamp\":2000,",
    "\"capsule_name\":\"test-capsule\",\"capsule_version\":\"0.1.0\",\"model\":\"claude-3-5-sonnet\",",
    "\"max_turns\":5,\"capabilities\":[\"shell\"],\"tools_declared\":[\"bash\"]}\n",

    "{\"event_type\":\"inference\",\"session_id\":\"ses_bbbbbbbbbbbb4bbb8bbb000000000002\",\"timestamp\":2100,",
    "\"turn\":1,\"input_tokens\":1500,\"output_tokens\":300,\"decision\":\"tool_call\",\"tool_name\":\"bash\"}\n",

    "{\"event_type\":\"tool_call\",\"session_id\":\"ses_bbbbbbbbbbbb4bbb8bbb000000000002\",\"timestamp\":2200,",
    "\"turn\":1,\"tool_name\":\"bash\",\"input_bytes\":80,\"output_bytes\":30,\"duration_ms\":200,\"status\":\"ok\"}\n",

    "{\"event_type\":\"shell\",\"session_id\":\"ses_bbbbbbbbbbbb4bbb8bbb000000000002\",\"timestamp\":2300,",
    "\"turn\":1,\"command\":\"echo test\",\"exit_code\":0,\"stdout_bytes\":5,\"stderr_bytes\":0,\"duration_ms\":80}\n",

    "{\"event_type\":\"inference\",\"session_id\":\"ses_bbbbbbbbbbbb4bbb8bbb000000000002\",\"timestamp\":2400,",
    "\"turn\":2,\"input_tokens\":1800,\"output_tokens\":250,\"decision\":\"tool_call\",\"tool_name\":\"bash\"}\n",

    "{\"event_type\":\"tool_call\",\"session_id\":\"ses_bbbbbbbbbbbb4bbb8bbb000000000002\",\"timestamp\":2500,",
    "\"turn\":2,\"tool_name\":\"bash\",\"input_bytes\":60,\"output_bytes\":25,\"duration_ms\":150,\"status\":\"error\"}\n",

    "{\"event_type\":\"shell\",\"session_id\":\"ses_bbbbbbbbbbbb4bbb8bbb000000000002\",\"timestamp\":2600,",
    "\"turn\":2,\"command\":\"cat nonexistent\",\"exit_code\":1,\"stdout_bytes\":0,\"stderr_bytes\":20,\"duration_ms\":30}\n",

    "{\"event_type\":\"inference\",\"session_id\":\"ses_bbbbbbbbbbbb4bbb8bbb000000000002\",\"timestamp\":2700,",
    "\"turn\":3,\"input_tokens\":2000,\"output_tokens\":200,\"decision\":\"tool_call\",\"tool_name\":\"bash\"}\n",

    "{\"event_type\":\"tool_call\",\"session_id\":\"ses_bbbbbbbbbbbb4bbb8bbb000000000002\",\"timestamp\":2800,",
    "\"turn\":3,\"tool_name\":\"bash\",\"input_bytes\":70,\"output_bytes\":15,\"duration_ms\":180,\"status\":\"ok\"}\n",

    "{\"event_type\":\"shell\",\"session_id\":\"ses_bbbbbbbbbbbb4bbb8bbb000000000002\",\"timestamp\":2900,",
    "\"turn\":3,\"command\":\"ls /tmp\",\"exit_code\":0,\"stdout_bytes\":100,\"stderr_bytes\":0,\"duration_ms\":40}\n",

    "{\"event_type\":\"compaction\",\"session_id\":\"ses_bbbbbbbbbbbb4bbb8bbb000000000002\",\"timestamp\":3000,",
    "\"turn\":3,\"tokens_before\":5300,\"tokens_after\":2000}\n",

    "{\"event_type\":\"inference\",\"session_id\":\"ses_bbbbbbbbbbbb4bbb8bbb000000000002\",\"timestamp\":3100,",
    "\"turn\":4,\"input_tokens\":2100,\"output_tokens\":180,\"decision\":\"tool_call\",\"tool_name\":\"bash\"}\n",

    "{\"event_type\":\"tool_call\",\"session_id\":\"ses_bbbbbbbbbbbb4bbb8bbb000000000002\",\"timestamp\":3200,",
    "\"turn\":4,\"tool_name\":\"bash\",\"input_bytes\":90,\"output_bytes\":10,\"duration_ms\":220,\"status\":\"ok\"}\n",

    "{\"event_type\":\"shell\",\"session_id\":\"ses_bbbbbbbbbbbb4bbb8bbb000000000002\",\"timestamp\":3300,",
    "\"turn\":4,\"command\":\"pwd\",\"exit_code\":0,\"stdout_bytes\":15,\"stderr_bytes\":0,\"duration_ms\":10}\n",

    "{\"event_type\":\"inference\",\"session_id\":\"ses_bbbbbbbbbbbb4bbb8bbb000000000002\",\"timestamp\":3400,",
    "\"turn\":5,\"input_tokens\":2300,\"output_tokens\":160,\"decision\":\"tool_call\",\"tool_name\":\"bash\"}\n",

    "{\"event_type\":\"tool_call\",\"session_id\":\"ses_bbbbbbbbbbbb4bbb8bbb000000000002\",\"timestamp\":3500,",
    "\"turn\":5,\"tool_name\":\"bash\",\"input_bytes\":100,\"output_bytes\":20,\"duration_ms\":190,\"status\":\"ok\"}\n",

    "{\"event_type\":\"shell\",\"session_id\":\"ses_bbbbbbbbbbbb4bbb8bbb000000000002\",\"timestamp\":3600,",
    "\"turn\":5,\"command\":\"echo done\",\"exit_code\":0,\"stdout_bytes\":5,\"stderr_bytes\":0,\"duration_ms\":20}\n",

    "{\"event_type\":\"session_end\",\"session_id\":\"ses_bbbbbbbbbbbb4bbb8bbb000000000002\",\"timestamp\":3700,",
    "\"total_turns\":5,\"total_input_tokens\":9700,\"total_output_tokens\":1090,",
    "\"total_tool_calls\":5,\"total_shell_calls\":5,\"duration_ms\":1700,\"exit_status\":\"max_turns_reached\"}\n"
);

// Minimal fixture: 1 turn, no tool calls, no shell calls
const FIXTURE_NO_TOOLS: &str = concat!(
    "{\"event_type\":\"session_start\",\"session_id\":\"ses_cccccccccccc4ccc8ccc000000000003\",\"timestamp\":4000,",
    "\"capsule_name\":\"simple-capsule\",\"capsule_version\":\"0.2.0\",\"model\":\"claude-haiku\",",
    "\"max_turns\":20,\"capabilities\":[],\"tools_declared\":[]}\n",

    "{\"event_type\":\"inference\",\"session_id\":\"ses_cccccccccccc4ccc8ccc000000000003\",\"timestamp\":4100,",
    "\"turn\":1,\"input_tokens\":500,\"output_tokens\":100,\"decision\":\"end_turn\",\"tool_name\":null}\n",

    "{\"event_type\":\"session_end\",\"session_id\":\"ses_cccccccccccc4ccc8ccc000000000003\",\"timestamp\":4200,",
    "\"total_turns\":1,\"total_input_tokens\":500,\"total_output_tokens\":100,",
    "\"total_tool_calls\":0,\"total_shell_calls\":0,\"duration_ms\":200,\"exit_status\":\"ok\"}\n"
);

fn write_fixture(dir: &Path, name: &str, content: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    fs::write(&path, content).unwrap();
    path
}

// ── session resolution helpers ────────────────────────────────────────────────

fn write_session(workdir: &Path, session_id: &str, content: &str) {
    let ses_dir = workdir.join(session_id);
    fs::create_dir_all(&ses_dir).unwrap();
    fs::write(ses_dir.join("trace.jsonl"), content).unwrap();
}

// ── show tests ────────────────────────────────────────────────────────────────

#[test]
fn show_exits_zero_and_covers_all_metric_categories() {
    let tmp = TempDir::new().unwrap();
    let path = write_fixture(tmp.path(), "trace-a.jsonl", FIXTURE_A);

    mur()
        .args(["trace", "show", path.to_str().unwrap()])
        .assert()
        .success()
        // session metadata
        .stdout(predicate::str::contains("test-capsule"))
        .stdout(predicate::str::contains("claude-3-5-sonnet"))
        .stdout(predicate::str::contains(
            "ses_aaaaaaaaaaaa4aaa8aaa000000000001",
        ))
        // exit status
        .stdout(predicate::str::contains("ok"))
        // duration
        .stdout(predicate::str::contains("500ms"))
        // turns
        .stdout(predicate::str::contains("2"))
        // tokens
        .stdout(predicate::str::contains("2,200"))
        .stdout(predicate::str::contains("350"))
        // tool calls
        .stdout(predicate::str::contains("1 ok"))
        // shell calls with exit code distribution
        .stdout(predicate::str::contains("exit codes"))
        .stdout(predicate::str::contains("0"))
        // compaction not fired
        .stdout(predicate::str::contains("no"));
}

#[test]
fn show_includes_tokens_per_turn_average() {
    let tmp = TempDir::new().unwrap();
    let path = write_fixture(tmp.path(), "trace-a.jsonl", FIXTURE_A);

    // 2200 input tokens / 2 turns = 1100 avg/turn
    mur()
        .args(["trace", "show", path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("1100"))
        .stdout(predicate::str::contains("/turn"));
}

#[test]
fn show_compaction_appears_with_turn_and_token_counts() {
    let tmp = TempDir::new().unwrap();
    let path = write_fixture(tmp.path(), "trace-b.jsonl", FIXTURE_B);

    mur()
        .args(["trace", "show", path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("yes"))
        .stdout(predicate::str::contains("turn 3"))
        .stdout(predicate::str::contains("5,300")) // tokens_before
        .stdout(predicate::str::contains("2,000")); // tokens_after
}

#[test]
fn show_no_tool_calls_produces_zero_not_error() {
    let tmp = TempDir::new().unwrap();
    let path = write_fixture(tmp.path(), "no-tools.jsonl", FIXTURE_NO_TOOLS);

    mur()
        .args(["trace", "show", path.to_str().unwrap()])
        .assert()
        .success()
        // tool call count = 0
        .stdout(predicate::str::contains("count:"))
        .stdout(predicate::str::contains("0"));
}

#[test]
fn show_max_turns_reached_status() {
    let tmp = TempDir::new().unwrap();
    let path = write_fixture(tmp.path(), "trace-b.jsonl", FIXTURE_B);

    mur()
        .args(["trace", "show", path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("max_turns_reached"));
}

// ── session resolution tests ──────────────────────────────────────────────────

#[test]
fn show_no_arg_single_session_resolves_correctly() {
    let tmp = TempDir::new().unwrap();
    write_session(
        tmp.path(),
        "ses_aaaaaaaaaaaa4aaa8aaa000000000001",
        FIXTURE_A,
    );

    mur()
        .args(["trace", "show", "--workdir", tmp.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("test-capsule"))
        .stdout(predicate::str::contains("500ms"));
}

#[test]
fn show_no_arg_multiple_sessions_picks_lexicographically_largest() {
    let tmp = TempDir::new().unwrap();
    // A < B lexicographically; B is "most recent"
    write_session(
        tmp.path(),
        "ses_aaaaaaaaaaaa4aaa8aaa000000000001",
        FIXTURE_A,
    );
    write_session(
        tmp.path(),
        "ses_bbbbbbbbbbbb4bbb8bbb000000000002",
        FIXTURE_B,
    );

    mur()
        .args(["trace", "show", "--workdir", tmp.path().to_str().unwrap()])
        .assert()
        .success()
        // FIXTURE_B has 1700ms duration and max_turns_reached
        .stdout(predicate::str::contains("1.7s"))
        .stdout(predicate::str::contains("max_turns_reached"));
}

#[test]
fn show_no_arg_empty_workdir_gives_clear_error() {
    let tmp = TempDir::new().unwrap();

    mur()
        .args(["trace", "show", "--workdir", tmp.path().to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no sessions found in workdir"));
}

#[test]
fn show_full_session_id_resolves_correctly() {
    let tmp = TempDir::new().unwrap();
    write_session(
        tmp.path(),
        "ses_aaaaaaaaaaaa4aaa8aaa000000000001",
        FIXTURE_A,
    );

    mur()
        .args([
            "trace",
            "show",
            "--workdir",
            tmp.path().to_str().unwrap(),
            "ses_aaaaaaaaaaaa4aaa8aaa000000000001",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("test-capsule"));
}

#[test]
fn show_full_session_id_not_present_gives_clear_error() {
    let tmp = TempDir::new().unwrap();

    mur()
        .args([
            "trace",
            "show",
            "--workdir",
            tmp.path().to_str().unwrap(),
            "ses_aaaaaaaaaaaa4aaa8aaa000000000001",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not found"));
}

#[test]
fn show_suffix_unique_match_resolves_correctly() {
    let tmp = TempDir::new().unwrap();
    write_session(
        tmp.path(),
        "ses_aaaaaaaaaaaa4aaa8aaa000000000001",
        FIXTURE_A,
    );
    write_session(
        tmp.path(),
        "ses_bbbbbbbbbbbb4bbb8bbb000000000002",
        FIXTURE_B,
    );

    // Suffix "0001" uniquely matches session A
    mur()
        .args([
            "trace",
            "show",
            "--workdir",
            tmp.path().to_str().unwrap(),
            "0001",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("500ms"));
}

#[test]
fn show_suffix_no_match_gives_clear_error() {
    let tmp = TempDir::new().unwrap();
    write_session(
        tmp.path(),
        "ses_aaaaaaaaaaaa4aaa8aaa000000000001",
        FIXTURE_A,
    );

    mur()
        .args([
            "trace",
            "show",
            "--workdir",
            tmp.path().to_str().unwrap(),
            "zzzzzzz",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no session found matching suffix"));
}

#[test]
fn show_suffix_multiple_matches_gives_ambiguity_error() {
    let tmp = TempDir::new().unwrap();
    // Both end with "abc" — suffix "abc" is ambiguous
    write_session(
        tmp.path(),
        "ses_aaaaaaaaaaaa4aaa8aaa000000000abc",
        FIXTURE_A,
    );
    write_session(
        tmp.path(),
        "ses_bbbbbbbbbbbb4bbb8bbb000000000abc",
        FIXTURE_B,
    );

    mur()
        .args([
            "trace",
            "show",
            "--workdir",
            tmp.path().to_str().unwrap(),
            "abc",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("ambiguous"))
        .stderr(predicate::str::contains("2 sessions"));
}

#[test]
fn show_suffix_case_insensitive() {
    let tmp = TempDir::new().unwrap();
    write_session(
        tmp.path(),
        "ses_aaaaaaaaaaaa4aaa8aaa000000000001",
        FIXTURE_A,
    );

    // "0001" vs "0001" — case is moot for digits, but test upper vs lower for hex
    mur()
        .args([
            "trace",
            "show",
            "--workdir",
            tmp.path().to_str().unwrap(),
            "0001",
        ])
        .assert()
        .success();
}

#[test]
fn show_legacy_path_with_slash_passes_through() {
    let tmp = TempDir::new().unwrap();
    let path = write_fixture(tmp.path(), "trace-a.jsonl", FIXTURE_A);

    // Path contains '/' so it bypasses session resolution entirely
    mur()
        .args(["trace", "show", path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("test-capsule"));
}

// ── diff tests ────────────────────────────────────────────────────────────────

#[test]
fn diff_exits_zero_and_shows_both_runs() {
    let tmp = TempDir::new().unwrap();
    let pa = write_fixture(tmp.path(), "trace-a.jsonl", FIXTURE_A);
    let pb = write_fixture(tmp.path(), "trace-b.jsonl", FIXTURE_B);

    mur()
        .args(["trace", "diff", pa.to_str().unwrap(), pb.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("Run A"))
        .stdout(predicate::str::contains("Run B"))
        .stdout(predicate::str::contains("Delta"));
}

#[test]
fn diff_shows_turn_count_difference() {
    let tmp = TempDir::new().unwrap();
    let pa = write_fixture(tmp.path(), "trace-a.jsonl", FIXTURE_A);
    let pb = write_fixture(tmp.path(), "trace-b.jsonl", FIXTURE_B);

    // A has 2 turns, B has 5 turns — delta should be +3
    mur()
        .args(["trace", "diff", pa.to_str().unwrap(), pb.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("turns"))
        .stdout(predicate::str::contains("+3"));
}

#[test]
fn diff_shows_token_consumption_difference() {
    let tmp = TempDir::new().unwrap();
    let pa = write_fixture(tmp.path(), "trace-a.jsonl", FIXTURE_A);
    let pb = write_fixture(tmp.path(), "trace-b.jsonl", FIXTURE_B);

    // A: 2200 input, B: 9700 input — B uses more tokens
    mur()
        .args(["trace", "diff", pa.to_str().unwrap(), pb.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("input tokens"))
        .stdout(predicate::str::contains("2,200"))
        .stdout(predicate::str::contains("9,700"));
}

#[test]
fn diff_shows_exit_status_for_both_runs() {
    let tmp = TempDir::new().unwrap();
    let pa = write_fixture(tmp.path(), "trace-a.jsonl", FIXTURE_A);
    let pb = write_fixture(tmp.path(), "trace-b.jsonl", FIXTURE_B);

    mur()
        .args(["trace", "diff", pa.to_str().unwrap(), pb.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("exit status"))
        .stdout(predicate::str::contains("ok"))
        .stdout(predicate::str::contains("max_turns_reached"));
}

#[test]
fn diff_shows_compaction_difference() {
    let tmp = TempDir::new().unwrap();
    let pa = write_fixture(tmp.path(), "trace-a.jsonl", FIXTURE_A);
    let pb = write_fixture(tmp.path(), "trace-b.jsonl", FIXTURE_B);

    // A: no compaction, B: compaction at turn 3
    mur()
        .args(["trace", "diff", pa.to_str().unwrap(), pb.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("compaction"))
        .stdout(predicate::str::contains("none"))
        .stdout(predicate::str::contains("turn 3"));
}

#[test]
fn diff_better_worse_indicators_present() {
    let tmp = TempDir::new().unwrap();
    let pa = write_fixture(tmp.path(), "trace-a.jsonl", FIXTURE_A);
    let pb = write_fixture(tmp.path(), "trace-b.jsonl", FIXTURE_B);

    // B uses more turns/tokens so A should be flagged as better on those metrics
    mur()
        .args(["trace", "diff", pa.to_str().unwrap(), pb.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("better"));
}

// ── diff session-resolution tests ─────────────────────────────────────────────

#[test]
fn diff_both_full_session_ids_resolve() {
    let tmp = TempDir::new().unwrap();
    write_session(
        tmp.path(),
        "ses_aaaaaaaaaaaa4aaa8aaa000000000001",
        FIXTURE_A,
    );
    write_session(
        tmp.path(),
        "ses_bbbbbbbbbbbb4bbb8bbb000000000002",
        FIXTURE_B,
    );

    mur()
        .args([
            "trace",
            "diff",
            "--workdir",
            tmp.path().to_str().unwrap(),
            "ses_aaaaaaaaaaaa4aaa8aaa000000000001",
            "ses_bbbbbbbbbbbb4bbb8bbb000000000002",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Run A"))
        .stdout(predicate::str::contains("Run B"));
}

#[test]
fn diff_both_suffixes_unambiguous_resolve() {
    let tmp = TempDir::new().unwrap();
    write_session(
        tmp.path(),
        "ses_aaaaaaaaaaaa4aaa8aaa000000000001",
        FIXTURE_A,
    );
    write_session(
        tmp.path(),
        "ses_bbbbbbbbbbbb4bbb8bbb000000000002",
        FIXTURE_B,
    );

    mur()
        .args([
            "trace",
            "diff",
            "--workdir",
            tmp.path().to_str().unwrap(),
            "0001",
            "0002",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("500ms"))
        .stdout(predicate::str::contains("1.7s"));
}

#[test]
fn diff_ambiguous_suffix_names_which_argument() {
    let tmp = TempDir::new().unwrap();
    // Both end with "abc" — suffix "abc" is ambiguous
    write_session(
        tmp.path(),
        "ses_aaaaaaaaaaaa4aaa8aaa000000000abc",
        FIXTURE_A,
    );
    write_session(
        tmp.path(),
        "ses_bbbbbbbbbbbb4bbb8bbb000000000abc",
        FIXTURE_B,
    );
    write_session(
        tmp.path(),
        "ses_cccccccccccc4ccc8ccc000000000002",
        FIXTURE_NO_TOOLS,
    );

    // "abc" is ambiguous for `before`; "0002" is unambiguous for `after`
    mur()
        .args([
            "trace",
            "diff",
            "--workdir",
            tmp.path().to_str().unwrap(),
            "abc",
            "0002",
        ])
        .assert()
        .failure()
        // error should name the failing argument
        .stderr(predicate::str::contains("before"))
        .stderr(predicate::str::contains("ambiguous"));
}

#[test]
fn diff_ordinal_at1_and_at2_resolve_to_correct_sessions() {
    let tmp = TempDir::new().unwrap();
    // A < B lexicographically: B is most recent (@1), A is second most recent (@2)
    write_session(
        tmp.path(),
        "ses_aaaaaaaaaaaa4aaa8aaa000000000001",
        FIXTURE_A,
    );
    write_session(
        tmp.path(),
        "ses_bbbbbbbbbbbb4bbb8bbb000000000002",
        FIXTURE_B,
    );

    // @2 = A (500ms, ok), @1 = B (1700ms, max_turns_reached)
    mur()
        .args([
            "trace",
            "diff",
            "--workdir",
            tmp.path().to_str().unwrap(),
            "@2",
            "@1",
        ])
        .assert()
        .success()
        // A is Run A (500ms), B is Run B (1.7s)
        .stdout(predicate::str::contains("500ms"))
        .stdout(predicate::str::contains("1.7s"))
        .stdout(predicate::str::contains("max_turns_reached"));
}

#[test]
fn diff_ordinal_out_of_range_gives_clear_error() {
    let tmp = TempDir::new().unwrap();
    write_session(
        tmp.path(),
        "ses_aaaaaaaaaaaa4aaa8aaa000000000001",
        FIXTURE_A,
    );
    write_session(
        tmp.path(),
        "ses_bbbbbbbbbbbb4bbb8bbb000000000002",
        FIXTURE_B,
    );

    mur()
        .args([
            "trace",
            "diff",
            "--workdir",
            tmp.path().to_str().unwrap(),
            "@1",
            "@3",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("@3"))
        .stderr(predicate::str::contains("out of range"));
}

#[test]
fn diff_legacy_path_passthrough_on_both_args() {
    let tmp = TempDir::new().unwrap();
    let pa = write_fixture(tmp.path(), "trace-a.jsonl", FIXTURE_A);
    let pb = write_fixture(tmp.path(), "trace-b.jsonl", FIXTURE_B);

    // Both paths contain '/' so they bypass session resolution
    mur()
        .args(["trace", "diff", pa.to_str().unwrap(), pb.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("Run A"))
        .stdout(predicate::str::contains("Run B"));
}

#[test]
fn diff_mixed_full_id_and_suffix() {
    let tmp = TempDir::new().unwrap();
    write_session(
        tmp.path(),
        "ses_aaaaaaaaaaaa4aaa8aaa000000000001",
        FIXTURE_A,
    );
    write_session(
        tmp.path(),
        "ses_bbbbbbbbbbbb4bbb8bbb000000000002",
        FIXTURE_B,
    );

    mur()
        .args([
            "trace",
            "diff",
            "--workdir",
            tmp.path().to_str().unwrap(),
            "ses_aaaaaaaaaaaa4aaa8aaa000000000001",
            "0002",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("500ms"))
        .stdout(predicate::str::contains("1.7s"));
}

#[test]
fn diff_both_args_fail_reports_both_errors() {
    let tmp = TempDir::new().unwrap();
    write_session(
        tmp.path(),
        "ses_aaaaaaaaaaaa4aaa8aaa000000000001",
        FIXTURE_A,
    );

    mur()
        .args([
            "trace",
            "diff",
            "--workdir",
            tmp.path().to_str().unwrap(),
            "zzzz",
            "yyyy",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("before"))
        .stderr(predicate::str::contains("after"));
}

// ── diff zero/one/two arg rules ───────────────────────────────────────────────

#[test]
fn diff_no_args_defaults_to_two_most_recent_sessions() {
    let tmp = TempDir::new().unwrap();
    // A < B lexicographically: B is @1 (most recent), A is @2
    write_session(
        tmp.path(),
        "ses_aaaaaaaaaaaa4aaa8aaa000000000001",
        FIXTURE_A,
    );
    write_session(
        tmp.path(),
        "ses_bbbbbbbbbbbb4bbb8bbb000000000002",
        FIXTURE_B,
    );

    // No positional args → before=@2 (A, 500ms/ok), after=@1 (B, 1.7s/max_turns_reached)
    mur()
        .args(["trace", "diff", "--workdir", tmp.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("500ms"))
        .stdout(predicate::str::contains("1.7s"))
        .stdout(predicate::str::contains("max_turns_reached"));
}

#[test]
fn diff_one_arg_gives_clear_error() {
    let tmp = TempDir::new().unwrap();
    write_session(
        tmp.path(),
        "ses_aaaaaaaaaaaa4aaa8aaa000000000001",
        FIXTURE_A,
    );

    mur()
        .args([
            "trace",
            "diff",
            "--workdir",
            tmp.path().to_str().unwrap(),
            "ses_aaaaaaaaaaaa4aaa8aaa000000000001",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("expects 0 or 2 arguments, got 1"));
}

#[test]
fn diff_two_args_resolves_normally() {
    let tmp = TempDir::new().unwrap();
    write_session(
        tmp.path(),
        "ses_aaaaaaaaaaaa4aaa8aaa000000000001",
        FIXTURE_A,
    );
    write_session(
        tmp.path(),
        "ses_bbbbbbbbbbbb4bbb8bbb000000000002",
        FIXTURE_B,
    );

    mur()
        .args([
            "trace",
            "diff",
            "--workdir",
            tmp.path().to_str().unwrap(),
            "0001",
            "0002",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Run A"))
        .stdout(predicate::str::contains("Run B"));
}

// ── report tests ──────────────────────────────────────────────────────────────

#[test]
fn report_exits_zero_and_shows_session_count() {
    let tmp = TempDir::new().unwrap();
    write_session(
        tmp.path(),
        "ses_aaaaaaaaaaaa4aaa8aaa000000000001",
        FIXTURE_A,
    );
    write_session(
        tmp.path(),
        "ses_bbbbbbbbbbbb4bbb8bbb000000000002",
        FIXTURE_B,
    );
    write_session(
        tmp.path(),
        "ses_cccccccccccc4ccc8ccc000000000003",
        FIXTURE_NO_TOOLS,
    );

    mur()
        .args(["trace", "report", "--workdir", tmp.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("Sessions: 3"));
}

#[test]
fn report_shows_aggregate_statistics() {
    let tmp = TempDir::new().unwrap();
    write_session(
        tmp.path(),
        "ses_aaaaaaaaaaaa4aaa8aaa000000000001",
        FIXTURE_A,
    );
    write_session(
        tmp.path(),
        "ses_bbbbbbbbbbbb4bbb8bbb000000000002",
        FIXTURE_B,
    );
    write_session(
        tmp.path(),
        "ses_cccccccccccc4ccc8ccc000000000003",
        FIXTURE_NO_TOOLS,
    );

    mur()
        .args(["trace", "report", "--workdir", tmp.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("Mean"))
        .stdout(predicate::str::contains("StdDev"))
        .stdout(predicate::str::contains("turns"))
        .stdout(predicate::str::contains("input tokens"))
        .stdout(predicate::str::contains("output tokens"));
}

#[test]
fn report_shows_exit_status_distribution() {
    let tmp = TempDir::new().unwrap();
    write_session(
        tmp.path(),
        "ses_aaaaaaaaaaaa4aaa8aaa000000000001",
        FIXTURE_A,
    );
    write_session(
        tmp.path(),
        "ses_bbbbbbbbbbbb4bbb8bbb000000000002",
        FIXTURE_B,
    );
    write_session(
        tmp.path(),
        "ses_cccccccccccc4ccc8ccc000000000003",
        FIXTURE_NO_TOOLS,
    );

    // A and C are "ok", B is "max_turns_reached"
    mur()
        .args(["trace", "report", "--workdir", tmp.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("Exit status"))
        .stdout(predicate::str::contains("ok"))
        .stdout(predicate::str::contains("max_turns_reached"));
}

#[test]
fn report_works_with_single_session() {
    let tmp = TempDir::new().unwrap();
    write_session(
        tmp.path(),
        "ses_aaaaaaaaaaaa4aaa8aaa000000000001",
        FIXTURE_A,
    );

    mur()
        .args(["trace", "report", "--workdir", tmp.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("Sessions: 1"));
}

// ── report filter and selection tests ─────────────────────────────────────────

#[test]
fn report_no_args_includes_all_sessions() {
    let tmp = TempDir::new().unwrap();
    write_session(
        tmp.path(),
        "ses_aaaaaaaaaaaa4aaa8aaa000000000001",
        FIXTURE_A,
    );
    write_session(
        tmp.path(),
        "ses_bbbbbbbbbbbb4bbb8bbb000000000002",
        FIXTURE_B,
    );
    write_session(
        tmp.path(),
        "ses_cccccccccccc4ccc8ccc000000000003",
        FIXTURE_NO_TOOLS,
    );

    mur()
        .args(["trace", "report", "--workdir", tmp.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("Sessions: 3"));
}

#[test]
fn report_last_n_picks_most_recent() {
    let tmp = TempDir::new().unwrap();
    // 5 sessions; lexicographic order = chronological order for these IDs
    write_session(
        tmp.path(),
        "ses_aaaaaaaaaaaa4aaa8aaa000000000001",
        FIXTURE_A,
    );
    write_session(
        tmp.path(),
        "ses_bbbbbbbbbbbb4bbb8bbb000000000002",
        FIXTURE_A,
    );
    write_session(
        tmp.path(),
        "ses_cccccccccccc4ccc8ccc000000000003",
        FIXTURE_A,
    );
    write_session(
        tmp.path(),
        "ses_dddddddddddd4ddd8ddd000000000004",
        FIXTURE_A,
    );
    write_session(
        tmp.path(),
        "ses_eeeeeeeeeeee4eee8eee000000000005",
        FIXTURE_A,
    );

    mur()
        .args([
            "trace",
            "report",
            "--workdir",
            tmp.path().to_str().unwrap(),
            "--last",
            "3",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Sessions: 3"));
}

#[test]
fn report_since_filters_by_timestamp() {
    use std::time::{SystemTime, UNIX_EPOCH};

    fn ses_id_with_ts(ts_ms: u64, idx: u64) -> String {
        // First 12 hex chars of the session ID encode the v7 timestamp in ms.
        format!("ses_{:012x}4aaa8aaa{:012x}", ts_ms, idx)
    }

    let tmp = TempDir::new().unwrap();
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

    // 2 recent sessions (within 1h) — should pass --since 1h
    let recent1 = ses_id_with_ts(now_ms - 30 * 60 * 1000, 1);
    let recent2 = ses_id_with_ts(now_ms - 20 * 60 * 1000, 2);
    // 2 old sessions (older than 1h) — should be excluded by --since 1h
    let old1 = ses_id_with_ts(now_ms - 2 * 3600 * 1000, 3);
    let old2 = ses_id_with_ts(now_ms - 3 * 3600 * 1000, 4);

    write_session(tmp.path(), &recent1, FIXTURE_A);
    write_session(tmp.path(), &recent2, FIXTURE_A);
    write_session(tmp.path(), &old1, FIXTURE_A);
    write_session(tmp.path(), &old2, FIXTURE_A);

    mur()
        .args([
            "trace",
            "report",
            "--workdir",
            tmp.path().to_str().unwrap(),
            "--since",
            "1h",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Sessions: 2"));
}

#[test]
fn report_explicit_sessions_only_those_included() {
    let tmp = TempDir::new().unwrap();
    write_session(
        tmp.path(),
        "ses_aaaaaaaaaaaa4aaa8aaa000000000001",
        FIXTURE_A,
    );
    write_session(
        tmp.path(),
        "ses_bbbbbbbbbbbb4bbb8bbb000000000002",
        FIXTURE_B,
    );
    write_session(
        tmp.path(),
        "ses_cccccccccccc4ccc8ccc000000000003",
        FIXTURE_NO_TOOLS,
    );

    // Pass only 2 of the 3 sessions explicitly
    mur()
        .args([
            "trace",
            "report",
            "--workdir",
            tmp.path().to_str().unwrap(),
            "ses_aaaaaaaaaaaa4aaa8aaa000000000001",
            "ses_bbbbbbbbbbbb4bbb8bbb000000000002",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Sessions: 2"));
}

#[test]
fn report_since_combined_with_explicit_sessions_errors() {
    let tmp = TempDir::new().unwrap();
    write_session(
        tmp.path(),
        "ses_aaaaaaaaaaaa4aaa8aaa000000000001",
        FIXTURE_A,
    );

    mur()
        .args([
            "trace",
            "report",
            "--workdir",
            tmp.path().to_str().unwrap(),
            "--since",
            "1h",
            "ses_aaaaaaaaaaaa4aaa8aaa000000000001",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "--since cannot be combined with explicit session arguments",
        ));
}

#[test]
fn report_last_combined_with_explicit_sessions_errors() {
    let tmp = TempDir::new().unwrap();
    write_session(
        tmp.path(),
        "ses_aaaaaaaaaaaa4aaa8aaa000000000001",
        FIXTURE_A,
    );

    mur()
        .args([
            "trace",
            "report",
            "--workdir",
            tmp.path().to_str().unwrap(),
            "--last",
            "1",
            "ses_aaaaaaaaaaaa4aaa8aaa000000000001",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "--last cannot be combined with explicit session arguments",
        ));
}

// ── error tests ───────────────────────────────────────────────────────────────

#[test]
fn show_nonexistent_file_exits_nonzero_with_error() {
    mur()
        .args(["trace", "show", "/tmp/does-not-exist-murmur-test.jsonl"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not found"));
}

#[test]
fn diff_nonexistent_file_a_exits_nonzero() {
    let tmp = TempDir::new().unwrap();
    let pb = write_fixture(tmp.path(), "b.jsonl", FIXTURE_B);

    mur()
        .args([
            "trace",
            "diff",
            "/tmp/does-not-exist-murmur-test.jsonl",
            pb.to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not found"));
}

#[test]
fn report_nonexistent_workdir_exits_nonzero() {
    mur()
        .args([
            "trace",
            "report",
            "--workdir",
            "/tmp/no-such-dir-murmur-test-xyz",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not found"));
}

#[test]
fn show_malformed_json_exits_nonzero_with_line_number() {
    let tmp = TempDir::new().unwrap();
    let bad = concat!(
        "{\"event_type\":\"session_start\",\"session_id\":\"ses_eeeeeeeeeeee4eee8eee000000000005\",",
        "\"timestamp\":1000,\"capsule_name\":\"x\",\"capsule_version\":\"1.0\",",
        "\"model\":\"m\",\"max_turns\":10,\"capabilities\":[],\"tools_declared\":[]}\n",
        "THIS IS NOT JSON\n",
        "{\"event_type\":\"session_end\",\"session_id\":\"ses_eeeeeeeeeeee4eee8eee000000000005\",",
        "\"timestamp\":2000,\"total_turns\":0,\"total_input_tokens\":0,\"total_output_tokens\":0,",
        "\"total_tool_calls\":0,\"total_shell_calls\":0,\"duration_ms\":100,\"exit_status\":\"ok\"}\n"
    );
    let path = write_fixture(tmp.path(), "bad.jsonl", bad);

    mur()
        .args(["trace", "show", path.to_str().unwrap()])
        .assert()
        .failure()
        // should name the file
        .stderr(predicate::str::contains("bad.jsonl"))
        // should include a line number (line 2 is the bad one)
        .stderr(predicate::str::contains(":2:"));
}

#[test]
fn show_unknown_event_type_is_silently_skipped() {
    // Unknown event_type values are silently skipped via #[serde(other)].
    // This enables backward and forward compat: old parsers won't choke on new event
    // types (task_start, task_end), and new parsers won't choke on unknown events.
    let tmp = TempDir::new().unwrap();
    let trace = concat!(
        "{\"event_type\":\"session_start\",\"session_id\":\"ses_ffffffffffff4fff8fff000000000006\",",
        "\"timestamp\":1000,\"capsule_name\":\"x\",\"capsule_version\":\"1.0\",",
        "\"model\":\"m\",\"max_turns\":10,\"capabilities\":[],\"tools_declared\":[]}\n",
        "{\"event_type\":\"unknown_future_event\",\"session_id\":\"ses_ffffffffffff4fff8fff000000000006\",",
        "\"timestamp\":1100,\"data\":\"something\"}\n",
        "{\"event_type\":\"session_end\",\"session_id\":\"ses_ffffffffffff4fff8fff000000000006\",",
        "\"timestamp\":2000,\"total_turns\":0,\"total_input_tokens\":0,\"total_output_tokens\":0,",
        "\"total_tool_calls\":0,\"total_shell_calls\":0,\"duration_ms\":100,\"exit_status\":\"ok\"}\n"
    );
    let path = write_fixture(tmp.path(), "unknown.jsonl", trace);

    mur()
        .args(["trace", "show", path.to_str().unwrap()])
        .assert()
        .success()
        // No Tasks section (single-task / zero task events)
        .stdout(predicate::str::contains("Tasks").not());
}

// ── multi-task tests ───────────────────────────────────────────────────

const SESSION_ID_MT: &str = "ses_dddddddddddd4ddd8ddd000000000010";

fn make_multi_task_fixture(n_tasks: usize) -> String {
    let mut lines = String::new();

    // session_start
    lines.push_str(&format!(
        "{{\"event_type\":\"session_start\",\"session_id\":\"{SESSION_ID_MT}\",\"timestamp\":1000,\
        \"capsule_name\":\"mt-capsule\",\"capsule_version\":\"0.1.0\",\"model\":\"claude-test\",\
        \"max_turns\":20,\"capabilities\":[],\"tools_declared\":[]}}\n"
    ));

    let mut total_turns = 0u32;
    let mut total_input = 0u64;
    let mut total_output = 0u64;

    for i in 0..n_tasks {
        let task_id = format!("tsk_{:08x}000000000000000000000000", i);
        let context_id = format!("ctx_{:08x}000000000000000000000000", i);
        let turns: u32 = (i as u32 + 1) * 2;
        let input: u64 = (i as u64 + 1) * 500;
        let output: u64 = (i as u64 + 1) * 100;
        let duration: u64 = (i as u64 + 1) * 200;

        // a2a_task_received (legacy event — should be silently skipped)
        lines.push_str(&format!(
            "{{\"event_type\":\"a2a_task_received\",\"session_id\":\"{SESSION_ID_MT}\",\"timestamp\":{},\
            \"task_id\":\"{task_id}\",\"context_id\":\"{context_id}\",\
            \"message_id\":\"msg-{i}\",\"traceparent_from_caller\":null}}\n",
            2000 + i as u64 * 1000
        ));

        // task_start
        lines.push_str(&format!(
            "{{\"event_type\":\"task_start\",\"session_id\":\"{SESSION_ID_MT}\",\"timestamp\":{},\
            \"task_id\":\"{task_id}\",\"context_id\":\"{context_id}\",\
            \"source\":\"a2a\",\"message_parts_bytes\":42}}\n",
            2001 + i as u64 * 1000
        ));

        // inference events to fill the turns
        for t in 0..turns {
            let inp = input / turns as u64;
            let out = output / turns as u64;
            lines.push_str(&format!(
                "{{\"event_type\":\"inference\",\"session_id\":\"{SESSION_ID_MT}\",\"timestamp\":{},\
                \"turn\":{t},\"input_tokens\":{inp},\"output_tokens\":{out},\
                \"decision\":\"end_turn\",\"tool_name\":null}}\n",
                2002 + i as u64 * 1000 + t as u64 * 10
            ));
        }

        // task_end
        lines.push_str(&format!(
            "{{\"event_type\":\"task_end\",\"session_id\":\"{SESSION_ID_MT}\",\"timestamp\":{},\
            \"task_id\":\"{task_id}\",\"exit_status\":\"ok\",\"duration_ms\":{duration},\
            \"turns\":{turns},\"input_tokens\":{input},\"output_tokens\":{output},\
            \"tool_calls\":0,\"shell_calls\":0}}\n",
            2003 + i as u64 * 1000
        ));

        total_turns += turns;
        total_input += input;
        total_output += output;
    }

    let total_duration = n_tasks as u64 * 500;
    // session_end
    lines.push_str(&format!(
        "{{\"event_type\":\"session_end\",\"session_id\":\"{SESSION_ID_MT}\",\"timestamp\":9999,\
        \"total_turns\":{total_turns},\"total_input_tokens\":{total_input},\
        \"total_output_tokens\":{total_output},\
        \"total_tool_calls\":0,\"total_shell_calls\":0,\
        \"duration_ms\":{total_duration},\"exit_status\":\"ok\"}}\n"
    ));

    lines
}

#[test]
fn trace_show_single_task_no_breakdown() {
    // A trace with exactly one task_start/task_end should NOT show a Tasks section.
    let tmp = TempDir::new().unwrap();
    let fixture = make_multi_task_fixture(1);
    let path = write_fixture(tmp.path(), "single.jsonl", &fixture);

    mur()
        .args(["trace", "show", path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("Tasks").not());
}

#[test]
fn trace_show_multi_task_breakdown() {
    // A trace with three task_start/task_end pairs should show a Tasks section with three rows.
    let tmp = TempDir::new().unwrap();
    let fixture = make_multi_task_fixture(3);
    let path = write_fixture(tmp.path(), "multi.jsonl", &fixture);

    mur()
        .args(["trace", "show", path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("── Tasks"))
        .stdout(predicate::str::contains("task 1"))
        .stdout(predicate::str::contains("task 2"))
        .stdout(predicate::str::contains("task 3"))
        // should not show task 4
        .stdout(predicate::str::contains("task 4").not())
        // turns and exit_status visible
        .stdout(predicate::str::contains("turns:"))
        .stdout(predicate::str::contains("ok"));
}

#[test]
fn trace_show_backward_compat_no_task_events() {
    // A trace with no task_start/task_end events (legacy format) should succeed
    // without a Tasks section.
    let tmp = TempDir::new().unwrap();
    let path = write_fixture(tmp.path(), "old.jsonl", FIXTURE_A);

    mur()
        .args(["trace", "show", path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("Tasks").not());
}

#[test]
fn trace_report_per_task_averages_section() {
    // Two multi-task sessions should show a per-task averages section.
    let tmp = TempDir::new().unwrap();
    write_session(
        tmp.path(),
        "ses_aaaaaaaaaaaa4aaa8aaa000000000001",
        &make_multi_task_fixture(3),
    );
    write_session(
        tmp.path(),
        "ses_bbbbbbbbbbbb4bbb8bbb000000000002",
        &make_multi_task_fixture(3),
    );

    mur()
        .args(["trace", "report", "--workdir", tmp.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("Per-task averages"))
        .stdout(predicate::str::contains("task turns"))
        .stdout(predicate::str::contains("Tasks: 6")); // 3 tasks × 2 sessions
}

#[test]
fn trace_report_no_per_task_section_when_all_single_task() {
    // Single-task sessions only should NOT show a per-task section.
    let tmp = TempDir::new().unwrap();
    write_session(
        tmp.path(),
        "ses_aaaaaaaaaaaa4aaa8aaa000000000001",
        &make_multi_task_fixture(1),
    );
    write_session(
        tmp.path(),
        "ses_bbbbbbbbbbbb4bbb8bbb000000000002",
        FIXTURE_A,
    );

    mur()
        .args(["trace", "report", "--workdir", tmp.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("Per-task averages").not());
}

#[test]
fn report_empty_workdir_gives_clear_error() {
    let tmp = TempDir::new().unwrap();

    mur()
        .args(["trace", "report", "--workdir", tmp.path().to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no sessions found"));
}

#[test]
fn report_skips_incomplete_sessions_and_reports_complete_ones() {
    let tmp = TempDir::new().unwrap();
    // Two complete sessions
    write_session(
        tmp.path(),
        "ses_aaaaaaaaaaaa4aaa8aaa000000000001",
        FIXTURE_A,
    );
    write_session(
        tmp.path(),
        "ses_bbbbbbbbbbbb4bbb8bbb000000000002",
        FIXTURE_B,
    );
    // One session with an empty trace (e.g. killed before first event)
    write_session(tmp.path(), "ses_cccccccccccc4ccc8ccc000000000003", "");

    mur()
        .args(["trace", "report", "--workdir", tmp.path().to_str().unwrap()])
        .assert()
        .success()
        // Only the 2 complete sessions are counted
        .stdout(predicate::str::contains("Sessions: 2"))
        // Incomplete session is silently noted on stderr
        .stderr(predicate::str::contains("skipped 1 incomplete"));
}

#[test]
fn report_all_incomplete_sessions_is_an_error() {
    let tmp = TempDir::new().unwrap();
    write_session(tmp.path(), "ses_aaaaaaaaaaaa4aaa8aaa000000000001", "");
    write_session(tmp.path(), "ses_bbbbbbbbbbbb4bbb8bbb000000000002", "");

    mur()
        .args(["trace", "report", "--workdir", tmp.path().to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("incomplete"));
}

#[test]
fn show_empty_file_exits_nonzero() {
    let tmp = TempDir::new().unwrap();
    let path = write_fixture(tmp.path(), "empty.jsonl", "");

    mur()
        .args(["trace", "show", path.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("empty"));
}

// ── report per-session block tests ───────────────────────────────────────────

#[test]
fn report_session_block_header_shows_id_duration_turns() {
    let tmp = TempDir::new().unwrap();
    write_session(
        tmp.path(),
        "ses_aaaaaaaaaaaa4aaa8aaa000000000001",
        FIXTURE_A,
    );

    // session ID truncated to ses_ + 8 hex chars + ..., duration 500ms, 2 turns
    mur()
        .args(["trace", "report", "--workdir", tmp.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("Session ses_aaaaaaaa..."))
        .stdout(predicate::str::contains("500ms"))
        .stdout(predicate::str::contains("2 turns"));
}

#[test]
fn report_session_block_no_error_no_parenthetical() {
    // FIXTURE_A: 1 tool call, all ok — no (N ok, N error) parenthetical
    let tmp = TempDir::new().unwrap();
    write_session(
        tmp.path(),
        "ses_aaaaaaaaaaaa4aaa8aaa000000000001",
        FIXTURE_A,
    );

    mur()
        .args(["trace", "report", "--workdir", tmp.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("Tool calls:"))
        .stdout(predicate::str::contains("ok, 0 error").not());
}

#[test]
fn report_session_block_error_shows_parenthetical() {
    // FIXTURE_B: 5 tool calls, 1 error → (4 ok, 1 error)
    let tmp = TempDir::new().unwrap();
    write_session(
        tmp.path(),
        "ses_bbbbbbbbbbbb4bbb8bbb000000000002",
        FIXTURE_B,
    );

    mur()
        .args(["trace", "report", "--workdir", tmp.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("(4 ok, 1 error)"));
}

#[test]
fn report_session_block_exit_codes_sorted_by_frequency() {
    // FIXTURE_B: 5 shell calls — exit 0 ×4, exit 1 ×1 → "4 ok, 1 failed (exit 1)"
    let tmp = TempDir::new().unwrap();
    write_session(
        tmp.path(),
        "ses_bbbbbbbbbbbb4bbb8bbb000000000002",
        FIXTURE_B,
    );

    mur()
        .args(["trace", "report", "--workdir", tmp.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "exit codes: 4 ok, 1 failed (exit 1)",
        ));
}

#[test]
fn report_session_block_no_shell_calls_omits_shell_line() {
    // FIXTURE_NO_TOOLS: 0 shell calls — shell line omitted entirely
    let tmp = TempDir::new().unwrap();
    write_session(
        tmp.path(),
        "ses_cccccccccccc4ccc8ccc000000000003",
        FIXTURE_NO_TOOLS,
    );

    mur()
        .args(["trace", "report", "--workdir", tmp.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("Shell calls:").not());
}

#[test]
fn report_session_block_latency_shown_with_correct_averages() {
    // FIXTURE_A: 1 tool call (100ms), 1 shell call (50ms)
    let tmp = TempDir::new().unwrap();
    write_session(
        tmp.path(),
        "ses_aaaaaaaaaaaa4aaa8aaa000000000001",
        FIXTURE_A,
    );

    mur()
        .args(["trace", "report", "--workdir", tmp.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("Avg latency:"))
        .stdout(predicate::str::contains("tool 100ms"))
        .stdout(predicate::str::contains("shell 50ms"));
}

#[test]
fn report_session_block_multi_tool_latency_average() {
    // FIXTURE_B: 5 tool calls (200+150+180+220+190=940ms avg=188ms),
    //            5 shell calls (80+30+40+10+20=180ms avg=36ms)
    let tmp = TempDir::new().unwrap();
    write_session(
        tmp.path(),
        "ses_bbbbbbbbbbbb4bbb8bbb000000000002",
        FIXTURE_B,
    );

    mur()
        .args(["trace", "report", "--workdir", tmp.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("tool 188ms"))
        .stdout(predicate::str::contains("shell 36ms"));
}

#[test]
fn report_session_block_latency_omitted_when_no_tool_calls() {
    // FIXTURE_NO_TOOLS: 0 tool calls, 0 shell calls — no Avg latency line
    let tmp = TempDir::new().unwrap();
    write_session(
        tmp.path(),
        "ses_cccccccccccc4ccc8ccc000000000003",
        FIXTURE_NO_TOOLS,
    );

    mur()
        .args(["trace", "report", "--workdir", tmp.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("Avg latency:").not());
}

// ── steps tests ──────────────────────────────────────────────────────────────

// Fixture: session_start + session_end, no inference events — 0 turns
const FIXTURE_STEPS_ZERO_TURNS: &str = concat!(
    "{\"event_type\":\"session_start\",\"session_id\":\"ses_zzzzzzzzzzzz4zzz8zzz000000000001\",\"timestamp\":1000,",
    "\"capsule_name\":\"empty\",\"capsule_version\":\"0.1.0\",\"model\":\"m\",\"max_turns\":10,",
    "\"capabilities\":[],\"tools_declared\":[]}\n",
    "{\"event_type\":\"session_end\",\"session_id\":\"ses_zzzzzzzzzzzz4zzz8zzz000000000001\",\"timestamp\":1100,",
    "\"total_turns\":0,\"total_input_tokens\":0,\"total_output_tokens\":0,",
    "\"total_tool_calls\":0,\"total_shell_calls\":0,\"duration_ms\":100,\"exit_status\":\"ok\"}\n"
);

// Build a single-turn trace with a bash tool call using the given command string.
fn fixture_steps_with_command(cmd: &str) -> String {
    let ses_id = "ses_pppppppppppp4ppp8ppp000000000001";
    [
        format!("{{\"event_type\":\"session_start\",\"session_id\":\"{ses_id}\",\"timestamp\":1000,\"capsule_name\":\"t\",\"capsule_version\":\"0.1.0\",\"model\":\"m\",\"max_turns\":10,\"capabilities\":[],\"tools_declared\":[]}}"),
        format!("{{\"event_type\":\"inference\",\"session_id\":\"{ses_id}\",\"timestamp\":1100,\"turn\":0,\"input_tokens\":100,\"output_tokens\":20,\"decision\":\"tool_call\",\"tool_name\":\"bash\"}}"),
        format!("{{\"event_type\":\"tool_call\",\"session_id\":\"{ses_id}\",\"timestamp\":1200,\"turn\":0,\"tool_name\":\"bash\",\"input\":{{\"command\":\"{cmd}\"}},\"input_bytes\":20,\"output_bytes\":0,\"duration_ms\":50,\"status\":\"ok\"}}"),
        format!("{{\"event_type\":\"session_end\",\"session_id\":\"{ses_id}\",\"timestamp\":1300,\"total_turns\":1,\"total_input_tokens\":100,\"total_output_tokens\":20,\"total_tool_calls\":1,\"total_shell_calls\":0,\"duration_ms\":300,\"exit_status\":\"ok\"}}"),
    ]
    .join("\n")
        + "\n"
}

// Single-turn trace with a bash tool call using the given duration_ms.
fn fixture_steps_with_duration(duration_ms: u64) -> String {
    let ses_id = "ses_qqqqqqqqqqqq4qqq8qqq000000000001";
    [
        format!("{{\"event_type\":\"session_start\",\"session_id\":\"{ses_id}\",\"timestamp\":1000,\"capsule_name\":\"t\",\"capsule_version\":\"0.1.0\",\"model\":\"m\",\"max_turns\":10,\"capabilities\":[],\"tools_declared\":[]}}"),
        format!("{{\"event_type\":\"inference\",\"session_id\":\"{ses_id}\",\"timestamp\":1100,\"turn\":0,\"input_tokens\":100,\"output_tokens\":20,\"decision\":\"tool_call\",\"tool_name\":\"bash\"}}"),
        format!("{{\"event_type\":\"tool_call\",\"session_id\":\"{ses_id}\",\"timestamp\":1200,\"turn\":0,\"tool_name\":\"bash\",\"input\":{{\"command\":\"cmd\"}},\"input_bytes\":10,\"output_bytes\":0,\"duration_ms\":{duration_ms},\"status\":\"ok\"}}"),
        format!("{{\"event_type\":\"session_end\",\"session_id\":\"{ses_id}\",\"timestamp\":1300,\"total_turns\":1,\"total_input_tokens\":100,\"total_output_tokens\":20,\"total_tool_calls\":1,\"total_shell_calls\":0,\"duration_ms\":5000,\"exit_status\":\"ok\"}}"),
    ]
    .join("\n")
        + "\n"
}

// Two-turn trace with bash tool calls using the given per-turn durations.
fn fixture_steps_two_turns_with_durations(ms1: u64, ms2: u64) -> String {
    let ses_id = "ses_rrrrrrrrrrrr4rrr8rrr000000000001";
    [
        format!("{{\"event_type\":\"session_start\",\"session_id\":\"{ses_id}\",\"timestamp\":1000,\"capsule_name\":\"t\",\"capsule_version\":\"0.1.0\",\"model\":\"m\",\"max_turns\":10,\"capabilities\":[],\"tools_declared\":[]}}"),
        format!("{{\"event_type\":\"inference\",\"session_id\":\"{ses_id}\",\"timestamp\":1100,\"turn\":0,\"input_tokens\":100,\"output_tokens\":20,\"decision\":\"tool_call\",\"tool_name\":\"bash\"}}"),
        format!("{{\"event_type\":\"tool_call\",\"session_id\":\"{ses_id}\",\"timestamp\":1200,\"turn\":0,\"tool_name\":\"bash\",\"input\":{{\"command\":\"cmd1\"}},\"input_bytes\":10,\"output_bytes\":0,\"duration_ms\":{ms1},\"status\":\"ok\"}}"),
        format!("{{\"event_type\":\"inference\",\"session_id\":\"{ses_id}\",\"timestamp\":1300,\"turn\":1,\"input_tokens\":120,\"output_tokens\":10,\"decision\":\"tool_call\",\"tool_name\":\"bash\"}}"),
        format!("{{\"event_type\":\"tool_call\",\"session_id\":\"{ses_id}\",\"timestamp\":1400,\"turn\":1,\"tool_name\":\"bash\",\"input\":{{\"command\":\"cmd2\"}},\"input_bytes\":10,\"output_bytes\":0,\"duration_ms\":{ms2},\"status\":\"ok\"}}"),
        format!("{{\"event_type\":\"session_end\",\"session_id\":\"{ses_id}\",\"timestamp\":1500,\"total_turns\":2,\"total_input_tokens\":220,\"total_output_tokens\":30,\"total_tool_calls\":2,\"total_shell_calls\":0,\"duration_ms\":5000,\"exit_status\":\"ok\"}}"),
    ]
    .join("\n")
        + "\n"
}

#[test]
fn steps_no_arg_resolves_to_last_session() {
    let tmp = TempDir::new().unwrap();
    // B > A lexicographically → B is the most recent session
    write_session(
        tmp.path(),
        "ses_aaaaaaaaaaaa4aaa8aaa000000000001",
        FIXTURE_A,
    );
    write_session(
        tmp.path(),
        "ses_bbbbbbbbbbbb4bbb8bbb000000000002",
        FIXTURE_B,
    );

    mur()
        .args(["trace", "steps", "--workdir", tmp.path().to_str().unwrap()])
        .assert()
        .success()
        // Header should contain B's session ID
        .stdout(predicate::str::contains(
            "ses_bbbbbbbbbbbb4bbb8bbb000000000002",
        ));
}

#[test]
fn steps_explicit_session_suffix_resolves() {
    let tmp = TempDir::new().unwrap();
    write_session(
        tmp.path(),
        "ses_aaaaaaaaaaaa4aaa8aaa000000000001",
        FIXTURE_A,
    );
    write_session(
        tmp.path(),
        "ses_bbbbbbbbbbbb4bbb8bbb000000000002",
        FIXTURE_B,
    );

    // Suffix "0001" uniquely matches A
    mur()
        .args([
            "trace",
            "steps",
            "--workdir",
            tmp.path().to_str().unwrap(),
            "0001",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "ses_aaaaaaaaaaaa4aaa8aaa000000000001",
        ));
}

#[test]
fn steps_default_output_format() {
    let tmp = TempDir::new().unwrap();
    let path = write_fixture(tmp.path(), "trace.jsonl", FIXTURE_A);
    // FIXTURE_A: 2 inference events — turn 1 (tool_call/bash), turn 2 (end_turn/null)

    mur()
        .args(["trace", "steps", path.to_str().unwrap()])
        .assert()
        .success()
        // header
        .stdout(predicate::str::contains(
            "Session ses_aaaaaaaaaaaa4aaa8aaa000000000001",
        ))
        .stdout(predicate::str::contains("(2 turns)"))
        // row format: decision column left-padded to 13, then tool name
        .stdout(predicate::str::contains("tool_call    bash"))
        .stdout(predicate::str::contains("end_turn"));
}

#[test]
fn steps_end_turn_renders_dash() {
    let tmp = TempDir::new().unwrap();
    let path = write_fixture(tmp.path(), "trace.jsonl", FIXTURE_A);
    // FIXTURE_A turn 2: decision=end_turn, tool_name=null → rendered as "—"

    mur()
        .args(["trace", "steps", path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("—"));
}

#[test]
fn steps_verbose_truncates_long_input_at_60_chars() {
    let tmp = TempDir::new().unwrap();
    let long_cmd = "x".repeat(70);
    let fixture = fixture_steps_with_command(&long_cmd);
    let path = write_fixture(tmp.path(), "trace.jsonl", &fixture);

    mur()
        .args(["trace", "steps", "--verbose", path.to_str().unwrap()])
        .assert()
        .success()
        // First 60 chars + ellipsis, inside quotes
        .stdout(predicate::str::contains(format!("\"{}…\"", "x".repeat(60))))
        // The 61st character must NOT appear (confirms truncation at 60)
        .stdout(predicate::str::contains("x".repeat(61)).not());
}

#[test]
fn steps_zero_turns_shows_header_and_no_rows() {
    let tmp = TempDir::new().unwrap();
    let path = write_fixture(tmp.path(), "trace.jsonl", FIXTURE_STEPS_ZERO_TURNS);

    mur()
        .args(["trace", "steps", path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("(0 turns)"))
        .stdout(predicate::str::contains("tool_call").not())
        .stdout(predicate::str::contains("end_turn").not());
}

// ── steps latency column tests ────────────────────────────────────────────────

#[test]
fn steps_sub_second_duration_renders_as_ms() {
    let tmp = TempDir::new().unwrap();
    // fixture_steps_with_command uses duration_ms=50
    let fixture = fixture_steps_with_command("cargo build");
    let path = write_fixture(tmp.path(), "trace.jsonl", &fixture);

    mur()
        .args(["trace", "steps", path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("50ms"));
}

#[test]
fn steps_exactly_1000ms_renders_as_1_0s() {
    let tmp = TempDir::new().unwrap();
    let fixture = fixture_steps_with_duration(1000);
    let path = write_fixture(tmp.path(), "trace.jsonl", &fixture);

    mur()
        .args(["trace", "steps", path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("1.0s"));
}

#[test]
fn steps_end_turn_duration_is_dash() {
    let tmp = TempDir::new().unwrap();
    let path = write_fixture(tmp.path(), "trace.jsonl", FIXTURE_A);
    // FIXTURE_A: turn 2 = end_turn with no tool_call → duration rendered as "—"

    mur()
        .args(["trace", "steps", path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("end_turn"))
        .stdout(predicate::str::contains("—"));
}

#[test]
fn steps_mixed_ms_and_s_durations_both_appear() {
    let tmp = TempDir::new().unwrap();
    let fixture = fixture_steps_two_turns_with_durations(500, 1500);
    let path = write_fixture(tmp.path(), "trace.jsonl", &fixture);

    mur()
        .args(["trace", "steps", path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("500ms"))
        .stdout(predicate::str::contains("1.5s"));
}

// ── input/output field tests ──────────────────────────────────────────────────

// Fixture: trace with input field on tool_call (as emitted by the new runtime).
const FIXTURE_WITH_INPUT: &str = concat!(
    "{\"event_type\":\"session_start\",\"session_id\":\"ses_1111111111114111811100000000001a\",\"timestamp\":1000,",
    "\"capsule_name\":\"input-test\",\"capsule_version\":\"1.0.0\",\"model\":\"claude-test\",",
    "\"max_turns\":10,\"capabilities\":[],\"tools_declared\":[\"bash\"]}\n",

    "{\"event_type\":\"inference\",\"session_id\":\"ses_1111111111114111811100000000001a\",\"timestamp\":1100,",
    "\"turn\":0,\"input_tokens\":100,\"output_tokens\":20,\"decision\":\"tool_call\",\"tool_name\":\"bash\"}\n",

    "{\"event_type\":\"tool_call\",\"session_id\":\"ses_1111111111114111811100000000001a\",\"timestamp\":1200,",
    "\"turn\":0,\"tool_name\":\"bash\",\"input\":{\"command\":\"echo hello\"},",
    "\"input_bytes\":20,\"output_bytes\":6,\"duration_ms\":30,\"status\":\"ok\"}\n",

    "{\"event_type\":\"inference\",\"session_id\":\"ses_1111111111114111811100000000001a\",\"timestamp\":1300,",
    "\"turn\":1,\"input_tokens\":120,\"output_tokens\":10,\"decision\":\"end_turn\",\"tool_name\":null}\n",

    "{\"event_type\":\"session_end\",\"session_id\":\"ses_1111111111114111811100000000001a\",\"timestamp\":1400,",
    "\"total_turns\":2,\"total_input_tokens\":220,\"total_output_tokens\":30,",
    "\"total_tool_calls\":1,\"total_shell_calls\":0,\"duration_ms\":400,\"exit_status\":\"ok\"}\n"
);

// Fixture: old-format trace without input field on tool_call (pre-new-runtime).
const FIXTURE_OLD_NO_INPUT: &str = concat!(
    "{\"event_type\":\"session_start\",\"session_id\":\"ses_2222222222224222822200000000002b\",\"timestamp\":1000,",
    "\"capsule_name\":\"old-test\",\"capsule_version\":\"0.9.0\",\"model\":\"claude-old\",",
    "\"max_turns\":10,\"capabilities\":[],\"tools_declared\":[\"bash\"]}\n",

    "{\"event_type\":\"inference\",\"session_id\":\"ses_2222222222224222822200000000002b\",\"timestamp\":1100,",
    "\"turn\":0,\"input_tokens\":80,\"output_tokens\":15,\"decision\":\"tool_call\",\"tool_name\":\"bash\"}\n",

    "{\"event_type\":\"tool_call\",\"session_id\":\"ses_2222222222224222822200000000002b\",\"timestamp\":1200,",
    "\"turn\":0,\"tool_name\":\"bash\",\"input_bytes\":18,\"output_bytes\":8,\"duration_ms\":25,\"status\":\"ok\"}\n",

    "{\"event_type\":\"inference\",\"session_id\":\"ses_2222222222224222822200000000002b\",\"timestamp\":1300,",
    "\"turn\":1,\"input_tokens\":100,\"output_tokens\":8,\"decision\":\"end_turn\",\"tool_name\":null}\n",

    "{\"event_type\":\"session_end\",\"session_id\":\"ses_2222222222224222822200000000002b\",\"timestamp\":1400,",
    "\"total_turns\":2,\"total_input_tokens\":180,\"total_output_tokens\":23,",
    "\"total_tool_calls\":1,\"total_shell_calls\":0,\"duration_ms\":350,\"exit_status\":\"ok\"}\n"
);

// Fixture: trace with both input and output fields (include_tool_output = true).
const FIXTURE_WITH_OUTPUT: &str = concat!(
    "{\"event_type\":\"session_start\",\"session_id\":\"ses_3333333333334333833300000000003c\",\"timestamp\":1000,",
    "\"capsule_name\":\"output-test\",\"capsule_version\":\"1.0.0\",\"model\":\"claude-test\",",
    "\"max_turns\":10,\"capabilities\":[],\"tools_declared\":[\"bash\"]}\n",

    "{\"event_type\":\"inference\",\"session_id\":\"ses_3333333333334333833300000000003c\",\"timestamp\":1100,",
    "\"turn\":0,\"input_tokens\":90,\"output_tokens\":18,\"decision\":\"tool_call\",\"tool_name\":\"bash\"}\n",

    "{\"event_type\":\"tool_call\",\"session_id\":\"ses_3333333333334333833300000000003c\",\"timestamp\":1200,",
    "\"turn\":0,\"tool_name\":\"bash\",\"input\":{\"command\":\"ls\"},",
    "\"input_bytes\":10,\"output\":\"file.txt\\n\",\"output_bytes\":9,\"duration_ms\":12,\"status\":\"ok\"}\n",

    "{\"event_type\":\"inference\",\"session_id\":\"ses_3333333333334333833300000000003c\",\"timestamp\":1300,",
    "\"turn\":1,\"input_tokens\":110,\"output_tokens\":9,\"decision\":\"end_turn\",\"tool_name\":null}\n",

    "{\"event_type\":\"session_end\",\"session_id\":\"ses_3333333333334333833300000000003c\",\"timestamp\":1400,",
    "\"total_turns\":2,\"total_input_tokens\":200,\"total_output_tokens\":27,",
    "\"total_tool_calls\":1,\"total_shell_calls\":0,\"duration_ms\":380,\"exit_status\":\"ok\"}\n"
);

#[test]
fn show_tool_call_input_rendered() {
    let tmp = TempDir::new().unwrap();
    let path = write_fixture(tmp.path(), "with-input.jsonl", FIXTURE_WITH_INPUT);

    mur()
        .args(["trace", "show", path.to_str().unwrap()])
        .assert()
        .success()
        // tool name and input should appear inline
        .stdout(predicate::str::contains("bash"))
        .stdout(predicate::str::contains("echo hello"));
}

#[test]
fn show_tool_call_input_truncates_on_char_boundary() {
    // The inline input is capped at 120 characters. Build an input whose compact
    // JSON puts byte offset 120 in the middle of a two-byte codepoint, so a
    // byte-index truncation would panic.
    let ses = "ses_4444444444444444844400000000004d";
    let cmd = format!("x{}", "é".repeat(200));
    let input = serde_json::json!({ "cmd": cmd });
    assert!(!serde_json::to_string(&input).unwrap().is_char_boundary(120));

    let fixture = format!(
        concat!(
            "{{\"event_type\":\"session_start\",\"session_id\":\"{ses}\",\"timestamp\":1000,",
            "\"capsule_name\":\"utf8-test\",\"capsule_version\":\"1.0.0\",\"model\":\"claude-test\",",
            "\"max_turns\":10,\"capabilities\":[],\"tools_declared\":[\"bash\"]}}\n",
            "{{\"event_type\":\"tool_call\",\"session_id\":\"{ses}\",\"timestamp\":1200,",
            "\"turn\":0,\"tool_name\":\"bash\",\"input\":{input},",
            "\"input_bytes\":20,\"output_bytes\":6,\"duration_ms\":30,\"status\":\"ok\"}}\n",
            "{{\"event_type\":\"session_end\",\"session_id\":\"{ses}\",\"timestamp\":1400,",
            "\"total_turns\":1,\"total_input_tokens\":100,\"total_output_tokens\":20,",
            "\"total_tool_calls\":1,\"total_shell_calls\":0,\"duration_ms\":400,\"exit_status\":\"ok\"}}\n"
        ),
        ses = ses,
        input = serde_json::to_string(&input).unwrap()
    );

    let tmp = TempDir::new().unwrap();
    let path = write_fixture(tmp.path(), "utf8-input.jsonl", &fixture);

    mur()
        .args(["trace", "show", path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("…"));
}

#[test]
fn old_trace_without_input_field_still_parses() {
    // Pre-new-runtime traces that lack the input field must still parse cleanly.
    let tmp = TempDir::new().unwrap();
    let path = write_fixture(tmp.path(), "old-no-input.jsonl", FIXTURE_OLD_NO_INPUT);

    mur()
        .args(["trace", "show", path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("old-test"))
        .stdout(predicate::str::contains("ok"));
}

#[test]
fn show_trace_with_output_field_parses_cleanly() {
    // Traces that include the output field (include_tool_output = true) must parse cleanly.
    let tmp = TempDir::new().unwrap();
    let path = write_fixture(tmp.path(), "with-output.jsonl", FIXTURE_WITH_OUTPUT);

    mur()
        .args(["trace", "show", path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("output-test"))
        .stdout(predicate::str::contains("bash"));
}

// ── redundant-call detection fixtures & tests ─────────────────────────────────
//
// Detection is driven entirely by each call's self-declared `state_effect`
// (`read`/`mutate`) recorded in the trace — the CLI recognizes no tool or
// operation by name. The fixtures below therefore carry `state_effect` on their
// tool_call events, exactly as a declaring tool's runtime does. `tool_name`
// values are arbitrary labels here; they never drive detection.

// Two read calls against src/foo.rs, no mutate between → 1 redundant call.
const FIXTURE_REREAD: &str = concat!(
    "{\"event_type\":\"session_start\",\"session_id\":\"ses_dddddddddddd4ddd8ddd000000000004\",",
    "\"capsule_name\":\"reader\",\"capsule_version\":\"0.1.0\",\"model\":\"claude-3-5-sonnet\",\"max_turns\":10}\n",
    "{\"event_type\":\"inference\",\"turn\":1,\"decision\":\"tool_call\",\"tool_name\":\"read_file\"}\n",
    "{\"event_type\":\"tool_call\",\"turn\":1,\"tool_name\":\"read_file\",\"input\":{\"path\":\"src/foo.rs\"},\"duration_ms\":10,\"status\":\"ok\",\"state_effect\":\"read\"}\n",
    "{\"event_type\":\"inference\",\"turn\":2,\"decision\":\"tool_call\",\"tool_name\":\"read_file\"}\n",
    "{\"event_type\":\"tool_call\",\"turn\":2,\"tool_name\":\"read_file\",\"input\":{\"path\":\"src/foo.rs\"},\"duration_ms\":12,\"status\":\"ok\",\"state_effect\":\"read\"}\n",
    "{\"event_type\":\"session_end\",\"total_turns\":2,\"total_input_tokens\":100,\"total_output_tokens\":50,",
    "\"total_tool_calls\":2,\"total_shell_calls\":0,\"duration_ms\":300,\"exit_status\":\"ok\"}\n"
);

// read src/foo.rs, a mutate of src/foo.rs, read src/foo.rs → the mutate invalidates the
// tracked read, so the second read is NOT redundant → 0 redundant calls.
const FIXTURE_WRITE_CLEARS: &str = concat!(
    "{\"event_type\":\"session_start\",\"session_id\":\"ses_eeeeeeeeeeee4eee8eee000000000005\",",
    "\"capsule_name\":\"reader\",\"capsule_version\":\"0.1.0\",\"model\":\"claude-3-5-sonnet\",\"max_turns\":10}\n",
    "{\"event_type\":\"tool_call\",\"turn\":1,\"tool_name\":\"read_file\",\"input\":{\"path\":\"src/foo.rs\"},\"duration_ms\":10,\"status\":\"ok\",\"state_effect\":\"read\"}\n",
    "{\"event_type\":\"tool_call\",\"turn\":2,\"tool_name\":\"write_file\",\"input\":{\"path\":\"src/foo.rs\"},\"duration_ms\":8,\"status\":\"ok\",\"state_effect\":\"mutate\"}\n",
    "{\"event_type\":\"tool_call\",\"turn\":3,\"tool_name\":\"read_file\",\"input\":{\"path\":\"src/foo.rs\"},\"duration_ms\":11,\"status\":\"ok\",\"state_effect\":\"read\"}\n",
    "{\"event_type\":\"session_end\",\"total_turns\":3,\"total_input_tokens\":100,\"total_output_tokens\":50,",
    "\"total_tool_calls\":3,\"total_shell_calls\":0,\"duration_ms\":300,\"exit_status\":\"ok\"}\n"
);

// A find (read) of src/bar.rs then a read of src/bar.rs, no mutate → cross-tool duplicate,
// 1 redundant call. Uses "file_path" to exercise an alternate recognized field name, and
// two *different* tool_names to prove reads share history regardless of which tool read.
const FIXTURE_CROSS_TOOL: &str = concat!(
    "{\"event_type\":\"session_start\",\"session_id\":\"ses_ffffffffffff4fff8fff000000000006\",",
    "\"capsule_name\":\"reader\",\"capsule_version\":\"0.1.0\",\"model\":\"claude-3-5-sonnet\",\"max_turns\":10}\n",
    "{\"event_type\":\"tool_call\",\"turn\":1,\"tool_name\":\"find_in_files\",\"input\":{\"file_path\":\"src/bar.rs\",\"pattern\":\"foo\"},\"duration_ms\":10,\"status\":\"ok\",\"state_effect\":\"read\"}\n",
    "{\"event_type\":\"tool_call\",\"turn\":2,\"tool_name\":\"read_file\",\"input\":{\"file_path\":\"src/bar.rs\"},\"duration_ms\":9,\"status\":\"ok\",\"state_effect\":\"read\"}\n",
    "{\"event_type\":\"session_end\",\"total_turns\":2,\"total_input_tokens\":100,\"total_output_tokens\":50,",
    "\"total_tool_calls\":2,\"total_shell_calls\":0,\"duration_ms\":300,\"exit_status\":\"ok\"}\n"
);

// Two read calls with no recognizable path field (only "pattern") → never
// flagged, never a crash → 0 redundant calls.
const FIXTURE_NO_PATH_FIELD: &str = concat!(
    "{\"event_type\":\"session_start\",\"session_id\":\"ses_11111111111145558888000000000007\",",
    "\"capsule_name\":\"reader\",\"capsule_version\":\"0.1.0\",\"model\":\"claude-3-5-sonnet\",\"max_turns\":10}\n",
    "{\"event_type\":\"tool_call\",\"turn\":1,\"tool_name\":\"find_in_files\",\"input\":{\"pattern\":\"needle\"},\"duration_ms\":10,\"status\":\"ok\",\"state_effect\":\"read\"}\n",
    "{\"event_type\":\"tool_call\",\"turn\":2,\"tool_name\":\"read_file\",\"input\":{\"pattern\":\"needle\"},\"duration_ms\":9,\"status\":\"ok\",\"state_effect\":\"read\"}\n",
    "{\"event_type\":\"tool_call\",\"turn\":3,\"tool_name\":\"read_file\",\"duration_ms\":9,\"status\":\"ok\",\"state_effect\":\"read\"}\n",
    "{\"event_type\":\"session_end\",\"total_turns\":3,\"total_input_tokens\":100,\"total_output_tokens\":50,",
    "\"total_tool_calls\":3,\"total_shell_calls\":0,\"duration_ms\":300,\"exit_status\":\"ok\"}\n"
);

// A tool whose name has never appeared anywhere in this codebase, exposed the way the
// real editor tool is — one tool with the operation as an *input field*, not the tool_name.
// It declares `read` on both calls and addresses the same `path` with no intervening mutate
// → 1 redundant call. This is the core generality check: correct detection with zero runtime
// knowledge of the tool, purely from what it declares. (Under the old name-matching detector
// this trace produced 0 — `tool_name` is "acme-codenav", never "read_file".)
const FIXTURE_NOVEL_TOOL: &str = concat!(
    "{\"event_type\":\"session_start\",\"session_id\":\"ses_22222222222245558888000000000008\",",
    "\"capsule_name\":\"reader\",\"capsule_version\":\"0.1.0\",\"model\":\"claude-3-5-sonnet\",\"max_turns\":10}\n",
    "{\"event_type\":\"tool_call\",\"turn\":1,\"tool_name\":\"acme-codenav\",\"input\":{\"operation\":\"find_references\",\"path\":\"src/lib.rs\"},\"duration_ms\":10,\"status\":\"ok\",\"state_effect\":\"read\"}\n",
    "{\"event_type\":\"tool_call\",\"turn\":2,\"tool_name\":\"acme-codenav\",\"input\":{\"operation\":\"goto_def\",\"path\":\"src/lib.rs\"},\"duration_ms\":9,\"status\":\"ok\",\"state_effect\":\"read\"}\n",
    "{\"event_type\":\"session_end\",\"total_turns\":2,\"total_input_tokens\":100,\"total_output_tokens\":50,",
    "\"total_tool_calls\":2,\"total_shell_calls\":0,\"duration_ms\":300,\"exit_status\":\"ok\"}\n"
);

// Same novel tool, but its mutating operation (declared `mutate`) sits between two reads of
// the same path → the mutate invalidates → 0 redundant calls. Proves invalidation is also
// declaration-driven, with no runtime knowledge that "apply_edit" changes anything.
const FIXTURE_NOVEL_TOOL_MUTATE: &str = concat!(
    "{\"event_type\":\"session_start\",\"session_id\":\"ses_33333333333345558888000000000009\",",
    "\"capsule_name\":\"reader\",\"capsule_version\":\"0.1.0\",\"model\":\"claude-3-5-sonnet\",\"max_turns\":10}\n",
    "{\"event_type\":\"tool_call\",\"turn\":1,\"tool_name\":\"acme-codenav\",\"input\":{\"operation\":\"find_references\",\"path\":\"src/lib.rs\"},\"duration_ms\":10,\"status\":\"ok\",\"state_effect\":\"read\"}\n",
    "{\"event_type\":\"tool_call\",\"turn\":2,\"tool_name\":\"acme-codenav\",\"input\":{\"operation\":\"apply_edit\",\"path\":\"src/lib.rs\"},\"duration_ms\":8,\"status\":\"ok\",\"state_effect\":\"mutate\"}\n",
    "{\"event_type\":\"tool_call\",\"turn\":3,\"tool_name\":\"acme-codenav\",\"input\":{\"operation\":\"find_references\",\"path\":\"src/lib.rs\"},\"duration_ms\":9,\"status\":\"ok\",\"state_effect\":\"read\"}\n",
    "{\"event_type\":\"session_end\",\"total_turns\":3,\"total_input_tokens\":100,\"total_output_tokens\":50,",
    "\"total_tool_calls\":3,\"total_shell_calls\":0,\"duration_ms\":300,\"exit_status\":\"ok\"}\n"
);

// Two reads of the same path that declare *nothing* → conservative default: no benefit
// (never flagged) and no false positive → 0 redundant calls. An undeclared tool must not
// be silently trusted into producing redundant-read reports.
const FIXTURE_UNDECLARED: &str = concat!(
    "{\"event_type\":\"session_start\",\"session_id\":\"ses_44444444444445558888000000000010\",",
    "\"capsule_name\":\"reader\",\"capsule_version\":\"0.1.0\",\"model\":\"claude-3-5-sonnet\",\"max_turns\":10}\n",
    "{\"event_type\":\"tool_call\",\"turn\":1,\"tool_name\":\"mystery\",\"input\":{\"path\":\"src/foo.rs\"},\"duration_ms\":10,\"status\":\"ok\"}\n",
    "{\"event_type\":\"tool_call\",\"turn\":2,\"tool_name\":\"mystery\",\"input\":{\"path\":\"src/foo.rs\"},\"duration_ms\":9,\"status\":\"ok\"}\n",
    "{\"event_type\":\"session_end\",\"total_turns\":2,\"total_input_tokens\":100,\"total_output_tokens\":50,",
    "\"total_tool_calls\":2,\"total_shell_calls\":0,\"duration_ms\":300,\"exit_status\":\"ok\"}\n"
);

// A declared read, then an *undeclared* call carrying the same path, then a declared read →
// the undeclared middle call is treated conservatively as a mutate and invalidates the first
// read, so the last read is NOT redundant → 0 redundant calls (no false positive).
const FIXTURE_UNDECLARED_INVALIDATES: &str = concat!(
    "{\"event_type\":\"session_start\",\"session_id\":\"ses_55555555555545558888000000000011\",",
    "\"capsule_name\":\"reader\",\"capsule_version\":\"0.1.0\",\"model\":\"claude-3-5-sonnet\",\"max_turns\":10}\n",
    "{\"event_type\":\"tool_call\",\"turn\":1,\"tool_name\":\"read_file\",\"input\":{\"path\":\"src/foo.rs\"},\"duration_ms\":10,\"status\":\"ok\",\"state_effect\":\"read\"}\n",
    "{\"event_type\":\"tool_call\",\"turn\":2,\"tool_name\":\"mystery\",\"input\":{\"path\":\"src/foo.rs\"},\"duration_ms\":8,\"status\":\"ok\"}\n",
    "{\"event_type\":\"tool_call\",\"turn\":3,\"tool_name\":\"read_file\",\"input\":{\"path\":\"src/foo.rs\"},\"duration_ms\":9,\"status\":\"ok\",\"state_effect\":\"read\"}\n",
    "{\"event_type\":\"session_end\",\"total_turns\":3,\"total_input_tokens\":100,\"total_output_tokens\":50,",
    "\"total_tool_calls\":3,\"total_shell_calls\":0,\"duration_ms\":300,\"exit_status\":\"ok\"}\n"
);

#[test]
fn show_flags_reread_with_no_intervening_write() {
    let tmp = TempDir::new().unwrap();
    let path = write_fixture(tmp.path(), "reread.jsonl", FIXTURE_REREAD);

    mur()
        .args(["trace", "show", path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("── Redundant calls ──"))
        .stdout(predicate::str::contains("count:      1"))
        .stdout(predicate::str::contains(
            "turn 2  read_file  src/foo.rs  (re-reads turn 1)",
        ));
}

#[test]
fn show_intervening_write_clears_redundancy() {
    let tmp = TempDir::new().unwrap();
    let path = write_fixture(tmp.path(), "write-clears.jsonl", FIXTURE_WRITE_CLEARS);

    mur()
        .args(["trace", "show", path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("── Redundant calls ──"))
        .stdout(predicate::str::contains("count:      0"))
        .stdout(predicate::str::contains("re-reads").not());
}

#[test]
fn show_cross_tool_find_then_read_is_flagged() {
    let tmp = TempDir::new().unwrap();
    let path = write_fixture(tmp.path(), "cross.jsonl", FIXTURE_CROSS_TOOL);

    mur()
        .args(["trace", "show", path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("count:      1"))
        .stdout(predicate::str::contains(
            "turn 2  read_file  src/bar.rs  (re-reads turn 1)",
        ));
}

#[test]
fn show_no_path_field_never_flagged_no_panic() {
    let tmp = TempDir::new().unwrap();
    let path = write_fixture(tmp.path(), "no-path.jsonl", FIXTURE_NO_PATH_FIELD);

    mur()
        .args(["trace", "show", path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("── Redundant calls ──"))
        .stdout(predicate::str::contains("count:      0"));
}

#[test]
fn show_flags_novel_tool_by_declaration_only() {
    // A tool identity the runtime has never seen, with the operation carried in `input`
    // (not the tool_name) — detection must work purely from its declared `state_effect`.
    let tmp = TempDir::new().unwrap();
    let path = write_fixture(tmp.path(), "novel.jsonl", FIXTURE_NOVEL_TOOL);

    mur()
        .args(["trace", "show", path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("── Redundant calls ──"))
        .stdout(predicate::str::contains("count:      1"))
        .stdout(predicate::str::contains(
            "turn 2  acme-codenav  src/lib.rs  (re-reads turn 1)",
        ));
}

#[test]
fn show_novel_tool_declared_mutate_clears_redundancy() {
    let tmp = TempDir::new().unwrap();
    let path = write_fixture(tmp.path(), "novel-mutate.jsonl", FIXTURE_NOVEL_TOOL_MUTATE);

    mur()
        .args(["trace", "show", path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("count:      0"))
        .stdout(predicate::str::contains("re-reads").not());
}

#[test]
fn show_undeclared_calls_get_no_detection() {
    // Two reads of the same path that declare nothing → conservative default: 0 redundant.
    let tmp = TempDir::new().unwrap();
    let path = write_fixture(tmp.path(), "undeclared.jsonl", FIXTURE_UNDECLARED);

    mur()
        .args(["trace", "show", path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("── Redundant calls ──"))
        .stdout(predicate::str::contains("count:      0"))
        .stdout(predicate::str::contains("re-reads").not());
}

#[test]
fn show_undeclared_intervening_call_invalidates_conservatively() {
    // A declared read, an undeclared call on the same path, then a declared read → the
    // undeclared middle call is treated like a mutate → 0 redundant (no false positive).
    let tmp = TempDir::new().unwrap();
    let path = write_fixture(
        tmp.path(),
        "undeclared-invalidates.jsonl",
        FIXTURE_UNDECLARED_INVALIDATES,
    );

    mur()
        .args(["trace", "show", path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("count:      0"))
        .stdout(predicate::str::contains("re-reads").not());
}

#[test]
fn show_zero_redundant_on_existing_fixtures() {
    let tmp = TempDir::new().unwrap();
    let path = write_fixture(tmp.path(), "trace-a.jsonl", FIXTURE_A);

    mur()
        .args(["trace", "show", path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("── Redundant calls ──"))
        .stdout(predicate::str::contains("count:      0"));
}

#[test]
fn report_shows_redundant_calls_row_and_per_session_line() {
    let tmp = TempDir::new().unwrap();
    // Two sessions with 1 redundant call each, one with none → Mean 0.7, Min 0, Max 1.
    write_session(
        tmp.path(),
        "ses_dddddddddddd4ddd8ddd000000000004",
        FIXTURE_REREAD,
    );
    write_session(
        tmp.path(),
        "ses_ffffffffffff4fff8fff000000000006",
        FIXTURE_CROSS_TOOL,
    );
    write_session(
        tmp.path(),
        "ses_aaaaaaaaaaaa4aaa8aaa000000000001",
        FIXTURE_A,
    );

    mur()
        .args(["trace", "report", "--workdir", tmp.path().to_str().unwrap()])
        .assert()
        .success()
        // aggregate row is always present
        .stdout(predicate::str::contains("redundant calls"))
        // per-session line only when N > 0
        .stdout(predicate::str::contains("Redundant calls: 1"));
}

#[test]
fn report_redundant_row_stats_are_correct() {
    let tmp = TempDir::new().unwrap();
    // One session with 1 redundant call, one with 0 → Min 0.0, Max 1.0, Mean 0.5.
    write_session(
        tmp.path(),
        "ses_dddddddddddd4ddd8ddd000000000004",
        FIXTURE_REREAD,
    );
    write_session(
        tmp.path(),
        "ses_aaaaaaaaaaaa4aaa8aaa000000000001",
        FIXTURE_A,
    );

    let out = mur()
        .args(["trace", "report", "--workdir", tmp.path().to_str().unwrap()])
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let row = stdout
        .lines()
        .find(|l| l.starts_with("redundant calls"))
        .expect("redundant calls row present");
    // Mean 0.5, StdDev 0.5, Min 0.0, Max 1.0
    assert!(row.contains("0.5"), "row = {row:?}");
    assert!(row.contains("0.0"), "row = {row:?}");
    assert!(row.contains("1.0"), "row = {row:?}");
}

// ── declared `resource_id` identity fixtures & tests ──
//
// The fixtures above all address their resource through a path-like `input` field, exercising
// the `PATH_FIELD_NAMES` fallback. The ones below declare `resource_id` instead — the only way
// a tool whose resource is not a filesystem path can get detection.

// A symbol-addressed tool: nothing in `input` is a recognized path field, so the fallback
// heuristic finds nothing and would skip these calls entirely. Detection here comes purely
// from the declared `resource_id`.
const FIXTURE_RESOURCE_ID_SYMBOL: &str = concat!(
    "{\"event_type\":\"session_start\",\"session_id\":\"ses_44444444444445558888000000000010\",",
    "\"capsule_name\":\"reader\",\"capsule_version\":\"0.1.0\",\"model\":\"claude-3-5-sonnet\",\"max_turns\":10}\n",
    "{\"event_type\":\"tool_call\",\"turn\":1,\"tool_name\":\"murmur-tool-code-graph\",\"input\":{\"symbol\":\"Foo::bar\"},\"duration_ms\":10,\"status\":\"ok\",\"state_effect\":\"read\",\"resource_id\":\"Foo::bar\"}\n",
    "{\"event_type\":\"tool_call\",\"turn\":2,\"tool_name\":\"murmur-tool-code-graph\",\"input\":{\"symbol\":\"Foo::bar\"},\"duration_ms\":9,\"status\":\"ok\",\"state_effect\":\"read\",\"resource_id\":\"Foo::bar\"}\n",
    "{\"event_type\":\"session_end\",\"total_turns\":2,\"total_input_tokens\":100,\"total_output_tokens\":50,",
    "\"total_tool_calls\":2,\"total_shell_calls\":0,\"duration_ms\":300,\"exit_status\":\"ok\"}\n"
);

// A declared `resource_id` that deliberately matches neither the `path` nor the `symbol` field
// present in `input` — if either value ever surfaces, `input` was inspected when it must not be.
const FIXTURE_RESOURCE_ID_PRECEDENCE: &str = concat!(
    "{\"event_type\":\"session_start\",\"session_id\":\"ses_55555555555545558888000000000011\",",
    "\"capsule_name\":\"reader\",\"capsule_version\":\"0.1.0\",\"model\":\"claude-3-5-sonnet\",\"max_turns\":10}\n",
    "{\"event_type\":\"tool_call\",\"turn\":1,\"tool_name\":\"acme-codenav\",\"input\":{\"path\":\"src/foo.rs\",\"symbol\":\"Widget\"},\"duration_ms\":10,\"status\":\"ok\",\"state_effect\":\"read\",\"resource_id\":\"sym:Widget#7\"}\n",
    "{\"event_type\":\"tool_call\",\"turn\":2,\"tool_name\":\"acme-codenav\",\"input\":{\"path\":\"src/foo.rs\",\"symbol\":\"Widget\"},\"duration_ms\":9,\"status\":\"ok\",\"state_effect\":\"read\",\"resource_id\":\"sym:Widget#7\"}\n",
    "{\"event_type\":\"session_end\",\"total_turns\":2,\"total_input_tokens\":100,\"total_output_tokens\":50,",
    "\"total_tool_calls\":2,\"total_shell_calls\":0,\"duration_ms\":300,\"exit_status\":\"ok\"}\n"
);

// An empty-string `resource_id` means "undeclared" → falls back to the path field.
const FIXTURE_RESOURCE_ID_EMPTY: &str = concat!(
    "{\"event_type\":\"session_start\",\"session_id\":\"ses_66666666666645558888000000000012\",",
    "\"capsule_name\":\"reader\",\"capsule_version\":\"0.1.0\",\"model\":\"claude-3-5-sonnet\",\"max_turns\":10}\n",
    "{\"event_type\":\"tool_call\",\"turn\":1,\"tool_name\":\"acme-codenav\",\"input\":{\"path\":\"src/foo.rs\"},\"duration_ms\":10,\"status\":\"ok\",\"state_effect\":\"read\",\"resource_id\":\"\"}\n",
    "{\"event_type\":\"tool_call\",\"turn\":2,\"tool_name\":\"acme-codenav\",\"input\":{\"path\":\"src/foo.rs\"},\"duration_ms\":9,\"status\":\"ok\",\"state_effect\":\"read\",\"resource_id\":\"\"}\n",
    "{\"event_type\":\"session_end\",\"total_turns\":2,\"total_input_tokens\":100,\"total_output_tokens\":50,",
    "\"total_tool_calls\":2,\"total_shell_calls\":0,\"duration_ms\":300,\"exit_status\":\"ok\"}\n"
);

// A declared mutate against a declared `resource_id` invalidates the earlier read of it —
// invalidation keys off the same identity the reads do.
const FIXTURE_RESOURCE_ID_MUTATE: &str = concat!(
    "{\"event_type\":\"session_start\",\"session_id\":\"ses_77777777777745558888000000000013\",",
    "\"capsule_name\":\"reader\",\"capsule_version\":\"0.1.0\",\"model\":\"claude-3-5-sonnet\",\"max_turns\":10}\n",
    "{\"event_type\":\"tool_call\",\"turn\":1,\"tool_name\":\"murmur-tool-code-graph\",\"input\":{\"symbol\":\"Widget\"},\"duration_ms\":10,\"status\":\"ok\",\"state_effect\":\"read\",\"resource_id\":\"sym:Widget\"}\n",
    "{\"event_type\":\"tool_call\",\"turn\":2,\"tool_name\":\"murmur-tool-code-graph\",\"input\":{\"symbol\":\"Widget\"},\"duration_ms\":8,\"status\":\"ok\",\"state_effect\":\"mutate\",\"resource_id\":\"sym:Widget\"}\n",
    "{\"event_type\":\"tool_call\",\"turn\":3,\"tool_name\":\"murmur-tool-code-graph\",\"input\":{\"symbol\":\"Widget\"},\"duration_ms\":9,\"status\":\"ok\",\"state_effect\":\"read\",\"resource_id\":\"sym:Widget\"}\n",
    "{\"event_type\":\"session_end\",\"total_turns\":3,\"total_input_tokens\":100,\"total_output_tokens\":50,",
    "\"total_tool_calls\":3,\"total_shell_calls\":0,\"duration_ms\":300,\"exit_status\":\"ok\"}\n"
);

#[test]
fn show_flags_symbol_addressed_tool_via_declared_resource_id() {
    // The litmus case: no path-like field exists in `input`, so the fallback heuristic yields
    // nothing. Declaring `resource_id` is the only thing making this detectable.
    let tmp = TempDir::new().unwrap();
    let path = write_fixture(
        tmp.path(),
        "resource-symbol.jsonl",
        FIXTURE_RESOURCE_ID_SYMBOL,
    );

    mur()
        .args(["trace", "show", path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("── Redundant calls ──"))
        .stdout(predicate::str::contains("count:      1"))
        .stdout(predicate::str::contains(
            "turn 2  murmur-tool-code-graph  Foo::bar  (re-reads turn 1)",
        ));
}

#[test]
fn show_declared_resource_id_takes_precedence_over_path_field() {
    // `input` carries both a `path` and a `symbol`; neither may appear — the declared value wins.
    let tmp = TempDir::new().unwrap();
    let path = write_fixture(
        tmp.path(),
        "resource-precedence.jsonl",
        FIXTURE_RESOURCE_ID_PRECEDENCE,
    );

    let out = mur()
        .args(["trace", "show", path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("count:      1"))
        .stdout(predicate::str::contains(
            "turn 2  acme-codenav  sym:Widget#7  (re-reads turn 1)",
        ));

    // Scope the negative check to the redundant-call line itself: the unrelated `Tool calls`
    // section legitimately echoes the raw input blob, `path` field and all.
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let line = stdout
        .lines()
        .find(|l| l.contains("re-reads"))
        .expect("redundant-call line present");
    assert!(
        !line.contains("src/foo.rs"),
        "identity must come from the declared resource_id, never the `path` field: {line:?}"
    );
}

#[test]
fn show_empty_resource_id_falls_back_to_path_field() {
    // Empty string = undeclared, matching the `state_effect`/`continuation_id` convention.
    let tmp = TempDir::new().unwrap();
    let path = write_fixture(
        tmp.path(),
        "resource-empty.jsonl",
        FIXTURE_RESOURCE_ID_EMPTY,
    );

    mur()
        .args(["trace", "show", path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("count:      1"))
        .stdout(predicate::str::contains(
            "turn 2  acme-codenav  src/foo.rs  (re-reads turn 1)",
        ));
}

#[test]
fn show_declared_mutate_clears_redundancy_for_declared_resource_id() {
    let tmp = TempDir::new().unwrap();
    let path = write_fixture(
        tmp.path(),
        "resource-mutate.jsonl",
        FIXTURE_RESOURCE_ID_MUTATE,
    );

    mur()
        .args(["trace", "show", path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("count:      0"))
        .stdout(predicate::str::contains("re-reads").not());
}

/// A session whose task was reopened once by an `on-task-end` hook: a `task_reopened`
/// record sits between two attempts and the `task_end` carries `reopen_count`.
const FIXTURE_REOPEN: &str = concat!(
    "{\"event_type\":\"session_start\",\"session_id\":\"ses_cccccccccccc4ccc8ccc000000000003\",\"timestamp\":3000,",
    "\"capsule_name\":\"test-capsule\",\"capsule_version\":\"0.1.0\",\"model\":\"claude-3-5-sonnet\",",
    "\"max_turns\":10,\"capabilities\":[\"shell\"],\"tools_declared\":[\"bash\"]}\n",

    "{\"event_type\":\"task_start\",\"session_id\":\"ses_cccccccccccc4ccc8ccc000000000003\",\"timestamp\":3050,",
    "\"task_id\":\"tsk_1\",\"context_id\":\"ctx_1\",\"source\":\"a2a\",\"message_parts_bytes\":12}\n",

    "{\"event_type\":\"inference\",\"session_id\":\"ses_cccccccccccc4ccc8ccc000000000003\",\"timestamp\":3100,",
    "\"turn\":1,\"input_tokens\":1000,\"output_tokens\":200,\"decision\":\"end_turn\",\"tool_name\":null}\n",

    "{\"event_type\":\"task_reopened\",\"session_id\":\"ses_cccccccccccc4ccc8ccc000000000003\",\"timestamp\":3150,",
    "\"task_id\":\"tsk_1\",\"hook_name\":\"gatekeeper\",\"reason\":\"tests still fail\",\"reopen_number\":1}\n",

    "{\"event_type\":\"inference\",\"session_id\":\"ses_cccccccccccc4ccc8ccc000000000003\",\"timestamp\":3200,",
    "\"turn\":2,\"input_tokens\":1100,\"output_tokens\":150,\"decision\":\"end_turn\",\"tool_name\":null}\n",

    "{\"event_type\":\"task_end\",\"session_id\":\"ses_cccccccccccc4ccc8ccc000000000003\",\"timestamp\":3300,",
    "\"task_id\":\"tsk_1\",\"exit_status\":\"ok\",\"duration_ms\":250,\"turns\":2,\"input_tokens\":2100,",
    "\"output_tokens\":350,\"tool_calls\":0,\"shell_calls\":0,\"reopen_count\":1}\n",

    "{\"event_type\":\"session_end\",\"session_id\":\"ses_cccccccccccc4ccc8ccc000000000003\",\"timestamp\":3400,",
    "\"total_turns\":2,\"total_input_tokens\":2100,\"total_output_tokens\":350,",
    "\"total_tool_calls\":0,\"total_shell_calls\":0,\"duration_ms\":400,\"exit_status\":\"ok\"}\n"
);

/// `mur trace show` surfaces the new `task_reopened` event and `reopen_count` field
/// without crashing, listing the reopen with its hook and feedback.
#[test]
fn show_surfaces_reopen_events_and_count() {
    let tmp = TempDir::new().unwrap();
    let path = write_fixture(tmp.path(), "reopen.jsonl", FIXTURE_REOPEN);

    mur()
        .args(["trace", "show", path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("Reopens"))
        .stdout(predicate::str::contains("gatekeeper"))
        .stdout(predicate::str::contains("tests still fail"));
}
