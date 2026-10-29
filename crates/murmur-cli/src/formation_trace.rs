//! Which recorded sessions belong to one formation, read from their traces.
//!
//! This is the only reader that looks at more than one session's trace. It leans on one
//! property of the file: `session_start` is the first line of every agent session's
//! `trace.jsonl`, and a member's `session_start` carries its `formation_id`. So telling a member
//! from any other session costs one line, and a whole trace is read only for a session already
//! known to be a member. A session whose first line is anything else — a script capsule's
//! drain-only trace, a torn file — is no member.
//!
//! A formation's members record under more than one records group: the entry member under the
//! roster's project's, each peer under the group of its own directory in
//! `~/.murmur/formations/<frm_id>/`.
//! [`search_formation`] reads a given root and then those, and merges what they record. Nothing
//! here follows a delegation edge into another session root: a member's delegated child that was
//! not found under a searched root is counted, not chased.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

use capsule_runtime::{retention::session_id_timestamp_ms, FormationId};
use serde::Deserialize;

use crate::error::CliError;
use crate::session_address::ses_entries;

/// One session under a root whose `session_start` names the formation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecordedMember {
    /// The session directory's name, a `ses_` id.
    pub(crate) session_id: String,
    /// The session directory the trace was read from, under the root it was found in.
    pub(crate) session_dir: PathBuf,
    /// `name@version` from `session_start`.
    pub(crate) capsule: String,
    /// The roster name, `session_start.formation_member`. `None` for a member's delegated child,
    /// which the formation launcher handed no credentials.
    pub(crate) member: Option<String>,
    /// The members this one may call, `session_start.formation_callees`, in roster order.
    pub(crate) callees: Vec<String>,
    /// The session that delegated this one, from `session_start`.
    pub(crate) spawned_by: Option<String>,
    /// `exit_status` of the first `session_end`; `None` for a session that wrote none — one still
    /// running, or one that was killed.
    pub(crate) exit_status: Option<String>,
    /// `child_session_id` of every `delegation_start`, in file order.
    pub(crate) delegated_children: Vec<String>,
    /// Every `call-member` call this session made, in the order its first line was written.
    pub(crate) calls_made: Vec<MemberCallRecord>,
    /// Every task the session's door let in, one per `a2a_task_received`, in the order received,
    /// whoever sent it: another member's `call-member` call, the operator, a declared credential,
    /// a public-door caller, or a caller an older trace does not name.
    pub(crate) tasks_received: Vec<ReceivedTask>,
}

impl RecordedMember {
    /// The roster name, or the session id for a session that has none.
    fn name(&self) -> &str {
        self.member.as_deref().unwrap_or(&self.session_id)
    }
}

/// What one session root records about a formation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RecordedMembers {
    /// Every member found, by session id ascending.
    pub(crate) members: Vec<RecordedMember>,
    /// The members' delegated children that are not themselves among `members`, ascending: their
    /// traces are in another session root, or nowhere.
    pub(crate) children_elsewhere: Vec<String>,
}

/// One `call-member` call, as its `member_call_busy`, `member_call_start` and `member_call` lines
/// join up on the call id. A call with no start was never held by its callee; one with no ending is
/// still outstanding, including one whose callee has so far only turned it away busy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MemberCallRecord {
    pub(crate) call_id: String,
    /// The callee's roster name.
    pub(crate) member: String,
    /// The caller's task the call was made from.
    pub(crate) calling_task_id: String,
    /// The callee's task, `None` for a call it never held.
    pub(crate) member_task_id: Option<String>,
    /// When the call was made: the first line's timestamp less the time it says had passed since,
    /// or a `member_call_start`'s own timestamp when that is the first line.
    pub(crate) started_ms: u64,
    /// `None` while the call is outstanding.
    pub(crate) status: Option<String>,
    pub(crate) duration_ms: u64,
    pub(crate) delivered: bool,
    /// How many offers of the task the callee turned away busy: one per `member_call_busy`.
    pub(crate) busy_offers: u32,
    /// The members further down the callee reported as giving it no answer.
    pub(crate) no_answer_below: Vec<NoAnswerBelow>,
}

/// One member a `member_call` line's callee reported as giving it no answer.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct NoAnswerBelow {
    pub(crate) member: String,
    pub(crate) status: String,
}

/// How a call row names the members its callee reported with no answer, or `None` for none:
/// `no answer below: q (timed_out), r (rejected)`.
pub(crate) fn no_answer_below_note(below: &[NoAnswerBelow]) -> Option<String> {
    (!below.is_empty()).then(|| {
        let named = below
            .iter()
            .map(|entry| format!("{} ({})", entry.member, entry.status))
            .collect::<Vec<_>>()
            .join(", ");
        format!("no answer below: {named}")
    })
}

/// One `member_call_start` line: a `call-member` call whose callee holds the task.
#[derive(Debug, Deserialize)]
pub(crate) struct MemberCallStartLine {
    #[serde(default)]
    pub(crate) task_id: String,
    pub(crate) call_id: String,
    pub(crate) member: String,
    pub(crate) member_task_id: String,
    #[serde(default)]
    pub(crate) timestamp: u64,
}

/// One `member_call_busy` line: an offer of a `call-member` call's task that the callee's door
/// turned away busy. Its `message` is always the busy refusal, so it is not read.
#[derive(Debug, Deserialize)]
pub(crate) struct MemberCallBusyLine {
    #[serde(default)]
    pub(crate) task_id: String,
    pub(crate) call_id: String,
    pub(crate) member: String,
    #[serde(default)]
    pub(crate) offer: u32,
    #[serde(default)]
    pub(crate) waited_ms: u64,
    #[serde(default)]
    pub(crate) timestamp: u64,
}

/// One `member_call` line: how a `call-member` call ended. A call the callee never held names no
/// task.
#[derive(Debug, Deserialize)]
pub(crate) struct MemberCallLine {
    #[serde(default)]
    pub(crate) task_id: String,
    pub(crate) call_id: String,
    pub(crate) member: String,
    #[serde(default)]
    pub(crate) member_task_id: Option<String>,
    pub(crate) status: String,
    #[serde(default)]
    pub(crate) duration_ms: u64,
    #[serde(default)]
    pub(crate) delivered: bool,
    #[serde(default)]
    pub(crate) timestamp: u64,
    #[serde(default)]
    pub(crate) no_answer_below: Vec<NoAnswerBelow>,
}

/// The first call with `call_id` its callee does not yet hold.
fn open_call<'a>(
    calls: &'a mut [MemberCallRecord],
    call_id: &str,
) -> Option<&'a mut MemberCallRecord> {
    calls
        .iter_mut()
        .find(|call| call.call_id == call_id && call.member_task_id.is_none())
}

/// Fold one `member_call_busy` line into `calls`: it counts against the call with its call id
/// that has no ending yet, or opens that call, outstanding and not yet held.
///
/// With [`fold_member_call_start`] and [`fold_member_call`], the one rule a call's lines join by,
/// for `mur trace show`'s Member calls section and for the formation's call list alike.
pub(crate) fn fold_member_call_busy(calls: &mut Vec<MemberCallRecord>, line: MemberCallBusyLine) {
    match calls
        .iter_mut()
        .find(|call| call.call_id == line.call_id && call.status.is_none())
    {
        Some(call) => call.busy_offers += 1,
        None => calls.push(MemberCallRecord {
            call_id: line.call_id,
            member: line.member,
            calling_task_id: line.task_id,
            member_task_id: None,
            started_ms: line.timestamp.saturating_sub(line.waited_ms),
            status: None,
            duration_ms: 0,
            delivered: false,
            busy_offers: 1,
            no_answer_below: Vec::new(),
        }),
    }
}

/// Fold one `member_call_start` line into `calls`: the callee now holds the call with its call id
/// that it did not hold before — one it had turned away busy — or the line opens a call of its
/// own.
pub(crate) fn fold_member_call_start(calls: &mut Vec<MemberCallRecord>, line: MemberCallStartLine) {
    match open_call(calls, &line.call_id) {
        Some(call) => call.member_task_id = Some(line.member_task_id),
        None => calls.push(MemberCallRecord {
            call_id: line.call_id,
            member: line.member,
            calling_task_id: line.task_id,
            member_task_id: Some(line.member_task_id),
            started_ms: line.timestamp,
            status: None,
            duration_ms: 0,
            delivered: false,
            busy_offers: 0,
            no_answer_below: Vec::new(),
        }),
    }
}

/// Fold one `member_call` line into `calls`: it ends the first call with its call id that has no
/// ending yet, or is a call of its own, one its callee never held.
pub(crate) fn fold_member_call(calls: &mut Vec<MemberCallRecord>, line: MemberCallLine) {
    match calls
        .iter_mut()
        .find(|call| call.call_id == line.call_id && call.status.is_none())
    {
        Some(call) => {
            call.status = Some(line.status);
            call.duration_ms = line.duration_ms;
            call.delivered = line.delivered;
            call.no_answer_below = line.no_answer_below;
            if call.member_task_id.is_none() {
                call.member_task_id = line.member_task_id;
            }
        }
        None => calls.push(MemberCallRecord {
            call_id: line.call_id,
            member: line.member,
            calling_task_id: line.task_id,
            member_task_id: line.member_task_id,
            started_ms: line.timestamp.saturating_sub(line.duration_ms),
            status: Some(line.status),
            duration_ms: line.duration_ms,
            delivered: line.delivered,
            busy_offers: 0,
            no_answer_below: line.no_answer_below,
        }),
    }
}

