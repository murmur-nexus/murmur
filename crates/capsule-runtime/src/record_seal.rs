//! A session's records, made checkable by anyone holding the host's public key and nothing else.
//!
//! Three parts, each the one implementation `mur`, the escape-conformance harness and a check
//! capsule call:
//!
//! * **The host key** ([`HostKey`]): an Ed25519 seed in `~/.murmur/keys/record-seal.key` (64 hex
//!   characters, `0600`, directory `0700`) and its public half in `record-seal.pub` (64 hex
//!   characters, `0644`). Only the `mur` process reads the seed; no capsule grant at `scoped` or
//!   `sealed` reaches `~/.murmur/keys`.
//! * **The chain.** Every `trace.jsonl` line ends with a `"prev_hash"` key naming the
//!   [`line_hash`] of the line before it ([`GENESIS`] on line 1), written through one
//!   [`InlineChain`] per file however many appenders hold it. `hooks/eval.jsonl`,
//!   `completion.json` and a conversation record keep their bytes as written; their chain is the
//!   list of line hashes in the seal.
//! * **The seal** ([`Seal`]): `<stream>.seal` beside the stream, signing the chain's head, line
//!   count, stream name and sealing session with the host key, written temp-and-rename.
//!
//! [`verify_session_records`] checks every stream of one records directory with a public key
//! alone and reports a [`Verdict`] per stream and for the whole record.
//!
//! What is not sealed: a records directory's `home/`, `tmp/`, `logs/`, `out/` and `blobs/`
//! (blobs are named by their own sha256). Deleting a stream together with its seal reads as
//! [`Verdict::Absent`].

use std::collections::{HashMap, HashSet};
use std::io::{Read as _, Write as _};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, Weak};

use ring::signature::{Ed25519KeyPair, KeyPair as _, UnparsedPublicKey, ED25519};
use serde::{Deserialize, Serialize};

use crate::workdir_fs::{WorkdirRoot, DEFAULT_FILE_MODE};

// ── Names and formats ─────────────────────────────────────────────────────────

/// `~/.murmur/keys`, relative to the murmur home.
pub const KEYS_DIR: &str = "keys";
/// The Ed25519 seed, as 64 lowercase hex characters and a newline.
pub const PRIVATE_KEY_FILE: &str = "record-seal.key";
/// The Ed25519 public key, as 64 lowercase hex characters and a newline.
pub const PUBLIC_KEY_FILE: &str = "record-seal.pub";
/// Mode of `~/.murmur/keys`.
pub const KEYS_DIR_MODE: u32 = 0o700;
/// Mode of [`PRIVATE_KEY_FILE`]; a key file with any group or other bit is refused.
pub const PRIVATE_KEY_MODE: u32 = 0o600;
/// Mode of [`PUBLIC_KEY_FILE`].
pub const PUBLIC_KEY_MODE: u32 = 0o644;

/// The `format` of every seal this build writes, and the first line of its signed message.
pub const SEAL_FORMAT: &str = "murmur.seal/1";
/// Appended to a stream's file name to name its seal.
pub const SEAL_SUFFIX: &str = ".seal";
/// The only `algorithm` a seal names.
pub const SEAL_ALGORITHM: &str = "ed25519";
/// Line 1's `prev_hash`.
pub const GENESIS: &str = "genesis";
/// The key every `trace.jsonl` line ends with.
pub const PREV_HASH_KEY: &str = "prev_hash";
/// `chain` of a seal whose stream carries its own `prev_hash` on every line.
pub const CHAIN_INLINE: &str = "inline";
/// `chain` of a seal that lists every line's hash itself.
pub const CHAIN_LISTED: &str = "listed";

/// The trace's stream name, which is also its file name in the records directory.
pub const TRACE_STREAM: &str = crate::trace::TRACE_FILE_NAME;
/// The scoring hook's stream, relative to the records directory.
pub const EVAL_STREAM: &str = "hooks/eval.jsonl";
/// A delegated child's outcome, relative to its records directory. Sealed as one "line": the
/// whole file's bytes.
pub const COMPLETION_STREAM: &str = crate::delegation::COMPLETION_FILE;
/// Prefix of a conversation record's stream name: `conversation:<record>/<context-id>`.
pub const CONVERSATION_STREAM_PREFIX: &str = "conversation:";
/// `~/.murmur/conversations`, relative to the murmur home: where conversation records live.
pub const CONVERSATIONS_DIR: &str = crate::conversation::CONVERSATION_ROOT_DIR;
/// The `session_id` a conversation seal carries when no session rewrote it: `mur conversation`
/// truncating a record from the command line.
pub const CLI_SEALER: &str = "mur conversation";
/// `schema` of [`RecordReport`], and of `mur trace verify --json`.
pub const REPORT_SCHEMA: u32 = 1;

/// How long an append waits for another holder's `flock` before writing without it. A capsule
/// can read `trace.jsonl`, so it can hold a shared lock on it; a writer that waited forever would
/// hand it the session.
const FLOCK_WAIT: std::time::Duration = std::time::Duration::from_millis(500);

/// `"sha256:"` and the lowercase hex SHA-256 of `bytes`: one line without its newline.
#[must_use]
pub fn line_hash(bytes: &[u8]) -> String {
    format!("sha256:{}", murmur_artifact::sha256_hex(bytes))
}

/// The first 16 hex characters of the SHA-256 of a raw 32-byte public key.
#[must_use]
pub fn key_id_of(raw_public_key: &[u8]) -> String {
    murmur_artifact::sha256_hex(raw_public_key)[..16].to_string()
}

/// The UTF-8 bytes a seal's signature covers:
/// `murmur.seal/1\n<stream>\n<session_id>\n<lines>\n<head>\n`.
#[must_use]
pub fn signed_message(stream: &str, session_id: &str, lines: u64, head: &str) -> Vec<u8> {
    format!("{SEAL_FORMAT}\n{stream}\n{session_id}\n{lines}\n{head}\n").into_bytes()
}

/// A `"listed"` seal's head: `"sha256:"` and the hex SHA-256 of the line hashes joined by `\n`.
#[must_use]
pub fn listed_head(line_hashes: &[String]) -> String {
    line_hash(line_hashes.join("\n").as_bytes())
}

/// `<murmur home>/keys`.
#[must_use]
pub fn keys_dir(murmur_home: &Path) -> PathBuf {
    murmur_home.join(KEYS_DIR)
}

/// `<murmur home>/keys/record-seal.key`.
#[must_use]
pub fn private_key_path(murmur_home: &Path) -> PathBuf {
    keys_dir(murmur_home).join(PRIVATE_KEY_FILE)
}

/// `<murmur home>/keys/record-seal.pub`.
#[must_use]
pub fn public_key_path(murmur_home: &Path) -> PathBuf {
    keys_dir(murmur_home).join(PUBLIC_KEY_FILE)
}

/// The conversation record a session's `trace.jsonl.seal` points at.
#[must_use]
pub fn conversation_stream(record: &str, context_id: &str) -> String {
    format!("{CONVERSATION_STREAM_PREFIX}{record}/{context_id}")
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect()
}

