//! Code mode end to end: the capsule `docs/content/how-to/use-code-mode.md` shows, run by the real
//! `mur` against a scripted model with the real `tool-repl-js` (vendored in
//! `fixtures/tool-repl-js/`). Five programs in one session keep a value across calls, list the
//! capsule's operations, call two tools in one batch through `murmur:bus`, run a host program
//! through `murmur:process`, and outrun `code_mode.detach_after_secs` so that
//! `background-command wait` returns what the program printed. The same capsule without its
//! `code_mode` block is refused at launch.
//!
//! The page is read at test time: its manifest becomes the run's, its JavaScript block is one of
//! the programs run, and its `mur trace show` excerpt is found in the run's own.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde_json::{json, Value};
use tempfile::TempDir;

const ANTHROPIC: &str = "local/murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.0";
const REPL: &str = "local/tool-repl-js";
/// The name the model calls the REPL by: its artifact's bare name.
const REPL_TOOL: &str = "tool-repl-js";
const AUTH: &str = "upstream_auth:\n  header: x-api-key\n  value: \"{key}\"\n";
const KEY: &str = "sk-code-mode-e2e-canary-93f1";
const WAIT: Duration = Duration::from_secs(240);
const END_TURN: &str = r#"{"id":"msg_end","type":"message","role":"assistant","model":"test-model","content":[{"type":"text","text":"done"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#;

/// Lists the operations a program may call and keeps their names for later calls.
const P1: &str = r#"globalThis.ops = tools().map((op) => op.name);
print(ops.join(" "));"#;

/// Reads the value P1 left in the instance.
const P2: &str = r#"print("ops:", ops.length, ops.includes("echo"));"#;

/// Calls two tools in one batch and prints a short summary: the how-to's program.
const P3: &str = r#"const [said, status] = callAll([
  ["echo", { command: "hello from the bus" }],
  ["session-status", {}],
]);
print(said.status, status.status, said.data.includes("hello from the bus"));"#;

/// Runs a host program through `murmur:process`.
const P4: &str = r#"const uname = run("uname", ["-s"]);
print(uname.exitCode, uname.stdout.trim());"#;

/// Runs past `detach_after_secs` (10) on a wall-clock loop, with no host call.
const P5: &str = r#"const started = Date.now();
while (Date.now() - started < 12000) {}
print("still here:", ops.length);"#;

const PROGRAMS: [&str; 5] = [P1, P2, P3, P4, P5];

fn mur() -> PathBuf {
    assert_cmd::cargo::cargo_bin("mur")
}

fn fixture(file: &str) -> PathBuf {
    common::fixture_path(&format!("tool-repl-js/{file}"))
}

/// The vendored `tool-repl-js` version, from its own `murmur.yaml`.
fn repl_version() -> String {
    common::manifest_field(
        &fs::read_to_string(fixture("murmur.yaml")).unwrap(),
        "version",
    )
}

fn how_to() -> String {
    let page =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/content/how-to/use-code-mode.md");
    fs::read_to_string(&page).unwrap_or_else(|e| panic!("{}: {e}", page.display()))
}

/// The body of every ```` ```<lang> ```` fence on `page`, without its trailing newline.
fn fences(page: &str, lang: &str) -> Vec<String> {
    let open = format!("```{lang}");
    let mut found = Vec::new();
    let mut body: Option<Vec<&str>> = None;
    for line in page.lines() {
        match body.as_mut() {
            Some(lines) if line.trim_start().starts_with("```") => {
                found.push(lines.join("\n"));
                body = None;
            }
            Some(lines) => lines.push(line),
            None if line.trim_end() == open => body = Some(Vec::new()),
            None => {}
        }
    }
    found
}

/// The page's one YAML fence holding `code_mode:`.
fn page_manifest(page: &str) -> String {
    let yaml: Vec<String> = fences(page, "yaml")
        .into_iter()
        .filter(|fence| fence.lines().any(|line| line.starts_with("code_mode:")))
        .collect();
    assert_eq!(yaml.len(), 1, "one yaml fence with code_mode: on the page");
    yaml.into_iter().next().unwrap()
}

