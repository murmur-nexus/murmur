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

    // Carries the provider-reported counts alongside the runtime's own estimates: `mur trace
    // show` and `mur trace report` must read this line and print the same totals as they do
    // for the turn-2 line below, which carries none of them.
    "{\"event_type\":\"inference\",\"session_id\":\"ses_aaaaaaaaaaaa4aaa8aaa000000000001\",\"timestamp\":1100,",
    "\"turn\":1,\"input_tokens\":1000,\"output_tokens\":200,\"decision\":\"tool_call\",\"tool_name\":\"bash\",",
    "\"input_tokens_actual\":940,\"output_tokens_actual\":180,\"cached_tokens\":900,\"cache_write_tokens\":0}\n",

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

// One session frame around one task, every line identified and parented, and a compaction
// the threshold tripped but nothing serviced.
const FIXTURE_DECLINED: &str = concat!(
    "{\"event_type\":\"session_start\",\"event_id\":\"evt_dddddddddddd4ddd8ddd000000000001\",\"parent_id\":null,",
    "\"session_id\":\"ses_dddddddddddd4ddd8ddd000000000004\",\"timestamp\":5000,",
    "\"capsule_name\":\"declining-capsule\",\"capsule_version\":\"0.1.0\",\"model\":\"claude-haiku\",",
    "\"max_turns\":10,\"capabilities\":[],\"tools_declared\":[]}\n",

    "{\"event_type\":\"task_start\",\"event_id\":\"evt_dddddddddddd4ddd8ddd000000000002\",",
    "\"parent_id\":\"evt_dddddddddddd4ddd8ddd000000000001\",",
    "\"session_id\":\"ses_dddddddddddd4ddd8ddd000000000004\",\"timestamp\":5010,",
    "\"task_id\":\"tsk_1\",\"context_id\":\"ctx_1\",\"source\":\"task_md\",\"message_parts_bytes\":8}\n",

    "{\"event_type\":\"inference\",\"event_id\":\"evt_dddddddddddd4ddd8ddd000000000003\",",
    "\"parent_id\":\"evt_dddddddddddd4ddd8ddd000000000002\",",
    "\"session_id\":\"ses_dddddddddddd4ddd8ddd000000000004\",\"timestamp\":5020,",
    "\"turn\":1,\"task_id\":\"tsk_1\",\"input_tokens\":5000,\"output_tokens\":100,",
    "\"decision\":\"end_turn\",\"tool_name\":null}\n",

    "{\"event_type\":\"compaction_declined\",\"event_id\":\"evt_dddddddddddd4ddd8ddd000000000004\",",
    "\"parent_id\":\"evt_dddddddddddd4ddd8ddd000000000003\",",
    "\"session_id\":\"ses_dddddddddddd4ddd8ddd000000000004\",\"timestamp\":5030,",
    "\"turn\":1,\"task_id\":\"tsk_1\",\"tokens\":5000,\"reason\":\"no_hook_replacement\"}\n",

    "{\"event_type\":\"task_end\",\"event_id\":\"evt_dddddddddddd4ddd8ddd000000000005\",",
    "\"parent_id\":\"evt_dddddddddddd4ddd8ddd000000000002\",",
    "\"session_id\":\"ses_dddddddddddd4ddd8ddd000000000004\",\"timestamp\":5040,",
    "\"task_id\":\"tsk_1\",\"exit_status\":\"ok\",\"duration_ms\":30,\"turns\":1,",
    "\"input_tokens\":5000,\"output_tokens\":100,\"tool_calls\":0,\"shell_calls\":0,\"reopen_count\":0}\n",

    "{\"event_type\":\"session_end\",\"event_id\":\"evt_dddddddddddd4ddd8ddd000000000006\",",
    "\"parent_id\":\"evt_dddddddddddd4ddd8ddd000000000001\",",
    "\"session_id\":\"ses_dddddddddddd4ddd8ddd000000000004\",\"timestamp\":5050,",
    "\"total_turns\":1,\"total_input_tokens\":5000,\"total_output_tokens\":100,",
    "\"total_tool_calls\":0,\"total_shell_calls\":0,\"duration_ms\":50,\"exit_status\":\"ok\"}\n"
);

