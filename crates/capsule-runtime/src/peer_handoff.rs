//! Peer file handoff: runtime-minted handles over a two-sided grant.
//!
//! A capsule that declares `exports.peer_files` can hand one named file to one named peer
//! without a filesystem path ever crossing the wire. The runtime mints an opaque handle — a MAC'd
//! token naming exactly one file, for one audience, with an expiry — and the agent puts that
//! handle in an ordinary A2A message. A capsule that declares `capabilities.peer_fetch` can
//! redeem such a handle against the minter's `/resources/peer/<handle>` endpoint and lands the
//! bytes as a *file* in its own workdir.
//!
//! The same three-way split the operator plane rests on, with the audience added:
//!
//! * The **agent** owns what a file contains, and chooses which file to offer and to whom.
//! * The **runtime** owns whether it may. It is the minting authority, so an agent cannot forge a
//!   handle for a file the operator did not put under `exports.peer_files.root`, and cannot widen
//!   its own envelope by naming a path — the path is resolved here, by the same code that
//!   resolves a read.
//! * The **operator on the other side** owns whether the bytes may be ingested at all, through
//!   `capabilities.peer_fetch.allow`. Neither half implies the other and both default to deny.
//!
//! **The minting key is instance-scoped.** 32 bytes from the OS at launch, held only in memory,
//! zeroed on drop. When the session ends every outstanding handle becomes unverifiable at once —
//! revoke-all with no revocation list, and the reason a handle minted by one session of a capsule
//! does not redeem against the next.
//!
//! **A handle authorises a file, not a version of one.** There is no generation in the payload
//! and no `409` on this plane: a redeem always serves the file's current bytes, so a file
//! rewritten two turns later simply serves the newer content and the `etag` is how the holder
//! notices. Pinning would mean either failing an ordinary content update or retaining superseded
//! bytes, and nothing else in the runtime does either.
//!
//! **Redeem is idempotent, not single-use.** No used-set, no per-handle server state. Single-use
//! exists to bound a credential's blast radius over time, and the instance-scoped key plus the
//! capsule's own lifetime already do that structurally; making a retried transfer a lost file
//! buys nothing on top.

use std::{
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{SystemTime, UNIX_EPOCH},
};

use base64::Engine as _;
use hmac::{Hmac, Mac};
use murmur_artifact::{ContainmentClass, PeerFilesExport};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::{
    identity::CapsuleIdentity,
    outgoing::parse_host_port,
    resource_plane::{
        read_export_file_with_policy, resolve_relpath_beneath_root, symlink_policy, DeclaredExport,
        ReadResponse, ResourceError, ResourceResponse,
    },
    trace::ResourceTraceAppender,
};

/// Path prefix the peer plane answers under, alongside the operator plane's `/resources/files/`.
/// `GET`-only, and with no listing verb at any depth.
pub const PEER_PATH_PREFIX: &str = "/resources/peer";

/// Request header carrying the audience the caller asserts it is. Required on every redeem.
pub const AUDIENCE_HEADER: &str = "x-murmur-audience";

/// Response header naming the handle a served body was redeemed with.
pub const HANDLE_ID_HEADER: &str = "x-murmur-handle-id";

/// Version tag and first segment of every token.
const TOKEN_VERSION_TAG: &str = "mh1";

/// The only payload version this runtime mints or accepts.
const PAYLOAD_VERSION: u8 = 1;

/// Domain separator prefixed to every MAC input, so a MAC over a peer handle can never be
/// mistaken for a MAC this runtime computes over anything else.
const MAC_DOMAIN: &[u8] = b"murmur-peer-handle-v1";

/// Separator between the MAC input's fields. ASCII unit separator: it cannot occur in base64url
/// and cannot occur in an audience, so no pair of distinct (payload, audience) inputs can produce
/// the same MAC input by moving the boundary between them.
const MAC_FIELD_SEPARATOR: u8 = 0x1f;

/// Nonce width in bytes. Two handles for the same file and audience are distinct and
/// independently traceable.
const NONCE_BYTES: usize = 16;

/// Width of a `handle_id`, in lowercase hex characters.
const HANDLE_ID_HEX_CHARS: usize = 16;

/// Directory, relative to the accessible workdir, that fetched bytes land in.
pub const PEER_INBOX_DIR: &str = "peer-in";

/// The base64 alphabet used for both token segments: URL-safe, unpadded, so a whole token is
/// safe in a request path without escaping and has no `=` for a transport to mangle.
const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::URL_SAFE_NO_PAD;

type HmacSha256 = Hmac<Sha256>;

// ── The minting key ───────────────────────────────────────────────────────────

/// The 32-byte HMAC key one capsule *instance* mints and verifies with.
///
/// Generated in `launch_session`, and only when `exports.peer_files` is declared. Never written
/// to disk, never placed in an environment variable, and never copied out of this type — the
/// deleted checkpoint-signing mechanism persisted its key under `$HOME`, and this deliberately
/// does not.
pub struct PeerMintKey([u8; 32]);

impl PeerMintKey {
    /// 32 bytes from the operating system's CSPRNG.
    pub fn generate() -> Result<Self, String> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes)
            .map_err(|error| format!("failed to generate the peer mint key: {error}"))?;
        Ok(Self(bytes))
    }

    /// Builds a fresh MAC context. Private: the key bytes never leave this type.
    fn mac(&self) -> HmacSha256 {
        HmacSha256::new_from_slice(&self.0).expect("HMAC-SHA256 accepts a key of any length")
    }

    /// Overwrites the key bytes with volatile writes.
    ///
    /// Volatile so the compiler cannot elide a write whose result is provably never read — which
    /// is the whole of what the write is for. This is the module's only `unsafe`, and it does
    /// nothing but overwrite a fully-owned, correctly-aligned array.
    #[allow(unsafe_code)]
    fn zeroize(&mut self) {
        for byte in self.0.iter_mut() {
            // SAFETY: `byte` is a valid, aligned, exclusively-borrowed `u8` inside an array this
            // value owns. `write_volatile` of a `u8` through such a reference is always defined.
            unsafe { std::ptr::write_volatile(byte, 0) };
        }
        std::sync::atomic::compiler_fence(Ordering::SeqCst);
    }
}

impl Drop for PeerMintKey {
    /// When the session ends the key goes with it, and every outstanding handle becomes
    /// unverifiable at once — revoke-all with no revocation list.
    fn drop(&mut self) {
        self.zeroize();
    }
}

impl std::fmt::Debug for PeerMintKey {
    /// Prints no key material. A key that can reach a log line is a key on disk by another route.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PeerMintKey(<redacted>)")
    }
}

// ── The token ─────────────────────────────────────────────────────────────────

/// A handle's payload, carried in the clear in the token's middle segment.
///
/// Readable by anyone holding the token, and deliberately so: the payload is not the secret. The
/// MAC is what makes it unforgeable, and the audience — which is *not* carried here — is what
/// makes it non-bearer.
///
/// There is no generation field. A handle authorises a file, not a version of one; see the module
/// documentation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandlePayload {
    /// Payload version. Only [`PAYLOAD_VERSION`] is minted or accepted.
    pub v: u8,
    /// The minting session's `session_id` — the capsule *instance*, not the capsule.
    pub iss: String,
    /// Path relative to `exports.peer_files.root`, canonicalised at mint time. Never absolute,
    /// and never the accessible-workdir-relative form: the peer plane discloses no path structure
    /// above the root.
    pub p: String,
    /// Absolute expiry, in unix milliseconds.
    pub exp: u64,
    /// [`NONCE_BYTES`] random bytes, lowercase hex.
    pub n: String,
}

/// Why a token was not accepted.
///
/// Three variants and not more, on purpose: see [`verify`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandleError {
    /// Not `mh1.<base64url>.<base64url>`, or a payload that is not JSON, or `v != 1`.
    Malformed,
    /// MAC verification failed, for any reason at all.
    NotValid,
    /// The MAC verified and `exp` is in the past.
    Expired,
}

