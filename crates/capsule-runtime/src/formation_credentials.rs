//! Who in a formation may call whom: the credential a formation launcher issues for each roster
//! edge, the check every member's door makes, and the channel a member's runtime is handed its
//! credentials and its callees' addresses on.
//!
//! **There is one principal per formation, and it is the launcher.** After minting the
//! [`FormationId`], the launcher generates one Ed25519 key pair ([`FormationAuthority`]) and signs
//! one [`FormationToken`] for every roster edge `from → to`:
//!
//! ```text
//! mft1 "." base64url-nopad(payload JSON) "." base64url-nopad(Ed25519 signature)
//! payload: {"formation":"frm_…","from":"<member>","to":"<member>"}
//! signed:  FORMATION_TOKEN_DOMAIN ‖ 0x1f ‖ "mft1" ‖ 0x1f ‖ <payload base64url>
//! ```
//!
//! The signing key exists only in the launcher's memory: it is never written, never put in an
//! environment variable and never sent anywhere. Every member is handed the 32-byte public key
//! ([`FormationVerifyKey`]) and nothing that signs, so no member can mint a token.
//!
//! **A token is bound to its audience.** `to` is signed, so a token presented at any door but the
//! one it was issued for is refused there ([`FormationRefusal::NotPermitted`]) and reaches nothing.
//! Verification order is fixed: tag → signature → payload → formation id, and every failure among
//! them is the one [`FormationRefusal::Invalid`]. Only a token that passed all four is looked at
//! for its audience, so a forged or foreign token learns nothing about which members exist.
//!
//! **There is no expiry, rotation or revocation list.** One formation is one task. The launcher
//! drops the key when the formation is torn down, and a new launch of the same roster has a new id
//! and a new key, so no token outlives what it was issued for.
//!
//! **Tokens reach a member's runtime and nothing else.** The launcher writes each member's
//! [`FormationBundle`] as the first line of a pipe only that member's `mur run` inherits, named by
//! [`FORMATION_CHANNEL_ENV`], and later lines carry callees' door URLs as they become known.
//! [`FormationMember`] reads the channel. A guest sees its callees' names and the virtual address
//! `http://<name>.formation.invalid` under the reserved `.invalid` domain; the runtime's egress
//! resolves that to the real door and attaches the token. No token reaches an environment
//! variable, argv, a file, stdout, the trace, an error or `Debug` output: [`FormationToken`] has no
//! `Display` and no `Serialize`, and [`FormationToken::expose`] is the one way to read it.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read};
use std::sync::{mpsc, Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

use base64::Engine as _;
use ring::signature::{Ed25519KeyPair, KeyPair, UnparsedPublicKey, ED25519};
use serde_json::Value;

use crate::errors::RuntimeError;
use crate::formation::{peer_url_format_error, FormationId, FormationPeer, FormationPeers};
use crate::mac_token::{split_token, zeroize, B64};

/// The version tag of a formation token.
pub const FORMATION_TOKEN_TAG: &str = "mft1";

/// The signature domain of a formation token. Distinct from every MAC domain, so no other token
/// family's bytes are ever a formation token's signed input.
pub const FORMATION_TOKEN_DOMAIN: &[u8] = b"murmur-formation-token-v1";

/// Separator between the signed input's fields. It cannot occur in base64url.
const FIELD_SEPARATOR: u8 = 0x1f;

/// The variable a formation launcher sets in each member's environment, naming the inherited file
/// descriptor its formation channel is read from. Set by `mur run --roster` and nothing else.
pub const FORMATION_CHANNEL_ENV: &str = "MURMUR_FORMATION_CHANNEL";

/// How long a member waits for its channel's first line before its launch is refused.
pub const FORMATION_CHANNEL_FIRST_LINE_DEADLINE: Duration = Duration::from_secs(10);

/// How long a guest's request to a callee waits for that callee's address to arrive on the channel
/// before it is refused.
pub const FORMATION_ADDRESS_WAIT: Duration = Duration::from_secs(10);

/// The reserved domain every virtual callee address sits under. `.invalid` never resolves
/// (RFC 6761), so a request that escaped the runtime's egress reaches nobody.
pub const FORMATION_PEER_DOMAIN: &str = "formation.invalid";

/// Exactly what a valid formation token reaches at the door it was issued for: starting a task,
/// and reading the tasks this member started. No `message/stream`, whose connection forwards the
/// frames of every task on the door, and nothing that stops, cancels or reads files.
pub const FORMATION_CALL_SCOPES: [&str; 2] = ["message/send", "tasks/get"];

/// The prefix of the door credential a formation token is let in as: `member:<from>`. A declared
/// credential name cannot contain `:`, so the two never collide.
pub const FORMATION_CREDENTIAL_PREFIX: &str = "member:";

/// The longest line the channel reader accepts. A bundle is a few hundred bytes per callee.
const MAX_CHANNEL_LINE_BYTES: u64 = 1 << 20;

/// Width of an Ed25519 public key and of its seed.
const KEY_BYTES: usize = 32;

/// Width of an Ed25519 signature.
const SIGNATURE_BYTES: usize = 64;

// ── Tokens ────────────────────────────────────────────────────────────────────

/// One formation token's text.
///
/// `Debug` prints `FormationToken(<redacted>)`. There is no `Display` and no `Serialize`, so the
/// text reaches a log line, a trace event or an error body by no route but [`Self::expose`].
#[derive(Clone, PartialEq, Eq)]
pub struct FormationToken(String);

impl FormationToken {
    /// The token text, for the `Authorization` header the runtime attaches and the channel line
    /// the launcher writes.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for FormationToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FormationToken(<redacted>)")
    }
}

/// What a token's signature covers.
fn signed_input(payload_b64: &str) -> Vec<u8> {
    let mut input = Vec::with_capacity(FORMATION_TOKEN_DOMAIN.len() + payload_b64.len() + 6);
    input.extend_from_slice(FORMATION_TOKEN_DOMAIN);
    input.push(FIELD_SEPARATOR);
    input.extend_from_slice(FORMATION_TOKEN_TAG.as_bytes());
    input.push(FIELD_SEPARATOR);
    input.extend_from_slice(payload_b64.as_bytes());
    input
}

/// What a token whose signature verified says.
struct Claims {
    formation: String,
    from: String,
    to: String,
}

