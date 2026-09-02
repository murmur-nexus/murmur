use std::path::{Component as PathComponent, Path, PathBuf};

use crate::{
    containment::{PreopenReport, PreopenSurface},
    errors::RuntimeError,
};

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

/// Which of the three shapes a workdir subtree cannot have a declared path broke.
///
/// Carried instead of a built error so one rule can serve declarations that report under
/// different diagnostic codes; [`Self::message`] supplies the noun each of them names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkdirSubpathRejection {
    Absolute,
    Escapes,
    Prefixed,
}

impl WorkdirSubpathRejection {
    /// The operator-facing half of the message, with `noun` naming the declaration that was
    /// rejected (`"scope"`, `"read-only path"`).
    pub(crate) fn message(self, noun: &str) -> String {
        match self {
            Self::Absolute => format!("{noun} must be relative to the workdir"),
            Self::Escapes => format!("{noun} cannot escape the workdir via '..'"),
            Self::Prefixed => format!("{noun} must not contain absolute or prefixed components"),
        }
    }
}

/// Lower one manifest-declared workdir-relative path to its normalized form: `.` components
/// dropped and `..` applied against what is already accumulated.
///
/// The single definition of what a workdir subtree may be, shared by
/// `capabilities.filesystem.scope` and `capabilities.filesystem.read_only` so the two cannot drift
/// into accepting different shapes. Purely lexical — no filesystem access — so it is safe on a
/// path that does not exist yet.
pub(crate) fn lower_workdir_subpath(raw: &str) -> Result<PathBuf, WorkdirSubpathRejection> {
    let path = Path::new(raw);
    if path.is_absolute() {
        return Err(WorkdirSubpathRejection::Absolute);
    }

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            PathComponent::CurDir => {}
            PathComponent::Normal(part) => normalized.push(part),
            // A `..` that would pop past the workdir root escapes it, whatever follows.
            PathComponent::ParentDir => {
                if !normalized.pop() {
                    return Err(WorkdirSubpathRejection::Escapes);
                }
            }
            PathComponent::Prefix(_) | PathComponent::RootDir => {
                return Err(WorkdirSubpathRejection::Prefixed);
            }
        }
    }

    Ok(normalized)
}

pub(crate) fn validate_filesystem_scope(scope: &str) -> Result<(), RuntimeError> {
    lower_workdir_subpath(scope)
        .map(|_| ())
        .map_err(|rejection| RuntimeError::InvalidFilesystemScope {
            scope: scope.to_string(),
            message: rejection.message("scope"),
        })
}

/// Resolve a validated filesystem `scope` to the directory a guest's preopen should target,
/// creating it if it does not already exist.
///
/// Shared by `hooks.rs::build_wasi_ctx` and `runtime.rs::build_wasi_ctx` — both preopen
/// `root.join(scope)` as `"."` once a grant declares a scope, and both must fail the same way
/// (hard error naming the scope) rather than silently falling back to an unscoped preopen,
/// which would widen the grant.
pub(crate) fn resolve_scoped_dir(root: &Path, scope: &str) -> Result<PathBuf, RuntimeError> {
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
    /// Whether this hook may read the capsule's durable conversation record through the
    /// `murmur:conversation/read` host import. `false` = the import still links and
    /// `read-messages` returns `not-granted`.
    pub(crate) conversation_read: bool,
    /// Resolved, validated `capabilities.state.store` name, with the capsule-name default already
    /// applied. `None` = no `capabilities.state` block, so no durable store of any kind.
    pub(crate) state_store: Option<String>,
    /// Host path of the created store directory, filled by the staging path once
    /// [`crate::state_store::ensure_state_store`] has made it. Left `None` by [`Self::derive`],
    /// which stays pure — the same division [`ToolCapabilityGrant::dropped_network_entries`]
    /// already uses, where lowering is unit-testable and only staging touches the filesystem.
    pub(crate) state_dir: Option<PathBuf>,
    /// This hook's operator-declared `config:` block, already lowered to compact JSON by
    /// [`crate::artifact_config::lower_artifact_config`]. `Some` adds exactly one variable,
    /// [`crate::artifact_config::ARTIFACT_CONFIG_ENV`], to the hook's environment; `None` — the
    /// hook entry declaring no `config:` — adds nothing at all.
    ///
    /// Filled by the staging path rather than by [`Self::derive`], because config is declared
    /// beside `capabilities:` and not inside it: the two are independent, and a hook may carry
    /// either without the other. Carried on the grant because dispatch already looks a grant up by
    /// artifact name, which is what scopes the value to the declaring hook alone.
    pub(crate) config_json: Option<String>,
}

