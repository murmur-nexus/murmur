//! A headless Chrome started from a shell command inside a capsule, driven through the real `mur`
//! binary on a scripted model with `transport: http`.
//!
//! The browser is the host's: `MUR_TEST_CHROME`, else the newest Playwright
//! `chrome-headless-shell` under the real `HOME`, else `chrome-headless-shell` or `chromium` on
//! `PATH`. A host with none skips with a `[SKIP-HOST]` line. Scratch directories live under
//! `CARGO_TARGET_TMPDIR`, never `/tmp`.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Child, ExitStatus, Stdio},
    sync::{mpsc, Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use assert_cmd::Command;
use serde_json::{json, Value};
use tempfile::TempDir;

const DRIVER: &str = "local/murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.0";

/// The page's marker, on a line of its own, which `--dump-dom` prints back when the page loaded.
const MARKER: &str = "murmur-chrome-marker";

/// What Chrome prints when its GPU process dies at start.
const GPU_EXITED: &str = "GPU process exited unexpectedly";

/// How long the whole test may run: the fixture and every tier's `mur run`. Below the 300 s kill
/// of nextest's `ci` profile (`.config/nextest.toml`), so a run that overruns is reported by the
/// test, with the phase it stood in, and not by a `TMT` that prints nothing.
const TEST_BUDGET: Duration = Duration::from_secs(270);

/// How long one `mur run` may take, within what is left of [`TEST_BUDGET`].
const RUN_LIMIT: Duration = Duration::from_secs(120);

/// How long a `mur run` past its deadline has to end its session on `SIGTERM` before `SIGKILL`.
const TERM_GRACE: Duration = Duration::from_secs(5);

/// How often a running `mur run` is checked for its exit, its pipes' end and its deadline.
const POLL: Duration = Duration::from_millis(50);

/// The flags Playwright starts Chrome with that bear on containment, and `--dump-dom`, which
/// prints the loaded page and exits.
const CHROME_FLAGS: &[&str] = &[
    "--headless",
    "--no-sandbox",
    "--disable-dev-shm-usage",
    "--disable-gpu",
    "--no-first-run",
    "--no-default-browser-check",
    "--dump-dom",
];

/// The host's headless Chrome, in the documented search order, or `None`.
fn find_chrome() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("MUR_TEST_CHROME") {
        return Some(PathBuf::from(path));
    }
    if let Some(home) = std::env::var_os("HOME") {
        let cache = Path::new(&home).join(".cache/ms-playwright");
        let mut found: Vec<PathBuf> = fs::read_dir(&cache)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("chromium_headless_shell-")
            })
            .map(|entry| {
                entry
                    .path()
                    .join("chrome-headless-shell-linux64/chrome-headless-shell")
            })
            .filter(|path| path.is_file())
            .collect();
        // `chromium_headless_shell-<build>`: the highest build is the newest.
        found.sort_by_key(|path| {
            path.parent()
                .and_then(Path::parent)
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
                .and_then(|name| name.rsplit('-').next())
                .and_then(|build| build.parse::<u64>().ok())
                .unwrap_or(0)
        });
        if let Some(newest) = found.pop() {
            return Some(newest);
        }
    }
    let path = std::env::var_os("PATH")?;
    for name in ["chrome-headless-shell", "chromium"] {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// One scripted call: the call id, the tool and its input.
struct Call {
    id: &'static str,
    tool: &'static str,
    input: Value,
}

struct Fixture {
    _root: TempDir,
    home: PathBuf,
    project: PathBuf,
    /// `PATH` for `mur`: the browser's folder first, so `shell.allow` names it.
    path: std::ffi::OsString,
    /// When [`TEST_BUDGET`] runs out.
    deadline: Instant,
}

impl Fixture {
    fn new(chrome: &Path) -> Self {
        let deadline = Instant::now() + TEST_BUDGET;
        let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let base = root.path().canonicalize().unwrap();
        let home = base.join("home");
        let project = base.join("project");
        let artifacts = base.join("artifacts");
        for dir in [&home, &project, &artifacts] {
            fs::create_dir_all(dir).unwrap();
        }
        let mut dirs = vec![chrome.parent().unwrap().to_path_buf()];
        dirs.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        let fixture = Self {
            _root: root,
            home,
            project,
            path: std::env::join_paths(dirs).unwrap(),
            deadline,
        };
        let driver = common::create_driver_artifact(
            &artifacts,
            DRIVER,
            DRIVER_VERSION,
            &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
        );
        fixture.write_manifest("http://127.0.0.1:9", "scoped", "");
        fixture
            .mur()
            .args(["install", driver.to_str().unwrap(), "--no-precompile"])
            .timeout(deadline.saturating_duration_since(Instant::now()))
            .assert()
            .success();
        fixture
    }

    /// The capsule at `containment` with `capabilities` lines, its driver pointed at `endpoint`.
    fn write_manifest(&self, endpoint: &str, containment: &str, capabilities: &str) -> PathBuf {
        let manifest = self.project.join("murmur.yaml");
        fs::write(
            &manifest,
            format!(
                "name: browser-in-capsule\nversion: 0.0.1\nartifacts:\n  - name: {DRIVER}\n    \
                 version: {DRIVER_VERSION}\n    runtime: driver\n    gateway:\n      endpoint: \
                 {endpoint}\n      api_key: test-key\ncapabilities:\n  containment: \
                 {containment}\n  network:\n    allow:\n      - {endpoint}\n{capabilities}\
                 inference:\n  transport: http\n  model: test-model\n  driver:\n    artifact: \
                 {DRIVER}\n"
            ),
        )
        .unwrap();
        manifest
    }

    fn mur(&self) -> Command {
        let mut command = Command::cargo_bin("mur").unwrap();
        command
            .env("HOME", &self.home)
            .env("PATH", &self.path)
            .current_dir(&self.project);
        command
    }

    /// One `mur run` of a capsule at `containment` with `capabilities` lines, in `workdir`, the
    /// model making each call in order. `marked` is the label of the command that writes
    /// `<marked>.started` and `<marked>.ended` in `workdir` (see [`chrome_command`]).
    ///
    /// A run still going at its deadline is stopped and the test fails with a [`Phase`] report.
    fn run(
        &self,
        containment: &str,
        capabilities: &str,
        workdir: &Path,
        calls: Vec<Call>,
        marked: &str,
    ) -> Run {
        let total = calls.len() + 1;
        let arrivals: Arc<Mutex<Vec<Instant>>> = Arc::default();
        let mut next = 0usize;
        let server = common::ScriptedServer::start_answering(total, {
            let arrivals = Arc::clone(&arrivals);
            move |_| {
                // Before answering: the server records a request only once it is answered.
                arrivals.lock().unwrap().push(Instant::now());
                let index = next;
                next += 1;
                match calls.get(index) {
                    Some(call) => common::tool_use_response(call.id, call.tool, call.input.clone()),
                    None => end_turn(),
                }
            }
        });
        let manifest = self.write_manifest(&server.endpoint, containment, capabilities);
        let started = Instant::now();
        let started_wall = SystemTime::now();
        let deadline = (started + RUN_LIMIT).min(self.deadline);
        let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("mur"))
            .env("HOME", &self.home)
            .env("PATH", &self.path)
            .current_dir(&self.project)
            .args(["run", "--manifest", manifest.to_str().unwrap()])
            .args(["--workdir", workdir.to_str().unwrap()])
            .args(["--task", "Run each call once.", "--verbose"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let stdout = Captured::read(child.stdout.take().unwrap(), done_tx.clone());
        let stderr = Captured::read(child.stderr.take().unwrap(), done_tx);
        let mut exited = false;
        let mut pipes_open = 2;
        while !exited || pipes_open > 0 {
            if Instant::now() >= deadline {
                let timeline = Timeline::read(
                    started,
                    started_wall,
                    &arrivals.lock().unwrap(),
                    workdir,
                    marked,
                    exited,
                );
                let processes = process_report(child.id());
                // A reaped child's pid may already name another process.
                if !exited {
                    stop(&mut child);
                }
                panic!(
                    "{containment}: `mur run` still going after {:.1} s\nphase: {}\n{}\n\
                     {marked}.err: {}\n{marked}.out: {}\nmur stderr, through its SIGTERM:\n{}\n\
                     mur stdout, through its SIGTERM:\n{}\nprocesses under mur (pid {}), before \
                     the SIGTERM:\n{processes}",
                    started.elapsed().as_secs_f64(),
                    timeline.phase().name(),
                    timeline.describe(),
                    file_or_absent(&workdir.join(format!("{marked}.err"))),
                    file_or_absent(&workdir.join(format!("{marked}.out"))),
                    stderr.text(),
                    stdout.text(),
                    child.id(),
                );
            }
            if !exited {
                exited = child.try_wait().unwrap().is_some();
            }
            match done_rx.recv_timeout(POLL) {
                Ok(()) => pipes_open -= 1,
                // Both readers are done; only the exit is left to poll for.
                Err(mpsc::RecvTimeoutError::Disconnected) => std::thread::sleep(POLL),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
        // Returns the status `try_wait` reaped.
        let status = child.wait().unwrap();
        let timeline = Timeline::read(
            started,
            started_wall,
            &arrivals.lock().unwrap(),
            workdir,
            marked,
            true,
        );
        let run = Run {
            status,
            stdout: stdout.text(),
            stderr: stderr.text(),
            requests: server.requests(),
        };
        eprintln!(
            "{containment}: `mur run` took {:.2} s\n{}",
            started.elapsed().as_secs_f64(),
            timeline.describe()
        );
        assert!(
            run.status.success() && run.stdout.contains("status:  ok"),
            "the run must finish ok\nstdout: {}\nstderr: {}",
            run.stdout,
            run.stderr
        );
        run
    }
}

/// Everything a child writes to one of its pipes, read on a thread so it can be shown before the
/// child ends.
#[derive(Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Captured {
    /// Reads `pipe` to its end, then sends on `done`.
    fn read(mut pipe: impl Read + Send + 'static, done: mpsc::Sender<()>) -> Self {
        let captured = Self::default();
        let buffer = Arc::clone(&captured.0);
        std::thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            while let Ok(read @ 1..) = pipe.read(&mut chunk) {
                buffer.lock().unwrap().extend_from_slice(&chunk[..read]);
            }
            let _ = done.send(());
        });
        captured
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

/// `SIGTERM`, on which `mur` ends its session and the processes in it, then `SIGKILL` if it is
/// still running after [`TERM_GRACE`].
fn stop(child: &mut Child) {
    // Only an unreaped child: its pid cannot yet name another process.
    common::signal(child.id(), libc::SIGTERM);
    let grace = Instant::now() + TERM_GRACE;
    while Instant::now() < grace {
        if child.try_wait().unwrap().is_some() {
            return;
        }
        std::thread::sleep(POLL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Where a `mur run` that has not finished stands, from the model requests that arrived and the
/// marked command's marks.
enum Phase {
    Launch,
    Dispatch,
    Chrome,
    AfterChrome,
    Teardown,
    PipesHeld,
}

impl Phase {
    fn name(&self) -> &'static str {
        match self {
            Phase::Launch => "capsule launch: no model request yet",
            Phase::Dispatch => {
                "tool dispatch: first model request answered, Chrome command not started"
            }
            Phase::Chrome => "Chrome running: Chrome command started, no end mark",
            Phase::AfterChrome => {
                "runtime after Chrome exited: end mark written, no second model request"
            }
            Phase::Teardown => "teardown: second model request answered, mur still running",
            Phase::PipesHeld => "after mur exited: its stdout or stderr is still held open",
        }
    }
}

/// The moments a `mur run` passed, in seconds since it started.
struct Timeline {
    arrivals: Vec<f64>,
    started_mark: Option<f64>,
    ended_mark: Option<f64>,
    exited: bool,
    elapsed: f64,
}

impl Timeline {
    fn read(
        started: Instant,
        started_wall: SystemTime,
        arrivals: &[Instant],
        workdir: &Path,
        marked: &str,
        exited: bool,
    ) -> Self {
        let run_epoch = started_wall
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        // `$EPOCHREALTIME` is seconds since the epoch with a fractional part.
        let mark = |suffix: &str| {
            fs::read_to_string(workdir.join(format!("{marked}.{suffix}")))
                .ok()
                .and_then(|text| text.trim().parse::<f64>().ok())
                .map(|epoch| epoch - run_epoch)
        };
        Self {
            arrivals: arrivals
                .iter()
                .map(|at| at.duration_since(started).as_secs_f64())
                .collect(),
            started_mark: mark("started"),
            ended_mark: mark("ended"),
            exited,
            elapsed: started.elapsed().as_secs_f64(),
        }
    }

    fn phase(&self) -> Phase {
        match (
            self.arrivals.len(),
            self.started_mark,
            self.ended_mark,
            self.exited,
        ) {
            (_, _, _, true) => Phase::PipesHeld,
            (0, _, _, _) => Phase::Launch,
            (1, None, _, _) => Phase::Dispatch,
            (1, Some(_), None, _) => Phase::Chrome,
            (1, Some(_), Some(_), _) => Phase::AfterChrome,
            _ => Phase::Teardown,
        }
    }

    /// The arrival count and each phase's time; the last one reached runs to now.
    fn describe(&self) -> String {
        let at = |moment: Option<f64>| moment.map_or("-".to_string(), |s| format!("{s:.2} s"));
        let span = |from: Option<f64>, to: Option<f64>| match (from, to) {
            (Some(from), Some(to)) => format!("{:.2} s", to - from),
            (Some(from), None) => format!("{:.2} s, not ended", self.elapsed - from),
            _ => "-".to_string(),
        };
        let first = self.arrivals.first().copied();
        let second = self.arrivals.get(1).copied();
        let end = (self.exited).then_some(self.elapsed);
        format!(
            "model requests arrived: {} at {:?}\nmarks: started {} ended {}\nphases: launch {} | \
             dispatch {} | Chrome {} | after Chrome {} | teardown {}",
            self.arrivals.len(),
            self.arrivals
                .iter()
                .map(|s| format!("{s:.2}"))
                .collect::<Vec<_>>(),
            at(self.started_mark),
            at(self.ended_mark),
            span(Some(0.0), first),
            span(first, self.started_mark),
            span(self.started_mark, self.ended_mark),
            span(self.ended_mark, second),
            span(second, end),
        )
    }
}

fn file_or_absent(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|error| format!("<absent: {error}>"))
}

/// The processes descended from `root`, one per line with its state and command line, each
/// thread's state, wait channel and current system call, and the limits of any Chrome process.
/// `stat`, `cmdline` and `limits` are readable for a non-dumpable process too; `wchan` and
/// `syscall` are not, and read as `0` or an error there.
fn process_report(root: u32) -> String {
    let mut parents = Vec::new();
    for entry in fs::read_dir("/proc").unwrap().flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        if let Some((_, _, ppid)) = proc_stat(&format!("/proc/{pid}/stat")) {
            parents.push((pid, ppid));
        }
    }
    let mut report = String::new();
    let mut stack = vec![(root, 0usize)];
    while let Some((pid, depth)) = stack.pop() {
        let (comm, state, _) = proc_stat(&format!("/proc/{pid}/stat")).unwrap_or_default();
        let cmdline = fs::read(format!("/proc/{pid}/cmdline"))
            .map(|raw| String::from_utf8_lossy(&raw).replace('\0', " "))
            .unwrap_or_default();
        let indent = "  ".repeat(depth);
        report.push_str(&format!(
            "{indent}{pid} {state} ({comm}) {}\n",
            cmdline.trim()
        ));
        for task in fs::read_dir(format!("/proc/{pid}/task"))
            .into_iter()
            .flatten()
            .flatten()
        {
            let tid = task.file_name().to_string_lossy().into_owned();
            let (comm, state, _) =
                proc_stat(&format!("/proc/{pid}/task/{tid}/stat")).unwrap_or_default();
            let read = |file: &str| {
                fs::read_to_string(format!("/proc/{pid}/task/{tid}/{file}"))
                    .map(|text| text.trim().to_string())
                    .unwrap_or_else(|error| format!("<{error}>"))
            };
            report.push_str(&format!(
                "{indent}    task {tid} {state} ({comm}) wchan={} syscall={}\n",
                read("wchan"),
                read("syscall"),
            ));
        }
        if cmdline.contains("chrome") || comm.contains("chrome") {
            report.push_str(&format!(
                "{indent}    limits:\n{}",
                file_or_absent(Path::new(&format!("/proc/{pid}/limits")))
            ));
        }
        for &(child, _) in parents.iter().filter(|(_, ppid)| *ppid == pid) {
            stack.push((child, depth + 1));
        }
    }
    report
}

/// `comm`, state and parent pid from a `/proc/<pid>/stat` file.
fn proc_stat(path: &str) -> Option<(String, String, u32)> {
    let text = fs::read_to_string(path).ok()?;
    // `comm` is in parentheses and may hold spaces and parentheses itself.
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    let mut rest = text[close + 1..].split_whitespace();
    let state = rest.next()?.to_string();
    let ppid = rest.next()?.parse().ok()?;
    Some((text[open + 1..close].to_string(), state, ppid))
}

struct Run {
    status: ExitStatus,
    stdout: String,
    stderr: String,
    requests: Vec<Value>,
}

impl Run {
    fn result_text(&self, id: &str) -> String {
        common::tool_result_text(&self.requests, id)
            .unwrap_or_else(|| panic!("no tool result for {id}; stderr: {}", self.stderr))
    }
}

fn end_turn() -> String {
    json!({
        "id": "msg_end",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": "done"}],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {"input_tokens": 1, "output_tokens": 1},
    })
    .to_string()
}

/// The system library folder. Chrome loads its NSS modules from it with `dlopen`, so they are
/// not in the `DT_NEEDED` closure the runtime grants from the binary, and a capsule that runs
/// native Node addons grants it too. It puts `libpci.so.3` in reach of every command.
const LIBRARY_FOLDER: &str = "/usr/lib/x86_64-linux-gnu";

/// What `libpci`, which Chrome's GPU process loads with `dlopen`, reads: the PCI bus and the
/// device folders its entries link to. Refused, `libpci` exits the GPU process with status 1.
const PCI_SYSFS: &[&str] = &["/sys/bus/pci", "/sys/devices"];

/// `capabilities:` lines granting `bash` and `chrome` (by file name) to shell commands, with the
/// grants headless Chrome reads outside the work folder, the library folder, and `resources` V8
/// runs under.
fn chrome_capabilities(chrome: &Path) -> String {
    let name = chrome.file_name().unwrap().to_str().unwrap();
    let folder = chrome.canonicalize().unwrap();
    let folder = folder.parent().unwrap().to_path_buf();
    let mut dirs: Vec<(PathBuf, bool)> = vec![(folder, true)];
    for dir in ["/proc", "/etc/fonts", "/usr/share/fonts"] {
        dirs.push((PathBuf::from(dir), true));
    }
    dirs.push((PathBuf::from(LIBRARY_FOLDER), false));
    for dir in PCI_SYSFS {
        dirs.push((PathBuf::from(dir), true));
    }
    let mut yaml = format!(
        "  shell:\n    allow:\n      - bash\n      - {name}\n    interpreter_runtime:\n      - \
         binary: {name}\n        dirs:\n"
    );
    // A host without one of these paths (another architecture's library folder, a container
    // without `/sys/bus/pci`) has nothing there to grant.
    for (path, list_dir) in dirs.into_iter().filter(|(path, _)| path.exists()) {
        yaml.push_str(&format!(
            "          - path: {}\n            list_dir: {list_dir}\n",
            path.display()
        ));
    }
    // V8 reserves tens of GiB of address space per process; the cgroup caps what it uses.
    yaml.push_str(
        "  resources:\n    memory_bytes: 274877906944\n    cgroup_pids_max: 1024\n    \
         cgroup_memory_bytes: 4294967296\n",
    );
    yaml
}

/// A work folder holding the page Chrome loads.
fn workdir_with_page(fixture: &Fixture, label: &str) -> PathBuf {
    let workdir = fixture.project.join(format!("work-{label}"));
    let _ = fs::remove_dir_all(&workdir);
    fs::create_dir_all(&workdir).unwrap();
    fs::write(
        workdir.join("page.html"),
        format!("<html><body><p id=\"m\">\n{MARKER}\n</p></body></html>\n"),
    )
    .unwrap();
    workdir
}

/// The `bash` command that starts `chrome` on the work folder's page, its output in
/// `<label>.out` and `<label>.err` in the work folder. It writes `$EPOCHREALTIME` to
/// `<label>.started` before Chrome and to `<label>.ended` after it, with builtins only: a
/// capsule's commands exec nothing outside `shell.allow`.
fn chrome_command(chrome: &Path, workdir: &Path, label: &str) -> String {
    let name = chrome.file_name().unwrap().to_str().unwrap();
    format!(
        "printf '%s\\n' \"$EPOCHREALTIME\" >{label}.started; {name} {} file://{}/page.html \
         >{label}.out 2>{label}.err; status=$?; printf '%s\\n' \"$EPOCHREALTIME\" >{label}.ended; \
         echo chrome-exit=$status",
        CHROME_FLAGS.join(" "),
        workdir.display()
    )
}

/// Chrome started from a `bash` shell command in a capsule keeps its GPU process and loads a page
/// from the work folder, at every tier the host reaches.
#[test]
fn chrome_from_a_shell_command_loads_a_page_in_a_capsule() {
    let name = "chrome_from_a_shell_command_loads_a_page_in_a_capsule";
    if common::skip_without_host_support(name) {
        return;
    }
    let Some(chrome) = find_chrome() else {
        eprintln!(
            "[SKIP-HOST] {name}: no chrome-headless-shell or chromium found (set MUR_TEST_CHROME)"
        );
        return;
    };
    let fixture = Fixture::new(&chrome);
    for tier in common::tiers(name) {
        let workdir = workdir_with_page(&fixture, tier);
        let run = fixture.run(
            tier,
            &chrome_capabilities(&chrome),
            &workdir,
            vec![Call {
                id: "chrome",
                tool: "bash",
                input: json!({"command": chrome_command(&chrome, &workdir, "chrome")}),
            }],
            "chrome",
        );
        let result = run.result_text("chrome");
        let out = fs::read_to_string(workdir.join("chrome.out")).unwrap_or_default();
        let err = fs::read_to_string(workdir.join("chrome.err")).unwrap_or_default();
        assert!(
            result.lines().any(|line| line == "chrome-exit=0"),
            "{tier}: Chrome must exit 0\nresult: {result}\nstderr: {err}"
        );
        assert!(
            out.lines().any(|line| line == MARKER),
            "{tier}: the page's marker line must be printed\nstdout: {out}\nstderr: {err}"
        );
        assert!(
            !err.contains(GPU_EXITED),
            "{tier}: the GPU process must stay up\nstderr: {err}"
        );
    }
}

/// A hung run's report names the phase its arrivals, marks and exit place it in.
#[test]
fn the_phase_report_names_each_phase() {
    let timeline =
        |arrivals: &[f64], started: Option<f64>, ended: Option<f64>, exited: bool| Timeline {
            arrivals: arrivals.to_vec(),
            started_mark: started,
            ended_mark: ended,
            exited,
            elapsed: 10.0,
        };
    let cases = [
        (
            timeline(&[], None, None, false),
            "capsule launch: no model request yet",
        ),
        (
            timeline(&[1.0], None, None, false),
            "tool dispatch: first model request answered, Chrome command not started",
        ),
        (
            timeline(&[1.0], Some(2.0), None, false),
            "Chrome running: Chrome command started, no end mark",
        ),
        (
            timeline(&[1.0], Some(2.0), Some(4.0), false),
            "runtime after Chrome exited: end mark written, no second model request",
        ),
        (
            timeline(&[1.0, 5.0], Some(2.0), Some(4.0), false),
            "teardown: second model request answered, mur still running",
        ),
        (
            timeline(&[1.0, 5.0], Some(2.0), Some(4.0), true),
            "after mur exited: its stdout or stderr is still held open",
        ),
        (
            timeline(&[], None, None, true),
            "after mur exited: its stdout or stderr is still held open",
        ),
    ];
    for (timeline, expected) in cases {
        assert_eq!(timeline.phase().name(), expected, "{}", timeline.describe());
    }
}
