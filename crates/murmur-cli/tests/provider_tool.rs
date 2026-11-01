//! A tool that is a computer: its own `murmur.yaml` declares `provider_tool:`, the runtime passes
//! the block to the driver on its tool-array entry, and the capsule must run `sealed` on a driver
//! choice whose model can see. A driver may attach `safety_checks` to a call, and the call runs
//! only once the person acknowledges them.
//!
//! The model is the `provider-tool-driver` fixture: its task line scripts the calls it makes,
//! safety checks included, and its final text reports each tool-array entry's `provider_tool` and
//! each tool message's `acknowledged_safety_checks`. The computer tool is the WASM `image-tool`
//! fixture packed with a `provider_tool:` block; its call writes `img-<index>.png` into the work
//! folder, the side effect these tests look for. What runs is real: the staged session, the
//! tools, the door, the stream, the conversation record and the trace.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use assert_cmd::Command;
use capsule_runtime::{
    capability_policy_from_runtime_manifest, launch_session, stage_session, ArtifactRequest,
    LifecycleConfig, RuntimeError, StageRequest, StagedSession,
};
use murmur_artifact::{
    load_runtime_manifest, AfterTask, ArtifactRuntime, ContainmentClass, LocalRegistry,
    TaskAcceptance,
};
use serde_json::{json, Value};
use tempfile::TempDir;

const DRIVER: &str = "local/provider-tool-driver";
const COMPUTER: &str = "local/computer";
const SCREEN: &str = "local/screen";
const PLAIN: &str = "local/image-tool";
const VERSION: &str = "0.1.0";
const ENDPOINT: &str = "http://127.0.0.1:9";
const MODEL: &str = "probe-model";

/// The `control:` block a capsule with alternates declares, so a controller can switch to one.
const CONTROL: &str = "control:\n  settings: [inference.driver]\n";

const PROVIDER_TOOL_BLOCK: &str =
    "provider_tool:\n  kind: computer\n  display_width: 1280\n  display_height: 800\n";

/// The checks the safety-check tests attach to their one call.
fn checks() -> Value {
    json!([{"id": "sc_1", "code": "irrelevant_domain", "message": "The page is not the one the task named."}])
}

/// The driver's `models:` table: [`MODEL`] with `vision` as given (absent for `None`), and
/// `blind-model` marked `vision: false`.
fn models(vision: Option<bool>) -> String {
    let vision = vision.map_or_else(String::new, |vision| format!("    vision: {vision}\n"));
    format!(
        "models:\n  {MODEL}:\n    max_output_tokens: 4096\n    context_window: 32000\n{vision}  \
         blind-model:\n    max_output_tokens: 4096\n    context_window: 32000\n    vision: false\n"
    )
}

/// A scratch home with the driver (its table from [`models`]), the two computer tools and the
/// plain image tool published into it.
fn publish_fixtures(vision: Option<bool>) -> (TempDir, TempDir) {
    let home = TempDir::new().unwrap();
    let artifacts = TempDir::new().unwrap();
    let driver = common::create_driver_artifact_with_auth(
        artifacts.path(),
        DRIVER,
        VERSION,
        &common::fixture_path("provider-tool-driver/tool/provider-tool-driver.wasm"),
        &format!("{}{}", common::FIXTURE_DRIVER_AUTH, models(vision)),
    );
    common::publish_local(&home, &driver).success();
    let wasm = common::fixture_path("image-tool/tool/image-tool.wasm");
    for (name, block) in [
        (COMPUTER, PROVIDER_TOOL_BLOCK),
        (SCREEN, PROVIDER_TOOL_BLOCK),
        (PLAIN, ""),
    ] {
        let tool =
            common::create_tool_artifact_with_auth(artifacts.path(), name, VERSION, &wasm, block);
        common::publish_local(&home, &tool).success();
    }
    (home, artifacts)
}

