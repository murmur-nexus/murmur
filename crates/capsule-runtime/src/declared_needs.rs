//! What the host programs a capsule's tools run need from the host, as each tool's own
//! `murmur.yaml` declares it ([`murmur_artifact::parse_program_needs`]), resolved against this
//! host at staging.
//!
//! A tool declares needs per program, by soname, by file name beside a declared library, relative
//! to the program's own folder, by a named system location, or by a name under `/etc`, and never
//! by host path. A need reaches a session only when the capsule makes both declarations: its
//! `artifacts:` pins the tool with an `operator` lock origin (or there is no lock), and its
//! `capabilities.shell.allow` names the program verbatim. A runtime pin and an artifact added
//! mid-session contribute nothing: the enforcement is resolved once, at launch.
//!
//! | Form | Resolves to | Grant |
//! |---|---|---|
//! | `library: S` | `S` as the loader resolves it for each binary the program resolves to on `PATH`, then that library's `DT_NEEDED` closure | each file read |
//! | `file: F, beside: S` | `F` in the folder `S` resolved to, a regular file | read |
//! | `program_folder: R` | `R` under the folder of each binary the program resolves to | read, and list for a folder |
//! | `system: fonts` | each existing [`FONT_LOCATIONS`] entry | read and list |
//! | `system: ca_bundle` | the trust-store locations [`crate::trust_store`] checks, with no network gate | as the trust store |
//! | `etc: N` | `/etc/N` | read, and list for a folder |
//!
//! Every path is canonical, and no grant is executable ([`crate::sandbox::resolve_declared_need_grants`]).
//! A resolved path that is `/`, or is, lies beneath or holds `/proc`, `/sys`, `/dev`, the home
//! folder, a tree the capsule's commands can write or an `/etc` credential location
//! ([`murmur_artifact::ETC_CREDENTIAL_LOCATIONS`]) is reported missing instead. A missing need
//! never refuses a launch.
//!
//! The grants join the session's one command scope, as the `DT_NEEDED` closure of a granted
//! program does: every command of the session, and every program a WASM tool runs, can read them.
//! "For this program" is the gate and the attribution, not a per-executable rule.
//!
//! Off Linux nothing is derived: commands' file reads are not path-scoped there.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use murmur_artifact::{DeclaredNeed, LockOrigin, ProgramNeeds};
#[cfg(target_os = "linux")]
use murmur_artifact::{NeedForm, SystemLocation};

/// The `missing` reason of a need that resolves to nothing on this host.
pub const NOT_FOUND: &str = "not found on this host";

/// The `missing` reason of a need relative to a program `PATH` resolves to no binary for.
pub const PROGRAM_NOT_ON_PATH: &str = "program not found on PATH";

/// The host locations `system: fonts` resolves to, each one this host has.
pub const FONT_LOCATIONS: &[&str] = &[
    "/etc/fonts",
    "/usr/share/fonts",
    "/usr/local/share/fonts",
    "/usr/share/fontconfig",
    "/var/cache/fontconfig",
];

/// The places os-release is read from, in order; the first that reads wins.
const OS_RELEASE_PATHS: &[&str] = &["/etc/os-release", "/usr/lib/os-release"];

/// One staged tool's `programs:` block, with the lock origin of its pin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredToolNeeds {
    /// The artifact as the capsule's manifest names it, scope included.
    pub artifact: String,
    pub origin: LockOrigin,
    pub programs: Vec<ProgramNeeds>,
}

/// What one artifact's needs for one program resolved to on this host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProgramNeeds {
    pub artifact: String,
    pub program: String,
    /// Each need with at least one grantable path, in declaration order.
    pub granted: Vec<ResolvedNeed>,
    /// Each need with a path that was not granted, or with none at all, in declaration order. A
    /// need some of whose paths were granted and some moved here is in both lists.
    pub missing: Vec<MissingNeed>,
}

/// A need and the host paths it is granted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedNeed {
    pub need: DeclaredNeed,
    /// Canonical, sorted and distinct.
    pub paths: Vec<NeedPath>,
}