/// One task a session's door let in, from its `a2a_task_received`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReceivedTask {
    pub(crate) task_id: String,
    /// The `mcl_` call id the caller minted, read from the message id `msg_<call id>`; `None`
    /// for a message id of any other shape.
    pub(crate) call_id: Option<String>,
    /// Who sent it.
    pub(crate) from: ReceivedFrom,
    pub(crate) received_ms: u64,
    /// `exit_status` of the task's `task_end`; `None` while it has none.
    pub(crate) ending: Option<String>,
    /// `duration_ms` of the task's `task_end`; 0 while it has none.
    pub(crate) duration_ms: u64,
    /// `message_sha256`, when the line carries a well-formed one: `None` under `capture: none`
    /// and in a trace older than the field.
    pub(crate) message_sha256: Option<String>,
    /// Whether `<session dir>/blobs/<message_sha256>` exists, so `mur trace show --body` can
    /// print the message.
    pub(crate) message_stored: bool,
}

/// Who sent a received task, as its `a2a_task_received` names it.
///
/// | `caller` | `caller_member` | `caller_credential` | Sender |
/// |---|---|---|---|
/// | `member` or absent | the name | - | [`Self::Member`] |
/// | `operator` | - | - | [`Self::Operator`] |
/// | `credential` | - | the name | [`Self::Credential`] |
/// | `public` | - | - | [`Self::Public`] |
/// | anything else, or a name missing | | | [`Self::Unknown`] |
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReceivedFrom {
    /// A formation member's `call-member` call, by the caller's roster name.
    Member(String),
    /// The holder of the session's operator token.
    Operator,
    /// The holder of a declared credential's token, by the credential's name.
    Credential(String),
    /// A caller of a door with no `network.authentication`.
    Public,
    /// A line that does not say who sent it: an older trace's task no member sent, or a `caller`
    /// this reader does not know.
    Unknown,
}

impl ReceivedFrom {
    /// The sender an `a2a_task_received` line's caller fields name.
    fn of(
        caller: Option<&str>,
        caller_member: Option<&str>,
        caller_credential: Option<&str>,
    ) -> Self {
        match (caller, caller_member, caller_credential) {
            (Some("member") | None, Some(member), _) => Self::Member(member.to_string()),
            (Some("operator"), _, _) => Self::Operator,
            (Some("public"), _, _) => Self::Public,
            (Some("credential"), _, Some(credential)) => Self::Credential(credential.to_string()),
            _ => Self::Unknown,
        }
    }

    /// The kind of sender, as a count names it: `operator`, `member`, `credential`, `public` or
    /// `unknown caller`.
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::Operator => "operator",
            Self::Member(_) => "member",
            Self::Credential(_) => "credential",
            Self::Public => "public",
            Self::Unknown => "unknown caller",
        }
    }

    /// Where [`Self::kind`] sorts among the kinds: operator, member, credential, public, unknown
    /// caller.
    fn rank(&self) -> usize {
        match self {
            Self::Operator => 0,
            Self::Member(_) => 1,
            Self::Credential(_) => 2,
            Self::Public => 3,
            Self::Unknown => 4,
        }
    }

    /// The sender as a row names it: the member's roster name, `operator`, `credential <name>`,
    /// `public` or `unknown caller`.
    pub(crate) fn label(&self) -> String {
        match self {
            Self::Member(member) => member.clone(),
            Self::Credential(credential) => format!("credential {credential}"),
            Self::Operator | Self::Public | Self::Unknown => self.kind().to_string(),
        }
    }
}

/// How many of `senders` are of each kind, the kinds in [`ReceivedFrom::rank`] order and each
/// present at least once: `[("operator", 1), ("public", 2)]`.
pub(crate) fn sender_kind_counts<'a>(
    senders: impl IntoIterator<Item = &'a ReceivedFrom>,
) -> Vec<(&'static str, usize)> {
    let mut counts = [0usize; 5];
    let mut kinds = [""; 5];
    for sender in senders {
        counts[sender.rank()] += 1;
        kinds[sender.rank()] = sender.kind();
    }
    kinds
        .into_iter()
        .zip(counts)
        .filter(|(_, count)| *count > 0)
        .collect()
}

/// `message_sha256` as a blob name: 64 lowercase hex digits, or `None` for any other value, so
/// no line can name a path outside the blob directory.
fn blob_name(sha: Option<String>) -> Option<String> {
    sha.filter(|sha| {
        sha.len() == 64
            && sha
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

/// The call id a `call-member` message id carries: `msg_mcl_…` is call `mcl_…`.
fn call_id_of_message(message_id: &str) -> Option<String> {
    message_id
        .strip_prefix("msg_")
        .filter(|rest| rest.starts_with("mcl_"))
        .map(str::to_string)
}

/// What one `session_start` line says about a session.
#[derive(Deserialize)]
struct SessionStartLine {
    #[serde(default)]
    formation_id: Option<String>,
    #[serde(default)]
    capsule_name: Option<String>,
    #[serde(default)]
    capsule_version: Option<String>,
    #[serde(default)]
    spawned_by: Option<String>,
    #[serde(default)]
    formation_member: Option<String>,
    #[serde(default)]
    formation_callees: Vec<String>,
}

#[derive(Deserialize)]
struct A2aTaskReceivedLine {
    task_id: String,
    #[serde(default)]
    message_id: String,
    #[serde(flatten)]
    caller: ReceivedCallerFields,
    #[serde(default)]
    timestamp: u64,
}

/// The caller fields and message hash of an `a2a_task_received` line. Every one is optional: a
/// trace older than them carries none, and its task is from an unknown caller.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ReceivedCallerFields {
    #[serde(default)]
    caller: Option<String>,
    #[serde(default)]
    caller_member: Option<String>,
    #[serde(default)]
    caller_credential: Option<String>,
    #[serde(default)]
    pub(crate) message_sha256: Option<String>,
}

impl ReceivedCallerFields {
    /// Who the line says sent the task.
    pub(crate) fn sender(&self) -> ReceivedFrom {
        ReceivedFrom::of(
            self.caller.as_deref(),
            self.caller_member.as_deref(),
            self.caller_credential.as_deref(),
        )
    }
}

/// The trace lines this reader reads, by `event_type`. Each type reads its own fields only, so a
/// field one event spells differently never makes another event's line fail to parse.
#[derive(Deserialize)]
#[serde(tag = "event_type", rename_all = "snake_case")]
enum Line {
    SessionStart(SessionStartLine),
    SessionEnd {
        #[serde(default)]
        exit_status: Option<String>,
    },
    DelegationStart {
        #[serde(default)]
        child_session_id: Option<String>,
    },
    MemberCallBusy(MemberCallBusyLine),
    MemberCallStart(MemberCallStartLine),
    MemberCall(MemberCallLine),
    A2aTaskReceived(A2aTaskReceivedLine),
    TaskEnd {
        task_id: String,
        #[serde(default)]
        exit_status: Option<String>,
        #[serde(default)]
        duration_ms: u64,
    },
    #[serde(other)]
    Other,
}

/// The formation `<session_dir>/trace.jsonl` names on its first line, or `None` when it names
/// none: a missing or unreadable file, a first line that is not JSON or not `session_start`, or a
/// `session_start` with no `formation_id`.
pub(crate) fn session_formation_id(session_dir: &Path) -> Option<String> {
    first_line(session_dir)?.formation_id
}

/// The first non-empty line of a session's trace, if it is a `session_start`.
fn first_line(session_dir: &Path) -> Option<SessionStartLine> {
    let file = fs::File::open(session_dir.join("trace.jsonl")).ok()?;
    let line = BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .find(|line| !line.trim().is_empty())?;
    match serde_json::from_str::<Line>(line.trim()).ok()? {
        Line::SessionStart(start) => Some(start),
        _ => None,
    }
}

/// Every session under `root` whose `session_start` names `formation`.
///
/// A root that does not exist holds no member. The `Err` is a root that exists and cannot be
/// listed, `E-IO-003`; each caller decides whether that fails it. A session directory minted
/// before the formation id is not opened. A member's trace is read to its end with every line
/// that does not parse skipped, so a writer killed mid-line costs only that line.
pub(crate) fn recorded_members(
    root: &Path,
    formation: &FormationId,
) -> Result<RecordedMembers, CliError> {
    let minted = formation.minted_at_ms();
    let mut names = ses_entries(root)?;
    names.sort();

    let mut members = Vec::new();
    for name in names {
        if matches!(session_id_timestamp_ms(&name), Some(at) if at < minted) {
            continue;
        }
        let session_dir = root.join(&name);
        if session_formation_id(&session_dir).as_deref() == Some(formation.as_str()) {
            members.push(read_member(&session_dir, name));
        }
    }

    let found: BTreeSet<&str> = members.iter().map(|m| m.session_id.as_str()).collect();
    let children_elsewhere: BTreeSet<String> = members
        .iter()
        .flat_map(|member| member.delegated_children.iter())
        .filter(|child| !found.contains(child.as_str()))
        .cloned()
        .collect();
    Ok(RecordedMembers {
        members,
        children_elsewhere: children_elsewhere.into_iter().collect(),
    })
}

/// One root [`search_formation`] searched, and why it could not be read when it could not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SearchedRoot {
    pub(crate) root: PathBuf,
    /// `None` for a root that was read.
    pub(crate) unreadable: Option<String>,
}

/// What every searched root records about one formation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct FormationSearch {
    /// Every root searched, in the order searched.
    pub(crate) searched: Vec<SearchedRoot>,
    /// Every member found under any of them, once each, by session id ascending.
    pub(crate) found: RecordedMembers,
}

