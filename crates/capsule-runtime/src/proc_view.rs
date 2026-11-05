//! The `/proc` a capsule command sees: a private `procfs` of the session's own PID namespace, or
//! the host's.
//!
//! Every command on a Linux kernel tier runs inside its session's PID namespace
//! ([`crate::network_namespace`] creates it with the network namespace; each command's relay
//! enters it in `sandbox::linux_enforce`). Where the host lets a process of that namespace mount
//! `proc`, the command mounts one over `/proc` in a private mount namespace, and on `scoped` and
//! `sealed` Landlock grants `ReadFile | ReadDir` beneath it — never `Execute`, never a write right.
//!
//! | View | What a command reaches at `/proc` |
//! |---|---|
//! | [`ProcView::Private`] | the session's own processes and the kernel's world-readable files (`cpuinfo`, `meminfo`, `stat`, `/proc/sys`, …); no host process |
//! | [`ProcView::Host`] | the host's `/proc`; on `scoped` and `sealed` every open beneath it is `EACCES` |
//!
//! [`proc_view`] is the one decision, made from the session's [`crate::sandbox::HostProbe`]: the
//! private view exactly when the probe's private-procfs rehearsal mounted one
//! ([`crate::namespace_probe::NamespaceReading::private_proc`]). A session's
//! [`crate::sandbox::ShellEnforcement`] carries the answer and its
//! [`crate::containment::ScopeReport`] says it, both from the same probe.
//!
//! `hidepid` masks, within the private view, every process the command may not `ptrace`-read:
//! the namespace's holder (pid 1, which holds capabilities the command does not) and, under
//! Landlock, a process of another command, whose domain is not the reader's.

use serde::Serialize;

use crate::namespace_probe::NamespaceReading;
use crate::sandbox::EnforcementTier;

/// What every failure to mount the private `/proc` at spawn starts with.
pub const PRIVATE_PROC_FAILURE_PREFIX: &str =
    "sandbox: could not mount this session's private /proc:";

/// The `mounted_by` wire name of the private view.
pub const PID_NAMESPACE_MECHANISM: &str = "pid-namespace";

/// Which `/proc` a session's commands see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcView {
    /// A `procfs` of the session's PID namespace, mounted over `/proc` by each command.
    Private,
    /// The host's `/proc`, for the reason given.
    Host(HostProcReason),
}

/// Why a session's commands see the host's `/proc`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostProcReason {
    /// No kernel containment: macOS and every non-Linux host, and the environment-only tier.
    NoKernelContainment,
    /// The session runs unconfined, outside every namespace.
    Unconfined,
    /// The probe's private-procfs rehearsal failed: the failure, as `mur doctor` prints it.
    MountRefused(String),
    /// The report was built without a namespace probe to decide from.
    NotProbed,
}

impl ProcView {
    /// Whether the commands see a private `/proc`.
    #[must_use]
    pub fn is_private(&self) -> bool {
        matches!(self, Self::Private)
    }

    /// The mechanism's wire name, `null` in the report for the host's `/proc`.
    #[must_use]
    pub fn mounted_by(&self) -> Option<&'static str> {
        match self {
            Self::Private => Some(PID_NAMESPACE_MECHANISM),
            Self::Host(_) => None,
        }
    }
}

impl HostProcReason {
    /// The reason in words an operator can act on: `not_private_because` in the report.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::NoKernelContainment => "this host has no kernel containment for capsule \
                 commands, so they see the host's own /proc"
                .to_string(),
            Self::Unconfined => {
                "the session runs unconfined, so commands see the host's own /proc".to_string()
            }
            Self::MountRefused(failure) => format!(
                "this host cannot mount a private /proc in the session's PID namespace \
                 ({failure}), so commands keep the host's /proc, and on scoped and sealed every \
                 open beneath it is refused"
            ),
            Self::NotProbed => "no namespace probe decided this view".to_string(),
        }
    }
}

/// The view a session on the tier `applied` gets from the namespace reading its probe took.
#[must_use]
pub(crate) fn proc_view(applied: EnforcementTier, reading: &NamespaceReading) -> ProcView {
    match applied {
        EnforcementTier::Unconfined => ProcView::Host(HostProcReason::Unconfined),
        EnforcementTier::EnvironmentOnly => ProcView::Host(HostProcReason::NoKernelContainment),
        EnforcementTier::KernelFull
        | EnforcementTier::KernelSealed
        | EnforcementTier::KernelSeccompOnly => {
            if !cfg!(target_os = "linux") {
                return ProcView::Host(HostProcReason::NoKernelContainment);
            }
            if reading.private_proc_created() {
                ProcView::Private
            } else {
                ProcView::Host(HostProcReason::MountRefused(
                    reading
                        .private_proc
                        .failure_summary()
                        .or_else(|| reading.egress_namespace_failure().map(|f| f.summary()))
                        .unwrap_or_else(|| "the rehearsal was not attempted".to_string()),
                ))
            }
        }
    }
}

