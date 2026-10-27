//! The runtime directories of an allowlisted interpreter, asked of the interpreter itself.
//!
//! A `capabilities.shell.allow` binary's ELF closure (the binary, its `PT_INTERP` and its
//! `DT_NEEDED` libraries) is enough to start CPython or Perl and not enough to run either: each
//! reads its standard library out of directories no ELF header names. On `scoped` those
//! directories have no Landlock rule, so CPython dies in `init_fs_encoding` looking for
//! `encodings`. Where they are differs by distribution, interpreter version and install method, so
//! the runtime does not write any of them down. It recognises the interpreter from its own
//! dynamic symbol table and runs it with a fixed program that prints the interpreter's import
//! path: at the first staging that meets it, and again whenever [`crate::host_facts`] finds that
//! a file it was worked out from has changed.
//!
//! Each existing directory the interpreter reports becomes a readable, listable, non-executable
//! Landlock grant (see [`crate::sandbox::resolve_derived_interpreter_grants`]), and on `sealed` an
//! optional composed-root bind. The ELF shared objects directly inside those directories are C
//! extension modules; the `DT_NEEDED` closure of each (`libsqlite3.so.0` behind `_sqlite3`) is
//! granted read-only too, because the interpreter's own closure does not include it.
//!
//! An interpreter the table below does not recognise is granted exactly its ELF closure, and
//! `capabilities.shell.interpreter_runtime` remains the way to name its directories.

use std::path::{Path, PathBuf};

use crate::host_facts::{HostFactsCache, HostFactsUse};

/// One interpreter family the runtime recognises from the binary itself.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InterpreterKind<'a> {
    /// Names the family in a diagnostic.
    pub(crate) name: &'a str,
    /// A symbol every build of this interpreter lists in its `.dynsym`, defined in a statically
    /// linked build and undefined in one that imports it from the interpreter's shared library.
    pub(crate) marker_symbol: &'a str,
    /// The arguments that make the interpreter print its import path as NUL-separated entries and
    /// exit, without reading any per-user or per-site customisation.
    pub(crate) args: &'a [&'a str],
}

/// Every interpreter family the runtime recognises. To add one, name a symbol its binary always
/// lists in `.dynsym` and a program that prints its import path NUL-separated while running no
/// file the host or a user can customise.
///
///   - CPython: `-I` ignores every `PYTHON*` variable and the user site directory, and `-S` stops
///     `site` running at startup, so no `.pth` file executes. `site.getsitepackages()` still
///     names the distribution's own site directories (Debian's `dist-packages` among them).
///   - Perl: `-f` skips `sitecustomize.pl`.
#[cfg(target_os = "linux")]
pub(crate) const INTERPRETER_KINDS: &[InterpreterKind<'static>] = &[
    InterpreterKind {
        name: "CPython",
        marker_symbol: "Py_BytesMain",
        args: &[
            "-I",
            "-S",
            "-c",
            "import sys, site; sys.stdout.write('\\0'.join(sys.path + site.getsitepackages()))",
        ],
    },
    InterpreterKind {
        name: "Perl",
        marker_symbol: "perl_parse",
        args: &["-f", "-e", "print join qq{\\0}, @INC"],
    },
];

/// How long one interpreter query may run before it is killed and derives nothing.
#[cfg(target_os = "linux")]
pub(crate) const QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// What staging derived for one allowlisted interpreter.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DerivedInterpreterRuntime {
    /// The interpreter, canonical: the file that was queried and that `shell.allow` resolves to.
    pub binary: PathBuf,
    /// Each existing, canonical entry of the interpreter's import path, in the order it reported
    /// them: directories, and the zip archives CPython can import from.
    pub dirs: Vec<PathBuf>,
    /// The `DT_NEEDED` closure of the ELF shared objects directly inside [`Self::dirs`], less every
    /// file already inside one of those directories or in the interpreter's own ELF closure.
    pub libraries: Vec<PathBuf>,
}

impl DerivedInterpreterRuntime {
    /// Each entry of [`Self::dirs`] with whether it is a directory, which decides whether its
    /// grant is listable. The one place both the grant and the report ask.
    pub(crate) fn entries(&self) -> impl Iterator<Item = (&Path, bool)> {
        self.dirs.iter().map(|path| {
            (
                path.as_path(),
                std::fs::metadata(path).is_ok_and(|m| m.is_dir()),
            )
        })
    }
}

/// What one staging derived for its allowlisted binaries.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InterpreterRuntimes {
    /// One per recognised interpreter whose runtime was derived, ordered by binary path.
    pub runtimes: Vec<DerivedInterpreterRuntime>,
    /// One per examined binary: every resolved `shell.allow` binary that no `staged` grant names
    /// and that lies under no writable root, ordered by binary path.
    pub host_facts: Vec<HostFactsUse>,
}

