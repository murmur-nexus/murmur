//! Host-process resource bounds for every native subprocess this runtime spawns.
//!
//! Sibling of [`crate::limits`], and deliberately not a replacement for it: that module bounds
//! a WASM *guest* inside its wasmtime store (linear memory, tables, epoch deadline), this one
//! bounds the *operating-system processes* the runtime forks — `shell::execute_shell` and
//! `runtime::dispatch_native_tool`. A capsule that cannot escape its containment can still
//! wedge the host it runs on: fork bombs, unbounded open files, unbounded writes, unbounded
//! per-process CPU and address space, and unbounded workdir growth are all *denial of service*,
//! not boundary defeats — nothing outside the granted scope is read, written, or reached — but
//! a wedged host is still a broken host.
//!
//! Three mechanisms, in descending order of portability:
//!
//! * **`setrlimit(2)` ceilings** ([`apply_hard_rlimits`]), applied inside the forked child's
//!   `pre_exec` window on every Unix platform. Per-process, POSIX-portable, no configuration
//!   required of the host.
//! * **a cgroup v2 scope** ([`crate::cgroup`]) around the whole subprocess *tree*, Linux only.
//!   This is what rlimits structurally cannot do: `RLIMIT_NPROC` is a **per-uid** ceiling, so a
//!   tree of distinct, rapidly-forking, short-lived processes evades it in practice even when
//!   set correctly. `pids.max` on a cgroup is per-cgroup and does not.
//! * **a periodic workdir-size check** ([`WorkdirGuard`]), on every platform. A plain filesystem
//!   walk on a fixed cadence, so a breach is caught within one poll interval — not instantly.
//!
//! ## A silent manifest means defaults, never "unlimited"
//!
//! [`HostResourceLimits::resolve`] follows [`crate::limits::ExecutionLimits::resolve`] exactly:
//! the manifest's `capabilities.resources` block with every omitted field replaced by its
//! `DEFAULT_*` constant, and the whole block's absence treated as "every field omitted". There
//! is no "unset" state downstream — a capsule that declares nothing is bounded identically to
//! one that declares every default by hand.
//!
//! ## Hard limits, not soft ones
//!
//! `setrlimit(2)` takes both a soft (`rlim_cur`) and a hard (`rlim_max`) value, and an
//! unprivileged process may raise its own soft limit up to the hard one at any time. Against a
//! hostile agent a soft-only cap is therefore advisory: one `setrlimit` call from inside the
//! capsule undoes it. Every limit this module sets writes `rlim_cur == rlim_max`, leaving no
//! ceiling to raise into. Lowering `rlim_max` is irreversible for an unprivileged process,
//! which is precisely the property being bought.

use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

/// Default `RLIMIT_NPROC` headroom — how many processes past the runtime's own uid baseline a
/// subprocess tree may add. See [`apply_hard_rlimits`] for why this is headroom rather than an
/// absolute ceiling.
pub const DEFAULT_MAX_PROCESSES: u64 = 128;

/// Default `RLIMIT_NOFILE` hard ceiling — open descriptors per spawned subprocess.
pub const DEFAULT_MAX_OPEN_FILES: u64 = 1024;

/// Default `RLIMIT_FSIZE` hard ceiling — 4 GiB, the largest single file a subprocess may write.
pub const DEFAULT_MAX_FILE_SIZE_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Default `RLIMIT_CPU` hard ceiling — one hour of CPU time per spawned subprocess.
pub const DEFAULT_CPU_SECONDS: u64 = 3600;

/// Default `RLIMIT_AS` (Linux) / `RLIMIT_DATA` (macOS) hard ceiling — 2 GiB per subprocess.
pub const DEFAULT_MEMORY_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Default cgroup `memory.max` — 4 GiB aggregate across the whole subprocess tree. Above the
/// per-process ceiling on purpose: the tree bound exists to catch what *many* processes do
/// together, so setting it at or below the single-process bound would make it redundant.
pub const DEFAULT_CGROUP_MEMORY_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Default cgroup `pids.max` — 256 tasks aggregate across the whole subprocess tree.
pub const DEFAULT_CGROUP_PIDS_MAX: u64 = 256;

