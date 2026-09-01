use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    thread,
};

use serde::Deserialize;
use serde_json::Value;

use crate::{
    bindings::host::murmur::tool::run::{Status as ToolStatus, ToolInput, ToolResult},
    delegation_plane::{DelegationPlane, DelegationRequest, DelegationStatus},
    sandbox,
    shell::{execute_shell, split_shell_words},
    spawn_credential::SpawnCredential,
    types::CapabilityPolicy,
};

pub mod condition;

#[derive(Debug, Deserialize)]
struct PlanFile {
    id: String,
    steps: Vec<StepDef>,
}

#[derive(Debug, Clone, Deserialize)]
struct StepDef {
    id: String,
    tool: Option<String>,
    shell: Option<String>,
    capsule: Option<String>,
    #[serde(default)]
    input: Value,
    #[serde(default)]
    depends_on: Vec<String>,
    #[serde(rename = "if")]
    condition: Option<String>,
    #[serde(default = "default_on_error")]
    on_error: String,
    #[serde(default)]
    retries: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepKind {
    Tool,
    Shell,
    Spawn,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepStatus {
    Success,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepResult {
    pub step_id: String,
    pub status: StepStatus,
    pub output: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionReport {
    pub results: Vec<StepResult>,
    pub completed: bool,
    pub failed_step: Option<String>,
}

pub struct SchedulerContext<'a> {
    pub workdir: PathBuf,
    pub capability_policy: CapabilityPolicy,
    pub installed_tools: HashSet<String>,
    pub capsule_versions: HashMap<String, String>,
    pub current_session_id: Option<String>,
    /// The credential this session presents to `mur-roost` when a `capsule` step delegates.
    ///
    /// `None` for every session that was not granted `capabilities.spawn.allow`, and a `capsule`
    /// step in such a session fails before any request is made. The token is held here and read
    /// exactly twice — into the two request headers below — so nothing that formats this context,
    /// a step result or a trace record can carry it.
    pub spawn_credential: Option<SpawnCredential>,
    pub invoke_tool: &'a (dyn Fn(&str, ToolInput) -> Result<ToolResult, String> + Sync),
}

pub fn execute(plan_path: &Path, ctx: &SchedulerContext<'_>) -> ExecutionReport {
    match execute_inner(plan_path, ctx) {
        Ok(report) => report,
        Err((failed_step, error)) => ExecutionReport {
            results: vec![StepResult {
                step_id: failed_step.clone(),
                status: StepStatus::Failed,
                output: None,
                error: Some(error),
            }],
            completed: false,
            failed_step: Some(failed_step),
        },
    }
}

fn execute_inner(
    plan_path: &Path,
    ctx: &SchedulerContext<'_>,
) -> Result<ExecutionReport, (String, String)> {
    let raw = fs::read_to_string(plan_path).map_err(|error| {
        (
            "plan".to_string(),
            format!("failed to read plan file {}: {error}", plan_path.display()),
        )
    })?;
    let plan: PlanFile = serde_json::from_str(&raw).map_err(|error| {
        (
            "plan".to_string(),
            format!("failed to parse plan JSON: {error}"),
        )
    })?;

    if let Err((step_id, error)) = validate_plan(&plan, ctx) {
        return Ok(ExecutionReport {
            results: vec![StepResult {
                step_id: step_id.clone(),
                status: StepStatus::Failed,
                output: None,
                error: Some(error),
            }],
            completed: false,
            failed_step: Some(step_id.clone()),
        });
    }

    // Aggregate process bounding for this plan's shell steps, on the same fail-closed terms as
    // `runtime::launch_session`: a plan that can run a subprocess on Linux does not start at all
    // unless its tree can be put in a cgroup scope. `has_native_artifact` is `false` here — a
    // plan step reaches native binaries only through `invoke_tool`, which routes back into the
    // session that already established its own scope.
    let cgroup_scope = match crate::cgroup::prepare_scope(
        crate::cgroup::requires_process_bounding(&ctx.capability_policy, false),
        &ctx.capability_policy.resources,
        &plan.id,
        &ctx.workdir,
    ) {
        Ok(scope) => scope,
        Err(reason) => {
            return Ok(ExecutionReport {
                results: vec![StepResult {
                    step_id: "plan".to_string(),
                    status: StepStatus::Failed,
                    output: None,
                    error: Some(
                        crate::errors::RuntimeError::CgroupDelegationUnavailable { reason }
                            .to_string(),
                    ),
                }],
                completed: false,
                failed_step: Some("plan".to_string()),
            });
        }
    };
    let workdir_guard = Some(crate::resources::WorkdirGuard::spawn(
        &ctx.workdir,
        ctx.capability_policy.resources.workdir_max_bytes,
    ));

    // Host-probed kernel enforcement tier + resolved network allowlist for this plan's shell
    // steps — resolved once, up front, same cadence as `runtime::launch_session`.
    let shell_enforcement = match sandbox::ShellEnforcement::resolve(
        &ctx.capability_policy,
        ctx.capability_policy.containment_floor,
        sandbox::HostProbe::probe(),
    ) {
        Ok(enforcement) => enforcement.with_host_bounding(cgroup_scope, workdir_guard),
        Err(error) => {
            return Ok(ExecutionReport {
                results: vec![StepResult {
                    step_id: "plan".to_string(),
                    status: StepStatus::Failed,
                    output: None,
                    error: Some(format!(
                        "failed to resolve shell subprocess sandbox enforcement: {error}"
                    )),
                }],
                completed: false,
                failed_step: Some("plan".to_string()),
            });
        }
    };

    let mut results = HashMap::<String, StepResult>::new();
    let mut result_order = Vec::<String>::new();
    let mut failed_step = None;

    loop {
        if results.len() == plan.steps.len() || failed_step.is_some() {
            break;
        }

        let mut ready = Vec::new();
        let mut progressed = false;

        for step in &plan.steps {
            if results.contains_key(&step.id) {
                continue;
            }
            if !step.depends_on.iter().all(|dep| results.contains_key(dep)) {
                continue;
            }

            match condition::evaluate(step.condition.as_deref(), &results) {
                Ok(true) => ready.push(step.clone()),
                Ok(false) => {
                    insert_result(
                        &mut results,
                        &mut result_order,
                        StepResult {
                            step_id: step.id.clone(),
                            status: StepStatus::Skipped,
                            output: None,
                            error: None,
                        },
                    );
                    progressed = true;
                }
                Err(error) => {
                    insert_result(
                        &mut results,
                        &mut result_order,
                        StepResult {
                            step_id: step.id.clone(),
                            status: StepStatus::Failed,
                            output: None,
                            error: Some(error),
                        },
                    );
                    failed_step = Some(step.id.clone());
                    progressed = true;
                    break;
                }
            }
        }

        if failed_step.is_some() {
            break;
        }

        if ready.is_empty() {
            if progressed {
                continue;
            }
            let blocked = plan
                .steps
                .iter()
                .find(|step| !results.contains_key(&step.id))
                .map(|step| step.id.clone())
                .unwrap_or_else(|| "plan".to_string());
            insert_result(
                &mut results,
                &mut result_order,
                StepResult {
                    step_id: blocked.clone(),
                    status: StepStatus::Failed,
                    output: None,
                    error: Some(
                        "plan is blocked by a dependency cycle or missing dependency".to_string(),
                    ),
                },
            );
            failed_step = Some(blocked);
            break;
        }

        let mut completed = Vec::new();
        thread::scope(|scope| {
            let mut handles = Vec::new();
            for step in ready {
                let snapshot = results.clone();
                let shell_enforcement = &shell_enforcement;
                handles.push(scope.spawn(move || {
                    execute_step_with_retries(&step, ctx, &snapshot, shell_enforcement)
                }));
            }
            for handle in handles {
                completed.push(handle.join().unwrap_or_else(|_| StepResult {
                    step_id: "unknown".to_string(),
                    status: StepStatus::Failed,
                    output: None,
                    error: Some("step dispatch thread panicked".to_string()),
                }));
            }
        });

        for result in completed {
            let Some(step) = plan.steps.iter().find(|step| step.id == result.step_id) else {
                insert_result(&mut results, &mut result_order, result);
                failed_step = Some("unknown".to_string());
                break;
            };

            if result.status == StepStatus::Failed {
                match step.on_error.as_str() {
                    "fail" => {
                        let id = result.step_id.clone();
                        insert_result(&mut results, &mut result_order, result);
                        failed_step = Some(id);
                        break;
                    }
                    "skip" => insert_result(
                        &mut results,
                        &mut result_order,
                        StepResult {
                            step_id: result.step_id,
                            status: StepStatus::Skipped,
                            output: None,
                            error: result.error,
                        },
                    ),
                    "continue" => insert_result(&mut results, &mut result_order, result),
                    _ => {
                        let id = result.step_id.clone();
                        insert_result(
                            &mut results,
                            &mut result_order,
                            StepResult {
                                error: Some(format!("invalid on_error policy '{}'", step.on_error)),
                                ..result
                            },
                        );
                        failed_step = Some(id);
                        break;
                    }
                }
            } else {
                insert_result(&mut results, &mut result_order, result);
            }
        }
    }

    let ordered_results = result_order
        .into_iter()
        .filter_map(|id| results.remove(&id))
        .collect::<Vec<_>>();

    Ok(ExecutionReport {
        results: ordered_results,
        completed: failed_step.is_none(),
        failed_step,
    })
}

fn validate_plan(plan: &PlanFile, ctx: &SchedulerContext<'_>) -> Result<(), (String, String)> {
    let mut ids = HashSet::new();
    for step in &plan.steps {
        if !ids.insert(step.id.clone()) {
            return Err((step.id.clone(), format!("duplicate step id '{}'", step.id)));
        }

        match infer_kind(step) {
            Ok(StepKind::Tool) => {
                let tool = step.tool.as_ref().unwrap();
                if !ctx.installed_tools.contains(tool) {
                    return Err((step.id.clone(), format!("tool '{tool}' is not installed")));
                }
            }
            Ok(StepKind::Shell) => {
                let Some(binary) = shell_binary(step.shell.as_deref().unwrap_or_default()) else {
                    return Err((step.id.clone(), "shell command is empty".to_string()));
                };
                if !ctx.capability_policy.shell_allow.contains(&binary) {
                    return Err((
                        step.id.clone(),
                        format!("binary '{binary}' is not in capabilities.shell.allow"),
                    ));
                }
            }
            Ok(StepKind::Spawn) => {
                // spawn_allow is enforced by mur-roost from the parent's CapabilityPolicy.
                // No local check here — the capsule cannot self-authorize its own spawn rights.
            }
            Err(error) => return Err((step.id.clone(), error)),
        }

        if !matches!(step.on_error.as_str(), "fail" | "skip" | "continue") {
            return Err((
                step.id.clone(),
                format!("invalid on_error policy '{}'", step.on_error),
            ));
        }
    }

    for step in &plan.steps {
        for dep in &step.depends_on {
            if !ids.contains(dep) {
                return Err((step.id.clone(), format!("unknown dependency '{dep}'")));
            }
        }
        validate_references(step, &ids)?;
    }

    Ok(())
}

fn validate_references(step: &StepDef, ids: &HashSet<String>) -> Result<(), (String, String)> {
    let mut references = Vec::new();
    collect_references(&step.input, &mut references);
    if let Some(condition) = &step.condition {
        for token in condition.split_whitespace() {
            if token.starts_with('$') {
                references.push(token.trim_matches(|c| c == '(' || c == ')').to_string());
            }
        }
    }

    for reference in references {
        if let Some((step_id, field)) = parse_reference(&reference) {
            if !ids.contains(step_id) {
                return Err((step.id.clone(), format!("unknown reference '{reference}'")));
            }
            if field != "output" && field != "status" {
                return Err((
                    step.id.clone(),
                    format!("unknown reference field '{field}'"),
                ));
            }
        }
    }

    Ok(())
}

fn collect_references(value: &Value, references: &mut Vec<String>) {
    match value {
        Value::String(value) if value.starts_with('$') => references.push(value.clone()),
        Value::Array(values) => values
            .iter()
            .for_each(|value| collect_references(value, references)),
        Value::Object(map) => map
            .values()
            .for_each(|value| collect_references(value, references)),
        _ => {}
    }
}

fn execute_step_with_retries(
    step: &StepDef,
    ctx: &SchedulerContext<'_>,
    results: &HashMap<String, StepResult>,
    enforcement: &sandbox::ShellEnforcement,
) -> StepResult {
    let attempts = step.retries.saturating_add(1);
    let mut last = None;
    for _ in 0..attempts {
        let result = execute_step_once(step, ctx, results, enforcement);
        if result.status != StepStatus::Failed {
            return result;
        }
        last = Some(result);
    }

    last.unwrap_or_else(|| StepResult {
        step_id: step.id.clone(),
        status: StepStatus::Failed,
        output: None,
        error: Some("step was not attempted".to_string()),
    })
}

fn execute_step_once(
    step: &StepDef,
    ctx: &SchedulerContext<'_>,
    results: &HashMap<String, StepResult>,
    enforcement: &sandbox::ShellEnforcement,
) -> StepResult {
    let mut input = step.input.clone();
    if let Err(error) = interpolate_value(&mut input, results) {
        return failed(&step.id, error);
    }
    let input_json = serde_json::to_string(&input).unwrap_or_else(|_| "{}".to_string());

    match infer_kind(step) {
        Ok(StepKind::Tool) => dispatch_tool_step(step, ctx, input_json),
        Ok(StepKind::Shell) => dispatch_shell_step(step, ctx, enforcement),
        Ok(StepKind::Spawn) => dispatch_capsule_step(step, ctx, input),
        Err(error) => failed(&step.id, error),
    }
}

fn dispatch_tool_step(
    step: &StepDef,
    ctx: &SchedulerContext<'_>,
    input_json: String,
) -> StepResult {
    let name = step.tool.as_deref().unwrap_or_default();
    match (ctx.invoke_tool)(
        name,
        ToolInput {
            data: Some(input_json),
            log_path: None,
        },
    ) {
        Ok(result) if matches!(result.status, ToolStatus::Passed) => StepResult {
            step_id: step.id.clone(),
            status: StepStatus::Success,
            output: result
                .data
                .or_else(|| read_optional_path(result.data_path.as_deref())),
            error: None,
        },
        Ok(result) => failed(
            &step.id,
            result
                .summary
                .or(result.data)
                .unwrap_or_else(|| "tool step failed".to_string()),
        ),
        Err(error) => failed(&step.id, error),
    }
}

fn dispatch_shell_step(
    step: &StepDef,
    ctx: &SchedulerContext<'_>,
    enforcement: &sandbox::ShellEnforcement,
) -> StepResult {
    let command = step.shell.as_deref().unwrap_or_default();
    let Some((binary, args)) = parse_shell_command(command) else {
        return failed(&step.id, "shell command is empty");
    };
    let arg_refs = args.iter().map(String::as_str).collect::<Vec<_>>();

    match execute_shell(
        &binary,
        &arg_refs,
        &[],
        &ctx.workdir,
        &ctx.capability_policy,
        enforcement,
    ) {
        Ok(result) if result.exit_code == 0 => StepResult {
            step_id: step.id.clone(),
            status: StepStatus::Success,
            output: Some(result.stdout),
            error: None,
        },
        // A step killed for exceeding a host resource limit fails with the limit named, not with
        // whatever the dying process managed to write to stderr — for `SIGXCPU`/`SIGXFSZ` and the
        // cgroup kills that is usually nothing at all, which would otherwise report an empty
        // error for a step that was very deliberately terminated.
        Ok(result) => match result.resource_limit_hit {
            Some(limit) => failed(
                &step.id,
                crate::errors::RuntimeError::ShellResourceLimitExceeded {
                    binary: result.binary,
                    limit,
                    detail: if result.stderr.trim().is_empty() {
                        format!("exit code {}", result.exit_code)
                    } else {
                        result.stderr
                    },
                }
                .to_string(),
            ),
            None => failed(&step.id, result.stderr),
        },
        // Including a sealed composed-root failure, which fails the step carrying its full
        // named text. Unlike the agent turn loop (which owns a session and ends it — see
        // `agent::run_agent_loop`), this scheduler owns nothing above the step it is running,
        // so naming the cause in the step's error is the whole of what it can do.
        Err(error) => failed(&step.id, error.to_string()),
    }
}

fn dispatch_capsule_step(step: &StepDef, ctx: &SchedulerContext<'_>, input: Value) -> StepResult {
    let capsule = step.capsule.as_deref().unwrap_or_default();
    let roost_url = match std::env::var("MURMUR_ROOST_URL") {
        Ok(value) if !value.trim().is_empty() => value,
        _ => {
            return failed(
                &step.id,
                "MURMUR_ROOST_URL is not set; capsule steps require mur-roost",
            )
        }
    };
    let Some(credential) = ctx.spawn_credential.as_ref() else {
        return failed(
            &step.id,
            "this session holds no spawn credential; a capsule step requires one, and mur-roost \
             mints it only for a session whose manifest declares capabilities.spawn.allow",
        );
    };

    // Everything a delegation is — the referee's approval, the child's process, the task delivery
    // and the bounded wait — belongs to `DelegationPlane`, and a capsule step is input and output
    // marshalling around it. The agent-facing `delegate-task` tool calls the same code, so there
    // is one delegation mechanism in this crate rather than two that drift.
    let plane = DelegationPlane::new(roost_url, credential.clone(), ctx.workdir.clone());
    let result = plane.delegate(&DelegationRequest {
        capsule: capsule.to_string(),
        // A plan names a capsule; the version comes from the context the plan was validated
        // against, and `0.1.0` is what a context that names none has always meant here.
        version: ctx
            .capsule_versions
            .get(capsule)
            .cloned()
            .unwrap_or_else(|| "0.1.0".to_string()),
        task: capsule_step_input_text(input),
    });

    match result.status {
        DelegationStatus::Completed => StepResult {
            step_id: step.id.clone(),
            status: StepStatus::Success,
            output: Some(result.output),
            error: None,
        },
        _ => failed(&step.id, result.output),
    }
}

/// The task text a capsule step hands to the child capsule.
///
/// Whatever this returns becomes the child's user message verbatim: the receiving side pushes
/// the delivered text straight at the model (`agent::process`), and nothing anywhere parses a
/// task envelope. Wrapping the objective in one therefore does not deliver structure — it
/// delivers prose that happens to be JSON, and spends the child's first turn on decoding it.
/// So a plain `objective` is sent as plain text.
///
/// Any richer shape is passed through as its own JSON rather than flattened to one field, so a
/// plan carrying data this runtime does not model still delivers everything its author wrote.
/// The routing design that would give such structure a real reader — mur-roost staging a workdir
/// the worker reads from — is still unbuilt; see
/// `.nexus/roadmap/dag-capsule-step-workdir-and-input-routing-gap.md`.
fn capsule_step_input_text(input: Value) -> String {
    let bare_objective = input.get("objective").and_then(Value::as_str).filter(|_| {
        ["instructions", "context"]
            .iter()
            .all(|key| input.get(key).is_none_or(Value::is_null))
    });

    match bare_objective {
        Some(objective) => objective.to_string(),
        None => serde_json::to_string(&input).unwrap_or_default(),
    }
}

fn interpolate_value(
    value: &mut Value,
    results: &HashMap<String, StepResult>,
) -> Result<(), String> {
    match value {
        Value::String(text) if text.starts_with('$') => {
            *text = condition::resolve_reference(text, results)?;
        }
        Value::Array(values) => {
            for value in values {
                interpolate_value(value, results)?;
            }
        }
        Value::Object(map) => {
            for value in map.values_mut() {
                interpolate_value(value, results)?;
            }
        }
        _ => {}
    }

    Ok(())
}

fn infer_kind(step: &StepDef) -> Result<StepKind, String> {
    let count =
        step.tool.is_some() as u8 + step.shell.is_some() as u8 + step.capsule.is_some() as u8;
    match count {
        1 if step.tool.is_some() => Ok(StepKind::Tool),
        1 if step.shell.is_some() => Ok(StepKind::Shell),
        1 if step.capsule.is_some() => Ok(StepKind::Spawn),
        0 => Err("step must declare exactly one of tool, shell, or capsule".to_string()),
        _ => Err("step declares more than one of tool, shell, or capsule".to_string()),
    }
}

fn default_on_error() -> String {
    "fail".to_string()
}

fn failed(step_id: &str, error: impl Into<String>) -> StepResult {
    StepResult {
        step_id: step_id.to_string(),
        status: StepStatus::Failed,
        output: None,
        error: Some(error.into()),
    }
}

fn insert_result(
    results: &mut HashMap<String, StepResult>,
    order: &mut Vec<String>,
    result: StepResult,
) {
    order.push(result.step_id.clone());
    results.insert(result.step_id.clone(), result);
}

fn shell_binary(command: &str) -> Option<String> {
    parse_shell_command(command).map(|(binary, _)| binary)
}

fn parse_shell_command(command: &str) -> Option<(String, Vec<String>)> {
    let parts = split_shell_words(command);
    let (binary, args) = parts.split_first()?;
    Some((binary.clone(), args.to_vec()))
}

fn parse_reference(reference: &str) -> Option<(&str, &str)> {
    reference.strip_prefix('$')?.rsplit_once('.')
}

fn read_optional_path(path: Option<&str>) -> Option<String> {
    path.and_then(|path| fs::read_to_string(path).ok())
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        time::{Duration, Instant},
    };

    use serde_json::json;
    use tempfile::tempdir;

    use super::*;

    /// The `shell_allow` grant below is what makes every test built on this context need a
    /// delegated cgroup v2 scope: `execute` bounds the plan's whole subprocess tree before
    /// running a step, and fails closed when it cannot.
    fn test_ctx<'a>(
        workdir: PathBuf,
        invoke_tool: &'a (dyn Fn(&str, ToolInput) -> Result<ToolResult, String> + Sync),
    ) -> SchedulerContext<'a> {
        SchedulerContext {
            workdir,
            capability_policy: CapabilityPolicy {
                shell_allow: vec!["bash".to_string()],
                spawn_allow: vec!["worker".to_string()],
                ..CapabilityPolicy::default()
            },
            installed_tools: HashSet::from([
                "ok".to_string(),
                "fail".to_string(),
                "slow".to_string(),
                "a".to_string(),
                "b".to_string(),
            ]),
            capsule_versions: HashMap::new(),
            current_session_id: None,
            spawn_credential: None,
            invoke_tool,
        }
    }

