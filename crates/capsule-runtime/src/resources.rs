//! Host-process resource bounds for every subprocess this runtime spawns.
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
//!   This is what rlimits structurally cannot do: `RLIMIT_NPROC` is a **per-uid** ceiling,
//!   checked against every thread the uid owns on the host, so it bounds neither this tree nor
//!   anything else in particular. `pids.max` on a cgroup counts only the scope's own tasks, and
//!   a spawn that joins a scope gets no `RLIMIT_NPROC` from this module at all ([`NprocBound`]).
//! * **a periodic workdir-size check** ([`WorkdirGuard`]), on every platform. A plain filesystem
//!   walk on a fixed cadence, so a breach is caught within one poll interval — not instantly.
//!
//! ## A silent manifest: no CPU limit, a default for everything else
//!
//! [`HostResourceLimits::resolve`] reads the manifest's `capabilities.resources` block, and
//! treats the whole block's absence as "every field omitted". The two CPU fields carry over as
//! declared: an omitted `cpu_seconds` sets no `RLIMIT_CPU`, so a subprocess keeps the limit this
//! process inherited, and an omitted `cgroup_cpu_percent` writes no `cpu.max`, so the scope
//! keeps the kernel's `max`. Both change how fast and how long an agent's work may run, not
//! whether the host survives it. Every other field replaces an omission with its `DEFAULT_*`
//! constant, as [`crate::limits::ExecutionLimits::resolve`] does: a capsule that declares none
//! of them is bounded identically to one that declares every default by hand.
//!
//! ## Hard limits, not soft ones
//!
//! `setrlimit(2)` takes both a soft (`rlim_cur`) and a hard (`rlim_max`) value, and an
//! unprivileged process may raise its own soft limit up to the hard one at any time. Against a
//! hostile agent a soft-only cap is therefore advisory: one `setrlimit` call from inside the
//! capsule undoes it. Every limit this module sets writes `rlim_cur == rlim_max`, leaving no
//! ceiling to raise into, and lowering `rlim_max` is irreversible for an unprivileged process.
//!
//! A set `RLIMIT_CPU` is the one exception: its soft slot is `cpu_seconds` and its hard slot one
//! second above ([`cpu_rlimit_slots`]). The kernel sends `SIGXCPU`, the signal
//! [`limit_from_signal`] attributes, only while the soft slot sits below the hard one. The bound
//! is the hard slot: a process that raises its soft slot to the hard one, or ignores `SIGXCPU`,
//! gets `SIGKILL` at `cpu_seconds + 1`, so CPU time per process stays within `cpu_seconds` plus
//! one second.

use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

/// Default `RLIMIT_NPROC` headroom for a spawn with no cgroup scope — how much past the uid's
/// count at that spawn a subprocess tree may add, in whichever unit this platform's
/// `RLIMIT_NPROC` is checked against. Unused for a spawn that joins a scope. See [`NprocBound`]
/// for which spawns get it and [`uid_task_count`] for the per-platform unit.
pub const DEFAULT_MAX_PROCESSES: u64 = 128;

/// Default `RLIMIT_NOFILE` hard ceiling — open descriptors per spawned subprocess.
pub const DEFAULT_MAX_OPEN_FILES: u64 = 1024;

/// Default `RLIMIT_FSIZE` hard ceiling — 4 GiB, the largest single file a subprocess may write.
pub const DEFAULT_MAX_FILE_SIZE_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Default `RLIMIT_AS` (Linux) / `RLIMIT_DATA` (macOS) hard ceiling — 2 GiB per subprocess.
pub const DEFAULT_MEMORY_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// The `RLIMIT_CORE` of every process spawned at a tier other than `EnforcementTier::Unconfined`:
/// one byte, the value at which Linux writes no core dump under any `kernel.core_pattern`.
///
/// - A file pattern: the kernel skips a dump when the limit is below one page, as it does at `0`.
/// - A pipe pattern (`|/usr/share/apport/apport …`, `|…/systemd-coredump …`): the kernel ignores
///   `RLIMIT_CORE`, except that at exactly `1` it aborts the dump and logs `RLIMIT_CORE is set to
///   1, aborting core`. At `0` it starts the program as root, outside the capsule, and writes the
///   crashed process's memory to that program's stdin. The crashed process does not exit until
///   the program has read all of it or closed its stdin, so a program that does neither holds
///   that process, and the command waiting on it, until the session ends.
///
/// [`set_hard_rlimit`] clamps it to the hard limit `mur` inherited, so under an inherited hard
/// limit of `0` the pipe pattern still receives the dump. On macOS the kernel writes no core
/// larger than the limit.
pub(crate) const CORE_DUMP_LIMIT_BYTES: u64 = 1;

/// The address-space limit below which `W-RUN-010` warns for a V8 binary: 32 GiB. V8 reserves
/// far more address space than it uses, and `node` failed with "Cannot allocate Wasm memory" at
/// 16 GiB and ran at 32 GiB under `ulimit -v`.
pub const V8_ADDRESS_SPACE_FLOOR_BYTES: u64 = 32 * 1024 * 1024 * 1024;

/// The `capabilities.shell.allow` file names `W-RUN-010` treats as V8.
pub const V8_BINARY_NAMES: &[&str] = &["node", "nodejs"];

/// `W-RUN-010` as staging and `mur doctor` print it, or `None` when it does not fire.
///
/// It fires when an entry of `shell_allow` is named (by its file name) one of
/// [`V8_BINARY_NAMES`] and the effective address-space limit is below
/// [`V8_ADDRESS_SPACE_FLOOR_BYTES`]: `declared_memory_bytes` or [`DEFAULT_MEMORY_BYTES`] on a
/// contained tier, where the limit always applies; `declared_memory_bytes` alone on an unconfined
/// one, where an undeclared limit sets none.
#[must_use]
pub fn v8_address_space_warning(
    shell_allow: &[String],
    declared_memory_bytes: Option<u64>,
    unconfined: bool,
) -> Option<String> {
    use murmur_artifact::{runtime_warning_link, W_RUN_010};
    let binary = shell_allow.iter().find_map(|entry| {
        let name = std::path::Path::new(entry).file_name()?.to_str()?;
        V8_BINARY_NAMES.contains(&name).then_some(name)
    })?;
    let limit = match (declared_memory_bytes, unconfined) {
        (Some(declared), _) => declared,
        (None, false) => DEFAULT_MEMORY_BYTES,
        (None, true) => return None,
    };
    if limit >= V8_ADDRESS_SPACE_FLOOR_BYTES {
        return None;
    }
    let source = if declared_memory_bytes.is_some() {
        "capabilities.resources.memory_bytes"
    } else {
        "the default of capabilities.resources.memory_bytes, which is not declared"
    };
    Some(format!(
        "warning[{W_RUN_010}]: capabilities.shell.allow names {binary}, and its commands run under \
         an address-space limit of {limit} bytes ({source}). memory_bytes is RLIMIT_AS, and V8 \
         reserves far more address space than it uses, so {binary} can fail with \
         \"Cannot allocate Wasm memory\" below {V8_ADDRESS_SPACE_FLOOR_BYTES} bytes (32 GiB). \
         memory_bytes: 68719476736 (64 GiB) works; cap RAM with \
         capabilities.resources.cgroup_memory_bytes instead ({})",
        runtime_warning_link(W_RUN_010)
    ))
}

/// Default cgroup `memory.max` — 4 GiB aggregate across the whole subprocess tree. Above the
/// per-process ceiling on purpose: the tree bound exists to catch what *many* processes do
/// together, so setting it at or below the single-process bound would make it redundant.
pub const DEFAULT_CGROUP_MEMORY_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Default cgroup `pids.max` — 256 tasks aggregate across the whole subprocess tree.
pub const DEFAULT_CGROUP_PIDS_MAX: u64 = 256;

/// Default cgroup `io.max` read+write throughput on the workdir's backing device — 100 MiB/s.
pub const DEFAULT_CGROUP_IO_BYTES_PER_SEC: u64 = 100 * 1024 * 1024;

/// Default ceiling on total workdir size — 10 GiB, checked periodically.
pub const DEFAULT_WORKDIR_MAX_BYTES: u64 = 10 * 1024 * 1024 * 1024;

/// The shortest pause between two of [`WorkdirGuard`]'s walks of the workdir tree.
///
/// An internal constant rather than a manifest key, matching [`crate::limits::EPOCH_TICK_INTERVAL`]:
/// the cadence changes *how far past the ceiling* a writer gets before it is stopped, never
/// *whether* the breach is reported (that is [`WorkdirGuard::settle`]'s job, and it does not
/// depend on the cadence at all). A walk that took longer than this to finish is followed by a
/// pause twice as long as the walk, so a very large tree costs at most a third of a core.
pub(crate) const WORKDIR_CHECK_INTERVAL: Duration = Duration::from_millis(250);

