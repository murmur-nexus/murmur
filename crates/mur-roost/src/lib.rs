//! The daemon's request handling: the job store, the five endpoints, and the spawn referee that
//! keeps a child capsule inside the capability envelope of the capsule that asked for it.
//!
//! Split from the binary so the endpoints can be driven directly by tests — [`route`] is the same
//! entry point [`handle_connection`] reaches, with the socket removed.
//!
//! **This daemon referees; it does not run anything.** `POST /spawn` answers *may you*: it proves
//! which session is asking (an opaque credential minted for that session when it registered),
//! resolves the named artifact from its own registry, runs the referee, and returns an approval
//! naming that artifact by name, version and content hash. It creates no workdir, no session, no
//! trace and no process. The parent's own runtime does the launching.
//!
//! What the daemon knows about a session comes from that session registering. `POST /register`
//! names an artifact; the daemon resolves *that* artifact from *its* registry and lowers the
//! manifest into a [`SpawnEnvelope`] itself. A registrant states a name, never a grant — a
//! registrant that could state its grants would be a registrant that could declare its own
//! ceiling.

pub mod authority;
pub mod bounds;

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use capsule_runtime::{
    artifact::extract_manifest_yaml, SpawnEnvelope, SPAWN_APPROVAL_HEADER, SPAWN_CREDENTIAL_HEADER,
};
use murmur_artifact::{current_platform, LocalRegistry, Registry};
use serde::{Deserialize, Serialize};

use crate::authority::{now_ms, AuthorityError, SpawnAuthority, SPAWN_APPROVAL_TTL_SECS};
use crate::bounds::{live_children, BoundRefusal};

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
    /// Lowered by this daemon from the registry manifest of the artifact the session named when it
    /// registered, never from anything the session supplied. `handle_spawn` selects the record by
    /// the session id the presented credential names, never by a session id the request claims: the
    /// credential is minted by this daemon and handed to that session's runtime alone, so a caller
    /// cannot be judged against the envelope of a session it does not hold.
    pub envelope: SpawnEnvelope,
    /// How many further levels of delegation may hang below this session.
    ///
    /// `state.max_depth` for a session that registered with no approval; for every other session,
    /// the number sealed into the approval it registered with, which is one less than what its
    /// parent held. A session at `0` is refused every spawn it asks for, which is what terminates
    /// a capsule whose `capabilities.spawn.allow` names itself.
    pub depth_remaining: u32,
    /// The session whose approval admitted this one, and therefore the session this one is counted
    /// against for concurrency. `None` for a session that registered with no approval.
    pub parent_session: Option<String>,
    /// Approvals this session has been granted and nobody has redeemed yet. Each one holds a
    /// concurrency slot until it is redeemed or its expiry passes — see [`bounds::live_children`].
    pub pending: Vec<PendingApproval>,
}

/// An approval minted for a session, held by the daemon until the child it approves registers.
///
/// Holds no token bytes: the `jti` and the expiry are enough to match a redemption and to free
/// the slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingApproval {
    /// The `jti` of the approval, matched against [`authority::ApprovalPayload::jti`] on
    /// redemption.
    pub jti: String,
    /// The approval's absolute expiry in unix milliseconds. Past it, the slot is free again
    /// whether or not anybody ever presented the approval.
    pub expires_at_ms: u64,
}

pub type JobStore = Arc<Mutex<HashMap<String, JobRecord>>>;

// ── Shared server state ───────────────────────────────────────────────────────

#[derive(Clone)]
pub struct State {
    pub jobs: JobStore,
    pub registry_path: PathBuf,
    pub spawn_allow: Vec<String>,
    /// Levels of delegation allowed below a session that registered with no approval, from
    /// `--max-depth`. `0` refuses every delegation.
    pub max_depth: u32,
    /// Children one session may hold live at once, from `--max-concurrent`. `0` refuses every
    /// delegation.
    pub max_concurrent: u32,
    /// Mints and verifies every credential and approval this daemon issues. Generated once in
    /// `main` and dropped when the process exits, so a token from a previous daemon verifies
    /// against nothing.
    pub authority: Arc<SpawnAuthority>,
}

// ── Request / response types ─────────────────────────────────────────────────

/// The `POST /spawn` body.
///
/// Carries no manifest and no capabilities, and must not gain either: the grants a child is
/// refereed against are the ones in the *registry* manifest the daemon resolves for itself. A
/// request that could supply them would be a request that could declare its own ceiling.
#[derive(Debug, Deserialize)]
struct SpawnRequest {
    name: String,
    version: String,
}

