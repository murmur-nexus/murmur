//! The durable conversation record: `~/.murmur/conversations/<record>/<context-id>/conversation.jsonl`,
//! one JSON object per line, in the order the messages entered the context.
//!
//! Three properties decide the layout:
//!
//! * **Outside every workdir, and never preopened.** A record outlives the session that wrote it,
//!   so it cannot live in a session directory; and it is the whole conversation, so no artifact
//!   gets it through a filesystem grant. `murmur:conversation/read` is the only way in.
//! * **Keyed by capsule and context, not by workdir.** `<record>` defaults to the capsule name and
//!   is overridable with `context.record_store`; `<context-id>` is the task's context id, which is
//!   client-supplied over A2A and settable with `mur run --context`. Two runs sharing a context id
//!   share one record.
//! * **Append-only, and never fatal.** The runtime is the only writer. A directory that cannot be
//!   created, a line that cannot be appended and a line that will not parse are each reported once
//!   and then survived: a capsule whose record is unwritable must still do its work.
//!
//! Resolution and creation are separate, as they are for [`crate::state_store`]: [`record_root`]
//! resolves and validates without touching the filesystem, and only the first append creates
//! anything.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::bindings::hook::murmur::conversation::read::{Message, MessagePage};
use crate::errors::RuntimeError;

/// Directory under the murmur home holding every record, one subdirectory per record name.
pub(crate) const CONVERSATION_ROOT_DIR: &str = "conversations";

/// The record file inside `<record>/<context-id>/`.
pub(crate) const RECORD_FILE_NAME: &str = "conversation.jsonl";

/// The `type` value on a record's header line, and the only value a header is recognised by.
pub(crate) const RECORD_HEADER_TYPE: &str = "murmur.record";

/// Mode applied to the conversation root, each record directory and each context directory:
/// owner-only, because a record is the whole of one capsule's conversation.
const RECORD_DIR_MODE: u32 = 0o700;

/// Prefix on every host-minted cursor, so a value the host did not mint is refused rather than
/// parsed into a position.
const CURSOR_PREFIX: &str = "mc_";

/// Inclusive ceiling on `read-messages`' `limit`. A page is copied into the guest's memory whole,
/// so the host caps it rather than trusting a caller's number.
const MAX_PAGE_LIMIT: u32 = 100;

/// What every refusal of a record or context name says. The rule itself is
/// [`crate::state_store::validate_store_name`] — the one definition of "a single usable path
/// segment" — and this is the wording an operator gets, which names all of it at once because the
/// value they wrote is in front of them.
const SEGMENT_RULE: &str = "must be a single path segment: no '/', no '.' or '..', not absolute, \
                            and not starting with a dot";

/// What one record has already dropped from its front.
///
/// Cumulative over the record's life: `dropped` is every message truncation has ever taken from
/// this record, not what the last one took.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TruncationMarker {
    pub dropped: u64,
    /// The `msg_` id of the oldest message still in the file.
    pub oldest_surviving_id: String,
    /// The `msg_` id of the newest message that went. A reference to any message minted at or
    /// before this one reads as truncated rather than as unknown — see
    /// [`crate::retention::locate_message`].
    pub last_dropped_id: String,
    /// When the most recent truncation ran, in milliseconds since the epoch.
    pub at_ms: u64,
}

/// The first line of a record: a JSON object with a `type` key and no `role`.
///
/// Deliberately not a message, and deliberately not a sidecar. Not a message, because every
/// reader of a record skips a line that is not a JSON object carrying a string `role`, so a
/// header is invisible to `murmur:conversation/read`, to the `threaded` reload, and to the
/// `total` a page reports. Not a sidecar, because a sidecar needs a second write that a crash
/// could desynchronise from the rename — a header makes the whole truncation exactly one atomic
/// rename of one file.
///
/// [`Self::capsule`] is what makes automatic record pruning store-safe: two capsules pointed at
/// one `context.record_store` each own only the records their own name is on.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RecordHeader {
    /// Always [`RECORD_HEADER_TYPE`]. A first line carrying anything else is not a header.
    #[serde(rename = "type")]
    pub kind: String,
    /// The capsule that owns this record — `name:` from its manifest, never the record store.
    pub capsule: String,
    /// When the header was written, in milliseconds since the epoch. On a record adopted from
    /// before this slice, that is the adoption, not the conversation's first message.
    pub created_ms: u64,
    /// Absent until this record has been truncated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<TruncationMarker>,
}

/// `header` as the one line it occupies, newline included.
pub(crate) fn header_line(header: &RecordHeader) -> Result<String, String> {
    let mut line = serde_json::to_string(header)
        .map_err(|err| format!("failed to encode the record header: {err}"))?;
    line.push('\n');
    Ok(line)
}

/// The header `path` carries, or `None` for a record whose first line is not one — every record
/// written before this slice, and every file that does not exist.
pub(crate) fn read_header(path: &Path) -> Option<RecordHeader> {
    let raw = std::fs::read_to_string(path).ok()?;
    parse_header(raw.lines().next()?)
}

/// One line as a record header, or `None` when it is not one.
pub(crate) fn parse_header(line: &str) -> Option<RecordHeader> {
    let header: RecordHeader = serde_json::from_str(line).ok()?;
    (header.kind == RECORD_HEADER_TYPE).then_some(header)
}

