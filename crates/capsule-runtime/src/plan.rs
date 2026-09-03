use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    thread,
    time::Instant,
};

use serde::Deserialize;
use serde_json::Value;

use murmur_artifact::DEFAULT_EXPORT_MAX_BYTES;

use crate::{
    bindings::host::murmur::tool::run::{Status as ToolStatus, ToolInput, ToolResult},
    delegation_plane::{DelegationOrigin, DelegationPlane, DelegationRequest, DelegationStatus},
    errors::RuntimeError,
    resource_plane::{self, SymlinkPolicy},
    sandbox,
    shell::{execute_shell, split_shell_words},
    spawn_credential::SpawnCredential,
    trace::{PlanEndRecord, PlanStepRecord, PlanStepShape, PlanTraceAppender},
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

impl StepKind {
    /// The name a trace line carries for this kind. `"capsule"` rather than `"spawn"`: a trace
    /// names the plan key its author wrote, not the host's word for what it does.
    fn as_str(self) -> &'static str {
        match self {
            StepKind::Tool => "tool",
            StepKind::Shell => "shell",
            StepKind::Spawn => "capsule",
        }
    }
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
    /// Where this run's per-step lifecycle records go, or `None` to record nothing.
    ///
    /// Emission is opt-in at the call site: with `None` no file is opened and no line is
    /// written, and the run's `ExecutionReport` is identical either way.
    pub trace: Option<&'a PlanTraceAppender>,
    pub invoke_tool: &'a (dyn Fn(&str, ToolInput) -> Result<ToolResult, String> + Sync),
}

pub fn execute(plan_path: &Path, ctx: &SchedulerContext<'_>) -> ExecutionReport {
    let started = Instant::now();
    match execute_inner(plan_path, ctx, started) {
        Ok(report) => report,
        // The only failures that reach here are a plan file that could not be read or parsed, so
        // there is no DAG to have described and no plan node for the summary to hang off.
        Err((failed_step, error)) => {
            if let Some(appender) = ctx.trace {
                appender.write_plan_end(
                    None,
                    PlanEndRecord {
                        plan_id: String::new(),
                        outcome: "failed",
                        failed_step: Some(failed_step.clone()),
                        steps_total: 0,
                        steps_succeeded: 0,
                        steps_failed: 0,
                        steps_skipped: 0,
                        duration_ms: elapsed_ms(started),
                        reason: Some(error.clone()),
                    },
                );
            }
            ExecutionReport {
                results: vec![StepResult {
                    step_id: failed_step.clone(),
                    status: StepStatus::Failed,
                    output: None,
                    error: Some(error),
                }],
                completed: false,
                failed_step: Some(failed_step),
            }
        }
    }
}

