//! The one address vocabulary every command that names a session speaks.
//!
//! `mur trace show`, `mur trace steps`, `mur trace diff`, `mur trace report`, `mur eval show`,
//! `mur eval diff` and `mur run --resume` all name a session the same four ways, and all read
//! the same way when an address names nothing. That holds because [`resolve_kind`] is the only
//! body any of them reaches to parse an ordinal, match a suffix, recognise a full id or pass a
//! literal path through; every command reaches it through a [`SessionQuery`] carrying the three
//! things the vocabulary itself cannot know. `mur eval diff --run` names an eval-run record
//! (`evr_<id>.json`, beside the sessions) the same way, through [`EVAL_RUN`]. `mur trace list`
//! prints the candidates in the order ordinals count them, through [`sessions_newest_first`].
//! `mur trace rm` speaks the vocabulary less the literal path, which it refuses by
//! [`is_literal_path`], and accepts a full id by [`is_full_session_id`] while either half of that
//! session is left.
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

/// What an address names: the candidates of one records group and how messages call them.
pub(crate) struct RecordKind {
    /// The name prefix every candidate carries, which a full id starts with and 32 characters
    /// follow.
    prefix: &'static str,
    /// `None` when a candidate is a directory; else the extension of a candidate file, which its
    /// id leaves off.
    file_extension: Option<&'static str>,
    /// The suffix that makes an address a literal path even without a `/`.
    literal_suffix: &'static str,
    noun: &'static str,
    nouns: &'static str,
    /// The fewest characters a suffix address may have.
    min_suffix: usize,
}

/// A session: a `ses_` records directory.
pub(crate) const SESSION: RecordKind = RecordKind {
    prefix: "ses_",
    file_extension: None,
    literal_suffix: ".jsonl",
    noun: "session",
    nouns: "sessions",
    min_suffix: 0,
};

/// An eval run: an `evr_<id>.json` record `mur eval run` writes.
pub(crate) const EVAL_RUN: RecordKind = RecordKind {
    prefix: "evr_",
    file_extension: Some(".json"),
    literal_suffix: ".json",
    noun: "eval run",
    nouns: "eval runs",
    min_suffix: 4,
};

impl RecordKind {
    /// The file the candidate `id` resolves to in `group`.
    fn record_path(&self, group: &Path, id: &str, record_file: &str) -> PathBuf {
        match self.file_extension {
            None => group.join(id).join(record_file),
            Some(extension) => group.join(format!("{id}{extension}")),
        }
    }

    fn is_full_id(&self, address: &str) -> bool {
        address.starts_with(self.prefix) && address.len() == self.prefix.len() + 32
    }
}

/// What a command supplies that the address vocabulary cannot know.
pub(crate) struct SessionQuery<'a> {
    /// The file an address resolves to beneath a session's records directory, `trace.jsonl` or
    /// `hooks/eval.jsonl`. Unread for a kind whose candidates are files.
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
    entries(group, &SESSION)
}

/// The ids of every candidate of `kind` in `group`, unsorted; none when `group` does not exist.
fn entries(group: &Path, kind: &RecordKind) -> Result<Vec<String>, CliError> {
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
        if !name.starts_with(kind.prefix) {
            continue;
        }
        let path = entry.path();
        match kind.file_extension {
            None if path.is_dir() => entries.push(name),
            Some(extension) if path.is_file() => {
                if let Some(id) = name.strip_suffix(extension) {
                    entries.push(id.to_string());
                }
            }
            _ => {}
        }
    }
    Ok(entries)
}

/// The session directory names in `group`, most recent first: the order `@N` ordinals count in,
/// so entry `i` is the session `@(i+1)` names. Session ids are uuid v7 and sort chronologically,
/// which makes the order a plain sort of the names.
pub(crate) fn sessions_newest_first(group: &Path) -> Result<Vec<String>, CliError> {
    newest_first(group, &SESSION)
}