/// The runtime directories of each `shell_allow` binary that is a recognised interpreter.
///
/// `capsule_writable` are the trees the capsule's commands can write: a binary under one is never
/// run, and nothing under one is derived. A binary a `staged` grant names is not queried, because
/// its runtime is the staged tree.
///
/// Each examined binary's facts come from `cache` when its kept entry still matches the host, and
/// are otherwise worked out, which runs the interpreter, and stored there. Either way the result
/// is what a fresh derivation gives; [`InterpreterRuntimes::host_facts`] says which happened.
///
/// A query that cannot be completed (spawn error, a run past [`QUERY_TIMEOUT`], a non-zero exit,
/// output that is not UTF-8, or no usable entry) derives nothing for that binary, writes one line
/// to standard error naming the binary and the reason, and fails nothing. A query that did not
/// complete is never kept.
///
/// Off Linux this returns nothing and runs nothing: there is no Landlock rule to feed.
pub fn derive_interpreter_runtimes(
    shell_allow: &[String],
    staged: &[murmur_artifact::StagedRuntimeGrant],
    capsule_writable: &[&Path],
    cache: &HostFactsCache,
) -> InterpreterRuntimes {
    #[cfg(target_os = "linux")]
    {
        let path_dirs: Vec<PathBuf> = std::env::var_os("PATH")
            .map(|path| std::env::split_paths(&path).collect())
            .unwrap_or_default();
        derive_in(
            shell_allow,
            staged,
            capsule_writable,
            &path_dirs,
            INTERPRETER_KINDS,
            QUERY_TIMEOUT,
            cache,
        )
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (shell_allow, staged, capsule_writable, cache);
        InterpreterRuntimes::default()
    }
}

/// How many allowlisted binaries [`keep_host_facts`] examined, and how many of those the cache
/// holds an entry for afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HostFactsKept {
    /// Every resolved `shell.allow` binary that no `staged` grant names and that lies under no
    /// writable root, as [`InterpreterRuntimes::host_facts`] counts them.
    pub examined: usize,
    /// The examined binaries whose entry `cache` holds when the call returns: one that still
    /// matched, or one just worked out and stored. A binary whose query failed has none.
    pub kept: usize,
}

/// Fills `cache` with the entries [`derive_interpreter_runtimes`] would keep for the same
/// arguments, ahead of the launch that would otherwise work them out, and counts them.
///
/// The derivation is the launch's own, with the same `PATH`, interpreter table and query timeout,
/// so an entry written here is the one that launch would write and the launch reports it kept. A
/// staging passing other `capsule_writable` trees still keeps every entry whose recorded paths lie
/// outside both sets. Nothing is written to standard error: a query that fails stores no entry,
/// and the launch that works it out again reports it.
///
/// Off Linux this keeps nothing and runs nothing.
pub fn keep_host_facts(
    shell_allow: &[String],
    staged: &[murmur_artifact::StagedRuntimeGrant],
    capsule_writable: &[&Path],
    cache: &HostFactsCache,
) -> HostFactsKept {
    #[cfg(target_os = "linux")]
    {
        let path_dirs: Vec<PathBuf> = std::env::var_os("PATH")
            .map(|path| std::env::split_paths(&path).collect())
            .unwrap_or_default();
        fill_in(
            shell_allow,
            staged,
            capsule_writable,
            &path_dirs,
            INTERPRETER_KINDS,
            QUERY_TIMEOUT,
            cache,
            &mut |_| {},
        )
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (shell_allow, staged, capsule_writable, cache);
        HostFactsKept::default()
    }
}

/// The trees a session placed at `placement` lets its commands write, as
/// [`derive_interpreter_runtimes`] takes them: the workdir root and the records group, which holds
/// every session's temporary folder.
pub fn capsule_writable_roots(placement: &crate::runtime_dir::RuntimeDirPlacement) -> [&Path; 2] {
    [&placement.workdir, &placement.records_group]
}

