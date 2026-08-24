use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use capsule_runtime::{
    capability_policy_from_runtime_manifest, launch_session, stage_session, ArtifactRequest,
    LockExpectation, RuntimeError, StageRequest,
};
use clap::Subcommand;
use murmur_artifact::{
    load_dotenv_non_override, load_runtime_manifest, read_lockfile, resolve_manifest_path,
    write_lockfile_atomic, ArtifactRuntime, LocalRegistry, LockedArtifact, LockedSha256,
    LockfileError, MurmurLock, Registry, LOCK_VERSION,
};
use serde::{Deserialize, Serialize};

use crate::error::{CliError, E_IO_001, E_IO_003};
use crate::registry_client::FallbackRegistry;

use super::{fail_run, lockfile_error_to_cli, runtime_manifest_error_to_cli};

const E_EVAL_001: &str = "E-EVAL-001";
const E_EVAL_002: &str = "E-EVAL-002";

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Debug, Subcommand)]
pub(crate) enum EvalCommand {
    /// Show a human-readable (or JSON) summary of a single eval file
    Show {
        /// Session ID (full or last 4+ chars as suffix), or omit for the most recent session.
        /// A literal path is also accepted for backward compatibility.
        session: Option<String>,
        /// Directory containing session subdirectories (default: ./workdir)
        #[arg(long)]
        workdir: Option<PathBuf>,
        /// Output as JSON instead of human-readable text
        #[arg(long)]
        json: bool,
    },
    /// Compare two eval files side-by-side
    Diff {
        /// Session A: full ID, last 4+ chars as suffix, or literal path
        a: String,
        /// Session B: full ID, last 4+ chars as suffix, or literal path
        b: String,
        /// Directory containing session subdirectories (default: ./workdir)
        #[arg(long)]
        workdir: Option<PathBuf>,
    },
    /// Run a capsule against each case in a dataset and collect eval scores
    Run {
        /// Path to capsule directory (containing murmur.yaml). Defaults to current directory.
        capsule: Option<PathBuf>,
        /// Path to dataset JSONL file. Defaults to workdir/eval.jsonl.
        #[arg(long)]
        dataset: Option<PathBuf>,
    },
}

// ── Record model ──────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct EventScoreRecord {
    #[allow(dead_code)]
    ts: u64,
    turn: u32,
    event_type: String,
    scorer: String,
    result: String,
    score: Option<f64>,
    #[allow(dead_code)]
    reason: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct DatasetRunRecord {
    #[allow(dead_code)]
    ts: u64,
    dataset_id: Option<String>,
    case_id: Option<String>,
    overall: String,
    scores: HashMap<String, f64>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "record_type", rename_all = "snake_case")]
enum EvalRecord {
    EventScore(EventScoreRecord),
    DatasetRun(DatasetRunRecord),
}

// ── Computed metrics ──────────────────────────────────────────────────────────

struct EvalMetrics {
    event_scores: Vec<EventScoreRecord>,
    dataset_run: Option<DatasetRunRecord>,
}

impl EvalMetrics {
    fn pass_rate_by_scorer(&self) -> HashMap<String, (u32, u32)> {
        let mut counts: HashMap<String, (u32, u32)> = HashMap::new();
        for ev in &self.event_scores {
            let entry = counts.entry(ev.scorer.clone()).or_insert((0, 0));
            if ev.result == "pass" {
                entry.0 += 1;
            } else {
                entry.1 += 1;
            }
        }
        counts
    }

    fn overall(&self) -> String {
        if let Some(run) = &self.dataset_run {
            return run.overall.clone();
        }
        let rates = self.pass_rate_by_scorer();
        if rates.is_empty() {
            return "no_scores".to_string();
        }
        let all_pass = rates.values().all(|(pass, fail)| *fail == 0 && *pass > 0);
        if all_pass {
            "pass".to_string()
        } else {
            "fail".to_string()
        }
    }
}

// ── Session resolution ────────────────────────────────────────────────────────

