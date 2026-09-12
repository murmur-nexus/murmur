//! The address vocabulary for the commands that need a capsule to be running right now.
//!
//! A sibling of [`crate::session_address`], not a variant of it. Both spell an address the same
//! way — `@1`, a `ses_` id, a 4-plus-character suffix — and they differ in what they resolve
//! against, which is the whole point: `mur trace`, `mur eval` and `mur run --resume` name a
//! *recorded* session in a workdir, live or dead, while `mur watch` and `mur cancel` name a
//! *running* session on this machine. A command that has to connect never resolves to something
//! it cannot connect to.
//!
//! The candidate set is the machine-wide running record under `~/.murmur/running/`, so whoever
//! types `mur watch @1` does not have to know which directory the capsule was launched from.
//! Reading it prunes every record whose process is gone, and a record pruned by this read is still
//! what explains the address that named it.

use capsule_runtime::{running, Liveness, ProcessState, RunningRecord};

use crate::error::{CliError, E_RUN_022, E_RUN_023};

/// The address an omitted argument means: the most recent running session.
const LATEST: &str = "@1";

/// Length of a full session id: `ses_` and 32 hex characters.
const SESSION_ID_LEN: usize = 36;

/// The shortest suffix that may name a session.
const MIN_SUFFIX_LEN: usize = 4;

/// The capsule a command connects to, and how it was named.
pub(crate) enum Target {
    /// Named by session address, and verified reachable before this value existed.
    Session(Box<RunningRecord>),
    /// Named directly with `--url`, on the caller's word.
    Url(String),
}

impl Target {
    /// The `host:port` to connect to.
    pub(crate) fn url(&self) -> &str {
        match self {
            Target::Session(record) => &record.url,
            Target::Url(url) => url,
        }
    }

    /// What to call this capsule when telling a person which one they are looking at: the session
    /// it turned out to be, or the address they typed when that is all that was given.
    pub(crate) fn label(&self) -> &str {
        match self {
            Target::Session(record) => &record.session_id,
            Target::Url(url) => url,
        }
    }
}

/// The capsule a session address or a `--url` names.
///
/// One of the two, decided by which was given rather than by trying one and then the other: the
/// positional argument is a session address and nothing else, and `--url` conflicts with it at
/// argument parsing, so the two spellings can never both apply.
pub(crate) fn target(session: Option<&str>, url: Option<&str>) -> Result<Target, CliError> {
    match url {
        Some(url) => Ok(Target::Url(url.to_string())),
        None => {
            resolve_live(session.unwrap_or(LATEST)).map(|record| Target::Session(Box::new(record)))
        }
    }
}

/// The running session `address` names, verified reachable.
///
/// Three forms: an `@N` ordinal counting back over running sessions, a full `ses_` id, and a
/// 4-plus-character case-insensitive suffix of one. A literal path is refused — a path names a
/// recorded session directory on disk, which says nothing about whether a process is running.
///
/// Two ways to fail, and they mean different things. `E-RUN-022` is the address naming no running
/// session: the process is gone and its record has been unlinked. `E-RUN-023` is the capsule not
/// answering: the process is alive, so the record stands.
pub(crate) fn resolve_live(address: &str) -> Result<RunningRecord, CliError> {
    let record = resolve_live_process(address)?;
    match running::verify(&record) {
        Liveness::Live => Ok(record),
        Liveness::Unreachable(reason) => Err(CliError::with_hint(
            E_RUN_023,
            format!(
                "{} is running but its capsule did not answer at {}: {reason}",
                record.session_id, record.url
            ),
            "the process holding the door is alive — it may be mid-turn; its record is kept",
        )),
        // Between the candidacy read and here, which is a race rather than a state.
        Liveness::Gone(reason) => {
            running::prune(&record);
            Err(not_running(&record, &reason))
        }
    }
}

/// The running session `address` names, verified only as far as its process.
///
/// The same three forms and the same candidate set as [`resolve_live`], stopping one layer
/// earlier: the pid is alive and is still the process that wrote the record, and the door has not
/// been asked anything. `E-RUN-022` is the only way this fails.
///
/// What `mur stop` resolves with. A stop is three steps, and only the first goes through the
/// door; refusing the whole command because the door is quiet — which is what `E-RUN-023` would
/// do — would abandon a stop that still has two steps left and a process still running.
pub(crate) fn resolve_live_process(address: &str) -> Result<RunningRecord, CliError> {
    candidate(address)
}

/// Every record on this machine, split by what layers 1 and 2 say about the process it names.
struct Candidates {
    /// Records whose process is the one that wrote them, most recent first.
    alive: Vec<RunningRecord>,
    /// Records this read unlinked, with the reason, most recent first. Kept for the length of one
    /// resolution so an address that named one is answered with why rather than with silence.
    gone: Vec<(RunningRecord, String)>,
}

