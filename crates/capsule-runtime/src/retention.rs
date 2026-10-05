//! Retention: what bounds the three stores that grow, and what it costs to bound them.
//!
//! Three stores, two operations, because they are different shapes:
//!
//! * A **session directory** — `<workdir>/.murmur/<ses_…>/` — is an independent unit. Nothing
//!   references it and nothing spans two of them, so it is pruned by being deleted whole, taking
//!   its `trace.jsonl` and its `blobs/` with it.
//! * A **formation directory** — `~/.murmur/formations/<frm_…>/` — holds one ended formation's
//!   peer directories, and with them every peer's sessions. It is the other half of the evidence
//!   whose first half is the entry member's session in the project, so it is pruned whole, by the
//!   entry member's `trace.retain`, and only by the launcher of a later formation from the same
//!   project once every member of that later one is reaped.
//! * A **conversation record** — `~/.murmur/conversations/<record>/<ctx>/conversation.jsonl` — is
//!   one unit that grows. Deleting it destroys the conversation an operator wanted kept, so it is
//!   pruned by truncating its front: the oldest messages go, the recent ones stay, and every
//!   surviving message keeps the id it has always had.
//!
//! Three properties hold across everything below:
//!
//! * **No defaults.** Every entry point takes the policy it enforces. An absent policy is an
//!   absent call: nothing here has a fallback that deletes.
//! * **No clock of its own.** Every prune function takes `now_ms`, so age is a fixture's to supply
//!   and a test never sleeps.
//! * **No trace of its own.** Every prune function returns what it removed and writes nothing;
//!   the caller holds the [`crate::trace::TraceWriter`] and records the deletion against the
//!   session that performed it, or reports it as the launcher that performed it.
//!
//! Session and formation age is read out of the `ses_` or `frm_` id — a uuid v7 whose first 12
//! hex characters are a millisecond timestamp — so both policies are computed from one directory
//! listing with no `stat`. Record age is the mtime of `conversation.jsonl`, because a context id
//! is not necessarily a uuid and "untouched since" is the useful question for a record that spans
//! weeks.

use std::path::{Path, PathBuf};

use crate::conversation::{
    header_line, message_id_timestamp_ms, parse_message_line, read_header, CONVERSATION_ROOT_DIR,
    RECORD_FILE_NAME,
};

use crate::formation::FormationId;
use crate::formation_launch::FormationMarker;

pub use crate::conversation::{RecordHeader, TruncationMarker};

/// A session directory that was removed, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrunedSession {
    /// The `ses_…` directory name, as it appeared under the sessions root.
    pub name: String,
    /// [`crate::trace::RETENTION_REASON_MAX_SESSIONS`] or
    /// [`crate::trace::RETENTION_REASON_MAX_AGE`].
    pub reason: &'static str,
}

/// A formation directory that was removed, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrunedFormation {
    /// The formation whose directory went.
    pub formation_id: FormationId,
    /// The directory that was removed, whole: `<formations dir>/<frm_id>`.
    pub path: PathBuf,
    /// [`crate::trace::RETENTION_REASON_MAX_AGE`] or
    /// [`crate::trace::RETENTION_REASON_MAX_SESSIONS`].
    pub reason: &'static str,
}

/// A context directory that was removed, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrunedRecord {
    /// The context id, which is the directory name under the record root.
    pub context_id: String,
    /// The directory that was removed, whole.
    pub path: PathBuf,
    /// Always [`crate::trace::RETENTION_REASON_MAX_AGE`]: a record is never removed on a count.
    pub reason: &'static str,
}

/// What one truncation dropped and what it left.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TruncationOutcome {
    /// Messages this truncation removed from the front. `0` means the file was left untouched.
    pub dropped: u64,
    /// Messages left in the record.
    pub kept: u64,
    /// The `msg_` id of the oldest surviving message, empty when nothing survives with an id.
    pub oldest_surviving_id: String,
    /// The `msg_` id of the newest dropped message, empty when nothing dropped carried one.
    pub last_dropped_id: String,
}

/// Where one message id turned up, and in what state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageStatus {
    /// The id is a line in the record. `position` is 1-based, oldest first.
    Present { position: u64, total: u64 },
    /// The id is not a line, the header carries a truncation marker, and the id was minted at or
    /// before the newest dropped message. A reference an artifact stored is dangling because the
    /// record was trimmed — not because the id was never real.
    Truncated {
        dropped: u64,
        oldest_surviving_id: String,
    },
    /// Anything else: no such id, in a record that has dropped nothing that old.
    Unknown,
}

/// One record a message id was looked for in, and what that record had to say about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageLocation {
    pub record: String,
    pub context_id: String,
    pub path: PathBuf,
    pub status: MessageStatus,
}

/// One context's record, as `mur conversation ls` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordSummary {
    /// Record store: the directory under `~/.murmur/conversations/`.
    pub record: String,
    /// Context id: the directory under the record store.
    pub context_id: String,
    pub path: PathBuf,
    /// Message lines. The header line is not one and is never counted.
    pub messages: u64,
    /// Size of `conversation.jsonl` on disk.
    pub bytes: u64,
    /// Last write to `conversation.jsonl`, in milliseconds since the epoch. `None` when the
    /// host's mtime is unreadable or predates the epoch.
    pub last_touched_ms: Option<u64>,
    /// The capsule the header line names, and `None` for a record written before the header
    /// existed. An unowned record is never pruned automatically.
    pub capsule: Option<String>,
    /// What this record has already dropped, or `None` if it has never been truncated.
    pub truncation: Option<TruncationMarker>,
}

/// A context directory `mur conversation rm` removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemovedRecord {
    pub record: String,
    pub context_id: String,
    /// The directory that was removed, whole.
    pub path: PathBuf,
    /// Messages it held.
    pub messages: u64,
}

// ── Session pruning ──────────────────────────────────────────────────────────

/// The millisecond timestamp inside a `ses_` id, or `None` when the name is not one.
///
/// `ses_` ids are uuid v7 in simple form, so their first 12 hex characters are the mint time in
/// milliseconds. This is the whole of the session age policy: no `stat`, no walk, and a lexical
/// sort of the names is a chronological sort of the sessions.
pub fn session_id_timestamp_ms(name: &str) -> Option<u64> {
    uuid_v7_timestamp_ms(name, "ses_")
}

/// The millisecond timestamp in the first 12 hex characters after `prefix`, the mint time of a
/// uuid v7 in simple form. `None` when `id` does not start with `prefix` or they are not hex.
pub(crate) fn uuid_v7_timestamp_ms(id: &str, prefix: &str) -> Option<u64> {
    let hex = id.strip_prefix(prefix)?;
    u64::from_str_radix(hex.get(..12)?, 16).ok()
}