/// The page's manifest made runnable here: each reader-facing value swapped for this test's, each
/// matched exactly once.
fn capsule_manifest(page: &str, endpoint: &str) -> String {
    let mut manifest = page_manifest(page);
    let table = [
        (
            "version: \"{{ v.murmur_driver_anthropic }}\"".to_string(),
            format!("version: \"{DRIVER_VERSION}\""),
        ),
        (
            "name: murmur/driver-anthropic".to_string(),
            format!("name: {ANTHROPIC}"),
        ),
        (
            "artifact: murmur/driver-anthropic".to_string(),
            format!("artifact: {ANTHROPIC}"),
        ),
        (
            "endpoint: https://api.anthropic.com".to_string(),
            format!("endpoint: {endpoint}"),
        ),
        (
            "api_key: ${ANTHROPIC_API_KEY}".to_string(),
            format!("api_key: {KEY}"),
        ),
        (
            "name: murmur/tool-repl-js".to_string(),
            format!("name: {REPL}"),
        ),
        (
            "repl: murmur/tool-repl-js".to_string(),
            format!("repl: {REPL}"),
        ),
        (
            "version: \"{{ v.murmur_tool_repl_js }}\"".to_string(),
            format!("version: \"{}\"", repl_version()),
        ),
        (
            "model: {{ v.model_anthropic }}".to_string(),
            "model: test-model".to_string(),
        ),
    ];
    for (from, to) in &table {
        assert_eq!(
            manifest.matches(from.as_str()).count(),
            1,
            "`{from}` appears once in the page's manifest:\n{manifest}"
        );
        manifest = manifest.replace(from.as_str(), to);
    }
    assert!(
        !manifest.contains("{{") && !manifest.contains("murmur/") && !manifest.contains("${"),
        "the table leaves nothing of the reader's values:\n{manifest}"
    );
    manifest
}

/// `manifest` with its top-level `code_mode:` block removed.
fn without_code_mode(manifest: &str) -> String {
    let mut kept = Vec::new();
    let mut in_block = false;
    for line in manifest.lines() {
        if line.starts_with("code_mode:") {
            in_block = true;
            continue;
        }
        if in_block && (line.starts_with(' ') || line.is_empty()) {
            continue;
        }
        in_block = false;
        kept.push(line);
    }
    let stripped = kept.join("\n") + "\n";
    assert!(!stripped.contains("code_mode") && !stripped.contains("repl:"));
    stripped
}