/// [`derive_interpreter_runtimes`] with `PATH`, the kind table and the timeout injected.
#[cfg(target_os = "linux")]
pub(crate) fn derive_in(
    shell_allow: &[String],
    staged: &[murmur_artifact::StagedRuntimeGrant],
    capsule_writable: &[&Path],
    path_dirs: &[PathBuf],
    kinds: &[InterpreterKind<'_>],
    timeout: std::time::Duration,
    cache: &HostFactsCache,
) -> InterpreterRuntimes {
    examine(
        shell_allow,
        staged,
        capsule_writable,
        path_dirs,
        kinds,
        timeout,
        cache,
        &mut |line| crate::runtime_err!("{line}"),
    )
    .0
}

/// [`keep_host_facts`] with `PATH`, the kind table and the timeout injected, and each line saying
/// an interpreter's runtime was not derived handed to `report`.
#[cfg(target_os = "linux")]
#[allow(clippy::too_many_arguments)]
pub(crate) fn fill_in(
    shell_allow: &[String],
    staged: &[murmur_artifact::StagedRuntimeGrant],
    capsule_writable: &[&Path],
    path_dirs: &[PathBuf],
    kinds: &[InterpreterKind<'_>],
    timeout: std::time::Duration,
    cache: &HostFactsCache,
    report: &mut dyn FnMut(&str),
) -> HostFactsKept {
    let (derived, kept) = examine(
        shell_allow,
        staged,
        capsule_writable,
        path_dirs,
        kinds,
        timeout,
        cache,
        report,
    );
    HostFactsKept {
        examined: derived.host_facts.len(),
        kept,
    }
}

/// The derivation both [`derive_in`] and [`fill_in`] run, and how many examined binaries the cache
/// holds an entry for when it returns.
#[cfg(target_os = "linux")]
#[allow(clippy::too_many_arguments)]
fn examine(
    shell_allow: &[String],
    staged: &[murmur_artifact::StagedRuntimeGrant],
    capsule_writable: &[&Path],
    path_dirs: &[PathBuf],
    kinds: &[InterpreterKind<'_>],
    timeout: std::time::Duration,
    cache: &HostFactsCache,
    report: &mut dyn FnMut(&str),
) -> (InterpreterRuntimes, usize) {
    use crate::sandbox::resolve_exec_allowlist_in;

    let roots: Vec<PathBuf> = capsule_writable
        .iter()
        .map(|root| crate::protected_paths::canonicalize_existing_prefix(root))
        .collect();
    let staged_names: Vec<String> = staged.iter().map(|grant| grant.binary.clone()).collect();
    let staged_binaries = resolve_exec_allowlist_in(&staged_names, path_dirs);
    let queried: Vec<String> = shell_allow
        .iter()
        .filter(|name| !staged_names.contains(name))
        .cloned()
        .collect();
    let examined: Vec<PathBuf> = resolve_exec_allowlist_in(&queried, path_dirs)
        .into_iter()
        .filter(|binary| {
            !staged_binaries.contains(binary) && !crate::reachability::under_any(binary, &roots)
        })
        .collect();

    let mut derived = InterpreterRuntimes::default();
    let mut held = 0;
    if examined.is_empty() {
        return (derived, held);
    }
    let shelf = cache.open(&roots);
    for binary in examined {
        let found = crate::host_facts::runtime_of(
            shelf.as_ref(),
            &binary,
            kinds,
            &roots,
            timeout,
            &mut *report,
        );
        derived.runtimes.extend(found.runtime);
        held += usize::from(found.held);
        derived.host_facts.push(HostFactsUse {
            binary,
            source: found.source,
        });
    }
    (derived, held)
}

/// Hands `report` the line saying `binary`'s runtime was not derived and why. A launch writes it
/// to standard error; [`keep_host_facts`] drops it.
#[cfg(target_os = "linux")]
pub(crate) fn report_not_derived(
    binary: &Path,
    kind: InterpreterKind<'_>,
    reason: &str,
    report: &mut dyn FnMut(&str),
) {
    report(&format!(
        "[capsule-runtime] the runtime directories of {} interpreter {} were not derived: {reason}",
        kind.name,
        binary.display()
    ));
}

/// The first kind whose marker symbol the `.dynsym` of the file at `binary` lists. `None` for an
/// ELF that lists none of them, for anything that is not ELF, a `#!` script included, and for a
/// file that cannot be opened. Reads the ELF and section headers, `.dynsym` and its string table,
/// never the whole file.
#[cfg(target_os = "linux")]
pub(crate) fn recognise<'a>(
    binary: &Path,
    kinds: &[InterpreterKind<'a>],
) -> Option<InterpreterKind<'a>> {
    let image = crate::sandbox::ElfFile::open_host(binary).ok()?;
    let names = crate::sandbox::elf_dynamic_symbol_names(&image)?;
    kinds
        .iter()
        .find(|kind| names.iter().any(|name| name == kind.marker_symbol))
        .copied()
}

/// Runs `binary` with `kind`'s arguments, an empty environment, `/dev/null` as standard input and
/// `/` as working directory, and returns the NUL-separated entries it printed. Killed at
/// `timeout`. Standard error is read only to explain a failure.
#[cfg(target_os = "linux")]
pub(crate) fn query(
    binary: &Path,
    kind: InterpreterKind<'_>,
    timeout: std::time::Duration,
) -> Result<Vec<String>, String> {
    use std::process::{Command, Stdio};
    use std::time::Instant;

    let started = Instant::now();
    let mut child = Command::new(binary)
        .args(kind.args)
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("it could not be started: {error}"))?;

    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < timeout => {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "it did not exit within {} s and was killed",
                    timeout.as_secs_f64()
                ));
            }
            Err(error) => return Err(format!("waiting for it failed: {error}")),
        }
    };
    let remaining = || timeout.saturating_sub(started.elapsed());
    if !status.success() {
        let detail = stderr
            .recv_timeout(remaining())
            .ok()
            .and_then(|bytes| {
                String::from_utf8_lossy(&bytes)
                    .lines()
                    .find(|line| !line.trim().is_empty())
                    .map(|line| format!(": {}", line.trim()))
            })
            .unwrap_or_default();
        return Err(format!("it exited with {status}{detail}"));
    }
    let printed = stdout
        .recv_timeout(remaining())
        .map_err(|_| "its output did not close before the deadline".to_string())?;
    let printed = String::from_utf8(printed)
        .map_err(|_| "it printed output that is not UTF-8".to_string())?;
    Ok(printed
        .split('\0')
        .filter(|entry| !entry.is_empty())
        .map(str::to_string)
        .collect())
}