/// The capsule's manifest: the driver and each of `tools`, with `inference_extra` under
/// `inference:` and `extra` appended.
fn manifest(name: &str, tools: &[&str], inference_extra: &str, extra: &str) -> String {
    let tools: String = tools
        .iter()
        .map(|tool| format!("  - name: {tool}\n    version: {VERSION}\n    runtime: tool\n"))
        .collect();
    format!(
        "name: {name}\nversion: 0.1.0\n\
         artifacts:\n  - name: {DRIVER}\n    version: {VERSION}\n    runtime: driver\n    \
         gateway:\n      endpoint: {ENDPOINT}\n      api_key: test-key\n{tools}\
         capabilities:\n  network:\n    allow:\n      - {ENDPOINT}\n\
         inference:\n  transport: http\n  model: {MODEL}\n  driver:\n    artifact: {DRIVER}\n\
         {inference_extra}context:\n  record_store: {name}\n{extra}"
    )
}

/// Stages the capsule at `manifest_path` from `home`'s store at `floor`, with `lifecycle` when
/// given. `Err` is staging's refusal.
fn stage(
    home: &TempDir,
    manifest_path: &Path,
    floor: ContainmentClass,
    lifecycle: Option<LifecycleConfig>,
) -> Result<StagedSession, RuntimeError> {
    let runtime_manifest = load_runtime_manifest(manifest_path).unwrap();
    let mut allowlisted_tools = std::collections::HashSet::new();
    let mut requested_artifacts = Vec::new();
    for artifact in &runtime_manifest.artifacts {
        if matches!(artifact.runtime, ArtifactRuntime::Tool) {
            allowlisted_tools.insert(artifact.name.clone());
        }
        requested_artifacts.push(ArtifactRequest {
            name: artifact.name.clone(),
            version: artifact.version.clone(),
            runtime: artifact.runtime.clone(),
            source: artifact.source.clone(),
            on_overflow: artifact.on_overflow,
            config: artifact.config.clone(),
            gateway: artifact.gateway.clone(),
            capabilities: artifact.capabilities.clone(),
        });
    }
    let local_registry = LocalRegistry::new(home.path().join(".murmur").join("artifacts"));
    common::process_home();
    stage_session(
        std::sync::Arc::new(local_registry),
        StageRequest {
            source_supply: None,
            expected_workdir_identity: None,
            parent_workdir_snapshot: None,
            session_deadline: None,
            manifest_file: Some(manifest_path.to_path_buf()),
            credentials_file: None,
            manifest_dir: manifest_path.parent().unwrap().to_path_buf(),
            capsule_name: runtime_manifest.name.clone(),
            capsule_version: runtime_manifest.version.clone(),
            capsule_artifact: None,
            capsule_component_bytes: Vec::new(),
            artifacts: requested_artifacts,
            allowlisted_tools,
            lock_expectations: None,
            capability_policy: capability_policy_from_runtime_manifest(&runtime_manifest),
            inference: runtime_manifest.inference.clone(),
            system_prompt_overridden: false,
            context: runtime_manifest.context.clone(),
            context_id: None,
            resume: None,
            forget_session: false,
            otel_endpoint: None,
            eval_config_json: None,
            case_id: None,
            dataset_id: None,
            lifecycle,
            lifecycle_override: None,
            trace: None,
            workdir: None,
            bind_addr: "127.0.0.1".to_string(),
            internal_port: None,
            declared_containment_floor: floor,
            exports: None,
            control: None,
            door_authentication: None,
            spawn_grant: None,
            machine_tokens_per_day: None,
            formation_id: None,
            formation_member: None,
        },
    )
}

/// Whether this host can run the `sealed` tests; prints `[SKIP-HOST]` when it cannot.
fn skip_unless_sealed(test_name: &str) -> bool {
    if common::skip_without_host_support(test_name) {
        return true;
    }
    let achieved = capsule_runtime::detect_containment_decision(ContainmentClass::Sealed)
        .0
        .achieved();
    if achieved < ContainmentClass::Sealed {
        eprintln!("[SKIP-HOST] {test_name}: this host reaches {achieved}, the test needs sealed");
        return true;
    }
    false
}