/// Default cgroup `cpu.max` quota as a percentage of one core: 200 = two cores' worth.
pub const DEFAULT_CGROUP_CPU_PERCENT: u32 = 200;

/// Default cgroup `io.max` read+write throughput on the workdir's backing device — 100 MiB/s.
pub const DEFAULT_CGROUP_IO_BYTES_PER_SEC: u64 = 100 * 1024 * 1024;

/// Default ceiling on total workdir size — 10 GiB, checked periodically.
pub const DEFAULT_WORKDIR_MAX_BYTES: u64 = 10 * 1024 * 1024 * 1024;

/// How often [`WorkdirGuard`] walks the workdir tree.
///
/// An internal constant rather than a manifest key, matching [`crate::limits::EPOCH_TICK_INTERVAL`]:
/// the cadence changes *when* a breach is noticed, never *whether* it is, so exposing it would
/// add a knob with no security meaning. The consequence is stated rather than hidden — a disk
/// filler is caught within one interval of crossing the ceiling, not at the instant it crosses.
pub(crate) const WORKDIR_CHECK_INTERVAL: Duration = Duration::from_secs(10);

/// Fully-resolved host-process limits for one session: the manifest's `capabilities.resources`
/// block with every omitted field replaced by its `DEFAULT_*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostResourceLimits {
    /// `RLIMIT_NPROC` headroom above the runtime's own uid baseline. Per-**uid**, not
    /// per-process-tree — see the module docs for why the cgroup `pids.max` below exists
    /// alongside it rather than duplicating it, and [`apply_hard_rlimits`] for why it is applied
    /// as headroom.
    pub max_processes: u64,
    /// `RLIMIT_NOFILE` hard ceiling.
    pub max_open_files: u64,
    /// `RLIMIT_FSIZE` hard ceiling, in bytes.
    pub max_file_size_bytes: u64,
    /// `RLIMIT_CPU` hard ceiling, in CPU-seconds.
    pub cpu_seconds: u64,
    /// `RLIMIT_AS` on Linux, `RLIMIT_DATA` on macOS (which has no `RLIMIT_AS`), in bytes.
    pub memory_bytes: u64,
    /// cgroup v2 `memory.max`, in bytes. Linux only.
    pub cgroup_memory_bytes: u64,
    /// cgroup v2 `pids.max`. Linux only.
    pub cgroup_pids_max: u64,
    /// cgroup v2 `cpu.max` quota as a percentage of one core. Linux only.
    pub cgroup_cpu_percent: u32,
    /// cgroup v2 `io.max` rbps+wbps on the workdir's backing device. Linux only, best-effort.
    pub cgroup_io_bytes_per_sec: u64,
    /// Ceiling on total workdir size, in bytes. Every platform.
    pub workdir_max_bytes: u64,
}

impl Default for HostResourceLimits {
    fn default() -> Self {
        Self {
            max_processes: DEFAULT_MAX_PROCESSES,
            max_open_files: DEFAULT_MAX_OPEN_FILES,
            max_file_size_bytes: DEFAULT_MAX_FILE_SIZE_BYTES,
            cpu_seconds: DEFAULT_CPU_SECONDS,
            memory_bytes: DEFAULT_MEMORY_BYTES,
            cgroup_memory_bytes: DEFAULT_CGROUP_MEMORY_BYTES,
            cgroup_pids_max: DEFAULT_CGROUP_PIDS_MAX,
            cgroup_cpu_percent: DEFAULT_CGROUP_CPU_PERCENT,
            cgroup_io_bytes_per_sec: DEFAULT_CGROUP_IO_BYTES_PER_SEC,
            workdir_max_bytes: DEFAULT_WORKDIR_MAX_BYTES,
        }
    }
}

