//! The census a restarted daemon takes of the capsules already running on the host.
//!
//! Every case but the timing one starts the real `mur-roost` binary under a scratch `HOME`, writes
//! running records naming real processes, and drives the daemon over loopback HTTP, because the
//! census runs in `main` before the listener exists and nothing short of the binary exercises that
//! ordering.

#[path = "common/mod.rs"]
mod common;

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, Command, Stdio};
use std::time::Instant;

use capsule_runtime::running::process_start_token;
use capsule_runtime::SPAWN_CREDENTIAL_HEADER;
use common::{publish_capsule, PLAIN_WORKER_BODY};
use mur_roost::census;

/// A capsule that may spawn `worker-a`, so a registered root can delegate.
const ORCHESTRATOR_BODY: &str = "artifacts: []\ncapabilities:\n  network:\n    allow: \
                                 [registry.internal]\n  spawn:\n    allow: [worker-a]\n";

const CENSUS_PREFIX: &str = "mur-roost: cannot count the capsules already running on this host: ";

/// Processes the test started, killed and reaped however the test ends.
#[derive(Default)]
struct Sleepers(Vec<Child>);

impl Sleepers {
    /// Starts a `sleep 300` and returns its pid.
    fn start(&mut self) -> u32 {
        let child = Command::new("sleep").arg("300").spawn().unwrap();
        let pid = child.id();
        self.0.push(child);
        pid
    }

    /// Kills and reaps the sleeper holding `pid`, so nothing holds the pid afterwards.
    fn kill(&mut self, pid: u32) {
        let at = self.0.iter().position(|child| child.id() == pid).unwrap();
        let mut child = self.0.remove(at);
        child.kill().unwrap();
        child.wait().unwrap();
    }
}

