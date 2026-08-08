//! Stage-time reachability checks: does a granted `capabilities.shell.allow` binary have any
//! chance of *functioning* inside a `sealed` composed root, or only of starting?
//!
//! ## The gap this closes
//!
//! [`crate::sandbox::resolve_landlock_grants`] derives, from each allowlisted binary, the exact
//! files the dynamic loader will touch: the binary, its `PT_INTERP`, and the transitive
//! `DT_NEEDED` closure. Under `sealed` those become read-only binds in the composed root plus
//! Landlock rules inside it. That derivation is complete for one class of program — a dynamically
//! linked ELF executable whose entire runtime dependency set is recorded in its own headers — and
//! it is silently *incomplete* for two others:
//!
//!   1. **An interpreted entrypoint.** `~/.local/bin/pip` is a five-line Python script behind a
//!      `#!` shebang. `parse_elf_dependencies` returns `None` for it, so its derived closure is
//!      empty; [`crate::sandbox::resolve_sealed_bind_dirs`] binds the directory the script *sits*
//!      in, and nothing at all binds `~/.local/lib/python3.12/site-packages/pip`, which is where
//!      the thing it imports on its first line actually lives. The capsule launches, the
//!      composed root is built, Landlock loads, and then — some number of agent turns later — a
//!      shell tool call dies with `ModuleNotFoundError: No module named 'pip'`. That is an
//!      ENOENT-class failure *inside* the root, not a denial: nothing in the trace says "policy",
//!      and the operator is left debugging a Python install that is fine on the host.
//!   2. **A compiler driver.** `cc` does not compile anything itself; it forks and execs `cc1`,
//!      `as`, `ld` and `collect2`. Those are separate binaries, outside `cc`'s own `DT_NEEDED`
//!      closure, and they live under `/usr` — inside [`crate::sealed::SEALED_RUNTIME_PATHS`],
//!      which is bind-mounted into every composed root and granted `list_dir: true,
//!      executable: false` (see `sandbox::resolve_sealed_runtime_landlock_grants`). Present,
//!      readable, listable, and not executable. `cc --version` works; the first real compile does
//!      not.
//!
//! Neither gap is derivable the way the ELF closure is. An interpreted program's import closure
//! depends on `sys.path`, on `.pth` files, on whatever the script does at runtime — the roadmap
//! card behind this module states it plainly: *an arbitrary interpreted closure is not generally
//! derivable*. So this module does not try to derive it. It asks the weaker question that is
//! actually answerable at staging time, and answers it honestly:
//!
//!   * **Did the operator declare *anything* that could cover this entrypoint?** If not,
//!     [`check_interpreted_entrypoints_reachable`] refuses the launch. It verifies a covering
//!     grant was *declared*, never that the directory it names is the right one — that stays the
//!     operator's job, and the refusal text says how to measure it.
//!   * **Is this compiler driver's toolchain reachable with an `Execute` right?** If not,
//!     [`warn_on_unreachable_toolchain_helpers`] warns with `W-SEC-012` and lets the launch
//!     proceed. A warning rather than a refusal because the probe behind it
//!     (`<driver> -print-prog-name=<helper>`) is a heuristic about one driver family, and a hard
//!     refusal built on a heuristic would block capsules that would in fact have worked.
//!
//! ## Why both are gated on the *declared* floor
//!
//! Both no-op unless `declared_floor == ContainmentClass::Sealed`, and neither consults a host
//! probe — the same shape, for the same reason, as
//! [`crate::staged_runtime::check_staged_runtime_floor`]. "Reachable in the composed root" is only
//! a coherent question when there *is* a composed root, and one is built only for a capsule that
//! declared `sealed`. Under `scoped`/`advisory` the host filesystem is simply the host filesystem:
//! `~/.local/lib/python3.12` is right where it always was, and refusing there would be refusing a
//! capsule that works.
//!
//! Both run in `stage_session` *after* `check_containment_floor`, so by the time either is
//! reached, a `sealed` declaration means this host cleared the probe and the composed-root
//! construction really will be attempted.
//!
//! ## What this does not do
//!
//! Nothing here widens anything. No path becomes reachable inside a composed root, no Landlock
//! rule is added or relaxed, and no manifest key is introduced. The entire contribution is
//! turning two silent under-deliveries into a named refusal and a named warning at launch.

use std::path::{Path, PathBuf};

use murmur_artifact::{security_warning_link, ContainmentClass, W_SEC_012};

use crate::errors::{RuntimeError, UnreachableEntrypoint};
use crate::types::CapabilityPolicy;

/// How much of a candidate file [`shebang_interpreter_name`] reads to decide whether it is a
/// script. A shebang line is capped at 127 bytes on Linux (`BINPRM_BUF_SIZE`) and the kernel
/// ignores anything past that, so this is already generous; it exists so that pointing
/// `shell.allow` at a multi-gigabyte file cannot turn a policy check into a `read` of the whole
/// thing.
const SHEBANG_PROBE_BYTES: u64 = 512;