impl HostResourceLimits {
    /// Resolve a manifest `capabilities.resources` block, substituting the default for each
    /// field the manifest left out (and for the whole block when it is absent).
    #[must_use]
    pub fn resolve(declared: Option<&murmur_artifact::ResourceCapabilities>) -> Self {
        let defaults = Self::default();
        let Some(declared) = declared else {
            return defaults;
        };
        Self {
            max_processes: declared.max_processes.unwrap_or(defaults.max_processes),
            max_open_files: declared.max_open_files.unwrap_or(defaults.max_open_files),
            max_file_size_bytes: declared
                .max_file_size_bytes
                .unwrap_or(defaults.max_file_size_bytes),
            cpu_seconds: declared.cpu_seconds.unwrap_or(defaults.cpu_seconds),
            memory_bytes: declared.memory_bytes.unwrap_or(defaults.memory_bytes),
            cgroup_memory_bytes: declared
                .cgroup_memory_bytes
                .unwrap_or(defaults.cgroup_memory_bytes),
            cgroup_pids_max: declared.cgroup_pids_max.unwrap_or(defaults.cgroup_pids_max),
            cgroup_cpu_percent: declared
                .cgroup_cpu_percent
                .unwrap_or(defaults.cgroup_cpu_percent),
            cgroup_io_bytes_per_sec: declared
                .cgroup_io_bytes_per_sec
                .unwrap_or(defaults.cgroup_io_bytes_per_sec),
            workdir_max_bytes: declared
                .workdir_max_bytes
                .unwrap_or(defaults.workdir_max_bytes),
        }
    }
}

/// Resolve a manifest `capabilities.resources` block into [`HostResourceLimits`].
///
/// Free function mirroring `ExecutionLimits::resolve`'s call shape at the one call site that
/// matters (`types::capability_policy_from_runtime_manifest`), so the two sibling limit blocks
/// read identically there.
#[must_use]
pub(crate) fn resolve(
    declared: Option<&murmur_artifact::ResourceCapabilities>,
) -> HostResourceLimits {
    HostResourceLimits::resolve(declared)
}

/// The named limit a kill signal unambiguously attributes to, or `None`.
///
/// Only two signals qualify, and the list is deliberately not longer:
///
/// * `SIGXCPU` — the kernel raises exactly this for an `RLIMIT_CPU` overrun. Unambiguous.
/// * `SIGXFSZ` — likewise for `RLIMIT_FSIZE`. Unambiguous.
///
/// Everything else stays unattributed **on purpose**. `RLIMIT_AS`/`RLIMIT_DATA` overrun surfaces
/// as `ENOMEM` inside the child's own allocator, not as a parent-visible signal with a unique
/// identity, so mapping `SIGSEGV`/`SIGABRT` to `memory_bytes` would be a guess presented as a
/// fact. `RLIMIT_NPROC` and `RLIMIT_NOFILE` do not kill anything at all: they fail a
/// `fork()`/`open()` inside the child with `EAGAIN`/`EMFILE`, visible in that child's own stderr
/// and exit code. A bare `SIGKILL` with no cgroup evidence has too many causes (host OOM killer,
/// an operator, an unrelated crash) to name one.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn limit_from_signal(signal: i32) -> Option<&'static str> {
    match signal {
        libc::SIGXCPU => Some("cpu_seconds"),
        libc::SIGXFSZ => Some("max_file_size_bytes"),
        _ => None,
    }
}

