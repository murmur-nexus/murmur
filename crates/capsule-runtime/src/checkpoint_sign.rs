//! Checkpoint file integrity (H-3).
//!
//! `checkpoints/{summary.md,plan.json,decisions.json}` under a capsule's
//! `accessible_workdir` are HMAC-SHA256-signed by the runtime whenever it has
//! visibility into a legitimate write (compaction, session end), and verified
//! on every session start before the agent gets control. The signing key is
//! stored outside the WASI pre-opened directory tree so no WASM guest can
//! read or forge it.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use hmac::{Hmac, Mac};
use murmur_artifact::sha256_hex;
use sha2::Sha256;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

type HmacSha256 = Hmac<Sha256>;

const CHECKPOINT_KEY_LEN: usize = 32;

/// Filenames covered by checkpoint signing, in the order the design doc lists them.
const CHECKPOINT_FILENAMES: [&str; 3] = ["summary.md", "plan.json", "decisions.json"];

fn checkpoints_dir(accessible_workdir: &Path) -> PathBuf {
    accessible_workdir.join("checkpoints")
}

/// Derives the path of the persistent per-workdir signing key.
///
/// The key lives at `$HOME/.murmur/checkpoint-keys/<sha256_hex(canonicalized accessible_workdir)>.key`,
/// i.e. outside `accessible_workdir`'s own subtree, since `build_wasi_ctx` preopens that entire
/// subtree with full read/write to the WASM guest.
pub(crate) fn checkpoint_key_path(accessible_workdir: &Path) -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .ok_or_else(|| {
            "HOME (and USERPROFILE) unset; cannot locate checkpoint signing key".to_string()
        })?;

    // Mirrors `murmur-cli/src/config.rs`'s `mur_config_path`: a relative `HOME` value is
    // resolved against the current working directory rather than treated as a path fragment.
    let mut home = PathBuf::from(home);
    if !home.is_absolute() {
        home = std::env::current_dir()
            .map_err(|e| format!("failed to determine current working directory: {e}"))?
            .join(home);
    }

    let canonical = fs::canonicalize(accessible_workdir).map_err(|e| {
        format!(
            "failed to canonicalize accessible workdir '{}': {e}",
            accessible_workdir.display()
        )
    })?;

    let hash = sha256_hex(canonical.to_string_lossy().as_bytes());
    Ok(home
        .join(".murmur")
        .join("checkpoint-keys")
        .join(format!("{hash}.key")))
}

/// Loads the persistent signing key for `accessible_workdir`, generating and
/// persisting a fresh CSPRNG key on first use.
pub(crate) fn load_or_create_checkpoint_key(accessible_workdir: &Path) -> Result<Vec<u8>, String> {
    let key_path = checkpoint_key_path(accessible_workdir)?;

    if let Ok(existing) = fs::read(&key_path) {
        if existing.len() == CHECKPOINT_KEY_LEN {
            return Ok(existing);
        }
    }

    if let Some(parent) = key_path.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            format!(
                "failed to create checkpoint key directory '{}': {e}",
                parent.display()
            )
        })?;
    }

    let mut key = vec![0u8; CHECKPOINT_KEY_LEN];
    getrandom::fill(&mut key)
        .map_err(|e| format!("failed to generate checkpoint signing key: {e}"))?;

    write_atomic(&key_path, &key, 0o600)?;

    Ok(key)
}

/// Signs every checkpoint file that currently exists under `accessible_workdir/checkpoints`,
/// writing a hex-encoded HMAC-SHA256 `.sig` sidecar next to each one.
///
/// Returns the list of filenames signed. Returns `Err` only when the signing key itself
/// cannot be loaded/created (fail-open — callers should log a warning and skip signing
/// entirely rather than treat this as fatal).
pub(crate) fn sign_existing_checkpoints(accessible_workdir: &Path) -> Result<Vec<String>, String> {
    let dir = checkpoints_dir(accessible_workdir);
    if !dir.is_dir() {
        return Ok(Vec::new());
    }

    let key = load_or_create_checkpoint_key(accessible_workdir)?;
    let mut signed = Vec::new();

    for name in CHECKPOINT_FILENAMES {
        let path = dir.join(name);
        let Ok(bytes) = fs::read(&path) else {
            continue;
        };

        let sig_hex = hmac_hex(&key, &bytes);
        let sig_path = dir.join(format!("{name}.sig"));
        if write_atomic(&sig_path, sig_hex.as_bytes(), 0o644).is_ok() {
            signed.push(name.to_string());
        }
    }

    Ok(signed)
}

