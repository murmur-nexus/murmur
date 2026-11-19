//! `mur trace attach`, and the attachments `mur trace verify` lists.
//!
//! An attachment is an outside party's judgment of a sealed record: one JSON file under
//! `<records dir>/attachments/`, written beside the record and never into it. It names the head of
//! `trace.jsonl.seal` that verified when it was written, so `mur trace verify` can say later
//! whether it still points at the record that verifies. It is not signed: anyone who can write the
//! records directory can add or edit one, and nothing here vouches for its value or evaluator.

use std::{
    io::Read,
    path::{Path, PathBuf},
};

use capsule_runtime::record_seal::{self, RecordReport, Seal, Verdict, TRACE_STREAM};
use capsule_runtime::workdir_fs::DEFAULT_FILE_MODE;
use capsule_runtime::{EntryKind, WorkdirRoot};
use serde::{Deserialize, Serialize};

use crate::commands::trace::{
    records_dir_of, resolve_session, stream_finding, verdict_code, verify_records, worst_stream,
    E_TRC_003, E_TRC_006, E_TRC_007,
};
use crate::error::CliError;

/// The `format` every attachment file carries.
pub(crate) const ATTACHMENT_FORMAT: &str = "murmur.attachment/1";

/// The directory of a records directory that holds its attachments.
pub(crate) const ATTACHMENTS_DIR: &str = "attachments";

/// The largest attachment file, and the largest value, read: the bound `record_seal::read_seal`
/// reads a seal with.
pub(crate) const ATTACHMENT_READ_BOUND: u64 = 1 << 26;

/// The longest value `mur trace verify` prints inline; a longer one is shown by its size.
const INLINE_VALUE_CHARS: usize = 120;

/// One `attachments/<attachment_id>.json` file. Field order is the serialized key order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Attachment {
    /// [`ATTACHMENT_FORMAT`].
    pub(crate) format: String,
    /// `att_` + a UUID v7 in simple form; also the file's stem.
    pub(crate) attachment_id: String,
    pub(crate) session_id: String,
    pub(crate) evaluator: String,
    /// Exactly the bytes given, as UTF-8. Never parsed.
    pub(crate) value: String,
    /// Event ids of the judged trace, each one present in `trace.jsonl` when written.
    pub(crate) events: Vec<String>,
    /// Milliseconds since the epoch, set by `mur`.
    pub(crate) attached_at: u64,
    pub(crate) record: AttachedHead,
}

/// The sealed head of the record an attachment judges, copied from `trace.jsonl.seal`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AttachedHead {
    /// [`TRACE_STREAM`].
    pub(crate) stream: String,
    pub(crate) head: String,
    pub(crate) lines: u64,
    pub(crate) key_id: String,
}

/// One entry of `attachments/` as `mur trace verify` found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ListedAttachment {
    Read {
        /// `attachments/<name>`, relative to the records directory.
        file: String,
        attachment: Box<Attachment>,
    },
    Unreadable {
        file: String,
        reason: String,
    },
}

impl ListedAttachment {
    fn file(&self) -> &str {
        match self {
            Self::Read { file, .. } | Self::Unreadable { file, .. } => file,
        }
    }
}

/// What an attachment says about the record `mur trace verify` checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum AttachmentStatus {
    /// The record verified and the attachment names its sealed head.
    Matches,
    /// The record verified and the attachment names another head.
    Mismatch,
    /// The record did not verify, so no head is trusted to compare with.
    Unverified,
    /// The file could not be read as an attachment.
    Unreadable,
}

impl AttachmentStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Matches => "matches",
            Self::Mismatch => "mismatch",
            Self::Unverified => "unverified",
            Self::Unreadable => "unreadable",
        }
    }
}

/// An attachment's status and the sentence explaining it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AttachmentCheck {
    pub(crate) status: AttachmentStatus,
    pub(crate) detail: String,
}

/// One entry of `mur trace verify --json`'s `attachments` array. `null` stands for what an
/// unreadable entry lacks.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct AttachmentEntry {
    pub(crate) file: String,
    pub(crate) attachment_id: Option<String>,
    pub(crate) evaluator: Option<String>,
    pub(crate) value: Option<String>,
    pub(crate) events: Option<Vec<String>>,
    pub(crate) attached_at: Option<u64>,
    pub(crate) record_head: Option<String>,
    pub(crate) status: AttachmentStatus,
    pub(crate) detail: String,
}

