//! A native tool runs in the capsule's own sandbox: the same network namespace, seccomp filter,
//! Landlock scope and (on `sealed`) composed root as a shell command, with the capsule-wide grants
//! as its ceiling.
//!
//! The model is scripted. What runs is real: the staged session, the fixture native tool built from
//! `tests/fixtures/native-tool/`, the kernel-tier sandbox and the session's namespace. The fixture's
//! probe operations report each attempt's outcome in their `data`, so a refusal is read as the OS
//! error text the tool saw.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    io::Write,
    net::TcpListener,
    path::{Path, PathBuf},
};

use capsule_runtime::launch_session;
use murmur_artifact::{current_platform, ContainmentClass, ARTIFACT_INDEX_FILE_NAME};
use serde_json::{json, Value};
use tempfile::TempDir;

const DRIVER_NAME: &str = "local/murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";
const FIXTURE_ARTIFACT: &str = "local/tool-fixture";
const FIXTURE_TOOL: &str = common::FIXTURE_NATIVE_TOOL_NAME;
const FIXTURE_VERSION: &str = "0.1.0";

/// An address no capsule here allows, on a port no allow entry implies.
const UNLISTED_HOST: &str = "10.255.255.1";
const UNLISTED_PORT: u16 = 9;

// ── helpers ──────────────────────────────────────────────────────────────────

