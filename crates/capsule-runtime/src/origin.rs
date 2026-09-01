//! Why a task woke the capsule, and how far its content is trusted.
//!
//! An origin is set by whoever enqueued the task. A trust class is *derived* from that origin
//! and, for the two origins that cross a capsule boundary, from the trust of the sending
//! capsule's own current task. No sender declares its trust class: [`TaskProvenance`] has no
//! constructor that takes one on its own, so "derived, never declared" is a property of the type
//! rather than a convention callers are asked to keep.
//!
//! Nothing in the runtime branches on trust: the value is recorded on `task_start` and left on
//! the store state for the task's duration, and no task is refused, delayed or reordered for it.
//! The origin half is not inert — it picks the queue lane a task waits in, so two tasks with the
//! same trust can still run in a different order. See [`crate::lanes`].

/// Header carrying the sending runtime's origin claim on an inter-capsule request.
///
/// Stamped by the sending *runtime*, never by a guest component: `murmur:message/send` has no
/// field a capsule author could put an origin in.
pub const PEER_ORIGIN_HEADER: &str = "x-murmur-task-origin";

/// Header carrying the trust class of the sending capsule's own current task.
pub const PEER_TRUST_HEADER: &str = "x-murmur-task-trust";

/// Why the runtime woke the capsule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskOrigin {
    /// A person handed the capsule an instruction — a local `mur run`, a `task.md`.
    User,
    /// A message from another capsule, carrying that capsule's own trust class.
    Peer,
    /// A timer fired.
    Schedule,
    /// A webhook, chat message or PR comment — third-party text, so never trusted.
    Event,
    /// A sub-capsule or detached shell reporting that its work finished. Inherits like
    /// [`TaskOrigin::Peer`]: it is the same boundary, seen from the other end.
    Completion,
    /// The runtime enqueued this for itself with no person in the loop — a retry, a sweep.
    System,
}

impl TaskOrigin {
    /// The lowercase wire spelling, as it appears in a header value and in a trace record.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Peer => "peer",
            Self::Schedule => "schedule",
            Self::Event => "event",
            Self::Completion => "completion",
            Self::System => "system",
        }
    }

    /// Exact match against [`Self::as_str`]. No aliases and no case folding — an inbound header
    /// arrives already lowercased, and every other caller is in-crate.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "user" => Some(Self::User),
            "peer" => Some(Self::Peer),
            "schedule" => Some(Self::Schedule),
            "event" => Some(Self::Event),
            "completion" => Some(Self::Completion),
            "system" => Some(Self::System),
            _ => None,
        }
    }
}

/// How far the content of a task is trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustClass {
    Trusted,
    Untrusted,
}

impl TrustClass {
    /// The lowercase wire spelling, as it appears in a header value and in a trace record.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Trusted => "trusted",
            Self::Untrusted => "untrusted",
        }
    }

    /// Exact match against [`Self::as_str`], on the same terms as [`TaskOrigin::parse`].
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "trusted" => Some(Self::Trusted),
            "untrusted" => Some(Self::Untrusted),
            _ => None,
        }
    }
}

/// An origin paired with the trust class derived from it.
///
/// Both fields are private and the only constructor is [`Self::derive`], so there is no way to
/// pair an origin with a trust class that does not follow from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskProvenance {
    origin: TaskOrigin,
    trust: TrustClass,
}

impl TaskProvenance {
    /// Derive the trust class for `origin`.
    ///
    /// `inherited` is the trust of the sending capsule's own current task, and is consulted only
    /// for [`TaskOrigin::Peer`] and [`TaskOrigin::Completion`] — the two origins that cross a
    /// capsule boundary. Passing `None` for either means the sender made no claim, which
    /// resolves to [`TrustClass::Untrusted`]: an absent claim is not a trusted one, so untrust
    /// cannot launder itself by dropping the header at the first hop.
    pub fn derive(origin: TaskOrigin, inherited: Option<TrustClass>) -> Self {
        let trust = match origin {
            TaskOrigin::User | TaskOrigin::Schedule | TaskOrigin::System => TrustClass::Trusted,
            TaskOrigin::Event => TrustClass::Untrusted,
            TaskOrigin::Peer | TaskOrigin::Completion => inherited.unwrap_or(TrustClass::Untrusted),
        };
        Self { origin, trust }
    }

    pub fn origin(&self) -> TaskOrigin {
        self.origin
    }

    pub fn trust(&self) -> TrustClass {
        self.trust
    }
}

