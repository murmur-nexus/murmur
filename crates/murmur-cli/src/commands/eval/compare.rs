//! `mur eval diff --run`: the comparison an eval-run record holds, per case and in total.
//!
//! Everything printed is read from the record alone, so a session `trace.retain` has pruned since
//! changes nothing.
//!
//! The exit status depends on the comparison only when `--require` declares thresholds; without
//! one, check results never change it.

use std::{
    fs,
    path::{Path, PathBuf},
    str::FromStr,
};

use serde_json::{json, Value};

use crate::error::{CliError, E_IO_001, E_IO_003};
use crate::session_address;

use super::run::{EvalRunRecord, RunCheckResult, RunOutcome, RunRecord, BASELINE, VARIANT};
use super::E_EVAL_002;

/// A `--require` threshold the record does not meet, or a record that is incomplete under one.
const E_EVAL_005: &str = "E-EVAL-005";
/// A `--require` threshold the record cannot answer: one naming an arm the run did not have.
const E_EVAL_006: &str = "E-EVAL-006";

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

/// Prints the comparison the run `run` names, then, when `require` holds any threshold, whether
/// each is met. Fails `E-EVAL-006` before printing anything when a threshold names an arm the run
/// lacks, and `E-EVAL-005` after printing when a threshold is not met or the record is incomplete.
pub(crate) fn run_eval_diff_run(
    run: &str,
    workdir: Option<PathBuf>,
    json: bool,
    require: &[Requirement],
) -> Result<(), CliError> {
    let workdir = workdir.unwrap_or_else(|| {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("workdir")
    });
    let path = resolve_run(run, &workdir)?;
    let record = read_record(&path)?;
    let requirements = require
        .iter()
        .map(|requirement| requirement.resolve(&record))
        .collect::<Result<Vec<_>, _>>()?;
    let verdicts = (!requirements.is_empty()).then(|| verdicts(&record, &requirements));
    if json {
        let mut comparison = comparison_json(&record);
        if let (Some(verdicts), Value::Object(object)) = (&verdicts, &mut comparison) {
            object.insert(
                "requirements".into(),
                verdicts.iter().map(Verdict::json).collect(),
            );
            object.insert(
                "requirements_met".into(),
                json!(record.complete && verdicts.iter().all(|verdict| verdict.met)),
            );
        }
        capsule_runtime::report_println!(
            "{}",
            serde_json::to_string_pretty(&comparison).unwrap_or_default()
        );
    } else {
        print_human(&record);
        if let Some(verdicts) = &verdicts {
            print_requirements(&record, verdicts);
        }
    }
    match &verdicts {
        Some(verdicts) => requirements_outcome(&record, verdicts),
        None => Ok(()),
    }
}

// ── Requirements ──────────────────────────────────────────────────────────────

/// What a `--require` threshold measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RequirementMeasure {
    /// The arm's strict passes over every case.
    Passes,
    /// The arm's spend over every run of a checked case, per strict pass.
    SpendPerPass,
    /// The variant's strict passes less the baseline's.
    PassLead,
}

impl RequirementMeasure {
    fn name(self) -> &'static str {
        match self {
            Self::Passes => "passes",
            Self::SpendPerPass => "spend-per-pass",
            Self::PassLead => "pass-lead",
        }
    }

    /// How the requirements block names the number it states.
    fn words(self) -> &'static str {
        match self {
            Self::Passes => "passes",
            Self::SpendPerPass => "spend per pass",
            Self::PassLead => "pass lead",
        }
    }

    /// The threshold `text` states for this measure: a count of passes, 0 or more; a number of
    /// tokens, 0 or more, with an optional fraction; or a signed count for the lead.
    fn threshold(self, text: &str) -> Result<f64, String> {
        let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        let parsed = match self {
            Self::Passes => digits(text).then(|| text.parse::<u64>().ok().map(|n| n as f64)),
            Self::SpendPerPass => {
                let (whole, fraction) = text.split_once('.').unwrap_or((text, "0"));
                (digits(whole) && digits(fraction)).then(|| text.parse::<f64>().ok())
            }
            Self::PassLead => {
                let unsigned = text.strip_prefix(['-', '+']).unwrap_or(text);
                digits(unsigned).then(|| text.parse::<i64>().ok().map(|n| n as f64))
            }
        };
        parsed.flatten().ok_or_else(|| match self {
            Self::Passes => {
                format!("passes takes a whole number of passes, 0 or more; got '{text}'")
            }
            Self::SpendPerPass => {
                format!("spend-per-pass takes a number of tokens, 0 or more; got '{text}'")
            }
            Self::PassLead => {
                format!(
                    "pass-lead takes a whole number of passes, which may be negative; got '{text}'"
                )
            }
        })
    }
}

