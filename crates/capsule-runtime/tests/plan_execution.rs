use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex, OnceLock,
    },
    thread,
    time::{Duration, Instant},
};

use capsule_runtime::{
    bindings::host::murmur::tool::run::{Status as ToolStatus, ToolInput, ToolResult},
    plan::{self, SchedulerContext, StepStatus},
    CapabilityPolicy,
};
use serde_json::{json, Value};
use tempfile::tempdir;

fn ctx<'a>(
    workdir: PathBuf,
    invoke_tool: &'a (dyn Fn(&str, ToolInput) -> Result<ToolResult, String> + Sync),
) -> SchedulerContext<'a> {
    SchedulerContext {
        workdir,
        capability_policy: CapabilityPolicy {
            shell_allow: vec!["bash".to_string(), "printf".to_string()],
            spawn_allow: vec!["worker".to_string()],
            ..CapabilityPolicy::default()
        },
        installed_tools: HashSet::from([
            "echo".to_string(),
            "fail".to_string(),
            "slow".to_string(),
            "flaky".to_string(),
        ]),
        capsule_versions: HashMap::from([("worker".to_string(), "0.1.0".to_string())]),
        current_job_id: None,
        invoke_tool,
    }
}

fn tool_result(status: ToolStatus, data: Option<String>, summary: Option<String>) -> ToolResult {
    ToolResult {
        status,
        summary,
        data,
        data_path: None,
        truncated: false,
        metadata: Vec::new(),
    }
}

fn write_plan(workdir: &Path, plan: Value) -> PathBuf {
    let path = workdir.join("plan.json");
    fs::write(&path, serde_json::to_string(&plan).unwrap()).unwrap();
    path
}

fn find<'a>(report: &'a plan::ExecutionReport, step_id: &str) -> &'a plan::StepResult {
    report
        .results
        .iter()
        .find(|result| result.step_id == step_id)
        .unwrap()
}

#[test]
fn test_parallel_tool_steps_run_concurrently() {
    let dir = tempdir().unwrap();
    let active = Arc::new(AtomicUsize::new(0));
    let max_active = Arc::new(AtomicUsize::new(0));
    let invoke_active = Arc::clone(&active);
    let invoke_max = Arc::clone(&max_active);
    let invoke = move |_name: &str, _input: ToolInput| {
        let now = invoke_active.fetch_add(1, Ordering::SeqCst) + 1;
        invoke_max.fetch_max(now, Ordering::SeqCst);
        thread::sleep(Duration::from_millis(150));
        invoke_active.fetch_sub(1, Ordering::SeqCst);
        Ok(tool_result(
            ToolStatus::Passed,
            Some("ok".to_string()),
            None,
        ))
    };
    let plan = write_plan(
        dir.path(),
        json!({"id":"p","steps":[{"id":"a","tool":"slow"},{"id":"b","tool":"slow"}]}),
    );

    let started = Instant::now();
    let report = plan::execute(&plan, &ctx(dir.path().to_path_buf(), &invoke));

    assert!(report.completed, "{report:?}");
    assert!(started.elapsed() < Duration::from_millis(280));
    assert_eq!(max_active.load(Ordering::SeqCst), 2);
}

#[test]
fn test_dependent_step_receives_upstream_output() {
    let dir = tempdir().unwrap();
    let inputs = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&inputs);
    let invoke = move |_name: &str, input: ToolInput| {
        let input_json = input.data.unwrap_or_default();
        seen.lock().unwrap().push(input_json.clone());
        let output = if input_json.contains("from-a") {
            "done"
        } else {
            "from-a"
        };
        Ok(tool_result(
            ToolStatus::Passed,
            Some(output.to_string()),
            None,
        ))
    };
    let plan = write_plan(
        dir.path(),
        json!({
            "id":"p",
            "steps":[
                {"id":"a","tool":"echo"},
                {"id":"b","tool":"echo","depends_on":["a"],"input":{"value":"$a.output"}}
            ]
        }),
    );

    let report = plan::execute(&plan, &ctx(dir.path().to_path_buf(), &invoke));

    assert!(report.completed, "{report:?}");
    assert!(inputs.lock().unwrap()[1].contains("from-a"));
}

