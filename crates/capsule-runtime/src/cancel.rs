//! Cancelling one running task without ending the session it runs in.
//!
//! Three things live here, because all three are read by both ends of a cancel — the A2A door
//! answering `tasks/cancel` and the agent loop that stops:
//!
//! * [`CancelSignal`], the one-way flag a cancelled task's waits race against;
//! * [`LiveDelegations`], the running task's delegations, which the door delivers outcomes into;
//! * [`Residue`], a snapshot of what is still running when a task stops.
//!
//! A cancel stops the runtime waiting on work. A detached shell command keeps its own lifecycle
//! and is named. A delegation is the cancelled task's own: the task ends it, once the residue has
//! named it.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};

use serde_json::json;
use tokio::sync::watch;

use crate::{
    a2a::{A2aArtifact, ArtifactPart},
    child_launch::LaunchedChild,
    delegation_plane::DelegationLaunch,
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
/// Cancelled while a `delegate-task` call was waiting for its child to be up, or while a finished
/// attempt was waiting for a sub-capsule's outcome.
pub(crate) const PHASE_DELEGATION: &str = "delegation";
/// Cancelled while a `call-member` call was reaching the callee's door, or while a finished
/// attempt was waiting for a callee's answer.
pub(crate) const PHASE_MEMBER_CALL: &str = "member_call";
/// Cancelled while a `transport: process` harness was running the turn. The runtime interrupts
/// the harness it spawned, and kills it when the interrupt is refused or unavailable.
pub(crate) const PHASE_HARNESS: &str = "harness";
/// Cancelled while a `submit-plan` call was running its steps.
pub(crate) const PHASE_PLAN: &str = "plan";

/// What the terminal `canceled` status frame says. One constant for every transport: a client
/// cannot tell from the frame which one ran the task.
pub(crate) const CANCELED_STATUS_MESSAGE: &str = "task canceled";

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

/// One delegation the running task started and has not yet accounted for.
///
/// Kept because the terminal `delegation` trace line names the capsule and the version, and the
/// completion the child posts carries neither: it names the delegation, and the runtime remembers
/// what that delegation was for. A cancel reads the same entries to name what was in flight.
#[derive(Debug)]
pub(crate) struct LiveDelegation {
    /// The task that started it, and the only task its outcome is delivered into.
    pub(crate) task_id: String,
    /// The child's directory, absolute — where its `completion.json` is read from.
    pub(crate) workdir: PathBuf,
    pub(crate) capsule: String,
    pub(crate) version: String,
    /// The session id the child's runtime minted for itself.
    pub(crate) child_session_id: String,
    /// The child's directory as the parent's own tools address it: relative to the parent's
    /// accessible workdir.
    pub(crate) child_workdir: String,
    /// When the launch was recorded: the start of the backstop's bound, and the duration of a
    /// row closed without the child's own record.
    pub(crate) started: Instant,
    /// The handle that ends the child; dropping it kills and reaps a child still running.
    ///
    /// For a `delegate-task` child, `None` from the launch notice until [`LiveDelegations::adopt`]
    /// attaches it, which is the length of the task's delivery to the child. Always `None` for a
    /// plan `capsule` step's child: the step's blocking `DelegationPlane::delegate` call holds
    /// that handle and is the only thing polling the child, so it is the one that ends it.
    pub(crate) child: Option<LaunchedChild>,
    /// Whether the child's completion has reached the door. An arrived outcome is delivered at the
    /// task's next wake, or closed from its record if the task ends first.
    pub(crate) arrived: bool,
}

impl LiveDelegation {
    /// The row for a child `task_id` has just launched, from its launch notice: no handle yet, no
    /// outcome, and `child_root` (the parent's accessible workdir) joined to the notice's relative
    /// child directory.
    pub(crate) fn launched(task_id: String, child_root: &Path, notice: &DelegationLaunch) -> Self {
        Self {
            task_id,
            workdir: child_root.join(&notice.child_workdir),
            capsule: notice.capsule.clone(),
            version: notice.version.clone(),
            child_session_id: notice.child_session_id.clone(),
            child_workdir: notice.child_workdir.clone(),
            started: Instant::now(),
            child: None,
            arrived: false,
        }
    }
}

/// What the door is to answer a completion naming one delegation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Arrival {
    /// The running task was waiting for it, and now holds it.
    Delivered,
    /// It already arrived once; a repeat is the watcher retrying a post it could not confirm.
    AlreadyDelivered,
    /// No task in this session started it, or the task that did has ended it.
    NotOutstanding,
}

