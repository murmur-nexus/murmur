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

    /// Whether `self` — a capsule-wide ceiling rule — already permits everything `narrower`
    /// would permit. Used to clamp a per-artifact allow-list to the ceiling: a rule no
    /// ceiling rule covers is dropped, never granted.
    ///
    /// An unset `scheme`/`port` on the ceiling side is a wildcard, so `example.com` covers
    /// `https://example.com`. The reverse is not true: a bare `example.com` on the artifact
    /// side spans both schemes and every port, so a ceiling of `https://example.com` does
    /// not cover it and the entry is dropped. That asymmetry is deliberate — clamping errs
    /// toward denial rather than silently keeping a broader rule than the ceiling states.
    pub(crate) fn covers(&self, narrower: &NetworkAllowRule) -> bool {
        if self.host != narrower.host {
            return false;
        }

        if let Some(expected_scheme) = &self.scheme {
            if narrower.scheme.as_ref() != Some(expected_scheme) {
                return false;
            }
        }

        match self.port {
            Some(expected_port) => narrower.port == Some(expected_port),
            None => true,
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

/// Resolve a validated filesystem `scope` to the directory a guest's preopen should target,
/// creating it if it does not already exist.
///
/// Shared by `hooks.rs::build_wasi_ctx` and `runtime.rs::build_wasi_ctx` — both preopen
/// `root.join(scope)` as `"."` once a grant declares a scope, and both must fail the same way
/// (hard error naming the scope) rather than silently falling back to an unscoped preopen,
/// which would widen the grant.
pub(crate) fn resolve_scoped_dir(
    root: &Path,
    scope: &str,
) -> Result<std::path::PathBuf, RuntimeError> {
    let scoped_dir = root.join(scope);
    std::fs::create_dir_all(&scoped_dir).map_err(|err| {
        RuntimeError::wasi(
            scoped_dir.clone(),
            format!("failed to create granted filesystem scope '{scope}': {err}"),
        )
    })?;
    Ok(scoped_dir)
}

/// A single hook's capability grant, lowered from the **capsule operator's own** manifest
/// entry for that hook (`murmur_artifact::RuntimeArtifact::capabilities`) at staging time.
///
/// [`Default`] is the default-deny state and is what a hook entry with no `capabilities:`
/// block lowers to: an empty allow-rule list (every outbound request is denied by
/// `NetworkPolicyHooks`, and no raw WASI socket capability is granted at all) and no
/// filesystem scope (no preopened directory of any kind).
///
/// Deliberately narrower than [`crate::types::CapabilityPolicy`]: the capsule-wide policy
/// carries shell/spawn/env/limits, none of which a per-hook grant governs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct HookCapabilityGrant {
    /// Hosts this hook may reach through the gated wasi-http path. Empty = deny all.
    pub(crate) network_allow_rules: Vec<NetworkAllowRule>,
    /// Relative path under the hook's working directory to preopen. `None` = preopen
    /// nothing, so the hook has no `wasi:filesystem` access whatsoever.
    pub(crate) filesystem_scope: Option<String>,
    /// Whether this hook may read the in-scope task's input and result text through the
    /// `murmur:task-io/read` host import. `false` = the import still links and every one of
    /// its functions returns `not-granted`.
    pub(crate) task_io_read: bool,
}

impl HookCapabilityGrant {
    /// Lower an operator-declared `capabilities:` block into an enforceable grant,
    /// validating both halves up front so a malformed grant fails staging rather than
    /// surfacing as a confusing denial once the hook is already running.
    ///
    /// `None` (no block declared) yields [`HookCapabilityGrant::default`] — full
    /// default-deny. Only `network`, `filesystem` and `task_io` are read; the other
    /// sub-blocks a [`murmur_artifact::Capabilities`] can carry govern capsule-wide concerns
    /// that a per-hook grant does not reach.
    pub(crate) fn derive(
        capabilities: Option<&murmur_artifact::Capabilities>,
    ) -> Result<Self, RuntimeError> {
        let Some(capabilities) = capabilities else {
            return Ok(Self::default());
        };

        let network_allow_rules = match capabilities.network.as_ref() {
            Some(network) => parse_network_allow_rules(&network.allow)?,
            None => Vec::new(),
        };

        let filesystem_scope = capabilities
            .filesystem
            .as_ref()
            .and_then(|filesystem| filesystem.scope.clone());
        if let Some(scope) = filesystem_scope.as_deref() {
            validate_filesystem_scope(scope)?;
        }

        Ok(Self {
            network_allow_rules,
            filesystem_scope,
            task_io_read: capabilities
                .task_io
                .as_ref()
                .is_some_and(|task_io| task_io.read),
        })
    }
}

/// One tool's or driver's capability grant, lowered from the **capsule operator's own**
/// manifest entry for that artifact (`murmur_artifact::RuntimeArtifact::capabilities`) at
/// staging time and applied on the shared `invoke_tool_component` dispatch path.
///
/// The baseline is the opposite of [`HookCapabilityGrant`]'s, which is why they are two types: a
/// hook derives its grant *from nothing* (absent block = deny), whereas a tool or driver derives
/// it *from the capsule ceiling* (absent block = the capsule-wide policy, unchanged). Both fields
/// are therefore `Option`-of-narrowing rather than plain values, and [`Default`] — what an entry
/// with no `capabilities:` block yields — means "inherit everything".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ToolCapabilityGrant {
    /// Effective allow-rules for this artifact, already clamped to the ceiling. `None` =
    /// no `capabilities.network` block, so the capsule-wide rules apply untouched.
    /// `Some(vec![])` is a real narrowing to zero network access, and is what both an
    /// explicit `allow: []` and an allow-list wholly outside the ceiling lower to.
    pub(crate) network_allow_rules: Option<Vec<NetworkAllowRule>>,
    /// Relative path under `accessible_workdir` to preopen as `"."` instead of the whole
    /// workdir. `None` = no `capabilities.filesystem.scope`, so the artifact keeps seeing
    /// the entire `accessible_workdir`, as every tool does today.
    pub(crate) filesystem_scope: Option<String>,
    /// Declared `network.allow` entries dropped because no ceiling rule covers them. Held
    /// (rather than warned about in `derive`) so lowering stays pure and unit-testable; the
    /// staging path turns a non-empty list into a `W-SEC-007` warning.
    pub(crate) dropped_network_entries: Vec<String>,
}

