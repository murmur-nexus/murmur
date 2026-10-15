//! WIT levels: numbered, exact sets of `murmur:*` package versions, and whether an artifact's
//! interfaces fit one.
//!
//! A level names every `murmur:*` package at one version. An artifact fits a level when every
//! `murmur:*` interface it names, exports and imports alike, is at exactly the version that level
//! lists for its package. [`level_fit`] is the one place that is decided; load, install, search,
//! `mur index` and `mur doctor` all reach it.
//!
//! The levels themselves live in `crates/capsule-runtime/wit/levels.txt`, read by
//! [`parse_wit_levels`]. The file is append-only: a committed level is never edited, renumbered
//! or reused, so a level number means the same package set on every `mur` that knows it.

use thiserror::Error;

use crate::{UnservedInterface, WitContracts};

/// One WIT level: its number and the exact version of every `murmur:*` package it serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WitLevel {
    /// Positive, and one more than the level before it.
    pub level: u64,
    /// `("murmur:<name>", "<major>.<minor>.<patch>")`, sorted by package and each package once.
    pub packages: Vec<(String, String)>,
}

impl WitLevel {
    /// The version this level lists for `package` (`murmur:driver`), if it lists one.
    #[must_use]
    pub fn version_of(&self, package: &str) -> Option<&str> {
        self.packages
            .iter()
            .find(|(name, _)| name == package)
            .map(|(_, version)| version.as_str())
    }

    /// The level as the `(package, version)` table [`WitContracts::unserved_against`] reads.
    #[must_use]
    pub fn served(&self) -> Vec<(&str, &str)> {
        self.packages
            .iter()
            .map(|(package, version)| (package.as_str(), version.as_str()))
            .collect()
    }
}

/// Whether an artifact's `murmur:*` interfaces fit one WIT level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LevelFit {
    Fits,
    /// Every interface whose version is not the one the level lists for its package, exports
    /// then imports, in recorded order. Never empty.
    Mismatched(Vec<UnservedInterface>),
}

impl LevelFit {
    #[must_use]
    pub fn fits(&self) -> bool {
        matches!(self, LevelFit::Fits)
    }
}

/// Whether `contracts` fits `level`: every `murmur:*` name in either direction must be at
/// exactly the version the level lists for its package. A name with no `@version`, or of a
/// package the level does not list, does not fit. Names outside `murmur:` are not read, so an
/// artifact naming no `murmur:*` interface fits every level.
#[must_use]
pub fn level_fit(contracts: &WitContracts, level: &WitLevel) -> LevelFit {
    let mismatched = contracts.unserved_against(&level.served());
    if mismatched.is_empty() {
        LevelFit::Fits
    } else {
        LevelFit::Mismatched(mismatched)
    }
}

/// The numbers of every level in `levels` that `contracts` fits, in `levels`' order.
#[must_use]
pub fn fitting_levels(contracts: &WitContracts, levels: &[WitLevel]) -> Vec<u64> {
    levels
        .iter()
        .filter(|level| level_fit(contracts, level).fits())
        .map(|level| level.level)
        .collect()
}

/// The newest level in `levels` that `contracts` fits; `None` when it fits none. This is an
/// artifact's built-at level.
#[must_use]
pub fn newest_fitting_level(contracts: &WitContracts, levels: &[WitLevel]) -> Option<u64> {
    fitting_levels(contracts, levels).into_iter().max()
}

/// Why `levels.txt` text could not be read. Every variant naming a line counts lines from 1.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum WitLevelsError {
    #[error("line {line}: expected `<level> <package>@<version>`, found {fields} field(s)")]
    FieldCount { line: usize, fields: usize },
    #[error("line {line}: level '{text}' is not a positive integer")]
    NotALevel { line: usize, text: String },
    #[error("line {line}: the first level is {level}; levels are numbered from 1")]
    FirstLevelNotOne { line: usize, level: u64 },
    #[error(
        "line {line}: level {level} follows level {previous}; levels are numbered with no gap"
    )]
    Gap {
        line: usize,
        level: u64,
        previous: u64,
    },
    #[error(
        "line {line}: level {level} follows level {previous}; a level's lines are contiguous and \
         levels ascend"
    )]
    OutOfOrder {
        line: usize,
        level: u64,
        previous: u64,
    },
    #[error("line {line}: '{text}' is not a murmur:<name>@<version> package")]
    BadPackage { line: usize, text: String },
    #[error("line {line}: version '{version}' of {package} is not <digits>.<digits>.<digits>")]
    BadVersion {
        line: usize,
        package: String,
        version: String,
    },
    #[error("line {line}: {package} is listed twice in level {level}")]
    DuplicatePackage {
        line: usize,
        level: u64,
        package: String,
    },
    #[error(
        "line {line}: {package} follows {previous} in level {level}; a level lists its packages \
         in ascending order"
    )]
    PackageOrder {
        line: usize,
        level: u64,
        package: String,
        previous: String,
    },
    #[error("the file lists no level; level 1 is the first")]
    NoLevels,
    #[error(
        "line {line}: level {level} lists the same package set as level {previous}; append a \
         level only when a version changes"
    )]
    SameAsPrevious {
        line: usize,
        level: u64,
        previous: u64,
    },
}