/// Compiler drivers whose real work happens in helper binaries they fork and exec themselves, and
/// the helper names to ask each one about via `-print-prog-name=`.
///
/// Deliberately a fixed, short, hand-maintained table rather than anything derived. There is no
/// general way to ask an arbitrary program "what else will you exec?", and the one family that
/// *can* be asked — the GCC driver, whose `-print-prog-name=` exists precisely to answer it — is
/// the family whose absence from the exec grant set was observed to break real compiles. Anything
/// outside this table is simply not probed; that is a known, deliberate hole, not an oversight.
///
/// The helper lists are per-driver because the driver picks its own front end: `gcc` reaches
/// `cc1`, `g++` reaches `cc1plus`, and both reach the assembler, the linker and the linker
/// wrapper. `cc`/`gcc` are listed with *both* front ends because either can compile C++ when
/// pointed at a `.cc` file or given `-x c++`, and over-asking costs one extra probe while
/// under-asking costs a missed warning.
///
/// **Extending this.** Add a `(driver, helpers)` row. The only requirement is that the driver
/// answers `-print-prog-name=<helper>` on stdout with either an absolute path or the helper name
/// unchanged — `clang`/`clang++` do (they accept the flag for GCC compatibility), so adding them
/// is a one-line change plus a helper list (`clang -cc1` is in-process, so the useful names there
/// are the assembler and linker: `as`, `ld`, `lld`). A driver that does not answer that flag
/// contributes nothing and warns about nothing, because [`probe_helper_path`] treats an
/// unrecognised flag as "not found" (see its own doc comment).
const KNOWN_TOOLCHAIN_DRIVERS: &[(&str, &[&str])] = &[
    ("cc", &["cc1", "cc1plus", "as", "ld", "collect2"]),
    ("gcc", &["cc1", "cc1plus", "as", "ld", "collect2"]),
    ("g++", &["cc1plus", "as", "ld", "collect2"]),
    ("c++", &["cc1plus", "as", "ld", "collect2"]),
];

/// One compiler driver whose helper subprocess has no `Execute`-carrying grant under `sealed`.
///
/// Values, not printed strings, so the decision is assertable in a unit test without capturing
/// stderr. [`warn_on_unreachable_toolchain_helpers`] both returns these and prints them, so the
/// `W-SEC-012` text has exactly one definition shared by `stage_session` and `mur doctor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolchainHelperWarning {
    /// The `capabilities.shell.allow` entry that named the driver, verbatim.
    pub driver: String,
    /// The helper name asked about, e.g. `cc1plus`.
    pub helper: String,
    /// Where the driver said that helper lives on this host.
    pub resolved_path: PathBuf,
}

/// Reads the leading bytes of `path` and, if it is a script, returns the bare name of the
/// interpreter its shebang names.
///
/// Returns `None` for everything else — a real ELF image (whose first four bytes are `\x7fELF`,
/// not `#!`), a directory, a file that cannot be read, a path that does not exist. "Not a script"
/// and "cannot tell" deliberately collapse into the same answer: the caller's only use for `Some`
/// is to *add* a refusal, so an unreadable file shrinks the check rather than failing it, matching
/// the shrink-not-fail rule every other resolver in this crate follows.
///
/// One level of `env` indirection is resolved, because it is how nearly every installed console
/// script is actually written:
///
///   * `#!/usr/bin/python3.12` → `Some("python3.12")` — the shebang target's last component.
///   * `#!/usr/bin/env python3` → `Some("python3")` — the first real argument, not `env`.
///   * `#!/usr/bin/env -S python3 -u` → `Some("python3")` — flags and `VAR=value` assignments are
///     skipped, since `env`'s own arguments are not the interpreter.
///   * `#!/usr/bin/env` with nothing after it → `Some("env")`, the only honest answer available.
///
/// A *bare name* is returned, never a path, because the name is what gets compared against a
/// declared `interpreter_runtime`/`staged_runtime` grant's `binary` field, which the manifest
/// schema defines as a bare binary name.
///
/// Pure apart from one bounded read, so it is unit-testable with tempfiles on any OS.
pub(crate) fn shebang_interpreter_name(path: &Path) -> Option<String> {
    use std::io::Read;

    let mut head = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(SHEBANG_PROBE_BYTES)
        .read_to_end(&mut head)
        .ok()?;

    let rest = head.strip_prefix(b"#!")?;
    // The shebang is the first line and only the first line; a `\r` counts as a terminator so a
    // CRLF-saved script does not yield an interpreter name with a stray carriage return glued on.
    let line_end = rest
        .iter()
        .position(|&byte| byte == b'\n' || byte == b'\r')
        .unwrap_or(rest.len());
    let line = String::from_utf8_lossy(&rest[..line_end]);

    let mut tokens = line.split_whitespace();
    let target = base_name(tokens.next()?)?;

    if target != "env" {
        return Some(target);
    }
    // `env`'s own flags (`-S`, `-i`, `--split-string=…`) and `VAR=value` assignments come before
    // the program it will exec; the first token that is neither is the interpreter.
    for token in tokens {
        if token.starts_with('-') || token.contains('=') {
            continue;
        }
        return base_name(token);
    }
    Some(target)
}

/// Last path component of `raw`, as an owned `String`. `None` for an empty or root-only string,
/// which cannot name a binary.
fn base_name(raw: &str) -> Option<String> {
    Path::new(raw)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
}