/// Every session directory under the process home's records root: none after a refused launch.
fn session_dirs() -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: usize, found: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if entry.file_name().to_string_lossy().starts_with("ses_") {
                    found.push(path);
                } else if depth < 12 {
                    walk(&path, depth + 1, found);
                }
            }
        }
    }
    let mut found = Vec::new();
    walk(
        &common::process_home().join(".murmur").join("records"),
        0,
        &mut found,
    );
    found
}

/// One launched session of a project and what it left behind.
struct Run {
    report: String,
    trace: Vec<Value>,
    workdir: PathBuf,
    _dirs: (TempDir, TempDir, TempDir),
}

impl Run {
    /// The report's `tool <name> <provider_tool>` lines as `(name, provider_tool)`.
    fn tools(&self) -> Vec<(String, String)> {
        self.report
            .lines()
            .filter_map(|line| line.strip_prefix("tool "))
            .filter_map(|rest| rest.split_once(' '))
            .map(|(name, block)| (name.to_string(), block.to_string()))
            .collect()
    }
}

/// Stages `manifest` at `floor` and runs its one task, whose driver plays `script`.
fn run(
    (home, artifacts): (TempDir, TempDir),
    manifest_text: &str,
    floor: ContainmentClass,
    script: Value,
) -> Run {
    let project = TempDir::new().unwrap();
    let manifest_path = project.path().join("murmur.yaml");
    fs::write(&manifest_path, manifest_text).unwrap();
    let staged = stage(&home, &manifest_path, floor, None)
        .unwrap_or_else(|error| panic!("the session must stage: {error}"));
    let workdir = staged.accessible_workdir.clone();
    let records_dir = staged.records_dir.clone();
    fs::write(workdir.join("task.md"), format!("SCRIPT {script}\n")).unwrap();
    if let Err(error) = launch_session(staged, |_| {}) {
        panic!("the session must launch: {error}");
    }
    let report = fs::read_to_string(records_dir.join("out/result.txt")).unwrap_or_default();
    println!("driver report:\n{report}");
    Run {
        report,
        trace: common::read_whole_trace(&records_dir.join("trace.jsonl")),
        workdir,
        _dirs: (home, artifacts, project),
    }
}

/// The task completed: the session's one task ended `ok` and the driver reported.
fn assert_completed(run: &Run) {
    let ends: Vec<&Value> = run
        .trace
        .iter()
        .filter(|event| event["event_type"] == "task_end")
        .collect();
    assert_eq!(ends.len(), 1, "{:?}", run.trace);
    assert_eq!(ends[0]["exit_status"], "ok", "{}", ends[0]);
    assert!(!run.report.is_empty(), "the driver reported");
}

fn computer_call(index: u64) -> Value {
    json!({"name": "computer", "input": {"mode": "png", "index": index}})
}

#[test]
fn the_declaration_reaches_the_driver() {
    if skip_unless_sealed("the_declaration_reaches_the_driver") {
        return;
    }
    let run = run(
        publish_fixtures(None),
        &manifest("declares", &[COMPUTER, PLAIN], "", ""),
        ContainmentClass::Sealed,
        json!([computer_call(1)]),
    );

    let tools = run.tools();
    assert!(
        tools.contains(&(
            "computer".to_string(),
            r#"{"kind":"computer","display_width":1280,"display_height":800}"#.to_string()
        )),
        "{tools:?}"
    );
    let others: Vec<_> = tools
        .iter()
        .filter(|(name, _)| name != "computer")
        .collect();
    assert!(
        others.iter().any(|(name, _)| name == "image-tool"),
        "{tools:?}"
    );
    assert!(others.iter().all(|(_, block)| block == "-"), "{tools:?}");
    assert!(run.workdir.join("img-1.png").is_file(), "the call ran");
    assert!(
        run.report.contains("message call_0_0 false - "),
        "{}",
        run.report
    );
    assert_completed(&run);
}

