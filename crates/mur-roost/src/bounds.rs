//! How far a delegation chain may go, and how wide.
//!
//! Two bounds, both the operator's and both refereed from this daemon's own records. Depth is a
//! budget: a session holds a number, and every child it is approved to launch holds one less, so a
//! chain terminates after `--max-depth` links whatever the manifests say. Concurrency is a census:
//! how many children of one session are live right now, counted against `--max-concurrent`.
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

/// A bound that refused a spawn, and the figures an operator needs to act on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoundRefusal {
    /// The asking session's depth budget is spent.
    DepthExhausted { max_depth: u32 },
    /// The asking session already holds as many live children as the daemon allows.
    ConcurrencyReached { max_concurrent: u32, live: u32 },
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
