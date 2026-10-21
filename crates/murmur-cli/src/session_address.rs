//! The one address vocabulary every command that names a session speaks.
//!
//! `mur trace show`, `mur trace steps`, `mur trace diff`, `mur trace report`, `mur eval show`,
//! `mur eval diff` and `mur run --resume` all name a session the same four ways, and all read
//! the same way when an address names nothing. That holds because [`resolve`] is the only body
//! any of them reaches to parse an ordinal, match a suffix, recognise a full `ses_` id or pass a
//! literal path through; every command reaches it through a [`SessionQuery`] carrying the three
//! things the vocabulary itself cannot know. `mur trace list` prints the candidates in the order
//! ordinals count them, through [`sessions_newest_first`]. `mur trace rm` speaks the vocabulary
//! less the literal path, which it refuses by [`is_literal_path`], and accepts a full id by
//! [`is_full_session_id`] while either half of that session is left.
//!
//! The candidates are the sessions of one **workdir root** — the directory a run was given as
//! `--workdir`, or `<manifest-dir>/workdir` — whose records directories sit in that root's records
//! group under `~/.murmur/records`, outside every workdir a capsule's commands can write.
//!
//! The commands that need a capsule running right now — `mur watch`, `mur cancel` — spell an
//! address the same way but resolve it against a different candidate set, and go through
//! [`crate::live_address`] instead. A literal path is a spelling only this module accepts: it
//! names a recorded session directory, which says nothing about whether a process is running.

use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::error::{CliError, E_IO_003};

/// The address an omitted argument means: the most recent session.
const LATEST: &str = "@1";

/// What a command supplies that the address vocabulary cannot know.
pub(crate) struct SessionQuery<'a> {
    /// The file an address resolves to beneath a session's records directory, `trace.jsonl` or
    /// `hooks/eval.jsonl`.
    pub(crate) record_file: &'a str,
    /// The diagnostic code every addressing failure carries.
    pub(crate) code: &'static str,
    /// The argument an addressing failure names, e.g. `--resume` or `before`. `None` where the
    /// command has one session argument and naming it would say nothing.
    pub(crate) label: Option<&'a str>,
}

impl SessionQuery<'_> {
    fn err(&self, message: impl std::fmt::Display) -> CliError {
        match self.label {
            Some(label) => CliError::new(self.code, format!("{label}: {message}")),
            None => CliError::new(self.code, message.to_string()),
        }
    }
}

/// The records group of `workdir`, the workdir root a reader was pointed at: where every session
/// it ran keeps its records directory. `E-IO-004` when `HOME` cannot hold `~/.murmur`.
pub(crate) fn records_group(workdir: &Path) -> Result<PathBuf, CliError> {
    capsule_runtime::records_group(workdir).map_err(CliError::from)
}

/// The names of every `ses_`-prefixed session directory in `group`, a records group, unsorted.
///
/// A group that does not exist yet holds no sessions rather than being an error, so the
/// callers that go on to report "no sessions found" get to phrase that themselves.
pub(crate) fn ses_entries(group: &Path) -> Result<Vec<String>, CliError> {
    if !group.exists() || !group.is_dir() {
        return Ok(Vec::new());
    }
    let mut entries = Vec::new();
    for entry_res in fs::read_dir(group)
        .map_err(|e| CliError::new(E_IO_003, format!("failed to read {}: {e}", group.display())))?
    {
        let entry = entry_res.map_err(|e| {
            CliError::new(
                E_IO_003,
                format!("failed to read entry in {}: {e}", group.display()),
            )
        })?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("ses_") && entry.path().is_dir() {
            entries.push(name);
        }
    }
    Ok(entries)
}

/// The session directory names in `group`, most recent first: the order `@N` ordinals count in,
/// so entry `i` is the session `@(i+1)` names. Session ids are uuid v7 and sort chronologically,
/// which makes the order a plain sort of the names.
pub(crate) fn sessions_newest_first(group: &Path) -> Result<Vec<String>, CliError> {
    let mut entries = ses_entries(group)?;
    entries.sort();
    entries.reverse();
    Ok(entries)
}

/// Whether `address` is spelled as a literal path rather than as a session address: anything
/// that is not an ordinal and contains `/` or ends in `.jsonl`.
pub(crate) fn is_literal_path(address: &str) -> bool {
    !address.starts_with('@') && (address.contains('/') || address.ends_with(".jsonl"))
}

/// Whether `address` is spelled as a full session id: `ses_` and 32 more characters, 36 in all.
/// The characters themselves are not checked.
pub(crate) fn is_full_session_id(address: &str) -> bool {
    address.starts_with("ses_") && address.len() == 36
}

/// The record file a session address names, among the sessions of the workdir root `workdir`.
///
/// Four forms, and nothing else: an `@N` ordinal counting back from the most recent session, a
/// literal path, a full `ses_` id, or a 4+-character case-insensitive suffix of one. A literal
/// path is read as given and consults no records group; every other form reads `workdir`'s. An omitted
/// address is [`LATEST`] and takes the ordinal arm, so `mur trace show` and `mur trace show @1`
/// resolve through the same code rather than through two derivations of "latest" that a later
/// edit could pull apart. Ordinals count [`sessions_newest_first`].
pub(crate) fn resolve(
    address: Option<&str>,
    workdir: &Path,
    query: &SessionQuery<'_>,
) -> Result<PathBuf, CliError> {
    let address = address.unwrap_or(LATEST);

    // A literal path is taken verbatim; the records group is not consulted.
    if is_literal_path(address) {
        return Ok(PathBuf::from(address));
    }
    let root = workdir;
    let workdir = records_group(root)?;
    let workdir = workdir.as_path();

    if let Some(n_str) = address.strip_prefix('@') {
        let n: usize = n_str.parse().map_err(|_| {
            query.err(format!(
                "invalid ordinal '{address}' — expected @1, @2, ..."
            ))
        })?;
        if n == 0 {
            return Err(query.err("ordinal must be @1 or higher"));
        }
        let entries = sessions_newest_first(workdir)?;
        if entries.is_empty() {
            // Unlabelled: an empty workdir is not a fault of the argument that named it.
            return Err(CliError::new(
                query.code,
                format!(
                    "no sessions found in workdir at {} (records in {})",
                    root.display(),
                    workdir.display()
                ),
            ));
        }
        if n > entries.len() {
            return Err(query.err(format!(
                "@{n} is out of range — workdir has {} session{}",
                entries.len(),
                if entries.len() == 1 { "" } else { "s" }
            )));
        }
        return Ok(workdir.join(&entries[n - 1]).join(query.record_file));
    }

    if is_full_session_id(address) {
        let path = workdir.join(address).join(query.record_file);
        if !path.exists() {
            return Err(query.err(format!(
                "session {} not found in {}",
                address,
                workdir.display()
            )));
        }
        return Ok(path);
    }

    // Suffix matching (case-insensitive).
    let suffix_lower = address.to_lowercase();
    let entries = ses_entries(workdir)?;
    let mut matches: Vec<String> = entries
        .into_iter()
        .filter(|e| e.to_lowercase().ends_with(&suffix_lower))
        .collect();
    match matches.len() {
        0 => Err(query.err(format!(
            "no session found matching suffix '{}' in {}",
            address,
            workdir.display()
        ))),
        1 => Ok(workdir.join(&matches[0]).join(query.record_file)),
        n => {
            matches.sort();
            Err(query.err(format!(
                "ambiguous: '{}' matches {} sessions — provide more characters\n{}",
                address,
                n,
                matches
                    .iter()
                    .map(|m| format!("  {m}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            )))
        }
    }
}
