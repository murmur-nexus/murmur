//! Host folders the operator lends every contained capsule on this host: the `host_paths:` list in
//! `~/.murmur/config.yaml`.
//!
//! Each entry names one host directory and an access, `read` or `read-write`. On `landlock+seccomp`
//! it becomes one Landlock rule for every command, plan shell step and `murmur:process` program; on
//! `mountns+pivot_root+landlock+seccomp` it is also bound into the composed root at the directory's
//! own absolute path. No rule ever carries `Execute`, so `capabilities.shell.allow` stays the
//! complete list of what a contained capsule can run. Below `landlock+seccomp` nothing is scoped,
//! and an entry is checked and reported but adds nothing.
//!
//! Checking happens in three steps, each refusing with the entry's index and its path as written:
//!
//! 1. [`declare_host_paths`]: the shape of what the operator wrote (`path`, `access`, `~`
//!    expansion, `/`, the same path twice). No filesystem access.
//! 2. [`resolve_host_paths`]: the entry on this host, at launch (exists, is a directory, not a
//!    system path, not read-write over murmur's own home, not inside the work folder, not the
//!    same directory twice), and the credential locations it reaches.
//! 3. [`check_execute_roots`]: against what the session may run, on the applied tier, at
//!    staging and by `mur run --explain-scope`.
//!
//! Platform-neutral: the Landlock rules and binds live in `sandbox` and `sealed`, behind the same
//! `target_os = "linux"` gates as every other rule.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// What a host path lets a capsule do there. Required in every entry: there is no default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HostPathAccess {
    /// Read files and list directories: Landlock `ReadFile | ReadDir`, and a read-only bind.
    Read,
    /// The workdir's rights without `Execute`, and a read-write bind.
    ReadWrite,
}

impl HostPathAccess {
    /// The spelling in `config.yaml` and in every report.
    #[must_use]
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::ReadWrite => "read-write",
        }
    }

    /// The access `value` spells, or `None` for anything but `read` and `read-write`.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "read" => Some(Self::Read),
            "read-write" => Some(Self::ReadWrite),
            _ => None,
        }
    }
}

impl fmt::Display for HostPathAccess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.wire_name())
    }
}

/// One `host_paths` entry exactly as the config file holds it. Both fields are optional here so
/// a missing one is refused by [`declare_host_paths`] naming the entry, rather than failing the
/// whole file's parse.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct HostPathEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access: Option<String>,
}

/// An entry whose shape is valid: `~` expanded, `.` and `..` folded, not yet looked up on the
/// host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredHostPath {
    /// Position in the config file's list.
    pub index: usize,
    /// `path` as written.
    pub declared: String,
    /// `path` made absolute against `HOME` and folded lexically.
    pub expanded: PathBuf,
    pub access: HostPathAccess,
}

/// A host path a session grants: checked on this host, in config order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostPath {
    /// `path` as written in the config file.
    pub declared: String,
    /// The directory it resolves to, symlinks followed. The rule and the bind are taken here.
    pub path: PathBuf,
    pub access: HostPathAccess,
    /// Every [`CREDENTIAL_LOCATIONS`] entry present on this host that this directory is, lies
    /// under, or contains, spelled `~/<location>`. Empty for an entry reaching none.
    pub credential_locations: Vec<String>,
}

impl HostPath {
    /// The locations joined for a report, or `None` when there are none.
    #[must_use]
    pub fn credential_location(&self) -> Option<String> {
        (!self.credential_locations.is_empty()).then(|| self.credential_locations.join(", "))
    }
}

/// Why an entry cannot be honoured. Every refusal names the entry's index, its path as written,
/// the directory it resolves to when it has one, and the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostPathRefusal {
    pub index: usize,
    /// `path` as written, or `<none>` for an entry with no `path`.
    pub declared: String,
    pub resolved: Option<PathBuf>,
    pub reason: String,
}

impl fmt::Display for HostPathRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "host_paths[{}] ({}", self.index, self.declared)?;
        if let Some(resolved) = &self.resolved {
            if Path::new(&self.declared) != resolved.as_path() {
                write!(f, ", resolved to {}", resolved.display())?;
            }
        }
        write!(f, "): {}", self.reason)
    }
}

impl std::error::Error for HostPathRefusal {}

/// What the applied tier does with a host path, as a stable wire name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HostPathMechanism {
    /// `landlock+seccomp`: one Landlock rule over the host's own filesystem.
    Landlock,
    /// `mountns+pivot_root+landlock+seccomp`: a bind into the composed root, plus the same rule.
    Bind,
    /// Any weaker tier: nothing is scoped, so the entry adds nothing.
    None,
}