fn ses_entries(workdir: &Path) -> Result<Vec<String>, CliError> {
    if !workdir.exists() || !workdir.is_dir() {
        return Ok(Vec::new());
    }
    let mut entries = Vec::new();
    for entry_res in fs::read_dir(workdir).map_err(|e| {
        CliError::new(
            E_IO_003,
            format!("failed to read {}: {e}", workdir.display()),
        )
    })? {
        let entry = entry_res.map_err(|e| {
            CliError::new(
                E_IO_003,
                format!("failed to read entry in {}: {e}", workdir.display()),
            )
        })?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("ses_") && entry.path().is_dir() {
            entries.push(name);
        }
    }
    Ok(entries)
}

fn resolve_eval_session(session: Option<String>, workdir: &Path) -> Result<PathBuf, CliError> {
    match session {
        None => {
            let mut entries = ses_entries(workdir)?;
            if entries.is_empty() {
                return Err(CliError::new(
                    E_EVAL_002,
                    format!("no sessions found in workdir at {}", workdir.display()),
                ));
            }
            entries.sort();
            let latest = entries.into_iter().last().unwrap();
            Ok(workdir.join(latest).join("eval.jsonl"))
        }
        Some(s) => {
            // Literal path: contains '/' or ends with '.jsonl'
            if s.contains('/') || s.ends_with(".jsonl") {
                return Ok(PathBuf::from(&s));
            }
            // Full session ID: "ses_" prefix + 32-char hex = 36 chars total
            if s.starts_with("ses_") && s.len() == 36 {
                let path = workdir.join(&s).join("eval.jsonl");
                if !path.exists() {
                    return Err(CliError::new(
                        E_EVAL_002,
                        format!("session {} not found in {}", s, workdir.display()),
                    ));
                }
                return Ok(path);
            }
            // Suffix matching (case-insensitive)
            let suffix_lower = s.to_lowercase();
            let entries = ses_entries(workdir)?;
            let mut matches: Vec<String> = entries
                .into_iter()
                .filter(|e| e.to_lowercase().ends_with(&suffix_lower))
                .collect();
            match matches.len() {
                0 => Err(CliError::new(
                    E_EVAL_002,
                    format!(
                        "no session found matching suffix '{}' in {}",
                        s,
                        workdir.display()
                    ),
                )),
                1 => Ok(workdir.join(&matches[0]).join("eval.jsonl")),
                n => {
                    matches.sort();
                    Err(CliError::new(
                        E_EVAL_002,
                        format!(
                            "ambiguous: '{}' matches {} sessions — provide more characters\n{}",
                            s,
                            n,
                            matches
                                .iter()
                                .map(|m| format!("  {m}"))
                                .collect::<Vec<_>>()
                                .join("\n")
                        ),
                    ))
                }
            }
        }
    }
}

// ── Parsing ───────────────────────────────────────────────────────────────────

fn parse_eval_file(path: &Path) -> Result<EvalMetrics, CliError> {
    let content = fs::read_to_string(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => {
            CliError::new(E_IO_001, format!("eval file not found: {}", path.display()))
        }
        _ => CliError::new(E_IO_003, format!("failed to read {}: {e}", path.display())),
    })?;

    let mut event_scores = Vec::new();
    let mut dataset_run: Option<DatasetRunRecord> = None;

    for (i, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<EvalRecord>(line) {
            Ok(EvalRecord::EventScore(ev)) => event_scores.push(ev),
            Ok(EvalRecord::DatasetRun(run)) => dataset_run = Some(run),
            Err(err) => {
                return Err(CliError::new(
                    E_EVAL_001,
                    format!("{}:{}: {err}", path.display(), i + 1),
                ));
            }
        }
    }

    Ok(EvalMetrics {
        event_scores,
        dataset_run,
    })
}

// ── Show ──────────────────────────────────────────────────────────────────────

pub(crate) fn run_eval_show(
    session: Option<String>,
    workdir_arg: Option<PathBuf>,
    json: bool,
) -> Result<(), CliError> {
    let workdir = workdir_arg.unwrap_or_else(|| {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("workdir")
    });
    let path = resolve_eval_session(session, &workdir)?;
    let metrics = parse_eval_file(&path)?;

    if json {
        print_show_json(&metrics, &path);
    } else {
        print_show_human(&metrics, &path);
    }
    Ok(())
}

