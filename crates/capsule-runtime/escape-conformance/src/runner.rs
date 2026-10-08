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
//! driven by this package's own process driver (`crate::driver_artifact`) and its `probe-driver`
//! harness instead of a subscription CLI. `mur run` stands up the tool bridge, advertises the
//! capsule's `shell.allow` binaries over it, and spawns `probe-driver` with the bridge and the
//! case in its environment; `probe-driver` makes one or two predetermined tool calls and exits.
//! Tool execution, capability enforcement and the trace are all murmur's, byte for byte the same
//! path a live model would drive — only the *choice* of which tool to call is scripted rather than
//! sampled.
//!
//! That is what makes this a gate rather than a demonstration. A release
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
use crate::driver_artifact;
use crate::harness_protocol::ProbeConfig;
use crate::probe;
use crate::verdict::{Expectation, Verdict};

/// Everything a run needs that is not the case itself.
pub struct RunnerConfig {
    /// The built `mur` binary. Cases drive real enforcement through it.
    pub mur: PathBuf,
    /// This package's `probe-driver`, named as `inference.command` in every generated manifest.
    pub probe_driver: PathBuf,
    /// The `escape-conformance-driver` artifact `driver_artifact::build_artifact` packed for this
    /// run, installed into every case's project store before its `mur run`.
    pub driver_artifact: PathBuf,
    /// Root under which each case gets its own directory, as
    /// `crate::work_root::prepare_work_root` returned it: empty but for its marker when the run
    /// starts. Kept after the run: it holds the generated manifest, the probe source, `mur`'s
    /// stdout/stderr and the session trace, and the record points at it per case.
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
    /// How far the run got before it was graded. `classify_preflight` reads it.
    pub reach: RunReach,
}

/// How far one case's run got, independent of the verdict it was graded on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunReach {
    /// The case directory could not be staged, so nothing was launched.
    NotStaged,
    /// Staged, but the launch command could not be spawned.
    NotLaunched,
    /// `mur run` was spawned and then exited or was killed.
    Ran {
        /// Killed at `RunnerConfig::timeout`.
        timed_out: bool,
        /// The first `LAUNCH_REFUSAL_CODES` or `HARNESS_CODES` entry found in `mur`'s output, as
        /// the whole code: `E-MAN-003` rather than the `E-MAN-` prefix it was matched on.
        refusal_code: Option<String>,
        /// The probe driver's summary line, `None` when it left none.
        tool_result: Option<String>,
    },
}

/// Derives the host directories `python3` needs outside the workdir, by asking the interpreter.
///
/// Hardcoding `/usr/lib/python3.11` (as `containment.md`'s example does) would break on the
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
        if !base.is_empty()
            && Path::new(&dynload).is_dir()
            && !dirs.iter().any(|(p, _)| p == &dynload)
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
/// capsule, with the divergences noted inline. `max_processes` is left out: every case here runs
/// in a cgroup scope, where the runtime sets no `RLIMIT_NPROC` and `cgroup_pids_max` is the only
/// process bound.
fn tight_resources_yaml() -> &'static str {
    "  resources:\n\
    \x20   # 128 rather than the document's 16, and the difference is a finding rather than a\n\
    \x20   # preference. `apply_hard_rlimits` runs first in the child's pre_exec window, so every\n\
    \x20   # later step in that window lives under this ceiling — and the Landlock grant fds, the\n\
    \x20   # netns/uid_map setup and the seccomp filter all need descriptors while it holds.\n\
    \x20   # Observed on Linux 7.0.0/libseccomp 2.5.5: at 16 and at 32 every spawn dies with\n\
    \x20   # `shell enforcement setup failed before exec: There was a system failure beyond the\n\
    \x20   # control of libseccomp`. 64 worked until slice fb1eea97 added one Landlock grant per\n\
    \x20   # `SEALED_ETC_PATHS` entry: sixteen more fds held open across that window put a `sealed`\n\
    \x20   # capsule back over the line, and every resource case reported\n\
    \x20   # `egress-netns: writing uid_map/gid_map failed` instead of its ceiling. Measured on\n\
    \x20   # that host, a [bash, python3] sealed capsule spawns at 72 and refuses at 64; 128 is\n\
    \x20   # that floor with room for a grant set that grows again. The ceiling still bites, and\n\
    \x20   # still far below the host default, so the fd-exhauster case stays runnable — which is\n\
    \x20   # the constraint that rules out simply raising it to the sky.\n\
    \x20   max_open_files: 128\n\
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

