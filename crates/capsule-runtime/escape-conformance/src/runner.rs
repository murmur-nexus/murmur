//! Running one case against real kernel enforcement, and grading what came back.
//!
//! # How a case reaches the sandbox
//!
//! `sandbox::prepare_enforcement` — the code that installs the Landlock ruleset and the seccomp
//! filter — runs inside the forked child of `shell::execute_shell`, and both of those are
//! `pub(crate)` to `capsule-runtime`. There is no library seam an external package can use to
//! reach them. The script-capsule path does not help either: a capsule component's linker gets
//! `murmur:tool-registry/invoke`, whose dispatch resolves WASM tool components only, so a script
//! capsule cannot invoke a shell binary at all. The only route into the enforcement path is the
//! agent loop.
//!
//! So each case launches a real capsule through the built `mur` binary, exactly as every existing
//! manual-verification document in this repository does — but with `inference.transport: process`
//! pointed at this package's own `probe-driver` binary instead of a subscription CLI. `mur run`
//! stands up the Claude Bridge, advertises the capsule's `shell.allow` binaries over it, and
//! spawns whatever `inference.command` names; `probe-driver` makes exactly one predetermined tool
//! call and exits. Tool execution, capability enforcement and the trace are all murmur's, byte
//! for byte the same path a live model would drive — only the *choice* of which tool to call is
//! scripted rather than sampled.
//!
//! That is deliberate and it is what makes this a gate rather than a demonstration. A release
//! gate whose verdicts depend on a model deciding to run the exact command it was asked to would
//! be flaky in the one direction that matters: a case the model skipped would produce no
//! evidence, and "no evidence" must never read as "contained". It also costs no API calls and
//! needs no key, so the suite is runnable by anyone with the repository and a Linux host.
//!
//! # Why the probe writes a file instead of returning a value
//!
//! Every resource-exhaustion case ends with its process killed. A verdict carried back through
//! the tool result would be lost; a file written in the capsule workdir survives, and its absence
//! is recorded as `INCONCLUSIVE` rather than read as clean.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use crate::cases::{Case, Evidence, Prepare, Profile};
use crate::probe;
use crate::verdict::{Expectation, Verdict};

/// Everything a run needs that is not the case itself.
pub struct RunnerConfig {
    /// The built `mur` binary. Cases drive real enforcement through it.
    pub mur: PathBuf,
    /// This package's `probe-driver`, named as `inference.command` in every generated manifest.
    pub probe_driver: PathBuf,
    /// Root under which each case gets its own directory. Kept after the run: it holds the
    /// generated manifest, the probe source, `mur`'s stdout/stderr and the session trace, and the
    /// record points at it per case.
    pub work_root: PathBuf,
    /// Wall-clock ceiling for one case's `mur run`.
    pub timeout: Duration,
    /// Wrap each `mur run` in `systemd-run --user --scope --property=Delegate=yes`. Without a
    /// delegated cgroup v2 subtree, `mur run` refuses any subprocess-capable capsule with
    /// `E-RUN-012` and no case can report anything — see the install requirement in
    /// `resource-limits-manual-verification.md`.
    pub systemd_scope: bool,
    /// `capabilities.shell.interpreter_runtime` dirs for `python3`, derived from the host's own
    /// interpreter at preflight rather than hardcoded.
    pub interpreter_dirs: Vec<(String, bool)>,
}

/// One case's result.
pub struct CaseOutcome {
    pub case: &'static Case,
    pub expectation: Expectation,
    pub verdict: Verdict,
    /// One line. Carries the attribution evidence — which errno, which control, which mechanism.
    pub detail: String,
    pub passed: bool,
    pub case_dir: PathBuf,
}

