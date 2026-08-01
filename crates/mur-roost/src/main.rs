use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use capsule_runtime::{
    artifact::{extract_manifest_yaml, extract_root_wasm},
    capability_policy_from_runtime_manifest,
    launch_session, stage_session,
    ArtifactRequest, CapabilityPolicy, StageRequest,
};
use murmur_artifact::{current_platform, ArtifactRuntime, LocalRegistry, Registry};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ── CLI args ──────────────────────────────────────────────────────────────────

struct Args {
    port: u16,
    registry_path: PathBuf,
    spawn_allow: Vec<String>,
}

fn parse_args() -> Result<Args, String> {
    let raw: Vec<String> = std::env::args().collect();
    let mut port: u16 = 7700;
    let mut registry_path: Option<PathBuf> = None;
    let mut spawn_allow: Vec<String> = Vec::new();
    let mut i = 1;
    while i < raw.len() {
        match raw[i].as_str() {
            "--port" => {
                i += 1;
                port = raw.get(i).ok_or("--port requires a value")?.parse::<u16>()
                    .map_err(|e| format!("invalid --port: {e}"))?;
            }
            "--registry-path" => {
                i += 1;
                registry_path = Some(PathBuf::from(raw.get(i).ok_or("--registry-path requires a value")?));
            }
            "--spawn-allow" => {
                i += 1;
                let val = raw.get(i).ok_or("--spawn-allow requires a value")?;
                spawn_allow.push(val.clone());
            }
            other if other.starts_with("--spawn-allow=") => {
                spawn_allow.push(other["--spawn-allow=".len()..].to_string());
            }
            other => return Err(format!("unknown argument: {other}")),
        }
        i += 1;
    }
    let registry_path = registry_path
        .or_else(|| std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".murmur").join("artifacts")))
        .ok_or("--registry-path is required")?;
    Ok(Args { port, registry_path, spawn_allow })
}

// ── Job store ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
enum JobStatus {
    Running,
    Complete,
    Failed,
}

#[derive(Debug, Clone)]
struct JobRecord {
    status: JobStatus,
    capability_policy: CapabilityPolicy,
}

type JobStore = Arc<Mutex<HashMap<String, JobRecord>>>;

// ── Shared server state ───────────────────────────────────────────────────────

#[derive(Clone)]
struct State {
    jobs: JobStore,
    registry_path: PathBuf,
    spawn_allow: Vec<String>,
}

// ── Spawn request / response types ───────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct SpawnRequest {
    name: String,
    version: String,
    workdir: String,
    #[serde(default)]
    spawned_by: Option<String>,
}

#[derive(Serialize)]
struct SpawnResponse {
    job_id: String,
    capsule_url: String,
}

// ── HTTP helpers ──────────────────────────────────────────────────────────────

fn http_response(status: u16, reason: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(),
        body,
    )
}

fn ok(body: &str) -> String {
    http_response(200, "OK", body)
}

fn err(status: u16, reason: &str, message: &str) -> String {
    let body = format!(r#"{{"error":{message:?}}}"#);
    http_response(status, reason, &body)
}

// ── Connection handler ────────────────────────────────────────────────────────

fn handle_connection(mut stream: TcpStream, state: Arc<State>) {
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(10))).ok();

    // Read request line
    let mut buf = [0u8; 8192];
    let n = match stream.read(&mut buf) {
        Ok(0) | Err(_) => return,
        Ok(n) => n,
    };
    let raw = String::from_utf8_lossy(&buf[..n]);

    // Parse method and path from request line
    let mut lines = raw.lines();
    let request_line = match lines.next() {
        Some(l) => l,
        None => return,
    };
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");

    // Find Content-Length and body
    let mut content_length: usize = 0;
    let mut header_end = 0;
    if let Some(pos) = raw.find("\r\n\r\n") {
        header_end = pos + 4;
        for line in raw[..pos].lines().skip(1) {
            let lower = line.to_ascii_lowercase();
            if let Some(rest) = lower.strip_prefix("content-length:") {
                content_length = rest.trim().parse().unwrap_or(0);
            }
        }
    }

    // Read remaining body bytes if content_length > what we already have
    let already_read = n.saturating_sub(header_end);
    let body_bytes_in_buf = &buf[header_end..header_end + already_read.min(n - header_end)];
    let body_str = if content_length > 0 {
        let mut body = body_bytes_in_buf.to_vec();
        while body.len() < content_length {
            let mut chunk = vec![0u8; content_length - body.len()];
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(m) => body.extend_from_slice(&chunk[..m]),
            }
        }
        String::from_utf8_lossy(&body).to_string()
    } else {
        String::from_utf8_lossy(body_bytes_in_buf).to_string()
    };

    let response = route(method, path, &body_str, &state);
    let _ = stream.write_all(response.as_bytes());
}

