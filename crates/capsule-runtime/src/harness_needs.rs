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

use crate::host_paths::{self, DeclaredHostPath, HostPath, HostPathAccess, HostPathContext};
use crate::process_driver::{Description, HarnessNeeds, PathAccess};
use crate::types::CapabilityPolicy;

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
    /// The interpreter a `#!` first line names; `None` for an ELF binary.
    pub interpreter: Option<PathBuf>,
    /// For `#!/usr/bin/env <name>`, the `env` the line names (`<name>` being the interpreter);
    /// `None` otherwise.
    pub launcher: Option<PathBuf>,
}

impl HarnessBinaryInfo {
    /// `binary` and the interpreter its `#!` line names, resolved on the harness's `PATH`.
    #[must_use]
    pub fn read(binary: &Path) -> Self {
        let shebang = read_shebang(binary, &harness_path_env());
        let (launcher, interpreter) = match shebang {
            Some(shebang) => (shebang.launcher, Some(shebang.interpreter)),
            None => (None, None),
        };
        Self {
            path: binary.to_path_buf(),
            interpreter,
            launcher,
        }
    }

    /// Every file the harness's `exec` runs: the binary, then for a `#!` script the `env` launcher
    /// the line names, if any, and the interpreter. Each is granted exactly as a
    /// `capabilities.shell.allow` binary.
    #[must_use]
    pub fn exec_files(&self) -> Vec<PathBuf> {
        std::iter::once(self.path.clone())
            .chain(self.launcher.clone())
            .chain(self.interpreter.clone())
            .collect()
    }
}

/// The `PATH` a harness's `#!` interpreter is resolved on: the runtime's own. The harness is
/// spawned with the operator's environment on an unconfined session and an `env.allow`-derived
/// one otherwise, both of which carry the host `PATH` when the manifest declares it.
pub(crate) fn harness_path_env() -> String {
    std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_string())
}

/// A capsule's harness grants and the binary they were resolved for: the one call staging,
/// `mur doctor` and `mur run --explain-scope` make.
pub(crate) struct ResolvedHarness {
    pub(crate) grants: HarnessGrants,
    /// `None` when the harness binary did not resolve on this host.
    pub(crate) binary: Option<HarnessBinaryInfo>,
}

