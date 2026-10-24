//! `mur eval diff --run`: the comparison an eval-run record holds, per case and in total.
//!
//! Everything printed is read from the record alone, so a session `trace.retain` has pruned since
//! changes nothing.

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde_json::{json, Value};

use crate::error::{CliError, E_IO_001, E_IO_003};
use crate::session_address;

use super::run::{EvalRunRecord, RunCheckResult, RunRecord, BASELINE, VARIANT};
use super::E_EVAL_002;

/// The address an omitted `--run` value means: the most recent eval run.
pub(super) const LATEST_RUN: &str = "@1";

// ── Resolution ────────────────────────────────────────────────────────────────

/// The record file an eval-run address names among the runs of the workdir root `workdir`, as
/// [`session_address`] resolves every address. Every failure is `E-EVAL-002`.
pub(super) fn resolve_run(address: &str, workdir: &Path) -> Result<PathBuf, CliError> {
    session_address::resolve_kind(
        Some(address),
        workdir,
        &session_address::EVAL_RUN,
        &session_address::SessionQuery {
            record_file: "",
            code: E_EVAL_002,
            label: Some("--run"),
        },
    )
}

pub(super) fn read_record(path: &Path) -> Result<EvalRunRecord, CliError> {
    let content = fs::read_to_string(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => CliError::new(
            E_IO_001,
            format!("eval-run record not found: {}", path.display()),
        ),
        _ => CliError::new(E_IO_003, format!("failed to read {}: {e}", path.display())),
    })?;
    serde_json::from_str(&content).map_err(|e| {
        CliError::new(
            super::E_EVAL_001,
            format!("{}: not an eval-run record: {e}", path.display()),
        )
    })
}

pub(crate) fn run_eval_diff_run(
    run: &str,
    workdir: Option<PathBuf>,
    json: bool,
) -> Result<(), CliError> {
    let workdir = workdir.unwrap_or_else(|| {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("workdir")
    });
    let path = resolve_run(run, &workdir)?;
    let record = read_record(&path)?;
    if json {
        capsule_runtime::report_println!(
            "{}",
            serde_json::to_string_pretty(&comparison_json(&record)).unwrap_or_default()
        );
    } else {
        print_human(&record);
    }
    Ok(())
}

// ── Aggregation ───────────────────────────────────────────────────────────────

/// The median and spread of one measure over the runs that reported it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Spread {
    pub(super) median: f64,
    pub(super) min: u64,
    pub(super) max: u64,
    pub(super) n: usize,
}

/// `None` when `values` is empty. The median of an even count is the mean of the middle two.
pub(super) fn spread(values: &[u64]) -> Option<Spread> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let mid = sorted.len() / 2;
    let median = if sorted.len() % 2 == 1 {
        sorted[mid] as f64
    } else {
        (sorted[mid - 1] as f64 + sorted[mid] as f64) / 2.0
    };
    Some(Spread {
        median,
        min: sorted[0],
        max: sorted[sorted.len() - 1],
        n: sorted.len(),
    })
}

/// One arm's runs of one case — or of every case, for the total — summarised.
#[derive(Debug, Default)]
struct ArmSummary {
    runs: usize,
    check_pass: usize,
    /// Runs of a case that has a check, `not_run` included.
    check_runs: usize,
    check_not_run: usize,
    scores_pass: usize,
    scores_runs: usize,
    turns: Vec<u64>,
    input_tokens: Vec<u64>,
    cached_tokens: Vec<u64>,
    cache_write_tokens: Vec<u64>,
    output_tokens: Vec<u64>,
    agent_ms: Vec<u64>,
}

impl ArmSummary {
    fn add(&mut self, run: &RunRecord) {
        self.runs += 1;
        if let Some(result) = run.check_result {
            self.check_runs += 1;
            match result {
                RunCheckResult::Pass => self.check_pass += 1,
                RunCheckResult::NotRun => self.check_not_run += 1,
                _ => {}
            }
        }
        if let Some(overall) = &run.scores_overall {
            self.scores_runs += 1;
            if overall == "pass" {
                self.scores_pass += 1;
            }
        }
        for (values, value) in [
            (&mut self.turns, run.turns.map(u64::from)),
            (&mut self.input_tokens, run.input_tokens),
            (&mut self.cached_tokens, run.cached_tokens),
            (&mut self.cache_write_tokens, run.cache_write_tokens),
            (&mut self.output_tokens, run.output_tokens),
            (&mut self.agent_ms, run.agent_ms),
        ] {
            if let Some(value) = value {
                values.push(value);
            }
        }
    }

