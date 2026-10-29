//! `mur trace show frm_…` lists every task a member's door let in, not only the `call-member`
//! calls: a task the operator sent a member is a row of its own beside the calls, and the summary
//! counts it apart from them.
//!
//! Every case writes the members' traces as files where a formation records them — the entry
//! member under the roster directory's records group, a peer under the records group of
//! `~/.murmur/formations/<frm>/<member>` — and runs the built `mur` from the roster directory
//! under a scratch `HOME`. Nothing here launches a capsule.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use capsule_runtime::{records_group_under, FormationId};
use serde_json::{json, Value};
use tempfile::TempDir;

/// The operator's task to `lead`.
const LEAD_OPERATOR_TASK: &str = "tsk_01a18807c2d97e1a9b3c4d5e6f708192";
/// The operator's task to `worker`, whose message is stored.
const WORKER_OPERATOR_TASK: &str = "tsk_01a18807c2d97e1a9b3c4d5e6f7081aa";
/// The task `lead`'s door held for `worker`'s call.
const CALLED_TASK: &str = "tsk_01a18807c2d97e1a9b3c4d5e6f7081bb";
const CALL_ID: &str = "mcl_01a18807c2d97e1a9b3c4d5e6f7081cc";
const WORKER_MESSAGE: &str = "Name three colours.";

/// A roster directory and a scratch `HOME` holding one formation's member traces.
struct Formation {
    home: TempDir,
    roster: TempDir,
    id: FormationId,
}

impl Formation {
    fn new() -> Self {
        Self {
            home: tempfile::tempdir().unwrap(),
            roster: tempfile::tempdir().unwrap(),
            id: FormationId::mint(),
        }
    }

    /// A `ses_` id minted `offset_ms` after the formation.
    fn session(&self, offset_ms: u64, tail: &str) -> String {
        format!("ses_{:012x}{tail:0>20}", self.id.minted_at_ms() + offset_ms)
    }

    /// The session directory `session` of the entry member, or of the peer `member`.
    fn session_dir(&self, member: Option<&str>, session: &str) -> PathBuf {
        let murmur = self.home.path().join(".murmur");
        let workdir = match member {
            None => self.roster.path().to_path_buf(),
            Some(member) => {
                let dir = murmur
                    .join("formations")
                    .join(self.id.as_str())
                    .join(member);
                fs::create_dir_all(&dir).unwrap();
                dir
            }
        };
        records_group_under(&murmur, &workdir).join(session)
    }

    fn write_trace(&self, dir: &Path, lines: &[Value]) {
        fs::create_dir_all(dir).unwrap();
        let text: Vec<String> = lines.iter().map(Value::to_string).collect();
        fs::write(dir.join("trace.jsonl"), text.join("\n") + "\n").unwrap();
    }

    fn start(&self, session: &str, member: &str, callees: &[&str]) -> Value {
        json!({
            "event_type": "session_start", "event_id": "evt_0", "session_id": session,
            "timestamp": 1, "capsule_name": member, "capsule_version": "0.1.0",
            "model": "gpt-5.6-luna", "max_turns": 10,
            "formation_id": self.id.as_str(), "formation_member": member,
            "formation_callees": callees,
        })
    }

    /// `lead` (entry) and `worker`: `worker` makes one completed call to `lead`, and `lead`
    /// received that call's task and, later, the operator's.
    fn lead_and_worker(&self) -> (PathBuf, PathBuf) {
        let lead = self.session(10, "1");
        let worker = self.session(20, "2");
        let lead_dir = self.session_dir(None, &lead);
        let worker_dir = self.session_dir(Some("worker"), &worker);
        self.write_trace(
            &lead_dir,
            &[
                self.start(&lead, "lead", &["worker"]),
                received(
                    CALLED_TASK,
                    &format!("msg_{CALL_ID}"),
                    json!({"caller": "member", "caller_member": "worker"}),
                    60,
                ),
                task_end(CALLED_TASK, "completed", 70),
                received(
                    LEAD_OPERATOR_TASK,
                    "msg_op",
                    json!({"caller": "operator", "message_sha256": "ab".repeat(32)}),
                    500,
                ),
                task_end(LEAD_OPERATOR_TASK, "ok", 35_500),
            ],
        );
        self.write_trace(
            &worker_dir,
            &[
                self.start(&worker, "worker", &["lead"]),
                json!({
                    "event_type": "member_call_start", "timestamp": 50, "task_id": "tsk_w",
                    "call_id": CALL_ID, "member": "lead", "member_task_id": CALLED_TASK,
                }),
                json!({
                    "event_type": "member_call", "timestamp": 140, "task_id": "tsk_w",
                    "call_id": CALL_ID, "member": "lead", "member_task_id": CALLED_TASK,
                    "status": "completed", "duration_ms": 90, "output": "x",
                    "truncated": false, "delivered": true,
                }),
            ],
        );
        (lead_dir, worker_dir)
    }

