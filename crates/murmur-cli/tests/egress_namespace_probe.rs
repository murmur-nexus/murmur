//! The `E-CAP-005` decision on a real host: `mur` decides whether a subprocess-capable capsule
//! gets its own network namespace by creating one, and reads AppArmor's unprivileged-userns
//! restriction only to explain a refusal.
//!
//! Each case drives the cargo-built `mur` inside a construction an unprivileged user can build
//! with `unshare`, so no root, no `sysctl` and no container is needed:
//!
//! | Construction | Restriction reads | `unshare(CLONE_NEWUSER)` from `mur` |
//! |---|---|---|
//! | restriction bound to `1` | `1` | succeeds |
//! | the same, then `chroot` | `1` | `EPERM` (the kernel refuses a chrooted process) |
//! | `user.max_user_namespaces=0` | the host's | `ENOSPC` |
//!
//! A host where a construction cannot be built prints `[SKIP-HOST]` and the case returns.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

const RESTRICT_PROC: &str = "/proc/sys/kernel/apparmor_restrict_unprivileged_userns";
/// Read before [`RESTRICT_PROC`] when it exists, so a construction binds over both.
const RESTRICT_SYS: &str = "/sys/module/apparmor/parameters/restrict_unprivileged_userns";
const APPARMOR_ENABLED: &str = "/sys/module/apparmor/parameters/enabled";
const MAX_USER_NAMESPACES: &str = "/proc/sys/user/max_user_namespaces";

/// How `mur` is started: the shell script `sh -c` runs before it `exec`s `mur`, and the
/// `unshare` flags it runs under.
#[derive(Clone, Copy)]
enum Construction {
    /// The restriction reads `1` and a nested user namespace still creates.
    RestrictionReadsOne,
    /// The restriction reads `1` and `mur` runs chrooted, where `unshare(CLONE_NEWUSER)` fails
    /// with `EPERM`.
    RestrictionReadsOneChrooted,
    /// `user.max_user_namespaces` is `0` in the namespace `mur` runs in, so `unshare` fails with
    /// `ENOSPC`.
    NoUserNamespacesLeft,
}

/// The scratch files a construction binds or chroots into.
struct Scratch {
    _dir: tempfile::TempDir,
    restrict: PathBuf,
    root: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let restrict = dir.path().join("restrict");
        fs::write(&restrict, "1\n").unwrap();
        let root = dir.path().join("root");
        fs::create_dir(&root).unwrap();
        Self {
            _dir: dir,
            restrict,
            root,
        }
    }
}

impl Construction {
    fn unshare_flags(self) -> &'static str {
        match self {
            Self::RestrictionReadsOne | Self::RestrictionReadsOneChrooted => "-Urm",
            Self::NoUserNamespacesLeft => "-Ur",
        }
    }

    /// The setup, ending in `exec "$0" "$@"` (or its `chroot` form) so the program runs as the
    /// construction's own process. `$RESTRICT` and `$ROOT` come from [`Scratch`]; `chroot` starts
    /// in `/`, so the chrooted form changes back to `$CWD`, the command's working directory.
    fn script(self) -> String {
        let bind = format!(
            "mount --bind \"$RESTRICT\" {RESTRICT_PROC} && \
             {{ [ ! -e {RESTRICT_SYS} ] || mount --bind \"$RESTRICT\" {RESTRICT_SYS}; }}"
        );
        match self {
            Self::RestrictionReadsOne => format!("{bind} && exec \"$0\" \"$@\""),
            Self::RestrictionReadsOneChrooted => format!(
                "{bind} && mount --rbind / \"$ROOT\" && \
                 exec chroot \"$ROOT\" sh -c 'cd \"$CWD\" && exec \"$0\" \"$@\"' \"$0\" \"$@\""
            ),
            Self::NoUserNamespacesLeft => {
                format!("echo 0 > {MAX_USER_NAMESPACES} && exec \"$0\" \"$@\"")
            }
        }
    }

    /// `program args…` inside the construction, in `cwd`.
    fn command(self, scratch: &Scratch, cwd: &Path, program: &Path, args: &[&str]) -> Command {
        let mut command = Command::new("unshare");
        command
            .arg(self.unshare_flags())
            .args(["sh", "-c", &self.script()])
            .arg(program)
            .args(args)
            .env("RESTRICT", &scratch.restrict)
            .env("ROOT", &scratch.root)
            .env("CWD", cwd)
            .current_dir(cwd);
        command
    }

    /// `None` when the construction works here, else why it does not. Checks what the case
    /// relies on: the setup succeeds, the reading is what the case expects, and AppArmor is
    /// enabled where the case needs a reading of `1` to mean the restriction is on.
    fn unavailable(self, scratch: &Scratch) -> Option<String> {
        let probe = |program: &str, args: &[&str]| {
            self.command(scratch, Path::new("/"), Path::new(program), args)
                .output()
                .map_err(|error| format!("unshare could not be run: {error}"))
        };
        match probe("true", &[]) {
            Ok(output) if output.status.success() => {}
            Ok(output) => {
                return Some(format!(
                    "`unshare {}` with the construction's setup failed: {}",
                    self.unshare_flags(),
                    String::from_utf8_lossy(&output.stderr).trim()
                ))
            }
            Err(error) => return Some(error),
        }
        let (file, expected) = match self {
            Self::RestrictionReadsOne | Self::RestrictionReadsOneChrooted => (RESTRICT_PROC, "1"),
            Self::NoUserNamespacesLeft => (MAX_USER_NAMESPACES, "0"),
        };
        let read = match probe("cat", &[file]) {
            Ok(output) => String::from_utf8_lossy(&output.stdout).trim().to_string(),
            Err(error) => return Some(error),
        };
        if read != expected {
            return Some(format!(
                "{file} reads {read:?} inside the construction, not {expected}"
            ));
        }
        if !matches!(self, Self::NoUserNamespacesLeft) {
            let enabled = fs::read_to_string(APPARMOR_ENABLED).unwrap_or_default();
            if !matches!(enabled.trim(), "Y" | "1") {
                return Some(
                    "AppArmor is not enabled, so a restriction reading of 1 means nothing here"
                        .to_string(),
                );
            }
        }
        None
    }
}