/// Remove the session directories under `sessions_root` that `policy` does not keep.
///
/// `current_session_id` is a hard floor: no directory whose name sorts at or after it is ever a
/// candidate, whatever the policy says. That is what keeps the running session's own trace, and
/// what keeps a sibling capsule launched a moment ago from having its live session deleted —
/// uuid v7 ordering makes "minted after mine" a property of the name, so no lock file is needed.
///
/// Both keys are ANDed: a session survives only if it is inside every limit declared. The count
/// is over every entry present, the current session included, so `max_sessions: 3` leaves three
/// directories and one of them is the running one. Reasons are attributed age-first, because
/// "too old" is the more specific statement about a directory that fails both.
///
/// A directory that cannot be removed is skipped and left out of the returned list: retention
/// never fails a launch, and a trace event must name only what actually went.
pub fn prune_sessions(
    sessions_root: &Path,
    current_session_id: &str,
    policy: &murmur_artifact::TraceRetainConfig,
    now_ms: u64,
) -> Vec<PrunedSession> {
    if policy.max_sessions.is_none() && policy.max_age_secs.is_none() {
        return Vec::new();
    }

    let mut entries: Vec<String> = match std::fs::read_dir(sessions_root) {
        Ok(dir) => dir
            .filter_map(Result::ok)
            .filter(|entry| entry.path().is_dir())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| session_id_timestamp_ms(name).is_some())
            .collect(),
        Err(_) => return Vec::new(),
    };
    // Newest first, which for uuid v7 names is both the lexical and the chronological order.
    entries.sort_unstable_by(|a, b| b.cmp(a));

    let age_floor_ms = policy
        .max_age_secs
        .map(|secs| now_ms.saturating_sub(secs.saturating_mul(1000)));

    let mut pruned = Vec::new();
    for (rank, name) in entries.iter().enumerate() {
        // The floor. `>=` rather than `>` so the current session is excluded by the same rule
        // that excludes everything minted after it.
        if name.as_str() >= current_session_id {
            continue;
        }
        let over_age = age_floor_ms.is_some_and(|floor| {
            session_id_timestamp_ms(name).is_some_and(|minted| minted < floor)
        });
        let over_count = policy.max_sessions.is_some_and(|max| rank >= max as usize);
        if !over_age && !over_count {
            continue;
        }
        let reason = if over_age {
            crate::trace::RETENTION_REASON_MAX_AGE
        } else {
            crate::trace::RETENTION_REASON_MAX_SESSIONS
        };
        if std::fs::remove_dir_all(sessions_root.join(name)).is_ok() {
            pruned.push(PrunedSession {
                name: name.clone(),
                reason,
            });
        }
    }
    pruned
}

// ── Formation pruning ────────────────────────────────────────────────────────

/// Remove the formation directories under `formations_dir` that belong to `project_dir` and that
/// `policy` — the entry member's `trace.retain` — does not keep.
///
/// A directory is this project's when it is a real directory, not a symlink, its name is a
/// formation id, and its [`FormationMarker`] parses and names `project_dir`, both sides
/// canonicalized. Only those are ranked. A directory with no marker or an unreadable one is
/// unowned, and one whose marker names another project is foreign: neither is ranked, and neither
/// is ever removed. Every directory made before markers existed is unowned.
///
/// The policy reads as it does for sessions:
///
/// * `max_age_secs` is measured from the formation id's mint time, with no `stat`.
/// * `max_sessions` counts this project's directories present, newest first, `current` and any
///   minted after it included.
/// * Both keys are ANDed, and reasons are attributed age-first.
///
/// `current` is a hard floor: no directory whose id sorts at or after it is ever removed. A
/// directory `is_live` answers `true` for is skipped and keeps its rank, so a formation still
/// running is never removed and still counts against `max_sessions`.
///
/// A missing `formations_dir` removes nothing. A directory that cannot be removed is left out of
/// the returned list, which is newest first.
pub fn prune_formations(
    formations_dir: &Path,
    current: &FormationId,
    project_dir: &Path,
    policy: &murmur_artifact::TraceRetainConfig,
    now_ms: u64,
    is_live: impl Fn(&FormationId, &FormationMarker) -> bool,
) -> Vec<PrunedFormation> {
    if policy.max_sessions.is_none() && policy.max_age_secs.is_none() {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(formations_dir) else {
        return Vec::new();
    };
    let project_dir = crate::formation_launch::canonical_project_dir(project_dir);
    let mut owned: Vec<(FormationId, FormationMarker)> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let id = FormationId::parse(entry.file_name().to_str()?).ok()?;
            let path = entry.path();
            if !std::fs::symlink_metadata(&path).ok()?.is_dir() {
                return None;
            }
            let marker = FormationMarker::read(&path)?;
            (crate::formation_launch::canonical_project_dir(&marker.project_dir) == project_dir)
                .then_some((id, marker))
        })
        .collect();
    // Newest first, which for uuid v7 ids is both the lexical and the chronological order.
    owned.sort_unstable_by(|(a, _), (b, _)| b.as_str().cmp(a.as_str()));

    let age_floor_ms = policy
        .max_age_secs
        .map(|secs| now_ms.saturating_sub(secs.saturating_mul(1000)));

    let mut pruned = Vec::new();
    for (rank, (id, marker)) in owned.iter().enumerate() {
        if id.as_str() >= current.as_str() {
            continue;
        }
        let over_age = age_floor_ms.is_some_and(|floor| id.minted_at_ms() < floor);
        let over_count = policy.max_sessions.is_some_and(|max| rank >= max as usize);
        if !over_age && !over_count {
            continue;
        }
        if is_live(id, marker) {
            continue;
        }
        let reason = if over_age {
            crate::trace::RETENTION_REASON_MAX_AGE
        } else {
            crate::trace::RETENTION_REASON_MAX_SESSIONS
        };
        let path = formations_dir.join(id.as_str());
        if std::fs::remove_dir_all(&path).is_ok() {
            pruned.push(PrunedFormation {
                formation_id: id.clone(),
                path,
                reason,
            });
        }
    }
    pruned
}

// ── Record pruning ───────────────────────────────────────────────────────────

