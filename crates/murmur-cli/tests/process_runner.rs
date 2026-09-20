//! The process driver runner, end to end: `mur run` on a `transport: process` capsule spawns the
//! harness its driver names, drives it through the driver, and records what happened.
//!
//! Every test publishes the fixture process driver, points `inference.command` (or `PATH`) at the
//! fixture fake harness, and reads back `out/result.txt` and `trace.jsonl`. The fake harness is a
//! bash script, so these tests are Unix-only — which every platform murmur targets is.

#![cfg(unix)]

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Output,
    time::{Duration, Instant},
};

use assert_cmd::Command;
use serde_json::Value;
use tempfile::TempDir;
use zip::{
    write::{FileOptions, SimpleFileOptions},
    CompressionMethod, ZipWriter,
};

const DRIVER: &str = "fixture-process-driver";
const DRIVER_VERSION: &str = "0.1.0";
const TOOL: &str = "echo-tool";

fn fixture_wasm() -> PathBuf {
    common::fixture_path("process-driver/tool/process-driver.wasm")
}

fn fake_harness_source() -> PathBuf {
    common::fixture_path("process-driver/fake-harness")
}

fn echo_tool_wasm() -> PathBuf {
    common::fixture_path("run/components/echo-tool.wasm")
}

/// Pack `wasm` as a `runtime: wasm` tool artifact, the shape `create_driver_artifact_with_auth`
/// produces for drivers.
fn create_tool_artifact(dir: &Path, name: &str, version: &str, wasm: &Path) -> PathBuf {
    let path = dir.join(format!("{name}-{version}.mur.zip"));
    let mut zip = ZipWriter::new(fs::File::create(&path).unwrap());
    let options: SimpleFileOptions =
        FileOptions::default().compression_method(CompressionMethod::Deflated);
    zip.start_file("murmur.yaml", options).unwrap();
    writeln!(zip, "name: {name}").unwrap();
    writeln!(zip, "version: {version}").unwrap();
    writeln!(zip, "runtime: wasm").unwrap();
    zip.start_file("tool.wasm", options).unwrap();
    zip.write_all(&fs::read(wasm).unwrap()).unwrap();
    zip.finish().unwrap();
    path
}

/// How a capsule reaches its harness binary.
enum HarnessBinary {
    /// `inference.command` names the copied script outright.
    Command,
    /// Nothing is declared, so `describe().binary` — `fixture-cli` — is looked up on `PATH`.
    OnPath,
    /// Nothing is declared and nothing is on `PATH`.
    Missing,
}

/// A published driver (and optionally a tool), a project, and a copy of the fake harness.
struct Capsule {
    home: TempDir,
    project: TempDir,
    _artifacts: TempDir,
    /// Prepended to `PATH` for the `mur` process, so `fixture-cli` is found there or nowhere.
    bin_dir: PathBuf,
    manifest: PathBuf,
}

struct Built {
    binary: HarnessBinary,
    tool: bool,
    config: Option<String>,
    max_turns: Option<u32>,
    model: Option<String>,
}

impl Built {
    /// A capsule whose harness is the fixture script, reached through `inference.command`.
    fn new() -> Self {
        Self {
            binary: HarnessBinary::Command,
            tool: false,
            config: None,
            max_turns: None,
            model: None,
        }
    }

    fn binary(mut self, binary: HarnessBinary) -> Self {
        self.binary = binary;
        self
    }

    fn with_tool(mut self) -> Self {
        self.tool = true;
        self
    }

    fn config(mut self, config: &str) -> Self {
        self.config = Some(config.to_string());
        self
    }

    fn max_turns(mut self, turns: u32) -> Self {
        self.max_turns = Some(turns);
        self
    }