/// How a `--require` threshold compares the actual number with its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RequirementOp {
    AtLeast,
    AtMost,
    Above,
    Below,
}

impl RequirementOp {
    /// Two-character operators first, so `>=` is never read as `>` followed by `=`.
    const ALL: [RequirementOp; 4] = [Self::AtLeast, Self::AtMost, Self::Above, Self::Below];

    fn symbol(self) -> &'static str {
        match self {
            Self::AtLeast => ">=",
            Self::AtMost => "<=",
            Self::Above => ">",
            Self::Below => "<",
        }
    }

    fn holds(self, actual: f64, threshold: f64) -> bool {
        match self {
            Self::AtLeast => actual >= threshold,
            Self::AtMost => actual <= threshold,
            Self::Above => actual > threshold,
            Self::Below => actual < threshold,
        }
    }
}

/// One `--require` threshold: `[baseline.|variant.]<measure><op><number>`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Requirement {
    /// [`BASELINE`] or [`VARIANT`]; `None` for `pass-lead`, and for an arm-scoped measure until
    /// [`Self::resolve`] names the arm the record defaults it to.
    arm: Option<&'static str>,
    measure: RequirementMeasure,
    op: RequirementOp,
    threshold: f64,
}

impl FromStr for Requirement {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        const GRAMMAR: &str = "expected [baseline.|variant.]<measure><op><number>";
        let at = text
            .find(['<', '>', '=', '!', '~'])
            .ok_or_else(|| format!("no operator; {GRAMMAR} with one of >=, <=, >, <"))?;
        let (name, rest) = text.split_at(at);
        let (arm, measure) = match name.split_once('.') {
            None => (None, name),
            Some((BASELINE, measure)) => (Some(BASELINE), measure),
            Some((VARIANT, measure)) => (Some(VARIANT), measure),
            Some((other, _)) => {
                return Err(format!(
                    "unknown arm '{other}': expected baseline or variant"
                ))
            }
        };
        let measure = match measure {
            "passes" => RequirementMeasure::Passes,
            "spend-per-pass" => RequirementMeasure::SpendPerPass,
            "pass-lead" => RequirementMeasure::PassLead,
            "" => return Err(format!("no measure; {GRAMMAR}")),
            other => {
                return Err(format!(
                    "unknown measure '{other}': expected passes, spend-per-pass or pass-lead"
                ))
            }
        };
        if measure == RequirementMeasure::PassLead && arm.is_some() {
            return Err(
                "pass-lead compares the variant with the baseline and takes no arm prefix"
                    .to_string(),
            );
        }
        let (op, number) = RequirementOp::ALL
            .into_iter()
            .find_map(|op| rest.strip_prefix(op.symbol()).map(|number| (op, number)))
            .ok_or_else(|| {
                let written: String = rest.chars().take_while(|c| !c.is_alphanumeric()).collect();
                format!("unknown operator '{written}': expected >=, <=, > or <")
            })?;
        Ok(Self {
            arm,
            measure,
            op,
            threshold: measure.threshold(number)?,
        })
    }
}

/// The `--require` value parser: [`Requirement::from_str`], whose error clap prints as
/// `invalid value '<text>' for '--require <REQ>': <reason>`.
pub(crate) fn parse_requirement(text: &str) -> Result<Requirement, String> {
    text.parse()
}

impl Requirement {
    /// `variant.spend-per-pass<=400000`: the arm prefix when one is set, the measure, the operator
    /// and the threshold as the shortest number that reads it.
    fn text(&self) -> String {
        let prefix = self.arm.map(|arm| format!("{arm}.")).unwrap_or_default();
        format!(
            "{prefix}{}{}{}",
            self.measure.name(),
            self.op.symbol(),
            self.threshold
        )
    }

