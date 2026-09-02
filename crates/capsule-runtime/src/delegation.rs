//! Which capsule delegated to this one, and how a delegated child says that it finished.
//!
//! A child launched by [`crate::child_launch`] knows its spawner from one injected environment
//! variable, [`SPAWNER_ENV`], and from nothing it inherited: the parent's launcher composes a
//! [`SpawnerHandle`] and applies it in the runtime-owned tail of the child's environment. A
//! capsule launched any other way has no handle, records no lineage and contacts nothing.
//!
//! **Knowing your parent and reporting to it are separate.** Every handle names the session that
//! spawned this one and the delegation that created it, which is what the child writes into its
//! own `session_start`. Only a handle carrying a [`CompletionAddress`] also has somewhere to post
//! an outcome: a parent that waits on the connection it already holds — every delegation made
//! through [`crate::delegation_plane`] — supplies none, so its child is attributable without
//! anything being sent anywhere.
//!
//! **The completion names the delegation; it never carries the child's output.** Every field of
//! [`DelegationOutcome`] is composed by a runtime — ids, a status word, a path, a duration — and
//! the one input that could grow, the crash detail, is capped twice: at the 20 stderr lines
//! `child_launch` retains, and again at [`MAX_DETAIL_BYTES`]. The child's result stays in a file
//! in the child's own directory, which sits inside the parent's single WASI preopen, so a parent
//! that wants it reads it deliberately through an ordinary tool call and the untrusted-content
//! fence applies at that read. This is the rule [`crate::detached`] already follows one layer
//! down for a demoted shell command, which names `output_path` and never the output.
//!
//! Two reporters, one arrival path. The child reports for itself at the end of its own session;
//! the parent's [`crate::child_launch::LaunchedChild`] reports for a child that could not. Both
//! write [`COMPLETION_FILE`] into the child's directory and both post the same JSON-RPC
//! `message/send` to the parent's A2A door, which is the only way a completion enters the
//! parent's queue. A completion that cannot be delivered is recorded in that file with
//! `delivered: false` and the refusal's reason, and a line goes to stderr; it is not retried
//! beyond the launcher's single retry and it is not silently discarded.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::errors::RuntimeError;
use crate::http_client::http_json;
use crate::origin::{stamp_for_completion, TrustClass, PEER_ORIGIN_HEADER, PEER_TRUST_HEADER};

/// The variable the parent's launcher injects into a delegated child, carrying its
/// [`SpawnerHandle`] as compact JSON.
///
/// Applied in the runtime-owned tail of the child's environment alongside `MURMUR_ROOST_URL`, so
/// a child cannot displace it by allowlisting the name in `capabilities.env.allow`.
pub const SPAWNER_ENV: &str = "MURMUR_SPAWNER";

/// Header naming the delegation a completion reports on. Read by the parent's door only for a
/// request classified `completion`, and carried onto the enqueued task and its `task_start`.
pub const DELEGATION_ID_HEADER: &str = "x-murmur-delegation-id";

/// Header naming the session a completion is addressed to.
///
/// The parent's door refuses a completion whose addressed session is not its own — the shape a
/// parent that restarted onto the same address leaves behind, where the port answers but the
/// session that made the delegation is gone.
pub const COMPLETION_SESSION_HEADER: &str = "x-murmur-completion-session";

/// Prefix on every delegation id. Distinct from `tsk_`/`ctx_`/`ses_`/`wrk_`, so a reader of
/// `trace.jsonl` can tell at a glance which id space a value belongs to.
pub const DELEGATION_ID_PREFIX: &str = "dlg_";

/// The child's own record of how it ended, in the child's directory.
pub const COMPLETION_FILE: &str = "completion.json";

/// Ceiling on [`DelegationOutcome::detail`], the one field built from something a child wrote.
///
/// `child_launch` already bounds the stderr tail to its last 20 lines; this bounds the bytes, so
/// a child that logged one enormous line cannot grow the text the parent's agent reads.
pub const MAX_DETAIL_BYTES: usize = 2_000;

/// A fresh delegation id, matching `^dlg_[0-9a-f]+$`.
///
/// Time-ordered like every other id the runtime mints, so two delegation ids sort in the order
/// the delegations were made.
pub fn new_delegation_id() -> String {
    format!("{DELEGATION_ID_PREFIX}{}", uuid::Uuid::now_v7().simple())
}

