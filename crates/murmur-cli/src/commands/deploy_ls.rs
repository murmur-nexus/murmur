use crate::error::CliError;

use super::deploy_state::load_deployments;

pub(crate) fn run_deploy_ls() -> Result<(), CliError> {
    let records = load_deployments()?;

    if records.is_empty() {
        capsule_runtime::report_println!("no deployments");
        return Ok(());
    }

    // Header
    capsule_runtime::report_println!(
        "{:<38}  {:<12}  {:<12}  {:<10}  URL",
        "DEPLOYMENT_ID",
        "PROVIDER",
        "REGION",
        "STATUS"
    );
    capsule_runtime::report_println!("{}", "-".repeat(100));

    for r in &records {
        capsule_runtime::report_println!(
            "{:<38}  {:<12}  {:<12}  {:<10}  {}",
            r.deployment_id,
            r.provider,
            r.region,
            r.status,
            r.url
        );
    }

    Ok(())
}