/// The stable identifier a mint, a redeem and a fetch are correlated by: the first
/// [`HANDLE_ID_HEX_CHARS`] lowercase hex characters of `sha256(token)`.
///
/// This — never the token — is what appears in a trace, a log line or an error body.
pub fn handle_id(token: &str) -> String {
    murmur_artifact::sha256_hex(token.as_bytes())[..HANDLE_ID_HEX_CHARS].to_string()
}

/// The token's three segments, checked for shape alone.
///
/// Returns the payload segment as base64 text — decoded, but not parsed. Nothing here looks at
/// what the payload *says*.
fn split_token(token: &str) -> Result<(&str, Vec<u8>, Vec<u8>), HandleError> {
    let mut segments = token.split('.');
    let tag = segments.next().ok_or(HandleError::Malformed)?;
    let payload_b64 = segments.next().ok_or(HandleError::Malformed)?;
    let mac_b64 = segments.next().ok_or(HandleError::Malformed)?;
    if segments.next().is_some() || tag != TOKEN_VERSION_TAG {
        return Err(HandleError::Malformed);
    }
    let payload = B64
        .decode(payload_b64)
        .map_err(|_| HandleError::Malformed)?;
    let mac = B64.decode(mac_b64).map_err(|_| HandleError::Malformed)?;
    if payload.is_empty() || mac.is_empty() {
        return Err(HandleError::Malformed);
    }
    Ok((payload_b64, payload, mac))
}

/// The bytes a handle's MAC is taken over.
///
/// `MAC_DOMAIN ‖ 0x1f ‖ <payload base64url> ‖ 0x1f ‖ <audience>`.
///
/// **The audience is covered by the MAC but is not carried in the token.** That is what makes a
/// handle audience-scoped rather than pure bearer: a token scraped out of persisted message
/// history is not by itself redeemable, because redemption also requires asserting the exact
/// audience string it was minted for.
fn mac_input(payload_b64: &str, audience: &str) -> Vec<u8> {
    let mut input = Vec::with_capacity(MAC_DOMAIN.len() + payload_b64.len() + audience.len() + 2);
    input.extend_from_slice(MAC_DOMAIN);
    input.push(MAC_FIELD_SEPARATOR);
    input.extend_from_slice(payload_b64.as_bytes());
    input.push(MAC_FIELD_SEPARATOR);
    input.extend_from_slice(audience.as_bytes());
    input
}

/// Mints one token: `mh1.<base64url-nopad(payload JSON)>.<base64url-nopad(mac)>`.
pub fn mint(key: &PeerMintKey, payload: &HandlePayload, audience: &str) -> Result<String, String> {
    let json = serde_json::to_vec(payload)
        .map_err(|error| format!("failed to serialize the handle payload: {error}"))?;
    let payload_b64 = B64.encode(json);
    let mut mac = key.mac();
    mac.update(&mac_input(&payload_b64, audience));
    let tag = B64.encode(mac.finalize().into_bytes());
    Ok(format!("{TOKEN_VERSION_TAG}.{payload_b64}.{tag}"))
}

/// Verifies one token against this instance's key and the audience the caller asserted.
///
/// Check order is fixed and nothing downstream of the MAC is evaluated before it, so a caller
/// that fails the MAC learns nothing about the file: shape → MAC → payload → expiry. The MAC
/// comparison is `verify_slice`, which is constant-time; comparing decoded bytes with `==` would
/// leak the tag one byte at a time.
///
/// **[`HandleError::NotValid`] is deliberately one outcome.** A tampered payload, a handle minted
/// by a different capsule instance, and a correct handle presented with the wrong audience are
/// indistinguishable to the caller — same status, same code, same message. Splitting them builds
/// an oracle that tells a prober which field to change next. A payload whose `iss` is not this
/// session's id answers the same way, and is unreachable in any case: only this instance's key
/// can produce a MAC this instance accepts.
///
/// **Audience binding is not peer authentication.** Nothing here proves that the process
/// asserting `x-murmur-audience: reporter@localhost:41234` *is* that capsule; an attacker who
/// both intercepts a handle and knows which peer it was minted for can assert that identity and
/// redeem. What the binding buys is that a handle is not a credential for whoever finds it — a
/// third capsule with its own identity is refused, and a token alone is insufficient.
pub fn verify(
    key: &PeerMintKey,
    token: &str,
    audience: &str,
    session_id: &str,
) -> Result<HandlePayload, HandleError> {
    let (payload_b64, payload_bytes, mac_bytes) = split_token(token)?;

    let mut mac = key.mac();
    mac.update(&mac_input(payload_b64, audience));
    mac.verify_slice(&mac_bytes)
        .map_err(|_| HandleError::NotValid)?;

    // Only now: the payload has been proven to be one this instance minted, so reading it is
    // reading our own record rather than trusting the caller's.
    let payload: HandlePayload =
        serde_json::from_slice(&payload_bytes).map_err(|_| HandleError::Malformed)?;
    if payload.v != PAYLOAD_VERSION {
        return Err(HandleError::Malformed);
    }
    if payload.iss != session_id {
        return Err(HandleError::NotValid);
    }
    if payload.exp <= now_ms() {
        return Err(HandleError::Expired);
    }
    Ok(payload)
}

/// The payload of a token this runtime did **not** mint and cannot verify.
///
/// Used on the fetching side for one thing only: naming the local file the bytes land in. The
/// value it returns is caller-controlled and must never decide anything — the stored path is
/// prefixed with the `handle_id` and the basename is sanitised precisely because this is not a
/// fact about anything.
pub fn decode_payload_unverified(token: &str) -> Option<HandlePayload> {
    let (_, payload_bytes, _) = split_token(token).ok()?;
    serde_json::from_slice(&payload_bytes).ok()
}

/// Replaces every peer handle in `text` with its `handle_id`.
///
/// A handle is a credential, and the trace is a durable record — so the two must never meet.
/// Applied at the trace boundary rather than at each call site, because the token reaches the
/// record by an ordinary route: it is an argument the model passed to `fetch-peer-file`, and the
/// `tool_call` event records the arguments of every call. Substituting the `handle_id` keeps the
/// record correlatable with the mint and the redeem while carrying nothing redeemable.
pub(crate) fn redact_handle_tokens(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains(TOKEN_VERSION_TAG) {
        return std::borrow::Cow::Borrowed(text);
    }
    let bytes = text.as_bytes();
    let prefix = format!("{TOKEN_VERSION_TAG}.");
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    let mut replaced = false;
    while index < bytes.len() {
        if text[index..].starts_with(&prefix) {
            if let Some(end) = token_end(&text[index..]) {
                let token = &text[index..index + end];
                out.push_str(&format!("<handle:{}>", handle_id(token)));
                index += end;
                replaced = true;
                continue;
            }
        }
        // Advance by one whole character: indexing a `str` mid-codepoint would panic.
        let step = text[index..].chars().next().map_or(1, char::len_utf8);
        out.push_str(&text[index..index + step]);
        index += step;
    }
    if replaced {
        std::borrow::Cow::Owned(out)
    } else {
        std::borrow::Cow::Borrowed(text)
    }
}

/// The byte length of the `mh1.<seg>.<seg>` token starting at `text`, or `None` if there is none.
///
/// Deliberately not greedy across the second separator: a token at the end of a sentence must not
/// swallow the full stop after it, and a token is exactly three segments.
fn token_end(text: &str) -> Option<usize> {
    let mut cursor = TOKEN_VERSION_TAG.len() + 1;
    let mut segments = 0;
    while segments < 2 {
        let start = cursor;
        cursor += text[cursor..]
            .bytes()
            .take_while(|b| b.is_ascii_alphanumeric() || *b == b'-' || *b == b'_')
            .count();
        if cursor == start {
            return None;
        }
        segments += 1;
        if segments == 1 {
            if text[cursor..].starts_with('.') {
                cursor += 1;
            } else {
                return None;
            }
        }
    }
    Some(cursor)
}