/// Where a completion is posted and the trust it inherits.
///
/// Absent for a delegation whose parent waits on the connection it already holds and therefore
/// wants no completion. Reporting needs an address; knowing your parent does not, which is why
/// this is the optional half of a [`Spawner`] and the lineage is the unconditional half.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionAddress {
    /// The parent's own A2A endpoint, `http://host:port`.
    pub url: String,
    /// The trust class of the parent task that made the delegation. Inherited by the completion
    /// through [`stamp_for_completion`] and decided nowhere else.
    pub trust: TrustClass,
}

/// The parent a delegated child belongs to, supplied by the parent at launch.
///
/// Carries no delegation id: the id is minted by the launcher, one per launch, so a caller
/// holding one `Spawner` cannot make two delegations report under one id.
#[derive(Debug, Clone)]
pub struct Spawner {
    /// The parent's session id. A completion addressed to any other session is refused at the
    /// door rather than delivered to whoever answers the address now, and it is the value the
    /// child writes to its own `session_start.spawned_by`.
    pub session_id: String,
    /// The conversation the delegation was made from. The completion task runs under this id, so
    /// the outcome joins the thread that asked for it.
    pub context_id: String,
    /// Where this delegation's completion goes, or `None` for a parent that wants none.
    pub report_to: Option<CompletionAddress>,
}

/// The value injected into the child: a [`Spawner`] plus the delegation id the launcher minted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnerHandle {
    pub session_id: String,
    pub context_id: String,
    pub delegation_id: String,
    pub report_to: Option<CompletionAddress>,
}

impl SpawnerHandle {
    /// The handle for one launch of `spawner`'s child.
    pub fn for_delegation(spawner: &Spawner, delegation_id: String) -> Self {
        Self {
            session_id: spawner.session_id.clone(),
            context_id: spawner.context_id.clone(),
            delegation_id,
            report_to: spawner.report_to.clone(),
        }
    }

    /// The compact JSON written into [`SPAWNER_ENV`].
    ///
    /// `url` and `trust` appear together or not at all: a lineage-only handle carries the three
    /// keys that name the relationship and nothing that could be read as an address.
    pub fn to_env_value(&self) -> String {
        let mut value = serde_json::json!({
            "session_id": self.session_id,
            "context_id": self.context_id,
            "delegation_id": self.delegation_id,
        });
        if let Some(address) = &self.report_to {
            value["url"] = Value::String(address.url.clone());
            value["trust"] = Value::String(address.trust.as_str().to_string());
        }
        value.to_string()
    }

    /// This process's own handle, read from [`SPAWNER_ENV`].
    ///
    /// `Ok(None)` for a capsule nobody delegated — the variable absent, or blank. A variable that
    /// is set to something else is an error and not an absence: a child that cannot read its
    /// spawner cannot record which session spawned it or report to it, and running it anyway
    /// would produce work whose provenance and outcome reach nobody.
    pub fn from_env() -> Result<Option<Self>, RuntimeError> {
        let Some(raw) = std::env::var_os(SPAWNER_ENV) else {
            return Ok(None);
        };
        let raw = raw.to_string_lossy().to_string();
        if raw.trim().is_empty() {
            return Ok(None);
        }
        Self::parse(raw.trim()).map(Some)
    }

    /// Parse one [`Self::to_env_value`] string.
    pub fn parse(value: &str) -> Result<Self, RuntimeError> {
        let unreadable = |reason: String| RuntimeError::SpawnerHandleUnreadable { reason };
        let parsed: Value = serde_json::from_str(value)
            .map_err(|error| unreadable(format!("it is not JSON: {error}")))?;
        let field = |name: &str| -> Result<String, RuntimeError> {
            parsed
                .get(name)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .ok_or_else(|| unreadable(format!("it carries no '{name}'")))
        };
        // Half an address is not a lineage-only handle with a stray key: it is a handle whose
        // author meant a completion to arrive somewhere and did not say where, or under what
        // trust, and delivering one on a guess is the thing this refuses to do.
        let report_to =
            match (parsed.get("url").is_some(), parsed.get("trust").is_some()) {
                (false, false) => None,
                (true, true) => {
                    let trust_value = field("trust")?;
                    // The one place in this module that names a trust class: every other use passes
                    // the parsed value through `stamp_for_completion`, so the completion's class is
                    // derived once, from the delegating task, and never decided here.
                    let trust = TrustClass::parse(&trust_value).ok_or_else(|| {
                        unreadable(format!("'{trust_value}' is not a trust class"))
                    })?;
                    Some(CompletionAddress {
                        url: field("url")?,
                        trust,
                    })
                }
                (true, false) => return Err(unreadable(
                    "it carries a 'url' with no 'trust'; a completion address is both or neither"
                        .to_string(),
                )),
                (false, true) => return Err(unreadable(
                    "it carries a 'trust' with no 'url'; a completion address is both or neither"
                        .to_string(),
                )),
            };
        Ok(Self {
            session_id: field("session_id")?,
            context_id: field("context_id")?,
            delegation_id: field("delegation_id")?,
            report_to,
        })
    }
}