    /// This requirement with its arm named: an unprefixed arm-scoped measure applies to the
    /// variant when `record` has one, else to the baseline. A requirement on an arm `record` lacks
    /// is `E-EVAL-006`.
    fn resolve(&self, record: &EvalRunRecord) -> Result<Requirement, CliError> {
        let has_variant = arm_names(record).contains(&VARIANT);
        let refuse = |why: &str| {
            CliError::with_hint(
                E_EVAL_006,
                format!(
                    "--require {}: run {} has one arm; {why}",
                    self.text(),
                    record.run_id
                ),
                "record both arms with `mur eval run --variant <dir>`, or require a measure of the \
                 baseline",
            )
        };
        let arm = match (self.measure, self.arm) {
            (RequirementMeasure::PassLead, _) if !has_variant => {
                return Err(refuse("pass-lead compares the variant with the baseline"))
            }
            (RequirementMeasure::PassLead, _) => None,
            (_, Some(VARIANT)) if !has_variant => {
                return Err(refuse("it has no variant to measure"))
            }
            (_, Some(arm)) => Some(arm),
            (_, None) if has_variant => Some(VARIANT),
            (_, None) => Some(BASELINE),
        };
        Ok(Requirement {
            arm,
            ..self.clone()
        })
    }
}

/// One resolved requirement measured against a record's totals.
#[derive(Debug, Clone, PartialEq)]
struct Verdict {
    requirement: Requirement,
    /// `None` when there is no number to compare, `reason` then says why.
    actual: Option<f64>,
    reason: Option<&'static str>,
    met: bool,
}

/// Each requirement in `requirements`, resolved, measured against `record`'s total for its arm.
fn verdicts(record: &EvalRunRecord, requirements: &[Requirement]) -> Vec<Verdict> {
    let names = arm_names(record);
    let totals = arm_totals(record, &names);
    let total_of = |arm: &str| {
        names
            .iter()
            .position(|name| *name == arm)
            .map(|index| &totals[index])
    };
    let passes = |arm: &str| total_of(arm).map_or(0.0, |total| total.check_pass as f64);
    requirements
        .iter()
        .map(|requirement| {
            let arm = requirement.arm.unwrap_or(BASELINE);
            let (actual, reason) = match requirement.measure {
                RequirementMeasure::Passes => (Some(passes(arm)), None),
                RequirementMeasure::PassLead => (Some(passes(VARIANT) - passes(BASELINE)), None),
                RequirementMeasure::SpendPerPass => {
                    match total_of(arm).map(ArmSummary::spend_per_pass) {
                        Some(SpendPerPass::Tokens(tokens)) => (Some(tokens), None),
                        Some(SpendPerPass::Unreported) => (None, Some("spend not recorded")),
                        _ => (None, Some("no strict pass")),
                    }
                }
            };
            Verdict {
                requirement: requirement.clone(),
                actual,
                reason,
                met: actual
                    .is_some_and(|actual| requirement.op.holds(actual, requirement.threshold)),
            }
        })
        .collect()
}

impl Verdict {
    /// The actual number as the requirements block states it: the number compared, never rounded
    /// as the `spend per pass` row rounds it, so a line cannot read as meeting a threshold it
    /// failed.
    fn actual_text(&self) -> Option<String> {
        self.actual.map(|actual| format!("{actual}"))
    }

    fn json(&self) -> Value {
        json!({
            "requirement": self.requirement.text(),
            "arm": self.requirement.arm,
            "measure": self.requirement.measure.name(),
            "op": self.requirement.op.symbol(),
            "threshold": number_json(self.requirement.threshold),
            "actual": self.actual.map(number_json),
            "met": self.met,
            "reason": self.reason,
        })
    }
}

/// `Ok` when every verdict is met and `record` is complete; `E-EVAL-005` naming what is not
/// otherwise.
fn requirements_outcome(record: &EvalRunRecord, verdicts: &[Verdict]) -> Result<(), CliError> {
    let unmet: Vec<String> = verdicts
        .iter()
        .filter(|verdict| !verdict.met)
        .map(|verdict| verdict.requirement.text())
        .collect();
    let incomplete = (!record.complete).then(|| {
        format!(
            "run {} is incomplete: {} of {} runs recorded",
            record.run_id,
            recorded_runs(record),
            expected_runs(record)
        )
    });
    let message = match (unmet.is_empty(), incomplete) {
        (true, None) => return Ok(()),
        (true, Some(incomplete)) => incomplete,
        (false, incomplete) => {
            let mut message = format!(
                "{} of {} requirements not met: {}",
                unmet.len(),
                verdicts.len(),
                unmet.join(", ")
            );
            if let Some(incomplete) = incomplete {
                message.push_str(&format!("; {incomplete}"));
            }
            message
        }
    };
    Err(CliError::with_hint(
        E_EVAL_005,
        message,
        "the thresholds are the ones given with --require; the requirements block above states \
         each number",
    ))
}