fn print_show_human(m: &EvalMetrics, path: &Path) {
    println!(
        "── Eval: {} ─────────────────────────────────────",
        path.display()
    );

    if m.event_scores.is_empty() && m.dataset_run.is_none() {
        println!("  (no scored events)");
        return;
    }

    let rates = m.pass_rate_by_scorer();
    if rates.is_empty() && m.dataset_run.is_none() {
        println!("  (no scored events)");
        return;
    }

    println!();
    println!("── Scorers ──────────────────────────────────────");
    let mut scorer_names: Vec<&String> = rates.keys().collect();
    scorer_names.sort();
    for name in &scorer_names {
        let (pass, fail) = rates[*name];
        let total = pass + fail;
        let pct = if total > 0 {
            100.0 * pass as f64 / total as f64
        } else {
            0.0
        };
        println!("  {:<24} {}/{} pass  ({:.1}%)", name, pass, total, pct);
    }

    println!();
    println!("── Overall ──────────────────────────────────────");
    println!("  result:  {}", m.overall());

    if let Some(run) = &m.dataset_run {
        if let Some(case_id) = &run.case_id {
            println!("  case:    {case_id}");
        }
        if let Some(dataset_id) = &run.dataset_id {
            println!("  dataset: {dataset_id}");
        }
        if !run.scores.is_empty() {
            println!();
            println!("── Score summary ────────────────────────────────");
            let mut scorer_names: Vec<&String> = run.scores.keys().collect();
            scorer_names.sort();
            for name in scorer_names {
                println!("  {:<24} {:.4}", name, run.scores[name]);
            }
        }
    }

    if !m.event_scores.is_empty() {
        println!();
        println!("── Worst events ─────────────────────────────────");
        let mut fails: Vec<&EventScoreRecord> = m
            .event_scores
            .iter()
            .filter(|e| e.result == "fail")
            .collect();
        fails.sort_by_key(|e| (e.scorer.as_str(), e.turn));
        for ev in fails.iter().take(5) {
            println!(
                "  turn {:>3}  {:12}  {:<24} {}  score={:.2}",
                ev.turn,
                ev.event_type,
                ev.scorer,
                ev.result,
                ev.score.unwrap_or(0.0)
            );
        }
        if fails.is_empty() {
            println!("  (no failing events)");
        }
    }
}

fn print_show_json(m: &EvalMetrics, _path: &Path) {
    let rates = m.pass_rate_by_scorer();
    let mut scorer_pass_rates: HashMap<String, serde_json::Value> = HashMap::new();
    for (name, (pass, fail)) in &rates {
        let total = pass + fail;
        scorer_pass_rates.insert(
            name.clone(),
            serde_json::json!({
                "pass": pass,
                "fail": fail,
                "total": total,
                "pass_rate": if total > 0 { *pass as f64 / total as f64 } else { 0.0 }
            }),
        );
    }

    let output = serde_json::json!({
        "overall": m.overall(),
        "scorers": scorer_pass_rates,
        "dataset_run": m.dataset_run,
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&output).unwrap_or_default()
    );
}

// ── Diff ──────────────────────────────────────────────────────────────────────

pub(crate) fn run_eval_diff(
    a: Option<String>,
    b: Option<String>,
    workdir: Option<PathBuf>,
) -> Result<(), CliError> {
    let workdir = workdir.unwrap_or_else(|| PathBuf::from("workdir"));
    let path_a = resolve_eval_session(a, &workdir)?;
    let path_b = resolve_eval_session(b, &workdir)?;
    let ma = parse_eval_file(&path_a)?;
    let mb = parse_eval_file(&path_b)?;
    print_diff(&ma, &mb);
    Ok(())
}

