//! Process-level hardening against the `/proc/<pid>/environ` side channel.
//!
//! `build_shell_env` (see `shell.rs`) sanitizes the environment array handed to each
//! spawned subprocess, but that only covers the array `Command` passes at `execve`
//! time. It does nothing about a bash child reading its own parent's raw kernel-recorded
//! environment via `/proc/$PPID/environ` — a channel entirely outside `Command`'s
//! control. Marking the runtime process non-dumpable closes that channel: the kernel's
//! `ptrace_may_access` check (which gates `/proc/pid/environ` reads) fails for a
//! non-dumpable target even when the reader shares the same UID.

/// Marks the current process non-dumpable so no same-UID process — including its own
/// shell descendants — can read this process's `/proc/<pid>/environ`.
///
/// Must be called as the first statement of `main()` in every binary that links this
/// crate and can reach `execute_shell`, before any argument parsing or subprocess-capable
/// code runs. A future binary that links `capsule-runtime` and reaches `execute_shell`
/// inherits the same obligation.
///
/// No-op on non-Linux targets: `/proc/<pid>/environ` and `prctl(2)` don't exist there,
/// so there is no side channel to close.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
pub fn harden_process_dumpable() -> Result<(), String> {
    // SAFETY: PR_SET_DUMPABLE takes a single int argument and has no pointer/lifetime
    // requirements; the extra 0 args are ignored by prctl's variadic C signature.
    let result = unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) };
    if result == 0 {
        Ok(())
    } else {
        Err(format!(
            "prctl(PR_SET_DUMPABLE, 0) failed: {}",
            std::io::Error::last_os_error()
        ))
    }
}

#[cfg(not(target_os = "linux"))]
pub fn harden_process_dumpable() -> Result<(), String> {
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    #[allow(unsafe_code)]
    fn harden_process_dumpable_clears_dumpable_flag() {
        harden_process_dumpable().expect("prctl(PR_SET_DUMPABLE) should succeed");

        // SAFETY: PR_GET_DUMPABLE takes no further arguments; reading back our own
        // process's flag is side-effect-free.
        let flag = unsafe { libc::prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0) };
        assert_eq!(flag, 0, "process should report non-dumpable after hardening");
    }
}

#[cfg(all(test, not(target_os = "linux")))]
mod non_linux_tests {
    use super::*;

    #[test]
    fn harden_process_dumpable_is_noop_ok() {
        assert!(harden_process_dumpable().is_ok());
    }
}