// ── The host key ──────────────────────────────────────────────────────────────

/// A public key a seal is checked against, with the id seals name it by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicKey {
    raw: [u8; 32],
    key_id: String,
    path: Option<PathBuf>,
}

impl PublicKey {
    /// A key from its 32 raw bytes.
    #[must_use]
    pub fn from_raw(raw: [u8; 32]) -> Self {
        Self {
            key_id: key_id_of(&raw),
            raw,
            path: None,
        }
    }

    /// A key from 64 hex characters, surrounding whitespace ignored.
    pub fn from_hex(text: &str) -> Result<Self, String> {
        let bytes = unhex(text.trim())
            .filter(|bytes| bytes.len() == 32)
            .ok_or_else(|| "is not 64 hex characters naming an Ed25519 public key".to_string())?;
        let mut raw = [0u8; 32];
        raw.copy_from_slice(&bytes);
        Ok(Self::from_raw(raw))
    }

    /// The key in a `record-seal.pub` file.
    pub fn load(path: &Path) -> Result<Self, String> {
        let mut text = String::new();
        std::fs::File::open(path)
            .and_then(|file| file.take(1024).read_to_string(&mut text))
            .map_err(|error| format!("{}: {error}", path.display()))?;
        let mut key =
            Self::from_hex(&text).map_err(|reason| format!("{}: {reason}", path.display()))?;
        key.path = Some(path.to_path_buf());
        Ok(key)
    }

    /// First 16 hex characters of the SHA-256 of the raw key.
    #[must_use]
    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    /// The file the key was loaded from, when it was.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// The raw key as 64 lowercase hex characters.
    #[must_use]
    pub fn to_hex(&self) -> String {
        hex(&self.raw)
    }

    /// Whether `signature_hex` is a valid Ed25519 signature of `message` under this key.
    #[must_use]
    pub fn verifies(&self, message: &[u8], signature_hex: &str) -> bool {
        let Some(signature) = unhex(signature_hex).filter(|sig| sig.len() == 64) else {
            return false;
        };
        UnparsedPublicKey::new(&ED25519, &self.raw)
            .verify(message, &signature)
            .is_ok()
    }
}

/// The host's record-seal key pair. Only the `mur` process holds one.
///
/// `ring` keeps the expanded key in its own type, which does not zeroize on drop; the seed it was
/// built from is zeroized as soon as the key pair exists.
pub struct HostKey {
    pair: Ed25519KeyPair,
    public: PublicKey,
}

impl std::fmt::Debug for HostKey {
    /// Names the key by its id and nothing that signs.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostKey")
            .field("key_id", &self.public.key_id)
            .finish_non_exhaustive()
    }
}

/// One loaded key per murmur home: tests in one process run under different scratch homes.
static HOST_KEYS: LazyLock<Mutex<HashMap<PathBuf, Arc<HostKey>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

impl HostKey {
    /// The key under `murmur_home`, created on first use.
    ///
    /// Ensures `keys/` at `0700`; opens the seed without following a link and refuses a file that
    /// is not regular, not owned by this user, or carries any group or other bit; creates a
    /// missing one from fresh randomness under a temporary name linked into place, so two racing
    /// first launches end with one key; and writes `record-seal.pub` when it is missing or does
    /// not match. The `Err` names the file and the reason, never a byte of the key.
    pub fn load_or_create(murmur_home: &Path) -> Result<Arc<HostKey>, String> {
        let mut cache = HOST_KEYS.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(key) = cache.get(murmur_home) {
            return Ok(Arc::clone(key));
        }
        let key = Arc::new(Self::load_or_create_uncached(murmur_home)?);
        cache.insert(murmur_home.to_path_buf(), Arc::clone(&key));
        Ok(key)
    }

    fn load_or_create_uncached(murmur_home: &Path) -> Result<HostKey, String> {
        let dir = keys_dir(murmur_home);
        match std::fs::symlink_metadata(&dir) {
            Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
                return Err(format!("{}: is not a directory", dir.display()));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("{}: {error}", dir.display())),
        }
        crate::state_store::ensure_private_dir(&dir, KEYS_DIR_MODE)
            .map_err(|reason| format!("{}: {reason}", dir.display()))?;
        let private = dir.join(PRIVATE_KEY_FILE);
        let mut seed = match read_seed(&private)? {
            Some(seed) => seed,
            None => create_seed(&dir, &private)?,
        };
        let pair = Ed25519KeyPair::from_seed_unchecked(&seed);
        crate::mac_token::zeroize(&mut seed);
        let pair =
            pair.map_err(|_| format!("{}: does not hold a usable key", private.display()))?;
        let mut raw = [0u8; 32];
        raw.copy_from_slice(pair.public_key().as_ref());
        let mut public = PublicKey::from_raw(raw);
        let pub_path = dir.join(PUBLIC_KEY_FILE);
        let want = format!("{}\n", public.to_hex());
        let have = std::fs::read_to_string(&pub_path).ok();
        if have.as_deref() != Some(want.as_str()) || mode_bits(&pub_path) != Some(PUBLIC_KEY_MODE) {
            crate::retention::StagedRewrite::stage_with_mode(
                &pub_path,
                want.as_bytes(),
                Some(PUBLIC_KEY_MODE),
            )?
            .commit()?;
        }
        public.path = Some(pub_path);
        Ok(HostKey { pair, public })
    }

    #[must_use]
    pub fn public_key(&self) -> &PublicKey {
        &self.public
    }

    #[must_use]
    pub fn key_id(&self) -> &str {
        self.public.key_id()
    }

    /// 128 hex characters: the Ed25519 signature of `message`.
    #[must_use]
    pub fn sign_hex(&self, message: &[u8]) -> String {
        hex(self.pair.sign(message).as_ref())
    }
}

fn mode_bits(path: &Path) -> Option<u32> {
    std::fs::symlink_metadata(path)
        .ok()
        .map(|meta| meta.permissions().mode() & 0o777)
}

/// The seed in `path`, `None` when there is no file, `Err` for a file that may not be used.
fn read_seed(path: &Path) -> Result<Option<[u8; 32]>, String> {
    let located = |reason: String| format!("{}: {reason}", path.display());
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) if error.raw_os_error() == Some(libc::ELOOP) => {
            return Err(located(
                "is a symbolic link; the record-seal key must be a regular file".into(),
            ));
        }
        Err(error) => return Err(located(error.to_string())),
    };
    let meta = file
        .metadata()
        .map_err(|error| located(error.to_string()))?;
    if !meta.is_file() {
        return Err(located("is not a regular file".into()));
    }
    if meta.uid() != crate::murmur_home::current_euid() {
        return Err(located(format!(
            "is owned by uid {}, not by this user",
            meta.uid()
        )));
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(located(format!(
            "has mode {mode:04o}; the record-seal key must be 0600, readable by its owner alone"
        )));
    }
    let mut text = Vec::with_capacity(70);
    file.take(256)
        .read_to_end(&mut text)
        .map_err(|error| located(error.to_string()))?;
    let parsed = std::str::from_utf8(&text)
        .ok()
        .and_then(|text| unhex(text.trim()))
        .filter(|bytes| bytes.len() == 32);
    crate::mac_token::zeroize(&mut text);
    let Some(mut bytes) = parsed else {
        return Err(located("does not hold 64 hex characters".into()));
    };
    let mut seed = [0u8; 32];
    seed.copy_from_slice(&bytes);
    crate::mac_token::zeroize(&mut bytes);
    Ok(Some(seed))
}