/// A number as JSON: an integer when it is whole, so a count never reads `3.0`.
fn number_json(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() < 9_007_199_254_740_992.0 {
        json!(value as i64)
    } else {
        json!(value)
    }
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
    spend_tokens: Vec<u64>,
    /// The spend of every run of a case that has a check. A run that was never launched reached
    /// no provider and adds 0.
    checked_spend: u64,
    /// A launched run of a case that has a check recorded no spend, so `checked_spend` is short.
    checked_spend_unreported: bool,
}

/// An arm's spend per strict pass, or why there is none.
#[derive(Debug, Clone, Copy, PartialEq)]
enum SpendPerPass {
    /// No run of a case that has a check.
    NoCheck,
    NoPass,
    /// A launched run of a checked case recorded no spend.
    Unreported,
    Tokens(f64),
}

impl SpendPerPass {
    /// The number, `no pass`, or `—` for no check and for unrecorded spend.
    fn cell(self) -> String {
        match self {
            Self::Tokens(tokens) => Measure::Spend.value(tokens),
            Self::NoPass => "no pass".to_string(),
            Self::NoCheck | Self::Unreported => "—".to_string(),
        }
    }

    fn tokens(self) -> Option<f64> {
        match self {
            Self::Tokens(tokens) => Some(tokens),
            _ => None,
        }
    }
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
            let launched = matches!(
                run.outcome,
                RunOutcome::Completed | RunOutcome::LaunchFailed
            );
            match run.spend_tokens {
                _ if !launched => {}
                Some(spend) => self.checked_spend = self.checked_spend.saturating_add(spend),
                None => self.checked_spend_unreported = true,
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
            (&mut self.spend_tokens, run.spend_tokens),
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

    /// The spend of every run of a checked case over the strict passes among them.
    fn spend_per_pass(&self) -> SpendPerPass {
        if self.check_runs == 0 {
            SpendPerPass::NoCheck
        } else if self.check_pass == 0 {
            SpendPerPass::NoPass
        } else if self.checked_spend_unreported {
            SpendPerPass::Unreported
        } else {
            SpendPerPass::Tokens(self.checked_spend as f64 / self.check_pass as f64)
        }
    }
}

/// Each arm's runs of every case, in `names` order: the total block, and what `--require` measures.
fn arm_totals(record: &EvalRunRecord, names: &[&str]) -> Vec<ArmSummary> {
    names
        .iter()
        .map(|name| {
            ArmSummary::of(
                record
                    .cases
                    .iter()
                    .flat_map(|case| case.runs.iter().filter(|run| run.arm == *name)),
            )
        })
        .collect()
}

/// The measures a comparison prints, each with how a value of it reads.
#[derive(Clone, Copy)]
enum Measure {
    /// The tokens the spend meter was charged.
    Spend,
    Turns,
    InputTokens,
    CachedTokens,
    CacheWriteTokens,
    OutputTokens,
    Time,
}

impl Measure {
    const ALL: [Measure; 7] = [
        Measure::Spend,
        Measure::Turns,
        Measure::InputTokens,
        Measure::CachedTokens,
        Measure::CacheWriteTokens,
        Measure::OutputTokens,
        Measure::Time,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Spend => "spend",
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
            Self::Spend => "spend_tokens",
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
            Self::Spend => &summary.spend_tokens,
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
/// `spend per pass` is computed from the block's own runs in both, and its delta is the variant's
/// number minus the baseline's.
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
    for case in &record.cases {
        let summaries: Vec<ArmSummary> = names
            .iter()
            .map(|name| ArmSummary::of(case.runs.iter().filter(|run| run.arm == *name)))
            .collect();

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
    print_block(
        &header_cells,
        &arm_totals(record, &names),
        paired,
        Measure::total,
        Measure::sum,
    );
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

    let per_pass: Vec<SpendPerPass> = summaries.iter().map(ArmSummary::spend_per_pass).collect();
    let per_pass_cells: Vec<String> = per_pass.iter().map(|value| value.cell()).collect();
    let per_pass_delta = paired.then(|| match (per_pass[0].tokens(), per_pass[1].tokens()) {
        (Some(a), Some(b)) => Measure::Spend.delta(b - a),
        _ => "—".to_string(),
    });
    row("spend per pass", &per_pass_cells, per_pass_delta);

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

/// The requirements block: one line per verdict, then one for an incomplete record.
fn print_requirements(record: &EvalRunRecord, verdicts: &[Verdict]) {
    let texts: Vec<String> = verdicts
        .iter()
        .map(|verdict| verdict.requirement.text())
        .collect();
    let width = texts
        .iter()
        .map(|text| text.chars().count())
        .max()
        .unwrap_or(0);
    capsule_runtime::report_println!();
    capsule_runtime::report_println!("── requirements {}", "─".repeat(24));
    for (verdict, text) in verdicts.iter().zip(&texts) {
        let stated = match verdict.actual_text() {
            Some(actual) => format!("{} {actual}", verdict.requirement.measure.words()),
            None => verdict.reason.unwrap_or_default().to_string(),
        };
        let status = if verdict.met { "met" } else { "not met" };
        capsule_runtime::report_println!("  {status:<8} {text:<width$}  {stated}");
    }
    if !record.complete {
        capsule_runtime::report_println!(
            "  {:<8} run is incomplete: {} of {} runs recorded",
            "not met",
            recorded_runs(record),
            expected_runs(record)
        );
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
    object.insert("spend_per_pass".into(), spend_per_pass_json(summary));
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
    object.insert("spend_per_pass".into(), spend_per_pass_json(summary));
    object.insert("scores_pass".into(), json!(summary.scores_pass));
    object.insert("scores_runs".into(), json!(summary.scores_runs));
    Value::Object(object)
}

/// The number, or `null` for no check, no pass and unrecorded spend alike.
fn spend_per_pass_json(summary: &ArmSummary) -> Value {
    summary
        .spend_per_pass()
        .tokens()
        .map_or(Value::Null, number_json)
}

/// The comparison `record` holds, for an outer capsule: what [`print_human`] prints, as data.
pub(super) fn comparison_json(record: &EvalRunRecord) -> Value {
    let names = arm_names(record);
    let cases: Vec<Value> = record
        .cases
        .iter()
        .map(|case| {
            let mut arms = serde_json::Map::new();
            for name in &names {
                let runs = case.runs.iter().filter(|run| run.arm == *name);
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
    for (name, summary) in names.iter().zip(&arm_totals(record, &names)) {
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

    fn requirement(text: &str) -> Requirement {
        text.parse().unwrap_or_else(|e| panic!("{text}: {e}"))
    }

    #[test]
    fn requirements_parse_into_arm_measure_operator_and_threshold() {
        let parsed = requirement("variant.spend-per-pass<=400000.5");
        assert_eq!(
            parsed,
            Requirement {
                arm: Some(VARIANT),
                measure: RequirementMeasure::SpendPerPass,
                op: RequirementOp::AtMost,
                threshold: 400000.5,
            }
        );
        assert_eq!(parsed.text(), "variant.spend-per-pass<=400000.5");
        for (text, op) in [
            ("passes>=3", RequirementOp::AtLeast),
            ("passes<=3", RequirementOp::AtMost),
            ("passes>3", RequirementOp::Above),
            ("passes<3", RequirementOp::Below),
        ] {
            let parsed = requirement(text);
            assert_eq!((parsed.op, parsed.arm, parsed.threshold), (op, None, 3.0));
            assert_eq!(parsed.text(), text);
        }
        assert_eq!(requirement("baseline.passes>=0").arm, Some(BASELINE));
        assert_eq!(requirement("pass-lead>=-2").threshold, -2.0);
        assert_eq!(requirement("pass-lead>+2").text(), "pass-lead>2");
        assert_eq!(
            requirement("spend-per-pass<1000.0").text(),
            "spend-per-pass<1000"
        );
    }

    #[test]
    fn malformed_requirements_name_what_is_wrong() {
        for (text, reason) in [
            ("cost<=5", "unknown measure 'cost'"),
            ("passes=~3", "unknown operator '=~'"),
            ("passes", "no operator"),
            (">=3", "no measure"),
            ("passes>=-1", "passes takes a whole number"),
            ("passes>=1.5", "passes takes a whole number"),
            ("passes>=", "passes takes a whole number"),
            ("spend-per-pass<=abc", "spend-per-pass takes a number"),
            ("spend-per-pass<=-1", "spend-per-pass takes a number"),
            ("spend-per-pass<=1e5", "spend-per-pass takes a number"),
            ("pass-lead>=1.5", "pass-lead takes a whole number"),
            ("variant.pass-lead>=1", "takes no arm prefix"),
            ("other.passes>=1", "unknown arm 'other'"),
            ("passes >= 3", "unknown measure 'passes '"),
        ] {
            let error = Requirement::from_str(text).expect_err(text);
            assert!(error.contains(reason), "{text}: {error}");
        }
    }

    fn run(arm: &str, outcome: &str, check: Option<&str>, spend: Option<u64>) -> Value {
        json!({
            "arm": arm, "repeat": 0, "order": 0, "session_id": null, "records_dir": null,
            "workdir": null, "start_sha256": null, "outcome": outcome, "exit_status": null,
            "check_result": check, "check": null, "turns": null, "input_tokens": null,
            "output_tokens": null, "cached_tokens": null, "cache_write_tokens": null,
            "agent_ms": null, "spend_tokens": spend, "scores_overall": null
        })
    }

    /// A one-case record of `runs`, with a variant arm when `paired`.
    fn record(paired: bool, complete: bool, runs: Vec<Value>) -> EvalRunRecord {
        let arm = |name: &str| json!({"name": name, "manifest": "m", "manifest_sha256": "0", "model": null});
        let mut arms = vec![arm(BASELINE)];
        if paired {
            arms.push(arm(VARIANT));
        }
        serde_json::from_value(json!({
            "record_version": 1, "run_id": "evr_test", "started_at_ms": 0, "ended_at_ms": null,
            "complete": complete, "dataset": "d", "repeat": 2, "arms": arms,
            "cases": [{"case_id": "c", "task_path": "t", "start": null, "start_sha256": null,
                       "check": null, "runs": runs}]
        }))
        .unwrap()
    }

    fn verdict(record: &EvalRunRecord, text: &str) -> Verdict {
        let resolved = requirement(text).resolve(record).unwrap();
        verdicts(record, &[resolved]).remove(0)
    }

    #[test]
    fn an_unprefixed_measure_applies_to_the_variant_when_there_is_one() {
        let paired = record(true, true, vec![]);
        let single = record(false, true, vec![]);
        let resolve = |record: &EvalRunRecord, text: &str| requirement(text).resolve(record);
        assert_eq!(resolve(&paired, "passes>=1").unwrap().arm, Some(VARIANT));
        assert_eq!(resolve(&single, "passes>=1").unwrap().arm, Some(BASELINE));
        assert_eq!(
            resolve(&paired, "baseline.spend-per-pass<=1").unwrap().arm,
            Some(BASELINE)
        );
        assert_eq!(resolve(&paired, "pass-lead>=1").unwrap().arm, None);
        for text in ["pass-lead>=0", "variant.passes>=1"] {
            let error = resolve(&single, text).unwrap_err();
            assert_eq!(error.code, E_EVAL_006);
            assert!(
                error
                    .message
                    .starts_with(&format!("--require {text}: run evr_test has one arm; ")),
                "{}",
                error.message
            );
        }
    }

    #[test]
    fn requirements_compare_the_totals() {
        let record = record(
            true,
            true,
            vec![
                run(BASELINE, "completed", Some("pass"), Some(1000)),
                run(BASELINE, "completed", Some("fail"), Some(2000)),
                run(VARIANT, "completed", Some("pass"), Some(900)),
                run(VARIANT, "launch_failed", Some("pass"), Some(1100)),
            ],
        );
        let lead = verdict(&record, "pass-lead>=1");
        assert_eq!((lead.actual, lead.met), (Some(1.0), true));
        assert!(!verdict(&record, "pass-lead>1").met);
        assert!(verdict(&record, "pass-lead<2").met);
        assert!(!verdict(&record, "pass-lead<1").met);
        assert!(verdict(&record, "baseline.passes<=1").met);
        let spend = verdict(&record, "spend-per-pass<=1000");
        assert_eq!(
            (spend.actual, spend.met, spend.reason),
            (Some(1000.0), true, None)
        );
        let baseline = verdict(&record, "baseline.spend-per-pass<3000");
        assert_eq!((baseline.actual, baseline.met), (Some(3000.0), false));
        assert!(requirements_outcome(&record, &[lead, spend]).is_ok());
        let unmet = requirements_outcome(&record, &[baseline]).unwrap_err();
        assert_eq!(unmet.code, E_EVAL_005);
        assert_eq!(
            unmet.message,
            "1 of 1 requirements not met: baseline.spend-per-pass<3000"
        );
    }

    #[test]
    fn spend_per_pass_without_a_pass_or_a_recorded_spend_is_not_met() {
        let no_pass = record(
            false,
            true,
            vec![run(BASELINE, "completed", Some("fail"), Some(10))],
        );
        let none = verdict(&no_pass, "spend-per-pass<=1000000");
        assert_eq!(
            (none.actual, none.met, none.reason),
            (None, false, Some("no strict pass"))
        );
        assert_eq!(
            arm_totals(&no_pass, &[BASELINE])[0].spend_per_pass().cell(),
            "no pass"
        );

        let unreported = record(
            false,
            true,
            vec![
                run(BASELINE, "completed", Some("pass"), None),
                run(BASELINE, "completed", Some("pass"), Some(10)),
            ],
        );
        let missing = verdict(&unreported, "spend-per-pass<=1000000");
        assert_eq!(
            (missing.actual, missing.met, missing.reason),
            (None, false, Some("spend not recorded"))
        );
        assert_eq!(
            arm_totals(&unreported, &[BASELINE])[0]
                .spend_per_pass()
                .cell(),
            "—"
        );

        let unchecked = record(
            false,
            true,
            vec![run(BASELINE, "completed", None, Some(10))],
        );
        assert_eq!(
            arm_totals(&unchecked, &[BASELINE])[0].spend_per_pass(),
            SpendPerPass::NoCheck
        );
    }

    #[test]
    fn a_run_never_launched_adds_nothing_to_spend_per_pass() {
        let record = record(
            false,
            true,
            vec![
                run(BASELINE, "completed", Some("pass"), Some(300)),
                run(BASELINE, "stage_failed", Some("not_run"), None),
                run(BASELINE, "start_mismatch", Some("not_run"), None),
                run(BASELINE, "completed", Some("pass"), Some(400)),
                run(BASELINE, "completed", Some("fail"), Some(301)),
            ],
        );
        let total = &arm_totals(&record, &[BASELINE])[0];
        assert_eq!(total.spend_per_pass(), SpendPerPass::Tokens(500.5));
        assert_eq!(total.spend_per_pass().cell(), "500.5");
        assert_eq!(Measure::Spend.total(total), "1001");
    }

    /// The requirements block states the number it compared, so a line never reads as meeting a
    /// threshold it failed.
    #[test]
    fn a_requirement_states_the_number_it_compared_unrounded() {
        let record = record(
            false,
            true,
            vec![
                run(BASELINE, "completed", Some("pass"), Some(300)),
                run(BASELINE, "completed", Some("pass"), Some(300)),
                run(BASELINE, "completed", Some("pass"), Some(400)),
            ],
        );
        let total = &arm_totals(&record, &[BASELINE])[0];
        assert_eq!(total.spend_per_pass().cell(), "333.3");
        let failed = verdict(&record, "spend-per-pass<=333.3");
        assert!(!failed.met);
        assert_eq!(failed.actual_text().as_deref(), Some("333.3333333333333"));
        assert_eq!(
            verdict(&record, "passes>=3").actual_text().as_deref(),
            Some("3")
        );
    }

    #[test]
    fn an_incomplete_record_fails_requirements_it_meets() {
        let record = record(
            false,
            false,
            vec![run(BASELINE, "completed", Some("pass"), Some(1))],
        );
        let met = verdict(&record, "passes>=1");
        assert!(met.met);
        let error = requirements_outcome(&record, std::slice::from_ref(&met)).unwrap_err();
        assert_eq!(error.code, E_EVAL_005);
        assert_eq!(
            error.message,
            "run evr_test is incomplete: 1 of 2 runs recorded"
        );
        let unmet = verdict(&record, "passes>=2");
        let error = requirements_outcome(&record, &[met, unmet]).unwrap_err();
        assert_eq!(
            error.message,
            "1 of 2 requirements not met: baseline.passes>=2; run evr_test is incomplete: 1 of 2 \
             runs recorded"
        );
    }
}
