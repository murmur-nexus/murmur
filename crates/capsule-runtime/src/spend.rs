//! Inference spend ceilings: how many tokens a session, and every session on this machine, may
//! spend before a driver call is refused without reaching the provider.
//!
//! Every number here is the runtime's own tiktoken measurement — `input_tokens` plus
//! `output_tokens`, exactly what the trace's `inference` lines carry — so an operator checks a
//! ceiling by summing the trace. A provider's reported `usage` never enters.
//!
//! [`SpendMeter`] is the one per-session account. A call is admitted against it before dispatch
//! and settled once its output is measured; the [`SpendAdmission`] guard releases its reservation
//! on every other path. The inference gateway sends nothing unless an admission is open, so a
//! driver invocation that skipped admission cannot reach the provider with the key.
//!
//! [`MachineLedger`] is the shared, append-only daily file every `mur run` on this `~/.murmur`
//! writes its settled calls to. There is no lock and no daemon: each process reads only the bytes
//! appended since its last read, so the machine total lags by whatever other processes have
//! admitted and not yet settled, and the ceiling can be overshot by up to the calls in flight
//! when it is reached.

use std::{
    fmt,
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use crate::errors::RuntimeError;

/// The ledger directory, under `~/.murmur`.
pub(crate) const SPEND_DIR: &str = "spend";

/// A daily ledger whose date is more than this many days before today is removed when a ledger
/// is opened.
pub(crate) const SPEND_LEDGER_RETENTION_DAYS: i64 = 30;

/// Held on the ledger directory whether or not this run created it.
pub(crate) const SPEND_DIR_MODE: u32 = 0o700;

/// Held on each daily ledger file.
pub(crate) const SPEND_LEDGER_FILE_MODE: u32 = 0o600;

/// What the ledger reads the date from. Injectable so a day rollover is testable.
pub(crate) type SpendClock = fn() -> DateTime<Utc>;

/// Which ceiling refused a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum SpendLimit {
    /// `inference.max_session_tokens`.
    Session,
    /// `spend.machine_tokens_per_day`.
    Machine,
}

/// A call refused because it would cross a ceiling. Never a driver or provider error: nothing was
/// sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct SpendRefusal {
    pub(crate) limit: SpendLimit,
    pub(crate) ceiling: u64,
    /// Settled tokens plus open reservations when the call was refused — this session's for a
    /// session refusal, the machine ledger's total plus this session's reservations for a machine
    /// refusal.
    pub(crate) used: u64,
    /// The refused call's input tokens plus the most output it may request.
    pub(crate) requested: u64,
}

impl fmt::Display for SpendRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            limit,
            ceiling,
            used,
            requested,
        } = self;
        match limit {
            SpendLimit::Session => write!(
                f,
                "spend ceiling reached: inference.max_session_tokens is {ceiling} and this session \
                 has used {used}; the next call could use up to {requested} more. The operator set \
                 this limit; retrying will not get past it."
            ),
            SpendLimit::Machine => write!(
                f,
                "spend ceiling reached: spend.machine_tokens_per_day is {ceiling} and capsules on \
                 this machine have used {used} today (UTC); the next call could use up to \
                 {requested} more. The operator set this limit; retrying will not get past it \
                 before it resets at 00:00 UTC."
            ),
        }
    }
}

#[derive(Debug, Default)]
struct MeterState {
    /// Settled tokens.
    used: u64,
    /// The sum of open admissions' reservations.
    reserved: u64,
    /// Open admissions.
    open: u32,
    /// The first session refusal. Once set, every later admission is refused.
    session_latched: Option<SpendRefusal>,
}

/// One session's spend account. Shared, as `Arc<SpendMeter>`, by the agent loop, every hook's
/// `run-inference` and the inference gateway.
pub(crate) struct SpendMeter {
    session_ceiling: Option<u64>,
    machine: Option<MachineLedger>,
    /// No file I/O happens while this is held.
    state: Mutex<MeterState>,
}

impl fmt::Debug for SpendMeter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpendMeter")
            .field("session_ceiling", &self.session_ceiling)
            .field("machine_ceiling", &self.machine_ceiling())
            .finish()
    }
}

