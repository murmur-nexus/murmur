//! What a `transport: process` harness needs beyond what the capsule's class gives every command,
//! and where the capsule manifest allows it.
//!
//! A process driver's `describe().harness-needs` names the host directories and hosts the harness
//! reaches on its own — Claude Code's config folder, its inference host — the environment half
//! being `required-env`. Each need is granted only where the capsule manifest also allows it:
//! a path by `capabilities.harness.paths`, a host by `capabilities.network.allow`. A need the
//! manifest does not allow refuses the launch with [`E_CAP_021`](crate::errors); nothing is
//! spawned.
//!
//! What is granted is the driver's path at the driver's access, never the wider manifest entry,
//! and a granted path goes through the same host-folder checks as an operator `host_paths` entry
//! ([`crate::host_paths::resolve_host_paths`]) and never carries `Execute`. The resolution is
//! computed once at staging, set on the policy's report, and recomputed identically by
//! `mur run --explain-scope` (read-only, exit 0 even when a need is refused).

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::host_paths::{
    self, DeclaredHostPath, HostPath, HostPathAccess, HostPathContext, HostPathEntry,
};
use crate::process_driver::{HarnessNeeds, PathAccess};

/// The resolved harness grant for one session: the report `--explain-scope` and
/// `session_start.effective_grants` carry, the host paths to add to the harness's enforcement,
/// and the refusal (`E-CAP-021`) when any need the manifest does not allow, or a granted path
/// resolving to a refused location, was found.
///
/// The report is always built, with every need's status, so `--explain-scope` can print it and
/// exit 0 even when `refusal` is set. Staging and `mur doctor` propagate `refusal`; explain-scope
/// ignores it.
pub struct HarnessGrants {
    pub report: HarnessReport,
    /// The host paths to add to the harness's enforcement: one per path need that resolved to a
    /// real directory on this host (`status: granted`). A missing or refused need contributes
    /// none.
    pub granted_paths: Vec<HostPath>,
    /// `Some` when a path or host need the manifest does not allow, or a granted path resolving to
    /// a refused location, was found: the `E-CAP-021` refusal naming every one.
    pub refusal: Option<crate::errors::RuntimeError>,
}

/// The `harness` key of a [`crate::containment::ScopeReport`]: the driver, the harness binary and
/// its interpreter, and each path, host and env need with its status. `null` on a capsule whose
/// transport is not `process`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HarnessReport {
    /// The process driver, `name@version`.
    pub driver: String,
    /// The resolved harness binary and its `#!` interpreter, or `null` when the binary did not
    /// resolve (explain-scope on a host missing the harness).
    pub binary: Option<HarnessBinaryReport>,
    /// One row per declared path need, in declaration order.
    pub paths: Vec<HarnessPathReport>,
    /// One row per declared host need, in declaration order.
    pub hosts: Vec<HarnessHostReport>,
    /// One row per `required-env` variable, in declaration order.
    pub env: Vec<HarnessEnvReport>,
    /// Why the driver could not be read, for `--explain-scope` on a missing or unloadable driver;
    /// `null` otherwise, when every list above was resolved from a loaded driver.
    pub unresolved: Option<String>,
}

/// The resolved harness binary and, for a `#!` script, the interpreter its first line names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HarnessBinaryReport {
    pub path: String,
    /// The interpreter a `#!` line names, resolved on the harness's `PATH`; `null` for an ELF
    /// binary or a script whose interpreter did not resolve.
    pub interpreter: Option<String>,
}

/// One path need's row: what the driver asked for, where it resolved, and whether it was granted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HarnessPathReport {
    /// The need as the driver declared it (`~/…` or absolute).
    pub need: String,
    /// The directory it resolved to on this host, or `null` when missing or not allowed.
    pub path: Option<String>,
    /// The access the driver asked for.
    pub access: &'static str,
    /// The `capabilities.harness.paths` entry that allows it, as written, or `null` when none
    /// does.
    pub allowed_by: Option<String>,
    /// `granted` (allowed and present on this host), `missing` (allowed but absent) or `refused`
    /// (not allowed, or resolving to a refused location).
    pub status: &'static str,
    /// Why, for a `refused` status; `null` otherwise.
    pub reason: Option<String>,
}