/// Verifies every checkpoint file under `accessible_workdir/checkpoints` against its `.sig`
/// sidecar. A file with a missing/undecodable `.sig`, or one whose HMAC doesn't match its
/// current bytes, is renamed to `<name>.rejected` (removing any stale prior `.rejected` file
/// first) and its `.sig` is removed. Valid files are left untouched.
///
/// Returns the list of quarantined filenames. Returns `Err` only when the signing key itself
/// cannot be loaded/created (fail-open — callers should log a warning and skip verification
/// entirely; an unattainable key is not grounds to quarantine an otherwise-untouched file).
pub(crate) fn verify_and_quarantine_checkpoints(
    accessible_workdir: &Path,
) -> Result<Vec<String>, String> {
    let dir = checkpoints_dir(accessible_workdir);
    if !dir.is_dir() {
        return Ok(Vec::new());
    }

    let key = load_or_create_checkpoint_key(accessible_workdir)?;
    let mut quarantined = Vec::new();

    for name in CHECKPOINT_FILENAMES {
        let path = dir.join(name);
        let Ok(bytes) = fs::read(&path) else {
            continue;
        };

        let sig_path = dir.join(format!("{name}.sig"));
        let valid = fs::read_to_string(&sig_path)
            .ok()
            .and_then(|hex| decode_hex(hex.trim()))
            .is_some_and(|expected| verify_hmac(&key, &bytes, &expected));

        if valid {
            continue;
        }

        let rejected_path = dir.join(format!("{name}.rejected"));
        let _ = fs::remove_file(&rejected_path);
        if fs::rename(&path, &rejected_path).is_ok() {
            let _ = fs::remove_file(&sig_path);
            quarantined.push(name.to_string());
        }
    }

    Ok(quarantined)
}

fn hmac_hex(key: &[u8], bytes: &[u8]) -> String {
    let mut mac = <HmacSha256 as Mac>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(bytes);
    encode_hex(&mac.finalize().into_bytes())
}

fn verify_hmac(key: &[u8], bytes: &[u8], expected: &[u8]) -> bool {
    let Ok(mut mac) = <HmacSha256 as Mac>::new_from_slice(key) else {
        return false;
    };
    mac.update(bytes);
    mac.verify_slice(expected).is_ok()
}

fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<(), String> {
    let mut tmp_name = path.as_os_str().to_os_string();
    tmp_name.push(".tmp");
    let tmp = PathBuf::from(tmp_name);

    {
        // `.mode(mode)` applies the permission bits at creation time (subject to umask),
        // so the file is never briefly world/group-readable between creation and a
        // separate `set_permissions` call — this matters for the 0o600 signing key file.
        let mut file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(mode)
            .open(&tmp)
            .map_err(|e| format!("failed to create temp file '{}': {e}", tmp.display()))?;

        file.write_all(bytes)
            .map_err(|e| format!("failed to write temp file '{}': {e}", tmp.display()))?;

        // umask can still widen the mode passed to `open`; enforce it explicitly.
        file.set_permissions(fs::Permissions::from_mode(mode))
            .map_err(|e| format!("failed to set permissions on '{}': {e}", tmp.display()))?;

        file.sync_all()
            .map_err(|e| format!("failed to sync temp file '{}': {e}", tmp.display()))?;
    }

    fs::rename(&tmp, path)
        .map_err(|e| format!("failed to install '{}': {e}", path.display()))
}

/// Shared test-only HOME-mutation helper. `checkpoint_key_path` derives its location from the
/// `HOME` env var, so any test exercising real signing/verification needs to redirect it to a
/// tempdir; `HOME_LOCK` is shared across every test module in this crate that does so (see
/// `hooks.rs`'s test module) to prevent concurrently-run tests from racing on the process-wide
/// env var. Mirrors the pattern already used by `murmur-cli/src/config.rs`'s tests.
#[cfg(test)]
pub(crate) mod test_support {
    use std::path::Path;