/// One granted path.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct NeedPath {
    pub path: PathBuf,
    /// Whether `path` is a directory, which makes its grant listable.
    pub directory: bool,
}

impl NeedPath {
    /// `<path>`, or `<path> (list_dir)` for a directory: how every report spells a grant.
    #[must_use]
    pub fn render(&self) -> String {
        let listable = if self.directory { " (list_dir)" } else { "" };
        format!("{}{listable}", self.path.display())
    }
}

/// A need, or part of one, that is not granted, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingNeed {
    pub need: DeclaredNeed,
    /// [`NOT_FOUND`], [`PROGRAM_NOT_ON_PATH`], or the location a resolved path may not reach.
    pub reason: String,
}

/// The needs `tools` declare for the programs `shell_allow` grants, resolved against this host.
///
/// Only a tool whose `origin` is [`LockOrigin::Operator`] contributes, and only for a program key
/// equal to an entry of `shell_allow`. One element per (artifact, program) with at least one
/// need, tools in the order given and programs in declaration order. `capsule_writable` are the
/// trees the capsule's commands can write ([`crate::interpreter_runtime::capsule_writable_roots`]).
///
/// Reads the filesystem only, runs nothing, and fails nothing. Off Linux this returns nothing.
pub fn derive_declared_needs(
    tools: &[DeclaredToolNeeds],
    shell_allow: &[String],
    capsule_writable: &[&Path],
) -> Vec<ResolvedProgramNeeds> {
    #[cfg(target_os = "linux")]
    {
        let host = NeedHost {
            path_dirs: std::env::var_os("PATH")
                .map(|path| std::env::split_paths(&path).collect())
                .unwrap_or_default(),
            search_dirs: crate::sandbox::DEFAULT_LIBRARY_SEARCH_DIRS
                .iter()
                .map(PathBuf::from)
                .collect(),
            font_locations: FONT_LOCATIONS.iter().map(PathBuf::from).collect(),
            ca_candidates: crate::trust_store::TRUST_STORE_CANDIDATES,
            etc_root: PathBuf::from("/etc"),
            home: std::env::var_os("HOME")
                .filter(|home| !home.is_empty())
                .map(PathBuf::from),
            capsule_writable: capsule_writable
                .iter()
                .map(|root| root.to_path_buf())
                .collect(),
        };
        derive_in(tools, shell_allow, &host)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (tools, shell_allow, capsule_writable);
        Vec::new()
    }
}

/// Everything [`derive_in`] reads from the host, injected.
#[cfg(target_os = "linux")]
pub(crate) struct NeedHost<'a> {
    /// `PATH`, split.
    pub(crate) path_dirs: Vec<PathBuf>,
    /// The soname fallback directories ([`crate::sandbox::DEFAULT_LIBRARY_SEARCH_DIRS`]).
    pub(crate) search_dirs: Vec<PathBuf>,
    /// [`FONT_LOCATIONS`].
    pub(crate) font_locations: Vec<PathBuf>,
    /// [`crate::trust_store::TRUST_STORE_CANDIDATES`].
    pub(crate) ca_candidates: &'a [crate::trust_store::TrustStoreCandidate<'a>],
    /// `/etc`: where an `etc:` need and the credential table are rooted.
    pub(crate) etc_root: PathBuf,
    pub(crate) home: Option<PathBuf>,
    pub(crate) capsule_writable: Vec<PathBuf>,
}

/// [`derive_declared_needs`] with the host injected.
#[cfg(target_os = "linux")]
pub(crate) fn derive_in(
    tools: &[DeclaredToolNeeds],
    shell_allow: &[String],
    host: &NeedHost<'_>,
) -> Vec<ResolvedProgramNeeds> {
    let unreachable = Unreachable::of(host);
    let mut resolved = Vec::new();
    for tool in tools {
        if tool.origin != LockOrigin::Operator {
            continue;
        }
        for program in &tool.programs {
            if program.needs.is_empty() || !shell_allow.contains(&program.program) {
                continue;
            }
            resolved.push(resolve_program(&tool.artifact, program, host, &unreachable));
        }
    }
    resolved
}

