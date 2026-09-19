//! The credential gateway for any artifact: a tool, hook or driver whose operator entry declares
//! `gateway: {endpoint, api_key}` reaches that upstream through the runtime, which attaches the
//! operator's key the way the artifact's own `upstream_auth:` block says. The artifact never
//! holds the key.
//!
//! Driven through the real `mur` binary against recording upstreams on loopback, so every
//! assertion is about bytes that crossed a socket or a file the session wrote.

#[path = "common/mod.rs"]
mod common;

use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    net::TcpListener,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
};

use assert_cmd::Command;
use common::recording_upstream::{RecordingUpstream, Reply};
use murmur_artifact::{ArtifactMeta, LocalRegistry, Registry, RuntimeType};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const TOOL: &str = "gateway-probe";
/// The same component installed under a second name, whose entry declares no `gateway:`.
const TOOL_B: &str = "gateway-probe-b";
const HOOK: &str = "gateway-probe-hook";
const DRIVER: &str = "murmur-driver-anthropic";
const NATIVE_TOOL: &str = "native-gateway-tool";
const VERSION: &str = "0.1.0";

/// The key the operator holds. Assembled so no source line carries a credential-shaped literal.
const MARKER: &str = "gwk-2a94f6c1-probe-marker";
const ROTATED_MARKER: &str = "gwk-2a94f6c1-rotated-marker";
const DRIVER_MARKER: &str = "gwk-2a94f6c1-driver-marker";
const CREDENTIAL: &str = "GW_PROBE_KEY";
const DRIVER_CREDENTIAL: &str = "GW_DRIVER_KEY";

const BEARER_AUTH: &str = "upstream_auth:\n  header: Authorization\n  value: \"Bearer {key}\"\n";
const ANTHROPIC_AUTH: &str = "upstream_auth:\n  header: x-api-key\n  value: \"{key}\"\n";
/// What the tool upstream answers every request with.
const UPSTREAM_REPLY: &str = r#"{"results":["gateway-probe"]}"#;

const W_SEC_025_LINK: &str =
    "https://docs.murmur.nexus/murmur-nexus/murmur/reference/diagnostics/#w-sec-025";
const W_SEC_027_LINK: &str =
    "https://docs.murmur.nexus/murmur-nexus/murmur/reference/diagnostics/#w-sec-027";
const W_SEC_030_LINK: &str =
    "https://docs.murmur.nexus/murmur-nexus/murmur/reference/diagnostics/#w-sec-030";

// ── upstreams ───────────────────────────────────────────────────────────────

/// A loopback listener no grant names, which records whether anything connected to it.
struct UnlistedHost {
    authority: String,
    connected: Arc<Mutex<bool>>,
}

impl UnlistedHost {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let authority = listener.local_addr().unwrap().to_string();
        let connected = Arc::new(Mutex::new(false));
        let flag = Arc::clone(&connected);
        thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                *flag.lock().unwrap() = true;
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                );
            }
        });
        Self {
            authority,
            connected,
        }
    }

    fn was_reached(&self) -> bool {
        *self.connected.lock().unwrap()
    }
}

// ── the project under test ──────────────────────────────────────────────────

fn hex(text: &str) -> String {
    text.bytes().map(|byte| format!("{byte:02x}")).collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn fixture(relative: &str) -> PathBuf {
    common::fixture_path(relative)
}

/// The `key=value` report a probe wrote, for `field`.
fn reported<'a>(report: &'a str, field: &str) -> &'a str {
    report
        .split_whitespace()
        .find_map(|pair| pair.strip_prefix(&format!("{field}=")))
        .unwrap_or_else(|| panic!("'{field}=' missing from report: {report:?}"))
}

struct Project {
    home: TempDir,
    project: TempDir,
    artifacts: TempDir,
    credentials: BTreeMap<String, String>,
}

impl Project {
    fn new() -> Self {
        Self {
            home: TempDir::new().unwrap(),
            project: TempDir::new().unwrap(),
            artifacts: TempDir::new().unwrap(),
            credentials: BTreeMap::new(),
        }
    }

    fn config_path(&self) -> PathBuf {
        self.home.path().join(".murmur").join("config.yaml")
    }

    /// Writes `credentials.<name>` into the global config, owner-only, replacing the file by
    /// rename the way an editor does.
    fn set_credential(&mut self, name: &str, value: &str) {
        self.credentials.insert(name.to_string(), value.to_string());
        write_credentials(&self.config_path(), &self.credentials);
    }

    /// Packs the probe component under `name` with a bundled manifest carrying `auth_block`.
    fn publish_tool(&self, name: &str, auth_block: &str) {
        let wasm = fs::read(fixture("gateway-probe/tool/gateway-probe.wasm")).unwrap();
        let manifest = format!("name: {name}\nversion: {VERSION}\nruntime: wasm\n{auth_block}");
        self.publish_zip(name, &manifest, "tool.wasm", &wasm);
    }

    fn publish_hook(&self) {
        let wasm = fs::read(fixture("gateway-probe-hook/hook/gateway-probe-hook.wasm")).unwrap();
        let manifest = fs::read_to_string(fixture("gateway-probe-hook/murmur.yaml")).unwrap();
        self.publish_zip(HOOK, &manifest, "hook.wasm", &wasm);
    }