/// Classify a task arriving over the peer door from the two request headers.
///
/// Only `peer` and `completion` are accepted from the wire. `user`, `schedule`, `event` and
/// `system` are enqueued locally by the CLI or by the runtime and never legitimately arrive over
/// HTTP, so a caller naming one — or naming anything unrecognised, or sending no origin header
/// at all — is classified `event` / `untrusted` and its trust header is not read. No HTTP caller
/// can talk itself into a trusted class it was not given by a murmur runtime.
///
/// This does not authenticate the door: a caller that claims `peer` + `trusted` gets what a
/// genuine trusted peer gets, and nothing on the A2A path tells the two apart. The boundary
/// closed here is untrust laundering across an honest chain.
pub fn from_wire(origin_header: Option<&str>, trust_header: Option<&str>) -> TaskProvenance {
    let origin = origin_header.and_then(TaskOrigin::parse);
    match origin {
        Some(origin @ (TaskOrigin::Peer | TaskOrigin::Completion)) => {
            TaskProvenance::derive(origin, trust_header.and_then(TrustClass::parse))
        }
        _ => TaskProvenance::derive(TaskOrigin::Event, None),
    }
}

/// The provenance a runtime stamps on an outbound peer message, given its own current task.
///
/// The origin is always [`TaskOrigin::Peer`]; a completion is stamped by
/// [`stamp_for_completion`] instead, which is the other origin the inbound rule accepts. The
/// trust class is the sending task's own, so untrust survives the hop. `None`, meaning no task is
/// in scope, stamps [`TrustClass::Untrusted`].
pub fn stamp_for_peer(sender_task: Option<TaskProvenance>) -> TaskProvenance {
    TaskProvenance::derive(TaskOrigin::Peer, sender_task.map(|task| task.trust()))
}