/// Reads `pipe` to its end on a thread of its own, so a full pipe cannot stall the child, and
/// hands the bytes over a channel the caller reads with a deadline, so a descendant still holding
/// the pipe cannot stall staging.
#[cfg(target_os = "linux")]
fn drain<R: std::io::Read + Send + 'static>(pipe: Option<R>) -> std::sync::mpsc::Receiver<Vec<u8>> {
    let (sender, receiver) = std::sync::mpsc::channel();
    if let Some(mut pipe) = pipe {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            let _ = sender.send(bytes);
        });
    }
    receiver
}

/// The entries of `reported` that name an existing directory or regular file, canonical, in
/// order and deduplicated. An entry is dropped when it is relative, does not exist, is `/`, the
/// interpreter's own directory or an ancestor of it (never an install prefix), or lies under one
/// of the canonical `roots`.
#[cfg(target_os = "linux")]
pub(crate) fn keep_entries(binary: &Path, reported: &[String], roots: &[PathBuf]) -> Vec<PathBuf> {
    let own_dir = binary.parent().unwrap_or_else(|| Path::new("/"));
    let mut kept: Vec<PathBuf> = Vec::new();
    for entry in reported {
        let path = Path::new(entry);
        if !path.is_absolute() {
            continue;
        }
        let Ok(canonical) = std::fs::canonicalize(path) else {
            continue;
        };
        let Ok(metadata) = std::fs::metadata(&canonical) else {
            continue;
        };
        if !(metadata.is_dir() || metadata.is_file()) {
            continue;
        }
        if own_dir.starts_with(&canonical) || crate::reachability::under_any(&canonical, roots) {
            continue;
        }
        if !kept.contains(&canonical) {
            kept.push(canonical);
        }
    }
    kept
}

/// Where a derivation takes the facts about directories and ELF files it reads: from the host,
/// or from a kept entry that hands each out only while its recorded stamp still matches.
#[cfg(target_os = "linux")]
pub(crate) trait Facts {
    /// The path a kept entry did not record, or recorded with another stamp.
    type Changed;

    /// The ELF shared objects directly inside `dir`, which is a canonical kept import-path entry.
    /// Empty for one that cannot be listed.
    fn shared_objects_in(&mut self, dir: &Path) -> Result<Vec<PathBuf>, Self::Changed>;

    /// The dependencies of the canonical file at `path`, `None` when it cannot be read or is not
    /// ELF64.
    fn dependencies(
        &mut self,
        path: &Path,
    ) -> Result<Option<crate::sandbox::ElfDependencies>, Self::Changed>;
}

/// The runtime of interpreter `binary` from the entries its completed query `reported`, filtered
/// against the canonical `roots` and with its extension modules' closure walked through `facts`.
/// `None`, after one line to `report`, when no entry is left to grant.
#[cfg(target_os = "linux")]
pub(crate) fn runtime_from_report<F: Facts>(
    binary: &Path,
    kind: InterpreterKind<'_>,
    reported: &[String],
    roots: &[PathBuf],
    facts: &mut F,
    report: &mut dyn FnMut(&str),
) -> Result<Option<DerivedInterpreterRuntime>, F::Changed> {
    let dirs = keep_entries(binary, reported, roots);
    if dirs.is_empty() {
        report_not_derived(
            binary,
            kind,
            "it reported no existing directory to grant",
            report,
        );
        return Ok(None);
    }
    let libraries = extension_module_closure(binary, &dirs, roots, facts)?;
    Ok(Some(DerivedInterpreterRuntime {
        binary: binary.to_path_buf(),
        dirs,
        libraries,
    }))
}

/// The `DT_NEEDED` closure of every ELF shared object directly inside one of the directories in
/// `dirs` (no recursion), less the files inside `dirs` themselves and those in `binary`'s own ELF
/// closure, which `shell.allow` already grants. Nothing under `roots` is read or returned.
#[cfg(target_os = "linux")]
fn extension_module_closure<F: Facts>(
    binary: &Path,
    dirs: &[PathBuf],
    roots: &[PathBuf],
    facts: &mut F,
) -> Result<Vec<PathBuf>, F::Changed> {
    let mut seeds = Vec::new();
    for dir in dirs {
        seeds.extend(facts.shared_objects_in(dir)?);
    }
    if seeds.is_empty() {
        return Ok(Vec::new());
    }
    let search_dirs: Vec<PathBuf> = crate::sandbox::DEFAULT_LIBRARY_SEARCH_DIRS
        .iter()
        .map(PathBuf::from)
        .collect();
    let own_closure = crate::sandbox::resolve_landlock_grants(&[binary.to_path_buf()]);
    let closure = crate::sandbox::walk_dependency_closure(&seeds, &search_dirs, roots, |path| {
        facts.dependencies(path)
    })?;
    Ok(closure
        .into_iter()
        .filter(|path| !dirs.iter().any(|dir| path.starts_with(dir)))
        .filter(|path| !own_closure.contains(path))
        .collect())
}