#[test]
fn below_sealed_is_refused() {
    let (home, _artifacts) = publish_fixtures(None);
    for floor in [ContainmentClass::Scoped, ContainmentClass::Advisory] {
        let project = TempDir::new().unwrap();
        let manifest_path = project.path().join("murmur.yaml");
        fs::write(
            &manifest_path,
            manifest("refused", &[COMPUTER, PLAIN], "", ""),
        )
        .unwrap();
        let error = stage(&home, &manifest_path, floor, None)
            .err()
            .unwrap_or_else(|| panic!("a computer tool at {floor} is refused"));
        let message = error.to_string();
        println!("{floor}: {message}");
        assert!(
            matches!(error, RuntimeError::ProviderToolRequiresSealed { .. }),
            "{error:?}"
        );
        assert!(
            message.contains("tool 'computer' (provider_tool: computer)")
                && message.contains(&format!("the effective containment floor is '{floor}'"))
                && !message.contains("image-tool"),
            "{message}"
        );
        assert!(session_dirs().is_empty(), "{:?}", session_dirs());
    }

    // Two computer tools are both named.
    let project = TempDir::new().unwrap();
    let manifest_path = project.path().join("murmur.yaml");
    fs::write(
        &manifest_path,
        manifest("refused-two", &[COMPUTER, SCREEN], "", ""),
    )
    .unwrap();
    let message = stage(&home, &manifest_path, ContainmentClass::Scoped, None)
        .err()
        .expect("refused")
        .to_string();
    assert!(
        message.starts_with(
            "tool 'computer' (provider_tool: computer), tool 'screen' (provider_tool: computer) \
             declare a provider tool"
        ),
        "{message}"
    );

    // `mur run` renders it as E-CAP-003 and creates no records directory.
    let project = TempDir::new().unwrap();
    let manifest_path = project.path().join("murmur.yaml");
    fs::write(
        &manifest_path,
        manifest("refused-cli", &[COMPUTER], "", "")
            .replace("capabilities:\n", "capabilities:\n  containment: scoped\n"),
    )
    .unwrap();
    let output = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .current_dir(project.path())
        .args([
            "run",
            "--manifest",
            manifest_path.to_str().unwrap(),
            "--task",
            "SCRIPT []",
        ])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    println!("mur run stderr:\n{stderr}");
    assert!(!output.status.success());
    assert!(
        stderr.contains(
            "error[E-CAP-003]: tool 'computer' (provider_tool: computer) declares a provider \
             tool, which runs only under the 'sealed' containment floor, and the effective \
             containment floor is 'scoped'"
        ),
        "{stderr}"
    );
    assert!(
        stderr.contains("hint: set `capabilities.containment: sealed` in murmur.yaml"),
        "{stderr}"
    );
    assert!(
        !home.path().join(".murmur").join("records").exists()
            || fs::read_dir(home.path().join(".murmur").join("records"))
                .unwrap()
                .next()
                .is_none(),
        "mur run created a records directory"
    );
}

