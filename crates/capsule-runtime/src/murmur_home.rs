//! The murmur home, `~/.murmur`, and the modes of what it holds.
//!
//! Every writer under the home holds `~/.murmur` itself at [`MURMUR_HOME_DIR_MODE`] before it
//! creates anything beneath it, so a store directory is never reachable through a home the umask
//! left `0755`. Files that hold a secret go through [`write_private_file`], which lands them at
//! [`PRIVATE_FILE_MODE`] by rename.
//!
//! [`audit_murmur_home`] is the read-only half: it reports the mode of every entry and every wider
//! than expected path under an owner-only one, for `mur doctor`. It reads metadata only, never a
//! file's contents, and changes nothing.

use std::path::{Path, PathBuf};

use murmur_artifact::{security_warning_link, W_SEC_028};

/// Mode `~/.murmur` and every owner-only directory beneath it are held at.
pub const MURMUR_HOME_DIR_MODE: u32 = 0o700;

/// Mode every owner-only file under `~/.murmur` is held at.
pub const PRIVATE_FILE_MODE: u32 = 0o600;

/// The most wide descendants one entry's report lists; the rest are counted in
/// `HomeEntryState::Present::wide_descendants_omitted`.
const WIDE_DESCENDANT_CAP: usize = 20;

/// Resolve `~/.murmur`, create it at [`MURMUR_HOME_DIR_MODE`] if missing, and strip every group
/// and other bit from it whether or not this call created it.
///
/// An existing home keeps the owner bits it has: a home the owner closed to `0500` stays closed,
/// so every write beneath it fails and reports, rather than the runtime reopening it.
///
/// Errors when `HOME` is unset or relative, when the path is not a directory, or when the
/// directory cannot be created or its mode set. Each caller wraps the reason in its own error
/// type.
pub fn ensure_murmur_home() -> Result<PathBuf, String> {
    use std::os::unix::fs::PermissionsExt;

    let home = crate::state_store::murmur_home_dir()?;
    let located = |reason: String| format!("{}: {reason}", home.display());
    match std::fs::metadata(&home) {
        Ok(metadata) if !metadata.is_dir() => return Err(located("not a directory".to_string())),
        Ok(metadata) => {
            let mode = metadata.permissions().mode() & 0o777;
            let narrowed = mode & MURMUR_HOME_DIR_MODE;
            if narrowed != mode {
                std::fs::set_permissions(&home, std::fs::Permissions::from_mode(narrowed))
                    .map_err(|err| located(format!("failed to set mode {narrowed:04o}: {err}")))?;
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            hold_private_dir(&home).map_err(located)?;
        }
        Err(err) => return Err(located(err.to_string())),
    }
    Ok(home)
}

/// Create `path` if missing and hold it at [`MURMUR_HOME_DIR_MODE`]. Only `path` itself gets the
/// mode; a missing ancestor is created under the umask, so callers hold each level oldest first.
pub fn hold_private_dir(path: &Path) -> Result<(), String> {
    crate::state_store::ensure_private_dir(path, MURMUR_HOME_DIR_MODE)
}

/// Replace `target` with `contents` at [`PRIVATE_FILE_MODE`].
///
/// Staged beside `target` and renamed over it, so a reader sees the old file or the new one, and
/// the inode changes on every write — which is how a running capsule's credential stamp notices a
/// `mur config set -g`. The mode is set on the staged file before the rename, so the new contents
/// are never visible at a wider mode. The parent directory must already exist.
pub fn write_private_file(target: &Path, contents: &[u8]) -> Result<(), String> {
    crate::retention::StagedRewrite::stage_with_mode(target, contents, Some(PRIVATE_FILE_MODE))?
        .commit()
}

/// Whether `mode` grants any permission bit `allowed` does not.
pub fn is_wider_than(mode: u32, allowed: u32) -> bool {
    (mode & 0o777) & !allowed != 0
}

/// What one top-level entry of `~/.murmur` is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HomeEntryKind {
    Home,
    Config,
    DeployKeys,
    DeployStaging,
    Deployments,
    Spend,
    Conversations,
    Running,
    State,
    Artifacts,
    BinaryCache,
    /// A top-level name this build writes nothing under.
    Unrecognised(String),
}

impl HomeEntryKind {
    /// Every kind with a fixed name, in the order the audit reports them after the home itself.
    const KNOWN: [HomeEntryKind; 10] = [
        Self::Config,
        Self::DeployKeys,
        Self::DeployStaging,
        Self::Deployments,
        Self::Spend,
        Self::Conversations,
        Self::Running,
        Self::State,
        Self::Artifacts,
        Self::BinaryCache,
    ];