/// [`redact_handle_tokens`] applied to every string in a JSON value, in place.
pub(crate) fn redact_handles_in_json(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(text) => {
            if let std::borrow::Cow::Owned(redacted) = redact_handle_tokens(text) {
                *text = redacted;
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(redact_handles_in_json),
        serde_json::Value::Object(map) => {
            map.values_mut().for_each(redact_handles_in_json);
        }
        _ => {}
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn random_hex(bytes: usize) -> Result<String, String> {
    let mut buffer = vec![0u8; bytes];
    getrandom::fill(&mut buffer).map_err(|error| format!("failed to generate a nonce: {error}"))?;
    Ok(buffer
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>())
}

// ── The audience ──────────────────────────────────────────────────────────────

/// The audience string derived from a peer's own agent card: `<name>@<host:port>`, lowercased.
///
/// Both sides compute the same string without exchanging it, because both compute it from *the
/// fetching capsule's own advertised identity* — the minter reads it off the card it fetched, and
/// the fetcher asserts it from the same two fields of its own identity.
pub fn audience_from_card(card: &serde_json::Value) -> Result<String, String> {
    let name = card
        .get("name")
        .and_then(serde_json::Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| "the peer's agent card carries no 'name'".to_string())?;
    let url = card
        .get("url")
        .and_then(serde_json::Value::as_str)
        .filter(|url| !url.trim().is_empty())
        .ok_or_else(|| "the peer's agent card carries no 'url'".to_string())?;
    let host_port = parse_host_port(url)?;
    Ok(format!("{}@{}", name.trim(), host_port).to_lowercase())
}

/// This capsule's own audience, asserted on every redeem it issues.
///
/// Built from the same two fields `build_agent_card` publishes, so what a peer minted for and
/// what this runtime asserts are the same string by construction.
pub(crate) fn own_audience(identity: &CapsuleIdentity) -> String {
    let host_port =
        parse_host_port(&identity.capsule_url).unwrap_or_else(|_| identity.capsule_url.clone());
    format!("{}@{}", identity.capsule_name, host_port).to_lowercase()
}

// ── Errors on the wire ────────────────────────────────────────────────────────

/// Every way a redeem can be refused, in the vocabulary the caller and the trace share.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerError {
    /// The manifest declares no `exports.peer_files`. Absent means deny.
    NoPeerPlane,
    MalformedHandle,
    MissingAudience,
    /// The MAC did not verify — for any reason. Deliberately indivisible; see [`verify`].
    HandleNotValid,
    HandleExpired,
    NotFound,
    OutsideRoot,
    SymlinkRefused,
    NotARegularFile,
    TooLarge {
        max_bytes: u64,
    },
    MethodNotAllowed,
    IoError(String),
}

impl PeerError {
    /// The stable `error` string in the JSON body and the `outcome` in the trace — one vocabulary
    /// for both, so a refusal an auditor reads is spelled the way the caller saw it.
    pub fn code(&self) -> &'static str {
        match self {
            PeerError::NoPeerPlane => "no_peer_plane",
            PeerError::MalformedHandle => "malformed_handle",
            PeerError::MissingAudience => "missing_audience",
            PeerError::HandleNotValid => "handle_not_valid",
            PeerError::HandleExpired => "handle_expired",
            PeerError::NotFound => "not_found",
            PeerError::OutsideRoot => "outside_root",
            PeerError::SymlinkRefused => "symlink_refused",
            PeerError::NotARegularFile => "not_a_regular_file",
            PeerError::TooLarge { .. } => "too_large",
            PeerError::MethodNotAllowed => "method_not_allowed",
            PeerError::IoError(_) => "io_error",
        }
    }

    pub fn status(&self) -> u16 {
        match self {
            PeerError::NoPeerPlane | PeerError::NotFound => 404,
            PeerError::MalformedHandle | PeerError::MissingAudience => 400,
            PeerError::HandleNotValid
            | PeerError::OutsideRoot
            | PeerError::SymlinkRefused
            | PeerError::NotARegularFile => 403,
            PeerError::HandleExpired => 410,
            PeerError::TooLarge { .. } => 413,
            PeerError::MethodNotAllowed => 405,
            PeerError::IoError(_) => 500,
        }
    }

    /// One sentence per code. [`PeerError::HandleNotValid`]'s is a fixed string with nothing
    /// interpolated into it: three different causes must produce byte-identical bodies.
    pub fn message(&self) -> String {
        match self {
            PeerError::NoPeerPlane => {
                "this capsule declares no exports.peer_files block, so it has no peer plane"
                    .to_string()
            }
            PeerError::MalformedHandle => "the handle is not a well-formed mh1 token".to_string(),
            PeerError::MissingAudience => {
                format!("a redeem must assert its audience in the {AUDIENCE_HEADER} header")
            }
            PeerError::HandleNotValid => "the handle is not valid for this capsule".to_string(),
            PeerError::HandleExpired => "the handle has expired".to_string(),
            PeerError::NotFound => "no such file under the peer export root".to_string(),
            PeerError::OutsideRoot => {
                "the handle names a path outside the peer export root".to_string()
            }
            PeerError::SymlinkRefused => {
                "a symlink was encountered and the achieved containment class is scoped".to_string()
            }
            PeerError::NotARegularFile => "the handle does not name a regular file".to_string(),
            PeerError::TooLarge { max_bytes } => {
                format!(
                    "file exceeds the declared exports.peer_files.max_bytes of {max_bytes} bytes"
                )
            }
            PeerError::MethodNotAllowed => {
                "the peer plane serves GET only; it has no write path and no list verb".to_string()
            }
            PeerError::IoError(detail) => format!("read failed: {detail}"),
        }
    }
}

impl From<HandleError> for PeerError {
    fn from(error: HandleError) -> Self {
        match error {
            HandleError::Malformed => PeerError::MalformedHandle,
            HandleError::NotValid => PeerError::HandleNotValid,
            HandleError::Expired => PeerError::HandleExpired,
        }
    }
}

impl From<ResourceError> for PeerError {
    /// The reader's refusals, restated in the peer plane's vocabulary.
    ///
    /// `NoResourcePlane` and `MethodNotAllowed` cannot arrive here — the reader is called with a
    /// declared export and the method was settled before routing — but they are mapped rather
    /// than panicked on so a later caller cannot turn a wrong call into a crash.
    fn from(error: ResourceError) -> Self {
        match error {
            ResourceError::NotFound => PeerError::NotFound,
            ResourceError::OutsideRoot => PeerError::OutsideRoot,
            ResourceError::SymlinkRefused => PeerError::SymlinkRefused,
            ResourceError::NotARegularFile => PeerError::NotARegularFile,
            ResourceError::TooLarge { max_bytes } => PeerError::TooLarge { max_bytes },
            ResourceError::MethodNotAllowed => PeerError::MethodNotAllowed,
            ResourceError::NoResourcePlane => PeerError::NoPeerPlane,
            ResourceError::IoError(detail) => PeerError::IoError(detail),
        }
    }
}

// ── The plane ─────────────────────────────────────────────────────────────────

/// One minted handle, as `share-file` returns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MintedHandle {
    pub handle: String,
    pub handle_id: String,
    pub expires_at_ms: u64,
    pub audience: String,
    /// The canonical root-relative path the handle names. Recorded in the mint trace event; never
    /// returned to the agent, which asked for it and does not need it echoed as a fact.
    pub path: String,
}

/// The declared half of a peer plane: the subtree, the key and the lifetime ceiling.
struct DeclaredPeerExport {
    /// The peer subtree, root resolved against the session's accessible workdir.
    export: DeclaredExport,
    /// The instance key. Shared as an `Arc` because the listener and the agent loop both hold the
    /// plane, and destroyed when the last of them goes.
    key: Arc<PeerMintKey>,
    /// Ceiling on a minted handle's lifetime, in seconds. A `ttl` argument may only narrow.
    max_ttl_secs: u64,
}

