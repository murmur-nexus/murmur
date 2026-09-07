//! Cancelling one running task without ending the session it runs in.
//!
//! Three things live here, because all three are read by both ends of a cancel — the A2A door
//! answering `tasks/cancel` and the agent loop that stops:
//!
//! * [`CancelSignal`], the one-way flag a cancelled task's waits race against;
//! * [`LiveDelegations`], the registry of delegations this session started and has not closed;
//! * [`Residue`], a snapshot of what is still running when a task stops.
//!
//! Nothing here kills anything. A cancel stops the runtime waiting on work; the work itself keeps
//! whatever lifecycle it already had, and is named instead.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Instant,
};

use serde_json::json;
use tokio::sync::watch;

use crate::{
    a2a::{A2aArtifact, ArtifactPart},
    detached::DetachedRegistry,
};

/// The artifact name a cancel response carries its residue under.
pub(crate) const RESIDUE_ARTIFACT: &str = "residue";

// ── Where a cancel landed ─────────────────────────────────────────────────────
//
// The `phase` vocabulary of the `task_canceled` trace record. One word per place cancellation is
// checked, so the record says which wait was interrupted rather than only that one was.

/// Cancelled before the task ever started: it was still `submitted` when the loop reached it.
pub(crate) const PHASE_QUEUED: &str = "queued";
/// Cancelled at a turn boundary, between one turn's work and the next turn's request.
pub(crate) const PHASE_TURN: &str = "turn";
/// Cancelled with the inference call in flight. The call future is dropped.
pub(crate) const PHASE_INFERENCE: &str = "inference";
/// Cancelled while the task was waiting on a person's answer to `request-input`.
pub(crate) const PHASE_INPUT: &str = "input";
/// Cancelled while a `delegate-task` call was waiting for its child to be up.
pub(crate) const PHASE_DELEGATION: &str = "delegation";

// ── CancelSignal ──────────────────────────────────────────────────────────────

/// One task's "a person stopped this" flag, shared by the door that sets it and every wait inside
/// the task that races against it.
///
/// One-way and idempotent: once set it never clears, so a wait that arms after the cancel resolves
/// immediately rather than blocking forever. Cloning shares the flag.
#[derive(Debug, Clone)]
pub(crate) struct CancelSignal {
    /// Held by `Arc` on both sides so the sender outlives every observer — a `changed()` that
    /// could see a closed channel would be a wait nothing ever wakes.
    tx: Arc<watch::Sender<bool>>,
    /// Which wait actually saw the cancel first, for the `task_canceled` record.
    ///
    /// Every wait but the driver call unwinds its guest and lets the agent loop stop at its next
    /// turn boundary, so without this the record would say `turn` for all of them and lose the
    /// one fact it exists to carry.
    phase: Arc<Mutex<Option<&'static str>>>,
}

impl CancelSignal {
    pub(crate) fn new() -> Self {
        let (tx, _rx) = watch::channel(false);
        Self {
            tx: Arc::new(tx),
            phase: Arc::new(Mutex::new(None)),
        }
    }

    /// Claim this cancel for one wait. The first claim stands: a later boundary observing the
    /// same cancel is reporting where the loop stopped, not where the cancel landed.
    pub(crate) fn note_phase(&self, phase: &'static str) {
        self.phase
            .lock()
            .expect("cancel phase mutex")
            .get_or_insert(phase);
    }

    /// The wait that claimed this cancel, or [`PHASE_TURN`] when none did — the loop was between
    /// turns and nothing was waiting.
    pub(crate) fn phase(&self) -> &'static str {
        self.phase
            .lock()
            .expect("cancel phase mutex")
            .unwrap_or(PHASE_TURN)
    }

    /// Raise the flag. Every present and future [`Self::canceled`] resolves.
    ///
    /// `send_replace` rather than `send`: a signal nobody is waiting on yet still has to record
    /// the cancellation, and `send` refuses when no receiver is live.
    pub(crate) fn cancel(&self) {
        self.tx.send_replace(true);
    }

    pub(crate) fn is_canceled(&self) -> bool {
        *self.tx.borrow()
    }

    /// Resolve as soon as the flag is up, immediately if it already is.
    ///
    /// Cancellation-safe: dropping this future loses nothing, which is what lets it sit in a
    /// `tokio::select!` arm beside the work it is racing.
    pub(crate) async fn canceled(&self) {
        let mut rx = self.tx.subscribe();
        loop {
            if *rx.borrow_and_update() {
                return;
            }
            // The sender is held by this struct's `Arc`, so `changed()` cannot report a closed
            // channel while a caller still holds the signal.
            if rx.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }
}

