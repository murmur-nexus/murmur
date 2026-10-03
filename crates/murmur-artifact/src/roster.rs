//! The `roster.yaml` format: a formation's members, its entry member, and which members may call
//! which.
//!
//! This module parses the file's shape and nothing else. Whether two members share a name, whether
//! exactly one is the entry member, whether a rule names a member that exists, and everything about
//! the capsules the members name are decided by admission in `capsule-runtime`, which reads the
//! stores.
//!
//! The parse is strict: a key this build does not know is a refusal, not a `W-SEC-019` warning. A
//! roster authorizes peer traffic, so a misspelt `reachability` must not read as a closed roster.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_yaml::Value;
use thiserror::Error;

use crate::build::artifact_name_format_error;
use crate::manifest::describe_yaml_shape;
use crate::registry::is_reserved_version;

/// Filename of a project's formation roster, read from the directory that holds `murmur.yaml`.
pub const ROSTER_FILENAME: &str = "roster.yaml";

/// The only `roster_version` this build reads.
pub const ROSTER_VERSION: u64 = 1;

/// Longest member name `roster.yaml` accepts, in characters.
pub const MAX_MEMBER_NAME_LEN: usize = 32;

const TOP_LEVEL_KEYS: &[&str] = &["roster_version", "members", "reachability"];
const MEMBER_KEYS: &[&str] = &["name", "capsule", "version", "entry"];
const RULE_KEYS: &[&str] = &["from", "to"];

/// The `reachability` string that expands to every pair of members that both serve peers.
pub const REACHABILITY_ALL: &str = "all";

/// Resolve the roster path for a project directory.
#[must_use]
pub fn resolve_roster_path(project_dir: &Path) -> PathBuf {
    project_dir.join(ROSTER_FILENAME)
}

/// A parsed `roster.yaml`.
///
/// Every shape rule has held: names match the member-name pattern, capsules are valid artifact
/// names, versions are exact, and no rule calls itself. Nothing cross-member has been checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Roster {
    /// In the order the file lists them. Admission reports faults in this order.
    pub members: Vec<RosterMember>,
    pub reachability: RosterReachability,
}

/// One `members[]` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RosterMember {
    /// The member's name within this roster, matching `^[a-z][a-z0-9-]{0,31}$`. Distinct from
    /// `capsule`: two members may run the same capsule.
    pub name: String,
    /// The installed artifact name the member runs.
    pub capsule: String,
    /// The exact version, as written. Never blank and never a reserved alias.
    pub version: String,
    /// `entry: true`. `false` when the key is absent.
    pub entry: bool,
}

/// The declared `reachability`, before admission expands it into edges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RosterReachability {
    /// No `reachability` key, or an empty list: no member may call another.
    Closed,
    /// `reachability: all`.
    All,
    /// A non-empty list of rules, in file order.
    Rules(Vec<ReachabilityRule>),
}

/// One `reachability[]` rule: `from` may call each member in `to`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReachabilityRule {
    pub from: String,
    /// Non-empty, and never containing `from`. A name listed twice is kept as written.
    pub to: Vec<String>,
}

#[derive(Debug, Error)]
pub enum RosterError {
    #[error("{ROSTER_FILENAME} not found at {}", path.display())]
    NotFound { path: PathBuf },
    #[error("failed to read {ROSTER_FILENAME} at {}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// Every shape fault. The message starts with `roster.yaml`, names the key path, and names
    /// the member when the fault is inside a member entry that has a usable name.
    #[error("{message}")]
    Malformed { message: String },
}

/// Read and parse the roster at `path`.
pub fn load_roster(path: &Path) -> Result<Roster, RosterError> {
    let text = fs::read_to_string(path).map_err(|source| {
        if source.kind() == io::ErrorKind::NotFound {
            RosterError::NotFound {
                path: path.to_path_buf(),
            }
        } else {
            RosterError::Read {
                path: path.to_path_buf(),
                source,
            }
        }
    })?;
    Roster::from_yaml_str(&text)
}