/// A YAML scalar for `value`, JSON-quoted. JSON strings are YAML flow scalars, so a path with a
/// space, a colon or a `#` reads back as itself.
fn yaml_str(value: &str) -> String {
    serde_json::to_string(value).expect("a string serializes")
}

/// What `probe-driver` is told about one case, through `inference.driver.config`.
pub fn probe_config(case: &Case, driver_log: &Path) -> ProbeConfig {
    ProbeConfig {
        tool: "python3".to_string(),
        script: probe::PROBE_SCRIPT.to_string(),
        script2: match case.evidence {
            Evidence::SecondSpawnRefused(_) => Some(probe::SECOND_SCRIPT.to_string()),
            _ => None,
        },
        case: case.id.to_string(),
        log: driver_log.display().to_string(),
    }
}

/// The manifest one case runs under. `driver_log` is where `probe-driver` writes its summary.
///
/// Declares `capabilities.containment: <class>` explicitly. Without this, every case would run at
/// the manifest default (`advisory`), and on a `sealed`-capable host `applied_tier` would still
/// install `KernelFull` (Landlock+seccomp, `scoped`'s mechanism) — never the composed root — no
/// matter what `--class` was passed to this binary. The banner and grading table would say
/// `sealed` while every probe actually ran under `scoped`, which is exactly the "false assurance"
/// this harness exists to prevent (see `lib.rs`'s module docs).
///
/// `every_generated_manifest_parses_as_mur_run_parses_it` holds the result to the parser `mur run`
/// calls.
pub fn manifest_yaml(
    case: &Case,
    config: &RunnerConfig,
    class: murmur_artifact::ContainmentClass,
    driver_log: &Path,
) -> String {
    let mut yaml = String::new();
    yaml.push_str(&format!("name: escape-conformance-{}\n", case.id));
    yaml.push_str("version: 0.0.1\n\n");
    // A process driver has to come out of a store: `stage` installs this one into the case's
    // project store before `mur run` resolves it.
    yaml.push_str("artifacts:\n");
    yaml.push_str(&format!("  - name: {}\n", driver_artifact::DRIVER_NAME));
    yaml.push_str(&format!(
        "    version: {}\n",
        driver_artifact::DRIVER_VERSION
    ));
    yaml.push_str("    runtime: driver\n\n");
    yaml.push_str("capabilities:\n");
    yaml.push_str(&format!("  containment: {class}\n"));
    yaml.push_str("  shell:\n");
    // `bash` is allowlisted so `exec-renamed-disallowed-binary` has an allowlisted basename to
    // wear; `python3` is what every probe actually runs as.
    yaml.push_str("    allow:\n      - bash\n      - python3\n");
    if !config.interpreter_dirs.is_empty() {
        yaml.push_str("    interpreter_runtime:\n      - binary: python3\n        dirs:\n");
        for (path, list_dir) in &config.interpreter_dirs {
            yaml.push_str(&format!("          - path: {}\n", yaml_str(path)));
            yaml.push_str(&format!("            list_dir: {list_dir}\n"));
        }
    }
    if case.profile == Profile::TightResources {
        yaml.push_str(tight_resources_yaml());
    }
    // `network.allow` is left entirely undeclared, so every destination is unlisted and
    // `network.unix_sockets` keeps its default of false — which is what the four network cases
    // and the two AF_UNIX cases assert against. `env.allow` is left undeclared too: the harness
    // environment then holds only what the driver's launch plan sets, and so does every probe
    // subprocess's.
    //
    // A shell command that outruns `lifecycle.shell_grace_secs` is demoted to the background and
    // its tool result carries a `wrk_` handle instead of an exit code, which a `ShellExit` case
    // cannot grade. A resource case can run far past the default grace, so the grace matches how
    // long the harness waits for the case.
    yaml.push_str("\nlifecycle:\n");
    yaml.push_str(&format!(
        "  shell_grace_secs: {}\n",
        config.timeout.as_secs()
    ));
    let probe = probe_config(case, driver_log);
    yaml.push_str("\ninference:\n");
    yaml.push_str("  transport: process\n");
    yaml.push_str("  driver:\n");
    yaml.push_str(&format!("    artifact: {}\n", driver_artifact::DRIVER_NAME));
    yaml.push_str("    config:\n");
    yaml.push_str(&format!("      tool: {}\n", yaml_str(&probe.tool)));
    yaml.push_str(&format!("      script: {}\n", yaml_str(&probe.script)));
    if let Some(script2) = &probe.script2 {
        yaml.push_str(&format!("      script2: {}\n", yaml_str(script2)));
    }
    yaml.push_str(&format!("      case: {}\n", yaml_str(&probe.case)));
    yaml.push_str(&format!("      log: {}\n", yaml_str(&probe.log)));
    // The binary override: the driver's `describe().binary` names `probe-driver` bare, and this
    // package's build is not on any PATH `mur` would search.
    yaml.push_str(&format!(
        "  command: {}\n",
        yaml_str(&config.probe_driver.display().to_string())
    ));
    yaml.push_str("  max_turns: 2\n");
    yaml
}

