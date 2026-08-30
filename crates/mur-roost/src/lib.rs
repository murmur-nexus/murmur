//! The daemon's request handling: the job store, the three endpoints, and the spawn referee that
//! keeps a child capsule inside the capability envelope of the capsule that asked for it.
//!
//! Split from the binary so the endpoints can be driven directly by tests — [`route`] is the same
//! entry point [`handle_connection`] reaches, with the socket removed.
//!
//! A delegated spawn is a two-step exchange. `POST /delegate` proves which session is asking (an
//! opaque credential minted for that session at launch), runs the referee, and returns an approval
//! naming the exact artifact it resolved. `POST /spawn` redeems that approval, once, before it
//! expires, and launches the artifact the approval names and nothing else. The credential is the
//! authority on who is asking; the request body's `spawned_by` is at most a claim the credential
//! has to agree with.

pub mod authority;

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
    SpawnCredential, SpawnEnvelope, StageRequest, SPAWN_APPROVAL_HEADER, SPAWN_CREDENTIAL_HEADER,
};
use murmur_artifact::{current_platform, ArtifactRuntime, LocalRegistry, Registry};
use serde::{Deserialize, Serialize};

use crate::authority::{
    now_ms, ApprovalPayload, AuthorityError, SpawnAuthority, SPAWN_APPROVAL_TTL_SECS,
};

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
    /// Lowered from the session's own manifest when it was staged. `handle_delegate` selects the
    /// record by the session id the presented credential names, never by a session id the request
    /// claims: the credential is minted by this daemon and handed to that session's runtime alone,
    /// so a caller cannot be judged against the envelope of a session it does not hold.
    pub envelope: SpawnEnvelope,
}

pub type JobStore = Arc<Mutex<HashMap<String, JobRecord>>>;

// ── Shared server state ───────────────────────────────────────────────────────

#[derive(Clone)]
pub struct State {
    pub jobs: JobStore,
    pub registry_path: PathBuf,
    pub spawn_allow: Vec<String>,
    /// Mints and verifies every credential and approval this daemon issues. Generated once in
    /// `main` and dropped when the process exits, so a token from a previous daemon verifies
    /// against nothing.
    pub authority: Arc<SpawnAuthority>,
}