/// Fully-resolved host-process limits for one session: the manifest's `capabilities.resources`
/// block with every omitted field replaced by its `DEFAULT_*`, except the two CPU fields, which
/// stay `None` when omitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostResourceLimits {
    /// `RLIMIT_NPROC` headroom above the uid's count at each spawn, applied only to a spawn
    /// with no cgroup scope ([`NprocBound::Headroom`]) and counted in the unit that uid's kernel
    /// enforces against ([`uid_task_count`]). A spawn that joins a scope is bounded by
    /// `cgroup_pids_max` instead and ignores this field.
    pub max_processes: u64,
    /// `RLIMIT_NOFILE` hard ceiling.
    pub max_open_files: u64,
    /// `RLIMIT_FSIZE` hard ceiling, in bytes.
    pub max_file_size_bytes: u64,
    /// CPU time, in CPU-seconds, at which `RLIMIT_CPU` sends a spawned subprocess `SIGXCPU`, with
    /// `SIGKILL` one second later ([`cpu_rlimit_slots`]). `None` sets no `RLIMIT_CPU`: a
    /// subprocess keeps the one this process inherited.
    pub cpu_seconds: Option<u64>,
    /// `RLIMIT_AS` on Linux, `RLIMIT_DATA` on macOS (which has no `RLIMIT_AS`), in bytes.
    pub memory_bytes: u64,
    /// cgroup v2 `memory.max`, in bytes. Linux only.
    pub cgroup_memory_bytes: u64,
    /// cgroup v2 `pids.max`. Linux only.
    pub cgroup_pids_max: u64,
    /// cgroup v2 `cpu.max` quota as a percentage of one core. Linux only. `None` writes no
    /// `cpu.max`, so the scope keeps the kernel's `max`.
    pub cgroup_cpu_percent: Option<u32>,
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
            cpu_seconds: None,
            memory_bytes: DEFAULT_MEMORY_BYTES,
            cgroup_memory_bytes: DEFAULT_CGROUP_MEMORY_BYTES,
            cgroup_pids_max: DEFAULT_CGROUP_PIDS_MAX,
            cgroup_cpu_percent: None,
            cgroup_io_bytes_per_sec: DEFAULT_CGROUP_IO_BYTES_PER_SEC,
            workdir_max_bytes: DEFAULT_WORKDIR_MAX_BYTES,
        }
    }
}

impl HostResourceLimits {
    /// Resolve a manifest `capabilities.resources` block, substituting the default for each
    /// field the manifest left out (and for the whole block when it is absent). The CPU fields
    /// have no default and stay `None` when left out.
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
            cpu_seconds: declared.cpu_seconds,
            memory_bytes: declared.memory_bytes.unwrap_or(defaults.memory_bytes),
            cgroup_memory_bytes: declared
                .cgroup_memory_bytes
                .unwrap_or(defaults.cgroup_memory_bytes),
            cgroup_pids_max: declared.cgroup_pids_max.unwrap_or(defaults.cgroup_pids_max),
            cgroup_cpu_percent: declared.cgroup_cpu_percent,
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
/// * `SIGXCPU` — the kernel raises exactly this for an `RLIMIT_CPU` overrun, at the soft limit,
///   which [`apply_hard_rlimits`] places one second below the hard one so that it is raised at
///   all. Unambiguous.
/// * `SIGXFSZ` — likewise for `RLIMIT_FSIZE`. Unambiguous.
///
/// Everything else stays unattributed **on purpose**. `RLIMIT_AS`/`RLIMIT_DATA` overrun surfaces
/// as `ENOMEM` inside the child's own allocator, not as a parent-visible signal with a unique
/// identity, so mapping `SIGSEGV`/`SIGABRT` to `memory_bytes` would be a guess presented as a
/// fact. `RLIMIT_NPROC` and `RLIMIT_NOFILE` do not kill anything at all: they fail a
/// `fork()`/`open()` inside the child with `EAGAIN`/`EMFILE`, visible in that child's own stderr
/// and exit code. A bare `SIGKILL` with no cgroup evidence has too many causes (host OOM killer,
/// an operator, an unrelated crash) to name one. That includes the `SIGKILL` `RLIMIT_CPU` sends
/// one second after `SIGXCPU` to a process that ignored or handled it: it stays unattributed like
/// every other bare `SIGKILL`.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn limit_from_signal(signal: i32) -> Option<&'static str> {
    match signal {
        libc::SIGXCPU => Some("cpu_seconds"),
        libc::SIGXFSZ => Some("max_file_size_bytes"),
        _ => None,
    }
}

/// The line that tells the model which `capabilities.resources` limit stopped a shell command,
/// and what to do about it: `resource_limit: <name> — <what happened>. <what to do>.`
///
/// One line, no trailing newline. `limit` is a name a call was attributed to — one of
/// [`limit_from_signal`]'s or [`crate::cgroup::CgroupEventCounters::attribution_since`]'s. A name
/// with no line of its own gets a generic one that still names its `capabilities.resources` field.
///
/// The `cgroup_*` lines say "the capsule … while this command ran" because the cgroup scope is
/// shared by everything the session runs: a counter delta proves the capsule hit the ceiling
/// during the call, not which process did. The signal lines say "this command" because the kernel
/// delivered the signal to that process.
///
/// Every place the model reads an attributed limit renders it through this function.
pub(crate) fn resource_limit_line(limit: &str) -> String {
    let explanation = match limit {
        "cgroup_pids_max" => "the capsule reached its process limit while this command ran, so \
             starting a new process failed (EAGAIN). Retry with fewer parallel processes"
            .to_string(),
        "cgroup_memory_bytes" => "the capsule reached its memory limit while this command ran, \
             and the kernel killed a process in it to free memory. Retry using less memory at \
             once (smaller inputs, fewer parallel jobs)"
            .to_string(),
        "cpu_seconds" => "this command used up its CPU-time limit and was stopped. Retry with \
             less work per command"
            .to_string(),
        "max_file_size_bytes" => "this command tried to write a file larger than the capsule's \
             file-size limit and was stopped. Retry writing smaller files"
            .to_string(),
        other => format!(
            "this command was stopped by the capsule's capabilities.resources.{other} limit. \
             Retry with less work"
        ),
    };
    format!(
        "resource_limit: {limit} — {explanation}, or ask the operator to raise \
         capabilities.resources.{limit}."
    )
}

/// What one spawn's `RLIMIT_NPROC` is, decided in the parent before its `pre_exec` closure is
/// installed — the closure only receives this `Copy` value.
///
/// `RLIMIT_NPROC` is checked against **everything the uid owns on the host**, not against the
/// tasks in this tree, and on Linux the unit it counts is *threads*: `setrlimit(2)` calls it "the
/// maximum number of processes (or, more precisely on Linux, threads) that can be created for the
/// real user ID". On macOS, whose BSD-derived limit is per-process, it counts processes. Either
/// way an interactive account is already deep into that number before the capsule starts, and the
/// number moves with every thread the desktop, the harness or anything else on the host starts or
/// stops. A limit set from a count taken at launch therefore loses headroom to the rest of the
/// host for the whole session, and a `fork()` in the subprocess fails with `EAGAIN` once enough
/// unrelated threads have started.
///
/// Produced by `sandbox::ShellEnforcement::nproc_bound` once per spawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NprocBound {
    /// Leave `RLIMIT_NPROC` at the value the runtime inherited. The child joins a cgroup scope in
    /// the same `pre_exec` closure, fatally on failure, and the scope's `pids.max`
    /// ([`HostResourceLimits::cgroup_pids_max`]) bounds its tree: it counts only the scope's own
    /// tasks, so nothing else on the host can spend it.
    CgroupPidsMax,
    /// `baseline + max_processes`, where `baseline` is [`uid_task_count`] measured in the parent
    /// for this spawn, so `max_processes` means "how much past the host's usage at this spawn this
    /// tree may add". `0` when the count cannot be taken, which applies the declared value
    /// literally and errs toward the tighter bound.
    Headroom { baseline: u64 },
}

/// Sets every [`HostResourceLimits`] rlimit field as a **hard** limit (`rlim_cur == rlim_max`)
/// except `RLIMIT_CPU`, whose soft slot sits one second below its hard slot
/// ([`cpu_rlimit_slots`]) and which is left as inherited when `cpu_seconds` is `None`, plus
/// `RLIMIT_CORE` at [`CORE_DUMP_LIMIT_BYTES`]. `RLIMIT_NPROC` is set only for
/// [`NprocBound::Headroom`]; for [`NprocBound::CgroupPidsMax`] no `RLIMIT_NPROC` call is made at
/// all.
///
/// Called from inside a forked child's `pre_exec` closure, so everything here is restricted to
/// async-signal-safe operations: two syscalls per limit and no allocation, no locking, and no
/// error formatting (errors are returned as bare `io::Error::from_raw_os_error`).
///
/// `RLIMIT_CORE` is fixed at [`CORE_DUMP_LIMIT_BYTES`] with no manifest surface at all — a core
/// dump of a capsule subprocess holds whatever was in that process's memory, and goes into the
/// workdir or to the host's core-pattern program. It belongs to the same category as the fixed
/// capsule device set in `sandbox.rs`: an invariant, not a setting.
///
/// A requested value above the inherited hard limit is **clamped down to it** rather than
/// treated as an error: an unprivileged process cannot raise `rlim_max`, so the inherited value
/// is the real ceiling either way, and failing the spawn would replace an unwidenable bound with
/// no subprocess at all. For `RLIMIT_CPU` the hard slot clamps to the inherited hard limit and the
/// soft slot stays one second below it.
///
/// ## `RLIMIT_NPROC`: headroom without a scope, untouched with one
///
/// A per-uid count cannot be turned into a bound on one tree, only into headroom above whatever
/// the uid owns. Without a cgroup scope that headroom is the only process bound there is, so it
/// is set from a baseline measured in the unit the kernel checks: a *process* count fed to a
/// Linux kernel counting threads yields a ceiling below the uid's live usage, which is the same
/// `EAGAIN` the headroom exists to prevent.
///
/// With a scope, `pids.max` bounds the tree exactly and headroom adds nothing but a way for an
/// unrelated thread elsewhere on the host to fail a `fork()` here, so the inherited value is left
/// in place.
#[cfg(unix)]
pub(crate) fn apply_hard_rlimits(
    limits: &HostResourceLimits,
    nproc: NprocBound,
) -> std::io::Result<()> {
    apply_nproc_bound(limits, nproc)?;
    apply_per_process_rlimits(limits)
}

