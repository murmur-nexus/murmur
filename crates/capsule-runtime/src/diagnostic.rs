//! The runtime's one way to write a line to standard output or standard error.
//!
//! `println!` and friends panic when the write fails, and `panic = "abort"` in the release
//! profile turns that panic into `SIGABRT` at the print site — no unwind, no destructors, no
//! `session_end`. A supervisor that launches `mur run --json`, reads the one readiness line the
//! flag exists to produce and stops reading is the ordinary success case, and it closed the pipe
//! the runtime was about to write to. Nothing here panics on a write error.
//!
//! A line the stream refused is not discarded where it can be kept: it goes to
//! `<session workdir>/logs/bootstrap.log` through [`crate::agent::append_bootstrap_log`], the
//! same file the rest of the runtime's bootstrap diagnostics land in. Before a session workdir
//! exists there is nowhere else to put it and the line is dropped — the warnings that fire during
//! staging say so in their own doc comments.
//!
//! Nothing here touches the broken-pipe signal. Rust's default is to ignore it, and that default
//! is what turns a write to a closed pipe into an `EPIPE` this module can handle rather than an
//! immediate kill. Restoring the system default disposition would trade the abort for death by
//! signal, which is the same loss.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::agent::append_bootstrap_log;

/// What became of one diagnostic line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Emitted {
    /// The stream took it.
    Written,
    /// The stream refused it and it was appended to `logs/bootstrap.log` instead.
    Logged,
    /// The stream refused it and no session workdir was armed, so it went nowhere.
    Dropped,
}

/// Where a refused line goes. `None` until [`set_diagnostic_workdir`] names a session directory.
///
/// Process-wide rather than threaded through every caller: a diagnostic can be raised from a
/// spawned tokio task or a detached drain thread that holds no handle on the session.
static FALLBACK_WORKDIR: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Arm the fallback with this session's directory — the one that holds `logs/`, not the user
/// project directory a capsule sees as `"."`.
///
/// Called from `runtime::launch` before the session announces itself. Setting it again replaces
/// the previous value: a process that runs several sessions in sequence sends each session's
/// refused lines to the session that is currently launching.
pub fn set_diagnostic_workdir(workdir: &Path) {
    let mut slot = FALLBACK_WORKDIR
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *slot = Some(workdir.to_path_buf());
}