impl HookCapabilityGrant {
    /// Lower an operator-declared `capabilities:` block into an enforceable grant,
    /// validating both halves up front so a malformed grant fails staging rather than
    /// surfacing as a confusing denial once the hook is already running.
    ///
    /// `None` (no block declared) yields [`HookCapabilityGrant::default`] — full
    /// default-deny. Only `network`, `filesystem`, `state` and `task_io` are read; the other
    /// sub-blocks a [`murmur_artifact::Capabilities`] can carry govern capsule-wide concerns
    /// that a per-hook grant does not reach.
    ///
    /// `capsule_name` is what an undeclared `capabilities.state.store` defaults to. It is the
    /// operator's own capsule name, on the same sourcing rule as the block itself.
    pub(crate) fn derive(
        capabilities: Option<&murmur_artifact::Capabilities>,
        capsule_name: &str,
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
            conversation_read: capabilities
                .conversation
                .as_ref()
                .is_some_and(|conversation| conversation.read),
            state_store: derive_state_store(capabilities, capsule_name)?,
            state_dir: None,
            config_json: None,
        })
    }
}

/// Resolve and validate the store name a `capabilities:` block asks for, applying the capsule-name
/// default. `None` when the block declares no `state`, which is deny.
///
/// Shared by both grants because the rule is one rule: the name comes from the capsule operator's
/// own manifest entry, the default is the operator's capsule name, and a name that is not a single
/// usable path segment fails staging rather than surfacing later as a confusing denial.
fn derive_state_store(
    capabilities: &murmur_artifact::Capabilities,
    capsule_name: &str,
) -> Result<Option<String>, RuntimeError> {
    let Some(state) = capabilities.state.as_ref() else {
        return Ok(None);
    };
    let store = crate::state_store::resolve_store_name(state, capsule_name);
    crate::state_store::validate_store_name(&store)?;
    Ok(Some(store))
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
///
/// # The wide filesystem default
///
/// [`Default`]'s `filesystem_scope: None` preopens the entire accessible workdir, read-write, for
/// every tool and driver whose entry declares no `capabilities.filesystem.scope`. That default is
/// chosen against **prompt injection steering an honest artifact**: the artifact's code does what
/// its publisher wrote, and the hazard is a model that has read attacker-controlled text calling
/// it with attacker-chosen arguments. Against that, the containment class, the network allow-list,
/// the untrusted fence and `exports` are the mechanisms, and a per-artifact `scope` is the
/// operator's tool for the entries where a narrower working surface is known.
///
/// It is **not** chosen against a hostile artifact. Malicious artifact code is outside murmur's
/// documented threat model: no artifact trust model and no signing exist, so nothing here
/// establishes that an artifact's code is what its publisher intended. Hash-pinning through
/// `murmur.lock` establishes that an artifact has not changed since it was locked, and never that
/// it was safe when it was locked.
///
/// The alternative — deny by default, narrow by declaration — is rejected because nothing in the
/// system tells an operator what scope to write. A grant is read only from the operator's own
/// manifest and never from the artifact's bundled one (the anti-self-escalation property every
/// `derive` here rests on), so there is no declaration of need to migrate from. Under a narrow
/// default the discoverable path to a working capsule is trial and error, and its terminal state
/// is `scope: .` on every entry: this default restated, plus a migration that broke every existing
/// capsule.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ToolCapabilityGrant {
    /// Effective allow-rules for this artifact, already clamped to the ceiling. `None` =
    /// no `capabilities.network` block, so the capsule-wide rules apply untouched.
    /// `Some(vec![])` is a real narrowing to zero network access, and is what both an
    /// explicit `allow: []` and an allow-list wholly outside the ceiling lower to.
    pub(crate) network_allow_rules: Option<Vec<NetworkAllowRule>>,
    /// Relative path under `accessible_workdir` to preopen as `"."` instead of the whole
    /// workdir. `None` = no `capabilities.filesystem.scope`, so the artifact sees the entire
    /// `accessible_workdir` — the wide default this type documents above.
    pub(crate) filesystem_scope: Option<String>,
    /// Declared `network.allow` entries dropped because no ceiling rule covers them. Held
    /// (rather than warned about in `derive`) so lowering stays pure and unit-testable; the
    /// staging path turns a non-empty list into a `W-SEC-007` warning.
    pub(crate) dropped_network_entries: Vec<String>,
    /// Resolved, validated `capabilities.state.store` name, with the capsule-name default already
    /// applied. `None` = no `capabilities.state` block, so no durable store of any kind.
    ///
    /// The one axis on which a per-artifact block *widens* rather than narrows, and it can only
    /// ever open one directory the capsule ceiling does not otherwise reach — never a workdir
    /// path, and never another capsule's store.
    pub(crate) state_store: Option<String>,
    /// Host path of the created store directory, filled by the staging path once
    /// [`crate::state_store::ensure_state_store`] has made it. Left `None` by [`Self::derive`],
    /// which stays pure, on the same terms as `dropped_network_entries` above.
    pub(crate) state_dir: Option<PathBuf>,
    /// This artifact's operator-declared `config:` block, already lowered to compact JSON by
    /// [`crate::artifact_config::lower_artifact_config`]. `Some` adds exactly one variable,
    /// [`crate::artifact_config::ARTIFACT_CONFIG_ENV`], to this artifact's guest environment;
    /// `None` adds nothing.
    ///
    /// The one field here that neither narrows nor widens: it grants no reach the ceiling did not
    /// already allow, which is why an entry declaring `config:` and nothing else stages a grant
    /// equal to [`Default`] in every other field. Filled by the staging path rather than by
    /// [`Self::derive`], because `config:` is declared beside `capabilities:` and not inside it.
    pub(crate) config_json: Option<String>,
}