fn print_diff(a: &EvalMetrics, b: &EvalMetrics) {
    const COL: usize = 24;
    const VAL: usize = 14;

    println!(
        "{:<COL$} {:<VAL$} {:<VAL$} Delta",
        "Scorer", "Run A", "Run B"
    );
    println!(
        "{} {} {} {}",
        "─".repeat(COL),
        "─".repeat(VAL),
        "─".repeat(VAL),
        "─".repeat(26)
    );

    let rates_a = a.pass_rate_by_scorer();
    let rates_b = b.pass_rate_by_scorer();

    let mut all_scorers: HashSet<&String> = rates_a.keys().collect();
    all_scorers.extend(rates_b.keys());
    let mut sorted: Vec<&&String> = all_scorers.iter().collect();
    sorted.sort();

    for scorer in sorted {
        let fmt_rate = |(pass, fail): (u32, u32)| -> String {
            let total = pass + fail;
            if total == 0 {
                return "—".to_string();
            }
            format!("{:.1}%", 100.0 * pass as f64 / total as f64)
        };

        let va = rates_a.get(*scorer).copied().unwrap_or((0, 0));
        let vb = rates_b.get(*scorer).copied().unwrap_or((0, 0));
        let sa = fmt_rate(va);
        let sb = fmt_rate(vb);

        let delta = match (rates_a.get(*scorer), rates_b.get(*scorer)) {
            (Some(&(pa, fa)), Some(&(pb, fb))) => {
                let ta = pa + fa;
                let tb = pb + fb;
                if ta == 0 || tb == 0 {
                    "—".to_string()
                } else {
                    let ra = pa as f64 / ta as f64;
                    let rb = pb as f64 / tb as f64;
                    let diff = rb - ra;
                    if diff.abs() < 0.001 {
                        "=".to_string()
                    } else if diff > 0.0 {
                        format!("{:+.1}pp (B better)", diff * 100.0)
                    } else {
                        format!("{:+.1}pp (A better)", diff * 100.0)
                    }
                }
            }
            (None, Some(_)) => "(B only)".to_string(),
            (Some(_), None) => "(A only)".to_string(),
            (None, None) => "—".to_string(),
        };

        println!("{:<COL$} {:<VAL$} {:<VAL$} {}", scorer, sa, sb, delta);
    }

    println!();
    println!(
        "{:<COL$} {:<VAL$} {:<VAL$}",
        "overall",
        a.overall(),
        b.overall()
    );
}

// ── Run ───────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct DatasetCase {
    case_id: String,
    task_path: String,
    #[allow(dead_code)]
    expected: Option<serde_json::Value>,
}

