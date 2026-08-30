//! One opaque token grammar, and the memory-only key that seals it.
//!
//! ```text
//! <tag> "." base64url-nopad(payload JSON) "." base64url-nopad(HMAC-SHA256)
//! ```
//!
//! Two authorities mint tokens in this shape: the peer file handles in [`crate::peer_handoff`],
//! and the spawn credentials and approvals `mur-roost` issues. The parts that must not vary live
//! here — the constant-time comparison, the unambiguous MAC input, and a key that never leaves the
//! process it was generated in.
//!
//! **A token is bound to its family by two values a caller cannot influence**: the version tag,
//! compared before the MAC, and the MAC domain, mixed into the MAC. Distinct pairs mean a token of
//! one family can never verify as another, so an authority that mints several families cannot be
//! made to accept one in another's place.
//!
//! **Verification order is fixed**: shape → MAC → payload. Nothing downstream of the MAC is
//! evaluated before it passes, so a caller that fails it learns nothing about which field to
//! change next. For the same reason [`MacTokenError`] carries two variants and not five.
//!
//! A value passed as `bound_to` is covered by the MAC without being carried in the token. That is
//! what makes a token non-bearer: holding it is not enough, the holder must also assert the exact
//! value it was sealed against.

use base64::Engine as _;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::sync::atomic::Ordering;

/// Nonce width in bytes, for a payload field that has to distinguish two otherwise identical
/// tokens.
pub const NONCE_BYTES: usize = 16;

/// Width of a [`token_id`], in lowercase hex characters.
const TOKEN_ID_HEX_CHARS: usize = 16;

/// Separator between the MAC input's fields. ASCII unit separator: it cannot occur in base64url,
/// so no pair of distinct inputs can produce the same MAC input by moving a boundary between them.
const MAC_FIELD_SEPARATOR: u8 = 0x1f;

/// The base64 alphabet used for both token segments: URL-safe and unpadded, so a whole token is
/// safe in a header value or a request path and has no `=` for a transport to mangle.
pub const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::URL_SAFE_NO_PAD;

type HmacSha256 = Hmac<Sha256>;

/// The 32-byte HMAC key one authority mints and verifies with.
///
/// Never written to disk, never placed in an environment variable, and never copied out of this
/// type: a key that reaches durable storage outlives the process whose lifetime is the only
/// revocation mechanism there is. Dropping it makes every outstanding token unverifiable at once
/// — revoke-all with no revocation list.
pub struct MintKey([u8; 32]);

impl MintKey {
    /// 32 bytes from the operating system's CSPRNG.
    pub fn generate() -> Result<Self, String> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes)
            .map_err(|error| format!("failed to generate a mint key: {error}"))?;
        Ok(Self(bytes))
    }

    /// Builds a fresh MAC context. Private: the key bytes never leave this module.
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

/// Why a token's shape or MAC was not accepted.
///
/// Two variants and not more: a caller that cannot tell a tampered payload from a token minted by
/// a different key cannot use the endpoint as an oracle. Whatever an authority checks *after*
/// [`open`] returns is its own to distinguish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MacTokenError {
    /// Not `<tag>.<base64url>.<base64url>`, or a segment that is not base64url, or empty.
    Malformed,
    /// MAC verification failed, for any reason at all.
    NotValid,
}

/// The stable identifier a mint and a redemption are correlated by: the first
/// [`TOKEN_ID_HEX_CHARS`] lowercase hex characters of `sha256(token)`.
///
/// This — never the token — is what may appear in a trace, a log line or an error body.
pub fn token_id(token: &str) -> String {
    murmur_artifact::sha256_hex(token.as_bytes())[..TOKEN_ID_HEX_CHARS].to_string()
}

/// `bytes` random bytes as lowercase hex.
pub fn random_hex(bytes: usize) -> Result<String, String> {
    let mut buffer = vec![0u8; bytes];
    getrandom::fill(&mut buffer).map_err(|error| format!("failed to generate a nonce: {error}"))?;
    Ok(buffer
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>())
}

