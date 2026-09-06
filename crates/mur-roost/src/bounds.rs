//! How far a delegation chain may go, how wide, and how much of the host one daemon may hold.
//!
//! Three bounds, all the operator's and all refereed from this daemon's own records. Depth is a
//! budget: a session holds a number, and every child it is approved to launch holds one less, so a
//! chain terminates after `--max-depth` links whatever the manifests say. Concurrency is a census:
//! how many children of one session are live right now, counted against `--max-concurrent`. The
//! machine ceiling is the same census without the parent filter: every live capsule this daemon
//! has admitted, across every formation, counted against `--max-live-capsules`.
//!
//! Depth and concurrency ask about the asking session; the machine ceiling asks about the host, so
//! a parent comfortably inside its own `--max-concurrent` is refused when the machine is full. The
//! two refusals say different things to a caller: the per-parent one means this formation is too
//! wide, the machine one means come back later.
//!
//! The depth budget rides the approval: `POST /spawn` seals the child's remaining depth into the
//! MAC'd approval it mints, and `POST /register` reads it back out. No request body carries a depth
//! field, so neither capsule can state the number.
//!
//! [`live_children`] counts a parent's unredeemed [`crate::PendingApproval`]s alongside its running
//! children. Counting only registrations would let a parent hold two approvals at once under a cap
//! of one, because at the moment of the second `POST /spawn` neither child exists.

use std::collections::HashMap;
use std::fmt;

use crate::{JobRecord, JobStatus};

/// Links a delegation chain may have below a session that registered with no approval.
///
/// There is no value meaning unlimited, so every chain terminates.
pub const DEFAULT_MAX_DEPTH: u32 = 3;

/// Children one session may have live at once.
pub const DEFAULT_MAX_CONCURRENT: u32 = 4;

/// Live capsules one core is assumed to carry, before clamping.
///
/// Eight rather than one because a capsule spends most of its life waiting on inference rather
/// than burning a core, and what each one may consume is already bounded by
/// `capabilities.resources`.
pub const DEFAULT_CAPSULES_PER_CORE: u32 = 8;

/// The lowest ceiling the host-derived default takes, so a single-core box still runs a formation.
pub const MIN_MAX_LIVE_CAPSULES: u32 = 16;

/// The highest ceiling the host-derived default takes, so a 128-core machine does not default to a
/// number no operator chose.
pub const MAX_MAX_LIVE_CAPSULES: u32 = 256;

/// A bound that refused a spawn, and the figures an operator needs to act on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoundRefusal {
    /// The asking session's depth budget is spent.
    DepthExhausted { max_depth: u32 },
    /// The asking session already holds as many live children as the daemon allows.
    ConcurrencyReached { max_concurrent: u32, live: u32 },
    /// This daemon already holds as many live capsules on this host as the operator allows,
    /// counted across every formation rather than against the asking session.
    MachineCeilingReached { max_live_capsules: u32, live: u32 },
}

impl fmt::Display for BoundRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DepthExhausted { max_depth } => write!(
                f,
                "delegation depth bound reached: this daemon allows {max_depth} levels of \
                 delegation below a top-level capsule (--max-depth {max_depth}), and this session \
                 has none left to spend — a capsule whose capabilities.spawn.allow names itself \
                 terminates here rather than recursing",
            ),
            Self::ConcurrencyReached {
                max_concurrent,
                live,
            } => write!(
                f,
                "delegation concurrency bound reached: this daemon allows a capsule \
                 {max_concurrent} live children at a time (--max-concurrent {max_concurrent}), and \
                 this session already holds {live} — wait for one to finish, or raise \
                 --max-concurrent",
            ),
            Self::MachineCeilingReached {
                max_live_capsules,
                live,
            } => write!(
                f,
                "machine capsule ceiling reached: this daemon allows {max_live_capsules} live \
                 capsules on this host across every formation \
                 (--max-live-capsules {max_live_capsules}), and it is already holding {live} — \
                 this is the host's ceiling rather than this capsule's own --max-concurrent, so \
                 narrowing the formation does not help; the same request may be granted once \
                 other capsules finish",
            ),
        }
    }
}

/// How many children one session holds right now.
///
/// A child counts from the moment its parent is approved to launch it until that approval is
/// redeemed or expires, and from registration until it deregisters. The two never double-count the
/// same child: redeeming an approval removes its [`crate::PendingApproval`] in the same request that
/// inserts the child's record.
pub fn live_children(jobs: &HashMap<String, JobRecord>, session_id: &str, now_ms: u64) -> u32 {
    let registered = jobs
        .values()
        .filter(|job| {
            job.status == JobStatus::Running && job.parent_session.as_deref() == Some(session_id)
        })
        .count();
    let reserved = jobs
        .get(session_id)
        .map(|job| {
            job.pending
                .iter()
                .filter(|pending| pending.expires_at_ms > now_ms)
                .count()
        })
        .unwrap_or(0);
    (registered + reserved) as u32
}

