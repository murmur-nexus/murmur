use std::path::{Component as PathComponent, Path};

use crate::errors::RuntimeError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NetworkAllowRule {
    pub(crate) scheme: Option<String>,
    pub(crate) host: String,
    pub(crate) port: Option<u16>,
}

impl NetworkAllowRule {
    pub(crate) fn parse(entry: &str) -> Result<Self, RuntimeError> {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            return Err(RuntimeError::InvalidNetworkAllowEntry {
                entry: entry.to_string(),
                message: "entry must not be empty".to_string(),
            });
        }

        if trimmed.contains("://") {
            parse_url_allow_rule(trimmed)
        } else {
            parse_host_allow_rule(trimmed)
        }
    }

    pub(crate) fn matches(&self, target: &RequestTarget) -> bool {
        if self.host != target.host {
            return false;
        }

        if let Some(expected_scheme) = &self.scheme {
            if expected_scheme != &target.scheme {
                return false;
            }
        }

        match self.port {
            Some(expected_port) => target.port == Some(expected_port),
            None => true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RequestTarget {
    pub(crate) scheme: String,
    pub(crate) host: String,
    pub(crate) port: Option<u16>,
}

impl RequestTarget {
    pub(crate) fn from_request(uri: &http::Uri, use_tls: bool) -> Option<Self> {
        let authority = uri.authority()?;
        let scheme = uri
            .scheme_str()
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_else(|| if use_tls { "https" } else { "http" }.to_string());

        let port = authority
            .port_u16()
            .or_else(|| default_port_for_scheme(&scheme));

        Some(Self {
            scheme,
            host: authority.host().to_ascii_lowercase(),
            port,
        })
    }
}

pub(crate) fn parse_network_allow_rules(
    entries: &[String],
) -> Result<Vec<NetworkAllowRule>, RuntimeError> {
    entries
        .iter()
        .map(|entry| NetworkAllowRule::parse(entry))
        .collect()
}

fn parse_url_allow_rule(entry: &str) -> Result<NetworkAllowRule, RuntimeError> {
    let url = url::Url::parse(entry).map_err(|err| RuntimeError::InvalidNetworkAllowEntry {
        entry: entry.to_string(),
        message: format!("invalid URL: {err}"),
    })?;

    let scheme = url.scheme().to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return Err(RuntimeError::InvalidNetworkAllowEntry {
            entry: entry.to_string(),
            message: format!("unsupported URL scheme '{scheme}' (expected http or https)"),
        });
    }

    if url.query().is_some() || url.fragment().is_some() || (url.path() != "" && url.path() != "/")
    {
        return Err(RuntimeError::InvalidNetworkAllowEntry {
            entry: entry.to_string(),
            message: "URL allow entries must not include path, query, or fragment".to_string(),
        });
    }

    let host = url
        .host_str()
        .ok_or_else(|| RuntimeError::InvalidNetworkAllowEntry {
            entry: entry.to_string(),
            message: "URL allow entries must include a host".to_string(),
        })?
        .to_ascii_lowercase();

    let port = url.port().or_else(|| default_port_for_scheme(&scheme));
    Ok(NetworkAllowRule {
        scheme: Some(scheme),
        host,
        port,
    })
}

fn parse_host_allow_rule(entry: &str) -> Result<NetworkAllowRule, RuntimeError> {
    let prefixed = format!("http://{entry}");
    let parsed =
        url::Url::parse(&prefixed).map_err(|err| RuntimeError::InvalidNetworkAllowEntry {
            entry: entry.to_string(),
            message: format!("invalid host allow entry: {err}"),
        })?;

    if parsed.path() != "/" || parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(RuntimeError::InvalidNetworkAllowEntry {
            entry: entry.to_string(),
            message: "host allow entries must be host or host:port only".to_string(),
        });
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| RuntimeError::InvalidNetworkAllowEntry {
            entry: entry.to_string(),
            message: "host allow entries must include a host".to_string(),
        })?
        .to_ascii_lowercase();

    Ok(NetworkAllowRule {
        scheme: None,
        host,
        port: parsed.port(),
    })
}