/// `mur trace verify --json`: the record report with `attachments` and `bundle` appended.
#[derive(Serialize)]
pub(crate) struct VerifyOutput<'a> {
    #[serde(flatten)]
    pub(crate) report: &'a RecordReport,
    pub(crate) attachments: &'a [AttachmentEntry],
    /// The bundle checks with `--bundle`; `null` without.
    pub(crate) bundle: Option<&'a crate::commands::trace_export::BundleReport>,
}

/// `mur trace attach --json`: the attachment's keys, then the file written.
#[derive(Serialize)]
struct AttachOutput<'a> {
    #[serde(flatten)]
    attachment: &'a Attachment,
    path: String,
}

/// `mur trace attach`'s arguments.
pub(crate) struct AttachRequest {
    pub(crate) session: String,
    pub(crate) evaluator: String,
    pub(crate) value: Option<String>,
    /// `-` reads standard input.
    pub(crate) value_file: Option<PathBuf>,
    pub(crate) events: Vec<String>,
    pub(crate) workdir: Option<PathBuf>,
    pub(crate) public_key: Option<PathBuf>,
    pub(crate) json: bool,
}

/// A fresh attachment id: `att_` + a UUID v7 in simple form.
pub(crate) fn new_attachment_id() -> String {
    format!("att_{}", uuid::Uuid::now_v7().simple())
}

/// `attachments/<attachment_id>.json`.
pub(crate) fn attachment_rel(attachment_id: &str) -> String {
    format!("{ATTACHMENTS_DIR}/{attachment_id}.json")
}

/// Writes `attachment` as `attachments/<attachment_id>.json` beneath `records`, creating
/// `attachments/` when absent. Nothing else in `records` is written.
pub(crate) fn write_attachment(
    records: &WorkdirRoot,
    attachment: &Attachment,
) -> Result<(), String> {
    let mut body = serde_json::to_string_pretty(attachment).map_err(|error| error.to_string())?;
    body.push('\n');
    records
        .write_file(
            attachment_rel(&attachment.attachment_id),
            body.as_bytes(),
            DEFAULT_FILE_MODE,
        )
        .map_err(|error| error.to_string())
}

/// Every `*.json` entry of `attachments/` beneath `records`, ordered by `attached_at` then
/// `attachment_id`, unreadable entries last by file name. An absent `attachments/` holds none;
/// entries not named `*.json` and directories are skipped.
pub(crate) fn list_attachments(records: &WorkdirRoot) -> Vec<ListedAttachment> {
    let entries = match records.list_dir(ATTACHMENTS_DIR) {
        Ok(Some(entries)) => entries,
        Ok(None) => return Vec::new(),
        Err(error) if error.is_not_found() => return Vec::new(),
        Err(error) => {
            return vec![ListedAttachment::Unreadable {
                file: ATTACHMENTS_DIR.to_string(),
                reason: format!("{ATTACHMENTS_DIR} cannot be listed: {error}"),
            }]
        }
    };
    let mut read = Vec::new();
    let mut unreadable = Vec::new();
    for (name, kind) in entries {
        if !name.ends_with(".json") || kind == EntryKind::Directory {
            continue;
        }
        let file = format!("{ATTACHMENTS_DIR}/{name}");
        match read_attachment(records, &file, kind) {
            Ok(attachment) => read.push(ListedAttachment::Read {
                file,
                attachment: Box::new(attachment),
            }),
            Err(reason) => unreadable.push(ListedAttachment::Unreadable {
                reason: format!("{file} {reason}"),
                file,
            }),
        }
    }
    read.sort_by(|a, b| match (a, b) {
        (
            ListedAttachment::Read { attachment: a, .. },
            ListedAttachment::Read { attachment: b, .. },
        ) => (a.attached_at, &a.attachment_id).cmp(&(b.attached_at, &b.attachment_id)),
        _ => std::cmp::Ordering::Equal,
    });
    unreadable.sort_by(|a, b| a.file().cmp(b.file()));
    read.extend(unreadable);
    read
}

