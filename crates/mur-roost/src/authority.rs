//! The minting authority behind a delegated spawn: who is asking, what was approved, and for how
//! long.
//!
//! `spawned_by` used to be the whole of the daemon's idea of who a caller was, and it was a claim
//! rather than a proof — anything that reached the loopback port could name a well-provisioned
//! session and be judged against that session's envelope. This module replaces the claim with two
//! MAC'd tokens over one memory-only key.
//!
//! * A **credential** binds a token to a session id. It is minted once, when a session that
//!   declares `capabilities.spawn.allow` is staged, and it is the only thing that answers *which
//!   session is asking*.
//! * An **approval** binds a token to a session id, one artifact — by name, version and content
//!   hash — an absolute expiry and one `jti`. `POST /delegate` mints one after the referee has
//!   passed; `POST /spawn` redeems it, once.
//!
//! **The key is process-scoped.** 32 bytes from the OS at startup, held only in memory, zeroed on
//! drop. A credential minted by a previous daemon verifies against nothing, which is what makes
//! restarting the daemon a complete revocation with no revocation list to maintain.
//!
//! **The two token families cannot be confused for one another.** Distinct version tags and
//! distinct MAC domain separators mean a credential presented as an approval fails its MAC, so a
//! session cannot approve its own launch by handing back the token it was given.
//!
//! The shape of the token — opaque, MAC'd over a base64url payload, verified shape → MAC →
//! payload → expiry — is the one `capsule_runtime::peer_handoff` mints file handles with, one
//! authority further out. The reasoning transfers unchanged: a failure below the MAC teaches a
//! prober which field to change next, so nothing below the MAC is evaluated until it passes.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use capsule_runtime::{SpawnApproval, SpawnCredential};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

/// How long an approval is good for. Long enough for the caller to turn around and redeem it,
/// short enough that a token scraped out of a process at rest is already dead.
pub const SPAWN_APPROVAL_TTL_SECS: u64 = 60;

/// Version tag and first segment of every credential.
const CREDENTIAL_VERSION_TAG: &str = "msc1";

/// Version tag and first segment of every approval.
const APPROVAL_VERSION_TAG: &str = "msa1";

/// The only payload version this daemon mints or accepts, for either family.
const PAYLOAD_VERSION: u8 = 1;

/// Domain separator prefixed to every credential MAC input.
const CREDENTIAL_MAC_DOMAIN: &[u8] = b"murmur-spawn-credential-v1";

/// Domain separator prefixed to every approval MAC input. Distinct from the credential's, so a
/// credential can never verify as an approval or the reverse.
const APPROVAL_MAC_DOMAIN: &[u8] = b"murmur-spawn-approval-v1";

/// Separator between the MAC input's fields. ASCII unit separator: it cannot occur in base64url,
/// so no pair of distinct inputs can produce the same MAC input by moving the boundary between
/// them.
const MAC_FIELD_SEPARATOR: u8 = 0x1f;

/// Nonce width in bytes, for both the credential's `n` and the approval's `jti`.
const NONCE_BYTES: usize = 16;

/// Width of a [`token_id`], in lowercase hex characters.
const TOKEN_ID_HEX_CHARS: usize = 16;

/// The base64 alphabet used for both token segments: URL-safe and unpadded, so a whole token is
/// safe in a header value and has no `=` for a transport to mangle.
const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::URL_SAFE_NO_PAD;

type HmacSha256 = Hmac<Sha256>;

// ── The minting key ───────────────────────────────────────────────────────────

/// The 32-byte HMAC key this daemon process mints and verifies with.
///
/// Never written to disk, never placed in an environment variable, and never copied out of this
/// type: a key that reaches durable storage outlives the process whose lifetime is the only
/// revocation mechanism there is.
struct MintKey([u8; 32]);