/// One host need's row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HarnessHostReport {
    /// The need as the driver declared it, spelled as a `capabilities.network.allow` entry.
    pub need: String,
    /// The `capabilities.network.allow` entry that admits it, or `null` when none does.
    pub allowed_by: Option<String>,
    /// `allowed` or `refused`.
    pub status: &'static str,
}

/// One `required-env` variable's row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HarnessEnvReport {
    pub name: String,
    /// `allowed` when `capabilities.env.allow` declares it, `refused` otherwise.
    pub status: &'static str,
}

const STATUS_GRANTED: &str = "granted";
const STATUS_MISSING: &str = "missing";
const STATUS_REFUSED: &str = "refused";
const STATUS_ALLOWED: &str = "allowed";

/// The resolved harness binary, for the report and the enforcement derivation.
#[derive(Debug, Clone)]
pub struct HarnessBinaryInfo {
    pub path: PathBuf,
    /// The interpreter a `#!` first line names, resolved on `path_env`; `None` for an ELF binary.
    pub interpreter: Option<PathBuf>,
}

/// Validates the shape of every declared harness need, as a driver load-time check
/// ([`crate::process_driver::validate_description`], `E-RUN-032`). Returns the offending need in
/// the message, or `Ok` for well-formed and empty needs.
pub(crate) fn validate_harness_needs(needs: &HarnessNeeds) -> Result<(), String> {
    for need in &needs.paths {
        validate_need_path(&need.path)?;
    }
    for host in &needs.hosts {
        crate::network_policy::NetworkAllowRule::parse(host).map_err(|error| {
            format!("names the host need {host:?}, which is not a capabilities.network.allow entry: {error}")
        })?;
    }
    Ok(())
}

/// A path need is non-empty, starts with `~/` or `/`, has only normal components after that (no
/// `.`/`..`, no empty segment from `//` other than a trailing `/`), and is not `/` or `~`/`~/`
/// alone.
fn validate_need_path(path: &str) -> Result<(), String> {
    let bad = |reason: &str| Err(format!("names the path need {path:?}, which {reason}"));
    if path.is_empty() {
        return bad("is empty");
    }
    let rest = if let Some(rest) = path.strip_prefix("~/") {
        if rest.is_empty() {
            return bad("is ~/ alone, naming the home folder");
        }
        rest
    } else if path == "~" {
        return bad("is ~ alone, naming the home folder");
    } else if let Some(rest) = path.strip_prefix('/') {
        if rest.is_empty() {
            return bad("is /, the whole host filesystem");
        }
        rest
    } else {
        return bad("is relative; write an absolute path or ~/…");
    };
    // A trailing `/` is allowed; any other empty segment is a `//`, and `.`/`..` are not normal.
    let trimmed = rest.strip_suffix('/').unwrap_or(rest);
    for segment in trimmed.split('/') {
        match segment {
            "" => return bad("has an empty path segment (//)"),
            "." | ".." => return bad("has a . or .. segment"),
            _ => {}
        }
    }
    Ok(())
}