#[test]
fn vision_false_is_refused_and_true_or_absent_runs() {
    if skip_unless_sealed("vision_false_is_refused_and_true_or_absent_runs") {
        return;
    }
    let alternate = format!(
        "  alternates:\n    - name: fallback\n      model: blind-model\n      driver:\n        \
         artifact: {DRIVER}\n"
    );

    // The primary's model is `vision: false`: one E-RUN-057 line from `mur run`.
    let (home, _artifacts) = publish_fixtures(Some(false));
    let project = TempDir::new().unwrap();
    let manifest_path = project.path().join("murmur.yaml");
    fs::write(
        &manifest_path,
        manifest("blind", &[COMPUTER], "", "")
            .replace("capabilities:\n", "capabilities:\n  containment: sealed\n"),
    )
    .unwrap();
    let output = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .current_dir(project.path())
        .args([
            "run",
            "--manifest",
            manifest_path.to_str().unwrap(),
            "--task",
            "SCRIPT []",
        ])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    println!("mur run stderr:\n{stderr}");
    assert!(!output.status.success());
    let errors: Vec<&str> = stderr
        .lines()
        .filter(|line| line.starts_with("error["))
        .collect();
    assert_eq!(
        errors,
        vec![
            "error[E-RUN-057]: tool 'computer' is a computer (provider_tool: computer) but model \
             'probe-model' of driver choice 'primary' has no vision: driver \
             local/provider-tool-driver@0.1.0 declares vision: false for it"
        ],
        "{stderr}"
    );
    assert!(
        !home.path().join(".murmur").join("records").exists()
            || fs::read_dir(home.path().join(".murmur").join("records"))
                .unwrap()
                .next()
                .is_none(),
        "the refused launch created a records directory"
    );

    // Only an alternate's model is `vision: false`: refused naming that choice.
    let (home, _artifacts) = publish_fixtures(Some(true));
    let project = TempDir::new().unwrap();
    let manifest_path = project.path().join("murmur.yaml");
    fs::write(
        &manifest_path,
        manifest("blind-alt", &[COMPUTER], &alternate, CONTROL),
    )
    .unwrap();
    let error = stage(&home, &manifest_path, ContainmentClass::Sealed, None)
        .err()
        .expect("an alternate without vision is refused");
    assert_eq!(
        error.to_string(),
        "tool 'computer' is a computer (provider_tool: computer) but model 'blind-model' of \
         driver choice 'fallback' has no vision: driver local/provider-tool-driver@0.1.0 \
         declares vision: false for it"
    );
    assert!(session_dirs().is_empty(), "{:?}", session_dirs());

    // `vision: true`, and no `vision` key, start and complete.
    for vision in [Some(true), None] {
        let run = run(
            publish_fixtures(vision),
            &manifest("sees", &[COMPUTER], "", ""),
            ContainmentClass::Sealed,
            json!([computer_call(2)]),
        );
        assert!(run.workdir.join("img-2.png").is_file(), "{vision:?}");
        assert_completed(&run);
    }

    // `vision: false` without a computer tool starts as it always has.
    let run = run(
        publish_fixtures(Some(false)),
        &manifest("blind-plain", &[PLAIN], &alternate, CONTROL),
        ContainmentClass::Advisory,
        json!([{"name": "image-tool", "input": {"mode": "png", "index": 3}}]),
    );
    assert!(run.workdir.join("img-3.png").is_file());
    assert_completed(&run);
}

/// The control: a capsule with no computer tool and no safety check sends no `provider_tool`,
/// no `acknowledged_safety_checks`, and writes no `safety_check` event.
#[test]
fn control_without_a_provider_tool_runs_as_before() {
    let run = run(
        publish_fixtures(None),
        &manifest("control", &[PLAIN], "", ""),
        ContainmentClass::Advisory,
        json!([{"name": "image-tool", "input": {"mode": "png", "index": 4}}]),
    );
    assert!(
        run.tools().iter().all(|(_, block)| block == "-"),
        "{}",
        run.report
    );
    assert!(
        run.report.contains("message call_0_0 false - "),
        "{}",
        run.report
    );
    assert!(!run.report.contains("acknowledged"), "{}", run.report);
    assert!(
        !run.trace
            .iter()
            .any(|event| event["event_type"] == "safety_check"),
        "{:?}",
        run.trace
    );
    assert!(run.workdir.join("img-4.png").is_file());
    assert_completed(&run);
}

// ── Safety checks, over the door ──────────────────────────────────────────────

/// A served capsule of the driver and the plain image tool, launched in process.
struct Door {
    addr: String,
    records_dir: PathBuf,
    workdir: PathBuf,
    record_store: PathBuf,
    _dirs: (TempDir, TempDir, TempDir),
}