impl SpendMeter {
    pub(crate) fn new(session_ceiling: Option<u64>, machine: Option<MachineLedger>) -> Self {
        Self {
            session_ceiling,
            machine,
            state: Mutex::new(MeterState::default()),
        }
    }

    /// A meter with no ceiling: every admission succeeds and `used` still accumulates.
    #[cfg(test)]
    pub(crate) fn unlimited() -> Self {
        Self::new(None, None)
    }

    /// `inference.max_session_tokens`, as this meter enforces it.
    pub(crate) fn session_ceiling(&self) -> Option<u64> {
        self.session_ceiling
    }

    /// `spend.machine_tokens_per_day`, when this meter keeps a ledger — `None` otherwise, including
    /// for a session the ceiling does not cover.
    pub(crate) fn machine_ceiling(&self) -> Option<u64> {
        self.machine.as_ref().map(|ledger| ledger.ceiling)
    }

    /// Admit one driver call whose input measures `input_tokens` and which may request up to
    /// `max_output_tokens` of output.
    ///
    /// Refused when the call does not fit under a ceiling together with what is already settled
    /// and reserved. A session refusal latches; a machine refusal does not, and is asked again on
    /// every admission.
    pub(crate) fn admit(
        self: &Arc<Self>,
        input_tokens: u64,
        max_output_tokens: u64,
    ) -> Result<SpendAdmission, SpendRefusal> {
        let requested = input_tokens.saturating_add(max_output_tokens);
        // Read before the meter lock is taken, so no file I/O runs under it.
        let machine_total = self.machine.as_ref().map(MachineLedger::refresh_total);

        let mut state = self.lock();
        let committed = state.used.saturating_add(state.reserved);
        if let Some(latched) = state.session_latched.as_ref() {
            return Err(SpendRefusal {
                used: committed,
                requested,
                ..latched.clone()
            });
        }
        if let Some(ceiling) = self.session_ceiling {
            if committed.saturating_add(requested) > ceiling {
                let refusal = SpendRefusal {
                    limit: SpendLimit::Session,
                    ceiling,
                    used: committed,
                    requested,
                };
                state.session_latched = Some(refusal.clone());
                return Err(refusal);
            }
        }
        if let (Some(ledger), Some(total)) = (self.machine.as_ref(), machine_total) {
            let used = total.saturating_add(state.reserved);
            if used.saturating_add(requested) > ledger.ceiling {
                return Err(SpendRefusal {
                    limit: SpendLimit::Machine,
                    ceiling: ledger.ceiling,
                    used,
                    requested,
                });
            }
        }
        state.reserved = state.reserved.saturating_add(requested);
        state.open = state.open.saturating_add(1);
        drop(state);

        Ok(SpendAdmission {
            meter: Arc::clone(self),
            reserved: requested,
            input_tokens,
            finished: false,
        })
    }

    /// Whether any admission is open. The inference gateway sends nothing when none is.
    pub(crate) fn has_open_admission(&self) -> bool {
        self.lock().open > 0
    }

    /// Settled tokens so far.
    #[cfg(test)]
    pub(crate) fn used(&self) -> u64 {
        self.lock().used
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MeterState> {
        // A poisoned meter still holds consistent integers: every update under it is a handful of
        // saturating additions that cannot panic midway.
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn finish(&self, reserved: u64, input_tokens: u64, output_tokens: u64) {
        // Appended before the reservation is released, so a concurrent admission can count this
        // call twice — once in the ledger, once as reserved — but never zero times.
        if let Some(ledger) = self.machine.as_ref() {
            ledger.append(input_tokens, output_tokens);
        }
        let mut state = self.lock();
        state.reserved = state.reserved.saturating_sub(reserved);
        state.open = state.open.saturating_sub(1);
        state.used = state
            .used
            .saturating_add(input_tokens.saturating_add(output_tokens));
    }
}

/// An open admission. [`Self::settle`] charges the measured call; dropping it unsettled — a
/// cancelled call, a dispatch error, a failed status — charges the input alone, which the call
/// already sent.
#[must_use = "an admission dropped at once charges its input without having sent anything"]
pub(crate) struct SpendAdmission {
    meter: Arc<SpendMeter>,
    reserved: u64,
    input_tokens: u64,
    finished: bool,
}

impl fmt::Debug for SpendAdmission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpendAdmission")
            .field("reserved", &self.reserved)
            .field("input_tokens", &self.input_tokens)
            .finish()
    }
}

