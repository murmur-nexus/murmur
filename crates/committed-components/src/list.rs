//! The one reader of `components.list`.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use crate::LIST_PATH;

/// One line of the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Component {
    /// The first column; what `scripts/rebuild-components.sh <name>` takes.
    pub name: String,
    /// The WIT directory the component's `wit_bindgen::generate!` names as `path:`, relative to
    /// the repository root. `None` only on a frozen line.
    pub wit: Option<PathBuf>,
    /// The cargo package directory it is built from. `None` only on a frozen line.
    pub source: Option<PathBuf>,
    /// The cargo features it is built with; empty for the package's defaults.
    pub features: Vec<String>,
    /// The committed `.wasm`, relative to the repository root.
    pub output: PathBuf,
    /// Why no mode ever builds or writes this component; `None` when it is rebuildable.
    pub frozen: Option<String>,
    /// 1-based line number in the list.
    pub line: usize,
}

impl Component {
    /// Whether the line carries `frozen: <reason>`.
    pub fn is_frozen(&self) -> bool {
        self.frozen.is_some()
    }
}

/// Why the list could not be read. The message names the list and the offending line numbers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListError(pub String);

impl fmt::Display for ListError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ListError {}

const COLUMNS: &str = "name wit source features output";
const FROZEN: &str = "frozen:";

/// Parses the list's text. Text only: nothing on disk is consulted, which is [`read_list`]'s job.
///
/// Errors on a line without exactly the five columns before an optional `frozen: <reason>`, a
/// `frozen:` with no reason, a non-frozen line whose `wit` or `source` is `-`, an `output` of `-`,
/// a name starting with `-`, an empty feature, and a name or output that an earlier line already
/// uses. Every error names its line numbers.
pub fn parse_list(text: &str) -> Result<Vec<Component>, ListError> {
    let mut components = Vec::new();
    let mut names: HashMap<String, usize> = HashMap::new();
    let mut outputs: HashMap<PathBuf, usize> = HashMap::new();

    for (index, raw) in text.lines().enumerate() {
        let line = index + 1;
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let err = |message: String| ListError(format!("{LIST_PATH} line {line}: {message}"));

        let (columns_text, frozen) = match split_frozen(trimmed) {
            Some((_, "")) => return Err(err(format!("`{FROZEN}` needs a reason after it"))),
            Some((columns, reason)) => (columns, Some(reason.to_string())),
            None => (trimmed, None),
        };
        let columns: Vec<&str> = columns_text.split_whitespace().collect();
        let [name, wit, source, features, output] = columns[..] else {
            return Err(err(format!(
                "expected the columns `{COLUMNS}`, optionally followed by `{FROZEN} <reason>`; \
                 found {} column(s)",
                columns.len()
            )));
        };

        if name.starts_with('-') {
            return Err(err(format!("`{name}` is not a usable name")));
        }
        if output == "-" {
            return Err(err(format!("`{name}` names no output")));
        }
        if frozen.is_none() && (wit == "-" || source == "-") {
            return Err(err(format!(
                "`{name}` is not frozen, so it needs both a wit directory and a source; \
                 a line that cannot be rebuilt ends with `{FROZEN} <reason>`"
            )));
        }
        let features = if features == "-" {
            Vec::new()
        } else {
            let split: Vec<String> = features.split(',').map(str::to_string).collect();
            if split.iter().any(String::is_empty) {
                return Err(err(format!(
                    "`{name}` has an empty feature in `{features}`"
                )));
            }
            split
        };

        if let Some(first) = names.insert(name.to_string(), line) {
            return Err(err(format!(
                "the name `{name}` is already used on line {first}"
            )));
        }
        let output = PathBuf::from(output);
        if let Some(first) = outputs.insert(output.clone(), line) {
            return Err(err(format!(
                "the output {} is already listed on line {first}",
                output.display()
            )));
        }

        components.push(Component {
            name: name.to_string(),
            wit: optional_path(wit),
            source: optional_path(source),
            features,
            output,
            frozen,
            line,
        });
    }
    Ok(components)
}

/// Reads and parses `<repo_root>/LIST_PATH`, then checks the directories it names: a `wit` that
/// is named has to be a directory, and a `source` that is named has to hold `Cargo.toml`. A
/// missing `output` is not an error here; [`crate::check`] reports it against the component.
pub fn read_list(repo_root: &Path) -> Result<Vec<Component>, ListError> {
    let path = repo_root.join(LIST_PATH);
    let text = fs::read_to_string(&path)
        .map_err(|err| ListError(format!("could not read {}: {err}", path.display())))?;
    let components = parse_list(&text)?;
    for component in &components {
        let err = |message: String| {
            ListError(format!(
                "{LIST_PATH} line {} (`{}`): {message}",
                component.line, component.name
            ))
        };
        if let Some(wit) = &component.wit {
            if !repo_root.join(wit).is_dir() {
                return Err(err(format!(
                    "the wit directory {} does not exist",
                    wit.display()
                )));
            }
        }
        if let Some(source) = &component.source {
            if !repo_root.join(source).join("Cargo.toml").is_file() {
                return Err(err(format!(
                    "the source {} holds no Cargo.toml",
                    source.display()
                )));
            }
        }
    }
    Ok(components)
}

