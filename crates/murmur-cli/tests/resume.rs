//! `mur run --resume`: continuing a named conversation.
//!
//! Every case drives the real `mur` binary against a real Wasmtime driver and a scripted
//! provider, because the property under test spans two launches: the second one has to find the
//! first one's context on disk, load the record that context wrote, and put it in front of the
//! model. `common::ScriptedServer::requests()` returns every driver request body, so "the first
//! inference request carried that run's messages" is asserted directly rather than inferred.
//!
//! The compaction hook is hand-authored WAT compiled in-test, so nothing here depends on a
//! `default-artifacts` checkout and no case is `#[ignore]`d.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    path::{Path, PathBuf},
};

use assert_cmd::{assert::Assert, Command};
use serde_json::{json, Value};
use tempfile::TempDir;

use common::hook_wat::{compaction_hook_wasm, create_hook_zip};

const DRIVER_NAME: &str = "murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";
const CAPSULE_NAME: &str = "resume-capsule";
const SUMMARY: &str = "COMPACTED-SUMMARY-OF-EARLIER-TALK";

/// One Anthropic response that ends the turn.
fn end_turn(text: &str) -> String {
    json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": 10, "output_tokens": 5}
    })
    .to_string()
}

/// A capsule manifest declaring the driver, `blocks` of extra top-level YAML, and the hooks.
fn create_manifest(
    project_dir: &Path,
    endpoint: &str,
    blocks: &str,
    hook_names: &[&str],
) -> PathBuf {
    let hooks: String = hook_names
        .iter()
        .map(|name| format!("  - name: {name}\n    version: 0.1.0\n    runtime: hook\n"))
        .collect();
    let manifest = format!(
        "name: {CAPSULE_NAME}\nversion: 0.1.0\n{blocks}artifacts:\n  - name: {DRIVER_NAME}\n    \
         version: {DRIVER_VERSION}\n    runtime: driver\n{hooks}capabilities:\n  network:\n    \
         allow:\n      - {endpoint}\ninference:\n  transport: http\n  endpoint: {endpoint}\n  \
         model: test-model\n  api_key: test-key\n  driver:\n    artifact: {DRIVER_NAME}\n",
    );
    fs::write(project_dir.join("murmur.yaml"), manifest).unwrap();
    project_dir.join("murmur.yaml")
}