/// The attachment at `file`, or why it is unreadable, phrased to follow the file name.
fn read_attachment(
    records: &WorkdirRoot,
    file: &str,
    kind: EntryKind,
) -> Result<Attachment, String> {
    match kind {
        EntryKind::File => {}
        EntryKind::Symlink => return Err("is a link".to_string()),
        _ => return Err("is not a regular file".to_string()),
    }
    let bytes = match records.read_file(file, ATTACHMENT_READ_BOUND) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return Err("is gone".to_string()),
        Err(error) => return Err(format!("cannot be read: {error}")),
    };
    let json: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|error| format!("is not JSON: {error}"))?;
    match json.get("format").and_then(serde_json::Value::as_str) {
        Some(ATTACHMENT_FORMAT) => {}
        Some(other) => return Err(format!("has format {other:?}, not {ATTACHMENT_FORMAT}")),
        None => return Err(format!("has no format, not {ATTACHMENT_FORMAT}")),
    }
    serde_json::from_value(json)
        .map_err(|error| format!("is not a {ATTACHMENT_FORMAT} object: {error}"))
}

/// Whether `attachment` points at the head that verified.
///
/// `verdict` is the record's; `seal` is its `trace.jsonl.seal` and `trace` the bytes of its
/// `trace.jsonl`, which only a mismatch reads, to name the line an earlier head belongs to.
pub(crate) fn check_attachment(
    attachment: &Attachment,
    verdict: Verdict,
    seal: Option<&Seal>,
    trace: &[u8],
) -> AttachmentCheck {
    let head = &attachment.record.head;
    if verdict != Verdict::Verified {
        return AttachmentCheck {
            status: AttachmentStatus::Unverified,
            detail: format!("the record did not verify: {}", verdict.as_str()),
        };
    }
    let Some(seal) = seal else {
        return AttachmentCheck {
            status: AttachmentStatus::Mismatch,
            detail: format!("attached to head {head}; the record has no trace.jsonl seal"),
        };
    };
    if *head == seal.head {
        return AttachmentCheck {
            status: AttachmentStatus::Matches,
            detail: format!(
                "points at the verified head, trace.jsonl line {}",
                seal.lines
            ),
        };
    }
    let earlier = trace
        .split(|byte| *byte == b'\n')
        .take(usize::try_from(seal.lines.saturating_sub(1)).unwrap_or(usize::MAX))
        .position(|line| record_seal::line_hash(line) == *head);
    let detail = match earlier {
        Some(index) => format!(
            "attached to head {head}, which names line {} of a trace that now holds {} lines; \
             the record verified at head {}",
            index + 1,
            seal.lines,
            seal.head
        ),
        None => format!(
            "attached to head {head}; the record verified at head {}",
            seal.head
        ),
    };
    AttachmentCheck {
        status: AttachmentStatus::Mismatch,
        detail,
    }
}

/// Every attachment of the records directory `records_dir`, checked against `report`.
pub(crate) fn attachment_entries(
    records_dir: &Path,
    report: &RecordReport,
) -> Vec<AttachmentEntry> {
    let records = match WorkdirRoot::open(records_dir) {
        Ok(records) => records,
        Err(error) if error.is_not_found() => return Vec::new(),
        Err(error) => {
            return vec![unreadable_entry(
                ATTACHMENTS_DIR.to_string(),
                format!("{} cannot be opened: {error}", records_dir.display()),
            )]
        }
    };
    let listed = list_attachments(&records);
    if listed.is_empty() {
        return Vec::new();
    }
    let seal = record_seal::read_seal(&records, TRACE_STREAM)
        .ok()
        .flatten();
    let trace = records
        .read_file(TRACE_STREAM, u64::MAX)
        .ok()
        .flatten()
        .unwrap_or_default();
    listed
        .into_iter()
        .map(|entry| match entry {
            ListedAttachment::Read { file, attachment } => {
                let attachment = *attachment;
                let check = check_attachment(&attachment, report.verdict, seal.as_ref(), &trace);
                AttachmentEntry {
                    file,
                    attachment_id: Some(attachment.attachment_id),
                    evaluator: Some(attachment.evaluator),
                    value: Some(attachment.value),
                    events: Some(attachment.events),
                    attached_at: Some(attachment.attached_at),
                    record_head: Some(attachment.record.head),
                    status: check.status,
                    detail: check.detail,
                }
            }
            ListedAttachment::Unreadable { file, reason } => unreadable_entry(file, reason),
        })
        .collect()
}

