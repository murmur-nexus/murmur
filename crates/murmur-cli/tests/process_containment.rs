//! A `transport: process` harness runs under the capsule's containment class, through the same
//! enforcement path as a shell command, and is granted beyond it only what its driver declares
//! and the manifest also allows.
//!
//! Every test publishes the fixture process driver (the default build, or a `harness-needs` build)
//! and points `inference.command` at the fixture fake harness with the profile baked in. `HOME` and
//! the project are scratch folders outside `/tmp`, which a contained class replaces with a private
//! one, so a granted folder, the harness and the sentinel a harness must not read are at their host
//! paths. Every test
//! that launches a contained harness skips on a host with no network namespace.

mod common;

use std::fs;
use std::io::Read;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use assert_cmd::cargo::CommandCargoExt;
use serde_json::Value;
use tempfile::TempDir;

const DRIVER: &str = "local/fixture-process-driver";
const DRIVER_VERSION: &str = "0.1.0";
const TOOL: &str = "echo-tool";

/// Which fixture driver build a capsule runs.
#[derive(Clone, Copy)]
enum Driver {
    /// Declares no harness needs.
    Plain,
    /// Declares `~/fake-harness-sessions` (read-write) and `example.invalid:443`.
    Needs,
    /// Declares `~/fixture-harness-parent/sessions` (read-write).
    NeedsParent,
}

impl Driver {
    fn wasm(self) -> PathBuf {
        common::fixture_path(match self {
            Self::Plain => "process-driver/tool/process-driver.wasm",
            Self::Needs => "process-driver/tool/process-driver-needs.wasm",
            Self::NeedsParent => "process-driver/tool/process-driver-needs-parent.wasm",
        })
    }
}

struct Built {
    driver: Driver,
    profile: &'static str,
    /// `capabilities.containment`, or none declared.
    containment: Option<&'static str>,
    shell: Vec<&'static str>,
    network: Vec<&'static str>,
    harness_paths: Vec<(&'static str, &'static str)>,
    tool: bool,
    exports: String,
    home: Option<TempDir>,
}

impl Built {
    fn new(profile: &'static str) -> Self {
        Self {
            driver: Driver::Plain,
            profile,
            containment: Some("scoped"),
            shell: Vec::new(),
            network: Vec::new(),
            harness_paths: Vec::new(),
            tool: false,
            exports: String::new(),
            home: None,
        }
    }

    /// Run in `home`, which a test has already put files in; a fresh one off `/tmp` otherwise.
    fn home(mut self, home: TempDir) -> Self {
        self.home = Some(home);
        self
    }

    fn driver(mut self, driver: Driver) -> Self {
        self.driver = driver;
        self
    }

    fn containment(mut self, class: &'static str) -> Self {
        self.containment = Some(class);
        self
    }

    /// No declared floor, which `--unconfined` requires.
    fn no_floor(mut self) -> Self {
        self.containment = None;
        self
    }

