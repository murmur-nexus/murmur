use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{self, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex, OnceLock,
    },
    thread,
    time::{Duration, Instant},
};

use capsule_runtime::{
    bindings::host::murmur::tool::run::{Status as ToolStatus, ToolInput, ToolResult},
    plan::{self, SchedulerContext, StepStatus},
    CapabilityPolicy, SpawnCredential, SPAWN_APPROVAL_HEADER, SPAWN_CREDENTIAL_HEADER,
};
use serde_json::{json, Value};
use tempfile::tempdir;

/// A credential distinctive enough that a substring search over a whole workdir tree, and over
/// every step result, is a meaningful search.
const TEST_CREDENTIAL: &str = "msc1.CREDENTIALmustNEVERleakZZZ99.testsignature";

/// The session id the delegating context asks as.
const TEST_SESSION: &str = "ses_00000000000000000000000000000test";

/// The `shell_allow` grant below is what makes every test built on this context need a delegated
/// cgroup v2 scope: `plan::execute` bounds the plan's whole subprocess tree before running a step,
/// and fails closed when it cannot.
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
        current_session_id: Some(TEST_SESSION.to_string()),
        spawn_credential: Some(SpawnCredential::new(TEST_CREDENTIAL.to_string())),
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
    if capsule_runtime::skip_without_host_support("test_parallel_tool_steps_run_concurrently") {
        return;
    }
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
    if capsule_runtime::skip_without_host_support("test_dependent_step_receives_upstream_output") {
        return;
    }
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
    if capsule_runtime::skip_without_host_support("test_shell_step_executes") {
        return;
    }
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

/// A plain objective reaches the child capsule as plain text.
///
/// The child pushes whatever arrives straight at its model, and nothing parses a task envelope,
/// so wrapping the objective in one would only hand the child JSON to decode before it can start.
#[test]
fn test_capsule_step_sends_objective_as_plain_text() {
    if capsule_runtime::skip_without_host_support("test_capsule_step_sends_objective_as_plain_text")
    {
        return;
    }
    let _guard = roost_env_lock().lock().unwrap();
    let dir = tempdir().unwrap();
    let fake_roost = FakeRoost::start();
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
    // The spawn call names the capsule to start and nothing about the task; the task itself
    // rides the A2A message that follows.
    let spawned = fake_roost.spawn_request();
    assert_eq!(spawned["name"], "worker");
    assert!(spawned.get("input").is_none(), "{spawned}");
    assert_eq!(fake_roost.sent_text(), "Echo this task");
}

/// Input this runtime does not model is passed through as the author's own JSON rather than
/// flattened into one field, so nothing they wrote is dropped on the way to the child.
#[test]
fn test_capsule_step_passes_unmodelled_input_through_as_json() {
    if capsule_runtime::skip_without_host_support(
        "test_capsule_step_passes_unmodelled_input_through_as_json",
    ) {
        return;
    }
    let _guard = roost_env_lock().lock().unwrap();
    let dir = tempdir().unwrap();
    let fake_roost = FakeRoost::start();
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
    assert_eq!(fake_roost.sent_text(), "{\"task\":\"fallback task\"}");
}

/// Only `instructions` and `context` alongside an `objective` force the whole input through as
/// JSON. Any other companion key leaves the objective delivered as plain text, so a plan author
/// adding a key this runtime does not model does not silently change how the objective arrives.
#[test]
fn test_capsule_step_objective_survives_an_unmodelled_companion_key() {
    if capsule_runtime::skip_without_host_support(
        "test_capsule_step_objective_survives_an_unmodelled_companion_key",
    ) {
        return;
    }
    let _guard = roost_env_lock().lock().unwrap();
    let dir = tempdir().unwrap();
    let fake_roost = FakeRoost::start();
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
            "steps":[{
                "id":"worker",
                "capsule":"worker",
                "input":{"objective":"Echo this task","render_as":"json"}
            }]
        }),
    );

    let report = plan::execute(&plan, &ctx(dir.path().to_path_buf(), &invoke));
    std::env::remove_var("MURMUR_ROOST_URL");

    assert!(report.completed, "{report:?}");
    assert_eq!(fake_roost.sent_text(), "Echo this task");
}

