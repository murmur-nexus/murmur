//! The full name of a resolvable artifact: `<scope>/<name>`.
//!
//! The scope says whose artifact it is: the GitHub owner of the repository it is published
//! from, [`MURMUR_SCOPE`] for `murmur-nexus/default-artifacts`, or [`LOCAL_SCOPE`] for one
//! installed from a `.mur.zip` file. The name is the artifact's own `name:`, the bare part a
//! staged session uses as the tool name. Each part follows the bare-name rule,
//! [`artifact_name_format_error`].
//!
//! Every place that takes a name to resolve validates it through [`ArtifactName`]; a bare name is
//! refused with [`unscoped_name_hint`], one hint text for every surface.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

use crate::build::{artifact_name_format_error, MAX_ARTIFACT_NAME_LEN};

/// The scope of an artifact installed from a `.mur.zip` file or written by `mur publish`. No
/// source is ever asked for it.
pub const LOCAL_SCOPE: &str = "local";

/// The scope of the artifacts published from `murmur-nexus/default-artifacts`.
pub const MURMUR_SCOPE: &str = "murmur";

/// The scopes no `github:` owner can claim.
pub const RESERVED_SCOPES: [&str; 2] = [LOCAL_SCOPE, MURMUR_SCOPE];

/// A validated `<scope>/<name>`. Ordered, compared and hashed as its text; serialized as a plain
/// string.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ArtifactName {
    full: String,
    slash: usize,
}

/// Why a string is not a full artifact name.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ArtifactNameError {
    /// A valid bare name with no scope. [`ArtifactNameError::hint`] names the scoped forms.
    #[error("'{name}' is not a full artifact name: an artifact is named <scope>/<name>")]
    Unscoped { name: String },
    /// Not `<scope>/<name>` with two valid parts: more than one `/`, an empty part, or a
    /// character the part rule refuses.
    #[error("'{name}' is not a valid artifact name: {reason}")]
    Malformed { name: String, reason: String },
}

impl ArtifactNameError {
    /// The hint to print beside the refusal: the scoped forms of a bare name, `None` for a
    /// malformed one, whose message already carries the reason.
    #[must_use]
    pub fn hint(&self) -> Option<String> {
        match self {
            Self::Unscoped { name } => Some(unscoped_name_hint(name)),
            Self::Malformed { .. } => None,
        }
    }

    /// The text that was refused.
    #[must_use]
    pub fn input(&self) -> &str {
        match self {
            Self::Unscoped { name } | Self::Malformed { name, .. } => name,
        }
    }
}

impl ArtifactName {
    /// Parse `<scope>/<name>`.
    pub fn parse(input: &str) -> Result<Self, ArtifactNameError> {
        let malformed = |reason: String| ArtifactNameError::Malformed {
            name: input.to_string(),
            reason,
        };
        let Some((scope, name)) = input.split_once('/') else {
            return match artifact_name_format_error(input) {
                None => Err(ArtifactNameError::Unscoped {
                    name: input.to_string(),
                }),
                Some(reason) => Err(malformed(format!("the name {reason}"))),
            };
        };
        if name.contains('/') {
            return Err(malformed(
                "expected exactly one '/', between the scope and the name".to_string(),
            ));
        }
        if let Some(reason) = artifact_name_format_error(scope) {
            return Err(malformed(format!("the scope {reason}")));
        }
        if let Some(reason) = artifact_name_format_error(name) {
            return Err(malformed(format!("the name {reason}")));
        }
        Ok(Self {
            full: input.to_string(),
            slash: scope.len(),
        })
    }

    /// The full name of the artifact `name` in `scope`.
    pub fn from_parts(scope: &str, name: &str) -> Result<Self, ArtifactNameError> {
        Self::parse(&format!("{scope}/{name}"))
    }

    /// Who the artifact belongs to: a GitHub owner, [`MURMUR_SCOPE`] or [`LOCAL_SCOPE`].
    #[must_use]
    pub fn scope(&self) -> &str {
        &self.full[..self.slash]
    }

    /// The bare part: the artifact's own `name:`, and its tool name inside a session.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.full[self.slash + 1..]
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.full
    }

    /// Whether this artifact was installed from a `.mur.zip` file, which no source serves.
    #[must_use]
    pub fn is_local(&self) -> bool {
        self.scope() == LOCAL_SCOPE
    }
}

impl fmt::Display for ArtifactName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(&self.full)
    }
}

impl FromStr for ArtifactName {
    type Err = ArtifactNameError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        Self::parse(input)
    }
}

impl AsRef<str> for ArtifactName {
    fn as_ref(&self) -> &str {
        &self.full
    }
}

impl PartialEq<str> for ArtifactName {
    fn eq(&self, other: &str) -> bool {
        self.full == other
    }
}

impl PartialEq<&str> for ArtifactName {
    fn eq(&self, other: &&str) -> bool {
        self.full == *other
    }
}

impl Serialize for ArtifactName {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.full)
    }
}

impl<'de> Deserialize<'de> for ArtifactName {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).map_err(serde::de::Error::custom)
    }
}