/// The view an enforcement or report that was handed no probe states for the tier `applied`.
#[must_use]
pub(crate) fn tier_proc_view(applied: EnforcementTier) -> ProcView {
    match applied {
        EnforcementTier::Unconfined => ProcView::Host(HostProcReason::Unconfined),
        EnforcementTier::EnvironmentOnly => ProcView::Host(HostProcReason::NoKernelContainment),
        _ if !cfg!(target_os = "linux") => ProcView::Host(HostProcReason::NoKernelContainment),
        _ => ProcView::Host(HostProcReason::NotProbed),
    }
}

/// The `/proc` a session's commands see — `--explain-scope`'s `Process view` block,
/// `session_start.effective_grants.process_view` and `mur doctor`'s section, all from
/// [`process_view_report`]. Field names are the JSON keys; every field is always serialized.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProcessViewReport {
    /// Whether commands see a private `/proc` of the session's PID namespace.
    pub private: bool,
    /// The mechanism: `pid-namespace` for the private view, `null` for the host's.
    pub mounted_by: Option<&'static str>,
    /// Why commands see the host's `/proc`; `null` for the private view.
    pub not_private_because: Option<String>,
}

/// The report of `view`. Pure.
#[must_use]
pub fn process_view_report(view: &ProcView) -> ProcessViewReport {
    ProcessViewReport {
        private: view.is_private(),
        mounted_by: view.mounted_by(),
        not_private_because: match view {
            ProcView::Private => None,
            ProcView::Host(reason) => Some(reason.describe()),
        },
    }
}

/// The `Process view` block of `--explain-scope` and `mur doctor`. Ends with a newline.
#[must_use]
pub fn render_process_view(report: &ProcessViewReport) -> String {
    match (report.mounted_by, &report.not_private_because) {
        (Some(mechanism), _) => format!(
            "Process view\n  /proc:     private ({mechanism}) — the session's own processes and \
             the kernel's world-readable files; no host process\n  grant:     read and list \
             (scoped, sealed); never execute or write\n"
        ),
        (None, Some(reason)) => format!("Process view\n  /proc:     host — {reason}\n"),
        (None, None) => "Process view\n  /proc:     host\n".to_string(),
    }
}

/// One step of mounting the private `/proc` in a command's `pre_exec` window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcMountStep {
    UnshareMount,
    MakePrivate,
    MountProc,
    OpenProc,
}

impl ProcMountStep {
    /// The call, as the spawn failure names it.
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::UnshareMount => "unshare(CLONE_NEWNS)",
            Self::MakePrivate => "making / MS_REC|MS_PRIVATE",
            Self::MountProc => "mounting proc over /proc",
            Self::OpenProc => "opening the new /proc",
        }
    }
}

/// `<prefix> <step> failed: <errno>`, the spawn failure of a step. Allocates: called only on the
/// failure path, as every other step's message in the `pre_exec` window is.
#[must_use]
pub fn describe_mount_failure(step: ProcMountStep, errno: i32) -> String {
    format!(
        "{PRIVATE_PROC_FAILURE_PREFIX} {} failed: {} ({})",
        step.describe(),
        crate::network_namespace::errno_label(errno),
        crate::network_namespace::errno_words(errno)
    )
}