#[derive(Debug, Default)]
struct Delegations {
    /// The task now in scope, set by [`LiveDelegations::scope_task`].
    task_id: Option<String>,
    /// By `dlg_` id. Every entry belongs to the task in scope.
    entries: HashMap<String, LiveDelegation>,
    /// Every delegation whose outcome this session has received, so a repeated post is answered
    /// as the success the first one was.
    delivered: HashSet<String>,
}

/// The running task's delegations: started, not yet accounted for, by `dlg_` id.
///
/// One per session, shared by the door that receives completions and the task loop that delivers
/// them. A session runs one task at a time, so the set belongs to the running task:
/// [`run_task_with_reopens`](crate::runtime) delivers or ends every delegation before the task's
/// `on-task-end`, and dropping its [`DelegationScope`] ends whatever is left.
///
/// Registered from the launch callback, on the blocking thread that launched the child, so a
/// delegation is in the set from the moment the child is up — including when the call that
/// started it is cancelled before it returns.
#[derive(Debug, Default)]
pub(crate) struct LiveDelegations {
    inner: Mutex<Delegations>,
    /// Woken by the door each time an outcome arrives.
    arrival: tokio::sync::Notify,
}

impl LiveDelegations {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Delegations> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Put `task_id` in scope until the returned guard is dropped. Anything an earlier task left
    /// is ended first; the task loop accounts for every delegation, so there is never anything.
    pub(crate) fn scope_task(self: &Arc<Self>, task_id: &str) -> DelegationScope {
        let left = {
            let mut delegations = self.lock();
            delegations.task_id = Some(task_id.to_string());
            std::mem::take(&mut delegations.entries)
        };
        drop(left);
        DelegationScope(Arc::clone(self))
    }

    /// The task now in scope, or `None` outside every task.
    pub(crate) fn task_id(&self) -> Option<String> {
        self.lock().task_id.clone()
    }

    /// Record one launched child for `delegation.task_id`. Kept only while that task is still in
    /// scope; `false` means it has ended, and the child is ended once its handle is dropped.
    pub(crate) fn register(&self, delegation_id: String, delegation: LiveDelegation) -> bool {
        let mut delegations = self.lock();
        if delegations.task_id.as_deref() != Some(delegation.task_id.as_str()) {
            return false;
        }
        delegations.entries.insert(delegation_id, delegation);
        true
    }

    /// Attach the handle that ends `delegation_id`'s child. With no entry left for it — the task
    /// ended it while the launch was finishing — the handle is dropped, which ends the child.
    pub(crate) fn adopt(&self, delegation_id: &str, child: LaunchedChild) {
        let orphan = {
            let mut delegations = self.lock();
            match delegations.entries.get_mut(delegation_id) {
                Some(entry) => {
                    entry.child = Some(child);
                    None
                }
                None => Some(child),
            }
        };
        drop(orphan);
    }

    /// Drop a delegation whose caller closes it itself: one announced by the launcher whose start
    /// then failed, or a plan `capsule` step's that ended without its task being cancelled.
    ///
    /// `true` when a row was removed. The caller that removed it writes the terminal `delegation`
    /// line; `false` means the row is gone already — the task took it to account for — and the
    /// task writes that line instead.
    pub(crate) fn discard(&self, delegation_id: &str) -> bool {
        let entry = self.lock().entries.remove(delegation_id);
        let removed = entry.is_some();
        drop(entry);
        removed
    }

    /// Record that `delegation_id`'s completion reached the door, and wake the task loop.
    pub(crate) fn arrive(&self, delegation_id: &str) -> Arrival {
        let mut delegations = self.lock();
        if delegations.delivered.contains(delegation_id) {
            return Arrival::AlreadyDelivered;
        }
        let Some(entry) = delegations.entries.get_mut(delegation_id) else {
            return Arrival::NotOutstanding;
        };
        if entry.arrived {
            return Arrival::AlreadyDelivered;
        }
        entry.arrived = true;
        drop(delegations);
        self.arrival.notify_one();
        Arrival::Delivered
    }

    /// `(outstanding, arrived)`: delegations still waited on, and outcomes not yet delivered.
    pub(crate) fn counts(&self) -> (usize, usize) {
        let delegations = self.lock();
        let arrived = delegations
            .entries
            .values()
            .filter(|entry| entry.arrived)
            .count();
        (delegations.entries.len() - arrived, arrived)
    }