fn unreadable_entry(file: String, reason: String) -> AttachmentEntry {
    AttachmentEntry {
        file,
        attachment_id: None,
        evaluator: None,
        value: None,
        events: None,
        attached_at: None,
        record_head: None,
        status: AttachmentStatus::Unreadable,
        detail: reason,
    }
}

/// How `mur trace verify`'s text form shows a value: inline when it is one line of at most
/// [`INLINE_VALUE_CHARS`] characters with no control characters, otherwise its size.
pub(crate) fn value_cell(value: &str) -> String {
    let inline = !value.is_empty()
        && value.chars().count() <= INLINE_VALUE_CHARS
        && !value.chars().any(char::is_control);
    if inline {
        value.to_string()
    } else {
        format!("<{} bytes, see --json>", value.len())
    }
}

/// `text`, or its escaped form when it holds a control character a terminal would act on.
fn printable(text: &str) -> String {
    if text.chars().any(char::is_control) {
        format!("{text:?}")
    } else {
        text.to_string()
    }
}

/// `mur trace verify`'s `attachments:` block.
pub(crate) fn print_attachments(entries: &[AttachmentEntry]) {
    if entries.is_empty() {
        capsule_runtime::report_println!("attachments: none");
        return;
    }
    let rows: Vec<[String; 5]> = entries
        .iter()
        .map(|entry| {
            [
                entry
                    .attachment_id
                    .as_deref()
                    .map(printable)
                    .unwrap_or_else(|| printable(&entry.file)),
                entry
                    .evaluator
                    .as_deref()
                    .map(printable)
                    .unwrap_or_else(|| "-".to_string()),
                entry.status.as_str().to_string(),
                entry
                    .value
                    .as_deref()
                    .map(value_cell)
                    .unwrap_or_else(|| "-".to_string()),
                printable(&entry.detail),
            ]
        })
        .collect();
    let width = |column: usize| {
        rows.iter()
            .map(|row| row[column].chars().count())
            .max()
            .unwrap_or(0)
    };
    let (id, evaluator, status, value) = (width(0), width(1), width(2), width(3));
    capsule_runtime::report_println!("attachments:");
    for [a, b, c, d, e] in &rows {
        capsule_runtime::report_println!(
            "  {a:<id$}  {b:<evaluator$}  {c:<status$}  {d:<value$}  {e}"
        );
    }
}

/// The value `mur trace attach` stores: `--value` as given, or the bytes of `--value-file`
/// (`-` for standard input), which must be UTF-8. Refusals name the flag.
fn read_value(value: Option<String>, value_file: Option<&Path>) -> Result<String, CliError> {
    let refused = |message: String| CliError::new(E_TRC_006, message);
    let (value, flag) = match (value, value_file) {
        (Some(value), None) => (value, "--value".to_string()),
        (None, Some(path)) => {
            let stdin = path == Path::new("-");
            let flag = if stdin {
                "--value-file - (standard input)".to_string()
            } else {
                format!("--value-file {}", path.display())
            };
            let mut bytes = Vec::new();
            let read = if stdin {
                std::io::stdin()
                    .lock()
                    .take(ATTACHMENT_READ_BOUND + 1)
                    .read_to_end(&mut bytes)
            } else {
                std::fs::File::open(path)
                    .and_then(|file| file.take(ATTACHMENT_READ_BOUND + 1).read_to_end(&mut bytes))
            };
            read.map_err(|error| refused(format!("{flag} cannot be read: {error}")))?;
            if bytes.len() as u64 > ATTACHMENT_READ_BOUND {
                return Err(refused(format!(
                    "{flag} is larger than {ATTACHMENT_READ_BOUND} bytes"
                )));
            }
            let value = String::from_utf8(bytes)
                .map_err(|error| refused(format!("{flag} is not UTF-8: {error}")))?;
            (value, flag)
        }
        _ => {
            return Err(refused(
                "give exactly one of --value and --value-file".to_string(),
            ))
        }
    };
    if value.is_empty() {
        return Err(refused(format!("{flag} is empty")));
    }
    if value.len() as u64 > ATTACHMENT_READ_BOUND {
        return Err(refused(format!(
            "{flag} is larger than {ATTACHMENT_READ_BOUND} bytes"
        )));
    }
    Ok(value)
}

