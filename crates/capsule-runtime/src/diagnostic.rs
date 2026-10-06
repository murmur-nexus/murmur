//! The runtime's one way to write a line to standard output or standard error.
//!
//! `println!` and friends panic when the write fails, and `panic = "abort"` in the release
//! profile turns that panic into `SIGABRT` at the print site — no unwind, no destructors, no
//! `session_end`. A reader that takes the readiness line `mur run --json` writes and then closes
//! the pipe is the ordinary success case, and every later write to that stream fails. Nothing
//! here panics on a write error.
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
//!
//! It is also the `mur` CLI's one way to write report output — what a short-lived command such
//! as `mur trace show` or `mur ps` prints for its reader — through [`report_to_stdout`],
//! [`report_to_stderr`] and the `report_println!` family. A report line has no session to fall
//! back to, so a stream that refuses one is latched closed: every later report write to it is
//! dropped without a syscall, and the command finishes its work and exits with its own status.
//! A closed reader is the end of the output, not a failure. A stdout write that fails for any
//! other reason, such as `ENOSPC` on a redirect to a full disk, is recorded once and read back
//! with [`stdout_failure`], so the command can be reported as failed after it has finished.

use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
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
/// Called from `runtime::stage_session` as soon as it creates the session directory, and again
/// from `runtime::launch` before the session announces itself. Setting it again replaces the
/// previous value: a process that runs several sessions in sequence sends each session's
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