impl ToolCapabilityGrant {
    /// Lower an operator-declared `capabilities:` block into a grant that can only ever be
    /// narrower than `ceiling_network_allow_rules` (the capsule-wide allow-list, already
    /// parsed). Validating here rather than at dispatch means a malformed entry fails
    /// staging instead of surfacing as a confusing denial mid-run.
    ///
    /// `None` (no block declared) yields [`ToolCapabilityGrant::default`] — inherit the
    /// ceiling wholesale. Only `network` and `filesystem` are read; the other sub-blocks a
    /// [`murmur_artifact::Capabilities`] can carry govern capsule-wide concerns that
    /// per-artifact narrowing does not reach (the caller warns `W-SEC-008` for those).
    pub(crate) fn derive(
        capabilities: Option<&murmur_artifact::Capabilities>,
        ceiling_network_allow_rules: &[NetworkAllowRule],
    ) -> Result<Self, RuntimeError> {
        let Some(capabilities) = capabilities else {
            return Ok(Self::default());
        };

        let mut dropped_network_entries = Vec::new();
        let network_allow_rules = match capabilities.network.as_ref() {
            None => None,
            Some(network) => {
                let mut kept = Vec::new();
                for entry in &network.allow {
                    let rule = NetworkAllowRule::parse(entry)?;
                    if ceiling_network_allow_rules
                        .iter()
                        .any(|ceiling_rule| ceiling_rule.covers(&rule))
                    {
                        kept.push(rule);
                    } else {
                        dropped_network_entries.push(entry.clone());
                    }
                }
                Some(kept)
            }
        };

        let filesystem_scope = capabilities
            .filesystem
            .as_ref()
            .and_then(|filesystem| filesystem.scope.clone());
        if let Some(scope) = filesystem_scope.as_deref() {
            validate_filesystem_scope(scope)?;
        }

        Ok(Self {
            network_allow_rules,
            filesystem_scope,
            dropped_network_entries,
        })
    }
}