/// Where `probe-driver` writes its summary line for the case staged in `case_dir`.
fn driver_log_path(case_dir: &Path) -> PathBuf {
    case_dir.join("probe-driver.txt")
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
    fs::write(
        &manifest,
        manifest_yaml(case, config, class, &driver_log_path(&case_dir)),
    )?;
    // `mur install` upserts the artifact into the project the manifest's directory names, and
    // `mur run --manifest` resolves its artifacts from that same project's store.
    driver_artifact::install_into(&config.mur, &config.driver_artifact, &case_dir)
        .map_err(io::Error::other)?;

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
/// which writes a `shell` event, carrying `resource_limit`, per tool call. This harness drives
/// cases through process transport, where `ProcessEventSink` writes a `tool_call` record for each
/// call the harness reports — tool name, input, output size, duration and `ok`/`error` status —
/// and the bridge writes no `shell` event. So on this path `trace.jsonl` carries no
/// `resource_limit`, no matter what the kernel did, and grading on it would report every contained
/// ceiling as uncontained. It is still read and folded into DETAIL, because when it *is* present
/// it is the runtime's own attribution and worth having.
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
/// than "this harness declined to measure anything here". The refusal carries the hint for the
/// check that failed, from `classify_preflight` and `preflight_hint`.
pub fn preflight(
    config: &RunnerConfig,
    class: murmur_artifact::ContainmentClass,
) -> Result<String, String> {
    let outcome = run_case(&crate::cases::PREFLIGHT, config, class);
    if outcome.verdict == Verdict::Succeeded {
        return Ok(outcome.detail);
    }
    Err(preflight_refusal(&outcome))
}

/// The refusal `preflight` returns for an outcome that did not report `SUCCESS`.
pub fn preflight_refusal(outcome: &CaseOutcome) -> String {
    let check = classify_preflight(outcome);
    format!(
        "REFUSED — no probe can run on this host, so nothing can be measured.\n\n\
         The preflight capsule failed the {} check:\n\
         \x20 {}\n\n\
         Every case would report INCONCLUSIVE and every asserted case would fail, which would \
         read as a boundary escape when in fact nothing was exercised. No record file was \
         written.\n\n\
         {}\n\n\
         Artifacts for this preflight run: {}",
        check.name(),
        outcome.detail,
        preflight_hint(&check),
        outcome.case_dir.display()
    )
}

/// The step of the preflight case that failed, in the order a run reaches them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreflightCheck {
    /// The case directory could not be written, or the driver artifact not installed into it.
    Stage,
    /// `mur run` could not be spawned (`code: None`) or refused the case with a diagnostic that
    /// is not a harness failure.
    Launch { code: Option<String> },
    /// The `probe-driver` harness never reported a tool result: `mur` named a harness failure
    /// (`HARNESS_CODES`), the case hit its timeout, or the driver left no summary.
    Harness {
        code: Option<String>,
        timed_out: bool,
    },
    /// The tool call reached `execve` and the kernel refused the interpreter with `EACCES`.
    InterpreterExec,
    /// The tool call ran, and the probe left no `SUCCESS` verdict in `probe::PROBE_FILE`.
    ProbeFile,
}

impl PreflightCheck {
    /// The check's name as the refusal and its hint print it.
    pub fn name(&self) -> &'static str {
        match self {
            PreflightCheck::Stage => "stage",
            PreflightCheck::Launch { .. } => "launch",
            PreflightCheck::Harness { .. } => "harness",
            PreflightCheck::InterpreterExec => "interpreter exec",
            PreflightCheck::ProbeFile => "probe file",
        }
    }
}