    fn of<'a>(runs: impl IntoIterator<Item = &'a RunRecord>) -> Self {
        let mut summary = Self::default();
        for run in runs {
            summary.add(run);
        }
        summary
    }
}

/// The measures a comparison prints, each with how a value of it reads.
#[derive(Clone, Copy)]
enum Measure {
    Turns,
    InputTokens,
    CachedTokens,
    CacheWriteTokens,
    OutputTokens,
    Time,
}

impl Measure {
    const ALL: [Measure; 6] = [
        Measure::Turns,
        Measure::InputTokens,
        Measure::CachedTokens,
        Measure::CacheWriteTokens,
        Measure::OutputTokens,
        Measure::Time,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Turns => "turns",
            Self::InputTokens => "input tokens",
            Self::CachedTokens => "cached tokens",
            Self::CacheWriteTokens => "cache write tokens",
            Self::OutputTokens => "output tokens",
            Self::Time => "time",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::Turns => "turns",
            Self::InputTokens => "input_tokens",
            Self::CachedTokens => "cached_tokens",
            Self::CacheWriteTokens => "cache_write_tokens",
            Self::OutputTokens => "output_tokens",
            Self::Time => "agent_ms",
        }
    }

    fn values(self, summary: &ArmSummary) -> &[u64] {
        match self {
            Self::Turns => &summary.turns,
            Self::InputTokens => &summary.input_tokens,
            Self::CachedTokens => &summary.cached_tokens,
            Self::CacheWriteTokens => &summary.cache_write_tokens,
            Self::OutputTokens => &summary.output_tokens,
            Self::Time => &summary.agent_ms,
        }
    }

    fn value(self, value: f64) -> String {
        match self {
            Self::Time => format!("{:.1}s", value / 1000.0),
            _ if value.fract() == 0.0 => format!("{value:.0}"),
            _ => format!("{value:.1}"),
        }
    }

    fn delta(self, delta: f64) -> String {
        let sign = if delta < 0.0 { "-" } else { "+" };
        format!("{sign}{}", self.value(delta.abs()))
    }

    /// `median (min–max)` over several runs, the plain value over one, `—` over none.
    fn cell(self, summary: &ArmSummary) -> String {
        match spread(self.values(summary)) {
            None => "—".to_string(),
            Some(spread) if spread.n == 1 => self.value(spread.median),
            Some(spread) => format!(
                "{} ({}–{})",
                self.value(spread.median),
                self.value(spread.min as f64),
                self.value(spread.max as f64)
            ),
        }
    }

    /// The median over the runs that reported it.
    fn median(self, summary: &ArmSummary) -> Option<f64> {
        spread(self.values(summary)).map(|spread| spread.median)
    }

    /// The sum over the runs that reported it.
    fn sum(self, summary: &ArmSummary) -> Option<f64> {
        let values = self.values(summary);
        (!values.is_empty()).then(|| values.iter().sum::<u64>() as f64)
    }

    /// [`Self::sum`] as a cell, `—` when no run reported it.
    fn total(self, summary: &ArmSummary) -> String {
        self.sum(summary)
            .map_or_else(|| "—".to_string(), |sum| self.value(sum))
    }
}

fn check_cell(summary: &ArmSummary) -> String {
    if summary.check_runs == 0 {
        return "—".to_string();
    }
    let mut cell = format!("{}/{} pass", summary.check_pass, summary.check_runs);
    if summary.check_not_run > 0 {
        cell.push_str(&format!(" ({} not run)", summary.check_not_run));
    }
    cell
}

fn signed(delta: i64) -> String {
    if delta < 0 {
        delta.to_string()
    } else {
        format!("+{delta}")
    }
}

/// The arm names a record holds, baseline first.
fn arm_names(record: &EvalRunRecord) -> Vec<&str> {
    let mut names: Vec<&str> = record.arms.iter().map(|arm| arm.name.as_str()).collect();
    if names.is_empty() {
        names.push(BASELINE);
    }
    names
}

/// Each repeat's pair of traces, baseline first, where both runs reached a records directory.
fn trace_pairs(runs: &[RunRecord]) -> Vec<(String, String)> {
    let mut repeats: Vec<u32> = runs.iter().map(|run| run.repeat).collect();
    repeats.sort_unstable();
    repeats.dedup();
    let trace_of = |arm: &str, repeat: u32| {
        runs.iter()
            .find(|run| run.arm == arm && run.repeat == repeat)
            .and_then(|run| run.records_dir.as_ref())
            .map(|dir| Path::new(dir).join("trace.jsonl").display().to_string())
    };
    repeats
        .into_iter()
        .filter_map(|repeat| Some((trace_of(BASELINE, repeat)?, trace_of(VARIANT, repeat)?)))
        .collect()
}