#[cfg(all(test, target_os = "linux"))]
#[allow(clippy::print_stdout)]
pub(crate) mod tests {
    use super::*;
    use crate::host_facts::{HostFactsReason, HostFactsSource};
    use std::time::Duration;

    /// A kind that recognises `dash` (whose `.dynsym` lists `execve`) and runs `args` with it.
    pub(crate) fn dash_kind<'a>(args: &'a [&'a str]) -> InterpreterKind<'a> {
        InterpreterKind {
            name: "test",
            marker_symbol: "execve",
            args,
        }
    }

    /// A copy of the host's `dash` named `name` in a fresh directory under `parent`, or `None`
    /// with a skip line when the host has none.
    pub(crate) fn dash_copy(parent: &Path, name: &str) -> Option<PathBuf> {
        let dash = std::fs::canonicalize("/bin/dash")
            .or_else(|_| std::fs::canonicalize("/usr/bin/dash"))
            .ok();
        let Some(dash) = dash else {
            println!("[SKIP] no dash on this host to stand in for an interpreter");
            return None;
        };
        let bin = parent.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let copy = bin.join(name);
        std::fs::copy(&dash, &copy).unwrap();
        Some(copy)
    }

    pub(crate) fn canonical_tempdir() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        (temp, root)
    }

    /// This thread's `rchar` from `/proc/thread-self/io`: the bytes its `read`-family calls have
    /// returned, page cache hits included. Panics when the file cannot be read.
    pub(crate) fn thread_rchar() -> u64 {
        let io = std::fs::read_to_string("/proc/thread-self/io")
            .expect("/proc/thread-self/io is readable");
        io.lines()
            .find_map(|line| line.strip_prefix("rchar: "))
            .and_then(|value| value.trim().parse().ok())
            .expect("/proc/thread-self/io has an rchar line")
    }

    /// Appends `bytes` zero bytes to the file at `path`.
    pub(crate) fn pad_with_zeros(path: &Path, bytes: usize) {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        let chunk = vec![0u8; 1 << 20];
        for _ in 0..bytes / chunk.len() {
            file.write_all(&chunk).unwrap();
        }
        file.write_all(&chunk[..bytes % chunk.len()]).unwrap();
    }

    /// A host facts cache at `<root>/host_facts`, never the developer's own.
    pub(crate) fn cache_in(root: &Path) -> HostFactsCache {
        HostFactsCache::at(root.join("host_facts"))
    }

    #[test]
    fn cpython_is_recognised_by_py_bytes_main() {
        let Some(python) = crate::sandbox::find_on_path("python3") else {
            println!("[SKIP] no python3 on PATH");
            return;
        };
        let binary = std::fs::canonicalize(python).unwrap();
        assert_eq!(
            recognise(&binary, INTERPRETER_KINDS).unwrap().name,
            "CPython"
        );
    }

    #[test]
    fn perl_is_recognised_by_perl_parse() {
        let Some(perl) = crate::sandbox::find_on_path("perl") else {
            println!("[SKIP] no perl on PATH");
            return;
        };
        let binary = std::fs::canonicalize(perl).unwrap();
        assert_eq!(recognise(&binary, INTERPRETER_KINDS).unwrap().name, "Perl");
    }

    #[test]
    fn a_plain_elf_and_a_script_are_not_interpreters() {
        let test_binary = std::env::current_exe().unwrap();
        assert!(
            crate::sandbox::elf_dynamic_symbol_names(
                &crate::sandbox::ElfFile::open_host(&test_binary).unwrap()
            )
            .is_some(),
            "the test binary is a dynamic ELF with a .dynsym to read"
        );
        assert_eq!(recognise(&test_binary, INTERPRETER_KINDS), None);
        assert_eq!(recognise(Path::new("/bin/true"), INTERPRETER_KINDS), None);
        let (_temp, root) = canonical_tempdir();
        let script = root.join("script");
        std::fs::write(
            &script,
            b"#!/usr/bin/python3\nimport sys\n# Py_BytesMain perl_parse\n",
        )
        .unwrap();
        assert_eq!(recognise(&script, INTERPRETER_KINDS), None);
        assert_eq!(recognise(&root.join("missing"), INTERPRETER_KINDS), None);
    }