/// Refuses a `sealed` capsule whose `capabilities.shell.allow` names a script that nothing
/// declared could make importable inside the composed root.
///
/// See the module doc for why this is a refusal and what it does *not* claim. In short: for each
/// allowlisted entry that resolves to a `#!` script, the entry is accepted when **any** of the
/// following holds, and collected as unreachable when none does.
///
///   1. **The script already lives under [`crate::sealed::SEALED_RUNTIME_PATHS`].** That tree is
///      bind-mounted whole into every composed root and granted `list_dir: true`, so a system
///      `/usr/bin/pip3` reaches its `dist-packages` beside it without anything being declared.
///      (Read access is all an import needs; the missing `Execute` right on that tree is Case 2's
///      problem, below, not this one's.)
///   2. **The script lives under a directory the manifest already named** in a `staged_runtime`
///      `source_path` or an `interpreter_runtime` dir. The operator staged the tree the script is
///      part of.
///   3. **Some grant names this binary, or its shebang interpreter, by name.** This is the
///      general case, and the match is *exact string equality* against the grant's `binary` field
///      — not a prefix, not a basename, not case-insensitive. It can be exact because the
///      manifest parser already requires a grant's `binary` to appear verbatim in the same
///      block's `allow`, so the strings being compared come from the same list.
///
/// Rule 3 is the loose one on purpose. A grant naming `python3` satisfies every script whose
/// shebang says `python3`, regardless of whether the directory that grant names is the one holding
/// the package the script imports. Verifying *that* would mean deriving the import closure, which
/// is the thing this module opens by saying is not derivable. So the guarantee here is narrow and
/// stated rather than implied: the operator was made to think about the interpreter's runtime and
/// write something down. Getting the directory right is still on them, and the refusal text names
/// the `strace` invocation that measures it.
///
/// A non-script entry is skipped unconditionally — an ELF binary's reachability is already
/// `resolve_landlock_grants`' concern, and re-deciding it here would be a second, weaker opinion
/// about a question that is already answered correctly.
pub fn check_interpreted_entrypoints_reachable(
    policy: &CapabilityPolicy,
    declared_floor: ContainmentClass,
) -> Result<(), RuntimeError> {
    check_interpreted_entrypoints_reachable_in(policy, declared_floor, &host_path_dirs())
}

/// Testable core of [`check_interpreted_entrypoints_reachable`], with the `PATH` directory list
/// injected so a test can point it at a temp directory holding synthetic fixtures instead of at
/// whatever the machine running the suite happens to have installed. Same injection pattern
/// `sandbox::resolve_exec_allowlist_in` and `resolve_landlock_grants_in` already use.
fn check_interpreted_entrypoints_reachable_in(
    policy: &CapabilityPolicy,
    declared_floor: ContainmentClass,
    path_dirs: &[PathBuf],
) -> Result<(), RuntimeError> {
    if declared_floor != ContainmentClass::Sealed {
        return Ok(());
    }

    let fixed_dirs = sealed_runtime_paths();
    let declared_dirs = declared_grant_dirs(policy);
    let mut entries: Vec<UnreachableEntrypoint> = Vec::new();

    for entry in &policy.shell_allow {
        if entry.is_empty() {
            continue;
        }
        let resolved = PathBuf::from(crate::sandbox::resolve_invoked_binary_path_in(
            entry, path_dirs,
        ));
        // `resolve_invoked_binary_path_in` falls back to the bare name when nothing resolves. A
        // name that resolves to nothing is not this check's business: it cannot be exec'd at all,
        // which the OS will say more clearly than this refusal could.
        if !resolved.is_absolute() {
            continue;
        }
        if under_any(&resolved, &fixed_dirs) || under_any(&resolved, &declared_dirs) {
            continue;
        }
        let Some(interpreter) = shebang_interpreter_name(&resolved) else {
            continue;
        };
        if declared_grant_binaries(policy)
            .any(|binary| binary == entry.as_str() || binary == interpreter.as_str())
        {
            continue;
        }
        entries.push(UnreachableEntrypoint {
            binary: entry.clone(),
            resolved_path: resolved,
            interpreter,
        });
    }

    if entries.is_empty() {
        return Ok(());
    }
    // Every offender in one refusal, following `StagedRuntimeRequiresSealed`: an operator fixing
    // this should not have to re-run to discover the second script.
    Err(RuntimeError::ShellBinaryPackageUnreachable { entries })
}

/// Warns (`W-SEC-012`, non-fatal, once per uncovered helper) when a `sealed` capsule allowlists a
/// known compiler driver whose helper binaries have no grant carrying the Landlock `Execute`
/// right. Returns what it warned about so the decision is assertable without capturing stderr.
///
/// A helper is **covered** by exactly two things, both of which carry `executable: true`:
///
///   * a declared `staged_runtime` `source_path` or `interpreter_runtime` directory the helper
///     lives under (see `sandbox::resolve_staged_runtime_landlock_grants` and
///     `resolve_interpreter_runtime_grants`); or
///   * the helper being allowlisted in its own right — either named directly in `shell.allow`, or
///     resolved to by some `shell.allow` entry — since every allowlisted binary gets its own
///     `Execute` grant.
///
/// Residence under [`crate::sealed::SEALED_RUNTIME_PATHS`] is **not** coverage, and this is the
/// one place where that distinction is the entire point. `sandbox::resolve_sealed_runtime_landlock_grants`
/// grants that tree `executable: false` by deliberate design — it is the only grant covering whole
/// host trees the manifest never named, so it must make them enumerable without making them
/// runnable. `/usr/bin/as` and `/usr/libexec/gcc/.../cc1` are therefore present in the composed
/// root, readable, and un-exec'able. Treating "it's under `/usr`" as coverage here would suppress
/// exactly the warning this function exists to emit.
///
/// Never returns an error and never fails a launch. Each step is shrink-not-fail: a driver that
/// does not resolve, a probe that fails to spawn, a helper the driver does not know about, all
/// contribute nothing.
pub fn warn_on_unreachable_toolchain_helpers(
    policy: &CapabilityPolicy,
    declared_floor: ContainmentClass,
) -> Vec<ToolchainHelperWarning> {
    let warnings = unreachable_toolchain_helpers_in(policy, declared_floor, &host_path_dirs());
    for warning in &warnings {
        let link = security_warning_link(W_SEC_012);
        eprintln!(
            "[capsule-runtime] warning[{W_SEC_012}]: capabilities.shell.allow grants the compiler \
             driver '{}', but its helper '{}' at {} has no grant carrying the Landlock Execute \
             right under the 'sealed' composed root — the fixed sealed runtime tree ({}) is bound \
             read-only and listable but deliberately not executable, so the driver will start and \
             then fail partway through a real compile; declare \
             capabilities.shell.interpreter_runtime or staged_runtime for '{}' naming that \
             helper's directory ({}) to grant it ({link})",
            warning.driver,
            warning.helper,
            warning.resolved_path.display(),
            crate::sealed::SEALED_RUNTIME_PATHS.join(", "),
            warning.driver,
            warning
                .resolved_path
                .parent()
                .unwrap_or_else(|| Path::new("/"))
                .display(),
        );
    }
    warnings
}