/// Everything a mint or a redeem needs, and nothing a running task leaves behind.
///
/// Always built, declared or not — the same shape [`crate::resource_plane::ResourcePlane`] has,
/// and for the same reason: an undeclared capsule still has to *record* the redeem it refused, and
/// a plane that does not exist has nowhere to write.
pub struct PeerPlane {
    /// `None` means the capsule declared no `exports.peer_files`, which is the deny case: every
    /// redeem answers `no_peer_plane`, nothing mints, and no `share-file` tool manifest was ever
    /// written for the agent to see.
    declared: Option<DeclaredPeerExport>,
    /// The minting session's id, written into every payload's `iss` and compared on every redeem.
    session_id: String,
    /// The operator plane's counter, shared: a redeem reports the same provenance a read does.
    generation: Arc<AtomicU64>,
    /// The class this session actually achieved. Keys the symlink decision and rides on every
    /// response.
    containment_achieved: ContainmentClass,
    /// `None` when there is nowhere to write a record — a plane built outside a session.
    trace: Option<Arc<ResourceTraceAppender>>,
}

impl PeerPlane {
    /// The five inputs a plane needs, and the complete list of them: a host path, the declared
    /// export (`None` = undeclared = deny) paired with the instance key, the session id, the
    /// achieved class, the generation counter and somewhere to write the record.
    pub fn new(
        accessible_workdir: &Path,
        export: Option<(&PeerFilesExport, Arc<PeerMintKey>)>,
        session_id: String,
        containment_achieved: ContainmentClass,
        generation: Arc<AtomicU64>,
        trace: Option<Arc<ResourceTraceAppender>>,
    ) -> Self {
        Self {
            declared: export.map(|(export, key)| DeclaredPeerExport {
                export: DeclaredExport::for_peer_files(accessible_workdir, export),
                key,
                max_ttl_secs: export.effective_max_ttl_secs(),
            }),
            session_id,
            generation,
            containment_achieved,
            trace,
        }
    }

    /// Whether this capsule declared `exports.peer_files` at all.
    pub fn is_declared(&self) -> bool {
        self.declared.is_some()
    }

    /// The declared handle-lifetime ceiling, or `None` when nothing is declared.
    pub fn max_ttl_secs(&self) -> Option<u64> {
        self.declared.as_ref().map(|declared| declared.max_ttl_secs)
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// Mints a handle for one file under `exports.peer_files.root`, for one audience.
    ///
    /// `relpath` is relative to the export root and is resolved by the same code that resolves a
    /// read, so a path that escapes — `..`, absolute, percent-encoded, or a symlink leaving the
    /// root — fails the mint. It is refused, never normalised into something mintable.
    ///
    /// `ttl_secs` may only narrow: a value above the declared `max_ttl` is clamped down, never up,
    /// and `None` means the declared `max_ttl`.
    pub fn mint_handle(
        &self,
        relpath: &str,
        audience: &str,
        ttl_secs: Option<u64>,
    ) -> Result<MintedHandle, PeerError> {
        let declared = self.declared.as_ref().ok_or(PeerError::NoPeerPlane)?;
        let policy = symlink_policy(self.containment_achieved);
        let (target, canonical_relpath) =
            resolve_relpath_beneath_root(&declared.export, relpath, policy)?;
        // Resolved beneath the root, but not yet known to be servable: a directory or a fifo
        // under the root would otherwise mint a handle that can only ever be refused.
        let metadata = std::fs::metadata(&target).map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => PeerError::NotFound,
            _ => PeerError::IoError(error.to_string()),
        })?;
        if !metadata.is_file() {
            return Err(PeerError::NotARegularFile);
        }

        let ttl = ttl_secs
            .unwrap_or(declared.max_ttl_secs)
            .min(declared.max_ttl_secs);
        let payload = HandlePayload {
            v: PAYLOAD_VERSION,
            iss: self.session_id.clone(),
            p: canonical_relpath.clone(),
            exp: now_ms().saturating_add(ttl.saturating_mul(1000)),
            n: random_hex(NONCE_BYTES).map_err(PeerError::IoError)?,
        };
        let token = mint(&declared.key, &payload, audience).map_err(PeerError::IoError)?;
        Ok(MintedHandle {
            handle_id: handle_id(&token),
            handle: token,
            expires_at_ms: payload.exp,
            audience: audience.to_string(),
            path: canonical_relpath,
        })
    }

    /// Redeems one token: verify, then serve the named file's *current* bytes.
    ///
    /// Calls the operator plane's shared reader, so it inherits that reader's guarantees
    /// unchanged — the file is opened once and the bytes, size, mtime and `sha256` all come from
    /// that single descriptor, so the `etag` always describes the body it accompanies even while
    /// the agent is rewriting the file. There is no second read path, no re-`stat` and no lock.
    async fn redeem(
        &self,
        token: &str,
        audience: Option<&str>,
    ) -> Result<(HandlePayload, ReadResponse), PeerError> {
        let declared = self.declared.as_ref().ok_or(PeerError::NoPeerPlane)?;

        // Shape first, so a request that is not a token at all is told so without the audience
        // requirement standing in front of it.
        split_token(token).map_err(PeerError::from)?;

        let audience = audience
            .map(str::trim)
            .filter(|audience| !audience.is_empty())
            .ok_or(PeerError::MissingAudience)?;

        let payload = verify(&declared.key, token, audience, &self.session_id)?;

        let export = declared.export.clone();
        let relpath = payload.p.clone();
        let policy = symlink_policy(self.containment_achieved);
        let read = tokio::task::spawn_blocking(move || {
            read_export_file_with_policy(&export, &relpath, policy)
        })
        .await
        .unwrap_or_else(|join| Err(ResourceError::IoError(join.to_string())))?;

        Ok((payload, read))
    }
}

/// Whether `raw_path` addresses the peer plane rather than the operator plane.
///
/// Matched on the whole segment, so `/resources/peerless/x` is not a peer request and falls
/// through to the operator plane's own routing, which refuses it.
pub fn is_peer_path(raw_path: &str) -> bool {
    let path = raw_path.split('?').next().unwrap_or("");
    path == PEER_PATH_PREFIX || path.starts_with(&format!("{PEER_PATH_PREFIX}/"))
}

/// Answers one peer-plane request: a method, a raw request path, an asserted audience, and
/// nothing else.
///
/// Knows no socket, no framing and no authoriser, on the same terms as the operator plane's
/// `handle_resource_request`, so binding it on a second listener is a matter of calling exactly
/// this and writing out the reply.
pub async fn handle_peer_request(
    plane: &PeerPlane,
    method: &str,
    raw_path: &str,
    audience: Option<&str>,
) -> ResourceResponse {
    // A property of the request alone, so it is settled before anything about this capsule is
    // consulted: a `PUT` is refused identically whether or not a peer plane is declared.
    if method != "GET" {
        return error_response(&PeerError::MethodNotAllowed, None, None);
    }

    let path = raw_path.split('?').next().unwrap_or("");
    let rest = path.strip_prefix(PEER_PATH_PREFIX).unwrap_or("");
    // **There is no `list` verb on the peer plane.** `/resources/peer` and `/resources/peer/`
    // name no handle, and enumeration is the thing this plane exists to prevent — so they are
    // `not_found` rather than an empty listing.
    let Some(token) = rest.strip_prefix('/').filter(|token| !token.is_empty()) else {
        // No handle to name, so nothing is traced. `not_found` rather than an empty listing:
        // enumeration is the thing this plane exists to prevent.
        return error_response(&PeerError::NotFound, None, None);
    };

    let generation = plane.generation();
    let hid = handle_id(token);

    match plane.redeem(token, audience).await {
        Ok((payload, read)) => {
            if let Some(trace) = &plane.trace {
                trace
                    .write_peer_handle_redeem(
                        &hid,
                        Some(payload.p.clone()),
                        generation,
                        audience.map(str::to_string),
                        Some(read.bytes.len() as u64),
                        Some(read.sha256.clone()),
                        "ok",
                        None,
                    )
                    .await;
            }
            ResourceResponse::framed(
                200,
                vec![
                    (
                        "content-type".to_string(),
                        "application/octet-stream".to_string(),
                    ),
                    ("etag".to_string(), format!("\"sha256:{}\"", read.sha256)),
                    ("x-murmur-mtime-ms".to_string(), read.mtime_ms.to_string()),
                    ("x-murmur-generation".to_string(), generation.to_string()),
                    (
                        "x-murmur-containment".to_string(),
                        plane.containment_achieved.to_string(),
                    ),
                    (HANDLE_ID_HEADER.to_string(), hid),
                ],
                read.bytes,
            )
        }
        Err(error) => {
            if let Some(trace) = &plane.trace {
                // A payload that failed the MAC is caller-controlled and must not be copied into
                // this capsule's own audit record as if it were fact — so `path` is null until the
                // MAC has proven the payload is ours.
                trace
                    .write_peer_handle_redeem(
                        &hid,
                        None,
                        generation,
                        audience.map(str::to_string),
                        None,
                        None,
                        error.code(),
                        Some(error.message()),
                    )
                    .await;
            }
            error_response(&error, Some(generation), Some(hid))
        }
    }
}