#[cfg(target_os = "linux")]
fn resolve_program(
    artifact: &str,
    program: &ProgramNeeds,
    host: &NeedHost<'_>,
    unreachable: &Unreachable,
) -> ResolvedProgramNeeds {
    let binaries = crate::sandbox::resolve_exec_allowlist_in(
        std::slice::from_ref(&program.program),
        &host.path_dirs,
    );
    let mut granted = Vec::new();
    let mut missing = Vec::new();
    for need in &program.needs {
        let found = match &need.form {
            NeedForm::Library(soname) => {
                let direct = resolve_library(soname, &binaries, host);
                let seeds: Vec<PathBuf> = direct
                    .iter()
                    .filter(|path| unreachable.reason(path).is_none())
                    .cloned()
                    .collect();
                let Ok(closure) = crate::sandbox::walk_dependency_closure(
                    &seeds,
                    &host.search_dirs,
                    &unreachable.roots,
                    |path| -> Result<_, std::convert::Infallible> {
                        Ok(crate::sandbox::read_elf_dependencies(path))
                    },
                );
                // The libraries `unreachable` refused as seeds, so they are reported below.
                let refused = direct.into_iter().filter(|path| !seeds.contains(path));
                Ok(closure
                    .into_iter()
                    .chain(refused)
                    .map(|path| NeedPath {
                        path,
                        directory: false,
                    })
                    .collect::<Vec<_>>())
            }
            NeedForm::File { file, beside } => Ok(resolve_library(beside, &binaries, host)
                .iter()
                .filter_map(|library| library.parent())
                .filter_map(|dir| existing(&dir.join(file)))
                .filter(|path| !path.directory)
                .collect()),
            NeedForm::ProgramFolder(relative) => {
                if binaries.is_empty() {
                    Err(PROGRAM_NOT_ON_PATH.to_string())
                } else {
                    Ok(binaries
                        .iter()
                        .filter_map(|binary| binary.parent())
                        .filter_map(|dir| existing(&dir.join(relative)))
                        .collect())
                }
            }
            NeedForm::System(SystemLocation::Fonts) => Ok(host
                .font_locations
                .iter()
                .filter_map(|location| existing(location))
                .collect()),
            NeedForm::System(SystemLocation::CaBundle) => {
                let writable: Vec<&Path> =
                    host.capsule_writable.iter().map(PathBuf::as_path).collect();
                Ok(crate::trust_store::select_in(host.ca_candidates, &writable)
                    .into_iter()
                    .map(|entry| NeedPath {
                        path: entry.path,
                        directory: entry.directory,
                    })
                    .collect())
            }
            NeedForm::Etc(name) => Ok(existing(&host.etc_root.join(name)).into_iter().collect()),
        };
        let paths = match found {
            Ok(paths) if !paths.is_empty() => paths,
            Ok(_) => {
                missing.push(MissingNeed {
                    need: need.clone(),
                    reason: NOT_FOUND.to_string(),
                });
                continue;
            }
            Err(reason) => {
                missing.push(MissingNeed {
                    need: need.clone(),
                    reason,
                });
                continue;
            }
        };
        let mut kept: BTreeSet<NeedPath> = BTreeSet::new();
        let mut refusal: Option<String> = None;
        for path in paths {
            match unreachable.reason(&path.path) {
                None => {
                    kept.insert(path);
                }
                Some(reason) => {
                    refusal.get_or_insert(reason);
                }
            }
        }
        if !kept.is_empty() {
            granted.push(ResolvedNeed {
                need: need.clone(),
                paths: kept.into_iter().collect(),
            });
        }
        if let Some(reason) = refusal {
            missing.push(MissingNeed {
                need: need.clone(),
                reason,
            });
        }
    }
    ResolvedProgramNeeds {
        artifact: artifact.to_string(),
        program: program.program.clone(),
        granted,
        missing,
    }
}

