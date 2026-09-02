#![allow(dead_code)]

pub mod hook_wat;

use std::{
    collections::HashSet,
    fs,
    io::{Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    thread,
};

use assert_cmd::{assert::Assert, Command};
use capsule_runtime::{
    capability_policy_from_runtime_manifest, stage_session, ArtifactRequest, StageRequest,
    StagedSession,
};
use murmur_artifact::{load_runtime_manifest, ArtifactRuntime, ContainmentClass, LocalRegistry};
use serde_json::{json, Value};
use tempfile::TempDir;
use zip::{
    write::{FileOptions, SimpleFileOptions},
    CompressionMethod, ZipWriter,
};

pub fn publish_local(home: &TempDir, artifact_path: &Path) -> Assert {
    let mut cmd = Command::cargo_bin("mur").unwrap();
    cmd.env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .args(["publish", artifact_path.to_str().unwrap()])
        .assert()
}

/// Install an artifact into the project store of `project_dir` via `mur install <path>`.
/// The project directory must already contain murmur.yaml so that
/// `find_project_root()` can locate it.
pub fn install_artifact_to_project(project_dir: &Path, artifact_path: &Path) -> Assert {
    let mut cmd = Command::cargo_bin("mur").unwrap();
    cmd.current_dir(project_dir)
        .args(["install", artifact_path.to_str().unwrap()])
        .assert()
}

#[allow(dead_code)]
pub fn run_capsule(home: &TempDir, manifest_path: &Path) -> Assert {
    run_capsule_with_env(home, manifest_path, &[])
}

/// `run_capsule` with `extra_env` set on the `mur` process itself, for tests that need a
/// host variable present in the runtime's environment to observe whether it reaches a guest.
#[allow(dead_code)]
pub fn run_capsule_with_env(
    home: &TempDir,
    manifest_path: &Path,
    extra_env: &[(&str, &str)],
) -> Assert {
    let mut cmd = Command::cargo_bin("mur").unwrap();
    cmd.env("HOME", home.path()).env_remove("NEXUS_API_KEY");
    for (key, value) in extra_env {
        cmd.env(key, value);
    }
    cmd.args([
        "run",
        "--manifest",
        manifest_path.to_str().unwrap(),
        "--verbose",
    ])
    .assert()
}

/// Whether this host can hand `mur` a delegated cgroup v2 scope.
///
/// A capsule that declares `capabilities.shell.allow` or `capabilities.resources` refuses to
/// launch without one (`E-RUN-012`), so a test that launches such a capsule has nothing to observe
/// on a host that cannot delegate — a containerised CI runner, notably. Pair it with
/// [`skip_without_host_support`], which prints the line CI counts.
///
/// The probe itself lives in `capsule-runtime` beside the code it describes, so these tests and
/// the runtime's own answer the question the same way.
pub fn cgroup_delegation_available() -> bool {
    capsule_runtime::cgroup_delegation_available()
}

/// Skip guard for a test that launches a subprocess-capable capsule, written as
/// `if common::skip_without_host_support("test_name") { return; }`.
///
/// Covers both fail-closed launch gates, not just [`cgroup_delegation_available`]: such a capsule
/// also needs its own network namespace, and a runner under AppArmor's unprivileged-userns
/// restriction refuses that while delegating a cgroup scope perfectly well.
///
/// Prints one `[SKIP-HOST]`-prefixed line per skipped test, which the CI job's summary step
/// counts to report how much of the suite the runner could not exercise. Cargo swallows a passing
/// test's output, so the line only reaches the log under `--nocapture`.
pub fn skip_without_host_support(test_name: &str) -> bool {
    capsule_runtime::skip_without_host_support(test_name)
}

pub fn fixture_path(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(relative)
}

/// Root of a local `default-artifacts` checkout, used by tests marked
/// `#[ignore]` that depend on artifacts built there.
///
/// `MURMUR_DEFAULT_ARTIFACTS_DIR` is the only way to find that checkout, and
/// `None` means it was not set. No relative-path fallback to a sibling directory:
/// one would make the suite pass or fail on how the machine happens to be laid
/// out. Callers must skip when this returns `None`.
pub fn default_artifacts_dir() -> Option<PathBuf> {
    std::env::var_os("MURMUR_DEFAULT_ARTIFACTS_DIR").map(PathBuf::from)
}

/// Name of the fixture native tool crate under `tests/fixtures/native-tool/`,
/// used both as the artifact name and as the binary name inside `bin/`.
pub const FIXTURE_NATIVE_TOOL_NAME: &str = "murmur-tool-fixture";

/// Build (once) and locate the fixture native tool binary.
///
/// Returns `None` — for the caller to turn into a clean skip — if the build
/// cannot run or leaves nothing at the expected path. Unlike the WASM fixtures
/// alongside it, this binary is host-native and therefore not portable across
/// the project's platform targets, so it is compiled here rather than checked
/// in. Output lands in the workspace target directory, so repeat runs reuse it.
pub fn fixture_native_tool_binary() -> Option<PathBuf> {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let manifest = fixture_path("native-tool/Cargo.toml");
    let target_dir = workspace_root.join("target").join("native-tool-fixture");
    let binary_path = target_dir.join("release").join(FIXTURE_NATIVE_TOOL_NAME);

    if !binary_path.exists() {
        let status = std::process::Command::new("cargo")
            .args(["build", "--release", "--manifest-path"])
            .arg(&manifest)
            .arg("--target-dir")
            .arg(&target_dir)
            .status()
            .ok()?;
        if !status.success() {
            eprintln!("[fixture] cargo build of {FIXTURE_NATIVE_TOOL_NAME} failed");
            return None;
        }
    }

    // A build can exit 0 without producing the binary, so the caller gets a clean skip
    // rather than a path that fails to spawn inside a test body.
    if !binary_path.exists() {
        eprintln!(
            "[fixture] {FIXTURE_NATIVE_TOOL_NAME} not found at {} after a successful build",
            binary_path.display()
        );
        return None;
    }
    Some(binary_path)
}

/// The fixture native tool's own `murmur.yaml`, the manifest the tests pack into
/// its artifact zip so `input_schema` matches what a real artifact carries.
pub fn fixture_native_tool_manifest() -> PathBuf {
    fixture_path("native-tool/murmur.yaml")
}

/// Pack a native tool artifact zip with the canonical `murmur.yaml` + `bin/<name>` layout:
/// the manifest at the archive root and the binary executable at `bin/<name>`.
pub fn create_native_tool_zip(
    dir: &Path,
    name: &str,
    version: &str,
    manifest_bytes: &[u8],
    binary_path: &Path,
) -> PathBuf {
    let artifact_path = dir.join(format!("{name}-{version}.mur.zip"));
    let file = fs::File::create(&artifact_path).unwrap();
    let mut zip = ZipWriter::new(file);

    let options: SimpleFileOptions =
        FileOptions::default().compression_method(CompressionMethod::Deflated);
    zip.start_file("murmur.yaml", options).unwrap();
    zip.write_all(manifest_bytes).unwrap();

    let exec_options: SimpleFileOptions = FileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(0o755);
    zip.start_file(format!("bin/{name}"), exec_options).unwrap();
    zip.write_all(&fs::read(binary_path).unwrap()).unwrap();

    zip.finish().unwrap();
    artifact_path
}

/// Point `$HOME` at a scratch directory that lives as long as the test process.
///
/// Staging in-process means the runtime resolves `$HOME` from the test binary's own environment,
/// and an `http` capsule keeps a conversation record under it — so without this a suite that
/// stages a session writes into the developer's home. Set once, never per test: `set_var` mutates
/// process-global state that the launch reads on another thread, so a per-test value would race
/// with every sibling test in the same binary. No caller needs `$HOME` to be its own temp home:
/// the artifact store is passed to `stage_session` explicitly.
fn redirect_home_away_from_the_developers() {
    static SCRATCH_HOME: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    let home = SCRATCH_HOME.get_or_init(|| {
        let dir = tempfile::tempdir().expect("a scratch home");
        let path = dir.path().to_path_buf();
        // Held for the process: `$HOME` must not dangle while a later test is still launching.
        std::mem::forget(dir);
        std::env::set_var("HOME", &path);
        path
    });
    debug_assert_eq!(
        std::env::var_os("HOME").map(PathBuf::from).as_ref(),
        Some(home)
    );
}

pub fn stage_agent_session(
    home: &TempDir,
    project_dir: &Path,
    manifest_path: &Path,
) -> StagedSession {
    stage_agent_session_inner(home, project_dir, manifest_path, None, None)
}

/// `stage_agent_session` with an explicit accessible workdir, the shape `mur run --workdir` takes.
///
/// For the one property that needs the accessible workdir's path known *before* the session is
/// scripted: with no override the runtime mints `<manifest_dir>/workdir/<random session id>`, so a
/// test cannot name a file inside it up front.
pub fn stage_agent_session_with_workdir(
    home: &TempDir,
    project_dir: &Path,
    manifest_path: &Path,
    workdir: &Path,
) -> StagedSession {
    stage_agent_session_inner(
        home,
        project_dir,
        manifest_path,
        None,
        Some(workdir.to_path_buf()),
    )
}

/// `stage_agent_session` for a launch that continues `from_session` under `context_id`, the pair
/// `mur run --resume` resolves before staging.
pub fn stage_agent_session_resuming(
    home: &TempDir,
    project_dir: &Path,
    manifest_path: &Path,
    from_session: &str,
    context_id: &str,
) -> StagedSession {
    stage_agent_session_inner(
        home,
        project_dir,
        manifest_path,
        Some((from_session.to_string(), context_id.to_string())),
        None,
    )
}

fn stage_agent_session_inner(
    home: &TempDir,
    project_dir: &Path,
    manifest_path: &Path,
    resume: Option<(String, String)>,
    workdir: Option<PathBuf>,
) -> StagedSession {
    let runtime_manifest = load_runtime_manifest(manifest_path).unwrap();

    redirect_home_away_from_the_developers();

    let mut allowlisted_tools = HashSet::new();
    let mut requested_artifacts = Vec::new();
    for artifact in &runtime_manifest.artifacts {
        if matches!(artifact.runtime, ArtifactRuntime::Tool) {
            allowlisted_tools.insert(artifact.name.clone());
        }
        requested_artifacts.push(ArtifactRequest {
            name: artifact.name.clone(),
            version: artifact.version.clone(),
            runtime: artifact.runtime.clone(),
            source: artifact.source.clone(),
            on_overflow: artifact.on_overflow,
            config: artifact.config.clone(),
            capabilities: artifact.capabilities.clone(),
        });
    }

    let local_registry = LocalRegistry::new(home.path().join(".murmur").join("artifacts"));

    stage_session(
        std::sync::Arc::new(local_registry),
        StageRequest {
            manifest_dir: project_dir.to_path_buf(),
            capsule_name: runtime_manifest.name.clone(),
            capsule_version: runtime_manifest.version.clone(),
            capsule_component_bytes: Vec::new(),
            artifacts: requested_artifacts,
            allowlisted_tools,
            lock_expectations: None,
            capability_policy: capability_policy_from_runtime_manifest(&runtime_manifest),
            inference: runtime_manifest.inference.clone(),
            system_prompt_overridden: false,
            context: runtime_manifest.context.clone(),
            context_id: resume.as_ref().map(|(_, context_id)| context_id.clone()),
            resume: resume
                .as_ref()
                .map(|(from_session, _)| capsule_runtime::ResumeRequest {
                    from_session: from_session.clone(),
                    mode: capsule_runtime::ResumeMode::Full,
                }),
            otel_endpoint: None,
            eval_config_json: None,
            case_id: None,
            dataset_id: None,
            // The manifest's own block, exactly as `mur run` passes it, so a test capsule
            // declaring `lifecycle:` gets the lifecycle it declared rather than the default.
            lifecycle: runtime_manifest.lifecycle.clone(),
            lifecycle_override: None,
            // Carried through the same way `mur run` carries it, so a test manifest's `trace:`
            // block reaches the session's writer instead of being silently dropped.
            trace: runtime_manifest.trace,
            workdir,
            bind_addr: "127.0.0.1".to_string(),
            internal_port: None,
            declared_containment_floor: ContainmentClass::Advisory,
            exports: None,
            spawn_grant: None,
        },
    )
    .unwrap()
}

pub fn create_driver_artifact(dir: &Path, name: &str, version: &str, wasm_path: &Path) -> PathBuf {
    let artifact_path = dir.join(format!("{name}-{version}.mur.zip"));
    let file = fs::File::create(&artifact_path).unwrap();
    let mut zip = ZipWriter::new(file);
    let options: SimpleFileOptions =
        FileOptions::default().compression_method(CompressionMethod::Deflated);

    zip.start_file("murmur.yaml", options).unwrap();
    writeln!(zip, "name: {name}").unwrap();
    writeln!(zip, "version: {version}").unwrap();
    writeln!(zip, "runtime: driver").unwrap();

    zip.start_file("tool.wasm", options).unwrap();
    zip.write_all(&fs::read(wasm_path).unwrap()).unwrap();

    zip.finish().unwrap();
    artifact_path
}

pub fn create_hook_artifact(dir: &Path, name: &str, version: &str, wasm_path: &Path) -> PathBuf {
    let artifact_path = dir.join(format!("{name}-{version}.mur.zip"));
    let file = fs::File::create(&artifact_path).unwrap();
    let mut zip = ZipWriter::new(file);
    let options: SimpleFileOptions =
        FileOptions::default().compression_method(CompressionMethod::Deflated);

    zip.start_file("murmur.yaml", options).unwrap();
    writeln!(zip, "name: {name}").unwrap();
    writeln!(zip, "version: {version}").unwrap();
    writeln!(zip, "runtime: hook").unwrap();

    zip.start_file("hook.wasm", options).unwrap();
    zip.write_all(&fs::read(wasm_path).unwrap()).unwrap();

    zip.finish().unwrap();
    artifact_path
}

/// Create a native artifact .mur.zip with a shell-script binary.
///
/// The binary is named `<name>` and must be an executable script (shebang line included).
/// This is used in tests to exercise the native artifact staging and dispatch path
/// without requiring a pre-compiled Rust binary.
pub fn create_native_artifact(
    dir: &Path,
    name: &str,
    version: &str,
    binary_script: &str,
    description: Option<&str>,
    input_schema: Option<&str>,
) -> PathBuf {
    let artifact_path = dir.join(format!("{name}-{version}.mur.zip"));
    let file = fs::File::create(&artifact_path).unwrap();
    let mut zip = ZipWriter::new(file);

    // Manifest entry — Deflated for consistency
    let options: SimpleFileOptions =
        FileOptions::default().compression_method(CompressionMethod::Deflated);

    zip.start_file("murmur.yaml", options).unwrap();
    writeln!(zip, "name: {name}").unwrap();
    writeln!(zip, "version: {version}").unwrap();
    // A native tool is `runtime: tool` with the binary implementation declared
    // via `implementation: native` in the artifact's own manifest.
    writeln!(zip, "runtime: tool").unwrap();
    writeln!(zip, "implementation: native").unwrap();
    if let Some(desc) = description {
        writeln!(zip, "description: {desc}").unwrap();
    }
    if let Some(schema) = input_schema {
        writeln!(zip, "input_schema: |").unwrap();
        writeln!(zip, "  {schema}").unwrap();
    }

    // Binary entry — canonical native layout places the binary at `bin/<name>`
    // with Unix executable permissions (0o755) via zip external attributes.
    let exec_options: SimpleFileOptions = FileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(0o755);
    zip.start_file(format!("bin/{name}"), exec_options).unwrap();
    zip.write_all(binary_script.as_bytes()).unwrap();

    zip.finish().unwrap();
    artifact_path
}

/// Create a skill artifact .mur.zip with a murmur.yaml and skill.md guidance file.
pub fn create_skill_artifact(
    dir: &Path,
    name: &str,
    version: &str,
    skill_content: &str,
) -> PathBuf {
    let artifact_path = dir.join(format!("{name}-{version}.mur.zip"));
    let file = fs::File::create(&artifact_path).unwrap();
    let mut zip = ZipWriter::new(file);
    let options: SimpleFileOptions =
        FileOptions::default().compression_method(CompressionMethod::Deflated);

    zip.start_file("murmur.yaml", options).unwrap();
    writeln!(zip, "name: {name}").unwrap();
    writeln!(zip, "version: {version}").unwrap();
    writeln!(zip, "runtime: skill").unwrap();

    zip.start_file("skill.md", options).unwrap();
    zip.write_all(skill_content.as_bytes()).unwrap();

    zip.finish().unwrap();
    artifact_path
}

/// Create a shell-desc-driver native artifact .mur.zip from a pre-built binary.
///
/// The zip contains murmur.yaml (with artifact_type: shell-desc-driver) and the
/// binary named exactly `<name>` at the archive root, as required by extract_native_binary.
pub fn create_shell_desc_driver_artifact(
    dir: &Path,
    name: &str,
    version: &str,
    binary_path: &Path,
) -> PathBuf {
    let artifact_path = dir.join(format!("{name}-{version}.mur.zip"));
    let file = fs::File::create(&artifact_path).unwrap();
    let mut zip = ZipWriter::new(file);
    let options: SimpleFileOptions =
        FileOptions::default().compression_method(CompressionMethod::Deflated);

    zip.start_file("murmur.yaml", options).unwrap();
    writeln!(zip, "name: {name}").unwrap();
    writeln!(zip, "version: {version}").unwrap();
    writeln!(zip, "runtime: native").unwrap();
    writeln!(zip, "artifact_type: shell-desc-driver").unwrap();
    writeln!(
        zip,
        "description: Writes enriched tool manifests for common shell binaries during stage_session."
    )
    .unwrap();

    let exec_options: SimpleFileOptions = FileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(0o755);
    zip.start_file(name, exec_options).unwrap();
    zip.write_all(&fs::read(binary_path).unwrap()).unwrap();

    zip.finish().unwrap();
    artifact_path
}

/// A pinned port, claimed for the lifetime of the test process.
///
/// For tests that need the number itself: to write into a manifest before the capsule that binds
/// it launches, or to assert on after the capsule releases it. An OS-assigned `:0` port serves
/// neither, because the number is only known once something has bound it, and once released it
/// can be handed straight back out to any other binder in this process.
///
/// Candidates come from below the Linux ephemeral range (32768–60999), so no `:0` bind anywhere in
/// the process can be assigned one, and a process-wide claim set keeps two tests from picking the
/// same number.
pub fn free_port() -> u16 {
    static CLAIMED: OnceLock<Mutex<(u16, HashSet<u16>)>> = OnceLock::new();
    let claimed = CLAIMED.get_or_init(|| {
        // Seeded from the pid so two concurrent `cargo test` invocations start in different
        // places rather than racing over the same first candidate.
        let seed = (std::process::id() % 8000) as u16;
        Mutex::new((20_000 + seed, HashSet::new()))
    });

    let mut guard = claimed.lock().unwrap();
    for _ in 0..4000 {
        let candidate = guard.0;
        guard.0 = if candidate >= 30_000 {
            20_000
        } else {
            candidate + 1
        };
        if !guard.1.insert(candidate) {
            continue;
        }
        // Bindable now, and nothing in this process can be handed it afterwards.
        if TcpListener::bind(("127.0.0.1", candidate)).is_ok() {
            return candidate;
        }
    }
    panic!("no free pinned port in 20000..30000");
}

pub struct ScriptedServer {
    pub endpoint: String,
    requests: Arc<Mutex<Vec<Value>>>,
    join: Option<thread::JoinHandle<()>>,
}

impl ScriptedServer {
    pub fn start(responses: Vec<String>) -> Self {
        Self::start_with_delay(responses, std::time::Duration::ZERO)
    }

    /// `start`, with `delay` slept before each response is written.
    ///
    /// For the one property that needs a launch to take measurable wall-clock time: a retention
    /// policy whose age window is shorter than the run itself must still leave the running
    /// session's own directory standing.
    pub fn start_with_delay(responses: Vec<String>, delay: std::time::Duration) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let endpoint = format!("http://{addr}");

        let requests = Arc::new(Mutex::new(Vec::new()));
        let requests_for_thread = Arc::clone(&requests);

        let join = thread::spawn(move || {
            for body in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let request_body = read_http_request_body(&mut stream).unwrap_or_default();
                let parsed = serde_json::from_str::<Value>(&request_body)
                    .unwrap_or_else(|_| json!({"_raw": request_body}));
                requests_for_thread.lock().unwrap().push(parsed);

                if !delay.is_zero() {
                    thread::sleep(delay);
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );

                stream.write_all(response.as_bytes()).unwrap();
                stream.flush().unwrap();
            }
        });

        Self {
            endpoint,
            requests,
            join: Some(join),
        }
    }

    pub fn requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for ScriptedServer {
    fn drop(&mut self) {
        // Drop the JoinHandle without joining — this detaches the thread.
        // If the ScriptedServer is dropped while the server thread is still
        // blocked in accept() (the agent made fewer requests than scripted),
        // we must not block the test runner. The detached thread will exit
        // once the test process ends.
        let _ = self.join.take();
    }
}

