//! A full session id names one session wherever it ran: every command that takes a session
//! address finds it from any folder, while `@N` ordinals and suffixes keep counting the current
//! workdir root's sessions only. Each test runs `mur` under its own scratch `HOME`, so the records
//! root it searches holds exactly the sessions the test wrote.

#[path = "common/mod.rs"]
mod common;

use std::{fs, path::Path, path::PathBuf};

use assert_cmd::Command;
use tempfile::TempDir;

const ID: &str = "ses_0192aaaa00007000800000000000beef";
const OLDER: &str = "ses_0191aaaa000070008000000000000001";
const NOWHERE: &str = "ses_0192ffff0000700080000000ffff0000";

/// `mur` under `home`, run from `cwd`.
fn mur(home: &Path, cwd: &Path) -> Command {
    let mut command = Command::cargo_bin("mur").unwrap();
    command.env("HOME", home).current_dir(cwd);
    command
}

/// A finished session's `trace.jsonl` for `capsule`.
fn trace(id: &str, capsule: &str) -> String {
    format!(
        concat!(
            "{{\"event_type\":\"session_start\",\"session_id\":\"{id}\",\"timestamp\":1000,",
            "\"capsule_name\":\"{capsule}\",\"capsule_version\":\"0.1.0\",\"model\":\"m\",",
            "\"max_turns\":10,\"capabilities\":[],\"tools_declared\":[]}}\n",
            "{{\"event_type\":\"inference\",\"session_id\":\"{id}\",\"timestamp\":1100,",
            "\"turn\":1,\"input_tokens\":100,\"output_tokens\":20,\"decision\":\"end_turn\",",
            "\"tool_name\":null}}\n",
            "{{\"event_type\":\"session_end\",\"session_id\":\"{id}\",\"timestamp\":1500,",
            "\"total_turns\":1,\"total_input_tokens\":100,\"total_output_tokens\":20,",
            "\"total_tool_calls\":0,\"total_shell_calls\":0,\"duration_ms\":500,",
            "\"exit_status\":\"ok\"}}\n"
        ),
        id = id,
        capsule = capsule
    )
}

/// Session `id` of `capsule` recorded among the sessions of the workdir root `root`; its records
/// directory.
fn write_session(home: &Path, root: &Path, id: &str, capsule: &str) -> PathBuf {
    let dir = common::session_records_dir(home, root, id);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("trace.jsonl"), trace(id, capsule)).unwrap();
    dir
}