fn skip(test_name: &str, construction: Construction, scratch: &Scratch) -> bool {
    match construction.unavailable(scratch) {
        Some(why) => {
            eprintln!("[SKIP-HOST] {test_name}: {why}");
            true
        }
        None => false,
    }
}

fn mur_path() -> PathBuf {
    assert_cmd::cargo::cargo_bin("mur")
}

/// A capsule that can spawn (`shell.allow: [ls]`) and declares a `read_only` entry staging
/// refuses with `E-CAP-012`. That refusal comes right after the `E-CAP-005` check, so seeing it
/// proves the namespace check passed.
fn staging_project(read_only: &[&str], shell_allow: &[&str]) -> tempfile::TempDir {
    let project = tempfile::tempdir().unwrap();
    let allowed: String = shell_allow
        .iter()
        .map(|binary| format!("      - {binary}\n"))
        .collect();
    let read_only_yaml: String = read_only
        .iter()
        .map(|entry| format!("      - \"{entry}\"\n"))
        .collect();
    fs::write(
        project.path().join("murmur.yaml"),
        format!(
            "name: staging-capsule\n\
             version: 0.1.0\n\
             capabilities:\n\
             \x20 shell:\n\
             \x20   allow:\n\
             {allowed}\
             \x20 filesystem:\n\
             \x20   read_only:\n\
             {read_only_yaml}"
        ),
    )
    .unwrap();
    fs::copy(
        common::fixture_path("run/components/capsule-allowlisted.wasm"),
        project.path().join("capsule.wasm"),
    )
    .unwrap();
    project
}

/// What one `mur` invocation printed.
struct Run {
    success: bool,
    stdout: String,
    stderr: String,
}