    fn tool_result(status: ToolStatus, data: Option<String>) -> ToolResult {
        ToolResult {
            status,
            summary: None,
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

    #[test]
    fn dependent_step_receives_upstream_output() {
        if crate::cgroup::skip_without_host_support(
            "plan::tests::dependent_step_receives_upstream_output",
        ) {
            return;
        }
        let dir = tempdir().unwrap();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_tool = Arc::clone(&seen);
        let invoke = move |_name: &str, input: ToolInput| {
            seen_tool
                .lock()
                .unwrap()
                .push(input.data.unwrap_or_default());
            Ok(tool_result(
                ToolStatus::Passed,
                Some("upstream".to_string()),
            ))
        };
        let plan = write_plan(
            dir.path(),
            json!({
                "id": "p",
                "steps": [
                    {"id": "a", "tool": "ok"},
                    {"id": "b", "tool": "ok", "depends_on": ["a"], "input": {"value": "$a.output"}}
                ]
            }),
        );

        let report = execute(&plan, &test_ctx(dir.path().to_path_buf(), &invoke));
        assert!(report.completed);
        assert_eq!(report.results.len(), 2);
        assert!(seen.lock().unwrap()[1].contains("upstream"));
    }

    #[test]
    fn parallel_tool_steps_run_concurrently() {
        if crate::cgroup::skip_without_host_support(
            "plan::tests::parallel_tool_steps_run_concurrently",
        ) {
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
            Ok(tool_result(ToolStatus::Passed, Some("ok".to_string())))
        };
        let plan = write_plan(
            dir.path(),
            json!({
                "id": "p",
                "steps": [
                    {"id": "a", "tool": "slow"},
                    {"id": "b", "tool": "slow"}
                ]
            }),
        );

        let started = Instant::now();
        let report = execute(&plan, &test_ctx(dir.path().to_path_buf(), &invoke));
        assert!(report.completed);
        assert!(started.elapsed() < Duration::from_millis(280));
        assert_eq!(max_active.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn on_error_and_retries_are_applied() {
        if crate::cgroup::skip_without_host_support("plan::tests::on_error_and_retries_are_applied")
        {
            return;
        }
        let dir = tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let invoke_calls = Arc::clone(&calls);
        let invoke = move |name: &str, _input: ToolInput| {
            invoke_calls.fetch_add(1, Ordering::SeqCst);
            if name == "a" {
                Ok(tool_result(ToolStatus::Failed, None))
            } else {
                Ok(tool_result(ToolStatus::Passed, Some("ok".to_string())))
            }
        };
        let plan = write_plan(
            dir.path(),
            json!({
                "id": "p",
                "steps": [
                    {"id": "a", "tool": "a", "retries": 2, "on_error": "continue"},
                    {"id": "b", "tool": "b", "depends_on": ["a"]}
                ]
            }),
        );

        let report = execute(&plan, &test_ctx(dir.path().to_path_buf(), &invoke));
        assert!(report.completed);
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn condition_false_skips_step() {
        if crate::cgroup::skip_without_host_support("plan::tests::condition_false_skips_step") {
            return;
        }
        let dir = tempdir().unwrap();
        let invoke = |_name: &str, _input: ToolInput| {
            Ok(tool_result(ToolStatus::Passed, Some("ok".to_string())))
        };
        let plan = write_plan(
            dir.path(),
            json!({
                "id": "p",
                "steps": [
                    {"id": "a", "tool": "ok"},
                    {"id": "b", "tool": "ok", "depends_on": ["a"], "if": "$a.status == 'failed'"}
                ]
            }),
        );

        let report = execute(&plan, &test_ctx(dir.path().to_path_buf(), &invoke));
        assert!(report.completed);
        assert_eq!(report.results[1].status, StepStatus::Skipped);
    }
}