#[test]
fn test_shell_step_executes() {
    let dir = tempdir().unwrap();
    let invoke = move |_name: &str, _input: ToolInput| {
        Ok(tool_result(
            ToolStatus::Passed,
            Some("unused".to_string()),
            None,
        ))
    };
    let plan = write_plan(
        dir.path(),
        json!({"id":"p","steps":[{"id":"sh","shell":"bash -c 'printf shell-ok'"}]}),
    );

    let report = plan::execute(&plan, &ctx(dir.path().to_path_buf(), &invoke));

    assert!(report.completed);
    assert_eq!(find(&report, "sh").output.as_deref(), Some("shell-ok"));
}

#[test]
#[ignore = "requires a running mur-roost and published worker capsule fixture"]
fn test_capsule_step_spawns_and_reads_result() {
    let dir = tempdir().unwrap();
    let invoke = |_name: &str, _input: ToolInput| {
        Ok(tool_result(
            ToolStatus::Passed,
            Some("unused".to_string()),
            None,
        ))
    };
    let plan = write_plan(
        dir.path(),
        json!({"id":"p","steps":[{"id":"worker","capsule":"worker","input":{"task":"hello"}}]}),
    );

    let report = plan::execute(&plan, &ctx(dir.path().to_path_buf(), &invoke));

    assert!(report.completed, "{report:?}");
    assert_eq!(find(&report, "worker").status, StepStatus::Success);
    assert!(find(&report, "worker").output.is_some());
}

#[test]
fn test_capsule_step_posts_murmur_message_input() {
    let _guard = roost_env_lock().lock().unwrap();
    let dir = tempdir().unwrap();
    let fake_roost = FakeRoost::start(dir.path());
    std::env::set_var("MURMUR_ROOST_URL", &fake_roost.url);
    let invoke = |_name: &str, _input: ToolInput| {
        Ok(tool_result(
            ToolStatus::Passed,
            Some("unused".to_string()),
            None,
        ))
    };
    let plan = write_plan(
        dir.path(),
        json!({
            "id":"p",
            "steps":[{"id":"worker","capsule":"worker","input":{"objective":"Echo this task"}}]
        }),
    );

    let report = plan::execute(&plan, &ctx(dir.path().to_path_buf(), &invoke));
    std::env::remove_var("MURMUR_ROOST_URL");

    assert!(report.completed, "{report:?}");
    assert_eq!(
        find(&report, "worker").output.as_deref(),
        Some("worker-output")
    );
    let posted = fake_roost.spawn_request();
    let input = posted.get("input").and_then(Value::as_str).unwrap();
    let message: Value = serde_json::from_str(input).unwrap();
    assert_eq!(message["schema"], "murmur.message.v1");
    assert_eq!(message["type"], "murmur.code_task.request.v1");
    assert_eq!(message["payload"]["objective"], "Echo this task");
}

#[test]
fn test_capsule_step_input_fallback_uses_serialized_json_as_objective() {
    let _guard = roost_env_lock().lock().unwrap();
    let dir = tempdir().unwrap();
    let fake_roost = FakeRoost::start(dir.path());
    std::env::set_var("MURMUR_ROOST_URL", &fake_roost.url);
    let invoke = |_name: &str, _input: ToolInput| {
        Ok(tool_result(
            ToolStatus::Passed,
            Some("unused".to_string()),
            None,
        ))
    };
    let plan = write_plan(
        dir.path(),
        json!({
            "id":"p",
            "steps":[{"id":"worker","capsule":"worker","input":{"task":"fallback task"}}]
        }),
    );

    let report = plan::execute(&plan, &ctx(dir.path().to_path_buf(), &invoke));
    std::env::remove_var("MURMUR_ROOST_URL");

    assert!(report.completed, "{report:?}");
    let posted = fake_roost.spawn_request();
    let input = posted.get("input").and_then(Value::as_str).unwrap();
    let message: Value = serde_json::from_str(input).unwrap();
    assert_eq!(
        message["payload"]["objective"],
        "{\"task\":\"fallback task\"}"
    );
}