    fn shell(mut self, programs: &[&'static str]) -> Self {
        self.shell = programs.to_vec();
        self
    }

    fn network(mut self, allow: &[&'static str]) -> Self {
        self.network = allow.to_vec();
        self
    }

    fn harness_path(mut self, path: &'static str, access: &'static str) -> Self {
        self.harness_paths.push((path, access));
        self
    }

    fn with_tool(mut self) -> Self {
        self.tool = true;
        self
    }

    /// One `export NAME='value'` line baked into the harness ahead of its body.
    fn export(mut self, name: &str, value: &str) -> Self {
        self.exports.push_str(&format!("export {name}='{value}'\n"));
        self
    }

    fn build(self) -> Capsule {
        let home = self.home.unwrap_or_else(common::scratch_off_tmp);
        let artifacts = TempDir::new().unwrap();
        // Off `/tmp` too: a `sealed` composed root mounts the session's own temporary folder at
        // `/tmp`, so a harness binary under the host's `/tmp` has no folder to be bound at.
        let project = common::scratch_off_tmp();

        let driver = common::create_driver_artifact_with_auth(
            artifacts.path(),
            DRIVER,
            DRIVER_VERSION,
            &self.driver.wasm(),
            "",
        );
        common::publish_local(&home, &driver).success();
        let mut entries =
            format!("  - name: {DRIVER}\n    version: {DRIVER_VERSION}\n    runtime: driver\n");
        if self.tool {
            let tool = common::create_tool_artifact(
                artifacts.path(),
                TOOL,
                DRIVER_VERSION,
                &common::fixture_path("run/components/echo-tool.wasm"),
            );
            common::publish_local(&home, &tool).success();
            entries.push_str(&format!(
                "  - name: local/{TOOL}\n    version: {DRIVER_VERSION}\n    runtime: tool\n"
            ));
        }

        let harness = common::write_fake_harness(
            &project.path().join("harness.sh"),
            &format!(
                "export FIXTURE_HARNESS_PROFILE={}\n{}",
                self.profile, self.exports
            ),
        );

        // The fixture driver requires FIXTURE_HARNESS_PROFILE in the harness's environment.
        let mut capabilities = String::from("capabilities:\n");
        if let Some(class) = self.containment {
            capabilities.push_str(&format!("  containment: {class}\n"));
        }
        capabilities.push_str("  env:\n    allow: [HOME, PATH, FIXTURE_HARNESS_PROFILE]\n");
        if !self.shell.is_empty() {
            capabilities.push_str(&format!(
                "  shell:\n    allow: [{}]\n",
                self.shell.join(", ")
            ));
        }
        if !self.network.is_empty() {
            capabilities.push_str(&format!(
                "  network:\n    allow: [{}]\n",
                self.network.join(", ")
            ));
        }
        if !self.harness_paths.is_empty() {
            capabilities.push_str("  harness:\n    paths:\n");
            for (path, access) in &self.harness_paths {
                capabilities.push_str(&format!("      - {{ path: {path}, access: {access} }}\n"));
            }
        }
        let manifest = project.path().join("murmur.yaml");
        fs::write(
            &manifest,
            format!(
                "name: process-containment\nversion: 0.1.0\nartifacts:\n{entries}{capabilities}\
                 inference:\n  transport: process\n  driver:\n    artifact: {DRIVER}\n  \
                 command: {}\n",
                harness.display()
            ),
        )
        .unwrap();
        Capsule {
            home,
            project,
            _artifacts: artifacts,
            manifest,
        }
    }
}

struct Capsule {
    home: TempDir,
    project: TempDir,
    _artifacts: TempDir,
    manifest: PathBuf,
}

/// One finished `mur` invocation.
struct Run {
    output: Output,
    text: String,
}

impl Run {
    fn succeeded(self) -> Self {
        assert!(self.output.status.success(), "{}", self.text);
        self
    }

    fn records_dir(&self) -> PathBuf {
        common::parse_records_from_stdout(&self.text)
    }

    fn workdir(&self) -> PathBuf {
        common::parse_workdir_from_stdout(&self.text)
    }

    fn events(&self) -> Vec<Value> {
        common::read_whole_trace(&self.records_dir().join("trace.jsonl"))
    }

    fn one(&self, event_type: &str) -> Value {
        let found: Vec<Value> = self
            .events()
            .into_iter()
            .filter(|event| event["event_type"] == event_type)
            .collect();
        assert_eq!(found.len(), 1, "one {event_type}: {found:#?}");
        found.into_iter().next().unwrap()
    }

    fn result(&self) -> String {
        fs::read_to_string(self.records_dir().join("out").join("result.txt")).unwrap_or_default()
    }
}

impl Capsule {
    fn mur(&self) -> Command {
        let mut command = Command::cargo_bin("mur").unwrap();
        command
            .env_clear()
            .env("HOME", self.home.path())
            .env(
                "PATH",
                std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_string()),
            )
            .current_dir(self.project.path());
        command
    }

    fn invoke(&self, args: &[&str]) -> Run {
        let output = self.mur().args(args).output().unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        Run { output, text }
    }

    fn run(&self, extra: &[&str]) -> Run {
        let manifest = self.manifest.to_str().unwrap().to_string();
        let mut args = vec![
            "run",
            "--manifest",
            manifest.as_str(),
            "--task",
            "do the thing",
            "--verbose",
        ];
        args.extend_from_slice(extra);
        self.invoke(&args)
    }

    fn explain_json(&self) -> Value {
        let run = self.invoke(&[
            "run",
            "--manifest",
            self.manifest.to_str().unwrap(),
            "--explain-scope",
            "--json",
        ]);
        assert!(run.output.status.success(), "{}", run.text);
        serde_json::from_slice(&run.output.stdout).unwrap_or_else(|error| {
            panic!("--explain-scope --json is not JSON ({error}): {}", run.text)
        })
    }

    fn explain_text(&self) -> String {
        let run = self.invoke(&[
            "run",
            "--manifest",
            self.manifest.to_str().unwrap(),
            "--explain-scope",
        ]);
        assert!(run.output.status.success(), "{}", run.text);
        run.text
    }

    fn home(&self) -> &Path {
        self.home.path()
    }
}

/// `readlink /proc/self/ns/<name>` of the test process.
fn own_namespace(name: &str) -> String {
    fs::read_link(format!("/proc/self/ns/{name}"))
        .unwrap()
        .display()
        .to_string()
}

/// The `<key> <value>` lines of the `confinement` profile's file, by key.
fn confinement(workdir: &Path) -> std::collections::BTreeMap<String, String> {
    let text = fs::read_to_string(workdir.join("confinement.txt"))
        .unwrap_or_else(|error| panic!("no confinement.txt under {workdir:?}: {error}"));
    text.lines()
        .filter_map(|line| {
            let (key, value) = line.split_once([':', ' '])?;
            Some((key.to_string(), value.trim().to_string()))
        })
        .collect()
}

fn skip(test: &str) -> bool {
    capsule_runtime::skip_without_egress_namespace(test)
}

/// Asserts what the `confinement` profile read: seccomp filtering on, no_new_privs set, and the
/// harness in a network and PID namespace of its own. Inside the private `/proc` a contained
/// harness sees, `NSpid` holds its namespace pid alone, so the PID namespace is proved by its
/// inode differing from the test process's.
fn assert_confined(run: &Run) {
    let lines = confinement(&run.workdir());
    assert_eq!(
        lines.get("Seccomp").map(String::as_str),
        Some("2"),
        "{lines:?}"
    );
    assert_eq!(
        lines.get("NoNewPrivs").map(String::as_str),
        Some("1"),
        "{lines:?}"
    );
    assert!(lines.contains_key("NSpid"), "{lines:?}");
    let net = &lines["net"];
    let pid = &lines["pid"];
    assert!(
        net.starts_with("net:[") && pid.starts_with("pid:["),
        "{lines:?}"
    );
    assert_ne!(
        net,
        &own_namespace("net"),
        "the harness shares the test's network namespace"
    );
    assert_ne!(
        pid,
        &own_namespace("pid"),
        "the harness shares the test's PID namespace"
    );
}

/// At `scoped` the harness runs filtered by seccomp, with no_new_privs, in the
/// session's network and PID namespaces, and `harness_start` names the tier.
#[test]
fn harness_runs_under_scoped() {
    if skip("harness_runs_under_scoped") {
        return;
    }
    let capsule = Built::new("confinement").shell(&["readlink"]).build();
    let run = capsule.run(&[]).succeeded();
    assert_confined(&run);
    assert_eq!(run.one("harness_start")["tier"], "landlock+seccomp");
}

/// At `sealed` the harness runs the same way inside the composed root, where a folder
/// of the host home no grant names cannot be listed.
#[test]
fn harness_runs_under_sealed() {
    if skip("harness_runs_under_sealed") {
        return;
    }
    let probe = Built::new("happy").containment("sealed").build();
    let explain = probe.explain_json();
    if explain["enforcement_tier"] != "mountns+pivot_root+landlock+seccomp" {
        eprintln!(
            "[SKIP-HOST] harness_runs_under_sealed: this host does not reach the composed root; \
             the sealed tier is {}",
            explain["enforcement_tier"]
        );
        return;
    }
    let home = common::scratch_off_tmp();
    let ungranted = home.path().join("ungranted");
    fs::create_dir_all(&ungranted).unwrap();
    fs::write(ungranted.join("inside"), "host file").unwrap();
    let capsule = Built::new("confinement")
        .containment("sealed")
        .shell(&["readlink"])
        .export("FIXTURE_UNGRANTED", ungranted.to_str().unwrap())
        .home(home)
        .build();
    let run = capsule.run(&[]).succeeded();
    assert_confined(&run);
    assert_eq!(
        run.one("harness_start")["tier"],
        "mountns+pivot_root+landlock+seccomp"
    );
    let lines = confinement(&run.workdir());
    assert_eq!(
        lines.get("ungranted-listing").map(String::as_str),
        Some("<none>"),
        "{lines:?}"
    );
}

/// A host listener the harness must not reach, with the port written down before `mur` starts.
struct HostListener {
    listener: TcpListener,
    port: u16,
}

impl HostListener {
    fn bind() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        Self { listener, port }
    }