/// The ids of `kind` in `group`, most recent first. Every kind's ids are uuid v7.
fn newest_first(group: &Path, kind: &RecordKind) -> Result<Vec<String>, CliError> {
    let mut entries = entries(group, kind)?;
    entries.sort();
    entries.reverse();
    Ok(entries)
}

/// Whether `address` is spelled as a literal path rather than as a session address: anything
/// that is not an ordinal and contains `/` or ends in `.jsonl`.
pub(crate) fn is_literal_path(address: &str) -> bool {
    is_literal_path_of(address, &SESSION)
}

fn is_literal_path_of(address: &str, kind: &RecordKind) -> bool {
    !address.starts_with('@') && (address.contains('/') || address.ends_with(kind.literal_suffix))
}

/// Whether `address` is spelled as a full session id: `ses_` and 32 more characters, 36 in all.
/// The characters themselves are not checked.
pub(crate) fn is_full_session_id(address: &str) -> bool {
    SESSION.is_full_id(address)
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
    resolve_kind(address, workdir, &SESSION, query)
}

/// The record file an address names among the candidates of `kind` in the workdir root
/// `workdir`'s records group: [`resolve`] for any kind. A file kind also refuses a suffix
/// shorter than its `min_suffix`.
pub(crate) fn resolve_kind(
    address: Option<&str>,
    workdir: &Path,
    kind: &RecordKind,
    query: &SessionQuery<'_>,
) -> Result<PathBuf, CliError> {
    let address = address.unwrap_or(LATEST);

    // A literal path is taken verbatim; the records group is not consulted.
    if is_literal_path_of(address, kind) {
        return Ok(PathBuf::from(address));
    }
    let group = records_group(workdir)?;
    resolve_in_group(address, workdir, &group, kind, query)
}