/// Where `soname` resolves for the program: for each of its `binaries`, from that binary's folder
/// and `DT_RUNPATH`, then the search directories; from the search directories alone when it has
/// none. Canonical and distinct.
#[cfg(target_os = "linux")]
fn resolve_library(soname: &str, binaries: &[PathBuf], host: &NeedHost<'_>) -> Vec<PathBuf> {
    let mut found = BTreeSet::new();
    if binaries.is_empty() {
        found.extend(crate::sandbox::resolve_soname(
            soname,
            Path::new("/"),
            &[],
            &host.search_dirs,
        ));
    }
    for binary in binaries {
        let runpaths = crate::sandbox::read_elf_dependencies(binary)
            .map(|deps| deps.runpaths)
            .unwrap_or_default();
        let origin = binary.parent().unwrap_or_else(|| Path::new("/"));
        found.extend(crate::sandbox::resolve_soname(
            soname,
            origin,
            &runpaths,
            &host.search_dirs,
        ));
    }
    found.into_iter().collect()
}

/// `path` canonicalized, when it is a directory or a regular file.
#[cfg(target_os = "linux")]
fn existing(path: &Path) -> Option<NeedPath> {
    let canonical = std::fs::canonicalize(path).ok()?;
    let metadata = std::fs::metadata(&canonical).ok()?;
    (metadata.is_dir() || metadata.is_file()).then(|| NeedPath {
        path: canonical,
        directory: metadata.is_dir(),
    })
}

/// The locations no need may reach, each with how a reason names it.
#[cfg(target_os = "linux")]
struct Unreachable {
    /// Canonical; a path equal to, beneath or holding one is refused.
    roots: Vec<PathBuf>,
    labels: Vec<String>,
}

#[cfg(target_os = "linux")]
impl Unreachable {
    fn of(host: &NeedHost<'_>) -> Self {
        let mut unreachable = Self {
            roots: Vec::new(),
            labels: Vec::new(),
        };
        let mut add = |path: &Path, label: &str| {
            for root in [
                path.to_path_buf(),
                crate::protected_paths::canonicalize_existing_prefix(path),
            ] {
                if !unreachable.roots.contains(&root) {
                    unreachable
                        .labels
                        .push(format!("{label} {}", root.display()));
                    unreachable.roots.push(root);
                }
            }
        };
        for kernel in ["/proc", "/sys", "/dev"] {
            add(Path::new(kernel), "the kernel's");
        }
        if let Some(home) = &host.home {
            add(home, "the home folder");
        }
        for root in &host.capsule_writable {
            add(root, "the capsule-writable tree");
        }
        for location in murmur_artifact::ETC_CREDENTIAL_LOCATIONS {
            add(&host.etc_root.join(location), "the credential location");
        }
        unreachable
    }

    /// Why `path` may not be granted, or `None` when it may.
    fn reason(&self, path: &Path) -> Option<String> {
        if path == Path::new("/") {
            return Some("resolved to /, the filesystem root".to_string());
        }
        self.roots
            .iter()
            .zip(&self.labels)
            .find_map(|(root, label)| {
                let relation = if path == root {
                    "which is"
                } else if path.starts_with(root) {
                    "beneath"
                } else if root.starts_with(path) {
                    "which holds"
                } else {
                    return None;
                };
                Some(format!(
                    "resolved to {}, {relation} {label}, which no need reaches",
                    path.display()
                ))
            })
    }
}

/// This host's distribution IDs, most specific first: os-release's `ID`, then each word of its
/// `ID_LIKE`, read from `/etc/os-release`, else `/usr/lib/os-release`. Empty when neither reads.
#[must_use]
pub fn host_distributions() -> Vec<String> {
    OS_RELEASE_PATHS
        .iter()
        .find_map(|path| std::fs::read_to_string(path).ok())
        .map(|text| distributions_from_os_release(&text))
        .unwrap_or_default()
}

