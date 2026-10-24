//! The namespace decisions on a real host: `mur` decides whether a subprocess-capable capsule gets
//! its own network namespace (`E-CAP-005`), whether a command's literal `/tmp` is mapped, and
//! whether `sealed` is backed (`E-CAP-003`) by creating the namespaces, and reads AppArmor's
//! unprivileged-userns restriction only to explain a refusal.
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

use serde_json::{json, Value};

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
        self.command_under(Command::new("unshare"), scratch, cwd, program, args)
    }

    /// [`Self::command`] inside a transient systemd scope with the cgroup v2 controllers delegated
    /// to it, for a `mur run` that must bound its process tree (`E-RUN-012`) under a test harness
    /// whose own cgroup is undelegated.
    fn command_in_delegated_scope(
        self,
        scratch: &Scratch,
        cwd: &Path,
        program: &Path,
        args: &[&str],
    ) -> Command {
        let mut scope = Command::new("systemd-run");
        scope.args([
            "--user",
            "--scope",
            "--quiet",
            "--collect",
            "-p",
            "Delegate=yes",
            "--",
            "unshare",
        ]);
        self.command_under(scope, scratch, cwd, program, args)
    }

    fn command_under(
        self,
        mut command: Command,
        scratch: &Scratch,
        cwd: &Path,
        program: &Path,
        args: &[&str],
    ) -> Command {
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
    for created in [
        "network namespace: created",
        "mount namespace: created",
        "sealed root: created",
    ] {
        assert!(
            doctor.stdout.contains(created),
            "{created}\n{}",
            doctor.all()
        );
    }
    if doctor.stdout.contains("profile attached: none") {
        assert!(
            doctor.stdout.contains("userns grant: withheld"),
            "{}",
            doctor.all()
        );
        assert!(
            doctor
                .stdout
                .contains("the namespaces below were created regardless"),
            "{}",
            doctor.all()
        );
    } else {
        eprintln!(
            "{test}: a profile confines the cargo-built mur here, so its grant is not `withheld`"
        );
    }
    for absent in [
        "E-CAP-005",
        "E-CAP-003",
        "This path is why the grant below is withheld",
        "install-dev-apparmor.sh",
    ] {
        assert!(!doctor.all().contains(absent), "{absent}\n{}", doctor.all());
    }
}

/// With the restriction reading `1` and every namespace creatable, `--explain-scope` reports a
/// `sealed` capsule as backed on the composed root with its `/tmp` mapped by it, and a `scoped`
/// capsule as `scoped` on `landlock+seccomp` with its `/tmp` mapped by a mount namespace. Both
/// report the host can back `sealed`.
#[test]
fn sealed_is_backed_and_tmp_mapped_while_the_restriction_reads_one() {
    let test = "sealed_is_backed_and_tmp_mapped_while_the_restriction_reads_one";
    let construction = Construction::RestrictionReadsOne;
    let scratch = Scratch::new();
    if skip(test, construction, &scratch) {
        return;
    }
    let home = tempfile::tempdir().unwrap();
    for (class, tier, mapped_by) in [
        (
            "sealed",
            "mountns+pivot_root+landlock+seccomp",
            "composed-root",
        ),
        ("scoped", "landlock+seccomp", "mount-namespace"),
    ] {
        let project = driver_project("http://127.0.0.1:9", class);
        let report = explain_scope(construction, &scratch, home.path(), project.path());
        assert_eq!(report["declared_containment"], class, "{report}");
        assert_eq!(report["achieved_containment"], class, "{report}");
        assert_eq!(report["enforcement_tier"], tier, "{report}");
        assert_eq!(report["host_containment"], "sealed", "{report}");
        assert_eq!(report["floor_met"], true, "{report}");
        assert_eq!(report["temp_dir"]["mapped_by"], mapped_by, "{report}");
    }
}

/// With the restriction reading `1`, a real session's shell call writes a literal `/tmp` into the
/// session's own temporary folder, and a connection to a host listener the allowlist does not
/// name reaches nothing.
#[test]
fn a_session_writes_tmp_and_reaches_no_unlisted_listener_while_the_restriction_reads_one() {
    a_session_under_the_reading(
        "a_session_writes_tmp_and_reaches_no_unlisted_listener_while_the_restriction_reads_one",
        "scoped",
        "mount-namespace",
    );
}