/// Tag, then signature, then payload. `None` for any failure, indistinguishably.
fn open(key: &FormationVerifyKey, token: &str) -> Option<Claims> {
    let (payload_b64, payload, signature) = split_token(FORMATION_TOKEN_TAG, token).ok()?;
    if signature.len() != SIGNATURE_BYTES {
        return None;
    }
    UnparsedPublicKey::new(&ED25519, &key.0)
        .verify(&signed_input(payload_b64), &signature)
        .ok()?;
    let payload: Value = serde_json::from_slice(&payload).ok()?;
    let object = payload.as_object()?;
    if object.len() != 3 {
        return None;
    }
    let field = |name: &str| object.get(name)?.as_str().map(str::to_string);
    let claims = Claims {
        formation: field("formation")?,
        from: field("from")?,
        to: field("to")?,
    };
    let names_are_members = murmur_artifact::member_name_format_error(&claims.from).is_none()
        && murmur_artifact::member_name_format_error(&claims.to).is_none();
    names_are_members.then_some(claims)
}

// ── The launcher's authority ──────────────────────────────────────────────────

/// A formation's signing key: generated by the launcher for one formation and held in its memory
/// alone. Dropping it — at teardown — leaves nothing that can mint for that formation again.
///
/// `ring` keeps the expanded key in its own type, which does not zeroize on drop; the seed it was
/// built from is zeroized here as soon as the key pair exists.
pub struct FormationAuthority {
    formation_id: FormationId,
    key: Ed25519KeyPair,
}

impl std::fmt::Debug for FormationAuthority {
    /// Names the formation and nothing that signs.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FormationAuthority")
            .field("formation_id", &self.formation_id.as_str())
            .finish_non_exhaustive()
    }
}

impl FormationAuthority {
    /// A fresh key pair for `formation_id`, from the operating system's CSPRNG.
    ///
    /// The `Err` is the operating system refusing randomness, and carries no key material.
    pub fn generate(formation_id: &FormationId) -> Result<Self, String> {
        let mut seed = [0u8; KEY_BYTES];
        getrandom::fill(&mut seed)
            .map_err(|error| format!("failed to generate a formation key: {error}"))?;
        let key = Ed25519KeyPair::from_seed_unchecked(&seed);
        zeroize(&mut seed);
        let key = key.map_err(|_| "failed to build a formation key".to_string())?;
        Ok(Self {
            formation_id: formation_id.clone(),
            key,
        })
    }

    pub fn formation_id(&self) -> &FormationId {
        &self.formation_id
    }

    /// The token `from` presents at `to`'s door.
    pub fn mint(&self, from: &str, to: &str) -> FormationToken {
        let payload = serde_json::json!({
            "formation": self.formation_id.as_str(),
            "from": from,
            "to": to,
        });
        let payload_b64 = B64.encode(payload.to_string());
        let signature = self.key.sign(&signed_input(&payload_b64));
        FormationToken(format!(
            "{FORMATION_TOKEN_TAG}.{payload_b64}.{}",
            B64.encode(signature.as_ref())
        ))
    }

    /// The public half, which every member's door verifies with.
    pub fn verify_key(&self) -> FormationVerifyKey {
        let mut bytes = [0u8; KEY_BYTES];
        bytes.copy_from_slice(self.key.public_key().as_ref());
        FormationVerifyKey(bytes)
    }

    /// What `member` is handed on its channel's first line: the formation, its own name, the
    /// verification key, and one token per name in `callees`, in the order given.
    pub fn member_bundle(&self, member: &str, callees: &[&str]) -> FormationBundle {
        FormationBundle {
            formation_id: self.formation_id.clone(),
            member: member.to_string(),
            verify_key: self.verify_key(),
            calls: callees
                .iter()
                .map(|callee| (callee.to_string(), self.mint(member, callee)))
                .collect(),
        }
    }
}

#[cfg(test)]
impl FormationAuthority {
    /// An authority over a freshly minted formation.
    pub(crate) fn for_test() -> Self {
        Self::generate(&FormationId::mint()).expect("the OS hands out randomness")
    }

    /// The verifier `member`'s door holds in this formation.
    pub(crate) fn verifier_for(&self, member: &str) -> FormationVerifier {
        FormationVerifier::new(
            self.formation_id.clone(),
            member.to_string(),
            self.verify_key(),
        )
    }
}

/// A formation's 32-byte Ed25519 public key.
#[derive(Clone, PartialEq, Eq)]
pub struct FormationVerifyKey([u8; KEY_BYTES]);

impl FormationVerifyKey {
    /// base64url, unpadded.
    pub fn render(&self) -> String {
        B64.encode(self.0)
    }

    /// Exactly 32 bytes of base64url. The error never contains the value.
    pub fn parse(value: &str) -> Result<Self, String> {
        let bytes = B64
            .decode(value)
            .map_err(|_| "it is not base64url".to_string())?;
        let bytes: [u8; KEY_BYTES] = bytes
            .try_into()
            .map_err(|_| format!("it is not {KEY_BYTES} bytes"))?;
        Ok(Self(bytes))
    }
}

impl std::fmt::Debug for FormationVerifyKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FormationVerifyKey(..)")
    }
}

// ── The door's check ──────────────────────────────────────────────────────────

/// What one member's door holds to check a formation token: its formation, its own name, and the
/// formation's public key.
#[derive(Clone)]
pub struct FormationVerifier {
    formation_id: FormationId,
    member: String,
    key: FormationVerifyKey,
}

impl std::fmt::Debug for FormationVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FormationVerifier")
            .field("formation_id", &self.formation_id.as_str())
            .field("member", &self.member)
            .finish_non_exhaustive()
    }
}

/// The member a verified formation token was issued to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormationCaller {
    from: String,
}

impl FormationCaller {
    /// The calling member's roster name.
    pub fn from(&self) -> &str {
        &self.from
    }
}

/// Why a formation token was not let in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormationRefusal {
    /// Not a token this formation's key signed, for any reason at all: malformed, forged,
    /// tampered, or another formation's.
    Invalid,
    /// A genuine token of this formation, issued for another member's door. `caller` is the
    /// member it was issued to, which is who presented it.
    NotPermitted { caller: String },
}