    /// The entry's name under `~/.murmur`, or `None` for the home itself.
    pub fn file_name(&self) -> Option<&str> {
        Some(match self {
            Self::Home => return None,
            Self::Config => "config.yaml",
            Self::DeployKeys => "deploy_keys",
            Self::DeployStaging => "deploy_staging",
            Self::Deployments => "deployments.json",
            Self::Spend => "spend",
            Self::Conversations => "conversations",
            Self::Running => "running",
            Self::State => "state",
            Self::Artifacts => "artifacts",
            Self::BinaryCache => "bin",
            Self::Unrecognised(name) => name,
        })
    }

    /// What the entry holds, as a noun phrase that completes "`<path>` holds …".
    pub fn holds(&self) -> &'static str {
        match self {
            Self::Home => "the murmur home",
            Self::Config => "provider credentials",
            Self::DeployKeys => "SSH private keys",
            Self::DeployStaging => "deployment staging copies",
            Self::Deployments => "deployment records",
            Self::Spend => "the machine spend ledger",
            Self::Conversations => "conversation records",
            Self::Running => "running-capsule records",
            Self::State => "capsule state stores",
            Self::Artifacts => "installed artifacts",
            Self::BinaryCache => "cached mur binaries",
            Self::Unrecognised(_) => "something not recognised by this build",
        }
    }

    /// Whether the entry, and everything beneath it, is expected to be owner-only.
    pub fn expected_private(&self) -> bool {
        !matches!(
            self,
            Self::Artifacts | Self::BinaryCache | Self::Unrecognised(_)
        )
    }
}

/// A path under an owner-only entry whose mode grants more than it should.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WideEntry {
    pub path: PathBuf,
    /// Permission bits only, `& 0o777`.
    pub mode: u32,
    /// [`MURMUR_HOME_DIR_MODE`] for a directory, [`PRIVATE_FILE_MODE`] for a file.
    pub allowed: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HomeEntryState {
    Absent,
    Present {
        /// Permission bits only, `& 0o777`, from `lstat`.
        mode: u32,
        /// Whether the entry itself is wider than its kind allows. Always `false` for a kind that
        /// is not [`HomeEntryKind::expected_private`], and for a symlink.
        wide: bool,
        /// Wide paths beneath an owner-only directory, depth first, at most
        /// [`WIDE_DESCENDANT_CAP`].
        wide_descendants: Vec<WideEntry>,
        /// Wide paths found beyond the cap.
        wide_descendants_omitted: usize,
    },
    /// `lstat` failed for a reason other than the entry not existing.
    Unreadable {
        error: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomeEntryReport {
    pub path: PathBuf,
    pub kind: HomeEntryKind,
    pub state: HomeEntryState,
}

impl HomeEntryReport {
    /// The mode this entry is held to: [`MURMUR_HOME_DIR_MODE`] for a directory,
    /// [`PRIVATE_FILE_MODE`] otherwise.
    pub fn allowed(&self) -> u32 {
        allowed_for(&self.path)
    }
}

/// Report `murmur_home` and every entry in it.
///
/// The home first, then every [`HomeEntryKind`] with a fixed name, absent ones included, then
/// every other top-level name, sorted. Owner-only directories are walked without following
/// symlinks; entries that are not expected to be owner-only get their mode and nothing else.
/// Reads metadata only and changes nothing on disk.
pub fn audit_murmur_home(murmur_home: &Path) -> Vec<HomeEntryReport> {
    let mut reports = vec![report_entry(murmur_home.to_path_buf(), HomeEntryKind::Home)];

    for kind in HomeEntryKind::KNOWN {
        let name = kind.file_name().expect("a known kind has a fixed name");
        reports.push(report_entry(murmur_home.join(name), kind));
    }

    let mut others: Vec<String> = std::fs::read_dir(murmur_home)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| {
                    !HomeEntryKind::KNOWN
                        .iter()
                        .any(|kind| kind.file_name() == Some(name.as_str()))
                })
                .collect()
        })
        .unwrap_or_default();
    others.sort();
    for name in others {
        reports.push(report_entry(
            murmur_home.join(&name),
            HomeEntryKind::Unrecognised(name),
        ));
    }

    reports
}