/// Sets every [`HostResourceLimits`] rlimit field as a **hard** limit (`rlim_cur == rlim_max`),
/// plus `RLIMIT_CORE = 0`.
///
/// Called from inside a forked child's `pre_exec` closure, so everything here is restricted to
/// async-signal-safe operations: two syscalls per limit and no allocation, no locking, and no
/// error formatting (errors are returned as bare `io::Error::from_raw_os_error`).
///
/// `RLIMIT_CORE` is fixed at zero with no manifest surface at all — a core dump of a capsule
/// subprocess writes a potentially multi-gigabyte file, holding whatever was in that process's
/// memory, into the workdir or the host's core pattern. It belongs to the same category as the
/// fixed capsule device set in `sandbox.rs`: an invariant, not a setting.
///
/// A requested value above the inherited hard limit is **clamped down to it** rather than
/// treated as an error: an unprivileged process cannot raise `rlim_max`, so the inherited value
/// is the real ceiling either way, and failing the spawn would only replace a bound we cannot
/// widen with no subprocess at all.
///
/// ## Why `max_processes` is headroom, not an absolute ceiling
///
/// `RLIMIT_NPROC` counts **every process owned by the uid**, not the ones in this tree. A desktop
/// or workstation account routinely runs several hundred (measured: 389 on the machine this was
/// developed on), so a hard `RLIMIT_NPROC` of 128 does not bound the capsule at 128 processes —
/// it makes the very first `fork()` in the subprocess fail with `EAGAIN`, because the uid is
/// already past the limit before the capsule does anything. That is a broken runtime, not a
/// bound.
///
/// So `nproc_baseline` — the uid's process count, measured once in the parent at launch by
/// [`uid_process_count`] — is added to the declared `max_processes`, making the manifest field
/// mean "how many processes past the host's existing usage this capsule's tree may add". That is
/// the only reading of a per-uid limit that is both enforceable and non-breaking. When the count
/// cannot be measured the caller passes `0` and the declared value applies literally, which
/// errs toward the tighter bound.
///
/// This is also, concretely, why the Linux cgroup `pids.max` is not redundant with this: it
/// counts only the tasks in the capsule's own scope, so it needs no baseline and cannot be
/// evaded by the uid's other processes.
#[cfg(unix)]
#[allow(unsafe_code)]
pub(crate) fn apply_hard_rlimits(
    limits: &HostResourceLimits,
    nproc_baseline: u64,
) -> std::io::Result<()> {
    set_hard_rlimit(
        libc::RLIMIT_NPROC as u32,
        nproc_baseline.saturating_add(limits.max_processes),
    )?;
    set_hard_rlimit(libc::RLIMIT_NOFILE as u32, limits.max_open_files)?;
    set_hard_rlimit(libc::RLIMIT_FSIZE as u32, limits.max_file_size_bytes)?;
    set_hard_rlimit(libc::RLIMIT_CPU as u32, limits.cpu_seconds)?;
    set_hard_rlimit(libc::RLIMIT_CORE as u32, 0)?;

    // `memory_bytes` maps to `RLIMIT_AS` (total address space) on Linux, where it is enforced
    // and a failure to set it is fatal.
    #[cfg(target_os = "linux")]
    set_hard_rlimit(libc::RLIMIT_AS as u32, limits.memory_bytes)?;

    // macOS has no `RLIMIT_AS`, and its `RLIMIT_DATA` is present in the headers but not
    // enforceable: the kernel rejects any finite value with `EINVAL`. The call is still made (a
    // BSD that does honour it gets the bound) but its failure is tolerated rather than turned
    // into a failed spawn — refusing to run any subprocess at all on the primary development
    // platform, over a knob that platform's kernel will never implement, would trade a missing
    // bound for a broken runtime. The residual gap is exactly what `W-SEC-010` documents: on a
    // host with no cgroups, memory is bounded by neither an aggregate nor a per-process ceiling.
    #[cfg(not(target_os = "linux"))]
    let _ = set_hard_rlimit(libc::RLIMIT_DATA as u32, limits.memory_bytes);

    Ok(())
}