/// [`host_distributions`] from os-release text: `ID`, then each whitespace-separated word of
/// `ID_LIKE`, unquoted, each once.
#[must_use]
pub fn distributions_from_os_release(text: &str) -> Vec<String> {
    let value = |key: &str| {
        text.lines().find_map(|line| {
            let (name, value) = line.trim().split_once('=')?;
            (name.trim() == key).then(|| {
                let value = value.trim();
                value
                    .strip_prefix('"')
                    .and_then(|v| v.strip_suffix('"'))
                    .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                    .unwrap_or(value)
                    .to_string()
            })
        })
    };
    let mut distributions: Vec<String> = Vec::new();
    let id = value("ID");
    let like = value("ID_LIKE");
    for word in id
        .iter()
        .chain(like.iter())
        .flat_map(|value| value.split_whitespace())
    {
        if !distributions.iter().any(|known| known == word) {
            distributions.push(word.to_string());
        }
    }
    distributions
}

/// The package `need` declares for the first of `distributions` it names one for, with that
/// distribution.
#[must_use]
pub fn package_for<'a>(
    need: &'a DeclaredNeed,
    distributions: &'a [String],
) -> Option<(&'a str, &'a str)> {
    distributions.iter().find_map(|distribution| {
        need.packages
            .get(distribution)
            .map(|package| (package.as_str(), distribution.as_str()))
    })
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use murmur_artifact::{parse_program_needs, NeedForm};
    use std::os::unix::fs::symlink;

    /// A scratch host: `bin/fixprog`, `lib/` with a fixture library, fonts and an `/etc`.
    struct Scratch {
        _temp: tempfile::TempDir,
        root: PathBuf,
    }

    impl Scratch {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = std::fs::canonicalize(temp.path()).unwrap();
            for dir in [
                "bin/share",
                "lib",
                "fonts",
                "home/user",
                "workdir",
                "etc/ssl/private",
                "etc/ssl/certs",
                "etc/fonts",
            ] {
                std::fs::create_dir_all(root.join(dir)).unwrap();
            }
            std::fs::copy(host_binary("true"), root.join("bin/fixprog")).unwrap();
            std::fs::copy(host_library("libm.so.6"), root.join("lib/libfix.so.1")).unwrap();
            std::fs::write(root.join("lib/libfix.chk"), b"chk").unwrap();
            std::fs::copy(host_library("libm.so.6"), root.join("lib/libother.so.1")).unwrap();
            std::fs::write(root.join("etc/fonts/fonts.conf"), b"<fontconfig/>").unwrap();
            std::fs::write(root.join("etc/ssl/certs/ca.pem"), b"pem").unwrap();
            Self { _temp: temp, root }
        }

        fn host(&self) -> NeedHost<'static> {
            let mut search_dirs = vec![self.root.join("lib")];
            search_dirs.extend(
                crate::sandbox::DEFAULT_LIBRARY_SEARCH_DIRS
                    .iter()
                    .map(PathBuf::from),
            );
            NeedHost {
                path_dirs: vec![self.root.join("bin")],
                search_dirs,
                font_locations: vec![self.root.join("fonts"), self.root.join("no-fonts")],
                ca_candidates: &[],
                etc_root: self.root.join("etc"),
                home: Some(self.root.join("home/user")),
                capsule_writable: vec![self.root.join("workdir")],
            }
        }
    }

    fn host_binary(name: &str) -> PathBuf {
        ["/bin", "/usr/bin"]
            .iter()
            .map(|dir| Path::new(dir).join(name))
            .find(|path| path.is_file())
            .expect("the host has the binary")
    }

    fn host_library(soname: &str) -> PathBuf {
        let dirs: Vec<PathBuf> = crate::sandbox::DEFAULT_LIBRARY_SEARCH_DIRS
            .iter()
            .map(PathBuf::from)
            .collect();
        crate::sandbox::resolve_soname(soname, Path::new("/"), &[], &dirs)
            .expect("the host has the library")
    }

    fn tool(artifact: &str, origin: LockOrigin, block: &str) -> DeclaredToolNeeds {
        DeclaredToolNeeds {
            artifact: artifact.to_string(),
            origin,
            programs: parse_program_needs(&format!(
                "name: t\nversion: 0.1.0\nruntime: tool\n{block}"
            ))
            .unwrap(),
        }
    }

    fn allow(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    }

    fn granted_paths(resolved: &ResolvedProgramNeeds) -> BTreeSet<PathBuf> {
        resolved
            .granted
            .iter()
            .flat_map(|need| need.paths.iter().map(|path| path.path.clone()))
            .collect()
    }

    const FIXPROG: &str = "programs:
  fixprog:
    needs:
      - library: libfix.so.1
      - file: libfix.chk
        beside: libfix.so.1
      - program_folder: share
      - system: fonts
