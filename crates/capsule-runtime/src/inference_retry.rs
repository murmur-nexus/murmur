//! Making a `transport: http` driver call again after a transient failure.
//!
//! The agent loop's turn and a hook's `run-inference` each wrap one driver call in a loop over
//! attempts. Every attempt sends the same payload through the same driver choice; one that failed
//! transiently is followed, after a wait, by another, until one succeeds, fails for a reason that
//! is not transient, or a stop in [`RetryPolicy::decide`] ends the retrying. Nothing here touches
//! the trace, the spend meter or the guest: this module decides, and the two call sites act.
//!
//! What makes a failure transient is [`classify`]'s: what the credential gateway recorded for the
//! attempt ([`UpstreamOutcome`]) and what the driver said about it through the reserved
//! [`TRANSIENT_METADATA_KEY`] on its `tool-result`.

use std::{
    future::Future,
    time::{Duration, Instant, SystemTime},
};

use http::HeaderMap;
use murmur_artifact::InferenceRetry;
use wasmtime_wasi_http::Error as WasiHttpError;

use crate::{agent::ProviderCounts, credential_gateway::UpstreamOutcome, spend::SpendAdmission};

/// The reserved `tool-result.metadata` key through which a driver marks its failure transient
/// (`"true"`) or vetoes a retry of a status that would otherwise be retried (`"false"`).
pub(crate) const TRANSIENT_METADATA_KEY: &str = "transient";

/// `inference.retry`, as the retry loop applies it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RetryPolicy {
    /// The most retries one call makes after its first attempt.
    pub(crate) max_retries: u32,
    initial_delay: Duration,
    max_delay: Duration,
}

impl From<InferenceRetry> for RetryPolicy {
    fn from(retry: InferenceRetry) -> Self {
        Self {
            max_retries: retry.max_retries,
            initial_delay: Duration::from_millis(u64::from(retry.initial_delay_ms)),
            max_delay: Duration::from_millis(u64::from(retry.max_delay_ms)),
        }
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        InferenceRetry::default().into()
    }
}

/// How one failed attempt ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttemptFailure {
    /// The driver could not be run to a result: instantiation failed, an import did not match, it
    /// trapped. It returned no metadata.
    Dispatch,
    /// The driver returned a status other than `passed`.
    Status,
    /// The driver returned `passed` with a payload whose `stop_reason` is `"error"`.
    StopReasonError,
}

/// Why a retried attempt failed, as `inference_retry.reason` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetryReason {
    /// The gateway received a non-2xx status.
    Status,
    /// The gateway's connection failed before a response head arrived.
    Connection,
    /// The driver marked its failure transient.
    Driver,
}

impl RetryReason {
    /// The reason recorded for an attempt the gateway recorded `outcome` for.
    pub(crate) fn of(outcome: Option<UpstreamOutcome>) -> Self {
        match outcome {
            Some(UpstreamOutcome::Response { status, .. }) if !(200..300).contains(&status) => {
                Self::Status
            }
            Some(UpstreamOutcome::ConnectionFailed { .. }) => Self::Connection,
            _ => Self::Driver,
        }
    }

    pub(crate) fn wire_name(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Connection => "connection",
            Self::Driver => "driver",
        }
    }

    /// The status an `inference_retry` line names for an attempt the gateway recorded `outcome`
    /// for: the recorded status of a [`Self::Status`] retry, and none for any other, whose
    /// response, if one arrived, was a `2xx`.
    pub(crate) fn provider_status(self, outcome: Option<UpstreamOutcome>) -> Option<u16> {
        outcome
            .and_then(UpstreamOutcome::status)
            .filter(|_| self == Self::Status)
    }

    /// Whether an attempt that failed this way is released without charge: the provider either
    /// refused it with a status or never answered it, and bills neither. A failure the driver
    /// reported after a successful response is charged as any failed call is.
    pub(crate) fn releases_admission(self) -> bool {
        matches!(self, Self::Status | Self::Connection)
    }
}