/// Why `name` is not a usable member name, or `None` when it is.
#[must_use]
pub fn member_name_format_error(name: &str) -> Option<String> {
    let mut chars = name.chars();
    match chars.next() {
        None => return Some("must not be empty".to_string()),
        Some(first) if !first.is_ascii_lowercase() => {
            return Some("must start with a lowercase letter".to_string())
        }
        Some(_) => {}
    }
    if name.chars().count() > MAX_MEMBER_NAME_LEN {
        return Some(format!("must be at most {MAX_MEMBER_NAME_LEN} characters"));
    }
    if let Some(bad) = chars.find(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-'))
    {
        return Some(format!(
            "may contain only lowercase letters, digits and '-' (found {bad:?})"
        ));
    }
    None
}

impl Roster {
    /// Parse `roster.yaml` text. Every refusal is [`RosterError::Malformed`].
    pub fn from_yaml_str(input: &str) -> Result<Self, RosterError> {
        let document: Value = serde_yaml::from_str(input).map_err(|err| match err.location() {
            Some(location) => malformed(format!(
                "{ROSTER_FILENAME}: YAML syntax error at line {}, column {}: {err}",
                location.line(),
                location.column()
            )),
            None => malformed(format!("{ROSTER_FILENAME}: YAML syntax error: {err}")),
        })?;

        let Value::Mapping(top) = document else {
            return Err(malformed(format!(
                "{ROSTER_FILENAME}: must be a mapping with `roster_version` and `members`, found {}",
                describe_yaml_shape(&document)
            )));
        };
        refuse_unknown_keys(&top, TOP_LEVEL_KEYS, "", None)?;

        match present(top.get("roster_version")) {
            None => return Err(fault("roster_version", None, "is required")),
            Some(Value::Number(number)) if number.as_u64() == Some(ROSTER_VERSION) => {}
            Some(other) => {
                return Err(fault(
                    "roster_version",
                    None,
                    &format!(
                        "must be {ROSTER_VERSION}, the only roster format this build reads; found \
                         {}",
                        describe_yaml_shape(other)
                    ),
                ))
            }
        }

        let entries = match present(top.get("members")) {
            None => return Err(fault("members", None, "is required")),
            Some(Value::Sequence(entries)) if entries.is_empty() => {
                return Err(fault("members", None, "must list at least one member"))
            }
            Some(Value::Sequence(entries)) => entries,
            Some(other) => {
                return Err(fault(
                    "members",
                    None,
                    &format!("must be a list, found {}", describe_yaml_shape(other)),
                ))
            }
        };
        let mut members = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| parse_member(index, entry))
            .collect::<Result<Vec<_>, _>>()?;

        let reachability = match present(top.get("reachability")) {
            None => RosterReachability::Closed,
            Some(Value::String(word)) if word == REACHABILITY_ALL => RosterReachability::All,
            Some(Value::String(word)) => {
                return Err(fault(
                    "reachability",
                    None,
                    &format!(
                        "'{word}' is not a reachability; write `{REACHABILITY_ALL}` or a list of \
                         rules"
                    ),
                ))
            }
            Some(Value::Sequence(rules)) if rules.is_empty() => RosterReachability::Closed,
            Some(Value::Sequence(rules)) => RosterReachability::Rules(
                rules
                    .iter()
                    .enumerate()
                    .map(|(index, rule)| parse_rule(index, rule))
                    .collect::<Result<Vec<_>, _>>()?,
            ),
            Some(other) => {
                return Err(fault(
                    "reachability",
                    None,
                    &format!(
                        "must be `{REACHABILITY_ALL}` or a list of rules, found {}",
                        describe_yaml_shape(other)
                    ),
                ))
            }
        };

        // A `serde_yaml::Value` holds an unquoted `1.10` as the float 1.1, so the version text is
        // read a second time through `String`, which keeps the scalar exactly as written — the
        // same reading `artifacts[].version` gets in `murmur.yaml`.
        let texts: VersionTexts = serde_yaml::from_str(input)
            .map_err(|err| malformed(format!("{ROSTER_FILENAME}: members[].version: {err}")))?;
        for (member, text) in members.iter_mut().zip(texts.members) {
            if let Some(text) = text.version {
                member.version = text;
            }
        }

        Ok(Roster {
            members,
            reachability,
        })
    }
}