    /// Wait for the next outcome to arrive, whatever has arrived already. An arrival since the
    /// last wait that nothing was waiting for resolves it at once, so an outcome that lands
    /// between reading [`Self::counts`] and this wait is never missed; it may also resolve with
    /// nothing new, so a caller re-reads the counts after every wake. Cancellation-safe: arrived
    /// delegations stay in the set until taken.
    pub(crate) async fn wait_for_arrival(&self) {
        self.arrival.notified().await;
    }

    /// Wait until at least one outcome has arrived. Resolves at once when one already has.
    #[cfg(test)]
    pub(crate) async fn wait_for_outcome(&self) {
        loop {
            if self.lock().entries.values().any(|entry| entry.arrived) {
                return;
            }
            self.arrival.notified().await;
        }
    }

    /// Every delegation whose outcome has arrived, in id order — which is start order — taken
    /// for delivery.
    pub(crate) fn take_arrived(&self) -> Vec<(String, LiveDelegation)> {
        self.take_where(|entry| entry.arrived)
    }

    /// Every outstanding delegation started more than `bound` ago, taken to be ended.
    pub(crate) fn take_overdue(&self, bound: Duration) -> Vec<(String, LiveDelegation)> {
        let now = Instant::now();
        self.take_where(|entry| {
            !entry.arrived
                && entry
                    .started
                    .checked_add(bound)
                    .is_some_and(|due| due <= now)
        })
    }

    /// The earliest instant an outstanding delegation passes `bound`, or `None` when none is
    /// outstanding or none ever will.
    pub(crate) fn next_overdue(&self, bound: Duration) -> Option<Instant> {
        self.lock()
            .entries
            .values()
            .filter(|entry| !entry.arrived)
            .filter_map(|entry| entry.started.checked_add(bound))
            .min()
    }

    /// Every delegation the task leaves behind, arrived or not, taken to be accounted for.
    pub(crate) fn take_all(&self) -> Vec<(String, LiveDelegation)> {
        self.take_where(|_| true)
    }

    fn take_where(&self, take: impl Fn(&LiveDelegation) -> bool) -> Vec<(String, LiveDelegation)> {
        let mut delegations = self.lock();
        let ids: Vec<String> = delegations
            .entries
            .iter()
            .filter(|(_, entry)| take(entry))
            .map(|(id, _)| id.clone())
            .collect();
        let mut taken: Vec<(String, LiveDelegation)> = ids
            .into_iter()
            .filter_map(|id| {
                let entry = delegations.entries.remove(&id)?;
                if entry.arrived {
                    delegations.delivered.insert(id.clone());
                }
                Some((id, entry))
            })
            .collect();
        taken.sort_by(|a, b| a.0.cmp(&b.0));
        taken
    }

    /// Everything still in flight, as residue items.
    pub(crate) fn live(&self) -> Vec<ResidueItem> {
        let mut items: Vec<ResidueItem> = self
            .lock()
            .entries
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

/// A task kept in scope of a [`LiveDelegations`]. Dropping it leaves no task in scope and ends
/// every delegation still held for it, so a delegation never outlives the task that made it.
pub(crate) struct DelegationScope(Arc<LiveDelegations>);

impl Drop for DelegationScope {
    fn drop(&mut self) {
        let left = {
            let mut delegations = self.0.lock();
            delegations.task_id = None;
            std::mem::take(&mut delegations.entries)
        };
        drop(left);
    }
}

// ── Residue ───────────────────────────────────────────────────────────────────

/// One thing that was still running when a task stopped.
///
/// A detached shell is named and keeps its own lifecycle. A delegation is named here and then
/// ended by the task that started it, so the record still says what was in flight.
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

    /// Every item in this snapshot as its own JSON object, in the order [`Self::snapshot`] fixed.
    ///
    /// The shape a `session/stop` result carries under `residue`, where an empty array is the
    /// answer "nothing" rather than an absence: a session stop has to be able to say that as a
    /// positive fact, which is the one place this differs from [`Self::into_artifact`].
    pub(crate) fn into_json_items(self) -> Vec<serde_json::Value> {
        self.items.iter().map(ResidueItem::to_json).collect()
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
            task_id: "tsk_a".to_string(),
            workdir: PathBuf::from("/tmp/child"),
            capsule: capsule.to_string(),
            version: "0.1.0".to_string(),
            child_session_id: "ses_child".to_string(),
            child_workdir: ".murmur/children/child-1".to_string(),
            started: Instant::now(),
            child: None,
            arrived: false,
        }
    }

