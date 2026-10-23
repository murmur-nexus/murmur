//! Where an agent session stands: what the runtime-provided `session-status` tool reports, the
//! hard deadline a session runs under, and the one notice an agent is given before it.
//!
//! [`SessionStatus`] holds no number of its own. It is fed at the funnels that already produce
//! each one — the trace writer's `inference`, `task_start` and `task_end` writes, and the http
//! agent loop's compaction check — and a call reads the spend meter as it stands. So every field a
//! call returns agrees with the trace written before it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use serde::Serialize;
use serde_json::Value;

use crate::agent::{DriverUsage, OccupancyBasis};
use crate::spend::SpendMeter;

/// The runtime-provided tool every agent capsule has, on both transports, with no grant.
pub const SESSION_STATUS_TOOL: &str = "session-status";

/// What a field reads when its limit is not set.
const NONE: &str = "none";

/// What a field reads when the runtime does not have the count.
const UNKNOWN: &str = "unknown";

/// The longest default notice margin: five minutes.
const DEFAULT_NOTICE_CEILING_SECS: u64 = 300;

/// The longest deadline a session runs under: ten years. A larger one overflows the clocks
/// the deadline is read against.
pub const MAX_DEADLINE_SECS: u64 = 315_360_000;

/// The hidden `mur run` flag a formation launcher and a child launcher pass a deadline down with:
/// the deadline's wall moment in milliseconds since the Unix epoch.
pub const INHERITED_DEADLINE_FLAG: &str = "--inherited-deadline";

/// Where a session's deadline came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeadlineSource {
    /// The session's own `mur run --deadline`.
    Own,
    /// The deadline of the formation or the capsule that launched the session.
    Inherited,
}

impl DeadlineSource {
    /// The `source` value the trace records carry.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Own => "own",
            Self::Inherited => "inherited",
        }
    }
}

/// The hard limit a session runs under: its own `mur run --deadline`, or the moment it inherited
/// from the formation or capsule that launched it.
///
/// Both clocks are read once, at the session's start. The monotonic one is what the runtime
/// enforces and counts down against; the wall one is only what `at` is spelled from, so a clock
/// change during the session moves neither the enforcement nor `left_secs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionDeadline {
    at: Instant,
    at_wall: SystemTime,
    secs: u64,
    source: DeadlineSource,
}

impl SessionDeadline {
    /// The deadline `secs` after a start read as `started` and `started_wall`. A `secs` above
    /// [`MAX_DEADLINE_SECS`] is taken as that bound.
    pub fn after(started: Instant, started_wall: SystemTime, secs: u64) -> Self {
        let secs = secs.min(MAX_DEADLINE_SECS);
        let length = Duration::from_secs(secs);
        Self {
            at: started + length,
            at_wall: started_wall + length,
            secs,
            source: DeadlineSource::Own,
        }
    }

    /// The inherited deadline at the wall moment `at_wall`, for a session whose start was read as
    /// `started` and `started_wall`. What is left is `at_wall - started_wall`: zero when the moment
    /// has passed, and at most [`MAX_DEADLINE_SECS`], beyond which `at` is that bound.
    pub fn until(started: Instant, started_wall: SystemTime, at_wall: SystemTime) -> Self {
        let bound = Duration::from_secs(MAX_DEADLINE_SECS);
        let left = at_wall
            .duration_since(started_wall)
            .unwrap_or(Duration::ZERO);
        let (left, at_wall) = if left > bound {
            (bound, started_wall + bound)
        } else {
            (left, at_wall)
        };
        let secs = left.as_secs() + u64::from(left.subsec_nanos() > 0);
        Self {
            at: started + left,
            at_wall,
            secs,
            source: DeadlineSource::Inherited,
        }
    }

    /// [`SessionDeadline::until`] the moment `millis` milliseconds after the Unix epoch, as
    /// [`INHERITED_DEADLINE_FLAG`] carries it. A moment the wall clock cannot hold is taken as the
    /// bound.
    pub fn from_unix_millis(started: Instant, started_wall: SystemTime, millis: u64) -> Self {
        let bound = started_wall + Duration::from_secs(MAX_DEADLINE_SECS);
        let at_wall = SystemTime::UNIX_EPOCH
            .checked_add(Duration::from_millis(millis))
            .unwrap_or(bound);
        Self::until(started, started_wall, at_wall)
    }