/// Mints one token over `payload_json`, for the family named by `tag` and `domain`.
///
/// Each value in `bound_to` is covered by the MAC and left out of the token, so redemption
/// requires asserting it.
pub fn seal(
    key: &MintKey,
    tag: &str,
    domain: &[u8],
    payload_json: &[u8],
    bound_to: &[&str],
) -> String {
    let payload_b64 = B64.encode(payload_json);
    let mut mac = key.mac();
    mac.update(&mac_input(domain, &payload_b64, bound_to));
    let signature = B64.encode(mac.finalize().into_bytes());
    format!("{tag}.{payload_b64}.{signature}")
}

/// Verifies one token of the family named by `tag` and `domain` and returns its payload bytes.
///
/// The MAC comparison is `verify_slice`, which is constant-time; comparing decoded bytes with `==`
/// would leak the signature one byte at a time.
pub fn open(
    key: &MintKey,
    tag: &str,
    domain: &[u8],
    token: &str,
    bound_to: &[&str],
) -> Result<Vec<u8>, MacTokenError> {
    let (payload_b64, payload, signature) = split_token(tag, token)?;
    let mut mac = key.mac();
    mac.update(&mac_input(domain, payload_b64, bound_to));
    mac.verify_slice(&signature)
        .map_err(|_| MacTokenError::NotValid)?;
    Ok(payload)
}

/// The payload bytes of a token whose MAC has *not* been checked, for the family named by `tag`.
///
/// The value is caller-controlled and must never decide anything. Two uses are legitimate:
/// rejecting a string that is not a token of this family before asking for anything else, and
/// naming a local artefact after a token this process could not have minted.
pub fn payload_segment(tag: &str, token: &str) -> Result<Vec<u8>, MacTokenError> {
    split_token(tag, token).map(|(_, payload, _)| payload)
}

/// The bytes a token's MAC is taken over: `<domain> ‖ 0x1f ‖ <payload base64url>`, then `0x1f` and
/// each bound value in turn.
fn mac_input(domain: &[u8], payload_b64: &str, bound_to: &[&str]) -> Vec<u8> {
    let mut input = Vec::with_capacity(domain.len() + payload_b64.len() + 1);
    input.extend_from_slice(domain);
    input.push(MAC_FIELD_SEPARATOR);
    input.extend_from_slice(payload_b64.as_bytes());
    for bound in bound_to {
        input.push(MAC_FIELD_SEPARATOR);
        input.extend_from_slice(bound.as_bytes());
    }
    input
}