impl Default for CancelSignal {
    fn default() -> Self {
        Self::new()
    }
}

// ── Live delegations ──────────────────────────────────────────────────────────

/// One delegation this session started and has not yet closed.
///
/// Kept because the terminal `delegation` trace line names the capsule and the version, and the
/// completion that arrives later carries neither: it names the delegation, and the runtime
/// remembers what that delegation was for. A cancel reads the same entries to name what is still
/// running.
#[derive(Debug, Clone)]
pub(crate) struct LiveDelegation {
    /// The child's directory, absolute — where its `completion.json` is read from.
    pub(crate) workdir: PathBuf,
    pub(crate) capsule: String,
    pub(crate) version: String,
    /// The session id the child's runtime minted for itself.
    pub(crate) child_session_id: String,
    /// The child's directory as the parent's own tools address it: relative to the parent's
    /// accessible workdir.
    pub(crate) child_workdir: String,
    /// When the launch was recorded, so an unreadable completion still closes the row with a
    /// duration rather than a zero.
    pub(crate) started: Instant,
}

/// Every delegation in flight, by `dlg_` id.
///
/// Registered from the launch callback, on the blocking thread that launched the child, so a
/// delegation is live from the moment the child is up — including when the call that started it
/// is cancelled before it returns.
#[derive(Debug, Default)]
pub(crate) struct LiveDelegations {
    inner: Mutex<HashMap<String, LiveDelegation>>,
}

impl LiveDelegations {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Record one launched child. A repeated id replaces the earlier entry, which is the only
    /// reading available: two launches cannot share a `dlg_` id.
    pub(crate) fn register(&self, delegation_id: String, delegation: LiveDelegation) {
        self.inner
            .lock()
            .expect("live delegations mutex")
            .insert(delegation_id, delegation);
    }

    /// Take the entry for a delegation whose outcome has arrived, so its terminal `delegation`
    /// trace line can be written from what the launch knew.
    pub(crate) fn finish(&self, delegation_id: &str) -> Option<LiveDelegation> {
        self.inner
            .lock()
            .expect("live delegations mutex")
            .remove(delegation_id)
    }

    /// Drop a delegation that will never produce a completion — one announced by the launcher
    /// whose start then failed. Unlike [`Self::finish`] there is nothing to close with.
    pub(crate) fn release(&self, delegation_id: &str) {
        self.inner
            .lock()
            .expect("live delegations mutex")
            .remove(delegation_id);
    }

    /// Everything still in flight, as residue items.
    pub(crate) fn live(&self) -> Vec<ResidueItem> {
        let mut items: Vec<ResidueItem> = self
            .inner
            .lock()
            .expect("live delegations mutex")
            .iter()
            .map(|(delegation_id, delegation)| ResidueItem::Delegation {
                delegation_id: delegation_id.clone(),
                capsule: delegation.capsule.clone(),
                version: delegation.version.clone(),
                child_session_id: delegation.child_session_id.clone(),
                child_workdir: delegation.child_workdir.clone(),
            })
            .collect();
        // A `HashMap` iterates in no order, and a response that lists the same two delegations in
        // a different order on every read is one nobody can diff.
        items.sort_by(|a, b| a.id().cmp(b.id()));
        items
    }
}

// ── Residue ───────────────────────────────────────────────────────────────────