    /// Whether any connection reached the listener.
    fn accepted(&self) -> bool {
        self.listener.accept().is_ok()
    }
}

/// The `escape` profile's outcomes, by key.
fn escape_outcomes(run: &Run) -> std::collections::BTreeMap<String, String> {
    let text = fs::read_to_string(run.workdir().join("escape.txt"))
        .unwrap_or_else(|error| panic!("no escape.txt: {error}\n{}", run.text));
    text.lines()
        .filter_map(|line| {
            let (key, value) = line.split_once(' ')?;
            Some((key.to_string(), value.to_string()))
        })
        .collect()
}

/// A capsule running the `escape` profile, a sentinel file in its `HOME` outside every grant and
/// the work folder, and the listener on the host's loopback.
fn escape_capsule(class: Option<&'static str>) -> (Capsule, String, HostListener) {
    let listener = HostListener::bind();
    let sentinel_value = format!("SENTINEL-{}", uuid::Uuid::new_v4().simple());
    let home = common::scratch_off_tmp();
    let sentinel = home.path().join("sentinel.txt");
    fs::write(&sentinel, &sentinel_value).unwrap();
    let built = match class {
        Some(class) => Built::new("escape").containment(class),
        None => Built::new("escape").no_floor(),
    };
    let capsule = built
        .shell(&["cat"])
        .export("FIXTURE_SENTINEL", sentinel.to_str().unwrap())
        .export("FIXTURE_HOST_PORT", &listener.port.to_string())
        .home(home)
        .build();
    (capsule, sentinel_value, listener)
}