impl HostPathMechanism {
    /// The mechanism of the tier whose wire name is `enforcement_tier`
    /// ([`crate::ScopeReport::enforcement_tier`]).
    #[must_use]
    pub fn for_enforcement_tier(enforcement_tier: &str) -> Self {
        match enforcement_tier {
            "mountns+pivot_root+landlock+seccomp" => Self::Bind,
            "landlock+seccomp" => Self::Landlock,
            _ => Self::None,
        }
    }

    #[must_use]
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::Landlock => "landlock",
            Self::Bind => "bind",
            Self::None => "none",
        }
    }

    /// What this mechanism does with one entry, in words, for `mur doctor`.
    #[must_use]
    pub fn describe(self, access: HostPathAccess) -> &'static str {
        match (self, access) {
            (Self::Landlock, HostPathAccess::Read) => {
                "a read-only Landlock rule over the host's own folder"
            }
            (Self::Landlock, HostPathAccess::ReadWrite) => {
                "a read-write Landlock rule over the host's own folder, never executable"
            }
            (Self::Bind, HostPathAccess::Read) => {
                "a read-only bind at its own path in the composed root, plus a read-only Landlock rule"
            }
            (Self::Bind, HostPathAccess::ReadWrite) => {
                "a read-write bind at its own path in the composed root, plus a read-write \
                 Landlock rule, never executable"
            }
            (Self::None, _) => "nothing: this tier scopes no filesystem, so every host folder is already reachable",
        }
    }
}

/// One element of `ScopeReport::host_paths`: `--explain-scope --json` and
/// `session_start.effective_grants.host_paths`. Every key is always serialized.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HostPathReport {
    /// The directory the entry resolves to.
    pub path: String,
    /// The entry's `path` as written.
    pub declared: String,
    pub access: HostPathAccess,
    pub mechanism: HostPathMechanism,
    /// The credential locations the entry reaches, joined with `, `, or `null`.
    pub credential_location: Option<String>,
}

impl HostPathReport {
    /// `declared → path (access)`, plus the credential note.
    #[must_use]
    pub fn render(&self) -> String {
        let mut line = format!("{} → {} ({})", self.declared, self.path, self.access);
        if let Some(location) = &self.credential_location {
            line.push_str(&format!(", reaches credential location {location}"));
        }
        line
    }
}

/// The reports for `host_paths` under `mechanism`, in config order.
#[must_use]
pub fn host_path_reports(
    host_paths: &[HostPath],
    mechanism: HostPathMechanism,
) -> Vec<HostPathReport> {
    host_paths
        .iter()
        .map(|host_path| HostPathReport {
            path: host_path.path.display().to_string(),
            declared: host_path.declared.clone(),
            access: host_path.access,
            mechanism,
            credential_location: host_path.credential_location(),
        })
        .collect()
}

/// A home-relative place that holds a credential, and the credential backstop patterns
/// (`shell::CREDENTIAL_ENV_PATTERNS`) for the same credential, where there is one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CredentialLocation {
    /// Relative to `HOME`.
    pub path: &'static str,
    pub backstop: &'static [&'static str],
}

/// Every credential location a host path is warned about (`W-SEC-035`). The one list.
pub const CREDENTIAL_LOCATIONS: &[CredentialLocation] = &[
    CredentialLocation {
        path: ".ssh",
        backstop: &[],
    },
    CredentialLocation {
        path: ".gnupg",
        backstop: &[],
    },
    CredentialLocation {
        path: ".aws",
        backstop: &["AWS_*"],
    },
    CredentialLocation {
        path: ".config/gh",
        backstop: &["GITHUB_TOKEN", "GH_TOKEN"],
    },
    CredentialLocation {
        path: ".docker",
        backstop: &["DOCKER_*"],
    },
    CredentialLocation {
        path: ".kube",
        backstop: &["KUBECONFIG"],
    },
    CredentialLocation {
        path: ".npmrc",
        backstop: &["NPM_TOKEN"],
    },
    CredentialLocation {
        path: ".pypirc",
        backstop: &["PYPI_TOKEN"],
    },
    CredentialLocation {
        path: ".cargo/credentials.toml",
        backstop: &["CARGO_REGISTRY_TOKEN"],
    },
    CredentialLocation {
        path: ".cargo/credentials",
        backstop: &["CARGO_REGISTRY_TOKEN"],
    },
    CredentialLocation {
        path: ".config/gcloud",
        backstop: &["GOOGLE_API_KEY"],
    },
    CredentialLocation {
        path: ".cache/huggingface",
        backstop: &["HUGGING_FACE_HUB_TOKEN"],
    },
    CredentialLocation {
        path: ".netrc",
        backstop: &[],
    },
    CredentialLocation {
        path: ".git-credentials",
        backstop: &[],
    },
    // Provider keys in `config.yaml`'s `credentials:` and `inference.api_key`.
    CredentialLocation {
        path: ".murmur",
        backstop: &[],
    },
];