/// How a delegated child ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DelegationStatus {
    /// The child's session finished. Reported by the child.
    Ok,
    /// The child's session ran and failed. Reported by the child.
    Error,
    /// The child's process ended without recording a completion. Reported by the launcher.
    Crashed,
    /// The parent ended the delegation itself. Recorded by the launcher and posted to nobody —
    /// the only party that would be told is the party that did it.
    Terminated,
}

impl DelegationStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Crashed => "crashed",
            Self::Terminated => "terminated",
        }
    }
}

/// Which of the two reporters built a completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Reporter {
    /// The child's own runtime, at the end of its session.
    Child,
    /// The parent's launcher, for a child that could not report for itself.
    Launcher,
}

impl Reporter {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Child => "child",
            Self::Launcher => "launcher",
        }
    }
}

/// What a delegation left behind: the notification the parent receives, and the shape of
/// [`COMPLETION_FILE`].
///
/// Carries no output. The child's result is on disk at [`Self::result_path`], relative to
/// [`Self::workdir`], and the parent is told where, never what.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelegationOutcome {
    /// The id the parent's launcher minted, injected into the child and echoed back here.
    pub delegation_id: String,
    pub capsule_name: String,
    pub capsule_version: String,
    /// The child's own session, so its trace is findable.
    pub session_id: String,
    pub status: DelegationStatus,
    /// Workdir-relative, `out/result.txt` for a capsule that wrote its result where the runtime
    /// writes one. Absent when the child wrote none — a terminal path that failed without result
    /// text legitimately leaves no file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_path: Option<String>,
    /// The child's directory, absolute — the root [`Self::result_path`] is relative to, and where
    /// [`COMPLETION_FILE`] itself sits.
    pub workdir: String,
    pub duration_ms: u64,
    /// Present only for a `crashed` or `terminated` outcome: the exit status, and for a crash the
    /// child's bounded stderr tail. Capped at [`MAX_DETAIL_BYTES`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub reported_by: Reporter,
    /// Whether the notification reached the parent's door. `false` on a `terminated` outcome,
    /// which is never posted.
    pub delivered: bool,
    /// Why delivery failed, when one was attempted and refused.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery_error: Option<String>,
}

impl DelegationOutcome {
    /// The `IncomingTask.message_text` the parent's agent reads: the delegation, the capsule, the
    /// outcome, the duration and where the result is. Never the result itself.
    pub fn message_text(&self) -> String {
        let mut text = format!(
            "Delegated capsule finished.\n\
             delegation_id: {}\n\
             capsule: {}@{}\n\
             session_id: {}\n\
             status: {}\n\
             duration_ms: {}\n\
             workdir: {}\n",
            self.delegation_id,
            self.capsule_name,
            self.capsule_version,
            self.session_id,
            self.status.as_str(),
            self.duration_ms,
            self.workdir,
        );
        match &self.result_path {
            Some(path) => text.push_str(&format!("result: {path} (in that workdir)")),
            None => text.push_str("result: none (the child wrote no result file)"),
        }
        if let Some(detail) = &self.detail {
            text.push_str(&format!("\ndetail: {detail}"));
        }
        text.push_str("\n\nThe child's own output is in that file and is not reproduced here.");
        text
    }

    /// Bound `detail` at [`MAX_DETAIL_BYTES`], on a character boundary.
    fn with_bounded_detail(mut self) -> Self {
        if let Some(detail) = self.detail.take() {
            self.detail = Some(bound_detail(detail));
        }
        self
    }
}