/// Every session under `first`, then under each of `more`, whose `session_start` names
/// `formation`.
///
/// `first` is the root the caller was given, and one that cannot be listed fails the search as
/// [`recorded_members`] does. A root among `more` that cannot be listed is reported in
/// [`FormationSearch::searched`] and fails nothing. A session found under two roots is listed
/// once; `more` repeating `first` is searched once.
pub(crate) fn search_formation(
    first: &Path,
    more: Vec<PathBuf>,
    formation: &FormationId,
) -> Result<FormationSearch, CliError> {
    let mut members = recorded_members(first, formation)?.members;
    let mut searched = vec![SearchedRoot {
        root: first.to_path_buf(),
        unreadable: None,
    }];
    for root in more {
        if searched.iter().any(|done| done.root == root) {
            continue;
        }
        match recorded_members(&root, formation) {
            Ok(found) => {
                for member in found.members {
                    if !members.iter().any(|m| m.session_id == member.session_id) {
                        members.push(member);
                    }
                }
                searched.push(SearchedRoot {
                    root,
                    unreadable: None,
                });
            }
            Err(error) => searched.push(SearchedRoot {
                root,
                unreadable: Some(error.message),
            }),
        }
    }
    members.sort_by(|a, b| a.session_id.cmp(&b.session_id));
    let found: BTreeSet<&str> = members.iter().map(|m| m.session_id.as_str()).collect();
    let children_elsewhere: BTreeSet<String> = members
        .iter()
        .flat_map(|member| member.delegated_children.iter())
        .filter(|child| !found.contains(child.as_str()))
        .cloned()
        .collect();
    Ok(FormationSearch {
        searched,
        found: RecordedMembers {
            members,
            children_elsewhere: children_elsewhere.into_iter().collect(),
        },
    })
}

/// How one row of a formation's call list ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CallEnding {
    /// The caller recorded a `member_call`.
    Ended {
        status: String,
        duration_ms: u64,
        delivered: bool,
    },
    /// The caller recorded a `member_call_start` and no `member_call`.
    Outstanding,
    /// No caller trace was found, so nothing says how the call ended.
    Unknown,
}

/// The side of a call the found traces are missing, at most one per call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CallGap {
    /// The call ended without the callee's door ever holding it.
    NeverHeld,
    /// The callee's door held a task no found trace received.
    CalleeTraceNotFound,
    /// An outstanding call whose callee task was found: how that task ended, `None` for one with
    /// no `task_end`.
    CalleeTaskEnded(Option<String>),
    /// The callee received the task and no caller trace records the call: how the callee's task
    /// ended, `None` for one with no `task_end`.
    CallerTraceNotFound(Option<String>),
}

impl CallGap {
    /// The note a call row carries for this gap.
    pub(crate) fn note(&self, caller: &str, callee: &str) -> String {
        let task_ended = |ending: &Option<String>| match ending {
            Some(status) => format!("{callee}'s task ended {status}"),
            None => format!("{callee}'s task has no task_end"),
        };
        match self {
            Self::NeverHeld => format!("never held by {callee}"),
            Self::CalleeTraceNotFound => format!("{callee}'s trace not found"),
            Self::CalleeTaskEnded(ending) => task_ended(ending),
            Self::CallerTraceNotFound(ending) => {
                format!("{caller}'s trace not found; {}", task_ended(ending))
            }
        }
    }
}

/// One `call-member` call in a formation, joined from whichever of its two sides was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FormationCall {
    /// How many rows this one is nested under: 0 for a call no found row handed its task over.
    pub(crate) depth: usize,
    /// The `mcl_` id; `None` for a received task whose message id carries none.
    pub(crate) call_id: Option<String>,
    /// The caller's roster name, or its session id when it has none.
    pub(crate) caller: String,
    /// The callee's roster name.
    pub(crate) callee: String,
    pub(crate) started_ms: u64,
    pub(crate) ending: CallEnding,
    pub(crate) gap: Option<CallGap>,
    /// The caller's task the call was made from; `None` for a call only its callee recorded.
    pub(crate) calling_task_id: Option<String>,
    /// The callee's task, `None` for a call it never held.
    pub(crate) member_task_id: Option<String>,
    /// How many offers of the task the callee turned away busy.
    pub(crate) busy_offers: u32,
    /// The members further down the callee reported as giving it no answer.
    pub(crate) no_answer_below: Vec<NoAnswerBelow>,
}

/// A task a member's door let in that no member sent: one the operator, a declared credential,
/// a public-door caller or an unnamed caller sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FormationTask {
    /// How many rows this one is nested under; always 0, since no call hands over such a task.
    pub(crate) depth: usize,
    pub(crate) task_id: String,
    /// Who sent it; never [`ReceivedFrom::Member`].
    pub(crate) from: ReceivedFrom,
    /// The receiving member's roster name, or its session id when it has none.
    pub(crate) member: String,
    pub(crate) received_ms: u64,
    /// `exit_status` of the task's `task_end`; `None` while it is outstanding.
    pub(crate) ending: Option<String>,
    /// `duration_ms` of the task's `task_end`.
    pub(crate) duration_ms: u64,
    /// Where the message text is stored, `None` when no blob holds it.
    pub(crate) message: Option<StoredMessage>,
}

/// A received message stored behind its hash in a member's session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoredMessage {
    /// The member's `<session dir>/trace.jsonl`.
    pub(crate) trace: PathBuf,
    /// The message's `sha256`, the blob's name.
    pub(crate) sha256: String,
}

/// How many hex digits of a message hash a task row's note gives `--body`.
pub(crate) const MESSAGE_SHA_SHOWN: usize = 12;

impl FormationTask {
    /// The row's note: the command that prints the message, or that it was not captured.
    pub(crate) fn note(&self) -> String {
        match &self.message {
            Some(message) => format!(
                "message: mur trace show {} --body {}",
                message.trace.display(),
                &message.sha256[..MESSAGE_SHA_SHOWN]
            ),
            None => "message not captured".to_string(),
        }
    }
}

/// One row of a formation's listing: a `call-member` call, or a task no member sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FormationRow {
    Call(FormationCall),
    Task(FormationTask),
}

impl FormationRow {
    fn set_depth(&mut self, depth: usize) {
        match self {
            Self::Call(call) => call.depth = depth,
            Self::Task(task) => task.depth = depth,
        }
    }

    /// The task this row hands a member, and that member: what a call made from it nests under.
    fn holds(&self) -> Option<(&str, &str)> {
        match self {
            Self::Call(call) => Some((call.member_task_id.as_deref()?, call.callee.as_str())),
            Self::Task(task) => Some((task.task_id.as_str(), task.member.as_str())),
        }
    }

    /// The task this row was made from, and the member holding it; `None` for a row that was
    /// made from no task found.
    fn made_from(&self) -> Option<(&str, &str)> {
        match self {
            Self::Call(call) => Some((call.calling_task_id.as_deref()?, call.caller.as_str())),
            Self::Task(_) => None,
        }
    }

    /// The order rows sort in among their siblings: when they started, then their id.
    fn sort_key(&self) -> (u64, Option<String>) {
        match self {
            Self::Call(call) => (call.started_ms, call.call_id.clone()),
            Self::Task(task) => (task.received_ms, Some(task.task_id.clone())),
        }
    }
}