/// What `POST /spawn` answers with: permission, and the exact artifact it is permission for.
#[derive(Serialize)]
struct SpawnResponse {
    approval: String,
    /// The artifact the daemon resolved, echoed so the caller launches by the same coordinate the
    /// referee judged rather than by the one it asked with.
    name: String,
    version: String,
    /// The resolved artifact's sha256, lowercase hex — the same digest the approval is bound to.
    sha256: String,
    /// Absolute expiry in unix milliseconds, so a caller can tell a stale approval from a refused
    /// one without having to guess this daemon's TTL.
    expires_at_ms: u64,
}

/// The `POST /register` body.
///
/// `#[serde(deny_unknown_fields)]` is deliberately *not* set: a body carrying extra `capabilities`
/// or `envelope` blocks is accepted and those blocks change nothing, which is a stronger statement
/// than refusing them. What the session holds is derived from `name`/`version` alone.
#[derive(Debug, Deserialize)]
struct RegisterRequest {
    session_id: String,
    name: String,
    version: String,
}

#[derive(Serialize)]
struct RegisterResponse {
    credential: String,
}

/// The `POST /deregister` body.
#[derive(Debug, Deserialize)]
struct DeregisterRequest {
    outcome: String,
}

/// What `GET /status/{session_id}` answers with.
#[derive(Serialize)]
struct StatusResponse {
    status: &'static str,
    /// Levels of delegation still available below this session.
    depth_remaining: u32,
    /// Children this session holds right now, counting one it has been approved to launch and has
    /// not launched yet.
    live_children: u32,
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

fn ok_json<T: Serialize>(value: &T) -> String {
    ok(&serde_json::to_string(value)
        .unwrap_or_else(|_| r#"{"error":"serialization failed"}"#.to_string()))
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
        ("POST", "/spawn") => handle_spawn(headers, body, state),
        ("POST", "/register") => handle_register(headers, body, state),
        ("POST", "/deregister") => handle_deregister(headers, body, state),
        ("GET", p) if p.starts_with("/status/") => {
            let session_id = &p["/status/".len()..];
            handle_status(session_id, state)
        }
        _ => err(404, "Not Found", "not found"),
    }
}

// ── Refusals ──────────────────────────────────────────────────────────────────

/// The one answer every identity failure gets, on every endpoint.
///
/// Names no session id, no capsule name, no manifest key and no envelope axis, and is returned
/// byte-for-byte identically whether a token was absent, malformed, minted by a previous daemon,
/// or names a session this daemon has never heard of. Two requests differing only in whether the
/// session they name exists are indistinguishable, so no endpoint is an oracle for which sessions
/// are running.
const IDENTITY_REFUSAL: &str = "not authorised: this daemon answers only a credential it minted for a running session, and an approval it minted for that same session";

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

/// The record of the session a credential names, or an identity refusal.
///
/// The two questions this answers — is the credential one of ours, and is the session it names
/// running — collapse into one outcome on purpose: a caller learns nothing about which sessions
/// exist. Both delegation bounds are decided from the record this returns, so a session that
/// cannot be found is refused rather than judged against a default.
fn session_record(state: &Arc<State>, session_id: &str) -> Result<JobRecord, String> {
    let jobs = state.jobs.lock().unwrap();
    match jobs.get(session_id) {
        Some(job) if job.status == JobStatus::Running => Ok(job.clone()),
        _ => Err(identity_refused()),
    }
}

// ── POST /spawn ───────────────────────────────────────────────────────────────

/// Ask for permission to spawn a capsule.
///
/// This is where the referee runs, and the only place it runs: the approval this returns pins the
/// artifact by content hash, and the content hash is what determines the manifest the referee
/// read. Running it again at registration would imply the digest binding is not trusted.
///
/// Nothing is created here. A request, granted or refused, leaves no session directory, no trace,
/// no job record and no process.
fn handle_spawn(headers: &RequestHeaders, body: &str, state: &Arc<State>) -> String {
    let req: SpawnRequest = match serde_json::from_str(body) {
        Ok(r) => r,
        Err(e) => return err(400, "Bad Request", &format!("invalid JSON: {e}")),
    };

    let Some(credential) = headers.get(SPAWN_CREDENTIAL_HEADER) else {
        return identity_refused();
    };
    let Ok(session_id) = state.authority.verify_credential(credential) else {
        return identity_refused();
    };
    // Selected by the session id the credential names, never by one the body claims: the figures
    // both bounds are decided from are this daemon's record of the asking session.
    let parent = match session_record(state, &session_id) {
        Ok(record) => record,
        Err(response) => return response,
    };
    let parent_envelope = parent.envelope;

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

    // The operator's two bounds, ahead of the registry: a spawn refused for depth or concurrency
    // resolves no artifact and reads no manifest.
    if parent.depth_remaining == 0 {
        return err(
            403,
            "Forbidden",
            &BoundRefusal::DepthExhausted {
                max_depth: state.max_depth,
            }
            .to_string(),
        );
    }
    let live = live_children(&state.jobs.lock().unwrap(), &session_id, now_ms());
    if live >= state.max_concurrent {
        return err(
            403,
            "Forbidden",
            &BoundRefusal::ConcurrencyReached {
                max_concurrent: state.max_concurrent,
                live,
            }
            .to_string(),
        );
    }

    let (resolved, manifest) = match resolve_capsule(state, &req.name, &req.version) {
        Ok(pair) => pair,
        Err(response) => return response,
    };

    // The referee, decided before anything anywhere is created: a refused request leaves no
    // workdir, no session, no trace and no job record.
    let child_envelope = SpawnEnvelope::from_runtime_manifest(&manifest);
    if let Err(violation) = parent_envelope.contains(&child_envelope) {
        return err(403, "Forbidden", &violation.to_string());
    }

    let expires_at_ms = now_ms() + SPAWN_APPROVAL_TTL_SECS * 1_000;
    // One less than the asking session holds, sealed into the approval rather than sent beside it.
    let child_depth = parent.depth_remaining - 1;
    let (approval, payload) = match state.authority.mint_approval_payload(
        &session_id,
        &resolved.meta.name,
        &resolved.meta.version,
        &resolved.sha256,
        child_depth,
        expires_at_ms,
    ) {
        Ok(minted) => minted,
        Err(e) => {
            return err(
                500,
                "Internal Server Error",
                &format!("failed to mint a spawn approval: {e}"),
            )
        }
    };

    // The slot is taken at approval, not at the child's registration — see
    // [`bounds::live_children`].
    //
    // The count is taken again here, under the same lock hold that records the reservation. The
    // check above is the one an ordinary refusal comes from, but it releases the lock before the
    // registry is read, so two requests from one session can both pass it; only a count and a push
    // that cannot be interleaved keep a parent from taking every slot at once.
    {
        let mut jobs = state.jobs.lock().unwrap();
        let live = live_children(&jobs, &session_id, now_ms());
        if live >= state.max_concurrent {
            return err(
                403,
                "Forbidden",
                &BoundRefusal::ConcurrencyReached {
                    max_concurrent: state.max_concurrent,
                    live,
                }
                .to_string(),
            );
        }
        match jobs.get_mut(&session_id) {
            Some(job) if job.status == JobStatus::Running => job.pending.push(PendingApproval {
                jti: payload.jti,
                expires_at_ms,
            }),
            _ => return identity_refused(),
        }
    }

    ok_json(&SpawnResponse {
        approval,
        name: resolved.meta.name,
        version: resolved.meta.version,
        sha256: resolved.sha256,
        expires_at_ms,
    })
}

// ── POST /register ────────────────────────────────────────────────────────────

/// A session announcing itself, and taking the credential it will delegate with.
///
/// The registrant names an artifact. The daemon resolves that artifact from its own registry and
/// derives the envelope from that manifest, so what a session is judged to hold never depends on
/// what it said about itself.
fn handle_register(headers: &RequestHeaders, body: &str, state: &Arc<State>) -> String {
    let req: RegisterRequest = match serde_json::from_str(body) {
        Ok(r) => r,
        Err(e) => return err(400, "Bad Request", &format!("invalid JSON: {e}")),
    };
    if req.session_id.trim().is_empty() {
        return err(400, "Bad Request", "session_id must not be empty");
    }
    // A session id already in the store belongs to somebody. Overwriting it would let a later
    // registrant replace a running session's envelope with one of its own choosing, so it is
    // refused — and refused in the identity words, which say nothing about whether that id exists.
    if state.jobs.lock().unwrap().contains_key(&req.session_id) {
        return identity_refused();
    }

    let (resolved, manifest) = match resolve_capsule(state, &req.name, &req.version) {
        Ok(pair) => pair,
        Err(response) => return response,
    };

    // The registrant's delegation budget, and whose census it counts against. Both come from the
    // approval it presented or, where there is none, from the operator's flag — never from the
    // body, which has no field either could arrive in.
    let (depth_remaining, parent_session, redeemed_jti) = match headers.get(SPAWN_APPROVAL_HEADER) {
        Some(approval) => {
            // The approval is marked spent the moment it verifies, above every check below: an
            // approval covers one launch, and presenting it for the wrong artifact or from a
            // session that has since ended is an error rather than a near-miss to retry.
            let approved = match state.authority.redeem_approval(approval) {
                Ok(payload) => payload,
                Err(AuthorityError::Unauthenticated) => return identity_refused(),
                Err(AuthorityError::Expired) => {
                    return err(
                        403,
                        "Forbidden",
                        &format!("this spawn approval has passed its expiry; an approval is valid for {SPAWN_APPROVAL_TTL_SECS} seconds from the POST /spawn that granted it"),
                    )
                }
                Err(AuthorityError::AlreadyRedeemed) => {
                    return err(
                        403,
                        "Forbidden",
                        "this spawn approval has already been redeemed; an approval covers one launch, so ask POST /spawn for another",
                    )
                }
            };
            // The parent that earned this approval must still be running: an approval outliving
            // the session it was granted to would let a child register under a ceiling nothing is
            // still accountable for.
            if session_record(state, &approved.sid).is_err() {
                return identity_refused();
            }
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
            (approved.depth, Some(approved.sid), Some(approved.jti))
        }
        // No approval: there is no parent to have been approved by, so the operator who started
        // the daemon is the only authority, and their list is the only gate. The top of a chain
        // starts from `--max-depth`.
        None => {
            if !state.spawn_allow.contains(&req.name) {
                return err(
                    403,
                    "Forbidden",
                    &format!("capsule '{}' is not in --spawn-allow", req.name),
                );
            }
            (state.max_depth, None, None)
        }
    };

    // The envelope is derived here, from the manifest this daemon resolved — never from anything
    // the registrant said about itself. A registrant that could state its grants would be a
    // registrant that could declare its own ceiling.
    let envelope = SpawnEnvelope::from_runtime_manifest(&manifest);

    // Minted for every registrant, including a leaf capsule that delegates nothing: a credential
    // over an envelope with an empty `spawn_allow` authorises no name at all, because
    // `handle_spawn`'s allow-list check refuses every one of them. Which sessions are handed a
    // credential is decided by which sessions register, and that decision belongs to the runtime
    // — a capsule declaring no `capabilities.spawn.allow` never calls this endpoint.
    let credential = match state.authority.mint_credential_token(&req.session_id) {
        Ok(token) => token,
        Err(e) => {
            return err(
                500,
                "Internal Server Error",
                &format!("failed to mint a session credential: {e}"),
            )
        }
    };

    let mut jobs = state.jobs.lock().unwrap();
    // Dropping the reservation and inserting the running record under one lock hold keeps the
    // child counted once rather than twice.
    if let (Some(parent), Some(jti)) = (parent_session.as_deref(), redeemed_jti) {
        if let Some(job) = jobs.get_mut(parent) {
            job.pending.retain(|pending| pending.jti != jti);
        }
    }
    jobs.insert(
        req.session_id.clone(),
        JobRecord {
            status: JobStatus::Running,
            envelope,
            depth_remaining,
            parent_session,
            pending: Vec::new(),
        },
    );
    drop(jobs);

    ok_json(&RegisterResponse { credential })
}

// ── POST /deregister ──────────────────────────────────────────────────────────

/// A session reporting that it has ended.
///
/// Moves the record to `complete` or `failed`. The credential still carries a valid MAC afterwards
/// — nothing can un-mint it — but every endpoint that means anything requires the session it names
/// to be running, so it authorises nothing from here on.
fn handle_deregister(headers: &RequestHeaders, body: &str, state: &Arc<State>) -> String {
    let req: DeregisterRequest = match serde_json::from_str(body) {
        Ok(r) => r,
        Err(e) => return err(400, "Bad Request", &format!("invalid JSON: {e}")),
    };
    let status = match req.outcome.as_str() {
        "complete" => JobStatus::Complete,
        "failed" => JobStatus::Failed,
        other => {
            return err(
                400,
                "Bad Request",
                &format!("outcome '{other}' is not 'complete' or 'failed'"),
            )
        }
    };

    let Some(credential) = headers.get(SPAWN_CREDENTIAL_HEADER) else {
        return identity_refused();
    };
    let Ok(session_id) = state.authority.verify_credential(credential) else {
        return identity_refused();
    };

    let mut jobs = state.jobs.lock().unwrap();
    match jobs.get_mut(&session_id) {
        Some(job) if job.status == JobStatus::Running => {
            job.status = status;
            // An approval this session earned can no longer be redeemed by anybody — registering
            // under it requires the granting session to be running — so the slots it was holding
            // are released rather than left to expire.
            job.pending.clear();
            ok(r#"{}"#)
        }
        _ => identity_refused(),
    }
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
            ok_json(&StatusResponse {
                status,
                depth_remaining: job.depth_remaining,
                live_children: live_children(&jobs, session_id, now_ms()),
            })
        }
        None => err(404, "Not Found", "session not found"),
    }
}