/// The rules `NetworkPolicyHooks` enforces for one tool/driver dispatch: the artifact's own
/// clamped set when it declared a `capabilities.network` block, otherwise the capsule ceiling
/// borrowed unchanged.
///
/// Takes the grant as an `Option` because that is how the dispatch path holds it — a name
/// missing from the session's grant map (no per-artifact block, or an artifact pulled in at
/// runtime) must land on the ceiling, not on an empty allow-list.
pub(crate) fn effective_tool_network_rules<'a>(
    grant: Option<&'a ToolCapabilityGrant>,
    ceiling_network_allow_rules: &'a [NetworkAllowRule],
) -> &'a [NetworkAllowRule] {
    grant
        .and_then(|grant| grant.network_allow_rules.as_deref())
        .unwrap_or(ceiling_network_allow_rules)
}

#[cfg(test)]
mod tests {
    use super::*;
    use murmur_artifact::{Capabilities, FilesystemCapabilities, NetworkCapabilities};

    /// A `Capabilities` block carrying only the two sub-blocks a per-artifact grant reads.
    fn capabilities_block(
        network: Option<Vec<&str>>,
        filesystem_scope: Option<&str>,
    ) -> Capabilities {
        Capabilities {
            network: network.map(|allow| NetworkCapabilities {
                allow: allow.into_iter().map(str::to_string).collect(),
                unix_sockets: false,
            }),
            filesystem: filesystem_scope.map(|scope| FilesystemCapabilities {
                scope: Some(scope.to_string()),
                workdir_exec: false,
            }),
            shell: None,
            spawn: None,
            env: None,
            limits: None,
            resources: None,
            task_io: None,
            containment: None,
        }
    }

    /// A hook entry with no `capabilities:` block gets nothing: no allow rules (so
    /// `NetworkPolicyHooks` denies every request) and no scope (so nothing is preopened).
    #[test]
    fn hook_grant_defaults_to_deny_network_and_filesystem() {
        let grant = HookCapabilityGrant::derive(None).unwrap();

        assert_eq!(grant, HookCapabilityGrant::default());
        assert!(grant.network_allow_rules.is_empty());
        assert!(grant.filesystem_scope.is_none());

        let target = RequestTarget {
            scheme: "https".to_string(),
            host: "telemetry.example.com".to_string(),
            port: Some(443),
        };
        assert!(!grant
            .network_allow_rules
            .iter()
            .any(|rule| rule.matches(&target)));
    }

    /// A declared-but-empty `capabilities:` block is still full default-deny — declaring
    /// the key must not, by itself, widen anything.
    #[test]
    fn hook_grant_empty_capabilities_block_grants_nothing() {
        let grant = HookCapabilityGrant::derive(Some(&capabilities_block(None, None))).unwrap();

        assert_eq!(grant, HookCapabilityGrant::default());
    }

    #[test]
    fn hook_grant_network_allows_exactly_the_declared_host() {
        let caps = capabilities_block(Some(vec!["https://telemetry.example.com"]), None);
        let grant = HookCapabilityGrant::derive(Some(&caps)).unwrap();

        let allowed = RequestTarget {
            scheme: "https".to_string(),
            host: "telemetry.example.com".to_string(),
            port: Some(443),
        };
        let other = RequestTarget {
            scheme: "https".to_string(),
            host: "evil.example.com".to_string(),
            port: Some(443),
        };

        assert!(grant
            .network_allow_rules
            .iter()
            .any(|rule| rule.matches(&allowed)));
        assert!(!grant
            .network_allow_rules
            .iter()
            .any(|rule| rule.matches(&other)));
        // A network grant alone never widens the filesystem.
        assert!(grant.filesystem_scope.is_none());
    }

    #[test]
    fn hook_grant_filesystem_scope_is_carried_through() {
        let caps = capabilities_block(None, Some("hook-state"));
        let grant = HookCapabilityGrant::derive(Some(&caps)).unwrap();

        assert_eq!(grant.filesystem_scope.as_deref(), Some("hook-state"));
        // A filesystem grant alone never widens the network.
        assert!(grant.network_allow_rules.is_empty());
    }