#[cfg(target_os = "linux")]
pub(crate) use linux::mount_private_proc;

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod linux {
    use std::os::fd::RawFd;

    use super::ProcMountStep;

    /// Mounts a `procfs` of the calling process's PID namespace over `/proc` and returns an
    /// `O_PATH | O_DIRECTORY | O_CLOEXEC` descriptor of its root, for the Landlock rule.
    ///
    /// With `enter_mount_namespace`, first enters a private mount namespace with private
    /// propagation, for a spawn whose `/tmp` mapping did not already. The mount is `MS_NOSUID |
    /// MS_NODEV | MS_NOEXEC`, with the first of [`crate::sealed::PROC_HIDEPID_OPTIONS`] the kernel
    /// accepts.
    ///
    /// # Safety
    /// Post-`fork()`, pre-exec child context only, inside the session's user and PID namespaces
    /// and before the seccomp filter (which denies `unshare` and `mount`). Every call is
    /// async-signal-safe and nothing allocates.
    pub(crate) unsafe fn mount_private_proc(
        enter_mount_namespace: bool,
    ) -> Result<RawFd, (ProcMountStep, i32)> {
        // SAFETY: the caller's contract covers this whole body; every pointer is a live local or
        // a static C string.
        unsafe {
            let errno = || *libc::__errno_location();
            if enter_mount_namespace {
                if libc::unshare(libc::CLONE_NEWNS) != 0 {
                    return Err((ProcMountStep::UnshareMount, errno()));
                }
                if libc::mount(
                    std::ptr::null(),
                    c"/".as_ptr(),
                    std::ptr::null(),
                    libc::MS_REC | libc::MS_PRIVATE,
                    std::ptr::null(),
                ) != 0
                {
                    return Err((ProcMountStep::MakePrivate, errno()));
                }
            }
            let mut mount_errno = 0;
            let mut mounted = false;
            for option in crate::sealed::PROC_HIDEPID_OPTIONS {
                let mut buffer = [0u8; 32];
                let bytes = option.as_bytes();
                if bytes.len() >= buffer.len() {
                    continue;
                }
                buffer[..bytes.len()].copy_from_slice(bytes);
                let data = if bytes.is_empty() {
                    std::ptr::null()
                } else {
                    buffer.as_ptr().cast::<libc::c_void>()
                };
                if libc::mount(
                    c"proc".as_ptr(),
                    c"/proc".as_ptr(),
                    c"proc".as_ptr(),
                    libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
                    data,
                ) == 0
                {
                    mounted = true;
                    break;
                }
                mount_errno = errno();
            }
            if !mounted {
                return Err((ProcMountStep::MountProc, mount_errno));
            }
            let fd = libc::open(
                c"/proc".as_ptr(),
                libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
            );
            if fd < 0 {
                return Err((ProcMountStep::OpenProc, errno()));
            }
            Ok(fd)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::namespace_probe::MountAttempt;

    fn reading(private_proc: MountAttempt) -> NamespaceReading {
        NamespaceReading {
            private_proc,
            ..NamespaceReading::not_linux()
        }
    }

    #[test]
    fn unconfined_and_environment_only_sessions_keep_the_host_proc_with_their_own_reason() {
        let created = reading(MountAttempt::Created);
        assert_eq!(
            proc_view(EnforcementTier::Unconfined, &created),
            ProcView::Host(HostProcReason::Unconfined)
        );
        assert_eq!(
            proc_view(EnforcementTier::EnvironmentOnly, &created),
            ProcView::Host(HostProcReason::NoKernelContainment)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_kernel_tier_gets_the_private_view_exactly_when_the_rehearsal_mounted_one() {
        use crate::namespace_probe::{MountAttemptError, MountStep};

        for tier in [
            EnforcementTier::KernelFull,
            EnforcementTier::KernelSealed,
            EnforcementTier::KernelSeccompOnly,
        ] {
            assert_eq!(
                proc_view(tier, &reading(MountAttempt::Created)),
                ProcView::Private
            );
            let refused = proc_view(
                tier,
                &reading(MountAttempt::Failed(MountAttemptError::Step {
                    step: MountStep::MountProc,
                    errno: libc::EPERM,
                })),
            );
            let ProcView::Host(HostProcReason::MountRefused(summary)) = &refused else {
                panic!("{refused:?}");
            };
            assert!(summary.contains("EPERM"), "{summary}");
            let report = process_view_report(&refused);
            assert!(!report.private);
            assert_eq!(report.mounted_by, None);
            assert!(report
                .not_private_because
                .as_deref()
                .is_some_and(|reason| reason.contains("mounting proc over /proc")));
        }
    }

    #[test]
    fn the_report_serializes_every_field_and_renders_one_block() {
        let report = process_view_report(&ProcView::Private);
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "private": true,
                "mounted_by": "pid-namespace",
                "not_private_because": null,
            })
        );
        let rendered = render_process_view(&report);
        assert!(rendered.starts_with("Process view\n"), "{rendered}");
        assert!(rendered.contains("private (pid-namespace)"), "{rendered}");
        assert!(rendered.contains("read and list"), "{rendered}");
        let host = render_process_view(&process_view_report(&ProcView::Host(
            HostProcReason::Unconfined,
        )));
        assert!(
            host.contains("/proc:     host — the session runs unconfined"),
            "{host}"
        );
    }

    #[test]
    fn a_mount_failure_names_its_step_and_errno_under_the_prefix() {
        let message = describe_mount_failure(ProcMountStep::MountProc, 1);
        assert!(
            message.starts_with(PRIVATE_PROC_FAILURE_PREFIX),
            "{message}"
        );
        assert!(
            message.contains("mounting proc over /proc failed: EPERM"),
            "{message}"
        );
    }
}
