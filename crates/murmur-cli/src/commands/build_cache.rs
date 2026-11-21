//! `mur build-cache`: importing a Cargo target folder the operator built on the host into the
//! build cache store, and listing what the store holds.
//!
//! The store and its layout are [`capsule_runtime::build_cache`]'s; this command is the only writer
//! of the store, and staging the only reader.

use std::path::{Path, PathBuf};

use capsule_runtime::build_cache::{self, AddError};
use clap::Subcommand;

use crate::error::{CliError, E_IO_001, E_IO_002, E_IO_003, E_IO_005};

#[derive(Debug, Subcommand)]
pub(crate) enum BuildCacheCommand {
    /// Import a Cargo target folder built on this host, keyed by the SHA-256 of the Cargo.lock it
    /// was built from. Every capsule whose work folder's root holds a byte-identical Cargo.lock
    /// then starts with a private copy of it as CARGO_TARGET_DIR. Replaces an entry for the same
    /// lockfile whole
    Add {
        /// The Cargo.lock the target folder was built from
        #[arg(long, value_name = "PATH")]
        lockfile: PathBuf,
        /// The target folder: one Cargo wrote CACHEDIR.TAG or .rustc_info.json into
        #[arg(value_name = "TARGET")]
        source: PathBuf,
    },
    /// List the entries of the build cache store
    List,
}

/// `mur build-cache add` and `mur build-cache list`.
pub(crate) fn run_build_cache(command: BuildCacheCommand) -> Result<(), CliError> {
    let store = build_cache::build_cache_store_root()
        .map_err(|reason| CliError::new(crate::error::E_IO_004, reason))?;
    match command {
        BuildCacheCommand::Add { lockfile, source } => run_add(&store, &lockfile, &source),
        BuildCacheCommand::List => run_list(&store),
    }
}

fn run_add(store: &Path, lockfile: &Path, source: &Path) -> Result<(), CliError> {
    let murmur_home = store.parent().unwrap_or(store);
    let added = build_cache::add(store, murmur_home, lockfile, source).map_err(add_error)?;
    capsule_runtime::report_println!(
        "added {} ({} files, {} bytes) from {}",
        added.record.lockfile_sha256,
        added.record.files,
        added.record.bytes,
        added.record.source
    );
    Ok(())
}

/// The code each refusal and failure of `add` is reported under.
fn add_error(error: AddError) -> CliError {
    let code = match &error {
        AddError::LockfileMissing { .. } | AddError::SourceMissing { .. } => E_IO_001,
        AddError::LockfileNotRegular { .. }
        | AddError::SourceNotTarget { .. }
        | AddError::SourceInsideMurmurHome { .. } => E_IO_005,
        AddError::Unreadable { source, .. } | AddError::Io { source, .. }
            if source.kind() == std::io::ErrorKind::PermissionDenied =>
        {
            E_IO_002
        }
        AddError::Unreadable { .. } | AddError::Io { .. } => E_IO_003,
    };
    CliError::new(code, format!("mur build-cache add: {error}"))
}

fn run_list(store: &Path) -> Result<(), CliError> {
    let entries = build_cache::list(store).map_err(|error| {
        CliError::new(
            E_IO_003,
            format!(
                "cannot list the build cache store {}: {error}",
                store.display()
            ),
        )
    })?;
    if entries.is_empty() {
        capsule_runtime::report_println!("no build cache entries in {}", store.display());
        return Ok(());
    }
    for entry in entries {
        match entry.record {
            Ok(record) => capsule_runtime::report_println!(
                "{}  {} files  {} bytes  {}  {}  {}",
                entry.key,
                record.files,
                record.bytes,
                record.rustc.as_deref().unwrap_or("-"),
                record.added_at,
                record.source
            ),
            Err(reason) => {
                capsule_runtime::report_println!("{}  unreadable entry record: {reason}", entry.key)
            }
        }
    }
    Ok(())
}
