//! What a test that launches a formation with `mur run --roster` needs: one launch at a time on
//! the host, the launcher read as it runs, and the check that no member outlives it.

use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

/// How long one launch may take end to end before the test gives up on it.
pub const LAUNCH_LIMIT: Duration = Duration::from_secs(240);

/// Held while a test launches a formation. Each launch starts several `mur run` processes, and
/// several launches at once on one host turn a readiness deadline into a measure of contention.
/// Exclusive across the threads of one test binary and, through an exclusive lock on a file in
/// the temp directory, across test binaries.
pub struct LaunchLock {
    _thread: MutexGuard<'static, ()>,
    _host: File,
}

pub fn launch_lock() -> LaunchLock {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let thread = LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let host = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(std::env::temp_dir().join("murmur-cli-formation-launch.lock"))
        .unwrap();
    // Released when `host` is closed, which is when the returned guard drops.
    host.lock()
        .unwrap_or_else(|error| panic!("locking the formation launch lock: {error}"));
    LaunchLock {
        _thread: thread,
        _host: host,
    }
}

/// Whether `pid` is a live process: present, and not a zombie.
pub fn alive(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => {
            stat.rsplit_once(')')
                .and_then(|(_, rest)| rest.split_whitespace().next())
                != Some("Z")
        }
        Err(_) => false,
    }
}

/// Every live process whose command line contains `needle`.
pub fn processes_mentioning(needle: &str) -> Vec<(u32, String)> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(raw) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let cmdline = String::from_utf8_lossy(&raw).replace('\0', " ");
        if cmdline.contains(needle) && alive(pid) {
            found.push((pid, cmdline));
        }
    }
    found
}

/// Fail unless, within `limit`, no process on the host carries `project` on its command line and
/// every pid in `pids` is dead. The entry member runs with `--workdir <project>` and every peer
/// with `--store-root <project>`, so a unique project path finds every member, reported or not.
pub fn assert_no_member_remains(project: &Path, pids: &[u32], limit: Duration) {
    let needle = project.to_string_lossy().into_owned();
    let deadline = Instant::now() + limit;
    loop {
        let holders = processes_mentioning(&needle);
        let alive: Vec<u32> = pids.iter().copied().filter(|pid| alive(*pid)).collect();
        if holders.is_empty() && alive.is_empty() {
            return;
        }
        if Instant::now() >= deadline {
            panic!(
                "member processes remain after {limit:?}: by command line {holders:?}, by \
                 reported pid {alive:?}"
            );
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// Every `ses_*` session directory directly under `root`, sorted. A root that does not exist holds
/// none.
pub fn session_dirs(root: &Path) -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut sessions: Vec<std::path::PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_dir()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("ses_"))
        })
        .collect();
    sessions.sort();
    sessions
}

/// Every session root the formations launched under the scratch `home` gave their peers:
/// `<home>/.murmur/formations/<frm_id>/<member>/.murmur`, sorted.
pub fn peer_session_roots(home: &Path) -> Vec<std::path::PathBuf> {
    let mut roots = Vec::new();
    let Ok(formations) = std::fs::read_dir(home.join(".murmur").join("formations")) else {
        return roots;
    };
    for formation in formations.flatten() {
        let Ok(members) = std::fs::read_dir(formation.path()) else {
            continue;
        };
        for member in members.flatten() {
            roots.push(member.path().join(".murmur"));
        }
    }
    roots.sort();
    roots
}

/// Send `signal` to `pid`, a process this test started.
#[allow(unsafe_code)]
pub fn signal(pid: u32, signal: i32) {
    // SAFETY: `kill` takes two integers and dereferences nothing; `pid` is a child of this test.
    unsafe {
        libc::kill(pid as libc::pid_t, signal);
    }
}

/// The operator token the running record of `session_id` under the scratch `home` holds.
pub fn door_token(home: &Path, session_id: &str) -> String {
    let record = home
        .join(".murmur")
        .join("running")
        .join(format!("{session_id}.json"));
    let record: Value = serde_json::from_str(&std::fs::read_to_string(&record).unwrap()).unwrap();
    record["door_token"].as_str().unwrap().to_string()
}