/// The non-literal arms of [`resolve_kind`], against `group`, the records group of the workdir
/// root `root`.
fn resolve_in_group(
    address: &str,
    root: &Path,
    group: &Path,
    kind: &RecordKind,
    query: &SessionQuery<'_>,
) -> Result<PathBuf, CliError> {
    let workdir = group;

    if let Some(n_str) = address.strip_prefix('@') {
        let n: usize = n_str.parse().map_err(|_| {
            query.err(format!(
                "invalid ordinal '{address}' — expected @1, @2, ..."
            ))
        })?;
        if n == 0 {
            return Err(query.err("ordinal must be @1 or higher"));
        }
        let entries = newest_first(workdir, kind)?;
        if entries.is_empty() {
            // Unlabelled: an empty workdir is not a fault of the argument that named it.
            return Err(CliError::new(
                query.code,
                format!(
                    "no {} found in workdir at {} (records in {})",
                    kind.nouns,
                    root.display(),
                    workdir.display()
                ),
            ));
        }
        if n > entries.len() {
            return Err(query.err(format!(
                "@{n} is out of range — workdir has {} {}",
                entries.len(),
                if entries.len() == 1 {
                    kind.noun
                } else {
                    kind.nouns
                }
            )));
        }
        return Ok(kind.record_path(workdir, &entries[n - 1], query.record_file));
    }

    if kind.is_full_id(address) {
        let path = kind.record_path(workdir, address, query.record_file);
        if !path.exists() {
            return Err(query.err(format!(
                "{} {} not found in {}",
                kind.noun,
                address,
                workdir.display()
            )));
        }
        return Ok(path);
    }

    if address.chars().count() < kind.min_suffix {
        return Err(query.err(format!(
            "'{address}' is too short a suffix — give {} or more characters",
            kind.min_suffix
        )));
    }

    // Suffix matching (case-insensitive).
    let suffix_lower = address.to_lowercase();
    let entries = entries(workdir, kind)?;
    let mut matches: Vec<String> = entries
        .into_iter()
        .filter(|e| e.to_lowercase().ends_with(&suffix_lower))
        .collect();
    match matches.len() {
        0 => Err(query.err(format!(
            "no {} found matching suffix '{}' in {}",
            kind.noun,
            address,
            workdir.display()
        ))),
        1 => Ok(kind.record_path(workdir, &matches[0], query.record_file)),
        n => {
            matches.sort();
            Err(query.err(format!(
                "ambiguous: '{}' matches {} {} — provide more characters\n{}",
                address,
                n,
                kind.nouns,
                matches
                    .iter()
                    .map(|m| format!("  {m}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const QUERY: SessionQuery<'static> = SessionQuery {
        record_file: "",
        code: "E-TEST",
        label: Some("--run"),
    };

    /// A records group holding two eval runs, a session and a stray file; the runs newest first.
    fn group() -> (tempfile::TempDir, [&'static str; 2]) {
        let dir = tempfile::tempdir().unwrap();
        let runs = [
            "evr_0192bbbb0000700080000000beef1234",
            "evr_0192aaaa0000700080000000cafe1234",
        ];
        for run in runs {
            fs::write(dir.path().join(format!("{run}.json")), "{}").unwrap();
        }
        fs::create_dir(dir.path().join("ses_0192cccc0000700080000000beef1234")).unwrap();
        fs::write(dir.path().join("evr_notes.txt"), "").unwrap();
        (dir, runs)
    }

    fn resolve_run(address: &str, group: &Path) -> Result<PathBuf, String> {
        resolve_in_group(address, Path::new("/w"), group, &EVAL_RUN, &QUERY)
            .map_err(|e| e.to_string())
    }

    #[test]
    fn an_ordinal_counts_back_from_the_newest() {
        let (dir, runs) = group();
        let path = |run: &str| dir.path().join(format!("{run}.json"));
        assert_eq!(resolve_run("@1", dir.path()).unwrap(), path(runs[0]));
        assert_eq!(resolve_run("@2", dir.path()).unwrap(), path(runs[1]));
        let out_of_range = resolve_run("@3", dir.path()).unwrap_err();
        assert!(
            out_of_range.contains("--run: @3 is out of range — workdir has 2 eval runs"),
            "{out_of_range}"
        );
        assert!(resolve_run("@0", dir.path()).is_err());
        let empty = tempfile::tempdir().unwrap();
        let none = resolve_run("@1", empty.path()).unwrap_err();
        assert!(none.contains("no eval runs found in workdir"), "{none}");
    }

    #[test]
    fn a_suffix_and_a_full_id_name_one_run() {
        let (dir, runs) = group();
        let path = |run: &str| dir.path().join(format!("{run}.json"));
        assert_eq!(resolve_run("CAFE1234", dir.path()).unwrap(), path(runs[1]));
        assert_eq!(resolve_run(runs[0], dir.path()).unwrap(), path(runs[0]));
        let ambiguous = resolve_run("1234", dir.path()).unwrap_err();
        assert!(ambiguous.contains("matches 2 eval runs"), "{ambiguous}");
        let short = resolve_run("afe", dir.path()).unwrap_err();
        assert!(short.contains("too short"), "{short}");
        let missing = resolve_run("evr_0192aaaa0000700080000000dead1234", dir.path()).unwrap_err();
        assert!(missing.contains("eval run evr_"), "{missing}");
        assert!(missing.contains("not found"), "{missing}");
    }

    #[test]
    fn a_session_suffix_has_no_minimum_and_names_sessions_only() {
        let (dir, _) = group();
        let found = resolve_in_group(
            "234",
            Path::new("/w"),
            dir.path(),
            &SESSION,
            &SessionQuery {
                record_file: "trace.jsonl",
                code: "E-TEST",
                label: None,
            },
        )
        .unwrap();
        assert_eq!(
            found,
            dir.path()
                .join("ses_0192cccc0000700080000000beef1234")
                .join("trace.jsonl")
        );
    }

    #[test]
    fn a_path_is_literal() {
        assert!(is_literal_path_of("./x/evr_1.json", &EVAL_RUN));
        assert!(is_literal_path_of("evr_1.json", &EVAL_RUN));
        assert!(!is_literal_path_of("@1", &EVAL_RUN));
        assert!(!is_literal_path_of("beef", &EVAL_RUN));
        assert!(is_literal_path("trace.jsonl"));
        assert!(!is_literal_path("evr_1.json"));
    }
}