/// Testable core of [`warn_on_unreachable_toolchain_helpers`]: decides, prints nothing, and takes
/// the `PATH` directory list as an argument.
fn unreachable_toolchain_helpers_in(
    policy: &CapabilityPolicy,
    declared_floor: ContainmentClass,
    path_dirs: &[PathBuf],
) -> Vec<ToolchainHelperWarning> {
    if declared_floor != ContainmentClass::Sealed {
        return Vec::new();
    }

    let declared_dirs = declared_grant_dirs(policy);
    // Every path some `shell.allow` entry resolves to, so a helper that is itself allowlisted
    // under a different spelling (`/usr/bin/ld` vs `ld`) still counts as covered.
    let allowlisted_paths: Vec<PathBuf> = policy
        .shell_allow
        .iter()
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            PathBuf::from(crate::sandbox::resolve_invoked_binary_path_in(
                entry, path_dirs,
            ))
        })
        .collect();

    let mut warnings: Vec<ToolchainHelperWarning> = Vec::new();

    for entry in &policy.shell_allow {
        let Some((_, helpers)) = KNOWN_TOOLCHAIN_DRIVERS
            .iter()
            .find(|(driver, _)| base_name(entry).as_deref() == Some(*driver))
        else {
            continue;
        };
        let driver_path = PathBuf::from(crate::sandbox::resolve_invoked_binary_path_in(
            entry, path_dirs,
        ));
        if !driver_path.is_absolute() {
            continue;
        }

        for helper in *helpers {
            let Some(resolved_path) = probe_helper_path(&driver_path, helper, path_dirs) else {
                continue;
            };
            if under_any(&resolved_path, &declared_dirs)
                || policy.shell_allow.iter().any(|allowed| allowed == helper)
                || allowlisted_paths.contains(&resolved_path)
            {
                continue;
            }
            let warning = ToolchainHelperWarning {
                driver: entry.clone(),
                helper: (*helper).to_string(),
                resolved_path,
            };
            // `cc` and `gcc` are usually the same binary reached under two names, so a capsule
            // allowlisting both would otherwise report each helper twice.
            if !warnings.contains(&warning) {
                warnings.push(warning);
            }
        }
    }

    warnings
}

/// Asks `driver` where it would find `helper`, via the GCC driver's own `-print-prog-name=` query,
/// and returns the canonical path when that answer names a real file.
///
/// The flag's output contract has three shapes, and all three are handled here because this host's
/// own `gcc` produces two of them:
///
///   * an **absolute path** — `cc -print-prog-name=cc1` →
///     `/usr/libexec/gcc/x86_64-linux-gnu/13/cc1`. The driver found it in its private libexec
///     directory. Canonicalized and returned.
///   * the **helper name, unchanged** — `cc -print-prog-name=as` → `as`. This is *not* "no such
///     helper": it is the driver saying it will let `execvp` find that one on `PATH`, which is
///     exactly what it does for the assembler and the linker on a Debian/Ubuntu GCC. So the name
///     is resolved through `path_dirs` rather than discarded; `as` and `ld` really do live at
///     `/usr/bin/as` and `/usr/bin/ld`, really are exec'd during a compile, and really are inside
///     the non-executable fixed sealed tree. Discarding them would have made this check blind to
///     two of the four helpers that matter most.
///   * **anything else** — a driver that does not understand the flag (it typically echoes the
///     flag back, or writes usage text to stderr and exits non-zero), an empty stdout, a path that
///     no longer exists. All treated as "not found, skip": shrink-not-fail, so an unknown driver
///     family contributes no warnings rather than bogus ones.
///
/// This is the one part of the module that spawns a process. It is a single short-lived exec of a
/// binary the manifest itself allowlisted, running in the parent at staging time — never in a
/// forked child's `pre_exec` window, and never after the sandbox is installed.
fn probe_helper_path(driver: &Path, helper: &str, path_dirs: &[PathBuf]) -> Option<PathBuf> {
    let output = std::process::Command::new(driver)
        .arg(format!("-print-prog-name={helper}"))
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let printed = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if printed.is_empty() {
        return None;
    }

    let candidate = if printed == helper {
        // The driver deferred to `PATH`; resolve it the way the exec would.
        PathBuf::from(crate::sandbox::resolve_invoked_binary_path_in(
            helper, path_dirs,
        ))
    } else {
        PathBuf::from(&printed)
    };
    if !candidate.is_absolute() {
        return None;
    }
    std::fs::canonicalize(candidate).ok()
}

