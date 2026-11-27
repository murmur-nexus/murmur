//! `mur run --task … --output <file>`: the file holds the bytes of the session's `out/result.txt`
//! when the task completed, and is absent after every other outcome — a failed or cut-short task,
//! a cancel, a refused lifecycle, a refused path — with a stale file from before the run removed.
//! Clap's usage refusals (exit 2) touch nothing.
//!
//! Every case drives the real `mur` binary: agent capsules against the real anthropic driver
//! fixture and a scripted provider, and the env-echo script capsule, because the properties are
//! what the process leaves on disk, its exit code and its `status:` line.

#[path = "common/mod.rs"]
mod common;

use std::{
    collections::BTreeSet,
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use assert_cmd::Command;
use serde_json::{json, Value};
use tempfile::TempDir;

const DRIVER: &str = "local/murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.0";
const TASK: &str = "Say hello.";
const STALE: &str = "stale answer";

/// A chat-completions body: well-formed JSON in another provider's shape. The anthropic driver
/// refuses it because it has no `stop_reason`, so the task fails with `driver_error`.
const CHAT_COMPLETIONS_BODY: &str = r#"{"id":"x","object":"chat.completion","choices":[{"message":{"role":"assistant","content":"hi"}}]}"#;

const CANCELED_RESULT: &str =
    "canceled: the task was canceled before it completed, so it has no result";

// ── Scripted provider responses ───────────────────────────────────────────────

fn end_turn(id: &str, text: &str) -> String {
    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

/// A turn that asks for a tool the capsule does not have, so the loop wants a second turn.
fn tool_call(id: &str) -> String {
    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "tool_use", "id": "toolu_output", "name": "no-such-tool", "input": {}}],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

// ── Homes, projects and `mur run` ─────────────────────────────────────────────

/// A scratch `HOME` with the driver published into it.
fn home_with_driver() -> TempDir {
    let home = TempDir::new().unwrap();
    let artifacts = TempDir::new().unwrap();
    let artifact = common::create_driver_artifact(
        artifacts.path(),
        DRIVER,
        DRIVER_VERSION,
        &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &artifact).success();
    home
}

/// An agent capsule reaching `endpoint` through the driver's gateway. `lifecycle` is spliced in
/// as top-level keys and `inference_extra` into the `inference:` block.
fn manifest(name: &str, endpoint: &str, lifecycle: &str, inference_extra: &str) -> String {
    format!(
        "name: {name}\nversion: 0.1.0\nartifacts:\n  - name: {DRIVER}\n    version: \
         {DRIVER_VERSION}\n    runtime: driver\n    gateway:\n      endpoint: {endpoint}\n      \
         api_key: test-key\ncapabilities:\n  network:\n    allow:\n      - {endpoint}\n\
         {lifecycle}inference:\n  transport: http\n  model: test-model\n{inference_extra}  \
         driver:\n    artifact: {DRIVER}\n"
    )
}

struct Project {
    dir: TempDir,
    manifest: PathBuf,
}

impl Project {
    fn new(manifest_yaml: &str) -> Self {
        let dir = TempDir::new().unwrap();
        let manifest = dir.path().join("murmur.yaml");
        fs::write(&manifest, manifest_yaml).unwrap();
        Self { dir, manifest }
    }

    /// The env-echo script capsule: a manifest with no `inference:` block beside its component.
    fn script() -> Self {
        let project = Self::new("name: output-script\nversion: 0.1.0\nartifacts: []\n");
        fs::copy(
            common::fixture_path("run/components/capsule-env-echo.wasm"),
            project.dir.path().join("capsule.wasm"),
        )
        .unwrap();
        project
    }

    fn manifest_arg(&self) -> &str {
        self.manifest.to_str().unwrap()
    }
}

/// `mur run --manifest <project> <args…>` under `home`, from the project directory.
fn mur_run(home: &TempDir, project: &Project, args: &[&str]) -> Run {
    let output = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .env_remove("GITHUB_TOKEN")
        .env_remove("MURMUR_TEST_ALLOWED_VAR")
        .current_dir(project.dir.path())
        .args(["run", "--manifest", project.manifest_arg()])
        .args(args)
        .output()
        .unwrap();
    Run::of(output)
}

/// `mur` with exactly `args`, under `home`, from `cwd`.
fn mur(home: &TempDir, cwd: &Path, args: &[&str]) -> Run {
    let output = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap();
    Run::of(output)
}

/// What one `mur run` left behind.
struct Run {
    output: std::process::Output,
    stdout: String,
    stderr: String,
}

impl Run {
    fn of(output: std::process::Output) -> Self {
        Self {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            output,
        }
    }

    fn code(&self) -> Option<i32> {
        self.output.status.code()
    }

    fn context(&self) -> String {
        format!(
            "exit {:?}\nstdout:\n{}\nstderr:\n{}",
            self.output.status.code(),
            self.stdout,
            self.stderr
        )
    }

    fn records_dir(&self) -> PathBuf {
        common::parse_records_from_stdout(&self.stdout)
    }

    fn workdir(&self) -> PathBuf {
        common::parse_workdir_from_stdout(&self.stdout)
    }

    /// Every `status:` line `mur run` printed.
    fn status_lines(&self) -> Vec<&str> {
        self.stdout
            .lines()
            .filter(|line| line.starts_with("status:"))
            .collect()
    }

    /// Every stderr line naming `code`.
    fn error_lines(&self, code: &str) -> Vec<&str> {
        self.stderr
            .lines()
            .filter(|line| line.contains(&format!("error[{code}]")))
            .collect()
    }

    /// Each stdout line up to its first `:`, which is what names the line; the values (session
    /// ids, paths, ports) differ from run to run.
    fn line_keys(&self) -> BTreeSet<String> {
        self.stdout
            .lines()
            .map(|line| line.split(':').next().unwrap_or(line).to_string())
            .collect()
    }

    /// Asserts a refusal with `E-IO-003` whose message contains `needle`.
    fn assert_io_003(&self, needle: &str) {
        assert_eq!(self.code(), Some(1), "{}", self.context());
        let lines = self.error_lines("E-IO-003");
        assert_eq!(lines.len(), 1, "{}", self.context());
        assert!(lines[0].contains(needle), "{}", self.context());
    }
}

/// The files directly in `dir` whose names end `.tmp`: what a write left behind.
fn temp_files(dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".tmp"))
        .collect()
}

