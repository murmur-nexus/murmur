//! `mur precompile`: storing the compiled forms of `.mur.zip` files for this machine, so the first
//! launch that stages them loads a form instead of compiling.
//!
//! The work is [`Precompiler::precompile`]'s, the same call `mur install` makes; this command only
//! reads files, fans them out and reports. It resolves nothing, touches no artifact store and
//! writes no `murmur.lock`, which is what lets `mur deploy run` run it on a target whose store it
//! has just filled by hand.

use std::path::{Path, PathBuf};

use capsule_runtime::precompile::{Precompiled, Precompiler};
use murmur_artifact::max_artifact_decompressed_bytes;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::error::CliError;

/// What `mur precompile --json` prints, as its one line on stdout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PrecompileReport {
    /// The version of the `mur` that compiled.
    pub(crate) mur_version: String,
    /// The artifact decompression ceiling the forms were keyed under, which a launch must share
    /// to load them.
    pub(crate) decompression_ceiling: u64,
    /// One entry per file argument, in argument order.
    pub(crate) artifacts: Vec<PrecompiledFile>,
}

/// One file of a [`PrecompileReport`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PrecompiledFile {
    /// The path as given on the command line.
    pub(crate) path: String,
    /// The packed manifest's `name`; `None` when the file cannot be read or has no manifest that
    /// parses.
    pub(crate) name: Option<String>,
    /// The packed manifest's `version`, `None` exactly when `name` is.
    pub(crate) version: Option<String>,
    pub(crate) outcome: Precompiled,
}

impl PrecompiledFile {
    /// `name@version`, or the path when the file has no manifest.
    pub(crate) fn label(&self) -> String {
        match (&self.name, &self.version) {
            (Some(name), Some(version)) => format!("{name}@{version}"),
            _ => self.path.clone(),
        }
    }
}

/// The words the text report prints for `outcome`.
fn outcome_text(outcome: Precompiled) -> &'static str {
    match outcome {
        Precompiled::Compiled => "compiled",
        Precompiled::AlreadyStored => "already stored",
        Precompiled::NotWasm => "not wasm",
        Precompiled::Failed => "failed",
    }
}

/// Compiles every file in `files` with one precompiler whose capsule writable root is `workdir`,
/// in parallel, and reports them in argument order. Never fails: a file that cannot be read, or
/// every file when no engine can be built, is [`Precompiled::Failed`].
pub(crate) fn precompile_files(files: &[PathBuf], workdir: Option<&Path>) -> PrecompileReport {
    let precompiler = Precompiler::new(workdir);
    let artifacts = files
        .par_iter()
        .map(|path| precompile_file(precompiler.as_ref(), path))
        .collect();
    PrecompileReport {
        mur_version: env!("CARGO_PKG_VERSION").to_string(),
        decompression_ceiling: max_artifact_decompressed_bytes(),
        artifacts,
    }
}

fn precompile_file(precompiler: Option<&Precompiler>, path: &Path) -> PrecompiledFile {
    let bytes = std::fs::read(path).ok();
    let manifest = bytes
        .as_deref()
        .and_then(|bytes| murmur_artifact::load_manifest_from_artifact_bytes(bytes).ok());
    let outcome = match (precompiler, &bytes, &manifest) {
        (Some(precompiler), Some(bytes), Some(manifest)) => {
            precompiler.precompile(&manifest.name, &manifest.version, bytes)
        }
        _ => Precompiled::Failed,
    };
    PrecompiledFile {
        path: path.to_string_lossy().into_owned(),
        name: manifest.as_ref().map(|manifest| manifest.name.clone()),
        version: manifest.map(|manifest| manifest.version),
        outcome,
    }
}

/// `mur precompile`. Prints the whole report, then exits 1 when any file is `failed`.
pub(crate) fn run_precompile(
    files: &[PathBuf],
    workdir: Option<&Path>,
    json: bool,
) -> Result<(), CliError> {
    let report = precompile_files(files, workdir);
    if json {
        let line = serde_json::to_string(&report)
            .map_err(|e| CliError::new(crate::error::E_IO_003, format!("report: {e}")))?;
        capsule_runtime::report_println!("{line}");
    } else {
        for file in &report.artifacts {
            capsule_runtime::report_println!(
                "{:<14}  {}",
                outcome_text(file.outcome),
                file.label()
            );
        }
    }
    if report
        .artifacts
        .iter()
        .any(|file| file.outcome == Precompiled::Failed)
    {
        // The report above names every failed file, which one `CliError` cannot.
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_report_serialises_in_the_documented_shape() {
        let report = PrecompileReport {
            mur_version: "0.4.0".to_string(),
            decompression_ceiling: 524_288_000,
            artifacts: vec![
                PrecompiledFile {
                    path: "a.mur.zip".to_string(),
                    name: Some("echo-tool".to_string()),
                    version: Some("0.1.0".to_string()),
                    outcome: Precompiled::AlreadyStored,
                },
                PrecompiledFile {
                    path: "b.mur.zip".to_string(),
                    name: None,
                    version: None,
                    outcome: Precompiled::Failed,
                },
            ],
        };
        let line = serde_json::to_string(&report).unwrap();
        assert_eq!(
            line,
            "{\"mur_version\":\"0.4.0\",\"decompression_ceiling\":524288000,\"artifacts\":[\
             {\"path\":\"a.mur.zip\",\"name\":\"echo-tool\",\"version\":\"0.1.0\",\
             \"outcome\":\"already_stored\"},\
             {\"path\":\"b.mur.zip\",\"name\":null,\"version\":null,\"outcome\":\"failed\"}]}"
        );
        assert_eq!(
            serde_json::from_str::<PrecompileReport>(&line).unwrap(),
            report
        );
    }

    #[test]
    fn a_file_is_labelled_by_its_manifest_or_else_its_path() {
        let mut file = PrecompiledFile {
            path: "x/echo.mur.zip".to_string(),
            name: Some("echo-tool".to_string()),
            version: Some("0.1.0".to_string()),
            outcome: Precompiled::Compiled,
        };
        assert_eq!(file.label(), "echo-tool@0.1.0");
        file.name = None;
        file.version = None;
        assert_eq!(file.label(), "x/echo.mur.zip");
    }
}