/// Every pid a formation line and a readiness line reported.
pub fn reported_pids(formation: &Value, entry: Option<&Value>) -> Vec<u32> {
    let mut pids: Vec<u32> = formation["peers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|peer| peer["pid"].as_u64().unwrap() as u32)
        .collect();
    if let Some(entry) = entry {
        pids.push(entry["pid"].as_u64().unwrap() as u32);
    }
    pids
}

/// A running `mur run --roster`, its stdout read line by line as it arrives and its stderr
/// relayed to this test's own. Dropped while running, it is sent `SIGTERM`, then killed.
pub struct Launcher {
    pub child: Child,
    /// When the launcher was spawned.
    pub started: Instant,
    lines: mpsc::Receiver<(Instant, String)>,
    stdout: Arc<Mutex<Vec<String>>>,
    stderr: Arc<Mutex<String>>,
    /// Set by [`Launcher::close_output`]: each drain thread drops its read end and returns.
    closing: Arc<AtomicBool>,
    drains: Vec<thread::JoinHandle<()>>,
}

impl Launcher {
    /// Spawn `command` with its stdout and stderr piped.
    pub fn spawn(mut command: Command) -> Self {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let started = Instant::now();
        let mut child = command.spawn().unwrap();
        let (line_tx, lines) = mpsc::channel::<(Instant, String)>();
        let stdout = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&stdout);
        let closing = Arc::new(AtomicBool::new(false));
        let out = child.stdout.take().unwrap();
        let close_out = Arc::clone(&closing);
        let stdout_drain = thread::spawn(move || {
            drain_lines(out, &close_out, |line| {
                seen.lock().unwrap().push(line.clone());
                let _ = line_tx.send((Instant::now(), line));
            });
        });
        let stderr = Arc::new(Mutex::new(String::new()));
        let sink = Arc::clone(&stderr);
        let err = child.stderr.take().unwrap();
        let close_err = Arc::clone(&closing);
        let stderr_drain = thread::spawn(move || {
            drain_lines(err, &close_err, |line| {
                eprintln!("[launcher] {line}");
                let mut sink = sink.lock().unwrap();
                sink.push_str(&line);
                sink.push('\n');
            });
        });
        Self {
            child,
            started,
            lines,
            stdout,
            stderr,
            closing,
            drains: vec![stdout_drain, stderr_drain],
        }
    }

    /// Close this test's read ends of the launcher's stdout and stderr, as a reader that goes
    /// away does: every later write the launcher makes to either stream fails with `EPIPE`.
    /// Returns once both are closed.
    pub fn close_output(&mut self) {
        self.closing.store(true, Ordering::Release);
        for drain in self.drains.drain(..) {
            drain.join().unwrap();
        }
    }

    /// The next stdout line, parsed, and when it arrived.
    pub fn next_json(&self) -> (Instant, Value) {
        let (at, line) = self
            .lines
            .recv_timeout(LAUNCH_LIMIT)
            .unwrap_or_else(|_| panic!("no stdout line; stderr:\n{}", self.stderr()));
        let value = serde_json::from_str(&line)
            .unwrap_or_else(|_| panic!("stdout line is not JSON: {line}"));
        (at, value)
    }

    /// The launcher's exit status, failing the test if it has not exited within [`LAUNCH_LIMIT`].
    pub fn wait(&mut self) -> ExitStatus {
        let deadline = Instant::now() + LAUNCH_LIMIT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                panic!("the launcher did not exit; stderr:\n{}", self.stderr());
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// Everything the launcher wrote to stderr so far.
    pub fn stderr(&self) -> String {
        self.stderr.lock().unwrap().clone()
    }

    /// Every stdout line so far.
    pub fn stdout(&self) -> Vec<String> {
        // The relay thread may still be appending the last line when the process has exited.
        thread::sleep(Duration::from_millis(200));
        self.stdout.lock().unwrap().clone()
    }

    pub fn signal(&self, signal: i32) {
        self::signal(self.child.id(), signal);
    }
}

/// Hand every line `stream` carries to `on_line`, without its line ending, until EOF or until
/// `closing` is set, when `stream` is dropped with whatever it still holds unread.
#[allow(unsafe_code)]
fn drain_lines<R: Read + AsRawFd>(
    mut stream: R,
    closing: &AtomicBool,
    mut on_line: impl FnMut(String),
) {
    let mut pending: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    while !closing.load(Ordering::Acquire) {
        let mut ready = libc::pollfd {
            fd: stream.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `ready` is one initialised `pollfd` that lives across the call, and `nfds` is 1;
        // its descriptor is `stream`'s, open until `stream` drops at the end of this function.
        let polled = unsafe { libc::poll(&mut ready, 1, 50) };
        if polled <= 0 {
            // A timeout, to look at `closing` again, or `EINTR`.
            continue;
        }
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => {
                pending.extend_from_slice(&chunk[..read]);
                while let Some(end) = pending.iter().position(|byte| *byte == b'\n') {
                    let line: Vec<u8> = pending.drain(..=end).collect();
                    let line = String::from_utf8_lossy(&line[..end]);
                    on_line(line.trim_end_matches('\r').to_string());
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    if !pending.is_empty() && !closing.load(Ordering::Acquire) {
        on_line(String::from_utf8_lossy(&pending).into_owned());
    }
}

impl Drop for Launcher {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            self.signal(libc::SIGTERM);
            let deadline = Instant::now() + Duration::from_secs(60);
            while matches!(self.child.try_wait(), Ok(None)) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(50));
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}