/// `detail` cut to [`MAX_DETAIL_BYTES`] at a character boundary, with the cut marked.
fn bound_detail(detail: String) -> String {
    if detail.len() <= MAX_DETAIL_BYTES {
        return detail;
    }
    let mut end = MAX_DETAIL_BYTES;
    while end > 0 && !detail.is_char_boundary(end) {
        end -= 1;
    }
    format!("{} […]", &detail[..end])
}

/// Where a child's own record of its ending goes: `<child workdir>/completion.json`.
///
/// One rule, used by the child that writes it and by the launcher's watcher that reads it.
pub fn completion_path(workdir: &Path) -> PathBuf {
    workdir.join(COMPLETION_FILE)
}

/// The completion the child recorded, or `None` when it recorded none.
///
/// This is what stops one delegation being reported twice: the launcher's watcher reads the file
/// before reporting anything, and a child that already delivered is not reported again.
pub fn read_completion(workdir: &Path) -> Option<DelegationOutcome> {
    let raw = std::fs::read_to_string(completion_path(workdir)).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Write `outcome` to [`COMPLETION_FILE`], replacing whatever was there.
pub fn write_completion(workdir: &Path, outcome: &DelegationOutcome) -> Result<(), String> {
    let body = serde_json::to_string_pretty(outcome)
        .map_err(|error| format!("failed to serialize the completion: {error}"))?;
    std::fs::write(completion_path(workdir), format!("{body}\n"))
        .map_err(|error| format!("failed to write {COMPLETION_FILE}: {error}"))
}

/// Post one completion to the parent's A2A door.
///
/// A JSON-RPC `message/send` carrying [`DelegationOutcome::message_text`], stamped with the four
/// headers the door reads: the origin and trust of [`stamp_for_completion`], the delegation id,
/// and the session the completion is addressed to. Blocking, because both reporters run outside
/// any async context — the child's is a `Drop` guard at the end of its session, and the
/// launcher's is a watcher thread.
pub fn deliver_completion(
    handle: &SpawnerHandle,
    address: &CompletionAddress,
    outcome: &DelegationOutcome,
) -> Result<(), String> {
    let stamped = stamp_for_completion(Some(address.trust));
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": format!("req_{}", uuid::Uuid::now_v7().simple()),
        "method": "message/send",
        "params": {
            "message": {
                "messageId": format!("msg_{}", uuid::Uuid::now_v7().simple()),
                "contextId": handle.context_id,
                "role": "user",
                "parts": [{"text": outcome.message_text()}]
            }
        }
    })
    .to_string();

    let response = http_json(
        "POST",
        &address.url,
        Some(&body),
        &[
            (PEER_ORIGIN_HEADER, stamped.origin().as_str()),
            (PEER_TRUST_HEADER, stamped.trust().as_str()),
            (DELEGATION_ID_HEADER, handle.delegation_id.as_str()),
            (COMPLETION_SESSION_HEADER, handle.session_id.as_str()),
        ],
    )?;

    // A door that refuses answers `200` with a JSON-RPC error, and one whose queue is full
    // answers a `rejected` task. Neither delivered the completion, so neither is success.
    if let Some(error) = response.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("the parent refused the completion");
        return Err(message.to_string());
    }
    if response
        .pointer("/result/status/state")
        .and_then(Value::as_str)
        == Some("rejected")
    {
        return Err("the parent rejected the completion: its queue is full".to_string());
    }
    Ok(())
}

/// Record `outcome` in the child's directory, post it, and record what the posting did.
///
/// The file is written before the post and rewritten after it, so a reporter that dies mid-post
/// still leaves the outcome on disk. The returned outcome is the one that was written last.
///
/// `address` is separate from `handle` because a handle need not carry one: a caller reaches this
/// only by having matched on [`SpawnerHandle::report_to`], so a lineage-only delegation cannot
/// take this path by accident.
pub fn report_completion(
    handle: &SpawnerHandle,
    address: &CompletionAddress,
    outcome: DelegationOutcome,
    workdir: &Path,
) -> DelegationOutcome {
    let mut outcome = outcome.with_bounded_detail();
    outcome.delivered = false;
    outcome.delivery_error = None;
    if let Err(reason) = write_completion(workdir, &outcome) {
        eprintln!(
            "[capsule-runtime] delegation {}: {reason}",
            outcome.delegation_id
        );
    }

    match deliver_completion(handle, address, &outcome) {
        Ok(()) => outcome.delivered = true,
        Err(reason) => {
            // The record, and the operator's only other sign that a result went nowhere. Not a
            // failure of the child's own session: the work was done, and where it went is what
            // could not be said.
            eprintln!(
                "[capsule-runtime] delegation {}: the completion could not be delivered to {}: {reason}; recorded in {}",
                outcome.delegation_id,
                address.url,
                completion_path(workdir).display(),
            );
            outcome.delivery_error = Some(reason);
        }
    }
    if let Err(reason) = write_completion(workdir, &outcome) {
        eprintln!(
            "[capsule-runtime] delegation {}: {reason}",
            outcome.delegation_id
        );
    }
    outcome
}