impl SpendAdmission {
    /// Release the reservation and charge `input_tokens + output_tokens`.
    pub(crate) fn settle(mut self, input_tokens: u64, output_tokens: u64) {
        self.finished = true;
        self.meter
            .finish(self.reserved, input_tokens, output_tokens);
    }
}

impl Drop for SpendAdmission {
    fn drop(&mut self) {
        if !self.finished {
            self.meter.finish(self.reserved, self.input_tokens, 0);
        }
    }
}

/// One settled call, as a ledger line.
#[derive(Serialize, Deserialize)]
struct LedgerLine<'a> {
    ts: u64,
    #[serde(borrow)]
    session_id: std::borrow::Cow<'a, str>,
    input_tokens: u64,
    output_tokens: u64,
}

struct LedgerState {
    date: NaiveDate,
    /// Today's file, opened `O_APPEND`. Read positionally, so appends never move a read cursor.
    file: File,
    /// Bytes of `file` already counted — always the end of a complete line.
    offset: u64,
    /// Tokens on every complete, parseable line before `offset`.
    total: u64,
}

/// The shared daily ledger behind `spend.machine_tokens_per_day`.
pub(crate) struct MachineLedger {
    ceiling: u64,
    dir: PathBuf,
    session_id: String,
    clock: SpendClock,
    state: Mutex<LedgerState>,
}

impl fmt::Debug for MachineLedger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MachineLedger")
            .field("ceiling", &self.ceiling)
            .field("dir", &self.dir)
            .field("session_id", &self.session_id)
            .finish()
    }
}

impl MachineLedger {
    /// Open today's ledger under `~/.murmur/spend` for `session_id`, pruning old days.
    pub(crate) fn open(session_id: &str, ceiling: u64) -> Result<Self, RuntimeError> {
        let home = crate::state_store::murmur_home_dir().map_err(|message| {
            RuntimeError::SpendLedgerUnavailable {
                path: format!("~/.murmur/{SPEND_DIR}"),
                message,
            }
        })?;
        Self::open_in(home.join(SPEND_DIR), session_id, ceiling, Utc::now)
    }

