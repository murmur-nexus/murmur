//! The minting authority behind a delegated spawn: who is asking, what was approved, and for how
//! long.
//!
//! The daemon takes no caller's word for which session it is. Two MAC'd tokens over one
//! memory-only key answer that, and what was approved:
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
//! The token grammar, the key and the verification order are
//! [`capsule_runtime::mac_token`]'s; this module is the payloads, the two families' tags and
//! domains, and the spent-`jti` set that makes an approval single-use.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use capsule_runtime::mac_token::{self, MacTokenError, MintKey, NONCE_BYTES};
use capsule_runtime::{SpawnApproval, SpawnCredential};
use serde::{Deserialize, Serialize};

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

/// The stable identifier a mint and a redemption are correlated by.
///
/// This — never the token — is what may appear in a log line, a trace or an error body.
pub fn token_id(token: &str) -> String {
    mac_token::token_id(token)
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
            n: mac_token::random_hex(NONCE_BYTES)?,
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
            jti: mac_token::random_hex(NONCE_BYTES)?,
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

    fn seal(&self, tag: &str, domain: &[u8], payload_json: &[u8]) -> String {
        mac_token::seal(&self.key, tag, domain, payload_json, &[])
    }

    /// The inverse of [`SpawnAuthority::seal`], for one family only.
    fn open<T: for<'de> Deserialize<'de>>(
        &self,
        tag: &str,
        domain: &[u8],
        token: &str,
    ) -> Result<T, AuthorityError> {
        let payload = mac_token::open(&self.key, tag, domain, token, &[])
            .map_err(|_: MacTokenError| AuthorityError::Unauthenticated)?;

        // Only now: the payload has been proven to be one this daemon minted, so reading it is
        // reading our own record rather than trusting the caller's.
        serde_json::from_slice(&payload).map_err(|_| AuthorityError::Unauthenticated)
    }
}

/// Unix milliseconds. A clock before the epoch reads as `0`, which expires everything rather than
/// accepting anything.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}
