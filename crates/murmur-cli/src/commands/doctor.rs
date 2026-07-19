use capsule_runtime::ArtifactRequest;
use murmur_artifact::{current_platform, load_runtime_manifest, resolve_manifest_path, LocalRegistry};

use crate::commands::install::find_project_root;
use crate::commands::run::{artifact_presence, ArtifactPresence};
use crate::commands::runtime_manifest_error_to_cli;
use crate::error::CliError;

/// Check every artifact the current project declares against the stores a session
/// resolves from. The checklist is the manifest — editing `murmur.yaml` changes what
/// is checked, with no change here.
pub(crate) fn run_doctor() -> Result<(), CliError> {
    let project_root = find_project_root().map_err(|mut error| {
        error.hint = Some("run `mur doctor` from inside a project directory".to_string());
        error
    })?;
    let manifest_path = resolve_manifest_path(&project_root);
    let runtime_manifest =
        load_runtime_manifest(&manifest_path).map_err(runtime_manifest_error_to_cli)?;

    let project_registry = LocalRegistry::new(project_root.join(".murmur").join("artifacts"));
    let global_registry = LocalRegistry::from_default_home().map_err(CliError::from)?;
    let platform = current_platform();

    println!("Checking {} for {platform}...", manifest_path.display());

    // Align every check line on the widest "name@version" reference string.
    let col_width = runtime_manifest
        .artifacts
        .iter()
        .map(|artifact| artifact.name.len() + 1 + artifact.version.len()) // +1 for '@'
        .max()
        .unwrap_or(0);

    let mut total_pass: u32 = 0;
    let mut missing: Vec<(String, String)> = Vec::new();

    for artifact in &runtime_manifest.artifacts {
        let request = ArtifactRequest {
            name: artifact.name.clone(),
            version: artifact.version.clone(),
            runtime: artifact.runtime.clone(),
            source: artifact.source.clone(),
        };
        let ref_str = format!("{}@{}", artifact.name, artifact.version);

        match artifact_presence(&project_registry, &global_registry, &request, platform) {
            ArtifactPresence::LocalSource => {
                println!("  \u{2713}  {ref_str:<col_width$}   local source");
                total_pass += 1;
            }
            ArtifactPresence::Installed => {
                println!("  \u{2713}  {ref_str:<col_width$}   {platform}");
                total_pass += 1;
            }
            ArtifactPresence::Missing => {
                println!("  \u{2717}  {ref_str:<col_width$}   {platform}   \u{2014} missing");
                missing.push((artifact.name.clone(), artifact.version.clone()));
            }
        }
    }

    println!();

    if missing.is_empty() {
        println!("All checks passed.");
        return Ok(());
    }

    let total_fail = missing.len();
    let ps = if total_pass == 1 { "" } else { "s" };
    let es = if total_fail == 1 { "" } else { "s" };
    println!("{total_pass} check{ps} passed, {total_fail} error{es} found.");
    println!();

    for (name, version) in &missing {
        println!("Fix: mur install {name}@{version}");
    }

    // Exit non-zero so `mur doctor` can be used in CI pre-flight checks.
    // std::process::exit terminates the process immediately; no destructors run,
    // which is acceptable here because we are done with all I/O.
    std::process::exit(1);
}