/// One `getrlimit`/`setrlimit` pair: read the inherited hard limit, clamp `value` to it, then
/// write the result to **both** the soft and hard slot.
#[cfg(unix)]
#[allow(unsafe_code)]
fn set_hard_rlimit(resource: u32, value: u64) -> std::io::Result<()> {
    let mut current = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `resource` is one of the `libc::RLIMIT_*` constants (cast to the width this
    // platform's `setrlimit` takes) and `current` is a live, correctly-sized stack `rlimit`.
    if unsafe { libc::getrlimit(resource as _, &mut current) } != 0 {
        return Err(std::io::Error::last_os_error());
    }

    // `rlim_t` is 64-bit on every platform this runtime targets, but saturate rather than cast
    // so a 32-bit `rlim_t` would clamp to its own maximum instead of silently wrapping.
    let requested = libc::rlim_t::try_from(value).unwrap_or(libc::rlim_t::MAX);
    let target = if current.rlim_max == libc::RLIM_INFINITY {
        requested
    } else {
        requested.min(current.rlim_max)
    };

    let limit = libc::rlimit {
        rlim_cur: target,
        rlim_max: target,
    };
    // SAFETY: same as above; `limit` is a live stack `rlimit` and is only read by the call.
    if unsafe { libc::setrlimit(resource as _, &limit) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// How many processes the runtime's own uid currently owns, or `None` if the host cannot be
/// asked.
///
/// Measured once per launch **in the parent** — it walks `/proc` (Linux) or issues a `sysctl`
/// (macOS), neither of which is async-signal-safe, so it must never be called from `pre_exec`.
/// Its one consumer is the `RLIMIT_NPROC` baseline in [`apply_hard_rlimits`]; see there for why a
/// per-uid limit is meaningless without it.
#[cfg_attr(not(unix), allow(dead_code))]
// Each `cfg` arm returns explicitly so that exactly one of them is live per target without the
// arms having to be arranged so the live one happens to be last — on the target where an arm is
// the final live statement, clippy sees its `return` as redundant, but removing it breaks every
// other target.
#[allow(clippy::needless_return)]
pub(crate) fn uid_process_count() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;

        // A `/proc/<pid>` directory is owned by the uid the process runs as, so a `stat` per
        // entry answers this without opening or parsing a single `status` file.
        // SAFETY: `geteuid` takes no arguments and cannot fail.
        let uid = {
            #[allow(unsafe_code)]
            unsafe {
                libc::geteuid()
            }
        };
        let entries = std::fs::read_dir("/proc").ok()?;
        let count = entries
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.bytes().all(|b| b.is_ascii_digit()))
                    && entry.metadata().is_ok_and(|metadata| metadata.uid() == uid)
            })
            .count();
        return Some(count as u64);
    }

    #[cfg(target_os = "macos")]
    {
        /// `PROC_UID_ONLY` from `<libproc.h>`.
        const PROC_UID_ONLY: u32 = 4;

        // Declared here rather than taken from the `libc` crate, which does not expose libproc on
        // this target. The symbol lives in libSystem, which every macOS binary already links, so
        // this needs no build-script or link attribute. Called with a null buffer, it returns the
        // *byte size* a full listing would need — the count without the listing.
        extern "C" {
            fn proc_listpids(
                r#type: u32,
                typeinfo: u32,
                buffer: *mut libc::c_void,
                buffersize: libc::c_int,
            ) -> libc::c_int;
        }

        // SAFETY: the null-buffer/zero-size form is the documented size query; it writes nothing.
        #[allow(unsafe_code)]
        let bytes = unsafe {
            let uid = libc::geteuid();
            proc_listpids(PROC_UID_ONLY, uid, std::ptr::null_mut(), 0)
        };
        if bytes <= 0 {
            return None;
        }
        return Some(bytes as u64 / std::mem::size_of::<u32>() as u64);
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    None
}

/// A workdir-size ceiling that was crossed: what was allowed, and what was actually observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WorkdirBreach {
    pub(crate) max_bytes: u64,
    pub(crate) observed_bytes: u64,
}

/// Periodic workdir-size check for one session.
///
/// Walks the workdir tree every [`WORKDIR_CHECK_INTERVAL`] on a plain OS thread (not a tokio
/// task — the session runs guests on several runtimes and the guard must outlive all of them),
/// and latches the first breach it sees. Three consumers read that latch:
///
///   * `shell::execute_shell` and `runtime::dispatch_native_tool` refuse to spawn once it is
///     set, which stops a disk filler from writing another byte;
///   * the agent turn loop turns it into [`crate::errors::RuntimeError::WorkdirSizeExceeded`],
///     which terminates the session.
///
/// **Why a periodic check and not a tmpfs-backed workdir with a size mount option.** The
/// structural version needs a mount namespace the runtime does not have: nothing in this
/// codebase's process model ever runs as root, and the `sealed-containment-runtime` work that
/// would create one is a separate, unbuilt roadmap card. A poll is what is available without
/// it, and its one-interval detection lag is stated rather than hidden.
#[derive(Debug)]
pub(crate) struct WorkdirGuard {
    max_bytes: u64,
    breach: Mutex<Option<WorkdirBreach>>,
    stop: AtomicBool,
}