impl FormationVerifier {
    pub fn new(formation_id: FormationId, member: String, key: FormationVerifyKey) -> Self {
        Self {
            formation_id,
            member,
            key,
        }
    }

    /// Check `token` in the fixed order tag → signature → payload → formation id, then its
    /// audience.
    pub fn verify(&self, token: &str) -> Result<FormationCaller, FormationRefusal> {
        let claims = open(&self.key, token).ok_or(FormationRefusal::Invalid)?;
        if claims.formation != self.formation_id.as_str() {
            return Err(FormationRefusal::Invalid);
        }
        if claims.to != self.member {
            return Err(FormationRefusal::NotPermitted {
                caller: claims.from,
            });
        }
        Ok(FormationCaller { from: claims.from })
    }
}

// ── The channel ───────────────────────────────────────────────────────────────

/// One member's credentials, as its channel's first line carries them:
///
/// ```json
/// {"formation_id":"frm_…","member":"coder","verify_key":"<b64url>","calls":[{"name":"reviewer","token":"mft1.…"}]}
/// ```
///
/// Every field is required; `calls` is empty for a member that may call nobody.
#[derive(Clone)]
pub struct FormationBundle {
    pub formation_id: FormationId,
    pub member: String,
    pub verify_key: FormationVerifyKey,
    /// One token per callee, in roster order.
    pub calls: Vec<(String, FormationToken)>,
}

impl std::fmt::Debug for FormationBundle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FormationBundle")
            .field("formation_id", &self.formation_id.as_str())
            .field("member", &self.member)
            .field(
                "calls",
                &self
                    .calls
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

fn channel_unreadable(reason: impl Into<String>) -> RuntimeError {
    RuntimeError::FormationChannelUnreadable {
        reason: reason.into(),
    }
}

/// A JSON string literal for `value`.
fn json_string(value: &str) -> String {
    Value::String(value.to_string()).to_string()
}

impl FormationBundle {
    /// The channel's first line, without its line ending. It carries tokens, and is written to the
    /// member's channel and nowhere else.
    pub fn render_line(&self) -> String {
        let calls: Vec<String> = self
            .calls
            .iter()
            .map(|(name, token)| {
                format!(
                    "{{\"name\":{},\"token\":{}}}",
                    json_string(name),
                    json_string(token.expose())
                )
            })
            .collect();
        format!(
            "{{\"formation_id\":{},\"member\":{},\"verify_key\":{},\"calls\":[{}]}}",
            json_string(self.formation_id.as_str()),
            json_string(&self.member),
            json_string(&self.verify_key.render()),
            calls.join(",")
        )
    }

    /// Read a first line. Every token must verify under the line's own key as issued by its member
    /// to the callee it is listed for, in its formation.
    ///
    /// The error names the field at fault, by position for a call, and never quotes the line.
    pub fn parse_line(line: &str) -> Result<Self, RuntimeError> {
        let value: Value = serde_json::from_str(line)
            .map_err(|_| channel_unreadable("its first line is not JSON"))?;
        let object = value
            .as_object()
            .ok_or_else(|| channel_unreadable("its first line is not a JSON object"))?;
        let string = |name: &str| {
            object
                .get(name)
                .and_then(Value::as_str)
                .ok_or_else(|| channel_unreadable(format!("its first line has no string {name}")))
        };
        let formation_id = FormationId::parse(string("formation_id")?).map_err(|error| {
            channel_unreadable(match error {
                RuntimeError::FormationIdUnreadable { reason } => {
                    format!("its first line's formation_id is not a formation id: {reason}")
                }
                _ => "its first line's formation_id is not a formation id".to_string(),
            })
        })?;
        let member = string("member")?.to_string();
        if let Some(problem) = murmur_artifact::member_name_format_error(&member) {
            return Err(channel_unreadable(format!(
                "its first line's member is not a member name: it {problem}"
            )));
        }
        let verify_key = FormationVerifyKey::parse(string("verify_key")?).map_err(|why| {
            channel_unreadable(format!(
                "its first line's verify_key is not a formation key: {why}"
            ))
        })?;
        let entries = object
            .get("calls")
            .and_then(Value::as_array)
            .ok_or_else(|| channel_unreadable("its first line has no calls array"))?;
        let mut calls = Vec::with_capacity(entries.len());
        let mut seen = HashSet::new();
        for (index, entry) in entries.iter().enumerate() {
            let field = |name: &str| {
                entry.get(name).and_then(Value::as_str).ok_or_else(|| {
                    channel_unreadable(format!(
                        "its first line's calls[{index}] has no string {name}"
                    ))
                })
            };
            let name = field("name")?;
            if let Some(problem) = murmur_artifact::member_name_format_error(name) {
                return Err(channel_unreadable(format!(
                    "its first line's calls[{index}].name is not a member name: it {problem}"
                )));
            }
            if !seen.insert(name.to_string()) {
                return Err(channel_unreadable(format!(
                    "its first line's calls[{index}] names a member a second time"
                )));
            }
            let token = field("token")?;
            let issued = open(&verify_key, token).is_some_and(|claims| {
                claims.formation == formation_id.as_str()
                    && claims.from == member
                    && claims.to == name
            });
            if !issued {
                return Err(channel_unreadable(format!(
                    "its first line's calls[{index}].token was not issued to this member for that \
                     callee under the line's own key"
                )));
            }
            calls.push((name.to_string(), FormationToken(token.to_string())));
        }
        Ok(Self {
            formation_id,
            member,
            verify_key,
            calls,
        })
    }
}

/// One address line: `{"addresses":[{"name":"coder","url":"http://127.0.0.1:41873"}]}`.
pub fn render_address_line(addresses: &[FormationPeer]) -> String {
    let entries: Vec<String> = addresses
        .iter()
        .map(|peer| {
            format!(
                "{{\"name\":{},\"url\":{}}}",
                json_string(&peer.name),
                json_string(&peer.url)
            )
        })
        .collect();
    format!("{{\"addresses\":[{}]}}", entries.join(","))
}

/// Read an address line. Every URL must be `http://host:port`. The error never quotes the line.
pub fn parse_address_line(line: &str) -> Result<Vec<FormationPeer>, String> {
    let value: Value =
        serde_json::from_str(line).map_err(|_| "an address line is not JSON".to_string())?;
    let entries = value
        .get("addresses")
        .and_then(Value::as_array)
        .ok_or_else(|| "an address line has no addresses array".to_string())?;
    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let field = |name: &str| {
                entry
                    .get(name)
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .ok_or_else(|| format!("addresses[{index}] has no string {name}"))
            };
            let peer = FormationPeer {
                name: field("name")?,
                url: field("url")?,
            };
            if let Some(problem) = murmur_artifact::member_name_format_error(&peer.name) {
                return Err(format!("addresses[{index}].name {problem}"));
            }
            if let Some(problem) = peer_url_format_error(&peer.url) {
                return Err(format!("addresses[{index}] {problem}"));
            }
            Ok(peer)
        })
        .collect()
}

