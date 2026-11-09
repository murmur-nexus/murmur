//! The Sigstore trusted root attestations are checked against: the transparency-log keys and the
//! certificate authorities, each with the time range it is valid for.
//!
//! The built-in root is Sigstore's public-good `trusted_root.json`, compiled in. Setting
//! [`TRUSTED_ROOT_ENV`] replaces it completely; a value that cannot be read or parsed refuses the
//! command rather than falling back.

use std::{path::PathBuf, sync::OnceLock};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use chrono::{DateTime, Utc};
use rustls_pki_types::{CertificateDer, TrustAnchor};
use serde::Deserialize;

use super::der;
use crate::error::{CliError, E_IO_003};

/// Replaces the built-in trusted root with the `trusted_root.json` at this path.
pub const TRUSTED_ROOT_ENV: &str = "MUR_SIGSTORE_TRUSTED_ROOT";

/// Sigstore's public-good trusted root, from `sigstore/root-signing` `targets/trusted_root.json`.
/// Refreshing it is a manual step: replace the file and rebuild.
const BUILT_IN: &str = include_str!("trusted_root.json");

/// The parsed trust material: what the attestation check believes about logs and authorities.
#[derive(Debug)]
pub struct TrustedRoot {
    pub(crate) tlogs: Vec<TlogKey>,
    pub(crate) authorities: Vec<CertificateAuthority>,
}

/// One transparency log's signing key.
#[derive(Debug)]
pub(crate) struct TlogKey {
    /// `logId.keyId`: the SHA-256 of the key's DER SubjectPublicKeyInfo.
    pub(crate) key_id: Vec<u8>,
    /// The key's DER SubjectPublicKeyInfo.
    pub(crate) spki: Vec<u8>,
    /// `publicKey.keyDetails`, e.g. `PKIX_ECDSA_P256_SHA_256`.
    pub(crate) key_details: String,
    pub(crate) valid_for: Validity,
}

/// One certificate authority: the trust anchor its chain ends in and the certificates between.
#[derive(Debug)]
pub(crate) struct CertificateAuthority {
    pub(crate) anchor: TrustAnchor<'static>,
    pub(crate) intermediates: Vec<CertificateDer<'static>>,
    pub(crate) valid_for: Validity,
}

/// A `validFor` range: `end` absent means still valid.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Validity {
    pub(crate) start: DateTime<Utc>,
    pub(crate) end: Option<DateTime<Utc>>,
}