/// Everything the run left that a leaked sentinel could be in: the work folder's files, the
/// harness's answer and the whole trace.
fn everything_written(run: &Run) -> String {
    let mut all = run.text.clone();
    all.push_str(&run.result());
    all.push_str(&fs::read_to_string(run.records_dir().join("trace.jsonl")).unwrap_or_default());
    for entry in fs::read_dir(run.workdir()).unwrap().flatten() {
        if let Ok(text) = fs::read_to_string(entry.path()) {
            all.push_str(&text);
        }
    }
    all
}

fn escape_is_refused_at(class: &'static str, test: &str) {
    if skip(test) {
        return;
    }
    let (capsule, sentinel, listener) = escape_capsule(Some(class));
    let run = capsule.run(&[]).succeeded();
    let outcomes = escape_outcomes(&run);
    assert_eq!(
        outcomes.get("read").map(String::as_str),
        Some("FAIL"),
        "{outcomes:?}"
    );
    assert_ne!(outcomes["tcp-1.1.1.1"], "OK", "{outcomes:?}");
    assert_ne!(outcomes["tcp-host"], "OK", "{outcomes:?}");
    assert!(
        !listener.accepted(),
        "the host's listener accepted a connection"
    );
    assert!(
        !everything_written(&run).contains(&sentinel),
        "the sentinel's contents reached the run's output"
    );
}

/// A harness doing its own work at `scoped` cannot read outside the capsule or reach
/// an unlisted host or the host's loopback.
#[test]
fn escape_from_the_harness_is_refused_at_scoped() {
    escape_is_refused_at("scoped", "escape_from_the_harness_is_refused_at_scoped");
}