/// A fresh seed written to a temporary name and linked into place without replacing an existing
/// file. A launch that loses the race reads the winner's key.
fn create_seed(dir: &Path, private: &Path) -> Result<[u8; 32], String> {
    let located = |path: &Path, reason: String| format!("{}: {reason}", path.display());
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed)
        .map_err(|error| format!("failed to generate the record-seal key: {error}"))?;
    let mut nonce = [0u8; 8];
    let _ = getrandom::fill(&mut nonce);
    let temp = dir.join(format!(
        ".{PRIVATE_KEY_FILE}.{}.{}",
        std::process::id(),
        hex(&nonce)
    ));
    let mut text = format!("{}\n", hex(&seed)).into_bytes();
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(PRIVATE_KEY_MODE)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&temp)
        .and_then(|mut file| {
            file.set_permissions(std::fs::Permissions::from_mode(PRIVATE_KEY_MODE))?;
            file.write_all(&text)?;
            file.sync_all()
        });
    crate::mac_token::zeroize(&mut text);
    if let Err(error) = written {
        let _ = std::fs::remove_file(&temp);
        crate::mac_token::zeroize(&mut seed);
        return Err(located(&temp, error.to_string()));
    }
    let linked = std::fs::hard_link(&temp, private);
    let _ = std::fs::remove_file(&temp);
    match linked {
        Ok(()) => Ok(seed),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            crate::mac_token::zeroize(&mut seed);
            read_seed(private)?
                .ok_or_else(|| located(private, "vanished after its creation".into()))
        }
        Err(error) => {
            crate::mac_token::zeroize(&mut seed);
            Err(located(private, error.to_string()))
        }
    }
}

// ── Where seals are signed ────────────────────────────────────────────────────

#[cfg(test)]
thread_local! {
    static TEST_SEAL_HOME: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Seals written by this test thread use `home` as the murmur home; `None` turns sealing off.
#[cfg(test)]
pub(crate) fn set_test_seal_home(home: Option<PathBuf>) {
    TEST_SEAL_HOME.with(|cell| *cell.borrow_mut() = home);
}

/// The murmur home whose key signs this process's seals, or `None` when sealing is off: a unit
/// test of this crate outside a scratch home, so no test writes a key into a developer's real
/// `~/.murmur`.
fn seal_home() -> Option<Result<PathBuf, String>> {
    #[cfg(test)]
    {
        if let Some(home) = TEST_SEAL_HOME.with(|cell| cell.borrow().clone()) {
            return Some(Ok(home));
        }
        if !crate::murmur_home::in_scratch_home() {
            return None;
        }
    }
    Some(crate::murmur_home::ensure_murmur_home())
}

/// Sessions (by id) whose missing key has already been reported: one stderr line per session.
static KEY_FAILURES_REPORTED: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// This process's host key, or `None` when sealing is off or the key is unusable. An unusable key
/// is reported on stderr once per `session_id`; the session keeps running and its record reads
/// as unsealed.
fn host_key_for(session_id: &str) -> Option<Arc<HostKey>> {
    let home = seal_home()?;
    let loaded = home.and_then(|home| HostKey::load_or_create(&home));
    match loaded {
        Ok(key) => Some(key),
        Err(reason) => {
            let first = KEY_FAILURES_REPORTED
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(session_id.to_string());
            if first {
                crate::runtime_err!(
                    "[capsule-runtime] the record-seal key could not be used ({reason}); session \
                     {session_id}'s records are not sealed and read as unsealed"
                );
            }
            None
        }
    }
}

/// The key id this process would seal with, without reporting a failure.
#[must_use]
pub fn host_key_id() -> Option<String> {
    let home = seal_home()?.ok()?;
    HostKey::load_or_create(&home)
        .ok()
        .map(|key| key.key_id().to_string())
}

// ── The seal file ─────────────────────────────────────────────────────────────

/// `trace.jsonl.seal`'s pointer to the conversation record its session appended to. Not signed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationPointer {
    pub record: String,
    pub context_id: String,
}

/// One `<stream>.seal` file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Seal {
    /// [`SEAL_FORMAT`].
    pub format: String,
    pub stream: String,
    /// [`CHAIN_INLINE`] or [`CHAIN_LISTED`].
    pub chain: String,
    pub session_id: String,
    pub lines: u64,
    /// [`CHAIN_LISTED`] only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_hashes: Option<Vec<String>>,
    pub head: String,
    /// Milliseconds since the epoch. Not signed.
    pub sealed_at: u64,
    pub key_id: String,
    pub algorithm: String,
    pub signature: String,
    /// `trace.jsonl.seal` only. Not signed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation: Option<ConversationPointer>,
}

impl Seal {
    #[allow(clippy::too_many_arguments)]
    fn sign(
        key: &HostKey,
        stream: &str,
        chain: &str,
        session_id: &str,
        lines: u64,
        head: String,
        line_hashes: Option<Vec<String>>,
        conversation: Option<ConversationPointer>,
    ) -> Self {
        let signature = key.sign_hex(&signed_message(stream, session_id, lines, &head));
        Self {
            format: SEAL_FORMAT.to_string(),
            stream: stream.to_string(),
            chain: chain.to_string(),
            session_id: session_id.to_string(),
            lines,
            line_hashes,
            head,
            sealed_at: crate::trace::timestamp_ms(),
            key_id: key.key_id().to_string(),
            algorithm: SEAL_ALGORITHM.to_string(),
            signature,
            conversation,
        }
    }

    /// Whether the signature verifies under `key`.
    #[must_use]
    pub fn signature_valid(&self, key: &PublicKey) -> bool {
        self.algorithm == SEAL_ALGORITHM
            && key.verifies(
                &signed_message(&self.stream, &self.session_id, self.lines, &self.head),
                &self.signature,
            )
    }
}

/// The seal beside `stream_file` in `root`, `Ok(None)` when there is none.
pub fn read_seal(root: &WorkdirRoot, stream_file: &str) -> Result<Option<Seal>, String> {
    let name = format!("{stream_file}{SEAL_SUFFIX}");
    match root.read_file(&name, 1 << 26) {
        Ok(Some(bytes)) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| format!("{}: {error}", root.display_path(&name))),
        Ok(None) => Ok(None),
        Err(error) if error.is_not_found() => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn write_seal(root: &WorkdirRoot, stream_file: &str, seal: &Seal, mode: u32) -> Result<(), String> {
    let mut body = serde_json::to_string(seal).map_err(|error| error.to_string())?;
    body.push('\n');
    root.write_file(format!("{stream_file}{SEAL_SUFFIX}"), body.as_bytes(), mode)
        .map_err(|error| error.to_string())
}

// ── The inline chain ──────────────────────────────────────────────────────────

/// A chain's head after its last line: [`GENESIS`] and `0` for an empty file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChainHead {
    pub head: String,
    pub lines: u64,
}