fn route(method: &str, path: &str, body: &str, state: &Arc<State>) -> String {
    match (method, path) {
        ("GET", "/health") => ok(r#"{}"#),
        ("POST", "/spawn") => handle_spawn(body, state),
        ("GET", p) if p.starts_with("/status/") => {
            let job_id = &p["/status/".len()..];
            handle_status(job_id, state)
        }
        _ => err(404, "Not Found", "not found"),
    }
}

// ── POST /spawn ───────────────────────────────────────────────────────────────

fn handle_spawn(body: &str, state: &Arc<State>) -> String {
    let req: SpawnRequest = match serde_json::from_str(body) {
        Ok(r) => r,
        Err(e) => return err(400, "Bad Request", &format!("invalid JSON: {e}")),
    };

    // Authorization check
    if let Some(ref parent_job_id) = req.spawned_by {
        // Parent capsule is spawning — check parent's spawn_allow
        let jobs = state.jobs.lock().unwrap();
        let Some(parent) = jobs.get(parent_job_id) else {
            return err(403, "Forbidden", &format!("unknown parent job_id '{parent_job_id}'"));
        };
        if !parent.capability_policy.spawn_allow.contains(&req.name) {
            return err(
                403,
                "Forbidden",
                &format!("capsule '{}' is not in parent's spawn_allow", req.name),
            );
        }
        drop(jobs);
    } else {
        // Top-level spawn — check CLI --spawn-allow list
        if !state.spawn_allow.contains(&req.name) {
            return err(
                403,
                "Forbidden",
                &format!("capsule '{}' is not in --spawn-allow", req.name),
            );
        }
    }

    // Resolve manifest from registry
    let registry = LocalRegistry::new(&state.registry_path);
    let platform = current_platform();
    let resolved = match registry.resolve_with_platform(&req.name, &req.version, Some(platform)) {
        Ok(r) => r,
        Err(e) => {
            return err(
                500,
                "Internal Server Error",
                &format!("registry error for '{}/{}': {e}", req.name, req.version),
            )
        }
    };

    let manifest_yaml = match extract_manifest_yaml(&req.name, &req.version, &resolved.bytes) {
        Ok(y) => y,
        Err(e) => return err(500, "Internal Server Error", &format!("manifest extraction failed: {e}")),
    };

    // Parse RuntimeManifest from an in-memory temp file (load_runtime_manifest reads from disk;
    // use from_yaml_str directly from murmur-artifact's public API).
    let manifest = match murmur_artifact::RuntimeManifest::from_yaml_str(&manifest_yaml) {
        Ok(m) => m,
        Err(e) => return err(500, "Internal Server Error", &format!("manifest parse error: {e}")),
    };

    let child_policy = capability_policy_from_runtime_manifest(&manifest);

    // For agent capsules: no WASM bytes needed.
    // For script capsules: extract WASM from the zip.
    let capsule_component_bytes = if manifest.inference.is_some() {
        Vec::new()
    } else {
        match extract_root_wasm(&req.name, &req.version, &resolved.bytes) {
            Ok(bytes) => bytes,
            Err(e) => return err(500, "Internal Server Error", &format!("WASM extraction failed: {e}")),
        }
    };

    // Build artifact list for StageRequest
    let mut allowlisted_tools = std::collections::HashSet::new();
    let artifacts: Vec<ArtifactRequest> = manifest
        .artifacts
        .iter()
        .map(|a| {
            if a.runtime == ArtifactRuntime::Tool {
                allowlisted_tools.insert(a.name.clone());
            }
            ArtifactRequest {
                name: a.name.clone(),
                version: a.version.clone(),
                runtime: a.runtime.clone(),
                source: a.source.clone(),
                capabilities: a.capabilities.clone(),
            }
        })
        .collect();

    // Generate job_id before building StageRequest so it can be injected as MURMUR_JOB_ID
    // into the child capsule's environment, enabling it to set spawned_by on sub-spawns.
    let job_id = Uuid::new_v4().to_string();

    let workdir_path = PathBuf::from(&req.workdir);
    let stage_request = StageRequest {
        manifest_dir: workdir_path.clone(),
        capsule_name: manifest.name.clone(),
        capsule_version: manifest.version.clone(),
        capsule_component_bytes,
        artifacts,
        allowlisted_tools,
        lock_expectations: None,
        capability_policy: child_policy.clone(),
        inference: manifest.inference.clone(),
        context: manifest.context.clone(),
        otel_endpoint: None,
        eval_config_json: None,
        case_id: None,
        dataset_id: None,
        lifecycle: manifest.lifecycle.clone(),
        lifecycle_override: None,
        trace: None,
        workdir: Some(workdir_path),
        bind_addr: "127.0.0.1".to_string(),
        internal_port: manifest.network.as_ref().and_then(|n| n.internal_port),
        job_id: Some(job_id.clone()),
        // The spawned child's own manifest is the only source of a floor here — roost has no
        // CLI flag and reads no workspace config on the child's behalf.
        declared_containment_floor: manifest
            .capabilities
            .as_ref()
            .and_then(|capabilities| capabilities.containment)
            .unwrap_or_default(),
    };
    {
        let mut jobs = state.jobs.lock().unwrap();
        jobs.insert(
            job_id.clone(),
            JobRecord {
                status: JobStatus::Running,
                capability_policy: child_policy,
            },
        );
    }

    // Launch in a background thread; communicate the capsule_url via channel
    let (url_tx, url_rx) = std::sync::mpsc::channel::<Result<String, String>>();
    let job_id_bg = job_id.clone();
    let jobs_bg = Arc::clone(&state.jobs);
    let registry_path_bg = state.registry_path.clone();

    thread::spawn(move || {
        let registry = LocalRegistry::new(&registry_path_bg);
        let staged = match stage_session(Arc::new(registry), stage_request) {
            Ok(s) => s,
            Err(e) => {
                url_tx.send(Err(format!("stage_session failed: {e}"))).ok();
                let mut jobs = jobs_bg.lock().unwrap();
                if let Some(job) = jobs.get_mut(&job_id_bg) {
                    job.status = JobStatus::Failed;
                }
                return;
            }
        };

        let url_tx_inner = url_tx;
        let result = launch_session(staged, |url| {
            // url is "localhost:{port}" — promote to "http://localhost:{port}"
            let capsule_url = format!("http://{url}");
            url_tx_inner.send(Ok(capsule_url)).ok();
        });

        let mut jobs = jobs_bg.lock().unwrap();
        if let Some(job) = jobs.get_mut(&job_id_bg) {
            job.status = match result {
                Ok(_) => JobStatus::Complete,
                Err(_) => JobStatus::Failed,
            };
        }
    });

    // Wait up to 60s for the capsule to bind its port
    let capsule_url = match url_rx.recv_timeout(Duration::from_secs(60)) {
        Ok(Ok(url)) => url,
        Ok(Err(e)) => {
            let mut jobs = state.jobs.lock().unwrap();
            if let Some(job) = jobs.get_mut(&job_id) {
                job.status = JobStatus::Failed;
            }
            return err(500, "Internal Server Error", &format!("capsule launch failed: {e}"));
        }
        Err(_) => {
            let mut jobs = state.jobs.lock().unwrap();
            if let Some(job) = jobs.get_mut(&job_id) {
                job.status = JobStatus::Failed;
            }
            return err(
                500,
                "Internal Server Error",
                "capsule did not bind a port within 60s",
            );
        }
    };

    let response = SpawnResponse { job_id, capsule_url };
    ok(&serde_json::to_string(&response).unwrap_or_else(|_| r#"{"error":"serialization failed"}"#.to_string()))
}

// ── GET /status/:job_id ───────────────────────────────────────────────────────

fn handle_status(job_id: &str, state: &Arc<State>) -> String {
    let jobs = state.jobs.lock().unwrap();
    match jobs.get(job_id) {
        Some(job) => {
            let status = match job.status {
                JobStatus::Running => "running",
                JobStatus::Complete => "complete",
                JobStatus::Failed => "failed",
            };
            ok(&format!(r#"{{"status":"{status}"}}"#))
        }
        None => err(404, "Not Found", "job not found"),
    }
}

// ── main ──────────────────────────────────────────────────────────────────────

fn main() {
    if let Err(e) = capsule_runtime::security::harden_process_dumpable() {
        eprintln!("mur-roost: warning: failed to harden process against /proc environ reads: {e}");
    }

    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("mur-roost: {e}");
            std::process::exit(1);
        }
    };

    let state = Arc::new(State {
        jobs: Arc::new(Mutex::new(HashMap::new())),
        registry_path: args.registry_path,
        spawn_allow: args.spawn_allow,
    });

    let listener = match TcpListener::bind(format!("127.0.0.1:{}", args.port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("mur-roost: failed to bind port {}: {e}", args.port);
            std::process::exit(1);
        }
    };

    eprintln!("mur-roost: listening on 127.0.0.1:{}", args.port);

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let state = Arc::clone(&state);
                thread::spawn(move || handle_connection(stream, state));
            }
            Err(e) => eprintln!("mur-roost: accept error: {e}"),
        }
    }
}