/// The millisecond timestamp inside a `msg_` id, or `None` when the id is not one the runtime
/// minted. Message ids are uuid v7 in simple form, so the first 12 hex characters are the mint
/// time — which is what lets a dropped id be placed against a truncation marker without the
/// messages themselves.
pub(crate) fn message_id_timestamp_ms(id: &str) -> Option<u64> {
    let hex = id.strip_prefix("msg_")?;
    if hex.len() < 12 {
        return None;
    }
    u64::from_str_radix(&hex[..12], 16).ok()
}

/// Whether `value` is usable as one directory segment of a record path.
///
/// `field` names where the value came from (`context.record_store`, `--context`), because that is
/// what an operator has to go and change. Refused at staging, before anything is created.
pub(crate) fn validate_record_segment(field: &str, value: &str) -> Result<(), RuntimeError> {
    crate::state_store::validate_store_name(value).map_err(|_| {
        RuntimeError::InvalidConversationRecord {
            field: field.to_string(),
            value: value.to_string(),
            message: SEGMENT_RULE.to_string(),
        }
    })
}

/// The record name a capsule's `context:` block resolves to: what it declared, or the capsule
/// name when it declared nothing. `None` when `context.record: off` — the switch wins over a
/// `record_store` declared beside it.
pub(crate) fn resolve_record_name(
    context: Option<&murmur_artifact::ContextConfig>,
    capsule_name: &str,
) -> Option<String> {
    match context {
        Some(context) if !context.record => None,
        Some(context) => Some(
            context
                .record_store
                .clone()
                .unwrap_or_else(|| capsule_name.to_string()),
        ),
        None => Some(capsule_name.to_string()),
    }
}

/// Where `record`'s conversations live. Resolves only — nothing is created and nothing is checked
/// for existence — so a launch that never appends a message leaves no directory behind.
pub(crate) fn record_root(record: &str) -> Result<PathBuf, String> {
    validate_record_segment("context.record_store", record).map_err(|err| err.to_string())?;
    Ok(crate::state_store::murmur_home_dir()?
        .join(CONVERSATION_ROOT_DIR)
        .join(record))
}

/// The record file one context writes under `root`. Resolves only, like [`record_root`]: nothing
/// is created and nothing is checked for existence.
///
/// `mur run --resume` asks whether this path exists before staging goes any further, which is the
/// one question that separates "continue this conversation" from "start fresh and say nothing".
pub(crate) fn record_file(root: &Path, context_id: &str) -> PathBuf {
    root.join(context_id).join(RECORD_FILE_NAME)
}

/// One context's durable record, and the ids it already holds.
///
/// Held by the agent loop for one attempt. Every message the loop puts in the context goes through
/// [`Self::append`], which is what makes the record the loop's own message list rather than a
/// summary of it.
pub(crate) struct ConversationRecord {
    /// `<home>/.murmur/conversations/<record>/<context-id>`.
    dir: PathBuf,
    /// Session directory the failures are reported into, beside stderr.
    workdir: PathBuf,
    /// The capsule this record's header names, or `None` for a capsule that declares no
    /// `context.retain` — which writes no header at all. See [`Self::ensure_header`].
    capsule: Option<String>,
    /// Whether the header has been settled for this instance. One check per attempt, not one per
    /// message: the header is written or adopted at most once and then never looked at again.
    header_ensured: bool,
    /// Ids already on disk: those loaded from the record and those this attempt appended. A
    /// compaction hook that hands a message back verbatim returns its id with it, and a threaded
    /// reload starts from messages that are already lines — neither may be written twice.
    recorded: HashSet<String>,
    /// A write failure is reported once, then survived silently: one full disk must not produce
    /// one stderr line per message.
    failure_reported: bool,
    /// A malformed line is reported once for this record, however many times it is read.
    malformed_reported: bool,
}

impl ConversationRecord {
    /// The record for `context_id` under `root`, or `None` when the context id is not a usable
    /// path segment.
    ///
    /// A context id arrives from an A2A client as well as from `mur run --context`, so it is
    /// checked here rather than trusted: a rejected id costs that conversation its record and
    /// nothing else.
    pub(crate) fn open(
        root: &Path,
        context_id: &str,
        workdir: &Path,
        capsule: Option<&str>,
    ) -> Option<Self> {
        if crate::state_store::validate_store_name(context_id).is_err() {
            report(
                workdir,
                &format!(
                    "[conversation] context id '{context_id}' {SEGMENT_RULE}; this conversation \
                     is not recorded"
                ),
            );
            return None;
        }
        Some(Self {
            dir: root.join(context_id),
            workdir: workdir.to_path_buf(),
            capsule: capsule.map(str::to_string),
            header_ensured: false,
            recorded: HashSet::new(),
            failure_reported: false,
            malformed_reported: false,
        })
    }

    pub(crate) fn path(&self) -> PathBuf {
        self.dir.join(RECORD_FILE_NAME)
    }

