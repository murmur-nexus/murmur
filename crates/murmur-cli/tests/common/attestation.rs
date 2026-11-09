//! Mints Sigstore bundles and the trusted root that verifies them, for tests.
//!
//! A [`Minter`] holds a certificate authority (a root and an intermediate) and a transparency-log
//! key, all generated per test. [`Minter::trusted_root_json`] is the `trusted_root.json` to point
//! `MUR_SIGSTORE_TRUSTED_ROOT` at; [`Minter::bundle`] signs a bundle the way GitHub's
//! `actions/attest-build-provenance` does: a DSSE envelope over an in-toto statement, a Fulcio-style
//! leaf certificate with the GitHub Actions identity extensions, and a Rekor `dsse` 0.0.1 entry
//! with an inclusion promise, an inclusion proof and a signed checkpoint. [`BundleSpec`] fields
//! change one part at a time, so each verification step can be made to fail on its own.
//!
//! Certificates are written with the small DER writer below and signed with `ring`.
//! Also compiled into `mur`'s unit tests through `#[path]`.

#![allow(dead_code)]

use base64::{engine::general_purpose::STANDARD, Engine as _};
use ring::{
    digest::{digest, SHA256},
    rand::SystemRandom,
    signature::{
        EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_ASN1_SIGNING, ECDSA_P384_SHA384_ASN1_SIGNING,
    },
};

/// 2026-01-01T00:00:00Z: when every minted leaf certificate becomes valid.
pub const NOT_BEFORE: i64 = 1_767_225_600;
/// A minted leaf is valid for ten minutes, as a Fulcio leaf is.
pub const LEAF_LIFETIME: i64 = 600;

pub const MEDIA_TYPE_V03: &str = "application/vnd.dev.sigstore.bundle.v0.3+json";
pub const GITHUB_ISSUER: &str = "https://token.actions.githubusercontent.com";

/// The signer `mur` records for a bundle minted for `repo` with `workflow` (a file name under
/// `.github/workflows/`).
pub fn signer_for(repo: &str, workflow: &str) -> String {
    format!("https://github.com/{repo}/.github/workflows/{workflow}")
}

/// Lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(digest(&SHA256, bytes).as_ref())
}

/// What one minted bundle says. [`BundleSpec::new`] gives a bundle that verifies for `repo`,
/// `owner_id` and `sha256`; each other field breaks or changes one part of it.
#[derive(Debug, Clone)]
pub struct BundleSpec {
    pub media_type: String,
    pub payload_type: String,
    pub statement_type: String,
    /// The digest the statement's subject carries.
    pub subject_sha256: String,
    /// `<owner>/<repo>`; the certificate's source repository URI is `https://github.com/<repo>`.
    pub repo: String,
    /// Fulcio extension values; `None` leaves the extension out.
    pub issuer: Option<String>,
    pub owner_id: Option<String>,
    pub source_repository_uri: Option<String>,
    pub build_signer_uri: Option<String>,
    pub san_uri: String,
    /// Sign the leaf with an authority outside the trusted root.
    pub foreign_ca: bool,
    /// Give the leaf a P-384 key instead of P-256.
    pub leaf_p384: bool,
    /// Flip one byte of the DSSE signature after signing.
    pub flip_signature: bool,
    /// Record another payload hash in the log entry body.
    pub body_payload_hash: Option<String>,
    /// Record another signature in the log entry body.
    pub body_other_signature: bool,
    /// Flip one byte of the signed entry timestamp.
    pub flip_set: bool,
    /// Carry an inclusion proof and checkpoint.
    pub inclusion_proof: bool,
    /// Make the inclusion proof's root hash wrong (the checkpoint commits to the wrong root too).
    pub wrong_proof_root: bool,
    /// Sign the checkpoint with a key other than the log's.
    pub foreign_checkpoint_key: bool,
    /// Carry an inclusion promise.
    pub inclusion_promise: bool,
    /// Rekor entry kind and version.
    pub kind: (String, String),
    /// `integratedTime` as seconds after [`NOT_BEFORE`].
    pub integrated_after: i64,
    /// Sign the log entry with a log key outside the trusted root.
    pub foreign_log_key: bool,
}

impl BundleSpec {
    pub fn new(repo: &str, owner_id: u64, sha256: &str) -> Self {
        Self::signed_by(repo, owner_id, sha256, "release.yml")
    }

