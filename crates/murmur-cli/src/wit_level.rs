//! What `mur` says when an artifact a launch stages does not fit the WIT level it serves.
//!
//! The runtime decides the refusal ([`RuntimeError::WitLevelMismatch`]) and has no index to name
//! a version that would fit. This module asks the artifact's own source for one, and only once a
//! launch has been refused for exactly that reason.

use capsule_runtime::RuntimeError;
use murmur_artifact::{current_platform, ArtifactName, WitLevel};

use crate::error::CliError;
use crate::source::{newest_fitting, Sources};

/// The `PACKAGE  VERSION` table of `level`, one row per package in ascending order, each column
/// padded to its widest cell plus two spaces. `mur version` and `mur doctor` print it.
pub(crate) fn package_table(level: &WitLevel) -> Vec<String> {
    let width = level
        .packages
        .iter()
        .map(|(package, _)| package.len())
        .chain(std::iter::once("PACKAGE".len()))
        .max()
        .unwrap_or(0)
        + 2;
    std::iter::once(("PACKAGE", "VERSION"))
        .chain(
            level
                .packages
                .iter()
                .map(|(package, version)| (package.as_str(), version.as_str())),
        )
        .map(|(package, version)| format!("{package:<width$}{version}"))
        .collect()
}

/// The hint a level mismatch carries wherever no source has been asked: where to look for a
/// version that fits, and where the level's packages are listed.
pub(crate) fn search_hint(name: &str, level: u64) -> String {
    format!(
        "run `mur search {name}` for a version that fits level {level}, pin it in murmur.yaml and \
         run `mur install`; `mur version` lists level {level}'s packages"
    )
}

/// The [`CliError`] for an error `stage_session` returned. A level mismatch on a `<scope>/<name>`
/// artifact asks that scope's source for the newest version of it that fits the served level and
/// names it in the hint; every other error is [`CliError::from`] unchanged.
pub(crate) fn staging_error(error: RuntimeError) -> CliError {
    let fitting = match &error {
        RuntimeError::WitLevelMismatch { name, level, .. } => Some(fitting_hint(name, *level)),
        _ => None,
    };
    let mut cli_error = CliError::from(error);
    if let Some(hint) = fitting {
        cli_error.hint = Some(hint);
    }
    cli_error
}

/// The hint naming the newest version of `full_name` its source's index lists that fits `level`,
/// or saying why none can be named.
fn fitting_hint(full_name: &str, level: u64) -> String {
    let rebuild = format!("rebuild it against level {level}'s packages (`mur version` lists them)");
    let Ok(name) = ArtifactName::parse(full_name) else {
        return search_hint(full_name, level);
    };
    if name.is_local() {
        return format!(
            "{full_name} was installed from a file, so no source's index can name a version of it \
             that fits level {level}; {rebuild}, then run `mur install <file>` on the rebuilt file"
        );
    }
    let sources = match Sources::load() {
        Ok(sources) => sources,
        Err(error) => {
            return format!(
                "the configured sources could not be read ({}), so no fitting version of \
                 {full_name} can be named; {rebuild}",
                error.message
            )
        }
    };
    let Some(source) = sources.for_scope(name.scope()) else {
        return format!(
            "no source serves the '{}' scope, so no index can name a version of {full_name} that \
             fits level {level}; add the repository it publishes from under registry.sources and \
             run `mur search {full_name}`, or {rebuild}",
            name.scope()
        );
    };
    let label = source.label();
    let published = match source.index() {
        Ok(published) => published,
        Err(error) => {
            return format!(
                "the index of {label} could not be read ({}), so no fitting version can be named; \
                 run `mur search {full_name}` once it answers, or {rebuild}",
                error.reason()
            )
        }
    };
    let fitting = published
        .index
        .artifacts
        .iter()
        .find(|artifact| artifact.name == name.name())
        .and_then(|artifact| newest_fitting(artifact, current_platform()));
    match fitting {
        Some(version) => format!(
            "{full_name}@{} is the newest version in {label}'s index that fits level {level}; pin \
             it in murmur.yaml and run `mur install`",
            version.version
        ),
        None => format!(
            "no version of {full_name} in {label}'s index fits level {level}; rebuild it against \
             level {level}'s packages (`mur version` lists them)"
        ),
    }
}

#[cfg(test)]
mod tests {
    use capsule_runtime::served_wit_level;

    use super::*;

    #[test]
    fn staging_error_passes_every_other_error_through() {
        let error = staging_error(RuntimeError::DriverNotConfigured);
        assert_eq!(error.code, crate::error::E_RUN_005);
    }

    #[test]
    fn a_local_artifact_is_named_as_installed_from_a_file_without_asking_a_source() {
        let error = staging_error(RuntimeError::WitLevelMismatch {
            name: "local/old-driver".to_string(),
            version: "1.0.0".to_string(),
            level: served_wit_level().level,
            mismatched: Vec::new(),
        });
        assert_eq!(error.code, crate::error::E_WIT_LEVEL_001);
        let hint = error.hint.unwrap();
        assert!(
            hint.starts_with("local/old-driver was installed from a file"),
            "{hint}"
        );
        assert!(hint.contains("mur install"), "{hint}");
    }
}