/// One thing that was still running when a task stopped.
///
/// Named rather than stopped: a detached shell keeps its own lifecycle and a delegated child is
/// left exactly as a delegation deadline leaves it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResidueItem {
    DetachedShell {
        work_id: String,
        binary: String,
        command: String,
        started_at_ms: u64,
    },
    Delegation {
        delegation_id: String,
        capsule: String,
        version: String,
        child_session_id: String,
        child_workdir: String,
    },
}

impl ResidueItem {
    /// The id this item is named by: a `wrk_` work id or a `dlg_` delegation id.
    pub(crate) fn id(&self) -> &str {
        match self {
            Self::DetachedShell { work_id, .. } => work_id,
            Self::Delegation { delegation_id, .. } => delegation_id,
        }
    }

    /// The `kind` discriminator this item carries on the wire.
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::DetachedShell { .. } => "detached_shell",
            Self::Delegation { .. } => "delegation",
        }
    }

    fn to_json(&self) -> serde_json::Value {
        match self {
            Self::DetachedShell {
                work_id,
                binary,
                command,
                started_at_ms,
            } => json!({
                "kind": self.kind(),
                "work_id": work_id,
                "binary": binary,
                "command": command,
                "started_at_ms": started_at_ms,
            }),
            Self::Delegation {
                delegation_id,
                capsule,
                version,
                child_session_id,
                child_workdir,
            } => json!({
                "kind": self.kind(),
                "delegation_id": delegation_id,
                "capsule": capsule,
                "version": version,
                "child_session_id": child_session_id,
                "child_workdir": child_workdir,
            }),
        }
    }
}

/// What was still running at one moment, as observed from the two registries that know.
///
/// A snapshot and not a promise: the door answers from one observation and the loop records
/// another, and the two may differ. Both are honest about the instant they were taken.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Residue {
    items: Vec<ResidueItem>,
}

impl Residue {
    /// Read both registries. `detached` is `None` for a session that demotes nothing — the
    /// script-capsule path and the test helpers — which contributes no shell items rather than an
    /// empty set of unknown provenance.
    pub(crate) fn snapshot(
        detached: Option<&Arc<DetachedRegistry>>,
        delegations: &LiveDelegations,
    ) -> Self {
        let mut items: Vec<ResidueItem> = detached
            .map(|registry| {
                let mut work: Vec<_> = registry.outstanding();
                work.sort_by(|a, b| a.work_id.cmp(&b.work_id));
                work.into_iter()
                    .map(|work| ResidueItem::DetachedShell {
                        work_id: work.work_id,
                        binary: work.binary,
                        command: work.command,
                        started_at_ms: work.started_at_ms,
                    })
                    .collect()
            })
            .unwrap_or_default();
        items.extend(delegations.live());
        Self { items }
    }

    /// Every detached shell work id in this snapshot, for the `task_canceled` record.
    pub(crate) fn detached_work_ids(&self) -> Vec<String> {
        self.items
            .iter()
            .filter(|item| matches!(item, ResidueItem::DetachedShell { .. }))
            .map(|item| item.id().to_string())
            .collect()
    }

    /// Every delegation id in this snapshot, for the `task_canceled` record.
    pub(crate) fn delegation_ids(&self) -> Vec<String> {
        self.items
            .iter()
            .filter(|item| matches!(item, ResidueItem::Delegation { .. }))
            .map(|item| item.id().to_string())
            .collect()
    }