/// `text` with each duration (`\d+(\.\d+)?(ms|s)`) replaced by one token.
fn normalize_durations(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_ascii_digit() {
            let mut j = i;
            while j < chars.len() && chars[j].is_ascii_digit() {
                j += 1;
            }
            if j + 1 < chars.len() && chars[j] == '.' && chars[j + 1].is_ascii_digit() {
                j += 1;
                while j < chars.len() && chars[j].is_ascii_digit() {
                    j += 1;
                }
            }
            let unit = if chars.get(j) == Some(&'m') && chars.get(j + 1) == Some(&'s') {
                2
            } else if chars.get(j) == Some(&'s') {
                1
            } else {
                0
            };
            if unit > 0 {
                out.push_str("<dur>");
                i = j + unit;
            } else {
                out.extend(&chars[i..j]);
                i = j;
            }
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// The page's trace excerpt: its one ```` ```text ```` fence holding `↳` lines.
fn page_trace_excerpt(page: &str) -> Vec<String> {
    let blocks: Vec<String> = fences(page, "text")
        .into_iter()
        .filter(|fence| fence.contains('↳'))
        .collect();
    assert_eq!(blocks.len(), 1, "one text fence with ↳ lines on the page");
    blocks[0].lines().map(str::to_string).collect()
}

/// A scratch `HOME` with the fixture driver and `tool-repl-js` published, precompiled when
/// `precompile` is set.
struct Home {
    home: TempDir,
}

impl Home {
    fn new(precompile: bool) -> Self {
        let home = TempDir::new().unwrap();
        let artifacts = TempDir::new().unwrap();
        let published = [
            common::create_driver_artifact_with_auth(
                artifacts.path(),
                ANTHROPIC,
                DRIVER_VERSION,
                &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
                &format!("{AUTH}{}", common::FIXTURE_DRIVER_MODELS),
            ),
            common::create_vendored_artifact(
                artifacts.path(),
                &fixture("murmur.yaml"),
                &fixture("tool_repl_js.wasm"),
            ),
        ];
        for artifact in &published {
            common::publish_local(&home, artifact).success();
            if precompile {
                let out = Command::new(mur())
                    .env("HOME", home.path())
                    .arg("precompile")
                    .arg(artifact)
                    .output()
                    .unwrap();
                assert!(out.status.success(), "precompile: {out:?}");
            }
        }
        Self { home }
    }

    fn project(&self, manifest_yaml: &str) -> Project {
        let project = TempDir::new().unwrap();
        let manifest = project.path().join("murmur.yaml");
        fs::write(&manifest, manifest_yaml).unwrap();
        Project { project, manifest }
    }

    fn run(&self, project: &Project) -> Run {
        let mut child = Command::new(mur())
            .env("HOME", self.home.path())
            .current_dir(project.project.path())
            .args(["run", "--manifest", project.manifest.to_str().unwrap()])
            .args(["--task", "Use code mode.", "--verbose"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let started = Instant::now();
        while child.try_wait().unwrap().is_none() {
            if started.elapsed() > WAIT {
                let _ = child.kill();
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        Run::of(child.wait_with_output().unwrap())
    }

    fn doctor(&self, project: &Project) -> Run {
        Run::of(
            Command::new(mur())
                .env("HOME", self.home.path())
                .current_dir(project.project.path())
                .arg("doctor")
                .output()
                .unwrap(),
        )
    }

    fn trace_show(&self, project: &Project, session: &str) -> Run {
        Run::of(
            Command::new(mur())
                .env("HOME", self.home.path())
                .current_dir(project.project.path())
                .args(["trace", "show", session])
                .output()
                .unwrap(),
        )
    }
}

struct Project {
    project: TempDir,
    manifest: PathBuf,
}

struct Run {
    output: Output,
    stdout: String,
    stderr: String,
}

impl Run {
    fn of(output: Output) -> Self {
        Self {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            output,
        }
    }

    fn session(&self) -> String {
        self.stdout
            .lines()
            .find_map(|line| line.strip_prefix("session: "))
            .map(|id| id.trim().to_string())
            .unwrap_or_else(|| panic!("no session line; {}", self.context()))
    }

    fn trace(&self) -> Vec<Value> {
        let trace = common::parse_records_from_stdout(&self.stdout).join("trace.jsonl");
        assert!(trace.is_file(), "no trace.jsonl; {}", self.context());
        common::read_whole_trace(&trace)
    }

    fn context(&self) -> String {
        format!("stdout:\n{}\nstderr:\n{}", self.stdout, self.stderr)
    }
}

type Step = Box<dyn FnMut(&Value) -> String + Send>;

/// A scripted model: each step answers one request, and [`END_TURN`] the one after the last.
struct Model {
    server: common::ScriptedServer,
}

impl Model {
    fn start(steps: Vec<Step>) -> Self {
        let limit = steps.len() + 1;
        let mut steps = steps.into_iter();
        let server =
            common::ScriptedServer::start_answering(limit, move |request| match steps.next() {
                Some(mut step) => step(request),
                None => END_TURN.to_string(),
            });
        Self { server }
    }

    fn requests(&self) -> Vec<Value> {
        self.server.requests()
    }

    /// The unfenced text of the result of tool call `id`.
    fn result(&self, id: &str) -> String {
        common::tool_result_text(&self.requests(), id)
            .unwrap_or_else(|| panic!("no result for {id} in {:#?}", self.requests()))
    }
}

/// Tool call `id`: `program` run by the REPL.
fn program(id: &'static str, code: &'static str) -> Step {
    Box::new(move |_| common::tool_use_response(id, REPL_TOOL, json!({ "code": code })))
}

/// A `background-command wait` call `id` on the work id tool call `of` returned.
fn wait_for(id: &'static str, of: &'static str) -> Step {
    Box::new(move |request| {
        let text = common::tool_result_text(std::slice::from_ref(request), of)
            .unwrap_or_else(|| panic!("no result for {of} in {request:#}"));
        common::tool_use_response(
            id,
            "background-command",
            json!({"operation": "wait", "work_id": work_id(&text)}),
        )
    })
}

/// The `wrk_…` id in `text`.
fn work_id(text: &str) -> String {
    let start = text
        .find("wrk_")
        .unwrap_or_else(|| panic!("no work id in {text}"));
    text[start..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect()
}

fn records_of<'a>(trace: &'a [Value], event_type: &str) -> Vec<&'a Value> {
    trace
        .iter()
        .filter(|event| event["event_type"] == event_type)
        .collect()
}

fn index_of(trace: &[Value], pred: impl Fn(&Value) -> bool) -> usize {
    trace
        .iter()
        .position(pred)
        .unwrap_or_else(|| panic!("no matching record in {trace:#?}"))
}

/// The index of the model's own `tool_call` record for call `id`.
fn model_call(trace: &[Value], id: &str) -> usize {
    index_of(trace, |e| {
        e["event_type"] == "tool_call" && e["tool_call_id"] == id
    })
}

/// The switch, the bus and the REPL in one session — five programs, the records they
/// write, and the page's trace excerpt in the run's `mur trace show`.
#[test]
fn code_mode_runs_the_how_to_capsule_end_to_end() {
    if common::skip_without_host_support("code_mode_runs_the_how_to_capsule_end_to_end") {
        return;
    }
    let page = how_to();
    let home = Home::new(true);
    let model = Model::start(vec![
        program("t1", P1),
        program("t2", P2),
        program("t3", P3),
        program("t4", P4),
        program("t5", P5),
        wait_for("t6", "t5"),
    ]);
    let project = home.project(&capsule_manifest(&page, &model.server.endpoint));
    let run = home.run(&project);
    assert!(run.output.status.success(), "{}", run.context());

    let requests = model.requests();
    assert_eq!(requests.len(), 7, "{requests:#?}");
    let mut offered: Vec<&str> = requests[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    offered.sort_unstable();
    assert_eq!(offered, ["background-command", REPL_TOOL]);

    for id in ["t1", "t2", "t3", "t4"] {
        let result = model.result(id);
        assert!(
            !result.contains("wrk_"),
            "{id} returned in the foreground: {result}"
        );
    }

    let listed = model.result("t1");
    let names: Vec<&str> = listed.split_whitespace().collect();
    for present in ["echo", "uname", "session-status", "background-command"] {
        assert!(names.contains(&present), "{present} not in {names:?}");
    }
    for absent in [REPL_TOOL, "pull-artifact", "remove-artifact", "submit-plan"] {
        assert!(!names.contains(&absent), "{absent} in {names:?}");
    }
    assert_eq!(
        model.result("t2").trim_end(),
        format!("ops: {} true", names.len())
    );
    assert_eq!(model.result("t3").trim_end(), "passed passed true");
    assert_eq!(model.result("t4").trim_end(), "0 Linux");

    let detached = model.result("t5");
    let work = work_id(&detached);
    assert_eq!(
        detached,
        format!(
            "The call is still running and was moved to the background as {work}. The \
             background-command tool reports on it, waits for it or stops it by that id. \
             Stopping it ends the code mode instance, and every value the instance holds is \
             lost. A call still running when the session ends is stopped."
        )
    );
    let waited = model.result("t6");
    let lines: Vec<&str> = waited.lines().collect();
    assert_eq!(lines[0], format!("work_id: {work}"), "{waited}");
    assert!(lines.contains(&"state: finished"), "{waited}");
    assert!(lines.contains(&"call_status: passed"), "{waited}");
    assert_eq!(
        lines.last().map(|line| line.trim_end()),
        Some(format!("still here: {}", names.len()).as_str()),
        "{waited}"
    );

    // The records.
    let trace = run.trace();
    let starts = records_of(&trace, "code_mode_instance_start");
    assert_eq!(starts.len(), 1, "{starts:#?}");
    assert_eq!(starts[0]["instance"], 1);
    assert_eq!(starts[0]["artifact"], REPL);
    let session_start = index_of(&trace, |e| e["event_type"] == "session_start");
    let start = index_of(&trace, |e| e["event_type"] == "code_mode_instance_start");
    assert!(session_start < start && start < model_call(&trace, "t1"));

    let ends = records_of(&trace, "code_mode_instance_end");
    assert_eq!(ends.len(), 1, "{ends:#?}");
    assert_eq!(ends[0]["reason"], "session_end");
    assert_eq!(ends[0]["calls"], 5);
    let end = index_of(&trace, |e| e["event_type"] == "code_mode_instance_end");
    assert!(end < index_of(&trace, |e| e["event_type"] == "session_end"));

    let detaches = records_of(&trace, "code_mode_detach");
    assert_eq!(detaches.len(), 1, "{detaches:#?}");
    assert_eq!(detaches[0]["work_id"], work.as_str());
    let background = records_of(&trace, "background_command");
    assert_eq!(background.len(), 1, "{background:#?}");
    assert_eq!(background[0]["work_id"], work.as_str());
    assert_eq!(background[0]["state"], "finished");
    assert_eq!(background[0]["artifact"], REPL);

    let bus: Vec<&Value> = trace
        .iter()
        .filter(|e| e["event_type"] == "tool_call" && e["origin"] == "code_mode")
        .collect();
    let mut bus_tools: Vec<&str> = bus
        .iter()
        .map(|e| e["tool_name"].as_str().unwrap())
        .collect();
    bus_tools.sort_unstable();
    assert_eq!(bus_tools, ["echo", "session-status"], "{bus:#?}");
    for call in &bus {
        assert_eq!(call["repl_call_id"], "t3", "{call}");
        assert_eq!(call["status"], "ok", "{call}");
    }

    let uname_start = index_of(&trace, |e| {
        e["event_type"] == "process_start"
            && e["binary"]
                .as_str()
                .is_some_and(|binary| binary.ends_with("/uname") || binary == "uname")
    });
    let process_id = &trace[uname_start]["process_id"];
    let uname_exit = index_of(&trace, |e| {
        e["event_type"] == "process_exit" && &e["process_id"] == process_id
    });
    assert_eq!(trace[uname_exit]["exit_code"], 0, "{}", trace[uname_exit]);
    let (t3, t4) = (model_call(&trace, "t3"), model_call(&trace, "t4"));
    assert!(t3 < uname_start && uname_start < uname_exit && uname_exit < t4);

    // `mur trace show` nests the batch's calls under the P3 call, and the page's excerpt is
    // that output.
    let show = home.trace_show(&project, &run.session());
    assert!(show.output.status.success(), "{}", show.context());
    eprintln!("mur trace show {}:\n{}", run.session(), show.stdout);
    let shown: Vec<String> = show
        .stdout
        .lines()
        .map(|line| normalize_durations(line.trim_end()))
        .collect();
    let turn = shown
        .iter()
        .position(|line| line.starts_with("  turn ") && line.contains("callAll"))
        .unwrap_or_else(|| panic!("no turn line for t3:\n{}", show.stdout));
    let mut nested: Vec<&str> = shown[turn + 1..turn + 3]
        .iter()
        .map(|line| line.trim_start())
        .collect();
    nested.sort_unstable();
    assert!(
        nested[0].starts_with("↳ echo ") && nested[1].starts_with("↳ session-status "),
        "{nested:?}"
    );
    assert!(
        shown.get(turn + 3).is_none_or(|line| !line.contains('↳')),
        "{shown:#?}"
    );
    let section = shown
        .iter()
        .position(|line| line.starts_with("── Tool calls"))
        .unwrap_or_else(|| panic!("{}", show.stdout));
    let count = shown[section..]
        .iter()
        .find(|line| line.starts_with("count:"))
        .unwrap_or_else(|| panic!("{}", show.stdout));
    assert!(count.ends_with("  code mode: 2"), "{count}");

    let excerpt: Vec<String> = page_trace_excerpt(&page)
        .iter()
        .map(|line| normalize_durations(line.trim_end()))
        .collect();
    assert!(
        shown.windows(excerpt.len()).any(|window| window == excerpt),
        "the page's trace excerpt:\n{}\nis not in the run's:\n{}",
        excerpt.join("\n"),
        shown.join("\n")
    );
}

/// The same capsule without `code_mode` is refused at launch, by `mur run` and `mur doctor`,
/// with `E-CAP-022` naming `tool-repl-js`; the model gets no request and no instance starts.
#[test]
fn without_code_mode_the_repl_is_refused_at_launch() {
    let home = Home::new(false);
    let model = Model::start(Vec::new());
    let project = home.project(&without_code_mode(&capsule_manifest(
        &how_to(),
        &model.server.endpoint,
    )));
    let run = home.run(&project);
    assert!(!run.output.status.success(), "{}", run.context());
    let refusal = format!(
        "error[E-CAP-022]: artifact '{REPL}' imports murmur:bus but is not the artifact \
         code_mode.repl names"
    );
    assert!(run.stderr.contains(&refusal), "{}", run.context());
    assert!(model.requests().is_empty(), "{:#?}", model.requests());
    for trace in common::sessions_under(home.home.path()) {
        let trace = trace.join("trace.jsonl");
        if trace.is_file() {
            assert!(
                records_of(
                    &common::read_whole_trace(&trace),
                    "code_mode_instance_start"
                )
                .is_empty(),
                "{}",
                trace.display()
            );
        }
    }

    let doctor = home.doctor(&project);
    assert!(!doctor.output.status.success(), "{}", doctor.context());
    let all = format!("{}{}", doctor.stdout, doctor.stderr);
    assert!(all.contains("E-CAP-022") && all.contains(REPL), "{all}");
}

/// The how-to holds the manifest, the programs and the trace excerpt the end-to-end test runs.
#[test]
fn how_to_shows_the_capsule_and_programs_the_test_runs() {
    let page = how_to();
    // Every substitution matches once; the run builds its manifest through the same function.
    let manifest = capsule_manifest(&page, "http://127.0.0.1:9");
    assert!(manifest.contains(&format!("repl: {REPL}")));
    assert!(
        without_code_mode(&manifest).contains(&format!("name: {REPL}")),
        "{manifest}"
    );

    // tool-repl-js is pinned only through its version macro.
    let yaml = page_manifest(&page);
    for line in yaml.lines().filter(|line| line.contains("version:")) {
        assert!(
            line.starts_with("version:") || line.contains("{{ v."),
            "an artifact version written literally: {line}"
        );
    }
    assert!(!page.contains("tool-repl-js@"), "a literal version");
    assert!(!page.contains(&format!("tool-repl-js {}", repl_version())));

    let programs = fences(&page, "js");
    assert!(!programs.is_empty(), "no js fence on the page");
    for shown in &programs {
        assert!(
            PROGRAMS.contains(&shown.as_str()),
            "the page shows a program the test does not run:\n{shown}"
        );
    }
    assert!(
        programs.iter().any(|shown| shown == P3),
        "the page shows P3"
    );
    assert!(!page_trace_excerpt(&page).is_empty());
}

#[test]
fn normalize_durations_replaces_each_duration() {
    assert_eq!(
        normalize_durations("  turn 3  tool-repl-js 1.2s ✓ 12ms 4s 7"),
        "  turn 3  tool-repl-js <dur> ✓ <dur> <dur> 7"
    );
}