/// The output of a `mur` that must succeed.
fn stdout_ok(command: &mut Command) -> String {
    let output = command.assert().success().get_output().clone();
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The stderr of a `mur` that must fail with `code`.
fn stderr_refused(command: &mut Command, code: &str) -> String {
    let output = command.assert().failure().get_output().clone();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(stderr.contains(&format!("error[{code}]")), "{stderr}");
    stderr
}

struct Folders {
    home: TempDir,
    a: TempDir,
    b: TempDir,
}

fn folders() -> Folders {
    Folders {
        home: TempDir::new().unwrap(),
        a: TempDir::new().unwrap(),
        b: TempDir::new().unwrap(),
    }
}

/// S1: the Finding 4 repro — a full id recorded under folder A, read from folder B.
#[test]
fn trace_show_finds_a_full_id_recorded_under_another_folder() {
    let f = folders();
    write_session(f.home.path(), f.a.path(), ID, "capsule-from-folder-a");

    let from_b = stdout_ok(mur(f.home.path(), f.b.path()).args(["trace", "show", ID]));
    assert!(from_b.contains("capsule-from-folder-a"), "{from_b}");

    let with_workdir = stdout_ok(
        mur(f.home.path(), f.b.path())
            .args(["trace", "show", ID, "--workdir"])
            .arg(f.b.path()),
    );
    assert!(
        with_workdir.contains("capsule-from-folder-a"),
        "{with_workdir}"
    );
}

/// S2: the same lookup serves `mur trace steps` and `mur eval show`.
#[test]
fn trace_steps_and_eval_show_find_it_too() {
    let f = folders();
    let dir = write_session(f.home.path(), f.a.path(), ID, "capsule-from-folder-a");
    fs::create_dir_all(dir.join("hooks")).unwrap();
    fs::write(
        dir.join("hooks/eval.jsonl"),
        concat!(
            "{\"record_type\":\"event_score\",\"ts\":1000,\"turn\":1,\"event_type\":\"session_end\",",
            "\"scorer\":\"scorer_from_folder_a\",\"result\":\"pass\",\"score\":1.0,\"reason\":\"ok\"}\n"
        ),
    )
    .unwrap();

    let steps = stdout_ok(mur(f.home.path(), f.b.path()).args(["trace", "steps", ID]));
    assert!(steps.contains("end_turn"), "{steps}");

    let eval = stdout_ok(mur(f.home.path(), f.b.path()).args(["eval", "show", ID]));
    assert!(eval.contains("scorer_from_folder_a"), "{eval}");
}

/// S3: ordinals and suffixes count the current workdir root's sessions only.
#[test]
fn short_addresses_stay_with_the_current_workdir_root() {
    let f = folders();
    write_session(f.home.path(), f.a.path(), ID, "capsule-from-folder-a");
    let b_group = common::records_group(f.home.path(), f.b.path());

    let empty = stderr_refused(
        mur(f.home.path(), f.b.path())
            .args(["trace", "show", "@1", "--workdir"])
            .arg(f.b.path()),
        "E-TRC-002",
    );
    assert!(empty.contains("no sessions found in workdir"), "{empty}");

    write_session(f.home.path(), f.b.path(), OLDER, "capsule-from-folder-b");

    let suffix = stderr_refused(
        mur(f.home.path(), f.b.path())
            .args(["trace", "show", "beef", "--workdir"])
            .arg(f.b.path()),
        "E-TRC-002",
    );
    assert!(
        suffix.contains(&format!(
            "no session found matching suffix 'beef' in {}",
            b_group.display()
        )),
        "{suffix}"
    );

    let latest = stdout_ok(
        mur(f.home.path(), f.b.path())
            .args(["trace", "show", "@1", "--workdir"])
            .arg(f.b.path()),
    );
    assert!(latest.contains("capsule-from-folder-b"), "{latest}");
    assert!(!latest.contains("capsule-from-folder-a"), "{latest}");

    let out_of_range = stderr_refused(
        mur(f.home.path(), f.b.path())
            .args(["trace", "show", "@2", "--workdir"])
            .arg(f.b.path()),
        "E-TRC-002",
    );
    assert!(
        out_of_range.contains("@2 is out of range — workdir has 1 session"),
        "{out_of_range}"
    );
}

/// S4: a full id recorded nowhere names the group and the records root it searched.
#[test]
fn a_missing_full_id_names_where_it_looked() {
    let f = folders();
    write_session(f.home.path(), f.a.path(), ID, "capsule-from-folder-a");
    let b_group = common::records_group(f.home.path(), f.b.path());
    let root = common::records_root(f.home.path());
    let looked = format!(
        "session {NOWHERE} not found: looked in {} and in 1 other records group under {}",
        b_group.display(),
        root.display()
    );

    let trace = stderr_refused(
        mur(f.home.path(), f.b.path())
            .args(["trace", "show", NOWHERE, "--workdir"])
            .arg(f.b.path()),
        "E-TRC-002",
    );
    assert!(trace.contains(&looked), "{trace}");

    let eval = stderr_refused(
        mur(f.home.path(), f.b.path())
            .args(["eval", "show", NOWHERE, "--workdir"])
            .arg(f.b.path()),
        "E-EVAL-002",
    );
    assert!(eval.contains(&looked), "{eval}");

    let a_group = common::records_group(f.home.path(), f.a.path());
    let diff = stderr_refused(
        mur(f.home.path(), f.b.path())
            .args(["trace", "diff", NOWHERE, "@1", "--workdir"])
            .arg(f.a.path()),
        "E-TRC-002",
    );
    assert!(
        diff.contains(&format!(
            "before: session {NOWHERE} not found: looked in {} and in 0 other records groups \
             under {}",
            a_group.display(),
            root.display()
        )),
        "{diff}"
    );

    let fresh = TempDir::new().unwrap();
    let none = stderr_refused(
        mur(fresh.path(), f.b.path())
            .args(["trace", "show", NOWHERE, "--workdir"])
            .arg(f.b.path()),
        "E-TRC-002",
    );
    assert!(
        none.contains(&format!(
            "and in 0 other records groups under {}",
            common::records_root(fresh.path()).display()
        )),
        "{none}"
    );
}

/// S5: the current workdir root's session wins over the same id recorded elsewhere.
#[test]
fn the_current_workdir_root_wins() {
    let f = folders();
    write_session(f.home.path(), f.a.path(), ID, "capsule-from-folder-a");
    write_session(f.home.path(), f.b.path(), ID, "capsule-from-folder-b");

    for (folder, mine, theirs) in [
        (&f.b, "capsule-from-folder-b", "capsule-from-folder-a"),
        (&f.a, "capsule-from-folder-a", "capsule-from-folder-b"),
    ] {
        let out = stdout_ok(
            mur(f.home.path(), folder.path())
                .args(["trace", "show", ID, "--workdir"])
                .arg(folder.path()),
        );
        assert!(out.contains(mine) && !out.contains(theirs), "{out}");
    }
}

/// S6: one id recorded in two other groups names neither; both are listed.
#[test]
fn a_full_id_in_two_other_groups_is_refused_with_both_listed() {
    let f = folders();
    let c = TempDir::new().unwrap();
    let in_a = write_session(f.home.path(), f.a.path(), ID, "capsule-from-folder-a");
    let in_c = write_session(f.home.path(), c.path(), ID, "capsule-from-folder-c");
    let mut listed = [in_a, in_c];
    listed.sort();

    let stderr = stderr_refused(
        mur(f.home.path(), f.b.path())
            .args(["trace", "show", ID, "--workdir"])
            .arg(f.b.path()),
        "E-TRC-002",
    );
    assert!(
        stderr.contains(&format!(
            "session {ID} is recorded in 2 records groups — name one by its trace.jsonl path \
             instead\n  {}\n  {}",
            listed[0].display(),
            listed[1].display()
        )),
        "{stderr}"
    );
}

/// S8: `mur trace rm` removes only the named workdir root's session, so its full id is looked up
/// there alone.
#[test]
fn trace_rm_stays_with_its_workdir_root() {
    let f = folders();
    let in_a = write_session(f.home.path(), f.a.path(), ID, "capsule-from-folder-a");
    let b_group = common::records_group(f.home.path(), f.b.path());

    let stderr = stderr_refused(
        mur(f.home.path(), f.b.path())
            .args(["trace", "rm", ID, "--workdir"])
            .arg(f.b.path()),
        "E-TRC-002",
    );
    assert!(
        stderr.contains(&format!("session {ID} not found in {}", b_group.display())),
        "{stderr}"
    );
    assert!(in_a.join("trace.jsonl").is_file());
}
