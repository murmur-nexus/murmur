//! Shared scaffolding for the two seccomp-notify TOCTOU race probes.
//!
//! The point of this crate is fidelity, not generality: every Linux helper below is a deliberate
//! mirror of a specific function in `crates/capsule-runtime/src/sandbox.rs`'s `mod linux_enforce`,
//! so that a race won here is a race won against the real supervisor's pattern and not against a
//! strawman. Where a helper diverges, the divergence is called out in its doc comment.
//!
//! Mirrored pairs:
//!
//! | here                              | `sandbox.rs::linux_enforce`   |
//! |-----------------------------------|-------------------------------|
//! | [`linux::install_notify_filter`]  | `install_seccomp_filter`      |
//! | [`linux::supervise`]              | `supervisor_loop`             |
//! | [`linux::read_cstr_from_child`]   | `read_cstr_from_child`        |
//! | [`linux::read_sockaddr_ip_from_child`] | *(retired — see below)*       |
//! | [`linux::send_fd_over_socket`]    | `send_fd_over_socket`         |
//! | [`linux::receive_fd_over_socket`] | `receive_fd_over_socket`      |
//!
//! See `docs/content/reference/seccomp-notify-toctou-audit.md` for the audit these probes belong
//! to, including what a win and a loss each mean.
//!
//! ## The `connect` probe now mirrors a mechanism that no longer exists
//!
//! The audit's conclusion was acted on. `connect`/`sendto` are no longer `Notify` syscalls: the
//! network half of the supervisor was deleted and replaced by a network namespace plus an egress
//! proxy (`capsule-runtime`'s `network_namespace` and `egress_proxy` modules), where the
//! destination is read from an already-established connection rather than out of the notifying
//! task's memory, so there is no pointer left to race. `read_sockaddr_ip_from_child` and
//! `network_ip_allowed`'s use by the supervisor went with it.
//!
//! `connect_race` is kept, still building and still winning its race, deliberately: it is the
//! evidence the audit rests on and the reason the replacement was built. It is now a probe of a
//! **historical** design. `exec_race` is the one that still mirrors live production code — the
//! `execve`/`execveat` notify arms were out of scope for that replacement and are unchanged.

#[cfg(target_os = "linux")]
pub mod linux;

/// The message every binary in this package prints when built for a non-Linux host.
///
/// The probes exist to exercise a Linux kernel mechanism, so there is nothing honest for them to
/// do anywhere else — but they must still *build* and *run* on a macOS dev machine, because that
/// is where they are written and reviewed. Exit status is 0: "not applicable here" is not a
/// failure, and a non-zero exit would read as a broken build.
pub fn non_linux_stub(bin: &str) {
    println!(
        "{bin}: this probe only runs its race on Linux; \
         see docs/content/reference/seccomp-notify-toctou-audit.md"
    );
}

/// Parses `--iterations <n>` out of the process arguments, falling back to `default`.
///
/// Hand-rolled rather than `clap`: this package's whole dependency set is the two crates the
/// supervisor itself uses, and keeping it that way means a reviewer can convince themselves the
/// probe has no behaviour beyond what is in these three files.
pub fn iterations_from_args(default: u64) -> Result<u64, String> {
    let mut args = std::env::args().skip(1);
    let mut iterations = default;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--iterations" => {
                let raw = args
                    .next()
                    .ok_or_else(|| "--iterations requires a value".to_string())?;
                iterations = raw
                    .parse::<u64>()
                    .map_err(|_| format!("--iterations: not a number: {raw}"))?;
                if iterations == 0 {
                    return Err("--iterations must be greater than 0".to_string());
                }
            }
            "--help" | "-h" => {
                return Err(format!(
                    "usage: {} [--iterations <n>]   (default {default})",
                    std::env::args().next().unwrap_or_default()
                ));
            }
            other => return Err(format!("unrecognised argument: {other}")),
        }
    }
    Ok(iterations)
}

/// How often to print a progress line, given a total iteration count.
pub fn progress_every(iterations: u64) -> u64 {
    (iterations / 10).max(1)
}