/// Remove the context directories under `record_root` that `policy.max_age_secs` does not keep.
///
/// Three things are never touched:
///
/// * **The context this launch is using** (`current_context_id`) — retention must not delete the
///   conversation it is about to continue.
/// * **A record no header line owns.** Every record written before the header existed is
///   unowned, and stays unowned until the capsule that writes it adopts it. `mur conversation rm`
///   is what reaches an abandoned one.
/// * **A record whose header names another capsule.** Two capsules pointed at one
///   `context.record_store` is a deliberate operator act; one silently pruning the other's
///   history is not.
///
/// `policy.max_messages` is not enforced here: it truncates the record this launch opens, at the
/// point it is opened, which is O(one record) instead of O(every conversation) — see
/// [`truncate_record`].
pub fn prune_records(
    record_root: &Path,
    capsule_name: &str,
    current_context_id: Option<&str>,
    policy: &murmur_artifact::ContextRetainConfig,
    now_ms: u64,
) -> Vec<PrunedRecord> {
    let Some(max_age_secs) = policy.max_age_secs else {
        return Vec::new();
    };
    let floor_ms = now_ms.saturating_sub(max_age_secs.saturating_mul(1000));

    let mut pruned = Vec::new();
    for (context_id, dir) in context_dirs(record_root) {
        if Some(context_id.as_str()) == current_context_id {
            continue;
        }
        let file = dir.join(RECORD_FILE_NAME);
        match read_header(&file) {
            Some(header) if header.capsule == capsule_name => {}
            _ => continue,
        }
        let Some(touched_ms) = last_touched_ms(&file) else {
            continue;
        };
        if touched_ms >= floor_ms {
            continue;
        }
        if std::fs::remove_dir_all(&dir).is_ok() {
            pruned.push(PrunedRecord {
                context_id,
                path: dir,
                reason: crate::trace::RETENTION_REASON_MAX_AGE,
            });
        }
    }
    pruned
}

/// Drop everything before the newest `keep` messages of `path`, atomically.
///
/// The file is rewritten, not edited in place: the kept tail plus a header line recording the
/// drop is staged in the record's own directory, fsynced, and renamed over the original. Same
/// directory means same filesystem, which is what makes the rename atomic — a crash at any point
/// leaves the original whole and the temp file orphaned, never a half-written record.
///
/// Every surviving message keeps the exact `id` bytes its line carried; truncation drops
/// messages and never renumbers them.
///
/// `capsule` is used only when the record has no header yet: an existing header's `capsule` is
/// preserved, so truncating never transfers ownership of a record between capsules.
///
/// A record already at or under `keep` is left untouched and reported as `dropped: 0`.
pub fn truncate_record(path: &Path, keep: u32, capsule: &str) -> Result<TruncationOutcome, String> {
    match stage_truncation(path, keep, capsule, now_ms())? {
        None => Ok(TruncationOutcome {
            dropped: 0,
            kept: count_messages(path),
            oldest_surviving_id: String::new(),
            last_dropped_id: String::new(),
        }),
        Some((staged, outcome)) => {
            staged.commit()?;
            Ok(outcome)
        }
    }
}

/// The half of [`truncate_record`] that touches nothing the reader can see: the rewritten record
/// is on disk under a temporary name in the record's own directory, and the original is still the
/// original until [`StagedRewrite::commit`] renames over it.
///
/// `None` means there was nothing to drop.
pub(crate) fn stage_truncation(
    path: &Path,
    keep: u32,
    capsule: &str,
    at_ms: u64,
) -> Result<Option<(StagedRewrite, TruncationOutcome)>, String> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(format!("{}: {err}", path.display())),
    };

    let existing = read_header(path);
    let lines: Vec<&str> = raw.lines().collect();
    // Every line that is a message, by its index in the file. Non-message lines — the header,
    // and a torn final line — are not messages to any reader and are not counted here either.
    let message_indices: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| parse_message_line(line).is_some())
        .map(|(index, _)| index)
        .collect();

    let total = message_indices.len();
    let keep = keep as usize;
    if total <= keep {
        return Ok(None);
    }
    let dropped = total - keep;
    let first_kept_index = message_indices[dropped];

    let id_of = |index: usize| -> String {
        parse_message_line(lines[index])
            .as_ref()
            .and_then(|message| crate::agent::message_id(message).map(str::to_string))
            .unwrap_or_default()
    };
    let last_dropped_id = id_of(message_indices[dropped - 1]);
    let oldest_surviving_id = message_indices
        .get(dropped)
        .map(|i| id_of(*i))
        .unwrap_or_default();

    let header = RecordHeader {
        kind: crate::conversation::RECORD_HEADER_TYPE.to_string(),
        capsule: existing
            .as_ref()
            .map(|h| h.capsule.clone())
            .unwrap_or_else(|| capsule.to_string()),
        created_ms: existing.as_ref().map(|h| h.created_ms).unwrap_or(at_ms),
        truncated: Some(TruncationMarker {
            // Cumulative over the record's life: an operator reading `dropped` wants to know how
            // much of this conversation is gone, not how much the last truncation took.
            dropped: existing
                .as_ref()
                .and_then(|h| h.truncated.as_ref())
                .map(|marker| marker.dropped)
                .unwrap_or(0)
                + dropped as u64,
            oldest_surviving_id: oldest_surviving_id.clone(),
            last_dropped_id: last_dropped_id.clone(),
            at_ms,
        }),
    };

    let mut contents = header_line(&header)?;
    for line in &lines[first_kept_index..] {
        contents.push_str(line);
        contents.push('\n');
    }

    let staged = StagedRewrite::stage_with_mode(
        path,
        contents.as_bytes(),
        Some(crate::conversation::RECORD_FILE_MODE),
    )?;
    Ok(Some((
        staged,
        TruncationOutcome {
            dropped: dropped as u64,
            kept: keep as u64,
            oldest_surviving_id,
            last_dropped_id,
        },
    )))
}

/// A rewrite that exists on disk but has not replaced anything yet.
///
/// Staged as `<record dir>/.conversation.jsonl.<pid>.<nonce>.tmp` — in the record's own
/// directory, so the rename that commits it is within one filesystem and therefore atomic, and
/// under a name no reader mistakes for a record.
pub(crate) struct StagedRewrite {
    target: PathBuf,
    temp: PathBuf,
    committed: bool,
}

impl StagedRewrite {
    /// Stage `contents` for `target`, with the staged file's mode set explicitly for a target whose
    /// permissions are part of its contract. `None` leaves the mode to the process umask.
    pub(crate) fn stage_with_mode(
        target: &Path,
        contents: &[u8],
        mode: Option<u32>,
    ) -> Result<Self, String> {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let dir = target
            .parent()
            .ok_or_else(|| format!("{} has no parent directory", target.display()))?;
        let name = target
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "record".to_string());
        let temp = dir.join(format!(
            ".{name}.{}.{}.tmp",
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        ));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        if let Some(mode) = mode {
            options.mode(mode);
        }
        let mut file = options
            .open(&temp)
            .map_err(|err| format!("{}: {err}", temp.display()))?;
        // The fsync is the point: a rename is only atomic with respect to a file whose bytes are
        // already durable. Without it a crash can leave the new name pointing at a short file.
        file.write_all(contents)
            .and_then(|()| file.sync_all())
            .map_err(|err| format!("{}: {err}", temp.display()))?;
        // Re-asserted after the write: `mode` on the open only applies when this call created the
        // file, and a mode that is part of the target's contract must hold for a staged file an
        // earlier run left behind too.
        if let Some(mode) = mode {
            std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(mode))
                .map_err(|err| format!("{}: {err}", temp.display()))?;
        }
        Ok(Self {
            target: target.to_path_buf(),
            temp,
            committed: false,
        })
    }

    #[cfg(test)]
    pub(crate) fn temp_path(&self) -> &Path {
        &self.temp
    }

    pub(crate) fn commit(mut self) -> Result<(), String> {
        std::fs::rename(&self.temp, &self.target)
            .map_err(|err| format!("{}: {err}", self.target.display()))?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for StagedRewrite {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_file(&self.temp);
        }
    }
}

