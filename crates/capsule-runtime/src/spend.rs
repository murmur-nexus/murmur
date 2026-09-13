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
//! writes its settled calls to. There is no lock and no daemon: each process reads only the
//! complete lines appended since its last read, so a call admitted and not yet settled is invisible
//! to every other admission. The machine total can therefore exceed the ceiling by at most the
//! measured tokens of the calls in flight — admitted and not yet settled — when the last call was
//! admitted. The last call is among them, so an output that measures more than its reservation
//! still counts in full.
//!
//! That bound holds only while the ledger can be read and appended to. After staging, a failed
//! append or read warns on stderr and the run carries on: a call whose line was never written is
//! never counted, and the overshoot is unbounded for as long as the failure lasts.

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
        // Appended before the reservation is released, so an admission that reads the ledger after
        // the append sees this call, and one that takes the meter lock before the release sees it
        // reserved; one that does both counts it twice. `admit` reads the ledger before taking the
        // lock, so an admission whose read ran before the append and whose lock is taken after the
        // release counts this call zero times. The call was then in flight at that read, which is
        // what the machine ceiling's overshoot is bounded by.
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
        let home = crate::murmur_home::ensure_murmur_home().map_err(|message| {
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
        process::{Child, Command, Stdio},
        sync::{
            atomic::{AtomicI64, Ordering},
            Barrier,
        },
        time::{Duration, Instant},
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

    // ── the machine ledger under concurrent admissions ─────────────────────────

    /// Replays a race's call sizes and jitter when set to the seed a run printed.
    const RACE_SEED_ENV: &str = "MURMUR_SPEND_RACE_SEED";
    const RACE_MAX_INPUT: u64 = 120;
    const RACE_MAX_OUTPUT: u64 = 80;
    /// The most one race call can settle: output never exceeds what the call reserved.
    const RACE_MAX_CALL: u64 = RACE_MAX_INPUT + RACE_MAX_OUTPUT;
    const RACE_CEILING: u64 = 30 * RACE_MAX_CALL;
    /// Today's ledger under `fixed_clock`.
    const RACE_LEDGER: &str = "2026-09-12.jsonl";

    /// xorshift64, so every call size and jitter in a race follows from one printed seed.
    struct XorShift(u64);

    impl XorShift {
        fn new(seed: u64) -> Self {
            // One splitmix64 step, so nearby seeds diverge at once and the state is never zero.
            let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            Self((z ^ (z >> 31)) | 1)
        }

        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        /// A value in `low..=high`.
        fn between(&mut self, low: u64, high: u64) -> u64 {
            low + self.next() % (high - low + 1)
        }
    }

    /// [`RACE_SEED_ENV`] when set, the clock otherwise. Printed before the race starts, so a failing
    /// run's captured output names it.
    fn race_seed(test: &str) -> u64 {
        let seed = std::env::var(RACE_SEED_ENV)
            .ok()
            .and_then(|seed| seed.parse().ok())
            .unwrap_or_else(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos() as u64
            });
        println!("{test}: seed {seed} (replay with {RACE_SEED_ENV}={seed})");
        seed
    }

    #[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
    struct Settled {
        calls: u64,
        tokens: u64,
    }

    impl std::ops::Add for Settled {
        type Output = Self;

        fn add(self, other: Self) -> Self {
            Self {
                calls: self.calls + other.calls,
                tokens: self.tokens + other.tokens,
            }
        }
    }

    /// Admit, sleep 0–2 ms, and settle with no more output than was reserved, until the machine
    /// ceiling refuses.
    fn race_until_refused(meter: &Arc<SpendMeter>, rng: &mut XorShift) -> Settled {
        let mut settled = Settled::default();
        loop {
            let input = rng.between(1, RACE_MAX_INPUT);
            let max_output = rng.between(1, RACE_MAX_OUTPUT);
            let admission = match meter.admit(input, max_output) {
                Ok(admission) => admission,
                Err(refusal) => {
                    assert_eq!(refusal.limit, SpendLimit::Machine, "{refusal}");
                    return settled;
                }
            };
            std::thread::sleep(Duration::from_micros(rng.between(0, 2_000)));
            let output = rng.between(0, max_output);
            admission.settle(input, output);
            settled = settled
                + Settled {
                    calls: 1,
                    tokens: input + output,
                };
        }
    }

    /// Assert the race ledger in `dir` holds exactly `settled`: every line a whole ledger record,
    /// one line per settled call, and a reader opened now totalling their tokens.
    fn assert_ledger_holds(dir: &Path, settled: Settled, context: &str) {
        let contents = std::fs::read_to_string(dir.join(RACE_LEDGER)).unwrap();
        assert!(
            contents.is_empty() || contents.ends_with('\n'),
            "{context}: the ledger ends in a torn line"
        );
        let mut lines = Settled::default();
        for line in contents.lines() {
            let record: LedgerLine<'_> = serde_json::from_str(line)
                .unwrap_or_else(|err| panic!("{context}: {line:?} is not a ledger record: {err}"));
            lines = lines
                + Settled {
                    calls: 1,
                    tokens: record.input_tokens + record.output_tokens,
                };
        }
        assert_eq!(lines, settled, "{context}: ledger lines against settles");
        assert_eq!(
            ledger(dir, "ses_fresh", RACE_CEILING, fixed_clock).refresh_total(),
            settled.tokens,
            "{context}: a freshly opened ledger's total"
        );
    }

    /// `meter_count` meters with their own ledger handles on one directory, `threads_per_meter`
    /// threads racing on each, over `rounds` fresh directories.
    fn race_meters(test: &str, meter_count: usize, threads_per_meter: usize, rounds: u64) {
        let seed = race_seed(test);
        let threads = meter_count * threads_per_meter;
        let bound = threads as u64 * RACE_MAX_CALL;
        let mut max_overshoot = 0;
        for round in 0..rounds {
            let context = format!("{test} seed {seed} round {round}");
            let dir = TempDir::new().unwrap();
            let meters: Vec<Arc<SpendMeter>> = (0..meter_count)
                .map(|index| {
                    let session_id = format!("ses_race_{index}");
                    Arc::new(SpendMeter::new(
                        None,
                        Some(ledger(dir.path(), &session_id, RACE_CEILING, fixed_clock)),
                    ))
                })
                .collect();
            let start = Barrier::new(threads);
            let settled = std::thread::scope(|scope| {
                let workers: Vec<_> = (0..threads)
                    .map(|thread| {
                        let meter = &meters[thread % meter_count];
                        let start = &start;
                        let mut rng = XorShift::new(seed ^ (round << 32) ^ thread as u64);
                        scope.spawn(move || {
                            start.wait();
                            race_until_refused(meter, &mut rng)
                        })
                    })
                    .collect();
                workers
                    .into_iter()
                    .map(|worker| worker.join().unwrap())
                    .fold(Settled::default(), std::ops::Add::add)
            });

            assert_ledger_holds(dir.path(), settled, &context);
            for (index, meter) in meters.iter().enumerate() {
                assert_eq!(
                    meter.machine.as_ref().unwrap().refresh_total(),
                    settled.tokens,
                    "{context}: meter {index}'s own ledger total"
                );
            }
            let overshoot = settled.tokens.saturating_sub(RACE_CEILING);
            assert!(
                overshoot <= bound,
                "{context}: total {} exceeds ceiling {RACE_CEILING} by {overshoot}, past the \
                 bound {bound}",
                settled.tokens
            );
            max_overshoot = max_overshoot.max(overshoot);
        }
        println!(
            "{test}: {threads} threads on {meter_count} meters, ceiling {RACE_CEILING}, max call \
             tokens {RACE_MAX_CALL}: max observed overshoot {max_overshoot} of bound {bound} over \
             {rounds} rounds"
        );
    }

    #[test]
    fn machine_ledger_concurrent_threads() {
        race_meters("machine_ledger_concurrent_threads", 8, 1, 50);
    }

    #[test]
    fn machine_ledger_concurrent_shared_meter() {
        // Several threads per meter, so one thread's settle can land between another's ledger
        // read and its meter lock.
        race_meters("machine_ledger_concurrent_shared_meter", 3, 4, 50);
    }

    #[test]
    fn machine_ledger_concurrent_worst_case() {
        const WORKERS: u64 = 8;
        const INPUT: u64 = 60;
        const MAX_OUTPUT: u64 = 40;
        const REQUESTED: u64 = INPUT + MAX_OUTPUT;
        // One call fits under it; a second on top of the first does not.
        const CEILING: u64 = REQUESTED + REQUESTED / 2;

        let dir = TempDir::new().unwrap();
        let meters: Vec<Arc<SpendMeter>> = (0..WORKERS)
            .map(|index| {
                let session_id = format!("ses_worst_{index}");
                Arc::new(SpendMeter::new(
                    None,
                    Some(ledger(dir.path(), &session_id, CEILING, fixed_clock)),
                ))
            })
            .collect();
        // Between admission and settling, so every call is admitted before any is settled.
        let all_admitted = Barrier::new(meters.len());
        let admitted: Vec<bool> = std::thread::scope(|scope| {
            let workers: Vec<_> = meters
                .iter()
                .map(|meter| {
                    let all_admitted = &all_admitted;
                    scope.spawn(move || {
                        let admission = meter.admit(INPUT, MAX_OUTPUT);
                        all_admitted.wait();
                        admission.map(|admission| admission.settle(INPUT, MAX_OUTPUT))
                    })
                })
                .collect();
            workers
                .into_iter()
                .map(|worker| worker.join().unwrap().is_ok())
                .collect()
        });

        assert!(admitted.iter().all(|ok| *ok), "{admitted:?}");
        let total = ledger(dir.path(), "ses_fresh", CEILING, fixed_clock).refresh_total();
        assert_eq!(total, WORKERS * REQUESTED);
        let bound = WORKERS * REQUESTED;
        let overshoot = total - CEILING;
        assert!(overshoot <= bound, "overshoot {overshoot} of bound {bound}");
        for meter in &meters {
            let refusal = meter.admit(1, 0).unwrap_err();
            assert_eq!(refusal.limit, SpendLimit::Machine);
            assert_eq!(refusal.used, total);
        }
        println!(
            "machine_ledger_concurrent_worst_case: {WORKERS} meters, ceiling {CEILING}, max call \
             tokens {REQUESTED}: max observed overshoot {overshoot} of bound {bound} over 1 rounds"
        );
    }

    const CHILD_ROLE_ENV: &str = "MURMUR_SPEND_CHILD_ROLE";
    const CHILD_DIR_ENV: &str = "MURMUR_SPEND_CHILD_DIR";
    const CHILD_SESSION_ENV: &str = "MURMUR_SPEND_CHILD_SESSION";
    const CHILD_CEILING_ENV: &str = "MURMUR_SPEND_CHILD_CEILING";
    const CHILD_SEED_ENV: &str = "MURMUR_SPEND_CHILD_SEED";
    /// A directory of marker files: `start` once every child is spawned, `settled-<session id>`
    /// once a writer has been refused, `all-settled` once every writer has, and `done` once every
    /// writer has exited.
    const CHILD_MARKERS_ENV: &str = "MURMUR_SPEND_CHILD_MARKERS";
    const CHILD_REPORT: &str = "SPEND_CHILD_REPORT";
    const CHILD_TIMEOUT: Duration = Duration::from_secs(120);

    fn wait_for_marker(path: &Path) {
        let deadline = Instant::now() + CHILD_TIMEOUT;
        while !path.exists() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {}",
                path.display()
            );
            std::thread::sleep(Duration::from_micros(100));
        }
    }

    /// One process of `machine_ledger_concurrent_processes`, selected by [`CHILD_ROLE_ENV`]. A
    /// writer races [`race_until_refused`] on its own ledger handle; a reader refreshes its total
    /// without pause until `done`.
    #[test]
    #[ignore = "run as a child process by machine_ledger_concurrent_processes"]
    fn machine_ledger_child_worker() {
        let Ok(role) = std::env::var(CHILD_ROLE_ENV) else {
            return;
        };
        let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} is unset"));
        let dir = PathBuf::from(var(CHILD_DIR_ENV));
        let markers = PathBuf::from(var(CHILD_MARKERS_ENV));
        let session_id = var(CHILD_SESSION_ENV);
        let ceiling: u64 = var(CHILD_CEILING_ENV).parse().unwrap();
        let seed: u64 = var(CHILD_SEED_ENV).parse().unwrap();
        let handle = ledger(&dir, &session_id, ceiling, fixed_clock);
        wait_for_marker(&markers.join("start"));

        let report = match role.as_str() {
            "writer" => {
                let meter = Arc::new(SpendMeter::new(None, Some(handle)));
                let settled = race_until_refused(&meter, &mut XorShift::new(seed));
                std::fs::write(markers.join(format!("settled-{session_id}")), "").unwrap();
                wait_for_marker(&markers.join("all-settled"));
                serde_json::json!({
                    "role": "writer",
                    "settled_calls": settled.calls,
                    "settled_tokens": settled.tokens,
                    "final_total": meter.machine.as_ref().unwrap().refresh_total(),
                })
            }
            "reader" => {
                let done = markers.join("done");
                let mut last = 0;
                while !done.exists() {
                    let total = handle.refresh_total();
                    assert!(total >= last, "the total went from {last} to {total}");
                    last = total;
                }
                serde_json::json!({"role": "reader", "final_total": handle.refresh_total()})
            }
            other => panic!("unknown {CHILD_ROLE_ENV} {other:?}"),
        };
        println!("{CHILD_REPORT} {report}");
    }

    /// Children killed when dropped, so a failed assertion does not leave them running.
    struct Children(Vec<(String, Child)>);

    impl Drop for Children {
        fn drop(&mut self) {
            for (_, child) in &mut self.0 {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    fn drain(child: &mut Child) -> (String, String) {
        let mut stdout = String::new();
        let mut stderr = String::new();
        if let Some(mut pipe) = child.stdout.take() {
            let _ = pipe.read_to_string(&mut stdout);
        }
        if let Some(mut pipe) = child.stderr.take() {
            let _ = pipe.read_to_string(&mut stderr);
        }
        (stdout, stderr)
    }

    /// Wait for `child` to exit by `deadline`, assert it ran the worker test and passed, and return
    /// its one report.
    fn child_report(
        context: &str,
        label: &str,
        child: &mut Child,
        deadline: Instant,
    ) -> serde_json::Value {
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "{context}: {label} did not exit within {CHILD_TIMEOUT:?}"
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        let (stdout, stderr) = drain(child);
        let output = format!("--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}");
        assert!(status.success(), "{context}: {label} failed\n{output}");
        assert!(
            stdout.contains("1 passed"),
            "{context}: {label} did not run the worker\n{output}"
        );
        let reports: Vec<&str> = stdout
            .lines()
            .filter_map(|line| line.split_once(&format!("{CHILD_REPORT} ")))
            .map(|(_, report)| report)
            .collect();
        assert_eq!(reports.len(), 1, "{context}: {label}\n{output}");
        serde_json::from_str(reports[0]).unwrap()
    }

    #[test]
    fn machine_ledger_concurrent_processes() {
        const WRITERS: usize = 8;
        const READERS: usize = 2;
        const ROUNDS: u64 = 5;
        let test = "machine_ledger_concurrent_processes";
        let seed = race_seed(test);
        let bound = WRITERS as u64 * RACE_MAX_CALL;
        let exe = std::env::current_exe().expect("test binary path");
        let mut max_overshoot = 0;

        for round in 0..ROUNDS {
            let context = format!("{test} seed {seed} round {round}");
            let dir = TempDir::new().unwrap();
            let markers = TempDir::new().unwrap();
            let spawn = |role: &str, session_id: String, child_seed: u64| {
                let child = Command::new(&exe)
                    .args([
                        "--exact",
                        "spend::tests::machine_ledger_child_worker",
                        "--ignored",
                        "--test-threads=1",
                        "--nocapture",
                    ])
                    .env(CHILD_ROLE_ENV, role)
                    .env(CHILD_DIR_ENV, dir.path())
                    .env(CHILD_SESSION_ENV, &session_id)
                    .env(CHILD_CEILING_ENV, RACE_CEILING.to_string())
                    .env(CHILD_SEED_ENV, child_seed.to_string())
                    .env(CHILD_MARKERS_ENV, markers.path())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .expect("re-exec the test binary for a ledger child");
                (session_id, child)
            };
            let mut children = Children(Vec::new());
            for writer in 0..WRITERS {
                let child_seed = seed ^ (round << 32) ^ writer as u64;
                children
                    .0
                    .push(spawn("writer", format!("ses_writer_{writer}"), child_seed));
            }
            for reader in 0..READERS {
                children
                    .0
                    .push(spawn("reader", format!("ses_reader_{reader}"), 0));
            }
            std::fs::write(markers.path().join("start"), "").unwrap();

            let deadline = Instant::now() + CHILD_TIMEOUT;
            loop {
                let all_settled = children.0[..WRITERS].iter().all(|(session_id, _)| {
                    markers
                        .path()
                        .join(format!("settled-{session_id}"))
                        .exists()
                });
                if all_settled {
                    break;
                }
                // No child exits before `all-settled`, so one that has is a failure to report now
                // rather than at the deadline.
                for (label, child) in &mut children.0 {
                    if let Some(status) = child.try_wait().unwrap() {
                        let (stdout, stderr) = drain(child);
                        panic!(
                            "{context}: {label} exited early with {status}\n--- stdout ---\n\
                             {stdout}\n--- stderr ---\n{stderr}"
                        );
                    }
                }
                assert!(
                    Instant::now() < deadline,
                    "{context}: writers did not all settle within {CHILD_TIMEOUT:?}"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            std::fs::write(markers.path().join("all-settled"), "").unwrap();

            let mut reports = Vec::new();
            for (label, child) in &mut children.0[..WRITERS] {
                reports.push(child_report(&context, label, child, deadline));
            }
            std::fs::write(markers.path().join("done"), "").unwrap();
            for (label, child) in &mut children.0[WRITERS..] {
                reports.push(child_report(&context, label, child, deadline));
            }

            let (writer_reports, reader_reports) = reports.split_at(WRITERS);
            assert!(writer_reports
                .iter()
                .all(|report| report["role"] == "writer"));
            assert!(reader_reports
                .iter()
                .all(|report| report["role"] == "reader"));
            let settled = writer_reports
                .iter()
                .map(|report| Settled {
                    calls: report["settled_calls"].as_u64().unwrap(),
                    tokens: report["settled_tokens"].as_u64().unwrap(),
                })
                .fold(Settled::default(), std::ops::Add::add);
            assert_ledger_holds(dir.path(), settled, &context);
            for report in &reports {
                assert_eq!(
                    report["final_total"].as_u64(),
                    Some(settled.tokens),
                    "{context}: {report}"
                );
            }
            let overshoot = settled.tokens.saturating_sub(RACE_CEILING);
            assert!(
                overshoot <= bound,
                "{context}: total {} exceeds ceiling {RACE_CEILING} by {overshoot}, past the \
                 bound {bound}",
                settled.tokens
            );
            max_overshoot = max_overshoot.max(overshoot);
        }
        println!(
            "{test}: {WRITERS} writer and {READERS} reader processes, ceiling {RACE_CEILING}, max \
             call tokens {RACE_MAX_CALL}: max observed overshoot {max_overshoot} of bound {bound} \
             over {ROUNDS} rounds"
        );
    }

    // ── the machine ledger ─────────────────────────────────────────────────────

    #[test]
    fn machine_ledger_two_unseen_admissions_both_pass() {
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

#[cfg(test)]
mod home_tests {
    use super::*;

    #[test]
    fn opening_the_ledger_holds_a_wide_murmur_home_owner_only() {
        let home = tempfile::tempdir().unwrap();
        crate::murmur_home::wide_dir(&home.path().join(".murmur"), 0o755);
        crate::murmur_home::run_with_home(
            "spend::home_tests::inner_open_ledger_under_scratch_home",
            home.path(),
        );
        assert_eq!(
            crate::murmur_home::mode_of(&home.path().join(".murmur")),
            0o700
        );
        assert_eq!(
            crate::murmur_home::mode_of(&home.path().join(".murmur").join(SPEND_DIR)),
            SPEND_DIR_MODE
        );
    }

    #[test]
    #[ignore = "run by opening_the_ledger_holds_a_wide_murmur_home_owner_only"]
    fn inner_open_ledger_under_scratch_home() {
        if !crate::murmur_home::in_scratch_home() {
            return;
        }
        MachineLedger::open("ses_home_test", 1_000).unwrap();
    }
}
