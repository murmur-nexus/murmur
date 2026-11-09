//! Checks who published an artifact: verifies the Sigstore bundle (a GitHub artifact attestation)
//! published beside a release asset as `<asset file>.sigstore.json`, offline, against a trusted
//! root compiled into `mur`.
//!
//! [`verify_bundle`] answers one of three verdicts. `Verified` names the workflow that signed the
//! payload. `Refused` means the bundle was read and a signature, the certificate chain, the log
//! entry, the identity or the subject does not hold for this payload. `Unverifiable` means the
//! bundle is in a form this module does not read, which the caller treats like no bundle at all.
//!
//! Supported: bundle media types v0.3 (`certificate`) and v0.2 (`x509CertificateChain`), a DSSE
//! envelope with one signature over an in-toto v1 statement, an ECDSA P-256 leaf key, and a Rekor
//! `dsse` 0.0.1 entry with an inclusion promise (its integrated time is the signing time) and, when
//! present, an RFC 6962 inclusion proof with a signed checkpoint. Signed timestamps, other Rekor
//! kinds and message-signature bundles are `Unverifiable`. Certificate transparency SCTs are not
//! checked.

mod der;
mod trusted_root;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use chrono::{DateTime, Utc};
use ring::{
    digest::{digest, SHA256},
    signature::{UnparsedPublicKey, ECDSA_P256_SHA256_ASN1},
};
use rustls_pki_types::{CertificateDer, UnixTime};
use serde::Deserialize;
use sha2::Digest as _;

pub(crate) use trusted_root::{trusted_root, trusted_root_override};
pub use trusted_root::{RootError, TrustedRoot, TRUSTED_ROOT_ENV};

use crate::error::{CliError, E_REG_016};

/// What a release asset's attestation adds to the asset's file name.
pub const BUNDLE_SUFFIX: &str = murmur_artifact::ATTESTATION_FILE_SUFFIX;

const MEDIA_TYPE_V03: &str = "application/vnd.dev.sigstore.bundle.v0.3+json";
const MEDIA_TYPE_V03_ALT: &str = "application/vnd.dev.sigstore.bundle+json;version=0.3";
const MEDIA_TYPE_V02: &str = "application/vnd.dev.sigstore.bundle+json;version=0.2";
const IN_TOTO_PAYLOAD_TYPE: &str = "application/vnd.in-toto+json";
const IN_TOTO_STATEMENT_V1: &str = "https://in-toto.io/Statement/v1";
const GITHUB_ACTIONS_ISSUER: &str = "https://token.actions.githubusercontent.com";
const P256_KEY_DETAILS: &str = "PKIX_ECDSA_P256_SHA_256";
/// `1.3.6.1.5.5.7.3.3`, the codeSigning extended key usage, as its encoded OID value.
const EKU_CODE_SIGNING: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x03];

/// Fulcio extension arcs under `1.3.6.1.4.1.57264.1`.
const EXT_ISSUER: u8 = 8;
const EXT_BUILD_SIGNER_URI: u8 = 9;
const EXT_SOURCE_REPOSITORY_URI: u8 = 12;
const EXT_SOURCE_REPOSITORY_OWNER_ID: u8 = 17;

/// What the bundle has to bind the payload to.
#[derive(Debug, Clone, Copy)]
pub struct Expected<'a> {
    /// Lowercase hex sha256 of the payload; some statement subject must carry it.
    pub sha256: &'a str,
    /// GitHub's numeric id of the repository owner, compared with the certificate's source
    /// repository owner id. `None` skips the comparison.
    pub owner_id: Option<u64>,
    /// `<owner>/<repo>` the payload was served from, compared ASCII-case-insensitively with the
    /// certificate's source repository. `None` skips the comparison: a stored payload records no
    /// repository.
    pub repo: Option<&'a str>,
}