    /// `at` in whole milliseconds since the Unix epoch, the value [`INHERITED_DEADLINE_FLAG`]
    /// carries. Truncated, so the moment a session inherits is never later than this one and
    /// spells the same second.
    pub fn at_unix_millis(&self) -> u64 {
        self.at_wall
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |since| {
                u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
            })
    }

    /// The session's deadline: the earlier of its own and the inherited one, the inherited one on a
    /// tie, and `None` when it has neither. Both must have been read against the same start.
    pub fn earliest(own: Option<Self>, inherited: Option<Self>) -> Option<Self> {
        match (own, inherited) {
            (Some(own), Some(inherited)) if own.at < inherited.at => Some(own),
            (_, Some(inherited)) => Some(inherited),
            (own, None) => own,
        }
    }

    /// Seconds from the session's start to `at`: the `--deadline` value for an own deadline, and
    /// rounded up, `0` once passed, for an inherited one.
    pub fn secs(&self) -> u64 {
        self.secs
    }

    /// Where the deadline came from.
    pub fn source(&self) -> DeadlineSource {
        self.source
    }

    /// When the runtime ends the session, as RFC 3339 UTC to the second.
    pub fn at_rfc3339(&self) -> String {
        chrono::DateTime::<chrono::Utc>::from(self.at_wall)
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    }

    /// How long is left at `now`; zero once the deadline has passed.
    pub fn left(&self, now: Instant) -> Duration {
        self.at.saturating_duration_since(now)
    }

    /// The monotonic instant the session ends at.
    pub(crate) fn instant(&self) -> Instant {
        self.at
    }
}

/// The notice margin a deadline of `deadline_secs` gets when `lifecycle.deadline_notice_secs` is
/// not declared: five minutes, or a tenth of the deadline when that is shorter, and at least one
/// second.
pub fn default_notice_secs(deadline_secs: u64) -> u64 {
    (deadline_secs / 10).clamp(1, DEFAULT_NOTICE_CEILING_SECS)
}

/// The margin in force: the declared `lifecycle.deadline_notice_secs`, or the default for the
/// deadline. `0` sends no notice.
pub fn notice_secs(declared: Option<u64>, deadline_secs: u64) -> u64 {
    declared.unwrap_or_else(|| default_notice_secs(deadline_secs))
}

/// The one deadline notice, taken by the first dispatched tool result inside the margin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeadlineNotice {
    /// `SessionDeadline::at_rfc3339`.
    pub(crate) at: String,
    /// Whole seconds left when it was taken.
    pub(crate) left_secs: u64,
    /// The margin it was due under.
    pub(crate) notice_secs: u64,
}

impl DeadlineNotice {
    /// The line appended after the tool result's fence.
    pub(crate) fn text(&self) -> String {
        format!(
            "Session deadline: the runtime ends this session at {}, in {} seconds.",
            self.at, self.left_secs
        )
    }
}

/// The spend meter as a call reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SpendReading {
    /// `SpendMeter::used`: tokens settled against the ceilings.
    pub(crate) used: u64,
    /// `inference.max_session_tokens`.
    pub(crate) ceiling: Option<u64>,
    /// The machine ledger's day total and `spend.machine_tokens_per_day`, when this session keeps
    /// the ledger.
    pub(crate) machine: Option<(u64, u64)>,
}

impl SpendReading {
    /// Read `meter` now, the machine ledger's file included.
    pub(crate) fn of(meter: &SpendMeter) -> Self {
        Self {
            used: meter.used(),
            ceiling: meter.session_ceiling(),
            machine: meter.machine_reading(),
        }
    }
}

/// How the context occupancy was last read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ContextReading {
    used_tokens: u32,
    window_tokens: Option<u32>,
    measured_by: OccupancyBasis,
}

/// What the funnels have fed so far.
#[derive(Debug, Default)]
struct Fed {
    /// The active task and when its `task_start` was written.
    task: Option<(String, Instant)>,
    /// `TraceWriter::task_turns` as of the last `inference` record.
    task_turns: u32,
    input: Option<u64>,
    output: Option<u64>,
    cached: Option<u64>,
    cache_write: Option<u64>,
    /// `inference` records that carried no token count.
    unreported_calls: u64,
    context: Option<ContextReading>,
}