impl MintKey {
    fn generate() -> Result<Self, String> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes)
            .map_err(|error| format!("failed to generate the spawn authority key: {error}"))?;
        Ok(Self(bytes))
    }

    /// Builds a fresh MAC context. Private: the key bytes never leave this type.
    fn mac(&self) -> HmacSha256 {
        HmacSha256::new_from_slice(&self.0).expect("HMAC-SHA256 accepts a key of any length")
    }

    /// Overwrites the key bytes with volatile writes.
    ///
    /// Volatile so the compiler cannot elide a write whose result is provably never read — which
    /// is the whole of what the write is for.
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

impl Drop for MintKey {
    /// When the daemon exits the key goes with it, and every outstanding credential and approval
    /// becomes unverifiable at once.
    fn drop(&mut self) {
        self.zeroize();
    }
}

impl std::fmt::Debug for MintKey {
    /// Prints no key material. A key that can reach a log line is a key on disk by another route.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MintKey(<redacted>)")
    }
}

// ── Payloads ──────────────────────────────────────────────────────────────────

/// A credential's payload, carried in the clear in the token's middle segment.
///
/// Readable by anyone holding the token, and deliberately so: the payload is not the secret, the
/// MAC is what makes it unforgeable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CredentialPayload {
    /// Payload version. Only [`PAYLOAD_VERSION`] is minted or accepted.
    v: u8,
    /// The session this credential names — the id `stage_session` minted for it.
    sid: String,
    /// [`NONCE_BYTES`] random bytes, lowercase hex, so two credentials for the same session are
    /// distinct and independently correlatable.
    n: String,
}

/// What one approval approves: one session, launching one artifact, once, before one instant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalPayload {
    /// Payload version. Only [`PAYLOAD_VERSION`] is minted or accepted.
    pub v: u8,
    /// The session that earned this approval. Redemption requires the credential naming this same
    /// session, so an approval is not redeemable by whichever session finds it.
    pub sid: String,
    /// The artifact name the daemon resolved at `/delegate`.
    pub name: String,
    /// The artifact version the daemon resolved at `/delegate`.
    pub version: String,
    /// The resolved artifact's sha256, lowercase hex. This is what makes the approval name an
    /// artifact rather than a coordinate: the same name and version resolving to different bytes
    /// is a different artifact and a different manifest, and therefore a decision the referee
    /// never made.
    pub digest: String,
    /// Absolute expiry, in unix milliseconds.
    pub exp: u64,
    /// [`NONCE_BYTES`] random bytes, lowercase hex. The daemon marks this spent on redemption,
    /// which is the whole of what makes an approval single-use.
    pub jti: String,
}

/// Why a token was not accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorityError {
    /// The token was absent, malformed, failed its MAC, named a different session, or was of the
    /// wrong family. One variant for all of them, deliberately: see [`SpawnAuthority::verify_credential`].
    Unauthenticated,
    /// The MAC verified and `exp` is in the past.
    Expired,
    /// The MAC verified, the approval is live, and its `jti` was already redeemed.
    AlreadyRedeemed,
}

/// The stable identifier a mint and a redemption are correlated by: the first
/// [`TOKEN_ID_HEX_CHARS`] lowercase hex characters of `sha256(token)`.
///
/// This — never the token — is what may appear in a log line, a trace or an error body.
pub fn token_id(token: &str) -> String {
    murmur_artifact::sha256_hex(token.as_bytes())[..TOKEN_ID_HEX_CHARS].to_string()
}

// ── The authority ─────────────────────────────────────────────────────────────

/// The daemon's minting key and its record of which approvals have been spent.
#[derive(Debug)]
pub struct SpawnAuthority {
    key: MintKey,
    /// Redeemed `jti`s, each held with the expiry of the approval it came from so the set can be
    /// pruned rather than grown without bound. An entry past its expiry is redundant: the approval
    /// it names would be refused as expired before the replay check is ever reached.
    spent: Mutex<HashMap<String, u64>>,
}