/// The rlimits of an unconfined spawn: exactly the fields `declared` names, each as a hard limit
/// on [`apply_hard_rlimits`]'s terms, and nothing else — no `RLIMIT_CORE`, no default for an
/// undeclared field, so an undeclared limit stays the one this process inherited.
/// `RLIMIT_NPROC` is set only when `max_processes` is declared, and then as
/// [`apply_nproc_bound`] sets it. Same `pre_exec` restrictions as [`apply_hard_rlimits`].
#[cfg(unix)]
#[allow(clippy::unnecessary_cast)]
pub(crate) fn apply_declared_rlimits(
    declared: &murmur_artifact::ResourceCapabilities,
    nproc: NprocBound,
) -> std::io::Result<()> {
    if let (Some(max_processes), NprocBound::Headroom { baseline }) =
        (declared.max_processes, nproc)
    {
        set_hard_rlimit(
            libc::RLIMIT_NPROC as u32,
            baseline.saturating_add(max_processes),
        )?;
    }
    if let Some(max_open_files) = declared.max_open_files {
        set_hard_rlimit(libc::RLIMIT_NOFILE as u32, max_open_files)?;
    }
    if let Some(max_file_size_bytes) = declared.max_file_size_bytes {
        set_hard_rlimit(libc::RLIMIT_FSIZE as u32, max_file_size_bytes)?;
    }
    if let Some(cpu_seconds) = declared.cpu_seconds {
        set_cpu_rlimit(cpu_seconds)?;
    }
    if let Some(memory_bytes) = declared.memory_bytes {
        #[cfg(target_os = "linux")]
        set_hard_rlimit(libc::RLIMIT_AS as u32, memory_bytes)?;
        // See `apply_per_process_rlimits` for why macOS tolerates this failing.
        #[cfg(not(target_os = "linux"))]
        let _ = set_hard_rlimit(libc::RLIMIT_DATA as u32, memory_bytes);
    }
    Ok(())
}

/// Whether `declared` names any cgroup field, the condition for an unconfined session to have a
/// cgroup scope at all.
pub(crate) fn declares_cgroup_limit(declared: &murmur_artifact::ResourceCapabilities) -> bool {
    declared.cgroup_memory_bytes.is_some()
        || declared.cgroup_pids_max.is_some()
        || declared.cgroup_cpu_percent.is_some()
        || declared.cgroup_io_bytes_per_sec.is_some()
}

/// The workdir ceiling a session's guard enforces, or `None` when it runs no guard: a contained
/// session always guards at the resolved [`HostResourceLimits::workdir_max_bytes`], an unconfined
/// one only when `capabilities.resources.workdir_max_bytes` is declared.
pub(crate) fn workdir_guard_max_bytes(policy: &crate::types::CapabilityPolicy) -> Option<u64> {
    if policy.containment_floor == murmur_artifact::ContainmentClass::Unconfined {
        policy.declared_resources.workdir_max_bytes
    } else {
        Some(policy.resources.workdir_max_bytes)
    }
}

/// The names of the `capabilities.resources` fields `declared` sets, in declaration order: what
/// an unconfined session's limits are, as its scope report lists them.
pub(crate) fn declared_resource_fields(
    declared: &murmur_artifact::ResourceCapabilities,
) -> Vec<&'static str> {
    [
        ("max_processes", declared.max_processes.is_some()),
        ("max_open_files", declared.max_open_files.is_some()),
        (
            "max_file_size_bytes",
            declared.max_file_size_bytes.is_some(),
        ),
        ("cpu_seconds", declared.cpu_seconds.is_some()),
        ("memory_bytes", declared.memory_bytes.is_some()),
        (
            "cgroup_memory_bytes",
            declared.cgroup_memory_bytes.is_some(),
        ),
        ("cgroup_pids_max", declared.cgroup_pids_max.is_some()),
        ("cgroup_cpu_percent", declared.cgroup_cpu_percent.is_some()),
        (
            "cgroup_io_bytes_per_sec",
            declared.cgroup_io_bytes_per_sec.is_some(),
        ),
        ("workdir_max_bytes", declared.workdir_max_bytes.is_some()),
    ]
    .into_iter()
    .filter_map(|(name, set)| set.then_some(name))
    .collect()
}

/// One `capabilities.resources` field as [`ResourcesReport`] states it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct ResourceValueReport {
    /// The value from [`HostResourceLimits`]: the declared one, or the default when the manifest
    /// leaves the field out. `null` for `cpu_seconds` or `cgroup_cpu_percent` left out, which set
    /// no limit.
    pub value: Option<u64>,
    /// Whether the manifest's `capabilities.resources` block sets the field.
    pub declared: bool,
}

/// The `capabilities.resources` values a session runs under: `ScopeReport::resources`, the
/// `Resources` block of `mur run --explain-scope`. One field per manifest key, named as the key
/// and in [`Self::rows`]' order, each always serialized.
///
/// Read from the policy's resolved [`HostResourceLimits`] and the block as declared, so it states
/// the values the launch applies. An unconfined session applies only the declared ones, which
/// `UnconfinedReport::resource_limits` names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct ResourcesReport {
    /// [`HostResourceLimits::memory_bytes`].
    pub memory_bytes: ResourceValueReport,
    /// [`HostResourceLimits::workdir_max_bytes`].
    pub workdir_max_bytes: ResourceValueReport,
    /// [`HostResourceLimits::max_open_files`].
    pub max_open_files: ResourceValueReport,
    /// [`HostResourceLimits::max_file_size_bytes`].
    pub max_file_size_bytes: ResourceValueReport,
    /// [`HostResourceLimits::max_processes`].
    pub max_processes: ResourceValueReport,
    /// [`HostResourceLimits::cpu_seconds`].
    pub cpu_seconds: ResourceValueReport,
    /// [`HostResourceLimits::cgroup_memory_bytes`].
    pub cgroup_memory_bytes: ResourceValueReport,
    /// [`HostResourceLimits::cgroup_pids_max`].
    pub cgroup_pids_max: ResourceValueReport,
    /// [`HostResourceLimits::cgroup_cpu_percent`].
    pub cgroup_cpu_percent: ResourceValueReport,
    /// [`HostResourceLimits::cgroup_io_bytes_per_sec`].
    pub cgroup_io_bytes_per_sec: ResourceValueReport,
}

impl ResourcesReport {
    /// The report for `limits`, the policy's resolved limits, and `declared`, the manifest's
    /// `capabilities.resources` block as written.
    #[must_use]
    pub(crate) fn new(
        limits: &HostResourceLimits,
        declared: &murmur_artifact::ResourceCapabilities,
    ) -> Self {
        let row = |value: Option<u64>, declared: bool| ResourceValueReport { value, declared };
        Self {
            memory_bytes: row(Some(limits.memory_bytes), declared.memory_bytes.is_some()),
            workdir_max_bytes: row(
                Some(limits.workdir_max_bytes),
                declared.workdir_max_bytes.is_some(),
            ),
            max_open_files: row(
                Some(limits.max_open_files),
                declared.max_open_files.is_some(),
            ),
            max_file_size_bytes: row(
                Some(limits.max_file_size_bytes),
                declared.max_file_size_bytes.is_some(),
            ),
            max_processes: row(Some(limits.max_processes), declared.max_processes.is_some()),
            cpu_seconds: row(limits.cpu_seconds, declared.cpu_seconds.is_some()),
            cgroup_memory_bytes: row(
                Some(limits.cgroup_memory_bytes),
                declared.cgroup_memory_bytes.is_some(),
            ),
            cgroup_pids_max: row(
                Some(limits.cgroup_pids_max),
                declared.cgroup_pids_max.is_some(),
            ),
            cgroup_cpu_percent: row(
                limits.cgroup_cpu_percent.map(u64::from),
                declared.cgroup_cpu_percent.is_some(),
            ),
            cgroup_io_bytes_per_sec: row(
                Some(limits.cgroup_io_bytes_per_sec),
                declared.cgroup_io_bytes_per_sec.is_some(),
            ),
        }
    }