/// Mints the credential a freshly staged session will present when it asks to spawn.
///
/// Returns `None` for a session that cannot delegate at all — a capsule with an empty
/// `capabilities.spawn.allow` has nothing to ask for, so it is handed no secret and its behaviour
/// is unchanged.
pub fn mint_session_credential(
    authority: &SpawnAuthority,
    session_id: &str,
    envelope: &SpawnEnvelope,
) -> Option<SpawnCredential> {
    if envelope.spawn_allow.is_empty() {
        return None;
    }
    match authority.mint_credential(session_id) {
        Ok(credential) => Some(credential),
        Err(error) => {
            eprintln!("mur-roost: failed to mint a spawn credential for {session_id}: {error}");
            None
        }
    }
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

// ── Request headers ───────────────────────────────────────────────────────────

/// One request's headers, keyed by lowercased name.
///
/// Kept as its own type rather than a bare map so [`route`] cannot be handed a map whose keys were
/// never normalised: HTTP header names are case-insensitive, and a credential presented as
/// `X-Murmur-Spawn-Credential` has to be the credential presented as `x-murmur-spawn-credential`.
#[derive(Debug, Clone, Default)]
pub struct RequestHeaders(HashMap<String, String>);

impl RequestHeaders {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, name: &str, value: &str) {
        self.0
            .insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.get(&name.to_ascii_lowercase()).map(String::as_str)
    }
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

    // Collect every header, and the content length, in one pass.
    let mut content_length: usize = 0;
    let mut headers = RequestHeaders::new();
    let mut header_end = 0;
    if let Some(pos) = raw.find("\r\n\r\n") {
        header_end = pos + 4;
        for line in raw[..pos].lines().skip(1) {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            headers.insert(name, value);
            if name.trim().eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap_or(0);
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

    let response = route(method, path, &headers, &body_str, &state);
    let _ = stream.write_all(response.as_bytes());
}

/// Dispatch one already-read request and return the complete HTTP response text.
pub fn route(
    method: &str,
    path: &str,
    headers: &RequestHeaders,
    body: &str,
    state: &Arc<State>,
) -> String {
    match (method, path) {
        ("GET", "/health") => ok(r#"{}"#),
        ("POST", "/delegate") => handle_delegate(headers, body, state),
        ("POST", "/spawn") => handle_spawn(headers, body, state),
        ("GET", p) if p.starts_with("/status/") => {
            let session_id = &p["/status/".len()..];
            handle_status(session_id, state)
        }
        _ => err(404, "Not Found", "not found"),
    }
}

// ── Refusals ──────────────────────────────────────────────────────────────────

/// The one answer every identity failure gets, on either endpoint.
///
/// Names no session id, no capsule name, no manifest key and no envelope axis, and is returned
/// byte-for-byte identically whether the credential was absent, malformed, minted by a previous
/// daemon, or names a session this daemon has never heard of. Two requests differing only in
/// whether the session they name exists are indistinguishable, so the endpoint is not an oracle
/// for which sessions are running.
const IDENTITY_REFUSAL: &str = "not authorised: a spawn must present a credential and an approval minted for the same running session";

fn identity_refused() -> String {
    err(403, "Forbidden", IDENTITY_REFUSAL)
}

// ── Shared resolution ─────────────────────────────────────────────────────────

/// Resolves one capsule from the registry and parses its manifest.
///
/// The `Err` side is a complete HTTP response rather than a message: every failure here is a
/// server-side `500` about the registry or the artifact, and the caller has nothing to add.
fn resolve_capsule(
    state: &Arc<State>,
    name: &str,
    version: &str,
) -> Result<
    (
        murmur_artifact::ResolvedArtifact,
        murmur_artifact::RuntimeManifest,
    ),
    String,
> {
    let registry = LocalRegistry::new(&state.registry_path);
    let resolved = registry
        .resolve_with_platform(name, version, Some(current_platform()))
        .map_err(|e| {
            err(
                500,
                "Internal Server Error",
                &format!("registry error for '{name}/{version}': {e}"),
            )
        })?;

    let manifest_yaml = extract_manifest_yaml(name, version, &resolved.bytes).map_err(|e| {
        err(
            500,
            "Internal Server Error",
            &format!("manifest extraction failed: {e}"),
        )
    })?;

    // `from_yaml_str` rather than `load_runtime_manifest`: the manifest comes out of the artifact
    // zip in memory and is never written to disk.
    let manifest =
        murmur_artifact::RuntimeManifest::from_yaml_str(&manifest_yaml).map_err(|e| {
            err(
                500,
                "Internal Server Error",
                &format!("manifest parse error: {e}"),
            )
        })?;

    Ok((resolved, manifest))
}

/// The envelope of the session a credential names, or an identity refusal.
///
/// The two questions this answers — is the credential one of ours, and is the session it names
/// running — collapse into one outcome on purpose: a caller learns nothing about which sessions
/// exist.
fn session_envelope(state: &Arc<State>, session_id: &str) -> Result<SpawnEnvelope, String> {
    let jobs = state.jobs.lock().unwrap();
    match jobs.get(session_id) {
        Some(job) if job.status == JobStatus::Running => Ok(job.envelope.clone()),
        _ => Err(identity_refused()),
    }
}

// ── POST /delegate ────────────────────────────────────────────────────────────

/// The `POST /delegate` body. Names a capsule to be approved and nothing else: the workdir, and
/// everything else a launch needs, belongs to the `POST /spawn` that redeems the approval.
#[derive(Debug, Deserialize)]
struct DelegateRequest {
    name: String,
    version: String,
}

#[derive(Serialize)]
struct DelegateResponse {
    approval: String,
    /// Absolute expiry in unix milliseconds, so a caller can tell a stale approval from a refused
    /// one without having to guess this daemon's TTL.
    expires_at_ms: u64,
}

/// Ask for permission to spawn a capsule.
///
/// This is where the referee runs, and the only place it runs: the approval this returns pins the
/// artifact by content hash, and the content hash is what determines the manifest the referee
/// read. Running it again at `/spawn` would imply the digest binding is not trusted.
fn handle_delegate(headers: &RequestHeaders, body: &str, state: &Arc<State>) -> String {
    let req: DelegateRequest = match serde_json::from_str(body) {
        Ok(r) => r,
        Err(e) => return err(400, "Bad Request", &format!("invalid JSON: {e}")),
    };

    let Some(credential) = headers.get(SPAWN_CREDENTIAL_HEADER) else {
        return identity_refused();
    };
    let Ok(session_id) = state.authority.verify_credential(credential) else {
        return identity_refused();
    };
    let parent_envelope = match session_envelope(state, &session_id) {
        Ok(envelope) => envelope,
        Err(response) => return response,
    };

    // Authorization check.
    //
    // Two separate questions, asked in order and never merged. This one is *which* capsules the
    // parent may spawn — the operator's own name list. The envelope comparison further down is
    // *how much* any of them may hold. A name missing from the list is refused here even when its
    // grants would pass the envelope, so the message an operator gets names the list they have to
    // edit.
    if !parent_envelope.spawn_allow.contains(&req.name) {
        return err(
            403,
            "Forbidden",
            &format!("capsule '{}' is not in parent's spawn_allow", req.name),
        );
    }

    let (resolved, manifest) = match resolve_capsule(state, &req.name, &req.version) {
        Ok(pair) => pair,
        Err(response) => return response,
    };

    // The referee, decided before the child's component bytes are read and long before anything is
    // staged, created or launched: a refused delegation leaves no workdir, no session, no trace and
    // no job record.
    let child_envelope = SpawnEnvelope::from_runtime_manifest(&manifest);
    if let Err(violation) = parent_envelope.contains(&child_envelope) {
        return err(403, "Forbidden", &violation.to_string());
    }

    let expires_at_ms = now_ms() + SPAWN_APPROVAL_TTL_SECS * 1_000;
    let approval = match state.authority.mint_approval_token(
        &session_id,
        &resolved.meta.name,
        &resolved.meta.version,
        &resolved.sha256,
        expires_at_ms,
    ) {
        Ok(token) => token,
        Err(e) => {
            return err(
                500,
                "Internal Server Error",
                &format!("failed to mint a spawn approval: {e}"),
            )
        }
    };

    let response = DelegateResponse {
        approval,
        expires_at_ms,
    };
    ok(&serde_json::to_string(&response)
        .unwrap_or_else(|_| r#"{"error":"serialization failed"}"#.to_string()))
}

// ── POST /spawn ───────────────────────────────────────────────────────────────

/// Launch a capsule: the operator's own top-level path, or the redemption of an approval.
///
/// The referee does not run here. On the delegated path it already ran, at `/delegate`, against
/// the manifest of the artifact whose digest the approval names — so re-resolving the same digest
/// can only reach the same answer. On the top-level path there is no parent to be within.
fn handle_spawn(headers: &RequestHeaders, body: &str, state: &Arc<State>) -> String {
    let req: SpawnRequest = match serde_json::from_str(body) {
        Ok(r) => r,
        Err(e) => return err(400, "Bad Request", &format!("invalid JSON: {e}")),
    };

    // Which of the three shapes this request is. A request carrying exactly one of the two headers
    // is refused outright rather than falling through to the operator's `--spawn-allow` gate: half
    // an exchange is a failed exchange, not a request for the other path.
    let approved: Option<ApprovalPayload> = match (
        headers.get(SPAWN_CREDENTIAL_HEADER),
        headers.get(SPAWN_APPROVAL_HEADER),
    ) {
        (None, None) => {
            // The operator's own path. `spawned_by` here is a claim with nothing behind it, so a
            // request that carries one and proves nothing is refused rather than quietly ignored:
            // ignoring it would launch under the operator's list a request that asked to be
            // judged as somebody else.
            if req.spawned_by.is_some() {
                return identity_refused();
            }
            // There is no parent, so there is no envelope to be within: the operator who started
            // the daemon named this capsule directly.
            if !state.spawn_allow.contains(&req.name) {
                return err(
                    403,
                    "Forbidden",
                    &format!("capsule '{}' is not in --spawn-allow", req.name),
                );
            }
            None
        }
        (Some(credential), Some(approval)) => {
            let Ok(session_id) = state.authority.verify_credential(credential) else {
                return identity_refused();
            };
            if session_envelope(state, &session_id).is_err() {
                return identity_refused();
            }
            // `spawned_by` stays optional and stays a claim. When it is present it has to agree
            // with the credential; it never selects anything on its own.
            if req.spawned_by.iter().any(|claimed| *claimed != session_id) {
                return identity_refused();
            }
            match state.authority.redeem_approval(approval, &session_id) {
                Ok(payload) => Some(payload),
                Err(AuthorityError::Unauthenticated) => return identity_refused(),
                Err(AuthorityError::Expired) => {
                    return err(
                        403,
                        "Forbidden",
                        &format!("this spawn approval has passed its expiry; an approval is valid for {SPAWN_APPROVAL_TTL_SECS} seconds from the POST /delegate that granted it"),
                    )
                }
                Err(AuthorityError::AlreadyRedeemed) => {
                    return err(
                        403,
                        "Forbidden",
                        "this spawn approval has already been redeemed; an approval covers one launch, so ask POST /delegate for another",
                    )
                }
            }
        }
        _ => return identity_refused(),
    };

    let (resolved, manifest) = match resolve_capsule(state, &req.name, &req.version) {
        Ok(pair) => pair,
        Err(response) => return response,
    };

    // What was approved. The approval was marked spent the moment it verified, above and before
    // this check: an approval names one artifact, and presenting it for another is an error rather
    // than a near-miss to retry.
    if let Some(approved) = approved.as_ref() {
        if approved.name != resolved.meta.name || approved.version != resolved.meta.version {
            return err(
                403,
                "Forbidden",
                &format!(
                    "this spawn approval was granted for '{}@{}', not '{}@{}'",
                    approved.name, approved.version, resolved.meta.name, resolved.meta.version,
                ),
            );
        }
        if approved.digest != resolved.sha256 {
            return err(
                403,
                "Forbidden",
                &format!(
                    "'{}@{}' now resolves to a different artifact than the one this spawn approval was granted for (approved sha256 {}, resolved sha256 {})",
                    resolved.meta.name, resolved.meta.version, approved.digest, resolved.sha256,
                ),
            );
        }
    }

    let child_policy = capability_policy_from_runtime_manifest(&manifest);
    let child_envelope = SpawnEnvelope::from_runtime_manifest(&manifest);

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
    let authority_bg = Arc::clone(&state.authority);

    thread::spawn(move || {
        let registry = LocalRegistry::new(&registry_path_bg);
        let mut staged = match stage_session(Arc::new(registry), stage_request) {
            Ok(staged) => staged,
            Err(e) => {
                ready_tx
                    .send(Err(format!("stage_session failed: {e}")))
                    .ok();
                return;
            }
        };
        let session_id = staged.session_id.clone();

        // The credential can only be minted here: it names a session id, and staging is what mints
        // one. It goes to the session's runtime and nowhere else — not the workdir, not the
        // environment, not the trace.
        if let Some(credential) =
            mint_session_credential(&authority_bg, &session_id, &child_envelope)
        {
            staged.set_spawn_credential(credential);
        }

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
