use crate::error::{CliError, E_IO_003};

use super::deploy_state::{deploy_keys_dir, deploy_staging_dir, remove_deployment};

pub(crate) fn run_destroy(deployment_id: &str) -> Result<(), CliError> {
    let record = remove_deployment(deployment_id)?.ok_or_else(|| {
        CliError::new(
            E_IO_003,
            format!("no deployment found with id '{deployment_id}'; check `mur ps`"),
        )
    })?;

    // Clean up under the record's *full* id, never the argument. `remove_deployment` accepts an
    // unambiguous prefix, so the argument is frequently shorter than the directory names — using
    // it here would remove nothing and silently leave the private key and the uploaded staging
    // tree on disk for a deployment that no longer exists.
    let id = &record.deployment_id;
    if let Ok(key_dir) = deploy_keys_dir(id) {
        let _ = std::fs::remove_dir_all(key_dir);
    }
    if let Ok(staging_dir) = deploy_staging_dir(id) {
        let _ = std::fs::remove_dir_all(staging_dir);
    }

    eprintln!("destroyed {id} ({})", record.ip);
    Ok(())
}