#[test]
fn test_if_condition_skips_step_on_false() {
    let dir = tempdir().unwrap();
    let invoke = |_name: &str, _input: ToolInput| {
        Ok(tool_result(
            ToolStatus::Passed,
            Some("ok".to_string()),
            None,
        ))
    };
    let plan = write_plan(
        dir.path(),
        json!({
            "id":"p",
            "steps":[
                {"id":"a","tool":"echo"},
                {"id":"b","tool":"echo","depends_on":["a"],"if":"$a.status == 'failed'"}
            ]
        }),
    );

    let report = plan::execute(&plan, &ctx(dir.path().to_path_buf(), &invoke));

    assert!(report.completed);
    assert_eq!(find(&report, "b").status, StepStatus::Skipped);
}

#[test]
fn test_validation_rejects_unresolvable_tool() {
    let dir = tempdir().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&calls);
    let invoke = |_name: &str, _input: ToolInput| {
        seen.fetch_add(1, Ordering::SeqCst);
        Ok(tool_result(
            ToolStatus::Passed,
            Some("unused".to_string()),
            None,
        ))
    };
    let plan = write_plan(
        dir.path(),
        json!({"id":"p","steps":[{"id":"bad","tool":"nonexistent"}]}),
    );

    let report = plan::execute(&plan, &ctx(dir.path().to_path_buf(), &invoke));

    assert!(!report.completed);
    assert_eq!(report.failed_step.as_deref(), Some("bad"));
    assert_eq!(find(&report, "bad").status, StepStatus::Failed);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn test_on_error_continue_completes_plan() {
    let dir = tempdir().unwrap();
    let invoke = |name: &str, _input: ToolInput| {
        if name == "fail" {
            Ok(tool_result(
                ToolStatus::Failed,
                None,
                Some("nope".to_string()),
            ))
        } else {
            Ok(tool_result(
                ToolStatus::Passed,
                Some("ok".to_string()),
                None,
            ))
        }
    };
    let plan = write_plan(
        dir.path(),
        json!({
            "id":"p",
            "steps":[
                {"id":"a","tool":"fail","on_error":"continue"},
                {"id":"b","tool":"echo","depends_on":["a"]}
            ]
        }),
    );

    let report = plan::execute(&plan, &ctx(dir.path().to_path_buf(), &invoke));

    assert!(report.completed);
    assert_eq!(find(&report, "a").status, StepStatus::Failed);
    assert_eq!(find(&report, "b").status, StepStatus::Success);
}

#[test]
fn test_on_error_fail_aborts_plan() {
    let dir = tempdir().unwrap();
    let invoke = |_name: &str, _input: ToolInput| {
        Ok(tool_result(
            ToolStatus::Failed,
            None,
            Some("nope".to_string()),
        ))
    };
    let plan = write_plan(
        dir.path(),
        json!({
            "id":"p",
            "steps":[
                {"id":"a","tool":"fail","on_error":"fail"},
                {"id":"b","tool":"echo","depends_on":["a"]}
            ]
        }),
    );

    let report = plan::execute(&plan, &ctx(dir.path().to_path_buf(), &invoke));

    assert!(!report.completed);
    assert_eq!(report.failed_step.as_deref(), Some("a"));
    assert!(report.results.iter().all(|result| result.step_id != "b"));
}

#[test]
fn test_retries_before_error_policy() {
    let dir = tempdir().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&calls);
    let invoke = move |_name: &str, _input: ToolInput| {
        let attempt = seen.fetch_add(1, Ordering::SeqCst);
        if attempt < 2 {
            Ok(tool_result(
                ToolStatus::Failed,
                None,
                Some("retry".to_string()),
            ))
        } else {
            Ok(tool_result(
                ToolStatus::Passed,
                Some("ok".to_string()),
                None,
            ))
        }
    };
    let plan = write_plan(
        dir.path(),
        json!({"id":"p","steps":[{"id":"a","tool":"flaky","retries":2}]}),
    );

    let report = plan::execute(&plan, &ctx(dir.path().to_path_buf(), &invoke));

    assert!(report.completed);
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert_eq!(find(&report, "a").status, StepStatus::Success);
}

