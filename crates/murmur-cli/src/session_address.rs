//! The one address vocabulary every command that names a session speaks.
//!
//! `mur trace show`, `mur trace steps`, `mur trace diff`, `mur trace report`, `mur eval show`,
//! `mur eval diff` and `mur run --resume` all name a session the same four ways, and all read
//! the same way when an address names nothing. That holds because [`resolve`] is the only body
//! in the crate that parses an ordinal, matches a suffix, recognises a full `ses_` id or passes
//! a literal path through; every command reaches it through a [`SessionQuery`] carrying the
//! three things the vocabulary itself cannot know.

use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::error::{CliError, E_IO_003};

/// The address an omitted argument means: the most recent session.
const LATEST: &str = "@1";

/// What a command supplies that the address vocabulary cannot know.
pub(crate) struct SessionQuery<'a> {
    /// The per-session file an address resolves to, `trace.jsonl` or `eval.jsonl`.
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

/// The names of every `ses_`-prefixed session directory in `workdir`, unsorted.
///
/// A workdir that does not exist yet holds no sessions rather than being an error, so the
/// callers that go on to report "no sessions found" get to phrase that themselves.
pub(crate) fn ses_entries(workdir: &Path) -> Result<Vec<String>, CliError> {
    if !workdir.exists() || !workdir.is_dir() {
        return Ok(Vec::new());
    }
    let mut entries = Vec::new();
    for entry_res in fs::read_dir(workdir).map_err(|e| {
        CliError::new(
            E_IO_003,
            format!("failed to read {}: {e}", workdir.display()),
        )
    })? {
        let entry = entry_res.map_err(|e| {
            CliError::new(
                E_IO_003,
                format!("failed to read entry in {}: {e}", workdir.display()),
            )
        })?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("ses_") && entry.path().is_dir() {
            entries.push(name);
        }
    }
    Ok(entries)
}

/// The record file a session address names, under `workdir`.
///
/// Four forms, and nothing else: an `@N` ordinal counting back from the most recent session, a
/// literal path, a full `ses_` id, or a 4+-character case-insensitive suffix of one. An omitted
/// address is [`LATEST`] and takes the ordinal arm, so `mur trace show` and `mur trace show @1`
/// resolve through the same code rather than through two derivations of "latest" that a later
/// edit could pull apart.
///
/// Session ids sort chronologically, which is what makes ordinals a plain sort of the directory
/// names.
pub(crate) fn resolve(
    address: Option<&str>,
    workdir: &Path,
    query: &SessionQuery<'_>,
) -> Result<PathBuf, CliError> {
    let address = address.unwrap_or(LATEST);

    if let Some(n_str) = address.strip_prefix('@') {
        let n: usize = n_str.parse().map_err(|_| {
            query.err(format!(
                "invalid ordinal '{address}' — expected @1, @2, ..."
            ))
        })?;
        if n == 0 {
            return Err(query.err("ordinal must be @1 or higher"));
        }
        let mut entries = ses_entries(workdir)?;
        if entries.is_empty() {
            // Unlabelled: an empty workdir is not a fault of the argument that named it.
            return Err(CliError::new(
                query.code,
                format!("no sessions found in workdir at {}", workdir.display()),
            ));
        }
        entries.sort();
        entries.reverse(); // descending: most recent first
        if n > entries.len() {
            return Err(query.err(format!(
                "@{n} is out of range — workdir has {} session{}",
                entries.len(),
                if entries.len() == 1 { "" } else { "s" }
            )));
        }
        return Ok(workdir.join(&entries[n - 1]).join(query.record_file));
    }

    // Backward compatibility: treat as a literal path if it looks like one.
    if address.contains('/') || address.ends_with(".jsonl") {
        return Ok(PathBuf::from(address));
    }

    // Full session ID: "ses_" prefix + 32-char hex = 36 chars total.
    if address.starts_with("ses_") && address.len() == 36 {
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
