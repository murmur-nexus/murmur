//! The runtime directories of an allowlisted interpreter, asked of the interpreter itself.
//!
//! A `capabilities.shell.allow` binary's ELF closure (the binary, its `PT_INTERP` and its
//! `DT_NEEDED` libraries) is enough to start CPython or Perl and not enough to run either: each
//! reads its standard library out of directories no ELF header names. On `scoped` those
//! directories have no Landlock rule, so CPython dies in `init_fs_encoding` looking for
//! `encodings`. Where they are differs by distribution, interpreter version and install method, so
//! the runtime does not write any of them down. It recognises the interpreter from its own
//! dynamic symbol table and runs it once, at staging, with a fixed program that prints the
//! interpreter's import path.
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

/// The runtime directories of each `shell_allow` binary that is a recognised interpreter.
///
/// `capsule_writable` are the trees the capsule's commands can write: a binary under one is never
/// run, and nothing under one is derived. A binary a `staged` grant names is not queried, because
/// its runtime is the staged tree.
///
/// A query that cannot be completed (spawn error, a run past [`QUERY_TIMEOUT`], a non-zero exit,
/// output that is not UTF-8, or no usable entry) derives nothing for that binary, writes one line
/// to standard error naming the binary and the reason, and fails nothing.
///
/// Off Linux this returns nothing and runs nothing: there is no Landlock rule to feed.
pub fn derive_interpreter_runtimes(
    shell_allow: &[String],
    staged: &[murmur_artifact::StagedRuntimeGrant],
    capsule_writable: &[&Path],
) -> Vec<DerivedInterpreterRuntime> {
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
        )
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (shell_allow, staged, capsule_writable);
        Vec::new()
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
) -> Vec<DerivedInterpreterRuntime> {
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

    let mut derived = Vec::new();
    for binary in resolve_exec_allowlist_in(&queried, path_dirs) {
        if staged_binaries.contains(&binary) || crate::reachability::under_any(&binary, &roots) {
            continue;
        }
        let Ok(image) = std::fs::read(&binary) else {
            continue;
        };
        let Some(kind) = recognise(&image, kinds) else {
            continue;
        };
        drop(image);
        let dirs = match query(&binary, kind, timeout) {
            Ok(entries) => keep_entries(&binary, &entries, &roots),
            Err(reason) => {
                report_not_derived(&binary, kind, &reason);
                continue;
            }
        };
        if dirs.is_empty() {
            report_not_derived(&binary, kind, "it reported no existing directory to grant");
            continue;
        }
        let libraries = extension_module_closure(&binary, &dirs, &roots);
        derived.push(DerivedInterpreterRuntime {
            binary,
            dirs,
            libraries,
        });
    }
    derived
}

#[cfg(target_os = "linux")]
fn report_not_derived(binary: &Path, kind: InterpreterKind<'_>, reason: &str) {
    crate::runtime_err!(
        "[capsule-runtime] the runtime directories of {} interpreter {} were not derived: {reason}",
        kind.name,
        binary.display()
    );
}

/// The first kind whose marker symbol `image`'s `.dynsym` lists. `None` for an ELF that lists
/// none of them and for anything that is not ELF, a `#!` script included.
#[cfg(target_os = "linux")]
pub(crate) fn recognise<'a>(
    image: &[u8],
    kinds: &[InterpreterKind<'a>],
) -> Option<InterpreterKind<'a>> {
    let names = crate::sandbox::elf_dynamic_symbol_names(image)?;
    kinds
        .iter()
        .find(|kind| names.iter().any(|name| name == kind.marker_symbol))
        .copied()
}