    /// `capabilities.task_io.read` lowers into the grant on its own axis: `true` grants it,
    /// `false` and an absent block do not, and neither ever widens network or filesystem.
    #[test]
    fn hook_grant_task_io_read_is_carried_through_independently() {
        for (declared, expected) in [(None, false), (Some(false), false), (Some(true), true)] {
            let caps = murmur_artifact::Capabilities {
                task_io: declared.map(|read| murmur_artifact::TaskIoCapabilities { read }),
                ..capabilities_block(None, None)
            };
            let grant = HookCapabilityGrant::derive(Some(&caps)).unwrap();
            assert_eq!(grant.task_io_read, expected, "declared: {declared:?}");
            assert!(grant.network_allow_rules.is_empty());
            assert!(grant.filesystem_scope.is_none());
        }
    }

    #[test]
    fn hook_grant_rejects_escaping_filesystem_scope() {
        for scope in ["../escape", "/etc"] {
            let caps = capabilities_block(None, Some(scope));
            let err = HookCapabilityGrant::derive(Some(&caps)).unwrap_err();
            assert!(
                matches!(err, RuntimeError::InvalidFilesystemScope { .. }),
                "scope {scope} should fail staging, got: {err}"
            );
        }
    }

    #[test]
    fn hook_grant_rejects_malformed_network_entry() {
        let caps = capabilities_block(Some(vec!["ftp://files.example.com"]), None);
        let err = HookCapabilityGrant::derive(Some(&caps)).unwrap_err();

        assert!(
            matches!(err, RuntimeError::InvalidNetworkAllowEntry { .. }),
            "got: {err}"
        );
    }

    /// The capsule ceiling used by the tool/driver narrowing tests: two hosts, one of which
    /// a narrowed artifact is expected to lose access to.
    fn ceiling() -> Vec<NetworkAllowRule> {
        parse_network_allow_rules(&[
            "https://api.example.com".to_string(),
            "https://other.example.com".to_string(),
        ])
        .unwrap()
    }

    fn target(host: &str) -> RequestTarget {
        RequestTarget {
            scheme: "https".to_string(),
            host: host.to_string(),
            port: Some(443),
        }
    }

    fn reaches(rules: &[NetworkAllowRule], host: &str) -> bool {
        rules.iter().any(|rule| rule.matches(&target(host)))
    }

    /// The no-op invariant: a tool/driver entry with no `capabilities:` block keeps the whole
    /// ceiling and the whole workdir, unchanged.
    #[test]
    fn tool_grant_without_entry_inherits_the_ceiling_unchanged() {
        let ceiling = ceiling();
        let grant = ToolCapabilityGrant::derive(None, &ceiling).unwrap();

        assert_eq!(grant, ToolCapabilityGrant::default());
        assert!(grant.network_allow_rules.is_none());
        assert!(grant.filesystem_scope.is_none());
        assert!(grant.dropped_network_entries.is_empty());

        let effective = effective_tool_network_rules(Some(&grant), &ceiling);
        assert_eq!(effective, ceiling.as_slice());
        assert!(reaches(effective, "api.example.com"));
        assert!(reaches(effective, "other.example.com"));
    }

    /// A declared subset keeps exactly the declared host and loses the rest of the ceiling.
    #[test]
    fn tool_grant_subset_of_ceiling_is_kept_and_narrows() {
        let ceiling = ceiling();
        let caps = capabilities_block(Some(vec!["https://api.example.com"]), None);
        let grant = ToolCapabilityGrant::derive(Some(&caps), &ceiling).unwrap();

        let effective = effective_tool_network_rules(Some(&grant), &ceiling);
        assert!(reaches(effective, "api.example.com"));
        assert!(!reaches(effective, "other.example.com"));
        assert!(grant.dropped_network_entries.is_empty());
    }

    /// Narrowing can only subtract: a host the ceiling never allowed is dropped (and
    /// reported for `W-SEC-007`), not granted.
    #[test]
    fn tool_grant_entry_outside_the_ceiling_is_dropped_not_granted() {
        let ceiling = ceiling();
        let caps = capabilities_block(
            Some(vec!["https://api.example.com", "https://evil.example.com"]),
            None,
        );
        let grant = ToolCapabilityGrant::derive(Some(&caps), &ceiling).unwrap();

        let effective = effective_tool_network_rules(Some(&grant), &ceiling);
        assert!(reaches(effective, "api.example.com"));
        assert!(!reaches(effective, "evil.example.com"));
        assert_eq!(
            grant.dropped_network_entries,
            vec!["https://evil.example.com".to_string()]
        );
    }