impl SpawnAuthority {
    /// A fresh authority with a key from the operating system's CSPRNG.
    pub fn generate() -> Result<Self, String> {
        Ok(Self {
            key: MintKey::generate()?,
            spent: Mutex::new(HashMap::new()),
        })
    }

    /// Mints the credential a session presents to prove which session it is.
    pub fn mint_credential(&self, session_id: &str) -> Result<SpawnCredential, String> {
        Ok(SpawnCredential::new(
            self.mint_credential_token(session_id)?,
        ))
    }

    /// The same credential as wire text.
    ///
    /// The daemon needs this to put a token on the wire; the runtime that receives it gets a
    /// [`SpawnCredential`], which has no way out but `expose`. Minting the text requires the key,
    /// so this widens nothing: whoever can call it is already the authority.
    pub fn mint_credential_token(&self, session_id: &str) -> Result<String, String> {
        let payload = CredentialPayload {
            v: PAYLOAD_VERSION,
            sid: session_id.to_string(),
            n: random_hex()?,
        };
        let json = serde_json::to_vec(&payload)
            .map_err(|error| format!("failed to serialize the credential payload: {error}"))?;
        Ok(self.seal(CREDENTIAL_VERSION_TAG, CREDENTIAL_MAC_DOMAIN, &json))
    }

    /// Verifies a credential and returns the session id it names.
    ///
    /// Check order is fixed and nothing downstream of the MAC is evaluated before it: shape → MAC
    /// → payload. The MAC comparison is `verify_slice`, which is constant-time.
    ///
    /// **[`AuthorityError::Unauthenticated`] is deliberately one outcome.** A tampered payload, a
    /// credential minted by a previous daemon, and an approval presented in a credential's place
    /// are indistinguishable to the caller. Splitting them builds an oracle that tells a prober
    /// which field to change next.
    pub fn verify_credential(&self, token: &str) -> Result<String, AuthorityError> {
        let payload: CredentialPayload =
            self.open(CREDENTIAL_VERSION_TAG, CREDENTIAL_MAC_DOMAIN, token)?;
        if payload.v != PAYLOAD_VERSION {
            return Err(AuthorityError::Unauthenticated);
        }
        Ok(payload.sid)
    }

    /// Mints one approval over an artifact the daemon has already resolved and the referee has
    /// already passed.
    ///
    /// `expires_at_ms` is absolute rather than a duration so a caller — including a test — states
    /// the instant it means, and so the value in the payload is the value the daemon answered
    /// with.
    pub fn mint_approval(
        &self,
        session_id: &str,
        name: &str,
        version: &str,
        digest: &str,
        expires_at_ms: u64,
    ) -> Result<SpawnApproval, String> {
        Ok(SpawnApproval::new(self.mint_approval_token(
            session_id,
            name,
            version,
            digest,
            expires_at_ms,
        )?))
    }

    /// The same approval as wire text, for the `POST /delegate` response body.
    pub fn mint_approval_token(
        &self,
        session_id: &str,
        name: &str,
        version: &str,
        digest: &str,
        expires_at_ms: u64,
    ) -> Result<String, String> {
        let payload = ApprovalPayload {
            v: PAYLOAD_VERSION,
            sid: session_id.to_string(),
            name: name.to_string(),
            version: version.to_string(),
            digest: digest.to_string(),
            exp: expires_at_ms,
            jti: random_hex()?,
        };
        let json = serde_json::to_vec(&payload)
            .map_err(|error| format!("failed to serialize the approval payload: {error}"))?;
        Ok(self.seal(APPROVAL_VERSION_TAG, APPROVAL_MAC_DOMAIN, &json))
    }