fn execute_inner(
    plan_path: &Path,
    ctx: &SchedulerContext<'_>,
    started: Instant,
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

    // The DAG is described the moment the file parses, before it is validated: a plan the
    // validator refuses still has a structure worth reading, and the refusal is only legible
    // beside the steps it names.
    let plan_node = ctx
        .trace
        .map(|appender| appender.write_plan_start(&plan.id, plan_shape(&plan)));

    if let Err((step_id, error)) = validate_plan(&plan, ctx) {
        trace_plan_end(
            ctx,
            plan_node.as_deref(),
            PlanEndRecord {
                plan_id: plan.id.clone(),
                outcome: "failed",
                failed_step: Some(step_id.clone()),
                steps_total: plan.steps.len(),
                steps_succeeded: 0,
                steps_failed: 0,
                steps_skipped: 0,
                duration_ms: elapsed_ms(started),
                reason: Some(error.clone()),
            },
        );
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
            let error =
                crate::errors::RuntimeError::CgroupDelegationUnavailable { reason }.to_string();
            return Ok(refused_before_any_step(
                &plan,
                ctx,
                plan_node.as_deref(),
                started,
                error,
            ));
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
            let error = format!("failed to resolve shell subprocess sandbox enforcement: {error}");
            return Ok(refused_before_any_step(
                &plan,
                ctx,
                plan_node.as_deref(),
                started,
                error,
            ));
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
                    // Never dispatched, so it writes no `plan_step_start` and no attempt: a step
                    // that did not run must not read like one that did.
                    trace_undispatched(ctx, plan_node.as_deref(), &plan.id, step, "skipped", None);
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
                    trace_undispatched(
                        ctx,
                        plan_node.as_deref(),
                        &plan.id,
                        step,
                        "failed",
                        Some(error.clone()),
                    );
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
            const BLOCKED: &str = "plan is blocked by a dependency cycle or missing dependency";
            let blocked_step = plan
                .steps
                .iter()
                .find(|step| !results.contains_key(&step.id));
            let blocked = blocked_step
                .map(|step| step.id.clone())
                .unwrap_or_else(|| "plan".to_string());
            if let Some(step) = blocked_step {
                trace_undispatched(
                    ctx,
                    plan_node.as_deref(),
                    &plan.id,
                    step,
                    "failed",
                    Some(BLOCKED.to_string()),
                );
            }
            insert_result(
                &mut results,
                &mut result_order,
                StepResult {
                    step_id: blocked.clone(),
                    status: StepStatus::Failed,
                    output: None,
                    error: Some(BLOCKED.to_string()),
                },
            );
            failed_step = Some(blocked);
            break;
        }

        for step in &ready {
            trace_step_start(ctx, plan_node.as_deref(), &plan.id, step);
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
                completed.push(handle.join().unwrap_or_else(|_| {
                    StepOutcome::undispatched(failed("unknown", "step dispatch thread panicked"))
                }));
            }
        });

        for outcome in completed {
            let Some(step) = plan
                .steps
                .iter()
                .find(|step| step.id == outcome.result.step_id)
            else {
                // A dispatch thread that panicked names no step this plan declared.
                trace_step(
                    ctx,
                    plan_node.as_deref(),
                    PlanStepRecord {
                        plan_id: plan.id.clone(),
                        step_id: outcome.result.step_id.clone(),
                        kind: "unknown",
                        status: outcome.result.status.as_str(),
                        attempts: outcome.attempts,
                        duration_ms: outcome.duration_ms,
                        error: outcome.result.error.clone(),
                        input: None,
                        state_effect: None,
                        resource_id: None,
                    },
                );
                insert_result(&mut results, &mut result_order, outcome.result);
                failed_step = Some("unknown".to_string());
                break;
            };

            let kind = step_kind_name(step);
            let StepOutcome {
                result,
                attempts,
                duration_ms,
                input,
                state_effect,
                resource_id,
            } = outcome;

            // The `on_error` policy is applied here, before the step is recorded either way, so
            // the trace and the returned report can never disagree about what a step settled as.
            let (settled, stops) = if result.status == StepStatus::Failed {
                match step.on_error.as_str() {
                    "fail" => (result, true),
                    "skip" => (
                        StepResult {
                            step_id: result.step_id,
                            status: StepStatus::Skipped,
                            output: None,
                            error: result.error,
                        },
                        false,
                    ),
                    "continue" => (result, false),
                    policy => (
                        StepResult {
                            error: Some(format!("invalid on_error policy '{policy}'")),
                            ..result
                        },
                        true,
                    ),
                }
            } else {
                (result, false)
            };

            trace_step(
                ctx,
                plan_node.as_deref(),
                PlanStepRecord {
                    plan_id: plan.id.clone(),
                    step_id: settled.step_id.clone(),
                    kind,
                    status: settled.status.as_str(),
                    attempts,
                    duration_ms,
                    error: settled.error.clone(),
                    input,
                    state_effect,
                    resource_id,
                },
            );

            let id = settled.step_id.clone();
            insert_result(&mut results, &mut result_order, settled);
            if stops {
                failed_step = Some(id);
                break;
            }
        }
    }

    let ordered_results = result_order
        .into_iter()
        .filter_map(|id| results.remove(&id))
        .collect::<Vec<_>>();

    trace_plan_end(
        ctx,
        plan_node.as_deref(),
        PlanEndRecord {
            plan_id: plan.id.clone(),
            outcome: if failed_step.is_none() {
                "completed"
            } else {
                "failed"
            },
            failed_step: failed_step.clone(),
            steps_total: plan.steps.len(),
            steps_succeeded: count_status(&ordered_results, StepStatus::Success),
            steps_failed: count_status(&ordered_results, StepStatus::Failed),
            steps_skipped: count_status(&ordered_results, StepStatus::Skipped),
            duration_ms: elapsed_ms(started),
            reason: None,
        },
    );

    Ok(ExecutionReport {
        results: ordered_results,
        completed: failed_step.is_none(),
        failed_step,
    })
}

// ── Trace emission ───────────────────────────────────────────────────────────

