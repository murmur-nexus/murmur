//! The daemon's request handling: the job store, the three endpoints, and the spawn referee that
//! keeps a child capsule inside the capability envelope of the capsule that asked for it.
//!
//! Split from the binary so the endpoints can be driven directly by tests — [`route`] is the same
//! entry point [`handle_connection`] reaches, with the socket removed.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use capsule_runtime::{
    artifact::{extract_manifest_yaml, extract_root_wasm},
    capability_policy_from_runtime_manifest, launch_session, stage_session, ArtifactRequest,
    SpawnEnvelope, StageRequest,
};
use murmur_artifact::{current_platform, ArtifactRuntime, LocalRegistry, Registry};
use serde::{Deserialize, Serialize};

// ── Job store ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobStatus {
    Running,
    Complete,
    Failed,
}

#[derive(Debug, Clone)]
pub struct JobRecord {
    pub status: JobStatus,
    /// What this session holds, and therefore the ceiling every capsule it spawns must fit inside.
    ///
    /// Lowered from the session's own manifest when it was staged. That is the seam a later slice
    /// replaces: an envelope selected by a `spawned_by` the daemon does not authenticate is a
    /// ceiling any caller can name, so the approval belongs on a credential minted at
    /// *registration* rather than on a lookup in this map.
    pub envelope: SpawnEnvelope,
}

pub type JobStore = Arc<Mutex<HashMap<String, JobRecord>>>;

// ── Shared server state ───────────────────────────────────────────────────────

#[derive(Clone)]
pub struct State {
    pub jobs: JobStore,
    pub registry_path: PathBuf,
    pub spawn_allow: Vec<String>,
}

// ── Spawn request / response types ───────────────────────────────────────────

/// The `POST /spawn` body.
///
/// Carries no manifest and no capabilities, and must not gain either: the grants a child is
/// refereed against are the ones in the *registry* manifest the daemon resolves for itself. A
/// request that could supply them would be a request that could declare its own ceiling.
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
    /// The runtime's own session id for the spawned capsule (`ses_…`).
    ///
    /// roost does not mint an identifier of its own: `stage_session` already returns one, it is
    /// the id the capsule knows itself by (`MURMUR_SESSION_ID`), the one its traces carry, and
    /// the one `mur run` prints. A second id for the same thing would only have to be correlated
    /// back to this one. It is what a child passes as `spawned_by`, so a spawn chain reads
    /// end-to-end against existing trace data.
    session_id: String,
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