    /// `mur <args>` from the roster directory under the scratch `HOME`.
    fn mur(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_mur"))
            .args(args)
            .current_dir(self.roster.path())
            .env("HOME", self.home.path())
            .output()
            .unwrap()
    }

    /// `mur trace show <frm>`'s stdout, failing on a non-zero exit.
    fn listing(&self) -> String {
        let shown = self.mur(&["trace", "show", self.id.as_str()]);
        let stdout = String::from_utf8_lossy(&shown.stdout).to_string();
        assert!(
            shown.status.success(),
            "mur trace show failed:\n{stdout}\n{}",
            String::from_utf8_lossy(&shown.stderr)
        );
        stdout
    }
}

fn received(task: &str, message_id: &str, fields: Value, at: u64) -> Value {
    let mut line = json!({
        "event_type": "a2a_task_received", "event_id": "evt_r", "session_id": "s",
        "timestamp": at, "task_id": task, "context_id": "ctx", "message_id": message_id,
        "traceparent_from_caller": null,
    });
    for (key, value) in fields.as_object().unwrap() {
        line[key] = value.clone();
    }
    line
}

fn task_end(task: &str, status: &str, duration_ms: u64) -> Value {
    json!({
        "event_type": "task_end", "timestamp": 1, "task_id": task, "exit_status": status,
        "duration_ms": duration_ms, "turns": 1, "input_tokens": 100, "output_tokens": 20,
        "tool_calls": 0, "shell_calls": 0, "reopen_count": 0,
    })
}

/// The listing's rows, each split into its words: every line after the summary lines.
fn rows(stdout: &str) -> Vec<Vec<&str>> {
    stdout
        .lines()
        .skip_while(|line| !line.starts_with("calls:") && !line.starts_with("tasks:"))
        .filter(|line| !line.starts_with("calls:") && !line.starts_with("tasks:"))
        .map(|line| line.split_whitespace().collect())
        .collect()
}

/// The operator's task to `lead` is a row after `worker`'s call, in time order, and the summary
/// counts the call and the task apart. No token is printed.
#[test]
fn an_operator_task_is_listed_beside_the_calls() {
    let formation = Formation::new();
    formation.lead_and_worker();
    let stdout = formation.listing();
    println!("{stdout}");

    assert!(
        stdout.lines().any(|line| line == "calls:      1"),
        "no calls line:\n{stdout}"
    );
    assert!(
        stdout
            .lines()
            .any(|line| line == "tasks:      1 not from a member — operator 1"),
        "no tasks line:\n{stdout}"
    );
    assert_eq!(
        rows(&stdout),
        [
            vec![
                CALL_ID,
                "worker",
                "→",
                "lead",
                "completed",
                "90ms",
                "delivered"
            ],
            vec![
                LEAD_OPERATOR_TASK,
                "operator",
                "→",
                "lead",
                "ok",
                "35.5s",
                "message",
                "not",
                "captured"
            ],
        ],
        "rows:\n{stdout}"
    );
    assert!(
        !stdout.contains("mdt1.") && !stdout.contains("mft1."),
        "a token was printed:\n{stdout}"
    );
}

/// A task whose message is stored names the command that prints it, and that command prints
/// exactly the message.
#[test]
fn a_task_row_names_the_command_that_prints_its_message() {
    let formation = Formation::new();
    let (_, worker_dir) = formation.lead_and_worker();
    let sha = murmur_artifact::sha256_hex(WORKER_MESSAGE.as_bytes());
    let mut trace = fs::read_to_string(worker_dir.join("trace.jsonl")).unwrap();
    for line in [
        received(
            WORKER_OPERATOR_TASK,
            "msg_op2",
            json!({"caller": "operator", "message_sha256": sha}),
            900,
        ),
        task_end(WORKER_OPERATOR_TASK, "ok", 1_200),
    ] {
        trace.push_str(&format!("{line}\n"));
    }
    fs::write(worker_dir.join("trace.jsonl"), trace).unwrap();
    fs::create_dir_all(worker_dir.join("blobs")).unwrap();
    fs::write(worker_dir.join("blobs").join(&sha), WORKER_MESSAGE).unwrap();

    let stdout = formation.listing();
    println!("{stdout}");
    assert!(
        stdout
            .lines()
            .any(|line| line == "tasks:      2 not from a member — operator 2"),
        "no tasks line:\n{stdout}"
    );
    let row = stdout
        .lines()
        .find(|line| line.starts_with(WORKER_OPERATOR_TASK))
        .unwrap_or_else(|| panic!("no row for the worker's task:\n{stdout}"));
    let expected_note = format!(
        "message: mur trace show {} --body {}",
        worker_dir.join("trace.jsonl").display(),
        &sha[..12]
    );
    assert!(row.ends_with(&expected_note), "{row}\nin:\n{stdout}");
    let command = expected_note.strip_prefix("message: mur ").unwrap();
    let args: Vec<&str> = command.split(' ').collect();
    let printed = formation.mur(&args);
    assert!(
        printed.status.success(),
        "{}",
        String::from_utf8_lossy(&printed.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&printed.stdout), WORKER_MESSAGE);
}