/// Every `call-member` call `members` record, once each, and every task a member received that
/// no member sent, in display order.
///
/// A call's caller side is its caller's [`MemberCallRecord`]; its callee side is the member's
/// [`ReceivedTask`] whose task id is the call's `member_task_id`. Every caller-side call is a
/// row, and so is every member's received task no caller-side call matched. Every received task
/// no member sent is a [`FormationTask`] row. A call row's parent is the row that handed over the
/// task it was made from: the row whose task is its calling task and whose member is its caller.
/// Rows with no parent come first by `(started, id)`, a task row's id being its task id, each
/// followed by its children, ordered the same way, one level deeper. Calls whose parents form a
/// cycle have no root to be reached from: they follow every other row, each cycle starting at its
/// earliest call with the rest nested beneath it, so every row is listed exactly once.
pub(crate) fn formation_rows(members: &[RecordedMember]) -> Vec<FormationRow> {
    // Each member task's callee and how it ended, by task id.
    let mut received: HashMap<&str, (&RecordedMember, &ReceivedTask)> = HashMap::new();
    for member in members {
        for task in &member.tasks_received {
            if matches!(task.from, ReceivedFrom::Member(_)) {
                received
                    .entry(task.task_id.as_str())
                    .or_insert((member, task));
            }
        }
    }
    let mut matched: BTreeSet<&str> = BTreeSet::new();
    let mut rows: Vec<FormationRow> = Vec::new();
    for member in members {
        for call in &member.calls_made {
            let callee_task = call
                .member_task_id
                .as_deref()
                .and_then(|task_id| received.get(task_id));
            if let Some((_, task)) = callee_task {
                matched.insert(task.task_id.as_str());
            }
            let gap = match (&call.status, &call.member_task_id, callee_task) {
                (Some(_), None, _) => Some(CallGap::NeverHeld),
                (_, Some(_), None) => Some(CallGap::CalleeTraceNotFound),
                (None, _, Some((_, task))) => Some(CallGap::CalleeTaskEnded(task.ending.clone())),
                (Some(_), Some(_), Some(_)) | (None, None, _) => None,
            };
            rows.push(FormationRow::Call(FormationCall {
                depth: 0,
                call_id: Some(call.call_id.clone()),
                caller: member.name().to_string(),
                callee: call.member.clone(),
                started_ms: call.started_ms,
                ending: match &call.status {
                    Some(status) => CallEnding::Ended {
                        status: status.clone(),
                        duration_ms: call.duration_ms,
                        delivered: call.delivered,
                    },
                    None => CallEnding::Outstanding,
                },
                gap,
                calling_task_id: Some(call.calling_task_id.clone()),
                member_task_id: call.member_task_id.clone(),
                busy_offers: call.busy_offers,
                no_answer_below: call.no_answer_below.clone(),
            }));
        }
    }
    for member in members {
        for task in &member.tasks_received {
            let row = match &task.from {
                ReceivedFrom::Member(_) if matched.contains(task.task_id.as_str()) => continue,
                ReceivedFrom::Member(caller) => FormationRow::Call(FormationCall {
                    depth: 0,
                    call_id: task.call_id.clone(),
                    caller: caller.clone(),
                    callee: member.name().to_string(),
                    started_ms: task.received_ms,
                    ending: CallEnding::Unknown,
                    gap: Some(CallGap::CallerTraceNotFound(task.ending.clone())),
                    calling_task_id: None,
                    member_task_id: Some(task.task_id.clone()),
                    busy_offers: 0,
                    no_answer_below: Vec::new(),
                }),
                from => FormationRow::Task(FormationTask {
                    depth: 0,
                    task_id: task.task_id.clone(),
                    from: from.clone(),
                    member: member.name().to_string(),
                    received_ms: task.received_ms,
                    ending: task.ending.clone(),
                    duration_ms: task.duration_ms,
                    message: task
                        .message_sha256
                        .as_ref()
                        .filter(|_| task.message_stored)
                        .map(|sha| StoredMessage {
                            trace: member.session_dir.join("trace.jsonl"),
                            sha256: sha.clone(),
                        }),
                }),
            };
            rows.push(row);
        }
    }
    nest(rows)
}

/// `rows` in display order, each with its depth: see [`formation_rows`].
fn nest(rows: Vec<FormationRow>) -> Vec<FormationRow> {
    let by_task: HashMap<(&str, &str), usize> = rows
        .iter()
        .enumerate()
        .filter_map(|(i, row)| Some((row.holds()?, i)))
        .collect();
    let parent: Vec<Option<usize>> = rows
        .iter()
        .map(|row| by_task.get(&row.made_from()?).copied())
        .collect();
    let key = |i: &usize| rows[*i].sort_key();
    let mut children: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    let mut roots = Vec::new();
    for (i, parent) in parent.iter().enumerate() {
        match parent {
            Some(parent) => children.entry(*parent).or_default().push(i),
            None => roots.push(i),
        }
    }
    for siblings in children.values_mut() {
        siblings.sort_by_key(key);
    }
    roots.sort_by_key(key);
    // A cycle has no root to be reached from; its rows are listed after the rest, from the first
    // of them in order.
    let mut rest: Vec<usize> = (0..rows.len()).collect();
    rest.sort_by_key(key);

    let mut order: Vec<(usize, usize)> = Vec::with_capacity(rows.len());
    let mut visited = vec![false; rows.len()];
    for start in roots.into_iter().chain(rest) {
        let mut stack = vec![(start, 0)];
        while let Some((i, depth)) = stack.pop() {
            if std::mem::replace(&mut visited[i], true) {
                continue;
            }
            order.push((i, depth));
            if let Some(siblings) = children.get(&i) {
                stack.extend(siblings.iter().rev().map(|child| (*child, depth + 1)));
            }
        }
    }
    let mut rows: Vec<Option<FormationRow>> = rows.into_iter().map(Some).collect();
    order
        .into_iter()
        .filter_map(|(i, depth)| {
            let mut row = rows[i].take()?;
            row.set_depth(depth);
            Some(row)
        })
        .collect()
}