    pub(crate) fn open_in(
        dir: PathBuf,
        session_id: &str,
        ceiling: u64,
        clock: SpendClock,
    ) -> Result<Self, RuntimeError> {
        let refuse = |path: &Path, message: String| RuntimeError::SpendLedgerUnavailable {
            path: path.display().to_string(),
            message,
        };
        crate::state_store::ensure_private_dir(&dir, SPEND_DIR_MODE)
            .map_err(|message| refuse(&dir, message))?;
        let date = clock().date_naive();
        prune_ledgers(&dir, date);
        let path = ledger_path(&dir, date);
        let file = open_ledger_file(&path).map_err(|err| refuse(&path, err.to_string()))?;
        Ok(Self {
            ceiling,
            dir,
            session_id: session_id.to_string(),
            clock,
            state: Mutex::new(LedgerState {
                date,
                file,
                offset: 0,
                total: 0,
            }),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, LedgerState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Today's machine total: every complete line appended to today's file, by any process,
    /// including this one's own settled calls.
    ///
    /// A failed read leaves the last total standing and says so on stderr: the ledger was usable at
    /// staging, and a transient read error is not a reason to stop a session the ceiling has not
    /// been shown to cover.
    pub(crate) fn refresh_total(&self) -> u64 {
        let mut state = self.lock();
        if let Err(err) = self.roll_to_today(&mut state) {
            eprintln!(
                "[capsule-runtime] warning: spend ledger in {} could not be opened for today: {err}",
                self.dir.display()
            );
            return state.total;
        }
        if let Err(err) = read_new_lines(&mut state) {
            eprintln!(
                "[capsule-runtime] warning: spend ledger {} could not be read: {err}",
                ledger_path(&self.dir, state.date).display()
            );
        }
        state.total
    }

    /// Append one settled call as a single `write_all` of the complete line.
    fn append(&self, input_tokens: u64, output_tokens: u64) {
        let line = LedgerLine {
            ts: u64::try_from(Utc::now().timestamp_millis()).unwrap_or_default(),
            session_id: std::borrow::Cow::Borrowed(&self.session_id),
            input_tokens,
            output_tokens,
        };
        let Ok(mut bytes) = serde_json::to_vec(&line) else {
            return;
        };
        bytes.push(b'\n');
        let mut state = self.lock();
        let written = self
            .roll_to_today(&mut state)
            .and_then(|()| (&state.file).write_all(&bytes));
        if let Err(err) = written {
            eprintln!(
                "[capsule-runtime] warning: spend ledger in {} could not be appended to: {err}",
                self.dir.display()
            );
        }
    }

    /// Switch to today's file when the UTC date has changed since the last read, starting its
    /// total from zero.
    fn roll_to_today(&self, state: &mut LedgerState) -> std::io::Result<()> {
        let today = (self.clock)().date_naive();
        if today != state.date {
            state.file = open_ledger_file(&ledger_path(&self.dir, today))?;
            state.date = today;
            state.offset = 0;
            state.total = 0;
        }
        Ok(())
    }
}

fn ledger_path(dir: &Path, date: NaiveDate) -> PathBuf {
    dir.join(format!("{}.jsonl", date.format("%Y-%m-%d")))
}

fn open_ledger_file(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let file = OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .mode(SPEND_LEDGER_FILE_MODE)
        .open(path)?;
    // Held, not only set at creation: a file an earlier umask or restore left wider is narrowed.
    file.set_permissions(std::fs::Permissions::from_mode(SPEND_LEDGER_FILE_MODE))?;
    Ok(file)
}

/// Count every complete line after `state.offset`, advancing the offset past complete lines only,
/// so a line still being written is read again, whole, next time.
fn read_new_lines(state: &mut LedgerState) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;

    let mut appended = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let position = state.offset + appended.len() as u64;
        let read = state.file.read_at(&mut chunk, position)?;
        if read == 0 {
            break;
        }
        appended.extend_from_slice(&chunk[..read]);
    }
    let Some(last_newline) = appended.iter().rposition(|byte| *byte == b'\n') else {
        return Ok(());
    };
    let complete = &appended[..=last_newline];
    for line in complete.split(|byte| *byte == b'\n') {
        // A complete line that is not a ledger record adds nothing.
        if let Ok(record) = serde_json::from_slice::<LedgerLine<'_>>(line) {
            state.total = state
                .total
                .saturating_add(record.input_tokens.saturating_add(record.output_tokens));
        }
    }
    state.offset += complete.len() as u64;
    Ok(())
}