/// The armed fallback directory, or `None` when no session workdir exists yet.
pub fn diagnostic_workdir() -> Option<PathBuf> {
    FALLBACK_WORKDIR
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// Write `line` and a newline to standard output.
pub fn to_stdout(line: &str) -> Emitted {
    let fallback = diagnostic_workdir();
    emit_to(&mut std::io::stdout().lock(), line, fallback.as_deref())
}

/// Write `line` and a newline to standard error.
pub fn to_stderr(line: &str) -> Emitted {
    let fallback = diagnostic_workdir();
    emit_to(&mut std::io::stderr().lock(), line, fallback.as_deref())
}

/// Write `chunk` to standard output exactly as given, adding no newline.
///
/// For relaying bytes that already carry their own line endings — a delegated child capsule's
/// stdout, a rendered report.
pub fn raw_to_stdout(chunk: &str) -> Emitted {
    let fallback = diagnostic_workdir();
    emit_raw_to(&mut std::io::stdout().lock(), chunk, fallback.as_deref())
}

/// [`to_stdout`]'s core, over any writer, so a test can hand it a stream that refuses writes.
pub(crate) fn emit_to<W: Write>(out: &mut W, line: &str, fallback: Option<&Path>) -> Emitted {
    let written = out
        .write_all(line.as_bytes())
        .and_then(|()| out.write_all(b"\n"))
        .and_then(|()| out.flush());
    match written {
        Ok(()) => Emitted::Written,
        Err(_) => fall_back(line, fallback),
    }
}

/// [`raw_to_stdout`]'s core. `chunk` is written verbatim; the fallback line is it without its
/// trailing newlines, since `append_bootstrap_log` ends every record with one of its own.
pub(crate) fn emit_raw_to<W: Write>(out: &mut W, chunk: &str, fallback: Option<&Path>) -> Emitted {
    let written = out.write_all(chunk.as_bytes()).and_then(|()| out.flush());
    match written {
        Ok(()) => Emitted::Written,
        Err(_) => fall_back(chunk.trim_end_matches('\n'), fallback),
    }
}

fn fall_back(line: &str, fallback: Option<&Path>) -> Emitted {
    match fallback {
        Some(workdir) => {
            append_bootstrap_log(workdir, line);
            Emitted::Logged
        }
        None => Emitted::Dropped,
    }
}

/// Write a formatted line to standard output, falling back to `logs/bootstrap.log`.
///
/// Takes `format!` arguments and appends the newline, and evaluates to `()`, so it is a drop-in
/// for `println!` anywhere one appears — statement or match arm. A caller that wants to know what
/// became of the line calls [`to_stdout`] directly.
#[macro_export]
macro_rules! runtime_out {
    ($($arg:tt)*) => {{
        $crate::diagnostic::to_stdout(&::std::format!($($arg)*));
    }};
}

/// [`runtime_out!`] for standard error, a drop-in for `eprintln!`. See [`to_stderr`].
#[macro_export]
macro_rules! runtime_err {
    ($($arg:tt)*) => {{
        $crate::diagnostic::to_stderr(&::std::format!($($arg)*));
    }};
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Error, ErrorKind};

    /// A stream that has gone away: every write fails the way a closed pipe fails.
    struct BrokenPipe;

    impl Write for BrokenPipe {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(Error::new(
                ErrorKind::BrokenPipe,
                "Broken pipe (os error 32)",
            ))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Err(Error::new(
                ErrorKind::BrokenPipe,
                "Broken pipe (os error 32)",
            ))
        }
    }

    #[test]
    fn refused_line_lands_in_the_bootstrap_log() {
        let workdir = tempfile::tempdir().expect("temp workdir");
        let outcome = emit_to(
            &mut BrokenPipe,
            "{\"url\":\"http://127.0.0.1:1/\"}",
            Some(workdir.path()),
        );
        assert_eq!(outcome, Emitted::Logged);
        let log = workdir.path().join("logs").join("bootstrap.log");
        let contents = std::fs::read_to_string(&log).expect("bootstrap.log was written");
        assert_eq!(contents, "{\"url\":\"http://127.0.0.1:1/\"}\n");
    }

    #[test]
    fn refused_line_with_no_workdir_is_dropped() {
        let outcome = emit_to(&mut BrokenPipe, "a warning raised during staging", None);
        assert_eq!(outcome, Emitted::Dropped);
    }

    #[test]
    fn accepted_line_gets_exactly_one_newline() {
        let mut sink: Vec<u8> = Vec::new();
        let outcome = emit_to(&mut sink, "session: ses_01", None);
        assert_eq!(outcome, Emitted::Written);
        assert_eq!(String::from_utf8(sink).unwrap(), "session: ses_01\n");
    }

    #[test]
    fn raw_chunk_is_written_verbatim() {
        let mut sink: Vec<u8> = Vec::new();
        let outcome = emit_raw_to(&mut sink, "child line\n", None);
        assert_eq!(outcome, Emitted::Written);
        assert_eq!(String::from_utf8(sink).unwrap(), "child line\n");
    }

    #[test]
    fn refused_raw_chunk_is_logged_without_a_doubled_newline() {
        let workdir = tempfile::tempdir().expect("temp workdir");
        let outcome = emit_raw_to(&mut BrokenPipe, "child line\n", Some(workdir.path()));
        assert_eq!(outcome, Emitted::Logged);
        let log = workdir.path().join("logs").join("bootstrap.log");
        let contents = std::fs::read_to_string(&log).expect("bootstrap.log was written");
        assert_eq!(contents, "child line\n");
    }
}