/// Read `levels.txt` text: one `<level> <package>@<version>` per line, `#` starting a comment
/// that runs to the end of the line, blank lines ignored.
///
/// Refuses, naming the line: a line without exactly two fields, a level that is not a positive
/// integer, a first level other than 1, a gap or a step back in numbering, a package not written
/// `murmur:<name>@<version>`, a version that is not three dot-separated digit runs, a package
/// listed twice or out of ascending order within its level, and a level whose set equals the one
/// before it. Text with no level at all is [`WitLevelsError::NoLevels`].
pub fn parse_wit_levels(text: &str) -> Result<Vec<WitLevel>, WitLevelsError> {
    let mut levels: Vec<WitLevel> = Vec::new();
    // The line each level starts on, for naming it when its whole set is refused.
    let mut first_lines: Vec<usize> = Vec::new();

    for (index, raw) in text.lines().enumerate() {
        let line = index + 1;
        let content = raw.split_once('#').map_or(raw, |(before, _)| before);
        let fields: Vec<&str> = content.split_whitespace().collect();
        if fields.is_empty() {
            continue;
        }
        if fields.len() != 2 {
            return Err(WitLevelsError::FieldCount {
                line,
                fields: fields.len(),
            });
        }
        let level = parse_level(fields[0]).ok_or_else(|| WitLevelsError::NotALevel {
            line,
            text: fields[0].to_string(),
        })?;
        let (package, version) = parse_package(line, fields[1])?;

        let current = levels.last().map(|l| l.level);
        match current {
            None if level != 1 => return Err(WitLevelsError::FirstLevelNotOne { line, level }),
            Some(previous) if level < previous => {
                return Err(WitLevelsError::OutOfOrder {
                    line,
                    level,
                    previous,
                })
            }
            Some(previous) if level > previous + 1 => {
                return Err(WitLevelsError::Gap {
                    line,
                    level,
                    previous,
                })
            }
            _ => {}
        }
        if current != Some(level) {
            check_differs_from_previous(&levels, &first_lines)?;
            levels.push(WitLevel {
                level,
                packages: Vec::new(),
            });
            first_lines.push(line);
        }

        let open = levels.last_mut().expect("a level was just opened");
        if let Some((previous, _)) = open.packages.last() {
            if *previous == package {
                return Err(WitLevelsError::DuplicatePackage {
                    line,
                    level,
                    package,
                });
            }
            if *previous > package {
                return Err(WitLevelsError::PackageOrder {
                    line,
                    level,
                    package,
                    previous: previous.clone(),
                });
            }
        }
        open.packages.push((package, version));
    }

    if levels.is_empty() {
        return Err(WitLevelsError::NoLevels);
    }
    check_differs_from_previous(&levels, &first_lines)?;
    Ok(levels)
}

/// Refuse the newest level in `levels` when its set equals the one before it.
fn check_differs_from_previous(
    levels: &[WitLevel],
    first_lines: &[usize],
) -> Result<(), WitLevelsError> {
    if let [.., previous, newest] = levels {
        if previous.packages == newest.packages {
            return Err(WitLevelsError::SameAsPrevious {
                line: first_lines[first_lines.len() - 1],
                level: newest.level,
                previous: previous.level,
            });
        }
    }
    Ok(())
}

/// A positive integer written in plain decimal: no sign, no leading zero.
fn parse_level(text: &str) -> Option<u64> {
    let level: u64 = text.parse().ok()?;
    (level > 0 && level.to_string() == text).then_some(level)
}