/// Derives the host directories `python3` needs outside the workdir, by asking the interpreter.
///
/// Hardcoding `/usr/lib/python3.11` (as `manifest-schema.md`'s example does) would break on the
/// next point release, and `W-SEC-009` exists precisely to warn that such a grant couples a
/// capsule to one host layout. Asking the interpreter keeps the harness portable across distros.
///
/// **This grant is not part of the boundary under test and does not weaken any case.** It opens
/// the Python standard library and the system library directory for read+execute. Every boundary
/// case targets something else entirely — `/etc`, `/tmp`, `/proc`, a device node, a socket, a
/// syscall number — so no case's verdict can be produced by this grant. It is stated here, and in
/// the reference document, so no reader has to work that out for themselves.
pub fn derive_interpreter_dirs(python: &str) -> Result<Vec<(String, bool)>, String> {
    let script = "import sysconfig,sys;\
                  p=sysconfig.get_paths();\
                  print(p['stdlib']);\
                  print(p.get('platstdlib', p['stdlib']));\
                  print(sysconfig.get_config_var('MULTIARCH') or '')";
    let output = Command::new(python)
        .args(["-c", script])
        .output()
        .map_err(|err| format!("could not run `{python} -c ...`: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "`{python}` could not report its own paths: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines();
    let stdlib = lines.next().unwrap_or_default().trim().to_string();
    let platstdlib = lines.next().unwrap_or_default().trim().to_string();
    let multiarch = lines.next().unwrap_or_default().trim().to_string();
    if stdlib.is_empty() {
        return Err(format!("`{python}` reported no stdlib path"));
    }

    let mut dirs = vec![(stdlib.clone(), true)];
    if !platstdlib.is_empty() && platstdlib != stdlib {
        dirs.push((platstdlib.clone(), true));
    }
    // `lib-dynload` holds the C extension modules (`_ctypes`, `fcntl`, `_socket`) the probes
    // import; it is a subdirectory, but naming it explicitly keeps the grant legible.
    for base in [&stdlib, &platstdlib] {
        let dynload = format!("{base}/lib-dynload");
        if !base.is_empty() && Path::new(&dynload).is_dir() && !dirs.iter().any(|(p, _)| p == &dynload)
        {
            dirs.push((dynload, false));
        }
    }
    // libffi, which `_ctypes` dlopens, is a dependency of the extension module rather than of the
    // `python3` binary, so it is not in the library closure the runtime derives from `shell.allow`
    // and has to be granted explicitly. Without it the six dangerous-syscall cases cannot import
    // `ctypes` and would all report INCONCLUSIVE.
    if !multiarch.is_empty() {
        let libdir = format!("/usr/lib/{multiarch}");
        if Path::new(&libdir).is_dir() {
            dirs.push((libdir, false));
        }
    }
    Ok(dirs)
}

/// The tight `capabilities.resources` block from `resource-limits-manual-verification.md`'s test
/// capsule, with two deliberate divergences noted inline.
fn tight_resources_yaml() -> &'static str {
    "  resources:\n\
    \x20   # 512 rather than the document's 64, so the fork-bomb case's stopping point is\n\
    \x20   # attributable: RLIMIT_NPROC headroom well above cgroup_pids_max means a tree that\n\
    \x20   # stops in the low tens stopped because of pids.max and nothing else.\n\
    \x20   max_processes: 512\n\
    \x20   # 64 rather than the document's 16, and the difference is a finding rather than a\n\
    \x20   # preference. `apply_hard_rlimits` runs first in the child's pre_exec window, so every\n\
    \x20   # later step in that window lives under this ceiling — and installing the seccomp\n\
    \x20   # filter needs descriptors. Observed on Linux 7.0.0/libseccomp 2.5.5: at 16 and at 32\n\
    \x20   # every spawn dies with `shell enforcement setup failed before exec: There was a system\n\
    \x20   # failure beyond the control of libseccomp`; at 64 the spawn succeeds and the ceiling\n\
    \x20   # still bites, with EMFILE at descriptor 61 rather than in the 1000s. 16 would make the\n\
    \x20   # fd-exhauster case unrunnable, which is not the same as contained.\n\
    \x20   max_open_files: 64\n\
    \x20   max_file_size_bytes: 10485760      # 10 MiB\n\
    \x20   # 60 rather than the document's 5: pids.max is hit in milliseconds, so a generous CPU\n\
    \x20   # ceiling keeps the fork-bomb attribution unambiguously cgroup_pids_max.\n\
    \x20   cpu_seconds: 60\n\
    \x20   # Generous on purpose: an RLIMIT_AS overrun surfaces as ENOMEM inside the child's own\n\
    \x20   # allocator and identifies nothing, so the cgroup bound below must be what bites.\n\
    \x20   memory_bytes: 4294967296\n\
    \x20   cgroup_memory_bytes: 268435456     # 256 MiB\n\
    \x20   cgroup_pids_max: 32\n\
    \x20   cgroup_cpu_percent: 50\n\
    \x20   workdir_max_bytes: 52428800        # 50 MiB\n"
}

/// The manifest one case runs under.
///
/// Declares `capabilities.containment: <class>` explicitly. Without this, every case would run at
/// the manifest default (`advisory`), and on a `sealed`-capable host `applied_tier` would still
/// install `KernelFull` (Landlock+seccomp, `scoped`'s mechanism) — never the composed root — no
/// matter what `--class` was passed to this binary. The banner and grading table would say
/// `sealed` while every probe actually ran under `scoped`, which is exactly the "false assurance"
/// this harness exists to prevent (see `lib.rs`'s module docs).
fn manifest_yaml(case: &Case, config: &RunnerConfig, class: murmur_artifact::ContainmentClass) -> String {
    let mut yaml = String::new();
    yaml.push_str(&format!("name: escape-conformance-{}\n", case.id));
    yaml.push_str("version: 0.0.1\n\n");
    yaml.push_str("capabilities:\n");
    yaml.push_str(&format!("  containment: {class}\n"));
    yaml.push_str("  shell:\n");
    // `bash` is allowlisted so `exec-renamed-disallowed-binary` has an allowlisted basename to
    // wear; `python3` is what every probe actually runs as.
    yaml.push_str("    allow:\n      - bash\n      - python3\n");
    if !config.interpreter_dirs.is_empty() {
        yaml.push_str("    interpreter_runtime:\n      - binary: python3\n        dirs:\n");
        for (path, list_dir) in &config.interpreter_dirs {
            yaml.push_str(&format!("          - path: {path}\n"));
            yaml.push_str(&format!("            list_dir: {list_dir}\n"));
        }
    }
    if case.profile == Profile::TightResources {
        yaml.push_str(tight_resources_yaml());
    }
    // `network.allow` is left entirely undeclared, so every destination is unlisted and
    // `network.unix_sockets` keeps its default of false — which is what the four network cases
    // and the two AF_UNIX cases assert against.
    yaml.push_str("\ninference:\n");
    yaml.push_str("  transport: process\n");
    yaml.push_str(&format!(
        "  command: {}\n",
        config.probe_driver.display()
    ));
    yaml.push_str("  max_turns: 2\n");
    yaml
}

/// Stages one case's directory and returns `(case_dir, capsule_workdir, manifest_path)`.
fn stage(
    case: &Case,
    config: &RunnerConfig,
    class: murmur_artifact::ContainmentClass,
) -> io::Result<(PathBuf, PathBuf, PathBuf)> {
    let case_dir = config.work_root.join(case.id);
    let workdir = case_dir.join("wd");
    fs::create_dir_all(&workdir)?;

    fs::write(workdir.join(probe::PROBE_SCRIPT), probe::render(case.body))?;
    if let Evidence::SecondSpawnRefused(_) = case.evidence {
        fs::write(
            workdir.join(probe::SECOND_SCRIPT),
            probe::SECOND_SCRIPT_SOURCE,
        )?;
    }
    let manifest = case_dir.join("murmur.yaml");
    fs::write(&manifest, manifest_yaml(case, config, class))?;

    match case.prepare {
        Prepare::None | Prepare::LeakFdIntoMur { .. } => {}
        Prepare::CopyBinaryAs { sources, dest } => {
            let source = sources
                .iter()
                .find(|path| Path::new(path).is_file())
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("none of {sources:?} exists on this host"),
                    )
                })?;
            let target = workdir.join(dest);
            fs::copy(source, &target)?;
            set_executable(&target)?;
        }
    }
    Ok((case_dir, workdir, manifest))
}