/// Resolves a session's harness grants: the allow rule against the manifest, then the host-folder
/// checks for each allowed path, collecting every disallowed or refused need into one
/// `E-CAP-021` refusal.
///
/// `driver_ref` is `name@version`. `manifest_paths` is `capabilities.harness.paths`. `binary` is
/// the resolved harness binary, or `None` when it did not resolve (explain-scope on a host
/// missing it). `context` is the same `HostPathContext` an operator `host_paths` entry is checked
/// against.
#[allow(clippy::too_many_arguments)]
pub fn resolve_harness_grants(
    driver_ref: &str,
    needs: &HarnessNeeds,
    manifest_paths: &[murmur_artifact::HarnessPathEntry],
    network_allow: &[String],
    required_env: &[String],
    env_allow: &[String],
    binary: Option<&HarnessBinaryInfo>,
    context: &HostPathContext<'_>,
) -> HarnessGrants {
    let mut refusals: Vec<String> = Vec::new();
    let mut granted_paths: Vec<HostPath> = Vec::new();

    // Both the needs and the manifest entries folded lexically against the same home, so the
    // ancestor comparison is purely lexical and `--explain-scope` answers it without the host.
    let home = context.home;
    let manifest = fold_manifest_paths(manifest_paths, home);

    let mut path_reports = Vec::with_capacity(needs.paths.len());
    for need in &needs.paths {
        let access = host_path_access(need.access);
        let folded = match host_paths::expand_and_fold(&need.path, home) {
            Ok(folded) => folded,
            // Shape is validated at load, so this is only reachable off a host with no home for a
            // `~/` need; report it refused rather than panicking.
            Err(reason) => {
                refusals.push(format!("{} ({}): {reason}", need.path, access.wire_name()));
                path_reports.push(HarnessPathReport {
                    need: need.path.clone(),
                    path: None,
                    access: access.wire_name(),
                    allowed_by: None,
                    status: STATUS_REFUSED,
                    reason: Some(reason),
                });
                continue;
            }
        };
        match allow_path(&manifest, &folded, access) {
            PathAllow::Allowed { allowed_by } => {
                // Granted at the driver's own path and access, never the wider manifest entry.
                let declared = DeclaredHostPath {
                    index: 0,
                    declared: need.path.clone(),
                    expanded: folded,
                    access,
                };
                match host_paths::resolve_host_paths(std::slice::from_ref(&declared), context) {
                    Ok(resolved) => {
                        let host_path = resolved.into_iter().next().expect("one in, one out");
                        path_reports.push(HarnessPathReport {
                            need: need.path.clone(),
                            path: Some(host_path.path.display().to_string()),
                            access: access.wire_name(),
                            allowed_by: Some(allowed_by),
                            status: STATUS_GRANTED,
                            reason: None,
                        });
                        granted_paths.push(host_path);
                    }
                    Err(refusal) if refusal_is_absent(&refusal) => {
                        // Allowed but absent on this host: reported, not granted, never refuses.
                        path_reports.push(HarnessPathReport {
                            need: need.path.clone(),
                            path: None,
                            access: access.wire_name(),
                            allowed_by: Some(allowed_by),
                            status: STATUS_MISSING,
                            reason: None,
                        });
                    }
                    Err(refusal) => {
                        let reason = refusal.reason.clone();
                        refusals.push(format!("{} ({}): {reason}", need.path, access.wire_name()));
                        path_reports.push(HarnessPathReport {
                            need: need.path.clone(),
                            path: refusal.resolved.map(|p| p.display().to_string()),
                            access: access.wire_name(),
                            allowed_by: Some(allowed_by),
                            status: STATUS_REFUSED,
                            reason: Some(reason),
                        });
                    }
                }
            }
            PathAllow::NotAllowed { reason } => {
                refusals.push(format!("{} ({}): {reason}", need.path, access.wire_name()));
                path_reports.push(HarnessPathReport {
                    need: need.path.clone(),
                    path: None,
                    access: access.wire_name(),
                    allowed_by: None,
                    status: STATUS_REFUSED,
                    reason: Some(reason),
                });
            }
        }
    }

    let network_rules =
        crate::network_policy::parse_network_allow_rules(network_allow).unwrap_or_default();
    let mut host_reports = Vec::with_capacity(needs.hosts.len());
    for host in &needs.hosts {
        match allow_host(host, network_allow, &network_rules) {
            Some(allowed_by) => host_reports.push(HarnessHostReport {
                need: host.clone(),
                allowed_by: Some(allowed_by),
                status: STATUS_ALLOWED,
            }),
            None => {
                refusals.push(format!(
                    "{host}: no capabilities.network.allow entry admits it"
                ));
                host_reports.push(HarnessHostReport {
                    need: host.clone(),
                    allowed_by: None,
                    status: STATUS_REFUSED,
                });
            }
        }
    }

    let env = required_env
        .iter()
        .map(|name| HarnessEnvReport {
            name: name.clone(),
            status: if env_allow.iter().any(|allowed| allowed == name) {
                STATUS_ALLOWED
            } else {
                STATUS_REFUSED
            },
        })
        .collect();

    let report = HarnessReport {
        driver: driver_ref.to_string(),
        binary: binary.map(|info| HarnessBinaryReport {
            path: info.path.display().to_string(),
            interpreter: info.interpreter.as_ref().map(|p| p.display().to_string()),
        }),
        paths: path_reports,
        hosts: host_reports,
        env,
        unresolved: None,
    };

    let refusal =
        (!refusals.is_empty()).then(|| crate::errors::RuntimeError::HarnessNeedsNotAllowed {
            driver: driver_ref.to_string(),
            refusals,
        });

    HarnessGrants {
        report,
        granted_paths,
        refusal,
    }
}

