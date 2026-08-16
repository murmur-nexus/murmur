use std::env;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{CliError, E_IO_001, E_IO_003};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct DeploymentRecord {
    /// Identifies one provisioned deployment (a VM plus its keys and staging directory), not a
    /// capsule session — a deployment outlives the sessions that run on it.
    ///
    /// `alias` keeps deployments written before the rename loadable: the value is a path segment
    /// under `~/.murmur/deploy_keys/` and `deploy_staging/`, so a record that failed to
    /// deserialise would orphan a live VM from `mur destroy`.
    #[serde(alias = "job_id")]
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
    let raw = fs::read_to_string(&path).map_err(|e| {
        CliError::new(E_IO_003, format!("failed to read {}: {e}", path.display()))
    })?;
    serde_json::from_str(&raw).map_err(|e| {
        CliError::new(E_IO_003, format!("deployments.json is malformed: {e}"))
    })
}

#[cfg(feature = "beta-mur-deploy")]
pub(crate) fn save_deployments(records: &[DeploymentRecord]) -> Result<(), CliError> {
    let path = deployments_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            CliError::new(E_IO_003, format!("failed to create {}: {e}", parent.display()))
        })?;
    }
    let json = serde_json::to_string_pretty(records)
        .map_err(|e| CliError::new(E_IO_003, format!("failed to serialize deployments: {e}")))?;

    // Write-then-rename, the same shape as `write_lockfile_atomic`. A torn `deployments.json` is
    // not a recoverable inconvenience: `load_deployments` fails closed on malformed JSON, so a
    // partial write takes out `mur ps` and `mur destroy` for *every* deployment at once, leaving
    // running VMs with no record of how to reach them. The rename is atomic within the directory.
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, json).map_err(|e| {
        CliError::new(E_IO_003, format!("failed to write {}: {e}", tmp.display()))
    })?;
    fs::rename(&tmp, &path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        CliError::new(
            E_IO_003,
            format!("failed to replace {} with {}: {e}", path.display(), tmp.display()),
        )
    })
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
    if let Some(pos) = records.iter().position(|r| r.deployment_id == deployment_id) {
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
                format!("ambiguous prefix '{deployment_id}' matches multiple deployments: {candidates}"),
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
    fn deployments_are_written_under_the_current_field_name() {
        let json = serde_json::to_string(&record()).unwrap();
        assert!(json.contains("\"deployment_id\""), "got: {json}");
        assert!(!json.contains("\"job_id\""), "got: {json}");
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
        let records = vec![r];
        let prefix = "dep_018f4b2c";
        let hit: Vec<_> = records
            .iter()
            .filter(|r| r.deployment_id.starts_with(prefix))
            .collect();

        assert_eq!(hit.len(), 1);
        assert_ne!(hit[0].deployment_id, prefix, "the record must carry the full id, not the prefix");
        assert_eq!(hit[0].deployment_id, full);
    }

    /// A `deployments.json` written before the rename still has to load: its id is a path segment
    /// under `~/.murmur/deploy_keys/` and `deploy_staging/`, so a record that failed to
    /// deserialise would strand a live VM with no way to reach it through `mur destroy`.
    #[test]
    fn deployments_written_before_the_rename_still_load() {
        let legacy = r#"{
            "job_id": "job_018f4b2c1234567890abcdef12345678",
            "provider": "manual",
            "provider_vm_id": "",
            "provider_key_id": "",
            "region": "",
            "ip": "1.2.3.4",
            "url": "https://1.2.3.4:8080",
            "manifest_path": "/tmp/murmur.yaml",
            "started_at": "2026-06-03T00:00:00Z",
            "status": "running"
        }"#;

        let parsed: DeploymentRecord = serde_json::from_str(legacy).unwrap();
        assert_eq!(parsed.deployment_id, "job_018f4b2c1234567890abcdef12345678");
    }
}
