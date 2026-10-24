use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

use clap::Subcommand;
use serde::{Deserialize, Serialize};

use crate::error::{CliError, E_IO_001, E_IO_003};
use crate::session_address::{self, SessionQuery};

mod compare;
mod run;

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
        /// The directory a run was given as --workdir, or <manifest-dir>/workdir, whose sessions'
        /// records are read from ~/.murmur/records (default: ./workdir)
        #[arg(long)]
        workdir: Option<PathBuf>,
        /// Output as JSON instead of human-readable text
        #[arg(long)]
        json: bool,
    },
    /// Compare two eval sessions side-by-side.
    ///
    /// Each argument accepts: a full session ID (ses_<32hex>), the last 4+ characters
    /// of a session ID as a suffix, an ordinal shortcut (@1 = most recent, @2 = second
    /// most recent, …), or a literal file path for backward compatibility.
    Diff {
        /// Session A: full ID, suffix (4+ chars), @N ordinal, or literal path.
        /// Omit both arguments to diff the two most recent sessions (@2 vs @1).
        a: Option<String>,
        /// Session B: full ID, suffix (4+ chars), @N ordinal, or literal path.
        b: Option<String>,
        /// The directory a run was given as --workdir, or <manifest-dir>/workdir, whose sessions'
        /// records are read from ~/.murmur/records (default: ./workdir)
        #[arg(long)]
        workdir: Option<PathBuf>,
        /// Compare the arms of one `mur eval run` instead of two sessions: its evr_ id, a suffix
        /// (4+ chars), an @N ordinal (@1 = most recent), or a literal path to its record.
        /// Without a value, the most recent run.
        #[arg(
            long,
            value_name = "RUN",
            num_args = 0..=1,
            default_missing_value = compare::LATEST_RUN,
            conflicts_with_all = ["a", "b"]
        )]
        run: Option<String>,
        /// With --run, print the comparison as JSON
        #[arg(long, requires = "run")]
        json: bool,
    },
    /// Run a capsule against each case in a dataset and collect eval scores
    Run {
        /// Path to capsule directory (containing murmur.yaml). Defaults to current directory.
        capsule: Option<PathBuf>,
        /// Path to dataset JSONL file. Defaults to workdir/eval.jsonl.
        #[arg(long)]
        dataset: Option<PathBuf>,
        /// A second capsule directory or manifest file to run every case against, from the same
        /// start, with the same task and the same inference.model
        #[arg(long)]
        variant: Option<PathBuf>,
        /// How many times to run each case on each manifest, alternating which goes first
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..))]
        repeat: u32,
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

/// The records directory an `eval.jsonl` path names, for a message.
fn staged_records_display(eval_path: &Path) -> String {
    eval_path
        .parent()
        .and_then(Path::parent)
        .unwrap_or(eval_path)
        .display()
        .to_string()
}

// ── Session resolution ────────────────────────────────────────────────────────

/// Where a session's scoring hook's `eval.jsonl` is, beneath its records directory: the `hooks/`
/// a hook declared `capabilities.filesystem.records: true` is preopened at.
const EVAL_RECORD_FILE: &str = "hooks/eval.jsonl";

/// `hooks/eval.jsonl` addressing: the vocabulary every session-naming command shares, under this
/// command's own diagnostic code. Neither `mur eval show` nor either side of `mur eval diff`
/// labels its failures — the command line already shows which address was written.
fn eval_query() -> SessionQuery<'static> {
    SessionQuery {
        record_file: EVAL_RECORD_FILE,
        code: E_EVAL_002,
        label: None,
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
    let path = session_address::resolve(session.as_deref(), &workdir, &eval_query())?;
    let metrics = parse_eval_file(&path)?;

    if json {
        print_show_json(&metrics, &path);
    } else {
        print_show_human(&metrics, &path);
    }
    Ok(())
}