impl ToolCapabilityGrant {
    /// Lower an operator-declared `capabilities:` block into a grant that can only ever be
    /// narrower than `ceiling_network_allow_rules` (the capsule-wide allow-list, already
    /// parsed). Validating here rather than at dispatch means a malformed entry fails
    /// staging instead of surfacing as a confusing denial mid-run.
    ///
    /// `None` (no block declared) yields [`ToolCapabilityGrant::default`] — inherit the
    /// ceiling wholesale. Only `network`, `filesystem` and `state` are read; the other sub-blocks
    /// a [`murmur_artifact::Capabilities`] can carry govern capsule-wide concerns that a
    /// per-artifact grant does not reach (the caller warns `W-SEC-008` for those).
    ///
    /// `state` is the one member of that set that widens rather than narrows: it opens one
    /// directory outside every workdir and touches nothing the ceiling governs. `capsule_name` is
    /// what an undeclared `capabilities.state.store` defaults to.
    pub(crate) fn derive(
        capabilities: Option<&murmur_artifact::Capabilities>,
        ceiling_network_allow_rules: &[NetworkAllowRule],
        capsule_name: &str,
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
            state_store: derive_state_store(capabilities, capsule_name)?,
            state_dir: None,
            config_json: None,
        })
    }
}

/// The filesystem surface each of the given artifacts will be preopened into, resolved but not
/// opened, one entry per artifact that has a guest.
///
/// Lives here, beside the two grant types whose [`Default`]s it reports: [`ToolCapabilityGrant`]
/// supplies the whole-workdir baseline a tool or driver falls back to, and
/// [`HookCapabilityGrant`] the deny baseline a hook falls back to. Resolving the same question in
/// a second place is how a report starts disagreeing with a launch.
///
/// Takes `(artifact name, role, that artifact's operator-declared capabilities)` triples rather
/// than a concrete artifact type so the two callers that must agree can both satisfy it:
/// `stage_session` holds [`crate::types::ArtifactRequest`]s, and `mur run --explain-scope` and
/// `mur doctor` hold [`murmur_artifact::RuntimeArtifact`]s and have staged nothing.
///
/// [`murmur_artifact::ArtifactRuntime::Skill`] produces no entry: a skill is markdown staged into
/// the workdir, no component is instantiated for it, and it holds no descriptor to report.
///
/// Every declared scope goes through [`validate_filesystem_scope`], so a scope a launch would
/// refuse is refused here too. Nothing is created — [`resolve_scoped_dir`] is the staging path's
/// job — so this stays usable from a read-only diagnostic.
pub fn preopen_reports<'a, I>(artifacts: I) -> Result<Vec<PreopenReport>, RuntimeError>
where
    I: IntoIterator<
        Item = (
            &'a str,
            &'a murmur_artifact::ArtifactRuntime,
            Option<&'a murmur_artifact::Capabilities>,
        ),
    >,
{
    let mut reports = Vec::new();

    for (name, runtime, capabilities) in artifacts {
        if matches!(runtime, murmur_artifact::ArtifactRuntime::Skill) {
            continue;
        }

        let scope = capabilities
            .and_then(|capabilities| capabilities.filesystem.as_ref())
            .and_then(|filesystem| filesystem.scope.clone());
        if let Some(scope) = scope.as_deref() {
            validate_filesystem_scope(scope)?;
        }

        let surface = match (runtime, scope.is_some()) {
            // A declared scope resolves the same way for every role: `build_wasi_ctx` in both
            // `runtime.rs` and `hooks.rs` preopens `<workdir>/<scope>` as `"."`.
            (_, true) => PreopenSurface::ScopedSubtree,
            (murmur_artifact::ArtifactRuntime::Hook, false) => PreopenSurface::Nothing,
            (_, false) => PreopenSurface::WholeWorkdir,
        };

        reports.push(PreopenReport {
            artifact: name.to_string(),
            role: runtime.as_str().to_string(),
            scope,
            surface,
        });
    }

    Ok(reports)
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
            peer_fetch: None,
            network: network.map(|allow| NetworkCapabilities {
                allow: allow.into_iter().map(str::to_string).collect(),
                unix_sockets: false,
            }),
            filesystem: filesystem_scope.map(|scope| FilesystemCapabilities {
                scope: Some(scope.to_string()),
                workdir_exec: false,
                read_only: Vec::new(),
            }),
            shell: None,
            spawn: None,
            env: None,
            limits: None,
            resources: None,
            state: None,
            task_io: None,
            conversation: None,
            containment: None,
        }
    }

    /// A hook entry with no `capabilities:` block gets nothing: no allow rules (so
    /// `NetworkPolicyHooks` denies every request) and no scope (so nothing is preopened).
    #[test]
    fn hook_grant_defaults_to_deny_network_and_filesystem() {
        let grant = HookCapabilityGrant::derive(None, "test-capsule").unwrap();

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
        let grant =
            HookCapabilityGrant::derive(Some(&capabilities_block(None, None)), "test-capsule")
                .unwrap();

        assert_eq!(grant, HookCapabilityGrant::default());
    }

    #[test]
    fn hook_grant_network_allows_exactly_the_declared_host() {
        let caps = capabilities_block(Some(vec!["https://telemetry.example.com"]), None);
        let grant = HookCapabilityGrant::derive(Some(&caps), "test-capsule").unwrap();

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
        let grant = HookCapabilityGrant::derive(Some(&caps), "test-capsule").unwrap();

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
                conversation: None,
                ..capabilities_block(None, None)
            };
            let grant = HookCapabilityGrant::derive(Some(&caps), "test-capsule").unwrap();
            assert_eq!(grant.task_io_read, expected, "declared: {declared:?}");
            assert!(grant.network_allow_rules.is_empty());
            assert!(grant.filesystem_scope.is_none());
        }
    }

    /// `capabilities.conversation.read` lowers on its own axis, exactly as `task_io.read` does,
    /// and neither ever widens network or filesystem.
    #[test]
    fn hook_grant_conversation_read_is_carried_through_independently() {
        for (declared, expected) in [(None, false), (Some(false), false), (Some(true), true)] {
            let caps = murmur_artifact::Capabilities {
                conversation: declared
                    .map(|read| murmur_artifact::ConversationCapabilities { read }),
                ..capabilities_block(None, None)
            };
            let grant = HookCapabilityGrant::derive(Some(&caps), "test-capsule").unwrap();
            assert_eq!(grant.conversation_read, expected, "declared: {declared:?}");
            assert!(!grant.task_io_read, "one grant never implies the other");
            assert!(grant.network_allow_rules.is_empty());
            assert!(grant.filesystem_scope.is_none());
        }
    }

    /// The store name defaults to the capsule's, and an explicit `store:` wins. Applied where the
    /// grant is lowered, so a report and a preopen can never name different directories.
    #[test]
    fn state_store_defaults_to_the_capsule_name_and_an_explicit_name_wins() {
        for (declared, expected) in [(None, "state-capsule"), (Some("shey"), "shey")] {
            let caps = Capabilities {
                state: Some(murmur_artifact::StateCapabilities {
                    store: declared.map(str::to_string),
                }),
                ..capabilities_block(None, None)
            };

            let hook = HookCapabilityGrant::derive(Some(&caps), "state-capsule").unwrap();
            let tool =
                ToolCapabilityGrant::derive(Some(&caps), &ceiling(), "state-capsule").unwrap();

            assert_eq!(hook.state_store.as_deref(), Some(expected));
            assert_eq!(tool.state_store.as_deref(), Some(expected));
            // `derive` stays pure: the directory is made by the staging path, not here.
            assert!(hook.state_dir.is_none());
            assert!(tool.state_dir.is_none());
            // A state grant alone widens nothing else.
            assert!(hook.filesystem_scope.is_none());
            assert!(hook.network_allow_rules.is_empty());
            assert!(tool.filesystem_scope.is_none());
            assert!(tool.network_allow_rules.is_none());
        }
    }

    /// Absent `capabilities.state` is deny for both roles, including on a block that declares
    /// other things — declaring the key is what grants a store, never declaring the block.
    #[test]
    fn an_absent_state_block_grants_no_store_to_either_role() {
        let caps = capabilities_block(Some(vec!["https://api.example.com"]), Some("cache"));

        assert!(HookCapabilityGrant::derive(Some(&caps), "state-capsule")
            .unwrap()
            .state_store
            .is_none());
        assert!(
            ToolCapabilityGrant::derive(Some(&caps), &ceiling(), "state-capsule")
                .unwrap()
                .state_store
                .is_none()
        );
    }

    /// A malformed store name fails lowering, so it fails staging — never a confusing denial once
    /// a guest is already running. Refused identically for a hook and for a tool.
    #[test]
    fn a_malformed_store_name_fails_lowering_for_both_roles() {
        for store in ["../escape", "/abs/path", "a/b", "", ".hidden"] {
            let caps = Capabilities {
                state: Some(murmur_artifact::StateCapabilities {
                    store: Some(store.to_string()),
                }),
                ..capabilities_block(None, None)
            };

            for err in [
                HookCapabilityGrant::derive(Some(&caps), "state-capsule").unwrap_err(),
                ToolCapabilityGrant::derive(Some(&caps), &ceiling(), "state-capsule").unwrap_err(),
            ] {
                assert!(
                    matches!(err, RuntimeError::InvalidStateStore { .. }),
                    "store '{store}' must refuse as InvalidStateStore, got: {err}"
                );
            }
        }
    }

    /// An empty capsule name is not a usable store name either, so a capsule that somehow has one
    /// is refused rather than silently given `~/.murmur/state/`.
    #[test]
    fn an_empty_capsule_name_cannot_become_a_store_name() {
        let caps = Capabilities {
            state: Some(murmur_artifact::StateCapabilities { store: None }),
            ..capabilities_block(None, None)
        };

        assert!(matches!(
            ToolCapabilityGrant::derive(Some(&caps), &ceiling(), ""),
            Err(RuntimeError::InvalidStateStore { .. })
        ));
    }

    #[test]
    fn hook_grant_rejects_escaping_filesystem_scope() {
        for scope in ["../escape", "/etc"] {
            let caps = capabilities_block(None, Some(scope));
            let err = HookCapabilityGrant::derive(Some(&caps), "test-capsule").unwrap_err();
            assert!(
                matches!(err, RuntimeError::InvalidFilesystemScope { .. }),
                "scope {scope} should fail staging, got: {err}"
            );
        }
    }

    #[test]
    fn hook_grant_rejects_malformed_network_entry() {
        let caps = capabilities_block(Some(vec!["ftp://files.example.com"]), None);
        let err = HookCapabilityGrant::derive(Some(&caps), "test-capsule").unwrap_err();

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
        let grant = ToolCapabilityGrant::derive(None, &ceiling, "test-capsule").unwrap();

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
        let grant = ToolCapabilityGrant::derive(Some(&caps), &ceiling, "test-capsule").unwrap();

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
        let grant = ToolCapabilityGrant::derive(Some(&caps), &ceiling, "test-capsule").unwrap();

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
        let grant = ToolCapabilityGrant::derive(Some(&caps), &ceiling, "test-capsule").unwrap();

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
        let grant = ToolCapabilityGrant::derive(Some(&caps), &ceiling, "test-capsule").unwrap();

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
        let grant = ToolCapabilityGrant::derive(Some(&caps), &ceiling, "test-capsule").unwrap();

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
            let err =
                ToolCapabilityGrant::derive(Some(&caps), &ceiling(), "test-capsule").unwrap_err();
            assert!(
                matches!(err, RuntimeError::InvalidFilesystemScope { .. }),
                "scope {scope} should fail staging, got: {err}"
            );
        }
    }

    #[test]
    fn tool_grant_rejects_malformed_network_entry() {
        let caps = capabilities_block(Some(vec!["ftp://files.example.com"]), None);
        let err = ToolCapabilityGrant::derive(Some(&caps), &ceiling(), "test-capsule").unwrap_err();

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
        let grant = ToolCapabilityGrant::derive(Some(&caps), &[], "test-capsule").unwrap();

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
        let grant = ToolCapabilityGrant::derive(Some(&caps), &ceiling, "test-capsule").unwrap();

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

    /// The two opposite baselines, read off one manifest: a `runtime: tool` and a
    /// `runtime: driver` entry that declare nothing keep the whole accessible workdir, and a
    /// `runtime: hook` entry that declares nothing holds no descriptor at all.
    #[test]
    fn undeclared_entries_report_the_baseline_their_role_starts_from() {
        let tool = murmur_artifact::ArtifactRuntime::Tool;
        let driver = murmur_artifact::ArtifactRuntime::Driver;
        let hook = murmur_artifact::ArtifactRuntime::Hook;

        let reports = preopen_reports(vec![
            ("notes-tool", &tool, None),
            ("anthropic-driver", &driver, None),
            ("telemetry-hook", &hook, None),
        ])
        .unwrap();

        assert_eq!(
            reports,
            vec![
                PreopenReport {
                    artifact: "notes-tool".to_string(),
                    role: "tool".to_string(),
                    scope: None,
                    surface: PreopenSurface::WholeWorkdir,
                },
                PreopenReport {
                    artifact: "anthropic-driver".to_string(),
                    role: "driver".to_string(),
                    scope: None,
                    surface: PreopenSurface::WholeWorkdir,
                },
                PreopenReport {
                    artifact: "telemetry-hook".to_string(),
                    role: "hook".to_string(),
                    scope: None,
                    surface: PreopenSurface::Nothing,
                },
            ]
        );
    }

    /// A declared scope resolves the same way whatever the role, and is carried verbatim so an
    /// operator reads back the line they wrote.
    #[test]
    fn a_declared_scope_narrows_every_role_to_a_subtree() {
        let tool = murmur_artifact::ArtifactRuntime::Tool;
        let hook = murmur_artifact::ArtifactRuntime::Hook;
        let tool_caps = capabilities_block(None, Some("cache"));
        let hook_caps = capabilities_block(None, Some("hook-state"));

        let reports = preopen_reports(vec![
            ("notes-tool", &tool, Some(&tool_caps)),
            ("telemetry-hook", &hook, Some(&hook_caps)),
        ])
        .unwrap();

        assert_eq!(
            reports,
            vec![
                PreopenReport {
                    artifact: "notes-tool".to_string(),
                    role: "tool".to_string(),
                    scope: Some("cache".to_string()),
                    surface: PreopenSurface::ScopedSubtree,
                },
                PreopenReport {
                    artifact: "telemetry-hook".to_string(),
                    role: "hook".to_string(),
                    scope: Some("hook-state".to_string()),
                    surface: PreopenSurface::ScopedSubtree,
                },
            ]
        );
    }

    /// A skill is markdown staged into the workdir with no component behind it, so it produces no
    /// entry at all — not a `Nothing` entry, which would claim a guest was denied a descriptor.
    #[test]
    fn a_skill_produces_no_entry() {
        let skill = murmur_artifact::ArtifactRuntime::Skill;
        let tool = murmur_artifact::ArtifactRuntime::Tool;

        let reports = preopen_reports(vec![
            ("notes-skill", &skill, None),
            ("notes-tool", &tool, None),
        ])
        .unwrap();

        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].artifact, "notes-tool");
    }

    /// The report runs the declared scope through the same validator the grant derivation does,
    /// so a scope that would refuse a launch refuses the report.
    #[test]
    fn an_escaping_scope_is_refused() {
        let tool = murmur_artifact::ArtifactRuntime::Tool;
        let caps = capabilities_block(None, Some("../escape"));

        let err = preopen_reports(vec![("notes-tool", &tool, Some(&caps))]).unwrap_err();

        assert!(
            matches!(
                err,
                RuntimeError::InvalidFilesystemScope { ref scope, .. } if scope == "../escape"
            ),
            "unexpected error: {err}"
        );
        assert!(
            ToolCapabilityGrant::derive(Some(&caps), &[], "test-capsule").is_err(),
            "the grant derivation must refuse what the report refuses"
        );
    }

    /// The three surfaces carry stable kebab-case wire names, and the enum has exactly three
    /// variants: a guest holds the workdir, one subtree of it, or no descriptor.
    #[test]
    fn preopen_surfaces_serialize_to_stable_wire_names() {
        for (surface, wire) in [
            (PreopenSurface::WholeWorkdir, "whole-workdir"),
            (PreopenSurface::ScopedSubtree, "scoped-subtree"),
            (PreopenSurface::Nothing, "nothing"),
        ] {
            assert_eq!(
                serde_json::to_value(surface).unwrap(),
                serde_json::Value::String(wire.to_string())
            );
        }
    }
}