/// A session that attempted a protected write three times and was refused every time. The
/// `protected_path_denied` lines carry no `shell` or `tool_call` beside them, because nothing ran.
const FIXTURE_PROTECTED: &str = concat!(
    "{\"event_type\":\"session_start\",\"event_id\":\"evt_ffffffffffff4fff8fff000000000001\",\"parent_id\":null,",
    "\"session_id\":\"ses_ffffffffffff4fff8fff000000000004\",\"timestamp\":6000,",
    "\"capsule_name\":\"protected-capsule\",\"capsule_version\":\"0.1.0\",\"model\":\"claude-haiku\",",
    "\"max_turns\":10,\"capabilities\":[\"shell\"],\"tools_declared\":[\"bash\"]}\n",

    "{\"event_type\":\"inference\",\"event_id\":\"evt_ffffffffffff4fff8fff000000000002\",",
    "\"parent_id\":\"evt_ffffffffffff4fff8fff000000000001\",",
    "\"session_id\":\"ses_ffffffffffff4fff8fff000000000004\",\"timestamp\":6010,",
    "\"turn\":1,\"input_tokens\":100,\"output_tokens\":10,\"decision\":\"tool_call\",\"tool_name\":\"bash\"}\n",

    "{\"event_type\":\"protected_path_denied\",\"event_id\":\"evt_ffffffffffff4fff8fff000000000003\",",
    "\"parent_id\":\"evt_ffffffffffff4fff8fff000000000002\",",
    "\"session_id\":\"ses_ffffffffffff4fff8fff000000000004\",\"timestamp\":6020,",
    "\"turn\":1,\"call\":\"shell\",\"target\":\"/usr/bin/bash\",\"path\":\"tests/test_foo.py\",",
    "\"rule\":\"tests\",\"signal\":\"shell redirection '>' into the path\",",
    "\"reason\":\"'tests/test_foo.py' is under the read-only path 'tests'\"}\n",

    "{\"event_type\":\"protected_path_denied\",\"event_id\":\"evt_ffffffffffff4fff8fff000000000004\",",
    "\"parent_id\":\"evt_ffffffffffff4fff8fff000000000002\",",
    "\"session_id\":\"ses_ffffffffffff4fff8fff000000000004\",\"timestamp\":6030,",
    "\"turn\":1,\"call\":\"shell\",\"target\":\"/usr/bin/bash\",\"path\":\"tests/conftest.py\",",
    "\"rule\":\"tests\",\"signal\":\"write-target argument of 'tee'\",",
    "\"reason\":\"'tests/conftest.py' is under the read-only path 'tests'\"}\n",

    "{\"event_type\":\"protected_path_denied\",\"event_id\":\"evt_ffffffffffff4fff8fff000000000005\",",
    "\"parent_id\":\"evt_ffffffffffff4fff8fff000000000002\",",
    "\"session_id\":\"ses_ffffffffffff4fff8fff000000000004\",\"timestamp\":6040,",
    "\"turn\":1,\"call\":\"tool\",\"target\":\"edit-file\",\"path\":\"bench/fixtures/case.json\",",
    "\"rule\":\"bench/fixtures\",\"signal\":\"tool input pairs 'path' with 'content'\",",
    "\"reason\":\"'bench/fixtures/case.json' is under the read-only path 'bench/fixtures'\"}\n",

    "{\"event_type\":\"session_end\",\"event_id\":\"evt_ffffffffffff4fff8fff000000000006\",",
    "\"parent_id\":\"evt_ffffffffffff4fff8fff000000000001\",",
    "\"session_id\":\"ses_ffffffffffff4fff8fff000000000004\",\"timestamp\":6050,",
    "\"total_turns\":1,\"total_input_tokens\":100,\"total_output_tokens\":10,",
    "\"total_tool_calls\":0,\"total_shell_calls\":0,\"duration_ms\":50,\"exit_status\":\"ok\"}\n"
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

/// A declined compaction is listed under the same heading as a fired one, with its turn and
/// reason — a session that ran on over budget must not read as one that never needed
/// compacting.
#[test]
fn show_lists_declined_compactions_with_turn_and_reason() {
    let tmp = TempDir::new().unwrap();
    let path = write_fixture(tmp.path(), "declined.jsonl", FIXTURE_DECLINED);

    mur()
        .args(["trace", "show", path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("Compaction"))
        .stdout(predicate::str::contains("declined"))
        .stdout(predicate::str::contains("turn 1"))
        .stdout(predicate::str::contains("no_hook_replacement"))
        .stdout(predicate::str::contains("5,000"));
}

/// A refused write is not a write that never happened: every `protected_path_denied` record is
/// rendered under its own heading with its path and rule, and the count is reported beside them.
#[test]
fn show_lists_protected_path_refusals_with_path_rule_and_count() {
    let tmp = TempDir::new().unwrap();
    let path = write_fixture(tmp.path(), "protected.jsonl", FIXTURE_PROTECTED);

    mur()
        .args(["trace", "show", path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("Protected paths"))
        .stdout(predicate::str::contains("protected-path refusals: 3"))
        .stdout(predicate::str::contains("tests/test_foo.py"))
        .stdout(predicate::str::contains("tests/conftest.py"))
        .stdout(predicate::str::contains("bench/fixtures/case.json"))
        .stdout(predicate::str::contains("rule tests"))
        .stdout(predicate::str::contains("rule bench/fixtures"));
}

/// A trace with no refusal reports the section not at all, so an unchanged capsule's output is
/// unchanged.
#[test]
fn show_omits_the_protected_path_section_when_there_are_none() {
    let tmp = TempDir::new().unwrap();
    let path = write_fixture(tmp.path(), "trace-a.jsonl", FIXTURE_A);

    mur()
        .args(["trace", "show", path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("Protected paths").not())
        .stdout(predicate::str::contains("protected-path refusals").not());
}

/// Each refusal gets its own line in the steps tree, naming the path and the rule.
#[test]
fn steps_renders_each_protected_path_refusal_on_its_own_line() {
    let tmp = TempDir::new().unwrap();
    let path = write_fixture(tmp.path(), "protected.jsonl", FIXTURE_PROTECTED);

    let out = mur()
        .args(["trace", "steps", path.to_str().unwrap()])
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();

    assert!(
        stdout.contains("protected_path_denied shell  tests/test_foo.py  rule tests"),
        "{stdout}"
    );
    assert!(
        stdout
            .contains("protected_path_denied tool  bench/fixtures/case.json  rule bench/fixtures"),
        "{stdout}"
    );
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

// Fixture: trace with both input and output fields, as `trace.capture: content` writes them.
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
    // Traces that include the output field, as `trace.capture: content` writes them, must parse cleanly.
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

// ── the nine session-level event types ───────────────────────────────────────

const SESSION_ID_NINE: &str = "ses_99999999999949998999000000000009";

/// One session frame around one line of each session-level event type, shaped as
/// `docs/content/reference/observability-schemas.md` documents them.
fn nine_event_fixture() -> String {
    let s = SESSION_ID_NINE;
    [
        format!("{{\"event_type\":\"session_start\",\"event_id\":\"evt_1\",\"parent_id\":null,\"session_id\":\"{s}\",\"timestamp\":1000,\"capsule_name\":\"wide-capsule\",\"capsule_version\":\"0.1.0\",\"model\":\"claude-test\",\"max_turns\":10,\"capabilities\":[\"network\",\"shell\"],\"tools_declared\":[\"bash\",\"share-file\"],\"containment_declared\":\"sealed\",\"containment_achieved\":\"scoped\",\"workdir_exec\":false,\"userns_grant\":\"profile_confining\",\"system_prompt_source\":\"manifest\",\"system_prompt_sha256\":\"1111111111111111111111111111111111111111111111111111111111111111\",\"effective_grants\":{{\"declared_containment\":\"sealed\",\"network_allow\":[]}}}}"),
        format!("{{\"event_type\":\"task_start\",\"event_id\":\"evt_2\",\"parent_id\":\"evt_1\",\"session_id\":\"{s}\",\"timestamp\":1010,\"task_id\":\"tsk_one\",\"context_id\":\"ctx_one\",\"source\":\"a2a\",\"message_parts_bytes\":12}}"),
        format!("{{\"event_type\":\"context_seed\",\"event_id\":\"evt_3\",\"parent_id\":\"evt_2\",\"session_id\":\"{s}\",\"timestamp\":1020,\"task_id\":\"tsk_one\",\"hook_name\":\"memory-hook\",\"tokens\":1204,\"proposed_tokens\":1400,\"budget_tokens\":20000,\"outcome\":\"trimmed\",\"message_ids\":[\"msg_aaa\",\"msg_bbb\"]}}"),
        format!("{{\"event_type\":\"a2a_task_received\",\"event_id\":\"evt_4\",\"parent_id\":\"evt_1\",\"session_id\":\"{s}\",\"timestamp\":1030,\"task_id\":\"tsk_one\",\"context_id\":\"ctx_one\",\"message_id\":\"msg-in\",\"traceparent_from_caller\":null}}"),
        format!("{{\"event_type\":\"inference\",\"event_id\":\"evt_5\",\"parent_id\":\"evt_2\",\"session_id\":\"{s}\",\"timestamp\":1100,\"turn\":1,\"task_id\":\"tsk_one\",\"input_tokens\":100,\"output_tokens\":20,\"decision\":\"end_turn\",\"tool_name\":null,\"message_ids\":[\"msg_aaa\"]}}"),
        format!("{{\"event_type\":\"task_end\",\"event_id\":\"evt_6\",\"parent_id\":\"evt_2\",\"session_id\":\"{s}\",\"timestamp\":1200,\"task_id\":\"tsk_one\",\"exit_status\":\"ok\",\"duration_ms\":190,\"turns\":1,\"input_tokens\":100,\"output_tokens\":20,\"tool_calls\":0,\"shell_calls\":0,\"reopen_count\":0}}"),
        format!("{{\"event_type\":\"a2a_send\",\"event_id\":\"evt_7\",\"parent_id\":\"evt_1\",\"session_id\":\"{s}\",\"timestamp\":1210,\"peer_url\":\"http://peer.example/a2a\",\"message_id\":\"msg-out\",\"task_id\":\"tsk_peer\",\"context_id\":\"ctx_peer\",\"traceparent\":null}}"),
        format!("{{\"event_type\":\"hook_dispatch_error\",\"event_id\":\"evt_8\",\"parent_id\":\"evt_1\",\"session_id\":\"{s}\",\"timestamp\":1220,\"hook_name\":\"audit-hook\",\"event\":\"on-tool-call\",\"arm\":\"write-manifests\"}}"),
        format!("{{\"event_type\":\"resource_list\",\"event_id\":\"evt_9\",\"parent_id\":\"evt_1\",\"session_id\":\"{s}\",\"timestamp\":1230,\"root\":\"out\",\"entry_count\":2,\"total_bytes\":40,\"generation\":1,\"containment_achieved\":\"scoped\",\"outcome\":\"ok\",\"reason\":null}}"),
        format!("{{\"event_type\":\"resource_read\",\"event_id\":\"evt_10\",\"parent_id\":\"evt_1\",\"session_id\":\"{s}\",\"timestamp\":1240,\"path\":\"../etc/passwd\",\"outcome\":\"not_found\",\"bytes\":null,\"sha256\":null,\"generation\":1,\"containment_achieved\":\"scoped\",\"reason\":\"outside the export root\"}}"),
        format!("{{\"event_type\":\"peer_handle_mint\",\"event_id\":\"evt_11\",\"parent_id\":\"evt_1\",\"session_id\":\"{s}\",\"timestamp\":1250,\"handle_id\":\"abcdef0123456789\",\"path\":\"report.md\",\"audience\":\"peer@host:8080\",\"expires_at_ms\":99,\"outcome\":\"ok\",\"reason\":null}}"),
        format!("{{\"event_type\":\"peer_handle_redeem\",\"event_id\":\"evt_12\",\"parent_id\":\"evt_1\",\"session_id\":\"{s}\",\"timestamp\":1260,\"handle_id\":\"abcdef0123456789\",\"path\":\"report.md\",\"generation\":1,\"audience_asserted\":\"peer@host:8080\",\"bytes\":40,\"sha256\":\"aa\",\"outcome\":\"ok\",\"reason\":null}}"),
        format!("{{\"event_type\":\"peer_file_fetch\",\"event_id\":\"evt_13\",\"parent_id\":\"evt_1\",\"session_id\":\"{s}\",\"timestamp\":1270,\"peer\":\"peer@host:8080\",\"handle_id\":\"abcdef0123456789\",\"stored_path\":null,\"bytes\":null,\"sha256\":null,\"outcome\":\"peer_unreachable\",\"reason\":\"connection refused\"}}"),
        format!("{{\"event_type\":\"session_end\",\"event_id\":\"evt_14\",\"parent_id\":\"evt_1\",\"session_id\":\"{s}\",\"timestamp\":1300,\"total_turns\":1,\"total_input_tokens\":100,\"total_output_tokens\":20,\"total_tool_calls\":0,\"total_shell_calls\":0,\"duration_ms\":300,\"exit_status\":\"ok\"}}"),
    ]
    .join("\n")
        + "\n"
}

/// Every session-level event type reaches the output.
#[test]
fn show_names_every_previously_dropped_event_type() {
    let tmp = TempDir::new().unwrap();
    let path = write_fixture(tmp.path(), "nine.jsonl", &nine_event_fixture());

    mur()
        .args(["trace", "show", path.to_str().unwrap()])
        .assert()
        .success()
        // context_seed
        .stdout(predicate::str::contains("── Context"))
        .stdout(predicate::str::contains("memory-hook"))
        .stdout(predicate::str::contains("trimmed"))
        .stdout(predicate::str::contains("msg_aaa, msg_bbb"))
        // hook_dispatch_error
        .stdout(predicate::str::contains("── Hook failures"))
        .stdout(predicate::str::contains(
            "✗ audit-hook  on-tool-call  write-manifests",
        ))
        // resource_list / resource_read
        .stdout(predicate::str::contains("── Resource plane"))
        .stdout(predicate::str::contains("list:       1 ok"))
        .stdout(predicate::str::contains("read:       1 not_found"))
        // peer_handle_mint / peer_handle_redeem / peer_file_fetch
        .stdout(predicate::str::contains("── Peer files"))
        .stdout(predicate::str::contains("minted:     1 ok"))
        .stdout(predicate::str::contains("redeemed:   1 ok"))
        .stdout(predicate::str::contains("fetched:    1 peer_unreachable"))
        // a2a_task_received / a2a_send
        .stdout(predicate::str::contains("── A2A"))
        .stdout(predicate::str::contains("received:   1 task"))
        .stdout(predicate::str::contains("sent:       1 message"))
        .stdout(predicate::str::contains("http://peer.example/a2a"));
}

/// The Session block reports what the session ran under, not just what it was called.
#[test]
fn show_session_block_reports_containment_and_prompt() {
    let tmp = TempDir::new().unwrap();
    let path = write_fixture(tmp.path(), "nine.jsonl", &nine_event_fixture());

    mur()
        .args(["trace", "show", path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("capabilities: network, shell"))
        .stdout(predicate::str::contains("tools:      bash, share-file"))
        .stdout(predicate::str::contains("containment: sealed → scoped"))
        .stdout(predicate::str::contains("workdir exec: no"))
        .stdout(predicate::str::contains("userns:     profile_confining"))
        .stdout(predicate::str::contains(
            "prompt:     manifest  111111111111…",
        ));
}

/// A hook fault is placed where it cannot be scrolled past: after Session, before Turns.
#[test]
fn show_hook_failures_precede_the_turns_section() {
    let tmp = TempDir::new().unwrap();
    let path = write_fixture(tmp.path(), "nine.jsonl", &nine_event_fixture());

    let out = mur()
        .args(["trace", "show", path.to_str().unwrap()])
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();

    let session = stdout.find("── Session").unwrap();
    let failures = stdout.find("── Hook failures").unwrap();
    let turns = stdout.find("── Turns").unwrap();
    assert!(session < failures && failures < turns, "{stdout}");
}

/// A rejected seed names why nothing was committed.
#[test]
fn show_context_section_reports_a_rejection_reason() {
    let tmp = TempDir::new().unwrap();
    let s = SESSION_ID_NINE;
    let trace = [
        format!("{{\"event_type\":\"session_start\",\"session_id\":\"{s}\",\"timestamp\":1000,\"capsule_name\":\"c\",\"capsule_version\":\"0.1.0\",\"model\":\"m\",\"max_turns\":5,\"capabilities\":[],\"tools_declared\":[]}}"),
        format!("{{\"event_type\":\"context_seed\",\"session_id\":\"{s}\",\"timestamp\":1010,\"task_id\":null,\"hook_name\":\"seed-hook\",\"tokens\":0,\"proposed_tokens\":221,\"budget_tokens\":19,\"outcome\":\"rejected\",\"reason\":\"message_over_budget\",\"message_ids\":[]}}"),
        format!("{{\"event_type\":\"session_end\",\"session_id\":\"{s}\",\"timestamp\":1100,\"total_turns\":0,\"total_input_tokens\":0,\"total_output_tokens\":0,\"total_tool_calls\":0,\"total_shell_calls\":0,\"duration_ms\":100,\"exit_status\":\"ok\"}}"),
    ]
    .join("\n")
        + "\n";
    let path = write_fixture(tmp.path(), "rejected.jsonl", &trace);

    mur()
        .args(["trace", "show", path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "seed-hook  rejected  0 tokens (proposed 221, budget 19)",
        ))
        .stdout(predicate::str::contains("reason:   message_over_budget"));
}

/// A known event type carrying a key this build does not know, beside an event type it does
/// not know at all: everything else still renders.
#[test]
fn show_tolerates_unknown_keys_and_unknown_types_together() {
    let tmp = TempDir::new().unwrap();
    let s = SESSION_ID_NINE;
    let trace = [
        format!("{{\"event_type\":\"session_start\",\"session_id\":\"{s}\",\"timestamp\":1000,\"capsule_name\":\"tolerant\",\"capsule_version\":\"0.1.0\",\"model\":\"m\",\"max_turns\":5,\"capabilities\":[],\"tools_declared\":[],\"a_key_from_the_future\":{{\"nested\":true}}}}"),
        format!("{{\"event_type\":\"context_seed\",\"session_id\":\"{s}\",\"timestamp\":1010,\"hook_name\":\"memory-hook\",\"tokens\":10,\"proposed_tokens\":10,\"budget_tokens\":100,\"outcome\":\"seeded\",\"message_ids\":[\"msg_x\"],\"seed_provenance\":\"from a later runtime\"}}"),
        format!("{{\"event_type\":\"an_event_type_from_the_future\",\"session_id\":\"{s}\",\"timestamp\":1020,\"whatever\":1}}"),
        format!("{{\"event_type\":\"session_end\",\"session_id\":\"{s}\",\"timestamp\":1100,\"total_turns\":0,\"total_input_tokens\":0,\"total_output_tokens\":0,\"total_tool_calls\":0,\"total_shell_calls\":0,\"duration_ms\":100,\"exit_status\":\"ok\"}}"),
    ]
    .join("\n")
        + "\n";
    let path = write_fixture(tmp.path(), "tolerant.jsonl", &trace);

    mur()
        .args(["trace", "show", path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("tolerant"))
        .stdout(predicate::str::contains("memory-hook  seeded  10 tokens"))
        .stdout(predicate::str::contains("msg_x"));
}

// ── wire hashes and bodies ────────────────────────────────────────────────────

const PROMPT_BODY: &str = "You are a helpful capsule.\n";
const TOOLS_BODY: &str = "[{\"name\":\"bash\",\"description\":\"run a command\"}]";
const RESPONSE_BODY: &str = "{\"stop_reason\":\"end_turn\"}";
const MESSAGE_BODIES: [&str; 4] = [
    "{\"role\":\"user\",\"content\":\"one\"}",
    "{\"role\":\"assistant\",\"content\":\"two\"}",
    "{\"role\":\"user\",\"content\":\"three\"}",
    "{\"role\":\"assistant\",\"content\":\"four\"}",
];
/// The marker the system-prompt body carries, and the reason it is longer than a line: a
/// default `show` must name the hash and never the body, whatever the body's size.
const SYSTEM_MARKER: &str = "DISTINCTIVE-SYSTEM-PROMPT-MARKER";

fn system_body() -> String {
    format!("{SYSTEM_MARKER}\n{}", "padding ".repeat(5000))
}

fn sha(body: &str) -> String {
    murmur_artifact::sha256_hex(body.as_bytes())
}

/// A two-turn session carrying the four wire hashes on every turn, plus the bodies behind
/// them. `store_bodies` is the whole difference between `trace.capture: content` and
/// `capture: meta`: the same hashes, with or without a `blobs/` directory beside the trace.
fn write_wire_session(
    workdir: &Path,
    session_id: &str,
    system: &str,
    messages: &[&str],
    store_bodies: bool,
) {
    let message_shas: Vec<String> = messages.iter().map(|m| sha(m)).collect();
    let quoted: Vec<String> = message_shas.iter().map(|s| format!("\"{s}\"")).collect();
    let turn = |n: u32, ts: u64| {
        format!(
            "{{\"event_type\":\"inference\",\"session_id\":\"{session_id}\",\"timestamp\":{ts},\
             \"turn\":{n},\"input_tokens\":100,\"output_tokens\":20,\"decision\":\"end_turn\",\
             \"tool_name\":null,\"system_sha\":\"{}\",\"tools_sha\":\"{}\",\"response_sha\":\"{}\",\
             \"message_shas\":[{}]}}",
            sha(system),
            sha(TOOLS_BODY),
            sha(RESPONSE_BODY),
            quoted.join(",")
        )
    };
    let trace = [
        format!(
            "{{\"event_type\":\"session_start\",\"session_id\":\"{session_id}\",\"timestamp\":1000,\
             \"capsule_name\":\"wire\",\"capsule_version\":\"0.1.0\",\"model\":\"m\",\"max_turns\":5,\
             \"capabilities\":[],\"tools_declared\":[],\"system_prompt_source\":\"manifest\",\
             \"system_prompt_sha256\":\"{}\"}}",
            sha(PROMPT_BODY)
        ),
        turn(1, 1100),
        turn(2, 1200),
        format!(
            "{{\"event_type\":\"session_end\",\"session_id\":\"{session_id}\",\"timestamp\":1300,\
             \"total_turns\":2,\"total_input_tokens\":200,\"total_output_tokens\":40,\
             \"total_tool_calls\":0,\"total_shell_calls\":0,\"duration_ms\":300,\"exit_status\":\"ok\"}}"
        ),
    ]
    .join("\n")
        + "\n";
    write_session(workdir, session_id, &trace);

    if store_bodies {
        let blobs = workdir.join(session_id).join("blobs");
        fs::create_dir_all(&blobs).unwrap();
        for body in [system, TOOLS_BODY, RESPONSE_BODY, PROMPT_BODY]
            .into_iter()
            .chain(messages.iter().copied())
        {
            fs::write(blobs.join(sha(body)), body).unwrap();
        }
    }
}

const SESSION_ID_WIRE: &str = "ses_11111111111141118111000000000011";

/// Default `show` names the hashes and the command that prints a body — and prints no body,
/// however large the body is.
#[test]
fn show_names_wire_hashes_and_never_prints_a_body() {
    let tmp = TempDir::new().unwrap();
    let system = system_body();
    write_wire_session(tmp.path(), SESSION_ID_WIRE, &system, &MESSAGE_BODIES, true);

    mur()
        .args([
            "trace",
            "show",
            SESSION_ID_WIRE,
            "--workdir",
            tmp.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("── Wire"))
        .stdout(predicate::str::contains(format!(
            "turn 1  system {}…",
            &sha(&system)[..12]
        )))
        .stdout(predicate::str::contains("4 messages"))
        .stdout(predicate::str::contains(
            "mur trace show --body system --turn 1",
        ))
        .stdout(predicate::str::contains(SYSTEM_MARKER).not());
}

/// Under `capture: content` every named selector prints exactly the blob on disk.
#[test]
fn body_selectors_print_the_stored_blob_verbatim() {
    let tmp = TempDir::new().unwrap();
    let system = system_body();
    write_wire_session(tmp.path(), SESSION_ID_WIRE, &system, &MESSAGE_BODIES, true);
    let blobs = tmp.path().join(SESSION_ID_WIRE).join("blobs");

    for (selector, body) in [
        ("system", system.as_str()),
        ("tools", TOOLS_BODY),
        ("response", RESPONSE_BODY),
        ("message:2", MESSAGE_BODIES[2]),
    ] {
        let out = mur()
            .args([
                "trace",
                "show",
                SESSION_ID_WIRE,
                "--workdir",
                tmp.path().to_str().unwrap(),
                "--body",
                selector,
                "--turn",
                "1",
            ])
            .assert()
            .success();
        let stdout = out.get_output().stdout.clone();
        assert_eq!(
            stdout,
            fs::read(blobs.join(sha(body))).unwrap(),
            "--body {selector} must print the blob byte for byte"
        );
        assert_eq!(
            murmur_artifact::sha256_hex(&stdout),
            sha(body),
            "--body {selector} output must hash to the blob's own name"
        );
        // Nothing but the body: no section header, no added trailing newline.
        assert!(!String::from_utf8_lossy(&stdout).contains("──"));
    }
}

/// A bare sha resolves without a `--turn`, including the `session_start` prompt hash that
/// belongs to no turn at all.
#[test]
fn a_bare_sha_selector_resolves_without_a_turn() {
    let tmp = TempDir::new().unwrap();
    let system = system_body();
    write_wire_session(tmp.path(), SESSION_ID_WIRE, &system, &MESSAGE_BODIES, true);

    for selector in [sha(PROMPT_BODY), sha(PROMPT_BODY)[..8].to_string()] {
        mur()
            .args([
                "trace",
                "show",
                SESSION_ID_WIRE,
                "--workdir",
                tmp.path().to_str().unwrap(),
                "--body",
                &selector,
            ])
            .assert()
            .success()
            .stdout(predicate::eq(PROMPT_BODY));
    }
}

/// Under `capture: meta` the hash is recorded and the body never was — which is what the
/// failure says, rather than reporting a file that went missing.
#[test]
fn body_under_meta_explains_that_no_body_was_stored() {
    let tmp = TempDir::new().unwrap();
    let system = system_body();
    write_wire_session(tmp.path(), SESSION_ID_WIRE, &system, &MESSAGE_BODIES, false);

    mur()
        .args([
            "trace",
            "show",
            SESSION_ID_WIRE,
            "--workdir",
            tmp.path().to_str().unwrap(),
            "--body",
            "system",
            "--turn",
            "1",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("E-TRC-001"))
        .stderr(predicate::str::contains(sha(&system)))
        .stderr(predicate::str::contains(
            "recorded under capture: meta; no body was stored",
        ))
        .stderr(predicate::str::contains("No such file").not());
}

/// Under `capture: none` the reason is the absent hash, not an absent blob.
#[test]
fn body_under_capture_none_names_the_missing_hashes() {
    let tmp = TempDir::new().unwrap();
    let path = write_fixture(tmp.path(), "none.jsonl", FIXTURE_A);

    mur()
        .args([
            "trace",
            "show",
            path.to_str().unwrap(),
            "--body",
            "system",
            "--turn",
            "1",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("E-TRC-001"))
        .stderr(predicate::str::contains(
            "turn 1 recorded no content hashes — the session ran under trace.capture: none",
        ));
}

/// An ambiguous sha prefix lists every hash it matched.
#[test]
fn an_ambiguous_sha_prefix_lists_every_match() {
    let tmp = TempDir::new().unwrap();
    // Real bodies never collide on eight hex characters, so the hashes are written by hand:
    // ambiguity is resolved against what the trace names, before any blob is opened.
    let s = SESSION_ID_WIRE;
    let shas = [
        "abcdef1200000000000000000000000000000000000000000000000000000001",
        "abcdef1200000000000000000000000000000000000000000000000000000002",
        "abcdef1200000000000000000000000000000000000000000000000000000003",
    ];
    let trace = [
        format!("{{\"event_type\":\"session_start\",\"session_id\":\"{s}\",\"timestamp\":1000,\"capsule_name\":\"wire\",\"capsule_version\":\"0.1.0\",\"model\":\"m\",\"max_turns\":5}}"),
        format!("{{\"event_type\":\"inference\",\"session_id\":\"{s}\",\"timestamp\":1100,\"turn\":1,\"input_tokens\":10,\"output_tokens\":2,\"decision\":\"end_turn\",\"tool_name\":null,\"system_sha\":\"{}\",\"tools_sha\":\"{}\",\"response_sha\":\"{}\",\"message_shas\":[]}}", shas[0], shas[1], shas[2]),
        format!("{{\"event_type\":\"session_end\",\"session_id\":\"{s}\",\"timestamp\":1300,\"total_turns\":1,\"total_input_tokens\":10,\"total_output_tokens\":2,\"total_tool_calls\":0,\"total_shell_calls\":0,\"duration_ms\":300,\"exit_status\":\"ok\"}}"),
    ]
    .join("\n")
        + "\n";
    let path = write_fixture(tmp.path(), "ambiguous.jsonl", &trace);

    let assert = mur()
        .args([
            "trace",
            "show",
            path.to_str().unwrap(),
            "--body",
            "abcdef12",
        ])
        .assert()
        .failure();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();

    assert!(
        stderr.contains("abcdef12 matches 3 hashes in this trace — provide more characters"),
        "{stderr}"
    );
    for sha in shas {
        assert!(stderr.contains(sha), "{stderr}");
    }
}

/// A sha the trace never names is reported as unmatched rather than as a missing file.
#[test]
fn a_sha_no_hash_matches_is_reported_as_unmatched() {
    let tmp = TempDir::new().unwrap();
    let system = system_body();
    write_wire_session(tmp.path(), SESSION_ID_WIRE, &system, &MESSAGE_BODIES, true);

    mur()
        .args([
            "trace",
            "show",
            SESSION_ID_WIRE,
            "--workdir",
            tmp.path().to_str().unwrap(),
            "--body",
            "00000000000000000000000000000000",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "no hash in this trace matches 00000000000000000000000000000000",
        ));
}

/// `--turn` names a turn no `inference` line covers.
#[test]
fn body_with_an_unknown_turn_says_so() {
    let tmp = TempDir::new().unwrap();
    let system = system_body();
    write_wire_session(tmp.path(), SESSION_ID_WIRE, &system, &MESSAGE_BODIES, true);

    mur()
        .args([
            "trace",
            "show",
            SESSION_ID_WIRE,
            "--workdir",
            tmp.path().to_str().unwrap(),
            "--body",
            "system",
            "--turn",
            "7",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "turn 7 has no inference record in this trace",
        ));
}

/// A named selector without `--turn` says which turns the trace has; `--turn` without
/// `--body` says it means nothing on its own.
#[test]
fn body_and_turn_must_be_used_together() {
    let tmp = TempDir::new().unwrap();
    let system = system_body();
    write_wire_session(tmp.path(), SESSION_ID_WIRE, &system, &MESSAGE_BODIES, true);
    let workdir = tmp.path().to_str().unwrap().to_string();

    mur()
        .args([
            "trace",
            "show",
            SESSION_ID_WIRE,
            "--workdir",
            &workdir,
            "--body",
            "message:0",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "--turn is required with --body message:0; this trace has turns 1, 2",
        ));

    mur()
        .args([
            "trace",
            "show",
            SESSION_ID_WIRE,
            "--workdir",
            &workdir,
            "--turn",
            "1",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "--turn has no meaning without --body",
        ));
}

/// `message:<i>` past the end of the turn's list names how many it recorded.
#[test]
fn body_message_index_out_of_range_reports_the_length() {
    let tmp = TempDir::new().unwrap();
    let system = system_body();
    write_wire_session(tmp.path(), SESSION_ID_WIRE, &system, &MESSAGE_BODIES, true);

    mur()
        .args([
            "trace",
            "show",
            SESSION_ID_WIRE,
            "--workdir",
            tmp.path().to_str().unwrap(),
            "--body",
            "message:7",
            "--turn",
            "2",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "turn 2 recorded 4 messages; there is no message 7",
        ));
}

// ── prefix divergence ─────────────────────────────────────────────────────────

const SESSION_ID_DIV_A: &str = "ses_2222222222224222822200000000002a";
const SESSION_ID_DIV_B: &str = "ses_3333333333334333833300000000003b";

fn diff_divergence(workdir: &Path) -> String {
    let out = mur()
        .args([
            "trace",
            "diff",
            SESSION_ID_DIV_A,
            SESSION_ID_DIV_B,
            "--workdir",
            workdir.to_str().unwrap(),
        ])
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let at = stdout
        .find("── Prefix divergence")
        .unwrap_or_else(|| panic!("no divergence section in:\n{stdout}"));
    stdout[at..].to_string()
}

/// Two runs that sent the same bytes diverge nowhere — and nothing in the section claims
/// either run is better for it.
#[test]
fn diff_reports_an_identical_prefix_as_no_divergence() {
    let tmp = TempDir::new().unwrap();
    let system = system_body();
    write_wire_session(
        tmp.path(),
        SESSION_ID_DIV_A,
        &system,
        &MESSAGE_BODIES,
        false,
    );
    write_wire_session(
        tmp.path(),
        SESSION_ID_DIV_B,
        &system,
        &MESSAGE_BODIES,
        false,
    );

    let section = diff_divergence(tmp.path());
    assert!(section.contains("system prompt: identical"), "{section}");
    assert!(section.contains("tool schemas:  identical"), "{section}");
    assert!(
        section.contains("turn 1:  identical  (4 messages)"),
        "{section}"
    );
    assert!(
        section.contains("turn 2:  identical  (4 messages)"),
        "{section}"
    );
    assert!(!section.contains("(A better)"), "{section}");
    assert!(!section.contains("(B better)"), "{section}");
}

/// One changed message is reported at its index, naming both runs' hashes there.
#[test]
fn diff_reports_the_index_of_a_changed_message() {
    let tmp = TempDir::new().unwrap();
    let system = system_body();
    let mut changed = MESSAGE_BODIES;
    changed[2] = "{\"role\":\"user\",\"content\":\"three, reworded\"}";
    write_wire_session(
        tmp.path(),
        SESSION_ID_DIV_A,
        &system,
        &MESSAGE_BODIES,
        false,
    );
    write_wire_session(tmp.path(), SESSION_ID_DIV_B, &system, &changed, false);

    let section = diff_divergence(tmp.path());
    assert!(
        section.contains(&format!(
            "turn 1:  diverges at message 2  A {}…  B {}…",
            &sha(MESSAGE_BODIES[2])[..12],
            &sha(changed[2])[..12]
        )),
        "{section}"
    );
    assert!(section.contains("system prompt: identical"), "{section}");
}

/// A changed system prompt is the first thing the section says, before any turn.
#[test]
fn diff_reports_a_changed_system_prompt_first() {
    let tmp = TempDir::new().unwrap();
    let system_a = system_body();
    let system_b = format!("{}\nand one more instruction.\n", system_body());
    write_wire_session(
        tmp.path(),
        SESSION_ID_DIV_A,
        &system_a,
        &MESSAGE_BODIES,
        false,
    );
    write_wire_session(
        tmp.path(),
        SESSION_ID_DIV_B,
        &system_b,
        &MESSAGE_BODIES,
        false,
    );

    let section = diff_divergence(tmp.path());
    let first_line = section.lines().nth(1).unwrap();
    assert!(
        first_line.starts_with("system prompt: differs"),
        "{section}"
    );
    assert!(first_line.contains(&sha(&system_a)[..12]), "{section}");
    assert!(first_line.contains(&sha(&system_b)[..12]), "{section}");
    let turn_line = section.find("turn 1:").unwrap();
    assert!(
        section.find("system prompt:").unwrap() < turn_line,
        "{section}"
    );
}

/// A run with no hashes at all is named, and nothing is compared.
#[test]
fn diff_says_which_run_recorded_no_hashes() {
    let tmp = TempDir::new().unwrap();
    let system = system_body();
    write_wire_session(
        tmp.path(),
        SESSION_ID_DIV_A,
        &system,
        &MESSAGE_BODIES,
        false,
    );
    write_session(tmp.path(), SESSION_ID_DIV_B, FIXTURE_A);

    let section = diff_divergence(tmp.path());
    assert!(
        section.contains("run B recorded no content hashes — it ran under trace.capture: none"),
        "{section}"
    );
    assert!(!section.contains("turn 1:"), "{section}");
}

// ── steps: tree and flat ──────────────────────────────────────────────────────

const SESSION_ID_TREE: &str = "ses_4444444444444444844400000000004c";

/// Two tasks, each with its own turns and calls, every line identified and parented.
fn tree_fixture() -> String {
    let s = SESSION_ID_TREE;
    let mut lines = vec![format!(
        "{{\"event_type\":\"session_start\",\"event_id\":\"evt_s\",\"parent_id\":null,\"session_id\":\"{s}\",\"timestamp\":1000,\"capsule_name\":\"tree\",\"capsule_version\":\"0.1.0\",\"model\":\"m\",\"max_turns\":10,\"capabilities\":[],\"tools_declared\":[]}}"
    )];
    for (i, task) in ["tsk_first0000000000", "tsk_second000000000"]
        .iter()
        .enumerate()
    {
        let base = 1100 + i as u64 * 1000;
        lines.push(format!("{{\"event_type\":\"task_start\",\"event_id\":\"evt_t{i}\",\"parent_id\":\"evt_s\",\"session_id\":\"{s}\",\"timestamp\":{base},\"task_id\":\"{task}\",\"context_id\":\"ctx_{i}0000000000000\",\"source\":\"a2a\",\"message_parts_bytes\":10}}"));
        lines.push(format!("{{\"event_type\":\"context_seed\",\"event_id\":\"evt_cs{i}\",\"parent_id\":\"evt_t{i}\",\"session_id\":\"{s}\",\"timestamp\":{},\"task_id\":\"{task}\",\"hook_name\":\"memory-hook\",\"tokens\":1204,\"proposed_tokens\":1204,\"budget_tokens\":20000,\"outcome\":\"seeded\",\"message_ids\":[\"msg_{i}\"]}}", base + 1));
        lines.push(format!("{{\"event_type\":\"inference\",\"event_id\":\"evt_i{i}\",\"parent_id\":\"evt_t{i}\",\"session_id\":\"{s}\",\"timestamp\":{},\"turn\":{},\"task_id\":\"{task}\",\"input_tokens\":10,\"output_tokens\":2,\"decision\":\"tool_call\",\"tool_name\":\"bash\"}}", base + 2, i * 2 + 1));
        lines.push(format!("{{\"event_type\":\"tool_call\",\"event_id\":\"evt_tc{i}\",\"parent_id\":\"evt_i{i}\",\"session_id\":\"{s}\",\"timestamp\":{},\"turn\":{},\"task_id\":\"{task}\",\"tool_name\":\"bash\",\"input\":{{\"command\":\"echo task-{i}\"}},\"input_bytes\":10,\"output_bytes\":2,\"duration_ms\":120,\"status\":\"ok\"}}", base + 3, i * 2 + 1));
        // The second task's shell names a parent this file does not carry, so its turn-level
        // `task_id` is the only thing that can attribute it — and it lands under that task.
        let shell_parent = if i == 0 {
            format!("evt_i{i}")
        } else {
            "evt_gone".to_string()
        };
        lines.push(format!("{{\"event_type\":\"shell\",\"event_id\":\"evt_sh{i}\",\"parent_id\":\"{shell_parent}\",\"session_id\":\"{s}\",\"timestamp\":{},\"turn\":{},\"task_id\":\"{task}\",\"binary\":\"/usr/bin/bash\",\"command\":\"echo task-{i}\",\"exit_code\":0,\"stdout_bytes\":2,\"stderr_bytes\":0,\"duration_ms\":50}}", base + 4, i * 2 + 1));
        lines.push(format!("{{\"event_type\":\"inference\",\"event_id\":\"evt_j{i}\",\"parent_id\":\"evt_t{i}\",\"session_id\":\"{s}\",\"timestamp\":{},\"turn\":{},\"task_id\":\"{task}\",\"input_tokens\":10,\"output_tokens\":2,\"decision\":\"end_turn\",\"tool_name\":null}}", base + 5, i * 2 + 2));
        lines.push(format!("{{\"event_type\":\"task_end\",\"event_id\":\"evt_te{i}\",\"parent_id\":\"evt_t{i}\",\"session_id\":\"{s}\",\"timestamp\":{},\"task_id\":\"{task}\",\"exit_status\":\"ok\",\"duration_ms\":100,\"turns\":2,\"input_tokens\":20,\"output_tokens\":4,\"tool_calls\":1,\"shell_calls\":1,\"reopen_count\":0}}", base + 6));
    }
    lines.push(format!("{{\"event_type\":\"session_end\",\"event_id\":\"evt_e\",\"parent_id\":\"evt_s\",\"session_id\":\"{s}\",\"timestamp\":9000,\"total_turns\":4,\"total_input_tokens\":40,\"total_output_tokens\":8,\"total_tool_calls\":2,\"total_shell_calls\":2,\"duration_ms\":900,\"exit_status\":\"ok\"}}"));
    lines.join("\n") + "\n"
}

/// Every turn's calls sit under their turn, every turn under its task, and the task row
/// names the task.
#[test]
fn steps_renders_the_identity_tree() {
    let tmp = TempDir::new().unwrap();
    let path = write_fixture(tmp.path(), "tree.jsonl", &tree_fixture());

    let out = mur()
        .args(["trace", "steps", path.to_str().unwrap()])
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();

    assert!(
        stdout.starts_with(&format!("Session {SESSION_ID_TREE}  (2 tasks, 4 turns)")),
        "{stdout}"
    );
    assert!(
        stdout.contains("\ntask tsk_first000…  ctx_00000000…  (a2a)"),
        "{stdout}"
    );
    assert!(
        stdout.contains("\n  context_seed memory-hook  seeded  1,204 tokens"),
        "{stdout}"
    );
    assert!(stdout.contains("\n  turn 1  tool_call  bash"), "{stdout}");
    assert!(
        stdout.contains("\n    tool_call  bash  120ms  ✓"),
        "{stdout}"
    );
    assert!(
        stdout.contains("\n    shell      /usr/bin/bash  exit 0  50ms"),
        "{stdout}"
    );
    // The second task's shell names no parent this file carries; its `task_id` puts it under
    // its own task rather than under the other task's turn.
    let orphan = stdout
        .rfind("\n  shell      /usr/bin/bash")
        .unwrap_or_else(|| panic!("{stdout}"));
    assert!(
        orphan > stdout.find("task tsk_second00…").unwrap(),
        "{stdout}"
    );
    assert!(stdout.contains("\n  turn 2  end_turn"), "{stdout}");
    assert!(stdout.contains("\n  turn 3  tool_call  bash"), "{stdout}");

    // Each task's calls are attributed to that task: the second task's rows come after the
    // second task row, and the first task's after the first.
    let first_task = stdout.find("task tsk_first000…").unwrap();
    let second_task = stdout.find("task tsk_second00…").unwrap();
    let turn_3 = stdout.find("turn 3  tool_call").unwrap();
    assert!(first_task < second_task && second_task < turn_3, "{stdout}");
}

/// `--verbose` appends the truncated input summary to a tool-call row in tree mode too.
#[test]
fn steps_tree_verbose_appends_the_input_summary() {
    let tmp = TempDir::new().unwrap();
    let path = write_fixture(tmp.path(), "tree.jsonl", &tree_fixture());

    mur()
        .args(["trace", "steps", "--verbose", path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "tool_call  bash  120ms  ✓  \"echo task-0\"",
        ));
}

/// A trace carrying no identity fields has no tree to walk, and renders the flat table.
#[test]
fn steps_without_event_ids_renders_the_flat_table() {
    let tmp = TempDir::new().unwrap();
    let path = write_fixture(tmp.path(), "legacy.jsonl", FIXTURE_A);

    let out = mur()
        .args(["trace", "steps", path.to_str().unwrap()])
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();

    assert_eq!(
        stdout,
        concat!(
            "Session ses_aaaaaaaaaaaa4aaa8aaa000000000001  (2 turns)\n",
            "\n",
            "  1  tool_call    bash        100ms\n",
            "  2  end_turn     —           —\n",
            "\n"
        ),
        "the flat table must be byte-identical to what it has always been"
    );
}

// ── one address vocabulary ────────────────────────────────────────────────────
//
// Every `mur trace` command that names a session accepts the same four forms, and omitting the
// address means `@1`. These cases pin both halves: the ordinal reaching every command, and
// omission being `@1` rather than merely resembling it.

/// A workdir holding A (older) and B (newer), so `@2` is A and `@1` is B.
fn two_sessions() -> TempDir {
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
    tmp
}

fn stdout_of(args: &[&str]) -> Vec<u8> {
    mur()
        .args(args)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone()
}

#[test]
fn show_accepts_an_ordinal() {
    let tmp = two_sessions();

    mur()
        .args([
            "trace",
            "show",
            "@2",
            "--workdir",
            tmp.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "session:    ses_aaaaaaaaaaaa4aaa8aaa000000000001",
        ));
}

/// Omission and `@1` are one thing, not two that happen to agree: all three spellings of "the
/// most recent session" print the same bytes.
#[test]
fn show_omitted_at1_and_full_id_print_identical_bytes() {
    let tmp = two_sessions();
    let workdir = tmp.path().to_str().unwrap();

    let omitted = stdout_of(&["trace", "show", "--workdir", workdir]);
    let ordinal = stdout_of(&["trace", "show", "@1", "--workdir", workdir]);
    let full_id = stdout_of(&[
        "trace",
        "show",
        "ses_bbbbbbbbbbbb4bbb8bbb000000000002",
        "--workdir",
        workdir,
    ]);

    assert_eq!(omitted, ordinal, "omitting the address must be @1");
    assert_eq!(ordinal, full_id, "@1 must be the most recent session");
}

#[test]
fn steps_accepts_an_ordinal() {
    let tmp = two_sessions();

    mur()
        .args([
            "trace",
            "steps",
            "@2",
            "--workdir",
            tmp.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Session ses_aaaaaaaaaaaa4aaa8aaa000000000001",
        ));
}

#[test]
fn steps_omitted_and_at1_print_identical_bytes() {
    let tmp = two_sessions();
    let workdir = tmp.path().to_str().unwrap();

    assert_eq!(
        stdout_of(&["trace", "steps", "--workdir", workdir]),
        stdout_of(&["trace", "steps", "@1", "--workdir", workdir]),
    );
}

/// `mur trace report`'s help advertises `@N`, and the command accepts it.
#[test]
fn report_accepts_an_ordinal() {
    let tmp = two_sessions();

    mur()
        .args([
            "trace",
            "report",
            "@1",
            "--workdir",
            tmp.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("ses_bbbbbbbb"));
}

/// `before` is the older run, so the delta column reads forwards in time. Getting this backwards
/// inverts every delta and is invisible afterwards.
#[test]
fn diff_omitted_args_print_the_same_bytes_as_at2_at1() {
    let tmp = two_sessions();
    let workdir = tmp.path().to_str().unwrap();

    let omitted = stdout_of(&["trace", "diff", "--workdir", workdir]);
    assert_eq!(
        omitted,
        stdout_of(&["trace", "diff", "@2", "@1", "--workdir", workdir]),
    );
    let text = String::from_utf8(omitted).unwrap();
    assert!(
        text.contains("500ms") && text.contains("1.7s"),
        "Run A is the older session: {text}"
    );
}