/// What a session-end seal hands the OTel `capsule.session` span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedHead {
    /// The `session_end` line's hash.
    pub head: String,
    /// Its line number.
    pub lines: u64,
    /// `None` when no key signed it.
    pub key_id: Option<String>,
}

#[derive(Debug)]
struct ChainState {
    head: String,
    lines: u64,
    /// The file length after this state's last write, or `None` before the state is read.
    len: Option<u64>,
    /// The hash of an unterminated last line, which the next append terminates first.
    torn: Option<String>,
    /// Set once a `flock` wait ran out: something else holds the lock, so later appends try it
    /// once and never wait again.
    lock_contended: bool,
}

/// One `trace.jsonl`'s chain, shared by every appender holding the same file.
///
/// Every append locks the state, takes the file's `flock`, re-reads the real last line when the
/// file's length is not what this state last wrote (another process appended, or this is the
/// first write), terminates a torn last line, and writes its line with `"prev_hash"` in one
/// `write_all`.
pub struct InlineChain {
    file: std::fs::File,
    /// The directory and name the file was opened by, for reading it back.
    root: WorkdirRoot,
    rel: String,
    state: Mutex<ChainState>,
}

impl std::fmt::Debug for InlineChain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InlineChain")
            .field("path", &self.root.host_path(&self.rel))
            .finish_non_exhaustive()
    }
}

/// Every live chain by the `(st_dev, st_ino)` of its file. Weak: a chain goes when its last
/// appender does, and while it lives it holds the file open, so the inode cannot be reused.
static CHAINS: LazyLock<ChainRegistry> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// [`CHAINS`]' type: each live chain, weakly, by its file's `(st_dev, st_ino)`.
type ChainRegistry = Mutex<HashMap<(u64, u64), Weak<InlineChain>>>;

impl InlineChain {
    /// The chain of `rel` in `root`, opened for appending without following a link, and shared
    /// with every other holder of the same file in this process.
    pub fn open(root: &WorkdirRoot, rel: &str) -> std::io::Result<Arc<Self>> {
        let file = root
            .open_append(rel, DEFAULT_FILE_MODE)
            .map_err(std::io::Error::from)?;
        let meta = file.metadata()?;
        let identity = (meta.dev(), meta.ino());
        let mut chains = CHAINS.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(chain) = chains.get(&identity).and_then(Weak::upgrade) {
            return Ok(chain);
        }
        chains.retain(|_, weak| weak.strong_count() > 0);
        let chain = Arc::new(Self {
            file,
            root: root.clone(),
            rel: rel.to_string(),
            state: Mutex::new(ChainState {
                head: GENESIS.to_string(),
                lines: 0,
                len: None,
                torn: None,
                lock_contended: false,
            }),
        });
        chains.insert(identity, Arc::downgrade(&chain));
        Ok(chain)
    }

    /// A chain writing to `file` while reading `rel` in `root` back, outside the registry: a
    /// test's unwritable sink.
    #[cfg(test)]
    pub(crate) fn over_file_for_test(
        file: std::fs::File,
        root: &WorkdirRoot,
        rel: &str,
    ) -> Arc<Self> {
        Arc::new(Self {
            file,
            root: root.clone(),
            rel: rel.to_string(),
            state: Mutex::new(ChainState {
                head: GENESIS.to_string(),
                lines: 0,
                len: None,
                torn: None,
                lock_contended: false,
            }),
        })
    }

    /// Appends `event` as one line ending in `"prev_hash"`.
    pub fn append_event(&self, event: &impl Serialize) -> std::io::Result<()> {
        let json = serde_json::to_string(event).map_err(std::io::Error::other)?;
        self.append_json(&json)
    }

    /// Appends one serialized JSON object as one line ending in `"prev_hash"`.
    pub fn append_json(&self, json: &str) -> std::io::Result<()> {
        let body = json.trim_end();
        if !body.starts_with('{') || !body.ends_with('}') {
            return Err(std::io::Error::other("a trace line must be a JSON object"));
        }
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let _lock = FileLock::take(&self.file, &mut state.lock_contended);
        self.sync(&mut state)?;
        let mut line = Vec::with_capacity(body.len() + 90);
        let mut lines = state.lines;
        let prev = match state.torn.take() {
            Some(torn) => {
                line.push(b'\n');
                lines += 1;
                torn
            }
            None => state.head.clone(),
        };
        let open = &body[..body.len() - 1];
        line.extend_from_slice(open.as_bytes());
        if open.trim() != "{" {
            line.push(b',');
        }
        line.extend_from_slice(format!("\"{PREV_HASH_KEY}\":\"{prev}\"}}").as_bytes());
        let hash = line_hash(line.strip_prefix(b"\n").unwrap_or(&line));
        line.push(b'\n');
        let written = (&self.file).write_all(&line);
        if let Err(error) = written {
            // What landed is unknown; the next append reads the file back.
            state.len = None;
            return Err(error);
        }
        state.head = hash;
        state.lines = lines + 1;
        state.len = state.len.map(|len| len + line.len() as u64);
        Ok(())
    }

    /// The head and line count as the file holds them now.
    pub fn head(&self) -> std::io::Result<ChainHead> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let _lock = FileLock::take(&self.file, &mut state.lock_contended);
        self.sync(&mut state)?;
        Ok(ChainHead {
            head: state.head.clone(),
            lines: state.lines,
        })
    }

    /// Re-reads the head when the file's length is not the one this state wrote.
    fn sync(&self, state: &mut ChainState) -> std::io::Result<()> {
        let len = self.file.metadata()?.len();
        if state.len == Some(len) {
            return Ok(());
        }
        let recovered = recover_head(&self.root, &self.rel, len)?;
        state.head = recovered.head;
        state.lines = recovered.lines;
        state.torn = recovered.torn;
        state.len = Some(len);
        Ok(())
    }
}

struct Recovered {
    head: String,
    lines: u64,
    torn: Option<String>,
}

/// The head of the first `len` bytes of `rel`: the last complete line's hash and how many there
/// are, and the hash of an unterminated tail.
///
/// Reads the whole file rather than its tail: the seal names the line count as well as the head,
/// and recovery happens once per open and once per append by another process, not per line.
fn recover_head(root: &WorkdirRoot, rel: &str, len: u64) -> std::io::Result<Recovered> {
    if len == 0 {
        return Ok(Recovered {
            head: GENESIS.to_string(),
            lines: 0,
            torn: None,
        });
    }
    let bytes = root
        .read_file(rel, u64::MAX)
        .map_err(std::io::Error::other)?
        .unwrap_or_default();
    let bytes = &bytes[..bytes.len().min(usize::try_from(len).unwrap_or(usize::MAX))];
    let split = split_lines(bytes);
    let head = split
        .complete
        .last()
        .map_or_else(|| GENESIS.to_string(), |line| line_hash(line));
    Ok(Recovered {
        head,
        lines: split.complete.len() as u64,
        torn: split.tail.map(line_hash),
    })
}