/// Why the runtime stopped retrying a call that failed transiently, as the `inference` record's
/// `retry_stopped` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetryStop {
    /// `max_retries` retries were already made.
    MaxRetries,
    /// The provider asked for a wait longer than `max_delay_ms`.
    RetryAfter,
    /// The wait would end at or after the session deadline.
    Deadline,
}

impl RetryStop {
    pub(crate) fn wire_name(self) -> &'static str {
        match self {
            Self::MaxRetries => "max_retries",
            Self::RetryAfter => "retry_after",
            Self::Deadline => "deadline",
        }
    }
}

/// What follows a transient failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetryDecision {
    /// Wait this long, then make the call again.
    Retry { wait: Duration },
    /// Fail the call as it failed.
    Stop(RetryStop),
}

/// What the retry loop knows about one call once it ends: how many attempts it took, and why a
/// transient failure was not retried. Written on the call's `inference` record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct RetryNote {
    /// The attempts made, when more than one.
    pub(crate) attempts: Option<u32>,
    /// Set only when the call's last attempt failed transiently and was not retried.
    pub(crate) stopped: Option<RetryStop>,
}

impl RetryNote {
    /// The note for a call that took `attempts` attempts and ended on `stopped`.
    pub(crate) fn new(attempts: u32, stopped: Option<RetryStop>) -> Self {
        Self {
            attempts: (attempts > 1).then_some(attempts),
            stopped,
        }
    }

    /// `<n> attempts; retry stopped: <reason>`, or the half of it that applies, naming what a
    /// failed call's retrying came to; `None` for a call that made one attempt and was not stopped.
    pub(crate) fn describe(self) -> Option<String> {
        if self.attempts.is_none() && self.stopped.is_none() {
            return None;
        }
        let attempts = match self.attempts.unwrap_or(1) {
            1 => "1 attempt".to_string(),
            n => format!("{n} attempts"),
        };
        Some(match self.stopped {
            Some(stop) => format!("{attempts}; retry stopped: {}", stop.wire_name()),
            None => attempts,
        })
    }
}

/// One retry, as its `inference_retry` trace line records it. A hook's `run-inference` buffers it
/// beside its other records for the agent loop to write; the agent loop writes its own at once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InferenceRetryRecord {
    /// `hook:<name>` for a hook's `run-inference`; `None` for an agent-loop turn.
    pub(crate) origin: Option<String>,
    /// The attempt that failed, 1-based.
    pub(crate) attempt: u32,
    pub(crate) max_retries: u32,
    pub(crate) reason: RetryReason,
    /// The status the gateway received for the failed attempt, when it received one.
    pub(crate) provider_status: Option<u16>,
    /// The failed attempt's error text, already redacted.
    pub(crate) error: String,
    /// The wait before the next attempt.
    pub(crate) delay_ms: u64,
    /// The wait the provider asked for, when it asked for one; equal to `delay_ms` then.
    pub(crate) retry_after_ms: Option<u64>,
    /// What the spend meter was charged for the failed attempt.
    pub(crate) charged_tokens: u64,
}

/// Account for an attempt that is about to be retried, returning what it was charged.
///
/// An attempt that failed on a status or a broken connection is released without charge. Any
/// other is charged as a failed call is: the provider's counts from its failed payload when they
/// make a report, its input estimate alone otherwise. Only that last charge reads the estimate,
/// so only it waits for an estimate still counting.
pub(crate) fn charge_retried_attempt(
    admission: SpendAdmission,
    reason: RetryReason,
    counts: Option<ProviderCounts>,
) -> u64 {
    if reason.releases_admission() {
        admission.release();
        return 0;
    }
    match counts {
        Some(counts) => {
            admission.settle(counts.request, counts.completion);
            counts.request.saturating_add(counts.completion)
        }
        None => admission.charge_input(),
    }
}

/// `duration` in whole milliseconds, saturating.
pub(crate) fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