    #[test]
    fn reported_entries_keep_only_existing_absolute_paths_outside_the_prefix() {
        let (_temp, root) = canonical_tempdir();
        let prefix = root.join("prefix");
        let stdlib = prefix.join("lib/python");
        std::fs::create_dir_all(prefix.join("bin")).unwrap();
        std::fs::create_dir_all(&stdlib).unwrap();
        let zip = prefix.join("lib/python312.zip");
        std::fs::write(&zip, b"PK").unwrap();
        let excluded = root.join("work");
        std::fs::create_dir_all(excluded.join("lib")).unwrap();
        let binary = prefix.join("bin/python3");
        let string = |path: &Path| path.to_string_lossy().into_owned();
        let reported = vec![
            "relative/lib".to_string(),
            String::new(),
            string(&root.join("missing")),
            "/".to_string(),
            string(&root),
            string(&prefix),
            string(&prefix.join("bin")),
            string(&stdlib),
            format!("{}/../python", stdlib.display()),
            string(&zip),
            string(&excluded.join("lib")),
        ];
        assert_eq!(
            keep_entries(&binary, &reported, &[excluded]),
            vec![stdlib, zip.clone()]
        );
        let derived = DerivedInterpreterRuntime {
            binary,
            dirs: vec![zip.clone()],
            libraries: Vec::new(),
        };
        assert_eq!(
            derived.entries().collect::<Vec<_>>(),
            vec![(zip.as_path(), false)],
            "a zip archive is kept as a non-listable file"
        );
    }

    #[test]
    fn a_binary_under_an_excluded_root_is_never_run() {
        let (_temp, root) = canonical_tempdir();
        let excluded = root.join("work");
        let Some(fake) = dash_copy(&excluded, "fakeinterp") else {
            return;
        };
        let marker = root.join("ran");
        let script = format!("touch {}", marker.display());
        let args = ["-c", script.as_str()];
        let kinds = [dash_kind(&args)];
        let path_dirs = vec![fake.parent().unwrap().to_path_buf()];
        let allow = vec!["fakeinterp".to_string()];

        let derived = derive_in(
            &allow,
            &[],
            &[excluded.as_path()],
            &path_dirs,
            &kinds,
            QUERY_TIMEOUT,
            &cache_in(&root),
        );
        assert_eq!(derived, InterpreterRuntimes::default());
        assert!(!marker.exists(), "the excluded binary was executed");

        // The same stand-in outside every excluded root is recognised and run, so the absence
        // of the marker above is the exclusion and not a stand-in that never runs.
        derive_in(
            &allow,
            &[],
            &[],
            &path_dirs,
            &kinds,
            QUERY_TIMEOUT,
            &cache_in(&root),
        );
        assert!(
            marker.exists(),
            "the stand-in did not run when nothing excluded it"
        );
    }

    #[test]
    fn a_staged_runtime_binary_is_not_queried() {
        let (_temp, root) = canonical_tempdir();
        let Some(fake) = dash_copy(&root.join("staged"), "stagedinterp") else {
            return;
        };
        let marker = root.join("ran");
        let script = format!("touch {}", marker.display());
        let args = ["-c", script.as_str()];
        let kinds = [dash_kind(&args)];
        let path_dirs = vec![fake.parent().unwrap().to_path_buf()];
        let staged = vec![murmur_artifact::StagedRuntimeGrant {
            binary: "stagedinterp".to_string(),
            source_path: root.join("staged").to_string_lossy().into_owned(),
            pin: "test".to_string(),
        }];
        let derived = derive_in(
            &["stagedinterp".to_string()],
            &staged,
            &[],
            &path_dirs,
            &kinds,
            QUERY_TIMEOUT,
            &cache_in(&root),
        );
        assert_eq!(derived, InterpreterRuntimes::default());
        assert!(!marker.exists(), "the staged binary was queried");
    }

    #[test]
    fn a_query_that_times_out_fails_or_prints_non_utf8_derives_nothing() {
        let (_temp, root) = canonical_tempdir();
        let Some(fake) = dash_copy(&root, "fakeinterp") else {
            return;
        };
        let path_dirs = vec![fake.parent().unwrap().to_path_buf()];
        let allow = vec!["fakeinterp".to_string()];
        let cache = cache_in(&root);
        // The positive control: the same stand-in printing an existing directory derives it.
        let lib = root.join("lib");
        std::fs::create_dir_all(&lib).unwrap();
        let good = format!("printf '%s' {}", lib.display());
        let cases: [(&str, &str, Duration); 4] = [
            ("answers", good.as_str(), QUERY_TIMEOUT),
            ("times out", "sleep 10", Duration::from_millis(300)),
            ("exits non-zero", "printf /; exit 3", QUERY_TIMEOUT),
            ("prints non-UTF-8", "printf '/\\377\\376'", QUERY_TIMEOUT),
        ];
        for (case, script, timeout) in cases {
            let args = ["-c", script];
            let kinds = [dash_kind(&args)];
            let started = std::time::Instant::now();
            let derived = derive_in(&allow, &[], &[], &path_dirs, &kinds, timeout, &cache);
            if case == "answers" {
                assert_eq!(derived.runtimes.len(), 1, "{case}");
                assert_eq!(derived.runtimes[0].dirs, vec![lib.clone()], "{case}");
            } else {
                assert!(derived.runtimes.is_empty(), "{case}: {derived:?}");
                // Never kept: the same query a second time runs again and is not taken from
                // the cache.
                let again = derive_in(&allow, &[], &[], &path_dirs, &kinds, timeout, &cache);
                assert!(again.runtimes.is_empty(), "{case}: {again:?}");
                assert!(
                    !matches!(again.host_facts[0].source, HostFactsSource::Kept),
                    "{case}: {again:?}"
                );
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "{case} took {:?}",
                started.elapsed()
            );
        }
    }