fn read_http_request_body(stream: &mut std::net::TcpStream) -> std::io::Result<String> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];

    let header_end;
    loop {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Ok(String::new());
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(index) = find_header_end(&buffer) {
            header_end = index;
            break;
        }
    }

    let headers = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let content_length = headers
        .lines()
        .find_map(|line| {
            let mut parts = line.splitn(2, ':');
            let key = parts.next()?.trim().to_ascii_lowercase();
            let value = parts.next()?.trim();
            (key == "content-length")
                .then(|| value.parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);

    let mut body = buffer[header_end + 4..].to_vec();
    while body.len() < content_length {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }

    Ok(String::from_utf8_lossy(&body[..content_length]).to_string())
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

/// The `tool_result` block a scripted server saw posted back for one `tool_use` id.
///
/// Scans every request in order, so the first post of a given id wins — a session that retries a
/// call still reports what the tool answered the first time.
pub fn find_tool_result(requests: &[Value], tool_id: &str) -> Option<Value> {
    for request in requests {
        for message in request.get("messages")?.as_array()? {
            if message.get("role").and_then(Value::as_str) != Some("user") {
                continue;
            }
            let Some(content) = message.get("content").and_then(Value::as_array) else {
                continue;
            };
            for block in content {
                if block.get("type").and_then(Value::as_str) == Some("tool_result")
                    && block.get("tool_use_id").and_then(Value::as_str) == Some(tool_id)
                {
                    return Some(block.clone());
                }
            }
        }
    }
    None
}

/// A `tool_result` block's text.
///
/// The runtime sends the tool's `data`, falling back to its `summary`, rather than the whole JSON
/// envelope. The Anthropic driver writes that as either a plain string or an array of
/// `{type: "text", text: …}` blocks, so both shapes have to be read here.
pub fn extract_result_text(tool_result: &Value) -> String {
    if let Some(text) = tool_result.get("content").and_then(Value::as_str) {
        return text.to_string();
    }
    tool_result
        .get("content")
        .and_then(|content| content.as_array())
        .and_then(|blocks| {
            blocks.iter().find_map(|block| {
                (block.get("type").and_then(Value::as_str) == Some("text"))
                    .then(|| block.get("text").and_then(Value::as_str))
                    .flatten()
                    .map(str::to_string)
            })
        })
        .unwrap_or_default()
}

/// `mur run --explain-scope --json`, which resolves the capability scope and creates nothing.
pub fn explain_scope_json(home: &TempDir, manifest: &Path) -> Value {
    let stdout = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .args([
            "run",
            "--manifest",
            manifest.to_str().unwrap(),
            "--json",
            "--explain-scope",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&stdout).expect("--explain-scope --json emits one JSON object")
}

/// The session workdir `mur run --verbose` reports in its startup block.
pub fn parse_workdir_from_stdout(stdout: &str) -> PathBuf {
    let marker = "workdir: ";
    let start = stdout
        .find(marker)
        .unwrap_or_else(|| panic!("missing '{marker}' in stdout:\n{stdout}"));
    PathBuf::from(
        stdout[start + marker.len()..]
            .lines()
            .next()
            .unwrap_or_default()
            .trim(),
    )
}