    /// A set with `tsk_a` in scope.
    fn scoped() -> (Arc<LiveDelegations>, DelegationScope) {
        let live = Arc::new(LiveDelegations::new());
        let scope = live.scope_task("tsk_a");
        (live, scope)
    }

    /// The vocabulary the `task_canceled` record's `phase` is written from. One word per place
    /// cancellation is checked, and a reader of an old trace matches on these strings.
    #[test]
    fn every_phase_has_its_own_word() {
        let phases = [
            PHASE_QUEUED,
            PHASE_TURN,
            PHASE_INFERENCE,
            PHASE_INPUT,
            PHASE_DELEGATION,
            PHASE_HARNESS,
            PHASE_MEMBER_CALL,
            PHASE_PLAN,
        ];
        assert_eq!(phases[5], "harness");
        assert_eq!(PHASE_PLAN, "plan");
        assert_eq!(
            phases
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            phases.len()
        );
        assert_eq!(CANCELED_STATUS_MESSAGE, "task canceled");
    }

    #[test]
    fn a_registered_delegation_is_live_until_it_is_taken() {
        let (live, _scope) = scoped();
        assert!(live.live().is_empty());

        assert!(live.register("dlg_one".to_string(), live_delegation("worker")));
        let items = live.live();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id(), "dlg_one");
        assert_eq!(items[0].kind(), "delegation");