    /// Every message the record already holds, oldest first, and never appended again.
    ///
    /// The agent loop starts a `threaded` task's message list from this. Each message keeps the
    /// `id` its line carries, byte for byte.
    pub(crate) fn load(&mut self) -> Vec<Value> {
        let path = self.path();
        let messages = match read_record(&path, &self.workdir, &mut self.malformed_reported) {
            Ok(messages) => messages,
            Err(reason) => {
                report(
                    &self.workdir,
                    &format!(
                        "[conversation] {} could not be read ({reason}); this task starts with no \
                         prior messages",
                        path.display()
                    ),
                );
                Vec::new()
            }
        };
        for message in &messages {
            if let Some(id) = crate::agent::message_id(message) {
                self.recorded.insert(id.to_string());
            }
        }
        messages
    }

    /// Append every message not already in this record, in the order given.
    ///
    /// Called at each point a message enters the agent loop's context. A message whose id is
    /// already recorded is skipped, which is what a compaction hook returning part of the context
    /// verbatim produces.
    ///
    /// An id joins `recorded` only once its line is on disk. A failure the caller survives — a
    /// disk that fills and is freed again, a descriptor limit that passes — is followed by an
    /// append carrying the same messages, and a message marked as written before its write
    /// succeeded would be skipped by that retry and lost from the record for good.
    pub(crate) fn append(&mut self, messages: &[Value]) {
        for message in messages {
            let id = crate::agent::message_id(message).map(str::to_string);
            if let Some(id) = id.as_deref() {
                if self.recorded.contains(id) {
                    continue;
                }
            }
            if let Err(reason) = self.append_line(message) {
                if !self.failure_reported {
                    self.failure_reported = true;
                    report(
                        &self.workdir,
                        &format!(
                            "[conversation] {} could not be written ({reason}); this task \
                             continues unrecorded",
                            self.path().display()
                        ),
                    );
                }
                return;
            }
            if let Some(id) = id {
                self.recorded.insert(id);
            }
        }
    }

    /// Create the directory chain if it is missing and append one line.
    ///
    /// Creation happens here rather than at staging so `context.record: off`, and a launch that
    /// never reaches a message, both leave nothing behind.
    fn append_line(&mut self, message: &Value) -> Result<(), String> {
        let mut line = serde_json::to_string(message)
            .map_err(|err| format!("failed to encode the message: {err}"))?;
        line.push('\n');

        self.ensure_dirs()?;
        self.ensure_header()?;

        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.path())
            .and_then(|mut file| file.write_all(line.as_bytes()))
            .map_err(|err| err.to_string())
    }

    /// Create the record root, the record directory and the context directory if they are
    /// missing, oldest ancestor first, so a context directory is never reachable through a parent
    /// a wider mode left open.
    fn ensure_dirs(&self) -> Result<(), String> {
        let mut chain = Vec::new();
        let mut dir = Some(self.dir.as_path());
        for _ in 0..3 {
            if let Some(current) = dir {
                chain.push(current);
                dir = current.parent();
            }
        }
        for path in chain.into_iter().rev() {
            crate::state_store::ensure_private_dir(path, RECORD_DIR_MODE)
                .map_err(|reason| format!("{}: {reason}", path.display()))?;
        }
        Ok(())
    }

    /// Put a [`RecordHeader`] at the front of this record, once, before its first new line.
    ///
    /// A capsule that declares no `context.retain` writes no header: there is nothing to own a
    /// record for, and a record that gains a line on upgrade is a behaviour change an operator
    /// did not ask for. Everything below is therefore conditional on [`Self::capsule`].
    ///
    /// Three cases, and the third is the whole reason this is not just a write:
    ///
    /// * A record that does not exist yet gets its header as its first line.
    /// * A record that already carries one is left exactly as it is.
    /// * A record written before the header existed is **adopted**: the header is prepended
    ///   through the same atomic temp-and-rename rewrite a truncation uses, so a crash leaves the
    ///   original whole. Until that adoption the record is unowned and automatic pruning skips
    ///   it, however old it is.
    ///
    /// Adoption is what makes retention apply to a pre-slice record without ever guessing at
    /// ownership: the capsule that writes to a record is the capsule that owns it.
    fn ensure_header(&mut self) -> Result<(), String> {
        if self.header_ensured {
            return Ok(());
        }
        let Some(capsule) = self.capsule.clone() else {
            self.header_ensured = true;
            return Ok(());
        };
        let path = self.path();
        let existing = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(err) => return Err(format!("{}: {err}", path.display())),
        };
        if existing.lines().next().and_then(parse_header).is_some() {
            self.header_ensured = true;
            return Ok(());
        }

        let header = RecordHeader {
            kind: RECORD_HEADER_TYPE.to_string(),
            capsule,
            created_ms: crate::retention::now_ms(),
            truncated: None,
        };
        let line = header_line(&header)?;
        // The flag is set only once the header is on disk: a failure the caller survives is
        // followed by an append carrying the same messages, and a record marked as headered
        // before its header was written would take those messages headerless and stay unowned.
        if existing.is_empty() {
            std::fs::write(&path, line).map_err(|err| format!("{}: {err}", path.display()))?;
        } else {
            let mut contents = line;
            contents.push_str(&existing);
            crate::retention::StagedRewrite::stage(&path, contents.as_bytes())?.commit()?;
        }
        self.header_ensured = true;
        Ok(())
    }
}