    /// Every field with its manifest key, in serialization order.
    #[must_use]
    pub fn rows(&self) -> [(&'static str, ResourceValueReport); 10] {
        [
            ("memory_bytes", self.memory_bytes),
            ("workdir_max_bytes", self.workdir_max_bytes),
            ("max_open_files", self.max_open_files),
            ("max_file_size_bytes", self.max_file_size_bytes),
            ("max_processes", self.max_processes),
            ("cpu_seconds", self.cpu_seconds),
            ("cgroup_memory_bytes", self.cgroup_memory_bytes),
            ("cgroup_pids_max", self.cgroup_pids_max),
            ("cgroup_cpu_percent", self.cgroup_cpu_percent),
            ("cgroup_io_bytes_per_sec", self.cgroup_io_bytes_per_sec),
        ]
    }

    /// The `Resources` block of `--explain-scope`: one `<key>: <value> (declared|default)` line
    /// per field, `<none>` for a value that sets no limit. Ends with a newline.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::from("Resources\n");
        for (key, row) in self.rows() {
            let value = row
                .value
                .map_or_else(|| "<none>".to_string(), |value| value.to_string());
            let source = if row.declared { "declared" } else { "default" };
            out.push_str(&format!("  {:<25}{value} ({source})\n", format!("{key}:")));
        }
        out
    }
}

/// The `RLIMIT_NPROC` half of [`apply_hard_rlimits`]: set for [`NprocBound::Headroom`], left
/// alone for [`NprocBound::CgroupPidsMax`]. Same `pre_exec` restrictions.
///
/// A kernel-tier spawn must call this before any `unshare(CLONE_NEWUSER)` in its `pre_exec`
/// closure. A new user namespace caps its parent namespace's task count at the creator's
/// `RLIMIT_NPROC` as of the `unshare`, and a later `setrlimit` bounds only the tasks inside the
/// new namespace.
#[cfg(unix)]
// The `as u32` on each `RLIMIT_*` here and in `apply_per_process_rlimits` is redundant on Linux,
// where the constants are already `__rlimit_resource_t` (`c_uint`), and load-bearing on macOS,
// where they are `c_int`. Clippy only ever sees one of the two platforms at a time, so on Linux it
// reads the portability cast as a no-op.
#[allow(clippy::unnecessary_cast)]
pub(crate) fn apply_nproc_bound(
    limits: &HostResourceLimits,
    nproc: NprocBound,
) -> std::io::Result<()> {
    if let NprocBound::Headroom { baseline } = nproc {
        set_hard_rlimit(
            libc::RLIMIT_NPROC as u32,
            baseline.saturating_add(limits.max_processes),
        )?;
    }
    Ok(())
}

/// Every limit of [`apply_hard_rlimits`] except `RLIMIT_NPROC`: the ones that bound this process
/// alone. Same `pre_exec` restrictions.
///
/// A kernel-tier spawn calls this last in its `pre_exec` closure. After `RLIMIT_NOFILE` is set,
/// every descriptor the child opens must be numbered below it, and the child holds every
/// descriptor its parent had open at the fork.
#[cfg(unix)]
#[allow(clippy::unnecessary_cast)]
pub(crate) fn apply_per_process_rlimits(limits: &HostResourceLimits) -> std::io::Result<()> {
    set_hard_rlimit(libc::RLIMIT_NOFILE as u32, limits.max_open_files)?;
    set_hard_rlimit(libc::RLIMIT_FSIZE as u32, limits.max_file_size_bytes)?;
    if let Some(cpu_seconds) = limits.cpu_seconds {
        set_cpu_rlimit(cpu_seconds)?;
    }
    set_hard_rlimit(libc::RLIMIT_CORE as u32, CORE_DUMP_LIMIT_BYTES)?;

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
fn set_hard_rlimit(resource: u32, value: u64) -> std::io::Result<()> {
    let current = read_rlimit(resource)?;

    // `rlim_t` is 64-bit on every platform this runtime targets, but saturate rather than cast
    // so a 32-bit `rlim_t` would clamp to its own maximum instead of silently wrapping.
    let requested = libc::rlim_t::try_from(value).unwrap_or(libc::rlim_t::MAX);
    let target = if current.rlim_max == libc::RLIM_INFINITY {
        requested
    } else {
        requested.min(current.rlim_max)
    };

    write_rlimit(
        resource,
        &libc::rlimit {
            rlim_cur: target,
            rlim_max: target,
        },
    )
}

/// One `getrlimit`/`setrlimit` pair for `RLIMIT_CPU`: read the inherited hard limit, then write
/// the two slots [`cpu_rlimit_slots`] derives from it.
#[cfg(unix)]
// See `apply_nproc_bound` for why the `as u32` is kept.
#[allow(clippy::unnecessary_cast)]
fn set_cpu_rlimit(cpu_seconds: u64) -> std::io::Result<()> {
    let resource = libc::RLIMIT_CPU as u32;
    let current = read_rlimit(resource)?;
    write_rlimit(resource, &cpu_rlimit_slots(cpu_seconds, current.rlim_max))
}

/// The `RLIMIT_CPU` slots for a declared `cpu_seconds`, given the hard limit this process
/// inherited: the hard slot one second above `cpu_seconds` and the soft slot one second below
/// the hard one.
///
/// The kernel sends `SIGXCPU` when a process reaches its soft limit and `SIGKILL` when it reaches
/// its hard limit, and with the two equal the hard check fires first, so only `SIGKILL` is ever
/// sent. `SIGXCPU` is the signal [`limit_from_signal`] can attribute; the gap of one second is
/// what makes the kernel send it at `cpu_seconds`.
///
/// * The hard slot is `cpu_seconds + 1`, clamped down to a finite `inherited_hard`.
/// * The soft slot is the hard slot minus one, except that it equals the hard slot when that is
///   `RLIM_INFINITY` (a `cpu_seconds` that saturates) or `≤ 1` (an inherited hard limit of 1).
///
/// Pure arithmetic with no allocation, because it runs inside `pre_exec`.
#[cfg(unix)]
// `RLIM_INFINITY` is `u64::MAX` on Linux but `i64::MAX` on macOS, so every value is capped at it
// rather than at `rlim_t::MAX`. On Linux the cap equals the type's maximum, which clippy reads as
// a no-op `min`.
#[allow(clippy::unnecessary_min_or_max)]
fn cpu_rlimit_slots(cpu_seconds: u64, inherited_hard: libc::rlim_t) -> libc::rlimit {
    let requested = libc::rlim_t::try_from(cpu_seconds)
        .unwrap_or(libc::RLIM_INFINITY)
        .min(libc::RLIM_INFINITY);
    let mut hard = requested.saturating_add(1).min(libc::RLIM_INFINITY);
    if inherited_hard != libc::RLIM_INFINITY {
        hard = hard.min(inherited_hard);
    }
    let soft = if hard == libc::RLIM_INFINITY || hard <= 1 {
        hard
    } else {
        hard - 1
    };
    libc::rlimit {
        rlim_cur: soft,
        rlim_max: hard,
    }
}

/// `getrlimit(resource)`, with a failure returned as the bare OS error.
#[cfg(unix)]
#[allow(unsafe_code)]
pub(crate) fn read_rlimit(resource: u32) -> std::io::Result<libc::rlimit> {
    let mut current = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `resource` is one of the `libc::RLIMIT_*` constants (cast to the width this
    // platform's `getrlimit` takes) and `current` is a live, correctly-sized stack `rlimit`.
    if unsafe { libc::getrlimit(resource as _, &mut current) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(current)
}

/// `setrlimit(resource, limit)`, with a failure returned as the bare OS error.
#[cfg(unix)]
#[allow(unsafe_code)]
fn write_rlimit(resource: u32, limit: &libc::rlimit) -> std::io::Result<()> {
    // SAFETY: `resource` is one of the `libc::RLIMIT_*` constants (cast to the width this
    // platform's `setrlimit` takes) and `limit` is a live `rlimit` that the call only reads.
    if unsafe { libc::setrlimit(resource as _, limit) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// How many **tasks** — in the unit this platform's `RLIMIT_NPROC` is actually checked against —
/// the runtime's own uid currently owns, or `None` if the host cannot be asked.
///
/// The unit differs per platform, and matching it is the entire point of this function:
///
///   - **Linux**: threads. `setrlimit(2)` states the limit is "the maximum number of processes
///     (or, more precisely on Linux, threads) that can be created for the real user ID", and the
///     kernel's `fork()` path compares against the uid's live task count. So this sums each owned
///     process's `/proc/<pid>/task/` entries (one per thread, the leader included) rather than
///     counting `/proc/<pid>` directories, which would count one per *process* and land an order
///     of magnitude low on any account running threaded software.
///   - **macOS**: processes. The BSD-derived `RLIMIT_NPROC` there genuinely counts one entry per
///     process, so `proc_listpids` is already the matching unit.
///
/// Measured **in the parent** once per spawn that has no cgroup scope — it walks `/proc` (Linux)
/// or calls into libproc (macOS), neither of which is async-signal-safe, so it must never be
/// called from `pre_exec`. Its one consumer is [`NprocBound::Headroom`]; see there for why a
/// per-uid limit is meaningless without it.
///
/// The Linux walk is inherently racy — a process can exit between the `read_dir("/proc")` and the
/// read of its `task/` directory — and every such race is absorbed by skipping that process. A
/// baseline slightly stale in either direction is harmless: it is added to `max_processes`
/// headroom, not compared for equality against anything.
#[cfg_attr(not(unix), allow(dead_code))]
// Each `cfg` arm returns explicitly so that exactly one of them is live per target without the
// arms having to be arranged so the live one happens to be last — on the target where an arm is
// the final live statement, clippy sees its `return` as redundant, but removing it breaks every
// other target.
#[allow(clippy::needless_return)]
pub(crate) fn uid_task_count() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: `geteuid` takes no arguments and cannot fail.
        let uid = {
            #[allow(unsafe_code)]
            unsafe {
                libc::geteuid()
            }
        };
        return uid_task_count_in(Path::new("/proc"), uid);
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

/// The Linux walk behind [`uid_task_count`], over a `/proc`-shaped tree at `proc_root`: the
/// `task/` entries of every all-digit directory owned by `uid`, summed.
///
/// `None` only when `proc_root` itself cannot be read. A process directory whose `task/` cannot be
/// read — one that exited mid-walk — contributes 0.
#[cfg(target_os = "linux")]
pub(crate) fn uid_task_count_in(proc_root: &Path, uid: u32) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;

    // A `/proc/<pid>` directory is owned by the uid the process runs as, so a `stat` per entry
    // finds this uid's processes without opening or parsing a single `status` file.
    let entries = std::fs::read_dir(proc_root).ok()?;
    let count = entries
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.bytes().all(|b| b.is_ascii_digit()))
                && entry.metadata().is_ok_and(|metadata| metadata.uid() == uid)
        })
        .map(|entry| {
            // One `task/` entry per thread of that process, the group leader included.
            std::fs::read_dir(entry.path().join("task"))
                .map(|threads| threads.flatten().count() as u64)
                .unwrap_or(0)
        })
        .sum();
    Some(count)
}

/// A workdir-size ceiling that was crossed: what was allowed, and what was actually observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WorkdirBreach {
    pub(crate) max_bytes: u64,
    pub(crate) observed_bytes: u64,
}