/// `mur` diagnostics that mean the harness failed rather than that `mur run` refused the capsule:
/// `E-RUN-006` the harness binary was not found, `E-RUN-033` the harness reported a failed turn,
/// `E-RUN-034` a driver call failed, `E-RUN-035` the harness went quiet past the inactivity limit.
const HARNESS_CODES: [&str; 4] = ["E-RUN-006", "E-RUN-033", "E-RUN-034", "E-RUN-035"];

/// How the kernel's `EACCES` reads in a tool result: `io::Error`'s rendering of errno 13.
const EXEC_DENIED: &str = "Permission denied (os error 13)";

/// Which preflight check `outcome` failed. Meaningful only for an outcome that did not report
/// `SUCCESS`; one that did classifies as [`PreflightCheck::ProbeFile`].
///
/// An `EACCES` in the tool result wins over every diagnostic code: it is the tool call's own
/// account of why the interpreter never ran.
pub fn classify_preflight(outcome: &CaseOutcome) -> PreflightCheck {
    let (timed_out, refusal_code, tool_result) = match &outcome.reach {
        RunReach::NotStaged => return PreflightCheck::Stage,
        RunReach::NotLaunched => return PreflightCheck::Launch { code: None },
        RunReach::Ran {
            timed_out,
            refusal_code,
            tool_result,
        } => (*timed_out, refusal_code, tool_result),
    };
    if tool_result
        .as_deref()
        .is_some_and(|result| result.contains(EXEC_DENIED))
    {
        return PreflightCheck::InterpreterExec;
    }
    match refusal_code {
        Some(code) if HARNESS_CODES.contains(&code.as_str()) => PreflightCheck::Harness {
            code: Some(code.clone()),
            timed_out,
        },
        Some(code) => PreflightCheck::Launch {
            code: Some(code.clone()),
        },
        None if timed_out
            || tool_result
                .as_deref()
                .is_none_or(|result| result.starts_with("probe-driver failed")) =>
        {
            PreflightCheck::Harness {
                code: None,
                timed_out,
            }
        }
        None => PreflightCheck::ProbeFile,
    }
}

/// What to check first when the preflight failed `check`. Starts `Failed check: <name>.`
pub fn preflight_hint(check: &PreflightCheck) -> String {
    let name = check.name();
    match check {
        PreflightCheck::Stage => format!(
            "Failed check: {name}. The harness could not write the preflight case's directory, or \
             `mur install` could not install the {driver} artifact into it, so `mur run` never \
             started. The error above names the step; check that the work root is writable and \
             that `mur install` accepts the archive packed into the work root.",
            driver = driver_artifact::DRIVER_NAME
        ),
        PreflightCheck::Launch { code: None } => format!(
            "Failed check: {name}. The launch command could not be spawned at all. Check the \
             `--mur` path, and that `systemd-run` is on PATH when `--systemd-scope` is on."
        ),
        PreflightCheck::Launch { code: Some(code) } if code.starts_with("E-MAN-") => format!(
            "Failed check: {name}. `mur run` refused the generated manifest with `{code}`: it no \
             longer meets the manifest contract `mur run` enforces. \
             `cargo test -p escape-conformance` reproduces it without a host."
        ),
        PreflightCheck::Launch { code: Some(code) } if code == "E-RUN-012" => format!(
            "Failed check: {name}. `mur run` refused the capsule with `E-RUN-012`: it can spawn \
             subprocesses and no cgroup v2 scope could be delegated to bound them. `mur` asks the \
             systemd user session for a delegated scope, and otherwise needs the cgroup it \
             inherited to carry `Delegate=yes`. Leave `--systemd-scope` on, or run the harness \
             itself under `systemd-run --user --scope --property=Delegate=yes`, and check that \
             the systemd user manager is running for this user."
        ),
        PreflightCheck::Launch { code: Some(code) } => format!(
            "Failed check: {name}. `mur run` refused the preflight capsule with `{code}`. The \
             diagnostics reference explains the code; `mur-stderr.txt` in the case directory \
             carries the full message."
        ),
        PreflightCheck::Harness { code, timed_out } => {
            let cause = match (code, timed_out) {
                (_, true) => "the case hit its `--timeout-secs` ceiling and was killed".to_string(),
                (Some(code), false) => format!("`mur` reported `{code}`"),
                (None, false) => "the harness left no summary of its tool call".to_string(),
            };
            format!(
                "Failed check: {name}. The `probe-driver` harness never reported a tool result: \
                 {cause}. Check that `--probe-driver` names this build's `probe-driver`, read \
                 `mur-stderr.txt` and `probe-driver.txt` in the case directory, and on a loaded \
                 host raise `--timeout-secs`."
            )
        }
        PreflightCheck::InterpreterExec => format!(
            "Failed check: {name}. The tool result reads `{EXEC_DENIED}`: the kernel refused \
             `execve` of the interpreter. `capabilities.shell.allow` is enforced by Landlock \
             `Execute` rights (`sandbox::linux_enforce::apply_landlock_scope`), so a binary \
             reachable on this host but absent from the derived grant set gets EACCES before it \
             runs. The two shapes that produce it: an interpreter whose real path \
             `resolve_exec_allowlist` did not resolve at launch (check `PATH` as `mur` sees it), \
             and an interpreter whose stdlib or shared libraries live outside both the workdir and \
             the derived `DT_NEEDED` closure — which is what \
             `capabilities.shell.interpreter_runtime` and `.staged_runtime` exist to declare. \
             Nothing the capsule writes into its own workdir can be executed unless the manifest \
             declares `capabilities.filesystem.workdir_exec: true`."
        ),
        PreflightCheck::ProbeFile => format!(
            "Failed check: {name}. The tool call ran, but the probe left no `SUCCESS` verdict in \
             `wd/{probe_file}`. The tool result above carries the probe's own output: a Python \
             traceback means the probe script failed, an exit code with no output means it was \
             killed before writing.",
            probe_file = probe::PROBE_FILE
        ),
    }
}