impl WorkdirGuard {
    /// Spawn the checker thread for `workdir`. The returned handle owns the latch; dropping the
    /// last clone stops the thread within one interval.
    pub(crate) fn spawn(workdir: &Path, max_bytes: u64) -> Arc<Self> {
        let guard = Arc::new(Self {
            max_bytes,
            breach: Mutex::new(None),
            stop: AtomicBool::new(false),
        });

        // The thread holds a Weak, so it can never keep the session's guard alive; the stop flag
        // covers the reverse (guard dropped while the thread is mid-sleep). Same both-ends shape
        // as `limits::EpochTicker`.
        let weak = Arc::downgrade(&guard);
        let workdir = workdir.to_path_buf();
        thread::spawn(move || loop {
            thread::sleep(WORKDIR_CHECK_INTERVAL);
            let Some(guard) = weak.upgrade() else {
                break;
            };
            if guard.stop.load(Ordering::Relaxed) || guard.breach().is_some() {
                break;
            }
            let observed = directory_size_bytes(&workdir);
            if observed > guard.max_bytes {
                guard.record_breach(&workdir, observed);
                break;
            }
        });

        guard
    }

    /// The latched breach, if the workdir has crossed its ceiling.
    pub(crate) fn breach(&self) -> Option<WorkdirBreach> {
        self.breach.lock().ok().and_then(|breach| *breach)
    }

    fn record_breach(&self, workdir: &Path, observed_bytes: u64) {
        let breach = WorkdirBreach {
            max_bytes: self.max_bytes,
            observed_bytes,
        };
        if let Ok(mut slot) = self.breach.lock() {
            if slot.is_none() {
                *slot = Some(breach);
            }
        }
        let message = workdir_breach_message(breach);
        eprintln!("[capsule-runtime] {message}");
        crate::agent::append_bootstrap_log(workdir, &format!("[resources] {message}"));
    }
}

impl Drop for WorkdirGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// The one wording for a workdir breach, shared by the guard's log line and by the error the
/// subprocess spawn paths return, so the two cannot drift.
pub(crate) fn workdir_breach_message(breach: WorkdirBreach) -> String {
    format!(
        "workdir grew to {} bytes, past the {} byte ceiling \
         (capabilities.resources.workdir_max_bytes)",
        breach.observed_bytes, breach.max_bytes
    )
}