/// The same escape at `sealed`.
#[test]
fn escape_from_the_harness_is_refused_at_sealed() {
    escape_is_refused_at("sealed", "escape_from_the_harness_is_refused_at_sealed");
}

/// The escape's control: unconfined, the same harness reads the sentinel and reaches the host's
/// listener, so the refusals above are the class at work. Whether 1.1.1.1 answers depends on the
/// host's own network and is not asserted.
#[test]
fn escape_from_the_harness_succeeds_unconfined() {
    let (capsule, sentinel, listener) = escape_capsule(None);
    let run = capsule.run(&["--unconfined"]).succeeded();
    let outcomes = escape_outcomes(&run);
    assert_eq!(
        outcomes.get("read").map(String::as_str),
        Some(format!("OK {sentinel}").as_str()),
        "{outcomes:?}"
    );
    assert_eq!(outcomes["tcp-host"], "OK", "{outcomes:?}");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !listener.accepted() {
        assert!(
            Instant::now() < deadline,
            "the host's listener accepted nothing"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Asserts `run` was refused with `E-CAP-021` before anything was spawned, naming the driver and
/// each of `named`.
fn assert_refused_naming(run: &Run, named: &[&str]) {
    assert!(!run.output.status.success(), "{}", run.text);
    assert!(run.text.contains("error[E-CAP-021]"), "{}", run.text);
    assert!(
        run.text.contains(&format!("'{DRIVER}@{DRIVER_VERSION}'")),
        "{}",
        run.text
    );
    for need in named {
        assert!(run.text.contains(need), "{need}: {}", run.text);
    }
    assert!(
        !run.text.contains("records: "),
        "a session was staged: {}",
        run.text
    );
}

/// The needs build with no grant, a grant at too low an access, and the path granted
/// with the host left out are each refused naming what is missing and the key to add; nothing is
/// spawned. `--explain-scope` exits 0 and shows each need refused.
#[test]
fn a_missing_grant_refuses_the_launch_naming_it() {
    if common::skip_without_host_support("a_missing_grant_refuses_the_launch_naming_it") {
        return;
    }
    // (a) No capabilities.harness.paths and no network.allow.
    let none = Built::new("happy").driver(Driver::Needs).build();
    fs::create_dir_all(none.home().join("fake-harness-sessions")).unwrap();
    let run = none.run(&[]);
    assert_refused_naming(
        &run,
        &[
            "~/fake-harness-sessions (read-write)",
            "no capabilities.harness.paths entry names it",
            "example.invalid:443",
            "capabilities.network.allow",
        ],
    );
    assert!(common::find_file(none.project.path(), "harness.pid").is_none());

    let report = none.explain_json();
    let harness = &report["harness"];
    assert_eq!(harness["driver"], format!("{DRIVER}@{DRIVER_VERSION}"));
    assert_eq!(harness["paths"][0]["status"], "refused", "{harness:#}");
    assert!(harness["paths"][0]["reason"].is_string(), "{harness:#}");
    assert_eq!(harness["hosts"][0]["status"], "refused", "{harness:#}");
    let text = none.explain_text();
    assert!(
        text.contains(&format!("Process harness ({DRIVER}@{DRIVER_VERSION})")),
        "{text}"
    );
    assert!(
        text.contains("path   ~/fake-harness-sessions (read-write): refused"),
        "{text}"
    );
    assert!(
        text.contains("host   example.invalid:443: refused"),
        "{text}"
    );

    // (b) The path covered at `read`, below the need's `read-write`.
    let low = Built::new("happy")
        .driver(Driver::Needs)
        .harness_path("~/fake-harness-sessions", "read")
        .network(&["example.invalid"])
        .build();
    let run = low.run(&[]);
    assert_refused_naming(
        &run,
        &[
            "~/fake-harness-sessions (read-write)",
            "covers it at access read",
        ],
    );
    assert!(!run.text.contains("example.invalid:443:"), "{}", run.text);

    // (c) The path granted, the host not.
    let no_host = Built::new("happy")
        .driver(Driver::Needs)
        .harness_path("~/fake-harness-sessions", "read-write")
        .build();
    fs::create_dir_all(no_host.home().join("fake-harness-sessions")).unwrap();
    let run = no_host.run(&[]);
    assert_refused_naming(
        &run,
        &["example.invalid:443: no capabilities.network.allow"],
    );
    assert!(
        !run.text.contains("~/fake-harness-sessions (read-write)"),
        "{}",
        run.text
    );
}

/// A manifest entry wider than the driver's need grants the need alone — the harness
/// writes in the declared folder and not beside it — and `--explain-scope` names the wider entry
/// as what allowed it.
#[test]
fn a_parent_grant_reaches_only_the_declared_path() {
    if skip("a_parent_grant_reaches_only_the_declared_path") {
        return;
    }
    let capsule = Built::new("parent-write")
        .driver(Driver::NeedsParent)
        .harness_path("~/fixture-harness-parent", "read-write")
        .build();
    let parent = capsule.home().join("fixture-harness-parent");
    fs::create_dir_all(parent.join("sessions")).unwrap();

    let report = capsule.explain_json();
    let path = &report["harness"]["paths"][0];
    assert_eq!(path["status"], "granted", "{report:#}");
    assert_eq!(
        path["path"],
        fs::canonicalize(parent.join("sessions"))
            .unwrap()
            .to_str()
            .unwrap()
    );
    assert_eq!(path["allowed_by"], "~/fixture-harness-parent");

    let run = capsule.run(&[]).succeeded();
    assert!(
        run.result().contains("sessions=OK sibling=FAIL"),
        "{}",
        run.result()
    );
    assert!(parent.join("sessions/written").exists());
    assert!(!parent.join("sibling-written").exists());
}

/// A need whose folder a symlink below the allowing entry leads out of
/// it is refused, never granted at the link's target.
#[test]
fn a_symlinked_need_outside_its_entry_is_refused() {
    if common::skip_without_host_support("a_symlinked_need_outside_its_entry_is_refused") {
        return;
    }
    let capsule = Built::new("parent-write")
        .driver(Driver::NeedsParent)
        .harness_path("~/fixture-harness-parent", "read-write")
        .build();
    let parent = capsule.home().join("fixture-harness-parent");
    let elsewhere = capsule.home().join("elsewhere");
    fs::create_dir_all(&parent).unwrap();
    fs::create_dir_all(&elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, parent.join("sessions")).unwrap();

    let report = capsule.explain_json();
    assert_eq!(
        report["harness"]["paths"][0]["status"], "refused",
        "{report:#}"
    );
    let run = capsule.run(&[]);
    assert_refused_naming(
        &run,
        &[
            "~/fixture-harness-parent/sessions (read-write)",
            "a symlink on the way leads out",
        ],
    );
    assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
}

/// An allowed need absent on this host is reported `missing`, the task still runs,
/// and `session_start.effective_grants.harness` is what `--explain-scope --json` printed.
#[test]
fn an_absent_allowed_need_is_reported_missing() {
    if skip("an_absent_allowed_need_is_reported_missing") {
        return;
    }
    let capsule = Built::new("happy")
        .driver(Driver::Needs)
        .harness_path("~/fake-harness-sessions", "read-write")
        .network(&["example.invalid"])
        .build();
    let report = capsule.explain_json();
    let path = &report["harness"]["paths"][0];
    assert_eq!(path["status"], "missing", "{report:#}");
    assert_eq!(path["allowed_by"], "~/fake-harness-sessions");
    assert!(path["path"].is_null(), "{report:#}");

    let run = capsule.run(&[]).succeeded();
    let start = run.one("session_start");
    assert_eq!(start["effective_grants"]["harness"], report["harness"]);
}

/// On a granted process capsule both harness routes are under the class at the
/// installed tier with no note, `--explain-scope` prints the `Process harness` block, and
/// `session_start.effective_grants` is what `--explain-scope --json` printed, byte for byte.
#[test]
fn session_start_effective_grants_match_explain_scope_json_on_a_process_capsule() {
    if skip("session_start_effective_grants_match_explain_scope_json_on_a_process_capsule") {
        return;
    }
    let capsule = Built::new("happy")
        .driver(Driver::Needs)
        .harness_path("~/fake-harness-sessions", "read-write")
        .network(&["example.invalid"])
        .build();
    fs::create_dir_all(capsule.home().join("fake-harness-sessions")).unwrap();

    let report = capsule.explain_json();
    for row in report["routes"].as_array().unwrap() {
        if row["route"] == "process-harness" || row["route"] == "harness-tools" {
            assert_eq!(row["present"], true, "{row}");
            assert_eq!(row["under_class"], true, "{row}");
            assert_eq!(row["mechanism"], report["enforcement_tier"], "{row}");
            assert!(row["note"].is_null(), "{row}");
        }
    }
    let harness = &report["harness"];
    assert_eq!(harness["paths"][0]["status"], "granted", "{harness:#}");
    assert_eq!(harness["hosts"][0]["status"], "allowed", "{harness:#}");
    assert_eq!(harness["hosts"][0]["allowed_by"], "example.invalid");
    assert!(harness["unresolved"].is_null(), "{harness:#}");
    let binary = harness["binary"]["path"].as_str().unwrap();
    assert!(binary.ends_with("harness.sh"), "{harness:#}");
    assert!(
        harness["binary"]["interpreter"]
            .as_str()
            .is_some_and(|interpreter| interpreter.ends_with("/bash")),
        "{harness:#}"
    );

    let text = capsule.explain_text();
    assert!(
        text.contains(&format!("Process harness ({DRIVER}@{DRIVER_VERSION})")),
        "{text}"
    );
    assert!(text.contains(&format!("  binary     {binary}")), "{text}");
    assert!(text.contains("  interpreter "), "{text}");
    assert!(
        text.contains("  path   ~/fake-harness-sessions → ")
            && text.contains("(read-write), allowed by ~/fake-harness-sessions"),
        "{text}"
    );
    assert!(
        text.contains("  host   example.invalid:443, allowed by example.invalid"),
        "{text}"
    );
    assert!(
        !text.contains("W-SEC-034") || !text.contains("process harness"),
        "{text}"
    );

    let run = capsule.run(&[]).succeeded();
    assert_eq!(run.one("session_start")["effective_grants"], report);
}

/// The bridge the harness is handed listens inside the session's network namespace —
/// the harness's bridged call succeeds, and while it is serving nothing in the host's namespace
/// reaches its address.
#[test]
fn the_bridge_listens_inside_the_session_namespace() {
    if skip("the_bridge_listens_inside_the_session_namespace") {
        return;
    }
    let capsule = Built::new("bridge-hold").with_tool().build();
    let workdir = capsule.project.path().join("wd");
    fs::create_dir_all(&workdir).unwrap();
    let manifest = capsule.manifest.to_str().unwrap().to_string();
    let mut child = capsule
        .mur()
        .args([
            "run",
            "--manifest",
            manifest.as_str(),
            "--task",
            "call the bridge",
            "--verbose",
            "--workdir",
            workdir.to_str().unwrap(),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let url_file = workdir.join("bridge-url.txt");
    let deadline = Instant::now() + Duration::from_secs(240);
    while !url_file.exists() {
        if Instant::now() > deadline || child.try_wait().unwrap().is_some() {
            let _ = child.kill();
            let mut err = String::new();
            child
                .stderr
                .take()
                .unwrap()
                .read_to_string(&mut err)
                .unwrap();
            panic!("the harness wrote no bridge URL: {err}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let url = fs::read_to_string(&url_file).unwrap();
    let address = url
        .strip_prefix("http://")
        .and_then(|rest| rest.split('/').next())
        .unwrap_or_else(|| panic!("not an http URL: {url}"))
        .to_string();
    assert!(address.starts_with("127.0.0.1:"), "{url}");
    let from_host =
        std::net::TcpStream::connect_timeout(&address.parse().unwrap(), Duration::from_secs(2));
    fs::write(workdir.join("go"), "").unwrap();
    let answer = fs::read_to_string(workdir.join("bridge-answer.txt")).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        from_host.is_err(),
        "a connection from the host's namespace reached {address} while the bridge served it"
    );
    assert!(
        answer.contains("ping-7"),
        "the bridged call did not answer: {answer}"
    );
}