        let taken = live.take_all();
        assert_eq!(taken.len(), 1);
        assert_eq!(taken[0].1.capsule, "worker");
        assert!(live.live().is_empty());
        assert!(live.take_all().is_empty());
    }

    /// Discarding says whether it removed the row, because whoever removes a row writes its
    /// terminal `delegation` line, and a row the task already took is the task's to close.
    #[test]
    fn discarding_reports_whether_it_removed_the_row() {
        let (live, _scope) = scoped();
        live.register("dlg_one".to_string(), live_delegation("worker"));
        assert!(live.discard("dlg_one"));
        assert!(live.live().is_empty());
        assert!(!live.discard("dlg_one"), "a second discard removes nothing");
        assert!(!live.discard("dlg_missing"));

        live.register("dlg_two".to_string(), live_delegation("worker"));
        assert_eq!(live.take_all().len(), 1);
        assert!(
            !live.discard("dlg_two"),
            "a row the task took to account for is not the caller's to close"
        );
    }

    #[test]
    fn live_delegations_are_listed_in_id_order() {
        let (live, _scope) = scoped();
        live.register("dlg_b".to_string(), live_delegation("second"));
        live.register("dlg_a".to_string(), live_delegation("first"));
        let listed = live.live();
        let ids: Vec<&str> = listed.iter().map(ResidueItem::id).collect();
        assert_eq!(ids, vec!["dlg_a", "dlg_b"]);
    }

    /// A delegation belongs to the task that started it: outside that task's scope it is not
    /// kept, and the scope's end takes everything left with it.
    #[test]
    fn a_delegation_is_kept_only_while_its_task_is_in_scope() {
        let live = Arc::new(LiveDelegations::new());
        assert!(live.task_id().is_none());
        assert!(
            !live.register("dlg_early".to_string(), live_delegation("worker")),
            "no task is in scope"
        );

        let scope = live.scope_task("tsk_b");
        assert_eq!(live.task_id().as_deref(), Some("tsk_b"));
        assert!(
            !live.register("dlg_other".to_string(), live_delegation("worker")),
            "started for tsk_a, which is not the task in scope"
        );
        let mut mine = live_delegation("worker");
        mine.task_id = "tsk_b".to_string();
        assert!(live.register("dlg_mine".to_string(), mine));
        assert_eq!(live.counts(), (1, 0));

        drop(scope);
        assert!(live.task_id().is_none());
        assert!(live.live().is_empty(), "the scope's end takes what is left");
        assert_eq!(live.arrive("dlg_mine"), Arrival::NotOutstanding);
    }

    /// The door's three answers: an outstanding delegation is delivered once, a repeat is
    /// answered as the success the first post was, and an id nobody is waiting for is refused.
    #[test]
    fn the_set_delivers_each_outcome_once_and_refuses_one_nobody_waits_for() {
        let (live, _scope) = scoped();
        live.register("dlg_a".to_string(), live_delegation("worker"));
        live.register("dlg_b".to_string(), live_delegation("worker"));

        assert_eq!(live.arrive("dlg_a"), Arrival::Delivered);
        assert_eq!(live.arrive("dlg_a"), Arrival::AlreadyDelivered);
        assert_eq!(live.arrive("dlg_nobody"), Arrival::NotOutstanding);
        assert_eq!(live.counts(), (1, 1));

        let arrived = live.take_arrived();
        assert_eq!(arrived.len(), 1);
        assert_eq!(arrived[0].0, "dlg_a");
        assert_eq!(live.counts(), (1, 0));
        // Taken for delivery, and still a success when the watcher posts it again.
        assert_eq!(live.arrive("dlg_a"), Arrival::AlreadyDelivered);

        let left = live.take_all();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].0, "dlg_b");
        // Ended unarrived: a later post finds nobody waiting.
        assert_eq!(live.arrive("dlg_b"), Arrival::NotOutstanding);
    }

    /// The backstop's instant is the earliest outstanding start plus the bound, and only an
    /// outstanding delegation past it is taken.
    #[test]
    fn the_backstop_takes_only_an_outstanding_delegation_past_its_bound() {
        let (live, _scope) = scoped();
        let mut old = live_delegation("worker");
        old.started = Instant::now() - Duration::from_secs(120);
        let old_started = old.started;
        live.register("dlg_old".to_string(), old);
        live.register("dlg_new".to_string(), live_delegation("worker"));
        let mut arrived = live_delegation("worker");
        arrived.started = Instant::now() - Duration::from_secs(600);
        live.register("dlg_arrived".to_string(), arrived);
        live.arrive("dlg_arrived");

        let bound = Duration::from_secs(60);
        assert_eq!(live.next_overdue(bound), Some(old_started + bound));
        let overdue = live.take_overdue(bound);
        let ids: Vec<&str> = overdue.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["dlg_old"]);
        assert_eq!(live.counts(), (1, 1));
    }

    /// An arrival wakes a waiter, and a wait armed after it resolves at once.
    #[tokio::test]
    async fn an_arrival_wakes_the_wait() {
        let (live, _scope) = scoped();
        live.register("dlg_a".to_string(), live_delegation("worker"));
        let door = Arc::clone(&live);
        let waiting = tokio::spawn(async move {
            tokio::time::timeout(Duration::from_secs(5), live.wait_for_outcome())
                .await
                .expect("the arrival wakes the wait");
            live
        });
        tokio::task::yield_now().await;
        assert_eq!(door.arrive("dlg_a"), Arrival::Delivered);
        let live = waiting.await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), live.wait_for_outcome())
            .await
            .expect("an outcome already there does not wait");
    }

    /// `wait_for_arrival` resolves once per arrival — one that landed before the wait included —
    /// and an outcome already taken note of does not resolve it again, so a task can wait for
    /// every outstanding delegation without draining the ones that have arrived.
    #[tokio::test]
    async fn each_arrival_wakes_the_wait_for_the_next_one() {
        let (live, _scope) = scoped();
        live.register("dlg_a".to_string(), live_delegation("worker"));
        live.register("dlg_b".to_string(), live_delegation("worker"));
        assert_eq!(live.arrive("dlg_a"), Arrival::Delivered);
        tokio::time::timeout(Duration::from_secs(5), live.wait_for_arrival())
            .await
            .expect("an arrival nobody was waiting for resolves the next wait");
        assert!(
            tokio::time::timeout(Duration::from_millis(100), live.wait_for_arrival())
                .await
                .is_err(),
            "an arrival already woken for does not resolve the wait again"
        );
        assert_eq!(
            live.counts(),
            (1, 1),
            "the arrived delegation stays in the set"
        );

        let door = Arc::clone(&live);
        let waiting = tokio::spawn(async move {
            tokio::time::timeout(Duration::from_secs(5), live.wait_for_arrival())
                .await
                .expect("the arrival wakes the wait");
            live
        });
        tokio::task::yield_now().await;
        assert_eq!(door.arrive("dlg_b"), Arrival::Delivered);
        let live = waiting.await.unwrap();
        assert_eq!(live.counts(), (0, 2));
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
        let (live, _scope) = scoped();
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