    fn build(self) -> Capsule {
        let home = TempDir::new().unwrap();
        let artifacts = TempDir::new().unwrap();
        let project = TempDir::new().unwrap();

        let driver = common::create_driver_artifact_with_auth(
            artifacts.path(),
            DRIVER,
            DRIVER_VERSION,
            &fixture_wasm(),
            "",
        );
        common::publish_local(&home, &driver).success();

        let mut entries =
            format!("  - name: {DRIVER}\n    version: {DRIVER_VERSION}\n    runtime: driver\n");
        if self.tool {
            let tool =
                create_tool_artifact(artifacts.path(), TOOL, DRIVER_VERSION, &echo_tool_wasm());
            common::publish_local(&home, &tool).success();
            entries.push_str(&format!(
                "  - name: {TOOL}\n    version: {DRIVER_VERSION}\n    runtime: tool\n"
            ));
        }

        // Somewhere only this capsule's `PATH` points at, so `fixture-cli` is resolvable exactly
        // when a test says it should be.
        let bin_dir = project.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let script = match self.binary {
            HarnessBinary::OnPath => Some(bin_dir.join("fixture-cli")),
            HarnessBinary::Command => Some(project.path().join("fake-harness")),
            HarnessBinary::Missing => None,
        };
        if let Some(script) = script.as_ref() {
            fs::copy(fake_harness_source(), script).unwrap();
            let mut perms = fs::metadata(script).unwrap().permissions();
            std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
            fs::set_permissions(script, perms).unwrap();
        }

        let command = match self.binary {
            HarnessBinary::Command => format!(
                "  command: {}\n",
                script.as_ref().unwrap().to_str().unwrap()
            ),
            HarnessBinary::OnPath | HarnessBinary::Missing => String::new(),
        };
        let model = self
            .model
            .map(|m| format!("  model: {m}\n"))
            .unwrap_or_default();
        let max_turns = self
            .max_turns
            .map(|t| format!("  max_turns: {t}\n"))
            .unwrap_or_default();
        let config = self
            .config
            .map(|c| format!("    config:\n      mode: {c}\n"))
            .unwrap_or_default();

        let manifest = project.path().join("murmur.yaml");
        fs::write(
            &manifest,
            format!(
                "name: process-runner\nversion: 0.1.0\nartifacts:\n{entries}\
                 capabilities:\n  env:\n    allow: [HOME, PATH, FIXTURE_HARNESS_PROFILE]\n\
                 inference:\n  transport: process\n  driver:\n    artifact: {DRIVER}\n{config}\
                 {command}{model}{max_turns}"
            ),
        )
        .unwrap();

        Capsule {
            home,
            project,
            _artifacts: artifacts,
            bin_dir,
            manifest,
        }
    }
}

/// One `mur run`, with the harness profile and whatever else a test needs in the environment.
struct Run {
    output: Output,
    text: String,
    workdir: PathBuf,
}

impl Capsule {
    fn run(&self, profile: &str, extra_env: &[(&str, &str)]) -> Run {
        let mut command = Command::cargo_bin("mur").unwrap();
        command
            .env_clear()
            .env("HOME", self.home.path().to_str().unwrap())
            .env("PATH", self.path_value())
            .env("FIXTURE_HARNESS_PROFILE", profile)
            .current_dir(self.project.path());
        for (key, value) in extra_env {
            command.env(key, value);
        }
        let output = command
            .args([
                "run",
                "--manifest",
                self.manifest.to_str().unwrap(),
                "--task",
                "do the thing",
                "--verbose",
            ])
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let workdir = if text.contains("workdir: ") {
            common::parse_workdir_from_stdout(&text)
        } else {
            self.project.path().to_path_buf()
        };
        Run {
            output,
            text,
            workdir,
        }
    }

    /// The bin directory first, so a `fixture-cli` placed there wins, then the host's own `PATH`
    /// so the fake harness's `sleep` / `env` / `ls` resolve.
    fn path_value(&self) -> String {
        format!(
            "{}:{}",
            self.bin_dir.display(),
            std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_string())
        )
    }

    fn trace_show(&self, workdir: &Path) -> String {
        let output = Command::cargo_bin("mur")
            .unwrap()
            .env("HOME", self.home.path())
            .current_dir(self.project.path())
            .args([
                "trace",
                "show",
                workdir.join("trace.jsonl").to_str().unwrap(),
            ])
            .output()
            .unwrap();
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    }
}

impl Run {
    fn succeeded(self) -> Self {
        assert!(
            self.output.status.success(),
            "mur run failed:\n{}",
            self.text
        );
        self
    }

    fn failed(self) -> Self {
        assert!(
            !self.output.status.success(),
            "mur run succeeded:\n{}",
            self.text
        );
        self
    }

    fn result(&self) -> String {
        fs::read_to_string(self.workdir.join("out").join("result.txt")).unwrap_or_default()
    }

    fn trace_raw(&self) -> String {
        fs::read_to_string(self.workdir.join("trace.jsonl")).unwrap_or_default()
    }