    fn publish_driver(&self) {
        let artifact = common::create_driver_artifact_with_auth(
            self.artifacts.path(),
            DRIVER,
            VERSION,
            &fixture("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
            ANTHROPIC_AUTH,
        );
        common::publish_local(&self.home, &artifact).success();
    }

    fn publish_zip(&self, name: &str, manifest: &str, entry: &str, wasm: &[u8]) {
        use zip::{
            write::{FileOptions, SimpleFileOptions},
            CompressionMethod, ZipWriter,
        };
        let path = self
            .artifacts
            .path()
            .join(format!("{name}-{VERSION}.mur.zip"));
        let mut zip = ZipWriter::new(fs::File::create(&path).unwrap());
        let options: SimpleFileOptions =
            FileOptions::default().compression_method(CompressionMethod::Deflated);
        zip.start_file("murmur.yaml", options).unwrap();
        zip.write_all(manifest.as_bytes()).unwrap();
        zip.start_file(entry, options).unwrap();
        zip.write_all(wasm).unwrap();
        zip.finish().unwrap();
        common::publish_local(&self.home, &path).success();
    }

    /// A script capsule running the probe capsule, with `entries` as its `artifacts:` list and
    /// `extra` spliced in at the top level.
    fn script_manifest(&self, entries: &str, extra: &str) -> PathBuf {
        fs::copy(
            fixture("gateway-probe/capsule/capsule-gateway-probe.wasm"),
            self.project.path().join("capsule.wasm"),
        )
        .unwrap();
        self.write_manifest(&format!(
            "name: gateway-capsule\nversion: 0.1.0\nartifacts:\n{entries}{extra}"
        ))
    }

    fn write_manifest(&self, text: &str) -> PathBuf {
        let path = self.project.path().join("murmur.yaml");
        fs::write(&path, text).unwrap();
        path
    }

    fn mur(&self, env: &[(&str, &str)]) -> Command {
        let mut command = Command::cargo_bin("mur").unwrap();
        command
            .env("HOME", self.home.path())
            .env_remove("NEXUS_API_KEY")
            .env_remove(CREDENTIAL)
            .env_remove(DRIVER_CREDENTIAL)
            .current_dir(self.project.path());
        for (name, value) in env {
            command.env(name, value);
        }
        command
    }

    fn run(&self, env: &[(&str, &str)], extra_args: &[&str]) -> Run {
        let manifest = self.project.path().join("murmur.yaml");
        let output = self
            .mur(env)
            .args([
                "run",
                "--manifest",
                manifest.to_str().unwrap(),
                "--task",
                &hex(MARKER),
                "--verbose",
            ])
            .args(extra_args)
            .output()
            .unwrap();
        Run::of(output)
    }

    fn explain_scope(&self, env: &[(&str, &str)]) -> Run {
        let manifest = self.project.path().join("murmur.yaml");
        Run::of(
            self.mur(env)
                .args([
                    "run",
                    "--manifest",
                    manifest.to_str().unwrap(),
                    "--explain-scope",
                ])
                .output()
                .unwrap(),
        )
    }

    fn doctor(&self, env: &[(&str, &str)]) -> Run {
        Run::of(self.mur(env).arg("doctor").output().unwrap())
    }

    /// No file the session or a refusal could have written holds `key`: the project directory
    /// (which holds every script-capsule workdir), `workdir` when the session had one, and
    /// `$HOME/.murmur/running`. `run`'s own output holds it nowhere either.
    fn assert_key_recorded_nowhere(&self, run: &Run, key: &str) {
        let mut roots = vec![
            self.project.path().to_path_buf(),
            self.home.path().join(".murmur").join("running"),
        ];
        if let Some(workdir) = run.workdir() {
            roots.push(workdir);
        }
        for root in roots {
            let leaks = files_containing(&root, key.as_bytes());
            assert!(leaks.is_empty(), "{}: {leaks:?}", root.display());
        }
        assert!(!run.stdout.contains(key), "stdout holds the key");
        assert!(!run.stderr.contains(key), "stderr holds the key");
    }
}

fn write_credentials(path: &Path, credentials: &BTreeMap<String, String>) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut text = String::from("credentials:\n");
    for (name, value) in credentials {
        text.push_str(&format!("  {name}: {value}\n"));
    }
    let staging = path.with_extension("yaml.tmp");
    fs::write(&staging, text).unwrap();
    fs::set_permissions(&staging, fs::Permissions::from_mode(0o600)).unwrap();
    fs::rename(&staging, path).unwrap();
}

fn files_containing(root: &Path, needle: &[u8]) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if fs::read(&path)
                .is_ok_and(|bytes| bytes.windows(needle.len()).any(|w| w == needle))
            {
                found.push(path);
            }
        }
    }
    found
}

fn session_dirs(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("ses_"))
            {
                found.push(path.clone());
            }
            stack.push(path);
        }
    }
    found
}

struct Run {
    output: std::process::Output,
    stdout: String,
    stderr: String,
}

impl Run {
    fn of(output: std::process::Output) -> Self {
        Self {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            output,
        }
    }

    fn ok(&self) -> bool {
        self.output.status.success()
    }

    fn context(&self) -> String {
        format!("stdout:\n{}\nstderr:\n{}", self.stdout, self.stderr)
    }

    fn workdir(&self) -> Option<PathBuf> {
        self.stdout
            .contains("workdir: ")
            .then(|| common::parse_workdir_from_stdout(&self.stdout))
    }

    fn published(&self, file: &str) -> String {
        let workdir = self
            .workdir()
            .unwrap_or_else(|| panic!("{}", self.context()));
        fs::read_to_string(workdir.join("out").join(file))
            .unwrap_or_else(|err| panic!("no out/{file} ({err}); {}", self.context()))
    }

    fn warning_lines(&self, code: &str) -> Vec<&str> {
        self.stderr
            .lines()
            .filter(|line| line.contains(&format!("warning[{code}]")))
            .collect()
    }