impl Door {
    fn launch(name: &str, input_timeout_secs: Option<u64>) -> Self {
        let (home, artifacts) = publish_fixtures(None);
        let project = TempDir::new().unwrap();
        let manifest_path = project.path().join("murmur.yaml");
        fs::write(&manifest_path, manifest(name, &[PLAIN], "", "")).unwrap();
        let lifecycle = LifecycleConfig {
            task_acceptance: TaskAcceptance::Queue,
            after_task: AfterTask::Sleep,
            input_timeout_secs,
            ..Default::default()
        };
        let staged = stage(
            &home,
            &manifest_path,
            ContainmentClass::Advisory,
            Some(lifecycle),
        )
        .unwrap_or_else(|error| panic!("the door must stage: {error}"));
        let records_dir = staged.records_dir.clone();
        let workdir = staged.accessible_workdir.clone();
        let (url_tx, url_rx) = std::sync::mpsc::channel::<String>();
        std::thread::spawn(move || {
            let _ = launch_session(staged, move |url| {
                let _ = url_tx.send(url.to_string());
            });
        });
        let addr = url_rx
            .recv_timeout(common::CAPSULE_URL_WAIT)
            .expect("the door opens");
        Self {
            addr,
            records_dir,
            workdir,
            record_store: common::process_home()
                .join(".murmur")
                .join("conversations")
                .join(name),
            _dirs: (home, artifacts, project),
        }
    }

    fn rpc(&self, method: &str, params: Value) -> Value {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        http_post_json(&self.addr, &body.to_string())
    }

    /// Starts a task whose driver makes one image-tool call carrying `safety_checks`.
    fn start(&self, safety_checks: &Value) -> String {
        let script = json!([{
            "name": "image-tool",
            "input": {"mode": "png", "index": 1},
            "safety_checks": safety_checks,
        }]);
        let answer = self.rpc(
            "SendMessage",
            json!({
                "message": {"messageId": "msg-1", "contextId": "ctx-safety", "role": "ROLE_USER",
                            "parts": [{"text": format!("SCRIPT {script}")}]},
                "configuration": {"returnImmediately": true},
            }),
        );
        answer["result"]["task"]["id"]
            .as_str()
            .unwrap_or_else(|| panic!("a task id: {answer}"))
            .to_string()
    }

    fn reply(&self, task_id: &str, text: &str) -> Value {
        self.rpc(
            "SendMessage",
            json!({
                "message": {"messageId": "msg-2", "contextId": "ctx-safety", "role": "ROLE_USER",
                            "taskId": task_id, "parts": [{"text": text}]},
                "configuration": {"returnImmediately": true},
            }),
        )
    }