    fn events(&self) -> Vec<Value> {
        self.trace_raw()
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn of_type(&self, event_type: &str) -> Vec<Value> {
        self.events()
            .into_iter()
            .filter(|event| event["event_type"] == event_type)
            .collect()
    }

    fn one(&self, event_type: &str) -> Value {
        let mut found = self.of_type(event_type);
        assert_eq!(
            found.len(),
            1,
            "expected exactly one {event_type}, got {found:#?}\n{}",
            self.text
        );
        found.remove(0)
    }

    /// The pid a profile wrote for a test that has to prove it dead.
    fn harness_pid(&self) -> i32 {
        let path = common::find_file(&self.workdir, "harness.pid")
            .unwrap_or_else(|| panic!("the profile wrote no harness.pid under {:?}", self.workdir));
        fs::read_to_string(path).unwrap().trim().parse().unwrap()
    }
}

// ── S1: the happy path ────────────────────────────────────────────────────────

#[test]
fn s1_a_process_capsule_runs_its_harness_and_records_one_turn() {
    println!("S1: `mur run` drives the harness through its driver and records the run");
    let capsule = Built::new().build();
    let run = capsule.run("happy", &[]).succeeded();

    assert_eq!(run.result(), "HAPPY-RESULT");

    let start = run.one("harness_start");
    assert_eq!(start["driver"], DRIVER);
    assert_eq!(start["driver_version"], DRIVER_VERSION);
    assert_eq!(start["harness"], "fixture-harness");
    assert_eq!(start["harness_version"], "fixture-harness 1.0.0");
    assert_eq!(start["version_tested"], true);
    assert_eq!(start["session_mode"], "new");
    assert_eq!(start["binary_source"], "inference.command");
    assert!(start["bridge_tools"].as_array().unwrap().is_empty());

    let session = run.one("harness_session");
    assert_eq!(session["harness_session_id"], "fixture-session-1");
    assert_eq!(session["auth"], "subscription");

    let inference = run.one("inference");
    assert_eq!(inference["decision"], "end_turn");

    let exit = run.one("harness_exit");
    assert_eq!(exit["cause"], "terminal");
    assert_eq!(exit["code"], 0);

    assert!(run.of_type("harness_warning").is_empty(), "{}", run.text);
    assert!(!run.text.contains("warning[W-RUN-002]"), "{}", run.text);
    assert!(!run.text.contains("warning[W-SEC-031]"), "{}", run.text);
}

// ── S2: the binary the driver names ───────────────────────────────────────────

#[test]
fn s2_the_binary_comes_from_describe_when_the_manifest_names_none() {
    println!("S2: with no inference.command the driver's describe() binary is found on PATH");
    let capsule = Built::new().binary(HarnessBinary::OnPath).build();
    let run = capsule.run("happy", &[]).succeeded();

    let start = run.one("harness_start");
    assert_eq!(start["binary_source"], "the process driver's describe()");
    let binary = start["binary"].as_str().unwrap();
    assert!(binary.starts_with('/'), "{binary} is not absolute");
    assert!(binary.ends_with("/fixture-cli"), "{binary}");
    assert_eq!(run.result(), "HAPPY-RESULT");
}

#[test]
fn s2_a_binary_that_is_not_installed_is_refused() {
    println!("S2: with nothing named and nothing on PATH the launch is refused at E-RUN-006");
    let capsule = Built::new().binary(HarnessBinary::Missing).build();
    let run = capsule.run("happy", &[]).failed();

    assert!(run.text.contains("E-RUN-006"), "{}", run.text);
    assert!(run.text.contains("fixture-cli"), "{}", run.text);
    assert!(run.text.contains("describe"), "{}", run.text);
    // The refusal and its hint, alone: other lines carry doc links of their own.
    let refusal: String = run
        .text
        .lines()
        .skip_while(|line| !line.contains("E-RUN-006"))
        .take(2)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !refusal.contains("http"),
        "no install URL for any particular harness:\n{refusal}"
    );
}

// ── S3: a bridged tool call ───────────────────────────────────────────────────