/// The virtual address a guest calls `name` at: `http://<name>.formation.invalid`.
pub fn virtual_url(name: &str) -> String {
    format!("http://{name}.{FORMATION_PEER_DOMAIN}")
}

/// Whether `host` lies under [`FORMATION_PEER_DOMAIN`], in any letter case. Every such request is
/// the runtime's to route or to refuse; none is ever sent as written.
pub fn is_formation_host(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    host == FORMATION_PEER_DOMAIN || host.ends_with(&format!(".{FORMATION_PEER_DOMAIN}"))
}

/// The member `authority` names when it is exactly `<name>.formation.invalid` — one label, no
/// port, no user info — or `None`.
pub fn virtual_member(authority: &str) -> Option<&str> {
    let suffix_len = FORMATION_PEER_DOMAIN.len() + 1;
    if authority.len() <= suffix_len || !authority.is_char_boundary(authority.len() - suffix_len) {
        return None;
    }
    let (name, suffix) = authority.split_at(authority.len() - suffix_len);
    let suffix_matches = suffix
        .strip_prefix('.')
        .is_some_and(|domain| domain.eq_ignore_ascii_case(FORMATION_PEER_DOMAIN));
    (suffix_matches && murmur_artifact::member_name_format_error(name).is_none()).then_some(name)
}

/// Where the channel reader has got to.
#[derive(Default)]
struct AddressBook {
    urls: HashMap<String, String>,
    /// The channel reached its end: no address will arrive any more.
    closed: bool,
}

/// One member's live formation state: who it is, what its door accepts, the tokens for the members
/// it may call, and the door URLs of those members as the channel delivers them.
pub struct FormationMember {
    verifier: FormationVerifier,
    /// Roster order.
    calls: Vec<(String, FormationToken)>,
    book: Mutex<AddressBook>,
    arrived: Condvar,
    /// How long a call to a callee waits for its address: [`FORMATION_ADDRESS_WAIT`].
    address_wait: Duration,
}

impl std::fmt::Debug for FormationMember {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FormationMember")
            .field("formation_id", &self.verifier.formation_id.as_str())
            .field("member", &self.verifier.member)
            .field("callees", &self.callees().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl FormationMember {
    /// A member holding `bundle`, with no address yet.
    pub fn from_bundle(bundle: FormationBundle) -> Self {
        Self {
            verifier: FormationVerifier::new(bundle.formation_id, bundle.member, bundle.verify_key),
            calls: bundle.calls,
            book: Mutex::new(AddressBook::default()),
            arrived: Condvar::new(),
            address_wait: FORMATION_ADDRESS_WAIT,
        }
    }

    /// This member, with calls waiting at most `wait` for an address.
    #[cfg(test)]
    pub(crate) fn with_address_wait(mut self, wait: Duration) -> Self {
        self.address_wait = wait;
        self
    }

    /// How long a call to a callee waits for the callee's address to arrive.
    pub fn address_wait(&self) -> Duration {
        self.address_wait
    }

    /// This session's formation membership, read from the channel [`FORMATION_CHANNEL_ENV`] names.
    ///
    /// `Ok(None)` when the variable is absent, which is every session but a formation member's.
    /// Present, it requires `formation_id` — the `MURMUR_FORMATION_ID` the caller already read —
    /// marks the descriptor close-on-exec before anything else can be started, and reads the first
    /// line within [`FORMATION_CHANNEL_FIRST_LINE_DEADLINE`]. A thread then applies address lines
    /// until the channel ends.
    pub fn from_env(formation_id: Option<&FormationId>) -> Result<Option<Arc<Self>>, RuntimeError> {
        let Some(raw) = std::env::var_os(FORMATION_CHANNEL_ENV) else {
            return Ok(None);
        };
        let Some(formation_id) = formation_id else {
            return Err(channel_unreadable(
                "it is set, but MURMUR_FORMATION_ID is not, and a formation launcher always sets \
                 both",
            ));
        };
        let fd: i32 = raw
            .to_str()
            .filter(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|digits| digits.parse().ok())
            .ok_or_else(|| channel_unreadable("it does not name a file descriptor by number"))?;
        let channel = adopt_channel_fd(fd)?;
        Self::from_channel(channel, formation_id, FORMATION_CHANNEL_FIRST_LINE_DEADLINE).map(Some)
    }

    /// Read a member's first line from `channel` within `deadline`, check that it belongs to
    /// `formation_id`, and leave a thread applying the address lines that follow.
    pub fn from_channel<R: Read + Send + 'static>(
        channel: R,
        formation_id: &FormationId,
        deadline: Duration,
    ) -> Result<Arc<Self>, RuntimeError> {
        type FirstLine = Result<Option<String>, &'static str>;
        let (first_tx, first_rx) = mpsc::sync_channel::<FirstLine>(1);
        let (member_tx, member_rx) = mpsc::sync_channel::<Arc<Self>>(1);
        std::thread::Builder::new()
            .name("formation-channel".to_string())
            .spawn(move || {
                let mut reader = BufReader::new(channel);
                let first = read_channel_line(&mut reader);
                let read_one = matches!(first, Ok(Some(_)));
                if first_tx.send(first).is_err() || !read_one {
                    return;
                }
                let Ok(member) = member_rx.recv() else {
                    return;
                };
                while let Ok(Some(line)) = read_channel_line(&mut reader) {
                    member.apply_address_line(&line);
                }
                member.close();
            })
            .map_err(|error| {
                channel_unreadable(format!(
                    "the thread that reads it could not be started: {error}"
                ))
            })?;
        let line = match first_rx.recv_timeout(deadline) {
            Ok(Ok(Some(line))) => line,
            Ok(Ok(None)) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(channel_unreadable(
                    "the channel ended before its first line arrived",
                ))
            }
            Ok(Err(why)) => return Err(channel_unreadable(why)),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Err(channel_unreadable(format!(
                    "no first line arrived within {}s",
                    deadline.as_secs_f32()
                )))
            }
        };
        let bundle = FormationBundle::parse_line(&line)?;
        if bundle.formation_id != *formation_id {
            return Err(channel_unreadable(
                "its first line names another formation than MURMUR_FORMATION_ID",
            ));
        }
        let member = Arc::new(Self::from_bundle(bundle));
        let _ = member_tx.send(Arc::clone(&member));
        Ok(member)
    }

    /// This member's roster name.
    pub fn name(&self) -> &str {
        &self.verifier.member
    }

    pub fn formation_id(&self) -> &FormationId {
        &self.verifier.formation_id
    }

    /// What this member's door checks a formation token with.
    pub fn verifier(&self) -> &FormationVerifier {
        &self.verifier
    }

    /// The members this one may call, in roster order.
    pub fn callees(&self) -> impl Iterator<Item = &str> {
        self.calls.iter().map(|(name, _)| name.as_str())
    }

    /// What every WASM guest of this member is handed as `MURMUR_FORMATION_PEERS`: each callee at
    /// its virtual address. `None` when it may call nobody.
    pub fn guest_peers(&self) -> Option<FormationPeers> {
        let peers: Vec<FormationPeer> = self
            .callees()
            .map(|name| FormationPeer {
                name: name.to_string(),
                url: virtual_url(name),
            })
            .collect();
        if peers.is_empty() {
            return None;
        }
        FormationPeers::new(peers).ok()
    }

    /// `name`'s real door URL and the token for it, waiting up to `wait` for the address to
    /// arrive. `None` for a member this one may not call, and for an address that does not arrive
    /// in time or will not arrive at all.
    pub fn resolve(&self, name: &str, wait: Duration) -> Option<(String, FormationToken)> {
        let token = self
            .calls
            .iter()
            .find(|(callee, _)| callee == name)
            .map(|(_, token)| token.clone())?;
        let book = self.book.lock().unwrap_or_else(PoisonError::into_inner);
        let (book, _) = self
            .arrived
            .wait_timeout_while(book, wait, |book| {
                !book.closed && !book.urls.contains_key(name)
            })
            .unwrap_or_else(PoisonError::into_inner);
        book.urls.get(name).map(|url| (url.clone(), token))
    }

    /// Install each address in `addresses` whose name is a callee; every other name is ignored, so
    /// no route exists without a token.
    pub fn install_addresses(&self, addresses: Vec<FormationPeer>) {
        let mut book = self.book.lock().unwrap_or_else(PoisonError::into_inner);
        for peer in addresses {
            if self.calls.iter().any(|(callee, _)| *callee == peer.name) {
                book.urls.insert(peer.name, peer.url);
            }
        }
        drop(book);
        self.arrived.notify_all();
    }

    fn apply_address_line(&self, line: &str) {
        match parse_address_line(line) {
            Ok(addresses) => self.install_addresses(addresses),
            Err(why) => crate::runtime_err!(
                "[capsule-runtime] the formation channel sent an address line that could not be \
                 read ({why}); it was ignored"
            ),
        }
    }

    /// No address will arrive any more: every waiter gives up now rather than at its bound.
    fn close(&self) {
        self.book
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .closed = true;
        self.arrived.notify_all();
    }
}