fn tool_call(index: usize, tool_use_id: &str, name: &str, input: Value) -> String {
    json!({
        "id": format!("msg_{index}"),
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "tool_use", "id": tool_use_id, "name": name, "input": input}],
        "stop_reason": "tool_use",
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

fn end_turn() -> String {
    json!({
        "id": "msg_end",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": "Done."}],
        "stop_reason": "end_turn",
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

/// A capsule: the scripted driver, the native tools in `tools`, and the grants below.
#[derive(Default)]
struct Capsule {
    /// `(full name, version)` of each native tool, beside the fixture.
    tools: Vec<(String, String)>,
    /// `capabilities.network.allow` beyond the driver's own endpoint.
    network_allow: Vec<String>,
    shell_allow: Vec<String>,
    unix_sockets: bool,
}

impl Capsule {
    fn manifest(&self, endpoint: &str) -> String {
        let mut manifest = format!(
            "name: native-confinement\n\
             version: 0.1.0\n\
             artifacts:\n\
             \x20 - name: {DRIVER_NAME}\n\
             \x20   version: {DRIVER_VERSION}\n\
             \x20   runtime: driver\n\
             \x20   gateway:\n\
             \x20     endpoint: {endpoint}\n\
             \x20     api_key: test-key\n\
             \x20 - name: {FIXTURE_ARTIFACT}\n\
             \x20   version: {FIXTURE_VERSION}\n\
             \x20   runtime: tool\n"
        );
        for (name, version) in &self.tools {
            manifest.push_str(&format!(
                "  - name: {name}\n    version: {version}\n    runtime: tool\n"
            ));
        }
        manifest.push_str("capabilities:\n  network:\n");
        manifest.push_str(&format!("    unix_sockets: {}\n", self.unix_sockets));
        manifest.push_str(&format!("    allow:\n      - {endpoint}\n"));
        for entry in &self.network_allow {
            manifest.push_str(&format!("      - {entry}\n"));
        }
        if !self.shell_allow.is_empty() {
            manifest.push_str("  shell:\n    allow:\n");
            for binary in &self.shell_allow {
                manifest.push_str(&format!("      - {binary}\n"));
            }
        }
        manifest.push_str(&format!(
            "inference:\n  transport: http\n  model: test-model\n  driver:\n    artifact: \
             {DRIVER_NAME}\n"
        ));
        manifest
    }
}

/// What a launched session left behind: each tool call's result text, keyed by its id, and the
/// session's workdirs.
struct Ran {
    results: Vec<(String, String)>,
    accessible_workdir: PathBuf,
    /// The scratch home, artifact folder and project, kept until the test has read the workdir.
    _dirs: [TempDir; 3],
}

impl Ran {
    fn text(&self, id: &str) -> &str {
        self.results
            .iter()
            .find(|(call, _)| call == id)
            .map(|(_, text)| text.as_str())
            .unwrap_or_else(|| panic!("no tool_result for {id}: {:?}", self.results))
    }

    /// A probe's `data` object.
    fn probe(&self, id: &str) -> Value {
        let text = self.text(id);
        serde_json::from_str(text).unwrap_or_else(|_| panic!("{id} returned no probe: {text}"))
    }
}

/// Whether this host can run a session at `floor` with a kernel tier and a network namespace,
/// printing a `[SKIP-HOST]` line when it cannot.
fn skip_host(test_name: &str, floor: ContainmentClass) -> bool {
    if common::skip_without_host_support(test_name) {
        return true;
    }
    let achieved = capsule_runtime::detect_achieved_containment();
    if achieved < floor {
        eprintln!("[SKIP-HOST] {test_name}: this host reaches {achieved}, the test needs {floor}");
        return true;
    }
    false
}

/// Publishes the driver, the fixture tool and each artifact in `extra`, then runs one session at
/// `floor` making `calls` in order. `None` when the fixture tool cannot be built.
fn run(
    capsule: &Capsule,
    extra: &[PathBuf],
    calls: &[(&str, &str, Value)],
    floor: ContainmentClass,
) -> Option<Ran> {
    run_in(capsule, extra, calls, floor, None)
}

/// [`run`] with `workdir` as the accessible workdir when given, so a test can prepare files in it
/// and name them by absolute path before the session is scripted.
fn run_in(
    capsule: &Capsule,
    extra: &[PathBuf],
    calls: &[(&str, &str, Value)],
    floor: ContainmentClass,
    workdir: Option<&Path>,
) -> Option<Ran> {
    let home = TempDir::new().unwrap();
    let artifact_dir = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();

    let binary = common::fixture_native_tool_binary()?;
    let driver = common::create_driver_artifact(
        artifact_dir.path(),
        DRIVER_NAME,
        DRIVER_VERSION,
        &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver).success();
    let manifest_bytes = fs::read(common::fixture_native_tool_manifest()).unwrap();
    let fixture = common::create_native_tool_zip(
        artifact_dir.path(),
        FIXTURE_TOOL,
        FIXTURE_VERSION,
        &manifest_bytes,
        &binary,
    );
    common::publish_local(&home, &fixture).success();
    for artifact in extra {
        common::publish_local(&home, artifact).success();
    }

    let mut responses: Vec<String> = calls
        .iter()
        .enumerate()
        .map(|(index, (id, tool, input))| tool_call(index, id, tool, input.clone()))
        .collect();
    responses.push(end_turn());
    let server = common::ScriptedServer::start(responses);

    let manifest_path = project.path().join("murmur.yaml");
    fs::write(&manifest_path, capsule.manifest(&server.endpoint)).unwrap();
    let staged =
        common::stage_agent_session_at_floor(&home, project.path(), &manifest_path, floor, workdir);
    let accessible_workdir = staged.accessible_workdir.clone();
    fs::write(accessible_workdir.join("task.md"), "Run the probes.").unwrap();

    launch_session(staged, |_| {}).expect("the session launches");

    let requests = server.requests();
    let results = calls
        .iter()
        .map(|(id, _, _)| {
            let text = common::tool_result_text(&requests, id)
                .unwrap_or_else(|| panic!("no tool_result for {id}"));
            println!("{id}: {text}");
            (id.to_string(), text)
        })
        .collect();
    Some(Ran {
        results,
        accessible_workdir,
        _dirs: [home, artifact_dir, project],
    })
}

fn probe(operation: &str, fields: Value) -> Value {
    let mut input = fields;
    input["operation"] = json!(operation);
    input
}

fn assert_refused(ran: &Ran, id: &str, expected_error: &str) {
    let probe = ran.probe(id);
    assert_eq!(probe["ok"], false, "{id} must be refused: {probe}");
    let error = probe["error"].as_str().unwrap_or_default();
    assert!(
        error.contains(expected_error),
        "{id} must fail with '{expected_error}': {probe}"
    );
}

fn assert_allowed(ran: &Ran, id: &str) -> Value {
    let probe = ran.probe(id);
    assert_eq!(probe["ok"], true, "{id} must succeed: {probe}");
    probe
}

/// A host directory outside every workdir, holding `secret.txt`.
fn outside_dir() -> TempDir {
    let outside = TempDir::new().unwrap();
    fs::write(outside.path().join("secret.txt"), "host secret\n").unwrap();
    outside
}

/// The calls S1 and S3 share: read and write outside the workdir, connect somewhere unlisted, then
/// write and read inside the workdir.
fn boundary_calls(outside: &Path) -> Vec<(&'static str, &'static str, Value)> {
    vec![
        (
            "toolu_read_outside",
            FIXTURE_TOOL,
            probe(
                "read",
                json!({"path": outside.join("secret.txt").to_str().unwrap()}),
            ),
        ),
        (
            "toolu_write_outside",
            FIXTURE_TOOL,
            probe(
                "write",
                json!({"path": outside.join("planted.txt").to_str().unwrap(), "text": "x"}),
            ),
        ),
        (
            "toolu_connect_unlisted",
            FIXTURE_TOOL,
            probe(
                "connect",
                json!({"host": UNLISTED_HOST, "port": UNLISTED_PORT}),
            ),
        ),
        (
            "toolu_write_inside",
            FIXTURE_TOOL,
            probe("write", json!({"path": "inside.txt", "text": "kept"})),
        ),
        (
            "toolu_read_inside",
            FIXTURE_TOOL,
            probe("read", json!({"path": "inside.txt"})),
        ),
    ]
}

/// A one-connection-at-a-time server on the host's loopback that answers every connection with
/// `reply` and closes it. Runs until the test process exits.
fn host_server(reply: &'static str) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let _ = stream.write_all(reply.as_bytes());
        }
    });
    port
}

// ── tests ────────────────────────────────────────────────────────────────────

/// S1: under `scoped`, a native tool cannot read or write a host file outside the workdir, nor
/// reach a host the capsule did not allow; inside the workdir it reads and writes as before.
#[test]
#[allow(clippy::print_stdout)]
fn a_native_tool_is_refused_outside_its_grants() {
    let test = "a_native_tool_is_refused_outside_its_grants";
    if skip_host(test, ContainmentClass::Scoped) {
        return;
    }
    let outside = outside_dir();
    let Some(ran) = run(
        &Capsule::default(),
        &[],
        &boundary_calls(outside.path()),
        ContainmentClass::Scoped,
    ) else {
        eprintln!("[SKIP] {test}: the fixture tool could not be built");
        return;
    };

    assert_refused(&ran, "toolu_read_outside", "Permission denied");
    assert_refused(&ran, "toolu_write_outside", "Permission denied");
    assert!(
        !outside.path().join("planted.txt").exists(),
        "the refused write must leave nothing on the host"
    );
    // Refused at once inside the session's namespace. Unconfined, the same connect leaves through
    // the host's route and times out, so a bare `ok: false` would not tell the two apart.
    assert_refused(&ran, "toolu_connect_unlisted", "Connection refused");
    assert_allowed(&ran, "toolu_write_inside");
    let read = assert_allowed(&ran, "toolu_read_inside");
    assert_eq!(read["contents"], "kept");
    assert_eq!(
        fs::read_to_string(ran.accessible_workdir.join("inside.txt")).unwrap(),
        "kept"
    );
}

/// S2: a native tool reaches an allowlisted host through the session's egress proxy, and a server
/// an earlier shell call started on the session's loopback. Both native calls are in one
/// namespace, which is not the host's.
#[test]
#[allow(clippy::print_stdout)]
fn a_native_tool_reaches_what_the_session_allows() {
    let test = "a_native_tool_reaches_what_the_session_allows";
    if skip_host(test, ContainmentClass::Scoped) {
        return;
    }
    let port = host_server("pong\n");
    // Three one-shot servers in a row on a port no allow entry implies, so it exists only on the
    // session's loopback. The shell call waits for the first to listen, by taking its one
    // connection, which leaves the next for the native tool.
    let start_server = "(for i in 1 2 3; do printf \"pong-from-shell\\n\" | nc -N -l 127.0.0.1 \
                        8080; done) >/dev/null 2>&1 & \
                        for i in {1..50}; do exec 3<>/dev/tcp/127.0.0.1/8080 2>/dev/null && \
                        break; sleep 0.1; done; read -r reply <&3; echo \"$reply\"";
    let capsule = Capsule {
        network_allow: vec![format!("http://127.0.0.1:{port}")],
        shell_allow: vec!["bash".into(), "nc".into(), "sleep".into()],
        ..Capsule::default()
    };
    let calls = vec![
        ("toolu_netns_first", FIXTURE_TOOL, probe("netns", json!({}))),
        (
            "toolu_connect_allowed",
            FIXTURE_TOOL,
            probe("connect", json!({"host": "127.0.0.1", "port": port})),
        ),
        (
            "toolu_shell_server",
            "bash",
            json!({"command": start_server}),
        ),
        (
            "toolu_connect_session_loopback",
            FIXTURE_TOOL,
            probe("connect", json!({"host": "127.0.0.1", "port": 8080})),
        ),
        (
            "toolu_netns_second",
            FIXTURE_TOOL,
            probe("netns", json!({})),
        ),
    ];
    let Some(ran) = run(&capsule, &[], &calls, ContainmentClass::Scoped) else {
        eprintln!("[SKIP] {test}: the fixture tool could not be built");
        return;
    };

    assert_eq!(
        assert_allowed(&ran, "toolu_connect_allowed")["reply"],
        "pong\n"
    );
    assert!(
        ran.text("toolu_shell_server").contains("pong-from-shell"),
        "the shell call reached its own server: {}",
        ran.text("toolu_shell_server")
    );
    assert_eq!(
        assert_allowed(&ran, "toolu_connect_session_loopback")["reply"],
        "pong-from-shell\n"
    );

    let first = assert_allowed(&ran, "toolu_netns_first")["netns"].clone();
    let second = assert_allowed(&ran, "toolu_netns_second")["netns"].clone();
    assert_eq!(
        first, second,
        "both native calls share the session's namespace"
    );
    let host = fs::read_link("/proc/self/ns/net").unwrap();
    assert_ne!(
        first.as_str(),
        Some(host.to_string_lossy().as_ref()),
        "a native tool never runs in the host's namespace"
    );
}

/// S3: under `sealed`, a host path outside the workdir does not exist for a native tool, and an
/// unlisted host is unreachable; the workdir is as readable as under `scoped`.
#[test]
#[allow(clippy::print_stdout)]
fn a_sealed_native_tool_cannot_see_outside_its_root() {
    let test = "a_sealed_native_tool_cannot_see_outside_its_root";
    if skip_host(test, ContainmentClass::Sealed) {
        return;
    }
    let outside = outside_dir();
    let Some(ran) = run(
        &Capsule::default(),
        &[],
        &boundary_calls(outside.path()),
        ContainmentClass::Sealed,
    ) else {
        eprintln!("[SKIP] {test}: the fixture tool could not be built");
        return;
    };

    assert_refused(&ran, "toolu_read_outside", "No such file or directory");
    assert_refused(&ran, "toolu_write_outside", "No such file or directory");
    assert!(
        !outside.path().join("planted.txt").exists(),
        "the refused write must leave nothing on the host"
    );
    // Refused at once inside the session's namespace. Unconfined, the same connect leaves through
    // the host's route and times out, so a bare `ok: false` would not tell the two apart.
    assert_refused(&ran, "toolu_connect_unlisted", "Connection refused");
    assert_allowed(&ran, "toolu_write_inside");
    assert_eq!(
        assert_allowed(&ran, "toolu_read_inside")["contents"],
        "kept"
    );
}

/// S4: a native tool is under the seccomp filter (no `AF_UNIX` socket without
/// `capabilities.network.unix_sockets`) and the exec allowlist: a host binary outside
/// `capabilities.shell.allow` cannot be run, one inside it can.
#[test]
#[allow(clippy::print_stdout)]
fn a_native_tool_runs_under_the_seccomp_filter_and_the_exec_scope() {
    let test = "a_native_tool_runs_under_the_seccomp_filter_and_the_exec_scope";
    if skip_host(test, ContainmentClass::Scoped) {
        return;
    }
    assert!(
        Path::new("/usr/bin/id").is_file(),
        "/usr/bin/id must exist on the host for its refusal to mean anything"
    );
    let sleep = capsule_runtime::find_on_path("sleep").expect("sleep is on PATH");
    let capsule = Capsule {
        shell_allow: vec!["sleep".into()],
        ..Capsule::default()
    };
    let calls = vec![
        ("toolu_unix", FIXTURE_TOOL, probe("unix_socket", json!({}))),
        (
            "toolu_exec_id",
            FIXTURE_TOOL,
            probe("exec", json!({"path": "/usr/bin/id"})),
        ),
        (
            "toolu_exec_sleep",
            FIXTURE_TOOL,
            probe(
                "exec",
                json!({"path": sleep.to_str().unwrap(), "args": ["0"]}),
            ),
        ),
    ];
    let Some(ran) = run(&capsule, &[], &calls, ContainmentClass::Scoped) else {
        eprintln!("[SKIP] {test}: the fixture tool could not be built");
        return;
    };

    assert_eq!(
        ran.probe("toolu_unix")["ok"],
        false,
        "{}",
        ran.text("toolu_unix")
    );
    assert_refused(&ran, "toolu_exec_id", "Permission denied");
    let sleep = assert_allowed(&ran, "toolu_exec_sleep");
    assert_eq!(sleep["code"], 0, "{sleep}");
}

/// A `#!/bin/sh` native tool that answers from builtins only.
const BUILTIN_SCRIPT: &str = "#!/bin/sh\n\
    while read -r _line; do :; done\n\
    printf '{\"status\":\"passed\",\"data\":\"script-ok\"}'\n";

/// A `#!/bin/sh` native tool that also runs `/usr/bin/id` and reports whether it could.
const ID_SCRIPT: &str = "#!/bin/sh\n\
    while read -r _line; do :; done\n\
    if /usr/bin/id -u >/dev/null 2>&1; then r=id-ran; else r=id-refused; fi\n\
    printf '{\"status\":\"passed\",\"data\":\"%s\"}' \"$r\"\n";

/// S5: a script entrypoint gets its `#!` interpreter and nothing more. It runs on its builtins,
/// and a program it starts is refused unless the capsule's `shell.allow` names it.
#[test]
#[allow(clippy::print_stdout)]
fn a_script_native_tool_gets_its_interpreter_and_nothing_more() {
    let test = "a_script_native_tool_gets_its_interpreter_and_nothing_more";
    if skip_host(test, ContainmentClass::Scoped) {
        return;
    }
    let artifacts = TempDir::new().unwrap();
    let scripts = vec![
        common::create_native_artifact(
            artifacts.path(),
            "builtin-script",
            "0.1.0",
            BUILTIN_SCRIPT,
            Some("Answers from shell builtins."),
            None,
        ),
        common::create_native_artifact(
            artifacts.path(),
            "id-script",
            "0.1.0",
            ID_SCRIPT,
            Some("Runs /usr/bin/id."),
            None,
        ),
    ];
    let tools = vec![
        (common::local("builtin-script"), "0.1.0".to_string()),
        (common::local("id-script"), "0.1.0".to_string()),
    ];
    let calls = vec![
        ("toolu_builtin", "builtin-script", json!({})),
        ("toolu_id", "id-script", json!({})),
    ];

    let without_id = Capsule {
        tools: tools.clone(),
        ..Capsule::default()
    };
    let Some(ran) = run(&without_id, &scripts, &calls, ContainmentClass::Scoped) else {
        eprintln!("[SKIP] {test}: the fixture tool could not be built");
        return;
    };
    assert_eq!(ran.text("toolu_builtin"), "script-ok");
    assert_eq!(ran.text("toolu_id"), "id-refused");

    let with_id = Capsule {
        tools,
        shell_allow: vec!["id".into()],
        ..Capsule::default()
    };
    let ran = run(&with_id, &scripts, &calls, ContainmentClass::Scoped).unwrap();
    assert_eq!(ran.text("toolu_builtin"), "script-ok");
    assert_eq!(ran.text("toolu_id"), "id-ran");
}

// ── the default-artifacts native tools ───────────────────────────────────────

/// The variable naming a directory of default-artifacts native tool zips, each built by that
/// tool's own `package.sh`. Read only by the ignored test below, so no ordinary run depends on a
/// sibling checkout.
const NATIVE_TOOL_ZIPS_ENV: &str = "MURMUR_NATIVE_TOOL_ZIPS";

/// A small Rust crate for code-graph to index and code-coverage to localize against.
fn write_rust_crate(root: &Path) {
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "/// Adds two numbers.\n\
         pub fn add(a: u32, b: u32) -> u32 {\n    a + b\n}\n\
         \n\
         /// Doubles a number through `add`.\n\
         pub fn double(a: u32) -> u32 {\n    add(a, a)\n}\n\
         \n\
         #[cfg(test)]\n\
         mod tests {\n\
         \x20   #[test]\n\
         \x20   fn adds() {\n        assert_eq!(super::add(1, 2), 3);\n    }\n\
         \x20   #[test]\n\
         \x20   fn doubles() {\n        assert_eq!(super::double(2), 5);\n    }\n\
         }\n",
    )
    .unwrap();
}