/// The token's three segments, checked for shape alone. Nothing here looks at what the payload
/// *says*. The payload segment is returned both as base64 text, which is what the MAC covers, and
/// decoded.
fn split_token<'a>(
    expected_tag: &str,
    token: &'a str,
) -> Result<(&'a str, Vec<u8>, Vec<u8>), MacTokenError> {
    let mut segments = token.split('.');
    let tag = segments.next().ok_or(MacTokenError::Malformed)?;
    let payload_b64 = segments.next().ok_or(MacTokenError::Malformed)?;
    let signature_b64 = segments.next().ok_or(MacTokenError::Malformed)?;
    if segments.next().is_some() || tag != expected_tag {
        return Err(MacTokenError::Malformed);
    }
    let payload = B64
        .decode(payload_b64)
        .map_err(|_| MacTokenError::Malformed)?;
    let signature = B64
        .decode(signature_b64)
        .map_err(|_| MacTokenError::Malformed)?;
    if payload.is_empty() || signature.is_empty() {
        return Err(MacTokenError::Malformed);
    }
    Ok((payload_b64, payload, signature))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TAG: &str = "tst1";
    const DOMAIN: &[u8] = b"murmur-mac-token-test";

    /// `Drop` is `zeroize` and nothing else, so testing the helper tests the teardown guarantee
    /// without reading memory the value has already released.
    #[test]
    fn zeroizing_clears_every_key_byte() {
        let mut key = MintKey([0xAB; 32]);
        assert_ne!(key.0, [0u8; 32]);
        key.zeroize();
        assert_eq!(key.0, [0u8; 32]);
    }

    /// A generated key is not the all-zero array a zeroized one becomes, and two are distinct —
    /// enough to catch a `generate` that stopped reaching the OS CSPRNG.
    #[test]
    fn a_generated_key_is_random() {
        let one = MintKey::generate().unwrap();
        let two = MintKey::generate().unwrap();
        assert_ne!(one.0, [0u8; 32]);
        assert_ne!(one.0, two.0);
    }

    #[test]
    fn a_key_prints_no_material() {
        assert_eq!(format!("{:?}", MintKey([0xAB; 32])), "MintKey(<redacted>)");
    }

    #[test]
    fn a_sealed_token_opens_with_the_key_that_sealed_it() {
        let key = MintKey::generate().unwrap();
        let token = seal(&key, TAG, DOMAIN, b"{\"a\":1}", &[]);
        assert!(token.starts_with("tst1."));
        assert_eq!(open(&key, TAG, DOMAIN, &token, &[]).unwrap(), b"{\"a\":1}");
    }

    #[test]
    fn another_key_does_not_open_it() {
        let token = seal(&MintKey::generate().unwrap(), TAG, DOMAIN, b"{}", &[]);
        assert_eq!(
            open(&MintKey::generate().unwrap(), TAG, DOMAIN, &token, &[]),
            Err(MacTokenError::NotValid)
        );
    }

    /// The tag is compared before the MAC and the domain is mixed into it, so neither a token of
    /// another family nor the same payload under another domain verifies.
    #[test]
    fn a_token_of_another_family_does_not_open() {
        let key = MintKey::generate().unwrap();
        let token = seal(&key, TAG, DOMAIN, b"{}", &[]);
        assert_eq!(
            open(&key, "oth1", DOMAIN, &token, &[]),
            Err(MacTokenError::Malformed)
        );
        assert_eq!(
            open(&key, TAG, b"murmur-other-domain", &token, &[]),
            Err(MacTokenError::NotValid)
        );
    }

    /// A bound value is covered by the MAC and absent from the token, so asserting the wrong one
    /// fails and the right one is not recoverable from the token itself.
    #[test]
    fn a_bound_value_is_required_but_not_carried() {
        let key = MintKey::generate().unwrap();
        let token = seal(&key, TAG, DOMAIN, b"{}", &["reporter@localhost:1"]);
        assert!(!token.contains("reporter"));
        assert!(open(&key, TAG, DOMAIN, &token, &["reporter@localhost:1"]).is_ok());
        assert_eq!(
            open(&key, TAG, DOMAIN, &token, &["reporter@localhost:2"]),
            Err(MacTokenError::NotValid)
        );
        assert_eq!(
            open(&key, TAG, DOMAIN, &token, &[]),
            Err(MacTokenError::NotValid)
        );
    }

    #[test]
    fn a_token_that_is_not_three_base64_segments_is_malformed() {
        let key = MintKey::generate().unwrap();
        for token in [
            "",
            "tst1",
            "tst1.abc",
            "tst1.abc.def.ghi",
            "tst1..def",
            "tst1.abc.",
            "tst1.not base64!.def",
        ] {
            assert_eq!(
                open(&key, TAG, DOMAIN, token, &[]),
                Err(MacTokenError::Malformed),
                "{token}"
            );
        }
    }

    #[test]
    fn a_payload_segment_reads_without_the_key() {
        let token = seal(
            &MintKey::generate().unwrap(),
            TAG,
            DOMAIN,
            b"{\"a\":1}",
            &[],
        );
        assert_eq!(payload_segment(TAG, &token).unwrap(), b"{\"a\":1}");
        assert_eq!(
            payload_segment("oth1", &token),
            Err(MacTokenError::Malformed)
        );
    }

    #[test]
    fn a_token_id_is_stable_and_distinguishing() {
        let id = token_id("tst1.abc.def");
        assert_eq!(id.len(), TOKEN_ID_HEX_CHARS);
        assert_eq!(id, token_id("tst1.abc.def"));
        assert_ne!(id, token_id("tst1.abc.deg"));
    }

    #[test]
    fn a_nonce_is_hex_of_the_requested_width() {
        let nonce = random_hex(NONCE_BYTES).unwrap();
        assert_eq!(nonce.len(), NONCE_BYTES * 2);
        assert!(nonce.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(nonce, random_hex(NONCE_BYTES).unwrap());
    }
}