    /// A bundle signed by the workflow file `workflow` of `repo`.
    pub fn signed_by(repo: &str, owner_id: u64, sha256: &str, workflow: &str) -> Self {
        let signer = format!("{}@refs/tags/v1", signer_for(repo, workflow));
        Self {
            media_type: MEDIA_TYPE_V03.to_string(),
            payload_type: "application/vnd.in-toto+json".to_string(),
            statement_type: "https://in-toto.io/Statement/v1".to_string(),
            subject_sha256: sha256.to_string(),
            repo: repo.to_string(),
            issuer: Some(GITHUB_ISSUER.to_string()),
            owner_id: Some(owner_id.to_string()),
            source_repository_uri: Some(format!("https://github.com/{repo}")),
            build_signer_uri: Some(signer.clone()),
            san_uri: signer,
            foreign_ca: false,
            leaf_p384: false,
            flip_signature: false,
            body_payload_hash: None,
            body_other_signature: false,
            flip_set: false,
            inclusion_proof: true,
            wrong_proof_root: false,
            foreign_checkpoint_key: false,
            inclusion_promise: true,
            kind: ("dsse".to_string(), "0.0.1".to_string()),
            integrated_after: 60,
            foreign_log_key: false,
        }
    }
}

struct Authority {
    root_der: Vec<u8>,
    intermediate_der: Vec<u8>,
    intermediate_key: EcdsaKeyPair,
    intermediate_name: Vec<u8>,
}

/// A certificate authority and a transparency log, generated fresh.
pub struct Minter {
    rng: SystemRandom,
    authority: Authority,
    foreign: Authority,
    log_key: EcdsaKeyPair,
    foreign_log_key: EcdsaKeyPair,
}

impl Default for Minter {
    fn default() -> Self {
        Self::new()
    }
}

impl Minter {
    pub fn new() -> Self {
        let rng = SystemRandom::new();
        let authority = Authority::new(&rng, "test-sigstore");
        let foreign = Authority::new(&rng, "foreign-sigstore");
        let log_key = p256_key(&rng);
        let foreign_log_key = p256_key(&rng);
        Self {
            rng,
            authority,
            foreign,
            log_key,
            foreign_log_key,
        }
    }

    /// A Sigstore `trusted_root.json` holding this minter's log key and authority.
    pub fn trusted_root_json(&self) -> String {
        let spki = p256_spki(self.log_key.public_key().as_ref());
        serde_json::json!({
            "mediaType": "application/vnd.dev.sigstore.trustedroot+json;version=0.1",
            "tlogs": [{
                "baseUrl": "https://rekor.test",
                "hashAlgorithm": "SHA2_256",
                "publicKey": {
                    "rawBytes": STANDARD.encode(&spki),
                    "keyDetails": "PKIX_ECDSA_P256_SHA_256",
                    "validFor": {"start": "2020-01-01T00:00:00Z"}
                },
                "logId": {"keyId": STANDARD.encode(digest(&SHA256, &spki))}
            }],
            "certificateAuthorities": [{
                "subject": {"organization": "test", "commonName": "test-sigstore"},
                "uri": "https://fulcio.test",
                "certChain": {"certificates": [
                    {"rawBytes": STANDARD.encode(&self.authority.intermediate_der)},
                    {"rawBytes": STANDARD.encode(&self.authority.root_der)}
                ]},
                "validFor": {"start": "2020-01-01T00:00:00Z"}
            }],
            "ctlogs": [],
            "timestampAuthorities": []
        })
        .to_string()
    }

    /// Writes [`Self::trusted_root_json`] to `<dir>/trusted_root.json` and returns its path.
    pub fn write_trusted_root(&self, dir: &std::path::Path) -> std::path::PathBuf {
        let path = dir.join("trusted_root.json");
        std::fs::write(&path, self.trusted_root_json()).unwrap();
        path
    }

    /// The bundle `spec` describes, as the bytes of a `.sigstore.json` file.
    pub fn bundle(&self, spec: &BundleSpec) -> Vec<u8> {
        let rng = &self.rng;
        let authority = if spec.foreign_ca {
            &self.foreign
        } else {
            &self.authority
        };

        let (leaf_key, leaf_spki): (EcdsaKeyPair, Vec<u8>) = if spec.leaf_p384 {
            let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P384_SHA384_ASN1_SIGNING, rng).unwrap();
            let key =
                EcdsaKeyPair::from_pkcs8(&ECDSA_P384_SHA384_ASN1_SIGNING, pkcs8.as_ref(), rng)
                    .unwrap();
            let spki = spki(OID_P384, key.public_key().as_ref());
            (key, spki)
        } else {
            let key = p256_key(rng);
            let spki = p256_spki(key.public_key().as_ref());
            (key, spki)
        };