/// A held exclusive `flock`, released on drop. Taken without blocking for at most
/// [`FLOCK_WAIT`]; past that the write goes ahead unlocked.
struct FileLock<'a>(Option<&'a std::fs::File>);

impl<'a> FileLock<'a> {
    /// `contended` is the chain's record of an earlier wait that ran out: with it set, the lock
    /// is tried once and not waited for, so a holder that never lets go costs one wait, not one
    /// per line.
    fn take(file: &'a std::fs::File, contended: &mut bool) -> Self {
        use rustix::fs::{flock, FlockOperation};
        let started = std::time::Instant::now();
        loop {
            match flock(file, FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => return Self(Some(file)),
                Err(rustix::io::Errno::WOULDBLOCK)
                    if !*contended && started.elapsed() < FLOCK_WAIT =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                Err(rustix::io::Errno::WOULDBLOCK) => {
                    *contended = true;
                    return Self(None);
                }
                Err(_) => return Self(None),
            }
        }
    }
}

impl Drop for FileLock<'_> {
    fn drop(&mut self) {
        if let Some(file) = self.0 {
            let _ = rustix::fs::flock(file, rustix::fs::FlockOperation::Unlock);
        }
    }
}

struct Lines<'a> {
    /// Every `\n`-terminated line, without its newline.
    complete: Vec<&'a [u8]>,
    /// Bytes after the last newline, when there are any.
    tail: Option<&'a [u8]>,
}

fn split_lines(bytes: &[u8]) -> Lines<'_> {
    let mut complete = Vec::new();
    let mut start = 0;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            complete.push(&bytes[start..index]);
            start = index + 1;
        }
    }
    let tail = (start < bytes.len()).then(|| &bytes[start..]);
    Lines { complete, tail }
}

// ── Sealing ───────────────────────────────────────────────────────────────────

/// Seals `stream` (an inline-chained file in `records_root`, normally [`TRACE_STREAM`]) at its
/// current head, signed for `session_id`.
///
/// `conversation` names the record the session appended to; `None` keeps the pointer an earlier
/// seal of the same file carried. Returns the head sealed, with `key_id: None` when no key was
/// usable — the stream then has no new seal and reads as unsealed. `Err` only when the stream
/// could not be read.
pub fn seal_inline(
    records_root: &WorkdirRoot,
    stream: &str,
    session_id: &str,
    conversation: Option<ConversationPointer>,
) -> Result<SealedHead, String> {
    let chain = InlineChain::open(records_root, stream).map_err(|error| error.to_string())?;
    let head = chain.head().map_err(|error| error.to_string())?;
    let Some(key) = host_key_for(session_id) else {
        return Ok(SealedHead {
            head: head.head,
            lines: head.lines,
            key_id: None,
        });
    };
    let conversation = conversation.or_else(|| {
        read_seal(records_root, stream)
            .ok()
            .flatten()
            .and_then(|seal| seal.conversation)
    });
    let seal = Seal::sign(
        &key,
        stream,
        CHAIN_INLINE,
        session_id,
        head.lines,
        head.head.clone(),
        None,
        conversation,
    );
    if let Err(reason) = write_seal(records_root, stream, &seal, DEFAULT_FILE_MODE) {
        crate::runtime_err!(
            "[capsule-runtime] could not write {}{SEAL_SUFFIX} ({reason}); session {session_id}'s \
             trace reads as unsealed",
            records_root.display_path(stream)
        );
        return Ok(SealedHead {
            head: head.head,
            lines: head.lines,
            key_id: None,
        });
    }
    Ok(SealedHead {
        head: head.head,
        lines: head.lines,
        key_id: Some(key.key_id().to_string()),
    })
}

/// Seals `stream_file` in `dir_root` as a listed chain: every complete line's hash in order (the
/// whole file as one line when `whole_file`, as for `completion.json`), written to
/// `<stream_file>.seal` at `mode`. A failure is reported on stderr and leaves the stream reading
/// as unsealed; it never fails the caller.
pub fn seal_listed(
    dir_root: &WorkdirRoot,
    stream_file: &str,
    stream_name: &str,
    session_id: &str,
    whole_file: bool,
    mode: u32,
) {
    let Some(key) = host_key_for(session_id) else {
        return;
    };
    let bytes = match dir_root.read_file(stream_file, u64::MAX) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return,
        Err(error) if error.is_not_found() => return,
        Err(error) => {
            crate::runtime_err!(
                "[capsule-runtime] could not read {error} to seal it; it reads as unsealed"
            );
            return;
        }
    };
    let line_hashes = listed_line_hashes(&bytes, whole_file);
    let seal = Seal::sign(
        &key,
        stream_name,
        CHAIN_LISTED,
        session_id,
        line_hashes.len() as u64,
        listed_head(&line_hashes),
        Some(line_hashes),
        None,
    );
    if let Err(reason) = write_seal(dir_root, stream_file, &seal, mode) {
        crate::runtime_err!(
            "[capsule-runtime] could not write {}{SEAL_SUFFIX} ({reason}); it reads as unsealed",
            dir_root.display_path(stream_file)
        );
    }
}

/// Every line's hash, an unterminated last line included; the whole file's when `whole_file`.
fn listed_line_hashes(bytes: &[u8], whole_file: bool) -> Vec<String> {
    if whole_file {
        return vec![line_hash(bytes)];
    }
    let split = split_lines(bytes);
    split
        .complete
        .iter()
        .copied()
        .chain(split.tail)
        .map(line_hash)
        .collect()
}

/// `<record>` and `<context-id>` of a conversation record directory
/// `~/.murmur/conversations/<record>/<context-id>`.
fn conversation_names(dir: &Path) -> Option<(String, String)> {
    let context_id = dir.file_name()?.to_str()?.to_string();
    let record = dir.parent()?.file_name()?.to_str()?.to_string();
    Some((record, context_id))
}

/// Seals the conversation record in `dir` (`.../<record>/<context-id>/conversation.jsonl`) for
/// `session_id`. A failure is reported and leaves the record reading as unsealed.
pub fn seal_conversation(dir: &Path, session_id: &str) {
    let Some((record, context_id)) = conversation_names(dir) else {
        return;
    };
    let root = match WorkdirRoot::open(dir) {
        Ok(root) => root,
        Err(error) if error.is_not_found() => return,
        Err(error) => {
            crate::runtime_err!(
                "[capsule-runtime] could not open {} to seal its conversation record ({error})",
                dir.display()
            );
            return;
        }
    };
    seal_listed(
        &root,
        crate::conversation::RECORD_FILE_NAME,
        &conversation_stream(&record, &context_id),
        session_id,
        false,
        crate::murmur_home::PRIVATE_FILE_MODE,
    );
}

