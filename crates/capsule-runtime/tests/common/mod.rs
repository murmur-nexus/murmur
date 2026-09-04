//! A real daemon, a real registry, and the real `mur` binary, for tests that launch children as
//! operating-system processes.
//!
//! Nothing here is a double. The daemon is `mur_roost`'s own connection handler on a loopback
//! port, the registry is a `LocalRegistry` in a temporary directory, and the children are
//! subprocesses of the built `mur`. What a test observes is therefore what an operator observes.

#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{Cursor, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

use capsule_runtime::MUR_BINARY_ENV;
use mur_roost::{authority::SpawnAuthority, JobStatus, State};
use murmur_artifact::{ArtifactMeta, LocalRegistry, Registry, RuntimeType};
use serde_json::{json, Value};
use tempfile::TempDir;

/// The pre-built fixture components every suite packs, from `murmur-cli`'s fixture directory.
pub fn fixture_path(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("murmur-cli")
        .join("tests")
        .join("fixtures")
        .join(relative)
}

pub fn component(name: &str) -> PathBuf {
    fixture_path(&format!("run/components/{name}"))
}

/// The `mur` binary children are launched from, built up to date.
///
/// `cargo test -p capsule-runtime` builds this crate and not another package's binary, so a suite
/// whose whole subject is launching `mur` has to ask for it: an absent binary would make the suite
/// unrunnable.
///
/// **Only when it is absent.** `cargo build -p murmur-cli --bin mur` uplifts over
/// `target/<profile>/mur`, the one path every suite in the workspace reads, and it carries no
/// Cargo features. A workspace run has already put a binary there built with whatever features
/// that run asked for — `--features "beta-mur-topology beta-mur-deploy"` on CI — so building
/// unconditionally would replace it with a featureless one, and every suite ordered after this
/// crate would find a `mur` whose gated subcommands had gone.
///
/// [`MUR_BINARY_ENV`] overrides it, for a harness that has already built one.
pub fn mur_binary() -> &'static Path {
    static BINARY: OnceLock<PathBuf> = OnceLock::new();
    BINARY.get_or_init(|| {
        if let Some(path) = std::env::var_os(MUR_BINARY_ENV) {
            return PathBuf::from(path);
        }
        // .../target/<profile>/deps/<test binary>
        let profile_dir = std::env::current_exe()
            .expect("the test binary has a path")
            .parent()
            .and_then(Path::parent)
            .expect("the test binary lives under target/<profile>/deps")
            .to_path_buf();
        let candidate = profile_dir.join("mur");
        if !candidate.is_file() {
            let status = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
                .args(["build", "-p", "murmur-cli", "--bin", "mur"])
                .status()
                .expect("cargo build -p murmur-cli must run");
            assert!(status.success(), "failed to build the mur binary");
        }
        assert!(
            candidate.is_file(),
            "cargo built murmur-cli but {} is missing",
            candidate.display()
        );
        candidate
    })
}

// ── Artifacts ─────────────────────────────────────────────────────────────────

/// Pack a `.mur.zip` holding `murmur.yaml` and, when given, a root `capsule.wasm`, and publish it
/// into `registry_root`.
pub fn publish_capsule(
    registry_root: &Path,
    name: &str,
    version: &str,
    manifest_body: &str,
    component_path: Option<&Path>,
) {
    let mut cursor = Cursor::new(Vec::<u8>::new());
    {
        let mut zip = zip::ZipWriter::new(&mut cursor);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zip.start_file("murmur.yaml", options).unwrap();
        zip.write_all(format!("name: {name}\nversion: {version}\n{manifest_body}").as_bytes())
            .unwrap();
        if let Some(component_path) = component_path {
            zip.start_file("capsule.wasm", options).unwrap();
            zip.write_all(&std::fs::read(component_path).unwrap())
                .unwrap();
        }
        zip.finish().unwrap();
    }
    publish_bytes(registry_root, name, version, &cursor.into_inner());
}

/// Publish the inference driver an agent capsule needs, as a `driver` artifact.
pub fn publish_driver(registry_root: &Path, name: &str, version: &str, wasm_path: &Path) {
    let mut cursor = Cursor::new(Vec::<u8>::new());
    {
        let mut zip = zip::ZipWriter::new(&mut cursor);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zip.start_file("murmur.yaml", options).unwrap();
        zip.write_all(format!("name: {name}\nversion: {version}\nruntime: driver\n").as_bytes())
            .unwrap();
        zip.start_file("tool.wasm", options).unwrap();
        zip.write_all(&std::fs::read(wasm_path).unwrap()).unwrap();
        zip.finish().unwrap();
    }
    publish_bytes(registry_root, name, version, &cursor.into_inner());
}

fn publish_bytes(registry_root: &Path, name: &str, version: &str, bytes: &[u8]) {
    LocalRegistry::new(registry_root)
        .publish(
            ArtifactMeta {
                name: name.to_string(),
                version: version.to_string(),
                runtime: RuntimeType::Wasm,
                artifact_runtime: "capsule".to_string(),
                platforms: Vec::new(),
                description: None,
                tags: Vec::new(),
                wit_contracts: None,
            },
            bytes,
        )
        .unwrap();
}

// ── The daemon ────────────────────────────────────────────────────────────────

/// `mur-roost` on an ephemeral loopback port, over a temporary registry.
pub struct Roost {
    pub url: String,
    pub registry: TempDir,
    pub state: Arc<State>,
}

impl Roost {
    pub fn start(spawn_allow: &[&str]) -> Self {
        let registry = TempDir::new().unwrap();
        let path = registry.path().to_path_buf();
        Self::build(registry, path, spawn_allow)
    }