#[derive(Deserialize)]
struct VersionTexts {
    members: Vec<VersionText>,
}

#[derive(Deserialize)]
struct VersionText {
    #[serde(default)]
    version: Option<String>,
}

fn parse_member(index: usize, entry: &Value) -> Result<RosterMember, RosterError> {
    let path = format!("members[{index}]");
    let Value::Mapping(fields) = entry else {
        return Err(fault(
            &path,
            None,
            &format!("must be a mapping, found {}", describe_yaml_shape(entry)),
        ));
    };
    // The member's own name labels every later fault in this entry, once it is a usable one.
    let label = match present(fields.get("name")) {
        Some(Value::String(name)) if member_name_format_error(name).is_none() => {
            Some(name.as_str())
        }
        _ => None,
    };
    refuse_unknown_keys(fields, MEMBER_KEYS, &path, label)?;

    let name = match present(fields.get("name")) {
        None => return Err(fault(&format!("{path}.name"), None, "is required")),
        Some(Value::String(name)) => {
            if let Some(problem) = member_name_format_error(name) {
                return Err(fault(
                    &format!("{path}.name"),
                    None,
                    &format!("'{name}' {problem}; a member name matches ^[a-z][a-z0-9-]{{0,31}}$"),
                ));
            }
            name.clone()
        }
        Some(other) => {
            return Err(fault(
                &format!("{path}.name"),
                None,
                &format!("must be a string, found {}", describe_yaml_shape(other)),
            ))
        }
    };
    let label = Some(name.as_str());

    let capsule = match present(fields.get("capsule")) {
        None => return Err(fault(&format!("{path}.capsule"), label, "is required")),
        Some(Value::String(capsule)) => {
            if let Some(problem) = artifact_name_format_error(capsule) {
                return Err(fault(
                    &format!("{path}.capsule"),
                    label,
                    &format!("'{capsule}' is not a valid artifact name: it {problem}"),
                ));
            }
            capsule.clone()
        }
        Some(other) => {
            return Err(fault(
                &format!("{path}.capsule"),
                label,
                &format!("must be a string, found {}", describe_yaml_shape(other)),
            ))
        }
    };

    // Only a string can be blank or a reserved alias. A number or boolean is a version by its
    // text, which `Roster::from_yaml_str` reads afterwards.
    let version = match present(fields.get("version")) {
        None => return Err(fault(&format!("{path}.version"), label, "is required")),
        Some(Value::String(version)) => {
            if version.trim().is_empty() {
                return Err(fault(
                    &format!("{path}.version"),
                    label,
                    "must not be blank; a member pins an exact version",
                ));
            }
            if is_reserved_version(version) {
                return Err(fault(
                    &format!("{path}.version"),
                    label,
                    &format!(
                        "'{version}' is a reserved alias, not an exact version; pin the version \
                         the member runs"
                    ),
                ));
            }
            version.clone()
        }
        Some(Value::Number(number)) => number.to_string(),
        Some(Value::Bool(flag)) => flag.to_string(),
        Some(other) => {
            return Err(fault(
                &format!("{path}.version"),
                label,
                &format!(
                    "must be a version string, found {}",
                    describe_yaml_shape(other)
                ),
            ))
        }
    };

    let entry = match present(fields.get("entry")) {
        None => false,
        Some(Value::Bool(flag)) => *flag,
        Some(other) => {
            return Err(fault(
                &format!("{path}.entry"),
                label,
                &format!(
                    "must be true or false, found {}",
                    describe_yaml_shape(other)
                ),
            ))
        }
    };

    Ok(RosterMember {
        name,
        capsule,
        version,
        entry,
    })
}