/// `instructions` beside an `objective` sends the whole input as JSON.
#[test]
fn test_capsule_step_instructions_force_json_passthrough() {
    if capsule_runtime::skip_without_host_support(
        "test_capsule_step_instructions_force_json_passthrough",
    ) {
        return;
    }
    let _guard = roost_env_lock().lock().unwrap();
    let dir = tempdir().unwrap();
    let fake_roost = FakeRoost::start();
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
            "steps":[{
                "id":"worker",
                "capsule":"worker",
                "input":{"objective":"Echo this task","instructions":"Be brief"}
            }]
        }),
    );

    let report = plan::execute(&plan, &ctx(dir.path().to_path_buf(), &invoke));
    std::env::remove_var("MURMUR_ROOST_URL");

    assert!(report.completed, "{report:?}");
    assert_eq!(
        fake_roost.sent_text(),
        "{\"instructions\":\"Be brief\",\"objective\":\"Echo this task\"}"
    );
}

#[test]
fn test_if_condition_skips_step_on_false() {
    if capsule_runtime::skip_without_host_support("test_if_condition_skips_step_on_false") {
        return;
    }
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
    if capsule_runtime::skip_without_host_support("test_on_error_continue_completes_plan") {
        return;
    }
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
    if capsule_runtime::skip_without_host_support("test_on_error_fail_aborts_plan") {
        return;
    }
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
    if capsule_runtime::skip_without_host_support("test_retries_before_error_policy") {
        return;
    }
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
    if capsule_runtime::skip_without_host_support("test_join_point_waits_for_multiple_upstreams") {
        return;
    }
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

/// The two tokens a delegated spawn travels on reach the daemon in headers, in the right order,
/// and the approval presented is the one the delegation returned.
#[test]
fn test_capsule_step_delegates_before_it_spawns() {
    if capsule_runtime::skip_without_host_support("test_capsule_step_delegates_before_it_spawns") {
        return;
    }
    let _guard = roost_env_lock().lock().unwrap();
    let dir = tempdir().unwrap();
    let fake_roost = FakeRoost::start();
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
        json!({"id":"p","steps":[{"id":"worker","capsule":"worker","input":{"objective":"go"}}]}),
    );

    let report = plan::execute(&plan, &ctx(dir.path().to_path_buf(), &invoke));
    std::env::remove_var("MURMUR_ROOST_URL");

    assert!(report.completed, "{report:?}");
    let paths: Vec<String> = fake_roost
        .requests()
        .into_iter()
        .map(|request| request.path)
        .collect();
    assert_eq!(paths, vec!["/delegate".to_string(), "/spawn".to_string()]);

    // The delegation asks about one capsule, by name and version, and proves who is asking.
    let delegate = fake_roost.request("/delegate");
    assert_eq!(delegate.body["name"], "worker");
    assert_eq!(delegate.body["version"], "0.1.0");
    assert_eq!(
        delegate
            .headers
            .get(SPAWN_CREDENTIAL_HEADER)
            .map(String::as_str),
        Some(TEST_CREDENTIAL)
    );
    assert!(!delegate.headers.contains_key(SPAWN_APPROVAL_HEADER));

    // The launch re-presents the credential alongside the approval it just earned.
    let spawn = fake_roost.request("/spawn");
    assert_eq!(
        spawn
            .headers
            .get(SPAWN_CREDENTIAL_HEADER)
            .map(String::as_str),
        Some(TEST_CREDENTIAL)
    );
    assert_eq!(
        spawn.headers.get(SPAWN_APPROVAL_HEADER).map(String::as_str),
        Some(TEST_APPROVAL)
    );
    assert_eq!(spawn.body["spawned_by"], TEST_SESSION);
}

/// A session that was never granted a credential cannot delegate, and says so before it opens a
/// connection.
#[test]
fn test_capsule_step_without_a_credential_asks_for_nothing() {
    if capsule_runtime::skip_without_host_support(
        "test_capsule_step_without_a_credential_asks_for_nothing",
    ) {
        return;
    }
    let _guard = roost_env_lock().lock().unwrap();
    let dir = tempdir().unwrap();
    let fake_roost = FakeRoost::start();
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
        json!({"id":"p","steps":[{"id":"worker","capsule":"worker","input":{"objective":"go"}}]}),
    );

    let mut context = ctx(dir.path().to_path_buf(), &invoke);
    context.spawn_credential = None;
    let report = plan::execute(&plan, &context);
    std::env::remove_var("MURMUR_ROOST_URL");

    assert!(!report.completed, "{report:?}");
    let error = find(&report, "worker").error.clone().unwrap();
    assert!(error.contains("holds no spawn credential"), "{error}");
    assert!(error.contains("capabilities.spawn.allow"), "{error}");
    assert!(
        fake_roost.requests().is_empty(),
        "{:?}",
        fake_roost
            .requests()
            .into_iter()
            .map(|request| request.path)
            .collect::<Vec<_>>()
    );
}