/// One session's standing, shared by the store, the trace writer and the agent loop.
#[derive(Debug)]
pub(crate) struct SessionStatus {
    started: Instant,
    /// `transport: process`: the harness keeps the context, and its counts are the tokens.
    harness_transport: bool,
    /// `inference.max_turns`.
    turn_cap: Option<u32>,
    deadline: Option<SessionDeadline>,
    /// The notice margin in force; `0` sends none.
    notice_secs: u64,
    notice_taken: AtomicBool,
    fed: Mutex<Fed>,
}

impl SessionStatus {
    /// A session that started at `started`. `notice_secs` is [`notice_secs`]'s answer, and is
    /// read only when there is a deadline.
    pub(crate) fn new(
        started: Instant,
        harness_transport: bool,
        turn_cap: Option<u32>,
        deadline: Option<SessionDeadline>,
        notice_secs: u64,
    ) -> Self {
        Self {
            started,
            harness_transport,
            turn_cap,
            deadline,
            notice_secs,
            notice_taken: AtomicBool::new(false),
            fed: Mutex::new(Fed::default()),
        }
    }

    fn fed(&self) -> std::sync::MutexGuard<'_, Fed> {
        // Every update under the lock is a handful of assignments that cannot panic midway.
        self.fed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The session's deadline: the earliest bound placed on it, if any.
    pub(crate) fn deadline(&self) -> Option<SessionDeadline> {
        self.deadline
    }

    /// The notice margin in force, `0` for none; meaningless without a deadline.
    pub(crate) fn notice_secs(&self) -> u64 {
        self.notice_secs
    }

    /// Fed by `TraceWriter::write_task_start`.
    pub(crate) fn task_started(&self, task_id: &str, at: Instant) {
        let mut fed = self.fed();
        fed.task = Some((task_id.to_string(), at));
        fed.task_turns = 0;
    }

    /// Fed by `TraceWriter::write_task_end`.
    pub(crate) fn task_ended(&self) {
        self.fed().task = None;
    }

    /// The active task, for the records a call writes.
    pub(crate) fn task_id(&self) -> Option<String> {
        self.fed().task.as_ref().map(|(id, _)| id.clone())
    }

    /// Fed by `TraceWriter::write_inference_event` with each `inference` record's counts, after
    /// the writer's own counters moved: `task_turns` is `TraceWriter::task_turns` as it now stands.
    ///
    /// The provider's report is `usage` under `transport: http`; under `transport: process` it is
    /// the harness's `input_tokens` / `output_tokens` with `usage`'s cache counts. A record whose
    /// report carries no count is an unreported call.
    pub(crate) fn record_inference(
        &self,
        task_turns: u32,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        usage: Option<&DriverUsage>,
    ) {
        let (input, output) = if self.harness_transport {
            (input_tokens, output_tokens)
        } else {
            (
                usage.and_then(|u| u.input_tokens),
                usage.and_then(|u| u.output_tokens),
            )
        };
        let cached = usage.and_then(|u| u.cached_tokens);
        let cache_write = usage.and_then(|u| u.cache_write_tokens);
        let mut guard = self.fed();
        let fed = &mut *guard;
        fed.task_turns = task_turns;
        if [input, output, cached, cache_write]
            .iter()
            .all(Option::is_none)
        {
            fed.unreported_calls = fed.unreported_calls.saturating_add(1);
            return;
        }
        for (sum, count) in [
            (&mut fed.input, input),
            (&mut fed.output, output),
            (&mut fed.cached, cached),
            (&mut fed.cache_write, cache_write),
        ] {
            if let Some(count) = count {
                *sum = Some(sum.unwrap_or(0).saturating_add(count));
            }
        }
    }

    /// Fed by the http agent loop right after its compaction check: the occupancy that check
    /// compared, the window it compared it against (`None` when the choice has none) and how the
    /// count was made.
    pub(crate) fn set_context(
        &self,
        used_tokens: u32,
        window_tokens: Option<u32>,
        measured_by: OccupancyBasis,
    ) {
        self.fed().context = Some(ContextReading {
            used_tokens,
            window_tokens,
            measured_by,
        });
    }

    /// The `session-status` result as it stands now.
    pub(crate) fn snapshot(&self, spend: SpendReading) -> StatusReport {
        self.snapshot_at(Instant::now(), spend)
    }