#[cfg(unix)]
fn set_executable(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// Builds the argv for one case's `mur run`, innermost command last.
fn build_argv(case: &Case, config: &RunnerConfig, manifest: &Path, workdir: &Path) -> Vec<String> {
    let mut argv: Vec<String> = Vec::new();

    if let Prepare::LeakFdIntoMur { fd, path } = case.prepare {
        // Exactly `subprocess-fd-hygiene-verification.md` step 4: open the descriptor in the
        // launching shell without FD_CLOEXEC so `mur` inherits it and still holds it open when it
        // spawns the capsule's subprocess. `/etc/hostname` sits outside the workdir and outside
        // every Landlock grant, so a child that can read it is reading through the scope.
        argv.push("bash".to_string());
        argv.push("-c".to_string());
        argv.push(format!("exec {fd}<{path}; exec \"$@\""));
        argv.push("escape-conformance".to_string());
    }

    if config.systemd_scope {
        argv.extend(
            [
                "systemd-run",
                "--user",
                "--scope",
                "--quiet",
                "--property=Delegate=yes",
                "--",
            ]
            .iter()
            .map(|s| s.to_string()),
        );
    }

    argv.push(config.mur.display().to_string());
    argv.extend(
        [
            "run",
            "--manifest",
            &manifest.display().to_string(),
            "--workdir",
            &workdir.display().to_string(),
            "--task",
            "Escape-conformance probe. The driver makes the tool call directly; this text is \
             never read by a model.",
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    argv
}

/// Spawns `argv`, waits up to `timeout`, and kills it if it overruns.
///
/// stdout and stderr go to files rather than pipes: a case that writes more than a pipe buffer
/// before exiting would otherwise deadlock the harness, and the files are evidence a reader can
/// open afterwards.
fn run_with_timeout(
    argv: &[String],
    case_dir: &Path,
    workdir: &Path,
    driver_env: &[(&str, String)],
    timeout: Duration,
) -> io::Result<(Option<i32>, bool)> {
    let stdout = fs::File::create(case_dir.join("mur-stdout.txt"))?;
    let stderr = fs::File::create(case_dir.join("mur-stderr.txt"))?;

    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .current_dir(workdir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    for (key, value) in driver_env {
        command.env(key, value);
    }

    let mut child = command.spawn()?;
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok((status.code(), false));
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Ok((None, true));
        }
        sleep(Duration::from_millis(200));
    }
}

/// Reads the `VERDICT=`/`DETAIL=` pair the probe wrote.
fn read_probe_file(workdir: &Path) -> Option<(Verdict, String)> {
    let text = fs::read_to_string(workdir.join(probe::PROBE_FILE)).ok()?;
    let mut verdict = None;
    let mut detail = String::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("VERDICT=") {
            verdict = Verdict::parse(rest);
        } else if let Some(rest) = line.strip_prefix("DETAIL=") {
            detail = rest.to_string();
        }
    }
    verdict.map(|v| (v, detail))
}

/// Every `trace.jsonl` under the capsule workdir's `.murmur/` session directories.
fn trace_files(workdir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(sessions) = fs::read_dir(workdir.join(".murmur")) else {
        return found;
    };
    for session in sessions.flatten() {
        let trace = session.path().join("trace.jsonl");
        if trace.is_file() {
            found.push(trace);
        }
    }
    found
}

/// The `resource_limit` attribution the trace carried, if any.
///
/// **Supplementary evidence, never the grading source.** `resource-limits-manual-verification.md`
/// grades its scenarios on this string, but that document assumes the HTTP-driver agent loop,
/// which writes a `shell` event per tool call. This harness drives cases through the
/// process-transport Claude Bridge, and `claude_bridge::dispatch_tool_call` returns the tool
/// result without writing any trace event at all — so on this path `trace.jsonl` carries no
/// `shell` record and therefore no `resource_limit`, no matter what the kernel did. Grading on it
/// would report every contained ceiling as uncontained. It is still read and folded into DETAIL,
/// because when it *is* present it is the runtime's own attribution and worth having.
///
/// Parsed as JSON rather than grepped: a substring match would credit an attribution that
/// appeared anywhere in the line, including inside a captured stderr string.
fn trace_resource_limit(workdir: &Path) -> Option<String> {
    for path in trace_files(workdir) {
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        for line in content.lines() {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if let Some(limit) = value.get("resource_limit").and_then(|v| v.as_str()) {
                if !limit.is_empty() {
                    return Some(limit.to_string());
                }
            }
        }
    }
    None
}

/// Everything `mur` said, plus the session bootstrap log, as one searchable blob.
fn session_output(case_dir: &Path, workdir: &Path) -> String {
    let mut text = String::new();
    for name in ["mur-stdout.txt", "mur-stderr.txt"] {
        if let Ok(content) = fs::read_to_string(case_dir.join(name)) {
            text.push_str(&content);
        }
    }
    if let Ok(sessions) = fs::read_dir(workdir.join(".murmur")) {
        for session in sessions.flatten() {
            let log = session.path().join("logs").join("bootstrap.log");
            if let Ok(content) = fs::read_to_string(log) {
                text.push_str(&content);
            }
        }
    }
    text
}

/// The shell tool's exit code out of the tool-result text the driver recorded.
///
/// `shell_result_to_tool_result` renders it as `Exit code: <n>`; a signal is kept legible as
/// `128 + signo` rather than collapsed to `-1`, which is what makes an OOM kill distinguishable
/// from an ordinary failure here.
fn shell_exit_code(driver_summary: &str) -> Option<i32> {
    let rest = driver_summary.split("Exit code:").nth(1)?;
    rest.trim()
        .split(|c: char| !c.is_ascii_digit() && c != '-')
        .find(|token| !token.is_empty())?
        .parse()
        .ok()
}

/// First line of `mur`'s output mentioning `needle`, trimmed for the record.
fn excerpt(text: &str, needle: &str) -> String {
    text.lines()
        .find(|line| line.contains(needle))
        .map(|line| {
            let trimmed = line.trim();
            if trimmed.len() > 240 {
                format!("{}…", &trimmed[..240])
            } else {
                trimmed.to_string()
            }
        })
        .unwrap_or_default()
}

/// Confirms a probe can start at all on this host, before any case runs.
///
/// Returns `Ok(evidence)` when the shell-tool path is live, `Err(refusal)` when it is not. The
/// caller treats an `Err` as a refusal — exit non-zero, no record — for the same reason the
/// containment-class gate does: a run in which nothing could execute would report every asserted
/// case as a failure, and "twenty-three boundary escapes" is a far more damaging false statement
/// than "this harness declined to measure anything here".
///
/// The hint on failure names the one cause seen so far in practice, because it is invisible from
/// the outside and costs an afternoon to rediscover.
pub fn preflight(config: &RunnerConfig, class: murmur_artifact::ContainmentClass) -> Result<String, String> {
    let outcome = run_case(&crate::cases::PREFLIGHT, config, class);
    if outcome.verdict == Verdict::Succeeded {
        return Ok(outcome.detail);
    }
    Err(format!(
        "REFUSED — no probe can run on this host, so nothing can be measured.\n\n\
         The preflight capsule could not start its interpreter:\n\
         \x20 {}\n\n\
         Every case would report INCONCLUSIVE and every asserted case would fail, which would \
         read as a boundary escape when in fact nothing was exercised. No record file was \
         written.\n\n\
         Known cause worth checking first — `capabilities.shell.allow` is enforced by Landlock \
         `Execute` rights (`sandbox::linux_enforce::apply_landlock_scope`), so a binary reachable \
         on this host but absent from the derived grant set gets EACCES on `execve` before it \
         runs, and the tool result reads exactly `Permission denied (os error 13)`. The two shapes \
         that produce it: an interpreter whose real path `resolve_exec_allowlist` did not resolve \
         at launch (check `PATH` as `mur` sees it), and an interpreter whose stdlib or shared \
         libraries live outside both the workdir and the derived `DT_NEEDED` closure — which is \
         what `capabilities.shell.interpreter_runtime` and `.staged_runtime` exist to declare. \
         Note also that nothing the capsule writes into its own workdir can be executed at all \
         unless the manifest declares `capabilities.filesystem.workdir_exec: true`; a preflight \
         that stages its interpreter into the workdir needs that key.\n\
         (Before the exec supervisor was retired this hint named `prctl(PR_SET_DUMPABLE, 0)` and \
         a `/proc/<pid>/mem` read instead. That mechanism is gone: nothing reads the child's \
         memory any more, and the dumpable restore went with it.)\n\n\
         Artifacts for this preflight run: {}",
        outcome.detail,
        outcome.case_dir.display()
    ))
}

/// Runs one case end to end and grades it.
pub fn run_case(
    case: &'static Case,
    config: &RunnerConfig,
    class: murmur_artifact::ContainmentClass,
) -> CaseOutcome {
    let expectation = case.expectation(class);

    let (case_dir, workdir, manifest) = match stage(case, config, class) {
        Ok(paths) => paths,
        Err(err) => {
            return CaseOutcome {
                case,
                expectation,
                verdict: Verdict::Inconclusive,
                detail: format!("could not stage the case: {err}"),
                passed: !expectation.gates(),
                case_dir: config.work_root.join(case.id),
            };
        }
    };

    let argv = build_argv(case, config, &manifest, &workdir);
    let driver_log = case_dir.join("probe-driver.txt");
    let mut driver_env = vec![
        ("MURMUR_EC_TOOL", "python3".to_string()),
        ("MURMUR_EC_SCRIPT", probe::PROBE_SCRIPT.to_string()),
        ("MURMUR_EC_CASE", case.id.to_string()),
        ("MURMUR_EC_DRIVER_LOG", driver_log.display().to_string()),
    ];
    if let Evidence::SecondSpawnRefused(_) = case.evidence {
        driver_env.push(("MURMUR_EC_SCRIPT2", probe::SECOND_SCRIPT.to_string()));
    }

    let (exit_code, timed_out) =
        match run_with_timeout(&argv, &case_dir, &workdir, &driver_env, config.timeout) {
            Ok(result) => result,
            Err(err) => {
                return CaseOutcome {
                    case,
                    expectation,
                    verdict: Verdict::Inconclusive,
                    detail: format!("could not launch `{}`: {err}", argv.join(" ")),
                    passed: !expectation.gates(),
                    case_dir,
                };
            }
        };

    let output = session_output(&case_dir, &workdir);
    let mut launch_refusal = ["E-RUN-012", "E-RUN-007", "E-RUN-008"]
        .into_iter()
        .find(|code| output.contains(code))
        .map(|code| excerpt(&output, code));
    // What the tool call itself came back with. When the probe never ran, this is usually the
    // only description of why — `mur run` reports `status: ok` for a session whose single tool
    // call was refused, so the exit code says nothing.
    if let Ok(driver) = fs::read_to_string(&driver_log) {
        let driver = driver.trim();
        if driver.contains("isError=true") || driver.contains("probe-driver failed") {
            launch_refusal = Some(format!("probe-driver: {driver}"));
        }
    }

    // What the tool call returned, whatever happened. When the probe was killed mid-case this is
    // the only description of how it died, and it is the difference between a diagnosable result
    // and a bare INCONCLUSIVE.
    let driver_summary = fs::read_to_string(&driver_log)
        .map(|text| text.trim().to_string())
        .unwrap_or_else(|_| "the probe driver left no record of its tool call".to_string());

    let (verdict, mut detail) = match case.evidence {
        Evidence::ProbeFile => match read_probe_file(&workdir) {
            Some(found) => found,
            None => (
                Verdict::Inconclusive,
                format!(
                    "the probe wrote no verdict file — a missing result is NOT a clean result. \
                     `mur run` exited {}; the tool call reported: {driver_summary}",
                    exit_code
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "on a timeout".to_string())
                ),
            ),
        },
        Evidence::SecondSpawnRefused(needle) => {
            let second = driver_summary
                .split("|| SECOND-CALL")
                .nth(1)
                .map(str::trim)
                .unwrap_or("");
            if second.is_empty() {
                (
                    Verdict::Inconclusive,
                    format!(
                        "the follow-up tool call never happened, so the latch had no spawn to \
                         refuse and nothing was measured. Tool call: {driver_summary}"
                    ),
                )
            } else if second.contains(needle) {
                (
                    Verdict::Contained,
                    format!(
                        "the next subprocess spawn after the breach was refused, naming \
                         `{needle}`: {second}"
                    ),
                )
            } else {
                (
                    Verdict::Uncontained,
                    format!(
                        "the next subprocess spawn after the breach was NOT refused with \
                         `{needle}` — the periodic check never latched. Follow-up call: {second}"
                    ),
                )
            }
        }
        Evidence::ShellExit { contained } => match shell_exit_code(&driver_summary) {
            Some(code) if contained.contains(&code) => (
                Verdict::Contained,
                format!(
                    "the shell tool exited {code} — the ceiling killed the process. Tool call: \
                     {driver_summary}"
                ),
            ),
            Some(code) => (
                Verdict::Uncontained,
                format!(
                    "the shell tool exited {code}, which is not one of the codes that mean the \
                     ceiling bit ({contained:?}). Tool call: {driver_summary}"
                ),
            ),
            None => (
                Verdict::Inconclusive,
                format!(
                    "no exit code could be read from the tool result, so nothing was measured. \
                     Tool call: {driver_summary}"
                ),
            ),
        },
    };

    // Supplementary, for the resource category only: the runtime's own attribution when it made
    // one. Never grades — see `trace_resource_limit`.
    if case.category == crate::verdict::Category::ResourceExhaustion {
        detail = match trace_resource_limit(&workdir) {
            Some(limit) => format!("{detail} [trace.jsonl attribution: resource_limit={limit}]"),
            None => format!(
                "{detail} [trace.jsonl carried no resource_limit attribution; expected on this \
                 path, since the process-transport bridge writes no shell event]"
            ),
        };
    }

    if timed_out {
        detail = format!(
            "[case exceeded the {}s ceiling and was killed] {detail}",
            config.timeout.as_secs()
        );
    }
    if let Some(refusal) = launch_refusal {
        if !detail.contains(&refusal) {
            detail = format!("{detail} [mur reported: {refusal}]");
        }
    }

    let passed = expectation.is_satisfied_by(verdict);
    CaseOutcome {
        case,
        expectation,
        verdict,
        detail: crate::record::sanitize_cell(&detail),
        passed,
        case_dir,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cases;

    fn config() -> RunnerConfig {
        RunnerConfig {
            mur: PathBuf::from("/nonexistent/mur"),
            probe_driver: PathBuf::from("/nonexistent/probe-driver"),
            work_root: PathBuf::from("/tmp/escape-conformance-test"),
            timeout: Duration::from_secs(1),
            systemd_scope: false,
            interpreter_dirs: vec![("/usr/lib/python3".to_string(), true)],
        }
    }

    #[test]
    fn boundary_manifest_has_no_resource_block_and_declares_no_network() {
        let case = cases::find("read-etc-shadow").unwrap();
        let yaml = manifest_yaml(case, &config(), murmur_artifact::ContainmentClass::Scoped);
        assert!(yaml.contains("allow:\n      - bash\n      - python3"));
        assert!(!yaml.contains("resources:"));
        // Nothing may be declared network-reachable, or the four network cases assert nothing.
        assert!(!yaml.contains("network:"));
        assert!(yaml.contains("transport: process"));
    }

    #[test]
    fn manifest_declares_the_containment_class_under_test() {
        let case = cases::find("read-etc-shadow").unwrap();
        for class in murmur_artifact::ContainmentClass::ALL {
            let yaml = manifest_yaml(case, &config(), class);
            assert!(
                yaml.contains(&format!("containment: {class}\n")),
                "manifest for class {class} must declare it, or applied_tier never installs \
                 that class's mechanism and the case's verdict is measuring the wrong class \
                 entirely: {yaml}"
            );
        }
    }

    #[test]
    fn resource_manifest_carries_the_tight_ceilings() {
        let case = cases::find("resource-fork-bomb").unwrap();
        let yaml = manifest_yaml(case, &config(), murmur_artifact::ContainmentClass::Scoped);
        assert!(yaml.contains("cgroup_pids_max: 32"));
        assert!(yaml.contains("workdir_max_bytes: 52428800"));
        // 64, not the manual-verification document's 16: below roughly 64 the seccomp filter
        // cannot be installed in the child's pre_exec window and every spawn fails outright.
        // Pinned here so the value cannot drift back without the reason being re-read.
        assert!(yaml.contains("max_open_files: 64"));
        assert!(yaml.contains("max_processes: 512"));
    }

    #[test]
    fn the_fd_leak_case_launches_mur_from_a_shell_holding_the_descriptor() {
        let case = cases::find("inherited-fd-after-exec").unwrap();
        let argv = build_argv(
            case,
            &config(),
            Path::new("/tmp/m.yaml"),
            Path::new("/tmp/wd"),
        );
        assert_eq!(argv[0], "bash");
        assert!(argv[2].contains("exec 7</etc/hostname"));
        assert!(argv.iter().any(|a| a.ends_with("mur")));
    }

    #[test]
    fn a_normal_case_launches_mur_directly() {
        let case = cases::find("read-etc-shadow").unwrap();
        let argv = build_argv(
            case,
            &config(),
            Path::new("/tmp/m.yaml"),
            Path::new("/tmp/wd"),
        );
        assert!(argv[0].ends_with("mur"));
        assert_eq!(argv[1], "run");
    }

    #[test]
    fn shell_exit_code_is_read_out_of_the_tool_result_text() {
        assert_eq!(
            shell_exit_code("case=x tool=python3 isError=false :: $ ec-probe.py Exit code: 137 Stdout:  Stderr:"),
            Some(137)
        );
        assert_eq!(shell_exit_code("$ ec-probe.py Exit code: 0 Stdout: hi"), Some(0));
        assert_eq!(shell_exit_code("no exit code here"), None);
    }

    #[test]
    fn systemd_scope_wraps_but_does_not_replace_mur() {
        let mut cfg = config();
        cfg.systemd_scope = true;
        let case = cases::find("read-etc-shadow").unwrap();
        let argv = build_argv(case, &cfg, Path::new("/tmp/m.yaml"), Path::new("/tmp/wd"));
        assert_eq!(argv[0], "systemd-run");
        assert!(argv.contains(&"--property=Delegate=yes".to_string()));
        assert!(argv.iter().any(|a| a.ends_with("mur")));
    }
}