    fn trace(&self) -> Vec<Value> {
        let workdir = self
            .workdir()
            .unwrap_or_else(|| panic!("{}", self.context()));
        fs::read_to_string(workdir.join("trace.jsonl"))
            .unwrap_or_else(|err| panic!("no trace.jsonl ({err}); {}", self.context()))
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

/// `gateway-probe`'s entry with a gateway at `endpoint`, keyed by `${GW_PROBE_KEY}`.
fn tool_entry(endpoint: &str) -> String {
    format!(
        "  - name: {TOOL}\n    version: {VERSION}\n    runtime: tool\n    gateway:\n      \
         endpoint: {endpoint}/v1\n      api_key: ${{{CREDENTIAL}}}\n"
    )
}

/// `gateway-probe`'s entry with a gateway at `endpoint` whose body after the endpoint is
/// `credential`, one YAML line per element, already indented.
fn tool_entry_with(endpoint: &str, credential: &str) -> String {
    format!(
        "  - name: {TOOL}\n    version: {VERSION}\n    runtime: tool\n    gateway:\n      \
         endpoint: {endpoint}/v1\n{credential}"
    )
}

fn bare_entry(name: &str) -> String {
    format!("  - name: {name}\n    version: {VERSION}\n    runtime: tool\n")
}

/// A tool upstream, a project with `gateway-probe` published and its key in the global config.
fn keyed_tool_project() -> (Project, RecordingUpstream) {
    let upstream = RecordingUpstream::replying(UPSTREAM_REPLY);
    let mut project = Project::new();
    project.publish_tool(TOOL, BEARER_AUTH);
    project.set_credential(CREDENTIAL, MARKER);
    (project, upstream)
}

// ── scenarios ───────────────────────────────────────────────────────────────

/// A script capsule's tool reaches a third-party upstream with the operator's key, although the
/// tool never holds it and no allow-list names the upstream. The upstream sees exactly one
/// `Authorization`, the rendered one, in place of the tool's forged header.
#[test]
fn tool_gateway_happy_path() {
    let (project, upstream) = keyed_tool_project();
    project.script_manifest(&tool_entry(&upstream.endpoint), "");

    let run = project.run(&[], &[]);
    assert!(run.ok(), "{}", run.context());

    let requests = upstream.requests();
    assert_eq!(requests.len(), 1, "{}", run.context());
    assert!(
        requests[0].target.starts_with("/v1/"),
        "{}",
        requests[0].target
    );
    assert_eq!(
        requests[0].header_values("authorization"),
        vec![format!("Bearer {MARKER}").as_str()]
    );
    assert_eq!(requests[0].body_text(), r#"{"query":"gateway-probe"}"#);

    let report = run.published("result.txt");
    assert_eq!(reported(&report, "gateway_env"), "set");
    assert_eq!(reported(&report, "endpoint"), "http://127.0.0.1:9/v1");
    assert_eq!(reported(&report, "marker_in_env"), "false");
    assert_eq!(reported(&report, "status"), "200");
    assert_eq!(
        reported(&report, "sha256"),
        sha256_hex(UPSTREAM_REPLY.as_bytes())
    );

    let unmetered = run.warning_lines("W-SEC-030");
    assert_eq!(unmetered.len(), 1, "{}", run.stderr);
    assert!(
        unmetered[0].contains(&format!(
            "artifact '{TOOL}' reaches 127.0.0.1:{}",
            upstream.port
        )),
        "{}",
        unmetered[0]
    );
    assert!(unmetered[0].contains("unmetered"), "{}", unmetered[0]);
    assert!(unmetered[0].contains(W_SEC_030_LINK), "{}", unmetered[0]);
    project.assert_key_recorded_nowhere(&run, MARKER);
}

/// An `on-stage` hook with no `capabilities:` on its entry reaches its upstream through its own
/// gateway with the rendered key, and its request to a host nothing grants is still denied.
#[test]
fn hook_gateway_happy_path() {
    let upstream = RecordingUpstream::replying(UPSTREAM_REPLY);
    let unlisted = UnlistedHost::start();
    let mut project = Project::new();
    project.publish_hook();
    project.set_credential(CREDENTIAL, MARKER);
    project.script_manifest(
        &format!(
            "  - name: {HOOK}\n    version: {VERSION}\n    runtime: hook\n    config:\n      \
             unlisted: \"{}\"\n    gateway:\n      endpoint: {}/v1\n      api_key: \
             ${{{CREDENTIAL}}}\n",
            unlisted.authority, upstream.endpoint
        ),
        "",
    );

    let run = project.run(&[], &[]);
    assert!(run.ok(), "{}", run.context());

    let requests = upstream.requests();
    assert_eq!(requests.len(), 2, "{}", run.context());
    assert_eq!(requests[0].target, "/v1/v1/hook");
    assert_eq!(
        requests[0].header_values("authorization"),
        vec![format!("Bearer {MARKER}").as_str()]
    );
    assert_eq!(requests[1].target, "/v1/v1/hook-report");
    let report = requests[1].body_text();
    assert_eq!(reported(&report, "gateway_env"), "set");
    assert_eq!(reported(&report, "first"), "200");
    assert!(
        reported(&report, "unlisted").contains("HttpRequestDenied"),
        "{report}"
    );
    assert!(
        !reported(&report, "env_sha256").contains(&sha256_hex(MARKER.as_bytes())),
        "a hook environment value is the key"
    );
    assert!(!unlisted.was_reached(), "the unlisted host was reached");
    project.assert_key_recorded_nowhere(&run, MARKER);
}

/// A gateway belongs to its own artifact: the same component installed under a second name
/// without `gateway:` is not told where a gateway is, is denied at the gateway authority, and
/// sends the upstream nothing.
#[test]
fn gateway_is_per_artifact() {
    let (project, upstream) = keyed_tool_project();
    project.publish_tool(TOOL_B, BEARER_AUTH);
    project.script_manifest(
        &format!("{}{}", tool_entry(&upstream.endpoint), bare_entry(TOOL_B)),
        "",
    );

    let run = project.run(&[], &[]);
    assert!(run.ok(), "{}", run.context());

    let first = run.published("result.txt");
    assert_eq!(reported(&first, "status"), "200", "{first}");
    let second = run.published("result-b.txt");
    assert_eq!(reported(&second, "gateway_env"), "absent");
    assert_eq!(reported(&second, "endpoint"), "http://127.0.0.1:9");
    let error = second
        .split_once("error=")
        .map(|(_, error)| error)
        .unwrap_or_else(|| panic!("no error in: {second}"));
    assert!(error.contains("HttpRequestDenied"), "{second}");

    let requests = upstream.requests();
    assert_eq!(requests.len(), 1, "{}", run.context());
    assert_eq!(
        requests[0].header_values("authorization"),
        vec![format!("Bearer {MARKER}").as_str()]
    );
    project.assert_key_recorded_nowhere(&run, MARKER);
}

/// An artifact with a `gateway:` whose own manifest does not say how its upstream takes the key
/// refuses the launch by name and version, before anything is sent or created. An artifact with
/// neither is never asked.
#[test]
fn gateway_without_upstream_auth_refuses() {
    for (auth_block, reason, keyless) in [
        ("", None, false),
        (
            "upstream_auth:\n  header: Authorization\n  value: \"Bearer\"\n",
            Some("exactly once"),
            false,
        ),
        ("", None, true),
    ] {
        let upstream = RecordingUpstream::replying(UPSTREAM_REPLY);
        let mut project = Project::new();
        project.publish_tool(TOOL, auth_block);
        project.set_credential(CREDENTIAL, MARKER);
        let entry = if keyless {
            tool_entry_with(&upstream.endpoint, "      keyless: true\n")
        } else {
            tool_entry(&upstream.endpoint)
        };
        project.script_manifest(&entry, "");

        let run = project.run(&[], &[]);
        assert!(!run.ok(), "{}", run.context());
        assert!(run.stderr.contains("E-RUN-025"), "{}", run.context());
        assert!(
            run.stderr
                .contains(&format!("artifact '{TOOL}@{VERSION}' declares no usable")),
            "{}",
            run.context()
        );
        if let Some(reason) = reason {
            assert!(run.stderr.contains(reason), "{}", run.context());
        }
        assert!(upstream.requests().is_empty());
        for root in [project.project.path(), project.home.path()] {
            assert!(session_dirs(root).is_empty(), "{:?}", session_dirs(root));
        }
        project.assert_key_recorded_nowhere(&run, MARKER);
    }

    let project = Project::new();
    project.publish_tool(TOOL, "");
    project.script_manifest(&bare_entry(TOOL), "");
    let run = project.run(&[], &[]);
    assert!(run.ok(), "{}", run.context());
    assert_eq!(
        reported(&run.published("result.txt"), "gateway_env"),
        "absent"
    );
    assert!(run.warning_lines("W-SEC-030").is_empty(), "{}", run.stderr);
}

/// The block's old name is refused outright, never read in place of `upstream_auth:`: a tool
/// whose manifest declares it, alone or beside a valid `upstream_auth:`, refuses the launch by
/// name and version before its upstream is reached.
#[test]
fn gateway_with_the_retired_block_name_refuses() {
    let retired = "inference_auth:\n  header: Authorization\n  value: \"Bearer {key}\"\n";
    for auth_block in [retired.to_string(), format!("{BEARER_AUTH}{retired}")] {
        let upstream = RecordingUpstream::replying(UPSTREAM_REPLY);
        let mut project = Project::new();
        project.publish_tool(TOOL, &auth_block);
        project.set_credential(CREDENTIAL, MARKER);
        project.script_manifest(&tool_entry(&upstream.endpoint), "");

        let run = project.run(&[], &[]);
        println!("{}", run.stderr.trim());
        assert!(!run.ok(), "{}", run.context());
        for expected in [
            "E-RUN-025",
            &format!("artifact '{TOOL}@{VERSION}' declares no usable upstream_auth: block"),
            "was renamed to 'upstream_auth'",
        ] {
            assert!(
                run.stderr.contains(expected),
                "{expected}: {}",
                run.context()
            );
        }
        assert!(upstream.requests().is_empty());
        project.assert_key_recorded_nowhere(&run, MARKER);
    }
}

/// The old name is refused only where the block would be read: an entry without `gateway:`
/// never asks for it, so the tool launches and its call runs.
#[test]
fn retired_block_without_a_gateway_is_never_read() {
    let project = Project::new();
    project.publish_tool(
        TOOL,
        "inference_auth:\n  header: Authorization\n  value: \"Bearer {key}\"\n",
    );
    project.script_manifest(&bare_entry(TOOL), "");
    let run = project.run(&[], &[]);
    assert!(!run.stderr.contains("E-RUN-025"), "{}", run.context());
    assert_eq!(
        reported(&run.published("result.txt"), "gateway_env"),
        "absent"
    );
}

/// The runtime never reads `gateway:` from an artifact's own bundled `murmur.yaml`: both tools
/// bundle one naming a decoy, the operator's entry for the first names the real upstream, and the
/// second's entry names none. The decoy is never reached, and the second tool gets no gateway.
#[test]
fn bundled_gateway_is_ignored() {
    let decoy = RecordingUpstream::replying(UPSTREAM_REPLY);
    let upstream = RecordingUpstream::replying(UPSTREAM_REPLY);
    let mut project = Project::new();
    project.set_credential(CREDENTIAL, MARKER);
    let bundled = format!(
        "{BEARER_AUTH}gateway:\n  endpoint: {}/v1\n  api_key: ${{{CREDENTIAL}}}\n",
        decoy.endpoint
    );
    project.publish_tool(TOOL, &bundled);
    project.publish_tool(TOOL_B, &bundled);
    project.script_manifest(
        &format!("{}{}", tool_entry(&upstream.endpoint), bare_entry(TOOL_B)),
        "",
    );

    let run = project.run(&[], &[]);
    assert!(run.ok(), "{}", run.context());

    assert!(decoy.requests().is_empty(), "the decoy was reached");
    let requests = upstream.requests();
    assert_eq!(requests.len(), 1, "{}", run.context());
    assert_eq!(
        requests[0].header_values("authorization"),
        vec![format!("Bearer {MARKER}").as_str()]
    );
    let second = run.published("result-b.txt");
    assert_eq!(reported(&second, "gateway_env"), "absent", "{second}");
    assert!(second.contains("HttpRequestDenied"), "{second}");
    project.assert_key_recorded_nowhere(&run, MARKER);
}

/// A `gateway:` that names an upstream but binds no credential and does not declare
/// `keyless: true` refuses the launch with `E-CAP-018` on `mur run` and `mur doctor` alike, before
/// anything is created or sent. Every endpoint here is loopback: nothing infers keylessness from
/// the address.
#[test]
fn gateway_without_credential_refuses() {
    const HINT: &str = "set gateway.api_key: ${NAME} on this entry to have the runtime present a \
                        key, or write gateway.keyless: true if this upstream takes no key — the \
                        runtime never infers keylessness from the address";
    let tool_cases = [
        ("no api_key", ""),
        ("blank api_key", "      api_key: \"\"\n"),
        ("keyless: false", "      keyless: false\n"),
    ];
    for (case, credential) in tool_cases {
        let upstream = RecordingUpstream::replying(UPSTREAM_REPLY);
        let project = Project::new();
        project.publish_tool(TOOL, BEARER_AUTH);
        project.script_manifest(&tool_entry_with(&upstream.endpoint, credential), "");
        assert_refused_without_credential(case, &project, &upstream, TOOL, HINT);
    }

    let provider = RecordingUpstream::with_handler(|_, _| Reply::json(end_turn()));
    let project = Project::new();
    project.publish_driver();
    project.write_manifest(&format!(
        "name: gateway-agent\nversion: 0.1.0\nartifacts:\n  - name: {DRIVER}\n    version: \
         {VERSION}\n    runtime: driver\n    gateway:\n      endpoint: {}\ninference:\n  \
         transport: http\n  model: test-model\n  driver:\n    artifact: {DRIVER}\n",
        provider.endpoint
    ));
    assert_refused_without_credential("driver", &project, &provider, DRIVER, HINT);
}

fn assert_refused_without_credential(
    case: &str,
    project: &Project,
    upstream: &RecordingUpstream,
    artifact: &str,
    hint: &str,
) {
    for (surface, run) in [
        ("run", project.run(&[], &[])),
        ("doctor", project.doctor(&[])),
    ] {
        assert!(!run.ok(), "{case} / {surface}: {}", run.context());
        assert!(
            run.stderr.contains("error[E-CAP-018]"),
            "{case} / {surface}: {}",
            run.context()
        );
        assert!(
            run.stderr.contains(&format!(
                "artifact '{artifact}' declares gateway.endpoint '{}",
                upstream.endpoint
            )),
            "{case} / {surface}: {}",
            run.context()
        );
        assert!(
            run.stderr.contains("but binds no credential"),
            "{case} / {surface}: {}",
            run.context()
        );
        assert!(
            run.stderr.contains(hint),
            "{case} / {surface}: {}",
            run.context()
        );
    }
    assert!(
        upstream.requests().is_empty(),
        "{case}: the upstream was reached"
    );
    for root in [project.project.path(), project.home.path()] {
        assert!(
            session_dirs(root).is_empty(),
            "{case}: {:?}",
            session_dirs(root)
        );
    }
}

/// A tool entry that declares `keyless: true`, with no credential of its own anywhere, reaches
/// its upstream with no credential header at all: the tool's forged one is stripped and nothing
/// replaces it. Run in an agent capsule, since only an inference session writes
/// `session_start.gateways`; the driver beside it is keyed as usual.
#[test]
fn keyless_gateway_reaches_its_upstream() {
    let tool_upstream = RecordingUpstream::replying(UPSTREAM_REPLY);
    let provider = RecordingUpstream::with_handler(|index, _| {
        Reply::json(if index == 0 {
            tool_use_turn("keyless")
        } else {
            end_turn()
        })
    });
    let mut project = agent_project_with(
        &provider,
        &tool_entry_with(&tool_upstream.endpoint, "      keyless: true\n"),
    );
    project.credentials.remove(CREDENTIAL);
    write_credentials(&project.config_path(), &project.credentials);

    let run = project.run(&[], &[]);
    assert!(run.ok(), "{}", run.context());
    assert!(run.stdout.contains("status:  ok"), "{}", run.context());

    let requests = tool_upstream.requests();
    assert_eq!(requests.len(), 1, "{}", run.context());
    assert!(
        requests[0].target.starts_with("/v1/"),
        "{}",
        requests[0].target
    );
    assert!(
        requests[0].header_values("authorization").is_empty(),
        "{:?}",
        requests[0].headers
    );

    let tool_result = provider.requests()[1].body_text();
    assert!(tool_result.contains("status=200"), "{tool_result}");
    assert!(
        tool_result.contains(&format!("sha256={}", sha256_hex(UPSTREAM_REPLY.as_bytes()))),
        "{tool_result}"
    );

    let session_start = run
        .trace()
        .into_iter()
        .find(|event| event["event_type"] == "session_start")
        .expect("a session_start event");
    let tool = session_start["gateways"]
        .as_array()
        .unwrap()
        .iter()
        .find(|gateway| gateway["artifact"] == TOOL)
        .cloned()
        .unwrap_or_else(|| panic!("{}", session_start["gateways"]));
    assert_eq!(tool["credential_source"], "keyless", "{tool}");
    assert_eq!(tool["metered"], false, "{tool}");

    let unmetered = run.warning_lines("W-SEC-030");
    assert_eq!(unmetered.len(), 1, "{}", run.stderr);
    assert!(
        unmetered[0].contains(&format!("artifact '{TOOL}'")),
        "{}",
        unmetered[0]
    );
    assert!(run.warning_lines("W-SEC-027").is_empty(), "{}", run.stderr);
    project.assert_key_recorded_nowhere(&run, DRIVER_MARKER);
}

/// A `transport: http` driver whose entry declares `keyless: true` completes a task against a
/// provider that takes no key. Its gateway is still the metered inference gateway.
#[test]
fn keyless_driver_completes() {
    let provider = RecordingUpstream::with_handler(|_, _| Reply::json(end_turn()));
    let project = Project::new();
    project.publish_driver();
    project.write_manifest(&format!(
        "name: gateway-agent\nversion: 0.1.0\nartifacts:\n  - name: {DRIVER}\n    version: \
         {VERSION}\n    runtime: driver\n    gateway:\n      endpoint: {}\n      keyless: \
         true\ninference:\n  transport: http\n  model: test-model\n  driver:\n    artifact: \
         {DRIVER}\n",
        provider.endpoint
    ));

    let run = project.run(&[], &[]);
    assert!(run.ok(), "{}", run.context());
    assert!(run.stdout.contains("status:  ok"), "{}", run.context());

    let requests = provider.requests();
    assert!(!requests.is_empty(), "{}", run.context());
    for request in &requests {
        assert!(
            request.header_values("x-api-key").is_empty(),
            "{:?}",
            request.headers
        );
        assert!(
            request.header_values("authorization").is_empty(),
            "{:?}",
            request.headers
        );
    }

    let session_start = run
        .trace()
        .into_iter()
        .find(|event| event["event_type"] == "session_start")
        .expect("a session_start event");
    let driver = session_start["gateways"]
        .as_array()
        .unwrap()
        .iter()
        .find(|gateway| gateway["artifact"] == DRIVER)
        .cloned()
        .unwrap_or_else(|| panic!("{}", session_start["gateways"]));
    assert_eq!(driver["metered"], true, "{driver}");
    assert_eq!(driver["credential_source"], "keyless", "{driver}");
    assert_eq!(session_start["credential_source"], "keyless");
    assert!(
        run.warning_lines("W-SEC-030")
            .iter()
            .all(|line| !line.contains(DRIVER)),
        "{}",
        run.stderr
    );
}

/// A native tool's requests leave through the egress proxy and never pass the gateway, so a
/// `gateway:` on one is refused with `E-CAP-017` rather than left silently inert.
#[test]
fn gateway_on_native_tool_refuses() {
    let Some(binary) = common::fixture_native_tool_binary() else {
        eprintln!("skipping gateway_on_native_tool_refuses: the native fixture did not build");
        return;
    };
    let upstream = RecordingUpstream::replying(UPSTREAM_REPLY);
    let mut project = Project::new();
    project.set_credential(CREDENTIAL, MARKER);
    let manifest_bytes = fs::read(common::fixture_native_tool_manifest()).unwrap();
    let artifact = common::create_native_tool_zip(
        project.artifacts.path(),
        NATIVE_TOOL,
        VERSION,
        &manifest_bytes,
        &binary,
    );
    LocalRegistry::new(project.home.path().join(".murmur").join("artifacts"))
        .publish(
            ArtifactMeta {
                name: NATIVE_TOOL.to_string(),
                version: VERSION.to_string(),
                runtime: RuntimeType::Native,
                artifact_runtime: "native".to_string(),
                platforms: Vec::new(),
                description: None,
                tags: Vec::new(),
                wit_contracts: None,
            },
            &fs::read(&artifact).unwrap(),
        )
        .unwrap();
    project.script_manifest(
        &format!(
            "  - name: {NATIVE_TOOL}\n    version: {VERSION}\n    runtime: tool\n    gateway:\n      \
             endpoint: {}/v1\n      api_key: ${{{CREDENTIAL}}}\n",
            upstream.endpoint
        ),
        "",
    );

    let run = project.run(&[], &[]);
    assert!(!run.ok(), "{}", run.context());
    assert!(run.stderr.contains("E-CAP-017"), "{}", run.context());
    assert!(
        run.stderr.contains(&format!("artifact '{NATIVE_TOOL}'")),
        "{}",
        run.context()
    );
    assert!(run.stderr.contains("egress proxy"), "{}", run.context());
    assert!(upstream.requests().is_empty());
    project.assert_key_recorded_nowhere(&run, MARKER);
}

// ── agent capsules: a metered driver beside an unmetered tool ───────────────

fn tool_use_turn(id: &str) -> String {
    json!({
        "id": format!("msg_{id}"),
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{
            "type": "tool_use",
            "id": format!("toolu_{id}"),
            "name": TOOL,
            "input": {"marker_hex": hex(MARKER)},
        }],
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
        "content": [{"type": "text", "text": "done"}],
        "stop_reason": "end_turn",
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

/// An agent capsule whose driver and `gateway-probe` tool both carry a `gateway:`, the driver
/// keyed by `${GW_DRIVER_KEY}` and the tool by `${GW_PROBE_KEY}`, both held in the global config.
fn agent_project(provider: &RecordingUpstream, tool_upstream: &RecordingUpstream) -> Project {
    agent_project_with(provider, &tool_entry(&tool_upstream.endpoint))
}

/// [`agent_project`] with `tool` as `gateway-probe`'s entry.
fn agent_project_with(provider: &RecordingUpstream, tool: &str) -> Project {
    let mut project = Project::new();
    project.publish_driver();
    project.publish_tool(TOOL, BEARER_AUTH);
    project.set_credential(CREDENTIAL, MARKER);
    project.set_credential(DRIVER_CREDENTIAL, DRIVER_MARKER);
    project.write_manifest(&format!(
        "name: gateway-agent\nversion: 0.1.0\nartifacts:\n  - name: {DRIVER}\n    version: \
         {VERSION}\n    runtime: driver\n    gateway:\n      endpoint: {}\n      api_key: \
         ${{{DRIVER_CREDENTIAL}}}\n{}inference:\n  transport: http\n  model: test-model\n  \
         driver:\n    artifact: {DRIVER}\n",
        provider.endpoint, tool,
    ));
    project
}

/// A key rotated in the global config between two calls of the same tool reaches the second
/// call, and the trace records the rotation as a `gateway_credential` event naming the tool.
#[test]
fn tool_gateway_credential_rotates() {
    let tool_upstream = RecordingUpstream::replying(UPSTREAM_REPLY);
    let config = Arc::new(Mutex::new(None::<(PathBuf, BTreeMap<String, String>)>));
    let rotate = Arc::clone(&config);
    // The second driver request is sent only after the first tool call returned, and the second
    // tool call only after this reply: rotating here puts the change squarely between the two.
    let provider = RecordingUpstream::with_handler(move |index, _| {
        Reply::json(match index {
            0 => tool_use_turn("first"),
            1 => {
                if let Some((path, mut credentials)) = rotate.lock().unwrap().clone() {
                    credentials.insert(CREDENTIAL.to_string(), ROTATED_MARKER.to_string());
                    write_credentials(&path, &credentials);
                }
                tool_use_turn("second")
            }
            _ => end_turn(),
        })
    });
    let project = agent_project(&provider, &tool_upstream);
    *config.lock().unwrap() = Some((project.config_path(), project.credentials.clone()));

    let run = project.run(&[], &[]);
    assert!(run.stdout.contains("status:  ok"), "{}", run.context());

    let requests = tool_upstream.requests();
    assert_eq!(requests.len(), 2, "{}", run.context());
    assert_eq!(
        requests[0].header_values("authorization"),
        vec![format!("Bearer {MARKER}").as_str()]
    );
    assert_eq!(
        requests[1].header_values("authorization"),
        vec![format!("Bearer {ROTATED_MARKER}").as_str()]
    );
    for request in provider.requests() {
        assert_eq!(request.header_values("x-api-key"), vec![DRIVER_MARKER]);
    }

    let events: Vec<Value> = run
        .trace()
        .into_iter()
        .filter(|event| event["event_type"] == "gateway_credential")
        .collect();
    assert!(
        events.iter().any(|event| event["artifact"] == TOOL
            && event["change"] == "rotated"
            && event["source"] == "config"
            && event["credential"] == CREDENTIAL),
        "{events:?}"
    );
    for key in [MARKER, ROTATED_MARKER, DRIVER_MARKER] {
        project.assert_key_recorded_nowhere(&run, key);
    }
}

/// Only the configured driver's gateway is metered. `mur run --explain-scope` and `mur doctor`
/// each say so once, for the tool and never the driver, and `session_start.gateways` records both
/// with their metering.
#[test]
fn unmetered_gateway_is_labelled() {
    let tool_upstream = RecordingUpstream::replying(UPSTREAM_REPLY);
    let provider = RecordingUpstream::with_handler(|_, _| Reply::json(end_turn()));
    let project = agent_project(&provider, &tool_upstream);

    for (surface, output) in [
        ("run --explain-scope", project.explain_scope(&[])),
        ("doctor", project.doctor(&[])),
    ] {
        let lines = output.warning_lines("W-SEC-030");
        assert_eq!(lines.len(), 1, "{surface}: {}", output.context());
        assert!(
            lines[0].contains(&format!("artifact '{TOOL}'")),
            "{surface}: {}",
            lines[0]
        );
        assert!(!lines[0].contains(DRIVER), "{surface}: {}", lines[0]);
        for key in [MARKER, DRIVER_MARKER] {
            assert!(!output.stdout.contains(key) && !output.stderr.contains(key));
        }
    }

    let run = project.run(&[], &[]);
    assert!(run.stdout.contains("status:  ok"), "{}", run.context());
    let session_start = run
        .trace()
        .into_iter()
        .find(|event| event["event_type"] == "session_start")
        .expect("a session_start event");
    let gateways = &session_start["gateways"];
    assert_eq!(
        gateways,
        &json!([
            {
                "artifact": DRIVER,
                "host": format!("127.0.0.1:{}", provider.port),
                "credential_source": "config",
                "metered": true,
            },
            {
                "artifact": TOOL,
                "host": format!("127.0.0.1:{}", tool_upstream.port),
                "credential_source": "config",
                "metered": false,
            },
        ])
    );
    assert_eq!(session_start["credential_source"], "config");
    for key in [MARKER, DRIVER_MARKER] {
        project.assert_key_recorded_nowhere(&run, key);
    }
}

/// `inference.api_key` and `inference.endpoint` are refused by name, pointing at where each moved,
/// with nothing staged and nothing sent — on `mur run` and on `mur doctor` alike.
#[test]
fn removed_inference_fields_are_refused() {
    for (removed, line, moved_to) in [
        (
            "inference.api_key",
            format!("  api_key: ${{{DRIVER_CREDENTIAL}}}\n"),
            "gateway.api_key",
        ),
        (
            "inference.endpoint",
            "  endpoint: https://api.anthropic.com\n".to_string(),
            "gateway.endpoint",
        ),
    ] {
        let provider = RecordingUpstream::with_handler(|_, _| Reply::json(end_turn()));
        let tool_upstream = RecordingUpstream::replying(UPSTREAM_REPLY);
        let project = agent_project(&provider, &tool_upstream);
        let manifest = project.project.path().join("murmur.yaml");
        let text = fs::read_to_string(&manifest).unwrap().replace(
            "  model: test-model\n",
            &format!("{line}  model: test-model\n"),
        );
        fs::write(&manifest, text).unwrap();

        for (surface, run) in [
            ("run", project.run(&[], &[])),
            ("doctor", project.doctor(&[])),
        ] {
            assert!(!run.ok(), "{surface}: {}", run.context());
            assert!(
                run.stderr.contains("E-MAN-003"),
                "{surface}: {}",
                run.context()
            );
            assert!(
                run.stderr.contains(&format!("'{removed}'")),
                "{surface}: {}",
                run.context()
            );
            assert!(
                run.stderr.contains(&format!(
                    "set {moved_to} on the artifacts: entry '{DRIVER}'"
                )),
                "{surface}: {}",
                run.context()
            );
            assert!(
                !run.stderr.contains("W-SEC-019"),
                "{surface}: an unknown-key warning: {}",
                run.context()
            );
            project.assert_key_recorded_nowhere(&run, DRIVER_MARKER);
        }
        assert!(provider.requests().is_empty());
        assert!(tool_upstream.requests().is_empty());
        for root in [project.project.path(), project.home.path()] {
            assert!(session_dirs(root).is_empty(), "{:?}", session_dirs(root));
        }
    }
}

/// A `${NAME}` held neither in the global config nor in the environment refuses the launch,
/// naming the artifact, the field and the variable, and says how to store it.
#[test]
fn gateway_credential_not_found_refuses() {
    const UNSET: &str = "GW_UNSET_2A94F6C1";
    let upstream = RecordingUpstream::replying(UPSTREAM_REPLY);
    let project = Project::new();
    project.publish_tool(TOOL, BEARER_AUTH);
    project.script_manifest(
        &tool_entry(&upstream.endpoint).replace(CREDENTIAL, UNSET),
        "",
    );

    let run = project.run(&[], &[]);
    assert!(!run.ok(), "{}", run.context());
    assert!(run.stderr.contains("E-MAN-003"), "{}", run.context());
    assert!(
        run.stderr.contains(&format!(
            "gateway.api_key on artifact '{TOOL}' references ${{{UNSET}}}"
        )),
        "{}",
        run.context()
    );
    assert!(
        run.stderr
            .contains(&format!("mur config set -g credentials.{UNSET}")),
        "{}",
        run.context()
    );
    assert!(upstream.requests().is_empty());
    for root in [project.project.path(), project.home.path()] {
        assert!(session_dirs(root).is_empty(), "{:?}", session_dirs(root));
    }
}

/// Naming the gateway's upstream in `network.allow` is accepted and warned about once, naming the
/// entry and the artifact; the keyed request still goes through the gateway with one header.
#[test]
fn gateway_endpoint_in_network_allow_warns() {
    let (project, upstream) = keyed_tool_project();
    let entry = format!("127.0.0.1:{}", upstream.port);
    project.script_manifest(
        &tool_entry(&upstream.endpoint),
        &format!("capabilities:\n  network:\n    allow:\n      - \"{entry}\"\n"),
    );

    let run = project.run(&[], &[]);
    assert!(run.ok(), "{}", run.context());
    let lines = run.warning_lines("W-SEC-025");
    assert_eq!(lines.len(), 1, "{}", run.stderr);
    assert!(
        lines[0].contains(&format!("entry '{entry}'")),
        "{}",
        lines[0]
    );
    assert!(
        lines[0].contains(&format!("artifact '{TOOL}'")),
        "{}",
        lines[0]
    );
    assert!(lines[0].contains(W_SEC_025_LINK), "{}", lines[0]);

    let requests = upstream.requests();
    assert_eq!(requests.len(), 1, "{}", run.context());
    assert_eq!(
        requests[0].header_values("authorization"),
        vec![format!("Bearer {MARKER}").as_str()]
    );
    project.assert_key_recorded_nowhere(&run, MARKER);
}

/// A key the environment alone supplies can only be read at launch, which is said once, naming
/// the artifact and the reference and never the value.
#[test]
fn launch_only_gateway_credential_warns() {
    let upstream = RecordingUpstream::replying(UPSTREAM_REPLY);
    let project = Project::new();
    project.publish_tool(TOOL, BEARER_AUTH);
    project.script_manifest(&tool_entry(&upstream.endpoint), "");

    let run = project.run(&[(CREDENTIAL, MARKER)], &[]);
    assert!(run.ok(), "{}", run.context());
    let lines = run.warning_lines("W-SEC-027");
    assert_eq!(lines.len(), 1, "{}", run.stderr);
    assert!(
        lines[0].contains(&format!("artifact '{TOOL}'")),
        "{}",
        lines[0]
    );
    assert!(
        lines[0].contains(&format!("gateway.api_key: ${{{CREDENTIAL}}}")),
        "{}",
        lines[0]
    );
    assert!(lines[0].contains(W_SEC_027_LINK), "{}", lines[0]);
    assert_eq!(
        upstream.requests()[0].header_values("authorization"),
        vec![format!("Bearer {MARKER}").as_str()]
    );
    project.assert_key_recorded_nowhere(&run, MARKER);
}