/// The credential the runtime presents is unreadable by the agent: it reaches no file under the
/// workdir and no step result, on the success path and on the client's error path alike.
#[test]
fn test_the_spawn_credential_reaches_no_file_and_no_step_result() {
    if capsule_runtime::skip_without_host_support(
        "test_the_spawn_credential_reaches_no_file_and_no_step_result",
    ) {
        return;
    }
    assert_eq!(
        format!("{:?}", SpawnCredential::new(TEST_CREDENTIAL.to_string())),
        "SpawnCredential(<redacted>)"
    );

    for refuse_spawn in [false, true] {
        let _guard = roost_env_lock().lock().unwrap();
        let dir = tempdir().unwrap();
        let fake_roost = if refuse_spawn {
            FakeRoost::refusing_spawn()
        } else {
            FakeRoost::start()
        };
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
                "steps":[{"id":"worker","capsule":"worker","input":{"objective":"go"}}]
            }),
        );

        let report = plan::execute(&plan, &ctx(dir.path().to_path_buf(), &invoke));
        std::env::remove_var("MURMUR_ROOST_URL");

        assert_eq!(report.completed, !refuse_spawn, "{report:?}");
        for result in &report.results {
            for text in [result.output.as_deref(), result.error.as_deref()]
                .into_iter()
                .flatten()
            {
                assert!(
                    !text.contains(TEST_CREDENTIAL) && !text.contains(TEST_APPROVAL),
                    "a step result carried token material (refuse_spawn={refuse_spawn}): {text}",
                );
            }
        }
        for (path, contents) in read_tree(dir.path()) {
            assert!(
                !contents.contains(TEST_CREDENTIAL) && !contents.contains(TEST_APPROVAL),
                "{} carried token material (refuse_spawn={refuse_spawn})",
                path.display(),
            );
        }
    }
}

/// Every file beneath `root`, read as lossy UTF-8 so a binary file is still searchable.
fn read_tree(root: &Path) -> Vec<(PathBuf, String)> {
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir(root) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(read_tree(&path));
        } else if let Ok(bytes) = fs::read(&path) {
            found.push((path, String::from_utf8_lossy(&bytes).to_string()));
        }
    }
    found
}

/// The approval this fake's `/delegate` hands back, and therefore the value its `/spawn` must be
/// presented with.
const TEST_APPROVAL: &str = "msa1.APPROVALmustNEVERleakYY88.testsignature";

/// One request the fake received, kept whole so a test can assert on headers as well as body.
#[derive(Clone)]
struct RecordedRequest {
    path: String,
    headers: HashMap<String, String>,
    body: Value,
}

/// A stand-in for mur-roost *and* the capsule it spawns, served on one loopback listener.
///
/// `dispatch_capsule_step` speaks two protocols against this, in order: `POST /delegate` and then
/// `POST /spawn` to mur-roost, which answers with the URL of the now-live capsule, then A2A
/// JSON-RPC against that URL — `message/send` to hand over the task, then `tasks/get` polled until
/// the state is terminal. All are served here, routed on the request path.
///
/// The server loop polls for connections rather than parking in a blocking `accept`, and every
/// read it makes is bounded, so `drop` can always stop it. A fake that could only be stopped by
/// the client making exactly the sequence of calls it expected would turn every failed
/// assertion into a hung test instead of a reported one — the panic unwinds into `drop`, which
/// then joins a thread waiting on a connection that is never coming.
struct FakeRoost {
    url: String,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    sent_text: Arc<Mutex<Option<String>>>,
    shutdown: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

impl FakeRoost {
    fn start() -> Self {
        Self::start_with(false)
    }

    /// A fake whose `/spawn` refuses with `403` and echoes the request it received back in the
    /// body, so the client's error path carries as much of the exchange as it ever could.
    fn refusing_spawn() -> Self {
        Self::start_with(true)
    }