    pub(crate) static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    pub(crate) fn with_home<T>(home: &Path, f: impl FnOnce() -> T) -> T {
        let saved = std::env::var_os("HOME");
        std::env::set_var("HOME", home);
        let result = f();
        match saved {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::{test_support::{with_home, HOME_LOCK}, *};
    use tempfile::TempDir;

    #[test]
    fn checkpoint_key_path_is_never_inside_accessible_workdir() {
        let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = TempDir::new().unwrap();
        let accessible = TempDir::new().unwrap();

        let key_path =
            with_home(home.path(), || checkpoint_key_path(accessible.path()).unwrap());

        assert!(
            !key_path.starts_with(accessible.path()),
            "key path {} must not live under accessible_workdir {}",
            key_path.display(),
            accessible.path().display()
        );
        assert!(key_path.starts_with(home.path()));
    }

    #[test]
    fn load_or_create_checkpoint_key_is_idempotent() {
        let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = TempDir::new().unwrap();
        let accessible = TempDir::new().unwrap();

        let (first, second) = with_home(home.path(), || {
            let first = load_or_create_checkpoint_key(accessible.path()).unwrap();
            let second = load_or_create_checkpoint_key(accessible.path()).unwrap();
            (first, second)
        });

        assert_eq!(first, second);
        assert_eq!(first.len(), CHECKPOINT_KEY_LEN);
    }

    #[test]
    fn sign_then_verify_round_trips_with_no_quarantine() {
        let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = TempDir::new().unwrap();
        let accessible = TempDir::new().unwrap();
        let checkpoints = accessible.path().join("checkpoints");
        fs::create_dir_all(&checkpoints).unwrap();
        fs::write(checkpoints.join("summary.md"), "goals: ship it").unwrap();
        fs::write(checkpoints.join("plan.json"), r#"{"tasks":[]}"#).unwrap();

        let quarantined = with_home(home.path(), || {
            sign_existing_checkpoints(accessible.path()).unwrap();
            verify_and_quarantine_checkpoints(accessible.path()).unwrap()
        });

        assert!(quarantined.is_empty());
        assert!(checkpoints.join("summary.md").exists());
        assert!(checkpoints.join("summary.md.sig").exists());
        assert!(checkpoints.join("plan.json").exists());
    }

    #[test]
    fn tampered_file_is_quarantined_and_sig_removed() {
        let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = TempDir::new().unwrap();
        let accessible = TempDir::new().unwrap();
        let checkpoints = accessible.path().join("checkpoints");
        fs::create_dir_all(&checkpoints).unwrap();
        fs::write(checkpoints.join("plan.json"), r#"{"tasks":[]}"#).unwrap();

        let quarantined = with_home(home.path(), || {
            sign_existing_checkpoints(accessible.path()).unwrap();
            // Simulate tampering by a compromised tool after the sign pass.
            fs::write(checkpoints.join("plan.json"), r#"{"tasks":["evil"]}"#).unwrap();
            verify_and_quarantine_checkpoints(accessible.path()).unwrap()
        });

        assert_eq!(quarantined, vec!["plan.json".to_string()]);
        assert!(!checkpoints.join("plan.json").exists());
        assert!(checkpoints.join("plan.json.rejected").exists());
        assert!(!checkpoints.join("plan.json.sig").exists());
    }

    #[test]
    fn missing_sig_is_quarantined() {
        let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = TempDir::new().unwrap();
        let accessible = TempDir::new().unwrap();
        let checkpoints = accessible.path().join("checkpoints");
        fs::create_dir_all(&checkpoints).unwrap();
        fs::write(checkpoints.join("decisions.json"), r#"{"decisions":[]}"#).unwrap();

        let quarantined = with_home(home.path(), || {
            verify_and_quarantine_checkpoints(accessible.path()).unwrap()
        });

        assert_eq!(quarantined, vec!["decisions.json".to_string()]);
        assert!(checkpoints.join("decisions.json.rejected").exists());
    }

    #[test]
    fn preexisting_rejected_file_is_overwritten() {
        let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = TempDir::new().unwrap();
        let accessible = TempDir::new().unwrap();
        let checkpoints = accessible.path().join("checkpoints");
        fs::create_dir_all(&checkpoints).unwrap();
        fs::write(checkpoints.join("summary.md"), "new content").unwrap();
        fs::write(checkpoints.join("summary.md.rejected"), "stale rejected content").unwrap();

        with_home(home.path(), || {
            verify_and_quarantine_checkpoints(accessible.path()).unwrap();
        });

        let rejected = fs::read_to_string(checkpoints.join("summary.md.rejected")).unwrap();
        assert_eq!(rejected, "new content");
    }

    #[test]
    fn no_checkpoints_dir_is_a_noop() {
        let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = TempDir::new().unwrap();
        let accessible = TempDir::new().unwrap();

        let (signed, quarantined) = with_home(home.path(), || {
            let signed = sign_existing_checkpoints(accessible.path()).unwrap();
            let quarantined = verify_and_quarantine_checkpoints(accessible.path()).unwrap();
            (signed, quarantined)
        });

        assert!(signed.is_empty());
        assert!(quarantined.is_empty());
    }

    #[test]
    fn key_derivation_failure_fails_open() {
        let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let accessible = TempDir::new().unwrap();
        let checkpoints = accessible.path().join("checkpoints");
        fs::create_dir_all(&checkpoints).unwrap();
        fs::write(checkpoints.join("summary.md"), "goals").unwrap();

        let saved = std::env::var_os("HOME");
        std::env::remove_var("HOME");
        std::env::remove_var("USERPROFILE");

        let sign_result = sign_existing_checkpoints(accessible.path());
        let verify_result = verify_and_quarantine_checkpoints(accessible.path());

        if let Some(v) = saved {
            std::env::set_var("HOME", v);
        }

        assert!(sign_result.is_err());
        assert!(verify_result.is_err());
        // The file must be untouched — a key-derivation failure never quarantines.
        assert!(checkpoints.join("summary.md").exists());
    }

    #[test]
    fn hex_round_trip() {
        let bytes = [0u8, 1, 255, 16, 128];
        let hex = encode_hex(&bytes);
        assert_eq!(decode_hex(&hex).unwrap(), bytes);
    }

    #[test]
    fn decode_hex_rejects_odd_length() {
        assert!(decode_hex("abc").is_none());
    }
}