/// How many capsules this daemon holds live on the host right now, across every formation.
///
/// [`live_children`] without the parent filter. Every running session counts however it got there:
/// a root that registered with no approval and delegates nothing occupies a machine slot, because
/// the host does not care whether a capsule was delegated or started by hand. An unredeemed
/// approval counts until it is redeemed or its expiry passes, on the same terms as a child's slot.
///
/// Records the daemon has deregistered do not count, which is what gives a slot back — the map
/// itself only grows, so a count of its length would refuse forever after the first hundred
/// formations.
pub fn live_capsules(jobs: &HashMap<String, JobRecord>, now_ms: u64) -> u32 {
    let registered = jobs
        .values()
        .filter(|job| job.status == JobStatus::Running)
        .count();
    let reserved = jobs
        .values()
        .flat_map(|job| job.pending.iter())
        .filter(|pending| pending.expires_at_ms > now_ms)
        .count();
    (registered + reserved) as u32
}

/// The machine ceiling an operator gets who never sets `--max-live-capsules`.
///
/// Derived from the host rather than fixed at a constant, so a laptop and a build VM run under
/// different numbers. When the core count cannot be read the floor is used, not the cap: a bound
/// that cannot read its input takes the more restrictive value.
pub fn default_max_live_capsules() -> u32 {
    match std::thread::available_parallelism() {
        Ok(cores) => max_live_capsules_for_cores(cores.get()),
        Err(_) => MIN_MAX_LIVE_CAPSULES,
    }
}

/// The arithmetic behind [`default_max_live_capsules`], separated from the host probe so both
/// clamp ends can be exercised without faking a core count.
fn max_live_capsules_for_cores(cores: usize) -> u32 {
    let derived = (cores as u64).saturating_mul(DEFAULT_CAPSULES_PER_CORE as u64);
    derived.clamp(MIN_MAX_LIVE_CAPSULES as u64, MAX_MAX_LIVE_CAPSULES as u64) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PendingApproval;
    use capsule_runtime::SpawnEnvelope;

    fn record(status: JobStatus, pending: Vec<PendingApproval>) -> JobRecord {
        JobRecord {
            status,
            envelope: SpawnEnvelope::default(),
            depth_remaining: 3,
            parent_session: None,
            pending,
        }
    }

    fn pending(jti: &str, expires_at_ms: u64) -> PendingApproval {
        PendingApproval {
            jti: jti.to_string(),
            expires_at_ms,
        }
    }

    #[test]
    fn the_derivation_multiplies_per_core_between_both_clamps() {
        // Below the floor at one and two cores: the clamp, not the multiple, decides.
        assert_eq!(max_live_capsules_for_cores(1), MIN_MAX_LIVE_CAPSULES);
        assert_eq!(max_live_capsules_for_cores(2), MIN_MAX_LIVE_CAPSULES);
        // Eight cores lands between the clamps, where the per-core multiple is the answer.
        assert_eq!(max_live_capsules_for_cores(8), 64);
        assert_eq!(max_live_capsules_for_cores(4096), MAX_MAX_LIVE_CAPSULES);
    }

    /// The public derivation reads this host, so the only thing that holds on every machine is
    /// that it lands inside the clamps.
    #[test]
    fn the_host_derived_default_lands_inside_the_clamps() {
        let derived = default_max_live_capsules();
        assert!(
            (MIN_MAX_LIVE_CAPSULES..=MAX_MAX_LIVE_CAPSULES).contains(&derived),
            "{derived}",
        );
    }

    /// Expiry is evaluated at read time — no sweep and no timer — so an approval nobody redeemed
    /// stops occupying its machine slot the moment its expiry passes.
    #[test]
    fn an_expired_approval_holds_no_machine_slot() {
        let now = 1_000_000;
        let mut jobs = HashMap::new();
        jobs.insert(
            "ses_holder".to_string(),
            record(
                JobStatus::Running,
                vec![
                    pending("jti_expired", now - 1),
                    pending("jti_live", now + 1),
                ],
            ),
        );

        assert_eq!(live_capsules(&jobs, now), 2);
        // Past the second approval's expiry too, only the running session is left.
        assert_eq!(live_capsules(&jobs, now + 1), 1);
    }

    /// The census counts running records, never map length: a deregistered session's record stays
    /// in the map forever and must stop occupying a slot.
    #[test]
    fn a_deregistered_record_holds_no_machine_slot() {
        let mut jobs = HashMap::new();
        jobs.insert(
            "ses_running".to_string(),
            record(JobStatus::Running, vec![]),
        );
        jobs.insert(
            "ses_complete".to_string(),
            record(JobStatus::Complete, vec![]),
        );
        jobs.insert("ses_failed".to_string(), record(JobStatus::Failed, vec![]));

        assert_eq!(jobs.len(), 3);
        // No record holds an approval, so the clock cannot change the answer.
        assert_eq!(live_capsules(&jobs, 0), 1);
    }
}