/// A member's whole trace, read once for its identity, its ending, its delegations, the calls it
/// made and the tasks it received.
fn read_member(session_dir: &Path, session_id: String) -> RecordedMember {
    let mut member = RecordedMember {
        session_id,
        session_dir: session_dir.to_path_buf(),
        capsule: String::new(),
        member: None,
        callees: Vec::new(),
        spawned_by: None,
        exit_status: None,
        delegated_children: Vec::new(),
        calls_made: Vec::new(),
        tasks_received: Vec::new(),
    };
    let Ok(file) = fs::File::open(session_dir.join("trace.jsonl")) else {
        return member;
    };
    let mut started = false;
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let Ok(line) = serde_json::from_str::<Line>(line.trim()) else {
            continue;
        };
        match line {
            Line::SessionStart(start) if !started => {
                started = true;
                member.capsule = format!(
                    "{}@{}",
                    start.capsule_name.unwrap_or_default(),
                    start.capsule_version.unwrap_or_default()
                );
                member.spawned_by = start.spawned_by;
                member.member = start.formation_member;
                member.callees = start.formation_callees;
            }
            Line::SessionEnd { exit_status } if member.exit_status.is_none() => {
                member.exit_status = Some(exit_status.unwrap_or_default());
            }
            Line::DelegationStart { child_session_id } => {
                member.delegated_children.extend(child_session_id)
            }
            Line::MemberCallBusy(busy) => fold_member_call_busy(&mut member.calls_made, busy),
            Line::MemberCallStart(start) => fold_member_call_start(&mut member.calls_made, start),
            Line::MemberCall(end) => fold_member_call(&mut member.calls_made, end),
            Line::A2aTaskReceived(received) => {
                let from = received.caller.sender();
                let message_sha256 = blob_name(received.caller.message_sha256);
                let message_stored = message_sha256.as_ref().is_some_and(|sha| {
                    session_dir
                        .join(crate::commands::trace::BLOB_DIR_NAME)
                        .join(sha)
                        .is_file()
                });
                member.tasks_received.push(ReceivedTask {
                    call_id: call_id_of_message(&received.message_id),
                    task_id: received.task_id,
                    from,
                    received_ms: received.timestamp,
                    ending: None,
                    duration_ms: 0,
                    message_sha256,
                    message_stored,
                });
            }
            Line::TaskEnd {
                task_id,
                exit_status,
                duration_ms,
            } => {
                if let Some(task) = member
                    .tasks_received
                    .iter_mut()
                    .find(|task| task.task_id == task_id && task.ending.is_none())
                {
                    task.ending = Some(exit_status.unwrap_or_default());
                    task.duration_ms = duration_ms;
                }
            }
            _ => {}
        }
    }
    member
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A `ses_` id minted `offset_ms` after `formation`, ending in `tail`.
    fn session_after(formation: &FormationId, offset_ms: i64, tail: &str) -> String {
        let at = (formation.minted_at_ms() as i64 + offset_ms) as u64;
        format!("ses_{at:012x}{tail:0>20}")
    }

    fn write_trace(root: &Path, session_id: &str, lines: &[String]) {
        let dir = root.join(session_id);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("trace.jsonl"), lines.join("\n") + "\n").unwrap();
    }

    fn start(session_id: &str, formation: Option<&str>, spawned_by: Option<&str>) -> String {
        let mut line = json!({
            "event_type": "session_start",
            "session_id": session_id,
            "capsule_name": "member",
            "capsule_version": "0.1.0",
            "model": "m",
            "max_turns": 3,
        });
        if let Some(id) = formation {
            line["formation_id"] = id.into();
        }
        if let Some(parent) = spawned_by {
            line["spawned_by"] = parent.into();
        }
        line.to_string()
    }

    fn end(status: &str) -> String {
        json!({"event_type": "session_end", "exit_status": status}).to_string()
    }

    fn delegation(child: &str) -> String {
        json!({
            "event_type": "delegation_start",
            "delegation_id": "dlg_1",
            "capsule": "child",
            "version": "0.1.0",
            "child_session_id": child,
            "child_workdir": "/elsewhere",
        })
        .to_string()
    }

    /// A formation's peers record under their own directories in the murmur home: the search
    /// reads the given root and then each peer's root, lists every member once, and reports a
    /// root it could not read without failing.
    #[test]
    fn a_formation_is_found_across_the_project_root_and_its_peers_roots() {
        let formation = FormationId::mint();
        let home = tempfile::tempdir().unwrap();
        let _home = crate::config::test_env::EnvGuard::set_up(home.path(), home.path());
        let project = tempfile::tempdir().unwrap();
        let root = project.path().join(".murmur");
        let entry = session_after(&formation, 30, "1");
        write_trace(
            &root,
            &entry,
            &[start(&entry, Some(formation.as_str()), None), end("ok")],
        );
        let formations = home.path().join("formations");
        std::fs::create_dir_all(formations.join(formation.as_str()).join("worker")).unwrap();
        let worker_root =
            capsule_runtime::records_group(&formations.join(formation.as_str()).join("worker"))
                .unwrap();
        let worker = session_after(&formation, 10, "2");
        write_trace(
            &worker_root,
            &worker,
            &[
                start(&worker, Some(formation.as_str()), None),
                end("canceled"),
            ],
        );
        let roots =
            capsule_runtime::formation_launch::formation_member_roots_in(&formations, &formation);
        assert_eq!(roots, std::slice::from_ref(&worker_root));

        // A directory this user may not list.
        let unreadable = home.path().join("closed");
        fs::create_dir(&unreadable).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).unwrap();
        let mut more = roots.clone();
        more.push(root.clone());
        more.push(unreadable.clone());
        let search = search_formation(&root, more, &formation).unwrap();
        let sessions: Vec<&str> = search
            .found
            .members
            .iter()
            .map(|member| member.session_id.as_str())
            .collect();
        assert_eq!(sessions, [worker.as_str(), entry.as_str()]);
        let searched: Vec<(&Path, bool)> = search
            .searched
            .iter()
            .map(|root| (root.root.as_path(), root.unreadable.is_some()))
            .collect();
        assert_eq!(
            searched,
            [
                (root.as_path(), false),
                (worker_root.as_path(), false),
                (unreadable.as_path(), fs::read_dir(&unreadable).is_err())
            ]
        );
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn members_are_found_by_their_first_line_and_nothing_else() {
        let root = tempfile::tempdir().unwrap();
        let formation = FormationId::mint();
        let other = FormationId::mint();
        let ended = session_after(&formation, 10, "a");
        let killed = session_after(&formation, 20, "b");
        let standalone = session_after(&formation, 30, "c");
        let stranger = session_after(&formation, 40, "d");
        let older = session_after(&formation, -1_000, "e");
        let drained = session_after(&formation, 50, "f");
        let empty = session_after(&formation, 60, "0");
        let child_here = session_after(&formation, 70, "1");
        let child_away = session_after(&formation, 80, "2");

        write_trace(
            root.path(),
            &ended,
            &[
                start(&ended, Some(formation.as_str()), None),
                delegation(&child_here),
                delegation(&child_away),
                end("completed"),
                end("a second ending is not the first"),
            ],
        );
        write_trace(
            root.path(),
            &killed,
            &[
                start(&killed, Some(formation.as_str()), None),
                json!({"event_type": "task_start", "task_id": "tsk_1"}).to_string(),
                r#"{"event_type":"inference","tur"#.to_string(),
            ],
        );
        write_trace(
            root.path(),
            &child_here,
            &[
                start(&child_here, Some(formation.as_str()), Some(&ended)),
                end("completed"),
            ],
        );
        write_trace(
            root.path(),
            &standalone,
            &[start(&standalone, None, None), end("completed")],
        );
        write_trace(
            root.path(),
            &stranger,
            &[
                start(&stranger, Some(other.as_str()), None),
                end("completed"),
            ],
        );
        // Older than the formation: never opened, so even a matching first line is not read.
        write_trace(
            root.path(),
            &older,
            &[
                start(&older, Some(formation.as_str()), None),
                end("completed"),
            ],
        );
        // A trace whose first line is not session_start: the formation is named, but not on it.
        write_trace(
            root.path(),
            &drained,
            &[
                json!({"event_type": "tool_call", "formation_id": formation.as_str()}).to_string(),
                start(&drained, Some(formation.as_str()), None),
            ],
        );
        fs::create_dir_all(root.path().join(&empty)).unwrap();
        fs::create_dir_all(root.path().join("not-a-session")).unwrap();

        let found = recorded_members(root.path(), &formation).unwrap();
        assert_eq!(
            found.members,
            vec![
                RecordedMember {
                    session_id: ended.clone(),
                    session_dir: root.path().join(&ended),
                    capsule: "member@0.1.0".to_string(),
                    member: None,
                    callees: Vec::new(),
                    spawned_by: None,
                    exit_status: Some("completed".to_string()),
                    delegated_children: vec![child_here.clone(), child_away.clone()],
                    calls_made: Vec::new(),
                    tasks_received: Vec::new(),
                },
                RecordedMember {
                    session_id: killed.clone(),
                    session_dir: root.path().join(&killed),
                    capsule: "member@0.1.0".to_string(),
                    member: None,
                    callees: Vec::new(),
                    spawned_by: None,
                    exit_status: None,
                    delegated_children: Vec::new(),
                    calls_made: Vec::new(),
                    tasks_received: Vec::new(),
                },
                RecordedMember {
                    session_id: child_here.clone(),
                    session_dir: root.path().join(&child_here),
                    capsule: "member@0.1.0".to_string(),
                    member: None,
                    callees: Vec::new(),
                    spawned_by: Some(ended.clone()),
                    exit_status: Some("completed".to_string()),
                    delegated_children: Vec::new(),
                    calls_made: Vec::new(),
                    tasks_received: Vec::new(),
                },
            ]
        );
        assert_eq!(found.children_elsewhere, vec![child_away]);

        assert_eq!(
            session_formation_id(&root.path().join(&ended)).as_deref(),
            Some(formation.as_str())
        );
        assert_eq!(session_formation_id(&root.path().join(&standalone)), None);
        assert_eq!(session_formation_id(&root.path().join(&drained)), None);
        assert_eq!(session_formation_id(&root.path().join(&empty)), None);
    }

    #[test]
    fn a_missing_root_holds_no_member() {
        let root = tempfile::tempdir().unwrap();
        let found =
            recorded_members(&root.path().join("never-created"), &FormationId::mint()).unwrap();
        assert_eq!(found, RecordedMembers::default());
    }

    #[test]
    fn a_first_line_that_is_not_json_names_no_formation() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("ses_x");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("trace.jsonl"), "\n\n{\"event_type\":\"session_sta").unwrap();
        assert_eq!(session_formation_id(&dir), None);
    }

    // ── Calls ─────────────────────────────────────────────────────────────────

    /// A member's `session_start`: its roster name, its callees and its capsule.
    fn member_start(
        session_id: &str,
        formation: &FormationId,
        member: Option<&str>,
        callees: &[&str],
        capsule: &str,
    ) -> String {
        let mut line = json!({
            "event_type": "session_start",
            "event_id": "evt_0",
            "session_id": session_id,
            "timestamp": 1,
            "capsule_name": capsule,
            "capsule_version": "0.1.0",
            "formation_id": formation.as_str(),
        });
        if let Some(member) = member {
            line["formation_member"] = member.into();
            line["formation_callees"] = callees.into();
        }
        line.to_string()
    }

    fn call_start(task: &str, call: &str, member: &str, member_task: &str, at: u64) -> String {
        json!({
            "event_type": "member_call_start", "timestamp": at, "task_id": task,
            "call_id": call, "member": member, "member_task_id": member_task,
        })
        .to_string()
    }

    #[allow(clippy::too_many_arguments)]
    fn call_end(
        task: &str,
        call: &str,
        member: &str,
        member_task: Option<&str>,
        status: &str,
        duration_ms: u64,
        delivered: bool,
        at: u64,
    ) -> String {
        let mut line = json!({
            "event_type": "member_call", "timestamp": at, "task_id": task, "call_id": call,
            "member": member, "status": status, "duration_ms": duration_ms, "output": "x",
            "truncated": false, "delivered": delivered,
        });
        if let Some(member_task) = member_task {
            line["member_task_id"] = member_task.into();
        }
        line.to_string()
    }

    fn received(task: &str, message_id: &str, caller: Option<&str>, at: u64) -> String {
        let mut line = json!({
            "event_type": "a2a_task_received", "timestamp": at, "task_id": task,
            "context_id": "ctx", "message_id": message_id, "traceparent_from_caller": null,
        });
        if let Some(caller) = caller {
            line["caller_member"] = caller.into();
        }
        line.to_string()
    }

    fn task_end(task: &str, status: &str) -> String {
        json!({"event_type": "task_end", "task_id": task, "exit_status": status}).to_string()
    }

    /// An `a2a_task_received` line as this version writes it, `fields` naming the caller and
    /// the message hash.
    fn received_as(task: &str, message_id: &str, fields: serde_json::Value, at: u64) -> String {
        let mut line = json!({
            "event_type": "a2a_task_received", "timestamp": at, "task_id": task,
            "context_id": "ctx", "message_id": message_id, "traceparent_from_caller": null,
        });
        for (key, value) in fields.as_object().unwrap() {
            line[key] = value.clone();
        }
        line.to_string()
    }

    fn task_end_after(task: &str, status: &str, duration_ms: u64) -> String {
        json!({"event_type": "task_end", "task_id": task, "exit_status": status,
               "duration_ms": duration_ms})
        .to_string()
    }

    /// The call rows of [`formation_rows`], in display order.
    fn formation_calls(members: &[RecordedMember]) -> Vec<FormationCall> {
        formation_rows(members)
            .into_iter()
            .filter_map(|row| match row {
                FormationRow::Call(call) => Some(call),
                FormationRow::Task(_) => None,
            })
            .collect()
    }

    /// Every row's depth, `from→to` and note: a call's gap note, a task's message note.
    fn row_summary(rows: &[FormationRow]) -> Vec<(usize, String, String)> {
        rows.iter()
            .map(|row| match row {
                FormationRow::Call(call) => (
                    call.depth,
                    format!("{}→{}", call.caller, call.callee),
                    call.gap
                        .as_ref()
                        .map(|gap| gap.note(&call.caller, &call.callee))
                        .unwrap_or_default(),
                ),
                FormationRow::Task(task) => (
                    task.depth,
                    format!("{}→{}", task.from.label(), task.member),
                    task.note(),
                ),
            })
            .collect()
    }

    /// Every row's depth, `caller→callee` and gap note.
    fn summary(calls: &[FormationCall]) -> Vec<(usize, String, String)> {
        calls
            .iter()
            .map(|call| {
                (
                    call.depth,
                    format!("{}→{}", call.caller, call.callee),
                    call.gap
                        .as_ref()
                        .map(|gap| gap.note(&call.caller, &call.callee))
                        .unwrap_or_default(),
                )
            })
            .collect()
    }

    /// Each session's roster name and callees come from its `session_start`, never from its
    /// capsule: two members sharing one capsule keep their own names, and a member's delegated
    /// child has none.
    #[test]
    fn members_are_named_by_their_roster_name_not_their_capsule() {
        let formation = FormationId::mint();
        let root = tempfile::tempdir().unwrap();
        let lead = session_after(&formation, 10, "1");
        let (w1, w2) = (
            session_after(&formation, 20, "2"),
            session_after(&formation, 30, "3"),
        );
        let child = session_after(&formation, 40, "4");
        write_trace(
            root.path(),
            &lead,
            &[member_start(
                &lead,
                &formation,
                Some("lead"),
                &["w1", "w2"],
                "lead",
            )],
        );
        for (session, name) in [(&w1, "w1"), (&w2, "w2")] {
            write_trace(
                root.path(),
                session,
                &[member_start(session, &formation, Some(name), &[], "worker")],
            );
        }
        write_trace(
            root.path(),
            &child,
            &[member_start(&child, &formation, None, &[], "helper")],
        );
        let found = recorded_members(root.path(), &formation).unwrap().members;
        let named: Vec<(Option<&str>, &str, Vec<&str>)> = found
            .iter()
            .map(|m| {
                (
                    m.member.as_deref(),
                    m.capsule.as_str(),
                    m.callees.iter().map(String::as_str).collect(),
                )
            })
            .collect();
        assert_eq!(
            named,
            [
                (Some("lead"), "lead@0.1.0", vec!["w1", "w2"]),
                (Some("w1"), "worker@0.1.0", vec![]),
                (Some("w2"), "worker@0.1.0", vec![]),
                (None, "helper@0.1.0", vec![]),
            ]
        );
    }

    /// A call the callee's door never held — refused busy, or failing any other way — is a row
    /// noting so, with its status and no callee side.
    #[test]
    fn a_never_held_call_is_a_row_of_its_own_whether_rejected_or_failed() {
        let formation = FormationId::mint();
        let root = tempfile::tempdir().unwrap();
        let lead = session_after(&formation, 10, "1");
        write_trace(
            root.path(),
            &lead,
            &[
                member_start(&lead, &formation, Some("lead"), &["busy", "gone"], "lead"),
                call_end("tsk_l", "mcl_1", "busy", None, "rejected", 4, true, 104),
                call_end("tsk_l", "mcl_2", "gone", None, "failed", 7, true, 207),
            ],
        );
        let members = recorded_members(root.path(), &formation).unwrap().members;
        assert_eq!(members[0].calls_made[0].started_ms, 100);
        let calls = formation_calls(&members);
        assert_eq!(
            summary(&calls),
            [
                (0, "lead→busy".into(), "never held by busy".into()),
                (0, "lead→gone".into(), "never held by gone".into()),
            ]
        );
        let statuses: Vec<&CallEnding> = calls.iter().map(|call| &call.ending).collect();
        assert_eq!(
            statuses,
            [
                &CallEnding::Ended {
                    status: "rejected".into(),
                    duration_ms: 4,
                    delivered: true
                },
                &CallEnding::Ended {
                    status: "failed".into(),
                    duration_ms: 7,
                    delivered: true
                },
            ]
        );
    }

    fn call_busy(task: &str, call: &str, member: &str, offer: u32, waited: u64, at: u64) -> String {
        json!({
            "event_type": "member_call_busy", "timestamp": at, "task_id": task, "call_id": call,
            "member": member, "offer": offer, "waited_ms": waited,
            "message": "task rejected: capsule is busy",
        })
        .to_string()
    }

    /// `member_call_busy` lines fold into the call with their call id: a call turned away busy
    /// and later held is one row, counting its busy offers, started when the call was made; one
    /// turned away busy and nothing since is outstanding and not yet held; one busy until it
    /// ended `rejected` is never held.
    #[test]
    fn member_call_busy_lines_fold_into_their_call() {
        let formation = FormationId::mint();
        let root = tempfile::tempdir().unwrap();
        let lead = session_after(&formation, 10, "1");
        write_trace(
            root.path(),
            &lead,
            &[
                member_start(&lead, &formation, Some("lead"), &["c", "d", "e"], "lead"),
                call_busy("tsk_l", "mcl_1", "c", 1, 2, 102),
                call_busy("tsk_l", "mcl_2", "d", 1, 1, 151),
                call_busy("tsk_l", "mcl_3", "e", 1, 1, 161),
                call_busy("tsk_l", "mcl_1", "c", 2, 1100, 1200),
                call_start("tsk_l", "mcl_1", "c", "tsk_c", 2300),
                call_busy("tsk_l", "mcl_3", "e", 2, 1000, 1160),
                call_end(
                    "tsk_l",
                    "mcl_1",
                    "c",
                    Some("tsk_c"),
                    "completed",
                    5000,
                    true,
                    5100,
                ),
                call_end("tsk_l", "mcl_3", "e", None, "rejected", 4000, true, 4160),
            ],
        );
        let members = recorded_members(root.path(), &formation).unwrap().members;
        let made = &members[0].calls_made;
        assert_eq!(made.len(), 3, "{made:?}");
        assert_eq!(
            made[0],
            MemberCallRecord {
                call_id: "mcl_1".into(),
                member: "c".into(),
                calling_task_id: "tsk_l".into(),
                member_task_id: Some("tsk_c".into()),
                started_ms: 100,
                status: Some("completed".into()),
                duration_ms: 5000,
                delivered: true,
                no_answer_below: Vec::new(),
                busy_offers: 2,
            }
        );
        assert_eq!(made[1].member_task_id, None);
        assert_eq!(made[1].status, None);
        assert_eq!(made[1].busy_offers, 1);
        assert_eq!(made[1].started_ms, 150);
        assert_eq!(made[2].busy_offers, 2);
        let calls = formation_calls(&members);
        assert_eq!(
            summary(&calls),
            [
                (0, "lead→c".into(), "c's trace not found".into()),
                (0, "lead→d".into(), String::new()),
                (0, "lead→e".into(), "never held by e".into()),
            ]
        );
        assert_eq!(calls[1].ending, CallEnding::Outstanding);
        let busy: Vec<u32> = calls.iter().map(|call| call.busy_offers).collect();
        assert_eq!(busy, [2, 1, 2]);
    }

    /// The lead's trace, in `lead_root`, calls `worker`, whose root is `worker_root`.
    fn lead_calling_worker(formation: &FormationId, lead_root: &Path) {
        let lead = session_after(formation, 10, "1");
        write_trace(
            lead_root,
            &lead,
            &[
                member_start(&lead, formation, Some("lead"), &["worker"], "lead"),
                call_start("tsk_l", "mcl_1", "worker", "tsk_w", 50),
                call_end(
                    "tsk_l",
                    "mcl_1",
                    "worker",
                    Some("tsk_w"),
                    "completed",
                    90,
                    true,
                    140,
                ),
            ],
        );
    }

    /// A callee whose root is missing keeps the caller's row, noting the callee's trace was not
    /// found.
    #[test]
    fn a_callee_whose_root_is_absent_keeps_the_call_row() {
        let formation = FormationId::mint();
        let home = tempfile::tempdir().unwrap();
        let lead_root = home.path().join("lead");
        lead_calling_worker(&formation, &lead_root);
        let search =
            search_formation(&lead_root, vec![home.path().join("worker")], &formation).unwrap();
        assert_eq!(
            summary(&formation_calls(&search.found.members)),
            [(0, "lead→worker".into(), "worker's trace not found".into())]
        );
    }

    /// A callee whose root cannot be listed keeps the caller's row, and the root is reported.
    #[test]
    fn a_callee_whose_root_is_unreadable_keeps_the_call_row() {
        use std::os::unix::fs::PermissionsExt;
        let formation = FormationId::mint();
        let home = tempfile::tempdir().unwrap();
        let lead_root = home.path().join("lead");
        lead_calling_worker(&formation, &lead_root);
        let worker_root = home.path().join("worker");
        let worker = session_after(&formation, 20, "2");
        write_trace(
            &worker_root,
            &worker,
            &[
                member_start(&worker, &formation, Some("worker"), &[], "worker"),
                received("tsk_w", "msg_mcl_1", Some("lead"), 60),
            ],
        );
        fs::set_permissions(&worker_root, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_dir(&worker_root).is_ok() {
            fs::set_permissions(&worker_root, fs::Permissions::from_mode(0o700)).unwrap();
            eprintln!("skipped: this user lists a 0o000 directory (running as uid 0)");
            return;
        }
        let search = search_formation(&lead_root, vec![worker_root.clone()], &formation).unwrap();
        fs::set_permissions(&worker_root, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(search.searched[1].unreadable.is_some());
        assert_eq!(
            summary(&formation_calls(&search.found.members)),
            [(0, "lead→worker".into(), "worker's trace not found".into())]
        );
    }

    /// A task received from a member whose trace was not found is a row of its own, its call id
    /// read from the message id, its status unknown.
    #[test]
    fn a_call_only_the_callee_recorded_takes_its_call_id_from_the_message_id() {
        let formation = FormationId::mint();
        let root = tempfile::tempdir().unwrap();
        let worker = session_after(&formation, 20, "2");
        write_trace(
            root.path(),
            &worker,
            &[
                member_start(&worker, &formation, Some("worker"), &[], "worker"),
                received(
                    "tsk_w",
                    "msg_mcl_01a10645751c70229f533dfd881ee485",
                    Some("lead"),
                    60,
                ),
                task_end("tsk_w", "completed"),
                received("tsk_v", "msg_other", Some("lead"), 70),
            ],
        );
        let members = recorded_members(root.path(), &formation).unwrap().members;
        let calls = formation_calls(&members);
        assert_eq!(
            calls
                .iter()
                .map(|call| call.call_id.as_deref())
                .collect::<Vec<_>>(),
            [Some("mcl_01a10645751c70229f533dfd881ee485"), None]
        );
        assert_eq!(
            calls.iter().map(|call| call.started_ms).collect::<Vec<_>>(),
            [60, 70]
        );
        assert!(calls.iter().all(|call| call.ending == CallEnding::Unknown));
        assert_eq!(
            summary(&calls),
            [
                (
                    0,
                    "lead→worker".into(),
                    "lead's trace not found; worker's task ended completed".into()
                ),
                (
                    0,
                    "lead→worker".into(),
                    "lead's trace not found; worker's task has no task_end".into()
                ),
            ]
        );
    }

    /// An outstanding call names how its callee's task ended, or that it has no `task_end`, or
    /// that the callee's trace was not found.
    #[test]
    fn an_outstanding_call_says_what_its_callee_recorded() {
        let formation = FormationId::mint();
        let root = tempfile::tempdir().unwrap();
        let lead = session_after(&formation, 10, "1");
        let (a, b) = (
            session_after(&formation, 20, "2"),
            session_after(&formation, 30, "3"),
        );
        write_trace(
            root.path(),
            &lead,
            &[
                member_start(&lead, &formation, Some("lead"), &["a", "b", "c"], "lead"),
                call_start("tsk_l", "mcl_a", "a", "tsk_a", 50),
                call_start("tsk_l", "mcl_b", "b", "tsk_b", 51),
                call_start("tsk_l", "mcl_c", "c", "tsk_c", 52),
            ],
        );
        write_trace(
            root.path(),
            &a,
            &[
                member_start(&a, &formation, Some("a"), &[], "worker"),
                received("tsk_a", "msg_mcl_a", Some("lead"), 55),
                task_end("tsk_a", "canceled"),
            ],
        );
        write_trace(
            root.path(),
            &b,
            &[
                member_start(&b, &formation, Some("b"), &[], "worker"),
                received("tsk_b", "msg_mcl_b", Some("lead"), 56),
            ],
        );
        let calls = formation_calls(&recorded_members(root.path(), &formation).unwrap().members);
        assert!(calls
            .iter()
            .all(|call| call.ending == CallEnding::Outstanding));
        assert_eq!(
            summary(&calls),
            [
                (0, "lead→a".into(), "a's task ended canceled".into()),
                (0, "lead→b".into(), "b's task has no task_end".into()),
                (0, "lead→c".into(), "c's trace not found".into()),
            ]
        );
    }

    /// A member that received `tasks` (task id, caller) and made `calls` (calling task, call id,
    /// callee, callee task, started at).
    fn joined_member(
        name: &str,
        tasks: &[(&str, &str)],
        calls: &[(&str, &str, &str, &str, u64)],
    ) -> RecordedMember {
        RecordedMember {
            session_id: format!("ses_{name}"),
            session_dir: PathBuf::from(format!("/records/ses_{name}")),
            capsule: "c@0.1.0".to_string(),
            member: Some(name.to_string()),
            callees: Vec::new(),
            spawned_by: None,
            exit_status: None,
            delegated_children: Vec::new(),
            calls_made: calls
                .iter()
                .map(|(task, call, callee, callee_task, at)| MemberCallRecord {
                    call_id: call.to_string(),
                    member: callee.to_string(),
                    calling_task_id: task.to_string(),
                    member_task_id: Some(callee_task.to_string()),
                    started_ms: *at,
                    status: Some("completed".to_string()),
                    duration_ms: 1,
                    delivered: true,
                    no_answer_below: Vec::new(),
                    busy_offers: 0,
                })
                .collect(),
            tasks_received: tasks
                .iter()
                .map(|(task, caller)| ReceivedTask {
                    task_id: task.to_string(),
                    call_id: None,
                    from: ReceivedFrom::Member(caller.to_string()),
                    received_ms: 0,
                    ending: Some("completed".to_string()),
                    duration_ms: 1,
                    message_sha256: None,
                    message_stored: false,
                })
                .collect(),
        }
    }

    /// Each lead's calls sit under the call that handed the lead its task, in start order, and the
    /// leads' calls are in start order among themselves.
    #[test]
    fn calls_nest_under_the_call_that_handed_over_their_task_in_start_order() {
        let members = [
            joined_member(
                "entry",
                &[],
                &[
                    ("tsk_e", "mcl_lb", "lead-b", "tsk_lb", 20),
                    ("tsk_e", "mcl_la", "lead-a", "tsk_la", 10),
                ],
            ),
            joined_member(
                "lead-a",
                &[("tsk_la", "entry")],
                &[
                    ("tsk_la", "mcl_w1", "w1", "tsk_w1", 40),
                    ("tsk_la", "mcl_w2", "w2", "tsk_w2", 30),
                ],
            ),
            joined_member(
                "lead-b",
                &[("tsk_lb", "entry")],
                &[("tsk_lb", "mcl_w3", "w3", "tsk_w3", 25)],
            ),
            joined_member("w1", &[("tsk_w1", "lead-a")], &[]),
            joined_member("w2", &[("tsk_w2", "lead-a")], &[]),
            joined_member("w3", &[("tsk_w3", "lead-b")], &[]),
        ];
        assert_eq!(
            summary(&formation_calls(&members)),
            [
                (0, "entry→lead-a".into(), String::new()),
                (1, "lead-a→w2".into(), String::new()),
                (1, "lead-a→w1".into(), String::new()),
                (0, "entry→lead-b".into(), String::new()),
                (1, "lead-b→w3".into(), String::new()),
            ]
        );
    }

    /// Calls whose parents form a cycle, and a call that is its own parent, end the list with
    /// every row exactly once.
    #[test]
    fn a_cycle_of_calls_terminates_with_every_row_once() {
        let members = [
            joined_member(
                "a",
                &[("tsk_x", "b")],
                &[("tsk_x", "mcl_ab", "b", "tsk_y", 20)],
            ),
            joined_member(
                "b",
                &[("tsk_y", "a")],
                &[("tsk_y", "mcl_ba", "a", "tsk_x", 10)],
            ),
            joined_member(
                "c",
                &[("tsk_z", "c")],
                &[
                    ("tsk_z", "mcl_cc", "c", "tsk_z", 5),
                    ("tsk_q", "mcl_cd", "d", "tsk_d", 50),
                ],
            ),
            joined_member("d", &[("tsk_d", "c")], &[]),
        ];
        let calls = formation_calls(&members);
        let ids: Vec<&str> = calls
            .iter()
            .map(|call| call.call_id.as_deref().unwrap())
            .collect();
        assert_eq!(ids, ["mcl_cd", "mcl_cc", "mcl_ba", "mcl_ab"]);
        assert_eq!(
            calls.iter().map(|call| call.depth).collect::<Vec<_>>(),
            [0, 0, 0, 1]
        );
    }

    /// `lead` received `worker`'s call and, later, the operator's task: both are rows in time
    /// order, the call joined from both sides and the operator's a task row of its own; without
    /// the operator's line only the call row is left.
    #[test]
    fn an_operator_task_is_a_row_beside_the_calls() {
        for with_operator in [true, false] {
            let formation = FormationId::mint();
            let root = tempfile::tempdir().unwrap();
            let lead = session_after(&formation, 10, "1");
            let worker = session_after(&formation, 20, "2");
            let mut lead_lines = vec![
                member_start(&lead, &formation, Some("lead"), &["worker"], "lead"),
                received_as(
                    "tsk_l",
                    "msg_mcl_1",
                    json!({"caller": "member", "caller_member": "worker"}),
                    60,
                ),
                task_end_after("tsk_l", "completed", 70),
            ];
            if with_operator {
                lead_lines.push(received_as(
                    "tsk_op",
                    "msg_op",
                    json!({"caller": "operator", "message_sha256": "ab".repeat(32)}),
                    500,
                ));
                lead_lines.push(task_end_after("tsk_op", "ok", 35_500));
            }
            write_trace(root.path(), &lead, &lead_lines);
            write_trace(
                root.path(),
                &worker,
                &[
                    member_start(&worker, &formation, Some("worker"), &["lead"], "worker"),
                    call_start("tsk_w", "mcl_1", "lead", "tsk_l", 50),
                    call_end(
                        "tsk_w",
                        "mcl_1",
                        "lead",
                        Some("tsk_l"),
                        "completed",
                        90,
                        true,
                        140,
                    ),
                ],
            );
            let members = recorded_members(root.path(), &formation).unwrap().members;
            let rows = formation_rows(&members);
            let mut expected = vec![(0, "worker→lead".to_string(), String::new())];
            if with_operator {
                expected.push((0, "operator→lead".into(), "message not captured".into()));
            }
            assert_eq!(row_summary(&rows), expected);
            if with_operator {
                let FormationRow::Task(task) = &rows[1] else {
                    panic!("the operator's task is a task row: {rows:?}");
                };
                assert_eq!(task.task_id, "tsk_op");
                assert_eq!(task.ending.as_deref(), Some("ok"));
                assert_eq!(task.duration_ms, 35_500);
            }
        }
    }

    /// A task row points at its message only when the blob holding it exists, and a hash that is
    /// no blob name is no hash.
    #[test]
    fn a_task_row_points_at_its_message_only_when_the_blob_exists() {
        let formation = FormationId::mint();
        let root = tempfile::tempdir().unwrap();
        let worker = session_after(&formation, 20, "2");
        let stored = "3f2a9c1e0b7d".to_string() + &"1".repeat(52);
        let missing = "c".repeat(64);
        write_trace(
            root.path(),
            &worker,
            &[
                member_start(&worker, &formation, Some("worker"), &[], "worker"),
                received_as(
                    "tsk_a",
                    "msg_a",
                    json!({"caller": "operator", "message_sha256": stored}),
                    10,
                ),
                received_as(
                    "tsk_b",
                    "msg_b",
                    json!({"caller": "operator", "message_sha256": missing}),
                    20,
                ),
                received_as(
                    "tsk_c",
                    "msg_c",
                    json!({"caller": "operator", "message_sha256": "../trace.jsonl"}),
                    30,
                ),
            ],
        );
        let blobs = root.path().join(&worker).join("blobs");
        fs::create_dir_all(&blobs).unwrap();
        fs::write(blobs.join(&stored), "Name three colours.").unwrap();
        let members = recorded_members(root.path(), &formation).unwrap().members;
        let trace = root.path().join(&worker).join("trace.jsonl");
        assert_eq!(
            row_summary(&formation_rows(&members)),
            [
                (
                    0,
                    "operator→worker".into(),
                    format!(
                        "message: mur trace show {} --body 3f2a9c1e0b7d",
                        trace.display()
                    )
                ),
                (0, "operator→worker".into(), "message not captured".into()),
                (0, "operator→worker".into(), "message not captured".into()),
            ]
        );
        assert_eq!(members[0].tasks_received[2].message_sha256, None);
    }

    /// Each sender `a2a_task_received` can name, and every line that names none or names it
    /// incompletely: an older trace's task with no `caller` is a member's call when it carries
    /// `caller_member` and from an unknown caller when it does not.
    #[test]
    fn every_received_task_is_classified_by_who_let_it_in() {
        let formation = FormationId::mint();
        let root = tempfile::tempdir().unwrap();
        let worker = session_after(&formation, 20, "2");
        write_trace(
            root.path(),
            &worker,
            &[
                member_start(&worker, &formation, Some("worker"), &[], "worker"),
                received_as("tsk_1", "m", json!({"caller": "operator"}), 1),
                received_as("tsk_2", "m", json!({"caller": "public"}), 2),
                received_as(
                    "tsk_3",
                    "m",
                    json!({"caller": "credential", "caller_credential": "alice"}),
                    3,
                ),
                received_as(
                    "tsk_4",
                    "m",
                    json!({"caller": "member", "caller_member": "lead"}),
                    4,
                ),
                received("tsk_5", "m", Some("lead"), 5),
                received("tsk_6", "m", None, 6),
                received_as("tsk_7", "m", json!({"caller": "wizard"}), 7),
                received_as("tsk_8", "m", json!({"caller": "credential"}), 8),
                received_as("tsk_9", "m", json!({"caller": "member"}), 9),
                r#"{"event_type":"a2a_task_received","task_id":"tsk_torn","cal"#.to_string(),
            ],
        );
        let members = recorded_members(root.path(), &formation).unwrap().members;
        let from: Vec<&ReceivedFrom> = members[0]
            .tasks_received
            .iter()
            .map(|task| &task.from)
            .collect();
        let lead = ReceivedFrom::Member("lead".into());
        assert_eq!(
            from,
            [
                &ReceivedFrom::Operator,
                &ReceivedFrom::Public,
                &ReceivedFrom::Credential("alice".into()),
                &lead,
                &lead,
                &ReceivedFrom::Unknown,
                &ReceivedFrom::Unknown,
                &ReceivedFrom::Unknown,
                &ReceivedFrom::Unknown,
            ]
        );
        assert_eq!(
            row_summary(&formation_rows(&members))
                .into_iter()
                .map(|(_, pair, _)| pair)
                .collect::<Vec<_>>(),
            [
                "operator→worker",
                "public→worker",
                "credential alice→worker",
                "lead→worker",
                "lead→worker",
                "unknown caller→worker",
                "unknown caller→worker",
                "unknown caller→worker",
                "unknown caller→worker",
            ]
        );
        assert_eq!(
            sender_kind_counts(&from.into_iter().cloned().collect::<Vec<_>>()),
            [
                ("operator", 1),
                ("member", 2),
                ("credential", 1),
                ("public", 1),
                ("unknown caller", 4)
            ]
        );
    }

    /// A call `lead` made from the operator's task sits under the operator's row; the same call
    /// with no operator's line is a root, as is a call from the launch's own task.
    #[test]
    fn a_call_made_inside_a_task_no_member_sent_nests_under_it() {
        for with_operator in [true, false] {
            let formation = FormationId::mint();
            let root = tempfile::tempdir().unwrap();
            let lead = session_after(&formation, 10, "1");
            let mut lead_lines = vec![member_start(
                &lead,
                &formation,
                Some("lead"),
                &["worker"],
                "lead",
            )];
            if with_operator {
                lead_lines.push(received_as(
                    "tsk_A",
                    "msg_a",
                    json!({"caller": "operator"}),
                    40,
                ));
            }
            lead_lines.extend([
                call_start("tsk_A", "mcl_1", "worker", "tsk_w", 50),
                call_end(
                    "tsk_A",
                    "mcl_1",
                    "worker",
                    Some("tsk_w"),
                    "completed",
                    90,
                    true,
                    140,
                ),
                call_start("tsk_launch", "mcl_0", "worker", "tsk_w0", 45),
            ]);
            write_trace(root.path(), &lead, &lead_lines);
            let rows = formation_rows(&recorded_members(root.path(), &formation).unwrap().members);
            let listed: Vec<(usize, String)> = row_summary(&rows)
                .into_iter()
                .map(|(depth, pair, _)| (depth, pair))
                .collect();
            if with_operator {
                assert_eq!(
                    listed,
                    [
                        (0, "operator→lead".to_string()),
                        (1, "lead→worker".to_string()),
                        (0, "lead→worker".to_string()),
                    ]
                );
                let FormationRow::Call(nested) = &rows[1] else {
                    panic!("{rows:?}");
                };
                assert_eq!(nested.call_id.as_deref(), Some("mcl_1"));
            } else {
                assert_eq!(
                    listed,
                    [
                        (0, "lead→worker".to_string()),
                        (0, "lead→worker".to_string()),
                    ]
                );
            }
        }
    }

    /// A torn line costs only itself, and a field another event spells with another type makes
    /// no call line fail to parse.
    #[test]
    fn a_torn_line_and_a_foreign_field_drop_nothing() {
        let formation = FormationId::mint();
        let root = tempfile::tempdir().unwrap();
        let lead = session_after(&formation, 10, "1");
        let worker = session_after(&formation, 20, "2");
        write_trace(
            root.path(),
            &lead,
            &[
                member_start(&lead, &formation, Some("lead"), &["worker"], "lead"),
                json!({"event_type": "tool_call", "exit_status": 3, "member": ["x"],
                       "call_id": 7, "child_session_id": false})
                .to_string(),
                call_start("tsk_l", "mcl_1", "worker", "tsk_w", 50),
                r#"{"event_type":"member_call","call_id":"mcl_1","sta"#.to_string(),
                json!({"event_type": "session_end", "exit_status": "completed",
                       "status": 9, "task_id": 4})
                .to_string(),
            ],
        );
        write_trace(
            root.path(),
            &worker,
            &[
                member_start(&worker, &formation, Some("worker"), &[], "worker"),
                json!({"event_type": "inference", "task_id": 12, "caller_member": 1}).to_string(),
                received("tsk_w", "msg_mcl_1", Some("lead"), 55),
                task_end("tsk_w", "completed"),
            ],
        );
        let members = recorded_members(root.path(), &formation).unwrap().members;
        assert_eq!(members[0].exit_status.as_deref(), Some("completed"));
        assert_eq!(
            summary(&formation_calls(&members)),
            [(
                0,
                "lead→worker".into(),
                "worker's task ended completed".into()
            )]
        );
    }
}