impl RetryPolicy {
    /// The backoff before retry `retry` (0-based): uniform over `[d/2, d]` where
    /// `d = min(max_delay, initial_delay × 2^retry)`, chosen by `draw`. Saturating, so no
    /// `retry` overflows.
    pub(crate) fn backoff(&self, retry: u32, draw: u64) -> Duration {
        let initial = millis(self.initial_delay);
        let max = millis(self.max_delay);
        let scaled = 1u64
            .checked_shl(retry)
            .and_then(|factor| initial.checked_mul(factor))
            .unwrap_or(u64::MAX);
        let d = scaled.min(max);
        let low = d / 2;
        let span = d - low;
        let jitter = match span.checked_add(1) {
            Some(width) => draw % width,
            None => draw,
        };
        Duration::from_millis(low + jitter)
    }

    /// Whether a call that failed transiently after `retries_made` retries is made again, and
    /// after how long. `retry_after` is the wait the provider asked for, which replaces the
    /// backoff; `deadline` is the session deadline, if any. Checked in order: the retry count,
    /// the provider's wait against `max_delay`, then the deadline.
    pub(crate) fn decide(
        &self,
        retries_made: u32,
        retry_after: Option<Duration>,
        draw: u64,
        deadline: Option<Instant>,
        now: Instant,
    ) -> RetryDecision {
        if retries_made >= self.max_retries {
            return RetryDecision::Stop(RetryStop::MaxRetries);
        }
        if retry_after.is_some_and(|wait| wait > self.max_delay) {
            return RetryDecision::Stop(RetryStop::RetryAfter);
        }
        let wait = retry_after.unwrap_or_else(|| self.backoff(retries_made, draw));
        if let Some(deadline) = deadline {
            if now.checked_add(wait).is_none_or(|end| end >= deadline) {
                return RetryDecision::Stop(RetryStop::Deadline);
            }
        }
        RetryDecision::Retry { wait }
    }
}

/// A uniformly random value for [`RetryPolicy::backoff`]'s jitter. `0` when the system has no
/// randomness to give, which waits the shortest backoff.
pub(crate) fn random_draw() -> u64 {
    let mut bytes = [0u8; 8];
    match getrandom::fill(&mut bytes) {
        Ok(()) => u64::from_le_bytes(bytes),
        Err(_) => 0,
    }
}

/// The wait a response asked for: `retry-after-ms` as integer milliseconds when present and
/// parseable, else `retry-after` as delta-seconds or an HTTP-date measured from `now`. A date in
/// the past asks for no wait; a value that parses as neither asks for none at all.
pub(crate) fn retry_after(headers: &HeaderMap, now: SystemTime) -> Option<Duration> {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
    };
    if let Some(ms) = header("retry-after-ms").and_then(|value| value.parse::<u64>().ok()) {
        return Some(Duration::from_millis(ms));
    }
    let value = header("retry-after")?;
    if let Ok(secs) = value.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let at = chrono::DateTime::parse_from_rfc2822(value).ok()?;
    let at = SystemTime::UNIX_EPOCH
        .checked_add(Duration::from_secs(u64::try_from(at.timestamp()).ok()?))?;
    Some(at.duration_since(now).unwrap_or(Duration::ZERO))
}

