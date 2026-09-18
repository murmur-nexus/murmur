//! The runtime's diagnostic writers against a standard output that really has no reader.
//!
//! `diagnostic`'s unit tests drive `emit_to` and `emit_raw_to` over a writer that fails every
//! write, which settles the branch but not the claim a capsule's life depends on: `to_stdout` and
//! `raw_to_stdout` lock the process's own `std::io::stdout()`, and that handle is what a
//! supervisor's closed pipe breaks. Proving it takes a process whose file descriptor 1 is a pipe
//! with no reader left.
//!
//! Descriptor 1 belongs to the process rather than to a test, so this case cannot run beside the
//! others in this binary — closing it would take the harness's own output with it. It runs in a
//! re-executed copy of this binary, and the two halves speak through files in a shared directory
//! because the channel they would otherwise use is the one under test.

// This target's streams are the `cargo test` harness's, and the probe writes to them on purpose.
#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use capsule_runtime::diagnostic;

/// Set only in the re-executed copy: the directory the two halves share.
const PROBE_DIR: &str = "MUR_TEST_CLOSED_STDOUT_PROBE_DIR";

/// The probe says this while the pipe still has a reader. The supervisor closes its end on seeing
/// it.
const READY: &str = "closed-stdout-probe-ready";

/// The supervisor creates this once its end of the pipe is gone, which is the probe's cue.
const CLOSED: &str = "reader-has-gone";

/// Where the probe records what became of each write, since it cannot say so on stdout.
const OUTCOME: &str = "outcome.txt";

/// A delegated child's line, carrying the newline `raw_to_stdout` relays rather than adds.
const RELAYED: &str = "a delegated child's line, written after the parent's reader went\n";

/// A diagnostic the runtime raises after the readiness line.
const DIAGNOSTIC: &str = "a diagnostic raised after the readiness line";

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

const CASE: &str = "the_runtimes_writers_survive_a_real_closed_standard_output";

/// Both halves of the case: the supervisor when the environment names no shared directory, the
/// probe when it does.
#[test]
fn the_runtimes_writers_survive_a_real_closed_standard_output() {
    match std::env::var_os(PROBE_DIR) {
        Some(shared) => probe(Path::new(&shared)),
        None => supervise(),
    }
}

/// Read the probe's one line, drop the pipe, and require that the probe outlives it.
fn supervise() {
    let shared = tempfile::tempdir().expect("shared directory");
    let mut probe = Command::new(std::env::current_exe().expect("this test binary"))
        .args(["--exact", CASE, "--nocapture", "--test-threads=1"])
        .env(PROBE_DIR, shared.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("re-execute this test binary");

    // Drained on a thread and kept: if the probe panics, its message is on stderr, and stderr is
    // the one stream this case leaves alone.
    let stderr = probe.stderr.take().expect("the probe's stderr");
    let notes = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = BufReader::new(stderr).read_to_string(&mut text);
        text
    });

    let stdout = probe.stdout.take().expect("the probe's stdout");
    let mut lines = BufReader::new(stdout).lines();
    // The harness prints the case's name on this stream too, so the marker is looked for within
    // the line rather than as the whole of it.
    let announced = lines.any(|line| line.is_ok_and(|text| text.contains(READY)));

    // The act under test: the read end goes away while the probe is still running, so every later
    // write on its stdout is `EPIPE` rather than a short write.
    drop(lines);
    let notes = || notes.join().unwrap_or_default();
    assert!(
        announced,
        "the probe never reported itself ready:\n{}",
        notes()
    );
    std::fs::write(shared.path().join(CLOSED), b"").expect("release the probe");

    let status = wait_for_exit(&mut probe);
    let notes = notes();
    assert!(
        status.success(),
        "the probe did not survive writing to its closed stdout ({status}):\n{notes}"
    );
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(
            status.signal(),
            None,
            "the probe died by signal rather than exiting:\n{notes}"
        );
    }

    assert_eq!(
        std::fs::read_to_string(shared.path().join(OUTCOME))
            .unwrap_or_else(|error| panic!("the probe wrote no outcome file: {error}\n{notes}")),
        "raw=Logged\nline=Logged\n",
        "a refused write must report itself kept, not written"
    );

    // Refused is not lost: both lines are in the file the runtime keeps its bootstrap diagnostics
    // in. `raw_to_stdout` logs its chunk without the trailing newline it relayed.
    let log = shared
        .path()
        .join("session")
        .join("logs")
        .join("bootstrap.log");
    let kept = std::fs::read_to_string(&log)
        .unwrap_or_else(|error| panic!("no bootstrap.log at {}: {error}", log.display()));
    assert!(
        kept.contains(RELAYED.trim_end_matches('\n')),
        "the relayed child line is not in bootstrap.log:\n{kept}"
    );
    assert!(
        kept.contains(DIAGNOSTIC),
        "the runtime diagnostic is not in bootstrap.log:\n{kept}"
    );
}

/// Announce readiness on the open pipe, wait for it to be closed, then write through the real
/// writers and record what became of each.
fn probe(shared: &Path) -> ! {
    println!("{READY}");
    let _ = std::io::stdout().flush();

    wait_for_closed_pipe(&shared.join(CLOSED));

    // A directory of this case's own, so the refused lines land in a file nothing else wrote.
    diagnostic::set_diagnostic_workdir(&shared.join("session"));

    // The two writers that can run after the readiness line: `child_launch`'s delegated-child
    // drain relays every line its child says through `raw_to_stdout`, and every other diagnostic
    // goes through `to_stdout`.
    let raw = diagnostic::raw_to_stdout(RELAYED);
    let line = diagnostic::to_stdout(DIAGNOSTIC);

    std::fs::write(
        shared.join(OUTCOME),
        format!("raw={raw:?}\nline={line:?}\n"),
    )
    .expect("write the outcome file");

    // Exit before the harness reports, because reporting is a write to the stream this case
    // closed — and the harness writes it with `println!`.
    std::process::exit(0);
}

fn wait_for_closed_pipe(flag: &Path) {
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    while Instant::now() < deadline {
        if flag.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("the supervisor never closed its end of the pipe");
}

fn wait_for_exit(probe: &mut Child) -> ExitStatus {
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    while Instant::now() < deadline {
        if let Some(status) = probe.try_wait().expect("read the probe's status") {
            return status;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = probe.kill();
    panic!("the probe neither finished nor died within {HANDSHAKE_TIMEOUT:?}");
}
