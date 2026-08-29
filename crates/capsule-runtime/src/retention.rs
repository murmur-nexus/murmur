//! Retention: what bounds the two stores that grow, and what it costs to bound them.
//!
//! Two stores, two operations, because they are different shapes:
//!
//! * A **session directory** — `<workdir>/.murmur/<ses_…>/` — is an independent unit. Nothing
//!   references it and nothing spans two of them, so it is pruned by being deleted whole, taking
//!   its `trace.jsonl` and its `blobs/` with it.
//! * A **conversation record** — `~/.murmur/conversations/<record>/<ctx>/conversation.jsonl` — is
//!   one unit that grows. Deleting it destroys the conversation an operator wanted kept, so it is
//!   pruned by truncating its front: the oldest messages go, the recent ones stay, and every
//!   surviving message keeps the id it has always had.
//!
//! Three properties hold across everything below:
//!
//! * **No defaults.** Every entry point takes the policy it enforces. An absent policy is an
//!   absent call: nothing here has a fallback that deletes.
//! * **No clock of its own.** Both prune functions take `now_ms`, so age is a fixture's to supply
//!   and a test never sleeps.
//! * **No trace of its own.** Both prune functions return what they removed and write nothing;
//!   the caller holds the [`crate::trace::TraceWriter`] and records the deletion against the
//!   session that performed it.
//!
//! Session age is read out of the `ses_` id — a uuid v7 whose first 12 hex characters are a
//! millisecond timestamp — so the whole session policy is computed from one directory listing
//! with no `stat`. Record age is the mtime of `conversation.jsonl`, because a context id is not
//! necessarily a uuid and "untouched since" is the useful question for a record that spans weeks.

use std::path::{Path, PathBuf};

use crate::conversation::{
    header_line, message_id_timestamp_ms, parse_message_line, read_header, CONVERSATION_ROOT_DIR,
    RECORD_FILE_NAME,
};

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

/// A context directory that was removed, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrunedRecord {
    /// The context id, which is the directory name under the record root.
    pub context_id: String,
    /// The directory that was removed, whole.
    pub path: PathBuf,
    /// Messages the removed record held, for the operator reading the trace event.
    pub messages: u64,
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
    let hex = name.strip_prefix("ses_")?;
    if hex.len() < 12 {
        return None;
    }
    u64::from_str_radix(&hex[..12], 16).ok()
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

// ── Record pruning ───────────────────────────────────────────────────────────

/// Remove the context directories under `record_root` that `policy.max_age_secs` does not keep.
///
/// Three things are never touched, and each is a decision this slice made rather than an
/// oversight:
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
        let messages = count_messages(&file);
        if std::fs::remove_dir_all(&dir).is_ok() {
            pruned.push(PrunedRecord {
                context_id,
                path: dir,
                messages,
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

    let staged = StagedRewrite::stage(path, contents.as_bytes())?;
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
    pub(crate) fn stage(target: &Path, contents: &[u8]) -> Result<Self, String> {
        use std::io::Write;

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
        let mut file =
            std::fs::File::create(&temp).map_err(|err| format!("{}: {err}", temp.display()))?;
        // The fsync is the point: a rename is only atomic with respect to a file whose bytes are
        // already durable. Without it a crash can leave the new name pointing at a short file.
        file.write_all(contents)
            .and_then(|()| file.sync_all())
            .map_err(|err| format!("{}: {err}", temp.display()))?;
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
/// keeps every existing reader's `total` unchanged by this slice.
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

    /// S2 at the unit level: the count is over every entry present, the running session included,
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

    /// S3 at the unit level: the decision comes out of the id. Every directory here is created in
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

    /// S4: the two keys are ANDed. An old-id directory never survives on rank, and a recent-id
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

    /// S5: the floor. Nothing at or after the current session's own id is ever a candidate, which
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

    /// S6: the newest N survive with their ids intact, the file is a header plus exactly N
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

    /// S7: a crash mid-truncate leaves the original intact. The staged rewrite is a temp file in
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

    /// S10 and S11: automatic record pruning touches only a record whose header names this
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
        assert_eq!(pruned[0].messages, 3);
        assert_eq!(pruned[0].reason, crate::trace::RETENTION_REASON_MAX_AGE);
        assert_eq!(names(root.path()), vec!["current", "theirs", "unowned"]);
    }

    /// S9: a record inside the window stays. Age is last write, so a record written a moment ago
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
}