/// Host directories a host path may not be, or lie under: a composed root carries its own copy of
/// each or replaces it, and the private `/tmp` replaces `/tmp`, so an entry there would not reach
/// the host's folder on every tier. The canonical `/tmp` is added by [`system_paths`].
const SYSTEM_PATHS: &[&str] = &["/etc", "/proc", "/sys", "/dev"];

/// Every system path a host path is refused under, literal and canonical.
fn system_paths() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = crate::sealed::SEALED_RUNTIME_PATHS
        .iter()
        .chain(SYSTEM_PATHS)
        .map(PathBuf::from)
        .collect();
    paths.push(PathBuf::from("/tmp"));
    paths.push(crate::temp_dir::canonical_tmp());
    let canonical: Vec<PathBuf> = paths
        .iter()
        .filter_map(|path| std::fs::canonicalize(path).ok())
        .collect();
    for path in canonical {
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    paths
}

/// Checks the shape of every entry and expands `~` against `home`, the `HOME` `mur` runs with.
///
/// Refuses an entry with no `path` or no `access`, an `access` other than `read` or
/// `read-write`, a relative path (`~user/…` and `$VAR/…` are not expanded, so they are relative),
/// a path that is or folds to `/`, and a path an earlier entry already names.
pub fn declare_host_paths(
    entries: &[HostPathEntry],
    home: Option<&Path>,
) -> Result<Vec<DeclaredHostPath>, HostPathRefusal> {
    let mut declared: Vec<DeclaredHostPath> = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let written = entry.path.clone().unwrap_or_else(|| "<none>".to_string());
        let refuse = |reason: String| HostPathRefusal {
            index,
            declared: written.clone(),
            resolved: None,
            reason,
        };
        let Some(path) = entry.path.as_deref().filter(|path| !path.trim().is_empty()) else {
            return Err(refuse(
                "the entry has no path (an unquoted `~` is YAML null: write `path: \"~\"`)"
                    .to_string(),
            ));
        };
        let access = match entry.access.as_deref() {
            None => {
                return Err(refuse(
                    "access is required: write `access: read` or `access: read-write`".to_string(),
                ))
            }
            Some(value) => HostPathAccess::parse(value).ok_or_else(|| {
                refuse(format!(
                    "access '{value}' is neither `read` nor `read-write`"
                ))
            })?,
        };
        let expanded = expand_home(path, home).map_err(refuse)?;
        let expanded = crate::protected_paths::normalize_absolute(&expanded);
        if expanded == Path::new("/") {
            return Err(refuse(
                "names /, the whole host filesystem; name the folder the capsule needs".to_string(),
            ));
        }
        if let Some(earlier) = declared.iter().find(|other| other.expanded == expanded) {
            return Err(refuse(format!(
                "names the same directory as host_paths[{}]",
                earlier.index
            )));
        }
        declared.push(DeclaredHostPath {
            index,
            declared: path.to_string(),
            expanded,
            access,
        });
    }
    Ok(declared)
}

/// `path` made absolute: `~` and `~/…` against `home`; anything else must already be absolute.
fn expand_home(path: &str, home: Option<&Path>) -> Result<PathBuf, String> {
    let rest = if path == "~" {
        Some("")
    } else {
        path.strip_prefix("~/")
    };
    if let Some(rest) = rest {
        let home = home
            .filter(|home| home.is_absolute())
            .ok_or_else(|| "starts with ~, and HOME is not set to an absolute path".to_string())?;
        return Ok(home.join(rest));
    }
    if path.starts_with('~') {
        return Err(
            "is relative: ~user is not expanded; write an absolute path or ~/…".to_string(),
        );
    }
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        return Err(
            "is relative; write an absolute path or ~/… (environment variables are not expanded)"
                .to_string(),
        );
    }
    Ok(path)
}

/// `path` folded lexically, then canonicalized through its nearest existing ancestor: for a
/// directory that may not exist yet, such as a workdir root before its first run. Folded first so
/// a `..` beneath the missing part cannot defeat a `starts_with` comparison.
fn resolve_through_existing_ancestor(path: &Path) -> PathBuf {
    crate::protected_paths::canonicalize_existing_prefix(
        &crate::protected_paths::normalize_absolute(path),
    )
}

/// What [`resolve_host_paths`] checks each entry against.
#[derive(Debug, Clone, Copy)]
pub struct HostPathContext<'a> {
    /// The `HOME` `mur` runs with: where the credential locations are looked for.
    pub home: Option<&'a Path>,
    /// Murmur's own home, `~/.murmur` (`murmur_home::home_path()`): a `read-write` entry over it
    /// is refused.
    pub murmur_home: Option<&'a Path>,
    /// The session's work folder root: an entry inside it is refused.
    pub workdir: &'a Path,
}