/// What `mur` prints when it refused a case's launch, matched as substrings of its output.
/// `E-MAN-` matches every manifest code. Searched before [`HARNESS_CODES`], which together with
/// these are the refusal codes a run is read for.
const LAUNCH_REFUSAL_CODES: [&str; 4] = ["E-RUN-012", "E-RUN-007", "E-RUN-008", "E-MAN-"];

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
                reach: RunReach::NotStaged,
            };
        }
    };

    let argv = build_argv(case, config, &manifest, &workdir);
    let driver_log = driver_log_path(&case_dir);

    let (exit_code, timed_out) = match run_with_timeout(&argv, &case_dir, &workdir, config.timeout)
    {
        Ok(result) => result,
        Err(err) => {
            return CaseOutcome {
                case,
                expectation,
                verdict: Verdict::Inconclusive,
                detail: format!("could not launch `{}`: {err}", argv.join(" ")),
                passed: !expectation.gates(),
                case_dir,
                reach: RunReach::NotLaunched,
            };
        }
    };

    let output = session_output(&case_dir, &workdir);
    let refusal_prefix = LAUNCH_REFUSAL_CODES
        .into_iter()
        .chain(HARNESS_CODES)
        .find(|code| output.contains(code));
    let refusal_code = refusal_prefix.map(|prefix| whole_code(&output, prefix));
    let mut launch_refusal = refusal_prefix.map(|code| excerpt(&output, code));
    // What the tool call returned, whatever happened. When the probe was killed mid-case this is
    // the only description of how it died, and it is the difference between a diagnosable result
    // and a bare INCONCLUSIVE.
    let tool_result = fs::read_to_string(&driver_log)
        .ok()
        .map(|text| text.trim().to_string());
    // When the probe never ran, the tool result is usually the only description of why — `mur
    // run` reports `status: ok` for a session whose single tool call was refused, so the exit
    // code says nothing.
    if let Some(driver) = &tool_result {
        if driver.contains("isError=true") || driver.contains("probe-driver failed") {
            launch_refusal = Some(format!("probe-driver: {driver}"));
        }
    }
    let driver_summary = tool_result
        .clone()
        .unwrap_or_else(|| "the probe driver left no record of its tool call".to_string());

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
        reach: RunReach::Ran {
            timed_out,
            refusal_code,
            tool_result,
        },
    }
}