#[test]
fn s3_the_harness_calls_a_capsule_tool_through_the_bridge() {
    println!("S3: a bridged tool call is executed by murmur and recorded under its bare name");
    let capsule = Built::new().with_tool().build();
    let run = capsule.run("tool", &[]).succeeded();

    assert!(run.result().contains("ping-7"), "{}", run.result());
    assert!(
        run.result().contains("\"isError\":false"),
        "{}",
        run.result()
    );

    let call = run.one("tool_call");
    assert_eq!(call["tool_name"], TOOL, "the name is recorded bare");
    assert_eq!(call["tool_call_id"], "c1");
    assert_eq!(call["status"], "ok");
    assert!(call["duration_ms"].is_number());

    let inferences = run.of_type("inference");
    assert_eq!(inferences.len(), 2, "{inferences:#?}");
    assert_eq!(inferences[0]["decision"], "tool_call");
    assert_eq!(inferences[1]["decision"], "end_turn");

    let start = run.one("harness_start");
    assert_eq!(start["bridge_tools"], serde_json::json!([TOOL]));

    let shown = capsule.trace_show(&run.workdir);
    assert!(shown.contains(TOOL), "{shown}");
    assert!(shown.contains("ok"), "{shown}");
}

// ── S4: every failure kind ────────────────────────────────────────────────────

#[test]
fn s4_a_failed_turn_fails_the_run_and_names_its_kind() {
    println!("S4: each failure-kind the harness reports comes back as E-RUN-033");
    for kind in [
        "auth",
        "quota",
        "max-turns",
        "canceled",
        "harness-error",
        "other",
    ] {
        let capsule = Built::new().build();
        let run = capsule.run(&format!("fail-{kind}"), &[]).failed();
        println!("  {kind}: exit {:?}", run.output.status.code());
        assert!(run.text.contains("E-RUN-033"), "{kind}: {}", run.text);
        assert!(run.text.contains(kind), "{kind}: {}", run.text);
        assert!(
            run.text.contains("the harness says so"),
            "{kind}: {}",
            run.text
        );
        let failed = run.one("harness_failed");
        assert_eq!(failed["kind"], kind);
        assert_eq!(failed["source"], "harness");
        if kind == "auth" {
            let shown = capsule.trace_show(&run.workdir);
            assert!(
                shown.contains("harness failed (auth): the harness says so"),
                "{shown}"
            );
        }
    }
}

// ── S5: stdout ends without a terminal event ──────────────────────────────────

#[test]
fn s5_an_exit_without_a_terminal_event_is_classified_by_the_driver() {
    println!("S5: EOF with no terminal event is handed to classify-exit");
    let capsule = Built::new().build();
    let run = capsule.run("exit-0", &[]).succeeded();
    assert_eq!(run.one("harness_exit")["cause"], "eof");
    assert_eq!(run.one("harness_exit")["code"], 0);

    let capsule = Built::new().build();
    let run = capsule.run("exit-3", &[]).failed();
    assert!(run.text.contains("E-RUN-033"), "{}", run.text);
    assert!(run.text.contains("harness-error"), "{}", run.text);
    assert!(run.text.contains("boom-on-stderr"), "{}", run.text);
    assert_eq!(run.one("harness_exit")["cause"], "eof");
    assert_eq!(run.one("harness_exit")["code"], 3);
}

// ── S6: inactivity and the exit grace ─────────────────────────────────────────

/// 1.5s of silence, so the three profiles below can be told apart in seconds.
const DEBUG_INACTIVITY_MS: &str = "1500";

#[test]
fn s6_a_silent_harness_is_killed_and_a_busy_one_is_not() {
    println!("S6: the inactivity window kills a silent harness and never a working one");
    let debug = [("MURMUR_DEBUG_PROCESS_INACTIVITY_MS", DEBUG_INACTIVITY_MS)];

    let capsule = Built::new().build();
    let started = Instant::now();
    let run = capsule.run("silent", &debug).failed();
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "the run should have been killed at the window, took {:?}",
        started.elapsed()
    );
    assert!(run.text.contains("E-RUN-035"), "{}", run.text);
    common::assert_dead_within(run.harness_pid(), Duration::from_secs(5));
    let exit = run.one("harness_exit");
    assert_eq!(exit["cause"], "inactivity");
    assert_eq!(exit["killed"], true);

    // Three seconds of output, in half-second steps: each line resets the window.
    let capsule = Built::new().build();
    let run = capsule.run("chatty", &debug).succeeded();
    assert_eq!(run.result(), "CHATTY-RESULT");
    assert_eq!(run.one("harness_exit")["cause"], "terminal");

    // The same again with nothing on stdout: only bridge traffic keeps it alive.
    let capsule = Built::new().with_tool().build();
    let run = capsule.run("bridge-busy", &debug).succeeded();
    assert_eq!(run.result(), "BRIDGE-BUSY-RESULT");
    assert_eq!(run.one("harness_exit")["cause"], "terminal");
}