/// Resolves every declared entry on this host and checks it, in config order.
///
/// Refuses an entry that does not exist or is not a directory, that resolves to `/`, that is or
/// lies under a system path the runtime replaces, a `read-write` entry that is, lies under or
/// contains murmur's own home, an entry inside the work folder, and an entry resolving to a
/// directory an earlier one already names. Credential locations are found but never refuse.
pub fn resolve_host_paths(
    declared: &[DeclaredHostPath],
    context: &HostPathContext<'_>,
) -> Result<Vec<HostPath>, HostPathRefusal> {
    let system = system_paths();
    let murmur_home = context.murmur_home.map(resolve_through_existing_ancestor);
    let workdir = resolve_through_existing_ancestor(context.workdir);
    let mut resolved: Vec<HostPath> = Vec::with_capacity(declared.len());
    let mut indices: Vec<usize> = Vec::with_capacity(declared.len());
    for entry in declared {
        let refuse = |resolved: Option<PathBuf>, reason: String| HostPathRefusal {
            index: entry.index,
            declared: entry.declared.clone(),
            resolved,
            reason,
        };
        let path = match std::fs::canonicalize(&entry.expanded) {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(refuse(
                    None,
                    format!("{} does not exist", entry.expanded.display()),
                ))
            }
            Err(error) => {
                return Err(refuse(
                    None,
                    format!("{} cannot be resolved: {error}", entry.expanded.display()),
                ))
            }
        };
        let is_dir = std::fs::metadata(&path).map(|meta| meta.is_dir());
        if !matches!(is_dir, Ok(true)) {
            return Err(refuse(
                Some(path),
                "is not a directory; host_paths names folders only".to_string(),
            ));
        }
        if path == Path::new("/") {
            return Err(refuse(
                Some(path),
                "resolves to /, the whole host filesystem".to_string(),
            ));
        }
        if let Some(system_path) = system.iter().find(|system| path.starts_with(system)) {
            return Err(refuse(
                Some(path.clone()),
                format!(
                    "lies under {}, which the runtime replaces with its own copy in a composed \
                     root (and a private /tmp replaces /tmp), so the entry would not reach the \
                     host's folder on every tier",
                    system_path.display()
                ),
            ));
        }
        if entry.access == HostPathAccess::ReadWrite {
            if let Some(murmur_home) = murmur_home.as_deref() {
                if path.starts_with(murmur_home) || murmur_home.starts_with(&path) {
                    return Err(refuse(
                        Some(path.clone()),
                        format!(
                            "is read-write over murmur's own home {}, which holds this config \
                             file and every session's records; a capsule that could write it \
                             would widen what the next capsule gets. Use `access: read`, or \
                             name a folder that does not contain it",
                            murmur_home.display()
                        ),
                    ));
                }
            }
        }
        if let Some(murmur_home) = murmur_home.as_deref() {
            let keys = crate::record_seal::keys_dir(murmur_home);
            if path.starts_with(&keys) || keys.starts_with(&path) {
                return Err(refuse(
                    Some(path.clone()),
                    format!(
                        "is or contains {}, which holds the record-seal key that signs every \
                         session's records; no capsule may reach it, at any access. Name a \
                         folder that does not contain it, such as {}",
                        keys.display(),
                        murmur_home
                            .join(crate::records_dir::RECORDS_DIR_NAME)
                            .display()
                    ),
                ));
            }
        }
        if path.starts_with(&workdir) {
            return Err(refuse(
                Some(path.clone()),
                format!(
                    "lies inside the work folder {}, which the capsule already reaches read-write",
                    workdir.display()
                ),
            ));
        }
        if let Some(position) = resolved.iter().position(|other| other.path == path) {
            return Err(refuse(
                Some(path),
                format!(
                    "resolves to the same directory as host_paths[{}]",
                    indices[position]
                ),
            ));
        }
        let credential_locations = context
            .home
            .map(|home| credential_locations(&path, home))
            .unwrap_or_default();
        indices.push(entry.index);
        resolved.push(HostPath {
            declared: entry.declared.clone(),
            path,
            access: entry.access,
            credential_locations,
        });
    }
    Ok(resolved)
}

/// Every [`CREDENTIAL_LOCATIONS`] entry that exists under `home` and that `path` is, lies under,
/// or contains, spelled `~/<location>`.
#[must_use]
pub fn credential_locations(path: &Path, home: &Path) -> Vec<String> {
    let home = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    CREDENTIAL_LOCATIONS
        .iter()
        .filter(|location| {
            let lexical = home.join(location.path);
            if std::fs::symlink_metadata(&lexical).is_err() {
                return false;
            }
            let canonical = std::fs::canonicalize(&lexical).unwrap_or_else(|_| lexical.clone());
            [lexical, canonical]
                .iter()
                .any(|location| path.starts_with(location) || location.starts_with(path))
        })
        .map(|location| format!("~/{}", location.path))
        .collect()
}

