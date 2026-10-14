//! The entry grammar of `capabilities.install.skill` and `capabilities.install.tool`.
//!
//! An entry is one of four forms:
//!
//! - `*`, every artifact of every scope;
//! - `<scope>/*`, every artifact in one scope;
//! - `<scope>/<prefix>*`, every artifact in `<scope>` whose name begins with `<prefix>`. The
//!   prefix uses only artifact-name characters and does not begin with `-`; it may end with `-`
//!   (`ana/review-*`);
//! - an exact full name, as [`ArtifactName`] accepts it.
//!
//! The manifest parser validates entries with [`validate`], the runtime's pull gate decides a name
//! with [`matches`], and the spawn referee compares a child's entries against its parent's with
//! [`covers`]. `covers(parent, child)` holds only when every name `child` matches is also matched
//! by `parent`; no entry but `*` reaches across scopes.

use crate::artifact_name::{prefix_part_error, unscoped_name_hint, ArtifactName};
use crate::build::artifact_name_format_error;

/// The accepted entry forms, quoted in every refusal of an entry.
pub const INSTALL_ENTRY_ACCEPTED_FORM: &str =
    "a full artifact name '<scope>/<name>', a scope and name prefix ending in a single trailing \
     '*' (e.g. 'ana/style-*' or 'ana/*'), or '*' alone";

/// Why an install entry is refused: the defect, and for an entry with no scope the hint naming
/// its scoped forms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallEntryError {
    pub reason: String,
    pub hint: Option<String>,
}

impl InstallEntryError {
    fn reason(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            hint: None,
        }
    }
}

enum Entry<'a> {
    Any,
    Prefix { scope: &'a str, prefix: &'a str },
    Exact(ArtifactName),
}

fn classify(entry: &str) -> Result<Entry<'_>, InstallEntryError> {
    if entry == "*" {
        return Ok(Entry::Any);
    }
    let Some((scope, rest)) = entry.split_once('/') else {
        let bare = entry
            .strip_suffix('*')
            .unwrap_or(entry)
            .trim_end_matches('-');
        return Err(match artifact_name_format_error(bare) {
            None => InstallEntryError {
                reason: format!("'{entry}' names no scope"),
                hint: Some(unscoped_name_hint(entry)),
            },
            Some(reason) => InstallEntryError::reason(format!("not an artifact name: {reason}")),
        });
    };
    if let Some(reason) = artifact_name_format_error(scope) {
        return Err(InstallEntryError::reason(format!("the scope {reason}")));
    }
    if let Some(prefix) = rest.strip_suffix('*') {
        if prefix.contains('*') {
            return Err(InstallEntryError::reason(
                "'*' may appear only once, as the last character",
            ));
        }
        if let Some(reason) = prefix_part_error(prefix) {
            return Err(InstallEntryError::reason(reason));
        }
        return Ok(Entry::Prefix { scope, prefix });
    }
    if rest.contains('*') {
        return Err(InstallEntryError::reason(
            "'*' may appear only once, as the last character",
        ));
    }
    ArtifactName::parse(entry)
        .map(Entry::Exact)
        .map_err(|error| InstallEntryError::reason(error.to_string()))
}

/// `Ok` when `entry` is one of the accepted forms, else why it is not.
///
/// The reason describes the defect only; callers quote the entry and
/// [`INSTALL_ENTRY_ACCEPTED_FORM`] around it, and print the hint when there is one.
pub fn validate(entry: &str) -> Result<(), InstallEntryError> {
    classify(entry).map(|_| ())
}

/// Whether the entry `pattern` admits the artifact `name`. `false` whenever `pattern` fails
/// [`validate`].
pub fn matches(pattern: &str, name: &ArtifactName) -> bool {
    match classify(pattern) {
        Ok(Entry::Any) => true,
        Ok(Entry::Prefix { scope, prefix }) => {
            name.scope() == scope && name.name().starts_with(prefix)
        }
        Ok(Entry::Exact(exact)) => &exact == name,
        Err(_) => false,
    }
}