fn expected_runs(record: &EvalRunRecord) -> usize {
    record.cases.len() * record.arms.len().max(1) * record.repeat as usize
}

fn recorded_runs(record: &EvalRunRecord) -> usize {
    record.cases.iter().map(|case| case.runs.len()).sum()
}

// ── Human output ──────────────────────────────────────────────────────────────

const LABEL: usize = 20;
const CELL: usize = 24;

fn row(label: &str, cells: &[String], delta: Option<String>) {
    let mut line = format!("  {label:<LABEL$}");
    for cell in cells {
        line.push_str(&format!(" {cell:<CELL$}"));
    }
    if let Some(delta) = delta {
        line.push_str(&format!(" {delta}"));
    }
    capsule_runtime::report_println!("{}", line.trim_end());
}

/// Prints the comparison `record` holds: a header, one block per case and a total block. A case's
/// delta is variant median minus baseline median; the total's is variant sum minus baseline sum.
pub(super) fn print_human(record: &EvalRunRecord) {
    let names = arm_names(record);
    let paired = names.len() > 1;

    capsule_runtime::report_println!(
        "── Eval run: {} ─────────────────────────────",
        record.run_id
    );
    capsule_runtime::report_println!("  dataset:  {}", record.dataset);
    capsule_runtime::report_println!("  repeat:   {}", record.repeat);
    if record.complete {
        capsule_runtime::report_println!("  status:   complete");
    } else {
        capsule_runtime::report_println!(
            "  status:   incomplete: {} of {} runs recorded",
            recorded_runs(record),
            expected_runs(record)
        );
    }
    for arm in &record.arms {
        capsule_runtime::report_println!("  {:<9} {}", format!("{}:", arm.name), arm.manifest);
    }
    let model = record
        .arms
        .first()
        .and_then(|arm| arm.model.as_deref())
        .unwrap_or("—");
    capsule_runtime::report_println!("  model:    {model}");

    let header_cells: Vec<String> = names.iter().map(|name| name.to_string()).collect();
    let mut totals: Vec<ArmSummary> = names.iter().map(|_| ArmSummary::default()).collect();

    for case in &record.cases {
        let summaries: Vec<ArmSummary> = names
            .iter()
            .map(|name| ArmSummary::of(case.runs.iter().filter(|run| run.arm == *name)))
            .collect();
        for (total, name) in totals.iter_mut().zip(&names) {
            for run in case.runs.iter().filter(|run| run.arm == *name) {
                total.add(run);
            }
        }

        capsule_runtime::report_println!();
        capsule_runtime::report_println!("── case: {} {}", case.case_id, "─".repeat(24));
        print_block(
            &header_cells,
            &summaries,
            paired,
            Measure::cell,
            Measure::median,
        );
        for (a, b) in trace_pairs(&case.runs) {
            capsule_runtime::report_println!("  mur trace diff {a} {b}");
        }
    }

    capsule_runtime::report_println!();
    capsule_runtime::report_println!("── total {}", "─".repeat(24));
    print_block(&header_cells, &totals, paired, Measure::total, Measure::sum);
}

fn print_block(
    header: &[String],
    summaries: &[ArmSummary],
    paired: bool,
    cell: fn(Measure, &ArmSummary) -> String,
    compared: fn(Measure, &ArmSummary) -> Option<f64>,
) {
    row(
        "",
        header,
        paired.then(|| "delta (variant − baseline)".to_string()),
    );

    let check_cells: Vec<String> = summaries.iter().map(check_cell).collect();
    let check_delta = (paired && summaries.iter().all(|s| s.check_runs > 0))
        .then(|| signed(summaries[1].check_pass as i64 - summaries[0].check_pass as i64));
    row("check", &check_cells, check_delta);

    for measure in Measure::ALL {
        let cells: Vec<String> = summaries.iter().map(|s| cell(measure, s)).collect();
        let delta = if paired {
            Some(
                match (
                    compared(measure, &summaries[0]),
                    compared(measure, &summaries[1]),
                ) {
                    (Some(a), Some(b)) => measure.delta(b - a),
                    _ => "—".to_string(),
                },
            )
        } else {
            None
        };
        row(measure.label(), &cells, delta);
    }

    if summaries.iter().any(|s| s.scores_runs > 0) {
        let cells: Vec<String> = summaries
            .iter()
            .map(|s| {
                if s.scores_runs == 0 {
                    "—".to_string()
                } else {
                    format!("{}/{} pass", s.scores_pass, s.scores_runs)
                }
            })
            .collect();
        let delta = paired
            .then(|| signed(summaries[1].scores_pass as i64 - summaries[0].scores_pass as i64));
        row("scorers", &cells, delta);
    }
}