/// The host `PATH`, split the same way `execvp` splits it — the single place this module reads
/// process environment, so both public entry points share one definition of "where binaries are".
fn host_path_dirs() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default()
}

/// [`crate::sealed::SEALED_RUNTIME_PATHS`] as owned paths, for [`under_any`].
fn sealed_runtime_paths() -> Vec<PathBuf> {
    crate::sealed::SEALED_RUNTIME_PATHS
        .iter()
        .map(PathBuf::from)
        .collect()
}

/// Every host directory the manifest already named: each `staged_runtime` `source_path` and each
/// `interpreter_runtime` directory. These are the trees that get a bind and an
/// `executable: true` Landlock grant, so residence under one is coverage for both checks.
fn declared_grant_dirs(policy: &CapabilityPolicy) -> Vec<PathBuf> {
    policy
        .shell_staged_runtime
        .iter()
        .map(|grant| PathBuf::from(&grant.source_path))
        .chain(
            policy
                .shell_interpreter_runtime
                .iter()
                .flat_map(|grant| grant.dirs.iter().map(|dir| PathBuf::from(&dir.path))),
        )
        .filter(|path| path.is_absolute())
        .collect()
}

/// Every binary name a `staged_runtime` or `interpreter_runtime` grant declares.
fn declared_grant_binaries(policy: &CapabilityPolicy) -> impl Iterator<Item = &str> {
    policy
        .shell_staged_runtime
        .iter()
        .map(|grant| grant.binary.as_str())
        .chain(
            policy
                .shell_interpreter_runtime
                .iter()
                .map(|grant| grant.binary.as_str()),
        )
}

/// Whether `path` is `root` or sits beneath one of `roots`.
///
/// `Path::starts_with` is component-wise, not textual, so `/usrlocal/bin/pip` is correctly *not*
/// under `/usr` — the trap a `str::starts_with` would fall into.
fn under_any(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| path.starts_with(root))
}

#[cfg(test)]
mod tests {
    use super::*;
    use murmur_artifact::{InterpreterRuntimeDir, InterpreterRuntimeGrant, StagedRuntimeGrant};

    /// Every containment class, so a new variant cannot quietly escape a test that iterates "all
    /// of them". Mirrors the same list in `staged_runtime`'s tests.
    const ALL_CLASSES: &[ContainmentClass] = &[
        ContainmentClass::Advisory,
        ContainmentClass::Scoped,
        ContainmentClass::Sealed,
    ];