fn print_show_human(m: &EvalMetrics, path: &Path) {
    capsule_runtime::report_println!(
        "── Eval: {} ─────────────────────────────────────",
        path.display()
    );

    if m.event_scores.is_empty() && m.dataset_run.is_none() {
        capsule_runtime::report_println!("  (no scored events)");
        return;
    }

    let rates = m.pass_rate_by_scorer();
    if rates.is_empty() && m.dataset_run.is_none() {
        capsule_runtime::report_println!("  (no scored events)");
        return;
    }

    capsule_runtime::report_println!();
    capsule_runtime::report_println!("── Scorers ──────────────────────────────────────");
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
        capsule_runtime::report_println!("  {:<24} {}/{} pass  ({:.1}%)", name, pass, total, pct);
    }

    capsule_runtime::report_println!();
    capsule_runtime::report_println!("── Overall ──────────────────────────────────────");
    capsule_runtime::report_println!("  result:  {}", m.overall());

    if let Some(run) = &m.dataset_run {
        if let Some(case_id) = &run.case_id {
            capsule_runtime::report_println!("  case:    {case_id}");
        }
        if let Some(dataset_id) = &run.dataset_id {
            capsule_runtime::report_println!("  dataset: {dataset_id}");
        }
        if !run.scores.is_empty() {
            capsule_runtime::report_println!();
            capsule_runtime::report_println!("── Score summary ────────────────────────────────");
            let mut scorer_names: Vec<&String> = run.scores.keys().collect();
            scorer_names.sort();
            for name in scorer_names {
                capsule_runtime::report_println!("  {:<24} {:.4}", name, run.scores[name]);
            }
        }
    }

    if !m.event_scores.is_empty() {
        capsule_runtime::report_println!();
        capsule_runtime::report_println!("── Worst events ─────────────────────────────────");
        let mut fails: Vec<&EventScoreRecord> = m
            .event_scores
            .iter()
            .filter(|e| e.result == "fail")
            .collect();
        fails.sort_by_key(|e| (e.scorer.as_str(), e.turn));
        for ev in fails.iter().take(5) {
            capsule_runtime::report_println!(
                "  turn {:>3}  {:12}  {:<24} {}  score={:.2}",
                ev.turn,
                ev.event_type,
                ev.scorer,
                ev.result,
                ev.score.unwrap_or(0.0)
            );
        }
        if fails.is_empty() {
            capsule_runtime::report_println!("  (no failing events)");
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
    capsule_runtime::report_println!(
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
    // Same two-or-nothing rule `mur trace diff` has, and the same default: the older run is
    // `a` so the delta column reads forwards in time.
    let (a, b) = match (a, b) {
        (None, None) => ("@2".to_string(), "@1".to_string()),
        (Some(a), Some(b)) => (a, b),
        _ => {
            return Err(CliError::new(
                E_EVAL_002,
                "mur eval diff expects 0 or 2 arguments, got 1. Usage: mur eval diff [<a> <b>]",
            ));
        }
    };

    let workdir = workdir.unwrap_or_else(|| PathBuf::from("workdir"));
    let path_a = session_address::resolve(Some(&a), &workdir, &eval_query())?;
    let path_b = session_address::resolve(Some(&b), &workdir, &eval_query())?;
    let ma = parse_eval_file(&path_a)?;
    let mb = parse_eval_file(&path_b)?;
    print_diff(&ma, &mb);
    Ok(())
}

fn print_diff(a: &EvalMetrics, b: &EvalMetrics) {
    const COL: usize = 24;
    const VAL: usize = 14;

    capsule_runtime::report_println!(
        "{:<COL$} {:<VAL$} {:<VAL$} Delta",
        "Scorer",
        "Run A",
        "Run B"
    );
    capsule_runtime::report_println!(
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

        capsule_runtime::report_println!("{:<COL$} {:<VAL$} {:<VAL$} {}", scorer, sa, sb, delta);
    }

    capsule_runtime::report_println!();
    capsule_runtime::report_println!(
        "{:<COL$} {:<VAL$} {:<VAL$}",
        "overall",
        a.overall(),
        b.overall()
    );
}

// ── Run ───────────────────────────────────────────────────────────────────────

pub(crate) use compare::run_eval_diff_run;
pub(crate) use run::{run_eval_run, EvalRunArgs};
