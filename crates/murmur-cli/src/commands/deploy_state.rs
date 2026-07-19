use std::env;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{CliError, E_IO_001, E_IO_003};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct DeploymentRecord {
    pub job_id: String,
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
pub(crate) fn deploy_keys_dir(job_id: &str) -> Result<PathBuf, CliError> {
    Ok(murmur_home()?.join("deploy_keys").join(job_id))
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
    fs::write(&path, json).map_err(|e| {
        CliError::new(E_IO_003, format!("failed to write {}: {e}", path.display()))
    })
}

#[cfg(feature = "beta-mur-deploy")]
pub(crate) fn append_deployment(record: DeploymentRecord) -> Result<(), CliError> {
    let mut records = load_deployments()?;
    records.push(record);
    save_deployments(&records)
}

#[cfg(feature = "beta-mur-deploy")]
pub(crate) fn remove_deployment(job_id: &str) -> Result<Option<DeploymentRecord>, CliError> {
    let mut records = load_deployments()?;

    // Exact match first.
    if let Some(pos) = records.iter().position(|r| r.job_id == job_id) {
        let removed = records.remove(pos);
        save_deployments(&records)?;
        return Ok(Some(removed));
    }

    // Prefix match — lets users pass the short form shown in the box (e.g. "job_019e9d85").
    let matches: Vec<usize> = records
        .iter()
        .enumerate()
        .filter(|(_, r)| r.job_id.starts_with(job_id))
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
                .map(|&i| records[i].job_id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            Err(CliError::new(
                E_IO_003,
                format!("ambiguous prefix '{job_id}' matches multiple deployments: {candidates}"),
            ))
        }
    }
}