/// Asserts no session was staged under `home`: no trace anywhere under its records root.
fn assert_no_trace(home: &TempDir, run: &Run) {
    assert_eq!(
        common::find_file(&common::records_root(home.path()), "trace.jsonl"),
        None,
        "{}",
        run.context()
    );
}

fn records_result(run: &Run) -> Vec<u8> {
    fs::read(run.records_dir().join("out/result.txt")).unwrap()
}

// ── A completed task writes its answer ────────────────────────────────────────

#[test]
fn s1_a_completed_task_writes_the_exact_answer() {
    const ANSWER: &str = "first line\nsecond line";
    let home = home_with_driver();
    let server =
        common::ScriptedServer::start(vec![end_turn("msg_1", ANSWER), end_turn("msg_2", ANSWER)]);
    let project = Project::new(&manifest("output-ok", &server.endpoint, "", ""));
    let out_dir = TempDir::new().unwrap();
    let answer = out_dir.path().join("answer.txt");
    let answer_arg = answer.to_str().unwrap();

    let run = mur_run(
        &home,
        &project,
        &["--task", TASK, "--output", answer_arg, "--verbose"],
    );
    assert_eq!(run.code(), Some(0), "{}", run.context());
    assert_eq!(run.status_lines(), ["status:  ok"], "{}", run.context());
    let written = fs::read(&answer).unwrap();
    assert_eq!(written, ANSWER.as_bytes(), "no byte trimmed or added");
    assert_eq!(written, records_result(&run));
    assert!(temp_files(out_dir.path()).is_empty());
    assert!(!run.stdout.contains(answer_arg), "{}", run.context());

    // The same capsule and task without --output prints the same lines.
    let plain = mur_run(&home, &project, &["--task", TASK, "--verbose"]);
    assert_eq!(plain.code(), Some(0), "{}", plain.context());
    assert_eq!(
        run.line_keys(),
        plain.line_keys(),
        "\nwith --output:\n{}\nwithout:\n{}",
        run.stdout,
        plain.stdout
    );
}

// ── A failed task leaves no file, and removes a stale one ────────────────────