    /// The `session-status` result as it stands at `now`.
    pub(crate) fn snapshot_at(&self, now: Instant, spend: SpendReading) -> StatusReport {
        let fed = self.fed();
        let whole_secs = |since: Instant| now.saturating_duration_since(since).as_secs();
        let time = TimeReport {
            elapsed_secs: whole_secs(self.started),
            task_elapsed_secs: fed.task.as_ref().map_or_else(
                || Value::from(UNKNOWN),
                |(_, at)| Value::from(whole_secs(*at)),
            ),
            deadline: self.deadline.map_or_else(
                || Value::from(NONE),
                |deadline| {
                    serde_json::json!({
                        "at": deadline.at_rfc3339(),
                        "left_secs": deadline.left(now).as_secs(),
                    })
                },
            ),
        };
        let context = match (self.harness_transport, fed.context) {
            (false, Some(reading)) => serde_json::json!({
                "used_tokens": reading.used_tokens,
                "window_tokens": or_none(reading.window_tokens),
                "measured_by": reading.measured_by,
            }),
            _ => Value::from(UNKNOWN),
        };
        StatusReport {
            time,
            turns: TurnsReport {
                used: fed.task_turns,
                cap: or_none(self.turn_cap),
            },
            context,
            tokens: TokensReport {
                input: or_unknown(fed.input),
                output: or_unknown(fed.output),
                cached: or_unknown(fed.cached),
                cache_write: or_unknown(fed.cache_write),
                unreported_calls: fed.unreported_calls,
            },
            spend: SpendReport {
                tokens: spend.used,
                ceiling: or_none(spend.ceiling),
                machine: spend.machine.map_or_else(
                    || Value::from(NONE),
                    |(tokens_today, ceiling)| {
                        serde_json::json!({"tokens_today": tokens_today, "ceiling": ceiling})
                    },
                ),
            },
        }
    }

    /// The deadline notice, when it is due at `now` and has not been taken: once per session, from
    /// the moment no more than the margin is left. Never with no deadline or a margin of `0`.
    pub(crate) fn take_due_notice(&self, now: Instant) -> Option<DeadlineNotice> {
        let deadline = self.deadline?;
        if self.notice_secs == 0 {
            return None;
        }
        let left = deadline.left(now);
        if left > Duration::from_secs(self.notice_secs) {
            return None;
        }
        if self.notice_taken.swap(true, Ordering::AcqRel) {
            return None;
        }
        Some(DeadlineNotice {
            at: deadline.at_rfc3339(),
            left_secs: left.as_secs(),
            notice_secs: self.notice_secs,
        })
    }
}

fn or_none<T: Into<Value>>(value: Option<T>) -> Value {
    value.map_or_else(|| Value::from(NONE), Into::into)
}

fn or_unknown<T: Into<Value>>(value: Option<T>) -> Value {
    value.map_or_else(|| Value::from(UNKNOWN), Into::into)
}