/// The listed component called `name`. Errors, pointing at the list's first column, when no line
/// is called that.
pub fn find_component<'a>(list: &'a [Component], name: &str) -> Result<&'a Component, ListError> {
    list.iter()
        .find(|component| component.name == name)
        .ok_or_else(|| {
            ListError(format!(
                "no component is called `{name}`; the names are the first column of {LIST_PATH}"
            ))
        })
}

/// Splits a trimmed line at a whitespace-separated `frozen:` token into the columns before it
/// and the trimmed reason after it.
fn split_frozen(line: &str) -> Option<(&str, &str)> {
    let mut search = 0;
    while let Some(found) = line[search..].find(FROZEN) {
        let at = search + found;
        let starts_token = at == 0 || line[..at].ends_with(char::is_whitespace);
        if starts_token {
            return Some((&line[..at], line[at + FROZEN.len()..].trim()));
        }
        search = at + FROZEN.len();
    }
    None
}

fn optional_path(column: &str) -> Option<PathBuf> {
    (column != "-").then(|| PathBuf::from(column))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rebuildable_and_a_frozen_line_parse() {
        let list = parse_list(
            "# header\n\
             \n\
             probe  wit/guest  src/probe  a,b  out/probe.wasm\n\
             old    -          -          -    out/old.wasm  frozen: built elsewhere;  see X\n",
        )
        .unwrap();
        assert_eq!(
            list,
            vec![
                Component {
                    name: "probe".to_string(),
                    wit: Some(PathBuf::from("wit/guest")),
                    source: Some(PathBuf::from("src/probe")),
                    features: vec!["a".to_string(), "b".to_string()],
                    output: PathBuf::from("out/probe.wasm"),
                    frozen: None,
                    line: 3,
                },
                Component {
                    name: "old".to_string(),
                    wit: None,
                    source: None,
                    features: Vec::new(),
                    output: PathBuf::from("out/old.wasm"),
                    frozen: Some("built elsewhere;  see X".to_string()),
                    line: 4,
                },
            ]
        );
    }

    #[test]
    fn a_line_with_the_wrong_number_of_columns_is_refused() {
        let err = parse_list("probe wit/guest src/probe out/probe.wasm\n").unwrap_err();
        assert!(err.0.contains("line 1"), "{err}");
        assert!(err.0.contains("found 4 column(s)"), "{err}");

        let err = parse_list("\nprobe w s - o extra\n").unwrap_err();
        assert!(err.0.contains("line 2"), "{err}");
    }

    #[test]
    fn a_duplicate_name_is_refused_naming_both_lines() {
        let err = parse_list("a w s - one.wasm\n# c\nb w s - two.wasm\na w s - three.wasm\n")
            .unwrap_err();
        assert!(err.0.contains("line 4"), "{err}");
        assert!(err.0.contains("`a` is already used on line 1"), "{err}");
    }

    #[test]
    fn a_duplicate_output_is_refused_naming_both_lines() {
        let err = parse_list("a w s - same.wasm\nb w s - same.wasm\n").unwrap_err();
        assert!(err.0.contains("line 2"), "{err}");
        assert!(err.0.contains("already listed on line 1"), "{err}");
    }

    #[test]
    fn frozen_with_no_reason_is_refused() {
        for line in [
            "a - - - out.wasm frozen:\n",
            "a - - - out.wasm frozen:   \n",
        ] {
            let err = parse_list(line).unwrap_err();
            assert!(err.0.contains("needs a reason"), "{err}");
        }
    }

    #[test]
    fn a_non_frozen_line_needs_wit_and_source() {
        for line in ["a - s - out.wasm\n", "a w - - out.wasm\n"] {
            let err = parse_list(line).unwrap_err();
            assert!(err.0.contains("is not frozen"), "{err}");
        }
    }

    #[test]
    fn an_empty_feature_and_a_missing_output_are_refused() {
        assert!(parse_list("a w s x,,y out.wasm\n")
            .unwrap_err()
            .0
            .contains("empty feature"));
        assert!(parse_list("a w s - -\n")
            .unwrap_err()
            .0
            .contains("names no output"));
    }

    #[test]
    fn frozen_inside_a_column_is_not_the_marker() {
        let list = parse_list("a w s - out/notfrozen:.wasm\n").unwrap();
        assert!(!list[0].is_frozen());
    }
}