#[test]
fn s2_a_failed_task_leaves_no_file_and_removes_the_stale_one() {
    let home = home_with_driver();
    let server = common::ScriptedServer::start(vec![CHAT_COMPLETIONS_BODY.to_string()]);
    let project = Project::new(&manifest("output-failed", &server.endpoint, "", ""));
    let out_dir = TempDir::new().unwrap();
    let answer = out_dir.path().join("answer.txt");
    fs::write(&answer, STALE).unwrap();

    let run = mur_run(
        &home,
        &project,
        &[
            "--task",
            TASK,
            "--output",
            answer.to_str().unwrap(),
            "--verbose",
        ],
    );
    assert_eq!(run.code(), Some(1), "{}", run.context());
    assert_eq!(run.status_lines(), ["status:  failed"], "{}", run.context());
    assert_eq!(run.error_lines("E-RUN-040").len(), 1, "{}", run.context());
    assert!(!answer.exists(), "{}", run.context());
    assert!(
        records_result(&run).starts_with(b"error: "),
        "{:?}",
        String::from_utf8_lossy(&records_result(&run))
    );
    assert!(temp_files(out_dir.path()).is_empty());
}

// ── A spent turn budget leaves no file ───────────────────────────────────────

#[test]
fn s3_max_turns_reached_leaves_no_file() {
    let home = home_with_driver();
    let server = common::ScriptedServer::start(vec![tool_call("msg_1")]);
    let project = Project::new(&manifest(
        "output-max-turns",
        &server.endpoint,
        "",
        "  max_turns: 1\n",
    ));
    let out_dir = TempDir::new().unwrap();
    let answer = out_dir.path().join("answer.txt");
    fs::write(&answer, STALE).unwrap();

    let run = mur_run(
        &home,
        &project,
        &[
            "--task",
            TASK,
            "--output",
            answer.to_str().unwrap(),
            "--verbose",
        ],
    );
    assert_eq!(run.code(), Some(1), "{}", run.context());
    assert_eq!(
        run.status_lines(),
        ["status:  max_turns_reached"],
        "{}",
        run.context()
    );
    assert!(!answer.exists(), "{}", run.context());
}

// ── A canceled task leaves no file ───────────────────────────────────────────

#[test]
fn s4_a_canceled_task_leaves_no_file() {
    let home = home_with_driver();
    let (server, arrived, release) = common::held_provider(end_turn("msg_1", "too late"));
    let project = Project::new(&manifest("output-canceled", &server.endpoint, "", ""));
    let out_dir = TempDir::new().unwrap();
    let answer = out_dir.path().join("answer.txt");
    fs::write(&answer, STALE).unwrap();

    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("mur"))
        .env("HOME", home.path())
        .current_dir(project.dir.path())
        .args([
            "run",
            "--manifest",
            project.manifest_arg(),
            "--task",
            TASK,
            "--output",
            answer.to_str().unwrap(),
            "--verbose",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout_pipe = child.stdout.take().unwrap();
    let mut stderr_pipe = child.stderr.take().unwrap();
    let stdout_reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout_pipe.read_to_string(&mut text);
        text
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr_pipe.read_to_string(&mut text);
        text
    });

    let reached = arrived.recv_timeout(Duration::from_secs(180));
    if reached.is_err() {
        let _ = child.kill();
    }
    assert!(reached.is_ok(), "the provider request never arrived");
    // SAFETY: `child.id()` is the pid of a child this test spawned and has not reaped.
    assert_eq!(
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) },
        0
    );
    // Answered only after the signal, so the task cannot have completed before it.
    std::thread::sleep(Duration::from_millis(500));
    let _ = release.send(());

    let status = child.wait().unwrap();
    let stdout = stdout_reader.join().unwrap();
    let stderr = stderr_reader.join().unwrap();
    let context = format!(
        "exit {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        status.code()
    );
    assert_eq!(status.code(), Some(1), "{context}");
    let status_lines: Vec<&str> = stdout
        .lines()
        .filter(|line| line.starts_with("status:"))
        .collect();
    assert_eq!(status_lines, ["status:  canceled"], "{context}");
    assert!(!answer.exists(), "{context}");
    let records = common::parse_records_from_stdout(&stdout);
    assert_eq!(
        fs::read_to_string(records.join("out/result.txt")).unwrap(),
        CANCELED_RESULT,
        "{context}"
    );
}

// ── Parallel runs each write their own file ──────────────────────────────────

