use std::{fs, path::Path};

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
    let dotenv_path = workspace_root.join(".env");
    if !dotenv_path.exists() {
        return Ok(());
    }

    let raw = fs::read_to_string(&dotenv_path).map_err(|source| DotenvError::Io {
        path: dotenv_path.display().to_string(),
        source,
    })?;

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

        if std::env::var_os(key).is_some() {
            continue;
        }

        std::env::set_var(key, unquote(value.trim()));
    }

    Ok(())
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
    fn does_not_override_existing_environment_variable() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".env"), "HELLO=from-dotenv\n").unwrap();

        std::env::set_var("HELLO", "already-set");
        load_dotenv_non_override(dir.path()).unwrap();

        assert_eq!(std::env::var("HELLO").unwrap(), "already-set");
    }
}
