//! Per-guest execution limits: epoch-based deadlines and store resource caps.
//!
//! Every guest this runtime executes — capsule `run`, tool/driver `run`, and each hook
//! lifecycle call — runs against a `Store` wired with two independent bounds:
//!
//! * an **epoch deadline**, set before each invocation and enforced by [`EpochTicker`],
//!   which advances the engine's epoch on a fixed interval so a guest that never returns
//!   traps instead of hanging the session; and
//! * a **resource limiter** ([`ExecutionLimiter`]) capping linear-memory growth, table
//!   growth, and instance count.
//!
//! Both are sourced from the manifest's `capabilities.limits` block, falling back to the
//! `DEFAULT_*` constants below for any field it omits. A silent manifest means defaults,
//! never "unlimited".
//!
//! ## What an epoch deadline does and does not bound
//!
//! Epoch checks are compiled into wasm loop back-edges and function entries, so the
//! deadline can only fire while guest code is executing. Time spent blocked inside a host
//! call (notably `wasi:io/poll`) elapses against the same wall-clock budget but cannot be
//! interrupted until control returns to wasm. This bounds runaway *guest compute*, which is
//! what makes a spin loop killable; it is not a general-purpose I/O timeout.

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

use wasmtime::{Engine, ResourceLimiter, StoreLimits, StoreLimitsBuilder};

/// How often [`EpochTicker`] advances the engine epoch. Also the granularity of every
/// deadline: a budget is rounded down to a whole number of ticks (minimum one).
pub(crate) const EPOCH_TICK_INTERVAL: Duration = Duration::from_millis(100);

/// Default cap on guest linear-memory growth — 512 MiB. Chosen to sit far above what any
/// real capsule, tool, or hook needs while still bounding a runaway allocation loop.
pub const DEFAULT_MEMORY_BYTES: usize = 512 * 1024 * 1024;

/// Default cap on guest table growth, in elements.
pub const DEFAULT_TABLE_ELEMENTS: usize = 100_000;

/// Default cap on instances a single store may create. A component instantiates one core
/// instance per core module, so single digits is typical; this only stops a pathological
/// instantiation loop.
pub const DEFAULT_INSTANCES: usize = 1_000;

/// Default wall-clock budget for a single guest invocation — 10 minutes.
///
/// Deliberately generous: the budget covers host-call time as well as guest compute (see
/// the module docs), so a capsule whose `run` drives a long agent loop through host
/// `invoke` calls must not trip it. Anything wanting a tight bound sets
/// `capabilities.limits.deadline_seconds` explicitly.
pub const DEFAULT_DEADLINE_SECONDS: u64 = 600;

/// Fully-resolved execution limits for one session's guests: the manifest's
/// `capabilities.limits` block with every omitted field replaced by its `DEFAULT_*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecutionLimits {
    /// Cap on linear-memory growth, in bytes.
    pub memory_bytes: usize,
    /// Cap on table growth, in elements.
    pub table_elements: usize,
    /// Cap on instances a single store may create.
    pub instances: usize,
    /// Wall-clock budget for a single guest invocation, in seconds.
    pub deadline_seconds: u64,
}

impl Default for ExecutionLimits {
    fn default() -> Self {
        Self {
            memory_bytes: DEFAULT_MEMORY_BYTES,
            table_elements: DEFAULT_TABLE_ELEMENTS,
            instances: DEFAULT_INSTANCES,
            deadline_seconds: DEFAULT_DEADLINE_SECONDS,
        }
    }
}

impl ExecutionLimits {
    /// Resolve a manifest `capabilities.limits` block, substituting the default for each
    /// field the manifest left out (and for the whole block when it is absent).
    #[must_use]
    pub fn resolve(declared: Option<&murmur_artifact::ResourceLimits>) -> Self {
        let defaults = Self::default();
        let Some(declared) = declared else {
            return defaults;
        };
        Self {
            memory_bytes: declared.memory_bytes.unwrap_or(defaults.memory_bytes),
            table_elements: declared.table_elements.unwrap_or(defaults.table_elements),
            instances: declared.instances.unwrap_or(defaults.instances),
            deadline_seconds: declared
                .deadline_seconds
                .unwrap_or(defaults.deadline_seconds),
        }
    }

    /// This budget expressed in epoch ticks, for `Store::set_epoch_deadline`.
    ///
    /// Rounds down to whole ticks but never to zero — a deadline of zero ticks would trap
    /// the guest before it executed a single instruction.
    pub(crate) fn deadline_ticks(&self) -> u64 {
        let tick_ms = EPOCH_TICK_INTERVAL.as_millis() as u64;
        (self.deadline_seconds.saturating_mul(1_000) / tick_ms).max(1)
    }

    /// Build the store limiter enforcing these limits.
    pub(crate) fn limiter(&self) -> ExecutionLimiter {
        ExecutionLimiter::new(*self)
    }
}