/// Whether the parent entry `parent` admits every name the child entry `child` admits.
///
/// `*` covers everything; `s/p*` covers an exact `s/name` or an `s/q*` whose name begins with
/// `p`; an exact name covers only itself. `false` when either side fails [`validate`].
pub fn covers(parent: &str, child: &str) -> bool {
    let (Ok(parent), Ok(child)) = (classify(parent), classify(child)) else {
        return false;
    };
    match (parent, child) {
        (Entry::Any, _) => true,
        (_, Entry::Any) => false,
        (
            Entry::Prefix { scope, prefix },
            Entry::Prefix {
                scope: child_scope,
                prefix: child_prefix,
            },
        ) => scope == child_scope && child_prefix.starts_with(prefix),
        (Entry::Prefix { scope, prefix }, Entry::Exact(name)) => {
            name.scope() == scope && name.name().starts_with(prefix)
        }
        (Entry::Exact(_), Entry::Prefix { .. }) => false,
        (Entry::Exact(name), Entry::Exact(child)) => name == child,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build::MAX_ARTIFACT_NAME_LEN;

    fn name(text: &str) -> ArtifactName {
        ArtifactName::parse(text).unwrap()
    }

    #[test]
    fn install_pattern_validate_accepts_the_four_forms() {
        for entry in [
            "*",
            "murmur/*",
            "ana/tool-*",
            "ana/t*",
            "ana/tool-x",
            "local/x",
            "a1/x9-y",
        ] {
            assert!(validate(entry).is_ok(), "{entry:?} should be accepted");
        }
    }

    #[test]
    fn install_pattern_refuses_a_bare_entry_with_the_scoped_hint() {
        for entry in [
            "tool-*",
            "mur*",
            "tool-x",
            "tool-editor",
            "murmur-tool-editor",
        ] {
            let error = validate(entry).unwrap_err();
            let hint = error
                .hint
                .unwrap_or_else(|| panic!("{entry:?} has no hint"));
            assert!(hint.contains("local/"), "{entry:?}: {hint}");
        }
        let hint = validate("murmur-tool-editor").unwrap_err().hint.unwrap();
        assert!(hint.contains("murmur/tool-editor"), "{hint}");
    }

    #[test]
    fn install_pattern_validate_refuses_everything_else() {
        let too_long_prefix = format!("a/{}*", "a".repeat(MAX_ARTIFACT_NAME_LEN + 1));
        let too_long_name = format!("a/{}", "a".repeat(MAX_ARTIFACT_NAME_LEN + 1));
        for entry in [
            "",
            " ",
            "*/x",
            "*/*",
            "a/*x",
            "a/x*y",
            "a/**",
            "a*b",
            "*x",
            "**",
            "Foo/x",
            "a/Foo",
            "a/Foo*",
            "a/-x",
            "a/-x*",
            "a/x-",
            "a/b/c",
            "a/b/*",
            "/x",
            "x/",
            "github:owner/repo",
            "a b",
            "a/b c",
            "a:b",
            too_long_prefix.as_str(),
            too_long_name.as_str(),
        ] {
            assert!(validate(entry).is_err(), "{entry:?} should be refused");
        }
    }

    #[test]
    fn install_pattern_matches_case_table() {
        let cases: &[(&str, &str, bool)] = &[
            ("*", "ana/anything", true),
            ("*", "murmur/a", true),
            ("murmur/*", "murmur/tool-git", true),
            ("murmur/*", "ana/tool-git", false),
            ("ana/style-*", "ana/style-rust", true),
            ("ana/style-*", "ana/stylist", false),
            ("ana/style-*", "ana/style", false),
            ("ana/style-*", "bob/style-rust", false),
            ("ana/style*", "ana/styles", true),
            ("ana/style*", "ana/style", true),
            ("ana/code-review", "ana/code-review", true),
            ("ana/code-review", "bob/code-review", false),
            ("ana/code-review", "ana/code-review-2", false),
            ("a/x*y", "a/xzy", false),
            ("code-review", "ana/code-review", false),
        ];
        for (pattern, candidate, expected) in cases {
            assert_eq!(
                matches(pattern, &name(candidate)),
                *expected,
                "matches({pattern:?}, {candidate:?})"
            );
        }
    }

    const COVER_CASES: &[(&str, &str, bool)] = &[
        ("*", "*", true),
        ("*", "ana/style-*", true),
        ("*", "ana/jq", true),
        ("ana/*", "ana/style-*", true),
        ("ana/*", "bob/*", false),
        ("ana/*", "*", false),
        ("ana/style-*", "*", false),
        ("ana/style-*", "ana/style-*", true),
        ("ana/style-*", "bob/style-*", false),
        ("ana/style-*", "ana/style-rust*", true),
        ("ana/style-*", "ana/style*", false),
        ("ana/style-*", "ana/style-rust", true),
        ("ana/style-*", "bob/style-rust", false),
        ("ana/style-*", "ana/stylist", false),
        ("ana/style*", "ana/style-*", true),
        ("ana/jq", "ana/jq", true),
        ("ana/jq", "bob/jq", false),
        ("ana/jq", "ana/jq*", false),
        ("ana/jq", "*", false),
        ("ana/j*", "ana/jq", true),
        ("a/x*y", "a/x", false),
        ("*", "a/x*y", false),
        ("jq", "jq", false),
    ];

    #[test]
    fn install_pattern_covers_case_table() {
        for (parent, child, expected) in COVER_CASES {
            assert_eq!(
                covers(parent, child),
                *expected,
                "covers({parent:?}, {child:?})"
            );
        }
    }

    #[test]
    fn install_pattern_a_covering_parent_matches_every_name_its_child_matches() {
        let names: Vec<ArtifactName> = [
            "ana/a",
            "ana/jq",
            "bob/jq",
            "ana/style",
            "ana/style-rust",
            "bob/style-rust",
            "ana/style-rust-2",
            "ana/stylist",
            "ana/styles",
            "murmur/code-review",
            "local/x",
        ]
        .into_iter()
        .map(name)
        .collect();
        let entries: Vec<&str> = COVER_CASES
            .iter()
            .flat_map(|(p, c, _)| [*p, *c])
            .chain(["murmur/*", "ana/code-review", "local/x"])
            .collect();
        for parent in &entries {
            for child in &entries {
                if !covers(parent, child) {
                    continue;
                }
                for candidate in &names {
                    if matches(child, candidate) {
                        assert!(
                            matches(parent, candidate),
                            "covers({parent:?}, {child:?}) holds but {parent:?} does not match \
                             {candidate}, which {child:?} does"
                        );
                    }
                }
            }
        }
    }
}