/// Total size of every regular file beneath `root`, in bytes.
///
/// Iterative rather than recursive, and reads `symlink_metadata` so a symlink counts as its own
/// (tiny) entry and is never followed — a symlinked directory would otherwise let a capsule both
/// hide growth outside the workdir and spin this walk on a cycle. Unreadable entries are skipped:
/// this is a best-effort accounting of growth, and a partial read that under-counts is strictly
/// better than a walk that gives up entirely.
pub(crate) fn directory_size_bytes(root: &Path) -> u64 {
    let mut total: u64 = 0;
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(metadata) = entry.path().symlink_metadata() else {
                continue;
            };
            if metadata.is_dir() {
                stack.push(entry.path());
            } else {
                total = total.saturating_add(metadata.len());
            }
        }
    }

    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_uses_defaults_when_manifest_is_silent() {
        assert_eq!(
            HostResourceLimits::resolve(None),
            HostResourceLimits::default()
        );
    }

    /// The rule this slice inherits from `capabilities.limits`: a manifest that declares the
    /// section but omits a field gets that field's default, never "unlimited".
    #[test]
    fn resolve_fills_each_omitted_field_independently() {
        let declared = murmur_artifact::ResourceCapabilities {
            max_open_files: Some(16),
            cgroup_pids_max: Some(32),
            ..murmur_artifact::ResourceCapabilities::default()
        };
        let limits = HostResourceLimits::resolve(Some(&declared));

        assert_eq!(limits.max_open_files, 16);
        assert_eq!(limits.cgroup_pids_max, 32);
        assert_eq!(limits.max_processes, DEFAULT_MAX_PROCESSES);
        assert_eq!(limits.max_file_size_bytes, DEFAULT_MAX_FILE_SIZE_BYTES);
        assert_eq!(limits.cpu_seconds, DEFAULT_CPU_SECONDS);
        assert_eq!(limits.memory_bytes, DEFAULT_MEMORY_BYTES);
        assert_eq!(limits.cgroup_memory_bytes, DEFAULT_CGROUP_MEMORY_BYTES);
        assert_eq!(limits.cgroup_cpu_percent, DEFAULT_CGROUP_CPU_PERCENT);
        assert_eq!(
            limits.cgroup_io_bytes_per_sec,
            DEFAULT_CGROUP_IO_BYTES_PER_SEC
        );
        assert_eq!(limits.workdir_max_bytes, DEFAULT_WORKDIR_MAX_BYTES);
    }

    #[test]
    fn free_resolve_matches_the_inherent_one() {
        let declared = murmur_artifact::ResourceCapabilities {
            cpu_seconds: Some(5),
            ..murmur_artifact::ResourceCapabilities::default()
        };
        assert_eq!(
            resolve(Some(&declared)),
            HostResourceLimits::resolve(Some(&declared))
        );
    }

    /// Attribution must name a limit only where the kernel's own signal identifies exactly one.
    /// The negative half of this test is the point: over-attributing a `SIGKILL` to
    /// `memory_bytes` would put a guess into the trace as if it were evidence.
    #[test]
    fn only_the_two_unambiguous_signals_name_a_limit() {
        assert_eq!(limit_from_signal(libc::SIGXCPU), Some("cpu_seconds"));
        assert_eq!(
            limit_from_signal(libc::SIGXFSZ),
            Some("max_file_size_bytes")
        );
        for ambiguous in [libc::SIGKILL, libc::SIGSEGV, libc::SIGABRT, libc::SIGTERM] {
            assert_eq!(
                limit_from_signal(ambiguous),
                None,
                "signal {ambiguous} has more than one possible cause and must stay unattributed"
            );
        }
    }

    #[test]
    fn directory_size_sums_nested_files() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("a"), vec![0u8; 100]).unwrap();
        std::fs::create_dir(temp.path().join("nested")).unwrap();
        std::fs::write(temp.path().join("nested").join("b"), vec![0u8; 250]).unwrap();

        assert_eq!(directory_size_bytes(temp.path()), 350);
    }

    #[test]
    fn directory_size_of_a_missing_path_is_zero() {
        assert_eq!(
            directory_size_bytes(Path::new("/nonexistent-murmur-workdir-probe")),
            0
        );
    }

    /// The guard starts un-breached, and stays that way for a workdir well under its ceiling —
    /// the "no false positive" half. The breach half is a real-host manual check (see
    /// `docs/content/reference/resource-limits-manual-verification.md`), not a timing-dependent
    /// unit test.
    #[test]
    fn guard_reports_no_breach_for_a_workdir_under_the_ceiling() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("small"), vec![0u8; 64]).unwrap();

        let guard = WorkdirGuard::spawn(temp.path(), DEFAULT_WORKDIR_MAX_BYTES);
        assert_eq!(guard.breach(), None);
    }

    /// `rlim_cur == rlim_max` is the whole point of this module's rlimit path: a soft-only cap
    /// can be raised back by the capsule itself. Asserted against the live process by lowering
    /// a limit we can afford to lower (`RLIMIT_CORE`, which this runtime pins at zero anyway)
    /// and reading it back.
    /// The baseline must be a real, plausible count — a `0` here would silently turn
    /// `max_processes` back into the absolute per-uid ceiling that makes every `fork()` in a
    /// subprocess fail on any account already running more processes than the limit.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn uid_process_count_reports_a_plausible_baseline() {
        let count = uid_process_count().expect("this platform can be asked for its process count");
        assert!(
            count > 0,
            "the test process itself is owned by this uid, so the count cannot be zero"
        );
    }

    #[cfg(unix)]
    #[test]
    #[allow(unsafe_code)]
    fn set_hard_rlimit_writes_both_the_soft_and_hard_slot() {
        set_hard_rlimit(libc::RLIMIT_CORE as u32, 0).unwrap();

        let mut current = libc::rlimit {
            rlim_cur: 1,
            rlim_max: 1,
        };
        // SAFETY: a plain `getrlimit` read into a live stack `rlimit`.
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_CORE as _, &mut current) },
            0
        );
        assert_eq!(current.rlim_cur, 0);
        assert_eq!(current.rlim_max, 0, "the hard slot must be set, not only the soft one");
    }
}