/// Workdir-size guard for one session: a periodic check that stops a writer, and a final
/// measurement that decides the session's status.
///
/// **Two jobs, and only the second one is a promise.**
///
///   * The polling thread walks the work folder, `$HOME` and `$TMPDIR` every
///     [`WORKDIR_CHECK_INTERVAL`] (a plain OS thread, not a tokio task: the session runs guests on
///     several runtimes and the guard must outlive all of them), latches the first breach it sees
///     and, when the session has a cgroup scope, kills everything in the scope at once. That keeps
///     a disk filler from writing the whole disk while it is noticed. It is a bound on *how far
///     past the ceiling* a writer gets (about one poll's worth of its write rate), not on whether
///     the breach is reported.
///   * [`Self::settle`] measures once, synchronously, on the caller's thread, and latches. The
///     runtime calls it at the one place a session's final status is decided, after the attempt
///     has returned and before anything reports it. So a breach can never be missed because the
///     poll had not come round yet: the status is decided by the measurement, and the poll only
///     decides how much was written first. The breach this reports is E-RUN-013.
///
/// The latch is read by `shell::spawn_contained` and `runtime::dispatch_native_tool`, which refuse
/// to spawn once it is set, and by the agent turn loop, which turns it into
/// [`crate::errors::RuntimeError::WorkdirSizeExceeded`].
///
/// **Why not enforce at write time.** The kernel offers two write-time bounds: a size-limited
/// mount (tmpfs `size=`, a project quota) and `RLIMIT_FSIZE`. The first needs a mount namespace
/// and a work folder that is not a real host directory; the runtime's work folder is one the host
/// and its records read directly, and nothing in the unsealed tiers ever runs as root, so the
/// mount is not available to every session the limit is promised to. `RLIMIT_FSIZE` bounds one
/// file, not the sum, and is already `max_file_size_bytes`. A guard that measures at the decision
/// point is the one design that holds on every tier; a mount-backed work folder for the sealed
/// tier remains a possible later tightening of the first job, not a change to the second.
#[derive(Debug)]
pub(crate) struct WorkdirGuard {
    max_bytes: u64,
    measured: Measured,
    breach: Mutex<Option<WorkdirBreach>>,
    stop: AtomicBool,
    /// The session's cgroup scope, once [`Self::attach_scope`] gave it one.
    scope: Mutex<Option<Arc<crate::cgroup::CgroupScope>>>,
}

/// The directories a guard measures, against one ceiling for their sum.
#[derive(Debug, Clone)]
struct Measured {
    workdir: crate::workdir_fs::WorkdirRoot,
    temp_dir: Option<crate::workdir_fs::WorkdirRoot>,
    home_dir: Option<crate::workdir_fs::WorkdirRoot>,
    /// Where a breach is logged, or `None` outside a session, where it is reported on stderr alone.
    records_root: Option<crate::workdir_fs::WorkdirRoot>,
}

impl WorkdirGuard {
    /// Spawn the checker thread for `workdir`. The returned handle owns the latch; dropping the
    /// last clone stops the thread within one interval.
    ///
    /// `workdir`, the work folder, `temp_dir`, the session's temporary folder, and `home_dir`,
    /// its home folder, are the directories measured, against one ceiling for their sum;
    /// `records_root` is the session's records directory, where a breach is logged, or `None`
    /// outside a session, where it is reported on stderr alone.
    pub(crate) fn spawn(
        workdir: &crate::workdir_fs::WorkdirRoot,
        temp_dir: Option<&crate::workdir_fs::WorkdirRoot>,
        home_dir: Option<&crate::workdir_fs::WorkdirRoot>,
        records_root: Option<&crate::workdir_fs::WorkdirRoot>,
        max_bytes: u64,
    ) -> Arc<Self> {
        let guard = Self::unpolled(workdir, temp_dir, home_dir, records_root, max_bytes);

        // The thread holds a Weak, so it can never keep the session's guard alive; the stop flag
        // covers the reverse (guard dropped while the thread is mid-sleep). Same both-ends shape
        // as `limits::EpochTicker`.
        let weak = Arc::downgrade(&guard);
        thread::spawn(move || loop {
            thread::sleep(WORKDIR_CHECK_INTERVAL);
            // The gap a test holds open: the poll has not come round yet. Before the upgrade, so a
            // held poll keeps nothing of the session alive.
            crate::race::window_blocking(crate::race::point::WORKDIR_POLL);
            let Some(guard) = weak.upgrade() else {
                break;
            };
            if guard.stop.load(Ordering::Relaxed) || guard.breach().is_some() {
                break;
            }
            let walk_started = std::time::Instant::now();
            let observed = guard.measure();
            if guard.latch(observed) {
                guard.stop_writers();
                break;
            }
            // A walk that was slow is followed by a pause in proportion, to cap the CPU it costs.
            let extra = walk_started
                .elapsed()
                .saturating_mul(2)
                .saturating_sub(WORKDIR_CHECK_INTERVAL);
            drop(guard);
            thread::sleep(extra);
        });

        guard
    }

    /// A guard with no polling thread: only [`Self::settle`] measures. What [`Self::spawn`] builds
    /// before it starts the thread, and what a test uses to measure exactly when it chooses.
    pub(crate) fn unpolled(
        workdir: &crate::workdir_fs::WorkdirRoot,
        temp_dir: Option<&crate::workdir_fs::WorkdirRoot>,
        home_dir: Option<&crate::workdir_fs::WorkdirRoot>,
        records_root: Option<&crate::workdir_fs::WorkdirRoot>,
        max_bytes: u64,
    ) -> Arc<Self> {
        Arc::new(Self {
            max_bytes,
            measured: Measured {
                workdir: workdir.clone(),
                temp_dir: temp_dir.cloned(),
                home_dir: home_dir.cloned(),
                records_root: records_root.cloned(),
            },
            breach: Mutex::new(None),
            stop: AtomicBool::new(false),
            scope: Mutex::new(None),
        })
    }

    /// Gives the guard the session's cgroup scope, which it kills when a breach is latched.
    pub(crate) fn attach_scope(&self, scope: Option<Arc<crate::cgroup::CgroupScope>>) {
        if let Ok(mut slot) = self.scope.lock() {
            *slot = scope;
        }
    }

    /// Measures the directories now and latches a breach when their sum is past the ceiling.
    /// `true` when the session has breached, whether this call latched it or an earlier one did.
    ///
    /// This is the measurement that decides the session's status: it runs on the caller's thread
    /// and returns only once it has the answer, so it does not depend on the polling thread having
    /// run since the writer last wrote.
    pub(crate) fn settle(&self) -> bool {
        if self.breach().is_some() {
            return true;
        }
        let observed = self.measure();
        let breached = self.latch(observed);
        if breached {
            self.stop_writers();
        }
        breached
    }

    /// The size of the measured directories, summed.
    fn measure(&self) -> u64 {
        self.paths().iter().fold(0u64, |sum, dir| {
            sum.saturating_add(directory_size_bytes(dir))
        })
    }

    fn paths(&self) -> Vec<&Path> {
        std::iter::once(&self.measured.workdir)
            .chain(self.measured.temp_dir.as_ref())
            .chain(self.measured.home_dir.as_ref())
            .map(crate::workdir_fs::WorkdirRoot::path)
            .collect()
    }