/// `ResourceLimiter` enforcing an [`ExecutionLimits`] on one store.
///
/// Wraps `wasmtime::StoreLimits` purely to reuse its comparison logic, built with
/// `trap_on_grow_failure(false)` so a denial reaches us as `Ok(false)` instead of
/// wasmtime's own untyped `bail!`. We then convert the denial into an error carrying a
/// message that names the manifest field responsible, and record it.
///
/// The recorded flag — not the error type — is what [`classify_guest_failure`] keys on.
/// wasmtime raises `Trap::AllocationTooLarge` only from its GC and libcall paths, never
/// from a limiter denial, so downcasting the resulting error to `Trap` cannot identify
/// this case and matching on message text would be brittle.
pub(crate) struct ExecutionLimiter {
    inner: StoreLimits,
    limits: ExecutionLimits,
    /// Set the first time this limiter denies a growth request.
    denial: Option<String>,
}

impl ExecutionLimiter {
    fn new(limits: ExecutionLimits) -> Self {
        let inner = StoreLimitsBuilder::new()
            .memory_size(limits.memory_bytes)
            .table_elements(limits.table_elements)
            .instances(limits.instances)
            .trap_on_grow_failure(false)
            .build();
        Self {
            inner,
            limits,
            denial: None,
        }
    }

    /// The limits this limiter enforces.
    pub(crate) fn limits(&self) -> ExecutionLimits {
        self.limits
    }

    /// The message from the first growth request this limiter denied, if any.
    pub(crate) fn denial(&self) -> Option<&str> {
        self.denial.as_deref()
    }
}

impl ResourceLimiter for ExecutionLimiter {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        if self.inner.memory_growing(current, desired, maximum)? {
            return Ok(true);
        }
        let message = format!(
            "linear memory growth to {desired} bytes exceeds \
             capabilities.limits.memory_bytes ({})",
            self.limits.memory_bytes
        );
        self.denial = Some(message.clone());
        Err(wasmtime::Error::msg(message))
    }

    fn table_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        if self.inner.table_growing(current, desired, maximum)? {
            return Ok(true);
        }
        let message = format!(
            "table growth to {desired} elements exceeds \
             capabilities.limits.table_elements ({})",
            self.limits.table_elements
        );
        self.denial = Some(message.clone());
        Err(wasmtime::Error::msg(message))
    }

    fn instances(&self) -> usize {
        self.inner.instances()
    }

    fn tables(&self) -> usize {
        self.inner.tables()
    }

    fn memories(&self) -> usize {
        self.inner.memories()
    }
}

/// Advances `Engine::increment_epoch` every [`EPOCH_TICK_INTERVAL`] for the lifetime of a
/// session's engine. Without a running ticker an epoch deadline can never fire, so exactly
/// one of these is spawned per `Engine` in `stage_session` and its guard is held on
/// `StagedSession` — a `wasmtime::Engine` is an `Arc` handle, so that single ticker reaches
/// every capsule, tool, and hook store cloned from it.
pub(crate) struct EpochTicker {
    stop: Arc<AtomicBool>,
}

impl EpochTicker {
    /// Spawn the ticker thread for `engine`.
    ///
    /// A plain OS thread rather than a tokio task: staging dispatches `on-stage` hooks on
    /// its own current-thread runtime while the rest of the session uses a multi-thread
    /// one, and the ticker must keep running across both regardless of which (if any)
    /// runtime is entered.
    ///
    /// The thread holds only an `EngineWeak`, so it can never keep the engine alive and
    /// exits on its own once the last `Engine` handle drops. The stop flag covers the
    /// reverse case — the guard dropping while cloned `Engine` handles still live in store
    /// states — so the two together bound the thread's life from both ends.
    pub(crate) fn spawn(engine: &Engine) -> Self {
        let weak = engine.weak();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);

        thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                thread::sleep(EPOCH_TICK_INTERVAL);
                match weak.upgrade() {
                    Some(engine) => engine.increment_epoch(),
                    None => break,
                }
            }
        });

        Self { stop }
    }
}

impl Drop for EpochTicker {
    /// Signals the ticker thread to exit. Deliberately does not join: the thread notices
    /// the flag within one tick and exits on its own, and blocking session teardown for up
    /// to `EPOCH_TICK_INTERVAL` to watch it happen buys nothing.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Why a guest invocation failed, separating the two limits this module enforces from an
/// ordinary trap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GuestFailure {
    /// The epoch deadline fired — the guest ran longer than `deadline_seconds`.
    DeadlineExceeded { seconds: u64 },
    /// The store's resource limiter denied a growth request.
    ResourceLimit { message: String },
    /// Anything else: guest panic, `unreachable`, host error.
    Other,
}