/// The `W-SEC-035` line for an entry reaching a credential location, or `None` when it reaches
/// none. Shared by `mur run` and `mur doctor` so the two cannot word it differently.
#[must_use]
pub fn credential_warning(host_path: &HostPath) -> Option<String> {
    use murmur_artifact::{security_warning_link, W_SEC_035};
    let locations = host_path.credential_location()?;
    let verb = match host_path.access {
        HostPathAccess::Read => "read",
        HostPathAccess::ReadWrite => "read and change",
    };
    Some(format!(
        "warning[{W_SEC_035}]: host_paths entry {} ({}) reaches the credential location {}, so \
         every scoped or sealed capsule on this host can {verb} it ({})",
        host_path.declared,
        host_path.access,
        locations,
        security_warning_link(W_SEC_035)
    ))
}

/// A directory the session may run something out of: the directory of an executable file its
/// Landlock grants name, or an executable directory grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecuteRoot {
    pub dir: PathBuf,
    /// The executable file or directory grant that makes `dir` an execute root.
    pub because: PathBuf,
}

/// Checks the `read-write` entries against what the session may run, under `mechanism`.
///
/// On both Landlock mechanisms a `read-write` entry that is or lies under an execute root is
/// refused. On `landlock` a `read-write` entry that contains one is refused too: nothing keeps
/// the capsule from rewriting a binary it may run in place. On `bind` such an entry is allowed,
/// and the execute roots inside it are returned, each to be bound read-only on top of it (a
/// required bind). Below both, nothing is scoped and nothing is checked.
pub fn check_execute_roots(
    host_paths: &[HostPath],
    roots: &[ExecuteRoot],
    mechanism: HostPathMechanism,
) -> Result<Vec<PathBuf>, HostPathRefusal> {
    if mechanism == HostPathMechanism::None {
        return Ok(Vec::new());
    }
    let mut protected: Vec<PathBuf> = Vec::new();
    for (index, host_path) in host_paths.iter().enumerate() {
        if host_path.access != HostPathAccess::ReadWrite {
            continue;
        }
        for root in roots {
            let refuse = |reason: String| HostPathRefusal {
                index,
                declared: host_path.declared.clone(),
                resolved: Some(host_path.path.clone()),
                reason,
            };
            if host_path.path.starts_with(&root.dir) {
                return Err(refuse(format!(
                    "is read-write inside {}, a folder this session may run {} from; name a \
                     folder outside it, or make the entry `access: read`",
                    root.dir.display(),
                    root.because.display()
                )));
            }
            if root.dir.starts_with(&host_path.path) {
                if mechanism == HostPathMechanism::Landlock {
                    return Err(refuse(format!(
                        "is read-write over {}, a folder this session may run {} from, so the \
                         capsule could rewrite what it runs; at scoped nothing prevents that. \
                         Name a folder that does not contain it, make the entry `access: read`, \
                         or declare containment: sealed, which binds it read-only",
                        root.dir.display(),
                        root.because.display()
                    )));
                }
                if !protected.contains(&root.dir) {
                    protected.push(root.dir.clone());
                }
            }
        }
    }
    Ok(protected)
}