fn report_entry(path: PathBuf, kind: HomeEntryKind) -> HomeEntryReport {
    use std::os::unix::fs::PermissionsExt;

    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return HomeEntryReport {
                path,
                kind,
                state: HomeEntryState::Absent,
            }
        }
        Err(err) => {
            return HomeEntryReport {
                path,
                kind,
                state: HomeEntryState::Unreadable {
                    error: err.to_string(),
                },
            }
        }
    };
    let mode = metadata.permissions().mode() & 0o777;
    let private = kind.expected_private();
    let file_type = metadata.file_type();
    let wide =
        private && !file_type.is_symlink() && is_wider_than(mode, allowed_for_type(file_type));

    let mut wide_descendants = Vec::new();
    let mut omitted = 0;
    // The home's own entries are reported one by one, so only an entry below it is walked.
    if private && file_type.is_dir() && kind != HomeEntryKind::Home {
        collect_wide_descendants(&path, &mut wide_descendants, &mut omitted);
    }

    HomeEntryReport {
        path,
        kind,
        state: HomeEntryState::Present {
            mode,
            wide,
            wide_descendants,
            wide_descendants_omitted: omitted,
        },
    }
}

fn collect_wide_descendants(dir: &Path, found: &mut Vec<WideEntry>, omitted: &mut usize) {
    use std::os::unix::fs::PermissionsExt;

    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        let file_type = metadata.file_type();
        if file_type.is_symlink() || !(file_type.is_dir() || file_type.is_file()) {
            continue;
        }
        let mode = metadata.permissions().mode() & 0o777;
        let allowed = allowed_for_type(file_type);
        if is_wider_than(mode, allowed) {
            if found.len() < WIDE_DESCENDANT_CAP {
                found.push(WideEntry {
                    path: path.clone(),
                    mode,
                    allowed,
                });
            } else {
                *omitted += 1;
            }
        }
        if file_type.is_dir() {
            collect_wide_descendants(&path, found, omitted);
        }
    }
}

fn allowed_for_type(file_type: std::fs::FileType) -> u32 {
    if file_type.is_dir() {
        MURMUR_HOME_DIR_MODE
    } else {
        PRIVATE_FILE_MODE
    }
}

fn allowed_for(path: &Path) -> u32 {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => allowed_for_type(metadata.file_type()),
        Err(_) => PRIVATE_FILE_MODE,
    }
}

/// The `W-SEC-028` line for a path under `~/.murmur` that holds `holds` and is wider than
/// `allowed`. Names the path and its mode, never its contents.
pub fn wide_entry_warning(path: &Path, holds: &str, mode: u32, allowed: u32) -> String {
    let path = path.display();
    format!(
        "[capsule-runtime] warning[{W_SEC_028}]: {path} holds {holds} and is mode {:04o}, which \
         other accounts on this host can read; run `chmod {:o} {path}` ({})",
        mode & 0o777,
        allowed,
        security_warning_link(W_SEC_028)
    )
}

/// Print `W-SEC-028` when the config file an inference credential was just read from grants any
/// group or other bit. Never a refusal, and never names the value.
pub fn warn_on_wide_credential_file(path: &Path, name: &str, mode: u32) {
    if let Some(warning) = wide_credential_file_warning(path, name, mode) {
        crate::runtime_err!("{warning}");
    }
}

fn wide_credential_file_warning(path: &Path, name: &str, mode: u32) -> Option<String> {
    if !is_wider_than(mode, PRIVATE_FILE_MODE) {
        return None;
    }
    let path = path.display();
    Some(format!(
        "[capsule-runtime] warning[{W_SEC_028}]: the inference credential credentials.{name} is \
         read from {path}, which is mode {:04o} and readable by other accounts on this host; run \
         `chmod 600 {path}` ({})",
        mode & 0o777,
        security_warning_link(W_SEC_028)
    ))
}

/// Runs one `#[ignore]`d test of this crate in a child process with `HOME` set to `home`.
///
/// `HOME` is process-global, and every staging test in this binary resolves `~/.murmur` from it,
/// so a test that needs a scratch home gets its own process rather than a lock no other test
/// takes. The child test returns without asserting unless [`INNER_HOME_ENV`] is set.
#[cfg(test)]
pub(crate) fn run_with_home(test: &str, home: &Path) {
    let exe = std::env::current_exe().expect("test binary path");
    let output = std::process::Command::new(exe)
        .args([
            test,
            "--exact",
            "--ignored",
            "--test-threads=1",
            "--nocapture",
        ])
        .env("HOME", home)
        .env(INNER_HOME_ENV, "1")
        .output()
        .expect("re-exec the test binary with a scratch HOME");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "{test} failed\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
    assert!(
        stdout.contains("1 passed"),
        "{test} was not selected\n--- stdout ---\n{stdout}"
    );
}