// ── JSON output ───────────────────────────────────────────────────────────────

fn spread_json(values: &[u64]) -> Value {
    match spread(values) {
        Some(spread) => json!({"median": spread.median, "min": spread.min, "max": spread.max}),
        None => Value::Null,
    }
}

fn arm_json(summary: &ArmSummary) -> Value {
    let mut object = serde_json::Map::new();
    object.insert("runs".into(), json!(summary.runs));
    object.insert("check_pass".into(), json!(summary.check_pass));
    object.insert("check_runs".into(), json!(summary.check_runs));
    object.insert("check_not_run".into(), json!(summary.check_not_run));
    for measure in Measure::ALL {
        object.insert(measure.key().into(), spread_json(measure.values(summary)));
    }
    object.insert(
        "scores_pass".into(),
        if summary.scores_runs == 0 {
            Value::Null
        } else {
            json!(summary.scores_pass)
        },
    );
    object.insert("scores_runs".into(), json!(summary.scores_runs));
    Value::Object(object)
}

fn total_json(summary: &ArmSummary) -> Value {
    let mut object = serde_json::Map::new();
    object.insert("runs".into(), json!(summary.runs));
    object.insert("check_pass".into(), json!(summary.check_pass));
    object.insert("check_runs".into(), json!(summary.check_runs));
    object.insert("check_not_run".into(), json!(summary.check_not_run));
    for measure in Measure::ALL {
        let values = measure.values(summary);
        object.insert(
            measure.key().into(),
            if values.is_empty() {
                Value::Null
            } else {
                json!(values.iter().sum::<u64>())
            },
        );
    }
    object.insert("scores_pass".into(), json!(summary.scores_pass));
    object.insert("scores_runs".into(), json!(summary.scores_runs));
    Value::Object(object)
}

/// The comparison `record` holds, for an outer capsule: what [`print_human`] prints, as data.
pub(super) fn comparison_json(record: &EvalRunRecord) -> Value {
    let names = arm_names(record);
    let mut totals: Vec<ArmSummary> = names.iter().map(|_| ArmSummary::default()).collect();
    let cases: Vec<Value> = record
        .cases
        .iter()
        .map(|case| {
            let mut arms = serde_json::Map::new();
            for (name, total) in names.iter().zip(totals.iter_mut()) {
                let runs = case.runs.iter().filter(|run| run.arm == *name);
                for run in runs.clone() {
                    total.add(run);
                }
                arms.insert(name.to_string(), arm_json(&ArmSummary::of(runs)));
            }
            json!({
                "case_id": case.case_id,
                "arms": arms,
                "trace_diffs": trace_pairs(&case.runs)
                    .into_iter()
                    .map(|(a, b)| json!([a, b]))
                    .collect::<Vec<_>>(),
            })
        })
        .collect();
    let mut total = serde_json::Map::new();
    for (name, summary) in names.iter().zip(&totals) {
        total.insert(name.to_string(), total_json(summary));
    }
    json!({
        "run_id": record.run_id,
        "complete": record.complete,
        "repeat": record.repeat,
        "runs_recorded": recorded_runs(record),
        "runs_expected": expected_runs(record),
        "arms": record.arms,
        "cases": cases,
        "total": total,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spread_is_median_min_max() {
        assert_eq!(spread(&[]), None);
        let one = spread(&[7]).unwrap();
        assert_eq!((one.median, one.min, one.max, one.n), (7.0, 7, 7, 1));
        let odd = spread(&[100, 300, 200]).unwrap();
        assert_eq!((odd.median, odd.min, odd.max), (200.0, 100, 300));
        let even = spread(&[4, 1, 3, 2]).unwrap();
        assert_eq!((even.median, even.min, even.max), (2.5, 1, 4));
    }

    #[test]
    fn a_spread_cell_reads_median_and_range() {
        let summary = ArmSummary {
            input_tokens: vec![100, 300, 200],
            agent_ms: vec![1500],
            ..ArmSummary::default()
        };
        assert_eq!(Measure::InputTokens.cell(&summary), "200 (100–300)");
        assert_eq!(Measure::Time.cell(&summary), "1.5s");
        assert_eq!(Measure::CachedTokens.cell(&summary), "—");
        assert_eq!(Measure::InputTokens.delta(-50.0), "-50");
        assert_eq!(Measure::InputTokens.delta(50.0), "+50");
    }
}