/// Conversation record directories each session appended to, by the session's records directory.
static APPENDED_CONVERSATIONS: LazyLock<Mutex<HashMap<PathBuf, Vec<PathBuf>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Notes that the session whose records directory is `records_dir` appended to the conversation
/// record in `conversation_dir`, so [`seal_session_teardown`] seals it.
pub fn note_conversation_append(records_dir: &Path, conversation_dir: &Path) {
    let mut noted = APPENDED_CONVERSATIONS
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let dirs = noted.entry(records_dir.to_path_buf()).or_default();
    if !dirs.iter().any(|dir| dir == conversation_dir) {
        dirs.push(conversation_dir.to_path_buf());
    }
}

/// The first conversation record the session in `records_dir` appended to.
#[must_use]
pub fn appended_conversation(records_dir: &Path) -> Option<ConversationPointer> {
    let noted = APPENDED_CONVERSATIONS
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let dir = noted.get(records_dir)?.first()?;
    let (record, context_id) = conversation_names(dir)?;
    Some(ConversationPointer { record, context_id })
}

/// The seal a session writes once `session_end` is on disk: `trace.jsonl` at its head, naming
/// the conversation record the session appended to. Returns the head for the OTel session span.
pub fn seal_session_end(records_root: &WorkdirRoot, session_id: &str) -> Option<SealedHead> {
    match seal_inline(
        records_root,
        TRACE_STREAM,
        session_id,
        appended_conversation(records_root.path()),
    ) {
        Ok(head) => Some(head),
        Err(reason) => {
            crate::runtime_err!(
                "[capsule-runtime] could not seal {} ({reason}); it reads as unsealed",
                records_root.display_path(TRACE_STREAM)
            );
            None
        }
    }
}

/// The seals a session writes at teardown, once its door is shut and every hook has drained:
/// `trace.jsonl` again when a line landed after its last seal, `hooks/eval.jsonl` when it
/// exists, and every conversation record the session appended to.
pub fn seal_session_teardown(records_root: &WorkdirRoot, session_id: &str) {
    if records_root
        .entry_kind(TRACE_STREAM)
        .ok()
        .flatten()
        .is_some()
    {
        let sealed = read_seal(records_root, TRACE_STREAM).ok().flatten();
        let current = InlineChain::open(records_root, TRACE_STREAM)
            .and_then(|chain| chain.head())
            .ok();
        let stale = match (&sealed, &current) {
            (Some(seal), Some(head)) => seal.lines != head.lines || seal.head != head.head,
            _ => true,
        };
        if stale {
            seal_session_end(records_root, session_id);
        }
    }
    if records_root
        .entry_kind(EVAL_STREAM)
        .ok()
        .flatten()
        .is_some()
    {
        seal_listed(
            records_root,
            EVAL_STREAM,
            EVAL_STREAM,
            session_id,
            false,
            DEFAULT_FILE_MODE,
        );
    }
    let dirs = APPENDED_CONVERSATIONS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(records_root.path())
        .cloned()
        .unwrap_or_default();
    for dir in dirs {
        seal_conversation(&dir, session_id);
    }
}

// ── Verifying ─────────────────────────────────────────────────────────────────

/// How a stream, or a whole record, reads. Ordered best to worst; the worst wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    /// The stream does not exist; it does not count toward the record's verdict.
    Absent,
    /// Chain intact, a seal covers every line, signature valid under the given key.
    Verified,
    /// Written before records were sealed, no public key, or sealed by another key.
    Unverifiable,
    /// Chain intact but no seal covers exactly the lines the stream holds.
    Unsealed,
    /// A line, the chain, or the seal no longer matches.
    Changed,
}

impl Verdict {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::Verified => "verified",
            Self::Unverifiable => "unverifiable",
            Self::Unsealed => "unsealed",
            Self::Changed => "changed",
        }
    }
}

/// One stream's verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamReport {
    pub stream: String,
    pub verdict: Verdict,
    /// Complete lines the stream holds (`1` for `completion.json`).
    pub lines: u64,
    /// Lines the seal covers, `None` with no seal.
    pub sealed_lines: Option<u64>,
    /// The last line through which the chain, or the seal's listed hashes, hold.
    pub chain_intact_through: Option<u64>,
    /// `changed` only: the first line that no longer hashes to what was recorded.
    pub first_bad_line: Option<u64>,
    /// The seal's key id.
    pub key_id: Option<String>,
    pub detail: String,
    /// Findings that change nothing, such as a torn line the chain still covers.
    pub notes: Vec<String>,
}

impl StreamReport {
    fn new(stream: &str, verdict: Verdict, lines: u64, detail: impl Into<String>) -> Self {
        Self {
            stream: stream.to_string(),
            verdict,
            lines,
            sealed_lines: None,
            chain_intact_through: None,
            first_bad_line: None,
            key_id: None,
            detail: detail.into(),
            notes: Vec::new(),
        }
    }

    fn absent(stream: &str) -> Self {
        Self::new(stream, Verdict::Absent, 0, "no such stream")
    }
}

/// The key a record was checked against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicKeyReport {
    pub path: Option<String>,
    pub key_id: Option<String>,
}

/// A whole records directory's verdict: `mur trace verify --json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordReport {
    /// [`REPORT_SCHEMA`].
    pub schema: u32,
    pub session_id: Option<String>,
    pub records_dir: String,
    pub public_key: PublicKeyReport,
    pub verdict: Verdict,
    pub streams: Vec<StreamReport>,
}

/// The worst of `verdicts`, ignoring [`Verdict::Absent`]; `Absent` when every one is.
#[must_use]
pub fn worst(verdicts: impl IntoIterator<Item = Verdict>) -> Verdict {
    verdicts.into_iter().max().unwrap_or(Verdict::Absent)
}

/// The `prev_hash` a line ends with, when its last key is `"prev_hash"` with a string value.
fn trailing_prev_hash(line: &[u8]) -> Option<&str> {
    let text = std::str::from_utf8(line).ok()?;
    let body = text.strip_suffix("\"}")?;
    let marker = format!("\"{PREV_HASH_KEY}\":\"");
    let at = body.rfind(&marker)?;
    let before = body[..at].trim_end();
    if !(before.ends_with(',') || before.ends_with('{')) {
        return None;
    }
    let value = &body[at + marker.len()..];
    (!value.contains('"') && !value.contains('\\')).then_some(value)
}

fn is_json_object(line: &[u8]) -> bool {
    line.first() == Some(&b'{') && serde_json::from_slice::<serde::de::IgnoredAny>(line).is_ok()
}