impl Run {
    fn of(output: Output) -> Self {
        Self {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    fn all(&self) -> String {
        format!(
            "--- stdout ---\n{}\n--- stderr ---\n{}",
            self.stdout, self.stderr
        )
    }
}

/// `mur run --manifest` and `mur doctor` on one project, inside one construction.
struct Case {
    construction: Construction,
    scratch: Scratch,
    home: tempfile::TempDir,
    project: tempfile::TempDir,
}

impl Case {
    fn new(construction: Construction, scratch: Scratch) -> Self {
        Self {
            construction,
            scratch,
            home: tempfile::tempdir().unwrap(),
            project: staging_project(&["../outside"], &["ls"]),
        }
    }

    fn mur(&self, args: &[&str]) -> Run {
        Run::of(
            self.construction
                .command(&self.scratch, self.project.path(), &mur_path(), args)
                .env("HOME", self.home.path())
                .output()
                .unwrap(),
        )
    }

    fn run(&self) -> Run {
        let manifest = self.project.path().join("murmur.yaml");
        self.mur(&["run", "--manifest", manifest.to_str().unwrap()])
    }

    fn doctor(&self) -> Run {
        self.mur(&["doctor"])
    }
}

/// With the restriction reading `1` and the namespace creatable, `mur run` is not refused with
/// `E-CAP-005`: staging goes on to its next refusal, and `mur doctor` reports the namespace as
/// created beside the `withheld` grant.
#[test]
fn a_creatable_namespace_is_not_refused_while_the_restriction_reads_one() {
    let test = "a_creatable_namespace_is_not_refused_while_the_restriction_reads_one";
    let construction = Construction::RestrictionReadsOne;
    let scratch = Scratch::new();
    if skip(test, construction, &scratch) {
        return;
    }
    let case = Case::new(construction, scratch);

    let run = case.run();
    assert!(!run.success, "{}", run.all());
    assert!(run.stderr.contains("error[E-CAP-012]"), "{}", run.all());
    assert!(!run.stderr.contains("E-CAP-005"), "{}", run.all());

    let doctor = case.doctor();
    assert!(
        doctor.stdout.contains("network namespace: created"),
        "{}",
        doctor.all()
    );
    if doctor.stdout.contains("profile attached: none") {
        assert!(
            doctor.stdout.contains("userns grant: withheld"),
            "{}",
            doctor.all()
        );
    } else {
        eprintln!(
            "{test}: a profile confines the cargo-built mur here, so its grant is not `withheld`"
        );
    }
    assert!(!doctor.all().contains("E-CAP-005"), "{}", doctor.all());
}

/// Where the namespace really cannot be created and the restriction reads `1`, `mur run` refuses
/// with `E-CAP-005` naming the failed call, its `errno` and the reading, before any session
/// exists; `mur doctor` gives the same reason and reports the namespace as refused.
#[test]
fn a_refused_namespace_names_the_call_the_errno_and_the_reading() {
    let test = "a_refused_namespace_names_the_call_the_errno_and_the_reading";
    let construction = Construction::RestrictionReadsOneChrooted;
    let scratch = Scratch::new();
    if skip(test, construction, &scratch) {
        return;
    }
    let case = Case::new(construction, scratch);

    let run = case.run();
    assert!(!run.success, "{}", run.all());
    for expected in [
        "error[E-CAP-005]",
        "unshare(CLONE_NEWUSER | CLONE_NEWNET) failed with EPERM (Operation not permitted)",
        "kernel.apparmor_restrict_unprivileged_userns=1",
    ] {
        assert!(run.stderr.contains(expected), "{expected}\n{}", run.all());
    }
    assert!(!run.stderr.contains("E-CAP-012"), "{}", run.all());
    assert!(
        !case.home.path().join(".murmur/sessions").exists(),
        "a refused launch creates no session directory"
    );

    let doctor = case.doctor();
    assert!(
        doctor.stdout.contains(
            "network namespace: refused \u{2014} unshare(CLONE_NEWUSER | CLONE_NEWNET) failed \
             with EPERM"
        ),
        "{}",
        doctor.all()
    );
    assert!(
        doctor.stderr.contains("warning[E-CAP-005]"),
        "{}",
        doctor.all()
    );
    let reason = reason_of(&run.stderr);
    assert!(
        doctor.stderr.contains(&reason),
        "doctor must give the reason `mur run` gave: {reason}\n{}",
        doctor.all()
    );
}

/// A kernel with no user namespaces left to give is named as such, with no AppArmor remedy.
#[test]
fn a_kernel_without_user_namespaces_is_named_without_apparmor() {
    let test = "a_kernel_without_user_namespaces_is_named_without_apparmor";
    let construction = Construction::NoUserNamespacesLeft;
    let scratch = Scratch::new();
    if skip(test, construction, &scratch) {
        return;
    }
    let case = Case::new(construction, scratch);

    let run = case.run();
    assert!(!run.success, "{}", run.all());
    for expected in [
        "error[E-CAP-005]",
        "failed with ENOSPC",
        "user.max_user_namespaces",
    ] {
        assert!(run.stderr.contains(expected), "{expected}\n{}", run.all());
    }
    assert!(!run.stderr.contains("apparmor_parser"), "{}", run.all());
    assert!(!run.stderr.contains("E-CAP-012"), "{}", run.all());
}

/// The reason in an `E-CAP-005` refusal: from the failed call through the readings line.
fn reason_of(stderr: &str) -> String {
    let start = stderr
        .find("unshare(")
        .expect("the refusal names the failed call");
    let end = stderr[start..]
        .find("Host readings:")
        .map(|offset| start + offset)
        .expect("the refusal ends with the readings line");
    let line_end = stderr[end..]
        .find('\n')
        .map_or(stderr.len(), |offset| end + offset);
    stderr[start..line_end].to_string()
}