/// Reads the record directory and unlinks what fails layer 1 or 2.
///
/// The only reaper there is, and it is enough because a record is a hint: whatever it missed is
/// removed by the next read. Layer 3 is a network round trip and is not applied here — it is
/// applied once, to the record an address resolved to, which is also what keeps "the door is
/// quiet" reportable as its own thing rather than collapsing into "the process is gone".
fn read_and_prune() -> Candidates {
    let mut alive = Vec::new();
    let mut gone = Vec::new();
    for record in running::list() {
        match running::process_state(&record) {
            ProcessState::Alive => alive.push(record),
            ProcessState::Gone(reason) => {
                running::prune(&record);
                gone.push((record, reason));
            }
        }
    }
    Candidates { alive, gone }
}

/// The record an address names, before the door has been asked anything.
fn candidate(address: &str) -> Result<RunningRecord, CliError> {
    // A path names a recorded session directory and a `host:port` names a port; neither says
    // anything about what is running, so neither is a spelling of a session address.
    if address.contains('/') || address.contains(':') || address.ends_with(".jsonl") {
        return Err(CliError::with_hint(
            E_RUN_022,
            format!("'{address}' is not a session address"),
            "name a running session with @1, a ses_ id, or a 4-character suffix of one",
        ));
    }

    let Candidates { alive, gone } = read_and_prune();

    if let Some(ordinal) = address.strip_prefix('@') {
        let n: usize = ordinal.parse().map_err(|_| {
            CliError::new(
                E_RUN_022,
                format!("invalid ordinal '{address}' — expected @1, @2, ..."),
            )
        })?;
        if n == 0 {
            return Err(CliError::new(E_RUN_022, "ordinal must be @1 or higher"));
        }
        if let Some(record) = alive.get(n - 1) {
            return Ok(record.clone());
        }
        // Ordinals count over running sessions, so a stale record never shifts the numbering.
        // When no running session can satisfy the ordinal, the same ordinal over what this read
        // just unlinked is what explains why.
        if let Some((record, reason)) = gone.get(n - 1) {
            return Err(not_running(record, reason));
        }
        return Err(nothing_matches(&alive, format!("@{n}")));
    }

    if address.starts_with("ses_") && address.len() == SESSION_ID_LEN {
        if let Some(record) = alive.iter().find(|record| record.session_id == address) {
            return Ok(record.clone());
        }
        if let Some((record, reason)) = gone.iter().find(|(record, _)| record.session_id == address)
        {
            return Err(not_running(record, reason));
        }
        return Err(nothing_matches(&alive, address.to_string()));
    }

    if address.len() < MIN_SUFFIX_LEN {
        return Err(CliError::with_hint(
            E_RUN_022,
            format!("'{address}' is too short to name a session"),
            "give at least 4 characters of a session id, a full ses_ id, or an ordinal such as @1",
        ));
    }

    let suffix = address.to_lowercase();
    let matches: Vec<&RunningRecord> = alive
        .iter()
        .filter(|record| record.session_id.to_lowercase().ends_with(&suffix))
        .collect();
    match matches.len() {
        1 => Ok(matches[0].clone()),
        0 => {
            if let Some((record, reason)) = gone
                .iter()
                .find(|(record, _)| record.session_id.to_lowercase().ends_with(&suffix))
            {
                return Err(not_running(record, reason));
            }
            Err(nothing_matches(&alive, address.to_string()))
        }
        count => Err(CliError::new(
            E_RUN_022,
            format!(
                "'{address}' matches {count} running sessions — provide more characters\n{}",
                matches
                    .iter()
                    .map(|record| format!("  {}", record.session_id))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
        )),
    }
}

/// The address named a session whose process is gone. Its record has just been unlinked.
fn not_running(record: &RunningRecord, reason: &str) -> CliError {
    CliError::with_hint(
        E_RUN_022,
        format!("{} is not running: {reason}", record.session_id),
        "its record has been removed; start the capsule again to make the address resolve",
    )
}

/// The address matched nothing that is running.
fn nothing_matches(alive: &[RunningRecord], address: String) -> CliError {
    if alive.is_empty() {
        return CliError::with_hint(
            E_RUN_022,
            "no capsule is running on this machine",
            "start a capsule with `mur run`; it is addressable for as long as it serves",
        );
    }
    CliError::new(
        E_RUN_022,
        format!(
            "{address} names no running session — {} running: {}",
            alive.len(),
            alive
                .iter()
                .map(|record| record.session_id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    )
}