fn parse_rule(index: usize, rule: &Value) -> Result<ReachabilityRule, RosterError> {
    let path = format!("reachability[{index}]");
    let Value::Mapping(fields) = rule else {
        return Err(fault(
            &path,
            None,
            &format!(
                "must be a mapping with `from` and `to`, found {}",
                describe_yaml_shape(rule)
            ),
        ));
    };
    refuse_unknown_keys(fields, RULE_KEYS, &path, None)?;

    let from = match present(fields.get("from")) {
        None => return Err(fault(&format!("{path}.from"), None, "is required")),
        Some(Value::String(from)) => from.clone(),
        Some(other) => {
            return Err(fault(
                &format!("{path}.from"),
                None,
                &format!(
                    "must be a member name, found {}",
                    describe_yaml_shape(other)
                ),
            ))
        }
    };

    let to = match present(fields.get("to")) {
        None => return Err(fault(&format!("{path}.to"), None, "is required")),
        Some(Value::Sequence(names)) if names.is_empty() => {
            return Err(fault(
                &format!("{path}.to"),
                None,
                &format!("must name at least one member '{from}' may call"),
            ))
        }
        Some(Value::Sequence(names)) => names
            .iter()
            .enumerate()
            .map(|(position, name)| match name {
                Value::String(name) => Ok(name.clone()),
                other => Err(fault(
                    &format!("{path}.to[{position}]"),
                    None,
                    &format!(
                        "must be a member name, found {}",
                        describe_yaml_shape(other)
                    ),
                )),
            })
            .collect::<Result<Vec<_>, _>>()?,
        Some(other) => {
            return Err(fault(
                &format!("{path}.to"),
                None,
                &format!(
                    "must be a list of member names, found {}",
                    describe_yaml_shape(other)
                ),
            ))
        }
    };

    if to.contains(&from) {
        return Err(fault(
            &format!("{path}.to"),
            None,
            &format!("lists '{from}', the rule's own `from`; a member never calls itself"),
        ));
    }

    Ok(ReachabilityRule { from, to })
}

/// A key whose value is YAML null reads as absent, so `entry:` with nothing after it is the
/// default and `name:` with nothing after it is missing.
fn present(value: Option<&Value>) -> Option<&Value> {
    value.filter(|value| !value.is_null())
}