/// One LCOV report per test: `doubles` (failing) covers `double` and `add`, `adds` covers `add`.
fn write_lcov(dir: &Path, crate_root: &Path) {
    fs::create_dir_all(dir).unwrap();
    let source = crate_root.join("src/lib.rs");
    let source = source.display();
    fs::write(
        dir.join("doubles.info"),
        format!("SF:{source}\nDA:2,1\nDA:3,1\nDA:7,1\nDA:8,1\nend_of_record\n"),
    )
    .unwrap();
    fs::write(
        dir.join("adds.info"),
        format!("SF:{source}\nDA:2,1\nDA:3,1\nend_of_record\n"),
    )
    .unwrap();
}

const CARGO_TEST_OUTPUT: &str = "\nrunning 2 tests\n\
test tests::adds ... ok\n\
test tests::doubles ... FAILED\n\
\n\
failures:\n\
\n\
---- tests::doubles stdout ----\n\
thread 'tests::doubles' panicked at src/lib.rs:17:9:\n\
assertion `left == right` failed\n\
  left: 4\n\
 right: 5\n\
\n\
failures:\n\
    tests::doubles\n\
\n\
test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n";

/// A git repository with one commit, made on the host.
fn write_git_repo(root: &Path) {
    fs::create_dir_all(root).unwrap();
    fs::write(root.join("README.md"), "sample\n").unwrap();
    for args in [
        &["init", "-q", "-b", "main"][..],
        &["add", "README.md"],
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-q",
            "-m",
            "first commit",
        ],
    ] {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }
}