        let mut extensions = vec![
            extension(OID_KEY_USAGE, true, &tlv(0x03, &[0x07, 0x80])),
            extension(OID_EXT_KEY_USAGE, false, &seq(&[&oid(OID_CODE_SIGNING)])),
            extension(
                OID_SUBJECT_ALT_NAME,
                true,
                &seq(&[&tlv(0x86, spec.san_uri.as_bytes())]),
            ),
        ];
        for (arc, value) in [
            (8u8, &spec.issuer),
            (9, &spec.build_signer_uri),
            (12, &spec.source_repository_uri),
            (17, &spec.owner_id),
        ] {
            if let Some(value) = value {
                let mut fulcio = OID_FULCIO_ARC.to_vec();
                fulcio.push(arc);
                extensions.push(extension(&fulcio, false, &tlv(0x0c, value.as_bytes())));
            }
        }
        let leaf_der = certificate(
            rng,
            &authority.intermediate_key,
            &authority.intermediate_name,
            &seq(&[]),
            &leaf_spki,
            NOT_BEFORE,
            NOT_BEFORE + LEAF_LIFETIME,
            &extensions,
        );

        let statement = serde_json::json!({
            "_type": spec.statement_type,
            "subject": [
                {"name": "other.mur.zip", "digest": {"sha256": "00".repeat(32)}},
                {"name": "payload.mur.zip", "digest": {"sha256": spec.subject_sha256}}
            ],
            "predicateType": "https://slsa.dev/provenance/v1",
            "predicate": {"buildDefinition": {"buildType": "https://actions.github.io/buildtypes/workflow/v1"}}
        })
        .to_string()
        .into_bytes();
        let pae = pae(&spec.payload_type, &statement);
        let mut signature = leaf_key.sign(rng, &pae).unwrap().as_ref().to_vec();
        if spec.flip_signature {
            let last = signature.len() - 1;
            signature[last] ^= 0x01;
        }

        let body_signature = if spec.body_other_signature {
            leaf_key
                .sign(rng, b"something else")
                .unwrap()
                .as_ref()
                .to_vec()
        } else {
            signature.clone()
        };
        let pem = format!(
            "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
            STANDARD
                .encode(&leaf_der)
                .as_bytes()
                .chunks(64)
                .map(|line| std::str::from_utf8(line).unwrap())
                .collect::<Vec<_>>()
                .join("\n")
        );
        let body = serde_json::json!({
            "apiVersion": spec.kind.1,
            "kind": spec.kind.0,
            "spec": {
                "envelopeHash": {"algorithm": "sha256", "value": "00".repeat(32)},
                "payloadHash": {
                    "algorithm": "sha256",
                    "value": spec.body_payload_hash.clone().unwrap_or_else(|| sha256_hex(&statement))
                },
                "signatures": [{
                    "signature": STANDARD.encode(&body_signature),
                    "verifier": STANDARD.encode(pem.as_bytes())
                }]
            }
        })
        .to_string()
        .into_bytes();
        let body_b64 = STANDARD.encode(&body);

        let log_key = if spec.foreign_log_key {
            &self.foreign_log_key
        } else {
            &self.log_key
        };
        let log_spki = p256_spki(log_key.public_key().as_ref());
        let key_id = digest(&SHA256, &log_spki).as_ref().to_vec();
        let integrated_time = NOT_BEFORE + spec.integrated_after;
        let log_index = 4242u64;
        let set_payload = format!(
            r#"{{"body":"{body_b64}","integratedTime":{integrated_time},"logID":"{}","logIndex":{log_index}}}"#,
            hex(&key_id)
        );
        let mut set = log_key
            .sign(rng, set_payload.as_bytes())
            .unwrap()
            .as_ref()
            .to_vec();
        if spec.flip_set {
            let last = set.len() - 1;
            set[last] ^= 0x01;
        }

        let mut entry = serde_json::json!({
            "logIndex": log_index.to_string(),
            "logId": {"keyId": STANDARD.encode(&key_id)},
            "kindVersion": {"kind": spec.kind.0, "version": spec.kind.1},
            "integratedTime": integrated_time.to_string(),
            "canonicalizedBody": body_b64,
        });
        if spec.inclusion_promise {
            entry["inclusionPromise"] =
                serde_json::json!({"signedEntryTimestamp": STANDARD.encode(&set)});
        }
        if spec.inclusion_proof {
            entry["inclusionProof"] = self.inclusion_proof(spec, &body, log_key, &key_id);
        }