/// The whole diagnostic code at the first occurrence of `prefix` in `output`: `E-MAN-` in
/// `error[E-MAN-003]: …` reads back as `E-MAN-003`.
fn whole_code(output: &str, prefix: &str) -> String {
    let start = output.find(prefix).unwrap_or_default();
    output[start..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cases;
    use murmur_artifact::{ArtifactRuntime, ContainmentClass, RuntimeManifest};

    fn log() -> PathBuf {
        PathBuf::from("/tmp/escape-conformance-test/case/probe-driver.txt")
    }

    fn config() -> RunnerConfig {
        RunnerConfig {
            mur: PathBuf::from("/nonexistent/mur"),
            probe_driver: PathBuf::from("/nonexistent/probe-driver"),
            driver_artifact: PathBuf::from("/nonexistent/escape-conformance-driver-0.1.0.mur.zip"),
            work_root: PathBuf::from("/tmp/escape-conformance-test"),
            timeout: Duration::from_secs(1),
            systemd_scope: false,
            interpreter_dirs: vec![("/usr/lib/python3".to_string(), true)],
        }
    }

    #[test]
    fn a_second_run_on_one_work_root_grades_only_its_own_files() {
        let work_root =
            crate::work_root::prepare_work_root(&crate::work_root::tests::scratch_path("rerun"))
                .expect("a fresh work root");

        // The files a first run leaves at the paths `stage`, the probe and the driver use.
        let case_dir = work_root.join(cases::PREFLIGHT.id);
        let workdir = case_dir.join("wd");
        let session = workdir.join(".murmur").join("sess-earlier");
        fs::create_dir_all(&session).unwrap();
        fs::write(
            workdir.join(probe::PROBE_FILE),
            "VERDICT=SUCCESS\nDETAIL=from the earlier run\n",
        )
        .unwrap();
        fs::write(
            driver_log_path(&case_dir),
            "case=preflight tool=python3 :: isError=false :: Exit code: 0\n",
        )
        .unwrap();
        fs::write(
            session.join("trace.jsonl"),
            "{\"resource_limit\":\"cgroup_pids_max\"}\n",
        )
        .unwrap();
        assert_eq!(
            read_probe_file(&workdir).map(|(verdict, _)| verdict),
            Some(Verdict::Succeeded)
        );
        assert_eq!(trace_files(&workdir).len(), 1);

        let again = crate::work_root::prepare_work_root(&work_root).expect("a marked work root");
        assert_eq!(again, work_root);
        assert!(read_probe_file(&workdir).is_none());
        assert!(!driver_log_path(&case_dir).exists());
        assert!(trace_files(&workdir).is_empty());
        assert!(trace_resource_limit(&workdir).is_none());
        assert!(work_root.join(crate::work_root::WORK_ROOT_MARKER).is_file());
        fs::remove_dir_all(&work_root).unwrap();
    }

    fn preflight_outcome(detail: &str, reach: RunReach) -> CaseOutcome {
        CaseOutcome {
            case: &cases::PREFLIGHT,
            expectation: Expectation::Must(Verdict::Succeeded),
            verdict: Verdict::Inconclusive,
            detail: detail.to_string(),
            passed: false,
            case_dir: PathBuf::from("/tmp/escape-conformance-test/preflight"),
            reach,
        }
    }

    fn ran(timed_out: bool, refusal_code: Option<&str>, tool_result: Option<&str>) -> RunReach {
        RunReach::Ran {
            timed_out,
            refusal_code: refusal_code.map(str::to_string),
            tool_result: tool_result.map(str::to_string),
        }
    }

    #[test]
    fn each_preflight_check_gets_its_own_hint() {
        let refused_exec = "case=preflight tool=python3 :: isError=true :: $ python3 ec-probe.py \
                            Permission denied (os error 13)";
        let traceback = "case=preflight tool=python3 :: isError=false :: $ python3 ec-probe.py \
                         Exit code: 1 Stdout:  Stderr: Traceback (most recent call last):";
        let table = [
            (
                preflight_outcome(
                    "could not stage the case: none of [\"/bin/x\"] exists on this host",
                    RunReach::NotStaged,
                ),
                PreflightCheck::Stage,
            ),
            (
                preflight_outcome(
                    "could not launch `/nonexistent/mur run`: No such file or directory",
                    RunReach::NotLaunched,
                ),
                PreflightCheck::Launch { code: None },
            ),
            (
                preflight_outcome(
                    "the probe wrote no verdict file [mur reported: error[E-MAN-003]: …]",
                    ran(false, Some("E-MAN-003"), None),
                ),
                PreflightCheck::Launch {
                    code: Some("E-MAN-003".to_string()),
                },
            ),
            (
                preflight_outcome(
                    "the probe wrote no verdict file [mur reported: error[E-RUN-012]: …]",
                    ran(false, Some("E-RUN-012"), None),
                ),
                PreflightCheck::Launch {
                    code: Some("E-RUN-012".to_string()),
                },
            ),
            (
                preflight_outcome(
                    "the probe wrote no verdict file [mur reported: error[E-RUN-033]: …]",
                    ran(false, Some("E-RUN-033"), None),
                ),
                PreflightCheck::Harness {
                    code: Some("E-RUN-033".to_string()),
                    timed_out: false,
                },
            ),
            (
                preflight_outcome(
                    "[case exceeded the 300s ceiling and was killed] the probe wrote no verdict \
                     file",
                    ran(true, None, None),
                ),
                PreflightCheck::Harness {
                    code: None,
                    timed_out: true,
                },
            ),
            (
                preflight_outcome(
                    "the probe wrote no verdict file; the tool call reported: the probe driver \
                     left no record of its tool call",
                    ran(false, None, None),
                ),
                PreflightCheck::Harness {
                    code: None,
                    timed_out: false,
                },
            ),
            (
                preflight_outcome(
                    &format!("the probe wrote no verdict file [mur reported: probe-driver: {refused_exec}]"),
                    ran(false, None, Some(refused_exec)),
                ),
                PreflightCheck::InterpreterExec,
            ),
            (
                preflight_outcome(
                    &format!("the probe wrote no verdict file; the tool call reported: {traceback}"),
                    ran(false, None, Some(traceback)),
                ),
                PreflightCheck::ProbeFile,
            ),
        ];

        for (outcome, expected) in &table {
            let check = classify_preflight(outcome);
            assert_eq!(&check, expected, "{}", outcome.detail);

            let hint = preflight_hint(&check);
            assert!(
                hint.starts_with(&format!("Failed check: {}.", check.name())),
                "{hint}"
            );
            assert_eq!(
                hint.contains("Execute"),
                check == PreflightCheck::InterpreterExec,
                "only an interpreter exec failure points at Landlock `Execute`: {hint}"
            );
            assert!(!hint.contains("PR_SET_DUMPABLE"), "{hint}");

            let refusal = preflight_refusal(outcome);
            assert!(refusal.starts_with(
                "REFUSED — no probe can run on this host, so nothing can be measured."
            ));
            assert!(refusal.contains(&hint));
            assert!(refusal.contains(&outcome.detail));
            assert!(refusal.contains("/tmp/escape-conformance-test/preflight"));
        }

        let hint_for = |code: &str| {
            preflight_hint(&PreflightCheck::Launch {
                code: Some(code.to_string()),
            })
        };
        assert!(hint_for("E-MAN-003").contains("cargo test -p escape-conformance"));
        let cgroup = hint_for("E-RUN-012");
        assert!(cgroup.contains("Delegate=yes"), "{cgroup}");
        assert!(cgroup.contains("--systemd-scope"), "{cgroup}");
        assert!(hint_for("E-CAP-001").contains("E-CAP-001"));
    }

    #[test]
    fn a_refusal_prefix_reads_back_as_the_whole_code() {
        let output =
            "starting\nerror[E-MAN-003]: capabilities.shell.allow names a missing binary\n";
        assert_eq!(whole_code(output, "E-MAN-"), "E-MAN-003");
        assert_eq!(
            whole_code("error[E-RUN-012]: no scope", "E-RUN-012"),
            "E-RUN-012"
        );
    }

    #[test]
    fn boundary_manifest_has_no_resource_block_and_declares_no_network() {
        let case = cases::find("read-etc-shadow").unwrap();
        let yaml = manifest_yaml(case, &config(), ContainmentClass::Scoped, &log());
        assert!(yaml.contains("allow:\n      - bash\n      - python3"));
        assert!(!yaml.contains("resources:"));
        // Nothing may be declared network-reachable, or the four network cases assert nothing.
        assert!(!yaml.contains("network:"));
        assert!(yaml.contains("transport: process"));
    }

    #[test]
    fn manifest_declares_the_containment_class_under_test() {
        let case = cases::find("read-etc-shadow").unwrap();
        for class in ContainmentClass::ALL {
            let yaml = manifest_yaml(case, &config(), class, &log());
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
        let yaml = manifest_yaml(case, &config(), ContainmentClass::Scoped, &log());
        assert!(yaml.contains("cgroup_pids_max: 32"));
        assert!(yaml.contains("workdir_max_bytes: 52428800"));
        // 128, not the manual-verification document's 16: the child's pre_exec window has to
        // install a seccomp filter, write uid_map/gid_map for the egress netns, and hold one
        // Landlock grant fd per granted path, all under this ceiling. Below roughly 64 the
        // seccomp install fails; at 64 a `sealed` capsule fails on uid_map once the
        // `SEALED_ETC_PATHS` grants are counted. Pinned here so the value
        // cannot drift back down without the reason being re-read.
        assert!(yaml.contains("max_open_files: 128"));
        assert!(!yaml.contains("max_processes"));
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
            shell_exit_code("case=x tool=python3 isError=false :: $ python3 ec-probe.py Exit code: 137 Stdout:  Stderr:"),
            Some(137)
        );
        assert_eq!(
            shell_exit_code("$ python3 ec-probe.py Exit code: 0 Stdout: hi"),
            Some(0)
        );
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

    #[test]
    fn every_generated_manifest_parses_as_mur_run_parses_it() {
        let mut config = config();
        // A work root with a space and a colon in it, and an interpreter dir with a `#`: every
        // path is a quoted scalar, or one of these would end the value early.
        config.work_root = PathBuf::from("/tmp/escape conformance: work");
        config.probe_driver = PathBuf::from("/opt/a build: tree/probe-driver");
        config.interpreter_dirs = vec![("/usr/lib/python3 #1".to_string(), true)];

        let mut profiles_seen = (false, false);
        let cases = std::iter::once(&cases::PREFLIGHT).chain(cases::all_cases());
        for case in cases {
            match case.profile {
                Profile::Boundary => profiles_seen.0 = true,
                Profile::TightResources => profiles_seen.1 = true,
            }
            let driver_log = driver_log_path(&config.work_root.join(case.id));
            for class in ContainmentClass::ALL {
                let yaml = manifest_yaml(case, &config, class, &driver_log);
                let id = case.id;
                let manifest = match RuntimeManifest::from_yaml_str(&yaml) {
                    Ok(manifest) => manifest,
                    Err(err) => panic!(
                        "case {id} at class {class}: `mur run` would refuse the generated \
                         manifest: {err}\n---\n{yaml}"
                    ),
                };
                assert!(
                    manifest.unknown_keys.is_empty(),
                    "case {id} at class {class}: unknown keys {:?}\n---\n{yaml}",
                    manifest.unknown_keys
                );
                assert_eq!(
                    manifest.effective_lifecycle().shell_grace_secs,
                    config.timeout.as_secs(),
                    "case {id} at class {class}: a probe must stay in the foreground for the \
                     whole case"
                );
                assert_eq!(
                    manifest.capabilities.as_ref().and_then(|c| c.containment),
                    Some(class),
                    "case {id} at class {class}"
                );

                let inference = manifest.inference.as_ref().expect("an inference block");
                assert_eq!(inference.transport, "process", "case {id} at class {class}");
                assert_eq!(
                    inference.command.as_deref(),
                    Some("/opt/a build: tree/probe-driver"),
                    "case {id} at class {class}"
                );
                let driver = inference.driver.as_ref().expect("an inference.driver");
                assert_eq!(driver.artifact, driver_artifact::DRIVER_NAME);
                assert!(
                    manifest
                        .artifacts
                        .iter()
                        .any(|a| a.name == driver_artifact::DRIVER_NAME
                            && a.version == driver_artifact::DRIVER_VERSION
                            && matches!(a.runtime, ArtifactRuntime::Driver)),
                    "case {id} at class {class}: the driver is not declared under artifacts:"
                );

                let probe = ProbeConfig::from_json(
                    driver
                        .config
                        .as_deref()
                        .expect("an inference.driver.config"),
                )
                .unwrap_or_else(|err| panic!("case {id} at class {class}: {err}"));
                assert_eq!(probe.case, id);
                assert_eq!(probe.tool, "python3");
                assert_eq!(probe.script, probe::PROBE_SCRIPT);
                assert_eq!(probe.log, driver_log.display().to_string());
                assert_eq!(
                    probe.script2.is_some(),
                    matches!(case.evidence, Evidence::SecondSpawnRefused(_)),
                    "case {id} at class {class}: script2 is {:?}",
                    probe.script2
                );
                if let Some(script2) = &probe.script2 {
                    assert_eq!(script2, probe::SECOND_SCRIPT);
                }
            }
        }
        assert_eq!(
            profiles_seen,
            (true, true),
            "both profiles must be generated, or one manifest shape goes unchecked"
        );
    }
}