    /// Latches a breach when `observed` is past the ceiling. `true` when it is.
    fn latch(&self, observed: u64) -> bool {
        if observed > self.max_bytes {
            self.record_breach(observed);
            return true;
        }
        false
    }

    /// Kills every process in the session's scope, so a writer stops where it is. A session with
    /// no scope has nothing to kill here: its writers stop at the next spawn, and the session
    /// ends at the next decision point.
    fn stop_writers(&self) {
        let scope = self.scope.lock().ok().and_then(|slot| slot.clone());
        if let Some(scope) = scope {
            let _ = scope.kill_all();
        }
    }

    /// The latched breach, if the workdir has crossed its ceiling.
    pub(crate) fn breach(&self) -> Option<WorkdirBreach> {
        self.breach.lock().ok().and_then(|breach| *breach)
    }

    fn record_breach(&self, observed_bytes: u64) {
        let breach = WorkdirBreach {
            max_bytes: self.max_bytes,
            observed_bytes,
        };
        // Only the first latch reports: the poll and a settle can both see the same breach.
        let first = match self.breach.lock() {
            Ok(mut slot) if slot.is_none() => {
                *slot = Some(breach);
                true
            }
            _ => false,
        };
        if !first {
            return;
        }
        let measured = self
            .paths()
            .iter()
            .map(|dir| dir.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let message = format!("{}; measured: {measured}", workdir_breach_message(breach));
        crate::runtime_err!("[capsule-runtime] {message}");
        if let Some(records_root) = self.measured.records_root.as_ref() {
            crate::agent::append_bootstrap_log(records_root, &format!("[resources] {message}"));
        }
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
        "work folder, $HOME and $TMPDIR together grew to {} bytes, past the {} byte ceiling \
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

#[cfg(all(test, target_os = "linux"))]
pub(crate) mod test_support {
    use std::fs::{File, OpenOptions};
    use std::sync::{Arc, Barrier};

    /// Serializes the tests that start [`HeldThreads`] or assert on how the uid's task count
    /// moves, so one test's holders never land inside another's before/after window. Held until
    /// the returned file is dropped. Other processes the uid owns still move the count; the
    /// margins in those tests absorb that.
    ///
    /// An exclusive lock on a file in the temp directory, not a static mutex: cargo-nextest runs
    /// every test in its own process, where a static serializes nothing. Each call opens its own
    /// file description, so the lock also excludes other threads of the same `cargo test` binary.
    pub(crate) fn host_threads_lock() -> File {
        let path = std::env::temp_dir().join("murmur-capsule-runtime-host-threads.lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .unwrap_or_else(|error| panic!("opening {}: {error}", path.display()));
        file.lock()
            .unwrap_or_else(|error| panic!("locking {}: {error}", path.display()));
        file
    }

    /// `count` threads in the test process, each parked on a barrier, standing in for threads the
    /// rest of the host starts while a session runs. [`Self::start`] returns only once every
    /// one of them is running, so the uid's task count already includes them; dropping the value
    /// releases and joins them.
    pub(crate) struct HeldThreads {
        release: Arc<Barrier>,
        handles: Vec<std::thread::JoinHandle<()>>,
    }

    impl HeldThreads {
        pub(crate) fn start(count: usize) -> Self {
            let started = Arc::new(Barrier::new(count + 1));
            let release = Arc::new(Barrier::new(count + 1));
            let handles = (0..count)
                .map(|_| {
                    let started = Arc::clone(&started);
                    let release = Arc::clone(&release);
                    std::thread::Builder::new()
                        .stack_size(64 * 1024)
                        .spawn(move || {
                            started.wait();
                            release.wait();
                        })
                        .expect("a holder thread must start")
                })
                .collect();
            started.wait();
            Self { release, handles }
        }
    }

    impl Drop for HeldThreads {
        fn drop(&mut self) {
            self.release.wait();
            for handle in self.handles.drain(..) {
                let _ = handle.join();
            }
        }
    }

    /// This process's own `(soft, hard)` `RLIMIT_NPROC`, the values a child inherits when
    /// `apply_hard_rlimits` leaves the limit alone.
    #[allow(unsafe_code)]
    pub(crate) fn own_rlimit_nproc() -> (libc::rlim_t, libc::rlim_t) {
        let mut current = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: a plain `getrlimit` read into a live stack `rlimit`.
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NPROC, &mut current) },
            0
        );
        (current.rlim_cur, current.rlim_max)
    }

    /// An rlimit as `ulimit` prints it: `unlimited` for `RLIM_INFINITY`, the number otherwise.
    pub(crate) fn rendered_rlimit(value: libc::rlim_t) -> String {
        if value == libc::RLIM_INFINITY {
            "unlimited".to_string()
        } else {
            value.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `W-RUN-010` keys on the effective address-space limit: the 2 GiB default and a declared
    /// 16 GiB warn for `node` and `nodejs`, 64 GiB does not, a binary that is not V8 never does,
    /// and an unconfined session with no declared limit has none to warn about.
    #[test]
    fn w_run_010_fires_below_the_floor_for_a_v8_binary() {
        const GIB: u64 = 1024 * 1024 * 1024;
        let allow = |names: &[&str]| names.iter().map(|n| (*n).to_string()).collect::<Vec<_>>();
        let undeclared = v8_address_space_warning(&allow(&["bash", "node"]), None, false).unwrap();
        assert!(
            undeclared.starts_with("warning[W-RUN-010]: "),
            "{undeclared}"
        );
        assert!(undeclared.contains("2147483648 bytes"), "{undeclared}");
        assert!(undeclared.contains("RLIMIT_AS"), "{undeclared}");
        assert!(
            undeclared.contains("Cannot allocate Wasm memory"),
            "{undeclared}"
        );
        assert!(
            undeclared.contains("68719476736 (64 GiB) works"),
            "{undeclared}"
        );
        assert!(undeclared.contains("cgroup_memory_bytes"), "{undeclared}");
        assert!(
            undeclared.ends_with("diagnostics/#w-run-010)"),
            "{undeclared}"
        );
        let declared = v8_address_space_warning(&allow(&["node"]), Some(16 * GIB), false).unwrap();
        assert!(declared.contains("17179869184 bytes"), "{declared}");
        assert!(
            v8_address_space_warning(&allow(&["/usr/bin/nodejs"]), Some(16 * GIB), false).is_some()
        );
        assert!(v8_address_space_warning(&allow(&["node"]), Some(16 * GIB), true).is_some());
        assert_eq!(
            v8_address_space_warning(&allow(&["node"]), Some(64 * GIB), false),
            None
        );
        assert_eq!(
            v8_address_space_warning(&allow(&["node"]), Some(32 * GIB), false),
            None
        );
        assert_eq!(
            v8_address_space_warning(&allow(&["cat"]), Some(GIB), false),
            None
        );
        assert_eq!(
            v8_address_space_warning(&allow(&["node"]), None, true),
            None
        );
        assert_eq!(
            v8_address_space_warning(&allow(&["nodemon"]), None, false),
            None
        );
    }

    /// What an unconfined session applies is what the manifest declares, field by field: the
    /// workdir guard only for a declared `workdir_max_bytes`, a scope only for a `cgroup_*` field,
    /// and the report lists exactly the declared names. A contained session keeps the resolved
    /// limits.
    #[test]
    fn an_unconfined_session_applies_only_the_declared_limits() {
        use murmur_artifact::{ContainmentClass, ResourceCapabilities};
        let empty = ResourceCapabilities::default();
        assert!(declared_resource_fields(&empty).is_empty());
        assert!(!declares_cgroup_limit(&empty));
        let declared = ResourceCapabilities {
            max_open_files: Some(256),
            cgroup_pids_max: Some(64),
            workdir_max_bytes: Some(1 << 20),
            ..Default::default()
        };
        assert_eq!(
            declared_resource_fields(&declared),
            vec!["max_open_files", "cgroup_pids_max", "workdir_max_bytes"]
        );
        assert!(declares_cgroup_limit(&declared));

        let policy = |containment_floor, declared_resources| crate::types::CapabilityPolicy {
            containment_floor,
            declared_resources,
            ..Default::default()
        };
        assert_eq!(
            workdir_guard_max_bytes(&policy(ContainmentClass::Unconfined, empty)),
            None
        );
        assert_eq!(
            workdir_guard_max_bytes(&policy(ContainmentClass::Unconfined, declared)),
            Some(1 << 20)
        );
        assert_eq!(
            workdir_guard_max_bytes(&policy(ContainmentClass::Advisory, empty)),
            Some(DEFAULT_WORKDIR_MAX_BYTES)
        );
    }

    #[test]
    fn resolve_uses_defaults_when_manifest_is_silent() {
        assert_eq!(
            HostResourceLimits::resolve(None),
            HostResourceLimits::default()
        );
    }

    /// A silent manifest sets no CPU-time limit and no CPU cap; a declared one passes through.
    #[test]
    fn the_cpu_fields_are_unset_unless_declared() {
        let silent = HostResourceLimits::resolve(None);
        assert_eq!(silent.cpu_seconds, None);
        assert_eq!(silent.cgroup_cpu_percent, None);

        let declared = murmur_artifact::ResourceCapabilities {
            cpu_seconds: Some(7),
            cgroup_cpu_percent: Some(50),
            ..murmur_artifact::ResourceCapabilities::default()
        };
        let limits = HostResourceLimits::resolve(Some(&declared));
        assert_eq!(limits.cpu_seconds, Some(7));
        assert_eq!(limits.cgroup_cpu_percent, Some(50));
        assert_eq!(limits.memory_bytes, DEFAULT_MEMORY_BYTES);
    }

    /// The rule inherited from `capabilities.limits`: a manifest that declares the section but
    /// omits a field gets that field's default, except the CPU fields, which stay unset.
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
        assert_eq!(limits.cpu_seconds, None);
        assert_eq!(limits.memory_bytes, DEFAULT_MEMORY_BYTES);
        assert_eq!(limits.cgroup_memory_bytes, DEFAULT_CGROUP_MEMORY_BYTES);
        assert_eq!(limits.cgroup_cpu_percent, None);
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
    /// The negative half matters too: over-attributing a `SIGKILL` to `memory_bytes` would put a
    /// guess into the trace as if it were evidence.
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

    /// Each name is taken from the producer that attributes it, so renaming an attribution
    /// without giving the new name its line fails here rather than falling through to the generic
    /// wording.
    #[test]
    fn every_attributable_limit_has_its_own_line() {
        use crate::cgroup::CgroupEventCounters;

        let cpu = limit_from_signal(libc::SIGXCPU).unwrap();
        let fsize = limit_from_signal(libc::SIGXFSZ).unwrap();
        let memory = CgroupEventCounters {
            oom_kill: 1,
            pids_max: 0,
        }
        .attribution_since(CgroupEventCounters::default())
        .unwrap();
        let pids = CgroupEventCounters {
            oom_kill: 0,
            pids_max: 1,
        }
        .attribution_since(CgroupEventCounters::default())
        .unwrap();

        let expected = [
            (
                pids,
                "resource_limit: cgroup_pids_max — the capsule reached its process limit while \
                 this command ran, so starting a new process failed (EAGAIN). Retry with fewer \
                 parallel processes, or ask the operator to raise \
                 capabilities.resources.cgroup_pids_max.",
            ),
            (
                memory,
                "resource_limit: cgroup_memory_bytes — the capsule reached its memory limit while \
                 this command ran, and the kernel killed a process in it to free memory. Retry \
                 using less memory at once (smaller inputs, fewer parallel jobs), or ask the \
                 operator to raise capabilities.resources.cgroup_memory_bytes.",
            ),
            (
                cpu,
                "resource_limit: cpu_seconds — this command used up its CPU-time limit and was \
                 stopped. Retry with less work per command, or ask the operator to raise \
                 capabilities.resources.cpu_seconds.",
            ),
            (
                fsize,
                "resource_limit: max_file_size_bytes — this command tried to write a file larger \
                 than the capsule's file-size limit and was stopped. Retry writing smaller files, \
                 or ask the operator to raise capabilities.resources.max_file_size_bytes.",
            ),
        ];
        for (name, text) in expected {
            let line = resource_limit_line(name);
            assert_eq!(line, text);
            assert!(line.starts_with(&format!("resource_limit: {name} — ")));
            assert!(line.contains(&format!("capabilities.resources.{name}")));
            assert!(line.contains("ask the operator to raise"));
            assert!(!line.contains('\n'), "{line:?} must be a single line");
        }

        for cgroup in [pids, memory] {
            let line = resource_limit_line(cgroup);
            assert!(line.contains("the capsule"), "{line}");
            assert!(line.contains("while this command ran"), "{line}");
        }
        for signal in [cpu, fsize] {
            assert!(resource_limit_line(signal).contains("this command"));
        }

        assert_eq!(
            resource_limit_line("some_future_limit"),
            "resource_limit: some_future_limit — this command was stopped by the capsule's \
             capabilities.resources.some_future_limit limit. Retry with less work, or ask the \
             operator to raise capabilities.resources.some_future_limit."
        );
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

        let guard = WorkdirGuard::spawn(
            &crate::workdir_fs::WorkdirRoot::open(temp.path()).unwrap(),
            None,
            None,
            None,
            DEFAULT_WORKDIR_MAX_BYTES,
        );
        assert_eq!(guard.breach(), None);
    }

    /// 2 MiB in `big`, written to one of the three measured directories, with 100 bytes in the
    /// work folder: the guard spawned on all three breaches 1 MiB and observes the sum, and the
    /// work folder alone does not.
    fn workdir_guard_breaches_on_bytes_in(dir: usize) {
        const MIB: u64 = 1024 * 1024;
        let dirs = [(); 3].map(|()| tempfile::tempdir().unwrap());
        std::fs::write(dirs[0].path().join("small"), vec![0u8; 100]).unwrap();
        std::fs::write(dirs[dir].path().join("big"), vec![0u8; 2 * MIB as usize]).unwrap();
        let [workdir, temp, home] =
            [0, 1, 2].map(|i| crate::workdir_fs::WorkdirRoot::open(dirs[i].path()).unwrap());

        if dir != 0 {
            let alone = WorkdirGuard::unpolled(&workdir, None, None, None, MIB);
            assert!(!alone.settle(), "the work folder alone is under 1 MiB");
            assert_eq!(alone.breach(), None);
        }
        let guard = WorkdirGuard::unpolled(&workdir, Some(&temp), Some(&home), None, MIB);
        assert!(guard.settle());
        let breach = guard.breach().expect("the bytes breach the ceiling");
        assert_eq!(breach.max_bytes, MIB);
        assert_eq!(breach.observed_bytes, 2 * MIB + 100);

        let message = workdir_breach_message(breach);
        assert!(
            message.contains("work folder, $HOME and $TMPDIR"),
            "{message}"
        );
        let error = crate::errors::RuntimeError::WorkdirSizeExceeded {
            max_bytes: breach.max_bytes,
            observed_bytes: breach.observed_bytes,
        }
        .to_string();
        assert!(error.contains("work folder, $HOME and $TMPDIR"), "{error}");
    }

    #[test]
    fn workdir_guard_counts_the_work_folder() {
        workdir_guard_breaches_on_bytes_in(0);
    }

    #[test]
    fn workdir_guard_counts_the_temp_folder() {
        workdir_guard_breaches_on_bytes_in(1);
    }

    #[test]
    fn workdir_guard_counts_the_home_folder() {
        workdir_guard_breaches_on_bytes_in(2);
    }

    /// A breach's bootstrap log line names every directory measured.
    #[test]
    fn workdir_guard_logs_what_it_measured() {
        let dirs = [(); 4].map(|()| tempfile::tempdir().unwrap());
        std::fs::write(dirs[0].path().join("big"), vec![0u8; 4096]).unwrap();
        let [workdir, temp, home, records] =
            [0, 1, 2, 3].map(|i| crate::workdir_fs::WorkdirRoot::open(dirs[i].path()).unwrap());
        let guard =
            WorkdirGuard::unpolled(&workdir, Some(&temp), Some(&home), Some(&records), 1024);
        assert!(guard.settle());
        let log = std::fs::read_to_string(dirs[3].path().join("logs/bootstrap.log")).unwrap();
        for dir in &dirs[..3] {
            assert!(log.contains(&dir.path().display().to_string()), "{log}");
        }
        assert!(log.contains("measured: "), "{log}");
    }

    /// The baseline must be a real, plausible count — a `0` here would silently turn
    /// `max_processes` back into the absolute per-uid ceiling that makes every `fork()` in a
    /// subprocess fail on any account already past the limit.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn uid_task_count_reports_a_plausible_baseline() {
        let count = uid_task_count().expect("this platform can be asked for its task count");
        assert!(
            count > 0,
            "the test process itself is owned by this uid, so the count cannot be zero"
        );
    }

    /// On Linux `RLIMIT_NPROC` is enforced against threads, so the baseline sums `task/` entries:
    /// the three owned processes below hold 7 threads between them, where a count of processes
    /// would come out as 3. Directories that are not pids (`self`), a process with no readable
    /// `task/` (`400`), and processes owned by another uid add nothing.
    ///
    /// Run against a synthetic tree because the live count covers the whole uid and moves with
    /// every other thread on the machine, so no assertion that it rises can be deterministic.
    #[cfg(target_os = "linux")]
    #[test]
    fn uid_task_count_sums_threads_not_processes() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        for (pid, threads) in [("100", 1), ("200", 4), ("300", 2), ("self", 5)] {
            let task = root.join(pid).join("task");
            std::fs::create_dir_all(&task).unwrap();
            for thread in 0..threads {
                std::fs::write(task.join(thread.to_string()), b"").unwrap();
            }
        }
        std::fs::create_dir(root.join("400")).unwrap();

        // The owner the walk reads off each pid directory, taken from one of them.
        let own_uid = {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(root.join("100")).unwrap().uid()
        };

        assert_eq!(uid_task_count_in(root, own_uid), Some(7));
        assert_eq!(uid_task_count_in(root, own_uid.wrapping_add(1)), Some(0));
        assert_eq!(uid_task_count_in(&root.join("missing"), own_uid), None);
    }

    /// `rlim_cur == rlim_max` is the whole point of this module's rlimit path: a soft-only cap
    /// can be raised back by the capsule itself. Asserted against a live process by lowering a
    /// safely lowerable limit (`RLIMIT_CORE`, which a spawned process gets at one byte anyway) and
    /// reading it back.
    ///
    /// The lowering happens in a forked child, never in the test process. A hard limit cannot be
    /// raised again, and the test binary is one process shared by every test in the crate: this
    /// test used to set the hard `RLIMIT_CORE` to 0 for all of them, which made
    /// `shell::tests::a_kernel_tier_command_gets_the_one_byte_core_limit` find a hard limit of 0
    /// and skip itself, whenever it ran after this one. The child does nothing but two
    /// async-signal-safe syscalls and `_exit`, which is all a fork of a threaded process may do.
    #[cfg(unix)]
    #[test]
    #[allow(unsafe_code)]
    // Same portability cast as `apply_per_process_rlimits`: `RLIMIT_CORE` is `c_uint` on Linux and
    // `c_int` on macOS, so the cast only looks redundant from whichever platform clippy runs on.
    #[allow(clippy::unnecessary_cast)]
    fn set_hard_rlimit_writes_both_the_soft_and_hard_slot() {
        let read_core = || {
            let mut current = libc::rlimit {
                rlim_cur: 1,
                rlim_max: 1,
            };
            // SAFETY: a plain `getrlimit` read into a live stack `rlimit`.
            let rc = unsafe { libc::getrlimit(libc::RLIMIT_CORE as _, &mut current) };
            assert_eq!(rc, 0);
            current
        };
        let before = read_core();

        // SAFETY: the child calls only `setrlimit`, `getrlimit` and `_exit`, none of which
        // allocates or takes a lock, so it is safe in a process forked from a threaded one.
        let child = unsafe { libc::fork() };
        assert!(child >= 0, "fork failed");
        if child == 0 {
            let code = (|| {
                // The function under test. Its success path allocates nothing.
                if set_hard_rlimit(libc::RLIMIT_CORE as u32, 0).is_err() {
                    return 11;
                }
                let mut current = libc::rlimit {
                    rlim_cur: 1,
                    rlim_max: 1,
                };
                // SAFETY: as above.
                if unsafe { libc::getrlimit(libc::RLIMIT_CORE as _, &mut current) } != 0 {
                    return 12;
                }
                if current.rlim_cur != 0 {
                    return 13;
                }
                if current.rlim_max != 0 {
                    return 14;
                }
                0
            })();
            // SAFETY: `_exit` ends the child without running the parent's destructors or the test
            // harness's exit path.
            unsafe { libc::_exit(code) };
        }
        let mut status = 0;
        // SAFETY: `child` is this process's own child, and `status` is a live stack integer.
        let waited = unsafe { libc::waitpid(child, &mut status, 0) };
        assert_eq!(waited, child);
        assert!(libc::WIFEXITED(status), "the child did not exit: {status}");
        assert_eq!(
            libc::WEXITSTATUS(status),
            0,
            "child exit: 11 = write failed, 12 = read failed, \
             13 = soft slot not set, 14 = the hard slot must be set, not only the soft one"
        );

        // The test process itself is untouched, which is what the other tests rely on.
        let after = read_core();
        assert_eq!(
            (after.rlim_cur, after.rlim_max),
            (before.rlim_cur, before.rlim_max)
        );
    }

    /// The `RLIMIT_CPU` slot rule: the hard slot one second above `cpu_seconds` and clamped to a
    /// finite inherited hard limit, the soft slot one second below the hard one, and both equal
    /// when the hard slot is `RLIM_INFINITY` or 1.
    #[cfg(unix)]
    #[test]
    fn cpu_rlimit_slots_warn_one_second_before_the_hard_ceiling() {
        let inf = libc::RLIM_INFINITY;
        let cases: [(u64, libc::rlim_t, (libc::rlim_t, libc::rlim_t)); 5] = [
            (5, inf, (5, 6)),
            (3600, 100, (99, 100)),
            (5, 6, (5, 6)),
            (5, 1, (1, 1)),
            (u64::MAX, inf, (inf, inf)),
        ];
        for (cpu_seconds, inherited_hard, expected) in cases {
            let slots = cpu_rlimit_slots(cpu_seconds, inherited_hard);
            assert_eq!(
                (slots.rlim_cur, slots.rlim_max),
                expected,
                "cpu_seconds {cpu_seconds}, inherited hard {inherited_hard}"
            );
        }
    }

    /// With no `capabilities.resources` block every line is the default `HostResourceLimits`
    /// value, and the two CPU fields set no limit.
    #[test]
    fn resources_report_states_the_defaults_when_nothing_is_declared() {
        let declared = murmur_artifact::ResourceCapabilities::default();
        let report = ResourcesReport::new(&HostResourceLimits::resolve(None), &declared);
        let defaults = HostResourceLimits::default();
        assert!(report.rows().iter().all(|(_, row)| !row.declared));
        assert_eq!(report.memory_bytes.value, Some(defaults.memory_bytes));
        assert_eq!(
            report.workdir_max_bytes.value,
            Some(defaults.workdir_max_bytes)
        );
        assert_eq!(report.max_processes.value, Some(defaults.max_processes));
        assert_eq!(report.cgroup_pids_max.value, Some(defaults.cgroup_pids_max));
        assert_eq!(report.cpu_seconds.value, None);
        assert_eq!(report.cgroup_cpu_percent.value, None);
    }

    /// A declared field reads its declared value and `declared: true`; its neighbours keep the
    /// default and `declared: false`.
    #[test]
    fn resources_report_marks_the_declared_fields_alone() {
        let declared = murmur_artifact::ResourceCapabilities {
            memory_bytes: Some(64 * 1024 * 1024 * 1024),
            max_open_files: Some(4096),
            cgroup_cpu_percent: Some(200),
            ..Default::default()
        };
        let report = ResourcesReport::new(&HostResourceLimits::resolve(Some(&declared)), &declared);
        let declared_keys: Vec<&str> = report
            .rows()
            .iter()
            .filter(|(_, row)| row.declared)
            .map(|(key, _)| *key)
            .collect();
        assert_eq!(
            declared_keys,
            ["memory_bytes", "max_open_files", "cgroup_cpu_percent"]
        );
        assert_eq!(report.memory_bytes.value, Some(68_719_476_736));
        assert_eq!(report.max_open_files.value, Some(4096));
        assert_eq!(report.cgroup_cpu_percent.value, Some(200));
        assert_eq!(
            report.workdir_max_bytes,
            ResourceValueReport {
                value: Some(DEFAULT_WORKDIR_MAX_BYTES),
                declared: false
            }
        );
    }

    /// Every `capabilities.resources` field has a row, the rows serialize under their own keys in
    /// their own order, and each row says `declared` for a fully declared block. The block is
    /// built without `..` so a new manifest field fails to compile here until it has a row.
    #[test]
    fn resources_report_has_one_row_per_manifest_field_under_its_key() {
        let declared = murmur_artifact::ResourceCapabilities {
            max_processes: Some(1),
            max_open_files: Some(2),
            max_file_size_bytes: Some(3),
            cpu_seconds: Some(4),
            memory_bytes: Some(5),
            cgroup_memory_bytes: Some(6),
            cgroup_pids_max: Some(7),
            cgroup_cpu_percent: Some(8),
            cgroup_io_bytes_per_sec: Some(9),
            workdir_max_bytes: Some(10),
        };
        let report = ResourcesReport::new(&HostResourceLimits::resolve(Some(&declared)), &declared);
        let mut keys: Vec<&str> = report.rows().iter().map(|(key, _)| *key).collect();
        assert!(report.rows().iter().all(|(_, row)| row.declared));

        let json = serde_json::to_string(&report).unwrap();
        let positions: Vec<usize> = keys
            .iter()
            .map(|key| {
                json.find(&format!("\"{key}\":"))
                    .unwrap_or_else(|| panic!("{key}"))
            })
            .collect();
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]), "{json}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&json).unwrap()["cpu_seconds"],
            serde_json::json!({"value": 4, "declared": true})
        );

        let mut fields = declared_resource_fields(&declared);
        keys.sort_unstable();
        fields.sort_unstable();
        assert_eq!(keys, fields);
    }

    /// The text block: one aligned `<key>: <value> (<source>)` line per field, `<none>` for an
    /// unset CPU field.
    #[test]
    fn resources_report_renders_one_line_per_field() {
        let declared = murmur_artifact::ResourceCapabilities {
            memory_bytes: Some(68_719_476_736),
            ..Default::default()
        };
        let rendered =
            ResourcesReport::new(&HostResourceLimits::resolve(Some(&declared)), &declared).render();
        assert_eq!(
            rendered,
            "Resources\n\
             \x20 memory_bytes:            68719476736 (declared)\n\
             \x20 workdir_max_bytes:       10737418240 (default)\n\
             \x20 max_open_files:          1024 (default)\n\
             \x20 max_file_size_bytes:     4294967296 (default)\n\
             \x20 max_processes:           128 (default)\n\
             \x20 cpu_seconds:             <none> (default)\n\
             \x20 cgroup_memory_bytes:     4294967296 (default)\n\
             \x20 cgroup_pids_max:         256 (default)\n\
             \x20 cgroup_cpu_percent:      <none> (default)\n\
             \x20 cgroup_io_bytes_per_sec: 104857600 (default)\n"
        );
    }
}
