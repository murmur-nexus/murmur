//! `mur index`: the schema 2 artifact index for one release's `.mur.zip` assets.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use murmur_artifact::{
    index_directory, merge_indexes, release_tag_error, IndexBuildError, MergeReport,
};

use crate::commands::search::read_index_file;
use crate::error::{CliError, E_IDX_001, E_IDX_003, E_IO_003};
use crate::source::{
    reserved_scope_hint, scope_of_github_repo, split_github_repo, GithubScopeError,
};

/// Index every `.mur.zip` directly in `dir` as release `release` of the GitHub repository `repo`,
/// optionally keeping the versions of the earlier index `merge` that `dir` does not hold, and
/// write the result to `output`.
///
/// Every input is checked before `output` is touched, and the file is replaced by a rename, so
/// a refusal leaves `output` as it was and no temporary file behind. `merge` may name `output`.
pub(crate) fn run_index(
    dir: &Path,
    repo: &str,
    release: &str,
    merge: Option<&Path>,
    output: &Path,
) -> Result<(), CliError> {
    let scope = repo_scope(repo)?;
    if let Some(reason) = release_tag_error(release) {
        return Err(CliError::new(
            E_IDX_003,
            format!("--release '{release}' is not a usable release tag: it {reason}"),
        ));
    }

    let previous = match merge {
        Some(path) => {
            let shown = path.display().to_string();
            let previous = read_index_file(&shown)?;
            if previous.scope != scope {
                return Err(CliError::new(
                    E_IDX_003,
                    format!(
                        "--merge {shown} indexes scope '{}', and --repo {repo} names scope \
                         '{scope}'",
                        previous.scope
                    ),
                ));
            }
            Some((shown, previous))
        }
        None => None,
    };

    let fresh = index_directory(dir, &scope, release).map_err(build_error)?;
    let (index, merged) = match previous {
        Some((shown, previous)) => {
            let (index, report) = merge_indexes(fresh, previous).map_err(build_error)?;
            (index, Some((shown, report)))
        }
        None => (fresh, None),
    };

    write_replacing(output, index.to_json().as_bytes())?;

    let mut summary = format!(
        "Indexed {} artifacts ({} versions) for scope {} into {}",
        index.artifacts.len(),
        index.version_count(),
        index.scope,
        output.display()
    );
    if let Some((shown, MergeReport { kept, replaced })) = merged {
        summary.push_str(&format!(
            "; {kept} versions kept from {shown}, {replaced} replaced"
        ));
    }
    capsule_runtime::report_println!("{summary}");
    Ok(())
}

/// The scope `--repo <owner>/<repo>` names, by the rule `mur install github:<owner>/<repo>` uses.
fn repo_scope(repo: &str) -> Result<String, CliError> {
    let Some((owner, name)) = split_github_repo(repo) else {
        return Err(CliError::new(
            E_IDX_003,
            format!("--repo '{repo}' is not a GitHub repository written <owner>/<repo>"),
        ));
    };
    scope_of_github_repo(owner, name).map_err(|error| match error {
        GithubScopeError::Reserved { scope } => CliError::with_hint(
            E_IDX_003,
            format!("--repo '{repo}' would index under '{scope}', a reserved scope"),
            reserved_scope_hint(),
        ),
        GithubScopeError::InvalidOwner { reason } => CliError::new(
            E_IDX_003,
            format!(
                "--repo '{repo}' names no usable scope: the owner '{owner}' is not a valid \
                 artifact scope (it {reason})"
            ),
        ),
    })
}

fn build_error(error: IndexBuildError) -> CliError {
    let code = match &error {
        IndexBuildError::ReadDir { .. } => E_IO_003,
        IndexBuildError::NoArtifacts { .. } | IndexBuildError::File { .. } => E_IDX_001,
        IndexBuildError::InvalidScope { .. }
        | IndexBuildError::InvalidRelease { .. }
        | IndexBuildError::ScopeMismatch { .. } => E_IDX_003,
    };
    CliError::new(code, error.to_string())
}

/// Write `bytes` to a new file beside `path`, then rename it over `path`. The temporary file is
/// removed on every failure.
fn write_replacing(path: &Path, bytes: &[u8]) -> Result<(), CliError> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            CliError::new(
                E_IO_003,
                format!(
                    "cannot write the index to {}: it names no file",
                    path.display()
                ),
            )
        })?;
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let temp = parent.join(format!(".{file_name}.{}.tmp", std::process::id()));

    let written = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temp, path)
    })();
    written.map_err(|e| {
        let _ = fs::remove_file(&temp);
        CliError::new(
            E_IO_003,
            format!("failed to write the index to {}: {e}", path.display()),
        )
    })
}