/// The provenance a runtime stamps on a completion it posts to the capsule that delegated to it.
///
/// `inherited` is the trust class of the parent task that made the delegation, carried to the
/// child in its [`crate::delegation::SpawnerHandle`]. It is not a second decision about trust:
/// the class was derived when the delegating task arrived, and this hands the same class back so
/// the completion inherits it rather than being reclassified as fresh. `None` stamps
/// [`TrustClass::Untrusted`], the safe class.
pub fn stamp_for_completion(inherited: Option<TrustClass>) -> TaskProvenance {
    TaskProvenance::derive(TaskOrigin::Completion, inherited)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_ORIGINS: [TaskOrigin; 6] = [
        TaskOrigin::User,
        TaskOrigin::Peer,
        TaskOrigin::Schedule,
        TaskOrigin::Event,
        TaskOrigin::Completion,
        TaskOrigin::System,
    ];

    #[test]
    fn origin_round_trips_through_its_wire_spelling() {
        for origin in ALL_ORIGINS {
            assert_eq!(TaskOrigin::parse(origin.as_str()), Some(origin));
        }
        assert_eq!(TaskOrigin::parse("User"), None);
        assert_eq!(TaskOrigin::parse("not-an-origin"), None);
        assert_eq!(TaskOrigin::parse(""), None);
    }

    #[test]
    fn trust_round_trips_through_its_wire_spelling() {
        for trust in [TrustClass::Trusted, TrustClass::Untrusted] {
            assert_eq!(TrustClass::parse(trust.as_str()), Some(trust));
        }
        assert_eq!(TrustClass::parse("Trusted"), None);
        assert_eq!(TrustClass::parse("maybe"), None);
    }

    /// The full derivation table: six origins crossed with every inherited value.
    #[test]
    fn derive_covers_every_origin_and_inherited_pair() {
        let cases: [(TaskOrigin, Option<TrustClass>, TrustClass); 18] = [
            (TaskOrigin::User, None, TrustClass::Trusted),
            (
                TaskOrigin::User,
                Some(TrustClass::Trusted),
                TrustClass::Trusted,
            ),
            (
                TaskOrigin::User,
                Some(TrustClass::Untrusted),
                TrustClass::Trusted,
            ),
            (TaskOrigin::Schedule, None, TrustClass::Trusted),
            (
                TaskOrigin::Schedule,
                Some(TrustClass::Trusted),
                TrustClass::Trusted,
            ),
            (
                TaskOrigin::Schedule,
                Some(TrustClass::Untrusted),
                TrustClass::Trusted,
            ),
            (TaskOrigin::System, None, TrustClass::Trusted),
            (
                TaskOrigin::System,
                Some(TrustClass::Trusted),
                TrustClass::Trusted,
            ),
            (
                TaskOrigin::System,
                Some(TrustClass::Untrusted),
                TrustClass::Trusted,
            ),
            (TaskOrigin::Event, None, TrustClass::Untrusted),
            (
                TaskOrigin::Event,
                Some(TrustClass::Trusted),
                TrustClass::Untrusted,
            ),
            (
                TaskOrigin::Event,
                Some(TrustClass::Untrusted),
                TrustClass::Untrusted,
            ),
            (TaskOrigin::Peer, None, TrustClass::Untrusted),
            (
                TaskOrigin::Peer,
                Some(TrustClass::Trusted),
                TrustClass::Trusted,
            ),
            (
                TaskOrigin::Peer,
                Some(TrustClass::Untrusted),
                TrustClass::Untrusted,
            ),
            (TaskOrigin::Completion, None, TrustClass::Untrusted),
            (
                TaskOrigin::Completion,
                Some(TrustClass::Trusted),
                TrustClass::Trusted,
            ),
            (
                TaskOrigin::Completion,
                Some(TrustClass::Untrusted),
                TrustClass::Untrusted,
            ),
        ];
        for (origin, inherited, expected) in cases {
            let provenance = TaskProvenance::derive(origin, inherited);
            assert_eq!(
                provenance.trust(),
                expected,
                "{origin:?} with {inherited:?} should be {expected:?}"
            );
            assert_eq!(provenance.origin(), origin);
        }
    }

    #[test]
    fn wire_accepts_only_peer_and_completion() {
        for accepted in ["peer", "completion"] {
            let provenance = from_wire(Some(accepted), Some("trusted"));
            assert_eq!(provenance.origin(), TaskOrigin::parse(accepted).unwrap());
            assert_eq!(provenance.trust(), TrustClass::Trusted);
        }
    }

    #[test]
    fn wire_refuses_locally_only_and_unrecognised_origins() {
        for refused in ["user", "schedule", "event", "system", "not-an-origin", ""] {
            let provenance = from_wire(Some(refused), Some("trusted"));
            assert_eq!(
                provenance.origin(),
                TaskOrigin::Event,
                "{refused:?} must not be accepted from the wire"
            );
            assert_eq!(provenance.trust(), TrustClass::Untrusted);
        }
    }

    #[test]
    fn wire_without_an_origin_header_is_an_untrusted_event() {
        let provenance = from_wire(None, Some("trusted"));
        assert_eq!(provenance.origin(), TaskOrigin::Event);
        assert_eq!(provenance.trust(), TrustClass::Untrusted);
    }

    #[test]
    fn wire_trust_defaults_to_untrusted_when_absent_or_unparseable() {
        for trust_header in [None, Some("Trusted"), Some("yes"), Some("")] {
            let provenance = from_wire(Some("peer"), trust_header);
            assert_eq!(provenance.origin(), TaskOrigin::Peer);
            assert_eq!(
                provenance.trust(),
                TrustClass::Untrusted,
                "trust header {trust_header:?} must not resolve to trusted"
            );
        }
    }

    #[test]
    fn stamp_for_peer_carries_the_senders_class_and_defaults_untrusted() {
        assert_eq!(stamp_for_peer(None).origin(), TaskOrigin::Peer);
        assert_eq!(stamp_for_peer(None).trust(), TrustClass::Untrusted);
        for (sender, expected) in [
            (TaskOrigin::User, TrustClass::Trusted),
            (TaskOrigin::Event, TrustClass::Untrusted),
        ] {
            let stamped = stamp_for_peer(Some(TaskProvenance::derive(sender, None)));
            assert_eq!(stamped.origin(), TaskOrigin::Peer);
            assert_eq!(stamped.trust(), expected);
        }
    }

    /// A completion inherits the delegating task's class and decides nothing of its own.
    #[test]
    fn stamp_for_completion_carries_the_delegating_tasks_class() {
        assert_eq!(stamp_for_completion(None).origin(), TaskOrigin::Completion);
        assert_eq!(stamp_for_completion(None).trust(), TrustClass::Untrusted);
        for trust in [TrustClass::Trusted, TrustClass::Untrusted] {
            let stamped = stamp_for_completion(Some(trust));
            assert_eq!(stamped.origin(), TaskOrigin::Completion);
            assert_eq!(stamped.trust(), trust);
        }
    }

    #[test]
    fn wire_untrusted_peer_stays_untrusted() {
        let provenance = from_wire(Some("peer"), Some("untrusted"));
        assert_eq!(provenance.origin(), TaskOrigin::Peer);
        assert_eq!(provenance.trust(), TrustClass::Untrusted);
    }
}