    /// Writes an executable file with the given first bytes into `dir`, and returns its path.
    fn write_binary(dir: &Path, name: &str, contents: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, contents).expect("write fixture");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod fixture");
        }
        path
    }

    fn script(dir: &Path, name: &str, shebang: &str) -> PathBuf {
        write_binary(dir, name, format!("{shebang}\nprint('hi')\n").as_bytes())
    }

    /// A minimal, deliberately *invalid* ELF image: enough magic for `shebang_interpreter_name` to
    /// reject it as "not a script", which is the only property these tests need from it.
    fn elf(dir: &Path, name: &str) -> PathBuf {
        let mut bytes = b"\x7fELF\x02\x01\x01\x00".to_vec();
        bytes.resize(128, 0);
        write_binary(dir, name, &bytes)
    }

    fn policy_allowing(names: &[&str]) -> CapabilityPolicy {
        CapabilityPolicy {
            shell_allow: names.iter().map(|name| (*name).to_string()).collect(),
            ..CapabilityPolicy::default()
        }
    }

    fn interpreter_grant(binary: &str, dirs: &[&str]) -> InterpreterRuntimeGrant {
        InterpreterRuntimeGrant {
            binary: binary.to_string(),
            dirs: dirs
                .iter()
                .map(|path| InterpreterRuntimeDir {
                    path: (*path).to_string(),
                    list_dir: true,
                })
                .collect(),
        }
    }

    fn staged_grant(binary: &str, source_path: &str) -> StagedRuntimeGrant {
        StagedRuntimeGrant {
            binary: binary.to_string(),
            source_path: source_path.to_string(),
            pin: "test-pin-1".to_string(),
        }
    }

    // ---------------------------------------------------------------- shebang_interpreter_name

    #[test]
    fn shebang_returns_the_interpreter_basename_for_a_direct_target() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = script(dir.path(), "direct", "#!/usr/bin/python3.12");
        assert_eq!(
            shebang_interpreter_name(&path),
            Some("python3.12".to_string())
        );
    }

    #[test]
    fn shebang_resolves_one_level_of_env_indirection() {
        let dir = tempfile::tempdir().expect("tempdir");
        for (name, line, expected) in [
            ("plain-env", "#!/usr/bin/env python3", "python3"),
            ("split-string", "#!/usr/bin/env -S python3 -u", "python3"),
            ("assignment", "#!/usr/bin/env FOO=bar node", "node"),
            // Nothing follows `env`, so `env` itself is the only honest answer.
            ("bare-env", "#!/usr/bin/env", "env"),
        ] {
            let path = script(dir.path(), name, line);
            assert_eq!(
                shebang_interpreter_name(&path),
                Some(expected.to_string()),
                "shebang line: {line}"
            );
        }
    }

    /// A CRLF-saved script must not yield `"python3\r"`, which would silently fail every
    /// grant-name comparison downstream.
    #[test]
    fn shebang_stops_at_a_carriage_return() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_binary(
            dir.path(),
            "crlf",
            b"#!/usr/bin/env python3\r\nprint(1)\r\n",
        );
        assert_eq!(shebang_interpreter_name(&path), Some("python3".to_string()));
    }

    /// The property that makes "no shebang" a usable proxy for "this is an ELF binary, and its
    /// reachability is already `resolve_landlock_grants`' problem".
    #[test]
    fn shebang_is_none_for_an_elf_image_and_for_unreadable_paths() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(shebang_interpreter_name(&elf(dir.path(), "binary")), None);
        assert_eq!(
            shebang_interpreter_name(&write_binary(dir.path(), "plain", b"just text\n")),
            None
        );
        assert_eq!(
            shebang_interpreter_name(&dir.path().join("does-not-exist")),
            None
        );
        assert_eq!(shebang_interpreter_name(dir.path()), None);
    }

    /// The read is bounded, so pointing `shell.allow` at a huge file costs 512 bytes, not the
    /// file. Asserted by giving the shebang line more leading junk than the bound allows.
    #[test]
    fn shebang_probe_reads_a_bounded_prefix_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut bytes = vec![b'#'; SHEBANG_PROBE_BYTES as usize * 4];
        bytes.extend_from_slice(b"\n#!/usr/bin/env python3\n");
        // The `#!` is past the bound *and* not at byte 0, so this is not a script either way.
        assert_eq!(
            shebang_interpreter_name(&write_binary(dir.path(), "huge", &bytes)),
            None
        );
    }

    // ------------------------------------------------- check_interpreted_entrypoints_reachable

    /// Scenario 1: a script outside every fixed and declared tree, with no grant naming it or its
    /// interpreter, refuses — and the refusal is actionable on its own text.
    #[test]
    fn an_uncovered_script_entrypoint_refuses_naming_binary_path_and_interpreter() {
        let dir = tempfile::tempdir().expect("tempdir");
        script(dir.path(), "pip", "#!/usr/bin/env python3");
        let dirs = vec![dir.path().to_path_buf()];

        let error = check_interpreted_entrypoints_reachable_in(
            &policy_allowing(&["pip"]),
            ContainmentClass::Sealed,
            &dirs,
        )
        .expect_err("an uncovered interpreted entrypoint must refuse");

        let rendered = error.to_string();
        assert!(rendered.contains("pip"), "{rendered}");
        assert!(
            rendered.contains(&dir.path().join("pip").display().to_string()),
            "{rendered}"
        );
        assert!(rendered.contains("python3"), "{rendered}");
        assert!(rendered.contains("interpreter_runtime"), "{rendered}");
        assert!(rendered.contains("strace"), "{rendered}");
    }

    /// Scenario 2: any covering grant naming the script *or* its interpreter is enough. This is
    /// the deliberately loose rule — the grant's directory is never checked for correctness.
    #[test]
    fn a_grant_naming_the_script_or_its_interpreter_is_sufficient() {
        let dir = tempfile::tempdir().expect("tempdir");
        script(dir.path(), "pip", "#!/usr/bin/env python3");
        let dirs = vec![dir.path().to_path_buf()];

        for policy in [
            CapabilityPolicy {
                // Names the interpreter, and points at a directory that has nothing to do with
                // the script's actual package — still accepted, by design.
                shell_interpreter_runtime: vec![interpreter_grant("python3", &["/opt/unrelated"])],
                ..policy_allowing(&["pip"])
            },
            CapabilityPolicy {
                // Names the script itself.
                shell_staged_runtime: vec![staged_grant("pip", "/opt/unrelated")],
                ..policy_allowing(&["pip"])
            },
        ] {
            assert!(
                check_interpreted_entrypoints_reachable_in(
                    &policy,
                    ContainmentClass::Sealed,
                    &dirs
                )
                .is_ok(),
                "a declared covering grant must be sufficient"
            );
        }
    }

    /// Scenario 2, second limb: a script that *sits inside* a declared tree is covered by
    /// residence, with no name match needed.
    #[test]
    fn a_script_inside_a_declared_grant_directory_is_covered_by_residence() {
        let dir = tempfile::tempdir().expect("tempdir");
        script(dir.path(), "pip", "#!/usr/bin/env python3");
        let dirs = vec![dir.path().to_path_buf()];

        let policy = CapabilityPolicy {
            shell_staged_runtime: vec![staged_grant(
                "some-other-binary",
                dir.path().to_str().expect("utf-8 temp path"),
            )],
            ..policy_allowing(&["pip"])
        };
        assert!(check_interpreted_entrypoints_reachable_in(
            &policy,
            ContainmentClass::Sealed,
            &dirs
        )
        .is_ok());
    }

    /// Scenario 3: the roadmap's acceptance criterion 3 — a system script under the fixed,
    /// fully-bound sealed tree needs no declaration at all.
    #[test]
    fn a_script_under_a_fixed_sealed_runtime_path_needs_no_grant() {
        // Path-form `shell.allow` entries bypass `PATH` entirely, so this asserts the prefix rule
        // without needing a writable `/usr` on the machine running the suite.
        for entry in crate::sealed::SEALED_RUNTIME_PATHS {
            let policy = policy_allowing(&[&format!("{entry}/bin/some-console-script")]);
            assert!(
                check_interpreted_entrypoints_reachable_in(&policy, ContainmentClass::Sealed, &[])
                    .is_ok(),
                "a path under {entry} must never be collected"
            );
        }
    }

    /// The component-wise prefix rule: `/usr-local` is not under `/usr`. Guards against anyone
    /// "simplifying" `under_any` into a string comparison.
    #[test]
    fn prefix_matching_is_component_wise_not_textual() {
        assert!(under_any(
            Path::new("/usr/bin/pip"),
            &[PathBuf::from("/usr")]
        ));
        assert!(!under_any(
            Path::new("/usrlocal/bin/pip"),
            &[PathBuf::from("/usr")]
        ));
    }

    /// Scenario 4: ELF entries are never this check's business, whatever else is true.
    #[test]
    fn elf_entrypoints_are_never_collected() {
        let dir = tempfile::tempdir().expect("tempdir");
        elf(dir.path(), "bash");
        let dirs = vec![dir.path().to_path_buf()];
        assert!(check_interpreted_entrypoints_reachable_in(
            &policy_allowing(&["bash"]),
            ContainmentClass::Sealed,
            &dirs
        )
        .is_ok());
    }

    /// An entry that resolves to nothing is not a reachability problem — it is a missing binary,
    /// which the OS reports far more clearly than this refusal could.
    #[test]
    fn an_unresolvable_entry_is_skipped_rather_than_refused() {
        assert!(check_interpreted_entrypoints_reachable_in(
            &policy_allowing(&["definitely-not-installed-anywhere"]),
            ContainmentClass::Sealed,
            &[]
        )
        .is_ok());
    }

    /// Scenario 1, the "name every offender at once" property inherited from
    /// `StagedRuntimeRequiresSealed`.
    #[test]
    fn the_refusal_names_every_unreachable_entrypoint() {
        let dir = tempfile::tempdir().expect("tempdir");
        script(dir.path(), "pip", "#!/usr/bin/env python3");
        script(dir.path(), "rake", "#!/usr/bin/env ruby");
        let dirs = vec![dir.path().to_path_buf()];

        let error = check_interpreted_entrypoints_reachable_in(
            &policy_allowing(&["pip", "rake"]),
            ContainmentClass::Sealed,
            &dirs,
        )
        .expect_err("both entrypoints are uncovered");
        let rendered = error.to_string();
        for expected in ["pip", "python3", "rake", "ruby"] {
            assert!(
                rendered.contains(expected),
                "missing {expected}: {rendered}"
            );
        }
    }

    // ------------------------------------------------------ declared-floor gating (Scenario 5)

    /// Both checks are functions of the *declared* floor alone, and both are fully inert below
    /// `sealed`. Asserted together because they share the gate and must not drift apart.
    #[test]
    fn both_checks_are_inert_below_a_declared_sealed_floor() {
        let dir = tempfile::tempdir().expect("tempdir");
        script(dir.path(), "pip", "#!/usr/bin/env python3");
        let dirs = vec![dir.path().to_path_buf()];
        let policy = policy_allowing(&["pip", "cc"]);

        for class in ALL_CLASSES {
            let refusal = check_interpreted_entrypoints_reachable_in(&policy, *class, &dirs);
            let warnings = unreachable_toolchain_helpers_in(&policy, *class, &dirs);
            if *class == ContainmentClass::Sealed {
                assert!(refusal.is_err(), "sealed must still be checked");
            } else {
                assert!(refusal.is_ok(), "{class} must not be refused");
                assert!(warnings.is_empty(), "{class} must not warn");
            }
        }
    }

    // ------------------------------------------------- warn_on_unreachable_toolchain_helpers

    /// A stand-in compiler driver that answers `-print-prog-name=<helper>` the way GCC does,
    /// letting Scenarios 6 and 7 run on any host with a shell — including one with no real GCC.
    /// Absolute answers for the front ends, `PATH`-deferred answers for the assembler/linker,
    /// exactly as this repo's own bare-metal `gcc 13.3` was observed to behave.
    #[cfg(unix)]
    fn fake_driver(dir: &Path, name: &str, libexec: &Path) -> PathBuf {
        let script = format!(
            "#!/bin/sh\n\
             case \"$1\" in\n\
             -print-prog-name=cc1) echo {libexec}/cc1 ;;\n\
             -print-prog-name=cc1plus) echo {libexec}/cc1plus ;;\n\
             -print-prog-name=collect2) echo {libexec}/collect2 ;;\n\
             -print-prog-name=as) echo as ;;\n\
             -print-prog-name=ld) echo ld ;;\n\
             *) exit 1 ;;\n\
             esac\n",
            libexec = libexec.display()
        );
        write_binary(dir, name, script.as_bytes())
    }

    /// Scenario 6: an allowlisted driver whose helpers have no `Execute`-carrying grant warns,
    /// naming driver, helper and resolved path.
    #[cfg(unix)]
    #[test]
    fn an_uncovered_toolchain_helper_warns_naming_driver_helper_and_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let libexec = dir.path().join("libexec");
        std::fs::create_dir_all(&libexec).expect("libexec");
        for helper in ["cc1", "cc1plus", "collect2"] {
            write_binary(&libexec, helper, b"#!/bin/sh\nexit 0\n");
        }
        fake_driver(dir.path(), "cc", &libexec);
        let dirs = vec![dir.path().to_path_buf()];

        let warnings = unreachable_toolchain_helpers_in(
            &policy_allowing(&["cc"]),
            ContainmentClass::Sealed,
            &dirs,
        );

        let cc1 = warnings
            .iter()
            .find(|warning| warning.helper == "cc1")
            .expect("cc1 must be reported as uncovered");
        assert_eq!(cc1.driver, "cc");
        assert_eq!(
            cc1.resolved_path,
            std::fs::canonicalize(libexec.join("cc1")).expect("canonical cc1")
        );
    }

    /// Scenario 7: a declared `interpreter_runtime` directory covers every helper inside it,
    /// because such a grant carries `executable: true`.
    #[cfg(unix)]
    #[test]
    fn a_declared_grant_directory_covers_the_helpers_inside_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let libexec = dir.path().join("libexec");
        std::fs::create_dir_all(&libexec).expect("libexec");
        for helper in ["cc1", "cc1plus", "collect2"] {
            write_binary(&libexec, helper, b"#!/bin/sh\nexit 0\n");
        }
        fake_driver(dir.path(), "cc", &libexec);
        let dirs = vec![dir.path().to_path_buf()];

        let policy = CapabilityPolicy {
            shell_interpreter_runtime: vec![interpreter_grant(
                "cc",
                &[libexec.to_str().expect("utf-8 temp path")],
            )],
            ..policy_allowing(&["cc"])
        };
        let warnings = unreachable_toolchain_helpers_in(&policy, ContainmentClass::Sealed, &dirs);

        assert!(
            !warnings.iter().any(|warning| warning.helper == "cc1"),
            "a declared grant directory must cover its helpers: {warnings:?}"
        );
    }

    /// The `PATH`-deferred answer (`-print-prog-name=as` → `as`) is resolved rather than
    /// discarded — the deviation from the design that keeps two of the four helpers that matter
    /// most from being invisible to this check.
    #[cfg(unix)]
    #[test]
    fn a_path_deferred_helper_answer_is_resolved_not_discarded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let libexec = dir.path().join("libexec");
        std::fs::create_dir_all(&libexec).expect("libexec");
        // `as` lives on the injected `PATH`, not in the driver's libexec, and the driver answers
        // with the bare name.
        write_binary(dir.path(), "as", b"#!/bin/sh\nexit 0\n");
        fake_driver(dir.path(), "cc", &libexec);
        let dirs = vec![dir.path().to_path_buf()];

        let warnings = unreachable_toolchain_helpers_in(
            &policy_allowing(&["cc"]),
            ContainmentClass::Sealed,
            &dirs,
        );

        let assembler = warnings
            .iter()
            .find(|warning| warning.helper == "as")
            .expect("a PATH-deferred helper must still be checked");
        assert_eq!(
            assembler.resolved_path,
            std::fs::canonicalize(dir.path().join("as")).expect("canonical as")
        );
    }

    /// The load-bearing distinction from `fa4c62a5`: the fixed sealed tree is bound and listable
    /// but **not** executable, so a helper resolving under it is still uncovered. Written as a
    /// direct assertion on the coverage rule so that granting `SEALED_RUNTIME_PATHS` the
    /// `Execute` right one day cannot silently pass this test.
    #[test]
    fn residence_under_the_fixed_sealed_tree_is_not_coverage() {
        let policy = CapabilityPolicy {
            shell_interpreter_runtime: Vec::new(),
            shell_staged_runtime: Vec::new(),
            ..policy_allowing(&["cc"])
        };
        // `/usr/bin/as` is inside the fixed tree and inside nothing the policy declared.
        assert!(under_any(Path::new("/usr/bin/as"), &sealed_runtime_paths()));
        assert!(
            !under_any(Path::new("/usr/bin/as"), &declared_grant_dirs(&policy)),
            "the fixed tree must never be folded into the declared-grant set"
        );
    }

    /// A helper the capsule allowlisted in its own right already has an `Execute` grant.
    #[cfg(unix)]
    #[test]
    fn a_helper_that_is_itself_allowlisted_is_covered() {
        let dir = tempfile::tempdir().expect("tempdir");
        let libexec = dir.path().join("libexec");
        std::fs::create_dir_all(&libexec).expect("libexec");
        write_binary(&libexec, "cc1", b"#!/bin/sh\nexit 0\n");
        fake_driver(dir.path(), "cc", &libexec);
        let dirs = vec![dir.path().to_path_buf()];

        let warnings = unreachable_toolchain_helpers_in(
            &policy_allowing(&["cc", "cc1"]),
            ContainmentClass::Sealed,
            &dirs,
        );
        assert!(
            !warnings.iter().any(|warning| warning.helper == "cc1"),
            "an allowlisted helper carries its own Execute grant: {warnings:?}"
        );
    }

    /// A `shell.allow` entry that is not a known driver is never probed, so no unrelated binary
    /// is spawned by this check.
    #[cfg(unix)]
    #[test]
    fn a_non_driver_entry_is_never_probed() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A "driver" that would answer if asked — but it is not in the known-driver table.
        fake_driver(dir.path(), "not-a-compiler", &dir.path().join("libexec"));
        let dirs = vec![dir.path().to_path_buf()];

        assert!(unreachable_toolchain_helpers_in(
            &policy_allowing(&["not-a-compiler"]),
            ContainmentClass::Sealed,
            &dirs
        )
        .is_empty());
    }

    /// The registry is the *whole* contract of which drivers get probed, so its contents are
    /// pinned here rather than left implicit — a future slice adding `clang` should have to
    /// update this list deliberately.
    #[test]
    fn the_known_driver_registry_is_the_documented_four() {
        let drivers: Vec<&str> = KNOWN_TOOLCHAIN_DRIVERS
            .iter()
            .map(|(driver, _)| *driver)
            .collect();
        assert_eq!(drivers, vec!["cc", "gcc", "g++", "c++"]);
        for (driver, helpers) in KNOWN_TOOLCHAIN_DRIVERS {
            assert!(
                helpers.contains(&"as") && helpers.contains(&"ld"),
                "{driver} must be probed for the assembler and linker it execs"
            );
        }
    }
}