#[test]
fn s6_a_harness_that_lingers_after_its_terminal_event_is_killed() {
    println!("S6: a harness that ignores stdin closing is killed after the exit grace");
    let capsule = Built::new().build();
    let started = Instant::now();
    let run = capsule.run("linger", &[]).succeeded();
    assert_eq!(run.result(), "LINGER");
    // Well inside the 60s the profile sleeps for: the run ended at the grace, not at the sleep.
    // The bound covers a whole debug-build `mur run`, staging and component compilation included.
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "the grace kill should have ended it, took {:?}",
        started.elapsed()
    );
    common::assert_dead_within(run.harness_pid(), Duration::from_secs(5));
    let exit = run.one("harness_exit");
    assert_eq!(exit["cause"], "terminal");
    assert_eq!(exit["killed"], true);
}

// ── S7: exactly the declared environment ──────────────────────────────────────

#[test]
fn s7_the_harness_sees_exactly_what_was_declared_and_set() {
    println!("S7: the harness environment is the allowlist plus the plan's env-set, nothing else");
    let capsule = Built::new().with_tool().build();
    let run = capsule
        .run(
            "env",
            &[("FIXTURE_UNDECLARED_SENTINEL", "must-not-reach-the-harness")],
        )
        .succeeded();

    let notes: Vec<String> = run
        .of_type("harness_note")
        .into_iter()
        .map(|note| note["text"].as_str().unwrap().to_string())
        .collect();
    let env_note = notes
        .iter()
        .find(|note| note.starts_with("env "))
        .unwrap_or_else(|| panic!("no env note in {notes:#?}"));
    // bash sets PWD, SHLVL, _ and OLDPWD itself; they are not inherited and not the runtime's.
    let mut names: Vec<&str> = env_note["env ".len()..]
        .split(',')
        .filter(|name| !name.is_empty())
        .filter(|name| !matches!(*name, "PWD" | "SHLVL" | "_" | "OLDPWD"))
        .collect();
    names.sort_unstable();
    assert_eq!(
        names.join(","),
        "FIXTURE_BRIDGE_TOKEN,FIXTURE_BRIDGE_TOOLS,FIXTURE_BRIDGE_URL,FIXTURE_FILES,\
         FIXTURE_HARNESS_PROFILE,HOME,PATH",
        "{env_note}"
    );
    assert!(
        !env_note.contains("FIXTURE_UNDECLARED_SENTINEL"),
        "{env_note}"
    );

    let files_note = notes
        .iter()
        .find(|note| note.starts_with("files "))
        .unwrap_or_else(|| panic!("no files note in {notes:#?}"));
    let mut parts = files_note.split_whitespace();
    parts.next();
    assert_eq!(parts.next(), Some("drwx------"), "{files_note}");
    let files_dir = PathBuf::from(parts.next().unwrap());
    assert!(
        files_dir.starts_with(&run.workdir),
        "{files_dir:?} is not under {:?}",
        run.workdir
    );
    assert!(
        !files_dir.exists(),
        "the run's files directory outlived the run: {files_dir:?}"
    );

    let start = run.one("harness_start");
    assert_eq!(
        start["env_names"],
        serde_json::json!([
            "FIXTURE_BRIDGE_TOKEN",
            "FIXTURE_BRIDGE_TOOLS",
            "FIXTURE_BRIDGE_URL",
            "FIXTURE_FILES",
            "FIXTURE_HARNESS_PROFILE",
            "HOME",
            "PATH"
        ])
    );
    assert_eq!(start["files"], serde_json::json!(["config.json"]));

    // The bridge's bearer token reaches the harness through an environment value, and no
    // environment value is ever recorded.
    let trace = run.trace_raw();
    assert!(!trace.contains("bearer"), "a token shape reached the trace");
    assert!(
        !trace.contains("FIXTURE_BRIDGE_TOKEN=") && !trace.contains("must-not-reach-the-harness"),
        "an environment value reached the trace"
    );
}

// ── S8: an untested harness version ───────────────────────────────────────────