/// One `session-status` result: what the model is handed and what `session_status.status` records.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct StatusReport {
    time: TimeReport,
    turns: TurnsReport,
    /// An object under `transport: http`; `"unknown"` under `transport: process`.
    context: Value,
    tokens: TokensReport,
    spend: SpendReport,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct TimeReport {
    elapsed_secs: u64,
    task_elapsed_secs: Value,
    deadline: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct TurnsReport {
    used: u32,
    cap: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct TokensReport {
    input: Value,
    output: Value,
    cached: Value,
    cache_write: Value,
    unreported_calls: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct SpendReport {
    tokens: u64,
    ceiling: Value,
    machine: Value,
}

/// The fields every description states, in the order the result carries them.
const FIELDS: &str = "`time`: `elapsed_secs` since the session started, `task_elapsed_secs` since \
     the current task started, and `deadline`, with `at`, the RFC 3339 UTC time the runtime ends \
     the session, and `left_secs` until then. `turns`: `used`, the inference calls this task has \
     made, and `cap`, the most it may make.";

/// The `spend` and closing sentences every description shares.
const SPEND_AND_VALUES: &str = "`spend`: `tokens`, what the spend ceilings count for this \
     session, `ceiling`, this session's token ceiling, and `machine`, this machine's \
     `tokens_today` against its daily `ceiling`. No money amount is kept: spend is counted in \
     tokens. A limit that is not set reads \"none\" and a count the runtime does not have reads \
     \"unknown\". Seconds are whole.";

/// `session-status`'s description under `transport: http`. Fixed, so the tool array's bytes do
/// not change from one request to the next.
pub fn description_http() -> String {
    format!(
        "Report where this session stands, measured at the moment of the call. Takes no input and \
         returns one JSON object. {FIELDS} `context`: `used_tokens`, the conversation's size as \
         the runtime last counted it, `window_tokens`, the model's context window, and \
         `measured_by`, how the count was made (\"actual\": the provider's own count of the last \
         call; \"estimate\": the runtime's own count). `tokens`: \
         the `input`, `output`, `cached` and `cache_write` tokens the provider reported for this \
         session's inference calls, and `unreported_calls`, the calls it reported none for. \
         {SPEND_AND_VALUES}"
    )
}

/// `session-status`'s description under `transport: process`.
pub fn description_process() -> String {
    format!(
        "Report where this session stands, measured at the moment of the call. Takes no input and \
         returns one JSON object. {FIELDS} Turns are counted as they close. `context` reads \
         \"unknown\": the harness keeps its own context. `tokens`: the `input`, `output`, \
         `cached` and `cache_write` tokens the harness reported for this session's closed turns, \
         and `unreported_calls`, the turns it reported none for. {SPEND_AND_VALUES}"
    )
}

/// `session-status`'s input schema: no input.
pub const INPUT_SCHEMA: &str = r#"{"type":"object","properties":{}}"#;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn usage(
        input: Option<u64>,
        output: Option<u64>,
        cached: Option<u64>,
        cache_write: Option<u64>,
    ) -> DriverUsage {
        DriverUsage {
            input_tokens: input,
            output_tokens: output,
            cached_tokens: cached,
            cache_write_tokens: cache_write,
            thinking_tokens: None,
        }
    }

    fn no_spend() -> SpendReading {
        SpendReading {
            used: 0,
            ceiling: None,
            machine: None,
        }
    }

    fn report(status: &SessionStatus, now: Instant, spend: SpendReading) -> Value {
        serde_json::to_value(status.snapshot_at(now, spend)).unwrap()
    }

    #[test]
    fn session_status_reports_every_fed_value_on_http() {
        let started = Instant::now();
        let wall = SystemTime::UNIX_EPOCH + Duration::from_secs(1_792_497_600);
        let deadline = SessionDeadline::after(started, wall, 7_200);
        let status = SessionStatus::new(started, false, Some(20), Some(deadline), 300);
        status.task_started("tsk_1", started + Duration::from_secs(44));
        status.record_inference(
            1,
            Some(900),
            Some(40),
            Some(&usage(Some(1_000), Some(50), Some(600), Some(100))),
        );
        status.record_inference(
            2,
            Some(950),
            Some(30),
            Some(&usage(Some(1_200), Some(70), Some(0), Some(10))),
        );
        status.set_context(61_234, Some(200_000), OccupancyBasis::Estimate);
        let now = started + Duration::from_millis(1_834_900);
        let spend = SpendReading {
            used: 1_920,
            ceiling: Some(1_000_000),
            machine: Some((5_000, 9_000_000)),
        };
        assert_eq!(
            report(&status, now, spend),
            json!({
                "time": {"elapsed_secs": 1834, "task_elapsed_secs": 1790,
                         "deadline": {"at": "2026-10-20T14:00:00Z", "left_secs": 5365}},
                "turns": {"used": 2, "cap": 20},
                "context": {"used_tokens": 61234, "window_tokens": 200000,
                            "measured_by": "estimate"},
                "tokens": {"input": 2200, "output": 120, "cached": 600, "cache_write": 110,
                           "unreported_calls": 0},
                "spend": {"tokens": 1920, "ceiling": 1000000,
                          "machine": {"tokens_today": 5000, "ceiling": 9000000}},
            })
        );
        // The wire text keeps the documented field order.
        let text = serde_json::to_string(&status.snapshot_at(now, spend)).unwrap();
        let order: Vec<usize> = [
            "\"time\"",
            "\"turns\"",
            "\"context\"",
            "\"tokens\"",
            "\"spend\"",
        ]
        .iter()
        .map(|key| text.find(key).unwrap())
        .collect();
        assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{text}");
    }

    #[test]
    fn session_status_says_none_for_unset_limits_and_unknown_for_missing_counts() {
        let started = Instant::now();
        let status = SessionStatus::new(started, false, None, None, 0);
        assert_eq!(
            report(&status, started, no_spend()),
            json!({
                "time": {"elapsed_secs": 0, "task_elapsed_secs": "unknown", "deadline": "none"},
                "turns": {"used": 0, "cap": "none"},
                "context": "unknown",
                "tokens": {"input": "unknown", "output": "unknown", "cached": "unknown",
                           "cache_write": "unknown", "unreported_calls": 0},
                "spend": {"tokens": 0, "ceiling": "none", "machine": "none"},
            })
        );

        status.task_started("tsk_1", started);
        // A record with no usage is an unreported call; one with only some members leaves the
        // others unknown.
        status.record_inference(1, Some(900), Some(40), None);
        status.record_inference(
            2,
            Some(900),
            Some(40),
            Some(&usage(Some(7), None, None, None)),
        );
        status.set_context(940, None, OccupancyBasis::Estimate);
        let value = report(&status, started + Duration::from_secs(3), no_spend());
        assert_eq!(
            value["tokens"],
            json!({"input": 7, "output": "unknown", "cached": "unknown",
                   "cache_write": "unknown", "unreported_calls": 1})
        );
        assert_eq!(
            value["context"],
            json!({"used_tokens": 940, "window_tokens": "none", "measured_by": "estimate"})
        );
        assert_eq!(value["turns"]["used"], 2);
        assert_eq!(value["time"]["task_elapsed_secs"], 3);

        // The provider's count of the last call, when it reported one, is named as such.
        status.set_context(1_010, None, OccupancyBasis::Actual);
        assert_eq!(
            report(&status, started, no_spend())["context"],
            json!({"used_tokens": 1010, "window_tokens": "none", "measured_by": "actual"})
        );
    }

    #[test]
    fn session_status_under_process_reads_the_harness_counts_and_no_context() {
        let started = Instant::now();
        let status = SessionStatus::new(started, true, Some(10), None, 0);
        status.task_started("tsk_1", started);
        status.record_inference(
            1,
            Some(3_000),
            Some(200),
            Some(&usage(None, None, Some(2_500), Some(400))),
        );
        status.record_inference(2, None, None, None);
        // A process session is never fed a context reading, and one fed anyway is not reported.
        status.set_context(1, Some(2), OccupancyBasis::Estimate);
        let value = report(&status, started, no_spend());
        assert_eq!(value["context"], json!("unknown"));
        assert_eq!(
            value["tokens"],
            json!({"input": 3000, "output": 200, "cached": 2500, "cache_write": 400,
                   "unreported_calls": 1})
        );
        assert_eq!(value["turns"], json!({"used": 2, "cap": 10}));
    }

    #[test]
    fn session_status_restarts_the_task_clock_and_turns_at_each_task() {
        let started = Instant::now();
        let status = SessionStatus::new(started, false, None, None, 0);
        status.task_started("tsk_1", started);
        status.record_inference(1, None, None, None);
        status.task_ended();
        assert_eq!(status.task_id(), None);
        status.task_started("tsk_2", started + Duration::from_secs(10));
        assert_eq!(status.task_id().as_deref(), Some("tsk_2"));
        let value = report(&status, started + Duration::from_secs(12), no_spend());
        assert_eq!(value["turns"]["used"], 0);
        assert_eq!(value["time"]["task_elapsed_secs"], 2);
        assert_eq!(value["time"]["elapsed_secs"], 12);
        // Tokens are the session's, not the task's.
        assert_eq!(value["tokens"]["unreported_calls"], 1);
    }

    #[test]
    fn session_status_default_notice_secs_is_five_minutes_a_tenth_or_one_second() {
        assert_eq!(default_notice_secs(7_200), 300);
        assert_eq!(default_notice_secs(600), 60);
        assert_eq!(default_notice_secs(5), 1);
        assert_eq!(default_notice_secs(1), 1);
        assert_eq!(notice_secs(Some(0), 7_200), 0);
        assert_eq!(notice_secs(Some(90), 150), 90);
        assert_eq!(notice_secs(None, 150), 15);
    }

    #[test]
    fn session_status_take_due_notice_fires_once_inside_the_margin() {
        let started = Instant::now();
        let deadline = SessionDeadline::after(started, SystemTime::now(), 30);
        let status = SessionStatus::new(started, false, None, Some(deadline), 20);
        assert_eq!(
            status.take_due_notice(started + Duration::from_secs(9)),
            None
        );
        let notice = status
            .take_due_notice(started + Duration::from_secs(10))
            .expect("due once 20 seconds are left");
        assert_eq!(notice.left_secs, 20);
        assert_eq!(notice.notice_secs, 20);
        assert_eq!(
            notice.text(),
            format!(
                "Session deadline: the runtime ends this session at {}, in 20 seconds.",
                deadline.at_rfc3339()
            )
        );
        assert_eq!(
            status.take_due_notice(started + Duration::from_secs(11)),
            None
        );
        assert_eq!(
            status.take_due_notice(started + Duration::from_secs(40)),
            None
        );
    }

    #[test]
    fn session_status_take_due_notice_never_fires_at_zero_or_without_a_deadline() {
        let started = Instant::now();
        let deadline = SessionDeadline::after(started, SystemTime::now(), 30);
        let silent = SessionStatus::new(started, false, None, Some(deadline), 0);
        assert_eq!(
            silent.take_due_notice(started + Duration::from_secs(29)),
            None
        );
        let unbounded = SessionStatus::new(started, false, None, None, 300);
        assert_eq!(unbounded.take_due_notice(started), None);
        // A deadline already past still gives its one notice, saying no time is left.
        let past = SessionStatus::new(started, false, None, Some(deadline), 5);
        let notice = past
            .take_due_notice(started + Duration::from_secs(31))
            .unwrap();
        assert_eq!(notice.left_secs, 0);
    }

    #[test]
    fn session_status_deadline_spells_at_in_utc_to_the_second() {
        let started = Instant::now();
        let wall = SystemTime::UNIX_EPOCH + Duration::from_millis(1_792_497_600_750);
        let deadline = SessionDeadline::after(started, wall, 600);
        assert_eq!(deadline.at_rfc3339(), "2026-10-20T12:10:00Z");
        assert_eq!(deadline.secs(), 600);
        assert_eq!(
            deadline.left(started + Duration::from_secs(700)),
            Duration::ZERO
        );
    }

    #[test]
    fn session_status_deadline_beyond_the_bound_is_taken_as_the_bound() {
        let started = Instant::now();
        let deadline = SessionDeadline::after(started, SystemTime::now(), u64::MAX);
        assert_eq!(deadline.secs(), MAX_DEADLINE_SECS);
        assert_eq!(
            deadline.left(started),
            Duration::from_secs(MAX_DEADLINE_SECS)
        );
        assert!(deadline.at_rfc3339().ends_with('Z'));
    }

    #[test]
    fn session_status_until_leaves_the_difference_zero_when_past_and_the_bound_at_most() {
        let started = Instant::now();
        let wall = SystemTime::UNIX_EPOCH + Duration::from_secs(1_792_497_600);
        let ahead = SessionDeadline::until(started, wall, wall + Duration::from_secs(90));
        assert_eq!(ahead.left(started), Duration::from_secs(90));
        assert_eq!(ahead.secs(), 90);
        assert_eq!(ahead.source(), DeadlineSource::Inherited);
        assert_eq!(ahead.at_rfc3339(), "2026-10-20T12:01:30Z");

        let past = SessionDeadline::until(started, wall, wall - Duration::from_secs(5));
        assert_eq!(past.left(started), Duration::ZERO);
        assert_eq!(past.secs(), 0);
        // A passed moment still spells the moment it was.
        assert_eq!(past.at_rfc3339(), "2026-10-20T11:59:55Z");

        let far = SessionDeadline::until(
            started,
            wall,
            wall + Duration::from_secs(MAX_DEADLINE_SECS + 10),
        );
        assert_eq!(far.left(started), Duration::from_secs(MAX_DEADLINE_SECS));
        assert_eq!(far.secs(), MAX_DEADLINE_SECS);
    }

    #[test]
    fn session_status_until_rounds_secs_up() {
        let started = Instant::now();
        let wall = SystemTime::UNIX_EPOCH + Duration::from_secs(1_792_497_600);
        let deadline = SessionDeadline::until(started, wall, wall + Duration::from_millis(29_001));
        assert_eq!(deadline.secs(), 30);
        assert_eq!(deadline.left(started), Duration::from_millis(29_001));
        let whole = SessionDeadline::until(started, wall, wall + Duration::from_secs(29));
        assert_eq!(whole.secs(), 29);
        let instant = SessionDeadline::until(started, wall, wall + Duration::from_nanos(1));
        assert_eq!(instant.secs(), 1);
    }

    #[test]
    fn session_status_unix_millis_round_trip_to_the_same_second() {
        let started = Instant::now();
        let wall = SystemTime::UNIX_EPOCH + Duration::from_nanos(1_792_497_600_999_999_999);
        let own = SessionDeadline::after(started, wall, 60);
        assert_eq!(own.source(), DeadlineSource::Own);
        let millis = own.at_unix_millis();
        assert_eq!(millis, 1_792_497_660_999);
        // A child that starts 3.5 s later inherits the same moment, to the millisecond.
        let child_started = started + Duration::from_millis(3_500);
        let child_wall = wall + Duration::from_millis(3_500);
        let inherited = SessionDeadline::from_unix_millis(child_started, child_wall, millis);
        assert_eq!(inherited.at_rfc3339(), own.at_rfc3339());
        assert_eq!(inherited.at_unix_millis(), millis);
        assert_eq!(inherited.secs(), 57);
        assert!(own.left(child_started) - inherited.left(child_started) < Duration::from_millis(1));

        // A moment the wall clock cannot hold is the bound, not a panic.
        let overflow = SessionDeadline::from_unix_millis(started, wall, u64::MAX);
        assert_eq!(overflow.secs(), MAX_DEADLINE_SECS);
        assert_eq!(
            overflow.left(started),
            Duration::from_secs(MAX_DEADLINE_SECS)
        );
        let epoch = SessionDeadline::from_unix_millis(started, wall, 0);
        assert_eq!(epoch.left(started), Duration::ZERO);
    }

    #[test]
    fn session_status_earliest_keeps_the_earlier_and_the_inherited_on_a_tie() {
        let started = Instant::now();
        let wall = SystemTime::UNIX_EPOCH + Duration::from_secs(1_792_497_600);
        let own = SessionDeadline::after(started, wall, 60);
        let sooner = SessionDeadline::until(started, wall, wall + Duration::from_secs(30));
        let later = SessionDeadline::until(started, wall, wall + Duration::from_secs(90));
        let tie = SessionDeadline::until(started, wall, wall + Duration::from_secs(60));

        assert_eq!(
            SessionDeadline::earliest(Some(own), Some(sooner)),
            Some(sooner)
        );
        assert_eq!(SessionDeadline::earliest(Some(own), Some(later)), Some(own));
        let tied = SessionDeadline::earliest(Some(own), Some(tie)).unwrap();
        assert_eq!(tied.source(), DeadlineSource::Inherited);
        assert_eq!(tied.secs(), 60);
        assert_eq!(SessionDeadline::earliest(Some(own), None), Some(own));
        assert_eq!(SessionDeadline::earliest(None, Some(later)), Some(later));
        assert_eq!(SessionDeadline::earliest(None, None), None);
        assert_eq!(DeadlineSource::Own.as_str(), "own");
        assert_eq!(DeadlineSource::Inherited.as_str(), "inherited");
    }

    #[test]
    fn session_status_descriptions_name_every_field_and_no_timing_advice() {
        for description in [description_http(), description_process()] {
            for field in [
                "elapsed_secs",
                "task_elapsed_secs",
                "deadline",
                "left_secs",
                "`turns`",
                "`cap`",
                "`context`",
                "`tokens`",
                "`cached`",
                "`cache_write`",
                "unreported_calls",
                "`spend`",
                "`ceiling`",
                "`machine`",
                "tokens_today",
                "No money amount is kept",
                "\"none\"",
                "\"unknown\"",
            ] {
                assert!(
                    description.contains(field),
                    "{field} missing: {description}"
                );
            }
            let lower = description.to_lowercase();
            for advice in ["when", "should", "call this", "periodically"] {
                assert!(!lower.contains(advice), "{advice}: {description}");
            }
        }
        assert!(description_http().contains("measured_by"));
        assert!(description_http().contains("\"actual\""));
        assert!(description_process().contains("the harness keeps its own context"));
    }
}