/// Checks a seal's key and signature: `Err` carries the verdict and detail when it does not hold.
fn check_seal_key(
    seal: &Seal,
    key: Option<&PublicKey>,
    stream: &str,
) -> Result<(), (Verdict, String)> {
    if seal.format != SEAL_FORMAT {
        return Err((
            Verdict::Unverifiable,
            format!("the seal's format is '{}', not {SEAL_FORMAT}", seal.format),
        ));
    }
    let Some(key) = key else {
        return Err((
            Verdict::Unverifiable,
            format!(
                "no public key is available to check the seal (sealed by key {})",
                seal.key_id
            ),
        ));
    };
    if seal.key_id != key.key_id() {
        return Err((
            Verdict::Unverifiable,
            format!(
                "sealed by key {}, not by the key given ({})",
                seal.key_id,
                key.key_id()
            ),
        ));
    }
    if !seal.signature_valid(key) {
        return Err((
            Verdict::Changed,
            format!(
                "the seal's signature does not verify under key {}",
                key.key_id()
            ),
        ));
    }
    if seal.stream != stream {
        return Err((
            Verdict::Changed,
            format!("the seal names stream '{}', not '{stream}'", seal.stream),
        ));
    }
    Ok(())
}

/// Checks an inline-chained stream's bytes against its seal.
///
/// `expected_session` is the session the record belongs to; a seal signed for another session is
/// `changed`.
#[must_use]
pub fn verify_inline(
    stream: &str,
    bytes: &[u8],
    seal: Option<&Seal>,
    key: Option<&PublicKey>,
    expected_session: Option<&str>,
) -> StreamReport {
    let split = split_lines(bytes);
    let total = split.complete.len() as u64;
    let mut report = StreamReport::new(stream, Verdict::Verified, total, "");
    report.sealed_lines = seal.map(|seal| seal.lines);
    report.key_id = seal.map(|seal| seal.key_id.clone());

    let mut hashes: Vec<String> = Vec::with_capacity(split.complete.len());
    let mut intact = 0u64;
    for (index, line) in split.complete.iter().enumerate() {
        let number = index as u64 + 1;
        let hash = line_hash(line);
        let expected = if index == 0 {
            GENESIS.to_string()
        } else {
            hashes[index - 1].clone()
        };
        match trailing_prev_hash(line) {
            Some(prev) if prev == expected => {}
            Some(_) if index == 0 => {
                report.verdict = Verdict::Changed;
                report.first_bad_line = Some(1);
                report.detail = "line 1 changed: its prev_hash is not \"genesis\"".to_string();
                report.chain_intact_through = Some(0);
                return report;
            }
            Some(_) => {
                report.verdict = Verdict::Changed;
                report.first_bad_line = Some(number - 1);
                report.detail = format!(
                    "line {} changed, or line {number}'s prev_hash did: line {number} names a \
                     hash line {} no longer has",
                    number - 1,
                    number - 1
                );
                report.chain_intact_through = Some(number.saturating_sub(2));
                return report;
            }
            None if !is_json_object(line) => {
                report.notes.push(format!(
                    "line {number} is not a JSON object (a writer stopped mid-line); the chain \
                     continues past it"
                ));
            }
            None if index == 0 => {
                report.verdict = Verdict::Unverifiable;
                report.detail =
                    "written before records were sealed: line 1 carries no prev_hash".to_string();
                return report;
            }
            None => {
                report.verdict = Verdict::Changed;
                report.first_bad_line = Some(number);
                report.detail = format!("line {number} changed: it carries no prev_hash");
                report.chain_intact_through = Some(number - 1);
                return report;
            }
        }
        hashes.push(hash);
        intact = number;
    }
    report.chain_intact_through = Some(intact);
    if split.tail.is_some() {
        report.notes.push(format!(
            "an incomplete line follows line {total} (a writer stopped mid-line)"
        ));
    }

    let Some(seal) = seal else {
        report.verdict = Verdict::Unsealed;
        report.detail = format!("no seal; the chain is intact through line {intact}");
        return report;
    };
    if let Err((verdict, detail)) = check_seal_key(seal, key, stream) {
        report.verdict = verdict;
        report.detail = detail;
        return report;
    }
    if let (Some(expected), false) = (expected_session, seal.session_id.is_empty()) {
        if seal.session_id != expected {
            report.verdict = Verdict::Changed;
            report.detail = format!(
                "the seal was signed for session {}, not {expected}",
                seal.session_id
            );
            return report;
        }
    }
    if seal.chain != CHAIN_INLINE {
        report.verdict = Verdict::Changed;
        report.detail = format!("the seal's chain is '{}', not inline", seal.chain);
        return report;
    }
    let sealed = seal.lines;
    if sealed > total {
        report.verdict = Verdict::Unsealed;
        report.detail = format!(
            "the seal covers {sealed} lines but the stream holds {total}: lines were cut off; \
             the chain is intact through line {intact}"
        );
        return report;
    }
    let head_at = if sealed == 0 {
        GENESIS.to_string()
    } else {
        hashes[usize::try_from(sealed - 1).unwrap_or(0)].clone()
    };
    if head_at != seal.head {
        report.verdict = Verdict::Changed;
        report.first_bad_line = Some(sealed.max(1));
        report.detail = format!(
            "line {sealed} no longer hashes to the seal's head: line {sealed} changed, or the \
             chain was rewritten before line {sealed}"
        );
        return report;
    }
    if sealed < total || split.tail.is_some() {
        report.verdict = Verdict::Unsealed;
        report.detail = format!(
            "the seal covers lines 1-{sealed}; {} line(s) were appended after it; the chain is \
             intact through line {intact}",
            total - sealed + u64::from(split.tail.is_some())
        );
        return report;
    }
    report.detail = format!("{total} lines sealed by key {}", seal.key_id);
    report
}

/// Checks a listed-chain stream's bytes against its seal. `whole_file` treats the file as one
/// line, as `completion.json` is sealed.
#[must_use]
pub fn verify_listed(
    stream: &str,
    bytes: &[u8],
    whole_file: bool,
    seal: Option<&Seal>,
    key: Option<&PublicKey>,
    expected_session: Option<&str>,
) -> StreamReport {
    let hashes = listed_line_hashes(bytes, whole_file);
    let total = hashes.len() as u64;
    let mut report = StreamReport::new(stream, Verdict::Verified, total, "");
    let Some(seal) = seal else {
        report.verdict = Verdict::Unsealed;
        report.detail = format!("no seal covers its {total} line(s)");
        return report;
    };
    report.sealed_lines = Some(seal.lines);
    report.key_id = Some(seal.key_id.clone());
    if let Err((verdict, detail)) = check_seal_key(seal, key, stream) {
        report.verdict = verdict;
        report.detail = detail;
        return report;
    }
    if let Some(expected) = expected_session {
        if seal.session_id != expected {
            report.verdict = Verdict::Changed;
            report.detail = format!(
                "the seal was signed for session {}, not {expected}",
                seal.session_id
            );
            return report;
        }
    }
    let listed = seal.line_hashes.clone().unwrap_or_default();
    if seal.chain != CHAIN_LISTED
        || listed.len() as u64 != seal.lines
        || listed_head(&listed) != seal.head
    {
        report.verdict = Verdict::Changed;
        report.detail = "the seal's listed hashes do not match its signed head".to_string();
        return report;
    }
    let mut intact = 0u64;
    for (index, (have, want)) in hashes.iter().zip(&listed).enumerate() {
        if have != want {
            let number = index as u64 + 1;
            report.verdict = Verdict::Changed;
            report.first_bad_line = Some(number);
            report.chain_intact_through = Some(intact);
            report.detail = if whole_file {
                format!("{stream} changed: its bytes no longer hash to what the seal recorded")
            } else {
                format!("line {number} changed: it no longer hashes to what the seal recorded")
            };
            return report;
        }
        intact = index as u64 + 1;
    }
    report.chain_intact_through = Some(intact);
    if seal.lines > total {
        report.verdict = Verdict::Unsealed;
        report.detail = format!(
            "the seal covers {} lines but the stream holds {total}: lines were cut off",
            seal.lines
        );
    } else if seal.lines < total {
        report.verdict = Verdict::Unsealed;
        report.detail = format!(
            "the seal covers lines 1-{}; {} line(s) were appended after it",
            seal.lines,
            total - seal.lines
        );
    } else {
        report.detail = format!("{total} line(s) sealed by key {}", seal.key_id);
    }
    report
}