/// The answer [`verify_bundle`] gives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Every check held. `signer` is the build signer URI with its `@<ref>` removed.
    Verified {
        signer: String,
        signed_at: DateTime<Utc>,
    },
    /// The bundle is in a form this `mur` does not read; the reason names what.
    Unverifiable(String),
    /// The bundle was read and does not hold for this payload; the reason names the failed check.
    Refused(String),
}

/// What an install learned about a payload's attestation, short of a refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttestationState {
    /// No bundle was published or kept beside the payload.
    Absent,
    /// A bundle is there, in a form this `mur` does not read.
    Unverifiable(String),
    /// The bundle verified; `signer` is the workflow that published the payload.
    Verified { signer: String },
}

impl AttestationState {
    /// The signer to record on the lock entry: `Some` only for a verified bundle.
    pub fn signer(&self) -> Option<&str> {
        match self {
            Self::Verified { signer } => Some(signer),
            Self::Absent | Self::Unverifiable(_) => None,
        }
    }
}

/// Verifies `bundle`, if any, and folds the verdict into an [`AttestationState`], or `Err` with
/// the refusal reason.
pub fn assess(
    bundle: Option<&[u8]>,
    expected: &Expected<'_>,
    root: &TrustedRoot,
) -> Result<AttestationState, String> {
    let Some(bundle) = bundle else {
        return Ok(AttestationState::Absent);
    };
    match verify_bundle(bundle, expected, root) {
        Verdict::Verified { signer, .. } => Ok(AttestationState::Verified { signer }),
        Verdict::Unverifiable(reason) => Ok(AttestationState::Unverifiable(reason)),
        Verdict::Refused(reason) => Err(reason),
    }
}