/// What a driver's metadata says about its failure: the first [`TRANSIENT_METADATA_KEY`] tuple in
/// list order, `Some(true)` for `"true"`, `Some(false)` for `"false"`, `None` for any other value
/// or none.
pub(crate) fn transient_metadata(metadata: &[(String, String)]) -> Option<bool> {
    let (_, value) = metadata
        .iter()
        .find(|(key, _)| key == TRANSIENT_METADATA_KEY)?;
    match value.as_str() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// Whether a status is retried: `408`, `429`, and every `5xx` but `501` and `505`.
pub(crate) fn retryable_status(status: u16) -> bool {
    matches!(status, 408 | 429) || ((500..600).contains(&status) && !matches!(status, 501 | 505))
}

/// Whether a connection failure the gateway saw before a response head is transient: the
/// connection was reset, terminated or timed out, or the response was incomplete. A refused
/// connection, a DNS failure, a TLS failure and a malformed request are not.
pub(crate) fn transient_connection_error(error: &WasiHttpError) -> bool {
    match error {
        WasiHttpError::ConnectionTerminated
        | WasiHttpError::ConnectionTimeout
        | WasiHttpError::ConnectionReadTimeout
        | WasiHttpError::ConnectionWriteTimeout
        | WasiHttpError::ConnectionLimitReached
        | WasiHttpError::DnsTimeout
        | WasiHttpError::DestinationUnavailable
        | WasiHttpError::HttpResponseIncomplete => true,
        WasiHttpError::Connect(io) => transient_io(io),
        WasiHttpError::Hyper(hyper) => {
            hyper.is_incomplete_message()
                || hyper.is_closed()
                || hyper.is_canceled()
                || hyper.is_timeout()
                || std::error::Error::source(hyper)
                    .and_then(|source| source.downcast_ref::<std::io::Error>())
                    .is_some_and(transient_io)
        }
        _ => false,
    }
}

fn transient_io(error: &std::io::Error) -> bool {
    use std::io::ErrorKind;
    matches!(
        error.kind(),
        ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
            | ErrorKind::TimedOut
            | ErrorKind::BrokenPipe
    )
}

/// Whether a failed attempt is retried, by these rules in order:
///
/// 1. a credential rejection the gateway recorded for it is never retried;
/// 2. the driver's `transient: "false"` is not;
/// 3. the driver's `transient: "true"` is;
/// 4. a transient connection failure the gateway recorded is;
/// 5. a recorded [`retryable_status`] is;
/// 6. nothing else is.
///
/// A [`AttemptFailure::Dispatch`] carries no metadata, so `transient` is ignored for it and only
/// rule 4 can make it retryable.
pub(crate) fn classify(
    failure: AttemptFailure,
    transient: Option<bool>,
    credential_rejected: bool,
    outcome: Option<UpstreamOutcome>,
) -> bool {
    if credential_rejected {
        return false;
    }
    let transient = match failure {
        AttemptFailure::Dispatch => None,
        AttemptFailure::Status | AttemptFailure::StopReasonError => transient,
    };
    if let Some(transient) = transient {
        return transient;
    }
    match outcome {
        Some(UpstreamOutcome::ConnectionFailed { transient }) => transient,
        Some(UpstreamOutcome::Response { status, .. }) => retryable_status(status),
        None => false,
    }
}

/// Waits `wait`, or until `cancel` resolves. `true` when the wait ran out, `false` when it was
/// canceled.
pub(crate) async fn wait(wait: Duration, cancel: Option<impl Future<Output = ()>>) -> bool {
    match cancel {
        Some(cancel) => tokio::select! {
            biased;
            () = cancel => false,
            () = tokio::time::sleep(wait) => true,
        },
        None => {
            tokio::time::sleep(wait).await;
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use http::HeaderValue;

    use super::*;

    fn policy_with(max_retries: u32, initial_delay_ms: u32, max_delay_ms: u32) -> RetryPolicy {
        InferenceRetry {
            max_retries,
            initial_delay_ms,
            max_delay_ms,
        }
        .into()
    }

    /// A status or connection failure releases its admission without waiting for an estimate
    /// still counting; a driver failure with no report charges the count.
    #[test]
    fn inference_retry_charge_reads_the_estimate_only_to_charge_it() {
        let meter = std::sync::Arc::new(crate::spend::SpendMeter::new(None, None));
        let (release, released) = std::sync::mpsc::channel::<()>();
        let estimate = crate::agent::RequestEstimate::counting_with(String::new(), move |_| {
            let _ = released.recv();
            120
        });

        for reason in [RetryReason::Status, RetryReason::Connection] {
            let admission = meter.admit_request(&estimate, 500).unwrap();
            assert_eq!(charge_retried_attempt(admission, reason, None), 0);
        }
        assert!(!meter.has_open_admission());
        assert_eq!(meter.used(), 0);

        let admission = meter.admit_request(&estimate, 500).unwrap();
        release.send(()).unwrap();
        assert_eq!(
            charge_retried_attempt(admission, RetryReason::Driver, None),
            120
        );
        assert_eq!(meter.used(), 120);
    }

    #[test]
    fn inference_retry_note_describes_the_attempts_and_the_stop() {
        assert_eq!(RetryNote::new(1, None).describe(), None);
        assert_eq!(
            RetryNote::new(3, Some(RetryStop::MaxRetries))
                .describe()
                .as_deref(),
            Some("3 attempts; retry stopped: max_retries")
        );
        assert_eq!(
            RetryNote::new(1, Some(RetryStop::Deadline))
                .describe()
                .as_deref(),
            Some("1 attempt; retry stopped: deadline")
        );
        assert_eq!(
            RetryNote::new(2, None).describe().as_deref(),
            Some("2 attempts")
        );
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    fn status(status: u16) -> Option<UpstreamOutcome> {
        Some(UpstreamOutcome::Response {
            status,
            retry_after: None,
        })
    }

    #[test]
    fn inference_retry_after_ms_wins_over_retry_after() {
        let now = SystemTime::now();
        assert_eq!(
            retry_after(
                &headers(&[("retry-after-ms", "1500"), ("retry-after", "9")]),
                now
            ),
            Some(Duration::from_millis(1500))
        );
    }

    #[test]
    fn inference_retry_after_delta_seconds() {
        assert_eq!(
            retry_after(&headers(&[("retry-after", "3")]), SystemTime::now()),
            Some(Duration::from_millis(3000))
        );
    }

    #[test]
    fn inference_retry_after_http_date_ahead_and_past() {
        let now = SystemTime::now();
        let ahead =
            chrono::DateTime::<chrono::Utc>::from(now + Duration::from_secs(5)).to_rfc2822();
        let wait = retry_after(&headers(&[("retry-after", &ahead)]), now).unwrap();
        assert!(
            (4_000..=6_000).contains(&wait.as_millis()),
            "{ahead}: {wait:?}"
        );
        let past =
            chrono::DateTime::<chrono::Utc>::from(now - Duration::from_secs(60)).to_rfc2822();
        assert_eq!(
            retry_after(&headers(&[("retry-after", &past)]), now),
            Some(Duration::ZERO)
        );
        assert_eq!(
            retry_after(
                &headers(&[("retry-after", "Wed, 21 Oct 2015 07:28:00 GMT")]),
                now
            ),
            Some(Duration::ZERO)
        );
    }

    #[test]
    fn inference_retry_after_garbage_is_none() {
        let now = SystemTime::now();
        assert_eq!(retry_after(&headers(&[("retry-after", "soon")]), now), None);
        assert_eq!(retry_after(&headers(&[("retry-after", "-3")]), now), None);
        assert_eq!(retry_after(&headers(&[]), now), None);
        // An unparseable `retry-after-ms` falls through to `retry-after`.
        assert_eq!(
            retry_after(
                &headers(&[("retry-after-ms", "x"), ("retry-after", "2")]),
                now
            ),
            Some(Duration::from_secs(2))
        );
    }

    #[test]
    fn inference_retry_backoff_is_jittered_within_half_to_full() {
        let policy = policy_with(6, 10, 2_000);
        assert_eq!(policy.backoff(0, 0), Duration::from_millis(5));
        assert_eq!(policy.backoff(0, 5), Duration::from_millis(10));
        assert_eq!(policy.backoff(0, 6), Duration::from_millis(5));
        assert_eq!(policy.backoff(1, 0), Duration::from_millis(10));
        assert_eq!(policy.backoff(1, 10), Duration::from_millis(20));
        for draw in [0, 1, 7, 99, u64::MAX] {
            let wait = policy.backoff(2, draw).as_millis();
            assert!((20..=40).contains(&wait), "{wait}");
        }
        // Capped at max_delay, and saturating for any retry index.
        assert_eq!(policy.backoff(10, 1_000), Duration::from_millis(2_000));
        assert_eq!(policy.backoff(63, 0), Duration::from_millis(1_000));
        assert_eq!(policy.backoff(64, 0), Duration::from_millis(1_000));
        assert!(policy.backoff(u32::MAX, u64::MAX) <= Duration::from_millis(2_000));
        // A one-millisecond delay waits 0 or 1 ms.
        assert!(policy_with(1, 1, 1).backoff(0, 0) <= Duration::from_millis(1));
    }

    #[test]
    fn inference_retry_decide_checks_count_then_retry_after_then_deadline() {
        let policy = policy_with(2, 10, 5_000);
        let now = Instant::now();
        assert_eq!(
            policy.decide(2, None, 0, None, now),
            RetryDecision::Stop(RetryStop::MaxRetries)
        );
        assert_eq!(
            policy.decide(2, Some(Duration::from_secs(60)), 0, None, now),
            RetryDecision::Stop(RetryStop::MaxRetries)
        );
        assert_eq!(
            policy.decide(0, Some(Duration::from_secs(6)), 0, None, now),
            RetryDecision::Stop(RetryStop::RetryAfter)
        );
        assert_eq!(
            policy.decide(0, Some(Duration::from_secs(5)), 0, None, now),
            RetryDecision::Retry {
                wait: Duration::from_secs(5)
            }
        );
        let deadline = now + Duration::from_secs(5);
        assert_eq!(
            policy.decide(0, Some(Duration::from_secs(5)), 0, Some(deadline), now),
            RetryDecision::Stop(RetryStop::Deadline),
            "a wait ending at the deadline is refused"
        );
        assert_eq!(
            policy.decide(1, None, 0, Some(deadline), now),
            RetryDecision::Retry {
                wait: Duration::from_millis(10)
            }
        );
        assert_eq!(
            policy.decide(0, None, 0, None, now),
            RetryDecision::Retry {
                wait: Duration::from_millis(5)
            }
        );
        assert_eq!(
            policy_with(0, 1, 1).decide(0, None, 0, None, now),
            RetryDecision::Stop(RetryStop::MaxRetries)
        );
    }

    #[test]
    fn inference_retry_transient_metadata_takes_the_first_tuple_and_two_values() {
        let md = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        assert_eq!(transient_metadata(&md(&[])), None);
        assert_eq!(
            transient_metadata(&md(&[("transient", "true")])),
            Some(true)
        );
        assert_eq!(
            transient_metadata(&md(&[("transient", "false")])),
            Some(false)
        );
        assert_eq!(transient_metadata(&md(&[("transient", "TRUE")])), None);
        assert_eq!(transient_metadata(&md(&[("transient", "yes")])), None);
        assert_eq!(
            transient_metadata(&md(&[("transient", "false"), ("transient", "true")])),
            Some(false)
        );
        assert_eq!(
            transient_metadata(&md(&[("transient", "maybe"), ("transient", "true")])),
            None,
            "only the first tuple counts"
        );
    }

    #[test]
    fn inference_retry_statuses() {
        for retried in [408, 429, 500, 502, 503, 504, 529, 599] {
            assert!(retryable_status(retried), "{retried}");
        }
        for not in [200, 307, 400, 401, 403, 404, 409, 413, 422, 501, 505, 600] {
            assert!(!retryable_status(not), "{not}");
        }
    }

    #[test]
    fn inference_retry_classify_follows_the_rules_in_order() {
        use AttemptFailure::*;
        // 1. A credential rejection is never retried, whatever the driver says.
        assert!(!classify(Status, Some(true), true, status(401)));
        // 2. and 3. The driver's word decides over the status.
        assert!(!classify(Status, Some(false), false, status(429)));
        assert!(classify(StopReasonError, Some(true), false, status(200)));
        assert!(classify(Status, Some(true), false, status(400)));
        // 4. A transient connection failure.
        let dropped = Some(UpstreamOutcome::ConnectionFailed { transient: true });
        let refused = Some(UpstreamOutcome::ConnectionFailed { transient: false });
        assert!(classify(Status, None, false, dropped));
        assert!(classify(Dispatch, None, false, dropped));
        assert!(!classify(Status, None, false, refused));
        // 5. A retryable status.
        assert!(classify(Status, None, false, status(503)));
        assert!(!classify(Status, None, false, status(501)));
        assert!(!classify(Status, None, false, status(400)));
        // 6. Nothing recorded and nothing said.
        assert!(!classify(Status, None, false, None));
        assert!(!classify(StopReasonError, None, false, status(200)));
        // A dispatch error has no metadata to read.
        assert!(!classify(Dispatch, Some(true), false, None));
        assert!(classify(Dispatch, Some(false), false, dropped));
        assert!(classify(Dispatch, None, false, status(429)));
    }

    #[test]
    fn inference_retry_connection_errors() {
        use std::io::{Error, ErrorKind};
        for transient in [
            WasiHttpError::ConnectionTerminated,
            WasiHttpError::ConnectionTimeout,
            WasiHttpError::ConnectionReadTimeout,
            WasiHttpError::ConnectionWriteTimeout,
            WasiHttpError::ConnectionLimitReached,
            WasiHttpError::DnsTimeout,
            WasiHttpError::DestinationUnavailable,
            WasiHttpError::HttpResponseIncomplete,
            WasiHttpError::Connect(Error::from(ErrorKind::ConnectionReset)),
            WasiHttpError::Connect(Error::from(ErrorKind::ConnectionAborted)),
            WasiHttpError::Connect(Error::from(ErrorKind::TimedOut)),
            WasiHttpError::Connect(Error::from(ErrorKind::BrokenPipe)),
        ] {
            assert!(transient_connection_error(&transient), "{transient:?}");
        }
        for not in [
            WasiHttpError::ConnectionRefused,
            WasiHttpError::Connect(Error::from(ErrorKind::ConnectionRefused)),
            WasiHttpError::DnsError {
                rcode: None,
                info_code: None,
            },
            WasiHttpError::DestinationNotFound,
            WasiHttpError::Tls(Error::from(ErrorKind::InvalidData)),
            WasiHttpError::TlsProtocolError,
            WasiHttpError::TlsCertificateError,
            WasiHttpError::HttpRequestUriInvalid,
            WasiHttpError::InternalError(None),
        ] {
            assert!(!transient_connection_error(&not), "{not:?}");
        }
    }

    #[test]
    fn inference_retry_reason_and_charging() {
        assert_eq!(RetryReason::of(status(429)), RetryReason::Status);
        assert_eq!(RetryReason::of(status(200)), RetryReason::Driver);
        assert_eq!(RetryReason::of(None), RetryReason::Driver);
        assert_eq!(
            RetryReason::of(Some(UpstreamOutcome::ConnectionFailed { transient: true })),
            RetryReason::Connection
        );
        assert_eq!(RetryReason::Status.provider_status(status(429)), Some(429));
        assert_eq!(RetryReason::Driver.provider_status(status(200)), None);
        assert!(RetryReason::Status.releases_admission());
        assert!(RetryReason::Connection.releases_admission());
        assert!(!RetryReason::Driver.releases_admission());
        assert_eq!(RetryNote::new(1, None), RetryNote::default());
        assert_eq!(
            RetryNote::new(3, Some(RetryStop::MaxRetries)),
            RetryNote {
                attempts: Some(3),
                stopped: Some(RetryStop::MaxRetries)
            }
        );
    }

    #[tokio::test]
    async fn inference_retry_wait_is_cut_short_by_a_cancel() {
        let started = Instant::now();
        assert!(!wait(Duration::from_secs(60), Some(async {})).await);
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(wait(Duration::from_millis(5), None::<std::future::Ready<()>>).await);
        assert!(wait(Duration::from_millis(5), Some(std::future::pending::<()>())).await);
    }
}