/// The hint printed wherever a bare name is refused: the three scoped forms of `bare`, saying
/// which is which. `murmur/` drops a leading `murmur-`, so `tool-editor` and
/// `murmur-tool-editor` both point at `murmur/tool-editor`.
#[must_use]
pub fn unscoped_name_hint(bare: &str) -> String {
    let default_name = bare.strip_prefix("murmur-").unwrap_or(bare);
    format!(
        "name it with its scope: '{MURMUR_SCOPE}/{default_name}' for a Murmur default artifact, \
         '<owner>/{bare}' for one published from github.com/<owner>, or '{LOCAL_SCOPE}/{bare}' \
         for one installed from a .mur.zip file"
    )
}

/// The reason the text after `<scope>/` in an install-entry prefix (`<scope>/<prefix>*`) is not
/// valid, or `None` when it is. The prefix may be empty (`<scope>/*`) and may end with `-`
/// (`ana/tool-*`); otherwise it follows the part rule.
#[must_use]
pub fn prefix_part_error(prefix: &str) -> Option<String> {
    if prefix.is_empty() {
        return None;
    }
    if prefix.chars().count() > MAX_ARTIFACT_NAME_LEN {
        return Some(format!(
            "the prefix must be at most {MAX_ARTIFACT_NAME_LEN} characters"
        ));
    }
    if prefix.starts_with('-') {
        return Some("the prefix must not start with '-'".to_string());
    }
    // A prefix may stop where a name continues with `-`; past that, it is held to the part rule.
    artifact_name_format_error(prefix.trim_end_matches('-'))
        .map(|reason| format!("the prefix {reason}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_name_accepts_scope_and_name() {
        for (input, scope, name) in [
            ("murmur/tool-editor", "murmur", "tool-editor"),
            ("ana/tool-x", "ana", "tool-x"),
            ("local/x", "local", "x"),
            ("a/b", "a", "b"),
        ] {
            let parsed = ArtifactName::parse(input).unwrap();
            assert_eq!(parsed.scope(), scope);
            assert_eq!(parsed.name(), name);
            assert_eq!(parsed.as_str(), input);
            assert_eq!(parsed.to_string(), input);
        }
    }

    #[test]
    fn artifact_name_refuses_a_bare_name_with_the_scoped_forms() {
        for bare in ["tool-editor", "murmur-tool-editor"] {
            let error = ArtifactName::parse(bare).unwrap_err();
            assert!(matches!(error, ArtifactNameError::Unscoped { .. }));
            let hint = error.hint().unwrap();
            assert!(hint.contains("murmur/tool-editor"), "{hint}");
            assert!(hint.contains(&format!("<owner>/{bare}")), "{hint}");
            assert!(hint.contains(&format!("local/{bare}")), "{hint}");
        }
    }

    #[test]
    fn artifact_name_refuses_malformed_names_with_the_part_reason() {
        let long = format!("a/{}", "b".repeat(MAX_ARTIFACT_NAME_LEN + 1));
        for input in [
            "a/b/c",
            "/x",
            "x/",
            "A/x",
            "a/-x",
            "a/x-",
            "a/X",
            "",
            "Tool",
            long.as_str(),
        ] {
            let error = ArtifactName::parse(input).unwrap_err();
            assert!(
                matches!(error, ArtifactNameError::Malformed { .. }),
                "{input:?}: {error:?}"
            );
            assert!(error.hint().is_none(), "{input:?}");
        }
        let error = ArtifactName::parse(&long).unwrap_err();
        assert!(error.to_string().contains("at most 100"), "{error}");
        let error = ArtifactName::parse("a/b/c").unwrap_err();
        assert!(error.to_string().contains("exactly one '/'"), "{error}");
    }

    #[test]
    fn artifact_name_round_trips_through_serde_as_a_string() {
        let name = ArtifactName::parse("ana/tool-x").unwrap();
        let json = serde_json::to_string(&name).unwrap();
        assert_eq!(json, "\"ana/tool-x\"");
        let back: ArtifactName = serde_json::from_str(&json).unwrap();
        assert_eq!(back, name);
        assert!(serde_json::from_str::<ArtifactName>("\"tool-x\"").is_err());
    }

    #[test]
    fn artifact_name_orders_by_full_name() {
        let mut names: Vec<ArtifactName> = ["murmur/b", "ana/z", "ana/a", "local/c"]
            .into_iter()
            .map(|n| ArtifactName::parse(n).unwrap())
            .collect();
        names.sort();
        let order: Vec<&str> = names.iter().map(ArtifactName::as_str).collect();
        assert_eq!(order, ["ana/a", "ana/z", "local/c", "murmur/b"]);
    }

    #[test]
    fn artifact_name_prefix_part_rule() {
        for ok in ["", "tool-", "tool", "t", "a1-b-"] {
            assert_eq!(prefix_part_error(ok), None, "{ok:?}");
        }
        for bad in ["-x", "X", "a_b", "a/b", "*"] {
            assert!(prefix_part_error(bad).is_some(), "{bad:?}");
        }
        let long = "a".repeat(MAX_ARTIFACT_NAME_LEN + 1);
        assert!(prefix_part_error(&long).is_some());
    }
}