/// The `session_id` line 1 of a trace names.
fn trace_session_id(bytes: &[u8]) -> Option<String> {
    let first = split_lines(bytes).complete.into_iter().next()?;
    let value: serde_json::Value = serde_json::from_slice(first).ok()?;
    value.get("session_id")?.as_str().map(str::to_string)
}

fn single_segment(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/') && !name.contains('\0')
}

/// Whether a running-session record for `session_id` names a live process.
fn session_is_live(session_id: &str) -> bool {
    crate::running::list().is_ok_and(|records| {
        records.iter().any(|record| {
            record.session_id == session_id && crate::running::verify(record).is_running()
        })
    })
}

fn read_stream(root: &WorkdirRoot, rel: &str) -> Result<Option<Vec<u8>>, String> {
    match root.read_file(rel, u64::MAX) {
        Ok(found) => Ok(found),
        Err(error) if error.is_not_found() => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

/// Checks every stream of the records directory `records_dir` with `public_key` alone.
///
/// `trace.jsonl`, `hooks/eval.jsonl` and `completion.json` are read from `records_dir`; the
/// conversation record `trace.jsonl.seal` points at is read from
/// `<conversation_root>/<record>/<context-id>/conversation.jsonl`, `absent` when it is not there
/// or `conversation_root` is `None`.
#[must_use]
pub fn verify_session_records(
    records_dir: &Path,
    public_key: Option<&PublicKey>,
    conversation_root: Option<&Path>,
) -> RecordReport {
    let mut streams = Vec::new();
    let mut session_id = None;
    let public = PublicKeyReport {
        path: public_key
            .and_then(PublicKey::path)
            .map(|path| path.display().to_string()),
        key_id: public_key.map(|key| key.key_id().to_string()),
    };
    let root = match WorkdirRoot::open(records_dir) {
        Ok(root) => root,
        Err(error) => {
            let mut report = StreamReport::absent(TRACE_STREAM);
            report.detail = error.to_string();
            return RecordReport {
                schema: REPORT_SCHEMA,
                session_id,
                records_dir: records_dir.display().to_string(),
                public_key: public,
                verdict: Verdict::Absent,
                streams: vec![report],
            };
        }
    };
    let error_report = |stream: &str, reason: String| {
        let mut report = StreamReport::new(stream, Verdict::Unverifiable, 0, reason);
        report.detail = format!("could not be read: {}", report.detail);
        report
    };

    let mut pointer = None;
    match read_stream(&root, TRACE_STREAM) {
        Ok(None) => streams.push(StreamReport::absent(TRACE_STREAM)),
        Err(reason) => streams.push(error_report(TRACE_STREAM, reason)),
        Ok(Some(bytes)) => {
            session_id = trace_session_id(&bytes);
            let seal = read_seal(&root, TRACE_STREAM);
            let mut report = match &seal {
                Err(reason) => {
                    let mut report = verify_inline(TRACE_STREAM, &bytes, None, public_key, None);
                    if report.verdict == Verdict::Unsealed {
                        report.verdict = Verdict::Changed;
                        report.detail = format!("the seal cannot be read: {reason}");
                    }
                    report
                }
                Ok(seal) => {
                    pointer = seal.as_ref().and_then(|seal| seal.conversation.clone());
                    verify_inline(
                        TRACE_STREAM,
                        &bytes,
                        seal.as_ref(),
                        public_key,
                        session_id.as_deref(),
                    )
                }
            };
            if report.verdict == Verdict::Unsealed {
                match session_id.as_deref() {
                    Some(id) if session_is_live(id) => {
                        report.detail.push_str("; the session is still running");
                    }
                    _ if report.sealed_lines.is_none() => {
                        report.detail =
                            format!("the session ended without a seal; {}", report.detail);
                    }
                    _ => {}
                }
            }
            streams.push(report);
        }
    }

    for (rel, whole_file) in [(EVAL_STREAM, false), (COMPLETION_STREAM, true)] {
        let report = match read_stream(&root, rel) {
            Ok(None) => StreamReport::absent(rel),
            Err(reason) => error_report(rel, reason),
            Ok(Some(bytes)) => match read_seal(&root, rel) {
                Ok(seal) => verify_listed(
                    rel,
                    &bytes,
                    whole_file,
                    seal.as_ref(),
                    public_key,
                    session_id.as_deref(),
                ),
                Err(reason) => {
                    let mut report = StreamReport::new(rel, Verdict::Changed, 0, "");
                    report.detail = format!("the seal cannot be read: {reason}");
                    report
                }
            },
        };
        streams.push(report);
    }

    if let Some(pointer) = pointer {
        let name = conversation_stream(&pointer.record, &pointer.context_id);
        let report = match conversation_root {
            Some(conversations)
                if single_segment(&pointer.record) && single_segment(&pointer.context_id) =>
            {
                let dir = conversations
                    .join(&pointer.record)
                    .join(&pointer.context_id);
                match WorkdirRoot::open(&dir) {
                    Err(_) => StreamReport::absent(&name),
                    Ok(dir_root) => {
                        match read_stream(&dir_root, crate::conversation::RECORD_FILE_NAME) {
                            Ok(None) => StreamReport::absent(&name),
                            Err(reason) => error_report(&name, reason),
                            Ok(Some(bytes)) => {
                                match read_seal(&dir_root, crate::conversation::RECORD_FILE_NAME) {
                                    Ok(seal) => verify_listed(
                                        &name,
                                        &bytes,
                                        false,
                                        seal.as_ref(),
                                        public_key,
                                        None,
                                    ),
                                    Err(reason) => {
                                        let mut report =
                                            StreamReport::new(&name, Verdict::Changed, 0, "");
                                        report.detail =
                                            format!("the seal cannot be read: {reason}");
                                        report
                                    }
                                }
                            }
                        }
                    }
                }
            }
            _ => StreamReport::absent(&name),
        };
        streams.push(report);
    }

    RecordReport {
        schema: REPORT_SCHEMA,
        session_id,
        records_dir: records_dir.display().to_string(),
        public_key: public,
        verdict: worst(streams.iter().map(|stream| stream.verdict)),
        streams,
    }
}

#[cfg(test)]
mod tests;
