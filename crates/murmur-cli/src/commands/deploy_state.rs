use std::env;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{CliError, E_IO_001, E_IO_003};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct DeploymentRecord {
    /// Identifies one provisioned deployment (a VM plus its keys and staging directory), not a
    /// capsule session — a deployment outlives the sessions that run on it. Also the path segment
    /// under `~/.murmur/deploy_keys/` and `deploy_staging/` for that deployment's key and staging
    /// directories.
    pub deployment_id: String,
    pub provider: String,
    pub provider_vm_id: String,
    pub provider_key_id: String,
    pub region: String,
    pub ip: String,
    pub url: String,
    pub manifest_path: String,
    pub started_at: String,
    pub status: String,
}

fn murmur_home() -> Result<PathBuf, CliError> {
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| CliError::new(E_IO_001, "HOME environment variable not set"))?;
    Ok(home.join(".murmur"))
}

pub(crate) fn deployments_path() -> Result<PathBuf, CliError> {
    Ok(murmur_home()?.join("deployments.json"))
}

#[cfg(feature = "beta-mur-deploy")]
pub(crate) fn deploy_keys_dir(deployment_id: &str) -> Result<PathBuf, CliError> {
    Ok(murmur_home()?.join("deploy_keys").join(deployment_id))
}

/// Where `mur deploy` assembles what it uploads for one deployment.
///
/// `deploy.rs` clears this on the way out via its `StagingGuard`, which covers every path that
/// returns — including failures. It does not cover the deploy being killed outright (`SIGKILL`,
/// power loss), where the tree is left holding a full copy of the manifest, workdir and `mur`
/// binary. Named here so `mur destroy` can sweep that orphan when the deployment goes away.
pub(crate) fn deploy_staging_dir(deployment_id: &str) -> Result<PathBuf, CliError> {
    Ok(murmur_home()?.join("deploy_staging").join(deployment_id))
}

pub(crate) fn load_deployments() -> Result<Vec<DeploymentRecord>, CliError> {
    let path = deployments_path()?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = fs::read_to_string(&path)
        .map_err(|e| CliError::new(E_IO_003, format!("failed to read {}: {e}", path.display())))?;
    serde_json::from_str(&raw)
        .map_err(|e| CliError::new(E_IO_003, format!("deployments.json is malformed: {e}")))
}

/// Create `path` if missing and hold it at `0700`, as `E-IO-003` naming the path on failure.
#[cfg(feature = "beta-mur-deploy")]
fn hold_private_dir(path: &std::path::Path) -> Result<(), CliError> {
    capsule_runtime::murmur_home::hold_private_dir(path).map_err(|reason| {
        CliError::new(
            E_IO_003,
            format!("failed to hold {} owner-only: {reason}", path.display()),
        )
    })
}

/// Writes `~/.murmur/deployments.json` at `0600` inside a `~/.murmur` held at `0700`.
#[cfg(feature = "beta-mur-deploy")]
pub(crate) fn save_deployments(records: &[DeploymentRecord]) -> Result<(), CliError> {
    let path = deployments_path()?;
    hold_private_dir(&murmur_home()?)?;
    let json = serde_json::to_string_pretty(records)
        .map_err(|e| CliError::new(E_IO_003, format!("failed to serialize deployments: {e}")))?;

    // Write-then-rename, the same shape as `write_lockfile_atomic`. A torn `deployments.json` is
    // not a recoverable inconvenience: `load_deployments` fails closed on malformed JSON, so a
    // partial write takes out `mur deploy ls` and `mur destroy` for *every* deployment at once,
    // leaving running VMs with no record of how to reach them. The rename is atomic within the
    // directory.
    capsule_runtime::murmur_home::write_private_file(&path, json.as_bytes()).map_err(|reason| {
        CliError::new(
            E_IO_003,
            format!("failed to write {}: {reason}", path.display()),
        )
    })
}

/// Creates [`deploy_staging_dir`] for `deployment_id`, holding `~/.murmur`, `deploy_staging` and
/// the deployment's own directory at `0700`, oldest first. The tree receives a copy of the
/// manifest, the workdir and the `mur` binary.
#[cfg(feature = "beta-mur-deploy")]
pub(crate) fn create_deploy_staging_dir(deployment_id: &str) -> Result<PathBuf, CliError> {
    let staging = deploy_staging_dir(deployment_id)?;
    hold_private_dir(&murmur_home()?)?;
    if let Some(root) = staging.parent() {
        hold_private_dir(root)?;
    }
    hold_private_dir(&staging)?;
    Ok(staging)
}

#[cfg(feature = "beta-mur-deploy")]
pub(crate) fn append_deployment(record: DeploymentRecord) -> Result<(), CliError> {
    let mut records = load_deployments()?;
    records.push(record);
    save_deployments(&records)
}