#[test]
fn s5_parallel_runs_write_separate_exact_files() {
    let home = home_with_driver();
    let alpha_server = common::ScriptedServer::start(vec![end_turn("msg_a", "alpha answer")]);
    let beta_server = common::ScriptedServer::start(vec![end_turn("msg_b", "beta answer")]);
    let alpha = Project::new(&manifest("output-alpha", &alpha_server.endpoint, "", ""));
    let beta = Project::new(&manifest("output-beta", &beta_server.endpoint, "", ""));
    let out_dir = TempDir::new().unwrap();
    let alpha_answer = out_dir.path().join("alpha.txt");
    let beta_answer = out_dir.path().join("beta.txt");

    let spawn = |project: &Project, answer: &Path, json: bool| {
        let mut command = std::process::Command::new(assert_cmd::cargo::cargo_bin("mur"));
        command
            .env("HOME", home.path())
            .current_dir(project.dir.path())
            .args([
                "run",
                "--manifest",
                project.manifest_arg(),
                "--task",
                TASK,
                "--output",
                answer.to_str().unwrap(),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if json {
            command.arg("--json");
        }
        command.spawn().unwrap()
    };
    let alpha_child = spawn(&alpha, &alpha_answer, false);
    let beta_child = spawn(&beta, &beta_answer, true);
    let alpha_run = Run::of(alpha_child.wait_with_output().unwrap());
    let beta_run = Run::of(beta_child.wait_with_output().unwrap());

    assert_eq!(alpha_run.code(), Some(0), "{}", alpha_run.context());
    assert_eq!(beta_run.code(), Some(0), "{}", beta_run.context());
    assert_eq!(fs::read(&alpha_answer).unwrap(), b"alpha answer");
    assert_eq!(fs::read(&beta_answer).unwrap(), b"beta answer");
    assert_eq!(alpha_run.status_lines(), ["status:  ok"]);

    let json_lines: Vec<&str> = beta_run
        .stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    assert_eq!(json_lines.len(), 1, "{}", beta_run.context());
    let ready: Value = serde_json::from_str(json_lines[0]).unwrap();
    for key in [
        "url",
        "pid",
        "session_id",
        "name",
        "version",
        "workdir",
        "records_dir",
    ] {
        assert!(ready.get(key).is_some(), "no {key} in {ready}");
    }
    assert!(temp_files(out_dir.path()).is_empty());
}

// ── A lifecycle that can run another task is refused ─────────────────────────

#[test]
fn s6_a_lifecycle_that_runs_other_tasks_is_refused_before_staging() {
    let home = home_with_driver();
    let server = common::ScriptedServer::start(vec![end_turn("msg_1", "never asked")]);
    let default_lifecycle = Project::new(&manifest("output-lifecycle", &server.endpoint, "", ""));
    let queue_lifecycle = Project::new(&manifest(
        "output-queue",
        &server.endpoint,
        "lifecycle:\n  task_acceptance: queue\n  after_task: exit\n",
        "",
    ));
    let out_dir = TempDir::new().unwrap();
    let answer = out_dir.path().join("answer.txt");
    let answer_arg = answer.to_str().unwrap();

    let cases: [(&Project, &[&str], &str); 3] = [
        (
            &default_lifecycle,
            &["--lifecycle-after-task", "sleep"],
            "(lifecycle.after_task: sleep)",
        ),
        (
            &default_lifecycle,
            &["--lifecycle-task-acceptance", "queue"],
            "(lifecycle.task_acceptance: queue)",
        ),
        (&queue_lifecycle, &[], "(lifecycle.task_acceptance: queue)"),
    ];
    for (project, extra, field) in cases {
        fs::write(&answer, STALE).unwrap();
        let mut args = vec!["--task", TASK, "--output", answer_arg];
        args.extend_from_slice(extra);
        let run = mur_run(&home, project, &args);
        run.assert_io_003(field);
        assert_eq!(run.status_lines(), ["status:  failed"], "{}", run.context());
        assert_no_trace(&home, &run);
        assert!(!answer.exists(), "{}", run.context());
    }
    assert!(server.requests().is_empty());
}

// ── A path that is not a file to write is refused ────────────────────────────

#[test]
fn s7_paths_that_are_not_files_to_write_are_refused_and_left_alone() {
    let home = home_with_driver();
    let server = common::ScriptedServer::start(vec![end_turn("msg_1", "never asked")]);
    let project = Project::new(&manifest("output-paths", &server.endpoint, "", ""));
    let scratch = TempDir::new().unwrap();

    // A directory, which keeps its file.
    let dir = scratch.path().join("a-dir");
    fs::create_dir(&dir).unwrap();
    fs::write(dir.join("inside"), "x").unwrap();
    let run = mur_run(
        &home,
        &project,
        &["--task", TASK, "--output", dir.to_str().unwrap()],
    );
    run.assert_io_003("is a directory");
    assert_eq!(fs::read(dir.join("inside")).unwrap(), b"x");
    assert_no_trace(&home, &run);

    // A missing parent, which is not created.
    let missing = scratch.path().join("missing");
    let run = mur_run(
        &home,
        &project,
        &[
            "--task",
            TASK,
            "--output",
            missing.join("a.txt").to_str().unwrap(),
        ],
    );
    run.assert_io_003("does not exist");
    assert!(!missing.exists());
    assert_no_trace(&home, &run);

    // A link to a file: link and target unchanged.
    let target = scratch.path().join("target.txt");
    fs::write(&target, "keep").unwrap();
    let link = scratch.path().join("link.txt");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let run = mur_run(
        &home,
        &project,
        &["--task", TASK, "--output", link.to_str().unwrap()],
    );
    run.assert_io_003("is a symbolic link");
    assert!(fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(fs::read_link(&link).unwrap(), target);
    assert_eq!(fs::read(&target).unwrap(), b"keep");
    assert_no_trace(&home, &run);

    // The --task file itself, which is left as it was.
    let task_file = scratch.path().join("task.md");
    fs::write(&task_file, "do the thing").unwrap();
    let task_arg = task_file.to_str().unwrap();
    let run = mur_run(&home, &project, &["--task", task_arg, "--output", task_arg]);
    run.assert_io_003("is the file --task names");
    assert_eq!(fs::read(&task_file).unwrap(), b"do the thing");
    assert_no_trace(&home, &run);

    // A character device, which stays one.
    let run = mur_run(&home, &project, &["--task", TASK, "--output", "/dev/null"]);
    run.assert_io_003("is not a regular file");
    use std::os::unix::fs::FileTypeExt;
    assert!(fs::symlink_metadata("/dev/null")
        .unwrap()
        .file_type()
        .is_char_device());
    assert_no_trace(&home, &run);

    assert!(server.requests().is_empty());
}

// ── Usage refusals touch nothing ─────────────────────────────────────────────

#[test]
fn s8_usage_refusals_exit_2_and_leave_the_stale_file() {
    let home = TempDir::new().unwrap();
    let scratch = TempDir::new().unwrap();
    let answer = scratch.path().join("answer.txt");
    fs::write(&answer, STALE).unwrap();
    let answer_arg = answer.to_str().unwrap();
    let roster = scratch.path().join("roster.yaml");

    let cases: [Vec<&str>; 3] = [
        vec!["run", "--output", answer_arg],
        vec![
            "run",
            "--roster",
            roster.to_str().unwrap(),
            "--task",
            TASK,
            "--output",
            answer_arg,
        ],
        vec![
            "run",
            "--explain-scope",
            "--task",
            TASK,
            "--output",
            answer_arg,
        ],
    ];
    for args in cases {
        let run = mur(&home, scratch.path(), &args);
        assert_eq!(run.code(), Some(2), "{args:?}\n{}", run.context());
        assert_eq!(fs::read_to_string(&answer).unwrap(), STALE, "{args:?}");
    }
}

// ── A script capsule's answer is the file it writes ──────────────────────────

#[test]
fn s9_a_script_capsule_writes_its_work_folder_answer() {
    let home = TempDir::new().unwrap();
    let project = Project::script();
    let out_dir = TempDir::new().unwrap();
    let answer = out_dir.path().join("answer.txt");

    let run = mur_run(
        &home,
        &project,
        &[
            "--task",
            "x",
            "--output",
            answer.to_str().unwrap(),
            "--no-env-file",
            "--verbose",
        ],
    );
    assert_eq!(run.code(), Some(0), "{}", run.context());
    assert_eq!(run.status_lines(), ["status:  ok"], "{}", run.context());
    let written = fs::read(&answer).unwrap();
    assert_eq!(
        written,
        b"GITHUB_TOKEN=absent\nMURMUR_TEST_ALLOWED_VAR=absent"
    );
    assert_eq!(
        written,
        fs::read(run.workdir().join("out/result.txt")).unwrap()
    );
}