/// Publish the driver, and each hook, into `home`'s artifact store.
fn publish_artifacts(home: &TempDir, artifact_dir: &Path, hooks: &[(&str, &str, &str, Vec<u8>)]) {
    let driver = common::create_driver_artifact(
        artifact_dir,
        DRIVER_NAME,
        DRIVER_VERSION,
        &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(home, &driver).success();
    for (name, binding, commit_policy, wasm) in hooks {
        let artifact = create_hook_zip(artifact_dir, name, binding, commit_policy, wasm);
        common::publish_local(home, &artifact).success();
    }
}

/// One project, its temp home and its scripted provider, wired together.
struct Fixture {
    home: TempDir,
    project: TempDir,
    _artifacts: TempDir,
    server: common::ScriptedServer,
    manifest: PathBuf,
}

fn fixture(responses: Vec<String>, blocks: &str, hooks: &[(&str, &str, &str, Vec<u8>)]) -> Fixture {
    let server = common::ScriptedServer::start(responses);
    let home = tempfile::tempdir().unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    publish_artifacts(&home, artifacts.path(), hooks);
    let names: Vec<&str> = hooks.iter().map(|(name, _, _, _)| *name).collect();
    let manifest = create_manifest(project.path(), &server.endpoint, blocks, &names);
    Fixture {
        home,
        project,
        _artifacts: artifacts,
        server,
        manifest,
    }
}

impl Fixture {
    /// `mur run` with `task` inline, plus whatever flags the case needs.
    fn run(&self, task: &str, extra: &[&str]) -> Assert {
        let mut cmd = Command::cargo_bin("mur").unwrap();
        cmd.env("HOME", self.home.path())
            .env_remove("NEXUS_API_KEY");
        cmd.args([
            "run",
            "--manifest",
            self.manifest.to_str().unwrap(),
            "--task",
            task,
            "--verbose",
        ]);
        cmd.args(extra);
        cmd.assert()
    }

    /// Where this fixture's sessions land with no `--workdir`: `<manifest_dir>/workdir`.
    fn session_root(&self) -> PathBuf {
        self.project.path().join("workdir")
    }
}

/// A launched run's session workdir, read off the line `mur run --verbose` prints.
fn workdir_of(assert: Assert) -> PathBuf {
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    common::parse_workdir_from_stdout(&stdout)
}

/// The session id a launched run reported, taken off its session directory.
fn session_id_of(assert: Assert) -> String {
    workdir_of(assert)
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

/// Every message one driver request carried, as one searchable string.
fn request_text(request: &Value) -> String {
    request["messages"].to_string()
}

/// The `session_start` line of a session directory's trace.
fn session_start(session_dir: &Path) -> Value {
    let content = fs::read_to_string(session_dir.join("trace.jsonl")).unwrap();
    content
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|event| event["event_type"] == "session_start")
        .expect("every launch writes one session_start")
}

/// Every line of the record one context wrote.
fn record_lines(home: &TempDir, context_id: &str) -> Vec<String> {
    let path = home
        .path()
        .join(".murmur/conversations")
        .join(CAPSULE_NAME)
        .join(context_id)
        .join("conversation.jsonl");
    fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("reading {}: {err}", path.display()))
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// A hand-written session directory holding exactly `lines`, for the cases that need a trace no
/// real launch produces.
fn write_session(root: &Path, session_id: &str, lines: &[Value]) {
    let dir = root.join(session_id);
    fs::create_dir_all(&dir).unwrap();
    let body: String = lines
        .iter()
        .map(|line| format!("{line}\n"))
        .collect::<Vec<_>>()
        .concat();
    fs::write(dir.join("trace.jsonl"), body).unwrap();
}

/// The minimum `session_start` `mur`'s own reader accepts.
fn session_start_line(session_id: &str) -> Value {
    json!({
        "event_type": "session_start",
        "session_id": session_id,
        "capsule_name": CAPSULE_NAME,
        "capsule_version": "0.1.0",
        "model": "test-model",
        "max_turns": 8
    })
}

fn task_start_line(session_id: &str, context_id: &str) -> Value {
    json!({
        "event_type": "task_start",
        "session_id": session_id,
        "task_id": "tsk_1",
        "context_id": context_id
    })
}

// ── Scenarios ────────────────────────────────────────────────────────────────

/// The whole point: a second launch that names the first one continues its conversation. Neither
/// run passes `--context`, so the only way run 1's messages can reach run 2 is the session
/// address resolving to the context run 1 minted for itself.
#[test]
fn resume_at_1_continues_the_previous_runs_conversation() {
    let f = fixture(
        vec![end_turn("first reply"), end_turn("second reply")],
        "",
        &[],
    );

    f.run("first task", &[]).success();
    f.run("second task", &["--resume", "@1"]).success();

    let requests = f.server.requests();
    assert_eq!(requests.len(), 2, "one inference per run");
    let second = request_text(&requests[1]);
    let first_task = second
        .find("first task")
        .expect("run 1's user text must be in front of the model");
    let first_reply = second
        .find("first reply")
        .expect("run 1's assistant text too");
    let second_task = second
        .find("second task")
        .expect("and this run's own task after them");
    assert!(
        first_task < first_reply && first_reply < second_task,
        "in conversation order: {second}"
    );
    drop(f.project);
}

/// `--resume` overrides `lifecycle.conversation`, and changes nothing without it: the capsule's
/// own behaviour is what it always was, and the operator overrides it once at the command line.
#[test]
fn a_stateless_capsule_resumes_and_is_unchanged_without_resume() {
    let f = fixture(
        vec![
            end_turn("r1"),
            end_turn("r2"),
            end_turn("r3"),
            end_turn("r4"),
        ],
        "lifecycle:\n  conversation: stateless\n",
        &[],
    );

    f.run("remembered task", &[]).success();
    f.run("resumed task", &["--resume", "@1"]).success();

    // The control: two runs sharing one context id, neither resuming.
    f.run("control one", &["--context", "ctx_control"])
        .success();
    f.run("control two", &["--context", "ctx_control"])
        .success();

    let requests = f.server.requests();
    assert!(
        request_text(&requests[1]).contains("remembered task"),
        "--resume loads the record under stateless: {}",
        request_text(&requests[1])
    );
    let control = request_text(&requests[3]);
    assert!(
        !control.contains("control one"),
        "a stateless capsule without --resume starts from its own message alone: {control}"
    );
    assert!(control.contains("control two"));
    drop(f.project);
}

/// Every address form `mur trace diff` accepts, under the layout sessions land in with no
/// `--workdir`: `<manifest_dir>/workdir/<ses_id>`, which is not `$CWD/workdir` — the test
/// process's own directory is the crate root, deliberately not the manifest's.
#[test]
fn every_address_form_resolves_without_workdir() {
    let f = fixture(
        (1..=6).map(|n| end_turn(&format!("reply {n}"))).collect(),
        "",
        &[],
    );
    assert_ne!(
        std::env::current_dir().unwrap(),
        f.project.path(),
        "the manifest directory must not be the CWD, or the layout under test is not exercised"
    );

    let first = session_id_of(f.run("alpha task", &[]));
    let second = session_id_of(f.run("beta task", &[]));
    f.run("gamma task", &[]).success();

    f.run("by full id", &["--resume", &first]).success();
    f.run("by ordinal", &["--resume", "@2"]).success();
    let suffix = &second[second.len() - 6..];
    f.run("by suffix", &["--resume", suffix]).success();

    let requests = f.server.requests();
    assert!(request_text(&requests[3]).contains("alpha task"));
    // Read before this launch's own session directory exists, so @1 is the `by full id` run and
    // @2 is the `gamma task` run.
    assert!(request_text(&requests[4]).contains("gamma task"));
    assert!(request_text(&requests[5]).contains("beta task"));
    drop(f.project);
}

/// The same three address forms under the other layout: `--workdir <dir>` puts sessions at
/// `<dir>/.murmur/<ses_id>`, and `--resume` has to look there instead.
#[test]
fn every_address_form_resolves_with_workdir() {
    let f = fixture(
        (1..=6).map(|n| end_turn(&format!("reply {n}"))).collect(),
        "",
        &[],
    );
    let mount = tempfile::tempdir().unwrap();
    let mount_arg = mount.path().to_str().unwrap();

    let first = session_id_of(f.run("alpha task", &["--workdir", mount_arg]));
    let second = session_id_of(f.run("beta task", &["--workdir", mount_arg]));
    f.run("gamma task", &["--workdir", mount_arg]).success();
    assert!(
        mount.path().join(".murmur").join(&first).is_dir(),
        "--workdir puts sessions under <dir>/.murmur"
    );

    f.run("by full id", &["--workdir", mount_arg, "--resume", &first])
        .success();
    f.run("by ordinal", &["--workdir", mount_arg, "--resume", "@2"])
        .success();
    let suffix = second[second.len() - 6..].to_string();
    f.run("by suffix", &["--workdir", mount_arg, "--resume", &suffix])
        .success();

    let requests = f.server.requests();
    assert!(request_text(&requests[3]).contains("alpha task"));
    assert!(request_text(&requests[4]).contains("gamma task"));
    assert!(request_text(&requests[5]).contains("beta task"));
    drop(f.project);
}

/// `--resume` and `--context` name the same thing two ways, and there is no sensible precedence.
#[test]
fn resume_with_context_is_an_error() {
    let f = fixture(vec![end_turn("first")], "", &[]);

    f.run("first task", &[]).success();
    let before = fs::read_dir(f.session_root()).unwrap().count();

    let assert = f
        .run("second task", &["--resume", "@1", "--context", "ctx_x"])
        .failure();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(stderr.contains("E-RUN-015"), "{stderr}");
    assert!(
        stderr.contains("--resume") && stderr.contains("--context"),
        "{stderr}"
    );

    assert_eq!(
        fs::read_dir(f.session_root()).unwrap().count(),
        before,
        "a refused resume must create no session directory"
    );
    drop(f.project);
}

/// A session that never ran a task has no conversation to continue, and the refusal names it.
#[test]
fn a_session_with_no_task_start_is_an_error() {
    let f = fixture(vec![end_turn("unused")], "", &[]);
    let session = "ses_00000000000000000000000000000001";
    write_session(&f.session_root(), session, &[session_start_line(session)]);

    let assert = f.run("second task", &["--resume", session]).failure();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(stderr.contains("E-RUN-016"), "{stderr}");
    assert!(stderr.contains(session), "{stderr}");
    assert!(f.server.requests().is_empty(), "nothing reached the driver");
    drop(f.project);
}

/// A context that resolved but kept no record is a refusal, never a silent fresh start.
#[test]
fn a_resolved_context_with_no_record_is_an_error() {
    let f = fixture(vec![end_turn("unused")], "", &[]);
    let session = "ses_00000000000000000000000000000002";
    write_session(
        &f.session_root(),
        session,
        &[
            session_start_line(session),
            task_start_line(session, "ctx_absent"),
        ],
    );

    let assert = f.run("second task", &["--resume", session]).failure();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(stderr.contains("E-RUN-017"), "{stderr}");
    assert!(stderr.contains("ctx_absent"), "{stderr}");
    assert!(stderr.contains(session), "{stderr}");
    assert!(
        stderr.contains("no conversation record"),
        "the refusal must say what is missing: {stderr}"
    );
    assert!(f.server.requests().is_empty(), "nothing reached the driver");
    drop(f.project);
}

/// `--resume-mode compact` runs the bound hook over the loaded record and continues from its
/// summary, and the summary joins the record beside the lines it stands for.
#[test]
fn resume_mode_compact_summarises_the_loaded_record() {
    let f = fixture(
        vec![end_turn("verbatim assistant text"), end_turn("second")],
        "",
        &[(
            "compactor",
            "on-compaction",
            "replace-context",
            compaction_hook_wasm(SUMMARY),
        )],
    );

    f.run("first task", &["--context", "ctx_compact"]).success();
    let before = record_lines(&f.home, "ctx_compact");
    assert_eq!(before.len(), 2, "{before:?}");

    f.run(
        "second task",
        &["--resume", "@1", "--resume-mode", "compact"],
    )
    .success();

    let requests = f.server.requests();
    let second = request_text(&requests[1]);
    assert!(second.contains(SUMMARY), "{second}");
    assert!(
        !second.contains("verbatim assistant text"),
        "compaction replaced the context it stood for: {second}"
    );

    let after = record_lines(&f.home, "ctx_compact");
    assert_eq!(
        &after[..2],
        &before[..],
        "the lines the summary stands for survive byte for byte"
    );
    assert!(
        after[2..].iter().any(|line| line.contains(SUMMARY)),
        "the summary joins the record: {after:?}"
    );
    drop(f.project);
}

/// `compact` with nothing bound to `on-compaction` has nothing to produce the summary. Refused,
/// never quietly served as `full`.
#[test]
fn resume_mode_compact_without_a_hook_is_an_error() {
    let f = fixture(vec![end_turn("first")], "", &[]);

    f.run("first task", &[]).success();
    let before = f.server.requests().len();

    let assert = f
        .run(
            "second task",
            &["--resume", "@1", "--resume-mode", "compact"],
        )
        .failure();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(stderr.contains("E-RUN-018"), "{stderr}");
    assert!(stderr.contains("on-compaction"), "{stderr}");
    assert_eq!(
        f.server.requests().len(),
        before,
        "a refused launch reaches no driver"
    );
    drop(f.project);
}

/// The chain a reader follows: which session this one continued, and which context it ran under.
#[test]
fn session_start_carries_resumed_from_and_context_id() {
    let f = fixture(vec![end_turn("first"), end_turn("second")], "", &[]);

    let first_dir = workdir_of(f.run("first task", &[]));
    let first_id = first_dir
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let second_dir = workdir_of(f.run("second task", &["--resume", "@1"]));

    let plain = session_start(&first_dir);
    assert_eq!(plain["resumed_from"], Value::Null);
    assert_eq!(
        plain["context_id"],
        Value::Null,
        "a launch that mints a context id per task names none at the session level"
    );

    let resumed = session_start(&second_dir);
    assert_eq!(resumed["resumed_from"], json!(first_id));
    let context_id = resumed["context_id"]
        .as_str()
        .expect("a resumed launch names the context it resolved");
    assert!(context_id.starts_with("ctx_"), "{context_id}");
    assert_eq!(
        record_lines(&f.home, context_id).len(),
        4,
        "both runs' messages are in the one record the resume named"
    );

    // Apart from the two new keys, `session_start` is what it always was.
    let mut keys: Vec<&str> = plain
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec![
            "capabilities",
            "capsule_name",
            "capsule_version",
            "containment_achieved",
            "containment_declared",
            "context_id",
            "effective_grants",
            "event_id",
            "event_type",
            "max_turns",
            "model",
            "parent_id",
            "resumed_from",
            "session_id",
            "system_prompt_sha256",
            "system_prompt_source",
            "timestamp",
            "tools_declared",
            "userns_grant",
            "workdir_exec",
        ]
    );
    drop(f.project);
}