/// Remove `<YYYY-MM-DD>.jsonl` files dated more than [`SPEND_LEDGER_RETENTION_DAYS`] before
/// `today`. Every other name is left alone, and a file that cannot be removed is left too.
fn prune_ledgers(dir: &Path, today: NaiveDate) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(date) = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(|stem| NaiveDate::parse_from_str(stem, "%Y-%m-%d").ok())
        else {
            continue;
        };
        if (today - date).num_days() > SPEND_LEDGER_RETENTION_DAYS {
            let _ = std::fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::Read,
        net::TcpListener,
        os::unix::fs::PermissionsExt,
        sync::atomic::{AtomicI64, Ordering},
        time::Duration,
    };

    use chrono::TimeZone;
    use http_body_util::{BodyExt, Empty};
    use tempfile::TempDir;
    use wasmtime_wasi_http::p2::{
        bindings::http::types::ErrorCode, types::OutgoingRequestConfig, WasiHttpHooks,
    };

    use super::*;
    use crate::{inference_gateway::InferenceGateway, runtime::NetworkPolicyHooks};

    fn fixed_clock() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 12, 12, 0, 0).unwrap()
    }

    fn session_meter(ceiling: u64) -> Arc<SpendMeter> {
        Arc::new(SpendMeter::new(Some(ceiling), None))
    }

    fn ledger(dir: &Path, session_id: &str, ceiling: u64, clock: SpendClock) -> MachineLedger {
        MachineLedger::open_in(dir.to_path_buf(), session_id, ceiling, clock).unwrap()
    }

    // ── admission ──────────────────────────────────────────────────────────────

    #[test]
    fn admission_settle_charges_input_and_output() {
        let meter = session_meter(1_000);
        meter.admit(100, 500).unwrap().settle(100, 40);
        assert_eq!(meter.used(), 140);
        assert!(!meter.has_open_admission());
    }

    #[test]
    fn admission_drop_charges_input_only() {
        let meter = session_meter(1_000);
        let admission = meter.admit(100, 500).unwrap();
        assert!(meter.has_open_admission());
        drop(admission);
        assert_eq!(meter.used(), 100);
        assert!(!meter.has_open_admission());
    }

    #[test]
    fn admission_overlapping_calls_are_checked_against_used_plus_reserved() {
        let meter = session_meter(1_000);
        meter.admit(100, 100).unwrap().settle(100, 100);
        let first = meter.admit(200, 200).unwrap();
        // 200 settled + 400 reserved + 401 requested > 1000.
        let refusal = meter.admit(201, 200).unwrap_err();
        assert_eq!(
            refusal,
            SpendRefusal {
                limit: SpendLimit::Session,
                ceiling: 1_000,
                used: 600,
                requested: 401,
            }
        );
        drop(first);
    }

    #[test]
    fn admission_session_refusal_latches() {
        let meter = session_meter(1_000);
        let refusal = meter.admit(900, 200).unwrap_err();
        assert_eq!(refusal.used, 0);
        // A call that would fit on its own is still refused, naming what it asked for.
        let again = meter.admit(1, 1).unwrap_err();
        assert_eq!(again.limit, SpendLimit::Session);
        assert_eq!(again.requested, 2);
        assert!(!meter.has_open_admission());
    }

    #[test]
    fn admission_without_ceilings_always_succeeds_and_accumulates() {
        let meter = Arc::new(SpendMeter::unlimited());
        for _ in 0..3 {
            meter
                .admit(u64::MAX / 2, u64::MAX / 2)
                .unwrap()
                .settle(1_000, 1_000);
        }
        assert_eq!(meter.used(), 6_000);
    }

    #[test]
    fn admission_refusal_display_is_one_line_per_limit() {
        let session = SpendRefusal {
            limit: SpendLimit::Session,
            ceiling: 200_000,
            used: 195_400,
            requested: 12_100,
        };
        assert_eq!(
            session.to_string(),
            "spend ceiling reached: inference.max_session_tokens is 200000 and this session has \
             used 195400; the next call could use up to 12100 more. The operator set this limit; \
             retrying will not get past it."
        );
        let machine = SpendRefusal {
            limit: SpendLimit::Machine,
            ..session
        };
        assert_eq!(
            machine.to_string(),
            "spend ceiling reached: spend.machine_tokens_per_day is 200000 and capsules on this \
             machine have used 195400 today (UTC); the next call could use up to 12100 more. The \
             operator set this limit; retrying will not get past it before it resets at 00:00 UTC."
        );
        assert_eq!(
            serde_json::to_value(&machine).unwrap()["limit"],
            serde_json::json!("machine")
        );
    }

    // ── the gateway rule ───────────────────────────────────────────────────────

    #[test]
    fn gateway_refuses_without_open_admission() {
        let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
        upstream.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}", upstream.local_addr().unwrap());

        let meter = Arc::new(SpendMeter::unlimited());
        let gateway = Arc::new(
            InferenceGateway::new(
                "the-driver",
                &endpoint,
                murmur_artifact::InferenceAuth {
                    header: "x-api-key".to_string(),
                    value: "{key}".to_string(),
                },
                Some(Arc::new(
                    crate::inference_credential::InferenceCredential::resolve(
                        &murmur_artifact::ApiKeyReference::Literal(
                            "sk-spend-gateway-marker".to_string(),
                        ),
                        None,
                    )
                    .unwrap(),
                )),
                Arc::clone(&meter),
            )
            .unwrap(),
        );
        let mut hooks = NetworkPolicyHooks {
            network_allow_rules: Vec::new(),
            inference_gateway: Some(gateway),
        };
        let rt = tokio::runtime::Runtime::new().unwrap();

        // Sends one gateway-addressed request, keeping the in-flight response alive long enough
        // for its connection to land, and reports whether it was denied and whether the upstream
        // accepted a connection.
        let attempt = |hooks: &mut NetworkPolicyHooks| -> (bool, bool) {
            let request = hyper::Request::builder()
                .method("POST")
                .uri("http://127.0.0.1:9/v1/messages")
                .body(
                    Empty::<bytes::Bytes>::new()
                        .map_err(|err| match err {})
                        .boxed_unsync(),
                )
                .unwrap();
            let config = OutgoingRequestConfig {
                use_tls: false,
                connect_timeout: Duration::from_secs(5),
                first_byte_timeout: Duration::from_secs(5),
                between_bytes_timeout: Duration::from_secs(5),
            };
            rt.block_on(async {
                let sent = hooks.send_request(request, config);
                let denied = match &sent {
                    Err(err) => {
                        assert!(matches!(
                            err.downcast_ref(),
                            Some(ErrorCode::HttpRequestDenied)
                        ));
                        assert!(!format!("{err:?}").contains("sk-spend-gateway-marker"));
                        true
                    }
                    Ok(_) => false,
                };
                let mut connected = false;
                for _ in 0..50 {
                    if let Ok((mut stream, _)) = upstream.accept() {
                        let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
                        let _ = stream.read(&mut [0u8; 64]);
                        connected = true;
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                drop(sent);
                (denied, connected)
            })
        };

        assert_eq!(attempt(&mut hooks), (true, false), "no admission open");

        let admission = meter.admit(10, 10).unwrap();
        assert_eq!(attempt(&mut hooks), (false, true), "an admission is open");
        admission.settle(10, 5);
        assert_eq!(attempt(&mut hooks), (true, false), "after settle");

        drop(meter.admit(10, 10).unwrap());
        assert_eq!(attempt(&mut hooks), (true, false), "after drop");
    }

    // ── the machine ledger ─────────────────────────────────────────────────────

    #[test]
    fn machine_ledger_overshoot_is_bounded_by_in_flight_calls() {
        let dir = TempDir::new().unwrap();
        let one = Arc::new(SpendMeter::new(
            None,
            Some(ledger(dir.path(), "ses_one", 100, fixed_clock)),
        ));
        let two = Arc::new(SpendMeter::new(
            None,
            Some(ledger(dir.path(), "ses_two", 100, fixed_clock)),
        ));

        // Each fits alone; together they do not, and neither process can see the other's.
        let first = one.admit(50, 10).unwrap();
        let second = two.admit(50, 10).unwrap();
        first.settle(50, 10);
        second.settle(50, 10);

        let refusal = one.admit(1, 0).unwrap_err();
        assert_eq!(refusal.limit, SpendLimit::Machine);
        assert_eq!(refusal.used, 120);
        assert_eq!(two.admit(1, 0).unwrap_err().limit, SpendLimit::Machine);
        // Not latched: re-asked, and still refused on the same total.
        assert_eq!(one.admit(1, 0).unwrap_err().used, 120);

        let overshoot = refusal.used - refusal.ceiling;
        assert!(
            overshoot <= 60,
            "overshoot {overshoot} exceeds the second reservation"
        );

        let dir_mode = fs_mode(dir.path());
        let file = dir.path().join("2026-09-12.jsonl");
        assert_eq!(dir_mode, 0o700);
        assert_eq!(fs_mode(&file), 0o600);
        let contents = std::fs::read_to_string(&file).unwrap();
        assert_eq!(contents.lines().count(), 2);
        for line in contents.lines() {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            let keys: Vec<&String> = value.as_object().unwrap().keys().collect();
            assert_eq!(
                keys,
                vec!["input_tokens", "output_tokens", "session_id", "ts"]
            );
        }
    }

    fn fs_mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn ledger_torn_trailing_line_is_counted_once_after_completion() {
        let dir = TempDir::new().unwrap();
        let reader = ledger(dir.path(), "ses_reader", 1_000_000, fixed_clock);
        let path = dir.path().join("2026-09-12.jsonl");
        let line = r#"{"ts":1,"session_id":"ses_other","input_tokens":30,"output_tokens":12}"#;
        let (head, tail) = line.split_at(20);

        let mut writer = OpenOptions::new().append(true).open(&path).unwrap();
        writer.write_all(head.as_bytes()).unwrap();
        assert_eq!(reader.refresh_total(), 0);
        writer.write_all(format!("{tail}\n").as_bytes()).unwrap();
        assert_eq!(reader.refresh_total(), 42);
        assert_eq!(reader.refresh_total(), 42);
    }

    #[test]
    fn ledger_malformed_complete_line_is_skipped() {
        let dir = TempDir::new().unwrap();
        let reader = ledger(dir.path(), "ses_reader", 1_000_000, fixed_clock);
        let path = dir.path().join("2026-09-12.jsonl");
        std::fs::write(
            &path,
            "not json\n{\"ts\":1}\n{\"ts\":1,\"session_id\":\"s\",\"input_tokens\":5,\"output_tokens\":6}\n",
        )
        .unwrap();
        assert_eq!(reader.refresh_total(), 11);
    }

    static ROLLOVER_NOW: AtomicI64 = AtomicI64::new(0);

    fn rollover_clock() -> DateTime<Utc> {
        Utc.timestamp_opt(ROLLOVER_NOW.load(Ordering::SeqCst), 0)
            .unwrap()
    }

    #[test]
    fn ledger_day_rollover_switches_files_and_resets_the_total() {
        let dir = TempDir::new().unwrap();
        let before_midnight = Utc.with_ymd_and_hms(2026, 9, 12, 23, 59, 59).unwrap();
        ROLLOVER_NOW.store(before_midnight.timestamp(), Ordering::SeqCst);

        let meter = Arc::new(SpendMeter::new(
            None,
            Some(ledger(dir.path(), "ses_roll", 100, rollover_clock)),
        ));
        meter.admit(40, 30).unwrap().settle(40, 30);
        assert_eq!(meter.admit(40, 0).unwrap_err().used, 70);

        ROLLOVER_NOW.store(before_midnight.timestamp() + 2, Ordering::SeqCst);
        meter.admit(40, 0).unwrap().settle(40, 1);
        assert!(dir.path().join("2026-09-12.jsonl").is_file());
        let today = std::fs::read_to_string(dir.path().join("2026-09-13.jsonl")).unwrap();
        assert_eq!(today.lines().count(), 1);
        assert_eq!(
            meter.machine.as_ref().unwrap().refresh_total(),
            41,
            "the new day's total counts only the new day's file"
        );
    }

    #[test]
    fn ledger_pruning_removes_only_old_dated_jsonl_files() {
        let dir = TempDir::new().unwrap();
        let names = [
            "2026-08-13.jsonl", // 30 days before today: kept
            "2026-08-12.jsonl", // 31 days: removed
            "2025-01-01.jsonl", // removed
            "2025-01-01.txt",   // not a ledger
            "notes.jsonl",      // not a dated ledger
            "2025-01-01.jsonl.bak",
        ];
        for name in names {
            std::fs::write(dir.path().join(name), "").unwrap();
        }
        let _ledger = ledger(dir.path(), "ses_prune", 1, fixed_clock);
        let mut left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(
            left,
            vec![
                "2025-01-01.jsonl.bak",
                "2025-01-01.txt",
                "2026-08-13.jsonl",
                "2026-09-12.jsonl",
                "notes.jsonl",
            ]
        );
    }

    #[test]
    fn ledger_unusable_directory_refuses() {
        let dir = TempDir::new().unwrap();
        let blocked = dir.path().join("spend");
        std::fs::write(&blocked, "a file, not a directory").unwrap();
        let err = MachineLedger::open_in(blocked, "ses_x", 1, fixed_clock).unwrap_err();
        assert!(
            matches!(err, RuntimeError::SpendLedgerUnavailable { ref path, .. } if path.ends_with("spend")),
            "{err}"
        );
    }
}