/// The zip in `zips` whose file name starts with `artifact-`: the one ending
/// `-<current platform>.mur.zip` when `zips` holds one, so a directory holding several platforms'
/// payloads works.
fn zip_for(zips: &Path, artifact: &str) -> PathBuf {
    let candidates: Vec<PathBuf> = fs::read_dir(zips)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(&format!("{artifact}-")))
        })
        .collect();
    let this_platform = format!("-{}.mur.zip", current_platform());
    candidates
        .iter()
        .find(|path| path.to_string_lossy().ends_with(&this_platform))
        .or_else(|| candidates.first())
        .cloned()
        .unwrap_or_else(|| panic!("no {artifact} zip in {}", zips.display()))
}

/// The version a default-artifacts zip carries, read from its packed `murmur.yaml`.
fn zip_version(zip: &Path) -> String {
    let mut archive = zip::ZipArchive::new(fs::File::open(zip).unwrap()).unwrap();
    let mut manifest = String::new();
    std::io::Read::read_to_string(&mut archive.by_name("murmur.yaml").unwrap(), &mut manifest)
        .unwrap();
    manifest
        .lines()
        .find_map(|line| line.strip_prefix("version: "))
        .unwrap()
        .trim()
        .to_string()
}

/// The five default-artifacts native tools, each driven through one scripted session under
/// `scoped` and under `sealed`, with the grants the coding capsules declare for them. Prints each
/// call's result; asserts the outcomes `docs/content/reference/default-artifacts.md` records.
#[test]
#[ignore = "needs MURMUR_NATIVE_TOOL_ZIPS pointing at default-artifacts' packaged native tools"]
#[allow(clippy::print_stdout)]
fn default_artifacts_native_tools_run_confined() {
    let test = "default_artifacts_native_tools_run_confined";
    let Some(zips) = std::env::var_os(NATIVE_TOOL_ZIPS_ENV).map(PathBuf::from) else {
        println!("[SKIP] {test}: {NATIVE_TOOL_ZIPS_ENV} is not set");
        return;
    };

    for floor in [ContainmentClass::Scoped, ContainmentClass::Sealed] {
        if skip_host(test, floor) {
            continue;
        }
        println!("== {floor}");
        for case in DefaultArtifactCase::ALL {
            let workdir = TempDir::new().unwrap();
            let root = workdir.path();
            let artifacts: Vec<PathBuf> = case
                .artifacts
                .iter()
                .map(|artifact| zip_for(&zips, artifact))
                .collect();
            let capsule = Capsule {
                tools: case
                    .artifacts
                    .iter()
                    .zip(&artifacts)
                    .map(|(artifact, zip)| (common::local(artifact), zip_version(zip)))
                    .collect(),
                network_allow: case.network_allow.iter().map(|s| s.to_string()).collect(),
                shell_allow: case.shell_allow.iter().map(|s| s.to_string()).collect(),
                ..Capsule::default()
            };
            let calls = case.prepare(root);
            let ran = run_in(&capsule, &artifacts, &calls, floor, Some(root))
                .expect("the fixture tool builds");
            for (id, tool, _) in &calls {
                let text = ran.text(id);
                let expected = expect(id);
                println!("[{floor}] {tool} {id}: expected one of {expected:?}");
                assert!(
                    expected.iter().any(|expect| expect.matches(text)),
                    "[{floor}] {tool} {id} must match one of {expected:?}: {text}"
                );
            }
        }
    }
}