pub(crate) fn run_eval_run(capsule: Option<&Path>, dataset: Option<&Path>) -> Result<(), CliError> {
    let session_id = "n/a".to_string();
    let mut workdir = std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("/"))
        .join("workdir");

    let capsule = capsule.unwrap_or_else(|| Path::new("."));

    let manifest_path = if capsule.is_absolute() {
        resolve_manifest_path(capsule)
    } else {
        let cwd = std::env::current_dir().map_err(|source| {
            fail_run(
                &session_id,
                &workdir,
                CliError::new(E_IO_003, format!("failed to determine cwd: {source}")),
            )
        })?;
        resolve_manifest_path(&cwd.join(capsule))
    };

    let project_dir = manifest_path
        .parent()
        .map(|p| p.to_path_buf())
        .ok_or_else(|| {
            fail_run(
                &session_id,
                &workdir,
                CliError::new(E_IO_003, "failed to determine manifest directory"),
            )
        })?;

    workdir = project_dir.join("workdir");

    // Resolve dataset — default to ./eval.jsonl (project root / cwd) if not specified
    let (resolved_dataset, dataset_explicit) = match dataset {
        Some(p) => (p.to_path_buf(), true),
        None => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
            (cwd.join("eval.jsonl"), false)
        }
    };

    load_dotenv_non_override(&project_dir).map_err(|source| {
        fail_run(
            &session_id,
            &workdir,
            CliError::new(E_IO_003, source.to_string()),
        )
    })?;

    let runtime_manifest = load_runtime_manifest(&manifest_path)
        .map_err(|err| fail_run(&session_id, &workdir, runtime_manifest_error_to_cli(err)))?;

    // Parse dataset
    let dataset_content = fs::read_to_string(&resolved_dataset).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => fail_run(
            &session_id,
            &workdir,
            CliError::new(
                E_IO_001,
                if dataset_explicit {
                    format!("dataset not found at {}", resolved_dataset.display())
                } else {
                    "no dataset found. Expected ./eval.jsonl or specify with --dataset <path>"
                        .to_string()
                },
            ),
        ),
        _ => fail_run(
            &session_id,
            &workdir,
            CliError::new(
                E_IO_003,
                format!("failed to read dataset {}: {e}", resolved_dataset.display()),
            ),
        ),
    })?;

    let mut cases: Vec<DatasetCase> = Vec::new();
    for (i, line) in dataset_content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let case: DatasetCase = serde_json::from_str(line).map_err(|err| {
            fail_run(
                &session_id,
                &workdir,
                CliError::new(
                    E_EVAL_001,
                    format!("{}:{}: {err}", resolved_dataset.display(), i + 1),
                ),
            )
        })?;
        cases.push(case);
    }

    if cases.is_empty() {
        return Err(fail_run(
            &session_id,
            &workdir,
            CliError::new(
                E_IO_003,
                format!("dataset is empty: {}", resolved_dataset.display()),
            ),
        ));
    }

    // Build common fields
    let eval_config_json = runtime_manifest
        .observability
        .as_ref()
        .and_then(|o| o.eval.as_ref())
        .and_then(|e| serde_json::to_string(e).ok());

    let dataset_id = runtime_manifest
        .observability
        .as_ref()
        .and_then(|o| o.eval.as_ref())
        .and_then(|e| e.dataset_id.clone());

    let capability_policy = capability_policy_from_runtime_manifest(&runtime_manifest);

    let mut allowlisted_tools = HashSet::new();
    let mut requested_artifacts: Vec<ArtifactRequest> = Vec::new();
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
            capabilities: artifact.capabilities.clone(),
        });
    }

    // Lockfile — required for eval runs (must already exist)
    let lock_path = project_dir.join("murmur.lock");
    let (pinned_artifacts, lock_expectations, write_lock) = match read_lockfile(&lock_path) {
        Ok(lock) => {
            let mut pinned = Vec::with_capacity(requested_artifacts.len());
            let mut expectations = Vec::with_capacity(requested_artifacts.len());
            for artifact in &requested_artifacts {
                if let Some(entry) = lock.artifact_for(&artifact.name) {
                    pinned.push(ArtifactRequest {
                        name: artifact.name.clone(),
                        version: entry.resolved_version.clone(),
                        runtime: artifact.runtime.clone(),
                        source: None,
                        on_overflow: artifact.on_overflow,
                        capabilities: artifact.capabilities.clone(),
                    });
                    expectations.push(LockExpectation {
                        name: artifact.name.clone(),
                        resolved_version: entry.resolved_version.clone(),
                        sha256_wasm: entry.sha256.wasm.clone(),
                    });
                } else {
                    pinned.push(artifact.clone());
                }
            }
            (pinned, Some(expectations), false)
        }
        Err(LockfileError::NotFound(_)) => (requested_artifacts.clone(), None, true),
        Err(error) => {
            return Err(fail_run(
                &session_id,
                &workdir,
                lockfile_error_to_cli(error),
            ));
        }
    };

    let project_registry = LocalRegistry::new(project_dir.join(".murmur").join("artifacts"));
    let global_registry = LocalRegistry::from_default_home()
        .map_err(|error| fail_run(&session_id, &workdir, CliError::from(error)))?;
    let local_registry: Arc<dyn Registry> = Arc::new(FallbackRegistry {
        primary: project_registry,
        secondary: global_registry,
    });

    println!("Running {} case(s) …", cases.len());

    let mut case_results: Vec<(String, String, PathBuf, Option<DatasetRunRecord>)> = Vec::new();
    let mut lock_written = false;

    for case in &cases {
        println!("  case: {}", case.case_id);

        let stage_request = StageRequest {
            manifest_dir: project_dir.clone(),
            capsule_name: runtime_manifest.name.clone(),
            capsule_version: runtime_manifest.version.clone(),
            capsule_component_bytes: Vec::new(),
            artifacts: pinned_artifacts.clone(),
            allowlisted_tools: allowlisted_tools.clone(),
            lock_expectations: lock_expectations.clone(),
            capability_policy: capability_policy.clone(),
            inference: runtime_manifest.inference.clone(),
            system_prompt_overridden: false,
            context: runtime_manifest.context.clone(),
            otel_endpoint: runtime_manifest
                .observability
                .as_ref()
                .and_then(|o| o.otel_endpoint.clone()),
            eval_config_json: eval_config_json.clone(),
            case_id: Some(case.case_id.clone()),
            dataset_id: dataset_id.clone(),
            lifecycle: runtime_manifest.lifecycle.clone(),
            lifecycle_override: None,
            trace: runtime_manifest.trace.clone(),
            workdir: None,
            bind_addr: "127.0.0.1".to_string(),
            internal_port: runtime_manifest
                .network
                .as_ref()
                .and_then(|n| n.internal_port),
            // `mur eval` has no --containment flag and reads no workspace config, so the
            // manifest is the only source of a floor here.
            declared_containment_floor: runtime_manifest
                .capabilities
                .as_ref()
                .and_then(|capabilities| capabilities.containment)
                .unwrap_or_default(),
            exports: runtime_manifest.exports.clone(),
        };

        let staged = match stage_session(Arc::clone(&local_registry), stage_request) {
            Ok(s) => s,
            Err(error) => {
                eprintln!("    stage failed: {error}");
                case_results.push((
                    case.case_id.clone(),
                    "stage_failed".to_string(),
                    workdir.clone(),
                    None,
                ));
                continue;
            }
        };

        workdir = staged.workdir.clone();

        if write_lock && !lock_written {
            let lock = MurmurLock {
                lock_version: LOCK_VERSION,
                artifacts: staged
                    .resolved_lock_artifacts
                    .iter()
                    .map(|entry| LockedArtifact {
                        name: entry.name.clone(),
                        resolved_version: entry.resolved_version.clone(),
                        sha256: LockedSha256 {
                            wasm: entry.sha256_wasm.clone(),
                        },
                    })
                    .collect(),
            };
            if write_lockfile_atomic(&lock_path, &lock).is_ok() {
                lock_written = true;
            }
        }

        // Copy case task to workdir/task.md
        let task_path = Path::new(&case.task_path);
        if task_path.exists() {
            let dst = staged.workdir.join("task.md");
            if let Err(e) = fs::copy(task_path, &dst) {
                eprintln!("    warning: failed to copy task file: {e}");
            }
        } else if !case.task_path.is_empty() {
            eprintln!("    warning: task_path '{}' not found", case.task_path);
        }

        let case_workdir = staged.workdir.clone();
        let case_session_id = staged.session_id.clone();

        match launch_session(staged, |_| {}) {
            Ok(_) => {}
            Err(RuntimeError::CapsuleTrap(msg)) => {
                eprintln!("    session trapped: {msg}");
            }
            Err(error) => {
                eprintln!("    session failed: {error}");
            }
        }

        // Read eval.jsonl from this session's workdir
        let eval_path = case_workdir.join("eval.jsonl");
        let run_record = if eval_path.exists() {
            match parse_eval_file(&eval_path) {
                Ok(metrics) => metrics.dataset_run,
                Err(e) => {
                    eprintln!("    warning: failed to read eval.jsonl: {e}");
                    None
                }
            }
        } else {
            None
        };

        let overall = run_record
            .as_ref()
            .map(|r| r.overall.clone())
            .unwrap_or_else(|| "no_scores".to_string());

        println!("    result: {overall}  session: {case_session_id}");
        case_results.push((case.case_id.clone(), overall, case_workdir, run_record));
    }

    println!();
    println!("── Summary ──────────────────────────────────────");
    let pass_count = case_results
        .iter()
        .filter(|(_, r, _, _)| r == "pass")
        .count();
    let total = case_results.len();
    println!("pass: {pass_count}/{total}");
    println!();

    for (case_id, result, case_workdir, run) in &case_results {
        let scores_str = run
            .as_ref()
            .map(|r| {
                let mut parts: Vec<String> = r
                    .scores
                    .iter()
                    .map(|(k, v)| format!("{k}={:.2}", v))
                    .collect();
                parts.sort();
                parts.join(" ")
            })
            .unwrap_or_default();
        println!(
            "  {:<24} {}  {}  ({})",
            case_id,
            result,
            scores_str,
            case_workdir.display()
        );
    }

    Ok(())
}