// ── The record store, as the CLI reads it ────────────────────────────────────

/// Every conversation record on this host, record store by record store.
///
/// Ordered by record then context id, so two runs of `mur conversation ls` print the same table.
/// A host with no `~/.murmur/conversations/` has no records, which is an empty list rather than
/// an error.
pub fn list_records() -> Result<Vec<RecordSummary>, String> {
    let root = conversations_root()?;
    let mut summaries = Vec::new();
    for record in child_dirs(&root) {
        let store = root.join(&record);
        for (context_id, dir) in context_dirs(&store) {
            let path = dir.join(RECORD_FILE_NAME);
            if !path.exists() {
                continue;
            }
            let header = read_header(&path);
            summaries.push(RecordSummary {
                record: record.clone(),
                context_id,
                messages: count_messages(&path),
                bytes: std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0),
                last_touched_ms: last_touched_ms(&path),
                capsule: header.as_ref().map(|h| h.capsule.clone()),
                truncation: header.and_then(|h| h.truncated),
                path,
            });
        }
    }
    summaries.sort_by(|a, b| {
        a.record
            .cmp(&b.record)
            .then_with(|| a.context_id.cmp(&b.context_id))
    });
    Ok(summaries)
}

/// Remove one context directory whole, and report what it held.
///
/// The only way to reclaim a record no capsule owns — one written before the header line
/// existed, whose capsule has since been retired. Automatic pruning never touches those.
pub fn remove_record(record: &str, context_id: &str) -> Result<RemovedRecord, String> {
    let dir = conversations_root()?.join(record).join(context_id);
    if !dir.is_dir() {
        return Err(format!("{} is not a context directory", dir.display()));
    }
    let messages = count_messages(&dir.join(RECORD_FILE_NAME));
    std::fs::remove_dir_all(&dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    Ok(RemovedRecord {
        record: record.to_string(),
        context_id: context_id.to_string(),
        path: dir,
        messages,
    })
}

/// Every record that has something to say about `message_id`.
///
/// A record answers [`MessageStatus::Present`] when the id is one of its lines, and
/// [`MessageStatus::Truncated`] when it is not but the record has dropped a message at least as
/// new as this one — the id's own uuid v7 timestamp against the marker's `last_dropped_id`.
/// Records with nothing to say are left out, so an empty list is
/// [`MessageStatus::Unknown`] everywhere.
///
/// That classification is the whole reason the marker exists: an artifact that stored
/// `source_id: msg_X` and now finds nothing must be told the conversation was trimmed, not that
/// its reference was never real.
pub fn locate_message(message_id: &str) -> Result<Vec<MessageLocation>, String> {
    let minted_ms = message_id_timestamp_ms(message_id);
    let mut found = Vec::new();
    for summary in list_records()? {
        let raw = std::fs::read_to_string(&summary.path).unwrap_or_default();
        let mut position = 0u64;
        let mut at = None;
        for line in raw.lines() {
            let Some(message) = parse_message_line(line) else {
                continue;
            };
            position += 1;
            if crate::agent::message_id(&message) == Some(message_id) {
                at = Some(position);
            }
        }
        let status = match (at, summary.truncation.as_ref()) {
            (Some(position), _) => MessageStatus::Present {
                position,
                total: position.max(summary.messages),
            },
            (None, Some(marker)) => {
                let dropped_at = message_id_timestamp_ms(&marker.last_dropped_id);
                match (minted_ms, dropped_at) {
                    (Some(minted), Some(last)) if minted <= last => MessageStatus::Truncated {
                        dropped: marker.dropped,
                        oldest_surviving_id: marker.oldest_surviving_id.clone(),
                    },
                    _ => MessageStatus::Unknown,
                }
            }
            (None, None) => MessageStatus::Unknown,
        };
        if status != MessageStatus::Unknown {
            found.push(MessageLocation {
                record: summary.record,
                context_id: summary.context_id,
                path: summary.path,
                status,
            });
        }
    }
    Ok(found)
}

// ── Shared helpers ───────────────────────────────────────────────────────────

/// `~/.murmur/conversations`, resolved and not created.
pub fn conversations_root() -> Result<PathBuf, String> {
    Ok(crate::state_store::murmur_home_dir()?.join(CONVERSATION_ROOT_DIR))
}

/// Milliseconds since the epoch, for the callers that have no fixture to take a clock from.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// Immediate subdirectory names of `dir`, sorted, skipping dotted entries.
fn child_dirs(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .filter(|entry| entry.path().is_dir())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| !name.starts_with('.'))
            .collect(),
        Err(_) => Vec::new(),
    };
    names.sort();
    names
}

/// Every context directory under one record store, as `(context id, path)`.
fn context_dirs(record_root: &Path) -> Vec<(String, PathBuf)> {
    child_dirs(record_root)
        .into_iter()
        .map(|name| {
            let path = record_root.join(&name);
            (name, path)
        })
        .collect()
}

/// Message lines in `path`. The header line is not a message and is never counted, which is what
/// keeps it out of the `total` every reader of a record reports.
fn count_messages(path: &Path) -> u64 {
    std::fs::read_to_string(path)
        .map(|raw| {
            raw.lines()
                .filter(|line| parse_message_line(line).is_some())
                .count() as u64
        })
        .unwrap_or(0)
}