/// The same session declaring `sealed`, which runs in the composed root.
#[test]
fn a_sealed_session_runs_while_the_restriction_reads_one() {
    a_session_under_the_reading(
        "a_sealed_session_runs_while_the_restriction_reads_one",
        "sealed",
        "composed-root",
    );
}

/// One scripted `mur run` at `class` under [`Construction::RestrictionReadsOne`]: a shell call
/// writes and reads `/tmp/probe.txt`, a second connects with bash's `/dev/tcp` to a listener this
/// test holds on the host and the allowlist does not name.
fn a_session_under_the_reading(test: &str, class: &str, mapped_by: &str) {
    const CGROUP_REFUSAL: &str = "E-RUN-012";
    let construction = Construction::RestrictionReadsOne;
    let scratch = Scratch::new();
    if skip(test, construction, &scratch) {
        return;
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let token = format!("probe-{}-{class}", std::process::id());
    let server = common::ScriptedServer::start(vec![
        tool_call(
            "msg_1",
            "toolu_tmp",
            json!({"command": format!("echo {token} > /tmp/probe.txt && cat /tmp/probe.txt")}),
        ),
        tool_call(
            "msg_2",
            "toolu_net",
            json!({"command": format!(
                "if (exec 3<>/dev/tcp/127.0.0.1/{port}) 2>/dev/null; then echo CONNECTED; \
                 else echo REFUSED; fi"
            )}),
        ),
        end_turn("msg_3", "Done."),
    ]);
    let home = tempfile::tempdir().unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let project = driver_project(&server.endpoint, class);
    let driver = common::create_driver_artifact(
        artifacts.path(),
        DRIVER_NAME,
        DRIVER_VERSION,
        &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver).success();

    let args = [
        "run",
        "--manifest",
        "murmur.yaml",
        "--task",
        "Write to /tmp, read it back, then try the listener.",
    ];
    let run_in = |command: Command| {
        let mut command = command;
        Run::of(command.env("HOME", home.path()).output().unwrap())
    };
    let mut run = run_in(construction.command(&scratch, project.path(), &mur_path(), &args));
    if !run.success && run.stderr.contains(CGROUP_REFUSAL) && common::cgroup_delegation_available()
    {
        fs::remove_dir_all(project.path().join("workdir")).ok();
        fs::remove_dir_all(common::records_root(home.path())).ok();
        run = run_in(construction.command_in_delegated_scope(
            &scratch,
            project.path(),
            &mur_path(),
            &args,
        ));
    }
    if !run.success && run.stderr.contains(CGROUP_REFUSAL) {
        eprintln!(
            "[SKIP-HOST] {test}: this host cannot delegate a cgroup v2 scope to `mur`, so `mur run` \
             refuses with {CGROUP_REFUSAL} before any shell call"
        );
        return;
    }
    assert!(run.success, "{}", run.all());

    let requests = server.requests();
    let wrote = common::tool_result_text(&requests, "toolu_tmp").expect("the /tmp call's result");
    assert!(wrote.contains(&token), "{wrote}\n{}", run.all());
    let reached =
        common::tool_result_text(&requests, "toolu_net").expect("the listener call's result");
    // The result echoes the command, so only whole output lines count.
    let printed = |word: &str| reached.lines().any(|line| line.trim() == word);
    assert!(printed("REFUSED") && !printed("CONNECTED"), "{reached}");
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
        "the host listener accepted a connection the allowlist does not name"
    );

    let records = common::newest_session_records(home.path(), &project.path().join("workdir"))
        .expect("the session's records directory");
    assert_eq!(
        fs::read_to_string(records.join("tmp/probe.txt")).unwrap(),
        format!("{token}\n")
    );
    assert!(
        !fs::read_to_string("/tmp/probe.txt").is_ok_and(|text| text.contains(&token)),
        "the host's /tmp received the session's write"
    );
    let trace = fs::read_to_string(records.join("trace.jsonl")).unwrap();
    let session_start: Value = trace
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|event| event["event_type"] == "session_start")
        .expect("trace carries session_start");
    let grants = &session_start["effective_grants"];
    assert_eq!(grants["temp_dir"]["mapped_by"], mapped_by, "{grants}");
    assert_eq!(grants["achieved_containment"], class, "{grants}");
    assert_eq!(
        session_start["containment_achieved"], class,
        "{session_start}"
    );
    assert_eq!(grants["host_containment"], "sealed", "{grants}");
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
    for line in [
        "mount namespace: not attempted (no network namespace to create it in)",
        "sealed root: not attempted (no network namespace to create it in)",
    ] {
        assert!(doctor.stdout.contains(line), "{line}\n{}", doctor.all());
    }

    let sealed = driver_project("http://127.0.0.1:9", "sealed");
    let report = explain_scope(construction, &case.scratch, case.home.path(), sealed.path());
    assert_eq!(report["floor_met"], false, "{report}");
    assert_eq!(report["temp_dir"]["mapped_by"], Value::Null, "{report}");
    let shortfall = report["shortfall_reason"].as_str().expect("a shortfall");
    assert!(
        shortfall.contains(
            "The probe's unshare(CLONE_NEWUSER | CLONE_NEWNET) failed with EPERM (Operation not \
             permitted)."
        ),
        "{shortfall}"
    );
    if doctor.stdout.contains("profile attached: none") {
        assert!(
            shortfall.starts_with(&capsule_runtime::SealedBlocker::AppArmorProfileMissing.reason()),
            "{shortfall}"
        );
        assert!(
            shortfall.ends_with(
                "Host readings: kernel.apparmor_restrict_unprivileged_userns=1; AppArmor profile \
                 confining mur: unconfined."
            ),
            "{shortfall}"
        );
    }
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

    let sealed = driver_project("http://127.0.0.1:9", "sealed");
    let report = explain_scope(construction, &case.scratch, case.home.path(), sealed.path());
    assert_eq!(report["floor_met"], false, "{report}");
    let shortfall = report["shortfall_reason"].as_str().expect("a shortfall");
    assert!(
        shortfall.starts_with(&capsule_runtime::SealedBlocker::KernelUnsupported.reason()),
        "{shortfall}"
    );
    assert!(shortfall.contains("failed with ENOSPC"), "{shortfall}");
    assert!(!shortfall.contains("mur-sealed"), "{shortfall}");
}