/// Runs `binary` with `kind`'s arguments, an empty environment, `/dev/null` as standard input and
/// `/` as working directory, and returns the NUL-separated entries it printed. Killed at
/// `timeout`. Standard error is read only to explain a failure.
#[cfg(target_os = "linux")]
fn query(
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

/// The `DT_NEEDED` closure of every ELF shared object directly inside one of the directories in
/// `dirs` (no recursion), less the files inside `dirs` themselves and those in `binary`'s own ELF
/// closure, which `shell.allow` already grants. Nothing under `roots` is read or returned.
#[cfg(target_os = "linux")]
fn extension_module_closure(binary: &Path, dirs: &[PathBuf], roots: &[PathBuf]) -> Vec<PathBuf> {
    use std::io::Read;

    let mut seeds = Vec::new();
    for dir in dirs {
        let Ok(listing) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in listing.flatten() {
            let path = entry.path();
            if !std::fs::metadata(&path).is_ok_and(|m| m.is_file()) {
                continue;
            }
            let mut head = [0u8; 18];
            let is_shared_object = std::fs::File::open(&path)
                .and_then(|mut file| file.read_exact(&mut head))
                .is_ok()
                && crate::sandbox::elf_is_shared_object(&head);
            if is_shared_object {
                seeds.push(path);
            }
        }
    }
    if seeds.is_empty() {
        return Vec::new();
    }
    let search_dirs: Vec<PathBuf> = crate::sandbox::DEFAULT_LIBRARY_SEARCH_DIRS
        .iter()
        .map(PathBuf::from)
        .collect();
    let own_closure = crate::sandbox::resolve_landlock_grants(&[binary.to_path_buf()]);
    crate::sandbox::resolve_landlock_grants_outside(&seeds, &search_dirs, roots)
        .into_iter()
        .filter(|path| !dirs.iter().any(|dir| path.starts_with(dir)))
        .filter(|path| !own_closure.contains(path))
        .collect()
}

#[cfg(all(test, target_os = "linux"))]
#[allow(clippy::print_stdout)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A kind that recognises `dash` (whose `.dynsym` lists `execve`) and runs `args` with it.
    fn dash_kind<'a>(args: &'a [&'a str]) -> InterpreterKind<'a> {
        InterpreterKind {
            name: "test",
            marker_symbol: "execve",
            args,
        }
    }

    /// A copy of the host's `dash` named `name` in a fresh directory under `parent`, or `None`
    /// with a skip line when the host has none.
    fn dash_copy(parent: &Path, name: &str) -> Option<PathBuf> {
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

    fn canonical_tempdir() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        (temp, root)
    }

    #[test]
    fn cpython_is_recognised_by_py_bytes_main() {
        let Some(python) = crate::sandbox::find_on_path("python3") else {
            println!("[SKIP] no python3 on PATH");
            return;
        };
        let image = std::fs::read(std::fs::canonicalize(python).unwrap()).unwrap();
        assert_eq!(
            recognise(&image, INTERPRETER_KINDS).unwrap().name,
            "CPython"
        );
    }

    #[test]
    fn perl_is_recognised_by_perl_parse() {
        let Some(perl) = crate::sandbox::find_on_path("perl") else {
            println!("[SKIP] no perl on PATH");
            return;
        };
        let image = std::fs::read(std::fs::canonicalize(perl).unwrap()).unwrap();
        assert_eq!(recognise(&image, INTERPRETER_KINDS).unwrap().name, "Perl");
    }

    #[test]
    fn a_plain_elf_and_a_script_are_not_interpreters() {
        let test_binary = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        assert!(
            crate::sandbox::elf_dynamic_symbol_names(&test_binary).is_some(),
            "the test binary is a dynamic ELF with a .dynsym to read"
        );
        assert_eq!(recognise(&test_binary, INTERPRETER_KINDS), None);
        if let Ok(true_binary) = std::fs::read("/bin/true") {
            assert_eq!(recognise(&true_binary, INTERPRETER_KINDS), None);
        }
        let script = b"#!/usr/bin/python3\nimport sys\n# Py_BytesMain perl_parse\n";
        assert_eq!(recognise(script, INTERPRETER_KINDS), None);
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
        );
        assert!(derived.is_empty());
        assert!(!marker.exists(), "the excluded binary was executed");

        // The same stand-in outside every excluded root is recognised and run, so the absence
        // of the marker above is the exclusion and not a stand-in that never runs.
        derive_in(&allow, &[], &[], &path_dirs, &kinds, QUERY_TIMEOUT);
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
        );
        assert!(derived.is_empty());
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
            let derived = derive_in(&allow, &[], &[], &path_dirs, &kinds, timeout);
            if case == "answers" {
                assert_eq!(derived.len(), 1, "{case}");
                assert_eq!(derived[0].dirs, vec![lib.clone()], "{case}");
            } else {
                assert!(derived.is_empty(), "{case}: {derived:?}");
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "{case} took {:?}",
                started.elapsed()
            );
        }
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

    /// The derivation for this host's `python3` and `perl`: each one on `PATH` is derived, none of
    /// its entries is its own directory or an ancestor of it, and the time taken is printed for the
    /// staging cost it adds.
    #[test]
    fn this_hosts_interpreters_derive_their_runtimes() {
        let allow = vec!["python3".to_string(), "perl".to_string()];
        let started = std::time::Instant::now();
        let derived = derive_interpreter_runtimes(&allow, &[], &[]);
        let elapsed = started.elapsed();
        for name in &allow {
            let Some(found) = crate::sandbox::find_on_path(name) else {
                println!("[SKIP] no {name} on PATH");
                continue;
            };
            let binary = std::fs::canonicalize(found).unwrap();
            assert!(
                derived
                    .iter()
                    .any(|runtime| runtime.binary == binary && !runtime.dirs.is_empty()),
                "{name} ({}) derived nothing: {derived:?}",
                binary.display()
            );
        }
        for runtime in &derived {
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
        println!("derive_interpreter_runtimes([python3, perl]) took {elapsed:?}");
    }
}