/// The parsed record one reader holds between pages.
///
/// A hook pages a record it does not shrink and cannot write, so re-reading and re-parsing the
/// whole file for each page makes a walk quadratic in a file designed to grow without bound. One
/// cache serves a whole launch: [`crate::conversation_import::ConversationState`] holds it across
/// every task, which is why the key carries the path as well as the length.
#[derive(Default)]
pub(crate) struct RecordCache {
    /// Record the messages below were parsed from, and `None` while no parse is trusted.
    path: Option<PathBuf>,
    /// Length of that file at the moment it was stat'd for the parse below.
    len: u64,
    /// The record, oldest first, as [`read_record`] returned it.
    messages: Vec<Value>,
    /// A malformed line is reported once for this reader, however many pages it walks.
    malformed_reported: bool,
    /// Reads of the record file that reached the filesystem. What pins the cost of a paging loop.
    #[cfg(test)]
    reads: u32,
}

impl RecordCache {
    /// The whole record, oldest first, parsed at most once per length the file has held.
    ///
    /// The runtime is the record's only writer and only ever appends, so a file whose length is
    /// unchanged holds the same bytes and the parse above it still stands. A missing file is an
    /// empty record of length 0, exactly as a read of one is.
    fn messages(&mut self, path: &Path, workdir: &Path) -> Result<&[Value], String> {
        let len = match std::fs::metadata(path) {
            Ok(metadata) => metadata.len(),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => 0,
            Err(err) => return Err(err.to_string()),
        };
        if self.path.as_deref() == Some(path) && self.len == len {
            return Ok(&self.messages);
        }

        // Cleared before the read and set again only after the messages are in place, so a read
        // that fails leaves a cache that re-reads rather than one record's messages under
        // another's key.
        self.path = None;
        #[cfg(test)]
        {
            self.reads += 1;
        }
        self.messages = read_record(path, workdir, &mut self.malformed_reported)?;
        self.len = len;
        self.path = Some(path.to_path_buf());
        Ok(&self.messages)
    }

    #[cfg(test)]
    pub(crate) fn reads(&self) -> u32 {
        self.reads
    }
}

/// One page of a record, newest first, as `murmur:conversation/read` serves it.
///
/// `path` is `None` for a session that writes no record — `context.record: off`, a
/// `process`-transport capsule, or a task whose context id was refused — and reads as an empty
/// page rather than an error: there is nothing wrong with a conversation that has said nothing
/// yet. That case leaves `cache` alone, so a hook dispatched between two tasks does not cost the
/// next one its parse.
pub(crate) fn page(
    cache: &mut RecordCache,
    path: Option<&Path>,
    workdir: &Path,
    cursor: Option<&str>,
    limit: u32,
) -> Result<MessagePage, String> {
    let Some(path) = path else {
        return Ok(empty_page());
    };
    let messages = cache
        .messages(path, workdir)
        .map_err(|reason| format!("unavailable: {reason}"))?;

    // A cursor is the exclusive upper bound of the next page, counted from the oldest message, so
    // it stays valid while the runtime appends to the record underneath a paging hook.
    let end = match cursor {
        None => messages.len(),
        Some(cursor) => decode_cursor(cursor)
            .filter(|end| *end <= messages.len())
            .ok_or_else(|| "invalid-cursor".to_string())?,
    };
    let limit = limit.clamp(1, MAX_PAGE_LIMIT) as usize;
    let start = end.saturating_sub(limit);

    let mut window: Vec<Value> = messages[start..end].to_vec();
    window.reverse();
    // `murmur:conversation/read` re-uses `murmur:hook/lifecycle`'s `message`, but bindgen
    // generates the imported and exported views of it as two Rust types. The lowering itself is
    // `to_wit_messages`, so a "tool" message reaches a reader folded exactly as it reaches a
    // compaction hook.
    Ok(MessagePage {
        messages: crate::agent::to_wit_messages(&window)
            .into_iter()
            .map(|message| Message {
                role: message.role,
                content: message.content,
                id: message.id,
                source_id: message.source_id,
            })
            .collect(),
        next_cursor: (start > 0).then(|| encode_cursor(start)),
        total: u32::try_from(messages.len()).unwrap_or(u32::MAX),
    })
}

/// What a record with nothing in it reads as.
fn empty_page() -> MessagePage {
    MessagePage {
        messages: Vec::new(),
        next_cursor: None,
        total: 0,
    }
}

/// Every message in `path`, oldest first.
///
/// The single parser both readers go through — the `murmur:conversation/read` page and the
/// `threaded` reload — so a line the hook can see is a line the next task will load. A line that
/// is not a JSON object with a string `role` is skipped; a truncated final line is exactly that
/// case. A missing file is an empty record, not an error.
///
/// `reported` is the reader's own once-only flag: the first skipped line it meets is reported
/// naming the path and the 1-based line number, and every line after it, in this read and in the
/// reader's later ones, is silent. One such line is worth one log entry however many times the
/// reader goes back to the file.
fn read_record(path: &Path, workdir: &Path, reported: &mut bool) -> Result<Vec<Value>, String> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.to_string()),
    };

    let mut messages = Vec::new();
    for (index, line) in raw.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match parse_message_line(line) {
            Some(message) => messages.push(message),
            None => {
                if !*reported {
                    *reported = true;
                    report(
                        workdir,
                        &format!(
                            "[conversation] {}: line {} is not a message and was skipped",
                            path.display(),
                            index + 1
                        ),
                    );
                }
            }
        }
    }
    Ok(messages)
}