/// The `harness` report for `--explain-scope` when the driver could not be read: every list empty,
/// `unresolved` naming why.
#[must_use]
pub fn unresolved_report(driver_ref: &str, reason: String) -> HarnessReport {
    HarnessReport {
        driver: driver_ref.to_string(),
        binary: None,
        paths: Vec::new(),
        hosts: Vec::new(),
        env: Vec::new(),
        unresolved: Some(reason),
    }
}

/// The `Process harness (<driver>)` block `mur run --explain-scope` prints: the binary and its
/// interpreter, then each path need (`need → path (access), allowed by <entry>` for a granted one,
/// or its status and reason), each host need and each required env variable. Starts with a header
/// and ends with a newline.
#[must_use]
pub fn render_harness_block(report: &HarnessReport) -> String {
    let mut out = format!("Process harness ({})\n", report.driver);
    if let Some(reason) = &report.unresolved {
        out.push_str(&format!("  driver not read: {reason}\n"));
        return out;
    }
    match &report.binary {
        Some(binary) => {
            out.push_str(&format!("  binary     {}\n", binary.path));
            if let Some(interpreter) = &binary.interpreter {
                out.push_str(&format!("  interpreter {interpreter}\n"));
            }
        }
        None => out.push_str("  binary     <unresolved>\n"),
    }
    for path in &report.paths {
        let line = match path.status {
            STATUS_GRANTED => format!(
                "  path   {} → {} ({}), allowed by {}",
                path.need,
                path.path.as_deref().unwrap_or("?"),
                path.access,
                path.allowed_by.as_deref().unwrap_or("?"),
            ),
            STATUS_MISSING => format!(
                "  path   {} ({}): missing on this host, allowed by {}",
                path.need,
                path.access,
                path.allowed_by.as_deref().unwrap_or("?"),
            ),
            _ => format!(
                "  path   {} ({}): refused — {}",
                path.need,
                path.access,
                path.reason.as_deref().unwrap_or(""),
            ),
        };
        out.push_str(&line);
        out.push('\n');
    }
    for host in &report.hosts {
        let line = match host.status {
            STATUS_ALLOWED => format!(
                "  host   {}, allowed by {}",
                host.need,
                host.allowed_by.as_deref().unwrap_or("?"),
            ),
            _ => format!("  host   {}: refused", host.need),
        };
        out.push_str(&line);
        out.push('\n');
    }
    for env in &report.env {
        out.push_str(&format!("  env    {} ({})\n", env.name, env.status));
    }
    out
}

/// A manifest `capabilities.harness.paths` entry folded lexically against `home`.
struct FoldedManifestPath {
    declared: String,
    expanded: PathBuf,
    access: HostPathAccess,
}

fn fold_manifest_paths(
    entries: &[murmur_artifact::HarnessPathEntry],
    home: Option<&Path>,
) -> Vec<FoldedManifestPath> {
    entries
        .iter()
        .filter_map(|entry| {
            let expanded = host_paths::expand_and_fold(&entry.path, home).ok()?;
            Some(FoldedManifestPath {
                declared: entry.path.clone(),
                expanded,
                access: manifest_access(entry.access),
            })
        })
        .collect()
}

enum PathAllow {
    Allowed { allowed_by: String },
    NotAllowed { reason: String },
}

/// Whether some manifest entry's path equals `folded` or is an ancestor of it, with access at
/// least the need's. The first match's declared text is `allowed_by`. An access shortfall names
/// the entry that covered the path at too low an access.
fn allow_path(
    manifest: &[FoldedManifestPath],
    folded: &Path,
    need_access: HostPathAccess,
) -> PathAllow {
    let mut shortfall: Option<&FoldedManifestPath> = None;
    for entry in manifest {
        if folded == entry.expanded || folded.starts_with(&entry.expanded) {
            if access_at_least(entry.access, need_access) {
                return PathAllow::Allowed {
                    allowed_by: entry.declared.clone(),
                };
            }
            shortfall.get_or_insert(entry);
        }
    }
    let reason = match shortfall {
        Some(entry) => format!(
            "capabilities.harness.paths entry {:?} covers it at access {}, lower than the need's \
             {}",
            entry.declared,
            entry.access.wire_name(),
            need_access.wire_name()
        ),
        None => "no capabilities.harness.paths entry names it or an ancestor of it".to_string(),
    };
    PathAllow::NotAllowed { reason }
}