/// Resolves a capsule's harness grants from its driver's `description` and its manifest's
/// `policy`, checking each allowed path on exactly an operator `host_paths` entry's terms: the
/// `HOME` `mur` runs with, murmur's own home `$HOME/.murmur`, and the session's work folder root
/// `workdir`. `binary` is the resolved harness binary, or `None` when it did not resolve.
pub(crate) fn resolve_capsule_harness(
    driver_ref: &str,
    description: &Description,
    binary: Option<&Path>,
    policy: &CapabilityPolicy,
    workdir: &Path,
) -> ResolvedHarness {
    let binary = binary.map(HarnessBinaryInfo::read);
    let home = crate::murmur_home::home_path().ok();
    let murmur_home = home.as_ref().map(|home| home.join(".murmur"));
    let context = HostPathContext {
        home: home.as_deref(),
        murmur_home: murmur_home.as_deref(),
        workdir,
    };
    let grants = resolve_harness_grants(
        driver_ref,
        &description.harness_needs,
        &policy.harness_paths,
        &policy.network_allow,
        &description.required_env,
        &policy.env_allow,
        binary.as_ref(),
        &context,
    );
    ResolvedHarness { grants, binary }
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
            PathAllow::Allowed {
                allowed_by,
                entry_path,
            } => {
                // Granted at the driver's own path and access, never the wider manifest entry.
                let declared = DeclaredHostPath {
                    index: 0,
                    declared: need.path.clone(),
                    expanded: folded,
                    access,
                };
                let resolved =
                    host_paths::resolve_host_paths(std::slice::from_ref(&declared), context)
                        .and_then(|resolved| {
                            let host_path = resolved.into_iter().next().expect("one in, one out");
                            escapes_allowing_entry(&host_path, &entry_path, &allowed_by)
                                .map_or(Ok(host_path), Err)
                        });
                match resolved {
                    Ok(host_path) => {
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
                    Err(refusal) if refusal.missing => {
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
    /// `allowed_by` is the entry as written; `entry_path` is it folded against the home.
    Allowed {
        allowed_by: String,
        entry_path: PathBuf,
    },
    NotAllowed {
        reason: String,
    },
}

/// The refusal for a granted path that resolved, through a symlink on its way, outside the entry
/// that allowed it, or `None` when it lies under that entry as resolved on this host. The allow
/// rule compares the driver's path lexically; this keeps a symlink below the entry from granting
/// whatever it points at.
fn escapes_allowing_entry(
    host_path: &HostPath,
    entry_path: &Path,
    allowed_by: &str,
) -> Option<host_paths::HostPathRefusal> {
    let refuse = |reason: String| host_paths::HostPathRefusal {
        index: 0,
        declared: host_path.declared.clone(),
        resolved: Some(host_path.path.clone()),
        reason,
        missing: false,
    };
    match std::fs::canonicalize(entry_path) {
        Ok(entry) if host_path.path.starts_with(&entry) => None,
        Ok(entry) => Some(refuse(format!(
            "resolves to {}, outside {} where the capabilities.harness.paths entry {allowed_by:?}              that allows it resolves; a symlink on the way leads out of the granted folder",
            host_path.path.display(),
            entry.display(),
        ))),
        Err(error) => Some(refuse(format!(
            "the capabilities.harness.paths entry {allowed_by:?} that allows it cannot be              resolved on this host: {error}"
        ))),
    }
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
                    entry_path: entry.expanded.clone(),
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

/// What a `#!` first line makes `exec` run: `interpreter`, and for `#!/usr/bin/env <name>` the
/// `env` the line names as `launcher` with `<name>` resolved on the PATH as `interpreter`.
struct Shebang {
    launcher: Option<PathBuf>,
    interpreter: PathBuf,
}

/// The longest `#!` line the kernel reads (`BINPRM_BUF_SIZE`): the most read of a harness binary.
const SHEBANG_MAX: u64 = 256;

/// Reads the `#!` first line of `binary`, its interpreter resolved on `path_env`. `None` for an ELF
/// binary, a script with no readable `#!` line, or an interpreter that does not resolve. Only the
/// one interpreter the line names is followed; an `env` chain beyond it is not.
fn read_shebang(binary: &Path, path_env: &str) -> Option<Shebang> {
    use std::io::Read;
    let mut head = Vec::with_capacity(SHEBANG_MAX as usize);
    std::fs::File::open(binary)
        .ok()?
        .take(SHEBANG_MAX)
        .read_to_end(&mut head)
        .ok()?;
    let line = head.strip_prefix(b"#!")?.split(|&b| b == b'\n').next()?;
    let line = std::str::from_utf8(line).ok()?;
    let mut parts = line.split_whitespace();
    let first = parts.next()?;
    let program = std::fs::canonicalize(first).ok()?;
    if Path::new(first).file_name().and_then(|n| n.to_str()) == Some("env") {
        let name = parts.next()?;
        return Some(Shebang {
            launcher: Some(program),
            interpreter: resolve_on_path(name, path_env)?,
        });
    }
    Some(Shebang {
        launcher: None,
        interpreter: program,
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process_driver::PathNeed;
    use murmur_artifact::{HarnessPathAccess, HarnessPathEntry};

    fn needs(paths: &[(&str, PathAccess)], hosts: &[&str]) -> HarnessNeeds {
        HarnessNeeds {
            paths: paths
                .iter()
                .map(|(path, access)| PathNeed {
                    path: (*path).to_string(),
                    access: *access,
                })
                .collect(),
            hosts: hosts.iter().map(|host| (*host).to_string()).collect(),
        }
    }

    fn grant(path: &str, access: HarnessPathAccess) -> HarnessPathEntry {
        HarnessPathEntry {
            path: path.to_string(),
            access,
        }
    }

    /// A scratch home outside `/tmp`, which a granted path may not be under: the workspace's
    /// `target/`, with murmur's own home and a work folder inside it as a session has them.
    struct Host {
        _root: tempfile::TempDir,
        home: PathBuf,
        murmur_home: PathBuf,
        workdir: PathBuf,
    }

    impl Host {
        fn new() -> Self {
            let root =
                tempfile::tempdir_in(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target"))
                    .unwrap();
            let home = std::fs::canonicalize(root.path()).unwrap().join("home");
            let murmur_home = home.join(".murmur");
            let workdir = home.join("project");
            std::fs::create_dir_all(&murmur_home).unwrap();
            std::fs::create_dir_all(&workdir).unwrap();
            Self {
                _root: root,
                home,
                murmur_home,
                workdir,
            }
        }

        fn context(&self) -> HostPathContext<'_> {
            HostPathContext {
                home: Some(&self.home),
                murmur_home: Some(&self.murmur_home),
                workdir: &self.workdir,
            }
        }

        fn resolve(
            &self,
            needs: &HarnessNeeds,
            manifest: &[HarnessPathEntry],
            network_allow: &[&str],
        ) -> HarnessGrants {
            let network_allow: Vec<String> = network_allow
                .iter()
                .map(|rule| (*rule).to_string())
                .collect();
            resolve_harness_grants(
                "local/d@1.0.0",
                needs,
                manifest,
                &network_allow,
                &[],
                &[],
                None,
                &self.context(),
            )
        }
    }

    fn refusal_text(grants: &HarnessGrants) -> String {
        grants
            .refusal
            .as_ref()
            .expect("the launch is refused")
            .to_string()
    }

    #[test]
    fn well_formed_and_empty_needs_pass_validation() {
        assert!(validate_harness_needs(&needs(&[], &[])).is_ok());
        assert!(validate_harness_needs(&needs(
            &[
                ("~/.claude", PathAccess::ReadWrite),
                ("/opt/harness/", PathAccess::Read),
            ],
            &["api.anthropic.com", "example.invalid:443"],
        ))
        .is_ok());
    }

    /// Scenario 7: every malformed need is refused naming it, and the driver load turns the reason
    /// into `E-RUN-032`.
    #[test]
    fn a_malformed_need_is_refused_naming_it() {
        for (path, expected) in [
            ("", "is empty"),
            ("relative/dir", "is relative"),
            ("~", "is ~ alone"),
            ("~/", "is ~/ alone"),
            ("/", "is /, the whole host filesystem"),
            ("~/a/../b", "has a . or .. segment"),
            ("/opt/./x", "has a . or .. segment"),
            ("/opt//x", "has an empty path segment"),
        ] {
            let reason = validate_harness_needs(&needs(&[(path, PathAccess::Read)], &[]))
                .expect_err("a malformed path need is refused");
            assert!(reason.contains(&format!("{path:?}")), "{path}: {reason}");
            assert!(reason.contains(expected), "{path}: {reason}");
        }
        let reason = validate_harness_needs(&needs(&[], &["http://example.com/x"]))
            .expect_err("a host the network.allow parser refuses is refused");
        assert!(reason.contains("\"http://example.com/x\""), "{reason}");
        assert!(
            reason.contains("not a capabilities.network.allow entry"),
            "{reason}"
        );
    }

    #[test]
    fn an_allowed_need_under_home_is_granted_at_its_own_path_and_access() {
        let host = Host::new();
        std::fs::create_dir_all(host.home.join("state/sessions")).unwrap();
        let grants = host.resolve(
            &needs(&[("~/state/sessions", PathAccess::ReadWrite)], &[]),
            &[grant("~/state", HarnessPathAccess::ReadWrite)],
            &[],
        );
        assert!(grants.refusal.is_none(), "{:?}", grants.refusal);
        assert_eq!(grants.granted_paths.len(), 1);
        assert_eq!(
            grants.granted_paths[0].path,
            host.home.join("state/sessions")
        );
        assert_eq!(grants.granted_paths[0].access, HostPathAccess::ReadWrite);
        let row = &grants.report.paths[0];
        assert_eq!(row.status, "granted");
        assert_eq!(row.allowed_by.as_deref(), Some("~/state"));
        assert_eq!(row.access, "read-write");
    }

    #[test]
    fn an_entry_below_the_need_access_names_the_shortfall() {
        let host = Host::new();
        std::fs::create_dir_all(host.home.join("state")).unwrap();
        let grants = host.resolve(
            &needs(&[("~/state", PathAccess::ReadWrite)], &[]),
            &[grant("~/state", HarnessPathAccess::Read)],
            &[],
        );
        let message = refusal_text(&grants);
        assert!(message.contains("'local/d@1.0.0'"), "{message}");
        assert!(message.contains("~/state (read-write)"), "{message}");
        assert!(
            message.contains(
                "entry \"~/state\" covers it at access read, lower than the need's read-write"
            ),
            "{message}"
        );
        assert!(grants.granted_paths.is_empty());
        assert_eq!(grants.report.paths[0].status, "refused");
    }

    #[test]
    fn every_disallowed_need_is_named_in_one_refusal() {
        let host = Host::new();
        let grants = host.resolve(
            &needs(
                &[("~/a", PathAccess::Read), ("/srv/b", PathAccess::ReadWrite)],
                &["example.invalid:443"],
            ),
            &[],
            &[],
        );
        let message = refusal_text(&grants);
        for need in [
            "~/a (read)",
            "/srv/b (read-write)",
            "example.invalid:443: no capabilities.network.allow",
        ] {
            assert!(message.contains(need), "{need}: {message}");
        }
        assert!(
            !message.contains('\n'),
            "one line, so doctor keeps every need: {message}"
        );
    }

    #[test]
    fn an_allowed_need_absent_on_this_host_is_missing_and_never_refuses() {
        let host = Host::new();
        let grants = host.resolve(
            &needs(&[("~/absent", PathAccess::ReadWrite)], &[]),
            &[grant("~/absent", HarnessPathAccess::ReadWrite)],
            &[],
        );
        assert!(grants.refusal.is_none(), "{:?}", grants.refusal);
        assert!(grants.granted_paths.is_empty());
        let row = &grants.report.paths[0];
        assert_eq!(row.status, "missing");
        assert_eq!(row.path, None);
        assert_eq!(row.allowed_by.as_deref(), Some("~/absent"));
    }

    /// A read-write need under the home is checked against murmur's own home `~/.murmur`, not
    /// against the home itself.
    #[test]
    fn a_read_write_need_over_murmurs_own_home_is_refused_and_a_sibling_is_not() {
        let host = Host::new();
        std::fs::create_dir_all(host.home.join("sibling")).unwrap();
        let grants = host.resolve(
            &needs(
                &[
                    ("~/.murmur", PathAccess::ReadWrite),
                    ("~/sibling", PathAccess::ReadWrite),
                ],
                &[],
            ),
            &[
                grant("~/.murmur", HarnessPathAccess::ReadWrite),
                grant("~/sibling", HarnessPathAccess::ReadWrite),
            ],
            &[],
        );
        let message = refusal_text(&grants);
        assert!(message.contains("~/.murmur (read-write)"), "{message}");
        assert!(
            message.contains("read-write over murmur's own home"),
            "{message}"
        );
        assert!(!message.contains("~/sibling"), "{message}");
        assert_eq!(grants.report.paths[1].status, "granted");
    }

    /// A symlink below the allowing entry cannot carry the grant out of it: the need is refused,
    /// not granted at the link's target.
    #[test]
    fn a_need_that_a_symlink_leads_outside_its_entry_is_refused() {
        let host = Host::new();
        let parent = host.home.join("parent");
        let elsewhere = host.home.join("elsewhere");
        std::fs::create_dir_all(&parent).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, parent.join("sessions")).unwrap();
        let grants = host.resolve(
            &needs(&[("~/parent/sessions", PathAccess::ReadWrite)], &[]),
            &[grant("~/parent", HarnessPathAccess::ReadWrite)],
            &[],
        );
        let message = refusal_text(&grants);
        assert!(
            message.contains("~/parent/sessions (read-write)"),
            "{message}"
        );
        assert!(
            message.contains("a symlink on the way leads out"),
            "{message}"
        );
        assert!(grants.granted_paths.is_empty());
        let row = &grants.report.paths[0];
        assert_eq!(row.status, "refused");
        assert_eq!(row.path.as_deref(), Some(elsewhere.to_str().unwrap()));

        // A link that stays inside the entry is granted at its target.
        let inside = parent.join("real");
        std::fs::create_dir_all(&inside).unwrap();
        std::os::unix::fs::symlink(&inside, parent.join("linked")).unwrap();
        let grants = host.resolve(
            &needs(&[("~/parent/linked", PathAccess::ReadWrite)], &[]),
            &[grant("~/parent", HarnessPathAccess::ReadWrite)],
            &[],
        );
        assert!(grants.refusal.is_none(), "{:?}", grants.refusal);
        assert_eq!(grants.granted_paths[0].path, inside);
    }

    /// A host need is allowed only when one rule admits every port it spans, under the egress
    /// proxy's matching: a bare host spans 80 and 443.
    #[test]
    fn a_host_need_is_allowed_only_when_every_port_it_spans_is_admitted() {
        let host = Host::new();
        let resolve = |need: &str, allow: &[&str]| {
            host.resolve(&needs(&[], &[need]), &[], allow).report.hosts[0].clone()
        };
        let row = resolve("example.invalid:443", &["example.invalid"]);
        assert_eq!(
            (row.status, row.allowed_by.as_deref()),
            ("allowed", Some("example.invalid"))
        );
        let row = resolve("example.invalid:443", &["example.invalid:443"]);
        assert_eq!(row.status, "allowed");
        assert_eq!(
            resolve("example.invalid", &["example.invalid:443"]).status,
            "refused"
        );
        assert_eq!(
            resolve("example.invalid:8443", &["example.invalid"]).status,
            "refused"
        );
        assert_eq!(
            resolve("example.invalid:443", &["other.invalid"]).status,
            "refused"
        );
    }

    #[test]
    fn a_shebang_names_its_interpreter_and_env_both() {
        let host = Host::new();
        let script = host.home.join("harness");
        std::fs::write(&script, "#!/bin/sh\necho hi\n").unwrap();
        let info = HarnessBinaryInfo::read(&script);
        let sh = std::fs::canonicalize("/bin/sh").unwrap();
        assert_eq!(info.interpreter.as_ref(), Some(&sh));
        assert_eq!(info.launcher, None);
        assert_eq!(info.exec_files(), vec![script.clone(), sh.clone()]);

        std::fs::write(&script, "#!/usr/bin/env sh\necho hi\n").unwrap();
        let info = HarnessBinaryInfo::read(&script);
        let env = std::fs::canonicalize("/usr/bin/env").unwrap();
        assert_eq!(info.launcher.as_ref(), Some(&env));
        assert_eq!(info.exec_files().len(), 3, "{:?}", info.exec_files());
        assert!(info.exec_files().contains(&env));

        std::fs::write(&script, b"\x7fELF\x02\x01").unwrap();
        let info = HarnessBinaryInfo::read(&script);
        assert_eq!((info.interpreter, info.launcher), (None, None));
    }
}