pub fn handle_connection(mut stream: TcpStream, state: Arc<State>) {
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

/// Dispatch one already-read request and return the complete HTTP response text.
pub fn route(method: &str, path: &str, body: &str, state: &Arc<State>) -> String {
    match (method, path) {
        ("GET", "/health") => ok(r#"{}"#),
        ("POST", "/spawn") => handle_spawn(body, state),
        ("GET", p) if p.starts_with("/status/") => {
            let session_id = &p["/status/".len()..];
            handle_status(session_id, state)
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

    // Authorization check.
    //
    // Two separate questions, asked in order and never merged. This one is *which* capsules the
    // parent may spawn — the operator's own name list. The envelope comparison further down is
    // *how much* any of them may hold. A name missing from the list is refused here even when its
    // grants would pass the envelope, so the message an operator gets names the list they have to
    // edit.
    let parent_envelope = if let Some(ref parent_session_id) = req.spawned_by {
        // Parent capsule is spawning — check parent's spawn_allow
        let jobs = state.jobs.lock().unwrap();
        let Some(parent) = jobs.get(parent_session_id) else {
            return err(
                403,
                "Forbidden",
                &format!("unknown parent session '{parent_session_id}'"),
            );
        };
        if !parent.envelope.spawn_allow.contains(&req.name) {
            return err(
                403,
                "Forbidden",
                &format!("capsule '{}' is not in parent's spawn_allow", req.name),
            );
        }
        Some(parent.envelope.clone())
    } else {
        // Top-level spawn — check CLI --spawn-allow list. There is no parent, so there is no
        // envelope to be within: the operator who started the daemon named this capsule directly.
        if !state.spawn_allow.contains(&req.name) {
            return err(
                403,
                "Forbidden",
                &format!("capsule '{}' is not in --spawn-allow", req.name),
            );
        }
        None
    };

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
        Err(e) => {
            return err(
                500,
                "Internal Server Error",
                &format!("manifest extraction failed: {e}"),
            )
        }
    };

    // Parse RuntimeManifest from an in-memory temp file (load_runtime_manifest reads from disk;
    // use from_yaml_str directly from murmur-artifact's public API).
    let manifest = match murmur_artifact::RuntimeManifest::from_yaml_str(&manifest_yaml) {
        Ok(m) => m,
        Err(e) => {
            return err(
                500,
                "Internal Server Error",
                &format!("manifest parse error: {e}"),
            )
        }
    };

    let child_policy = capability_policy_from_runtime_manifest(&manifest);
    let child_envelope = SpawnEnvelope::from_runtime_manifest(&manifest);

    // The referee, decided before the child's component bytes are read and long before anything is
    // staged, created or launched: a refused spawn leaves no workdir, no session, no trace and no
    // job record behind.
    if let Some(parent_envelope) = parent_envelope.as_ref() {
        if let Err(violation) = parent_envelope.contains(&child_envelope) {
            return err(403, "Forbidden", &violation.to_string());
        }
    }

    // For agent capsules: no WASM bytes needed.
    // For script capsules: extract WASM from the zip.
    let capsule_component_bytes = if manifest.inference.is_some() {
        Vec::new()
    } else {
        match extract_root_wasm(&req.name, &req.version, &resolved.bytes) {
            Ok(bytes) => bytes,
            Err(e) => {
                return err(
                    500,
                    "Internal Server Error",
                    &format!("WASM extraction failed: {e}"),
                )
            }
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
                on_overflow: a.on_overflow,
                config: a.config.clone(),
                capabilities: a.capabilities.clone(),
            }
        })
        .collect();

    let workdir_path = PathBuf::from(&req.workdir);
    let stage_request = StageRequest {
        manifest_dir: workdir_path.clone(),
        capsule_name: manifest.name.clone(),
        capsule_version: manifest.version.clone(),
        capsule_component_bytes,
        artifacts,
        allowlisted_tools,
        lock_expectations: None,
        capability_policy: child_policy,
        inference: manifest.inference.clone(),
        system_prompt_overridden: false,
        context: manifest.context.clone(),
        context_id: None,
        resume: None,
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
        // The spawned child's own manifest is the only source of a floor here — roost has no
        // CLI flag and reads no workspace config on the child's behalf.
        declared_containment_floor: manifest
            .capabilities
            .as_ref()
            .and_then(|capabilities| capabilities.containment)
            .unwrap_or_default(),
        exports: manifest.exports.clone(),
    };
    // The job is registered inside the launch thread rather than here, because its key is the
    // session id and `stage_session` is what mints one. A spawn that fails to stage never
    // becomes a job at all — nothing was started, and the caller is told synchronously below.
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(String, String), String>>();
    let jobs_bg = Arc::clone(&state.jobs);
    let registry_path_bg = state.registry_path.clone();

    thread::spawn(move || {
        let registry = LocalRegistry::new(&registry_path_bg);
        let staged = match stage_session(Arc::new(registry), stage_request) {
            Ok(staged) => staged,
            Err(e) => {
                ready_tx
                    .send(Err(format!("stage_session failed: {e}")))
                    .ok();
                return;
            }
        };
        let session_id = staged.session_id.clone();

        {
            let mut jobs = jobs_bg.lock().unwrap();
            jobs.insert(
                session_id.clone(),
                JobRecord {
                    status: JobStatus::Running,
                    envelope: child_envelope,
                },
            );
        }

        let session_id_cb = session_id.clone();
        let result = launch_session(staged, move |url| {
            // url is "localhost:{port}" — promote to "http://localhost:{port}"
            ready_tx
                .send(Ok((session_id_cb.clone(), format!("http://{url}"))))
                .ok();
        });

        let mut jobs = jobs_bg.lock().unwrap();
        if let Some(job) = jobs.get_mut(&session_id) {
            job.status = match result {
                Ok(_) => JobStatus::Complete,
                Err(_) => JobStatus::Failed,
            };
        }
    });

    // Wait up to 60s for the capsule to bind its port
    let (session_id, capsule_url) = match ready_rx.recv_timeout(Duration::from_secs(60)) {
        Ok(Ok(ready)) => ready,
        Ok(Err(e)) => {
            return err(
                500,
                "Internal Server Error",
                &format!("capsule launch failed: {e}"),
            )
        }
        Err(_) => {
            return err(
                500,
                "Internal Server Error",
                "capsule did not bind a port within 60s",
            )
        }
    };

    let response = SpawnResponse {
        session_id,
        capsule_url,
    };
    ok(&serde_json::to_string(&response)
        .unwrap_or_else(|_| r#"{"error":"serialization failed"}"#.to_string()))
}

// ── GET /status/:session_id ───────────────────────────────────────────────────

fn handle_status(session_id: &str, state: &Arc<State>) -> String {
    let jobs = state.jobs.lock().unwrap();
    match jobs.get(session_id) {
        Some(job) => {
            let status = match job.status {
                JobStatus::Running => "running",
                JobStatus::Complete => "complete",
                JobStatus::Failed => "failed",
            };
            ok(&format!(r#"{{"status":"{status}"}}"#))
        }
        None => err(404, "Not Found", "session not found"),
    }
}