impl Drop for Sleepers {
    fn drop(&mut self) {
        for child in &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn running_dir(home: &Path) -> PathBuf {
    home.join(".murmur").join("running")
}

/// Writes a record naming `pid` at `process_start`, in the shape the runtime writes.
fn write_record(dir: &Path, session_id: &str, pid: u32, process_start: &str, url: &str) {
    std::fs::create_dir_all(dir).unwrap();
    let record = serde_json::json!({
        "session_id": session_id,
        "url": url,
        "pid": pid,
        "process_start": process_start,
        "capsule_name": "survivor",
        "capsule_version": "0.1.0",
        "workdir": "/tmp/survivor",
        "outlives_launcher": true,
        "started_at": "2026-01-01T00:00:00Z",
    });
    std::fs::write(
        dir.join(format!("{session_id}.json")),
        serde_json::to_vec_pretty(&record).unwrap(),
    )
    .unwrap();
}

/// A record for `pid` as the process holding it now, which reads as alive.
fn write_live_record(dir: &Path, session_id: &str, pid: u32) {
    let token = process_start_token(pid).unwrap();
    write_record(dir, session_id, pid, &token, "127.0.0.1:1");
}

/// A port nothing holds at the moment of asking. The daemon prints the port it was given rather
/// than the one it bound, so `--port 0` would leave the test with no address to call.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A running daemon and the stderr it printed up to and including its `machine ceiling` line.
struct Roost {
    child: Child,
    port: u16,
    startup: Vec<String>,
    /// Held open so the daemon never writes to a closed pipe.
    _stderr: BufReader<ChildStderr>,
}

impl Roost {
    fn start(home: &Path, registry: &Path, args: &[&str]) -> Self {
        let port = free_port();
        let mut child = Command::new(env!("CARGO_BIN_EXE_mur-roost"))
            .env("HOME", home)
            .args(["--port", &port.to_string()])
            .args(["--registry-path", registry.to_str().unwrap()])
            .args(args)
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stderr = BufReader::new(child.stderr.take().unwrap());
        let mut startup = Vec::new();
        for _ in 0..8 {
            let mut line = String::new();
            if stderr.read_line(&mut line).unwrap_or(0) == 0 {
                break;
            }
            let ceiling = line.contains("machine ceiling");
            startup.push(line.trim_end().to_string());
            if ceiling {
                break;
            }
        }
        Self {
            child,
            port,
            startup,
            _stderr: stderr,
        }
    }

    fn census_line(&self) -> &str {
        self.startup
            .iter()
            .find(|line| line.contains("already running on this host"))
            .unwrap_or_else(|| panic!("no census line in {:?}", self.startup))
    }

    /// One request over loopback, answered and closed by the daemon.
    fn request(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> (u16, serde_json::Value) {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        let mut request = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\n",
            body.len()
        );
        for (name, value) in headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        request.push_str("\r\n");
        request.push_str(body);
        stream.write_all(request.as_bytes()).unwrap();
        let mut raw = String::new();
        stream.read_to_string(&mut raw).unwrap();
        let response = common::Response::parse(&raw);
        (response.status, response.body)
    }

    fn register_root(&self, session_id: &str) -> String {
        let (status, body) = self.request(
            "POST",
            "/register",
            &[],
            &format!(r#"{{"session_id":"{session_id}","name":"orchestrator","version":"0.1.0"}}"#),
        );
        assert_eq!(status, 200, "{body}");
        body["credential"].as_str().unwrap().to_string()
    }

    fn spawn_worker(&self, credential: &str) -> (u16, serde_json::Value) {
        self.request(
            "POST",
            "/spawn",
            &[(SPAWN_CREDENTIAL_HEADER, credential)],
            r#"{"name":"worker-a","version":"0.1.0"}"#,
        )
    }

    fn live_capsules(&self, session_id: &str) -> u64 {
        let (status, body) = self.request("GET", &format!("/status/{session_id}"), &[], "");
        assert_eq!(status, 200, "{body}");
        body["live_capsules"].as_u64().unwrap()
    }

    fn stop(mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }
}

impl Drop for Roost {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn publishing_registry() -> tempfile::TempDir {
    let registry = tempfile::tempdir().unwrap();
    publish_capsule(
        registry.path(),
        "orchestrator",
        "0.1.0",
        ORCHESTRATOR_BODY,
        "",
    );
    publish_capsule(registry.path(), "worker-a", "0.1.0", PLAIN_WORKER_BODY, "");
    registry
}

// ── S1. A restart resumes the machine census ──────────────────────────────────

/// Two capsules survive a restart. The restarted daemon counts them before it answers anything, so
/// a root it admits fills a ceiling of three, and the first survivor to exit gives its slot back.
#[test]
fn a_restarted_daemon_counts_the_capsules_still_running() {
    let home = tempfile::tempdir().unwrap();
    let dir = running_dir(home.path());
    let registry = publishing_registry();
    let mut sleepers = Sleepers::default();
    let first = sleepers.start();
    let second = sleepers.start();
    let reused = sleepers.start();
    write_live_record(&dir, "ses_0199survivor0000000000000000001", first);
    write_live_record(&dir, "ses_0199survivor0000000000000000002", second);
    write_record(
        &dir,
        "ses_0199reusedpid000000000000000003",
        reused,
        "0",
        "127.0.0.1:1",
    );
    std::fs::write(dir.join("ses_0199unparseable0000000000000004.json"), "{").unwrap();
    let args = ["--max-live-capsules", "3", "--spawn-allow", "orchestrator"];

    Roost::start(home.path(), registry.path(), &args).stop();
    let roost = Roost::start(home.path(), registry.path(), &args);

    let census = roost.census_line();
    assert!(
        census.starts_with(&format!(
            "mur-roost: 2 live capsules already running on this host, counted from {} (4 records \
             read in ",
            dir.display()
        )),
        "{census}"
    );
    assert!(
        census.ends_with(" ms; 1 stale and 1 unreadable not counted)"),
        "{census}"
    );

    let root = "ses_0199restartedroot00000000000005";
    let credential = roost.register_root(root);
    let (status, body) = roost.spawn_worker(&credential);
    assert_eq!(status, 403, "{body}");
    let refusal = body["error"].as_str().unwrap();
    assert!(
        refusal.starts_with("machine capsule ceiling reached: ")
            && refusal.contains("(--max-live-capsules 3), and it is already holding 3 — "),
        "{refusal}"
    );
    assert_eq!(roost.live_capsules(root), 3);

    sleepers.kill(first);
    assert_eq!(roost.live_capsules(root), 2);
    let (status, body) = roost.spawn_worker(&credential);
    assert_eq!(status, 200, "{body}");

    roost.stop();
}

// ── S3. A stale record is not counted ─────────────────────────────────────────

/// Each way a record goes stale, read from the real process table: a pid nothing holds, a pid
/// held by a process that started at another time, and the empty start token a writer that could
/// not read its own start time records.
#[test]
fn a_stale_record_is_not_counted() {
    let dir = tempfile::tempdir().unwrap();
    let mut sleepers = Sleepers::default();
    let freed = sleepers.start();
    let token = process_start_token(freed).unwrap();
    sleepers.kill(freed);
    let held = std::process::id();
    write_record(
        dir.path(),
        "ses_0199freedpid0000000000000000001",
        freed,
        &token,
        "127.0.0.1:1",
    );
    write_record(
        dir.path(),
        "ses_0199othertime000000000000000002",
        held,
        "0",
        "127.0.0.1:1",
    );
    write_record(
        dir.path(),
        "ses_0199emptytoken00000000000000003",
        held,
        "",
        "127.0.0.1:1",
    );

    let (mut inherited, report) = census::inherit(dir.path()).unwrap();

    assert_eq!(
        (report.records_read, report.counted, report.stale),
        (3, 0, 3),
        "{report:?}"
    );
    assert!(inherited.is_empty());
    assert_eq!(inherited.live(&HashMap::new()), 0);
}

// ── S6. The daemon writes nothing ─────────────────────────────────────────────

/// Name, size, mtime and mode of a directory and of every entry in it.
fn snapshot(dir: &Path) -> BTreeMap<String, (u64, i64, i64, u32)> {
    let describe = |path: &Path| {
        let metadata = std::fs::symlink_metadata(path).unwrap();
        (
            metadata.len(),
            metadata.mtime(),
            metadata.mtime_nsec(),
            metadata.permissions().mode(),
        )
    };
    let mut entries = BTreeMap::from([(".".to_string(), describe(dir))]);
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        entries.insert(
            entry.file_name().to_string_lossy().to_string(),
            describe(&entry.path()),
        );
    }
    entries
}

/// A wide running directory holding every kind of entry is exactly as it was after the daemon has
/// started and stopped: nothing re-moded, pruned, rewritten or added.
#[test]
fn the_daemon_leaves_the_running_directory_as_it_found_it() {
    let home = tempfile::tempdir().unwrap();
    let dir = running_dir(home.path());
    let registry = tempfile::tempdir().unwrap();
    let mut sleepers = Sleepers::default();
    let live = sleepers.start();
    write_live_record(&dir, "ses_0199live00000000000000000000001", live);
    write_record(
        &dir,
        "ses_0199stale0000000000000000000002",
        std::process::id(),
        "0",
        "127.0.0.1:1",
    );
    std::fs::write(dir.join("ses_0199unparseable0000000000000003.json"), "{").unwrap();
    std::fs::write(
        dir.join("ses_0199live00000000000000000000001.control"),
        "ctl1.token.mac",
    )
    .unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let murmur_mode = std::fs::metadata(home.path().join(".murmur"))
        .unwrap()
        .permissions()
        .mode();
    let before = snapshot(&dir);

    let roost = Roost::start(home.path(), registry.path(), &[]);
    assert!(
        roost.census_line().contains("1 live capsules"),
        "{:?}",
        roost.startup
    );
    roost.stop();

    assert_eq!(snapshot(&dir), before);
    assert_eq!(
        std::fs::metadata(home.path().join(".murmur"))
            .unwrap()
            .permissions()
            .mode(),
        murmur_mode
    );
}

/// A home with no `~/.murmur` still has none after the daemon has started and stopped.
#[test]
fn the_daemon_creates_no_murmur_home() {
    let home = tempfile::tempdir().unwrap();
    let registry = tempfile::tempdir().unwrap();

    let roost = Roost::start(home.path(), registry.path(), &[]);
    assert!(
        roost.startup.iter().any(|line| line.contains("listening")),
        "{:?}",
        roost.startup
    );
    roost.stop();

    assert!(!home.path().join(".murmur").exists());
}

// ── S7. Sixty-four records ────────────────────────────────────────────────────

/// The start-path cost over 64 records: one read and one local process reading each, no
/// connection to any door. Every record names a listener that would accept a probe, and none
/// arrives.
#[test]
fn sixty_four_records_are_counted_without_touching_a_door() {
    let dir = tempfile::tempdir().unwrap();
    let door = TcpListener::bind("127.0.0.1:0").unwrap();
    door.set_nonblocking(true).unwrap();
    let url = door.local_addr().unwrap().to_string();
    let mut sleepers = Sleepers::default();
    let mut live_pids: Vec<u32> = (0..4).map(|_| sleepers.start()).collect();
    live_pids.push(std::process::id());
    for index in 0..48 {
        let pid = live_pids[index % live_pids.len()];
        let token = process_start_token(pid).unwrap();
        write_record(
            dir.path(),
            &format!("ses_live{index:027}"),
            pid,
            &token,
            &url,
        );
    }
    for index in 0..16 {
        write_record(
            dir.path(),
            &format!("ses_stale{index:026}"),
            std::process::id(),
            "0",
            &url,
        );
    }

    let started = Instant::now();
    let (mut inherited, report) = census::inherit(dir.path()).unwrap();
    let census_took = started.elapsed();
    let started = Instant::now();
    let live = inherited.live(&HashMap::new());
    let recheck_took = started.elapsed();

    eprintln!(
        "census of 64 records: {census_took:?} (reported {:?}); re-check of {} held: \
         {recheck_took:?}",
        report.elapsed,
        inherited.len()
    );
    assert_eq!(
        (report.records_read, report.counted, report.stale),
        (64, 48, 16)
    );
    assert_eq!(live, 48);
    assert_eq!(
        door.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert!(census_took.as_secs() < 1, "{census_took:?}");
    assert!(recheck_took.as_secs() < 1, "{recheck_took:?}");
}

// ── S8. An unusable directory stops the daemon before it listens ──────────────

fn run_to_exit(command: &mut Command) -> (Option<i32>, String) {
    let output = command.output().unwrap();
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

#[test]
fn a_running_directory_that_cannot_be_listed_stops_the_daemon() {
    let home = tempfile::tempdir().unwrap();
    let registry = tempfile::tempdir().unwrap();
    std::fs::create_dir(home.path().join(".murmur")).unwrap();
    let dir = running_dir(home.path());
    std::fs::write(&dir, "").unwrap();

    let (code, stderr) = run_to_exit(
        Command::new(env!("CARGO_BIN_EXE_mur-roost"))
            .env("HOME", home.path())
            .args(["--port", &free_port().to_string()])
            .args(["--registry-path", registry.path().to_str().unwrap()]),
    );

    assert_eq!(code, Some(1), "{stderr}");
    assert!(
        stderr.starts_with(&format!("{CENSUS_PREFIX}{}: ", dir.display())),
        "{stderr}"
    );
    assert!(!stderr.contains("listening"), "{stderr}");
}

#[test]
fn an_unset_home_stops_the_daemon() {
    let registry = tempfile::tempdir().unwrap();

    let (code, stderr) = run_to_exit(
        Command::new(env!("CARGO_BIN_EXE_mur-roost"))
            .env_remove("HOME")
            .args(["--port", &free_port().to_string()])
            .args(["--registry-path", registry.path().to_str().unwrap()]),
    );

    assert_eq!(code, Some(1), "{stderr}");
    assert!(
        stderr.starts_with(&format!(
            "{CENSUS_PREFIX}the home directory could not be resolved: HOME is not set in the \
             environment"
        )),
        "{stderr}"
    );
    assert!(!stderr.contains("listening"), "{stderr}");
}

/// `--version` answers before the census, so the installer's smoke run works on a host whose
/// running directory is unusable.
#[test]
fn version_exits_before_the_census() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir(home.path().join(".murmur")).unwrap();
    std::fs::write(running_dir(home.path()), "").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_mur-roost"))
        .env("HOME", home.path())
        .arg("--version")
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
}

// ── S9. An empty host ─────────────────────────────────────────────────────────

/// The census line comes first and reads zero on an empty host; the two lines after it are the
/// ones the daemon has always printed.
#[test]
fn an_empty_host_reports_a_zero_census_ahead_of_the_listening_line() {
    let home = tempfile::tempdir().unwrap();
    let registry = tempfile::tempdir().unwrap();

    let roost = Roost::start(home.path(), registry.path(), &["--max-live-capsules", "7"]);

    assert_eq!(roost.startup.len(), 3, "{:?}", roost.startup);
    let census = &roost.startup[0];
    let elapsed = census
        .strip_prefix(&format!(
            "mur-roost: 0 live capsules already running on this host, counted from {} (0 records \
             read in ",
            running_dir(home.path()).display()
        ))
        .and_then(|rest| rest.strip_suffix(" ms; 0 stale and 0 unreadable not counted)"))
        .unwrap_or_else(|| panic!("{census}"));
    elapsed.parse::<f64>().unwrap();
    assert_eq!(
        roost.startup[1],
        format!("mur-roost: listening on 127.0.0.1:{}", roost.port)
    );
    let ceiling = roost.startup[2]
        .strip_prefix("mur-roost: machine ceiling ")
        .and_then(|rest| rest.strip_suffix(" live capsules (--max-live-capsules)"))
        .unwrap_or_else(|| panic!("{}", roost.startup[2]));
    assert_eq!(ceiling.parse::<u32>().unwrap(), 7);
    roost.stop();
}