fn parse_package(line: usize, text: &str) -> Result<(String, String), WitLevelsError> {
    let bad_package = || WitLevelsError::BadPackage {
        line,
        text: text.to_string(),
    };
    let (package, version) = text.split_once('@').ok_or_else(bad_package)?;
    let name = package.strip_prefix("murmur:").ok_or_else(bad_package)?;
    if name.is_empty() || name.contains(['/', ':', '@']) {
        return Err(bad_package());
    }
    let parts: Vec<&str> = version.split('.').collect();
    let numeric = |part: &&str| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
    if parts.len() != 3 || !parts.iter().all(numeric) {
        return Err(WitLevelsError::BadVersion {
            line,
            package: package.to_string(),
            version: version.to_string(),
        });
    }
    Ok((package.to_string(), version.to_string()))
}

/// `level <N> has <package>@<version>`, or `level <N> has no version of <package>`: what a level
/// lists for one mismatched interface's package.
#[must_use]
pub fn level_has_phrase(level: u64, mismatch: &UnservedInterface) -> String {
    let package = &mismatch.package;
    match &mismatch.served_version {
        Some(version) => format!("level {level} has {package}@{version}"),
        None => format!("level {level} has no version of {package}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ContractDirection;

    const LEVEL_1: &str = "\
1 murmur:artifact-manager@0.1.0
1 murmur:capsule@0.1.0
1 murmur:conversation@0.2.0
1 murmur:driver@0.3.0
1 murmur:hook@0.9.0
1 murmur:host@0.1.0
1 murmur:message@0.1.0
1 murmur:runtime@0.4.0
1 murmur:runtime-guest@0.1.0
1 murmur:shell@0.1.0
1 murmur:stream@0.1.0
1 murmur:task@0.1.0
1 murmur:task-io@0.1.0
1 murmur:tool@0.1.0
1 murmur:tool-registry@0.1.0
";

    /// Level 1, plus a level 2 identical but for `murmur:driver@0.4.0`.
    fn two_levels() -> Vec<WitLevel> {
        let level_2 = LEVEL_1
            .lines()
            .map(|line| {
                line.replacen("1 ", "2 ", 1)
                    .replace("murmur:driver@0.3.0", "murmur:driver@0.4.0")
            })
            .collect::<Vec<_>>()
            .join("\n");
        parse_wit_levels(&format!("{LEVEL_1}{level_2}\n")).unwrap()
    }

    fn contracts(exports: &[&str], imports: &[&str]) -> WitContracts {
        WitContracts {
            exports: exports.iter().map(|s| s.to_string()).collect(),
            imports: imports.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn refusal(text: &str) -> WitLevelsError {
        parse_wit_levels(text).unwrap_err()
    }

    #[test]
    fn wit_level_reads_comments_blank_lines_and_levels() {
        let levels = parse_wit_levels(&format!("# header\n\n{LEVEL_1}  # trailing\n")).unwrap();
        assert_eq!(levels.len(), 1);
        assert_eq!(levels[0].level, 1);
        assert_eq!(levels[0].packages.len(), 15);
        assert_eq!(levels[0].version_of("murmur:driver"), Some("0.3.0"));
        assert_eq!(levels[0].version_of("murmur:text"), None);
        assert_eq!(two_levels()[1].version_of("murmur:driver"), Some("0.4.0"));
    }

    #[test]
    fn wit_level_parser_refuses_each_malformed_line() {
        assert_eq!(
            refusal("# c\n1 murmur:a@0.1.0 extra\n"),
            WitLevelsError::FieldCount { line: 2, fields: 3 }
        );
        assert_eq!(
            refusal("1\n"),
            WitLevelsError::FieldCount { line: 1, fields: 1 }
        );
        for bad in ["0", "-1", "01", "+1", "one"] {
            assert_eq!(
                refusal(&format!("{bad} murmur:a@0.1.0\n")),
                WitLevelsError::NotALevel {
                    line: 1,
                    text: bad.to_string()
                }
            );
        }
        assert_eq!(
            refusal("2 murmur:a@0.1.0\n"),
            WitLevelsError::FirstLevelNotOne { line: 1, level: 2 }
        );
        assert_eq!(
            refusal("1 murmur:a@0.1.0\n3 murmur:a@0.2.0\n"),
            WitLevelsError::Gap {
                line: 2,
                level: 3,
                previous: 1
            }
        );
        assert_eq!(
            refusal("1 murmur:a@0.1.0\n2 murmur:a@0.2.0\n1 murmur:b@0.1.0\n"),
            WitLevelsError::OutOfOrder {
                line: 3,
                level: 1,
                previous: 2
            }
        );
        for bad in [
            "wasi:a@0.1.0",
            "murmur:a",
            "murmur:@0.1.0",
            "murmur:a/b@0.1.0",
        ] {
            assert_eq!(
                refusal(&format!("1 {bad}\n")),
                WitLevelsError::BadPackage {
                    line: 1,
                    text: bad.to_string()
                }
            );
        }
        for bad in ["0.1", "0.1.0.0", "0.x.0", "0..0", "v0.1.0", "0.1.0-rc1"] {
            assert_eq!(
                refusal(&format!("1 murmur:a@{bad}\n")),
                WitLevelsError::BadVersion {
                    line: 1,
                    package: "murmur:a".to_string(),
                    version: bad.to_string()
                }
            );
        }
        assert_eq!(
            refusal("1 murmur:a@0.1.0\n1 murmur:a@0.2.0\n"),
            WitLevelsError::DuplicatePackage {
                line: 2,
                level: 1,
                package: "murmur:a".to_string()
            }
        );
        assert_eq!(
            refusal("1 murmur:b@0.1.0\n1 murmur:a@0.1.0\n"),
            WitLevelsError::PackageOrder {
                line: 2,
                level: 1,
                package: "murmur:a".to_string(),
                previous: "murmur:b".to_string()
            }
        );
        assert_eq!(refusal("# only a comment\n\n"), WitLevelsError::NoLevels);
        assert_eq!(
            refusal("1 murmur:a@0.1.0\n2 murmur:a@0.1.0\n"),
            WitLevelsError::SameAsPrevious {
                line: 2,
                level: 2,
                previous: 1
            }
        );
        assert_eq!(
            refusal("1 murmur:a@0.1.0\n2 murmur:a@0.1.0\n3 murmur:a@0.2.0\n"),
            WitLevelsError::SameAsPrevious {
                line: 2,
                level: 2,
                previous: 1
            }
        );
    }

    #[test]
    fn wit_level_refusals_name_the_line() {
        assert_eq!(
            refusal("1 murmur:a@0.1.0\n1 murmur:b@x\n").to_string(),
            "line 2: version 'x' of murmur:b is not <digits>.<digits>.<digits>"
        );
    }

    #[test]
    fn wit_level_a_tool_naming_no_driver_interface_fits_both_levels() {
        let levels = two_levels();
        let tool = contracts(&["murmur:tool/run@0.1.0"], &[]);
        assert!(level_fit(&tool, &levels[0]).fits());
        assert!(level_fit(&tool, &levels[1]).fits());
        assert_eq!(fitting_levels(&tool, &levels), vec![1, 2]);
        assert_eq!(newest_fitting_level(&tool, &levels), Some(2));
    }

    #[test]
    fn wit_level_a_process_driver_fits_only_the_level_it_was_built_against() {
        let levels = two_levels();
        let driver = contracts(&["murmur:driver/process@0.3.0"], &[]);
        assert!(level_fit(&driver, &levels[0]).fits());
        let LevelFit::Mismatched(mismatched) = level_fit(&driver, &levels[1]) else {
            panic!("a 0.3.0 driver fits a level serving murmur:driver@0.4.0");
        };
        assert_eq!(mismatched.len(), 1);
        assert_eq!(mismatched[0].direction, ContractDirection::Export);
        assert_eq!(mismatched[0].package, "murmur:driver");
        assert_eq!(mismatched[0].served_version.as_deref(), Some("0.4.0"));
        assert_eq!(
            level_has_phrase(2, &mismatched[0]),
            "level 2 has murmur:driver@0.4.0"
        );
        assert_eq!(fitting_levels(&driver, &levels), vec![1]);
        assert_eq!(newest_fitting_level(&driver, &levels), Some(1));
    }

    #[test]
    fn wit_level_a_skill_fits_every_level() {
        let levels = two_levels();
        let skill = WitContracts::default();
        assert_eq!(fitting_levels(&skill, &levels), vec![1, 2]);
        let wasi_only = contracts(&[], &["wasi:cli/stdout@0.2.0"]);
        assert_eq!(newest_fitting_level(&wasi_only, &levels), Some(2));
    }

    #[test]
    fn wit_level_a_hook_at_an_older_version_fits_no_level() {
        let levels = two_levels();
        let hook = contracts(&["murmur:hook/lifecycle@0.8.0"], &[]);
        assert_eq!(fitting_levels(&hook, &levels), Vec::<u64>::new());
        assert_eq!(newest_fitting_level(&hook, &levels), None);
        let text = contracts(&[], &["murmur:text/stream@0.1.0"]);
        let LevelFit::Mismatched(mismatched) = level_fit(&text, &levels[0]) else {
            panic!("murmur:text fits level 1");
        };
        assert_eq!(
            level_has_phrase(1, &mismatched[0]),
            "level 1 has no version of murmur:text"
        );
    }
}