/// One line from the channel without its line ending, `None` at the channel's end.
fn read_channel_line(reader: &mut impl BufRead) -> Result<Option<String>, &'static str> {
    let mut line = Vec::new();
    let read = reader
        .by_ref()
        .take(MAX_CHANNEL_LINE_BYTES)
        .read_until(b'\n', &mut line)
        .map_err(|_| "the channel could not be read")?;
    if read == 0 {
        return Ok(None);
    }
    if line.last() == Some(&b'\n') {
        line.pop();
        if line.last() == Some(&b'\r') {
            line.pop();
        }
    } else if read as u64 >= MAX_CHANNEL_LINE_BYTES {
        return Err("a line is longer than the channel accepts");
    }
    String::from_utf8(line)
        .map(Some)
        .map_err(|_| "a line is not UTF-8")
}

/// Take ownership of the inherited descriptor `fd` as the channel: it must be open and a pipe or
/// a file, and is marked close-on-exec before anything else, so no process this one starts can
/// inherit it.
#[cfg(unix)]
#[allow(unsafe_code)]
fn adopt_channel_fd(fd: i32) -> Result<std::fs::File, RuntimeError> {
    use std::os::fd::FromRawFd;
    use std::os::unix::fs::FileTypeExt;

    if fd <= 2 {
        return Err(channel_unreadable(format!(
            "descriptor {fd} is standard input, output or error, never a channel"
        )));
    }
    // SAFETY: `fcntl(F_GETFD)` reads the flags of an integer descriptor and dereferences nothing.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags == -1 {
        return Err(channel_unreadable(format!("descriptor {fd} is not open")));
    }
    // SAFETY: as for `F_GETFD`; `F_SETFD` takes the flag word by value.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == -1 {
        return Err(channel_unreadable(format!(
            "descriptor {fd} could not be marked close-on-exec"
        )));
    }
    // SAFETY: `fd` is open (checked above), was inherited for exactly this purpose and is named by
    // the launcher alone, so nothing else in this process owns it; the `File` takes sole ownership
    // and closes it on drop.
    let channel = unsafe { std::fs::File::from_raw_fd(fd) };
    let kind = channel
        .metadata()
        .map_err(|_| channel_unreadable(format!("descriptor {fd} could not be examined")))?
        .file_type();
    if !kind.is_fifo() && !kind.is_file() {
        return Err(channel_unreadable(format!(
            "descriptor {fd} is neither a pipe nor a file"
        )));
    }
    Ok(channel)
}

