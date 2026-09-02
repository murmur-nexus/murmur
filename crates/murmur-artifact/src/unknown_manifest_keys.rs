//! `W-SEC-019`: keys `murmur.yaml` declares that this build does not recognize.
//!
//! Every `Raw*` deserialization struct in [`crate::runtime_manifest`] carries a
//! `#[serde(flatten)]` overflow map, so a key no field claims is captured during parse rather than
//! dropped. This module turns those captures into the lines an operator reads. Detection and
//! message construction live here; emission happens at the CLI seam, on the `W-SEC-004` precedent
//! in `murmur-cli`'s `build` command.
//!
//! Nothing here refuses anything. `#[serde(deny_unknown_fields)]` is set nowhere in the parser,
//! because an older `mur` that refuses a key a newer one added cannot load a fleet's manifests at
//! all — which turns every optional-key addition into a breaking change.

use crate::security_warnings::{security_warning_link, W_SEC_019};
use crate::RuntimeManifest;

/// One key the manifest declared and this build does not recognize.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownManifestKey {
    /// The key exactly as the manifest spelled it, without its containing path.
    pub key: String,
    /// Dotted path of the block that held the key, empty for a top-level key. A nested capability
    /// block reads `capabilities.filesystem`; a per-artifact one reads
    /// `artifacts[0].capabilities.shell`; a list element carries its index
    /// (`capabilities.shell.interpreter_runtime[0]`).
    pub block_path: String,
    /// The recognized key of that same block nearest [`Self::key`] by edit distance, when one is
    /// within [`suggestion_threshold`]. `None` means no recognized key of the block resembles it,
    /// which is what separates a typo from a key a newer `mur` understands.
    pub nearest_known: Option<String>,
}

impl UnknownManifestKey {
    /// How the containing block is named in the emitted line.
    fn location(&self) -> String {
        if self.block_path.is_empty() {
            "at the top level".to_string()
        } else {
            format!("in {}", self.block_path)
        }
    }
}

/// Restricted Damerau-Levenshtein distance (optimal string alignment) between two keys, counting
/// characters rather than bytes so a non-ASCII key is not scored by its UTF-8 length.
///
/// Transposing two adjacent characters costs 1 rather than the 2 plain Levenshtein charges it.
/// `allwo` for `allow` is one of the two typos this suggester exists to catch — the other being
/// `read-only` for `read_only` — and under plain Levenshtein it falls outside the threshold for
/// every key shorter than nine characters.
fn edit_distance(left: &str, right: &str) -> usize {
    let left: Vec<char> = left.chars().collect();
    let right: Vec<char> = right.chars().collect();
    if left.is_empty() {
        return right.len();
    }
    if right.is_empty() {
        return left.len();
    }

    let mut grid = vec![vec![0usize; right.len() + 1]; left.len() + 1];
    for (i, row) in grid.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in grid[0].iter_mut().enumerate() {
        *cell = j;
    }

    for i in 1..=left.len() {
        for j in 1..=right.len() {
            let substitution = grid[i - 1][j - 1] + usize::from(left[i - 1] != right[j - 1]);
            let mut best = substitution.min(grid[i - 1][j] + 1).min(grid[i][j - 1] + 1);
            if i > 1 && j > 1 && left[i - 1] == right[j - 2] && left[i - 2] == right[j - 1] {
                best = best.min(grid[i - 2][j - 2] + 1);
            }
            grid[i][j] = best;
        }
    }
    grid[left.len()][right.len()]
}

/// The largest edit distance at which a recognized key still counts as "near" the declared one:
/// a third of the longer of the two, and never below 1.
///
/// Proportional rather than fixed because a fixed threshold reads wrong at both ends — 2 lets
/// `env` suggest `evn`'s unrelated neighbours, and lets nothing at all be suggested for a
/// twenty-character key with one transposed pair. Below the threshold the line is worded as a
/// spelling problem; at or above it, as a key this build does not know.
#[must_use]
pub fn suggestion_threshold(declared: &str, candidate: &str) -> usize {
    (declared.chars().count().max(candidate.chars().count()) / 3).max(1)
}

