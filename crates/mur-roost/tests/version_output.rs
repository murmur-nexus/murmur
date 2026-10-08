//! `mur-roost --version` against a standard output that refuses the line.

#![cfg(target_os = "linux")]

use std::fs::File;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};

/// A version line `ENOSPC` refused fails the run: the installer's smoke run reads exit 0 as a
/// binary that starts.
#[test]
fn a_full_disk_on_stdout_fails_version_with_e_io_003() {
    let full = File::options().write(true).open("/dev/full").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_mur-roost"))
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::from(full))
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!stderr.contains("panicked at"), "{stderr}");
    assert_eq!(output.status.signal(), None, "{stderr}");
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.starts_with("error[E-IO-003]: failed to write to standard output: "),
        "{stderr}"
    );
}