/// The counter-intuitive part is in the help, because it is the one thing an operator choosing
/// between the two modes cannot work out from the names.
#[test]
fn run_help_states_that_full_is_often_cheaper_than_compact() {
    let assert = Command::cargo_bin("mur")
        .unwrap()
        .args(["run", "--help"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    // Rendered with the value in brackets because omitting it is the common case.
    assert!(stdout.contains("--resume [<SESSION>]"), "{stdout}");
    assert!(stdout.contains("--resume-mode <MODE>"), "{stdout}");
    assert!(
        stdout.contains("full is often the cheaper"),
        "the help must say full is often cheaper than compact: {stdout}"
    );
    assert!(stdout.contains("prompt cache"), "{stdout}");
}

/// An address that resolves to nothing is a `mur trace` addressing failure, and reads as one —
/// `--resume` adds no third vocabulary for the same mistake.
#[test]
fn an_unresolvable_address_reuses_the_existing_trace_errors() {
    let f = fixture(vec![end_turn("first")], "", &[]);
    f.run("first task", &[]).success();

    let out_of_range = f.run("second task", &["--resume", "@9"]).failure();
    let stderr = String::from_utf8(out_of_range.get_output().stderr.clone()).unwrap();
    assert!(stderr.contains("E-TRC-002"), "{stderr}");
    assert!(stderr.contains("out of range"), "{stderr}");

    // Two sessions sharing a suffix, so the ambiguity is the resolver's to report.
    let one = "ses_0000000000000000000000000000cafe";
    let two = "ses_1111111111111111111111111111cafe";
    write_session(&f.session_root(), one, &[session_start_line(one)]);
    write_session(&f.session_root(), two, &[session_start_line(two)]);

    let ambiguous = f.run("second task", &["--resume", "cafe"]).failure();
    let stderr = String::from_utf8(ambiguous.get_output().stderr.clone()).unwrap();
    assert!(stderr.contains("E-TRC-002"), "{stderr}");
    assert!(stderr.contains("ambiguous"), "{stderr}");
    assert!(stderr.contains(one) && stderr.contains(two), "{stderr}");
    drop(f.project);
}

/// Omitting `--resume`'s value means `@1`, the session that just finished — the same thing
/// `--resume @1` names, resolved through the same arm.
#[test]
fn bare_resume_means_at_1_without_workdir() {
    let f = fixture(
        vec![end_turn("first reply"), end_turn("second reply")],
        "",
        &[],
    );

    let first_dir = workdir_of(f.run("first task", &[]));
    let first_id = first_dir
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let second_dir = workdir_of(f.run("second task", &["--resume"]));

    let second = request_text(&f.server.requests()[1]);
    assert!(
        second.contains("first task") && second.contains("first reply"),
        "run 1's conversation must be in front of the model: {second}"
    );
    assert_eq!(session_start(&second_dir)["resumed_from"], json!(first_id));
    drop(f.project);
}

/// The other workdir layout: `--workdir <dir>` puts sessions at `<dir>/.murmur/<ses_id>`, and a
/// valueless `--resume` has to find `@1` there. `--workdir` following it also shows that clap
/// never takes a `--`-prefixed token as the optional value.
#[test]
fn bare_resume_means_at_1_with_workdir() {
    let f = fixture(
        vec![end_turn("first reply"), end_turn("second reply")],
        "",
        &[],
    );
    let mount = tempfile::tempdir().unwrap();
    let mount_arg = mount.path().to_str().unwrap();

    let first = session_id_of(f.run("first task", &["--workdir", mount_arg]));
    let second_dir = workdir_of(f.run("second task", &["--resume", "--workdir", mount_arg]));

    let second = request_text(&f.server.requests()[1]);
    assert!(second.contains("first task"), "{second}");
    assert_eq!(session_start(&second_dir)["resumed_from"], json!(first));
    drop(f.project);
}

/// `--resume-mode` reads `--resume`'s presence, not its value, so it still applies when the
/// value is omitted.
#[test]
fn bare_resume_takes_a_resume_mode() {
    let f = fixture(
        vec![end_turn("verbatim assistant text"), end_turn("second")],
        "",
        &[(
            "compactor",
            "on-compaction",
            "replace-context",
            compaction_hook_wasm(SUMMARY),
        )],
    );

    f.run("first task", &[]).success();
    f.run("second task", &["--resume", "--resume-mode", "compact"])
        .success();

    let second = request_text(&f.server.requests()[1]);
    assert!(second.contains(SUMMARY), "{second}");
    assert!(
        !second.contains("verbatim assistant text"),
        "compaction replaced the context it stood for: {second}"
    );
    drop(f.project);
}

/// A valueless `--resume` arrives as `@1`, so it names the same thing `--context` does and is
/// refused for the same reason.
#[test]
fn bare_resume_with_context_is_an_error() {
    let f = fixture(vec![end_turn("first")], "", &[]);

    f.run("first task", &[]).success();
    let before = fs::read_dir(f.session_root()).unwrap().count();

    let assert = f
        .run("second task", &["--resume", "--context", "ctx_x"])
        .failure();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(stderr.contains("E-RUN-015"), "{stderr}");
    assert!(
        stderr.contains("--resume and --context name the same thing two ways"),
        "{stderr}"
    );
    assert_eq!(
        fs::read_dir(f.session_root()).unwrap().count(),
        before,
        "a refused resume must create no session directory"
    );
    drop(f.project);
}

/// A bare word after `--resume` binds as the address, and an address that names nothing reads as
/// an addressing failure rather than as clap's "unexpected argument".
#[test]
fn a_value_after_bare_resume_is_read_as_an_address() {
    let f = fixture(vec![end_turn("first")], "", &[]);
    f.run("first task", &[]).success();

    let assert = f.run("second task", &["--resume", "nonesuch"]).failure();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(stderr.contains("E-TRC-002"), "{stderr}");
    assert!(stderr.contains("--resume"), "{stderr}");
    assert!(stderr.contains("nonesuch"), "{stderr}");
    drop(f.project);
}