    fn start_with(refuse_spawn: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{addr}");
        let capsule_url = format!("{url}/capsule");

        let requests = Arc::new(Mutex::new(Vec::new()));
        let sent_text = Arc::new(Mutex::new(None));
        let shutdown = Arc::new(AtomicBool::new(false));

        let join = thread::spawn({
            let requests = Arc::clone(&requests);
            let sent_text = Arc::clone(&sent_text);
            let shutdown = Arc::clone(&shutdown);
            move || {
                while !shutdown.load(Ordering::SeqCst) {
                    let mut stream = match listener.accept() {
                        Ok((stream, _)) => stream,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                            continue;
                        }
                        Err(_) => break,
                    };
                    // `accept` on a non-blocking listener may hand back a non-blocking socket.
                    // The request reader wants a blocking one, but time-bounded, so a client
                    // that opens a connection and then says nothing cannot wedge the loop.
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();

                    let (path, headers, body) = read_http_request(&mut stream);
                    let request: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                    if path == "/delegate" || path == "/spawn" {
                        requests.lock().unwrap().push(RecordedRequest {
                            path: path.clone(),
                            headers,
                            body: request.clone(),
                        });
                    }
                    let (status, response) = match path.as_str() {
                        "/delegate" => (
                            200,
                            json!({"approval": TEST_APPROVAL, "expires_at_ms": 1_u64 << 42}),
                        ),
                        "/spawn" if refuse_spawn => (
                            403,
                            json!({
                                "error": "spawn refused by the fake",
                                "request": {"path": path, "body": request},
                            }),
                        ),
                        "/spawn" => (200, json!({ "capsule_url": capsule_url })),
                        _ => {
                            let id = request.get("id").cloned().unwrap_or(Value::Null);
                            let body = match request.get("method").and_then(Value::as_str) {
                                Some("message/send") => {
                                    *sent_text.lock().unwrap() = Some(
                                        request["params"]["message"]["parts"][0]["text"]
                                            .as_str()
                                            .unwrap_or_default()
                                            .to_string(),
                                    );
                                    json!({"jsonrpc": "2.0", "id": id, "result": {"id": "task-1"}})
                                }
                                Some("tasks/get") => json!({
                                    "jsonrpc": "2.0",
                                    "id": id,
                                    "result": {
                                        "id": "task-1",
                                        "status": {"state": "completed"},
                                        "artifacts": [{"parts": [{"text": "worker-output"}]}]
                                    }
                                }),
                                other => panic!("unexpected request {other:?} at {path}"),
                            };
                            (200, body)
                        }
                    };
                    write_http_json(&mut stream, status, &response);
                }
            }
        });

        Self {
            url,
            requests,
            sent_text,
            shutdown,
            join: Some(join),
        }
    }

    /// Every `/delegate` and `/spawn` the fake received, in order.
    fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().unwrap().clone()
    }

    /// The one request the fake received at `path`.
    fn request(&self, path: &str) -> RecordedRequest {
        self.requests()
            .into_iter()
            .find(|request| request.path == path)
            .unwrap_or_else(|| panic!("no {path} request was made"))
    }

    /// The body of the `POST /spawn` that asked mur-roost for the capsule.
    fn spawn_request(&self) -> Value {
        self.request("/spawn").body
    }

    /// The task text the step handed to the capsule over A2A `message/send`, exactly as it
    /// travelled in the single text part. This is where a capsule step's input lives — the spawn
    /// call above carries only the capsule's identity and workdir. Returned unparsed: what the
    /// child's model receives is this string, so that is what the tests assert on.
    fn sent_text(&self) -> String {
        self.sent_text.lock().unwrap().clone().unwrap()
    }
}

impl Drop for FakeRoost {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn roost_env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Read one HTTP request, returning its request-target path, its headers (keyed lowercase) and
/// its body.
fn read_http_request(stream: &mut TcpStream) -> (String, HashMap<String, String>, String) {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4096];
    let header_end;
    loop {
        let read = stream.read(&mut chunk).unwrap();
        if read == 0 {
            return (String::new(), HashMap::new(), String::new());
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(index) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            header_end = index;
            break;
        }
    }

    let headers = String::from_utf8_lossy(&buffer[..header_end]);
    let path = headers
        .lines()
        .next()
        .and_then(|request_line| request_line.split_whitespace().nth(1))
        .unwrap_or_default()
        .to_string();
    let header_map: HashMap<String, String> = headers
        .lines()
        .skip(1)
        .filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            Some((key.trim().to_ascii_lowercase(), value.trim().to_string()))
        })
        .collect();
    let content_length = header_map
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);

    let mut body = buffer[header_end + 4..].to_vec();
    while body.len() < content_length {
        let read = stream.read(&mut chunk).unwrap();
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }

    (
        path,
        header_map,
        String::from_utf8_lossy(&body[..content_length]).to_string(),
    )
}

fn write_http_json(stream: &mut TcpStream, status: u16, body: &Value) {
    let reason = if status == 200 { "OK" } else { "Forbidden" };
    let body = body.to_string();
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(response.as_bytes()).unwrap();
    stream.flush().unwrap();
}

#[test]
fn test_cycle_is_reported_without_running_steps() {
    if capsule_runtime::skip_without_host_support("test_cycle_is_reported_without_running_steps") {
        return;
    }
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