/// The name a step's trace lines carry for its kind. `"unknown"` only for a step declaring
/// none or several of `tool`/`shell`/`capsule`, which `validate_plan` refuses — it can reach a
/// line only through `plan_start`, which is written before validation.
fn step_kind_name(step: &StepDef) -> &'static str {
    infer_kind(step).map(StepKind::as_str).unwrap_or("unknown")
}

/// The DAG as authored, for `plan_start`.
fn plan_shape(plan: &PlanFile) -> Vec<PlanStepShape> {
    plan.steps
        .iter()
        .map(|step| PlanStepShape {
            step_id: step.id.clone(),
            kind: step_kind_name(step),
            depends_on: step.depends_on.clone(),
            has_condition: step.condition.is_some(),
        })
        .collect()
}

fn trace_step_start(
    ctx: &SchedulerContext<'_>,
    plan_node: Option<&str>,
    plan_id: &str,
    step: &StepDef,
) {
    if let (Some(appender), Some(node)) = (ctx.trace, plan_node) {
        appender.write_plan_step_start(
            node,
            plan_id,
            &step.id,
            step_kind_name(step),
            step.depends_on.clone(),
        );
    }
}

fn trace_step(ctx: &SchedulerContext<'_>, plan_node: Option<&str>, record: PlanStepRecord) {
    if let (Some(appender), Some(node)) = (ctx.trace, plan_node) {
        appender.write_plan_step(node, record);
    }
}

/// The terminal line for a step that settled without ever being dispatched: a false `if`, a
/// condition that would not evaluate, a step the DAG left unreachable. No attempt was made, so
/// none is claimed.
fn trace_undispatched(
    ctx: &SchedulerContext<'_>,
    plan_node: Option<&str>,
    plan_id: &str,
    step: &StepDef,
    status: &'static str,
    error: Option<String>,
) {
    trace_step(
        ctx,
        plan_node,
        PlanStepRecord {
            plan_id: plan_id.to_string(),
            step_id: step.id.clone(),
            kind: step_kind_name(step),
            status,
            attempts: 0,
            duration_ms: 0,
            error,
            input: None,
            state_effect: None,
            resource_id: None,
        },
    );
}

fn trace_plan_end(ctx: &SchedulerContext<'_>, plan_node: Option<&str>, record: PlanEndRecord) {
    if let Some(appender) = ctx.trace {
        appender.write_plan_end(plan_node, record);
    }
}

/// The report and the trace for a plan the host refused before any step ran: a cgroup scope it
/// could not delegate, a shell sandbox it could not resolve. The DAG is already described, so
/// the summary hangs off the plan node and names the plan — only the reason is new.
fn refused_before_any_step(
    plan: &PlanFile,
    ctx: &SchedulerContext<'_>,
    plan_node: Option<&str>,
    started: Instant,
    error: String,
) -> ExecutionReport {
    trace_plan_end(
        ctx,
        plan_node,
        PlanEndRecord {
            plan_id: plan.id.clone(),
            outcome: "failed",
            failed_step: Some("plan".to_string()),
            steps_total: plan.steps.len(),
            steps_succeeded: 0,
            steps_failed: 0,
            steps_skipped: 0,
            duration_ms: elapsed_ms(started),
            reason: Some(error.clone()),
        },
    );
    ExecutionReport {
        results: vec![StepResult {
            step_id: "plan".to_string(),
            status: StepStatus::Failed,
            output: None,
            error: Some(error),
        }],
        completed: false,
        failed_step: Some("plan".to_string()),
    }
}