";

    #[test]
    fn declared_needs_resolve_to_exactly_the_declared_paths_read_only() {
        let scratch = Scratch::new();
        let host = scratch.host();
        let resolved = derive_in(
            &[tool("local/needs-tool", LockOrigin::Operator, FIXPROG)],
            &allow(&["fixprog"]),
            &host,
        );
        assert_eq!(resolved.len(), 1);
        let program = &resolved[0];
        assert_eq!(
            (program.artifact.as_str(), program.program.as_str()),
            ("local/needs-tool", "fixprog")
        );
        assert!(program.missing.is_empty(), "{:?}", program.missing);

        let library = scratch.root.join("lib/libfix.so.1");
        let Ok(closure) = crate::sandbox::walk_dependency_closure(
            std::slice::from_ref(&library),
            &host.search_dirs,
            &[],
            |path| -> Result<_, std::convert::Infallible> {
                Ok(crate::sandbox::read_elf_dependencies(path))
            },
        );
        assert!(closure.len() > 1, "libm's own closure: {closure:?}");
        let mut expected: BTreeSet<PathBuf> = closure.into_iter().collect();
        expected.insert(scratch.root.join("lib/libfix.chk"));
        expected.insert(scratch.root.join("bin/share"));
        expected.insert(scratch.root.join("fonts"));
        assert_eq!(granted_paths(program), expected);
        assert!(!expected.contains(&scratch.root.join("lib/libother.so.1")));
        assert!(!expected.contains(&scratch.root.join("lib")));

        let labels: Vec<String> = program
            .granted
            .iter()
            .map(|need| need.need.form.to_string())
            .collect();
        assert_eq!(
            labels,
            [
                "library libfix.so.1",
                "file libfix.chk beside libfix.so.1",
                "program_folder share",
                "system fonts",
            ]
        );

        let grants = crate::sandbox::resolve_declared_need_grants(&resolved);
        assert_eq!(grants.len(), expected.len());
        for grant in &grants {
            assert!(!grant.executable, "{grant:?}");
            let directory = grant.path == scratch.root.join("bin/share")
                || grant.path == scratch.root.join("fonts");
            assert_eq!(grant.list_dir, directory, "{grant:?}");
        }
    }

    #[test]
    fn declared_needs_an_unreachable_location_is_moved_to_missing() {
        let scratch = Scratch::new();
        let bin = scratch.root.join("bin");
        for (link, target) in [
            ("root_link", PathBuf::from("/")),
            ("proc_link", PathBuf::from("/proc")),
            ("home_link", scratch.root.join("home/user")),
            ("home_parent", scratch.root.join("home")),
            ("writable_link", scratch.root.join("workdir")),
            ("cred_link", scratch.root.join("etc/ssl/private")),
        ] {
            symlink(&target, bin.join(link)).unwrap();
        }
        symlink(
            scratch.root.join("etc/ssl/private"),
            scratch.root.join("etc/innocent"),
        )
        .unwrap();
        let block = "programs:
  fixprog:
    needs:
      - program_folder: root_link
      - program_folder: proc_link
      - program_folder: home_link
      - program_folder: home_parent
      - program_folder: writable_link
      - program_folder: cred_link
      - etc: innocent
      - library: libmurmur-absent.so.9
      - program_folder: share
  ghostprog:
    needs:
      - program_folder: .
      - library: libfix.so.1
";
        let resolved = derive_in(
            &[tool("local/needs-tool", LockOrigin::Operator, block)],
            &allow(&["fixprog", "ghostprog"]),
            &scratch.host(),
        );
        let fixprog = &resolved[0];
        assert_eq!(
            granted_paths(fixprog),
            BTreeSet::from([scratch.root.join("bin/share")])
        );
        let reasons: Vec<(String, String)> = fixprog
            .missing
            .iter()
            .map(|missing| (missing.need.form.to_string(), missing.reason.clone()))
            .collect();
        let root = scratch.root.display();
        assert_eq!(
            reasons,
            [
                (
                    "program_folder root_link".to_string(),
                    "resolved to /, the filesystem root".to_string()
                ),
                (
                    "program_folder proc_link".to_string(),
                    "resolved to /proc, which is the kernel's /proc, which no need reaches"
                        .to_string()
                ),
                (
                    "program_folder home_link".to_string(),
                    format!(
                        "resolved to {root}/home/user, which is the home folder \
                         {root}/home/user, which no need reaches"
                    )
                ),
                (
                    "program_folder home_parent".to_string(),
                    format!(
                        "resolved to {root}/home, which holds the home folder {root}/home/user, \
                         which no need reaches"
                    )
                ),
                (
                    "program_folder writable_link".to_string(),
                    format!(
                        "resolved to {root}/workdir, which is the capsule-writable tree \
                         {root}/workdir, which no need reaches"
                    )
                ),
                (
                    "program_folder cred_link".to_string(),
                    format!(
                        "resolved to {root}/etc/ssl/private, which is the credential location \
                         {root}/etc/ssl/private, which no need reaches"
                    )
                ),
                (
                    "etc innocent".to_string(),
                    format!(
                        "resolved to {root}/etc/ssl/private, which is the credential location \
                         {root}/etc/ssl/private, which no need reaches"
                    )
                ),
                (
                    "library libmurmur-absent.so.9".to_string(),
                    NOT_FOUND.to_string()
                ),
            ]
        );

        // A program PATH does not resolve: its folder is unknown, and a library resolves from the
        // search directories alone.
        let ghost = &resolved[1];
        assert_eq!(ghost.missing.len(), 1);
        assert_eq!(ghost.missing[0].reason, PROGRAM_NOT_ON_PATH);
        assert!(granted_paths(ghost).contains(&scratch.root.join("lib/libfix.so.1")));
    }

    #[test]
    fn declared_needs_reach_only_an_operator_pin_and_a_granted_program() {
        let scratch = Scratch::new();
        let bin = scratch.root.join("bin");
        std::fs::copy(host_binary("true"), bin.join("cat")).unwrap();
        std::fs::copy(host_binary("true"), bin.join("ls")).unwrap();
        let block = "programs:
  cat:
    needs:
      - program_folder: share
      - etc: fonts/fonts.conf
  ls:
    needs: []
";
        let host = scratch.host();
        let operator = [tool("local/needs-tool", LockOrigin::Operator, block)];

        // (a) The program with needs is not granted.
        assert_eq!(derive_in(&operator, &allow(&["ls"]), &host), Vec::new());

        // (b) Both granted: only cat's needs, under cat.
        let both = derive_in(&operator, &allow(&["cat", "ls"]), &host);
        assert_eq!(both.len(), 1);
        assert_eq!(both[0].program, "cat");
        assert_eq!(
            granted_paths(&both[0]),
            BTreeSet::from([
                scratch.root.join("bin/share"),
                scratch.root.join("etc/fonts/fonts.conf"),
            ])
        );

        // (c) A runtime pin contributes nothing.
        let runtime = [tool(
            "local/needs-tool",
            LockOrigin::Runtime {
                session: "ses_x".to_string(),
            },
            block,
        )];
        assert_eq!(derive_in(&runtime, &allow(&["cat"]), &host), Vec::new());

        // (d) An operator pin and a granted program.
        assert_eq!(derive_in(&operator, &allow(&["cat"]), &host), both);

        // The key must equal an allowlist entry verbatim.
        assert_eq!(
            derive_in(&operator, &allow(&["CAT", "cat2"]), &host),
            Vec::new()
        );

        // Two tools declaring needs for one program each contribute, each under its own artifact.
        let two = derive_in(
            &[
                tool("local/needs-tool", LockOrigin::Operator, block),
                tool(
                    "local/second",
                    LockOrigin::Operator,
                    "programs:\n  cat:\n    needs:\n      - system: fonts\n",
                ),
            ],
            &allow(&["cat"]),
            &host,
        );
        let owners: Vec<(&str, &str)> = two
            .iter()
            .map(|entry| (entry.artifact.as_str(), entry.program.as_str()))
            .collect();
        assert_eq!(
            owners,
            [("local/needs-tool", "cat"), ("local/second", "cat")]
        );
    }

    #[test]
    fn declared_needs_ca_bundle_is_the_trust_store_without_its_network_gate() {
        let scratch = Scratch::new();
        let bundle = scratch.root.join("etc/ssl/certs/ca.pem");
        let bundle_text = bundle.to_string_lossy().into_owned();
        let certs_text = scratch
            .root
            .join("etc/ssl/certs")
            .to_string_lossy()
            .into_owned();
        let candidates = [
            crate::trust_store::TrustStoreCandidate {
                path: &bundle_text,
                directory: false,
            },
            crate::trust_store::TrustStoreCandidate {
                path: &certs_text,
                directory: true,
            },
        ];
        let mut host = scratch.host();
        host.ca_candidates = &candidates;
        let resolved = derive_in(
            &[tool(
                "local/needs-tool",
                LockOrigin::Operator,
                "programs:\n  fixprog:\n    needs:\n      - system: ca_bundle\n",
            )],
            &allow(&["fixprog"]),
            &host,
        );
        assert_eq!(
            resolved[0].granted[0].paths,
            [NeedPath {
                path: scratch.root.join("etc/ssl/certs"),
                directory: true,
            }]
        );
        // The trust store itself stays gated on the network.
        assert_eq!(
            crate::trust_store::derive_in(&candidates, &allow(&["fixprog"]), &[], &[]),
            Vec::new()
        );
    }

    #[test]
    fn declared_needs_os_release_names_id_then_id_like() {
        assert_eq!(
            distributions_from_os_release("NAME=\"Ubuntu\"\nID=ubuntu\nID_LIKE=debian\n"),
            ["ubuntu", "debian"]
        );
        assert_eq!(
            distributions_from_os_release("ID=\"rocky\"\nID_LIKE=\"rhel centos fedora\"\n"),
            ["rocky", "rhel", "centos", "fedora"]
        );
        assert_eq!(
            distributions_from_os_release("ID_LIKE=\"rhel fedora\"\nID='almalinux'\n"),
            ["almalinux", "rhel", "fedora"]
        );
        assert_eq!(distributions_from_os_release(""), Vec::<String>::new());
        assert_eq!(
            distributions_from_os_release("ID=fedora\nVERSION_ID=40\n"),
            ["fedora"]
        );
    }

    #[test]
    fn declared_needs_package_for_takes_the_first_named_distribution() {
        let need = DeclaredNeed {
            form: NeedForm::Library("libx.so.1".to_string()),
            packages: [("debian", "libx1"), ("fedora", "x-libs")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        };
        let ubuntu = allow(&["ubuntu", "debian"]);
        assert_eq!(package_for(&need, &ubuntu), Some(("libx1", "debian")));
        assert_eq!(package_for(&need, &allow(&["arch"])), None);
    }
}