/// Refuses (E-TRC-006) an evaluator that is empty or holds a control character.
pub(crate) fn check_evaluator(evaluator: &str) -> Result<(), CliError> {
    if evaluator.is_empty() {
        return Err(CliError::new(E_TRC_006, "--evaluator is empty"));
    }
    if evaluator.chars().any(char::is_control) {
        return Err(CliError::new(
            E_TRC_006,
            format!("--evaluator {evaluator:?} holds a control character"),
        ));
    }
    Ok(())
}

/// The first of `events` that is no line's `event_id` in `trace`.
fn unknown_event<'a>(trace: &[u8], events: &'a [String]) -> Option<&'a str> {
    let recorded: std::collections::HashSet<String> = trace
        .split(|byte| *byte == b'\n')
        .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
        .filter_map(|line| line.get("event_id")?.as_str().map(str::to_string))
        .collect();
    events
        .iter()
        .find(|event| !recorded.contains(event.as_str()))
        .map(String::as_str)
}

/// Verifies the record in `records_dir` as `mur trace verify` does and, when it verifies, writes
/// one attachment naming its sealed head. Writes nothing on any refusal.
///
/// The caller checks `evaluator` with [`check_evaluator`] and `value` for emptiness and size first;
/// each of `events` must be an `event_id` of `trace.jsonl`. Returns the attachment and the absolute
/// path of the file written.
pub(crate) fn attach_to_records(
    records_dir: &Path,
    evaluator: String,
    value: String,
    events: Vec<String>,
    public_key: Option<&Path>,
) -> Result<(Attachment, PathBuf), CliError> {
    let report = verify_records(records_dir, public_key)?;
    let session_name = report
        .session_id
        .clone()
        .unwrap_or_else(|| records_dir.display().to_string());
    if let Some(code) = verdict_code(report.verdict) {
        let finding = worst_stream(&report)
            .map(stream_finding)
            .unwrap_or_else(|| format!("{} holds no trace.jsonl to verify", records_dir.display()));
        return Err(CliError::new(
            code,
            format!("cannot attach to session {session_name}: {finding}"),
        ));
    }

    let write_refused = |reason: String| {
        CliError::new(
            E_TRC_007,
            format!(
                "the attachment cannot be written to {}: {reason}",
                records_dir.join(ATTACHMENTS_DIR).display()
            ),
        )
    };
    let records =
        WorkdirRoot::open(records_dir).map_err(|error| write_refused(error.to_string()))?;
    let unread = |reason: String| {
        CliError::new(
            E_TRC_003,
            format!("cannot attach to session {session_name}: {reason}"),
        )
    };
    let trace = records
        .read_file(TRACE_STREAM, u64::MAX)
        .map_err(|error| unread(error.to_string()))?
        .ok_or_else(|| unread("trace.jsonl is gone".to_string()))?;
    if let Some(unknown) = unknown_event(&trace, &events) {
        return Err(CliError::new(
            E_TRC_006,
            format!("--event {unknown} names no event of session {session_name}'s trace.jsonl"),
        ));
    }
    let seal = record_seal::read_seal(&records, TRACE_STREAM)
        .map_err(unread)?
        .ok_or_else(|| unread("trace.jsonl.seal is gone".to_string()))?;

    let attachment = Attachment {
        format: ATTACHMENT_FORMAT.to_string(),
        attachment_id: new_attachment_id(),
        session_id: report
            .session_id
            .clone()
            .unwrap_or_else(|| seal.session_id.clone()),
        evaluator,
        value,
        events,
        attached_at: capsule_runtime::retention::now_ms(),
        record: AttachedHead {
            stream: TRACE_STREAM.to_string(),
            head: seal.head,
            lines: seal.lines,
            key_id: seal.key_id,
        },
    };
    let file = records_dir.join(attachment_rel(&attachment.attachment_id));
    write_attachment(&records, &attachment).map_err(|reason| {
        CliError::new(
            E_TRC_007,
            format!(
                "the attachment {} cannot be written: {reason}",
                file.display()
            ),
        )
    })?;
    Ok((attachment, file))
}