    /// The `residue` artifact for a cancel response, or `None` when nothing was running.
    ///
    /// `None` is what makes "nothing else is running" distinguishable from "these things are":
    /// `A2aTask::artifacts` omits the key entirely, so a caller never has to tell an empty list
    /// from an absent one.
    pub(crate) fn into_artifact(self) -> Option<A2aArtifact> {
        if self.items.is_empty() {
            return None;
        }
        Some(A2aArtifact {
            name: RESIDUE_ARTIFACT.to_string(),
            parts: self
                .items
                .iter()
                .map(|item| ArtifactPart {
                    text: item.to_json().to_string(),
                })
                .collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live_delegation(capsule: &str) -> LiveDelegation {
        LiveDelegation {
            workdir: PathBuf::from("/tmp/child"),
            capsule: capsule.to_string(),
            version: "0.1.0".to_string(),
            child_session_id: "ses_child".to_string(),
            child_workdir: ".murmur/children/child-1".to_string(),
            started: Instant::now(),
        }
    }

    #[test]
    fn a_registered_delegation_is_live_until_it_finishes() {
        let live = LiveDelegations::new();
        assert!(live.live().is_empty());

        live.register("dlg_one".to_string(), live_delegation("worker"));
        let items = live.live();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id(), "dlg_one");
        assert_eq!(items[0].kind(), "delegation");

        let finished = live.finish("dlg_one").expect("the entry is still there");
        assert_eq!(finished.capsule, "worker");
        assert!(live.live().is_empty());
        assert!(live.finish("dlg_one").is_none());
    }

    #[test]
    fn releasing_drops_a_delegation_that_will_never_report() {
        let live = LiveDelegations::new();
        live.register("dlg_one".to_string(), live_delegation("worker"));
        live.release("dlg_one");
        assert!(live.live().is_empty());
        // Releasing an id that is not there is the same nothing, not a panic.
        live.release("dlg_missing");
    }

    #[test]
    fn live_delegations_are_listed_in_id_order() {
        let live = LiveDelegations::new();
        live.register("dlg_b".to_string(), live_delegation("second"));
        live.register("dlg_a".to_string(), live_delegation("first"));
        let listed = live.live();
        let ids: Vec<&str> = listed.iter().map(ResidueItem::id).collect();
        assert_eq!(ids, vec!["dlg_a", "dlg_b"]);
    }

    #[test]
    fn an_empty_residue_produces_no_artifact() {
        assert!(Residue::default().into_artifact().is_none());
        assert!(Residue::snapshot(None, &LiveDelegations::new())
            .into_artifact()
            .is_none());
    }

    #[test]
    fn a_residue_produces_one_json_part_per_item() {
        let live = LiveDelegations::new();
        live.register("dlg_one".to_string(), live_delegation("worker"));
        let residue = Residue {
            items: vec![
                ResidueItem::DetachedShell {
                    work_id: "wrk_abc".to_string(),
                    binary: "sleep".to_string(),
                    command: "sleep 30".to_string(),
                    started_at_ms: 17,
                },
                live.live().remove(0),
            ],
        };
        assert_eq!(residue.detached_work_ids(), vec!["wrk_abc".to_string()]);
        assert_eq!(residue.delegation_ids(), vec!["dlg_one".to_string()]);

        let artifact = residue.into_artifact().expect("two items is not empty");
        assert_eq!(artifact.name, "residue");
        assert_eq!(artifact.parts.len(), 2);

        let shell: serde_json::Value = serde_json::from_str(&artifact.parts[0].text).unwrap();
        assert_eq!(shell["kind"], "detached_shell");
        assert_eq!(shell["work_id"], "wrk_abc");
        assert_eq!(shell["command"], "sleep 30");
        assert_eq!(shell["binary"], "sleep");
        assert_eq!(shell["started_at_ms"], 17);

        let delegation: serde_json::Value = serde_json::from_str(&artifact.parts[1].text).unwrap();
        assert_eq!(delegation["kind"], "delegation");
        assert_eq!(delegation["delegation_id"], "dlg_one");
        assert_eq!(delegation["capsule"], "worker");
        assert_eq!(delegation["child_session_id"], "ses_child");
    }

    #[tokio::test]
    async fn a_cancel_signal_resolves_before_and_after_it_is_raised() {
        let signal = CancelSignal::new();
        assert!(!signal.is_canceled());

        let observer = signal.clone();
        let waiting = tokio::spawn(async move { observer.canceled().await });
        signal.cancel();
        waiting.await.unwrap();

        assert!(signal.is_canceled());
        // A wait armed after the fact resolves immediately rather than blocking on a change
        // that has already happened.
        tokio::time::timeout(std::time::Duration::from_secs(5), signal.canceled())
            .await
            .expect("an already-cancelled signal does not wait");
    }
}