fn count_status(results: &[StepResult], status: StepStatus) -> usize {
    results
        .iter()
        .filter(|result| result.status == status)
        .count()
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis() as u64
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

                // The input shape is settled before any step runs, so a plan that could only
                // hand its child an envelope is refused without taking a cgroup scope, without
                // spawning anything, and without the earlier steps having already had effects.
                if let Err(error) = capsule_task_text(&step.input) {
                    return Err((step.id.clone(), error));
                }
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

/// What a worker thread reports back to the scheduler loop about the step it ran: the verdict,
/// plus the facts only the worker observed and the terminal `plan_step` line needs.
///
/// [`execute_step_once`] returns one attempt's own `attempts: 1` and duration;
/// [`execute_step_with_retries`] overwrites both with the totals for the step.
struct StepOutcome {
    result: StepResult,
    attempts: u32,
    duration_ms: u64,
    /// The interpolated input the attempt dispatched. `Some` for a tool step; `None` for every
    /// other kind, and for a failure that never reached a dispatch.
    input: Option<Value>,
    /// What the tool declared about this call in `tool-result.metadata`. Both `None` for every
    /// kind but a tool step, and for a tool that declared nothing.
    state_effect: Option<String>,
    resource_id: Option<String>,
}

impl StepOutcome {
    /// One attempt of a step that declares nothing about itself: every kind but a tool step, and
    /// every failure that never reached a dispatch.
    fn plain(result: StepResult, started: Instant) -> Self {
        Self {
            result,
            attempts: 1,
            duration_ms: elapsed_ms(started),
            input: None,
            state_effect: None,
            resource_id: None,
        }
    }

    /// A verdict reached without a dispatch, and so without an attempt to count or time.
    fn undispatched(result: StepResult) -> Self {
        Self {
            result,
            attempts: 0,
            duration_ms: 0,
            input: None,
            state_effect: None,
            resource_id: None,
        }
    }
}

fn execute_step_with_retries(
    step: &StepDef,
    ctx: &SchedulerContext<'_>,
    results: &HashMap<String, StepResult>,
    enforcement: &sandbox::ShellEnforcement,
) -> StepOutcome {
    let started = Instant::now();
    let attempts = step.retries.saturating_add(1);
    let mut last = None;
    for attempt in 1..=attempts {
        let mut outcome = execute_step_once(step, ctx, results, enforcement);
        outcome.attempts = attempt;
        outcome.duration_ms = elapsed_ms(started);
        if outcome.result.status != StepStatus::Failed {
            return outcome;
        }
        last = Some(outcome);
    }

    last.unwrap_or_else(|| StepOutcome::undispatched(failed(&step.id, "step was not attempted")))
}

fn execute_step_once(
    step: &StepDef,
    ctx: &SchedulerContext<'_>,
    results: &HashMap<String, StepResult>,
    enforcement: &sandbox::ShellEnforcement,
) -> StepOutcome {
    let started = Instant::now();
    let mut input = step.input.clone();
    if let Err(error) = interpolate_value(&mut input, results) {
        return StepOutcome::plain(failed(&step.id, error), started);
    }
    let input_json = serde_json::to_string(&input).unwrap_or_else(|_| "{}".to_string());

    match infer_kind(step) {
        Ok(StepKind::Tool) => {
            let (result, state_effect, resource_id) = dispatch_tool_step(step, ctx, input_json);
            StepOutcome {
                result,
                attempts: 1,
                duration_ms: elapsed_ms(started),
                // A step declaring no `input` interpolates to JSON null, which would write
                // `"input": null` rather than omitting the key. Absent means absent.
                input: (!input.is_null()).then_some(input),
                state_effect,
                resource_id,
            }
        }
        Ok(StepKind::Shell) => {
            StepOutcome::plain(dispatch_shell_step(step, ctx, enforcement), started)
        }
        Ok(StepKind::Spawn) => StepOutcome::plain(dispatch_capsule_step(step, ctx, input), started),
        Err(error) => StepOutcome::plain(failed(&step.id, error), started),
    }
}

/// Dispatches one tool step, and returns the verdict beside the `state_effect` and `resource_id`
/// the tool declared about the call. Both are read through the agent loop's own extractors, so
/// a plan step and an agent turn read one tool's self-description the same way.
fn dispatch_tool_step(
    step: &StepDef,
    ctx: &SchedulerContext<'_>,
    input_json: String,
) -> (StepResult, Option<String>, Option<String>) {
    let name = step.tool.as_deref().unwrap_or_default();
    match (ctx.invoke_tool)(
        name,
        ToolInput {
            data: Some(input_json),
            log_path: None,
        },
    ) {
        Ok(result) => {
            let state_effect = crate::agent::extract_state_effect(&result.metadata);
            let resource_id = crate::agent::extract_resource_id(&result.metadata);
            let verdict = if matches!(result.status, ToolStatus::Passed) {
                match tool_step_output(name, &ctx.workdir, result.data, result.data_path.as_deref())
                {
                    Ok(output) => StepResult {
                        step_id: step.id.clone(),
                        status: StepStatus::Success,
                        output,
                        error: None,
                    },
                    Err(error) => failed(&step.id, error),
                }
            } else {
                failed(
                    &step.id,
                    result
                        .summary
                        .or(result.data)
                        .unwrap_or_else(|| "tool step failed".to_string()),
                )
            };
            (verdict, state_effect, resource_id)
        }
        Err(error) => (failed(&step.id, error), None, None),
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
    // `validate_plan` already accepted this step's un-interpolated input, and interpolation
    // replaces a string with a string, so the refusal here is unreachable through `execute`.
    let task = match capsule_task_text(&input) {
        Ok(task) => task,
        Err(error) => return failed(&step.id, error),
    };
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
    // A plan step reads no manifest of its own here, so it delegates under the default bound
    // rather than under a declared one.
    let plane = DelegationPlane::new(
        roost_url,
        credential.clone(),
        ctx.workdir.clone(),
        ctx.current_session_id.clone().unwrap_or_default(),
        crate::delegation_plane::DELEGATION_RESULT_TIMEOUT,
    );
    // A plan step holds no conversation id and no trace appender, so its child is launched
    // without a handle and the step writes no delegation records — the same as before lineage
    // existed. The agent-facing `delegate-task` tool is where both are recorded.
    let result = plane.delegate(
        &DelegationRequest {
            capsule: capsule.to_string(),
            // A plan names a capsule; the version comes from the context the plan was validated
            // against, and a context that names none means `0.1.0`.
            version: ctx
                .capsule_versions
                .get(capsule)
                .cloned()
                .unwrap_or_else(|| "0.1.0".to_string()),
            task,
        },
        &DelegationOrigin::default(),
    );

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

/// The task text a capsule step hands to the child capsule, or the reason the step's `input`
/// cannot become task text.
///
/// Whatever this returns becomes the child's first user message verbatim: `DelegationPlane`
/// sends it as the sole text part of an A2A `message/send`, the receiving runtime writes that
/// text to `task.md`, and `agent::read_task` pushes it at the model. Nothing on the receiving
/// side parses a task envelope, so an envelope is not structure — it is prose that happens to be
/// JSON, and it spends the child's first turn on decoding. A capsule step's `input` is therefore
/// the task text and nothing else, in one of two shapes:
///
/// | `input` | task text |
/// | --- | --- |
/// | a JSON string | that string, verbatim |
/// | `{ "objective": "<text>" }`, and no other key | `<text>`, verbatim |
///
/// Every other shape is refused rather than serialized into the message. Interpolation replaces
/// a string with a string and so cannot change the shape, which is what lets `validate_plan`
/// refuse the whole plan before a cgroup scope is taken or a child is spawned.
fn capsule_task_text(input: &Value) -> Result<String, String> {
    const ACCEPTS: &str = "capsule step input must be a string or { \"objective\": \"<text>\" }";
    const NO_INPUT: &str = "capsule step has no input; its input is the task text the child \
                            receives, given as a string or as { \"objective\": \"<text>\" }";

    match input {
        Value::String(text) => Ok(text.clone()),
        Value::Object(map) => {
            // serde_json's map is a BTreeMap or an IndexMap depending on the `preserve_order`
            // feature, so the listed keys are sorted here rather than taken in iteration order.
            let mut unknown = map
                .keys()
                .filter(|key| key.as_str() != "objective")
                .map(String::as_str)
                .collect::<Vec<_>>();
            unknown.sort_unstable();
            if !unknown.is_empty() {
                return Err(format!(
                    "capsule step input has keys the child cannot receive: {}; nothing on the \
                     receiving side parses a task envelope, so put the whole instruction in \
                     \"objective\"",
                    unknown.join(", ")
                ));
            }

            match map.get("objective") {
                Some(Value::String(text)) => Ok(text.clone()),
                Some(_) => Err("capsule step input \"objective\" must be a string".to_string()),
                None => Err(ACCEPTS.to_string()),
            }
        }
        Value::Null => Err(NO_INPUT.to_string()),
        Value::Bool(_) => Err(format!("{ACCEPTS}, not a boolean")),
        Value::Number(_) => Err(format!("{ACCEPTS}, not a number")),
        Value::Array(_) => Err(format!("{ACCEPTS}, not an array")),
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

/// The output of a tool step that passed: in-band `data` when the tool sent it, otherwise the
/// file the tool named in `data_path`.
///
/// `data_path` is the escape hatch for a result too large to carry in band — the system prompt
/// tells the model to read it when `truncated` is set — so a path the host cannot honour is
/// resolved against the guest's own boundary rather than dropped from the interface. The
/// resolution is [`resource_plane::read_file_beneath_root`] against `workdir`, the directory the
/// tool's `.` means, and a refusal fails the step: a step whose declared output could not be read
/// did not succeed.
///
/// The symlink policy is [`SymlinkPolicy::Refuse`] whatever containment class the session
/// achieved. `resource_plane::symlink_policy` follows symlinks under `sealed` and `advisory`
/// because a capsule reading its own export root has no outside to name; this read runs in the
/// host process, which sits outside every class's boundary with ambient filesystem authority, so
/// a symlink under the workdir pointing at `/etc/shadow` would be followed successfully at any
/// class.
fn tool_step_output(
    tool: &str,
    workdir: &Path,
    data: Option<String>,
    data_path: Option<&str>,
) -> Result<Option<String>, String> {
    if let Some(data) = data {
        return Ok(Some(data));
    }
    let Some(relpath) = data_path else {
        return Ok(None);
    };
    match resource_plane::read_file_beneath_root(
        workdir,
        relpath,
        DEFAULT_EXPORT_MAX_BYTES,
        SymlinkPolicy::Refuse,
    ) {
        Ok(response) => Ok(Some(String::from_utf8_lossy(&response.bytes).into_owned())),
        Err(error) => Err(RuntimeError::ToolDataPathRefused {
            tool: tool.to_string(),
            path: relpath.to_string(),
            code: error.code().to_string(),
            detail: error.message(),
        }
        .to_string()),
    }
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
            trace: None,
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
    fn capsule_task_text_delivers_a_bare_string_verbatim() {
        assert_eq!(
            capsule_task_text(&json!("do the thing")).unwrap(),
            "do the thing"
        );
    }

    #[test]
    fn capsule_task_text_delivers_a_lone_objective_verbatim() {
        assert_eq!(
            capsule_task_text(&json!({"objective": "do the thing"})).unwrap(),
            "do the thing"
        );
    }

    /// An empty task is a well-shaped one. What a child does with nothing to do is the child's
    /// business, not this contract's.
    #[test]
    fn capsule_task_text_accepts_empty_task_text() {
        assert_eq!(capsule_task_text(&json!("")).unwrap(), "");
        assert_eq!(capsule_task_text(&json!({"objective": ""})).unwrap(), "");
    }

    #[test]
    fn capsule_task_text_refuses_a_task_envelope() {
        let error = capsule_task_text(&json!({"task": "hello"})).unwrap_err();
        assert!(
            error.contains("keys the child cannot receive: task"),
            "{error}"
        );
        assert!(error.contains("\"objective\""), "{error}");
    }

    #[test]
    fn capsule_task_text_lists_unaccepted_keys_in_sorted_order() {
        let error =
            capsule_task_text(&json!({"render_as": "json", "objective": "x", "instructions": "y"}))
                .unwrap_err();
        assert!(
            error.contains("keys the child cannot receive: instructions, render_as"),
            "{error}"
        );
    }

    #[test]
    fn capsule_task_text_refuses_a_non_string_objective() {
        for input in [json!({"objective": 7}), json!({"objective": null})] {
            assert_eq!(
                capsule_task_text(&input).unwrap_err(),
                "capsule step input \"objective\" must be a string"
            );
        }
    }

    #[test]
    fn capsule_task_text_refuses_an_object_without_an_objective() {
        assert_eq!(
            capsule_task_text(&json!({})).unwrap_err(),
            "capsule step input must be a string or { \"objective\": \"<text>\" }"
        );
    }

    #[test]
    fn capsule_task_text_names_the_type_it_was_handed() {
        assert!(capsule_task_text(&json!(7))
            .unwrap_err()
            .ends_with("not a number"));
        assert!(capsule_task_text(&json!(true))
            .unwrap_err()
            .ends_with("not a boolean"));
        assert!(capsule_task_text(&json!(["a"]))
            .unwrap_err()
            .ends_with("not an array"));
    }

    /// `StepDef::input` defaults to `Value::Null`, so an omitted `input` and an explicit null
    /// are the same value and get the same refusal.
    #[test]
    fn capsule_task_text_refuses_an_absent_input() {
        let error = capsule_task_text(&Value::Null).unwrap_err();
        assert_eq!(
            error,
            "capsule step has no input; its input is the task text the child receives, given as \
             a string or as { \"objective\": \"<text>\" }"
        );
    }

    /// A `$reference` is a string, so both accepted shapes survive load-time validation and are
    /// resolved at dispatch.
    #[test]
    fn capsule_task_text_accepts_an_uninterpolated_reference() {
        assert_eq!(
            capsule_task_text(&json!("$analyse.output")).unwrap(),
            "$analyse.output"
        );
        assert_eq!(
            capsule_task_text(&json!({"objective": "$analyse.output"})).unwrap(),
            "$analyse.output"
        );
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