fn error_response(
    error: &PeerError,
    generation: Option<u64>,
    handle_id: Option<String>,
) -> ResourceResponse {
    let body = serde_json::json!({"error": error.code(), "message": error.message()});
    let mut headers = vec![("content-type".to_string(), "application/json".to_string())];
    if let Some(generation) = generation {
        headers.push(("x-murmur-generation".to_string(), generation.to_string()));
    }
    if let Some(handle_id) = handle_id {
        headers.push((HANDLE_ID_HEADER.to_string(), handle_id));
    }
    if matches!(error, PeerError::MethodNotAllowed) {
        headers.push(("allow".to_string(), "GET".to_string()));
    }
    ResourceResponse::framed(
        error.status(),
        headers,
        serde_json::to_vec(&body).unwrap_or_default(),
    )
}

// ── Naming a fetched file ─────────────────────────────────────────────────────

/// The workdir-relative path fetched bytes land at: `peer-in/<handle_id>-<sanitised basename>`.
///
/// Runtime-chosen, never peer-chosen. The basename is a hint read out of the token's unverified
/// payload and is sanitised to `[A-Za-z0-9._-]` before use; the `handle_id` prefix is what keeps
/// two fetches of two different handles from colliding even when the hint is identical or
/// useless.
pub fn stored_path_for(handle_id: &str, basename_hint: Option<&str>) -> String {
    let sanitised: String = basename_hint
        .and_then(|hint| hint.rsplit('/').next())
        .map(|name| {
            name.chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                        c
                    } else {
                        '_'
                    }
                })
                .take(120)
                .collect()
        })
        .filter(|name: &String| !name.is_empty() && name != "." && name != "..")
        .unwrap_or_else(|| "file".to_string());
    format!("{PEER_INBOX_DIR}/{handle_id}-{sanitised}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn key() -> PeerMintKey {
        PeerMintKey::generate().unwrap()
    }

    fn payload_for(path: &str, session: &str, exp: u64) -> HandlePayload {
        HandlePayload {
            v: 1,
            iss: session.to_string(),
            p: path.to_string(),
            exp,
            n: "0123456789abcdef0123456789abcdef".to_string(),
        }
    }

    fn in_an_hour() -> u64 {
        now_ms() + 3_600_000
    }

    #[test]
    fn a_minted_handle_round_trips_through_verify() {
        let key = key();
        let payload = payload_for("report.md", "ses_1", in_an_hour());
        let token = mint(&key, &payload, "reporter@localhost:41234").unwrap();
        assert!(token.starts_with("mh1."));
        assert_eq!(token.split('.').count(), 3);
        let verified = verify(&key, &token, "reporter@localhost:41234", "ses_1").unwrap();
        assert_eq!(verified, payload);
    }

    #[test]
    fn a_token_uses_only_url_safe_unpadded_base64() {
        let key = key();
        // A payload long enough that padding would be required by the padded alphabet.
        let token = mint(
            &key,
            &payload_for("a/b/c.md", "ses_padding", in_an_hour()),
            "x@y",
        )
        .unwrap();
        let segments: Vec<&str> = token.split('.').collect();
        for segment in &segments[1..] {
            assert!(
                segment
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
                "segment '{segment}' must be base64url with no padding"
            );
        }
    }

    #[test]
    fn a_tampered_payload_is_not_valid() {
        let key = key();
        let token = mint(
            &key,
            &payload_for("report.md", "ses_1", in_an_hour()),
            "a@b",
        )
        .unwrap();
        let mut chars: Vec<char> = token.chars().collect();
        let payload_end = token.rfind('.').unwrap();
        chars[payload_end - 1] = if chars[payload_end - 1] == 'A' {
            'B'
        } else {
            'A'
        };
        let tampered: String = chars.into_iter().collect();
        assert_eq!(
            verify(&key, &tampered, "a@b", "ses_1"),
            Err(HandleError::NotValid)
        );
    }

    #[test]
    fn a_tampered_mac_is_not_valid() {
        let key = key();
        let token = mint(
            &key,
            &payload_for("report.md", "ses_1", in_an_hour()),
            "a@b",
        )
        .unwrap();
        let mut chars: Vec<char> = token.chars().collect();
        let last = chars.len() - 1;
        chars[last] = if chars[last] == 'A' { 'B' } else { 'A' };
        let tampered: String = chars.into_iter().collect();
        assert_eq!(
            verify(&key, &tampered, "a@b", "ses_1"),
            Err(HandleError::NotValid)
        );
    }

    #[test]
    fn the_wrong_audience_is_not_valid() {
        let key = key();
        let token = mint(
            &key,
            &payload_for("report.md", "ses_1", in_an_hour()),
            "a@b",
        )
        .unwrap();
        assert_eq!(
            verify(&key, &token, "attacker@localhost:1", "ses_1"),
            Err(HandleError::NotValid)
        );
    }

    /// The three causes the design requires to be indistinguishable: a tampered payload, a
    /// foreign key, and a correct handle presented with the wrong audience. Same variant, same
    /// code, same message, byte for byte.
    #[test]
    fn every_refusal_cause_is_indistinguishable_on_the_wire() {
        let key = key();
        let other = PeerMintKey::generate().unwrap();
        let token = mint(
            &key,
            &payload_for("report.md", "ses_1", in_an_hour()),
            "a@b",
        )
        .unwrap();

        let mut chars: Vec<char> = token.chars().collect();
        let payload_end = token.rfind('.').unwrap();
        chars[payload_end - 1] = if chars[payload_end - 1] == 'A' {
            'B'
        } else {
            'A'
        };
        let tampered: String = chars.into_iter().collect();

        let causes = [
            verify(&key, &tampered, "a@b", "ses_1"),
            verify(&other, &token, "a@b", "ses_1"),
            verify(&key, &token, "attacker@localhost:1", "ses_1"),
        ];
        for cause in &causes {
            assert_eq!(*cause, Err(HandleError::NotValid));
        }
        let errors: Vec<PeerError> = causes
            .iter()
            .map(|cause| PeerError::from(*cause.as_ref().unwrap_err()))
            .collect();
        for error in &errors {
            assert_eq!(error, &PeerError::HandleNotValid);
            assert_eq!(error.status(), 403);
            assert_eq!(error.code(), errors[0].code());
            assert_eq!(error.message(), errors[0].message());
        }
    }

    #[test]
    fn a_handle_from_another_instance_of_the_same_capsule_is_not_valid() {
        let one = key();
        let two = key();
        let token = mint(
            &one,
            &payload_for("report.md", "ses_1", in_an_hour()),
            "a@b",
        )
        .unwrap();
        assert_eq!(
            verify(&two, &token, "a@b", "ses_1"),
            Err(HandleError::NotValid)
        );
    }

    /// A foreign `iss` with a valid MAC is unreachable in production — only this instance's key
    /// produces a MAC this instance accepts — but the branch exists, and it must answer with the
    /// same undivided refusal rather than a code of its own.
    #[test]
    fn a_foreign_issuer_is_not_valid_rather_than_a_code_of_its_own() {
        let key = key();
        let token = mint(
            &key,
            &payload_for("report.md", "ses_other", in_an_hour()),
            "a@b",
        )
        .unwrap();
        assert_eq!(
            verify(&key, &token, "a@b", "ses_1"),
            Err(HandleError::NotValid)
        );
    }

    #[test]
    fn an_expired_handle_reports_expiry() {
        let key = key();
        let token = mint(
            &key,
            &payload_for("report.md", "ses_1", now_ms() - 1),
            "a@b",
        )
        .unwrap();
        assert_eq!(
            verify(&key, &token, "a@b", "ses_1"),
            Err(HandleError::Expired)
        );
    }

    #[test]
    fn every_malformed_shape_is_malformed() {
        let key = key();
        for candidate in [
            "",
            "report.md",
            "mh1",
            "mh1.",
            "mh1.abc",
            "mh1.abc.def.ghi",
            "mh2.YWJj.YWJj",
            "mh1.!!!.YWJj",
            "mh1.YWJj.!!!",
        ] {
            assert_eq!(
                verify(&key, candidate, "a@b", "ses_1"),
                Err(HandleError::Malformed),
                "'{candidate}' should be malformed"
            );
        }
    }

    /// A payload that is not JSON, or carries a version this runtime does not mint, is malformed
    /// — but only once the MAC has proven it is ours. Minted here with a real key so the check
    /// downstream of the MAC is the one being exercised.
    #[test]
    fn a_non_json_or_wrong_version_payload_is_malformed_behind_the_mac() {
        let key = key();
        for payload_json in [
            b"not json".to_vec(),
            br#"{"v":2,"iss":"ses_1","p":"a","exp":99999999999999,"n":"00"}"#.to_vec(),
        ] {
            let payload_b64 = B64.encode(&payload_json);
            let mut mac = key.mac();
            mac.update(&mac_input(&payload_b64, "a@b"));
            let tag = B64.encode(mac.finalize().into_bytes());
            let token = format!("mh1.{payload_b64}.{tag}");
            assert_eq!(
                verify(&key, &token, "a@b", "ses_1"),
                Err(HandleError::Malformed)
            );
        }
    }

    #[test]
    fn the_audience_is_covered_by_the_mac_but_not_carried_in_the_token() {
        let key = key();
        let payload = payload_for("report.md", "ses_1", in_an_hour());
        let one = mint(&key, &payload, "reporter@localhost:1").unwrap();
        let two = mint(&key, &payload, "reporter@localhost:2").unwrap();
        let (one_payload, two_payload) = (
            one.split('.').nth(1).unwrap(),
            two.split('.').nth(1).unwrap(),
        );
        assert_eq!(
            one_payload, two_payload,
            "the payload segment must not vary with the audience"
        );
        assert_ne!(one, two, "the MAC must vary with the audience");
        let decoded = String::from_utf8(B64.decode(one_payload).unwrap()).unwrap();
        assert!(
            !decoded.contains("reporter@localhost:1"),
            "the audience must not appear in the payload: {decoded}"
        );
    }

    #[test]
    fn two_handles_for_the_same_file_and_audience_are_distinct() {
        let key = key();
        let plane_nonce_a = random_hex(NONCE_BYTES).unwrap();
        let plane_nonce_b = random_hex(NONCE_BYTES).unwrap();
        assert_ne!(plane_nonce_a, plane_nonce_b);
        let mut first = payload_for("report.md", "ses_1", in_an_hour());
        first.n = plane_nonce_a;
        let mut second = payload_for("report.md", "ses_1", in_an_hour());
        second.n = plane_nonce_b;
        assert_ne!(
            mint(&key, &first, "a@b").unwrap(),
            mint(&key, &second, "a@b").unwrap()
        );
    }

    #[test]
    fn a_handle_id_is_sixteen_lowercase_hex_characters() {
        let id = handle_id("mh1.abc.def");
        assert_eq!(id.len(), 16);
        assert!(id
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
        assert_eq!(id, handle_id("mh1.abc.def"), "handle_id must be stable");
        assert_ne!(id, handle_id("mh1.abc.deg"));
    }

    #[test]
    fn an_audience_comes_from_the_peers_own_card() {
        let card = serde_json::json!({"name": "Reporter", "url": "http://localhost:41234/"});
        assert_eq!(
            audience_from_card(&card).unwrap(),
            "reporter@localhost:41234"
        );
    }

    #[test]
    fn a_card_without_a_name_or_a_url_is_refused() {
        for card in [
            serde_json::json!({"url": "localhost:1"}),
            serde_json::json!({"name": "a"}),
            serde_json::json!({"name": "", "url": "localhost:1"}),
            serde_json::json!({"name": "a", "url": ""}),
            serde_json::json!("not an object"),
        ] {
            assert!(audience_from_card(&card).is_err(), "card: {card}");
        }
    }

    #[test]
    fn own_audience_matches_what_a_peer_would_derive_from_the_card() {
        let identity = CapsuleIdentity {
            capsule_name: "Reporter".to_string(),
            capsule_version: "0.1.0".to_string(),
            session_id: "ses_1".to_string(),
            capsule_url: "localhost:41234".to_string(),
        };
        let card = serde_json::json!({
            "name": identity.capsule_name,
            "url": identity.capsule_url,
        });
        assert_eq!(own_audience(&identity), audience_from_card(&card).unwrap());
    }

    #[test]
    fn every_peer_error_has_the_status_the_wire_contract_states() {
        let expected: &[(PeerError, u16, &str)] = &[
            (PeerError::NoPeerPlane, 404, "no_peer_plane"),
            (PeerError::MalformedHandle, 400, "malformed_handle"),
            (PeerError::MissingAudience, 400, "missing_audience"),
            (PeerError::HandleNotValid, 403, "handle_not_valid"),
            (PeerError::HandleExpired, 410, "handle_expired"),
            (PeerError::NotFound, 404, "not_found"),
            (PeerError::OutsideRoot, 403, "outside_root"),
            (PeerError::SymlinkRefused, 403, "symlink_refused"),
            (PeerError::NotARegularFile, 403, "not_a_regular_file"),
            (PeerError::TooLarge { max_bytes: 1 }, 413, "too_large"),
            (PeerError::MethodNotAllowed, 405, "method_not_allowed"),
            (PeerError::IoError("x".to_string()), 500, "io_error"),
        ];
        for (error, status, code) in expected {
            assert_eq!(error.status(), *status, "{code}");
            assert_eq!(error.code(), *code);
            assert!(!error.message().is_empty(), "{code}");
        }
    }

    #[test]
    fn a_handle_never_survives_the_trace_boundary() {
        let key = key();
        let token = mint(
            &key,
            &payload_for("report.md", "ses_1", in_an_hour()),
            "a@b",
        )
        .unwrap();
        let id = handle_id(&token);

        let sentence = format!("fetching {token} from the peer.");
        let redacted = redact_handle_tokens(&sentence);
        assert_eq!(redacted, format!("fetching <handle:{id}> from the peer."));
        assert!(!redacted.contains("mh1."));

        // Two in one string, and one at the very end with nothing after it.
        let pair = format!("{token} and {token}");
        assert_eq!(
            redact_handle_tokens(&pair),
            format!("<handle:{id}> and <handle:{id}>")
        );

        // Text that merely mentions the tag is left alone and not copied.
        assert!(matches!(
            redact_handle_tokens("no handle here, just mh1 as a word"),
            std::borrow::Cow::Borrowed(_)
        ));
        assert!(matches!(
            redact_handle_tokens("nothing at all"),
            std::borrow::Cow::Borrowed(_)
        ));
        // Non-ASCII around a token must not panic on a mid-codepoint index.
        let dashed = format!("— {token} —");
        assert_eq!(redact_handle_tokens(&dashed), format!("— <handle:{id}> —"));
    }

    #[test]
    fn a_handle_never_survives_the_trace_boundary_in_json() {
        let key = key();
        let token = mint(
            &key,
            &payload_for("report.md", "ses_1", in_an_hour()),
            "a@b",
        )
        .unwrap();
        let mut value = serde_json::json!({
            "peer": "localhost:1",
            "handle": token,
            "nested": {"list": [token.clone(), "unrelated"]},
        });
        redact_handles_in_json(&mut value);
        let rendered = value.to_string();
        assert!(!rendered.contains("mh1."), "{rendered}");
        assert!(rendered.contains(&format!("<handle:{}>", handle_id(&token))));
        assert_eq!(value["peer"], "localhost:1");
        assert_eq!(value["nested"]["list"][1], "unrelated");
    }

    #[test]
    fn a_stored_path_is_runtime_chosen_and_sanitised() {
        assert_eq!(
            stored_path_for("3f2a", Some("report.md")),
            "peer-in/3f2a-report.md"
        );
        assert_eq!(
            stored_path_for("3f2a", Some("../../etc/passwd")),
            "peer-in/3f2a-passwd"
        );
        assert_eq!(stored_path_for("3f2a", Some("..")), "peer-in/3f2a-file");
        assert_eq!(stored_path_for("3f2a", Some("")), "peer-in/3f2a-file");
        assert_eq!(stored_path_for("3f2a", None), "peer-in/3f2a-file");
        assert_eq!(
            stored_path_for("3f2a", Some("a b;rm -rf.md")),
            "peer-in/3f2a-a_b_rm_-rf.md"
        );
    }

    // ── The plane, against a real filesystem ─────────────────────────────────

    fn plane_for(root: &Path, export: PeerFilesExport) -> PeerPlane {
        PeerPlane::new(
            root,
            Some((&export, Arc::new(key()))),
            "ses_test".to_string(),
            ContainmentClass::Sealed,
            Arc::new(AtomicU64::new(0)),
            None,
        )
    }

    /// The deny case: a plane that exists — so a refusal still has somewhere to be recorded — with
    /// nothing declared to serve from.
    fn undeclared_plane() -> PeerPlane {
        PeerPlane::new(
            Path::new("/nonexistent"),
            None,
            "ses_test".to_string(),
            ContainmentClass::Sealed,
            Arc::new(AtomicU64::new(0)),
            None,
        )
    }

    fn peer_export(root: &str) -> PeerFilesExport {
        PeerFilesExport {
            root: root.to_string(),
            max_ttl_secs: Some(60),
            max_bytes: 1024 * 1024,
        }
    }

    #[test]
    fn a_mint_names_a_file_under_the_root_and_redeems_to_its_bytes() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("out")).unwrap();
        fs::write(dir.path().join("out/report.md"), b"known bytes").unwrap();
        let plane = plane_for(dir.path(), peer_export("out/"));

        let minted = plane.mint_handle("report.md", "a@b", None).unwrap();
        assert_eq!(minted.path, "report.md");
        assert!(minted.handle.starts_with("mh1."));

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (payload, read) = runtime
            .block_on(plane.redeem(&minted.handle, Some("a@b")))
            .unwrap();
        assert_eq!(payload.p, "report.md");
        assert_eq!(read.bytes, b"known bytes");
    }

    #[test]
    fn a_mint_cannot_name_anything_outside_the_declared_subtree() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("out/handoff")).unwrap();
        fs::write(dir.path().join("out/handoff/ok.md"), b"fine").unwrap();
        fs::write(dir.path().join("out/secret.md"), b"SECRET").unwrap();
        fs::write(dir.path().join("secret-at-top.md"), b"SECRET").unwrap();
        std::os::unix::fs::symlink("../secret.md", dir.path().join("out/handoff/escape.md"))
            .unwrap();
        let plane = plane_for(dir.path(), peer_export("out/handoff/"));

        for attempt in [
            "../secret.md",
            "%2e%2e%2fsecret.md",
            "/etc/passwd",
            "escape.md",
            "handoff/../../secret-at-top.md",
        ] {
            let error = plane
                .mint_handle(attempt, "a@b", None)
                .expect_err("'{attempt}' must not mint");
            assert!(
                matches!(error, PeerError::OutsideRoot | PeerError::SymlinkRefused),
                "'{attempt}' produced {error:?}"
            );
        }
        assert!(plane.mint_handle("ok.md", "a@b", None).is_ok());
    }

    #[test]
    fn a_mint_refuses_a_directory_rather_than_handing_out_a_handle_that_cannot_redeem() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("out/nested")).unwrap();
        let plane = plane_for(dir.path(), peer_export("out/"));
        assert_eq!(
            plane.mint_handle("nested", "a@b", None).unwrap_err(),
            PeerError::NotARegularFile
        );
        assert_eq!(
            plane.mint_handle("missing.md", "a@b", None).unwrap_err(),
            PeerError::NotFound
        );
    }

    #[test]
    fn a_ttl_may_only_narrow() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("out")).unwrap();
        fs::write(dir.path().join("out/report.md"), b"x").unwrap();
        let plane = plane_for(dir.path(), peer_export("out/"));

        // Bracketed by clock readings taken either side of each mint, so the assertion is exact
        // rather than slack-tolerant: a loaded machine makes the mint slower, not the ceiling
        // looser.
        let before = now_ms();
        let clamped = plane.mint_handle("report.md", "a@b", Some(86_400)).unwrap();
        let after = now_ms();
        assert!(
            (before + 60_000..=after + 60_000).contains(&clamped.expires_at_ms),
            "a ttl above max_ttl must be clamped down to it"
        );

        let before = now_ms();
        let narrowed = plane.mint_handle("report.md", "a@b", Some(5)).unwrap();
        let after = now_ms();
        assert!(
            (before + 5_000..=after + 5_000).contains(&narrowed.expires_at_ms),
            "a ttl below max_ttl is applied as given"
        );

        let before = now_ms();
        let defaulted = plane.mint_handle("report.md", "a@b", None).unwrap();
        let after = now_ms();
        assert!(
            (before + 60_000..=after + 60_000).contains(&defaulted.expires_at_ms),
            "an absent ttl means max_ttl"
        );
    }

    /// A handle authorises a file, not a version of one: the second redeem serves the newer bytes
    /// and a different `etag`, and never a conflict.
    #[test]
    fn a_rewritten_file_redeems_to_its_current_bytes() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("out")).unwrap();
        fs::write(dir.path().join("out/report.md"), b"first").unwrap();
        let plane = plane_for(dir.path(), peer_export("out/"));
        let minted = plane.mint_handle("report.md", "a@b", None).unwrap();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (_, first) = runtime
            .block_on(plane.redeem(&minted.handle, Some("a@b")))
            .unwrap();
        assert_eq!(first.bytes, b"first");

        fs::write(dir.path().join("out/report.md.tmp"), b"second and longer").unwrap();
        fs::rename(
            dir.path().join("out/report.md.tmp"),
            dir.path().join("out/report.md"),
        )
        .unwrap();

        let (_, second) = runtime
            .block_on(plane.redeem(&minted.handle, Some("a@b")))
            .unwrap();
        assert_eq!(second.bytes, b"second and longer");
        assert_ne!(first.sha256, second.sha256);
    }

    #[test]
    fn a_redeem_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("out")).unwrap();
        fs::write(dir.path().join("out/report.md"), b"same bytes").unwrap();
        let plane = plane_for(dir.path(), peer_export("out/"));
        let minted = plane.mint_handle("report.md", "a@b", None).unwrap();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let first = runtime
            .block_on(plane.redeem(&minted.handle, Some("a@b")))
            .unwrap()
            .1;
        for _ in 0..4 {
            let again = runtime
                .block_on(plane.redeem(&minted.handle, Some("a@b")))
                .unwrap()
                .1;
            assert_eq!(again.bytes, first.bytes);
            assert_eq!(again.sha256, first.sha256);
        }
    }

    #[test]
    fn a_file_above_max_bytes_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("out")).unwrap();
        fs::write(dir.path().join("out/big.bin"), vec![0u8; 64]).unwrap();
        let plane = plane_for(
            dir.path(),
            PeerFilesExport {
                root: "out/".to_string(),
                max_ttl_secs: Some(60),
                max_bytes: 16,
            },
        );
        let minted = plane.mint_handle("big.bin", "a@b", None).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        assert_eq!(
            runtime
                .block_on(plane.redeem(&minted.handle, Some("a@b")))
                .unwrap_err(),
            PeerError::TooLarge { max_bytes: 16 }
        );
    }

    // ── Routing ──────────────────────────────────────────────────────────────

    #[test]
    fn only_the_peer_segment_routes_to_the_peer_plane() {
        assert!(is_peer_path("/resources/peer"));
        assert!(is_peer_path("/resources/peer/"));
        assert!(is_peer_path("/resources/peer/mh1.a.b"));
        assert!(is_peer_path("/resources/peer?x=1"));
        assert!(!is_peer_path("/resources/peerless/x"));
        assert!(!is_peer_path("/resources/files/report.md"));
        assert!(!is_peer_path("/"));
    }

    fn respond(
        plane: &PeerPlane,
        method: &str,
        path: &str,
        audience: Option<&str>,
    ) -> ResourceResponse {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(handle_peer_request(plane, method, path, audience))
    }

    fn body_error(response: &ResourceResponse) -> serde_json::Value {
        serde_json::from_slice(&response.body).unwrap()
    }

    #[test]
    fn an_undeclared_capsule_answers_no_peer_plane_and_mints_nothing() {
        let plane = undeclared_plane();
        assert!(!plane.is_declared());
        assert_eq!(plane.max_ttl_secs(), None);

        let response = respond(&plane, "GET", "/resources/peer/mh1.YWJj.YWJj", Some("a@b"));
        assert_eq!(response.status, 404);
        assert_eq!(body_error(&response)["error"], "no_peer_plane");

        assert_eq!(
            plane.mint_handle("report.md", "a@b", None).unwrap_err(),
            PeerError::NoPeerPlane
        );
    }

    #[test]
    fn there_is_no_list_verb() {
        let plane = undeclared_plane();
        for path in ["/resources/peer", "/resources/peer/"] {
            let response = respond(&plane, "GET", path, Some("a@b"));
            assert_eq!(response.status, 404, "{path}");
            assert_eq!(body_error(&response)["error"], "not_found", "{path}");
            let body = String::from_utf8_lossy(&response.body);
            assert!(
                !body.contains('['),
                "a listing must not be reachable: {body}"
            );
        }
    }

    #[test]
    fn every_write_verb_is_refused_with_an_allow_header() {
        let plane = undeclared_plane();
        for method in ["POST", "PUT", "PATCH", "DELETE", "HEAD"] {
            let response = respond(&plane, method, "/resources/peer/mh1.YWJj.YWJj", Some("a@b"));
            assert_eq!(response.status, 405, "{method}");
            assert_eq!(body_error(&response)["error"], "method_not_allowed");
            assert!(response
                .headers
                .iter()
                .any(|(name, value)| name == "allow" && value == "GET"));
        }
    }

    #[test]
    fn a_declared_plane_serves_a_redeem_and_refuses_every_tampered_form_identically() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("out")).unwrap();
        fs::write(dir.path().join("out/report.md"), b"known bytes").unwrap();
        let plane = plane_for(dir.path(), peer_export("out/"));
        let minted = plane.mint_handle("report.md", "a@b", None).unwrap();

        let ok = respond(
            &plane,
            "GET",
            &format!("/resources/peer/{}", minted.handle),
            Some("a@b"),
        );
        assert_eq!(ok.status, 200);
        assert_eq!(ok.body, b"known bytes");
        assert!(ok
            .headers
            .iter()
            .any(|(name, value)| name == HANDLE_ID_HEADER && value == &minted.handle_id));
        assert!(
            !ok.headers
                .iter()
                .any(|(name, _)| name == "x-murmur-export-root"),
            "the peer plane discloses no path structure"
        );

        let flip_last = |text: &str| {
            let mut chars: Vec<char> = text.chars().collect();
            let last = chars.len() - 1;
            chars[last] = if chars[last] == 'A' { 'B' } else { 'A' };
            chars.into_iter().collect::<String>()
        };
        let segments: Vec<&str> = minted.handle.split('.').collect();
        let bad_payload = format!("mh1.{}.{}", flip_last(segments[1]), segments[2]);
        let bad_mac = format!("mh1.{}.{}", segments[1], flip_last(segments[2]));

        let refusals = [
            respond(
                &plane,
                "GET",
                &format!("/resources/peer/{bad_payload}"),
                Some("a@b"),
            ),
            respond(
                &plane,
                "GET",
                &format!("/resources/peer/{bad_mac}"),
                Some("a@b"),
            ),
            respond(
                &plane,
                "GET",
                &format!("/resources/peer/{}", minted.handle),
                Some("attacker@localhost:1"),
            ),
        ];
        for refusal in &refusals {
            assert_eq!(refusal.status, 403);
            assert_eq!(refusal.body, refusals[0].body);
            assert_eq!(body_error(refusal)["error"], "handle_not_valid");
            assert!(
                !String::from_utf8_lossy(&refusal.body).contains("known bytes"),
                "a refusal must serve none of the file's bytes"
            );
        }

        let no_audience = respond(
            &plane,
            "GET",
            &format!("/resources/peer/{}", minted.handle),
            None,
        );
        assert_eq!(no_audience.status, 400);
        assert_eq!(body_error(&no_audience)["error"], "missing_audience");

        let not_a_token = respond(&plane, "GET", "/resources/peer/report.md", Some("a@b"));
        assert_eq!(not_a_token.status, 400);
        assert_eq!(body_error(&not_a_token)["error"], "malformed_handle");
    }

    /// The audience header is required *before* the MAC is checked, so an omitted header and a
    /// wrong one are different answers — but neither reveals anything about the file.
    #[test]
    fn a_malformed_shape_is_reported_before_the_audience_is_required() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("out")).unwrap();
        let plane = plane_for(dir.path(), peer_export("out/"));
        let response = respond(&plane, "GET", "/resources/peer/not-a-token", None);
        assert_eq!(body_error(&response)["error"], "malformed_handle");
    }

    #[test]
    fn an_expired_handle_is_refused_with_its_own_code() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("out")).unwrap();
        fs::write(dir.path().join("out/report.md"), b"bytes").unwrap();
        let plane = plane_for(
            dir.path(),
            PeerFilesExport {
                root: "out/".to_string(),
                max_ttl_secs: Some(1),
                max_bytes: 1024,
            },
        );
        let minted = plane.mint_handle("report.md", "a@b", Some(1)).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let response = respond(
            &plane,
            "GET",
            &format!("/resources/peer/{}", minted.handle),
            Some("a@b"),
        );
        assert_eq!(response.status, 410);
        assert_eq!(body_error(&response)["error"], "handle_expired");
    }

    /// `Drop` is `zeroize` and nothing else, so testing the helper tests the teardown guarantee
    /// without reading memory the value has already released.
    #[test]
    fn zeroizing_clears_every_key_byte() {
        let mut key = PeerMintKey([0xAB; 32]);
        assert_ne!(key.0, [0u8; 32]);
        key.zeroize();
        assert_eq!(key.0, [0u8; 32]);
    }

    /// A generated key is not the all-zero array a zeroized one becomes, and two are distinct —
    /// enough to catch a `generate` that stopped reaching the OS CSPRNG.
    #[test]
    fn a_generated_key_is_random() {
        let one = key();
        let two = key();
        assert_ne!(one.0, [0u8; 32]);
        assert_ne!(one.0, two.0);
    }
}