#[cfg(feature = "beta-mur-deploy")]
pub(crate) fn remove_deployment(deployment_id: &str) -> Result<Option<DeploymentRecord>, CliError> {
    let mut records = load_deployments()?;

    // Exact match first.
    if let Some(pos) = records
        .iter()
        .position(|r| r.deployment_id == deployment_id)
    {
        let removed = records.remove(pos);
        save_deployments(&records)?;
        return Ok(Some(removed));
    }

    // Prefix match — lets users pass the short form shown in the box (e.g. "dep_019e9d85").
    let matches: Vec<usize> = records
        .iter()
        .enumerate()
        .filter(|(_, r)| r.deployment_id.starts_with(deployment_id))
        .map(|(i, _)| i)
        .collect();

    match matches.len() {
        0 => Ok(None),
        1 => {
            let removed = records.remove(matches[0]);
            save_deployments(&records)?;
            Ok(Some(removed))
        }
        _ => {
            let candidates = matches
                .iter()
                .map(|&i| records[i].deployment_id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            Err(CliError::new(
                E_IO_003,
                format!(
                    "ambiguous prefix '{deployment_id}' matches multiple deployments: {candidates}"
                ),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> DeploymentRecord {
        DeploymentRecord {
            deployment_id: "dep_018f4b2c1234567890abcdef12345678".to_string(),
            provider: "manual".to_string(),
            provider_vm_id: String::new(),
            provider_key_id: String::new(),
            region: String::new(),
            ip: "1.2.3.4".to_string(),
            url: "https://1.2.3.4:8080".to_string(),
            manifest_path: "/tmp/murmur.yaml".to_string(),
            started_at: "2026-06-03T00:00:00Z".to_string(),
            status: "running".to_string(),
        }
    }

    #[test]
    fn deployments_are_written_with_a_deployment_id_field() {
        let json = serde_json::to_string(&record()).unwrap();
        assert!(json.contains("\"deployment_id\""), "got: {json}");
    }

    /// `remove_deployment` accepts an unambiguous prefix, so callers must clean up under the
    /// returned record's full id rather than the argument they were given — otherwise a
    /// destroy-by-prefix removes nothing and leaves the private key and staging tree on disk.
    #[test]
    fn remove_by_prefix_returns_the_record_carrying_the_full_id() {
        let full = "dep_018f4b2c1234567890abcdef12345678";
        let mut r = record();
        r.deployment_id = full.to_string();

        // Mirrors the prefix branch of `remove_deployment` without touching $HOME.
        let records = [r];
        let prefix = "dep_018f4b2c";
        let hit: Vec<_> = records
            .iter()
            .filter(|r| r.deployment_id.starts_with(prefix))
            .collect();

        assert_eq!(hit.len(), 1);
        assert_ne!(
            hit[0].deployment_id, prefix,
            "the record must carry the full id, not the prefix"
        );
        assert_eq!(hit[0].deployment_id, full);
    }

    /// Holds `HOME` at a scratch directory for one test, serialised with every other test in the
    /// crate that sets it.
    #[cfg(feature = "beta-mur-deploy")]
    struct HomeGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        saved: Option<std::ffi::OsString>,
    }

    #[cfg(feature = "beta-mur-deploy")]
    impl HomeGuard {
        fn set(home: &std::path::Path) -> Self {
            let lock = crate::config::HOME_ENV_LOCK
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let saved = env::var_os("HOME");
            // SAFETY: serialised by HOME_ENV_LOCK across every test in the crate that sets HOME.
            unsafe { env::set_var("HOME", home) };
            Self { _lock: lock, saved }
        }
    }

    #[cfg(feature = "beta-mur-deploy")]
    impl Drop for HomeGuard {
        fn drop(&mut self) {
            // SAFETY: still holding `_lock`.
            unsafe {
                match &self.saved {
                    Some(home) => env::set_var("HOME", home),
                    None => env::remove_var("HOME"),
                }
            }
        }
    }

    #[cfg(feature = "beta-mur-deploy")]
    fn mode_of(path: &std::path::Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[cfg(feature = "beta-mur-deploy")]
    fn set_mode(path: &std::path::Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    #[cfg(feature = "beta-mur-deploy")]
    #[test]
    fn save_deployments_writes_an_owner_only_file_in_an_owner_only_home() {
        let home = tempfile::tempdir().unwrap();
        let _guard = HomeGuard::set(home.path());
        let murmur = home.path().join(".murmur");
        fs::create_dir(&murmur).unwrap();
        set_mode(&murmur, 0o755);
        let path = murmur.join("deployments.json");
        fs::write(&path, "[]").unwrap();
        set_mode(&path, 0o644);

        save_deployments(&[record()]).unwrap();

        assert_eq!(mode_of(&murmur), 0o700);
        assert_eq!(mode_of(&path), 0o600);
        assert_eq!(load_deployments().unwrap().len(), 1);
    }

    #[cfg(feature = "beta-mur-deploy")]
    #[test]
    fn deploy_staging_dir_is_created_owner_only() {
        let home = tempfile::tempdir().unwrap();
        let _guard = HomeGuard::set(home.path());
        let murmur = home.path().join(".murmur");
        fs::create_dir_all(murmur.join("deploy_staging")).unwrap();
        set_mode(&murmur, 0o755);
        set_mode(&murmur.join("deploy_staging"), 0o755);

        let staging = create_deploy_staging_dir("dep_0123").unwrap();

        assert_eq!(staging, murmur.join("deploy_staging").join("dep_0123"));
        for dir in [&murmur, &murmur.join("deploy_staging"), &staging] {
            assert_eq!(mode_of(dir), 0o700, "{}", dir.display());
        }
    }
}