/// [`check_execute_roots`] for `policy`'s own host paths against the execute roots its grants
/// resolve to on this host. `stage_session` calls it on the applied tier, and
/// `mur run --explain-scope` on the tier its report names, so both refuse the same entry.
pub fn check_policy_execute_roots(
    policy: &crate::types::CapabilityPolicy,
    mechanism: HostPathMechanism,
) -> Result<(), HostPathRefusal> {
    if mechanism == HostPathMechanism::None
        || !policy
            .host_paths
            .iter()
            .any(|host_path| host_path.access == HostPathAccess::ReadWrite)
    {
        return Ok(());
    }
    check_execute_roots(
        &policy.host_paths,
        &crate::sandbox::host_path_execute_roots(policy),
        mechanism,
    )
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, access: &str) -> HostPathEntry {
        HostPathEntry {
            path: Some(path.to_string()),
            access: Some(access.to_string()),
        }
    }

    fn refusal(entries: &[HostPathEntry]) -> HostPathRefusal {
        declare_host_paths(entries, Some(Path::new("/home/op"))).unwrap_err()
    }

    /// A scratch directory outside `/tmp`, which a host path may not be under: the workspace's
    /// `target/` when there is one, else the crate directory.
    fn scratch() -> tempfile::TempDir {
        let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target");
        let base = if target.is_dir() {
            target
        } else {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        };
        tempfile::tempdir_in(base).unwrap()
    }

    #[test]
    fn host_paths_two_entries_parse_and_tilde_expands_against_home() {
        let declared = declare_host_paths(
            &[entry("~", "read"), entry("~/x/./y/../z", "read-write")],
            Some(Path::new("/home/op")),
        )
        .unwrap();
        assert_eq!(declared[0].expanded, PathBuf::from("/home/op"));
        assert_eq!(declared[0].access, HostPathAccess::Read);
        assert_eq!(declared[1].expanded, PathBuf::from("/home/op/x/z"));
        assert_eq!(declared[1].access, HostPathAccess::ReadWrite);
        assert_eq!(declared[1].declared, "~/x/./y/../z");
        assert_eq!(declared[1].index, 1);
    }

    #[test]
    fn host_paths_shape_refusals_name_the_index_and_the_path_as_written() {
        let cases: Vec<(Vec<HostPathEntry>, &str, &str)> = vec![
            (
                vec![entry("relative/dir", "read")],
                "host_paths[0] (relative/dir)",
                "is relative",
            ),
            (
                vec![entry("~user/x", "read")],
                "host_paths[0] (~user/x)",
                "~user is not expanded",
            ),
            (
                vec![entry("$HOME/x", "read")],
                "host_paths[0] ($HOME/x)",
                "is relative",
            ),
            (vec![entry("/", "read")], "host_paths[0] (/)", "names /"),
            (
                vec![entry("/home/..", "read")],
                "host_paths[0] (/home/..)",
                "names /",
            ),
            (
                vec![HostPathEntry {
                    path: Some("/srv".to_string()),
                    access: None,
                }],
                "host_paths[0] (/srv)",
                "access is required",
            ),
            (
                vec![entry("/srv", "write")],
                "host_paths[0] (/srv)",
                "access 'write'",
            ),
            (
                vec![entry("/srv", "read"), entry("/srv/", "read-write")],
                "host_paths[1] (/srv/)",
                "same directory as host_paths[0]",
            ),
            (
                vec![HostPathEntry {
                    path: None,
                    access: Some("read".to_string()),
                }],
                "host_paths[0] (<none>)",
                "no path",
            ),
        ];
        for (entries, head, reason) in cases {
            let text = refusal(&entries).to_string();
            assert!(text.starts_with(head), "{text}");
            assert!(text.contains(reason), "{text}");
        }
    }

    #[test]
    fn host_paths_tilde_needs_an_absolute_home() {
        let error = declare_host_paths(&[entry("~/x", "read")], None).unwrap_err();
        assert!(error.reason.contains("HOME"), "{error}");
    }

    fn resolve_one(
        path: &Path,
        access: HostPathAccess,
        murmur_home: Option<&Path>,
        workdir: &Path,
    ) -> Result<Vec<HostPath>, HostPathRefusal> {
        let declared = vec![DeclaredHostPath {
            index: 0,
            declared: path.display().to_string(),
            expanded: path.to_path_buf(),
            access,
        }];
        resolve_host_paths(
            &declared,
            &HostPathContext {
                home: None,
                murmur_home,
                workdir,
            },
        )
    }

    #[test]
    fn host_paths_launch_refusals_name_the_entry_its_resolution_and_the_reason() {
        let dir = scratch();
        let root = dir.path().canonicalize().unwrap();
        let workdir = root.join("project/workdir");
        std::fs::create_dir_all(workdir.join("inner")).unwrap();
        let home = root.join("home");
        let murmur_home = home.join(".murmur");
        std::fs::create_dir_all(murmur_home.join("records")).unwrap();
        let file = root.join("file.txt");
        std::fs::write(&file, "x").unwrap();
        let to_root = root.join("to-root");
        std::os::unix::fs::symlink("/", &to_root).unwrap();
        let tmp_dir = tempfile::tempdir_in("/tmp").unwrap();

        let read = HostPathAccess::Read;
        let read_write = HostPathAccess::ReadWrite;
        let cases: Vec<(PathBuf, HostPathAccess, &str)> = vec![
            (root.join("missing"), read, "does not exist"),
            (to_root.clone(), read, "resolves to /"),
            (file.clone(), read, "is not a directory"),
            (PathBuf::from("/usr/share"), read, "lies under /usr"),
            (PathBuf::from("/etc"), read, "lies under /etc"),
            (PathBuf::from("/proc"), read, "lies under /proc"),
            (tmp_dir.path().to_path_buf(), read, "/tmp"),
            (murmur_home.clone(), read_write, "murmur's own home"),
            (murmur_home.join("records"), read_write, "murmur's own home"),
            (home.clone(), read_write, "murmur's own home"),
            (workdir.join("inner"), read, "inside the work folder"),
            (workdir.clone(), read, "inside the work folder"),
        ];
        for (path, access, reason) in cases {
            let error = resolve_one(&path, access, Some(&murmur_home), &workdir)
                .expect_err(&format!("{} should be refused", path.display()));
            let text = error.to_string();
            assert!(
                text.starts_with(&format!("host_paths[0] ({}", path.display())),
                "{text}"
            );
            assert!(text.contains(reason), "{reason}: {text}");
        }
        // A symlink names its resolution as well as what was written.
        let error = resolve_one(&to_root, read, None, &workdir).unwrap_err();
        assert_eq!(error.resolved.as_deref(), Some(Path::new("/")));
        assert!(error.to_string().contains("resolved to /"), "{error}");
    }

    #[test]
    fn host_paths_murmur_home_read_and_a_symlinked_entry_resolve() {
        let dir = scratch();
        let root = dir.path().canonicalize().unwrap();
        let murmur_home = root.join("home/.murmur");
        std::fs::create_dir_all(&murmur_home).unwrap();
        let target = root.join("real");
        std::fs::create_dir(&target).unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let workdir = root.join("project/workdir");

        std::fs::create_dir_all(murmur_home.join("records")).unwrap();
        let resolved = resolve_one(
            &murmur_home.join("records"),
            HostPathAccess::Read,
            Some(&murmur_home),
            &workdir,
        )
        .unwrap();
        assert_eq!(resolved[0].path, murmur_home.join("records"));

        let resolved = resolve_one(&link, HostPathAccess::ReadWrite, None, &workdir).unwrap();
        assert_eq!(resolved[0].path, target);
        assert_eq!(resolved[0].declared, link.display().to_string());
    }

    /// The record-seal key's folder, and every folder containing it, is refused at any access;
    /// a sibling under murmur's home at `read` is not.
    #[test]
    fn host_paths_an_entry_reaching_the_record_seal_key_is_refused_at_any_access() {
        let dir = scratch();
        let root = dir.path().canonicalize().unwrap();
        let home = root.join("home");
        let murmur_home = home.join(".murmur");
        std::fs::create_dir_all(murmur_home.join("keys")).unwrap();
        std::fs::create_dir_all(murmur_home.join("records")).unwrap();
        let workdir = root.join("project/workdir");
        for (path, access) in [
            (murmur_home.clone(), HostPathAccess::Read),
            (murmur_home.join("keys"), HostPathAccess::Read),
            (home.clone(), HostPathAccess::Read),
        ] {
            let error = resolve_one(&path, access, Some(&murmur_home), &workdir)
                .expect_err(&format!("{} should be refused", path.display()));
            let text = error.to_string();
            assert!(text.contains("record-seal key"), "{text}");
            assert!(
                text.contains(&murmur_home.join("keys").display().to_string()),
                "{text}"
            );
        }
        // `read-write` beneath murmur's home is refused by the rule that came first.
        assert!(resolve_one(
            &murmur_home.join("keys"),
            HostPathAccess::ReadWrite,
            Some(&murmur_home),
            &workdir
        )
        .is_err());
        let resolved = resolve_one(
            &murmur_home.join("records"),
            HostPathAccess::Read,
            Some(&murmur_home),
            &workdir,
        )
        .unwrap();
        assert_eq!(resolved[0].path, murmur_home.join("records"));
    }

    #[test]
    fn host_paths_two_entries_resolving_to_one_directory_are_refused() {
        let dir = scratch();
        let root = dir.path().canonicalize().unwrap();
        let target = root.join("real");
        std::fs::create_dir(&target).unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let declared = declare_host_paths(
            &[
                entry(&target.display().to_string(), "read"),
                entry(&link.display().to_string(), "read"),
            ],
            None,
        )
        .unwrap();
        let error = resolve_host_paths(
            &declared,
            &HostPathContext {
                home: None,
                murmur_home: None,
                workdir: &root.join("workdir"),
            },
        )
        .unwrap_err();
        assert_eq!(error.index, 1);
        assert!(error.reason.contains("host_paths[0]"), "{error}");
    }

    #[test]
    fn host_paths_credential_locations_warn_when_is_under_or_contains() {
        let dir = scratch();
        let home = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(home.join(".ssh/keys")).unwrap();
        std::fs::create_dir_all(home.join(".cargo")).unwrap();
        std::fs::write(home.join(".cargo/credentials.toml"), "").unwrap();
        std::fs::create_dir_all(home.join("plain")).unwrap();

        // is
        assert_eq!(credential_locations(&home.join(".ssh"), &home), ["~/.ssh"]);
        // lies under
        assert_eq!(
            credential_locations(&home.join(".ssh/keys"), &home),
            ["~/.ssh"]
        );
        // contains
        assert_eq!(
            credential_locations(&home.join(".cargo"), &home),
            ["~/.cargo/credentials.toml"]
        );
        assert_eq!(
            credential_locations(&home, &home),
            ["~/.ssh", "~/.cargo/credentials.toml"]
        );
        // none
        assert!(credential_locations(&home.join("plain"), &home).is_empty());
        // A location absent from this host is never named: no `.aws` here.
        assert!(!credential_locations(&home, &home).contains(&"~/.aws".to_string()));

        let host_path = HostPath {
            declared: "~/.ssh".to_string(),
            path: home.join(".ssh"),
            access: HostPathAccess::Read,
            credential_locations: vec!["~/.ssh".to_string()],
        };
        let warning = credential_warning(&host_path).unwrap();
        assert!(warning.starts_with("warning[W-SEC-035]: host_paths entry ~/.ssh (read)"));
        assert!(warning.contains("diagnostics/#w-sec-035"), "{warning}");
        let silent = HostPath {
            credential_locations: Vec::new(),
            ..host_path
        };
        assert!(credential_warning(&silent).is_none());
    }

    #[test]
    fn host_paths_credential_backstops_are_credential_patterns() {
        for location in CREDENTIAL_LOCATIONS {
            for pattern in location.backstop {
                assert!(
                    crate::shell::CREDENTIAL_ENV_PATTERNS.contains(pattern),
                    "{pattern} for ~/{} is not a credential backstop pattern",
                    location.path
                );
            }
        }
    }

    fn host_path(path: &str, access: HostPathAccess) -> HostPath {
        HostPath {
            declared: path.to_string(),
            path: PathBuf::from(path),
            access,
            credential_locations: Vec::new(),
        }
    }

    fn root(dir: &str, because: &str) -> ExecuteRoot {
        ExecuteRoot {
            dir: PathBuf::from(dir),
            because: PathBuf::from(because),
        }
    }

    #[test]
    fn host_paths_execute_root_rules_per_mechanism() {
        let roots = [root("/opt/tools/bin", "/opt/tools/bin/tool")];
        let rw = HostPathAccess::ReadWrite;
        // Containing an execute root: refused on landlock, protected on bind.
        let tools = [host_path("/opt/tools", rw)];
        let error = check_execute_roots(&tools, &roots, HostPathMechanism::Landlock).unwrap_err();
        assert!(error.to_string().contains("/opt/tools/bin/tool"), "{error}");
        assert_eq!(
            check_execute_roots(&tools, &roots, HostPathMechanism::Bind).unwrap(),
            [PathBuf::from("/opt/tools/bin")]
        );
        // Is or lies under an execute root: refused on both.
        for path in ["/opt/tools/bin", "/opt/tools/bin/sub"] {
            let entry = [host_path(path, rw)];
            for mechanism in [HostPathMechanism::Landlock, HostPathMechanism::Bind] {
                let error = check_execute_roots(&entry, &roots, mechanism).unwrap_err();
                assert!(error.reason.contains("/opt/tools/bin"), "{error}");
            }
        }
        // Read entries and weak tiers are never checked.
        let read = [host_path("/opt/tools/bin", HostPathAccess::Read)];
        assert!(
            check_execute_roots(&read, &roots, HostPathMechanism::Landlock)
                .unwrap()
                .is_empty()
        );
        assert!(check_execute_roots(&tools, &roots, HostPathMechanism::None)
            .unwrap()
            .is_empty());
        // A disjoint entry is untouched.
        let other = [host_path("/srv/data", rw)];
        assert!(
            check_execute_roots(&other, &roots, HostPathMechanism::Landlock)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn host_paths_reports_carry_every_key_in_config_order() {
        let paths = [
            HostPath {
                declared: "~/.cargo".to_string(),
                path: PathBuf::from("/home/op/.cargo"),
                access: HostPathAccess::ReadWrite,
                credential_locations: vec!["~/.cargo/credentials.toml".to_string()],
            },
            host_path("/srv/data", HostPathAccess::Read),
        ];
        let reports = host_path_reports(&paths, HostPathMechanism::Landlock);
        let json = serde_json::to_value(&reports).unwrap();
        assert_eq!(
            json,
            serde_json::json!([
                {
                    "path": "/home/op/.cargo",
                    "declared": "~/.cargo",
                    "access": "read-write",
                    "mechanism": "landlock",
                    "credential_location": "~/.cargo/credentials.toml"
                },
                {
                    "path": "/srv/data",
                    "declared": "/srv/data",
                    "access": "read",
                    "mechanism": "landlock",
                    "credential_location": null
                }
            ])
        );
        assert_eq!(
            reports[0].render(),
            "~/.cargo → /home/op/.cargo (read-write), reaches credential location \
             ~/.cargo/credentials.toml"
        );
        for (tier, mechanism) in [
            ("mountns+pivot_root+landlock+seccomp", "bind"),
            ("landlock+seccomp", "landlock"),
            ("seccomp-only", "none"),
            ("none", "none"),
        ] {
            assert_eq!(
                HostPathMechanism::for_enforcement_tier(tier).wire_name(),
                mechanism
            );
        }
    }
}
