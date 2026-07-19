use crate::error::{CliError, E_IO_003};

use super::deploy_state::{deploy_keys_dir, remove_deployment};

pub(crate) fn run_destroy(job_id: &str) -> Result<(), CliError> {
    let record = remove_deployment(job_id)?.ok_or_else(|| {
        CliError::new(
            E_IO_003,
            format!("no deployment found with job_id '{job_id}'; check `mur ps`"),
        )
    })?;

    // Clean up any locally-generated SSH key from older deploys.
    if let Ok(key_dir) = deploy_keys_dir(job_id) {
        let _ = std::fs::remove_dir_all(key_dir);
    }

    eprintln!("destroyed {job_id} ({})", record.ip);
    Ok(())
}