/// Whether `have` is at least `want`: `read-write` ≥ `read`.
fn access_at_least(have: HostPathAccess, want: HostPathAccess) -> bool {
    matches!(
        (have, want),
        (HostPathAccess::ReadWrite, _) | (HostPathAccess::Read, HostPathAccess::Read)
    )
}

/// Whether some `capabilities.network.allow` entry admits every host and port `need` names, under
/// the matching the egress proxy applies (a bare host spans 80 and 443). Returns the admitting
/// entry as written.
fn allow_host(
    need: &str,
    network_allow: &[String],
    rules: &[crate::network_policy::NetworkAllowRule],
) -> Option<String> {
    let need_rule = crate::network_policy::NetworkAllowRule::parse(need).ok()?;
    // The ports the need spans, under the egress proxy's mapping.
    let need_ports: Vec<u16> = crate::egress_proxy::implied_tcp_ports(&need_rule).collect();
    for (rule, declared) in rules.iter().zip(network_allow.iter()) {
        if rule.host != need_rule.host {
            continue;
        }
        let covers_every_port = need_ports.iter().all(|&port| {
            crate::egress_proxy::implied_tcp_ports(rule).any(|allowed| allowed == port)
        });
        if covers_every_port {
            return Some(declared.clone());
        }
    }
    None
}

/// Whether a host-path refusal is "does not exist", the one that is reported `missing` rather than
/// refusing.
fn refusal_is_absent(refusal: &host_paths::HostPathRefusal) -> bool {
    refusal.reason.contains("does not exist")
}

fn host_path_access(access: PathAccess) -> HostPathAccess {
    match access {
        PathAccess::Read => HostPathAccess::Read,
        PathAccess::ReadWrite => HostPathAccess::ReadWrite,
    }
}

fn manifest_access(access: murmur_artifact::HarnessPathAccess) -> HostPathAccess {
    match access {
        murmur_artifact::HarnessPathAccess::Read => HostPathAccess::Read,
        murmur_artifact::HarnessPathAccess::ReadWrite => HostPathAccess::ReadWrite,
    }
}

/// Builds a manifest `HostPathEntry` from a harness manifest path, for the shape re-check staging
/// shares with `host_paths`.
#[must_use]
pub fn manifest_path_as_host_path_entry(
    entry: &murmur_artifact::HarnessPathEntry,
) -> HostPathEntry {
    HostPathEntry {
        path: Some(entry.path.clone()),
        access: Some(manifest_access(entry.access).wire_name().to_string()),
    }
}

/// Reads the interpreter a harness binary's `#!` first line names, resolved on `path_env`. `None`
/// for an ELF binary, a script with no readable `#!` line, or an interpreter not on the PATH.
///
/// Only the one interpreter a `#!` line names is followed; `/usr/bin/env <name>` resolves both
/// `env` (as the shebang names it) and `<name>` on `path_env`, and `<name>` is the interpreter.
#[must_use]
pub fn resolve_interpreter(binary: &Path, path_env: &str) -> Option<PathBuf> {
    let bytes = std::fs::read(binary).ok()?;
    if !bytes.starts_with(b"#!") {
        return None;
    }
    let line = bytes
        .split(|&b| b == b'\n')
        .next()
        .and_then(|line| std::str::from_utf8(line).ok())?;
    let shebang = line.strip_prefix("#!")?.trim();
    let mut parts = shebang.split_whitespace();
    let first = parts.next()?;
    // `/usr/bin/env <name>`: the interpreter is `<name>`, resolved on the harness's PATH.
    if Path::new(first).file_name().and_then(|n| n.to_str()) == Some("env") {
        let name = parts.next()?;
        return resolve_on_path(name, path_env);
    }
    std::fs::canonicalize(first).ok()
}

/// A bare name resolved to an executable on `path_env`, or an absolute path used as given.
fn resolve_on_path(name: &str, path_env: &str) -> Option<PathBuf> {
    if name.contains('/') {
        return std::fs::canonicalize(name).ok();
    }
    for dir in path_env.split(':').filter(|d| !d.is_empty()) {
        let candidate = Path::new(dir).join(name);
        if candidate.is_file() {
            return std::fs::canonicalize(&candidate).ok();
        }
    }
    None
}
