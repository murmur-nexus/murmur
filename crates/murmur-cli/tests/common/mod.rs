#![allow(dead_code)]

use std::{
    collections::HashSet,
    fs,
    io::{Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
};

use assert_cmd::{assert::Assert, Command};
use capsule_runtime::{
    capability_policy_from_runtime_manifest, stage_session, ArtifactRequest, StageRequest,
    StagedSession,
};
use murmur_artifact::{load_runtime_manifest, ArtifactRuntime, LocalRegistry};
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
    cmd.args(["run", "--manifest", manifest_path.to_str().unwrap(), "--verbose"])
        .assert()
}

pub fn fixture_path(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(relative)
}

/// Root of a local `default-artifacts` checkout, used by tests marked
/// `#[ignore]` that depend on artifacts built there. Set
/// `MURMUR_DEFAULT_ARTIFACTS_DIR` to point at the checkout; without the
/// override, a checkout next to this repository is assumed.
pub fn default_artifacts_dir() -> PathBuf {
    match std::env::var_os("MURMUR_DEFAULT_ARTIFACTS_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../default-artifacts"),
    }
}

pub fn stage_agent_session(
    home: &TempDir,
    project_dir: &Path,
    manifest_path: &Path,
) -> StagedSession {
    let runtime_manifest = load_runtime_manifest(manifest_path).unwrap();

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
            context: runtime_manifest.context.clone(),
            otel_endpoint: None,
            eval_config_json: None,
            case_id: None,
            dataset_id: None,
            lifecycle: None,
            lifecycle_override: None,
            trace: None,
            workdir: None,
            bind_addr: "127.0.0.1".to_string(),
            internal_port: None,
            job_id: None,
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

pub struct ScriptedServer {
    pub endpoint: String,
    requests: Arc<Mutex<Vec<Value>>>,
    join: Option<thread::JoinHandle<()>>,
}

impl ScriptedServer {
    pub fn start(responses: Vec<String>) -> Self {
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