impl Validity {
    pub(crate) fn covers(&self, at: DateTime<Utc>) -> bool {
        self.start <= at && self.end.is_none_or(|end| at <= end)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRoot {
    #[serde(default)]
    tlogs: Vec<RawTlog>,
    #[serde(default)]
    certificate_authorities: Vec<RawAuthority>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawTlog {
    public_key: RawPublicKey,
    log_id: RawLogId,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPublicKey {
    raw_bytes: String,
    key_details: String,
    valid_for: RawValidity,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawLogId {
    key_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawAuthority {
    cert_chain: RawCertChain,
    valid_for: RawValidity,
}

#[derive(Deserialize)]
struct RawCertChain {
    certificates: Vec<RawCertificate>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCertificate {
    raw_bytes: String,
}

#[derive(Deserialize)]
struct RawValidity {
    start: String,
    #[serde(default)]
    end: Option<String>,
}

impl TrustedRoot {
    /// Parses a Sigstore `trusted_root.json`, reading only `tlogs[]` and
    /// `certificateAuthorities[]`.
    pub fn from_json(json: &str) -> Result<Self, String> {
        let raw: RawRoot = serde_json::from_str(json).map_err(|error| error.to_string())?;
        let mut tlogs = Vec::new();
        for (index, tlog) in raw.tlogs.into_iter().enumerate() {
            let spki = decode(&tlog.public_key.raw_bytes, || format!("tlogs[{index}] key"))?;
            der::parse_spki(&spki).map_err(|error| format!("tlogs[{index}] key: {error}"))?;
            tlogs.push(TlogKey {
                key_id: decode(&tlog.log_id.key_id, || format!("tlogs[{index}] keyId"))?,
                spki,
                key_details: tlog.public_key.key_details,
                valid_for: validity(&tlog.public_key.valid_for, || format!("tlogs[{index}]"))?,
            });
        }
        let mut authorities = Vec::new();
        for (index, authority) in raw.certificate_authorities.into_iter().enumerate() {
            let label = || format!("certificateAuthorities[{index}]");
            let mut chain = Vec::new();
            for certificate in &authority.cert_chain.certificates {
                chain.push(CertificateDer::from(decode(&certificate.raw_bytes, label)?));
            }
            let Some(root) = chain.pop() else {
                return Err(format!("{} has an empty certChain", label()));
            };
            let anchor = webpki::anchor_from_trusted_cert(&root)
                .map_err(|error| format!("{}: {error}", label()))?
                .to_owned();
            authorities.push(CertificateAuthority {
                anchor,
                intermediates: chain,
                valid_for: validity(&authority.valid_for, label)?,
            });
        }
        if tlogs.is_empty() || authorities.is_empty() {
            return Err("it names no transparency log or no certificate authority".to_string());
        }
        Ok(Self { tlogs, authorities })
    }
}

fn decode(value: &str, label: impl Fn() -> String) -> Result<Vec<u8>, String> {
    STANDARD
        .decode(value)
        .map_err(|error| format!("{} is not base64: {error}", label()))
}

fn validity(raw: &RawValidity, label: impl Fn() -> String) -> Result<Validity, String> {
    let time = |value: &str| {
        DateTime::parse_from_rfc3339(value)
            .map(|time| time.with_timezone(&Utc))
            .map_err(|error| format!("{} validFor '{value}': {error}", label()))
    };
    Ok(Validity {
        start: time(&raw.start)?,
        end: raw.end.as_deref().map(time).transpose()?,
    })
}

/// The path [`TRUSTED_ROOT_ENV`] names, when it is set and not empty.
pub(crate) fn trusted_root_override() -> Option<PathBuf> {
    std::env::var_os(TRUSTED_ROOT_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// Why the trusted root could not be loaded: the message names [`TRUSTED_ROOT_ENV`] when the
/// override is at fault. Becomes `E-IO-003`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootError(pub String);

impl From<RootError> for CliError {
    fn from(error: RootError) -> Self {
        CliError::with_hint(E_IO_003, error.0, override_hint())
    }
}

/// The trusted root this process checks attestations against, parsed once: the file
/// [`TRUSTED_ROOT_ENV`] names, or the built-in root. An override that cannot be read or parsed is
/// an error every time it is asked for; it never falls back to the built-in root.
pub(crate) fn trusted_root() -> Result<&'static TrustedRoot, RootError> {
    static ROOT: OnceLock<Result<TrustedRoot, String>> = OnceLock::new();
    ROOT.get_or_init(load)
        .as_ref()
        .map_err(|message| RootError(message.clone()))
}

fn override_hint() -> String {
    format!(
        "point {TRUSTED_ROOT_ENV} at a readable Sigstore trusted_root.json, or unset it to use the \
         root built into mur"
    )
}

fn load() -> Result<TrustedRoot, String> {
    match trusted_root_override() {
        Some(path) => load_path(&path),
        None => TrustedRoot::from_json(BUILT_IN)
            .map_err(|error| format!("the trusted root built into mur does not parse: {error}")),
    }
}

fn load_path(path: &std::path::Path) -> Result<TrustedRoot, String> {
    let json = std::fs::read_to_string(path).map_err(|error| {
        format!(
            "{TRUSTED_ROOT_ENV}={} cannot be read: {error}",
            path.display()
        )
    })?;
    TrustedRoot::from_json(&json).map_err(|error| {
        format!(
            "{TRUSTED_ROOT_ENV}={} is not a Sigstore trusted root: {error}",
            path.display()
        )
    })
}

/// The built-in root, parsed fresh. Unit tests use it without touching the process-wide one.
#[cfg(test)]
pub(crate) fn built_in() -> TrustedRoot {
    TrustedRoot::from_json(BUILT_IN).expect("the built-in trusted root parses")
}

/// What [`trusted_root`] answers for an override of `path`, without the process-wide cache.
#[cfg(test)]
pub(crate) fn load_override(path: &std::path::Path) -> Result<TrustedRoot, CliError> {
    load_path(path).map_err(|message| RootError(message).into())
}
