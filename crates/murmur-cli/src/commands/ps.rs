use crate::error::CliError;

use super::deploy_state::load_deployments;

pub(crate) fn run_ps() -> Result<(), CliError> {
    let records = load_deployments()?;

    if records.is_empty() {
        println!("no deployments");
        return Ok(());
    }

    // Header
    println!(
        "{:<38}  {:<12}  {:<12}  {:<10}  URL",
        "DEPLOYMENT_ID", "PROVIDER", "REGION", "STATUS"
    );
    println!("{}", "-".repeat(100));

    for r in &records {
        println!(
            "{:<38}  {:<12}  {:<12}  {:<10}  {}",
            r.deployment_id, r.provider, r.region, r.status, r.url
        );
    }

    Ok(())
}