/// What one default-artifacts tool call is expected to produce.
#[derive(Debug, Clone, Copy)]
enum Expect {
    /// Succeeds, and its result contains this text.
    Ok(&'static str),
}

impl Expect {
    fn matches(self, text: &str) -> bool {
        match self {
            Self::Ok(marker) => text.contains(marker) && !text.contains("\"ok\":false"),
        }
    }
}

/// The tools one session installs, with the grants the coding capsules give them.
struct DefaultArtifactCase {
    artifacts: &'static [&'static str],
    shell_allow: &'static [&'static str],
    network_allow: &'static [&'static str],
}

impl DefaultArtifactCase {
    const ALL: [DefaultArtifactCase; 4] = [
        DefaultArtifactCase {
            artifacts: &["tool-git"],
            shell_allow: &["git"],
            network_allow: &[],
        },
        // One session: code-coverage localizes against the index code-graph builds.
        DefaultArtifactCase {
            artifacts: &["tool-code-graph", "tool-code-coverage"],
            shell_allow: &[],
            network_allow: &[],
        },
        DefaultArtifactCase {
            artifacts: &["tool-test-report"],
            shell_allow: &[],
            network_allow: &[],
        },
        DefaultArtifactCase {
            artifacts: &["tool-registry-search"],
            shell_allow: &[],
            network_allow: &[
                "https://github.com",
                "https://release-assets.githubusercontent.com",
            ],
        },
    ];