        serde_json::to_vec_pretty(&serde_json::json!({
            "mediaType": spec.media_type,
            "verificationMaterial": {
                "certificate": {"rawBytes": STANDARD.encode(&leaf_der)},
                "tlogEntries": [entry],
                "timestampVerificationData": {}
            },
            "dsseEnvelope": {
                "payload": STANDARD.encode(&statement),
                "payloadType": spec.payload_type,
                "signatures": [{"sig": STANDARD.encode(&signature)}]
            }
        }))
        .unwrap()
    }

    /// A proof for leaf 3 of a 7-leaf tree, its sibling hashes random, and a checkpoint for the
    /// root they yield.
    fn inclusion_proof(
        &self,
        spec: &BundleSpec,
        body: &[u8],
        log_key: &EcdsaKeyPair,
        key_id: &[u8],
    ) -> serde_json::Value {
        let (index, size) = (3u64, 7u64);
        let mut leaf_input = vec![0u8];
        leaf_input.extend_from_slice(body);
        let mut hash = digest(&SHA256, &leaf_input).as_ref().to_vec();
        let hashes: Vec<Vec<u8>> = (0..3u8)
            .map(|n| digest(&SHA256, &[n, 0xaa]).as_ref().to_vec())
            .collect();
        // Leaf 3 of 7: every level of the inner proof has the sibling on the left.
        for (level, sibling) in hashes.iter().enumerate() {
            let mut input = vec![1u8];
            if (index >> level) & 1 == 0 {
                input.extend_from_slice(&hash);
                input.extend_from_slice(sibling);
            } else {
                input.extend_from_slice(sibling);
                input.extend_from_slice(&hash);
            }
            hash = digest(&SHA256, &input).as_ref().to_vec();
        }
        if spec.wrong_proof_root {
            hash[0] ^= 0x01;
        }
        let root = STANDARD.encode(&hash);
        let note = format!("rekor.test - 1234\n{size}\n{root}\n");
        let signer = if spec.foreign_checkpoint_key {
            &self.foreign_log_key
        } else {
            log_key
        };
        let mut blob = key_id[..4].to_vec();
        blob.extend_from_slice(signer.sign(&self.rng, note.as_bytes()).unwrap().as_ref());
        let envelope = format!("{note}\n\u{2014} rekor.test {}\n", STANDARD.encode(&blob));
        serde_json::json!({
            "logIndex": index.to_string(),
            "rootHash": root,
            "treeSize": size.to_string(),
            "hashes": hashes.iter().map(|h| STANDARD.encode(h)).collect::<Vec<_>>(),
            "checkpoint": {"envelope": envelope}
        })
    }
}

impl Authority {
    fn new(rng: &SystemRandom, common_name: &str) -> Self {
        let root_key = p256_key(rng);
        let root_name = name(&format!("{common_name}-root"));
        let ca_extensions = [
            extension(OID_BASIC_CONSTRAINTS, true, &seq(&[&tlv(0x01, &[0xff])])),
            extension(OID_KEY_USAGE, true, &tlv(0x03, &[0x01, 0x06])),
            // Fulcio's intermediate carries codeSigning too, and webpki checks the EKU on every
            // certificate of the path.
            extension(OID_EXT_KEY_USAGE, false, &seq(&[&oid(OID_CODE_SIGNING)])),
        ];
        let root_der = certificate(
            rng,
            &root_key,
            &root_name,
            &root_name,
            &p256_spki(root_key.public_key().as_ref()),
            1_577_836_800, // 2020-01-01
            2_524_608_000, // 2050-01-01
            &ca_extensions,
        );
        let intermediate_key = p256_key(rng);
        let intermediate_name = name(&format!("{common_name}-intermediate"));
        let intermediate_der = certificate(
            rng,
            &root_key,
            &root_name,
            &intermediate_name,
            &p256_spki(intermediate_key.public_key().as_ref()),
            1_577_836_800,
            2_524_608_000 - 1,
            &ca_extensions,
        );
        Self {
            root_der,
            intermediate_der,
            intermediate_key,
            intermediate_name,
        }
    }
}