#[test]
fn s8_an_untested_harness_version_warns_and_runs() {
    println!("S8: an untested harness version is W-RUN-002 and nothing more");
    let capsule = Built::new().build();
    let run = capsule.run("old-version", &[]).succeeded();

    assert_eq!(run.result(), "OLD-VERSION-RESULT");
    assert!(run.text.contains("warning[W-RUN-002]"), "{}", run.text);
    assert!(run.text.contains("0.9.0"), "{}", run.text);
    assert!(run.text.contains("1.0.0"), "{}", run.text);

    let start = run.one("harness_start");
    assert_eq!(start["version_tested"], false);
    assert_eq!(start["harness_version"], "fixture-harness 0.9.0");

    let warning = run.one("harness_warning");
    assert_eq!(warning["code"], "W-RUN-002");

    let shown = capsule.trace_show(&run.workdir);
    assert!(shown.contains("warning W-RUN-002"), "{shown}");
}

// ── S9: the attempt's turn budget ─────────────────────────────────────────────

#[test]
fn s9_a_turn_past_max_turns_kills_the_harness() {
    println!("S9: opening a turn past inference.max_turns ends the run at once");
    let capsule = Built::new().max_turns(2).build();
    let run = capsule.run("turns", &[]).failed();

    assert!(run.text.contains("E-RUN-033"), "{}", run.text);
    assert!(run.text.contains("max-turns"), "{}", run.text);
    let failed = run.one("harness_failed");
    assert_eq!(failed["kind"], "max-turns");
    assert_eq!(failed["source"], "runtime");
    assert_eq!(run.of_type("inference").len(), 2);
    assert_eq!(run.one("harness_exit")["cause"], "max_turns");
    common::assert_dead_within(run.harness_pid(), Duration::from_secs(5));
}

#[test]
fn s9_thinking_does_not_spend_a_turn() {
    println!("S9: thinking and deltas do not open a turn, so one turn is enough");
    let capsule = Built::new().max_turns(1).build();
    let run = capsule.run("thinking", &[]).succeeded();
    assert_eq!(run.result(), "THINKING-RESULT");
    assert_eq!(run.of_type("inference").len(), 1);
}

// ── S10: a harness billing to an API key ──────────────────────────────────────

#[test]
fn s10_a_non_subscription_harness_session_warns_and_runs() {
    println!("S10: a harness session that is not a subscription is W-SEC-031 and nothing more");
    let capsule = Built::new().build();
    let run = capsule.run("api-key", &[]).succeeded();

    assert_eq!(run.result(), "API-KEY-RESULT");
    assert!(run.text.contains("warning[W-SEC-031]"), "{}", run.text);
    assert!(run.text.contains("api-key"), "{}", run.text);

    let warning = run.one("harness_warning");
    assert_eq!(warning["code"], "W-SEC-031");
    assert_eq!(run.one("harness_session")["auth"], "api-key");
}

// ── S11: the manifest rule ────────────────────────────────────────────────────

#[test]
fn s11_a_process_manifest_without_a_driver_is_refused() {
    println!("S11: a process manifest that names no driver is refused at parse");
    for inference in [
        "inference:\n  transport: process\n  command: some-cli\n",
        "inference:\n  transport: process\n  model: some-model\n",
    ] {
        let home = TempDir::new().unwrap();
        let project = TempDir::new().unwrap();
        let manifest = project.path().join("murmur.yaml");
        fs::write(
            &manifest,
            format!("name: no-driver\nversion: 0.1.0\nartifacts: []\n{inference}"),
        )
        .unwrap();
        let output = Command::cargo_bin("mur")
            .unwrap()
            .env("HOME", home.path())
            .env_remove("NEXUS_API_KEY")
            .current_dir(project.path())
            .args([
                "run",
                "--manifest",
                manifest.to_str().unwrap(),
                "--task",
                "hi",
            ])
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!output.status.success(), "{text}");
        assert!(text.contains("E-MAN-003"), "{text}");
        assert!(text.contains("inference.driver.artifact"), "{text}");
        assert!(text.contains("murmur-driver-claude-code"), "{text}");
    }
}

// ── S12: the driver refuses to launch ─────────────────────────────────────────

#[test]
fn s12_a_driver_that_refuses_to_launch_fails_the_run_before_anything_is_spawned() {
    println!("S12: a launch the driver refuses is E-RUN-034 and no harness_start");
    let capsule = Built::new().config("refuse-launch").build();
    let run = capsule.run("happy", &[]).failed();

    assert!(run.text.contains("E-RUN-034"), "{}", run.text);
    assert!(run.text.contains(DRIVER), "{}", run.text);
    assert!(run.text.contains("launch"), "{}", run.text);
    assert!(
        run.text.contains("launch refused by config"),
        "{}",
        run.text
    );
    assert!(run.of_type("harness_start").is_empty());
    assert!(run.of_type("harness_exit").is_empty());
}