    fn poll(&self, task_id: &str, state: &str) -> Value {
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        loop {
            let task = self.rpc("GetTask", json!({"id": task_id}));
            if task["result"]["status"]["state"] == state {
                return task;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no {state} for {task_id}: {task}"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn trace(&self) -> Vec<Value> {
        common::read_whole_trace(&self.records_dir.join("trace.jsonl"))
    }

    /// The trace once it holds `task_id`'s `task_end`.
    fn trace_after_end(&self, task_id: &str) -> Vec<Value> {
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        loop {
            let trace = self.trace();
            if trace
                .iter()
                .any(|event| event["event_type"] == "task_end" && event["task_id"] == task_id)
            {
                return trace;
            }
            assert!(std::time::Instant::now() < deadline, "no task_end");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// The driver's report, once `out/result.txt` holds it.
    fn report(&self) -> String {
        fs::read_to_string(self.records_dir.join("out/result.txt")).unwrap_or_default()
    }

    /// The conversation record's lines.
    fn record(&self) -> Vec<Value> {
        let context = fs::read_dir(&self.record_store)
            .unwrap_or_else(|e| panic!("{}: {e}", self.record_store.display()))
            .map(|entry| entry.unwrap().path())
            .find(|path| path.join("conversation.jsonl").is_file())
            .expect("the session recorded its conversation");
        fs::read_to_string(context.join("conversation.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

fn http_post_json(addr: &str, body: &str) -> Value {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(addr).expect("connect to the door");
    stream.set_read_timeout(Some(Duration::from_secs(30))).ok();
    let request = format!(
        "POST / HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nA2A-Version: \
         1.0\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let body = response.split_once("\r\n\r\n").map_or("", |(_, body)| body);
    serde_json::from_str(body).unwrap_or_else(|_| json!({"_raw": body}))
}

fn safety_events(trace: &[Value]) -> Vec<&Value> {
    trace
        .iter()
        .filter(|event| event["event_type"] == "safety_check")
        .collect()
}

fn tool_call_records(trace: &[Value]) -> Vec<&Value> {
    trace
        .iter()
        .filter(|event| event["event_type"] == "tool_call")
        .collect()
}

#[test]
fn a_safety_check_acknowledged_runs_the_call() {
    let door = Door::launch("safety-yes", None);
    let mut watch = common::stream::Watch::open(&door.addr, 0);
    let task_id = door.start(&checks());

    let waiting = door.poll(&task_id, "TASK_STATE_INPUT_REQUIRED");
    let shown = waiting.to_string();
    println!("GetTask while waiting: {waiting}");
    assert!(
        shown.contains("safety check on tool call 'image-tool' (call_0_0):")
            && shown.contains("irrelevant_domain: The page is not the one the task named.")
            && shown.contains(
                "reply yes to acknowledge and run the action; any other reply declines it"
            ),
        "{waiting}"
    );
    assert!(
        watch.read_until(Duration::from_secs(30), |event| {
            event.murmur_state() == Some("input-required")
        }),
        "the stream has input-required: {:?}",
        watch.events
    );
    assert!(!door.workdir.join("img-1.png").exists(), "nothing ran yet");
    assert!(tool_call_records(&door.trace()).is_empty());

    let resumed = door.reply(&task_id, "yes");
    assert_eq!(
        resumed["result"]["task"]["status"]["state"], "TASK_STATE_WORKING",
        "{resumed}"
    );
    assert!(
        watch.read_until(Duration::from_secs(30), |event| {
            event.murmur_state() == Some("working") && event.status_text() == Some("resumed")
        }),
        "a resumed status follows: {:?}",
        watch.events
    );
    door.poll(&task_id, "TASK_STATE_COMPLETED");
    let trace = door.trace_after_end(&task_id);

    assert!(door.workdir.join("img-1.png").is_file(), "the call ran");
    let report = door.report();
    println!("driver report:\n{report}");
    let message = report
        .lines()
        .find_map(|line| line.strip_prefix("message call_0_0 false "))
        .unwrap_or_else(|| panic!("{report}"));
    let acknowledged: Value = serde_json::Deserializer::from_str(message)
        .into_iter::<Value>()
        .next()
        .unwrap()
        .unwrap();
    assert_eq!(acknowledged, checks());

    let events = safety_events(&trace);
    assert_eq!(events.len(), 1, "{trace:?}");
    assert_eq!(events[0]["outcome"], "acknowledged");
    assert_eq!(events[0]["checks"], checks());
    assert_eq!(events[0]["tool"], "image-tool");
    assert_eq!(events[0]["tool_call_id"], "call_0_0");
    assert!(!events[0].to_string().contains("\"yes\""), "{}", events[0]);
    let position = |event_type: &str| {
        trace
            .iter()
            .position(|event| event["event_type"] == event_type)
            .unwrap()
    };
    assert!(
        position("safety_check") < position("tool_call"),
        "{trace:?}"
    );
    assert_eq!(tool_call_records(&trace).len(), 1);

    let record = door.record();
    let tool_message = record
        .iter()
        .find(|line| line["role"] == "tool")
        .unwrap_or_else(|| panic!("{record:?}"));
    assert_eq!(tool_message["acknowledged_safety_checks"], checks());
    println!(
        "safety_check event: {}\ntool_call record: {}\nrecorded tool message: {tool_message}",
        events[0],
        tool_call_records(&trace)[0]
    );
}

#[test]
fn a_safety_check_declined_does_not_run_the_call() {
    let door = Door::launch("safety-no", None);
    let task_id = door.start(&checks());
    door.poll(&task_id, "TASK_STATE_INPUT_REQUIRED");
    door.reply(&task_id, "no, wrong site");
    door.poll(&task_id, "TASK_STATE_COMPLETED");
    let trace = door.trace_after_end(&task_id);

    assert!(!door.workdir.join("img-1.png").exists(), "nothing ran");
    assert!(tool_call_records(&trace).is_empty(), "{trace:?}");
    let report = door.report();
    println!("driver report:\n{report}");
    let message = report
        .lines()
        .find_map(|line| line.strip_prefix("message call_0_0 true - "))
        .unwrap_or_else(|| panic!("{report}"));
    assert!(
        message.starts_with(
            "the person did not acknowledge the safety check on this call, so it was not run: \
             irrelevant_domain: The page is not the one the task named.; their reply: "
        ) && message.contains("no, wrong site"),
        "{message}"
    );
    let events = safety_events(&trace);
    assert_eq!(events.len(), 1, "{trace:?}");
    assert_eq!(events[0]["outcome"], "declined");
    assert!(
        !events[0].to_string().contains("wrong site"),
        "{}",
        events[0]
    );
    let record = door.record();
    let tool_message = record.iter().find(|line| line["role"] == "tool").unwrap();
    assert_eq!(tool_message["is_error"], true);
    assert!(tool_message.get("acknowledged_safety_checks").is_none());
}

#[test]
fn a_safety_check_times_out_without_running_the_call() {
    let door = Door::launch("safety-timeout", Some(2));
    let task_id = door.start(&checks());
    door.poll(&task_id, "TASK_STATE_INPUT_REQUIRED");
    let failed = door.poll(&task_id, "TASK_STATE_FAILED");
    assert!(failed.to_string().contains("input-timeout"), "{failed}");
    let trace = door.trace_after_end(&task_id);

    assert!(!door.workdir.join("img-1.png").exists(), "nothing ran");
    assert!(tool_call_records(&trace).is_empty(), "{trace:?}");
    let task_failed: Vec<&Value> = trace
        .iter()
        .filter(|event| event["event_type"] == "task_failed" && event["task_id"] == task_id)
        .collect();
    assert_eq!(task_failed.len(), 1, "{trace:?}");
    assert_eq!(task_failed[0]["cause"], "input_timeout");
    let events = safety_events(&trace);
    assert_eq!(events.len(), 1, "{trace:?}");
    assert_eq!(events[0]["outcome"], "timed_out");
}

/// A malformed check leaves the call unrun without asking anyone.
#[test]
fn a_safety_check_malformed_does_not_run_the_call() {
    let malformed = run(
        publish_fixtures(None),
        &manifest("safety-malformed", &[PLAIN], "", ""),
        ContainmentClass::Advisory,
        json!([{"name": "image-tool", "input": {"mode": "png", "index": 5},
                "safety_checks": [{"code": "c"}]}]),
    );
    assert!(!malformed.workdir.join("img-5.png").exists());
    assert!(
        malformed.report.contains(
            "message call_0_0 true - the driver returned a malformed safety check with this \
             call, so it was not run"
        ),
        "{}",
        malformed.report
    );
    let events = safety_events(&malformed.trace);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["outcome"], "malformed");
    assert_eq!(events[0]["checks"], json!([{"code": "c"}]));
    assert!(tool_call_records(&malformed.trace).is_empty());
    assert_completed(&malformed);
}