const OID_EC_PUBLIC_KEY: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
const OID_P256: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
const OID_P384: &[u8] = &[0x2b, 0x81, 0x04, 0x00, 0x22];
const OID_ECDSA_SHA256: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02];
const OID_COMMON_NAME: &[u8] = &[0x55, 0x04, 0x03];
const OID_BASIC_CONSTRAINTS: &[u8] = &[0x55, 0x1d, 0x13];
const OID_KEY_USAGE: &[u8] = &[0x55, 0x1d, 0x0f];
const OID_EXT_KEY_USAGE: &[u8] = &[0x55, 0x1d, 0x25];
const OID_SUBJECT_ALT_NAME: &[u8] = &[0x55, 0x1d, 0x11];
const OID_CODE_SIGNING: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x03];
const OID_FULCIO_ARC: &[u8] = &[0x2b, 0x06, 0x01, 0x04, 0x01, 0x83, 0xbf, 0x30, 0x01];

fn p256_key(rng: &SystemRandom) -> EcdsaKeyPair {
    let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, rng).unwrap();
    EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), rng).unwrap()
}

fn p256_spki(point: &[u8]) -> Vec<u8> {
    spki(OID_P256, point)
}

fn spki(curve: &[u8], point: &[u8]) -> Vec<u8> {
    let mut bits = vec![0u8];
    bits.extend_from_slice(point);
    seq(&[
        &seq(&[&oid(OID_EC_PUBLIC_KEY), &oid(curve)]),
        &tlv(0x03, &bits),
    ])
}

#[allow(clippy::too_many_arguments)]
fn certificate(
    rng: &SystemRandom,
    issuer_key: &EcdsaKeyPair,
    issuer: &[u8],
    subject: &[u8],
    spki: &[u8],
    not_before: i64,
    not_after: i64,
    extensions: &[Vec<u8>],
) -> Vec<u8> {
    let mut serial = [0u8; 9];
    ring::rand::SecureRandom::fill(rng, &mut serial).unwrap();
    serial[0] = 0x01;
    let algorithm = seq(&[&oid(OID_ECDSA_SHA256)]);
    let extension_refs: Vec<&[u8]> = extensions.iter().map(Vec::as_slice).collect();
    let tbs = seq(&[
        &tlv(0xa0, &tlv(0x02, &[0x02])),
        &tlv(0x02, &serial),
        &algorithm,
        issuer,
        &seq(&[&utc_time(not_before), &utc_time(not_after)]),
        subject,
        spki,
        &tlv(0xa3, &seq(&extension_refs)),
    ]);
    let signature = issuer_key.sign(rng, &tbs).unwrap();
    let mut bits = vec![0u8];
    bits.extend_from_slice(signature.as_ref());
    seq(&[&tbs, &algorithm, &tlv(0x03, &bits)])
}

fn name(common_name: &str) -> Vec<u8> {
    seq(&[&tlv(
        0x31,
        &seq(&[&oid(OID_COMMON_NAME), &tlv(0x0c, common_name.as_bytes())]),
    )])
}

fn extension(id: &[u8], critical: bool, value: &[u8]) -> Vec<u8> {
    let critical_flag = tlv(0x01, &[0xff]);
    let mut parts: Vec<&[u8]> = Vec::new();
    let id = oid(id);
    parts.push(&id);
    if critical {
        parts.push(&critical_flag);
    }
    let value = tlv(0x04, value);
    parts.push(&value);
    seq(&parts)
}

/// `YYMMDDHHMMSSZ`, for times in 1950–2049.
fn utc_time(unix: i64) -> Vec<u8> {
    let days = unix.div_euclid(86_400);
    let seconds = unix.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let text = format!(
        "{:02}{month:02}{day:02}{:02}{:02}{:02}Z",
        year % 100,
        seconds / 3600,
        seconds % 3600 / 60,
        seconds % 60
    );
    tlv(0x17, text.as_bytes())
}

/// Howard Hinnant's days-to-civil conversion.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

fn oid(value: &[u8]) -> Vec<u8> {
    tlv(0x06, value)
}

fn seq(parts: &[&[u8]]) -> Vec<u8> {
    tlv(0x30, &parts.concat())
}

fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    let length = content.len();
    if length < 0x80 {
        out.push(length as u8);
    } else {
        let bytes = length.to_be_bytes();
        let skip = bytes.iter().take_while(|&&b| b == 0).count();
        out.push(0x80 | (bytes.len() - skip) as u8);
        out.extend_from_slice(&bytes[skip..]);
    }
    out.extend_from_slice(content);
    out
}

fn pae(payload_type: &str, payload: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "DSSEv1 {} {payload_type} {} ",
        payload_type.len(),
        payload.len()
    )
    .into_bytes();
    out.extend_from_slice(payload);
    out
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