#[test]
fn test_join_point_waits_for_multiple_upstreams() {
    let dir = tempdir().unwrap();
    let invoke = |_name: &str, input: ToolInput| {
        let data = input.data.unwrap_or_default();
        let output = if data.contains("left") && data.contains("right") {
            "joined"
        } else if data.contains("side\":\"left") {
            "left"
        } else {
            "right"
        };
        Ok(tool_result(
            ToolStatus::Passed,
            Some(output.to_string()),
            None,
        ))
    };
    let plan = write_plan(
        dir.path(),
        json!({
            "id":"p",
            "steps":[
                {"id":"left","tool":"echo","input":{"side":"left"}},
                {"id":"right","tool":"echo","input":{"side":"right"}},
                {"id":"join","tool":"echo","depends_on":["left","right"],"input":{"l":"$left.output","r":"$right.output"}}
            ]
        }),
    );

    let report = plan::execute(&plan, &ctx(dir.path().to_path_buf(), &invoke));

    assert!(report.completed);
    assert_eq!(find(&report, "join").output.as_deref(), Some("joined"));
}

struct FakeRoost {
    url: String,
    request: Arc<Mutex<Option<Value>>>,
    join: Option<thread::JoinHandle<()>>,
}

impl FakeRoost {
    fn start(base_dir: &Path) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");
        let output_path = base_dir.join("fake-worker-output");
        fs::create_dir_all(output_path.join("out")).unwrap();
        fs::write(
            output_path.join("out/result.json"),
            json!({
                "schema": "murmur.message.v1",
                "type": "murmur.code_task.result.v1",
                "job_id": Value::Null,
                "payload": {
                    "status": Value::Null,
                    "summary": Value::Null,
                    "files": Value::Null,
                    "output": "worker-output"
                }
            })
            .to_string(),
        )
        .unwrap();

        let request = Arc::new(Mutex::new(None));
        let captured = Arc::clone(&request);
        let join = thread::spawn(move || {
            let (mut spawn_stream, _) = listener.accept().unwrap();
            let spawn_body = read_http_request_body(&mut spawn_stream);
            let spawn_json: Value = serde_json::from_str(&spawn_body).unwrap();
            *captured.lock().unwrap() = Some(spawn_json);
            write_http_json(&mut spawn_stream, &json!({"job_id": "job-1"}));
            drop(spawn_stream);

            let (mut status_stream, _) = listener.accept().unwrap();
            let _ = read_http_request_body(&mut status_stream);
            write_http_json(
                &mut status_stream,
                &json!({"status": "complete", "output_path": output_path}),
            );
            drop(status_stream);
        });

        Self {
            url,
            request,
            join: Some(join),
        }
    }

    fn spawn_request(&self) -> Value {
        self.request.lock().unwrap().clone().unwrap()
    }
}

impl Drop for FakeRoost {
    fn drop(&mut self) {
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn roost_env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn read_http_request_body(stream: &mut TcpStream) -> String {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4096];
    let header_end;
    loop {
        let read = stream.read(&mut chunk).unwrap();
        if read == 0 {
            return String::new();
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(index) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            header_end = index;
            break;
        }
    }

    let headers = String::from_utf8_lossy(&buffer[..header_end]);
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            (key.eq_ignore_ascii_case("content-length"))
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);

    let mut body = buffer[header_end + 4..].to_vec();
    while body.len() < content_length {
        let read = stream.read(&mut chunk).unwrap();
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }

    String::from_utf8_lossy(&body[..content_length]).to_string()
}

fn write_http_json(stream: &mut TcpStream, body: &Value) {
    let body = body.to_string();
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(response.as_bytes()).unwrap();
    stream.flush().unwrap();
}

#[test]
fn test_cycle_is_reported_without_running_steps() {
    let dir = tempdir().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&calls);
    let invoke = move |_name: &str, _input: ToolInput| {
        seen.fetch_add(1, Ordering::SeqCst);
        Ok(tool_result(
            ToolStatus::Passed,
            Some("unused".to_string()),
            None,
        ))
    };
    let plan = write_plan(
        dir.path(),
        json!({
            "id":"p",
            "steps":[
                {"id":"a","tool":"echo","depends_on":["b"]},
                {"id":"b","tool":"echo","depends_on":["a"]}
            ]
        }),
    );

    let report = plan::execute(&plan, &ctx(dir.path().to_path_buf(), &invoke));

    assert!(!report.completed);
    assert_eq!(report.failed_step.as_deref(), Some("a"));
    assert_eq!(find(&report, "a").status, StepStatus::Failed);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}