    /// Prepares `root`, the accessible workdir, and returns the calls to make.
    fn prepare(&self, root: &Path) -> Vec<(&'static str, &'static str, Value)> {
        let crate_root = root.join("sample");
        let crate_path = crate_root.to_str().unwrap().to_string();
        match self.artifacts[0] {
            "tool-git" => {
                let repo = root.join("repo");
                write_git_repo(&repo);
                fs::write(repo.join("untracked.txt"), "new\n").unwrap();
                let repo = repo.to_str().unwrap().to_string();
                vec![
                    (
                        "toolu_git_status",
                        "tool-git",
                        json!({"operation": "status", "repo": repo}),
                    ),
                    (
                        "toolu_git_log",
                        "tool-git",
                        json!({"operation": "log", "repo": repo, "n": 1}),
                    ),
                ]
            }
            "tool-code-graph" => {
                write_rust_crate(&crate_root);
                write_lcov(&root.join("coverage"), &crate_root);
                vec![
                    (
                        "toolu_graph_index",
                        "tool-code-graph",
                        json!({"operation": "index_repository", "repo_path": crate_path}),
                    ),
                    (
                        "toolu_graph_find",
                        "tool-code-graph",
                        json!({"operation": "find_symbol", "repo_path": crate_path,
                               "query": "double"}),
                    ),
                    (
                        "toolu_coverage_localize",
                        "tool-code-coverage",
                        json!({"operation": "localize", "repo_path": crate_path,
                               "coverage_dir": root.join("coverage").to_str().unwrap(),
                               "failing_tests": ["doubles"]}),
                    ),
                ]
            }
            "tool-test-report" => {
                let output = root.join("cargo-test.txt");
                fs::write(&output, CARGO_TEST_OUTPUT).unwrap();
                vec![(
                    "toolu_report_parse",
                    "tool-test-report",
                    json!({"operation": "parse", "input_path": output.to_str().unwrap()}),
                )]
            }
            "tool-registry-search" => {
                let index = root.join(ARTIFACT_INDEX_FILE_NAME);
                fs::copy(published_index_fixture(), &index).unwrap();
                vec![
                    (
                        "toolu_search_public",
                        "tool-registry-search",
                        json!({"query": "git", "limit": 3}),
                    ),
                    (
                        "toolu_search_local",
                        "tool-registry-search",
                        json!({"query": "registry-search", "index": index.to_str().unwrap()}),
                    ),
                ]
            }
            other => unreachable!("no case for {other}"),
        }
    }
}

/// The outcomes `docs/content/reference/default-artifacts.md` records for each call, the same at
/// `scoped` and `sealed`; a call passes when it matches any of them.
fn expect(id: &str) -> Vec<Expect> {
    match id {
        "toolu_git_status" => vec![Expect::Ok("untracked.txt")],
        "toolu_git_log" => vec![Expect::Ok("first commit")],
        "toolu_graph_index" => vec![Expect::Ok("total_symbols")],
        "toolu_graph_find" => vec![Expect::Ok("double")],
        "toolu_coverage_localize" => vec![Expect::Ok("top_suspects")],
        "toolu_report_parse" => vec![Expect::Ok("tests::doubles")],
        "toolu_search_public" => vec![Expect::Ok("murmur/tool-git")],
        "toolu_search_local" => vec![Expect::Ok("murmur/tool-registry-search")],
        other => unreachable!("no expectation for {other}"),
    }
}

/// The verbatim copy of the index a default-artifacts release publishes.
fn published_index_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/published-index")
        .join(ARTIFACT_INDEX_FILE_NAME)
}