/// Last write to `path`, in milliseconds since the epoch.
fn last_touched_ms(path: &Path) -> Option<u64> {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|elapsed| elapsed.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::parse_header;
    use murmur_artifact::{ContextRetainConfig, TraceRetainConfig};
    use serde_json::json;

    /// A `ses_` name whose uuid-v7 timestamp is exactly `ms`, and whose tail distinguishes it
    /// from any sibling minted in the same millisecond.
    fn ses(ms: u64, tail: u64) -> String {
        format!("ses_{ms:012x}{tail:020x}")
    }

    /// A `msg_` id whose uuid-v7 timestamp is exactly `ms`.
    fn msg(ms: u64, tail: u64) -> String {
        format!("msg_{ms:012x}{tail:020x}")
    }

    fn message_line(id: &str, text: &str) -> String {
        json!({"role": "user", "content": [{"type": "text", "text": text}], "id": id}).to_string()
    }

    fn make_sessions(root: &Path, names: &[String]) {
        for name in names {
            let dir = root.join(name);
            std::fs::create_dir_all(dir.join("blobs")).unwrap();
            std::fs::write(dir.join("trace.jsonl"), "{}\n").unwrap();
            std::fs::write(dir.join("blobs").join("deadbeef"), b"body").unwrap();
        }
    }

    fn names(root: &Path) -> Vec<String> {
        let mut found: Vec<String> = std::fs::read_dir(root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        found.sort();
        found
    }

    const NOW: u64 = 1_756_400_000_000;

    /// the count is over every entry present, the running session included,
    /// so `max_sessions: 3` leaves three directories and one of them is the current one. What went
    /// went whole — no orphaned `trace.jsonl`, no orphaned `blobs/`.
    #[test]
    fn max_sessions_leaves_exactly_the_newest_n_including_the_current() {
        let root = tempfile::tempdir().unwrap();
        let all: Vec<String> = (1..=6).map(|n| ses(NOW - (6 - n) * 1000, n)).collect();
        make_sessions(root.path(), &all);

        let pruned = prune_sessions(
            root.path(),
            &all[5],
            &TraceRetainConfig {
                max_sessions: Some(3),
                max_age_secs: None,
            },
            NOW,
        );

        assert_eq!(
            names(root.path()),
            vec![all[3].clone(), all[4].clone(), all[5].clone()]
        );
        let mut removed: Vec<String> = pruned.iter().map(|p| p.name.clone()).collect();
        removed.sort();
        assert_eq!(
            removed,
            vec![all[0].clone(), all[1].clone(), all[2].clone()]
        );
        assert!(pruned
            .iter()
            .all(|p| p.reason == crate::trace::RETENTION_REASON_MAX_SESSIONS));
        for name in &all[..3] {
            assert!(!root.path().join(name).exists(), "{name} went whole");
        }
    }

    /// the decision comes out of the id. Every directory here is created in
    /// the same instant, so every mtime is equally fresh; the two whose ids encode two hours ago
    /// are the two that go.
    #[test]
    fn max_age_reads_the_id_and_never_the_filesystem() {
        let root = tempfile::tempdir().unwrap();
        let old_a = ses(NOW - 7_200_000, 1);
        let old_b = ses(NOW - 7_100_000, 2);
        let recent = ses(NOW - 60_000, 3);
        let current = ses(NOW, 4);
        make_sessions(
            root.path(),
            &[
                old_a.clone(),
                old_b.clone(),
                recent.clone(),
                current.clone(),
            ],
        );

        let pruned = prune_sessions(
            root.path(),
            &current,
            &TraceRetainConfig {
                max_sessions: None,
                max_age_secs: Some(900),
            },
            NOW,
        );

        assert_eq!(pruned.len(), 2, "{pruned:?}");
        assert!(pruned
            .iter()
            .all(|p| p.reason == crate::trace::RETENTION_REASON_MAX_AGE));
        assert_eq!(names(root.path()), vec![recent, current]);
    }

    /// the two keys are ANDed. An old-id directory never survives on rank, and a recent-id
    /// directory outside the count does not survive on age.
    #[test]
    fn both_trace_keys_keep_only_sessions_inside_both() {
        let root = tempfile::tempdir().unwrap();
        let old = [ses(NOW - 7_200_000, 1), ses(NOW - 7_100_000, 2)];
        let recent = [
            ses(NOW - 300_000, 3),
            ses(NOW - 200_000, 4),
            ses(NOW - 100_000, 5),
        ];
        let current = ses(NOW, 6);
        let mut all = old.to_vec();
        all.extend(recent.iter().cloned());
        all.push(current.clone());
        make_sessions(root.path(), &all);

        let pruned = prune_sessions(
            root.path(),
            &current,
            &TraceRetainConfig {
                max_sessions: Some(2),
                max_age_secs: Some(900),
            },
            NOW,
        );

        assert_eq!(names(root.path()), vec![recent[2].clone(), current.clone()]);
        assert_eq!(pruned.len(), 4);
        for session in &pruned {
            let expected = if old.contains(&session.name) {
                crate::trace::RETENTION_REASON_MAX_AGE
            } else {
                crate::trace::RETENTION_REASON_MAX_SESSIONS
            };
            assert_eq!(session.reason, expected, "{session:?}");
        }
    }

    /// the floor. Nothing at or after the current session's own id is ever a candidate, which
    /// is also what keeps a sibling capsule's concurrently running session safe with no lock file.
    #[test]
    fn nothing_at_or_after_the_current_session_id_is_a_candidate() {
        let root = tempfile::tempdir().unwrap();
        let current = ses(NOW, 5);
        // A sibling launched a millisecond later, while this session was still staging.
        let sibling = ses(NOW + 1, 6);
        let older = ses(NOW - 1000, 4);
        make_sessions(
            root.path(),
            &[current.clone(), sibling.clone(), older.clone()],
        );

        let pruned = prune_sessions(
            root.path(),
            &current,
            &TraceRetainConfig {
                max_sessions: Some(1),
                max_age_secs: Some(1),
            },
            NOW,
        );

        assert_eq!(pruned.len(), 1);
        assert_eq!(pruned[0].name, older);
        assert_eq!(names(root.path()), vec![current.clone(), sibling]);
        assert!(root.path().join(&current).join("trace.jsonl").exists());
    }

    /// A policy with neither key set — which the manifest parser refuses, but which the runtime
    /// must still treat as inert rather than as "prune everything".
    #[test]
    fn an_empty_policy_removes_nothing() {
        let root = tempfile::tempdir().unwrap();
        let all = [ses(NOW - 9_000_000, 1), ses(NOW, 2)];
        make_sessions(root.path(), &all);
        assert!(prune_sessions(
            root.path(),
            &all[1],
            &TraceRetainConfig {
                max_sessions: None,
                max_age_secs: None
            },
            NOW
        )
        .is_empty());
        assert_eq!(names(root.path()), all.to_vec());
    }

    // ── Truncation ───────────────────────────────────────────────────────────

    /// A record with `count` messages, owned by `capsule` when `owned`.
    fn write_record(dir: &Path, capsule: Option<&str>, count: u64) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(RECORD_FILE_NAME);
        let mut contents = String::new();
        if let Some(capsule) = capsule {
            contents.push_str(
                &header_line(&RecordHeader {
                    kind: crate::conversation::RECORD_HEADER_TYPE.to_string(),
                    capsule: capsule.to_string(),
                    created_ms: NOW,
                    truncated: None,
                })
                .unwrap(),
            );
        }
        for i in 1..=count {
            contents.push_str(&message_line(
                &msg(NOW - (count - i) * 1000, i),
                &format!("m{i}"),
            ));
            contents.push('\n');
        }
        std::fs::write(&path, contents).unwrap();
        path
    }

    /// the newest N survive with their ids intact, the file is a header plus exactly N
    /// message lines, and every line parses.
    #[test]
    fn max_messages_leaves_the_newest_n_with_their_ids_intact() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_record(dir.path(), Some("capsule"), 10);
        let before: Vec<String> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .filter_map(parse_message_line)
            .map(|m| m["id"].as_str().unwrap().to_string())
            .collect();

        let outcome = truncate_record(&path, 3, "capsule").unwrap();
        assert_eq!(outcome.dropped, 7);
        assert_eq!(outcome.kept, 3);
        assert_eq!(outcome.last_dropped_id, before[6]);
        assert_eq!(outcome.oldest_surviving_id, before[7]);

        let raw = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(
            lines.len(),
            4,
            "a header plus exactly three messages: {lines:?}"
        );
        for line in &lines {
            serde_json::from_str::<serde_json::Value>(line).expect("every line is JSON");
        }
        let header = parse_header(lines[0]).expect("the first line is the header");
        assert_eq!(header.capsule, "capsule");
        let marker = header.truncated.expect("the header records the drop");
        assert_eq!(marker.dropped, 7);
        assert_eq!(marker.last_dropped_id, before[6]);
        assert_eq!(marker.oldest_surviving_id, before[7]);

        let after: Vec<String> = lines[1..]
            .iter()
            .map(|line| {
                parse_message_line(line).expect("a message with a string role")["id"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(after, before[7..], "surviving ids are the bytes they were");
    }

    /// The header does not count as a message: `count_messages` — the same rule
    /// `crate::conversation::read_record` applies to `total` — is unchanged by it.
    #[test]
    fn a_header_line_is_not_counted_as_a_message() {
        let dir = tempfile::tempdir().unwrap();
        let headerless = write_record(&dir.path().join("a"), None, 4);
        let owned = write_record(&dir.path().join("b"), Some("capsule"), 4);
        assert_eq!(count_messages(&headerless), 4);
        assert_eq!(count_messages(&owned), 4);
    }

    /// Truncating twice accumulates: `dropped` is what this record has lost over its life, not
    /// what the last rewrite took.
    #[test]
    fn a_second_truncation_accumulates_the_dropped_count() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_record(dir.path(), Some("capsule"), 10);
        truncate_record(&path, 6, "capsule").unwrap();
        truncate_record(&path, 2, "capsule").unwrap();
        let marker = read_header(&path).unwrap().truncated.unwrap();
        assert_eq!(marker.dropped, 8);
        assert_eq!(count_messages(&path), 2);
    }

    #[test]
    fn truncation_rewrite_keeps_the_record_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = write_record(dir.path(), Some("capsule"), 5);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        truncate_record(&path, 2, "capsule").unwrap();

        assert_eq!(count_messages(&path), 2);
        assert_eq!(crate::murmur_home::mode_of(&path), 0o600);
    }

    /// A record already at or under the limit is left untouched, byte for byte.
    #[test]
    fn a_record_inside_the_limit_is_not_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_record(dir.path(), Some("capsule"), 3);
        let before = std::fs::read(&path).unwrap();
        let outcome = truncate_record(&path, 3, "capsule").unwrap();
        assert_eq!(outcome.dropped, 0);
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    /// a crash mid-truncate leaves the original intact. The staged rewrite is a temp file in
    /// the record's own directory — same filesystem, so the rename that commits it is atomic —
    /// and until that rename the original parses whole and holds its original message count.
    #[test]
    fn a_staged_truncation_leaves_the_original_intact_until_the_rename() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_record(dir.path(), Some("capsule"), 10);
        let before = std::fs::read(&path).unwrap();

        let (staged, outcome) = stage_truncation(&path, 3, "capsule", NOW).unwrap().unwrap();
        assert_eq!(outcome.dropped, 7);

        let temp = staged.temp_path().to_path_buf();
        assert_eq!(
            temp.parent(),
            path.parent(),
            "the temp file is in the record's own directory, so the rename is atomic"
        );
        assert_ne!(temp.file_name().unwrap(), RECORD_FILE_NAME);

        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "the original is byte-identical before the rename"
        );
        assert_eq!(count_messages(&path), 10);
        for line in std::fs::read_to_string(&path).unwrap().lines() {
            serde_json::from_str::<serde_json::Value>(line).expect("the original parses whole");
        }

        // Dropping without committing is the crash: the original stands and no debris is left.
        drop(staged);
        assert!(!temp.exists());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(count_messages(&path), 10);
    }

    // ── Record pruning ───────────────────────────────────────────────────────

    /// automatic record pruning touches only a record whose header names this
    /// capsule. A record another capsule owns and a record no header owns both survive, however
    /// old they are — and the current launch's own context is never removed.
    #[test]
    fn record_pruning_skips_another_capsules_records_and_every_unowned_one() {
        let root = tempfile::tempdir().unwrap();
        write_record(&root.path().join("mine"), Some("mine"), 3);
        write_record(&root.path().join("theirs"), Some("theirs"), 3);
        write_record(&root.path().join("unowned"), None, 3);
        write_record(&root.path().join("current"), Some("mine"), 3);

        // Far enough in the future that every mtime is outside a one-second window.
        let far_future = now_ms() + 3_600_000;
        let pruned = prune_records(
            root.path(),
            "mine",
            Some("current"),
            &ContextRetainConfig {
                max_messages: None,
                max_age_secs: Some(1),
            },
            far_future,
        );

        assert_eq!(pruned.len(), 1, "{pruned:?}");
        assert_eq!(pruned[0].context_id, "mine");
        assert_eq!(pruned[0].reason, crate::trace::RETENTION_REASON_MAX_AGE);
        assert_eq!(names(root.path()), vec!["current", "theirs", "unowned"]);
    }

    /// a record inside the window stays. Age is last write, so a record written a moment ago
    /// survives a 90-day policy however long ago its conversation started.
    #[test]
    fn a_record_written_inside_the_window_is_kept() {
        let root = tempfile::tempdir().unwrap();
        write_record(&root.path().join("fresh"), Some("mine"), 3);
        let pruned = prune_records(
            root.path(),
            "mine",
            None,
            &ContextRetainConfig {
                max_messages: None,
                max_age_secs: Some(90 * 86_400),
            },
            now_ms(),
        );
        assert!(pruned.is_empty());
        assert_eq!(names(root.path()), vec!["fresh"]);
    }

    /// `max_messages` alone never removes a context directory: it is enforced on the record the
    /// launch opens, not by the age sweep.
    #[test]
    fn max_messages_alone_removes_no_record() {
        let root = tempfile::tempdir().unwrap();
        write_record(&root.path().join("ctx"), Some("mine"), 100);
        assert!(prune_records(
            root.path(),
            "mine",
            None,
            &ContextRetainConfig {
                max_messages: Some(1),
                max_age_secs: None
            },
            now_ms() + 3_600_000,
        )
        .is_empty());
        assert_eq!(names(root.path()), vec!["ctx"]);
    }

    // ── Formation pruning ────────────────────────────────────────────────────

    /// A formation id whose uuid-v7 timestamp is exactly `ms`.
    fn frm(ms: u64, tail: u64) -> FormationId {
        FormationId::parse(&format!("frm_{ms:012x}{tail:020x}")).unwrap()
    }

    fn retain(max_sessions: Option<u32>, max_age_secs: Option<u64>) -> TraceRetainConfig {
        TraceRetainConfig {
            max_sessions,
            max_age_secs,
        }
    }

    /// `<root>/<id>/worker/.murmur/ses_…/trace.jsonl`, with `marker` as `formation.json` when
    /// given.
    fn make_formation(root: &Path, id: &FormationId, marker: Option<&str>) -> PathBuf {
        let dir = root.join(id.as_str());
        let session = dir.join("worker/.murmur").join(ses(NOW, 1));
        std::fs::create_dir_all(&session).unwrap();
        std::fs::write(session.join("trace.jsonl"), "{}\n").unwrap();
        if let Some(marker) = marker {
            std::fs::write(
                dir.join(crate::formation_launch::FORMATION_MARKER_FILE),
                marker,
            )
            .unwrap();
        }
        dir
    }

    fn marker_naming(project: &Path) -> String {
        serde_json::to_string(&FormationMarker {
            project_dir: project.to_path_buf(),
            launcher_pid: 1,
            launcher_start: String::new(),
        })
        .unwrap()
    }

    /// Owned formations of `project` under `root`, one per id.
    fn make_owned(root: &Path, project: &Path, ids: &[&FormationId]) {
        for id in ids {
            make_formation(root, id, Some(&marker_naming(project)));
        }
    }

    fn removed(pruned: &[PrunedFormation]) -> Vec<(FormationId, &'static str)> {
        pruned
            .iter()
            .map(|p| (p.formation_id.clone(), p.reason))
            .collect()
    }

    const DAY_MS: u64 = 86_400_000;

    /// The current formation and every one minted after it stay, under both keys at their
    /// tightest and every directory past both.
    #[test]
    fn prune_formations_never_removes_the_current_or_a_later_formation() {
        let root = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let old = frm(NOW - 3 * DAY_MS, 1);
        let current = frm(NOW - 2 * DAY_MS, 2);
        let later = frm(NOW - 2 * DAY_MS + 1000, 3);
        make_owned(root.path(), project.path(), &[&old, &current, &later]);

        let pruned = prune_formations(
            root.path(),
            &current,
            project.path(),
            &retain(Some(1), Some(1)),
            NOW,
            |_, _| false,
        );

        assert_eq!(
            removed(&pruned),
            [(old.clone(), crate::trace::RETENTION_REASON_MAX_AGE)]
        );
        assert_eq!(pruned[0].path, root.path().join(old.as_str()));
        assert_eq!(
            names(root.path()),
            vec![current.as_str().to_string(), later.as_str().to_string()]
        );
    }

    /// `max_sessions: 3` leaves the three newest of this project's directories, the current one
    /// and one minted after it among them, and what went is listed newest first.
    #[test]
    fn prune_formations_keeps_the_newest_n_with_the_current_counted() {
        let root = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let all: Vec<FormationId> = (1..=5).map(|n| frm(NOW - (5 - n) * 1000, n)).collect();
        make_owned(root.path(), project.path(), &all.iter().collect::<Vec<_>>());

        let pruned = prune_formations(
            root.path(),
            &all[3],
            project.path(),
            &retain(Some(3), None),
            NOW,
            |_, _| false,
        );

        let reason = crate::trace::RETENTION_REASON_MAX_SESSIONS;
        assert_eq!(
            removed(&pruned),
            [(all[1].clone(), reason), (all[0].clone(), reason)]
        );
        assert_eq!(
            names(root.path()),
            all[2..]
                .iter()
                .map(|id| id.as_str().to_string())
                .collect::<Vec<_>>()
        );
    }

    /// Age comes out of the id: every directory here is made in the same instant, and the two
    /// whose ids encode two hours ago are the two that go.
    #[test]
    fn prune_formations_reads_age_from_the_id() {
        let root = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let old_a = frm(NOW - 7_200_000, 1);
        let old_b = frm(NOW - 7_100_000, 2);
        let recent = frm(NOW - 60_000, 3);
        let current = frm(NOW, 4);
        make_owned(
            root.path(),
            project.path(),
            &[&old_a, &old_b, &recent, &current],
        );

        let pruned = prune_formations(
            root.path(),
            &current,
            project.path(),
            &retain(None, Some(900)),
            NOW,
            |_, _| false,
        );

        let reason = crate::trace::RETENTION_REASON_MAX_AGE;
        assert_eq!(removed(&pruned), [(old_b, reason), (old_a, reason)]);
        assert_eq!(
            names(root.path()),
            vec![recent.as_str().to_string(), current.as_str().to_string()]
        );
    }

    /// A directory past both limits is reported as too old; one past the count alone, as over the
    /// count.
    #[test]
    fn prune_formations_attributes_reasons_age_first() {
        let root = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let old = frm(NOW - 7_200_000, 1);
        let recent = frm(NOW - 60_000, 2);
        let current = frm(NOW, 3);
        make_owned(root.path(), project.path(), &[&old, &recent, &current]);

        let pruned = prune_formations(
            root.path(),
            &current,
            project.path(),
            &retain(Some(1), Some(900)),
            NOW,
            |_, _| false,
        );

        assert_eq!(
            removed(&pruned),
            [
                (recent, crate::trace::RETENTION_REASON_MAX_SESSIONS),
                (old, crate::trace::RETENTION_REASON_MAX_AGE),
            ]
        );
    }

    /// A live formation is never removed, and still counts: the one ranked behind it goes under
    /// `max_sessions: 2` although only two directories are not live.
    #[test]
    fn prune_formations_skips_a_live_directory_and_keeps_its_rank() {
        let root = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let older = frm(NOW - 3000, 1);
        let live = frm(NOW - 2000, 2);
        let current = frm(NOW, 3);
        make_owned(root.path(), project.path(), &[&older, &live, &current]);

        let asked = std::cell::RefCell::new(Vec::new());
        let pruned = prune_formations(
            root.path(),
            &current,
            project.path(),
            &retain(Some(1), None),
            NOW,
            |id, marker| {
                assert_eq!(marker.launcher_pid, 1);
                asked.borrow_mut().push(id.clone());
                *id == live
            },
        );

        assert_eq!(
            removed(&pruned),
            [(older.clone(), crate::trace::RETENTION_REASON_MAX_SESSIONS)]
        );
        assert_eq!(*asked.borrow(), [live.clone(), older]);
        assert_eq!(
            names(root.path()),
            vec![live.as_str().to_string(), current.as_str().to_string()]
        );

        // Ranked behind the live one, a directory inside the count is kept.
        let kept = frm(NOW - 2500, 4);
        make_owned(root.path(), project.path(), &[&kept]);
        let pruned = prune_formations(
            root.path(),
            &current,
            project.path(),
            &retain(Some(3), None),
            NOW,
            |id, _| *id == live,
        );
        assert!(pruned.is_empty(), "{pruned:?}");
        let pruned = prune_formations(
            root.path(),
            &current,
            project.path(),
            &retain(Some(2), None),
            NOW,
            |id, _| *id == live,
        );
        assert_eq!(
            removed(&pruned),
            [(kept, crate::trace::RETENTION_REASON_MAX_SESSIONS)]
        );
    }

    /// No marker, a marker that does not parse, and a marker naming another project: none is
    /// removed, and none takes a rank from this project's own directories.
    #[test]
    fn prune_formations_never_ranks_or_removes_unowned_or_foreign_directories() {
        let root = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let owned_old = frm(NOW - 3 * DAY_MS, 1);
        let owned_kept = frm(NOW - 2 * DAY_MS, 2);
        let unmarked = frm(NOW - 2 * DAY_MS + 1, 3);
        let garbled = frm(NOW - 2 * DAY_MS + 2, 4);
        let foreign = frm(NOW - 2 * DAY_MS + 3, 5);
        let current = frm(NOW, 6);
        make_owned(
            root.path(),
            project.path(),
            &[&owned_old, &owned_kept, &current],
        );
        make_formation(root.path(), &unmarked, None);
        make_formation(root.path(), &garbled, Some("not json {"));
        make_formation(root.path(), &foreign, Some(&marker_naming(other.path())));

        let pruned = prune_formations(
            root.path(),
            &current,
            project.path(),
            &retain(Some(2), None),
            NOW,
            |_, _| false,
        );
        assert_eq!(
            removed(&pruned),
            [(owned_old, crate::trace::RETENTION_REASON_MAX_SESSIONS)]
        );

        let pruned = prune_formations(
            root.path(),
            &current,
            project.path(),
            &retain(Some(1), Some(DAY_MS / 1000)),
            NOW,
            |_, _| false,
        );
        assert_eq!(
            removed(&pruned),
            [(owned_kept, crate::trace::RETENTION_REASON_MAX_AGE)]
        );
        for id in [&unmarked, &garbled, &foreign, &current] {
            assert!(root.path().join(id.as_str()).is_dir(), "{id}");
        }
    }

    /// A symlink, a regular file, and a directory whose name is not a formation id are never
    /// candidates, marker or not; the symlink's target is left alone too.
    #[test]
    fn prune_formations_ignores_a_symlink_a_regular_file_and_a_name_that_is_not_an_id() {
        let root = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let marker = marker_naming(project.path());
        let target = make_formation(elsewhere.path(), &frm(NOW - 3 * DAY_MS, 1), Some(&marker));
        let linked = frm(NOW - 3 * DAY_MS, 2);
        std::os::unix::fs::symlink(&target, root.path().join(linked.as_str())).unwrap();
        std::fs::write(root.path().join(frm(NOW - 3 * DAY_MS, 3).as_str()), &marker).unwrap();
        for name in ["not-a-formation", "frm_ABC", "frm_0123"] {
            std::fs::create_dir(root.path().join(name)).unwrap();
            std::fs::write(
                root.path()
                    .join(name)
                    .join(crate::formation_launch::FORMATION_MARKER_FILE),
                &marker,
            )
            .unwrap();
        }
        let before = names(root.path());

        let pruned = prune_formations(
            root.path(),
            &frm(NOW, 9),
            project.path(),
            &retain(Some(1), Some(1)),
            NOW,
            |_, _| false,
        );

        assert!(pruned.is_empty(), "{pruned:?}");
        assert_eq!(names(root.path()), before);
        assert!(target.join("worker/.murmur").is_dir());
    }

    /// A policy with neither key is no policy, and a missing formations directory holds nothing.
    #[test]
    fn prune_formations_with_neither_key_or_no_directory_removes_nothing() {
        let root = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let old = frm(NOW - 3 * DAY_MS, 1);
        make_owned(root.path(), project.path(), &[&old]);

        let pruned = prune_formations(
            root.path(),
            &frm(NOW, 2),
            project.path(),
            &retain(None, None),
            NOW,
            |_, _| panic!("nothing is a candidate without a policy"),
        );
        assert!(pruned.is_empty());
        assert!(root.path().join(old.as_str()).is_dir());

        assert!(prune_formations(
            &root.path().join("missing"),
            &frm(NOW, 2),
            project.path(),
            &retain(Some(1), Some(1)),
            NOW,
            |_, _| false,
        )
        .is_empty());
    }

    /// The project directory is compared canonicalized: a launch through a symlink to the project
    /// owns what a launch through its real path marked.
    #[test]
    fn prune_formations_compares_project_directories_canonically() {
        let root = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let links = tempfile::tempdir().unwrap();
        let link = links.path().join("project");
        std::os::unix::fs::symlink(project.path(), &link).unwrap();
        let old = frm(NOW - 3 * DAY_MS, 1);
        make_owned(
            root.path(),
            &std::fs::canonicalize(project.path()).unwrap(),
            &[&old],
        );

        let pruned = prune_formations(
            root.path(),
            &frm(NOW, 2),
            &link,
            &retain(None, Some(1)),
            NOW,
            |_, _| false,
        );
        assert_eq!(
            removed(&pruned),
            [(old, crate::trace::RETENTION_REASON_MAX_AGE)]
        );
    }

    /// A directory that cannot be removed whole is not reported as removed.
    #[test]
    fn prune_formations_leaves_a_directory_it_could_not_remove_out_of_the_result() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let stuck = frm(NOW - 3 * DAY_MS, 1);
        let gone = frm(NOW - 3 * DAY_MS + 1, 2);
        make_owned(root.path(), project.path(), &[&stuck, &gone]);
        let stuck_dir = root.path().join(stuck.as_str());
        std::fs::set_permissions(&stuck_dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        // Root removes it regardless, so there is nothing to observe.
        if std::fs::write(stuck_dir.join("probe"), b"").is_ok() {
            std::fs::set_permissions(&stuck_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
            return;
        }

        let pruned = prune_formations(
            root.path(),
            &frm(NOW, 3),
            project.path(),
            &retain(None, Some(1)),
            NOW,
            |_, _| false,
        );
        std::fs::set_permissions(&stuck_dir, std::fs::Permissions::from_mode(0o700)).unwrap();

        assert_eq!(
            removed(&pruned),
            [(gone, crate::trace::RETENTION_REASON_MAX_AGE)]
        );
        assert!(stuck_dir.is_dir());
    }
}