/// The single classification point for every failed guest invocation — capsule `run`,
/// tool/driver `run`, and hook lifecycle calls all route their `Err` through here so the
/// three outcomes stay distinguishable without each call site re-deriving the logic.
///
/// A limiter denial is checked first and wins over any trap code, because the denial is
/// recorded at the moment of refusal and is therefore authoritative about the cause; the
/// error wasmtime surfaces afterwards may have been reshaped on its way out of the guest.
pub(crate) fn classify_guest_failure(
    error: &wasmtime::Error,
    limiter: &ExecutionLimiter,
) -> GuestFailure {
    if let Some(message) = limiter.denial() {
        return GuestFailure::ResourceLimit {
            message: message.to_string(),
        };
    }
    if matches!(
        error.downcast_ref::<wasmtime::Trap>(),
        Some(wasmtime::Trap::Interrupt)
    ) {
        return GuestFailure::DeadlineExceeded {
            seconds: limiter.limits().deadline_seconds,
        };
    }
    GuestFailure::Other
}

impl GuestFailure {
    /// Render this failure for the `String`-returning tool/driver and hook paths, which
    /// report failures as plain text rather than a [`crate::errors::RuntimeError`].
    ///
    /// `subject` names the guest, e.g. `tool 'echo'` or `hook 'audit'`. The [`Self::Other`]
    /// wording is the pre-existing generic phrasing, kept verbatim so only the two limit
    /// cases read differently.
    pub(crate) fn message(&self, subject: &str, error: &wasmtime::Error) -> String {
        match self {
            Self::DeadlineExceeded { seconds } => format!(
                "{subject} exceeded its {seconds}s execution deadline \
                 (capabilities.limits.deadline_seconds) and was interrupted"
            ),
            Self::ResourceLimit { message } => {
                format!("{subject} exceeded its configured resource limits: {message}")
            }
            Self::Other => format!("{subject} trapped: {error}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_uses_defaults_when_manifest_is_silent() {
        assert_eq!(ExecutionLimits::resolve(None), ExecutionLimits::default());
    }

    #[test]
    fn resolve_fills_each_omitted_field_independently() {
        let declared = murmur_artifact::ResourceLimits {
            memory_bytes: Some(1024),
            table_elements: None,
            instances: None,
            deadline_seconds: Some(5),
        };
        let limits = ExecutionLimits::resolve(Some(&declared));

        assert_eq!(limits.memory_bytes, 1024);
        assert_eq!(limits.deadline_seconds, 5);
        assert_eq!(limits.table_elements, DEFAULT_TABLE_ELEMENTS);
        assert_eq!(limits.instances, DEFAULT_INSTANCES);
    }

    #[test]
    fn deadline_ticks_converts_seconds_and_never_yields_zero() {
        let limits = ExecutionLimits {
            deadline_seconds: 1,
            ..ExecutionLimits::default()
        };
        // 1s / 100ms per tick.
        assert_eq!(limits.deadline_ticks(), 10);
        assert_eq!(ExecutionLimits::default().deadline_ticks(), 6_000);
    }

    /// A limiter that has denied a growth request classifies as `ResourceLimit` even though
    /// the resulting error is a plain message with no `Trap` to downcast to — the case the
    /// recorded-denial design exists to cover.
    #[test]
    fn classify_reports_resource_limit_after_a_denial() {
        let limits = ExecutionLimits {
            memory_bytes: 1024,
            ..ExecutionLimits::default()
        };
        let mut limiter = limits.limiter();
        let error = limiter
            .memory_growing(0, 4096, None)
            .expect_err("growth past memory_bytes must be denied");

        match classify_guest_failure(&error, &limiter) {
            GuestFailure::ResourceLimit { message } => {
                assert!(message.contains("capabilities.limits.memory_bytes"));
            }
            other => panic!("expected ResourceLimit, got {other:?}"),
        }
    }

    #[test]
    fn classify_reports_deadline_for_an_interrupt_trap() {
        let limits = ExecutionLimits {
            deadline_seconds: 7,
            ..ExecutionLimits::default()
        };
        let limiter = limits.limiter();
        let error = wasmtime::Error::from(wasmtime::Trap::Interrupt);

        assert_eq!(
            classify_guest_failure(&error, &limiter),
            GuestFailure::DeadlineExceeded { seconds: 7 }
        );
    }

    /// An ordinary guest panic must stay in the generic bucket, and must keep the exact
    /// pre-slice wording on the string-returning paths.
    #[test]
    fn classify_reports_other_for_an_unrelated_trap() {
        let limiter = ExecutionLimits::default().limiter();
        let error = wasmtime::Error::from(wasmtime::Trap::UnreachableCodeReached);

        let failure = classify_guest_failure(&error, &limiter);
        assert_eq!(failure, GuestFailure::Other);
        assert!(failure
            .message("tool 'echo'", &error)
            .starts_with("tool 'echo' trapped: "));
    }

    #[test]
    fn limiter_allows_growth_within_the_cap() {
        let limits = ExecutionLimits {
            memory_bytes: 4096,
            ..ExecutionLimits::default()
        };
        let mut limiter = limits.limiter();

        assert!(limiter.memory_growing(0, 4096, None).unwrap());
        assert!(limiter.denial().is_none());
    }
}