/// `mur trace attach`: resolves the session, checks the evaluator and value, then attaches through
/// [`attach_to_records`]. Refuses, writing nothing, unless the record verifies.
pub(crate) fn run_trace_attach(request: AttachRequest) -> Result<(), CliError> {
    let current_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let workdir = request
        .workdir
        .unwrap_or_else(|| current_dir.join("workdir"));
    let path = resolve_session(Some(request.session), &workdir)?;
    let records_dir = records_dir_of(&path);

    check_evaluator(&request.evaluator)?;
    let value = read_value(request.value, request.value_file.as_deref())?;

    let (attachment, file) = attach_to_records(
        &records_dir,
        request.evaluator,
        value,
        request.events,
        request.public_key.as_deref(),
    )?;

    if request.json {
        let output = AttachOutput {
            attachment: &attachment,
            path: file.display().to_string(),
        };
        capsule_runtime::report_println!(
            "{}",
            serde_json::to_string_pretty(&output).unwrap_or_else(|_| "{}".to_string())
        );
    } else {
        capsule_runtime::report_println!(
            "attached {} to session {} (trace.jsonl line {}, head {})",
            attachment.attachment_id,
            attachment.session_id,
            attachment.record.lines,
            attachment.record.head
        );
        capsule_runtime::report_println!("{}", file.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attachment(id: &str, attached_at: u64, head: &str) -> Attachment {
        Attachment {
            format: ATTACHMENT_FORMAT.to_string(),
            attachment_id: id.to_string(),
            session_id: "ses_test".to_string(),
            evaluator: "ci".to_string(),
            value: "0.75".to_string(),
            events: vec![],
            attached_at,
            record: AttachedHead {
                stream: TRACE_STREAM.to_string(),
                head: head.to_string(),
                lines: 3,
                key_id: "key".to_string(),
            },
        }
    }

    fn seal(head: &str, lines: u64) -> Seal {
        Seal {
            format: record_seal::SEAL_FORMAT.to_string(),
            stream: TRACE_STREAM.to_string(),
            chain: record_seal::CHAIN_INLINE.to_string(),
            session_id: "ses_test".to_string(),
            lines,
            line_hashes: None,
            head: head.to_string(),
            sealed_at: 0,
            key_id: "key".to_string(),
            algorithm: record_seal::SEAL_ALGORITHM.to_string(),
            signature: String::new(),
            conversation: None,
        }
    }

    const TRACE: &[u8] = b"{\"a\":1}\n{\"b\":2}\n{\"c\":3}\n";

    fn line(n: usize) -> String {
        record_seal::line_hash(TRACE.split(|b| *b == b'\n').nth(n - 1).unwrap())
    }

    #[test]
    fn trace_attach_serializes_every_key_in_order() {
        let mut written = attachment("att_1", 7, "sha256:x");
        written.events = vec!["evt_1".to_string()];
        let json = serde_json::to_string(&written).unwrap();
        assert_eq!(
            json,
            "{\"format\":\"murmur.attachment/1\",\"attachment_id\":\"att_1\",\
             \"session_id\":\"ses_test\",\"evaluator\":\"ci\",\"value\":\"0.75\",\
             \"events\":[\"evt_1\"],\"attached_at\":7,\"record\":{\"stream\":\"trace.jsonl\",\
             \"head\":\"sha256:x\",\"lines\":3,\"key_id\":\"key\"}}"
        );
        let empty = serde_json::to_value(attachment("att_2", 1, "h")).unwrap();
        assert_eq!(empty["events"], serde_json::json!([]));
    }

    #[test]
    fn trace_attach_ids_are_att_and_32_hex() {
        let id = new_attachment_id();
        let hex = id.strip_prefix("att_").unwrap();
        assert_eq!(hex.len(), 32);
        assert!(hex
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn trace_attach_lister_skips_others_and_orders_by_time_then_id() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = WorkdirRoot::open(dir.path()).unwrap();
        assert!(
            list_attachments(&root).is_empty(),
            "no attachments/ holds none"
        );

        for written in [
            attachment("att_c", 20, "h"),
            attachment("att_b", 10, "h"),
            attachment("att_a", 20, "h"),
        ] {
            write_attachment(&root, &written).unwrap();
        }
        let attachments = dir.path().join(ATTACHMENTS_DIR);
        std::fs::write(attachments.join("notes.txt"), "not listed").unwrap();
        std::fs::create_dir(attachments.join("nested.json")).unwrap();
        std::fs::write(dir.path().join("elsewhere.json"), "{}").unwrap();
        std::os::unix::fs::symlink(
            dir.path().join("elsewhere.json"),
            attachments.join("0-link.json"),
        )
        .unwrap();
        std::fs::write(attachments.join("1-bad.json"), "not json").unwrap();
        std::fs::write(
            attachments.join("2-other.json"),
            "{\"format\":\"something/1\"}",
        )
        .unwrap();

        let listed = list_attachments(&root);
        let names: Vec<&str> = listed.iter().map(ListedAttachment::file).collect();
        assert_eq!(
            names,
            [
                "attachments/att_b.json",
                "attachments/att_a.json",
                "attachments/att_c.json",
                "attachments/0-link.json",
                "attachments/1-bad.json",
                "attachments/2-other.json",
            ]
        );
        let reasons: Vec<&str> = listed
            .iter()
            .filter_map(|entry| match entry {
                ListedAttachment::Unreadable { reason, .. } => Some(reason.as_str()),
                ListedAttachment::Read { .. } => None,
            })
            .collect();
        assert_eq!(reasons[0], "attachments/0-link.json is a link");
        assert!(
            reasons[1].starts_with("attachments/1-bad.json is not JSON: "),
            "{}",
            reasons[1]
        );
        assert_eq!(
            reasons[2],
            "attachments/2-other.json has format \"something/1\", not murmur.attachment/1"
        );
    }

    #[test]
    fn trace_attach_check_matches_the_verified_head() {
        let check = check_attachment(
            &attachment("att_1", 1, &line(3)),
            Verdict::Verified,
            Some(&seal(&line(3), 3)),
            TRACE,
        );
        assert_eq!(check.status, AttachmentStatus::Matches);
        assert_eq!(
            check.detail,
            "points at the verified head, trace.jsonl line 3"
        );
    }

    #[test]
    fn trace_attach_check_names_the_earlier_line_of_a_mismatch() {
        let check = check_attachment(
            &attachment("att_1", 1, &line(2)),
            Verdict::Verified,
            Some(&seal(&line(3), 3)),
            TRACE,
        );
        assert_eq!(check.status, AttachmentStatus::Mismatch);
        assert_eq!(
            check.detail,
            format!(
                "attached to head {}, which names line 2 of a trace that now holds 3 lines; the \
                 record verified at head {}",
                line(2),
                line(3)
            )
        );

        let elsewhere = format!("sha256:{}", "ab".repeat(32));
        let check = check_attachment(
            &attachment("att_1", 1, &elsewhere),
            Verdict::Verified,
            Some(&seal(&line(3), 3)),
            TRACE,
        );
        assert_eq!(check.status, AttachmentStatus::Mismatch);
        assert_eq!(
            check.detail,
            format!(
                "attached to head {elsewhere}; the record verified at head {}",
                line(3)
            )
        );
    }

    #[test]
    fn trace_attach_check_is_unverified_unless_the_record_verified() {
        for verdict in [
            Verdict::Changed,
            Verdict::Unsealed,
            Verdict::Unverifiable,
            Verdict::Absent,
        ] {
            let check = check_attachment(
                &attachment("att_1", 1, &line(3)),
                verdict,
                Some(&seal(&line(3), 3)),
                TRACE,
            );
            assert_eq!(check.status, AttachmentStatus::Unverified);
            assert_eq!(
                check.detail,
                format!("the record did not verify: {}", verdict.as_str())
            );
        }
    }

    #[test]
    fn trace_attach_value_cell_inlines_one_short_line() {
        assert_eq!(value_cell("0.75"), "0.75");
        let longest = "x".repeat(INLINE_VALUE_CHARS);
        assert_eq!(value_cell(&longest), longest);
        assert_eq!(
            value_cell(&"x".repeat(INLINE_VALUE_CHARS + 1)),
            "<121 bytes, see --json>"
        );
        let wide = "é".repeat(INLINE_VALUE_CHARS);
        assert_eq!(value_cell(&wide), wide, "characters are counted, not bytes");
        assert_eq!(value_cell("  0.5\n"), "<6 bytes, see --json>");
        assert_eq!(value_cell("a\nb"), "<3 bytes, see --json>");
    }
}