/// Record a delegation the parent ended itself, without posting it.
///
/// The only party that would be told is the party that did it, so this writes the file and
/// contacts nobody.
pub fn record_terminated(workdir: &Path, outcome: DelegationOutcome) -> DelegationOutcome {
    let mut outcome = outcome.with_bounded_detail();
    outcome.delivered = false;
    outcome.delivery_error = None;
    if let Err(reason) = write_completion(workdir, &outcome) {
        eprintln!(
            "[capsule-runtime] delegation {}: {reason}",
            outcome.delegation_id
        );
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handle() -> SpawnerHandle {
        SpawnerHandle {
            session_id: "ses_parent".to_string(),
            context_id: "ctx_parent".to_string(),
            delegation_id: "dlg_0001".to_string(),
            report_to: Some(address()),
        }
    }

    fn address() -> CompletionAddress {
        CompletionAddress {
            url: "http://127.0.0.1:7777".to_string(),
            trust: TrustClass::Untrusted,
        }
    }

    fn outcome() -> DelegationOutcome {
        DelegationOutcome {
            delegation_id: "dlg_0001".to_string(),
            capsule_name: "worker".to_string(),
            capsule_version: "0.1.0".to_string(),
            session_id: "ses_child".to_string(),
            status: DelegationStatus::Ok,
            result_path: Some("out/result.txt".to_string()),
            workdir: "/tmp/parent/.murmur/children/worker-abc".to_string(),
            duration_ms: 42,
            detail: None,
            reported_by: Reporter::Child,
            delivered: false,
            delivery_error: None,
        }
    }

    #[test]
    fn a_handle_round_trips_through_its_env_value() {
        for trust in [TrustClass::Trusted, TrustClass::Untrusted] {
            let mut original = handle();
            original.report_to = Some(CompletionAddress {
                url: address().url,
                trust,
            });
            let parsed = SpawnerHandle::parse(&original.to_env_value()).expect("a written handle");
            assert_eq!(parsed, original);
        }
    }

    /// A child that is told who spawned it and nothing about where to report carries the lineage
    /// and no address at all — not an empty one.
    #[test]
    fn a_lineage_only_handle_carries_no_address() {
        let mut original = handle();
        original.report_to = None;
        let written = original.to_env_value();
        assert!(!written.contains("url"), "{written}");
        assert!(!written.contains("trust"), "{written}");

        let parsed = SpawnerHandle::parse(&written).expect("a written handle");
        assert_eq!(parsed, original);
        assert_eq!(parsed.session_id, "ses_parent");
        assert_eq!(parsed.delegation_id, "dlg_0001");
    }

    #[test]
    fn an_unreadable_handle_is_an_error_and_not_an_absence() {
        let refused = [
            "".to_string(),
            "not json".to_string(),
            "{}".to_string(),
            serde_json::json!({"url": "http://x", "session_id": "s", "context_id": "c",
                               "delegation_id": "dlg_1"})
            .to_string(),
            serde_json::json!({"url": "http://x", "session_id": "s", "context_id": "c",
                               "trust": "maybe", "delegation_id": "dlg_1"})
            .to_string(),
            // Half a completion address: the author meant an outcome to arrive somewhere and did
            // not say where, or under what trust.
            serde_json::json!({"session_id": "s", "context_id": "c", "delegation_id": "dlg_1",
                               "trust": "trusted"})
            .to_string(),
        ];
        for value in refused {
            let error = SpawnerHandle::parse(&value).unwrap_err();
            assert!(
                matches!(error, RuntimeError::SpawnerHandleUnreadable { .. }),
                "{value:?} produced {error}"
            );
            assert!(
                error
                    .to_string()
                    .contains("tell its spawner that it finished"),
                "the refusal must say why it is fatal: {error}"
            );
        }
    }

    /// A delegation id belongs to its own id space and two are never the same.
    #[test]
    fn delegation_ids_are_prefixed_and_unique() {
        let first = new_delegation_id();
        let second = new_delegation_id();
        assert_ne!(first, second);
        for id in [&first, &second] {
            assert!(id.starts_with(DELEGATION_ID_PREFIX), "{id}");
            assert!(
                id[DELEGATION_ID_PREFIX.len()..]
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                "{id}"
            );
        }
    }

    /// The notification names the delegation and where the result is; it never carries it.
    #[test]
    fn the_message_names_the_result_and_never_carries_it() {
        let text = outcome().message_text();
        assert!(text.contains("delegation_id: dlg_0001"), "{text}");
        assert!(text.contains("capsule: worker@0.1.0"), "{text}");
        assert!(text.contains("session_id: ses_child"), "{text}");
        assert!(text.contains("status: ok"), "{text}");
        assert!(text.contains("result: out/result.txt"), "{text}");
        assert!(text.contains("not reproduced here"), "{text}");
    }

    /// The only unbounded input near a completion is the crash detail, and it is capped.
    #[test]
    fn a_crash_detail_is_capped() {
        let mut crashed = outcome();
        crashed.status = DelegationStatus::Crashed;
        crashed.detail = Some("x".repeat(MAX_DETAIL_BYTES * 4));
        let bounded = crashed.with_bounded_detail();
        let detail = bounded.detail.expect("the detail survives, bounded");
        assert!(
            detail.len() <= MAX_DETAIL_BYTES + "  […]".len(),
            "detail is {} bytes",
            detail.len()
        );
        assert!(detail.ends_with("[…]"), "the cut is marked: {detail}");
    }

    #[test]
    fn a_completion_round_trips_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_completion(dir.path()).is_none());

        let mut written = outcome();
        written.delivered = true;
        write_completion(dir.path(), &written).unwrap();

        let read = read_completion(dir.path()).expect("the written completion reads back");
        assert_eq!(read.delegation_id, written.delegation_id);
        assert_eq!(read.status, DelegationStatus::Ok);
        assert!(read.delivered);
        assert_eq!(read.result_path.as_deref(), Some("out/result.txt"));

        // An absent field is omitted from the file rather than written as null.
        let raw = std::fs::read_to_string(completion_path(dir.path())).unwrap();
        assert!(!raw.contains("detail"), "{raw}");
        assert!(!raw.contains("delivery_error"), "{raw}");
    }

    /// A delegation the parent ended is recorded and posted to nobody. The url here answers
    /// nothing, so a post would fail loudly.
    #[test]
    fn a_terminated_delegation_is_recorded_and_not_posted() {
        let dir = tempfile::tempdir().unwrap();
        let mut ended = outcome();
        ended.status = DelegationStatus::Terminated;
        ended.reported_by = Reporter::Launcher;
        ended.detail = Some("the parent ended this delegation".to_string());

        let recorded = record_terminated(dir.path(), ended);
        assert!(!recorded.delivered);
        assert!(recorded.delivery_error.is_none());
        let read = read_completion(dir.path()).expect("the record exists");
        assert_eq!(read.status, DelegationStatus::Terminated);
        assert_eq!(read.reported_by, Reporter::Launcher);
    }

    /// A completion with nowhere to go is recorded rather than dropped.
    #[test]
    fn an_undeliverable_completion_is_recorded_with_its_reason() {
        let dir = tempfile::tempdir().unwrap();
        // Port 1 is reserved and nothing listens there.
        let nowhere = CompletionAddress {
            url: "http://127.0.0.1:1".to_string(),
            trust: TrustClass::Untrusted,
        };

        let reported = report_completion(&handle(), &nowhere, outcome(), dir.path());
        assert!(!reported.delivered);
        let reason = reported.delivery_error.expect("the refusal is recorded");
        assert!(reason.contains("failed to connect"), "{reason}");

        let read = read_completion(dir.path()).expect("the record exists");
        assert!(!read.delivered);
        assert!(read.delivery_error.is_some());
    }
}