#[cfg(not(unix))]
fn adopt_channel_fd(_fd: i32) -> Result<std::fs::File, RuntimeError> {
    Err(channel_unreadable(
        "an inherited descriptor channel is supported on Unix hosts only",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// A roster in which `planner` (entry) may call `coder`, and `coder` may call `reviewer`.
    fn chain_roster() -> (FormationAuthority, Vec<(&'static str, Vec<&'static str>)>) {
        let authority = FormationAuthority::generate(&FormationId::mint()).unwrap();
        (
            authority,
            vec![
                ("planner", vec!["coder"]),
                ("coder", vec!["reviewer"]),
                ("reviewer", vec![]),
            ],
        )
    }

    fn payload_of(token: &FormationToken) -> Value {
        let payload = token.expose().split('.').nth(1).unwrap();
        serde_json::from_slice(&B64.decode(payload).unwrap()).unwrap()
    }

    #[test]
    fn a_token_is_mft1_over_exactly_the_three_claims() {
        let (authority, _) = chain_roster();
        let token = authority.mint("planner", "coder");
        assert!(token.expose().starts_with("mft1."));
        assert_eq!(token.expose().split('.').count(), 3);
        let payload = token.expose().split('.').nth(1).unwrap();
        assert_eq!(
            String::from_utf8(B64.decode(payload).unwrap()).unwrap(),
            format!(
                r#"{{"formation":"{}","from":"planner","to":"coder"}}"#,
                authority.formation_id()
            )
        );
        let signature = token.expose().split('.').nth(2).unwrap();
        assert_eq!(B64.decode(signature).unwrap().len(), SIGNATURE_BYTES);
    }

    #[test]
    fn a_token_verifies_at_its_audience_and_is_not_permitted_anywhere_else() {
        let (authority, _) = chain_roster();
        let token = authority.mint("planner", "coder");
        assert_eq!(
            authority
                .verifier_for("coder")
                .verify(token.expose())
                .unwrap()
                .from(),
            "planner"
        );
        for door in ["reviewer", "planner"] {
            assert_eq!(
                authority.verifier_for(door).verify(token.expose()),
                Err(FormationRefusal::NotPermitted {
                    caller: "planner".to_string()
                }),
                "{door}"
            );
        }
    }

    #[test]
    fn every_forgery_is_the_one_invalid() {
        let (authority, _) = chain_roster();
        let coder = authority.verifier_for("coder");
        let token = authority.mint("planner", "coder");
        let (_, payload, signature) = {
            let mut parts = token.expose().split('.');
            (
                parts.next().unwrap(),
                parts.next().unwrap(),
                parts.next().unwrap(),
            )
        };

        // Another authority over the same names, in a new formation.
        let other = FormationAuthority::generate(&FormationId::mint()).unwrap();
        let foreign = other.mint("planner", "coder");
        // Re-addressed without re-signing.
        let readdressed = format!(
            "mft1.{}.{signature}",
            B64.encode(
                format!(
                    r#"{{"formation":"{}","from":"planner","to":"reviewer"}}"#,
                    authority.formation_id()
                )
                .as_bytes()
            )
        );
        // One signature byte flipped.
        let mut bytes = B64.decode(signature).unwrap();
        bytes[7] ^= 0x01;
        let flipped = format!("mft1.{payload}.{}", B64.encode(&bytes));
        // The same key, a different formation id in the payload.
        let mut same_key_other_formation = authority.mint("planner", "coder");
        same_key_other_formation.0 = {
            let other_id = FormationId::mint();
            let forged_payload = B64.encode(format!(
                r#"{{"formation":"{other_id}","from":"planner","to":"coder"}}"#
            ));
            let signature = authority.key.sign(&signed_input(&forged_payload));
            format!("mft1.{forged_payload}.{}", B64.encode(signature.as_ref()))
        };
        // A signed payload with an extra claim.
        let extra = {
            let forged_payload = B64.encode(format!(
                r#"{{"formation":"{}","from":"planner","to":"coder","admin":"yes"}}"#,
                authority.formation_id()
            ));
            let signature = authority.key.sign(&signed_input(&forged_payload));
            format!("mft1.{forged_payload}.{}", B64.encode(signature.as_ref()))
        };
        for forged in [
            foreign.expose().to_string(),
            readdressed,
            flipped,
            same_key_other_formation.expose().to_string(),
            extra,
            format!("mdt1.{payload}.{signature}"),
            format!("mft1.{payload}"),
            format!("mft1.{payload}.{signature}.x"),
            "mft1.x.y".to_string(),
            "garbage".to_string(),
            String::new(),
        ] {
            assert_eq!(
                coder.verify(&forged),
                Err(FormationRefusal::Invalid),
                "{forged}"
            );
        }
        // The other formation's door refuses this formation's token the same way.
        assert_eq!(
            FormationVerifier::new(
                other.formation_id().clone(),
                "coder".to_string(),
                other.verify_key()
            )
            .verify(token.expose()),
            Err(FormationRefusal::Invalid)
        );
    }

    /// Each bundle holds one token per callee in roster order, each verifying as
    /// `member → callee`, and nothing about a member it may not call.
    #[test]
    fn a_bundle_holds_exactly_its_callees_tokens_in_roster_order() {
        let (authority, roster) = chain_roster();
        for (member, callees) in &roster {
            let bundle = authority.member_bundle(member, callees);
            let names: Vec<&str> = bundle.calls.iter().map(|(name, _)| name.as_str()).collect();
            assert_eq!(&names, callees, "{member}");
            for (callee, token) in &bundle.calls {
                assert_eq!(
                    authority
                        .verifier_for(callee)
                        .verify(token.expose())
                        .unwrap()
                        .from(),
                    *member
                );
                assert_eq!(payload_of(token)["to"], callee.as_str());
            }
        }
        let planner = authority.member_bundle("planner", &["coder"]).render_line();
        assert!(!planner.contains("reviewer"), "{planner}");

        // `all` over three consenting members, entry excluded from every callee list.
        let all = FormationAuthority::generate(&FormationId::mint()).unwrap();
        let bundle = all.member_bundle("coder", &["reviewer", "tester"]);
        let names: Vec<&str> = bundle.calls.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["reviewer", "tester"]);
    }

    #[test]
    fn a_first_line_round_trips_and_its_field_order_is_fixed() {
        let (authority, _) = chain_roster();
        let bundle = authority.member_bundle("coder", &["reviewer"]);
        let line = bundle.render_line();
        assert!(
            line.starts_with(&format!(
                "{{\"formation_id\":\"{}\",\"member\":\"coder\",\"verify_key\":\"",
                authority.formation_id()
            )),
            "{line}"
        );
        assert!(line.contains(",\"calls\":[{\"name\":\"reviewer\",\"token\":\"mft1."));
        assert!(!line.contains('\n'));
        let back = FormationBundle::parse_line(&line).unwrap();
        assert_eq!(back.formation_id, bundle.formation_id);
        assert_eq!(back.member, "coder");
        assert_eq!(back.verify_key, authority.verify_key());
        assert_eq!(back.calls, bundle.calls);
        let empty = authority.member_bundle("reviewer", &[]).render_line();
        assert!(empty.ends_with("\"calls\":[]}"), "{empty}");
        assert!(FormationBundle::parse_line(&empty)
            .unwrap()
            .calls
            .is_empty());
    }

    fn line_refusal(line: &str) -> String {
        match FormationBundle::parse_line(line) {
            Err(error @ RuntimeError::FormationChannelUnreadable { .. }) => error.to_string(),
            other => panic!("expected FormationChannelUnreadable, got {other:?}"),
        }
    }

    /// Every malformed first line is refused naming its field, and no refusal quotes the line.
    #[test]
    fn a_first_line_refusal_names_the_problem_and_never_quotes_the_line() {
        let (authority, _) = chain_roster();
        let good: Value = serde_json::from_str(
            &authority
                .member_bundle("coder", &["reviewer"])
                .render_line(),
        )
        .unwrap();
        let token = good["calls"][0]["token"].as_str().unwrap().to_string();
        let with = |field: &str, value: Value| {
            let mut line = good.clone();
            line[field] = value;
            line.to_string()
        };
        let other = FormationAuthority::generate(&FormationId::mint()).unwrap();
        let cases = [
            ("not json".to_string(), "is not JSON"),
            ("[1]".to_string(), "is not a JSON object"),
            (with("formation_id", json_str("frm_XYZ")), "formation_id"),
            (
                with("member", json_str("Coder")),
                "member is not a member name",
            ),
            (with("verify_key", json_str("abc")), "verify_key is not"),
            (
                with("verify_key", json_str(&other.verify_key().render())),
                "calls[0].token was not issued",
            ),
            (with("calls", Value::Null), "no calls array"),
            (
                with(
                    "calls",
                    serde_json::json!([{"name": "Reviewer", "token": token}]),
                ),
                "calls[0].name is not a member name",
            ),
            (
                with(
                    "calls",
                    serde_json::json!([{"name": "tester", "token": token}]),
                ),
                "calls[0].token was not issued",
            ),
            (
                with(
                    "calls",
                    serde_json::json!([
                        {"name": "reviewer", "token": token},
                        {"name": "reviewer", "token": token}
                    ]),
                ),
                "calls[1] names a member a second time",
            ),
        ];
        for (line, wording) in cases {
            let message = line_refusal(&line);
            assert!(message.contains(wording), "{wording}: {message}");
            assert!(!message.contains("mft1."), "{message}");
            assert!(!message.contains(&token), "{message}");
            assert!(
                !message.contains(&authority.verify_key().render()),
                "{message}"
            );
            assert!(message.contains(FORMATION_CHANNEL_ENV), "{message}");
        }
    }

    fn json_str(value: &str) -> Value {
        Value::String(value.to_string())
    }

    #[test]
    fn an_address_line_round_trips_and_refuses_what_is_not_a_door() {
        let peers = vec![FormationPeer {
            name: "reviewer".to_string(),
            url: "http://127.0.0.1:41873".to_string(),
        }];
        let line = render_address_line(&peers);
        assert_eq!(
            line,
            r#"{"addresses":[{"name":"reviewer","url":"http://127.0.0.1:41873"}]}"#
        );
        assert_eq!(parse_address_line(&line).unwrap(), peers);
        for bad in [
            r#"{"addresses":[{"name":"reviewer","url":"https://127.0.0.1:1"}]}"#,
            r#"{"addresses":[{"name":"reviewer","url":"http://127.0.0.1"}]}"#,
            r#"{"addresses":[{"name":"Reviewer","url":"http://127.0.0.1:1"}]}"#,
            r#"{"addresses":{}}"#,
            "nope",
        ] {
            assert!(parse_address_line(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_virtual_address_names_one_member_and_nothing_else() {
        assert_eq!(virtual_url("coder"), "http://coder.formation.invalid");
        assert_eq!(virtual_member("coder.formation.invalid"), Some("coder"));
        assert_eq!(virtual_member("coder.FORMATION.invalid"), Some("coder"));
        for authority in [
            "coder.formation.invalid:80",
            "u@coder.formation.invalid",
            "a.coder.formation.invalid",
            ".formation.invalid",
            "formation.invalid",
            "Coder.formation.invalid",
            "coder.formation.invalid.",
            "coder.example.com",
            "",
        ] {
            assert_eq!(virtual_member(authority), None, "{authority}");
        }
        assert!(is_formation_host("coder.formation.invalid"));
        assert!(is_formation_host("x.y.FORMATION.INVALID"));
        assert!(is_formation_host("formation.invalid"));
        assert!(!is_formation_host("formation.invalid.example"));
        assert!(!is_formation_host("notformation.invalid"));
    }

    /// `guest_peers` holds virtual URLs only, with no port, for the callees only.
    #[test]
    fn a_members_guests_see_its_callees_at_virtual_addresses_only() {
        let (authority, _) = chain_roster();
        let planner = FormationMember::from_bundle(authority.member_bundle("planner", &["coder"]));
        let peers = planner.guest_peers().unwrap();
        assert_eq!(peers.render(), "coder=http://coder.formation.invalid");
        let reviewer = FormationMember::from_bundle(authority.member_bundle("reviewer", &[]));
        assert!(reviewer.guest_peers().is_none());
    }

    #[test]
    fn an_address_resolves_only_for_a_callee_and_waits_for_a_late_one() {
        let (authority, _) = chain_roster();
        let member = Arc::new(FormationMember::from_bundle(
            authority.member_bundle("coder", &["reviewer"]),
        ));
        assert!(member
            .resolve("reviewer", Duration::from_millis(50))
            .is_none());
        member.install_addresses(vec![FormationPeer {
            name: "planner".to_string(),
            url: "http://127.0.0.1:9".to_string(),
        }]);
        assert!(
            member.resolve("planner", Duration::ZERO).is_none(),
            "no route without a token"
        );
        let late = Arc::clone(&member);
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            late.install_addresses(vec![FormationPeer {
                name: "reviewer".to_string(),
                url: "http://127.0.0.1:41873".to_string(),
            }]);
        });
        let (url, token) = member.resolve("reviewer", Duration::from_secs(10)).unwrap();
        writer.join().unwrap();
        assert_eq!(url, "http://127.0.0.1:41873");
        assert_eq!(
            authority
                .verifier_for("reviewer")
                .verify(token.expose())
                .unwrap()
                .from(),
            "coder"
        );
        member.close();
    }

    #[test]
    fn a_channel_delivers_the_first_line_then_addresses_until_it_ends() {
        let (authority, _) = chain_roster();
        let (reader, mut writer) = std::io::pipe().unwrap();
        use std::io::Write;
        writeln!(
            writer,
            "{}",
            authority
                .member_bundle("coder", &["reviewer"])
                .render_line()
        )
        .unwrap();
        let member =
            FormationMember::from_channel(reader, authority.formation_id(), Duration::from_secs(5))
                .unwrap();
        assert_eq!(member.name(), "coder");
        writeln!(writer, "not an address line").unwrap();
        writeln!(
            writer,
            "{}",
            render_address_line(&[FormationPeer {
                name: "reviewer".to_string(),
                url: "http://127.0.0.1:41873".to_string(),
            }])
        )
        .unwrap();
        let (url, _) = member.resolve("reviewer", Duration::from_secs(5)).unwrap();
        assert_eq!(url, "http://127.0.0.1:41873");
        drop(writer);
        // Once the channel has ended, a name with no address gives up at once.
        let started = Instant::now();
        let bare = FormationMember::from_channel(
            {
                let (reader, mut writer) = std::io::pipe().unwrap();
                writeln!(
                    writer,
                    "{}",
                    authority.member_bundle("planner", &["coder"]).render_line()
                )
                .unwrap();
                reader
            },
            authority.formation_id(),
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(bare.resolve("coder", Duration::from_secs(10)).is_none());
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_channel_refuses_a_late_empty_or_foreign_first_line() {
        let (authority, _) = chain_roster();
        let (reader, _writer) = std::io::pipe().unwrap();
        let late = FormationMember::from_channel(
            reader,
            authority.formation_id(),
            Duration::from_millis(200),
        )
        .unwrap_err()
        .to_string();
        assert!(late.contains("no first line arrived within 0.2s"), "{late}");

        let (reader, writer) = std::io::pipe().unwrap();
        drop(writer);
        let ended =
            FormationMember::from_channel(reader, authority.formation_id(), Duration::from_secs(5))
                .unwrap_err()
                .to_string();
        assert!(ended.contains("ended before its first line"), "{ended}");

        let foreign = FormationMember::from_channel(
            std::io::Cursor::new(format!(
                "{}\n",
                authority.member_bundle("coder", &[]).render_line()
            )),
            &FormationId::mint(),
            Duration::from_secs(5),
        )
        .unwrap_err()
        .to_string();
        assert!(foreign.contains("another formation"), "{foreign}");
    }

    /// Once read, the channel's descriptor is close-on-exec.
    #[cfg(unix)]
    #[test]
    #[allow(unsafe_code)]
    fn an_adopted_channel_descriptor_is_close_on_exec() {
        use std::os::fd::{AsRawFd, IntoRawFd};
        let (reader, _writer) = std::io::pipe().unwrap();
        let fd = reader.into_raw_fd();
        // SAFETY: clears the flag `pipe` set on a descriptor this test owns.
        unsafe { libc::fcntl(fd, libc::F_SETFD, 0) };
        let adopted = adopt_channel_fd(fd).unwrap();
        // SAFETY: reads the flags of the descriptor `adopted` owns.
        let flags = unsafe { libc::fcntl(adopted.as_raw_fd(), libc::F_GETFD) };
        assert_ne!(flags & libc::FD_CLOEXEC, 0);

        assert!(adopt_channel_fd(987_654)
            .unwrap_err()
            .to_string()
            .contains("is not open"));
        assert!(adopt_channel_fd(1)
            .unwrap_err()
            .to_string()
            .contains("never a channel"));
    }

    /// No formation type's `Debug` carries a token, a key or a signature.
    #[test]
    fn no_debug_output_carries_a_token_or_key() {
        let (authority, _) = chain_roster();
        let bundle = authority.member_bundle("coder", &["reviewer"]);
        let token = bundle.calls[0].1.clone();
        let signature = token.expose().split('.').nth(2).unwrap().to_string();
        let key = authority.verify_key().render();
        let member = FormationMember::from_bundle(bundle.clone());
        let outputs = [
            format!("{token:?}"),
            format!("{bundle:?}"),
            format!("{member:?}"),
            format!("{:?}", member.verifier()),
            format!("{authority:?}"),
            format!("{:?}", authority.verify_key()),
        ];
        assert_eq!(outputs[0], "FormationToken(<redacted>)");
        for output in &outputs {
            assert!(!output.contains("mft1."), "{output}");
            assert!(!output.contains(&signature), "{output}");
            assert!(!output.contains(&key), "{output}");
        }
        assert!(outputs[2].contains("reviewer"), "{}", outputs[2]);
    }

    /// The channel variable is read in one place, as the formation id is.
    #[test]
    fn the_channel_is_read_from_the_process_environment_in_one_place() {
        let mut sources = crate::source_scan::sources_under(
            &crate::source_scan::sibling_crate_src("capsule-runtime"),
        );
        sources.extend(crate::source_scan::sources_under(
            &crate::source_scan::sibling_crate_src("murmur-cli"),
        ));
        let mut readers = 0;
        for (_, text) in sources {
            let production = crate::source_scan::production_part(&text);
            readers += production.matches("var_os(FORMATION_CHANNEL_ENV)").count()
                + production.matches("var(FORMATION_CHANNEL_ENV)").count()
                + production
                    .matches("var_os(\"MURMUR_FORMATION_CHANNEL\")")
                    .count()
                + production
                    .matches("var(\"MURMUR_FORMATION_CHANNEL\")")
                    .count();
        }
        assert_eq!(readers, 1, "FormationMember::from_env is the one reader");
    }
}
