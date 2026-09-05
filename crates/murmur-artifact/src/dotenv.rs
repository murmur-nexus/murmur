use std::{collections::BTreeSet, fs, path::Path};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum DotenvError {
    #[error("failed to read .env at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid .env entry at line {line}: {message}")]
    InvalidLine { line: usize, message: String },
}

pub fn load_dotenv_non_override(workspace_root: &Path) -> Result<(), DotenvError> {
    for (key, value) in read_declarations(workspace_root)? {
        if std::env::var_os(&key).is_some() {
            continue;
        }

        std::env::set_var(key, value);
    }

    Ok(())
}

/// The variable names `<workspace_root>/.env` declares, without their values.
///
/// Reads the same declarations [`load_dotenv_non_override`] would set, through the same parse, so
/// a caller asking which names a run would have cannot disagree with the loader about what counts
/// as one. No value is returned, and no variable is set. A missing `.env` declares nothing.
pub fn dotenv_variable_names(workspace_root: &Path) -> Result<BTreeSet<String>, DotenvError> {
    Ok(read_declarations(workspace_root)?
        .into_iter()
        .map(|(key, _)| key)
        .collect())
}

/// Every `KEY=VALUE` `<workspace_root>/.env` declares, in file order, with the value unquoted.
///
/// The one parse both public entry points share. A missing file yields no declarations; a line
/// with no `=`, or with an empty name, is [`DotenvError::InvalidLine`].
fn read_declarations(workspace_root: &Path) -> Result<Vec<(String, String)>, DotenvError> {
    let dotenv_path = workspace_root.join(".env");
    if !dotenv_path.exists() {
        return Ok(Vec::new());
    }

    let raw = fs::read_to_string(&dotenv_path).map_err(|source| DotenvError::Io {
        path: dotenv_path.display().to_string(),
        source,
    })?;

    let mut declarations = Vec::new();
    for (idx, line) in raw.lines().enumerate() {
        let line_no = idx + 1;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let Some((key, value)) = trimmed.split_once('=') else {
            return Err(DotenvError::InvalidLine {
                line: line_no,
                message: "expected KEY=VALUE".to_string(),
            });
        };

        let key = key.trim();
        if key.is_empty() {
            return Err(DotenvError::InvalidLine {
                line: line_no,
                message: "empty variable name".to_string(),
            });
        }

        declarations.push((key.to_string(), unquote(value.trim())));
    }

    Ok(declarations)
}

fn unquote(value: &str) -> String {
    if (value.starts_with('"') && value.ends_with('"'))
        || (value.starts_with('\'') && value.ends_with('\''))
    {
        return value[1..value.len() - 1].to_string();
    }

    value.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_read_without_setting_or_returning_a_value() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".env"),
            "# a comment\n\nFROM_DOTENV=secret-value\nQUOTED=\"also-secret\"\n",
        )
        .unwrap();

        let names = dotenv_variable_names(dir.path()).unwrap();

        assert_eq!(
            names,
            ["FROM_DOTENV".to_string(), "QUOTED".to_string()]
                .into_iter()
                .collect()
        );
        assert!(std::env::var_os("FROM_DOTENV").is_none());
    }

    #[test]
    fn a_missing_dotenv_declares_no_names() {
        let dir = tempfile::tempdir().unwrap();
        assert!(dotenv_variable_names(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn a_line_without_an_equals_is_rejected_by_both_readers() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".env"), "OK=1\nnot-a-declaration\n").unwrap();

        match dotenv_variable_names(dir.path()).unwrap_err() {
            DotenvError::InvalidLine { line, .. } => assert_eq!(line, 2),
            other => panic!("expected InvalidLine, got {other:?}"),
        }
        match load_dotenv_non_override(dir.path()).unwrap_err() {
            DotenvError::InvalidLine { line, .. } => assert_eq!(line, 2),
            other => panic!("expected InvalidLine, got {other:?}"),
        }
    }

    #[test]
    fn does_not_override_existing_environment_variable() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".env"), "HELLO=from-dotenv\n").unwrap();

        std::env::set_var("HELLO", "already-set");
        load_dotenv_non_override(dir.path()).unwrap();

        assert_eq!(std::env::var("HELLO").unwrap(), "already-set");
    }
}