fn default_port_for_scheme(scheme: &str) -> Option<u16> {
    match scheme {
        "http" => Some(80),
        "https" => Some(443),
        _ => None,
    }
}

pub(crate) fn validate_filesystem_scope(scope: &str) -> Result<(), RuntimeError> {
    let path = Path::new(scope);

    if path.is_absolute() {
        return Err(RuntimeError::InvalidFilesystemScope {
            scope: scope.to_string(),
            message: "scope must be relative to the workdir".to_string(),
        });
    }

    let mut depth = 0usize;
    for component in path.components() {
        match component {
            PathComponent::CurDir => {}
            PathComponent::Normal(_) => depth += 1,
            PathComponent::ParentDir => {
                if depth == 0 {
                    return Err(RuntimeError::InvalidFilesystemScope {
                        scope: scope.to_string(),
                        message: "scope cannot escape the workdir via '..'".to_string(),
                    });
                }
                depth -= 1;
            }
            PathComponent::Prefix(_) | PathComponent::RootDir => {
                return Err(RuntimeError::InvalidFilesystemScope {
                    scope: scope.to_string(),
                    message: "scope must not contain absolute or prefixed components".to_string(),
                });
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filesystem_scope_validation_accepts_safe_relative_paths() {
        for scope in [".", "./workdir", "a/../b", "sandbox/subdir"] {
            assert!(
                validate_filesystem_scope(scope).is_ok(),
                "scope should be valid: {scope}"
            );
        }
    }

    #[test]
    fn filesystem_scope_validation_rejects_escape_paths() {
        for scope in ["../x", "../../x", "/tmp"] {
            assert!(
                validate_filesystem_scope(scope).is_err(),
                "scope should be invalid: {scope}"
            );
        }
    }

    #[test]
    fn network_allowlist_empty_denies_all_requests() {
        let rules = parse_network_allow_rules(&Vec::<String>::new()).unwrap();

        let target = RequestTarget {
            scheme: "https".to_string(),
            host: "blocked.example.com".to_string(),
            port: Some(443),
        };

        assert!(!rules.iter().any(|rule| rule.matches(&target)));
    }

    #[test]
    fn network_allowlist_matches_allowed_host_with_default_port() {
        let rules =
            parse_network_allow_rules(&["https://allowed.example.com".to_string()]).unwrap();
        let target = RequestTarget {
            scheme: "https".to_string(),
            host: "allowed.example.com".to_string(),
            port: Some(443),
        };

        assert!(rules.iter().any(|rule| rule.matches(&target)));
    }

    #[test]
    fn network_allowlist_blocks_non_matching_host() {
        let rules =
            parse_network_allow_rules(&["https://allowed.example.com".to_string()]).unwrap();
        let target = RequestTarget {
            scheme: "https".to_string(),
            host: "blocked.example.com".to_string(),
            port: Some(443),
        };

        assert!(!rules.iter().any(|rule| rule.matches(&target)));
    }

    #[test]
    fn network_allowlist_normalizes_default_ports() {
        let rules = parse_network_allow_rules(&[
            "https://allowed.example.com".to_string(),
            "allowed-two.example.com:8080".to_string(),
        ])
        .unwrap();

        let https_default = RequestTarget {
            scheme: "https".to_string(),
            host: "allowed.example.com".to_string(),
            port: Some(443),
        };
        let custom_port = RequestTarget {
            scheme: "http".to_string(),
            host: "allowed-two.example.com".to_string(),
            port: Some(8080),
        };

        assert!(rules.iter().any(|rule| rule.matches(&https_default)));
        assert!(rules.iter().any(|rule| rule.matches(&custom_port)));
    }
}
