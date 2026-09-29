//! The entry grammar of `capabilities.install.skill` and `capabilities.install.tool`.
//!
//! An entry is one of three forms:
//!
//! - `*`, every artifact name;
//! - `<prefix>*`, every artifact name that begins with `<prefix>`. The prefix is non-empty, uses
//!   only artifact-name characters, and does not begin with `-`; it may end with `-`
//!   (`review-*`);
//! - an exact artifact name, as [`artifact_name_format_error`] accepts it.
//!
//! The manifest parser validates entries with [`validate`], the runtime's pull gate decides a name
//! with [`matches`], and the spawn referee compares a child's entries against its parent's with
//! [`covers`]. `covers(parent, child)` holds only when every name `child` matches is also matched
//! by `parent`.

use crate::build::{artifact_name_format_error, MAX_ARTIFACT_NAME_LEN};

/// The accepted entry forms, quoted in every refusal of an entry.
pub const INSTALL_ENTRY_ACCEPTED_FORM: &str =
    "an artifact name (lowercase letters, digits and inner '-'), a prefix ending in a single \
     trailing '*' (e.g. 'style-*'), or '*' alone";

enum Entry<'a> {
    Any,
    Prefix(&'a str),
    Exact(&'a str),
}

fn classify(entry: &str) -> Result<Entry<'_>, String> {
    if entry == "*" {
        return Ok(Entry::Any);
    }
    if let Some(prefix) = entry.strip_suffix('*') {
        if prefix.is_empty() {
            return Err("the prefix before '*' is empty".to_string());
        }
        if prefix.contains('*') {
            return Err("'*' may appear only once, as the last character".to_string());
        }
        if prefix.chars().count() > MAX_ARTIFACT_NAME_LEN {
            return Err(format!(
                "the prefix must be at most {MAX_ARTIFACT_NAME_LEN} characters"
            ));
        }
        if let Some(bad) = prefix
            .chars()
            .find(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-'))
        {
            return Err(format!(
                "the prefix may contain only lowercase letters, digits and '-' (found {bad:?})"
            ));
        }
        if prefix.starts_with('-') {
            return Err("the prefix must not start with '-'".to_string());
        }
        return Ok(Entry::Prefix(prefix));
    }
    if entry.contains('*') {
        return Err("'*' may appear only once, as the last character".to_string());
    }
    match artifact_name_format_error(entry) {
        Some(reason) => Err(format!("not an artifact name: {reason}")),
        None => Ok(Entry::Exact(entry)),
    }
}

/// `Ok` when `entry` is one of the accepted forms, else the reason it is not.
///
/// The reason describes the defect only; callers quote the entry and
/// [`INSTALL_ENTRY_ACCEPTED_FORM`] around it.
pub fn validate(entry: &str) -> Result<(), String> {
    classify(entry).map(|_| ())
}

/// Whether the entry `pattern` admits the artifact `name`.
///
/// `false` whenever `name` is not a valid artifact name, including under `*`, and whenever
/// `pattern` fails [`validate`].
pub fn matches(pattern: &str, name: &str) -> bool {
    if artifact_name_format_error(name).is_some() {
        return false;
    }
    match classify(pattern) {
        Ok(Entry::Any) => true,
        Ok(Entry::Prefix(prefix)) => name.starts_with(prefix),
        Ok(Entry::Exact(exact)) => exact == name,
        Err(_) => false,
    }
}

/// Whether the parent entry `parent` admits every name the child entry `child` admits.
///
/// `*` covers everything; `p*` covers an exact name or a `q*` whose text begins with `p`; an exact
/// name covers only itself. `false` when either side fails [`validate`].
pub fn covers(parent: &str, child: &str) -> bool {
    let (Ok(parent), Ok(child)) = (classify(parent), classify(child)) else {
        return false;
    };
    match (parent, child) {
        (Entry::Any, _) => true,
        (_, Entry::Any) => false,
        (Entry::Prefix(p), Entry::Prefix(q)) => q.starts_with(p),
        (Entry::Prefix(p), Entry::Exact(c)) => c.starts_with(p),
        (Entry::Exact(_), Entry::Prefix(_)) => false,
        (Entry::Exact(n), Entry::Exact(c)) => n == c,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_accepts_the_three_forms() {
        for entry in [
            "*",
            "style-*",
            "s*",
            "a1*",
            "code-review",
            "jq",
            "a",
            "x9-y",
        ] {
            assert!(validate(entry).is_ok(), "{entry:?} should be accepted");
        }
    }

    #[test]
    fn validate_refuses_everything_else() {
        let too_long_prefix = format!("{}*", "a".repeat(MAX_ARTIFACT_NAME_LEN + 1));
        let too_long_name = "a".repeat(MAX_ARTIFACT_NAME_LEN + 1);
        for entry in [
            "",
            " ",
            "a*b",
            "*x",
            "**",
            "a**",
            "Foo",
            "Foo*",
            "-a",
            "-a*",
            "a-",
            "a/b",
            "a/*",
            "github:owner/repo",
            "a b",
            "a:b",
            too_long_prefix.as_str(),
            too_long_name.as_str(),
        ] {
            assert!(validate(entry).is_err(), "{entry:?} should be refused");
        }
    }

    #[test]
    fn matches_case_table() {
        let cases: &[(&str, &str, bool)] = &[
            ("*", "anything", true),
            ("*", "a", true),
            ("*", "../escape", false),
            ("*", "Upper", false),
            ("*", "a/b", false),
            ("*", "", false),
            ("*", "github:owner/repo@v1", false),
            ("style-*", "style-rust", true),
            ("style-*", "style-", false),
            ("style-*", "stylist", false),
            ("style-*", "style", false),
            ("style*", "styles", true),
            ("style*", "style", true),
            ("code-review", "code-review", true),
            ("code-review", "code-review-2", false),
            ("code-review", "code", false),
            ("a*b", "axb", false),
            ("Foo", "Foo", false),
        ];
        for (pattern, name, expected) in cases {
            assert_eq!(
                matches(pattern, name),
                *expected,
                "matches({pattern:?}, {name:?})"
            );
        }
    }

    const COVER_CASES: &[(&str, &str, bool)] = &[
        ("*", "*", true),
        ("*", "style-*", true),
        ("*", "jq", true),
        ("style-*", "*", false),
        ("style-*", "style-*", true),
        ("style-*", "style-rust*", true),
        ("style-*", "style*", false),
        ("style-*", "style-rust", true),
        ("style-*", "stylist", false),
        ("style*", "style-*", true),
        ("style*", "styles", true),
        ("jq", "jq", true),
        ("jq", "yq", false),
        ("jq", "jq*", false),
        ("jq", "*", false),
        ("j*", "jq", true),
        ("a*b", "a", false),
        ("*", "a*b", false),
        ("Foo", "Foo", false),
    ];

    #[test]
    fn covers_case_table() {
        for (parent, child, expected) in COVER_CASES {
            assert_eq!(
                covers(parent, child),
                *expected,
                "covers({parent:?}, {child:?})"
            );
        }
    }

    #[test]
    fn a_covering_parent_matches_every_name_its_child_matches() {
        let names = [
            "a",
            "jq",
            "yq",
            "style",
            "style-",
            "style-rust",
            "style-rust-2",
            "stylist",
            "styles",
            "code-review",
            "lint-rs",
            "x",
            "Upper",
            "../escape",
            "",
        ];
        let entries: Vec<&str> = COVER_CASES
            .iter()
            .flat_map(|(p, c, _)| [*p, *c])
            .chain(["lint-*", "code-review", "a"])
            .collect();
        for parent in &entries {
            for child in &entries {
                if !covers(parent, child) {
                    continue;
                }
                for name in names {
                    if matches(child, name) {
                        assert!(
                            matches(parent, name),
                            "covers({parent:?}, {child:?}) holds but {parent:?} does not match \
                             {name:?}, which {child:?} does"
                        );
                    }
                }
            }
        }
    }
}