/// The recognized key of a block nearest `declared`, or `None` when none is near enough.
///
/// Ties break toward the first candidate in declaration order, so the suggestion for a given
/// manifest is the same on every run.
#[must_use]
pub fn nearest_known_key(declared: &str, known: &[&str]) -> Option<String> {
    known
        .iter()
        .map(|candidate| (edit_distance(declared, candidate), *candidate))
        .filter(|(distance, candidate)| *distance <= suggestion_threshold(declared, candidate))
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, candidate)| candidate.to_string())
}

/// A `major.minor.patch` version as a comparable triple, or `None` for anything else.
///
/// Deliberately not a semver parse: the workspace carries no semver crate and this slice adds no
/// dependency. A pin carrying a pre-release or build suffix, or any component that is not a plain
/// number, simply yields no version-gap line — silence is the correct answer to a pin this cannot
/// read, not an error.
fn numeric_triple(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.trim().split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// The extra line naming a stale binary, or `None` when the pin is absent, unreadable, or not
/// higher than the running version.
///
/// A pin lower than the running version produces nothing here: `mur_version` is a pin rather than
/// a floor, and the pre-existing "manifest requires mur … but you are running mur …" warning
/// already fires on a difference in either direction.
fn version_gap_line(
    unknown_count: usize,
    mur_version: Option<&str>,
    running_version: &str,
) -> Option<String> {
    let pinned = mur_version?;
    if numeric_triple(pinned)? <= numeric_triple(running_version)? {
        return None;
    }
    let link = security_warning_link(W_SEC_019);
    let keys = if unknown_count == 1 {
        "1 key in it is not recognized by this build".to_string()
    } else {
        format!("{unknown_count} keys in it are not recognized by this build")
    };
    Some(format!(
        "[murmur-artifact] warning[{W_SEC_019}]: this manifest pins mur {pinned}, you are running \
         {running_version}; {keys} ({link})"
    ))
}

/// The complete `W-SEC-019` output for one parsed manifest: one line per unrecognized key, in the
/// order they were captured, followed by the version-gap line when there is one.
///
/// Returns the lines rather than printing them so `mur run` and `mur doctor` emit one identical
/// set of bytes, and so a test can assert the wording without capturing stderr.
#[must_use]
pub fn unknown_manifest_key_warnings(
    keys: &[UnknownManifestKey],
    mur_version: Option<&str>,
    running_version: &str,
) -> Vec<String> {
    if keys.is_empty() {
        return Vec::new();
    }

    let link = security_warning_link(W_SEC_019);
    let mut lines: Vec<String> = keys
        .iter()
        .map(|unknown| {
            let key = &unknown.key;
            let location = unknown.location();
            match &unknown.nearest_known {
                Some(nearest) => format!(
                    "[murmur-artifact] warning[{W_SEC_019}]: unrecognized key '{key}' {location} \
                     — did you mean '{nearest}'? The manifest has a spelling problem here: the \
                     key was parsed and ignored, so whatever it declared is not in effect. \
                     Correct it in murmur.yaml ({link})"
                ),
                None => format!(
                    "[murmur-artifact] warning[{W_SEC_019}]: unrecognized key '{key}' {location} \
                     — this build of mur does not recognize it and no key it does recognize there \
                     is close to it, so the key may come from a newer mur. It was parsed and \
                     ignored; nothing here says the manifest is misspelled ({link})"
                ),
            }
        })
        .collect();

    lines.extend(version_gap_line(keys.len(), mur_version, running_version));
    lines
}

/// Prints [`unknown_manifest_key_warnings`] to stderr, one line each.
///
/// The single emitter both `mur run` and `mur doctor` call, on the
/// `warn_on_interpreter_runtime_grants` precedent: two surfaces cannot word one ignored key
/// differently if neither of them formats it.
pub fn warn_on_unknown_manifest_keys(manifest: &RuntimeManifest, running_version: &str) {
    for line in unknown_manifest_key_warnings(
        &manifest.unknown_keys,
        manifest.mur_version.as_deref(),
        running_version,
    ) {
        eprintln!("{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unknown(key: &str, path: &str, nearest: Option<&str>) -> UnknownManifestKey {
        UnknownManifestKey {
            key: key.to_string(),
            block_path: path.to_string(),
            nearest_known: nearest.map(str::to_string),
        }
    }

    #[test]
    fn edit_distance_counts_the_usual_single_edits() {
        assert_eq!(edit_distance("read_only", "read_only"), 0);
        assert_eq!(edit_distance("read-only", "read_only"), 1);
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("abc", ""), 3);
        assert_eq!(edit_distance("allwo", "allow"), 1);
        assert_eq!(edit_distance("abc", "cba"), 2);
    }

    /// The hyphen-for-underscore typo the whole warning exists for.
    #[test]
    fn a_one_character_typo_suggests_the_real_key() {
        assert_eq!(
            nearest_known_key("read-only", &["scope", "workdir_exec", "read_only"]),
            Some("read_only".to_string())
        );
    }

    /// A key with no neighbour gets no suggestion, which is what makes the two wordings
    /// distinguishable at all.
    #[test]
    fn an_unrelated_key_suggests_nothing() {
        assert_eq!(
            nearest_known_key(
                "quantum_teleport",
                &["name", "version", "artifacts", "capabilities", "context"]
            ),
            None
        );
    }

    #[test]
    fn a_near_match_is_worded_as_a_spelling_problem() {
        let lines = unknown_manifest_key_warnings(
            &[unknown(
                "read-only",
                "capabilities.filesystem",
                Some("read_only"),
            )],
            None,
            "0.2.0",
        );
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("unrecognized key 'read-only' in capabilities.filesystem"));
        assert!(lines[0].contains("did you mean 'read_only'?"));
        assert!(lines[0].contains("spelling problem"));
        assert!(lines[0].contains("#w-sec-019"));
    }

    #[test]
    fn no_near_match_is_worded_as_a_possibly_newer_key() {
        let lines =
            unknown_manifest_key_warnings(&[unknown("quantum_teleport", "", None)], None, "0.2.0");
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("unrecognized key 'quantum_teleport' at the top level"));
        assert!(lines[0].contains("may come from a newer mur"));
        assert!(!lines[0].contains("did you mean"));
        assert!(!lines[0].contains("spelling"));
    }

    #[test]
    fn a_higher_pin_names_both_versions_and_the_count() {
        let lines = unknown_manifest_key_warnings(
            &[unknown("a", "", None), unknown("b", "", None)],
            Some("99.0.0"),
            "0.2.0",
        );
        assert_eq!(lines.len(), 3);
        assert!(lines[2].contains(
            "this manifest pins mur 99.0.0, you are running 0.2.0; 2 keys in it are not \
             recognized by this build"
        ));
    }

    #[test]
    fn one_unrecognized_key_reads_in_the_singular() {
        let lines =
            unknown_manifest_key_warnings(&[unknown("a", "", None)], Some("99.0.0"), "0.2.0");
        assert_eq!(lines.len(), 2);
        assert!(lines[1].contains("1 key in it is not recognized by this build"));
    }

    /// `mur_version` is a pin, not a floor, so nothing is said about a lower one — the
    /// pre-existing mismatch warning in `mur run` already covers that direction.
    #[test]
    fn a_lower_or_equal_pin_adds_no_line() {
        for pinned in ["0.1.0", "0.2.0"] {
            let lines =
                unknown_manifest_key_warnings(&[unknown("a", "", None)], Some(pinned), "0.2.0");
            assert_eq!(lines.len(), 1, "pin {pinned}");
        }
    }

    /// A pin this cannot read is silence, never an error.
    #[test]
    fn an_unparseable_pin_adds_no_line_and_no_error() {
        for pinned in ["1.2.3-rc1", "latest", "1.2", "1.2.3.4", ""] {
            let lines =
                unknown_manifest_key_warnings(&[unknown("a", "", None)], Some(pinned), "0.2.0");
            assert_eq!(lines.len(), 1, "pin {pinned}");
        }
    }

    #[test]
    fn a_manifest_with_no_unrecognized_key_emits_nothing_at_all() {
        assert!(unknown_manifest_key_warnings(&[], Some("99.0.0"), "0.2.0").is_empty());
    }
}