/// `E-REG-016`: `target` (`<scope>/<name>@<version>`) from `origin` (a source label or a store)
/// does not match its attestation.
pub(crate) fn refusal(target: &str, origin: &str, reason: &str) -> CliError {
    CliError::with_hint(
        E_REG_016,
        format!("{target} from {origin} does not match its attestation: {reason}"),
        "the payload or its attestation changed after it was published; nothing was stored \
         \u{2014} tell the artifact's publisher",
    )
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Bundle {
    media_type: Option<String>,
    verification_material: Option<VerificationMaterial>,
    dsse_envelope: Option<Envelope>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct VerificationMaterial {
    certificate: Option<RawBytes>,
    x509_certificate_chain: Option<CertificateChain>,
    #[serde(default)]
    tlog_entries: Vec<TlogEntry>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawBytes {
    raw_bytes: String,
}

#[derive(Deserialize)]
struct CertificateChain {
    #[serde(default)]
    certificates: Vec<RawBytes>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TlogEntry {
    log_index: Option<String>,
    log_id: Option<LogId>,
    kind_version: Option<KindVersion>,
    integrated_time: Option<String>,
    inclusion_promise: Option<InclusionPromise>,
    inclusion_proof: Option<InclusionProof>,
    canonicalized_body: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LogId {
    key_id: String,
}

#[derive(Deserialize)]
struct KindVersion {
    kind: String,
    version: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InclusionPromise {
    signed_entry_timestamp: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InclusionProof {
    log_index: String,
    root_hash: String,
    tree_size: String,
    #[serde(default)]
    hashes: Vec<String>,
    checkpoint: Option<Checkpoint>,
}

#[derive(Deserialize)]
struct Checkpoint {
    envelope: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Envelope {
    payload: String,
    payload_type: String,
    #[serde(default)]
    signatures: Vec<EnvelopeSignature>,
}

#[derive(Deserialize)]
struct EnvelopeSignature {
    sig: String,
}

/// The `canonicalizedBody` of a Rekor `dsse` 0.0.1 entry.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DsseBody {
    api_version: String,
    kind: String,
    spec: DsseSpec,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DsseSpec {
    payload_hash: Option<HashValue>,
    #[serde(default)]
    signatures: Vec<DsseBodySignature>,
}

#[derive(Deserialize)]
struct HashValue {
    algorithm: String,
    value: String,
}

#[derive(Deserialize)]
struct DsseBodySignature {
    signature: String,
    verifier: String,
}

#[derive(Deserialize)]
struct Statement {
    #[serde(rename = "_type")]
    statement_type: String,
    #[serde(default)]
    subject: Vec<Subject>,
}

#[derive(Deserialize)]
struct Subject {
    #[serde(default)]
    digest: std::collections::BTreeMap<String, String>,
}

/// A bundle's parts once its form is understood, before anything is verified.
struct Parsed<'b> {
    leaf: Vec<u8>,
    payload: Vec<u8>,
    payload_type: &'b str,
    signature: Vec<u8>,
    entry: &'b TlogEntry,
    log_key_id: Vec<u8>,
    integrated_time: i64,
    log_index: i64,
    body_b64: &'b str,
    body: Vec<u8>,
    set: Vec<u8>,
}

/// Verifies `bundle` against `expected` with the trust material in `root`, in this order: form,
/// envelope signature, transparency log, certificate chain, identity, subject. Never touches the
/// network.
pub fn verify_bundle(bundle: &[u8], expected: &Expected<'_>, root: &TrustedRoot) -> Verdict {
    let parsed_bundle: Bundle = match serde_json::from_slice(bundle) {
        Ok(parsed) => parsed,
        Err(error) => return Verdict::Unverifiable(format!("not a JSON Sigstore bundle: {error}")),
    };
    let parsed = match read_form(&parsed_bundle) {
        Ok(parsed) => parsed,
        Err(reason) => return Verdict::Unverifiable(reason),
    };
    match verify_parsed(&parsed, expected, root) {
        Ok(verdict) => verdict,
        Err(reason) => Verdict::Refused(reason),
    }
}

fn b64(value: &str, what: &str) -> Result<Vec<u8>, String> {
    STANDARD
        .decode(value)
        .map_err(|error| format!("its {what} is not base64: {error}"))
}

/// Step 1: every part of the bundle this module needs, in a form it reads.
fn read_form(bundle: &Bundle) -> Result<Parsed<'_>, String> {
    let media_type = bundle.media_type.as_deref().unwrap_or("");
    let material = bundle
        .verification_material
        .as_ref()
        .ok_or("the bundle has no verificationMaterial")?;
    let leaf = match media_type {
        MEDIA_TYPE_V03 | MEDIA_TYPE_V03_ALT => material
            .certificate
            .as_ref()
            .ok_or("a v0.3 bundle without a certificate (public-key bundles are not read)")?,
        MEDIA_TYPE_V02 => material
            .x509_certificate_chain
            .as_ref()
            .and_then(|chain| chain.certificates.first())
            .ok_or("a v0.2 bundle without an x509CertificateChain")?,
        other => {
            return Err(format!(
                "media type '{other}' is not a bundle form this mur reads"
            ))
        }
    };
    let leaf = b64(&leaf.raw_bytes, "certificate")?;
    let certificate =
        der::parse_certificate(&leaf).map_err(|error| format!("its certificate: {error}"))?;
    if certificate.key_algorithm != der::OID_EC_PUBLIC_KEY
        || certificate.key_parameter != Some(der::OID_P256)
    {
        return Err("its signing certificate's key is not ECDSA P-256".to_string());
    }

    let envelope = bundle
        .dsse_envelope
        .as_ref()
        .ok_or("the bundle carries no DSSE envelope (message-signature bundles are not read)")?;
    let [signature] = envelope.signatures.as_slice() else {
        return Err(format!(
            "its DSSE envelope carries {} signatures, not one",
            envelope.signatures.len()
        ));
    };
    let payload = b64(&envelope.payload, "DSSE payload")?;
    let signature = b64(&signature.sig, "DSSE signature")?;

    let entry = material
        .tlog_entries
        .first()
        .ok_or("the bundle has no transparency log entry (signed timestamps alone are not read)")?;
    let kind = entry
        .kind_version
        .as_ref()
        .ok_or("its log entry names no kind")?;
    if (kind.kind.as_str(), kind.version.as_str()) != ("dsse", "0.0.1") {
        return Err(format!(
            "its Rekor entry kind {} {} is not read",
            kind.kind, kind.version
        ));
    }
    let promise = entry.inclusion_promise.as_ref().ok_or(
        "its log entry has no inclusion promise to fix the signing time (signed timestamps are \
         not read)",
    )?;
    let number = |value: Option<&String>, what: &str| {
        value
            .and_then(|value| value.parse::<i64>().ok())
            .ok_or_else(|| format!("its log entry's {what} is not a number"))
    };
    let body_b64 = entry
        .canonicalized_body
        .as_deref()
        .ok_or("its log entry has no canonicalizedBody")?;
    Ok(Parsed {
        leaf,
        payload,
        payload_type: &envelope.payload_type,
        signature,
        entry,
        log_key_id: b64(
            &entry
                .log_id
                .as_ref()
                .ok_or("its log entry has no logId")?
                .key_id,
            "log key id",
        )?,
        integrated_time: number(entry.integrated_time.as_ref(), "integratedTime")?,
        log_index: number(entry.log_index.as_ref(), "logIndex")?,
        body_b64,
        body: b64(body_b64, "canonicalizedBody")?,
        set: b64(&promise.signed_entry_timestamp, "signedEntryTimestamp")?,
    })
}

/// Steps 2 to 7. `Err` is a refusal; an `Ok` verdict may still be `Unverifiable` where a later
/// step meets a form this module does not read.
fn verify_parsed(
    parsed: &Parsed<'_>,
    expected: &Expected<'_>,
    root: &TrustedRoot,
) -> Result<Verdict, String> {
    let certificate = der::parse_certificate(&parsed.leaf)
        .map_err(|error| format!("its certificate: {error}"))?;

    // 2. Envelope.
    if parsed.payload_type != IN_TOTO_PAYLOAD_TYPE {
        return Err(format!(
            "its DSSE payload type is '{}', not {IN_TOTO_PAYLOAD_TYPE}",
            parsed.payload_type
        ));
    }
    UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, certificate.public_key)
        .verify(
            &pae(parsed.payload_type, &parsed.payload),
            &parsed.signature,
        )
        .map_err(|_| "its DSSE signature does not verify with the signing certificate's key")?;

    // 3. Transparency log.
    let signed_at = DateTime::<Utc>::from_timestamp(parsed.integrated_time, 0)
        .ok_or("its log entry's integratedTime is out of range")?;
    let Some(log) = root
        .tlogs
        .iter()
        .find(|log| log.key_id == parsed.log_key_id && log.valid_for.covers(signed_at))
    else {
        return Err(format!(
            "its log entry names a log key the trusted root does not hold for {signed_at}"
        ));
    };
    if log.key_details != P256_KEY_DETAILS {
        return Ok(Verdict::Unverifiable(format!(
            "its log key is {}, which this mur does not read",
            log.key_details
        )));
    }
    let log_key =
        der::parse_spki(&log.spki).map_err(|error| format!("the trusted log key: {error}"))?;
    let log_key = UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, log_key);
    let set_payload = format!(
        r#"{{"body":{},"integratedTime":{},"logID":{},"logIndex":{}}}"#,
        serde_json::Value::from(parsed.body_b64),
        parsed.integrated_time,
        serde_json::Value::from(hex(&parsed.log_key_id)),
        parsed.log_index,
    );
    log_key
        .verify(set_payload.as_bytes(), &parsed.set)
        .map_err(|_| "its signed entry timestamp does not verify with the log's key")?;
    if let Some(proof) = &parsed.entry.inclusion_proof {
        verify_inclusion(proof, &parsed.body, &log_key, &parsed.log_key_id)?;
    }
    verify_body_binding(parsed)?;

    // 4. Certificate.
    let leaf = CertificateDer::from(parsed.leaf.as_slice());
    let leaf = webpki::EndEntityCert::try_from(&leaf)
        .map_err(|error| format!("its signing certificate does not parse: {error}"))?;
    let time = UnixTime::since_unix_epoch(std::time::Duration::from_secs(
        u64::try_from(parsed.integrated_time).map_err(|_| "its integratedTime is negative")?,
    ));
    let algorithms = [
        webpki::ring::ECDSA_P256_SHA256,
        webpki::ring::ECDSA_P256_SHA384,
        webpki::ring::ECDSA_P384_SHA256,
        webpki::ring::ECDSA_P384_SHA384,
    ];
    let mut chain_error = None;
    let chained = root
        .authorities
        .iter()
        .filter(|authority| authority.valid_for.covers(signed_at))
        .any(|authority| {
            leaf.verify_for_usage(
                &algorithms,
                std::slice::from_ref(&authority.anchor),
                &authority.intermediates,
                time,
                webpki::KeyUsage::required(EKU_CODE_SIGNING),
                None,
                None,
            )
            .map_err(|error| chain_error = Some(error))
            .is_ok()
        });
    if !chained {
        let detail = chain_error.map_or_else(
            || "no authority is valid then".to_string(),
            |error| error.to_string(),
        );
        return Err(format!(
            "its signing certificate does not chain to a certificate authority in the trusted \
             root at {signed_at} ({detail})"
        ));
    }

    // 5. Identity.
    let Some(issuer) = certificate.fulcio_extension(EXT_ISSUER) else {
        return Ok(Verdict::Unverifiable(
            "its certificate names no OIDC issuer (extension 1.3.6.1.4.1.57264.1.8)".to_string(),
        ));
    };
    let Some(owner_id) = certificate.fulcio_extension(EXT_SOURCE_REPOSITORY_OWNER_ID) else {
        return Ok(Verdict::Unverifiable(
            "its certificate names no repository owner id (extension 1.3.6.1.4.1.57264.1.17)"
                .to_string(),
        ));
    };
    let Some(repository) = certificate.fulcio_extension(EXT_SOURCE_REPOSITORY_URI) else {
        return Ok(Verdict::Unverifiable(
            "its certificate names no source repository (extension 1.3.6.1.4.1.57264.1.12)"
                .to_string(),
        ));
    };
    if issuer != GITHUB_ACTIONS_ISSUER {
        return Err(format!(
            "it was issued by {issuer}, not by GitHub Actions ({GITHUB_ACTIONS_ISSUER})"
        ));
    }
    let owner_matches = expected
        .owner_id
        .is_none_or(|expected| owner_id == expected.to_string());
    let repo_matches = expected.repo.is_none_or(|expected| {
        repository.eq_ignore_ascii_case(&format!("https://github.com/{expected}"))
    });
    if !owner_matches || !repo_matches {
        let wanted = match (expected.repo, expected.owner_id) {
            (Some(repo), Some(id)) => format!("github.com/{repo} with owner id {id}"),
            (Some(repo), None) => format!("github.com/{repo}"),
            (None, Some(id)) => format!("owner id {id}"),
            (None, None) => String::new(),
        };
        return Err(format!(
            "its certificate was issued for {repository} with owner id {owner_id}, not \
             {wanted}"
        ));
    }

    // 6. Subject.
    let statement: Statement = match serde_json::from_slice(&parsed.payload) {
        Ok(statement) => statement,
        Err(error) => {
            return Ok(Verdict::Unverifiable(format!(
                "its payload is not an in-toto statement: {error}"
            )))
        }
    };
    if statement.statement_type != IN_TOTO_STATEMENT_V1 {
        return Ok(Verdict::Unverifiable(format!(
            "its statement type '{}' is not read",
            statement.statement_type
        )));
    }
    let covered = statement.subject.iter().any(|subject| {
        subject
            .digest
            .get("sha256")
            .is_some_and(|digest| digest.eq_ignore_ascii_case(expected.sha256))
    });
    if !covered {
        return Err(format!(
            "the attestation does not cover the payload's sha256 {}",
            expected.sha256
        ));
    }

    // 7. Signer.
    let signer_uri = certificate
        .fulcio_extension(EXT_BUILD_SIGNER_URI)
        .or_else(|| certificate.san_uris().into_iter().next())
        .ok_or("its certificate names no build signer")?;
    let signer = signer_uri
        .split_once('@')
        .map_or(signer_uri.as_str(), |(workflow, _)| workflow)
        .to_string();
    Ok(Verdict::Verified { signer, signed_at })
}

/// DSSE's pre-authentication encoding of `payload` under `payload_type`.
fn pae(payload_type: &str, payload: &[u8]) -> Vec<u8> {
    let mut encoded = format!(
        "DSSEv1 {} {payload_type} {} ",
        payload_type.len(),
        payload.len()
    )
    .into_bytes();
    encoded.extend_from_slice(payload);
    encoded
}

/// The decoded log entry body binds this bundle: it records the DSSE payload's hash, the
/// envelope's signature and the signing certificate.
fn verify_body_binding(parsed: &Parsed<'_>) -> Result<(), String> {
    let body: DsseBody = serde_json::from_slice(&parsed.body)
        .map_err(|error| format!("its log entry body is not a dsse entry: {error}"))?;
    if (body.kind.as_str(), body.api_version.as_str()) != ("dsse", "0.0.1") {
        return Err(format!(
            "its log entry body is {} {}, not the dsse 0.0.1 entry it is labelled",
            body.kind, body.api_version
        ));
    }
    let payload_hash = hex(sha2::Sha256::digest(&parsed.payload).as_slice());
    let recorded = body
        .spec
        .payload_hash
        .as_ref()
        .filter(|hash| hash.algorithm == "sha256")
        .map(|hash| hash.value.to_ascii_lowercase());
    if recorded.as_deref() != Some(payload_hash.as_str()) {
        return Err("its log entry records a different DSSE payload".to_string());
    }
    let bound = body.spec.signatures.iter().any(|recorded| {
        STANDARD.decode(&recorded.signature).ok().as_deref() == Some(parsed.signature.as_slice())
            && verifier_der(&recorded.verifier).as_deref() == Some(parsed.leaf.as_slice())
    });
    if !bound {
        return Err(
            "its log entry records a different signature or signing certificate".to_string(),
        );
    }
    Ok(())
}

/// The DER of a log entry's `verifier`: base64 of a PEM certificate.
fn verifier_der(verifier: &str) -> Option<Vec<u8>> {
    let pem = String::from_utf8(STANDARD.decode(verifier).ok()?).ok()?;
    let body = pem
        .split("-----BEGIN CERTIFICATE-----")
        .nth(1)?
        .split("-----END CERTIFICATE-----")
        .next()?;
    let joined: String = body.chars().filter(|c| !c.is_whitespace()).collect();
    STANDARD.decode(joined).ok()
}

/// The RFC 6962 inclusion proof leads from the entry's leaf hash to `rootHash`, and the checkpoint
/// is a note signed by the log's key that commits to the same tree size and root.
fn verify_inclusion(
    proof: &InclusionProof,
    body: &[u8],
    log_key: &UnparsedPublicKey<&[u8]>,
    key_id: &[u8],
) -> Result<(), String> {
    let index: u64 = proof
        .log_index
        .parse()
        .map_err(|_| "its inclusion proof's logIndex is not a number")?;
    let size: u64 = proof
        .tree_size
        .parse()
        .map_err(|_| "its inclusion proof's treeSize is not a number")?;
    let root_hash = STANDARD
        .decode(&proof.root_hash)
        .map_err(|_| "its inclusion proof's rootHash is not base64")?;
    let hashes = proof
        .hashes
        .iter()
        .map(|hash| STANDARD.decode(hash))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "its inclusion proof carries a hash that is not base64")?;
    let mut leaf_input = vec![0u8];
    leaf_input.extend_from_slice(body);
    let leaf = digest(&SHA256, &leaf_input).as_ref().to_vec();
    if root_from_inclusion_proof(index, size, leaf, &hashes).as_deref() != Some(&root_hash[..]) {
        return Err("its inclusion proof does not lead to the log root it names".to_string());
    }
    let checkpoint = proof
        .checkpoint
        .as_ref()
        .ok_or("its inclusion proof has no checkpoint")?;
    verify_checkpoint(&checkpoint.envelope, size, &root_hash, log_key, key_id)
}

/// RFC 6962 section 2.1.1: the root an inclusion proof for leaf `index` of a tree of `size`
/// leaves yields, or `None` when the proof has the wrong length for that position.
fn root_from_inclusion_proof(
    index: u64,
    size: u64,
    leaf: Vec<u8>,
    proof: &[Vec<u8>],
) -> Option<Vec<u8>> {
    if index >= size {
        return None;
    }
    let inner = (u64::BITS - (index ^ (size - 1)).leading_zeros()) as usize;
    let border = (index >> inner).count_ones() as usize;
    if proof.len() != inner + border {
        return None;
    }
    let node = |left: &[u8], right: &[u8]| {
        let mut input = vec![1u8];
        input.extend_from_slice(left);
        input.extend_from_slice(right);
        digest(&SHA256, &input).as_ref().to_vec()
    };
    let mut hash = leaf;
    for (level, sibling) in proof[..inner].iter().enumerate() {
        hash = if (index >> level) & 1 == 0 {
            node(&hash, sibling)
        } else {
            node(sibling, &hash)
        };
    }
    for sibling in &proof[inner..] {
        hash = node(sibling, &hash);
    }
    Some(hash)
}

/// A checkpoint is a signed note: `<origin>\n<size>\n<base64 root>\n[...]\n\n` then lines
/// `— <name> <base64(key hint ‖ signature)>`. One signature whose 4-byte key hint matches the
/// log's key id must verify over the note text, and the note must commit to `size` and `root`.
fn verify_checkpoint(
    envelope: &str,
    size: u64,
    root: &[u8],
    log_key: &UnparsedPublicKey<&[u8]>,
    key_id: &[u8],
) -> Result<(), String> {
    let (text, signatures) = envelope
        .split_once("\n\n")
        .ok_or("its checkpoint is not a signed note")?;
    let text = format!("{text}\n");
    let mut lines = text.lines();
    let _origin = lines.next();
    let note_size = lines.next().and_then(|line| line.parse::<u64>().ok());
    let note_root = lines.next().and_then(|line| STANDARD.decode(line).ok());
    if note_size != Some(size) || note_root.as_deref() != Some(root) {
        return Err("its checkpoint commits to another tree than its inclusion proof".to_string());
    }
    let hint = key_id
        .get(..4)
        .ok_or("the log key id is shorter than a key hint")?;
    let signed = signatures.lines().any(|line| {
        let Some(rest) = line.strip_prefix("\u{2014} ") else {
            return false;
        };
        let Some((_, blob)) = rest.rsplit_once(' ') else {
            return false;
        };
        let Ok(blob) = STANDARD.decode(blob) else {
            return false;
        };
        blob.len() > 4 && &blob[..4] == hint && log_key.verify(text.as_bytes(), &blob[4..]).is_ok()
    });
    if !signed {
        return Err("its checkpoint is not signed by the log's key".to_string());
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// The bundle and trusted-root minter the integration tests use, for unit tests across the crate.
#[cfg(test)]
#[path = "../../tests/common/attestation.rs"]
pub(crate) mod minting;

#[cfg(test)]
mod tests;