fn refuse_unknown_keys(
    fields: &serde_yaml::Mapping,
    known: &[&str],
    path: &str,
    member: Option<&str>,
) -> Result<(), RosterError> {
    let unknown: BTreeSet<String> = fields
        .keys()
        .filter(|key| !key.as_str().is_some_and(|key| known.contains(&key)))
        .map(|key| match key {
            Value::String(key) => key.clone(),
            other => serde_yaml::to_string(other)
                .map(|text| text.trim_end().to_string())
                .unwrap_or_else(|_| describe_yaml_shape(other).to_string()),
        })
        .collect();
    let Some(first) = unknown.first() else {
        return Ok(());
    };
    let key_path = if path.is_empty() {
        first.clone()
    } else {
        format!("{path}.{first}")
    };
    let block = if path.is_empty() {
        "the top level".to_string()
    } else {
        format!("`{path}`")
    };
    Err(fault(
        &key_path,
        member,
        &format!(
            "unknown key; {block} accepts only {}",
            known
                .iter()
                .map(|key| format!("`{key}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    ))
}

fn fault(key_path: &str, member: Option<&str>, problem: &str) -> RosterError {
    let mut message = format!("{ROSTER_FILENAME}: {key_path}");
    if let Some(member) = member {
        let _ = write!(message, " (member '{member}')");
    }
    let _ = write!(message, ": {problem}");
    malformed(message)
}

fn malformed(message: String) -> RosterError {
    RosterError::Malformed { message }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S1: &str = "roster_version: 1
members:
  - name: planner
    capsule: planner
    version: 0.3.0
    entry: true
  - name: coder
    capsule: coder
    version: 1.2.0
  - name: reviewer
    capsule: reviewer
    version: 0.9.0
reachability:
  - from: planner
    to: [coder, reviewer]
  - from: reviewer
    to: [coder]
";

    fn member(name: &str, capsule: &str, version: &str, entry: bool) -> RosterMember {
        RosterMember {
            name: name.to_string(),
            capsule: capsule.to_string(),
            version: version.to_string(),
            entry,
        }
    }

    /// The refusal message for `input`, which must be refused.
    fn refusal(input: &str) -> String {
        match Roster::from_yaml_str(input) {
            Err(RosterError::Malformed { message }) => message,
            other => panic!("expected a Malformed refusal for:\n{input}\ngot {other:?}"),
        }
    }

    /// S1 with `members[1]` (coder) given `extra` as an additional line.
    fn s1_with_coder_line(extra: &str) -> String {
        S1.replace(
            "    version: 1.2.0\n",
            &format!("    version: 1.2.0\n    {extra}\n"),
        )
    }

    #[test]
    fn resolve_roster_path_joins_roster_filename() {
        assert_eq!(
            resolve_roster_path(Path::new("/tmp/project")),
            PathBuf::from("/tmp/project/roster.yaml")
        );
    }

    #[test]
    fn the_documented_roster_parses_with_omitted_entry_as_false() {
        let roster = Roster::from_yaml_str(S1).unwrap();
        assert_eq!(
            roster.members,
            vec![
                member("planner", "planner", "0.3.0", true),
                member("coder", "coder", "1.2.0", false),
                member("reviewer", "reviewer", "0.9.0", false),
            ]
        );
        assert_eq!(
            roster.reachability,
            RosterReachability::Rules(vec![
                ReachabilityRule {
                    from: "planner".to_string(),
                    to: vec!["coder".to_string(), "reviewer".to_string()],
                },
                ReachabilityRule {
                    from: "reviewer".to_string(),
                    to: vec!["coder".to_string()],
                },
            ])
        );
    }

    #[test]
    fn absent_and_empty_reachability_are_closed_and_all_is_all() {
        let base =
            "roster_version: 1\nmembers:\n  - {name: a, capsule: a, version: 1.0.0, entry: true}\n";
        for tail in ["", "reachability: []\n", "reachability:\n"] {
            let roster = Roster::from_yaml_str(&format!("{base}{tail}")).unwrap();
            assert_eq!(roster.reachability, RosterReachability::Closed, "{tail:?}");
        }
        let roster = Roster::from_yaml_str(&format!("{base}reachability: all\n")).unwrap();
        assert_eq!(roster.reachability, RosterReachability::All);
    }

    #[test]
    fn version_keeps_every_scalar_form_exactly_as_written() {
        for (written, read) in [
            ("1.10", "1.10"),
            ("1.0", "1.0"),
            ("1", "1"),
            ("'1.2.0'", "1.2.0"),
            ("\"2.0\"", "2.0"),
            ("1.2.0-rc.1", "1.2.0-rc.1"),
        ] {
            let roster = Roster::from_yaml_str(&format!(
                "roster_version: 1\nmembers:\n  - name: a\n    capsule: a\n    version: {written}\n"
            ))
            .unwrap();
            assert_eq!(roster.members[0].version, read, "{written}");
        }
    }

    #[test]
    fn shape_only_duplicates_entries_and_unknown_rule_names_parse() {
        let roster = Roster::from_yaml_str(
            "roster_version: 1\nmembers:\n  - {name: coder, capsule: coder, version: 1.2.0}\n  \
             - {name: coder, capsule: coder, version: 1.2.0}\nreachability:\n  - from: ghost\n    \
             to: [tester]\n",
        )
        .unwrap();
        assert_eq!(roster.members.len(), 2);
        assert!(roster.members.iter().all(|member| !member.entry));
    }

    #[test]
    fn every_shape_fault_is_refused_naming_its_key_path() {
        let cases: Vec<(String, &[&str])> = vec![
            (
                "members: [\n".to_string(),
                &["roster.yaml", "YAML syntax error"],
            ),
            (
                S1.replace("roster_version: 1\n", ""),
                &["roster_version", "is required"],
            ),
            (
                S1.replace("roster_version: 1", "roster_version: 2"),
                &["roster_version", "must be 1"],
            ),
            (
                "roster_version: 1\nmembers: []\n".to_string(),
                &["members", "at least one"],
            ),
            (
                format!("{S1}reachabilty: all\n"),
                &["reachabilty", "unknown key"],
            ),
            (
                s1_with_coder_line("sha: abc"),
                &["members[1].sha", "member 'coder'", "unknown key"],
            ),
            (
                S1.replace(
                    "    to: [coder]\n",
                    "    to: [coder]\n    skills: [review]\n",
                ),
                &["reachability[1].skills", "unknown key"],
            ),
            (
                S1.replace("    capsule: coder\n", ""),
                &["members[1].capsule", "member 'coder'", "is required"],
            ),
            (
                S1.replace("name: planner", "name: Planner"),
                &["members[0].name", "'Planner'"],
            ),
            (
                S1.replace("name: coder", "name: a_b"),
                &["members[1].name", "'a_b'"],
            ),
            (
                S1.replace("name: coder", &format!("name: {}", "a".repeat(33))),
                &["members[1].name", "at most 32"],
            ),
            (
                S1.replace("version: 1.2.0", "version: latest"),
                &["members[1].version", "member 'coder'", "reserved alias"],
            ),
            (
                S1.replace("version: 1.2.0", "version: \"\""),
                &["members[1].version", "member 'coder'", "blank"],
            ),
            (
                format!(
                    "{}reachability: some\n",
                    S1.split("reachability:").next().unwrap()
                ),
                &["reachability", "'some'"],
            ),
            (
                S1.replace("to: [coder]", "to: []"),
                &["reachability[1].to", "at least one"],
            ),
            (
                S1.replace(
                    "  - from: reviewer\n    to: [coder]",
                    "  - {from: coder, to: [coder]}",
                ),
                &["reachability[1].to", "'coder'", "never calls itself"],
            ),
            (
                S1.replace("entry: true", "entry: yes-please"),
                &["members[0].entry", "true or false"],
            ),
            ("- a\n".to_string(), &["roster.yaml", "must be a mapping"]),
        ];
        for (input, needles) in cases {
            let message = refusal(&input);
            assert!(message.starts_with("roster.yaml"), "{message}");
            for needle in needles {
                assert!(
                    message.contains(needle),
                    "expected {needle:?} in {message:?}"
                );
            }
        }
    }

    #[test]
    fn member_name_pattern_boundaries() {
        assert_eq!(member_name_format_error("a"), None);
        assert_eq!(member_name_format_error("a-1"), None);
        assert_eq!(member_name_format_error(&"a".repeat(32)), None);
        for bad in ["", "1a", "-a", "A", "a_b", "a.b"] {
            assert!(member_name_format_error(bad).is_some(), "{bad:?}");
        }
        assert!(member_name_format_error(&"a".repeat(33)).is_some());
    }

    #[test]
    fn load_roster_reports_a_missing_file_as_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let error = load_roster(&resolve_roster_path(dir.path())).unwrap_err();
        assert!(matches!(error, RosterError::NotFound { .. }), "{error:?}");
        assert!(error.to_string().starts_with("roster.yaml not found"));
    }
}