    /// The file names in `cache`'s directory, each with its bytes, sorted.
    fn entries_of(root: &Path) -> Vec<(std::ffi::OsString, Vec<u8>)> {
        let mut entries: Vec<_> = std::fs::read_dir(root.join("host_facts"))
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (entry.file_name(), std::fs::read(entry.path()).unwrap())
            })
            .collect();
        entries.sort();
        entries
    }

    #[test]
    fn kept_host_facts_are_the_entries_a_launch_writes() {
        let (_temp, root) = canonical_tempdir();
        let Some(fake) = dash_copy(&root.join("prefix"), "fakeinterp") else {
            return;
        };
        let lib = root.join("lib");
        std::fs::create_dir_all(&lib).unwrap();
        let script = format!("printf '%s' {}", lib.display());
        let args = ["-c", script.as_str()];
        let kinds = [dash_kind(&args)];
        let path_dirs = vec![fake.parent().unwrap().to_path_buf()];
        let allow = vec!["fakeinterp".to_string()];
        let filled = root.join("filled");
        let launched = root.join("launched");

        let mut lines = Vec::new();
        let kept = fill_in(
            &allow,
            &[],
            &[],
            &path_dirs,
            &kinds,
            QUERY_TIMEOUT,
            &cache_in(&filled),
            &mut |line| lines.push(line.to_string()),
        );
        assert_eq!(
            kept,
            HostFactsKept {
                examined: 1,
                kept: 1
            }
        );
        assert_eq!(lines, Vec::<String>::new());
        let fresh = derive_in(
            &allow,
            &[],
            &[],
            &path_dirs,
            &kinds,
            QUERY_TIMEOUT,
            &cache_in(&launched),
        );
        assert_eq!(entries_of(&filled).len(), 1);
        assert_eq!(entries_of(&filled), entries_of(&launched));

        let after_fill = derive_in(
            &allow,
            &[],
            &[],
            &path_dirs,
            &kinds,
            QUERY_TIMEOUT,
            &cache_in(&filled),
        );
        assert_eq!(after_fill.host_facts[0].source, HostFactsSource::Kept);
        assert_eq!(after_fill.runtimes, fresh.runtimes);
        assert_eq!(fresh.runtimes[0].dirs, vec![lib]);

        // A second fill keeps the entry it finds.
        assert_eq!(
            fill_in(
                &allow,
                &[],
                &[],
                &path_dirs,
                &kinds,
                QUERY_TIMEOUT,
                &cache_in(&filled),
                &mut |_| {},
            ),
            kept
        );
    }

    #[test]
    fn keeping_host_facts_keeps_nothing_for_a_failing_query_and_reports_it_to_its_sink() {
        let (_temp, root) = canonical_tempdir();
        let Some(fake) = dash_copy(&root.join("prefix"), "fakeinterp") else {
            return;
        };
        let args = ["-c", "exit 3"];
        let kinds = [dash_kind(&args)];
        let path_dirs = vec![fake.parent().unwrap().to_path_buf()];
        let allow = vec!["fakeinterp".to_string()];
        let cache = cache_in(&root);
        let fill = || {
            let mut lines = Vec::new();
            let kept = fill_in(
                &allow,
                &[],
                &[],
                &path_dirs,
                &kinds,
                QUERY_TIMEOUT,
                &cache,
                &mut |line| lines.push(line.to_string()),
            );
            (kept, lines)
        };

        let (kept, lines) = fill();
        assert_eq!(
            kept,
            HostFactsKept {
                examined: 1,
                kept: 0
            }
        );
        assert_eq!(
            lines,
            vec![format!(
                "[capsule-runtime] the runtime directories of test interpreter {} were not \
                 derived: it exited with exit status: 3",
                fake.display()
            )]
        );
        assert_eq!(entries_of(&root), Vec::new());

        // The launch after it finds no entry and works the binary out again, as does a later fill.
        let launched = derive_in(&allow, &[], &[], &path_dirs, &kinds, QUERY_TIMEOUT, &cache);
        assert_eq!(
            launched.host_facts,
            vec![HostFactsUse {
                binary: fake.clone(),
                source: HostFactsSource::Derived {
                    reason: HostFactsReason::Absent,
                    changed_path: None
                }
            }]
        );
        assert!(launched.runtimes.is_empty());
        assert_eq!(entries_of(&root), Vec::new());
        assert_eq!(fill(), (kept, lines));
    }

    #[test]
    fn keeping_host_facts_with_nothing_to_examine_creates_no_cache() {
        let (_temp, root) = canonical_tempdir();
        let kinds = [dash_kind(&["-c", "exit 0"])];
        let kept = fill_in(
            &["no-such-binary-3d8f8b19".to_string()],
            &[],
            &[],
            std::slice::from_ref(&root),
            &kinds,
            QUERY_TIMEOUT,
            &cache_in(&root),
            &mut |_| {},
        );
        assert_eq!(kept, HostFactsKept::default());
        assert!(!root.join("host_facts").exists());
    }

    #[test]
    fn derived_grants_read_and_list_directories_and_never_execute() {
        let (_temp, root) = canonical_tempdir();
        let stdlib = root.join("lib/python");
        std::fs::create_dir_all(&stdlib).unwrap();
        let zip = root.join("lib/python312.zip");
        std::fs::write(&zip, b"PK").unwrap();
        let library = PathBuf::from("/opt/test/lib/libsqlite3.so.0");
        let derived = vec![DerivedInterpreterRuntime {
            binary: PathBuf::from("/opt/test/bin/python3"),
            dirs: vec![stdlib.clone(), zip.clone()],
            libraries: vec![library.clone()],
        }];
        let grant = |path: &Path, list_dir| crate::sandbox::LandlockGrant {
            path: path.to_path_buf(),
            list_dir,
            executable: false,
            pinned: None,
        };
        assert_eq!(
            crate::sandbox::resolve_derived_interpreter_grants(&derived),
            vec![
                grant(&stdlib, true),
                grant(&zip, false),
                grant(&library, false)
            ]
        );

        let policy = crate::types::CapabilityPolicy {
            shell_derived_interpreter_runtime: derived,
            ..Default::default()
        };
        let binds = crate::sandbox::resolve_sealed_bind_dirs(&[], &policy);
        assert!(binds.contains(&stdlib), "{binds:?}");
        assert!(binds.contains(&PathBuf::from("/opt/test/lib")), "{binds:?}");
    }

    /// The derivation for this host's `python3` and `perl`, through a temporary cache: each one
    /// on `PATH` is derived, none of its entries is its own directory or an ancestor of it, the
    /// second staging keeps every entry the first stored, and both equal a derivation through a
    /// cache that cannot be used. The cold and kept times are printed for the staging cost each
    /// adds.
    #[test]
    fn this_hosts_interpreters_derive_their_runtimes() {
        let allow = vec!["python3".to_string(), "perl".to_string()];
        let (_temp, root) = canonical_tempdir();
        let cache = cache_in(&root);
        let path_dirs: Vec<PathBuf> = std::env::var_os("PATH")
            .map(|path| std::env::split_paths(&path).collect())
            .unwrap_or_default();
        let derive = |cache: &HostFactsCache| {
            let started = std::time::Instant::now();
            let derived = derive_in(
                &allow,
                &[],
                &[],
                &path_dirs,
                INTERPRETER_KINDS,
                QUERY_TIMEOUT,
                cache,
            );
            (derived, started.elapsed())
        };
        let (cold, cold_elapsed) = derive(&cache);
        let (kept, kept_elapsed) = derive(&cache);
        // A cache whose directory is a file is unusable, so this derives with nothing kept.
        std::fs::write(root.join("not-a-cache"), b"").unwrap();
        let (uncached, _) = derive(&HostFactsCache::at(root.join("not-a-cache")));

        let mut found = 0;
        for name in &allow {
            let Some(path) = crate::sandbox::find_on_path(name) else {
                println!("[SKIP] no {name} on PATH");
                continue;
            };
            found += 1;
            let binary = std::fs::canonicalize(path).unwrap();
            assert!(
                cold.runtimes
                    .iter()
                    .any(|runtime| runtime.binary == binary && !runtime.dirs.is_empty()),
                "{name} ({}) derived nothing: {cold:?}",
                binary.display()
            );
            let source = |derived: &InterpreterRuntimes| {
                derived
                    .host_facts
                    .iter()
                    .find(|used| used.binary == binary)
                    .map(|used| used.source.clone())
            };
            assert_eq!(
                source(&cold),
                Some(HostFactsSource::Derived {
                    reason: HostFactsReason::Absent,
                    changed_path: None
                }),
                "{name}"
            );
            assert_eq!(source(&kept), Some(HostFactsSource::Kept), "{name}");
            assert_eq!(
                source(&uncached),
                Some(HostFactsSource::Derived {
                    reason: HostFactsReason::NotKept,
                    changed_path: None
                }),
                "{name}"
            );
        }
        assert_eq!(kept.runtimes, cold.runtimes);
        assert_eq!(uncached.runtimes, cold.runtimes);
        assert_eq!(cold.host_facts.len(), found);
        for runtime in &cold.runtimes {
            println!("{}", runtime.binary.display());
            for dir in &runtime.dirs {
                println!("  dir {}", dir.display());
            }
            for library in &runtime.libraries {
                println!("  library {}", library.display());
            }
            for dir in &runtime.dirs {
                assert!(!runtime.binary.parent().unwrap().starts_with(dir));
            }
        }
        println!("derive_in([python3, perl]) took {cold_elapsed:?} cold and {kept_elapsed:?} kept");
    }
}