/// One record line as a message, or `None` when it is not one. A message is a JSON object
/// carrying a string `role`: everything the runtime writes is, and nothing else can be lowered
/// into the WIT `message` record. The header line is the deliberate non-message — it is how a
/// truncation marker rides in the file without being visible to anything that reads messages.
pub(crate) fn parse_message_line(line: &str) -> Option<Value> {
    let value: Value = serde_json::from_str(line).ok()?;
    value
        .get("role")
        .and_then(Value::as_str)
        .is_some()
        .then_some(value)
}

fn encode_cursor(position: usize) -> String {
    format!("{CURSOR_PREFIX}{position}")
}

fn decode_cursor(cursor: &str) -> Option<usize> {
    cursor.strip_prefix(CURSOR_PREFIX)?.parse().ok()
}

/// Both places an operator looks for what the runtime did with a record. Never fatal.
fn report(workdir: &Path, message: &str) {
    eprintln!("[capsule-runtime] {message}");
    crate::agent::append_bootstrap_log(workdir, message);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The capsule every record below is opened for, and therefore the name its header carries.
    const CAPSULE: &str = "capsule";

    fn message(id: &str, role: &str, text: &str) -> Value {
        json!({"role": role, "content": [{"type": "text", "text": text}], "id": id})
    }

    fn id_at(id: usize) -> String {
        format!("msg_{:032x}", id)
    }

    /// The header is the first line a new record gets, and it names the capsule that wrote it —
    /// which is what makes automatic pruning store-safe when two capsules share one record store.
    #[test]
    fn the_first_append_writes_a_header_naming_the_capsule() {
        let home = tempfile::tempdir().unwrap();
        let workdir = tempfile::tempdir().unwrap();
        let root = home.path().join("conversations/shared");

        let mut record =
            ConversationRecord::open(&root, "ctx_1", workdir.path(), Some(CAPSULE)).unwrap();
        record.append(&[message(&id_at(1), "user", "hello")]);

        let raw = std::fs::read_to_string(record.path()).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 2, "a header and one message: {lines:?}");
        let header = parse_header(lines[0]).expect("the first line is a header");
        assert_eq!(header.kind, RECORD_HEADER_TYPE);
        assert_eq!(header.capsule, CAPSULE);
        assert!(header.truncated.is_none(), "nothing has been dropped yet");
        assert!(header.created_ms > 0);
    }

    /// The header is invisible to everything that reads messages: `read_record` skips it, so the
    /// `total` a `murmur:conversation/read` page reports counts messages and nothing else.
    #[test]
    fn a_header_line_changes_neither_the_loaded_messages_nor_total() {
        let home = tempfile::tempdir().unwrap();
        let workdir = tempfile::tempdir().unwrap();
        let root = home.path().join("conversations/c");

        let mut record =
            ConversationRecord::open(&root, "ctx_1", workdir.path(), Some(CAPSULE)).unwrap();
        record.append(&[
            message(&id_at(1), "user", "one"),
            message(&id_at(2), "assistant", "two"),
        ]);
        let path = record.path();

        let mut reader =
            ConversationRecord::open(&root, "ctx_1", workdir.path(), Some(CAPSULE)).unwrap();
        let loaded = reader.load();
        assert_eq!(loaded.len(), 2, "the header is not a message: {loaded:?}");

        let mut cache = RecordCache::default();
        let page = page(&mut cache, Some(&path), workdir.path(), None, 100).unwrap();
        assert_eq!(page.total, 2);
        assert_eq!(page.messages.len(), 2);
    }

    /// A record written before the header existed is unowned, and is adopted on the next append
    /// by the capsule that writes to it — through the same atomic rewrite a truncation uses, so
    /// every line it already held survives byte for byte.
    #[test]
    fn a_headerless_record_is_adopted_on_the_next_append() {
        let home = tempfile::tempdir().unwrap();
        let workdir = tempfile::tempdir().unwrap();
        let root = home.path().join("conversations/c");
        let dir = root.join("ctx_1");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(RECORD_FILE_NAME);
        let pre_slice = format!(
            "{}
{}
",
            serde_json::to_string(&message(&id_at(1), "user", "one")).unwrap(),
            serde_json::to_string(&message(&id_at(2), "assistant", "two")).unwrap()
        );
        std::fs::write(&path, &pre_slice).unwrap();
        assert!(read_header(&path).is_none(), "unowned before the append");

        let mut record =
            ConversationRecord::open(&root, "ctx_1", workdir.path(), Some(CAPSULE)).unwrap();
        record.append(&[message(&id_at(3), "user", "three")]);

        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(read_header(&path).unwrap().capsule, CAPSULE);
        assert!(
            raw.contains(pre_slice.trim_end()),
            "every pre-slice line survives byte for byte: {raw}"
        );
        assert_eq!(raw.lines().count(), 4, "a header and three messages");
    }

    /// A record already carrying a header is left exactly as it is — the header is written once,
    /// not once per launch, and a truncation marker on it is never cleared by an append.
    #[test]
    fn an_owned_record_keeps_the_header_it_already_has() {
        let home = tempfile::tempdir().unwrap();
        let workdir = tempfile::tempdir().unwrap();
        let root = home.path().join("conversations/c");
        let dir = root.join("ctx_1");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(RECORD_FILE_NAME);
        let header = RecordHeader {
            kind: RECORD_HEADER_TYPE.to_string(),
            capsule: "someone-else".to_string(),
            created_ms: 1_756_400_000_000,
            truncated: Some(TruncationMarker {
                dropped: 5,
                oldest_surviving_id: id_at(6),
                last_dropped_id: id_at(5),
                at_ms: 1_756_400_000_001,
            }),
        };
        std::fs::write(&path, header_line(&header).unwrap()).unwrap();

        let mut record =
            ConversationRecord::open(&root, "ctx_1", workdir.path(), Some(CAPSULE)).unwrap();
        record.append(&[message(&id_at(7), "user", "seven")]);

        assert_eq!(read_header(&path).unwrap(), header);
    }

    /// A record written by one session and read back by the next: the ids are the same bytes, and
    /// nothing loaded is appended a second time.
    #[test]
    fn a_reloaded_message_keeps_its_id_and_is_not_written_twice() {
        let home = tempfile::tempdir().unwrap();
        let workdir = tempfile::tempdir().unwrap();
        let root = home.path().join("conversations/capsule");

        let mut first =
            ConversationRecord::open(&root, "ctx_fixed", workdir.path(), Some(CAPSULE)).unwrap();
        first.append(&[message(&id_at(1), "user", "hello")]);

        let mut second =
            ConversationRecord::open(&root, "ctx_fixed", workdir.path(), Some(CAPSULE)).unwrap();
        let loaded = second.load();
        assert_eq!(loaded.len(), 1);
        assert_eq!(
            crate::agent::message_id(&loaded[0]),
            Some(id_at(1).as_str())
        );

        second.append(&loaded);
        second.append(&[message(&id_at(2), "assistant", "hi")]);
        assert_eq!(
            second.load().len(),
            2,
            "a reloaded message is never re-appended"
        );
    }

    /// Every directory on the record path is owner-only, and none of them exists before the first
    /// message is appended.
    #[test]
    fn the_directory_chain_is_created_at_0700_on_first_write() {
        use std::os::unix::fs::PermissionsExt;

        let home = tempfile::tempdir().unwrap();
        let workdir = tempfile::tempdir().unwrap();
        let conversations = home.path().join("conversations");
        let root = conversations.join("capsule");

        let mut record =
            ConversationRecord::open(&root, "ctx_1", workdir.path(), Some(CAPSULE)).unwrap();
        assert!(!conversations.exists(), "opening a record creates nothing");

        record.append(&[message(&id_at(1), "user", "hello")]);
        for dir in [&conversations, &root, &root.join("ctx_1")] {
            let mode = std::fs::metadata(dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                mode,
                0o700,
                "{} must be 0700, got {mode:04o}",
                dir.display()
            );
        }
    }

    /// A context id that is not one path segment costs that conversation its record, and refuses
    /// before any path is built from it.
    #[test]
    fn a_context_id_that_is_not_a_segment_records_nothing() {
        let home = tempfile::tempdir().unwrap();
        let workdir = tempfile::tempdir().unwrap();
        for context_id in ["../escape", "a/b", "", ".hidden"] {
            assert!(
                ConversationRecord::open(
                    &home.path().join("conversations/c"),
                    context_id,
                    workdir.path(),
                    Some(CAPSULE)
                )
                .is_none(),
                "'{context_id}' must not open a record"
            );
        }
        assert!(!home.path().join("conversations").exists());
    }

    /// Newest first, paged, with a cursor that neither repeats nor skips a message.
    #[test]
    fn paging_walks_the_record_newest_first_without_repeating_a_message() {
        let home = tempfile::tempdir().unwrap();
        let workdir = tempfile::tempdir().unwrap();
        let root = home.path().join("conversations/capsule");
        let mut record =
            ConversationRecord::open(&root, "ctx_1", workdir.path(), Some(CAPSULE)).unwrap();
        let written: Vec<Value> = (1..=5)
            .map(|n| message(&id_at(n), "user", &format!("m{n}")))
            .collect();
        record.append(&written);
        let path = record.path();

        let mut seen = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = page(
                &mut RecordCache::default(),
                Some(&path),
                workdir.path(),
                cursor.as_deref(),
                2,
            )
            .unwrap();
            assert_eq!(page.total, 5);
            seen.extend(page.messages.iter().map(|m| m.id.clone().unwrap()));
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        assert_eq!(
            seen,
            (1..=5).rev().map(id_at).collect::<Vec<_>>(),
            "the whole record, newest first, exactly once each"
        );
    }

    /// `limit` is the host's to bound: `0` still makes progress and a huge value cannot ask for
    /// more than the ceiling.
    #[test]
    fn limit_is_clamped_at_both_ends() {
        let home = tempfile::tempdir().unwrap();
        let workdir = tempfile::tempdir().unwrap();
        let root = home.path().join("conversations/capsule");
        let mut record =
            ConversationRecord::open(&root, "ctx_1", workdir.path(), Some(CAPSULE)).unwrap();
        record.append(
            &(1..=120)
                .map(|n| message(&id_at(n), "user", "m"))
                .collect::<Vec<_>>(),
        );
        let path = record.path();

        assert_eq!(
            page(
                &mut RecordCache::default(),
                Some(&path),
                workdir.path(),
                None,
                0
            )
            .unwrap()
            .messages
            .len(),
            1
        );
        assert_eq!(
            page(
                &mut RecordCache::default(),
                Some(&path),
                workdir.path(),
                None,
                u32::MAX
            )
            .unwrap()
            .messages
            .len(),
            MAX_PAGE_LIMIT as usize
        );
    }

    /// A value that is not `mc_<number>`, and a position past the end of the record, are both
    /// `invalid-cursor` rather than a page.
    #[test]
    fn a_foreign_cursor_is_refused() {
        let home = tempfile::tempdir().unwrap();
        let workdir = tempfile::tempdir().unwrap();
        let root = home.path().join("conversations/capsule");
        let mut record =
            ConversationRecord::open(&root, "ctx_1", workdir.path(), Some(CAPSULE)).unwrap();
        record.append(&[message(&id_at(1), "user", "m")]);
        let path = record.path();

        for cursor in ["7", "mc_", "mc_nine", "mc_9", "cur_1"] {
            assert_eq!(
                page(
                    &mut RecordCache::default(),
                    Some(&path),
                    workdir.path(),
                    Some(cursor),
                    10
                )
                .err(),
                Some("invalid-cursor".to_string()),
                "cursor '{cursor}'"
            );
        }
    }

    /// A record that was never written, and a session with no record at all, are both an empty
    /// page — never an error.
    #[test]
    fn an_absent_record_is_an_empty_page() {
        let home = tempfile::tempdir().unwrap();
        let workdir = tempfile::tempdir().unwrap();
        let missing = home
            .path()
            .join("conversations/capsule/ctx_1/conversation.jsonl");

        for path in [None, Some(missing.as_path())] {
            let page = page(&mut RecordCache::default(), path, workdir.path(), None, 10).unwrap();
            assert!(page.messages.is_empty());
            assert_eq!(page.next_cursor, None);
            assert_eq!(page.total, 0);
        }
    }

    /// A read for a session that keeps no record leaves the cache as it found it, so a hook
    /// dispatched between two tasks does not cost the next one its parse.
    #[test]
    fn a_read_with_no_record_leaves_the_cache_alone() {
        let home = tempfile::tempdir().unwrap();
        let workdir = tempfile::tempdir().unwrap();
        let root = home.path().join("conversations/capsule");
        let mut record =
            ConversationRecord::open(&root, "ctx_1", workdir.path(), Some(CAPSULE)).unwrap();
        record.append(&[message(&id_at(1), "user", "a")]);
        let path = record.path();

        let mut cache = RecordCache::default();
        assert_eq!(
            page(&mut cache, Some(&path), workdir.path(), None, 10)
                .unwrap()
                .total,
            1
        );
        assert!(page(&mut cache, None, workdir.path(), None, 10)
            .unwrap()
            .messages
            .is_empty());
        assert_eq!(
            page(&mut cache, Some(&path), workdir.path(), None, 10)
                .unwrap()
                .total,
            1
        );
        assert_eq!(cache.reads(), 1, "the parse survived the empty read");
    }

    /// A malformed line is skipped, reported once naming the path and its 1-based number, and
    /// costs the read nothing else.
    #[test]
    fn a_malformed_line_is_skipped_and_reported() {
        let home = tempfile::tempdir().unwrap();
        let workdir = tempfile::tempdir().unwrap();
        let dir = home.path().join("conversations/capsule/ctx_1");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(RECORD_FILE_NAME);
        let mut raw = String::new();
        for n in 1..=3 {
            raw.push_str(&serde_json::to_string(&message(&id_at(n), "user", "m")).unwrap());
            raw.push('\n');
        }
        raw.push_str("{\"role\":\"user\"");
        std::fs::write(&path, raw).unwrap();

        let page = page(
            &mut RecordCache::default(),
            Some(&path),
            workdir.path(),
            None,
            10,
        )
        .unwrap();
        assert_eq!(page.total, 3);
        assert_eq!(page.messages.len(), 3);

        let log =
            std::fs::read_to_string(workdir.path().join("logs/bootstrap.log")).unwrap_or_default();
        assert_eq!(
            log.lines()
                .filter(|line| line.contains("line 4 is not a message"))
                .count(),
            1,
            "reported once, naming the line: {log}"
        );
    }

    /// A write that fails and then can succeed leaves the record whole: the message the failure
    /// cost is written by the next append that carries it, in its own order.
    ///
    /// The failure is a regular file sitting where the context directory has to go, which no
    /// permission bit is involved in — the suite reads the same as root and as anyone else.
    #[test]
    fn a_failed_write_is_retried_and_leaves_no_hole() {
        let home = tempfile::tempdir().unwrap();
        let workdir = tempfile::tempdir().unwrap();
        let root = home.path().join("conversations/capsule");
        let blocker = root.join("ctx_1");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&blocker, "").unwrap();

        let mut record =
            ConversationRecord::open(&root, "ctx_1", workdir.path(), Some(CAPSULE)).unwrap();
        let first = message(&id_at(1), "user", "a");
        let second = message(&id_at(2), "assistant", "b");
        record.append(std::slice::from_ref(&first));
        assert!(
            blocker.is_file(),
            "the directory chain could not be created"
        );

        std::fs::remove_file(&blocker).unwrap();
        record.append(&[first, second]);

        let written: Vec<Value> = std::fs::read_to_string(record.path())
            .unwrap()
            .lines()
            .filter_map(parse_message_line)
            .collect();
        assert_eq!(
            written
                .iter()
                .map(|m| crate::agent::message_id(m).unwrap())
                .collect::<Vec<_>>(),
            vec![id_at(1), id_at(2)],
            "the message the failure cost is on disk, before the one that followed it"
        );
    }

    /// A whole paging walk costs one read of the record and one parse of it.
    #[test]
    fn a_paging_loop_reads_the_record_once() {
        let home = tempfile::tempdir().unwrap();
        let workdir = tempfile::tempdir().unwrap();
        let root = home.path().join("conversations/capsule");
        let mut record =
            ConversationRecord::open(&root, "ctx_1", workdir.path(), Some(CAPSULE)).unwrap();
        record.append(
            &(1..=250)
                .map(|n| message(&id_at(n), "user", "m"))
                .collect::<Vec<_>>(),
        );
        let path = record.path();
        let mut cache = RecordCache::default();

        let mut pages = 0;
        let mut seen = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = page(
                &mut cache,
                Some(&path),
                workdir.path(),
                cursor.as_deref(),
                25,
            )
            .unwrap();
            pages += 1;
            seen.extend(page.messages.iter().map(|m| m.id.clone().unwrap()));
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        assert_eq!(pages, 10);
        assert_eq!(seen, (1..=250).rev().map(id_at).collect::<Vec<_>>());
        assert_eq!(cache.reads(), 1, "one read for the whole walk");

        // The record grows underneath the same reader: the next walk sees the new messages and
        // pays for exactly one more read.
        record.append(
            &(251..=260)
                .map(|n| message(&id_at(n), "user", "m"))
                .collect::<Vec<_>>(),
        );
        let mut seen = Vec::new();
        let mut cursor: Option<String> = None;
        let mut first_total = None;
        loop {
            let page = page(
                &mut cache,
                Some(&path),
                workdir.path(),
                cursor.as_deref(),
                25,
            )
            .unwrap();
            first_total.get_or_insert(page.total);
            seen.extend(page.messages.iter().map(|m| m.id.clone().unwrap()));
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        assert_eq!(first_total, Some(260));
        assert_eq!(seen, (1..=260).rev().map(id_at).collect::<Vec<_>>());
        assert_eq!(cache.reads(), 2, "one further read for the second walk");
    }

    /// One truncated line is one log entry, however many pages are walked over it.
    #[test]
    fn a_malformed_line_is_reported_once_for_a_whole_walk() {
        let home = tempfile::tempdir().unwrap();
        let workdir = tempfile::tempdir().unwrap();
        let dir = home.path().join("conversations/capsule/ctx_1");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(RECORD_FILE_NAME);
        let mut raw = String::new();
        for n in 1..=10 {
            raw.push_str(&serde_json::to_string(&message(&id_at(n), "user", "m")).unwrap());
            raw.push('\n');
        }
        raw.push_str("{\"role\":\"user\"");
        std::fs::write(&path, raw).unwrap();

        let mut cache = RecordCache::default();
        let mut cursor: Option<String> = None;
        let mut pages = 0;
        loop {
            let page = page(
                &mut cache,
                Some(&path),
                workdir.path(),
                cursor.as_deref(),
                2,
            )
            .unwrap();
            pages += 1;
            assert_eq!(page.total, 10);
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        assert_eq!(pages, 5);

        let log =
            std::fs::read_to_string(workdir.path().join("logs/bootstrap.log")).unwrap_or_default();
        assert_eq!(
            log.lines()
                .filter(|line| line.contains("line 11 is not a message"))
                .count(),
            1,
            "one entry for the whole walk: {log}"
        );
    }

    /// `context.record: off` resolves to no record name at all, whatever `record_store` says
    /// beside it.
    #[test]
    fn record_off_wins_over_a_declared_store() {
        let off = murmur_artifact::ContextConfig {
            max_tokens: None,
            record: false,
            record_store: Some("shey".to_string()),
            seed_budget: murmur_artifact::DEFAULT_SEED_BUDGET,
            seed_overflow_margin: murmur_artifact::DEFAULT_SEED_OVERFLOW_MARGIN,
            retain: None,
        };
        assert_eq!(resolve_record_name(Some(&off), "capsule"), None);

        let on = murmur_artifact::ContextConfig {
            record: true,
            ..off.clone()
        };
        assert_eq!(
            resolve_record_name(Some(&on), "capsule"),
            Some("shey".to_string())
        );
        assert_eq!(
            resolve_record_name(None, "capsule"),
            Some("capsule".to_string()),
            "a capsule declaring no context block still records"
        );
    }

    /// Every refusal names the value the operator wrote and the rule it broke.
    #[test]
    fn a_malformed_record_name_is_refused_by_name() {
        for value in ["", "../escape", "/abs", "a/b", ".hidden"] {
            let err = validate_record_segment("context.record_store", value)
                .expect_err("'{value}' must be refused");
            let message = err.to_string();
            assert!(message.contains(&format!("'{value}'")), "{message}");
            assert!(message.contains("single path segment"), "{message}");
            assert!(message.contains("context.record_store"), "{message}");
        }
        validate_record_segment("--context", "ctx_fixed").unwrap();
    }
}