    /// The same daemon over a registry the caller owns — for a suite whose children resolve their
    /// own artifacts through `HOME`, which has to be the same store.
    pub fn start_at(registry_path: &Path, spawn_allow: &[&str]) -> Self {
        Self::build(
            TempDir::new().unwrap(),
            registry_path.to_path_buf(),
            spawn_allow,
        )
    }

    fn build(registry: TempDir, registry_path: PathBuf, spawn_allow: &[&str]) -> Self {
        let state = Arc::new(State {
            jobs: Arc::new(Mutex::new(HashMap::new())),
            registry_path,
            spawn_allow: spawn_allow.iter().map(|name| name.to_string()).collect(),
            max_depth: mur_roost::bounds::DEFAULT_MAX_DEPTH,
            // One parent session serves every case in a suite, so the children this daemon counts
            // are the suite's rather than one delegation's.
            max_concurrent: u32::MAX,
            authority: Arc::new(SpawnAuthority::generate().unwrap()),
        });

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let accept_state = Arc::clone(&state);
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let state = Arc::clone(&accept_state);
                thread::spawn(move || mur_roost::handle_connection(stream, state));
            }
        });

        Self {
            url,
            registry,
            state,
        }
    }

    pub fn registry_path(&self) -> &Path {
        &self.state.registry_path
    }

    pub fn publish(&self, name: &str, version: &str, body: &str, component_path: Option<&Path>) {
        publish_capsule(self.registry.path(), name, version, body, component_path);
    }

    /// Register a session directly, as that session's own runtime would, and take its credential.
    pub fn register(&self, session_id: &str, name: &str, version: &str) -> String {
        let response = self
            .post(
                "/register",
                &json!({"session_id": session_id, "name": name, "version": version}).to_string(),
                &[],
            )
            .expect("register must answer");
        response["credential"]
            .as_str()
            .unwrap_or_else(|| panic!("register was refused: {response}"))
            .to_string()
    }

    /// `POST /spawn` with a credential: ask for permission.
    pub fn permission(&self, credential: &str, name: &str, version: &str) -> Value {
        self.post(
            "/spawn",
            &json!({"name": name, "version": version}).to_string(),
            &[(capsule_runtime::SPAWN_CREDENTIAL_HEADER, credential)],
        )
        .expect("spawn must answer")
    }

    pub fn status(&self, session_id: &str) -> Option<String> {
        self.state
            .jobs
            .lock()
            .unwrap()
            .get(session_id)
            .map(|job| match job.status {
                JobStatus::Running => "running".to_string(),
                JobStatus::Complete => "complete".to_string(),
                JobStatus::Failed => "failed".to_string(),
            })
    }

    /// The same answer over the socket, so a test can assert what an operator's `curl` sees.
    pub fn status_over_http(&self, session_id: &str) -> Value {
        self.get(&format!("/status/{session_id}"))
            .expect("status must answer")
    }

    pub fn post(&self, path: &str, body: &str, headers: &[(&str, &str)]) -> Option<Value> {
        request("POST", &format!("{}{path}", self.url), Some(body), headers)
    }

    pub fn get(&self, path: &str) -> Option<Value> {
        request("GET", &format!("{}{path}", self.url), None, &[])
    }
}

/// One blocking HTTP/1.1 request, returning the parsed JSON body whatever the status.
pub fn request(
    method: &str,
    url: &str,
    body: Option<&str>,
    headers: &[(&str, &str)],
) -> Option<Value> {
    let rest = url.strip_prefix("http://")?;
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, "/"),
    };
    let mut stream = TcpStream::connect(authority).ok()?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(30)))
        .ok()?;
    let extra: String = headers
        .iter()
        .map(|(name, value)| format!("{name}: {value}\r\n"))
        .collect();
    let request = match body {
        Some(body) => format!(
            "POST {path} HTTP/1.1\r\nHost: {authority}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n{body}",
            body.len()
        ),
        None => format!(
            "{method} {path} HTTP/1.1\r\nHost: {authority}\r\n{extra}Connection: close\r\n\r\n"
        ),
    };
    stream.write_all(request.as_bytes()).ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    let (_, body) = response.split_once("\r\n\r\n")?;
    serde_json::from_str(body).ok()
}

// ── A scripted inference endpoint ─────────────────────────────────────────────

/// An HTTP endpoint that answers a fixed script of inference responses, so an agent capsule can
/// serve a task without a provider.
pub struct ScriptedServer {
    pub endpoint: String,
}

impl ScriptedServer {
    /// Answers every request with one end-of-turn assistant message.
    pub fn always_replying(text: &str) -> Self {
        Self::serving(json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "test-model",
            "content": [{"type": "text", "text": text}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        }))
    }

    fn serving(response: serde_json::Value) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let body = response.to_string();

        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let body = body.clone();
                thread::spawn(move || answer(stream, &body));
            }
        });

        Self { endpoint }
    }

    /// Host and port only, for `capabilities.network.allow`.
    pub fn authority(&self) -> &str {
        self.endpoint.trim_start_matches("http://")
    }
}

/// Read whatever the client sent and write one JSON body back, then close.
fn answer(mut stream: TcpStream, body: &str) {
    let mut buffer = [0u8; 65536];
    let _ = stream.read(&mut buffer);
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

// ── Filesystem helpers ────────────────────────────────────────────────────────

/// Every regular file beneath `root`, recursively.
pub fn files_under(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                found.push(path);
            }
        }
    }
    found
}

/// Whether `needle` appears in any file beneath `root`, naming the file when it does.
pub fn find_in_files(root: &Path, needle: &str) -> Option<PathBuf> {
    files_under(root).into_iter().find(|path| {
        std::fs::read(path)
            .map(|bytes| {
                bytes
                    .windows(needle.len())
                    .any(|window| window == needle.as_bytes())
            })
            .unwrap_or(false)
    })
}