    /// Verifies one approval against `session_id` and marks it spent.
    ///
    /// Shape → MAC → payload → session binding → expiry → replay, and the `jti` is recorded as
    /// spent the moment all of those pass. The caller's artifact check runs *after* this returns,
    /// so an approval presented for the wrong artifact is spent rather than retryable: an approval
    /// names one artifact, and presenting it for another is an error rather than a near-miss.
    pub fn redeem_approval(
        &self,
        token: &str,
        session_id: &str,
    ) -> Result<ApprovalPayload, AuthorityError> {
        let payload: ApprovalPayload =
            self.open(APPROVAL_VERSION_TAG, APPROVAL_MAC_DOMAIN, token)?;
        if payload.v != PAYLOAD_VERSION || payload.sid != session_id {
            return Err(AuthorityError::Unauthenticated);
        }
        let now = now_ms();
        if payload.exp <= now {
            return Err(AuthorityError::Expired);
        }

        let mut spent = self
            .spent
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        spent.retain(|_, expiry| *expiry > now);
        if spent.insert(payload.jti.clone(), payload.exp).is_some() {
            return Err(AuthorityError::AlreadyRedeemed);
        }
        Ok(payload)
    }

    /// `<tag>.<base64url-nopad(payload JSON)>.<base64url-nopad(mac)>`.
    fn seal(&self, tag: &str, domain: &[u8], payload_json: &[u8]) -> String {
        let payload_b64 = B64.encode(payload_json);
        let mut mac = self.key.mac();
        mac.update(&mac_input(domain, &payload_b64));
        let signature = B64.encode(mac.finalize().into_bytes());
        format!("{tag}.{payload_b64}.{signature}")
    }

    /// The inverse of [`SpawnAuthority::seal`], for one family only.
    fn open<T: for<'de> Deserialize<'de>>(
        &self,
        tag: &str,
        domain: &[u8],
        token: &str,
    ) -> Result<T, AuthorityError> {
        let (payload_b64, payload_bytes, signature) = split_token(tag, token)?;
        let mut mac = self.key.mac();
        mac.update(&mac_input(domain, payload_b64));
        mac.verify_slice(&signature)
            .map_err(|_| AuthorityError::Unauthenticated)?;

        // Only now: the payload has been proven to be one this daemon minted, so reading it is
        // reading our own record rather than trusting the caller's.
        serde_json::from_slice(&payload_bytes).map_err(|_| AuthorityError::Unauthenticated)
    }
}

/// The bytes a token's MAC is taken over: `<domain> ‖ 0x1f ‖ <payload base64url>`.
fn mac_input(domain: &[u8], payload_b64: &str) -> Vec<u8> {
    let mut input = Vec::with_capacity(domain.len() + payload_b64.len() + 1);
    input.extend_from_slice(domain);
    input.push(MAC_FIELD_SEPARATOR);
    input.extend_from_slice(payload_b64.as_bytes());
    input
}

/// The token's three segments, checked for shape alone. Nothing here looks at what the payload
/// *says*.
fn split_token<'a>(
    expected_tag: &str,
    token: &'a str,
) -> Result<(&'a str, Vec<u8>, Vec<u8>), AuthorityError> {
    let mut segments = token.split('.');
    let tag = segments.next().ok_or(AuthorityError::Unauthenticated)?;
    let payload_b64 = segments.next().ok_or(AuthorityError::Unauthenticated)?;
    let signature_b64 = segments.next().ok_or(AuthorityError::Unauthenticated)?;
    if segments.next().is_some() || tag != expected_tag {
        return Err(AuthorityError::Unauthenticated);
    }
    let payload = B64
        .decode(payload_b64)
        .map_err(|_| AuthorityError::Unauthenticated)?;
    let signature = B64
        .decode(signature_b64)
        .map_err(|_| AuthorityError::Unauthenticated)?;
    if payload.is_empty() || signature.is_empty() {
        return Err(AuthorityError::Unauthenticated);
    }
    Ok((payload_b64, payload, signature))
}

fn random_hex() -> Result<String, String> {
    let mut bytes = [0u8; NONCE_BYTES];
    getrandom::fill(&mut bytes).map_err(|error| format!("failed to generate a nonce: {error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Unix milliseconds. A clock before the epoch reads as `0`, which expires everything rather than
/// accepting anything.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}