/// Set only in the child [`run_with_home`] starts.
#[cfg(test)]
pub(crate) const INNER_HOME_ENV: &str = "MURMUR_TEST_INNER_HOME";

/// Whether this process is a [`run_with_home`] child.
#[cfg(test)]
pub(crate) fn in_scratch_home() -> bool {
    std::env::var_os(INNER_HOME_ENV).is_some()
}

/// `path`'s permission bits.
#[cfg(test)]
pub(crate) fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::symlink_metadata(path)
        .unwrap_or_else(|err| panic!("{}: {err}", path.display()))
        .permissions()
        .mode()
        & 0o777
}

/// Creates `path` as a directory at `mode`.
#[cfg(test)]
pub(crate) fn wide_dir(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// Writes `path` with `contents` at `mode`.
#[cfg(test)]
pub(crate) fn wide_file(path: &Path, contents: &str, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, contents).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn murmur_home_is_wider_than_truth_table() {
        for (mode, allowed, wide) in [
            (0o600, 0o600, false),
            (0o644, 0o600, true),
            (0o640, 0o600, true),
            (0o700, 0o700, false),
            (0o711, 0o700, true),
            (0o400, 0o600, false),
        ] {
            assert_eq!(
                is_wider_than(mode, allowed),
                wide,
                "{mode:04o} against {allowed:04o}"
            );
        }
    }

    #[test]
    fn ensure_murmur_home_creates_and_tightens_the_home_directory() {
        let wide = tempfile::tempdir().unwrap();
        wide_dir(&wide.path().join(".murmur"), 0o755);
        run_with_home(
            "murmur_home::tests::inner_ensure_murmur_home_holds_the_home_at_0700",
            wide.path(),
        );
        assert_eq!(mode_of(&wide.path().join(".murmur")), 0o700);

        let missing = tempfile::tempdir().unwrap();
        run_with_home(
            "murmur_home::tests::inner_ensure_murmur_home_holds_the_home_at_0700",
            missing.path(),
        );
        assert_eq!(mode_of(&missing.path().join(".murmur")), 0o700);

        let closed = tempfile::tempdir().unwrap();
        wide_dir(&closed.path().join(".murmur"), 0o555);
        run_with_home(
            "murmur_home::tests::inner_ensure_murmur_home_holds_the_home_at_0700",
            closed.path(),
        );
        assert_eq!(
            mode_of(&closed.path().join(".murmur")),
            0o500,
            "an owner-closed home loses group and other bits and is not reopened"
        );
    }

    #[test]
    #[ignore = "run by ensure_murmur_home_creates_and_tightens_the_home_directory"]
    fn inner_ensure_murmur_home_holds_the_home_at_0700() {
        if !in_scratch_home() {
            return;
        }
        let home = ensure_murmur_home().unwrap();
        assert!(home.ends_with(".murmur"));
        assert_eq!(mode_of(&home) & 0o077, 0);
    }

    #[test]
    fn murmur_home_write_private_file_replaces_a_wide_file_owner_only() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        wide_file(&path, "old", 0o644);
        let before = std::fs::metadata(&path).unwrap().ino();

        write_private_file(&path, b"new").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        assert_eq!(mode_of(&path), 0o600);
        assert_ne!(std::fs::metadata(&path).unwrap().ino(), before);
    }

    #[test]
    fn murmur_home_audit_reports_absent_entries_in_kind_order() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join(".murmur");

        let reports = audit_murmur_home(&home);

        assert_eq!(reports.len(), 11);
        assert_eq!(reports[0].kind, HomeEntryKind::Home);
        assert_eq!(reports[1].kind, HomeEntryKind::Config);
        assert_eq!(reports[10].kind, HomeEntryKind::BinaryCache);
        assert!(reports
            .iter()
            .all(|report| report.state == HomeEntryState::Absent));
    }

    #[test]
    fn murmur_home_audit_flags_wide_private_entries_and_never_non_private_ones() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join(".murmur");
        wide_dir(&home, 0o755);
        wide_file(&home.join("config.yaml"), "credentials: {}\n", 0o644);
        wide_dir(&home.join("artifacts"), 0o777);
        wide_file(&home.join("artifacts/loose"), "x", 0o666);
        wide_dir(&home.join("spend"), 0o700);
        wide_file(&home.join("spend/2026-09-13.jsonl"), "", 0o600);
        wide_file(&home.join("nexus-config.json"), "{}", 0o644);

        let reports = audit_murmur_home(&home);
        let find = |kind: &HomeEntryKind| {
            reports
                .iter()
                .find(|report| &report.kind == kind)
                .unwrap()
                .state
                .clone()
        };
        let wide = |state: HomeEntryState| match state {
            HomeEntryState::Present {
                wide,
                wide_descendants,
                ..
            } => (wide, wide_descendants.len()),
            other => panic!("{other:?}"),
        };

        assert_eq!(wide(find(&HomeEntryKind::Home)), (true, 0));
        assert_eq!(wide(find(&HomeEntryKind::Config)), (true, 0));
        assert_eq!(wide(find(&HomeEntryKind::Artifacts)), (false, 0));
        assert_eq!(wide(find(&HomeEntryKind::Spend)), (false, 0));
        let last = reports.last().unwrap();
        assert_eq!(
            last.kind,
            HomeEntryKind::Unrecognised("nexus-config.json".to_string())
        );
        assert_eq!(wide(last.state.clone()), (false, 0));
        assert_eq!(mode_of(&home), 0o755, "the audit changes nothing");
    }

    #[test]
    fn murmur_home_audit_does_not_follow_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join(".murmur");
        wide_dir(&home.join("state"), 0o700);
        let outside = dir.path().join("outside");
        wide_dir(&outside, 0o777);
        wide_file(&outside.join("secret"), "x", 0o666);
        std::os::unix::fs::symlink(&outside, home.join("state/link")).unwrap();

        let reports = audit_murmur_home(&home);
        let state = &reports
            .iter()
            .find(|report| report.kind == HomeEntryKind::State)
            .unwrap()
            .state;
        assert!(
            matches!(state, HomeEntryState::Present { wide: false, wide_descendants, .. } if wide_descendants.is_empty()),
            "{state:?}"
        );
    }

    #[test]
    fn murmur_home_audit_caps_wide_descendants_and_counts_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join(".murmur");
        let keys = home.join("deploy_keys");
        wide_dir(&keys, 0o700);
        for index in 0..25 {
            wide_file(&keys.join(format!("key_{index:02}")), "x", 0o644);
        }

        let reports = audit_murmur_home(&home);
        let state = &reports
            .iter()
            .find(|report| report.kind == HomeEntryKind::DeployKeys)
            .unwrap()
            .state;
        match state {
            HomeEntryState::Present {
                wide,
                wide_descendants,
                wide_descendants_omitted,
                ..
            } => {
                assert!(!wide);
                assert_eq!(wide_descendants.len(), WIDE_DESCENDANT_CAP);
                assert_eq!(*wide_descendants_omitted, 5);
                assert!(wide_descendants
                    .iter()
                    .all(|entry| entry.mode == 0o644 && entry.allowed == 0o600));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn murmur_home_wide_credential_file_warning_names_the_path_and_mode_only() {
        let path = Path::new("/home/someone/.murmur/config.yaml");
        assert_eq!(
            wide_credential_file_warning(path, "PROVIDER_KEY", 0o600),
            None
        );
        assert_eq!(
            wide_credential_file_warning(path, "PROVIDER_KEY", 0o100600),
            None
        );

        let warning = wide_credential_file_warning(path, "PROVIDER_KEY", 0o100644).unwrap();
        assert!(warning.contains("warning[W-SEC-028]"), "{warning}");
        assert!(warning.contains("credentials.PROVIDER_KEY"), "{warning}");
        assert!(
            warning.contains("/home/someone/.murmur/config.yaml"),
            "{warning}"
        );
        assert!(warning.contains("mode 0644"), "{warning}");
        assert!(
            warning.contains("`chmod 600 /home/someone/.murmur/config.yaml`"),
            "{warning}"
        );
        assert!(warning.ends_with("#w-sec-028)"), "{warning}");
    }

    #[test]
    fn murmur_home_wide_entry_warning_prints_the_chmod_for_the_allowed_mode() {
        let warning = wide_entry_warning(Path::new("/h/.murmur"), "the murmur home", 0o755, 0o700);
        assert_eq!(
            warning,
            "[capsule-runtime] warning[W-SEC-028]: /h/.murmur holds the murmur home and is mode \
             0755, which other accounts on this host can read; run `chmod 700 /h/.murmur` \
             (https://docs.murmur.nexus/murmur-nexus/murmur/reference/diagnostics/#w-sec-028)"
        );
    }
}