const DRIVER_NAME: &str = "local/murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";

fn tool_call(id: &str, tool_use_id: &str, input: Value) -> String {
    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "tool_use", "id": tool_use_id, "name": "bash", "input": input}],
        "stop_reason": "tool_use",
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

fn end_turn(id: &str, text: &str) -> String {
    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

/// A capsule on the fixture driver at `endpoint`, declaring `containment`, that may run `bash` and
/// `cat` and reach `endpoint` and nothing else.
fn driver_project(endpoint: &str, containment: &str) -> tempfile::TempDir {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("murmur.yaml"),
        format!(
            "name: namespace-probe-capsule\n\
             version: 0.1.0\n\
             artifacts:\n\
             \x20 - name: {DRIVER_NAME}\n\
             \x20   version: {DRIVER_VERSION}\n\
             \x20   runtime: driver\n\
             \x20   gateway:\n\
             \x20     endpoint: {endpoint}\n\
             \x20     api_key: test-key\n\
             capabilities:\n\
             \x20 containment: {containment}\n\
             \x20 network:\n\
             \x20   allow:\n\
             \x20     - {endpoint}\n\
             \x20 shell:\n\
             \x20   allow:\n\
             \x20     - bash\n\
             \x20     - cat\n\
             inference:\n\
             \x20 transport: http\n\
             \x20 model: test-model\n\
             \x20 driver:\n\
             \x20   artifact: {DRIVER_NAME}\n"
        ),
    )
    .unwrap();
    project
}

/// `mur run --explain-scope --json` for the project in `project`, inside `construction`.
fn explain_scope(
    construction: Construction,
    scratch: &Scratch,
    home: &Path,
    project: &Path,
) -> Value {
    let run = Run::of(
        construction
            .command(
                scratch,
                project,
                &mur_path(),
                &[
                    "run",
                    "--manifest",
                    "murmur.yaml",
                    "--explain-scope",
                    "--json",
                ],
            )
            .env("HOME", home)
            .output()
            .unwrap(),
    );
    serde_json::from_str(run.stdout.trim())
        .unwrap_or_else(|error| panic!("--explain-scope --json: {error}\n{}", run.all()))
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
