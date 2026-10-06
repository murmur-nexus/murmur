//! The `SIGPIPE` disposition a Rust process runs with and the one each child it spawns starts with.
//!
//! The Rust runtime sets `SIGPIPE` to ignored before `main`, so a write to a closed pipe is an
//! `EPIPE` the writer handles. A child spawned through `std::process::Command` starts with the
//! default disposition instead: std resets `SIGPIPE` to `SIG_DFL` between `fork` and `exec`, on the
//! plain path and on the `pre_exec` path that sandboxed shell commands and native tools take. A
//! child that is itself `mur` ignores it again when its own runtime starts.
//!
//! Signal numbers are Linux's: `SIGPIPE` is 13, bit `1 << 12` of `SigIgn` in `/proc/<pid>/status`.
#![cfg(target_os = "linux")]

use std::os::unix::process::CommandExt;
use std::process::Command;

const SIGPIPE_BIT: u64 = 1 << 12;

/// The `SigIgn` mask in a `/proc/<pid>/status` text.
fn ignored_mask(status: &str) -> u64 {
    let line = status
        .lines()
        .find(|line| line.starts_with("SigIgn:"))
        .unwrap_or_else(|| panic!("no SigIgn line in:\n{status}"));
    let hex = line.trim_start_matches("SigIgn:").trim();
    u64::from_str_radix(hex, 16).unwrap_or_else(|_| panic!("SigIgn is not hex: {line}"))
}

/// The `SigIgn` mask a child spawned through `command` starts its `exec`'d program with.
fn child_mask(mut command: Command) -> u64 {
    let output = command
        .args(["-c", "grep SigIgn /proc/self/status"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    ignored_mask(&String::from_utf8_lossy(&output.stdout))
}

#[test]
fn this_process_ignores_sigpipe() {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    assert_ne!(ignored_mask(&status) & SIGPIPE_BIT, 0, "{status}");
}

#[test]
fn a_spawned_child_starts_with_the_default_sigpipe() {
    let mask = child_mask(Command::new("sh"));
    assert_eq!(mask & SIGPIPE_BIT, 0, "SigIgn {mask:#x}");
}

#[test]
#[allow(unsafe_code)]
fn a_child_spawned_with_pre_exec_starts_with_the_default_sigpipe() {
    let mut command = Command::new("sh");
    // SAFETY: the closure runs in the forked child before `exec`, touches no memory and takes no
    // lock, so it is async-signal-safe.
    unsafe {
        command.pre_exec(|| Ok(()));
    }
    let mask = child_mask(command);
    assert_eq!(mask & SIGPIPE_BIT, 0, "SigIgn {mask:#x}");
}