/// [`to_stdout`] for a line that carries a credential, such as a door token. A line the stream
/// refuses is [`Emitted::Dropped`], never appended to `logs/bootstrap.log`: a credential written
/// into the session directory would outlive the one reader it was printed for.
pub fn credential_to_stdout(line: &str) -> Emitted {
    emit_to(&mut std::io::stdout().lock(), line, None)
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

/// Set once a report write to standard output has failed; every later report write to it is
/// dropped unattempted.
static STDOUT_CLOSED: AtomicBool = AtomicBool::new(false);

/// [`STDOUT_CLOSED`] for standard error.
static STDERR_CLOSED: AtomicBool = AtomicBool::new(false);

/// The first standard-output report error that was not `BrokenPipe`, as its display text.
static STDOUT_FAILURE: Mutex<Option<String>> = Mutex::new(None);

/// Write `bytes` verbatim to standard output as report output, and flush.
///
/// Never panics and never falls back to `logs/bootstrap.log`. A refused write latches standard
/// output closed and returns [`Emitted::Dropped`]; so does every call after it, without touching
/// the stream. A refusal other than `BrokenPipe` is also kept for [`stdout_failure`].
pub fn report_to_stdout(bytes: &[u8]) -> Emitted {
    if STDOUT_CLOSED.load(Ordering::Acquire) {
        return Emitted::Dropped;
    }
    report_to(
        &mut std::io::stdout().lock(),
        bytes,
        &STDOUT_CLOSED,
        Some(&STDOUT_FAILURE),
    )
}

/// [`report_to_stdout`] for standard error. A refusal of any kind only latches the stream:
/// there is nowhere left to report it.
pub fn report_to_stderr(bytes: &[u8]) -> Emitted {
    if STDERR_CLOSED.load(Ordering::Acquire) {
        return Emitted::Dropped;
    }
    report_to(&mut std::io::stderr().lock(), bytes, &STDERR_CLOSED, None)
}

/// Whether a report write to standard output has failed. A command rendering a long report
/// may stop early once this is true; nothing it writes afterwards reaches the stream.
pub fn stdout_closed() -> bool {
    STDOUT_CLOSED.load(Ordering::Acquire)
}

/// The first report write error on standard output that was not `BrokenPipe`, or `None` when
/// every report write succeeded or the reader merely went away.
pub fn stdout_failure() -> Option<String> {
    STDOUT_FAILURE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// [`report_to_stdout`]'s core, over any writer and any latch, so a test can hand it a refusing
/// stream without closing the process's own.
///
/// `failure`, when given, receives the text of the first error that is not `BrokenPipe`.
pub(crate) fn report_to<W: Write>(
    out: &mut W,
    bytes: &[u8],
    closed: &AtomicBool,
    failure: Option<&Mutex<Option<String>>>,
) -> Emitted {
    if closed.load(Ordering::Acquire) {
        return Emitted::Dropped;
    }
    match out.write_all(bytes).and_then(|()| out.flush()) {
        Ok(()) => Emitted::Written,
        Err(error) => {
            closed.store(true, Ordering::Release);
            if error.kind() != ErrorKind::BrokenPipe {
                if let Some(slot) = failure {
                    slot.lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .get_or_insert_with(|| error.to_string());
                }
            }
            Emitted::Dropped
        }
    }
}

/// The text `report_println!` and `report_eprintln!` write: the `format!` output and one
/// newline, or a lone newline for the empty form.
#[doc(hidden)]
#[macro_export]
macro_rules! __report_line {
    () => {
        ::std::string::String::from("\n")
    };
    ($($arg:tt)*) => {{
        let mut line = ::std::format!($($arg)*);
        line.push('\n');
        line
    }};
}

/// `println!` for report output, through [`report_to_stdout`]: same arguments, same bytes on an
/// open stream, and a closed stream drops the line instead of panicking. Evaluates to `()`.
#[macro_export]
macro_rules! report_println {
    ($($arg:tt)*) => {{
        $crate::diagnostic::report_to_stdout($crate::__report_line!($($arg)*).as_bytes());
    }};
}

/// `print!` for report output, through [`report_to_stdout`]. Flushes on every call.
#[macro_export]
macro_rules! report_print {
    ($($arg:tt)*) => {{
        $crate::diagnostic::report_to_stdout(::std::format!($($arg)*).as_bytes());
    }};
}

/// `eprintln!` for report output, through [`report_to_stderr`].
#[macro_export]
macro_rules! report_eprintln {
    ($($arg:tt)*) => {{
        $crate::diagnostic::report_to_stderr($crate::__report_line!($($arg)*).as_bytes());
    }};
}

/// `eprint!` for report output, through [`report_to_stderr`].
#[macro_export]
macro_rules! report_eprint {
    ($($arg:tt)*) => {{
        $crate::diagnostic::report_to_stderr(::std::format!($($arg)*).as_bytes());
    }};
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Error, ErrorKind};

    /// A stream that refuses every write and flush with `kind`, counting each call.
    /// `ErrorKind::BrokenPipe` is a stream whose reader has gone away.
    struct Refusing {
        kind: ErrorKind,
        calls: usize,
    }

    impl Refusing {
        fn new(kind: ErrorKind) -> Self {
            Self { kind, calls: 0 }
        }
    }

    impl Write for Refusing {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            self.calls += 1;
            Err(Error::new(self.kind, "refused"))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.calls += 1;
            Err(Error::new(self.kind, "refused"))
        }
    }

    #[test]
    fn refused_line_lands_in_the_bootstrap_log() {
        let workdir = tempfile::tempdir().expect("temp workdir");
        let outcome = emit_to(
            &mut Refusing::new(ErrorKind::BrokenPipe),
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
        let outcome = emit_to(
            &mut Refusing::new(ErrorKind::BrokenPipe),
            "a warning raised during staging",
            None,
        );
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
        let outcome = emit_raw_to(
            &mut Refusing::new(ErrorKind::BrokenPipe),
            "child line\n",
            Some(workdir.path()),
        );
        assert_eq!(outcome, Emitted::Logged);
        let log = workdir.path().join("logs").join("bootstrap.log");
        let contents = std::fs::read_to_string(&log).expect("bootstrap.log was written");
        assert_eq!(contents, "child line\n");
    }

    #[test]
    fn report_over_a_broken_pipe_latches_and_stops_writing() {
        let closed = AtomicBool::new(false);
        let failure = Mutex::new(None);
        let mut out = Refusing::new(ErrorKind::BrokenPipe);
        let first = report_to(&mut out, b"row one\n", &closed, Some(&failure));
        assert_eq!(first, Emitted::Dropped);
        assert!(closed.load(Ordering::Acquire));
        assert_eq!(*failure.lock().unwrap(), None);
        let calls_after_first = out.calls;
        assert!(calls_after_first > 0);

        let second = report_to(&mut out, b"row two\n", &closed, Some(&failure));
        assert_eq!(second, Emitted::Dropped);
        assert_eq!(
            out.calls, calls_after_first,
            "a latched stream is not written"
        );
    }

    #[test]
    fn report_over_a_full_disk_latches_and_records_the_error() {
        let closed = AtomicBool::new(false);
        let failure = Mutex::new(None);
        let mut out = Refusing::new(ErrorKind::StorageFull);
        let outcome = report_to(&mut out, b"row\n", &closed, Some(&failure));
        assert_eq!(outcome, Emitted::Dropped);
        assert!(closed.load(Ordering::Acquire));
        assert_eq!(failure.lock().unwrap().as_deref(), Some("refused"));
    }

    #[test]
    fn report_over_an_open_stream_writes_the_formatted_bytes() {
        let cases: [(String, &str); 4] = [
            (crate::__report_line!("{} rows", 3), "3 rows\n"),
            (crate::__report_line!(), "\n"),
            (::std::format!("value for {}: ", "KEY"), "value for KEY: "),
            (crate::__report_line!("{:>4}|", 7), "   7|\n"),
        ];
        for (text, expected) in cases {
            let closed = AtomicBool::new(false);
            let mut sink: Vec<u8> = Vec::new();
            let outcome = report_to(&mut sink, text.as_bytes(), &closed, None);
            assert_eq!(outcome, Emitted::Written);
            assert_eq!(String::from_utf8(sink).unwrap(), expected);
            assert!(!closed.load(Ordering::Acquire));
        }
    }
}
