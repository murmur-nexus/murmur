use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtifactRef {
    BareName(String),
    GitHub {
        owner: String,
        repo: String,
        tag: String,
    },
}

impl ArtifactRef {
    pub fn parse(input: &str) -> Result<Self, ArtifactRefError> {
        let value = input.trim();
        if value.is_empty() {
            return Err(ArtifactRefError::Empty);
        }

        if let Some(rest) = value.strip_prefix("github:") {
            return parse_github(rest);
        }

        if value.contains(':') {
            let prefix = value.split(':').next().unwrap_or_default();
            return Err(ArtifactRefError::UnknownSourcePrefix(prefix.to_string()));
        }

        Ok(Self::BareName(value.to_string()))
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ArtifactRefError {
    #[error("artifact reference cannot be empty")]
    Empty,
    #[error("invalid github artifact reference '{0}' (expected github:<owner>/<repo>@<tag>)")]
    InvalidGitHub(String),
    #[error("unsupported artifact source prefix '{0}'")]
    UnknownSourcePrefix(String),
}

fn parse_github(rest: &str) -> Result<ArtifactRef, ArtifactRefError> {
    let Some((repo_ref, tag)) = rest.split_once('@') else {
        return Err(ArtifactRefError::InvalidGitHub(format!("github:{rest}")));
    };

    let Some((owner, repo)) = repo_ref.split_once('/') else {
        return Err(ArtifactRefError::InvalidGitHub(format!("github:{rest}")));
    };

    if owner.trim().is_empty() || repo.trim().is_empty() || tag.trim().is_empty() {
        return Err(ArtifactRefError::InvalidGitHub(format!("github:{rest}")));
    }

    Ok(ArtifactRef::GitHub {
        owner: owner.to_string(),
        repo: repo.to_string(),
        tag: tag.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_ref_parses_bare_name() {
        assert_eq!(
            ArtifactRef::parse("murmur-driver-anthropic").unwrap(),
            ArtifactRef::BareName("murmur-driver-anthropic".to_string())
        );
    }

    #[test]
    fn artifact_ref_parses_github_uri() {
        assert_eq!(
            ArtifactRef::parse("github:murmur-nexus/default-artifacts@v0.1.1").unwrap(),
            ArtifactRef::GitHub {
                owner: "murmur-nexus".to_string(),
                repo: "default-artifacts".to_string(),
                tag: "v0.1.1".to_string(),
            }
        );
    }

    #[test]
    fn artifact_ref_rejects_malformed_github_uri() {
        let error = ArtifactRef::parse("github:murmur-nexus/default-artifacts").unwrap_err();
        assert!(matches!(error, ArtifactRefError::InvalidGitHub(_)));
    }
}