    /// A bare host is broader than a scheme-bound ceiling rule (it spans http too), so it is
    /// dropped rather than silently widening the artifact past the ceiling.
    #[test]
    fn tool_grant_entry_broader_than_the_ceiling_rule_is_dropped() {
        let ceiling = ceiling();
        let caps = capabilities_block(Some(vec!["api.example.com"]), None);
        let grant = ToolCapabilityGrant::derive(Some(&caps), &ceiling).unwrap();

        assert_eq!(grant.network_allow_rules.as_deref(), Some(&[][..]));
        assert_eq!(
            grant.dropped_network_entries,
            vec!["api.example.com".to_string()]
        );
    }

    /// An explicit empty allow-list is a real narrowing to zero network access — distinct
    /// from the key being absent, which inherits the ceiling.
    #[test]
    fn tool_grant_empty_network_allow_denies_all_for_that_artifact() {
        let ceiling = ceiling();
        let caps = capabilities_block(Some(vec![]), None);
        let grant = ToolCapabilityGrant::derive(Some(&caps), &ceiling).unwrap();

        let effective = effective_tool_network_rules(Some(&grant), &ceiling);
        assert!(effective.is_empty());
        assert!(!reaches(effective, "api.example.com"));
        assert!(!reaches(effective, "other.example.com"));
    }

    /// A filesystem-only block leaves the network on the ceiling: narrowing one axis must
    /// not implicitly clamp the other.
    #[test]
    fn tool_grant_filesystem_scope_is_carried_and_leaves_network_inherited() {
        let ceiling = ceiling();
        let caps = capabilities_block(None, Some("cache"));
        let grant = ToolCapabilityGrant::derive(Some(&caps), &ceiling).unwrap();

        assert_eq!(grant.filesystem_scope.as_deref(), Some("cache"));
        assert!(grant.network_allow_rules.is_none());
        assert_eq!(
            effective_tool_network_rules(Some(&grant), &ceiling),
            ceiling.as_slice()
        );
    }

    #[test]
    fn tool_grant_rejects_escaping_filesystem_scope() {
        for scope in ["../escape", "/etc"] {
            let caps = capabilities_block(None, Some(scope));
            let err = ToolCapabilityGrant::derive(Some(&caps), &ceiling()).unwrap_err();
            assert!(
                matches!(err, RuntimeError::InvalidFilesystemScope { .. }),
                "scope {scope} should fail staging, got: {err}"
            );
        }
    }

    #[test]
    fn tool_grant_rejects_malformed_network_entry() {
        let caps = capabilities_block(Some(vec!["ftp://files.example.com"]), None);
        let err = ToolCapabilityGrant::derive(Some(&caps), &ceiling()).unwrap_err();

        assert!(
            matches!(err, RuntimeError::InvalidNetworkAllowEntry { .. }),
            "got: {err}"
        );
    }

    /// An empty ceiling already denies everything, so every declared entry drops — a
    /// per-artifact block can never be the thing that turns network access on.
    #[test]
    fn tool_grant_cannot_widen_an_empty_ceiling() {
        let caps = capabilities_block(Some(vec!["https://api.example.com"]), None);
        let grant = ToolCapabilityGrant::derive(Some(&caps), &[]).unwrap();

        assert_eq!(grant.network_allow_rules.as_deref(), Some(&[][..]));
        assert_eq!(
            grant.dropped_network_entries,
            vec!["https://api.example.com".to_string()]
        );
    }

    /// An unset scheme/port on the ceiling side is a wildcard the narrower rule fits under.
    #[test]
    fn ceiling_rule_with_wildcard_scheme_covers_a_scheme_bound_rule() {
        let ceiling = parse_network_allow_rules(&["api.example.com".to_string()]).unwrap();
        let caps = capabilities_block(Some(vec!["https://api.example.com"]), None);
        let grant = ToolCapabilityGrant::derive(Some(&caps), &ceiling).unwrap();

        assert!(grant.dropped_network_entries.is_empty());
        assert!(reaches(
            effective_tool_network_rules(Some(&grant), &ceiling),
            "api.example.com"
        ));
    }

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
