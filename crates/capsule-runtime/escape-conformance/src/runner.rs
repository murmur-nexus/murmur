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
//! # Routes
//!
//! A case runs once per route it lists ([`Case::routes`]). On [`Route::ShellTool`] the probe is a
//! tool call, as above. On [`Route::ProcessHarness`] the same manifest shape is launched, but the
//! driver config tells `probe-driver` to run the staged probe as its own child instead of calling
//! the bridge, so the verdict says what a process-transport harness can reach. On
//! [`Route::WasmGuest`] the manifest also lists the probe guest, and `probe-driver` calls it with
//! the case id: the guest worker's host side runs the case in the worker process and writes the
//! same probe file. Each route stages its own case directory, so none grades another's probe file.
//!
//! # Scratch is removed after each case
//!
//! Once a case is graded, everything its probe left in the capsule workdir is deleted, along with
//! the contents of each session's `tmp/` and `home/` folders. The disk-filler cases write about a
//! gigabyte each, and the evidence a record points at is elsewhere: the staged probe and its
//! verdict file, the manifest, `mur`'s output, the driver's summary and each session's trace are
//! kept.
//!
//! # Why the probe writes a file instead of returning a value
//!
//! Every resource-exhaustion case ends with its process killed. A verdict carried back through
//! the tool result would be lost; a file written in the capsule workdir survives, and its absence
//! is recorded as `INCONCLUSIVE` rather than read as clean.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use crate::cases::{Case, Evidence, HostChannel, Prepare, Profile, Route};
use crate::driver_artifact;
use crate::harness_protocol::{ProbeConfig, ProbeRoute};
use crate::probe;
use crate::probe_guest;
use crate::verdict::{Expectation, Grade, Verdict};

/// Everything a run needs that is not the case itself.
pub struct RunnerConfig {
    /// The built `mur` binary. Cases drive real enforcement through it.
    pub mur: PathBuf,
    /// This package's `probe-driver`, named as `inference.command` in every generated manifest.
    pub probe_driver: PathBuf,
    /// The `escape-conformance-driver` artifact `driver_artifact::build_artifact` packed for this
    /// run, installed into every case's project store before its `mur run`.
    pub driver_artifact: PathBuf,
    /// The probe guest `probe_guest::build_artifact` packed for this run, installed into the
    /// project store of every case run on [`Route::WasmGuest`]. `None` when the run uses no such
    /// route; a case staged on it without one is not staged.
    pub probe_guest_artifact: Option<PathBuf>,
    /// Root under which each case gets its own directory, as
    /// `crate::work_root::prepare_work_root` returned it: empty but for its marker when the run
    /// starts. Kept after the run: it holds the generated manifest, the probe source, `mur`'s
    /// stdout/stderr and the session trace, and the record points at it per case.
    pub work_root: PathBuf,
    /// Wall-clock ceiling for one case's `mur run`.
    pub timeout: Duration,
    /// The interpreter `probe-driver` runs the probe with on [`Route::ProcessHarness`], absolute:
    /// the harness starts with a cleared environment, so it has no `PATH` to look `python3` up on.
    pub harness_interpreter: PathBuf,
    /// Wrap each `mur run` in `systemd-run --user --scope --property=Delegate=yes`. Without a
    /// delegated cgroup v2 subtree, `mur run` refuses any subprocess-capable capsule with
    /// `E-RUN-012` and no case can report anything — see the install requirement in
    /// `resource-limits-manual-verification.md`.
    pub systemd_scope: bool,
}

/// One case's result on one route.
pub struct CaseOutcome {
    pub case: &'static Case,
    pub route: Route,
    pub expectation: Expectation,
    pub verdict: Verdict,
    /// One line. Carries the attribution evidence — which errno, which control, which mechanism.
    pub detail: String,
    /// [`Self::verdict`] graded against [`Self::expectation`].
    pub grade: Grade,
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

/// The tight `capabilities.resources` block from `resource-limits-manual-verification.md`'s test
/// capsule, with the divergences noted inline. `max_processes` is left out: every case here runs
/// in a cgroup scope, where the runtime sets no `RLIMIT_NPROC` and `cgroup_pids_max` is the only
/// process bound.
fn tight_resources_yaml() -> &'static str {
    "  resources:\n\
    \x20   # 128 rather than the document's 64. The child's pre_exec window sets\n\
    \x20   # RLIMIT_NPROC (none in a cgroup scope), joins the scope, installs the enforcement\n\
    \x20   # (composed root, Landlock, netns, seccomp), and sets the per-process rlimits\n\
    \x20   # RLIMIT_NOFILE among them last, right before execve, so the setup is not bound by\n\
    \x20   # this ceiling. It bounds the exec'd program alone, whose interpreter and runtime\n\
    \x20   # libraries hold descriptors of their own before a probe opens its first. Keep it\n\
    \x20   # far below the host default: the fd-exhauster case grades a ceiling under 512.\n\
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

/// The directory one case is staged in on `route`. The shell tool keeps the bare case id; every
/// other route appends its name, so no two routes share a probe file.
pub fn case_dir(config: &RunnerConfig, case: &Case, route: Route) -> PathBuf {
    match route {
        Route::ShellTool => config.work_root.join(case.id),
        _ => config
            .work_root
            .join(format!("{}--{}", case.id, route.as_str())),
    }
}

/// The capsule workdir inside a case directory.
fn workdir_in(case_dir: &Path) -> PathBuf {
    case_dir.join("wd")
}

/// What `probe-driver` is told about one case on `route`, through `inference.driver.config`.
pub fn probe_config(
    case: &Case,
    route: Route,
    config: &RunnerConfig,
    case_dir: &Path,
) -> ProbeConfig {
    ProbeConfig {
        tool: match route {
            Route::WasmGuest => probe_guest::PROBE_GUEST_NAME.to_string(),
            _ => "python3".to_string(),
        },
        script: probe::PROBE_SCRIPT.to_string(),
        script2: match case.evidence {
            Evidence::SecondSpawnRefused(_) => Some(probe::SECOND_SCRIPT.to_string()),
            _ => None,
        },
        case: case.id.to_string(),
        log: driver_log_path(case_dir).display().to_string(),
        route: match route {
            Route::ShellTool => ProbeRoute::ShellTool,
            Route::ProcessHarness => ProbeRoute::ProcessHarness {
                interpreter: config.harness_interpreter.display().to_string(),
                workdir: workdir_in(case_dir).display().to_string(),
            },
            Route::WasmGuest => ProbeRoute::WasmGuest,
        },
    }
}

/// The manifest one case runs under on `route`, staged in `case_dir`. Every route declares the
/// same capabilities: only the driver config differs, and on [`Route::WasmGuest`] the probe guest
/// is one more artifact, so the class under test is the same.
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
    route: Route,
    config: &RunnerConfig,
    class: murmur_artifact::ContainmentClass,
    case_dir: &Path,
) -> String {
    let mut yaml = String::new();
    yaml.push_str(&format!("name: escape-conformance-{}\n", case.id));
    yaml.push_str("version: 0.0.1\n\n");
    // A process driver has to come out of a store: `stage` installs this one into the case's
    // project store before `mur run` resolves it.
    yaml.push_str("artifacts:\n");
    yaml.push_str(&format!("  - name: {}\n", driver_artifact::DRIVER_ARTIFACT));
    yaml.push_str(&format!(
        "    version: {}\n",
        driver_artifact::DRIVER_VERSION
    ));
    yaml.push_str("    runtime: driver\n");
    if route == Route::WasmGuest {
        yaml.push_str(&format!(
            "  - name: {}\n",
            probe_guest::PROBE_GUEST_ARTIFACT
        ));
        yaml.push_str(&format!(
            "    version: {}\n",
            probe_guest::PROBE_GUEST_VERSION
        ));
        yaml.push_str("    runtime: tool\n");
    }
    yaml.push('\n');
    yaml.push_str("capabilities:\n");
    yaml.push_str(&format!("  containment: {class}\n"));
    yaml.push_str("  shell:\n");
    // `bash` is allowlisted so `exec-renamed-disallowed-binary` has an allowlisted basename to
    // wear; `python3` is what every probe actually runs as.
    yaml.push_str("    allow:\n      - bash\n      - python3\n");
    let channel_names = host_channel_env_names(case);
    if !channel_names.is_empty() {
        // The shell-tool route's way in: a command reads a name from `mur`'s environment only
        // when the manifest declares it here.
        yaml.push_str("    baseline_env:\n");
        for name in channel_names {
            yaml.push_str(&format!("      - {name}\n"));
        }
    }
    if case.profile == Profile::TightResources {
        yaml.push_str(tight_resources_yaml());
    }
    if !channel_names.is_empty() {
        // The process-harness route's way in: the harness starts with a cleared environment and
        // receives a name from `mur`'s only when the manifest allows it here.
        yaml.push_str("  env:\n    allow:\n");
        for name in channel_names {
            yaml.push_str(&format!("      - {name}\n"));
        }
    }
    // `network.allow` is left entirely undeclared, so every destination is unlisted and
    // `network.unix_sockets` keeps its default of false — which is what the four network cases
    // and the two AF_UNIX cases assert against. `env.allow` and `shell.baseline_env` are declared
    // only for a `Prepare::HostChannel` case, naming exactly its variables: every other harness
    // environment holds only what the driver's launch plan sets, and so does every probe
    // subprocess's. `lifecycle.shell_grace_secs` is left undeclared too, so no probe is ever
    // demoted to the background: a demoted command's tool result carries a `wrk_` handle instead
    // of an exit code, which a `ShellExit` case cannot grade.
    let probe = probe_config(case, route, config, case_dir);
    yaml.push_str("\ninference:\n");
    yaml.push_str("  transport: process\n");
    yaml.push_str("  driver:\n");
    yaml.push_str(&format!(
        "    artifact: {}\n",
        driver_artifact::DRIVER_ARTIFACT
    ));
    yaml.push_str("    config:\n");
    yaml.push_str(&format!("      tool: {}\n", yaml_str(&probe.tool)));
    yaml.push_str(&format!("      script: {}\n", yaml_str(&probe.script)));
    if let Some(script2) = &probe.script2 {
        yaml.push_str(&format!("      script2: {}\n", yaml_str(script2)));
    }
    yaml.push_str(&format!("      case: {}\n", yaml_str(&probe.case)));
    yaml.push_str(&format!("      log: {}\n", yaml_str(&probe.log)));
    yaml.push_str(&format!(
        "      route: {}\n",
        yaml_str(probe.route.route().as_str())
    ));
    if let ProbeRoute::ProcessHarness {
        interpreter,
        workdir,
    } = &probe.route
    {
        yaml.push_str(&format!("      interpreter: {}\n", yaml_str(interpreter)));
        yaml.push_str(&format!("      workdir: {}\n", yaml_str(workdir)));
    }
    // The binary override: the driver's `describe().binary` names `probe-driver` bare, and this
    // package's build is not on any PATH `mur` would search.
    yaml.push_str(&format!(
        "  command: {}\n",
        yaml_str(&config.probe_driver.display().to_string())
    ));
    yaml.push_str("  max_turns: 2\n");
    yaml
}

/// The variables a [`Prepare::HostChannel`] case is given, empty for every other case.
fn host_channel_env_names(case: &Case) -> &'static [&'static str] {
    match case.prepare {
        Prepare::HostChannel(channel) => channel.env_names(),
        _ => &[],
    }
}

/// The empty marker [`HostChannel::RuntimeKill`] stages on [`Route::ProcessHarness`]: the probe
/// holds its run open once it has reported only when it finds this file in its working
/// directory, so the runner can kill `mur` from the host mid-session.
const HOLD_FOR_HOST_KILL: &str = "ec-hold-for-host-kill";

/// Whether `case` on `route` is the one the runner kills `mur` under once the probe reported.
fn kills_runtime_after_report(case: &Case, route: Route) -> bool {
    case.prepare == Prepare::HostChannel(HostChannel::RuntimeKill) && route == Route::ProcessHarness
}

/// The file `probe-driver` writes its summary line to, in the capsule workdir: `probe-driver` is
/// the capsule's harness and runs under the class under test, which lets it write the workdir and
/// nothing else of the case directory.
const DRIVER_LOG: &str = "probe-driver.txt";

/// Where `probe-driver` writes its summary line for the case staged in `case_dir`.
fn driver_log_path(case_dir: &Path) -> PathBuf {
    workdir_in(case_dir).join(DRIVER_LOG)
}

/// Stages one case's directory for `route` and returns `(capsule_workdir, manifest_path)`.
fn stage(
    case: &Case,
    route: Route,
    config: &RunnerConfig,
    class: murmur_artifact::ContainmentClass,
    case_dir: &Path,
) -> io::Result<(PathBuf, PathBuf)> {
    let workdir = workdir_in(case_dir);
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
        manifest_yaml(case, route, config, class, case_dir),
    )?;
    // `mur install` upserts the artifact into the project the manifest's directory names, and
    // `mur run --manifest` resolves its artifacts from that same project's store.
    driver_artifact::install_into(&config.mur, &config.driver_artifact, case_dir)
        .map_err(io::Error::other)?;
    if route == Route::WasmGuest {
        let guest = config.probe_guest_artifact.as_deref().ok_or_else(|| {
            io::Error::other("this run packed no probe guest, so the wasm-guest route cannot run")
        })?;
        driver_artifact::install_into(&config.mur, guest, case_dir).map_err(io::Error::other)?;
    }

    if kills_runtime_after_report(case, route) {
        fs::write(workdir.join(HOLD_FOR_HOST_KILL), "")?;
    }
    match case.prepare {
        Prepare::None | Prepare::LeakFdIntoMur { .. } | Prepare::HostChannel(_) => {}
        Prepare::WriteSealKeyPath { dest } => {
            let home =
                capsule_runtime::murmur_home::ensure_murmur_home().map_err(io::Error::other)?;
            capsule_runtime::record_seal::HostKey::load_or_create(&home)
                .map_err(io::Error::other)?;
            let key = capsule_runtime::record_seal::private_key_path(&home);
            fs::write(workdir.join(dest), format!("{}\n", key.display()))?;
        }
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
    Ok((workdir, manifest))
}

/// The workdir entries the harness staged for `case` or grades it on, which the scratch cleanup
/// keeps: the probe, its verdict file, the driver's summary, the follow-up script and whatever
/// [`Prepare`] wrote.
fn staged_names(case: &Case) -> Vec<&'static str> {
    let mut names = vec![probe::PROBE_SCRIPT, probe::PROBE_FILE, DRIVER_LOG];
    if let Evidence::SecondSpawnRefused(_) = case.evidence {
        names.push(probe::SECOND_SCRIPT);
    }
    match case.prepare {
        Prepare::None | Prepare::LeakFdIntoMur { .. } => {}
        Prepare::HostChannel(HostChannel::RuntimeKill) => names.push(HOLD_FOR_HOST_KILL),
        Prepare::HostChannel(_) => {}
        Prepare::CopyBinaryAs { dest, .. } | Prepare::WriteSealKeyPath { dest } => names.push(dest),
    }
    names
}

/// What [`remove_scratch`] removed, and what it could not.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ScratchRemoved {
    /// Top-level entries removed.
    pub entries: usize,
    /// Bytes of regular files removed, counted before removal.
    pub bytes: u64,
    /// One line per entry that could not be removed.
    pub errors: Vec<String>,
}

impl ScratchRemoved {
    /// The note folded into a case's DETAIL, `None` when there was nothing to say.
    fn note(&self) -> Option<String> {
        if self.entries == 0 && self.errors.is_empty() {
            return None;
        }
        let mut note = format!(
            "[scratch removed after grading: {} entr{}, {} bytes",
            self.entries,
            if self.entries == 1 { "y" } else { "ies" },
            self.bytes
        );
        if !self.errors.is_empty() {
            note.push_str(&format!("; NOT removed: {}", self.errors.join("; ")));
        }
        note.push(']');
        Some(note)
    }
}

/// Bytes of the regular files under `path`, without following symlinks.
fn tree_bytes(path: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return 0;
    };
    if meta.is_dir() {
        fs::read_dir(path)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| tree_bytes(&entry.path()))
                    .sum()
            })
            .unwrap_or(0)
    } else if meta.is_file() {
        meta.len()
    } else {
        0
    }
}

/// Removes every entry of `dir` whose name is not in `keep`, into `removed`. A symlink is removed
/// as a link; its target is never entered.
fn remove_entries(dir: &Path, keep: &[&str], removed: &mut ScratchRemoved) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if keep.iter().any(|name| entry.file_name() == *name) {
            continue;
        }
        let path = entry.path();
        let bytes = tree_bytes(&path);
        let result = match entry.file_type() {
            Ok(kind) if kind.is_dir() => fs::remove_dir_all(&path),
            Ok(_) => fs::remove_file(&path),
            Err(err) => Err(err),
        };
        match result {
            Ok(()) => {
                removed.entries += 1;
                removed.bytes += bytes;
            }
            Err(err) => removed.errors.push(format!("{}: {err}", path.display())),
        }
    }
}

/// Deletes what `case`'s probe left behind in `workdir`, and the contents of each of its sessions'
/// `tmp/` and `home/` folders, keeping what the harness staged and every session's record.
pub fn remove_scratch(case: &Case, workdir: &Path) -> ScratchRemoved {
    let mut removed = ScratchRemoved::default();
    remove_entries(workdir, &staged_names(case), &mut removed);
    let sessions = capsule_runtime::records_group(workdir)
        .ok()
        .and_then(|group| fs::read_dir(group).ok());
    for session in sessions.into_iter().flatten().flatten() {
        for folder in ["tmp", "home"] {
            remove_entries(&session.path().join(folder), &[], &mut removed);
        }
    }
    removed
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

    if let Prepare::HostChannel(_) = case.prepare {
        // Every wrapper from here on `exec`s, so the shell's own pid is `mur`'s: exported as
        // `MUR_EC_RUNTIME_PID` before the `exec`, it is the host pid of the runtime under test.
        argv.push("bash".to_string());
        argv.push("-c".to_string());
        argv.push("export MUR_EC_RUNTIME_PID=$$; exec \"$@\"".to_string());
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

/// What one launch adds to a plain `mur run`. The default adds nothing.
#[derive(Debug, Default, Clone, Copy)]
struct LaunchExtras<'a> {
    /// Set in the launched process's environment, on top of the harness's own.
    env: &'a [(String, String)],
    /// Kill the launched process (`mur`, every wrapper having `exec`ed) as soon as this file
    /// holds a `VERDICT=` line: the probe has reported, and the run is ended from the host.
    kill_after_report: Option<&'a Path>,
}

/// How one launch ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LaunchEnd {
    /// The exit code, `None` when killed.
    exit_code: Option<i32>,
    /// Killed at the timeout.
    timed_out: bool,
    /// Killed by [`LaunchExtras::kill_after_report`].
    killed_after_report: bool,
}

/// Whether `path` holds a `VERDICT=` line.
fn has_reported(path: &Path) -> bool {
    fs::read_to_string(path).is_ok_and(|text| text.lines().any(|line| line.starts_with("VERDICT=")))
}

/// Spawns `argv`, waits up to `timeout`, and kills it if it overruns, or once the probe reported
/// when [`LaunchExtras::kill_after_report`] says so.
///
/// stdout and stderr go to files rather than pipes: a case that writes more than a pipe buffer
/// before exiting would otherwise deadlock the harness, and the files are evidence a reader can
/// open afterwards.
fn run_with_timeout(
    argv: &[String],
    case_dir: &Path,
    workdir: &Path,
    timeout: Duration,
    extras: LaunchExtras<'_>,
) -> io::Result<LaunchEnd> {
    let stdout = fs::File::create(case_dir.join("mur-stdout.txt"))?;
    let stderr = fs::File::create(case_dir.join("mur-stderr.txt"))?;

    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .current_dir(workdir)
        .envs(extras.env.iter().map(|(name, value)| (name, value)))
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));

    let mut child = command.spawn()?;
    let started = Instant::now();
    let killed = |child: &mut Child, timed_out: bool| {
        let _ = child.kill();
        let _ = child.wait();
        LaunchEnd {
            exit_code: None,
            timed_out,
            killed_after_report: !timed_out,
        }
    };
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(LaunchEnd {
                exit_code: status.code(),
                timed_out: false,
                killed_after_report: false,
            });
        }
        if extras.kill_after_report.is_some_and(has_reported) {
            return Ok(killed(&mut child, false));
        }
        if started.elapsed() >= timeout {
            return Ok(killed(&mut child, true));
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

/// Every `trace.jsonl` of a session the capsule ran with `--workdir <workdir>`: each in its
/// session's records directory, in `workdir`'s records group under `~/.murmur/records`.
fn trace_files(workdir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(group) = capsule_runtime::records_group(workdir) else {
        return found;
    };
    let Ok(sessions) = fs::read_dir(group) else {
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
    let sessions = capsule_runtime::records_group(workdir)
        .ok()
        .and_then(|group| fs::read_dir(group).ok());
    if let Some(sessions) = sessions {
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
///
/// Runs once per route a registry case uses, so a host where the process harness cannot start a
/// probe is refused too, rather than reporting every harness-route known gap as changed.
pub fn preflight(
    config: &RunnerConfig,
    class: murmur_artifact::ContainmentClass,
    route: Route,
) -> Result<String, String> {
    let outcome = run_case(&crate::cases::PREFLIGHT, route, config, class);
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
         The preflight capsule failed the {} check on the {} route:\n\
         \x20 {}\n\n\
         Every case would report INCONCLUSIVE and every asserted case would fail, which would \
         read as a boundary escape when in fact nothing was exercised. No record file was \
         written.\n\n\
         {}\n\n\
         Artifacts for this preflight run: {}",
        check.name(),
        outcome.route.as_str(),
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
    /// On [`Route::WasmGuest`], the probe guest ran and its request came back unanswered: the
    /// `mur` under test was built without the `escape-probe` feature.
    ProbeGuestUnanswered,
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
            PreflightCheck::ProbeGuestUnanswered => "probe guest",
        }
    }
}

/// `mur` diagnostics that mean the harness failed rather than that `mur run` refused the capsule:
/// `E-RUN-006` the harness binary was not found, `E-RUN-033` the harness reported a failed turn,
/// `E-RUN-034` a driver call failed, `E-RUN-035` the harness went quiet past the inactivity limit.
const HARNESS_CODES: [&str; 4] = ["E-RUN-006", "E-RUN-033", "E-RUN-034", "E-RUN-035"];

/// How the kernel's `EACCES` reads in a tool result: `io::Error`'s rendering of errno 13.
const EXEC_DENIED: &str = "Permission denied (os error 13)";

/// The probe guest's own summary, which a guest worker built with `escape-probe` replaces.
const PROBE_GUEST_UNANSWERED: &str = "escape-probe request not answered";

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
        .is_some_and(|result| result.contains(PROBE_GUEST_UNANSWERED))
    {
        return PreflightCheck::ProbeGuestUnanswered;
    }
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
                 `mur-stderr.txt` and `wd/probe-driver.txt` in the case directory, and on a \
                 loaded host raise `--timeout-secs`."
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
        PreflightCheck::ProbeGuestUnanswered => format!(
            "Failed check: {name}. The probe guest ran in the tools worker, but its request came \
             back unanswered: the `mur` under test was built without the test-only \
             `escape-probe` feature, which only a debug build may carry. Build one with \
             `cargo build -p murmur-cli --bin mur --features escape-probe` and pass it as \
             `--mur`, or leave the route out with `--route shell-tool --route process-harness` \
             (the record is then stamped PARTIAL RUN)."
        ),
    }
}

/// What `mur` prints when it refused a case's launch, matched as substrings of its output.
/// `E-MAN-` matches every manifest code. Searched before [`HARNESS_CODES`], which together with
/// these are the refusal codes a run is read for.
const LAUNCH_REFUSAL_CODES: [&str; 4] = ["E-RUN-012", "E-RUN-007", "E-RUN-008", "E-MAN-"];

/// Runs one case end to end on `route`, grades it, and removes its scratch.
///
/// `route` is one of [`Case::routes`]; any other is reported `INCONCLUSIVE` without staging.
pub fn run_case(
    case: &'static Case,
    route: Route,
    config: &RunnerConfig,
    class: murmur_artifact::ContainmentClass,
) -> CaseOutcome {
    let case_dir = case_dir(config, case, route);
    let not_run = |expectation: Expectation, detail: String, reach: RunReach| CaseOutcome {
        case,
        route,
        expectation,
        verdict: Verdict::Inconclusive,
        grade: Grade::of(expectation, Verdict::Inconclusive),
        detail,
        case_dir: case_dir.clone(),
        reach,
    };
    let Some(expectation) = case.expectation_on(route, class) else {
        return not_run(
            Expectation::NotAsserted,
            format!("{} does not run on the {} route", case.id, route.as_str()),
            RunReach::NotStaged,
        );
    };

    let (workdir, manifest) = match stage(case, route, config, class, &case_dir) {
        Ok(paths) => paths,
        Err(err) => {
            return not_run(
                expectation,
                format!("could not stage the case: {err}"),
                RunReach::NotStaged,
            );
        }
    };

    // Held to the end of this function: dropping it kills and reaps whatever it started, on
    // every return path.
    let targets = match HostTargets::start(case, config, class, &case_dir) {
        Ok(targets) => targets,
        Err(err) => {
            return not_run(
                expectation,
                format!("could not set up the case's host targets: {err}"),
                RunReach::NotStaged,
            );
        }
    };

    let argv = build_argv(case, config, &manifest, &workdir);
    let driver_log = driver_log_path(&case_dir);
    let probe_file = workdir.join(probe::PROBE_FILE);
    let extras = LaunchExtras {
        env: &targets.env,
        kill_after_report: kills_runtime_after_report(case, route).then_some(probe_file.as_path()),
    };

    let end = match run_with_timeout(&argv, &case_dir, &workdir, config.timeout, extras) {
        Ok(end) => end,
        Err(err) => {
            return not_run(
                expectation,
                format!("could not launch `{}`: {err}", argv.join(" ")),
                RunReach::NotLaunched,
            );
        }
    };
    let LaunchEnd {
        exit_code,
        timed_out,
        killed_after_report,
    } = end;
    // After a host kill, the session's processes have to go with the runtime. Waited for here,
    // before anything reads the record, and any that outlive the wait are named.
    let lingering = killed_after_report.then(|| stop_processes_under(&case_dir, LINGER_WAIT));

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

    let (mut verdict, mut detail) = match case.evidence {
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
            let mut calls = driver_summary.split("|| SECOND-CALL");
            let first = calls.next().unwrap_or("");
            let second = calls.next().map(str::trim).unwrap_or("");
            if first.contains("EC_VERDICT=INCONCLUSIVE") {
                // A probe that gave up caused no breach, so the follow-up call has nothing to be
                // refused for and its outcome measures nothing.
                (
                    Verdict::Inconclusive,
                    format!(
                        "the probe gave up before causing a breach, so the follow-up call \
                         measured nothing. Tool call: {driver_summary}"
                    ),
                )
            } else if second.is_empty() && output.contains("E-RUN-013") && output.contains(needle) {
                // The breach ended the session itself, which is the promise the ceiling makes and
                // more than the next spawn being refused: the runtime kills what the session was
                // running, the harness's follow-up call included, so there is no second call to
                // refuse. The session's own error names the ceiling.
                (
                    Verdict::Contained,
                    format!(
                        "the breach ended the session with E-RUN-013 naming `{needle}`, and the \
                         probe was killed before it could finish. Tool call: {driver_summary}"
                    ),
                )
            } else if second.is_empty() && first.contains("EC_VERDICT=UNCONTAINED") {
                (
                    Verdict::Uncontained,
                    format!(
                        "the probe wrote everything it was going to write and the session was \
                         not ended with E-RUN-013 naming `{needle}`. Tool call: {driver_summary}"
                    ),
                )
            } else if second.is_empty() {
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

    if !targets.survivors.is_empty() {
        let alive: Vec<(u32, bool)> = targets
            .survivors
            .iter()
            .map(|pid| (*pid, pid_alive(*pid)))
            .collect();
        (verdict, detail) = fold_survival(verdict, detail, &alive);
    }
    if kills_runtime_after_report(case, route) {
        let checks = if killed_after_report {
            session_record_checks(&workdir)
        } else {
            Vec::new()
        };
        (verdict, detail) = fold_runtime_kill_record(verdict, detail, killed_after_report, &checks);
        if let Some(lingering) = &lingering {
            detail = format!("{detail} {}", lingering_note(lingering));
        }
    }

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

    // After everything above has read the workdir and the session folders, never before.
    if let Some(note) = remove_scratch(case, &workdir).note() {
        detail = format!("{detail} {note}");
    }

    CaseOutcome {
        case,
        route,
        expectation,
        verdict,
        grade: Grade::of(expectation, verdict),
        detail: crate::record::sanitize_cell(&detail),
        case_dir,
        reach: RunReach::Ran {
            timed_out,
            refusal_code,
            tool_result,
        },
    }
}

/// How long a host target has to come up: the sibling capsule's `mur` staging its session and
/// its probe writing `sibling-ready`.
const SIBLING_READY_WAIT: Duration = Duration::from_secs(60);

/// How long the session's processes have to exit after `mur` was killed from the host, and a
/// stopped sibling capsule's after its `mur` was.
const LINGER_WAIT: Duration = Duration::from_secs(10);

/// The file the sibling capsule's probe writes once it is up, in its workdir.
const SIBLING_READY: &str = "sibling-ready";

/// The file the runner writes into the sibling capsule's workdir before launching it: how many
/// seconds its probe holds the session open.
const SIBLING_HOLD: &str = "sibling-hold-secs";

/// How long a host target must outlive its own start: the sibling's start-up, then the case's
/// whole run up to `timeout`, with a minute to spare. A target that ends by itself before the
/// survival check reads as one the probe stopped, so the hold must never be shorter than a case.
fn target_hold(timeout: Duration) -> Duration {
    SIBLING_READY_WAIT + timeout + Duration::from_secs(60)
}

/// The host processes a [`Prepare::HostChannel`] case is given, and the variables that name them.
///
/// Dropping it kills and reaps every process it started, so no return path of [`run_case`] leaves
/// one running.
#[derive(Debug, Default)]
struct HostTargets {
    /// Exported to `mur`, on top of `MUR_EC_RUNTIME_PID`, which `build_argv`'s wrapper exports.
    env: Vec<(String, String)>,
    /// Host pids that must still be alive after the case: the probe was told to reach them.
    survivors: Vec<u32>,
    /// The same-user process outside every capsule ([`HostChannel::UnrelatedProcess`]).
    unrelated: Option<Child>,
    /// The sibling capsule's `mur` ([`HostChannel::SiblingCapsule`]) and its case directory.
    sibling: Option<(Child, PathBuf)>,
}

impl HostTargets {
    /// Starts or finds what `case`'s host channel names. `Err` when it cannot, and the case is
    /// then not staged: a target that is not there would make any verdict meaningless.
    fn start(
        case: &Case,
        config: &RunnerConfig,
        class: murmur_artifact::ContainmentClass,
        case_dir: &Path,
    ) -> Result<HostTargets, String> {
        let mut targets = HostTargets::default();
        let Prepare::HostChannel(channel) = case.prepare else {
            return Ok(targets);
        };
        match channel {
            HostChannel::RuntimePid | HostChannel::RuntimeKill => {}
            HostChannel::UnrelatedProcess => {
                use std::os::unix::process::CommandExt;
                let hold = target_hold(config.timeout).as_secs().to_string();
                let child = Command::new("sleep")
                    .arg(&hold)
                    .process_group(0)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .map_err(|err| format!("could not start `sleep {hold}`: {err}"))?;
                let pid = child.id();
                targets.unrelated = Some(child);
                targets
                    .env
                    .push(("MUR_EC_UNRELATED_PID".to_string(), pid.to_string()));
                targets.survivors.push(pid);
            }
            HostChannel::SiblingCapsule => {
                let pids = targets.start_sibling(config, class, &case_dir.join("sibling"))?;
                targets.env.push((
                    "MUR_EC_SIBLING_PIDS".to_string(),
                    pids.iter()
                        .map(u32::to_string)
                        .collect::<Vec<_>>()
                        .join(" "),
                ));
                targets.survivors = pids;
            }
            HostChannel::OtherSession => {
                let trace = Route::ALL
                    .into_iter()
                    .find_map(|route| {
                        let preflight = self::case_dir(config, &crate::cases::PREFLIGHT, route);
                        trace_files(&workdir_in(&preflight)).into_iter().next()
                    })
                    .ok_or(
                        "no preflight session left a trace.jsonl to name as another session's",
                    )?;
                let root = capsule_runtime::records_dir_root()
                    .map_err(|err| format!("no records root: {err}"))?;
                targets.env.push((
                    "MUR_EC_OTHER_TRACE".to_string(),
                    trace.display().to_string(),
                ));
                targets.env.push((
                    "MUR_EC_RECORDS_ROOT".to_string(),
                    root.display().to_string(),
                ));
            }
        }
        Ok(targets)
    }

    /// Stages [`crate::cases::SIBLING_TARGET`] in `sibling_dir` at `class`, launches it, waits
    /// for its probe to report it is up, and returns its `mur`'s host pid followed by every
    /// descendant's.
    fn start_sibling(
        &mut self,
        config: &RunnerConfig,
        class: murmur_artifact::ContainmentClass,
        sibling_dir: &Path,
    ) -> Result<Vec<u32>, String> {
        let sibling = &crate::cases::SIBLING_TARGET;
        let (workdir, manifest) = stage(sibling, Route::ShellTool, config, class, sibling_dir)
            .map_err(|err| format!("could not stage the sibling capsule: {err}"))?;
        fs::write(
            workdir.join(SIBLING_HOLD),
            target_hold(config.timeout).as_secs().to_string(),
        )
        .map_err(|err| format!("could not write the sibling's {SIBLING_HOLD}: {err}"))?;
        let argv = build_argv(sibling, config, &manifest, &workdir);
        let output = |name: &str| {
            fs::File::create(sibling_dir.join(name))
                .map_err(|err| format!("could not create the sibling's {name}: {err}"))
        };
        let child = Command::new(&argv[0])
            .args(&argv[1..])
            .current_dir(&workdir)
            .stdin(Stdio::null())
            .stdout(Stdio::from(output("mur-stdout.txt")?))
            .stderr(Stdio::from(output("mur-stderr.txt")?))
            .spawn()
            .map_err(|err| format!("could not launch the sibling capsule: {err}"))?;
        let runtime = child.id();
        let child = &mut self.sibling.insert((child, sibling_dir.to_path_buf())).0;

        let started = Instant::now();
        let ready = workdir.join(SIBLING_READY);
        loop {
            if let Ok(Some(status)) = child.try_wait() {
                return Err(format!(
                    "the sibling capsule's `mur` exited ({status}) before its probe was up; see \
                     {}",
                    sibling_dir.join("mur-stderr.txt").display()
                ));
            }
            if ready.is_file() {
                let mut pids = vec![runtime];
                pids.extend(descendants(runtime, &host_parent_table()));
                if pids
                    .iter()
                    .any(|pid| cmdline_contains(*pid, probe::PROBE_SCRIPT))
                {
                    return Ok(pids);
                }
                if started.elapsed() >= SIBLING_READY_WAIT {
                    return Err(format!(
                        "the sibling capsule wrote {SIBLING_READY}, but no process descended \
                         from its `mur` (pid {runtime}) runs {}: {pids:?}",
                        probe::PROBE_SCRIPT
                    ));
                }
            } else if started.elapsed() >= SIBLING_READY_WAIT {
                return Err(format!(
                    "the sibling capsule wrote no {SIBLING_READY} within {}s; see {}",
                    SIBLING_READY_WAIT.as_secs(),
                    sibling_dir.display()
                ));
            }
            sleep(Duration::from_millis(200));
        }
    }
}

impl Drop for HostTargets {
    fn drop(&mut self) {
        if let Some(child) = &mut self.unrelated {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some((child, sibling_dir)) = &mut self.sibling {
            // SIGTERM first, so the sibling's `mur` tears its session down and seals its record;
            // SIGKILL only if it has not exited by the deadline.
            let _ = Command::new("kill")
                .args(["-TERM", &child.id().to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let started = Instant::now();
            while !matches!(child.try_wait(), Ok(Some(_))) && started.elapsed() < LINGER_WAIT {
                sleep(Duration::from_millis(200));
            }
            let _ = child.kill();
            let _ = child.wait();
            stop_processes_under(sibling_dir, LINGER_WAIT);
        }
    }
}

/// Every host process as `(pid, parent pid)`, read from `/proc/<pid>/status`. The harness runs
/// outside every capsule, so these are host pids.
fn host_parent_table() -> Vec<(u32, u32)> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter_map(|pid| {
            let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
            let ppid = status
                .lines()
                .find_map(|line| line.strip_prefix("PPid:"))?
                .trim()
                .parse()
                .ok()?;
            Some((pid, ppid))
        })
        .collect()
}

/// Every pid in `table` whose parent chain reaches `root`, `root` itself excluded, ascending.
fn descendants(root: u32, table: &[(u32, u32)]) -> Vec<u32> {
    let mut found = Vec::new();
    let mut frontier = vec![root];
    while let Some(parent) = frontier.pop() {
        for (pid, ppid) in table {
            if *ppid == parent && *pid != root && !found.contains(pid) {
                found.push(*pid);
                frontier.push(*pid);
            }
        }
    }
    found.sort_unstable();
    found
}

/// Whether host pid `pid`'s command line contains `needle`.
fn cmdline_contains(pid: u32, needle: &str) -> bool {
    fs::read(format!("/proc/{pid}/cmdline"))
        .is_ok_and(|raw| String::from_utf8_lossy(&raw).contains(needle))
}

/// Whether host pid `pid` is alive: `/proc/<pid>` exists and its state is not `Z`.
fn pid_alive(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/status")).is_ok_and(|status| {
        status
            .lines()
            .find_map(|line| line.strip_prefix("State:"))
            .is_some_and(|state| !state.trim_start().starts_with('Z'))
    })
}

/// The survival check of a case whose probe was told to reach `alive`'s pids, each paired with
/// whether it was alive after the run. All alive leaves the verdict as the probe reported it; any
/// gone is `ALLOWED`, whatever the probe reported, since something stopped a target it was told
/// not to reach.
fn fold_survival(verdict: Verdict, detail: String, alive: &[(u32, bool)]) -> (Verdict, String) {
    let pids = |want: bool| {
        alive
            .iter()
            .filter(|(_, is_alive)| *is_alive == want)
            .map(|(pid, _)| pid.to_string())
            .collect::<Vec<_>>()
            .join(" ")
    };
    if alive.iter().all(|(_, is_alive)| *is_alive) {
        let note = format!(
            "[every target pid was alive after the case: {}]",
            pids(true)
        );
        (verdict, format!("{detail} {note}"))
    } else {
        let note = format!(
            "[target pid(s) {} were gone after the case: a target the probe was told about was \
             stopped]",
            pids(false)
        );
        (Verdict::Allowed, format!("{detail} {note}"))
    }
}

/// What one session directory's record held once its `mur` was killed from the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecordCheck {
    /// The session's records directory.
    pub dir: PathBuf,
    /// `trace.jsonl` lines whose `event_type` is `session_start`.
    pub session_start: usize,
    /// `trace.jsonl` lines whose `event_type` is `session_end`.
    pub session_end: usize,
    /// `mur trace verify`'s verdict on the directory, `Err` with the reason when there was no
    /// trace to read or no public key to check it with.
    pub record: Result<capsule_runtime::record_seal::Verdict, String>,
}

/// Reads every session directory of `workdir`'s records group after a host kill.
fn session_record_checks(workdir: &Path) -> Vec<SessionRecordCheck> {
    use capsule_runtime::record_seal;
    let key = capsule_runtime::murmur_home::ensure_murmur_home()
        .and_then(|home| record_seal::PublicKey::load(&record_seal::public_key_path(&home)));
    let sessions = capsule_runtime::records_group(workdir)
        .ok()
        .and_then(|group| fs::read_dir(group).ok());
    let mut checks = Vec::new();
    for session in sessions.into_iter().flatten().flatten() {
        let dir = session.path();
        if !dir.is_dir() {
            continue;
        }
        let trace = fs::read_to_string(dir.join("trace.jsonl"));
        let count = |event: &str| {
            trace.as_deref().map_or(0, |text| {
                text.lines()
                    .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                    .filter(|value| value.get("event_type").and_then(|v| v.as_str()) == Some(event))
                    .count()
            })
        };
        let record = match (&trace, &key) {
            (Err(err), _) => Err(format!("no trace.jsonl: {err}")),
            (_, Err(err)) => Err(format!("no record-seal public key: {err}")),
            (Ok(_), Ok(key)) => {
                Ok(record_seal::verify_session_records(&dir, Some(key), None).verdict)
            }
        };
        checks.push(SessionRecordCheck {
            session_start: count("session_start"),
            session_end: count("session_end"),
            dir,
            record,
        });
    }
    checks
}

/// The record check of a runtime the runner killed from the host once the probe reported.
///
/// Every session record `unsealed`, with a `session_start` and no `session_end`, leaves the
/// verdict as the probe reported it: the kill is visible in the record. A record that verifies
/// is `ALLOWED`: a killed runtime left a record indistinguishable from a finished one. Anything
/// else — no record, no key, `absent`, `unverifiable`, `changed`, a `session_end` — is
/// `INCONCLUSIVE`, and so is a run the runner never killed, because the probe never reported.
fn fold_runtime_kill_record(
    verdict: Verdict,
    detail: String,
    killed: bool,
    checks: &[SessionRecordCheck],
) -> (Verdict, String) {
    use capsule_runtime::record_seal::Verdict as Record;
    if !killed {
        return (
            Verdict::Inconclusive,
            format!(
                "{detail} [the probe never reported, so the runner never killed mur and the \
                 record was not checked]"
            ),
        );
    }
    if checks.is_empty() {
        return (
            Verdict::Inconclusive,
            format!(
                "{detail} [runner killed mur from the host after the probe reported, and no \
                 session record was found to check]"
            ),
        );
    }
    let mut folded = verdict;
    let mut found = Vec::new();
    for check in checks {
        let record = match &check.record {
            Ok(record) => record.as_str().to_string(),
            Err(reason) => format!("not checked ({reason})"),
        };
        found.push(format!(
            "session_start={}, session_end={}, record={record}",
            check.session_start, check.session_end
        ));
        let this = match check.record {
            Ok(Record::Verified) => Verdict::Allowed,
            Ok(Record::Unsealed) if check.session_start > 0 && check.session_end == 0 => verdict,
            _ => Verdict::Inconclusive,
        };
        folded = match (folded, this) {
            (Verdict::Allowed, _) | (_, Verdict::Allowed) => Verdict::Allowed,
            (Verdict::Inconclusive, _) | (_, Verdict::Inconclusive) => Verdict::Inconclusive,
            _ => folded,
        };
    }
    let mut note = format!(
        "[runner killed mur from the host after the probe reported: {}]",
        found.join("; ")
    );
    if folded == Verdict::Allowed && verdict != Verdict::Allowed {
        note.push_str(" [a killed runtime left a record that verifies: the kill is undetectable]");
    }
    (folded, format!("{detail} {note}"))
}

/// Every host process whose working directory lies under `dir`, with its name.
fn processes_under(dir: &Path) -> Vec<(u32, String)> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| {
            fs::read_link(format!("/proc/{pid}/cwd")).is_ok_and(|cwd| cwd.starts_with(dir))
        })
        .map(|pid| {
            let comm = fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
            (pid, comm.trim().to_string())
        })
        .collect()
}

/// Waits up to `wait` for every process working under `dir` to exit, then kills any left, and
/// returns those it had to kill.
fn stop_processes_under(dir: &Path, wait: Duration) -> Vec<(u32, String)> {
    let started = Instant::now();
    loop {
        let left = processes_under(dir);
        if left.is_empty() {
            return left;
        }
        if started.elapsed() >= wait {
            for (pid, _) in &left {
                let _ = Command::new("kill")
                    .args(["-KILL", &pid.to_string()])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
            return left;
        }
        sleep(Duration::from_millis(200));
    }
}

/// The DETAIL note for the processes [`stop_processes_under`] found after a host kill.
fn lingering_note(lingering: &[(u32, String)]) -> String {
    if lingering.is_empty() {
        return format!(
            "[every process of the case had exited within {}s of the host kill]",
            LINGER_WAIT.as_secs()
        );
    }
    let named: Vec<String> = lingering
        .iter()
        .map(|(pid, comm)| format!("{pid} ({comm})"))
        .collect();
    format!(
        "[still running {}s after the host kill, then killed by the runner: {}]",
        LINGER_WAIT.as_secs(),
        named.join(", ")
    )
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

    fn case_dir_for_test() -> PathBuf {
        PathBuf::from("/tmp/escape-conformance-test/case")
    }

    fn config() -> RunnerConfig {
        RunnerConfig {
            mur: PathBuf::from("/nonexistent/mur"),
            probe_driver: PathBuf::from("/nonexistent/probe-driver"),
            driver_artifact: PathBuf::from("/nonexistent/escape-conformance-driver-0.1.0.mur.zip"),
            probe_guest_artifact: None,
            work_root: PathBuf::from("/tmp/escape-conformance-test"),
            timeout: Duration::from_secs(1),
            harness_interpreter: PathBuf::from("/usr/bin/python3"),
            systemd_scope: false,
        }
    }

    fn scoped_yaml(id: &str, route: Route) -> String {
        let case = cases::find(id).unwrap();
        manifest_yaml(
            case,
            route,
            &config(),
            ContainmentClass::Scoped,
            &case_dir_for_test(),
        )
    }

    #[test]
    fn a_second_run_on_one_work_root_grades_only_its_own_files() {
        let work_root =
            crate::work_root::prepare_work_root(&crate::work_root::tests::scratch_path("rerun"))
                .expect("a fresh work root");

        // The files a first run leaves at the paths `stage`, the probe and the driver use.
        let case_dir = work_root.join(cases::PREFLIGHT.id);
        let workdir = case_dir.join("wd");
        // The earlier run's records directory, under a scratch home: nextest runs this test in a
        // process of its own, so no other test reads `HOME` while it is set.
        let home = crate::work_root::tests::scratch_path("rerun-home");
        fs::create_dir_all(&home).unwrap();
        std::env::set_var("HOME", &home);
        fs::create_dir_all(&workdir).unwrap();
        let session = capsule_runtime::records_group(&workdir)
            .unwrap()
            .join("sess-earlier");
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
        let _ = fs::remove_dir_all(&home);
    }

    fn preflight_outcome(detail: &str, reach: RunReach) -> CaseOutcome {
        CaseOutcome {
            case: &cases::PREFLIGHT,
            route: Route::ShellTool,
            expectation: Expectation::Must(Verdict::Succeeded),
            verdict: Verdict::Inconclusive,
            detail: detail.to_string(),
            grade: Grade::Fail,
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
        let refused_exec =
            "case=preflight tool=python3 :: isError=true :: Permission denied (os error 13)";
        let traceback = "case=preflight tool=python3 :: isError=false :: Exit code: 1 Stderr: \
                         Traceback (most recent call last):";
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
            (
                preflight_outcome(
                    "the probe wrote no verdict file; the tool call reported: case=preflight \
                     tool=escape-conformance-probe-guest :: isError=false :: escape-probe request \
                     not answered",
                    ran(
                        false,
                        None,
                        Some(
                            "case=preflight tool=escape-conformance-probe-guest :: \
                             isError=false :: escape-probe request not answered",
                        ),
                    ),
                ),
                PreflightCheck::ProbeGuestUnanswered,
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
            assert!(refusal.contains("on the shell-tool route"), "{refusal}");
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
        let yaml = scoped_yaml("read-etc-shadow", Route::ShellTool);
        assert!(yaml.contains("allow:\n      - bash\n      - python3"));
        assert!(!yaml.contains("resources:"));
        // Nothing may be declared network-reachable, or the four network cases assert nothing.
        assert!(!yaml.contains("network:"));
        assert!(yaml.contains("transport: process"));
    }

    #[test]
    fn manifest_declares_the_containment_class_under_test() {
        let case = cases::find("read-etc-shadow").unwrap();
        for class in ContainmentClass::DECLARABLE {
            let yaml = manifest_yaml(
                case,
                Route::ShellTool,
                &config(),
                class,
                &case_dir_for_test(),
            );
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
        let yaml = scoped_yaml("resource-fork-bomb", Route::ShellTool);
        assert!(yaml.contains("cgroup_pids_max: 32"));
        assert!(yaml.contains("workdir_max_bytes: 52428800"));
        // 128, not the manual-verification document's 64: see `tight_resources_yaml`. The
        // fd-exhauster case grades only a ceiling far below the 1024 default.
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
            shell_exit_code("case=x tool=python3 isError=false :: Exit code: 137"),
            Some(137)
        );
        assert_eq!(shell_exit_code("Exit code: 0 Stdout: hi"), Some(0));
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
        // A work root and a probe driver path with a space and a colon in them: every path is a
        // quoted scalar, or one of these would end the value early.
        config.work_root = PathBuf::from("/tmp/escape conformance: work");
        config.probe_driver = PathBuf::from("/opt/a build: tree/probe-driver");

        let mut profiles_seen = (false, false);
        let mut routes_seen = (false, false, false);
        let cases = std::iter::once(&cases::PREFLIGHT).chain(cases::all_cases());
        for (case, route) in cases.flat_map(|case| case.routes().map(move |route| (case, route))) {
            match case.profile {
                Profile::Boundary => profiles_seen.0 = true,
                Profile::TightResources => profiles_seen.1 = true,
            }
            match route {
                Route::ShellTool => routes_seen.0 = true,
                Route::ProcessHarness => routes_seen.1 = true,
                Route::WasmGuest => routes_seen.2 = true,
            }
            let case_dir = case_dir(&config, case, route);
            let driver_log = driver_log_path(&case_dir);
            for class in ContainmentClass::DECLARABLE {
                let yaml = manifest_yaml(case, route, &config, class, &case_dir);
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
                    None,
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
                assert_eq!(driver.artifact, driver_artifact::DRIVER_ARTIFACT);
                assert!(
                    manifest
                        .artifacts
                        .iter()
                        .any(|a| a.name == driver_artifact::DRIVER_ARTIFACT
                            && a.version == driver_artifact::DRIVER_VERSION
                            && matches!(a.runtime, ArtifactRuntime::Driver)),
                    "case {id} at class {class}: the driver is not declared under artifacts:"
                );
                let guest_declared = manifest.artifacts.iter().any(|a| {
                    a.name == probe_guest::PROBE_GUEST_ARTIFACT
                        && a.version == probe_guest::PROBE_GUEST_VERSION
                        && matches!(a.runtime, ArtifactRuntime::Tool)
                });
                assert_eq!(
                    guest_declared,
                    route == Route::WasmGuest,
                    "case {id} at class {class} on {}: the probe guest is declared on the \
                     wasm-guest route and only there",
                    route.as_str()
                );

                let probe = ProbeConfig::from_json(
                    driver
                        .config
                        .as_deref()
                        .expect("an inference.driver.config"),
                )
                .unwrap_or_else(|err| panic!("case {id} at class {class}: {err}"));
                assert_eq!(probe.case, id);
                let tool = match route {
                    Route::WasmGuest => probe_guest::PROBE_GUEST_NAME,
                    _ => "python3",
                };
                assert_eq!(probe.tool, tool);
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
                assert_eq!(probe.route.route(), route, "case {id} at class {class}");
                if let ProbeRoute::ProcessHarness {
                    interpreter,
                    workdir,
                } = &probe.route
                {
                    assert_eq!(interpreter, "/usr/bin/python3");
                    assert_eq!(workdir, &workdir_in(&case_dir).display().to_string());
                }
            }
        }
        assert_eq!(
            profiles_seen,
            (true, true),
            "both profiles must be generated, or one manifest shape goes unchecked"
        );
        assert_eq!(
            routes_seen,
            (true, true, true),
            "every route must be generated, or one driver config goes unchecked"
        );
    }

    #[test]
    fn each_route_stages_its_own_case_directory() {
        let config = config();
        let case = cases::find("read-record-seal-key").unwrap();
        let shell = case_dir(&config, case, Route::ShellTool);
        let harness = case_dir(&config, case, Route::ProcessHarness);
        assert_eq!(shell, config.work_root.join("read-record-seal-key"));
        assert_eq!(
            harness,
            config
                .work_root
                .join("read-record-seal-key--process-harness")
        );
    }

    /// The harness route's manifest is the shell route's with the driver config told to run the
    /// probe itself: same class, same shell allowlist, so the two verdicts differ by route alone.
    #[test]
    fn the_harness_route_manifest_differs_only_in_the_driver_config() {
        let shell = scoped_yaml("read-record-seal-key", Route::ShellTool);
        let harness = scoped_yaml("read-record-seal-key", Route::ProcessHarness);
        assert!(shell.contains("      route: \"shell-tool\"\n"), "{shell}");
        assert!(
            harness.contains("      route: \"process-harness\"\n"),
            "{harness}"
        );
        assert!(harness.contains("      interpreter: \"/usr/bin/python3\"\n"));
        assert!(harness.contains("      workdir: \"/tmp/escape-conformance-test/case/wd\"\n"));
        let strip = |yaml: &str| -> Vec<String> {
            yaml.lines()
                .filter(|line| {
                    !["      route:", "      interpreter:", "      workdir:"]
                        .iter()
                        .any(|key| line.starts_with(key))
                })
                .map(str::to_string)
                .collect()
        };
        assert_eq!(strip(&shell), strip(&harness));
    }

    #[test]
    fn a_route_the_case_does_not_run_on_is_not_staged() {
        let case = cases::find("read-etc-shadow").unwrap();
        let outcome = run_case(
            case,
            Route::ProcessHarness,
            &config(),
            ContainmentClass::Scoped,
        );
        assert_eq!(outcome.reach, RunReach::NotStaged);
        assert_eq!(outcome.verdict, Verdict::Inconclusive);
        assert_eq!(outcome.grade, Grade::NotAsserted);
        assert!(!config()
            .work_root
            .join("read-etc-shadow--process-harness")
            .exists());
    }

    /// The disk-filler leak: a graded case's probe output goes, its evidence stays.
    #[test]
    fn scratch_is_removed_after_grading_and_the_evidence_is_kept() {
        let root = crate::work_root::tests::scratch_path("scratch");
        let workdir = root.join("wd");
        fs::create_dir_all(workdir.join("nested")).unwrap();
        let case = cases::find("read-record-seal-key").unwrap();
        for kept in [probe::PROBE_SCRIPT, probe::PROBE_FILE, "seal-key-path.txt"] {
            fs::write(workdir.join(kept), "evidence").unwrap();
        }
        fs::write(workdir.join("ec-fill-0"), vec![0u8; 4096]).unwrap();
        fs::write(workdir.join("nested").join("ec-fill-1"), vec![0u8; 1024]).unwrap();
        // A link to something outside the workdir goes as a link; its target is untouched.
        let outside = root.join("outside");
        fs::write(&outside, "not scratch").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, workdir.join("ec-symlink")).unwrap();

        let removed = remove_scratch(case, &workdir);
        assert!(removed.errors.is_empty(), "{:?}", removed.errors);
        assert_eq!(removed.bytes, 4096 + 1024);
        let mut left: Vec<String> = fs::read_dir(&workdir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["__ec.probe", "ec-probe.py", "seal-key-path.txt"]);
        assert_eq!(fs::read_to_string(&outside).unwrap(), "not scratch");
        assert!(removed.note().unwrap().contains("5120 bytes"));

        // Nothing left: nothing to say.
        assert_eq!(remove_scratch(case, &workdir).note(), None);
        fs::remove_dir_all(&root).unwrap();
    }

    /// A host-channel case declares exactly its variables on both ways into a probe; every other
    /// case declares neither, so its manifest is what it always was.
    #[test]
    fn only_a_host_channel_case_declares_its_variables() {
        let cases = std::iter::once(&cases::PREFLIGHT)
            .chain(std::iter::once(&cases::SIBLING_TARGET))
            .chain(cases::all_cases());
        let mut channels = 0;
        for case in cases {
            for route in case.routes() {
                let yaml = manifest_yaml(
                    case,
                    route,
                    &config(),
                    ContainmentClass::Scoped,
                    &case_dir_for_test(),
                );
                let manifest = RuntimeManifest::from_yaml_str(&yaml).unwrap();
                let capabilities = manifest.capabilities.as_ref().unwrap();
                let baseline = capabilities
                    .shell
                    .as_ref()
                    .and_then(|shell| shell.baseline_env.clone())
                    .unwrap_or_default();
                let allowed = capabilities
                    .env
                    .as_ref()
                    .map(|env| env.allow.clone())
                    .unwrap_or_default();
                match case.prepare {
                    Prepare::HostChannel(channel) => {
                        channels += 1;
                        assert_eq!(baseline, channel.env_names(), "{}", case.id);
                        assert_eq!(allowed, channel.env_names(), "{}", case.id);
                    }
                    _ => {
                        assert!(!yaml.contains("baseline_env"), "{}: {yaml}", case.id);
                        assert!(!yaml.contains("\n  env:"), "{}: {yaml}", case.id);
                    }
                }
            }
        }
        // Six host-channel cases, each on the shell tool and the process harness.
        assert_eq!(channels, 12);
        let yaml = scoped_yaml("signal-sibling-capsule", Route::ProcessHarness);
        assert!(
            yaml.contains(
                "    baseline_env:\n      - MUR_EC_RUNTIME_PID\n      - MUR_EC_SIBLING_PIDS\n"
            ),
            "{yaml}"
        );
        assert!(
            yaml.contains(
                "  env:\n    allow:\n      - MUR_EC_RUNTIME_PID\n      - MUR_EC_SIBLING_PIDS\n"
            ),
            "{yaml}"
        );
    }

    /// A host-channel case's `mur` is launched from a shell that exports its own pid and `exec`s,
    /// so the exported pid is `mur`'s through every wrapper.
    #[test]
    fn a_host_channel_case_exports_the_runtime_pid_through_exec_wrappers() {
        let mut cfg = config();
        cfg.systemd_scope = true;
        let case = cases::find("signal-own-runtime").unwrap();
        let argv = build_argv(case, &cfg, Path::new("/tmp/m.yaml"), Path::new("/tmp/wd"));
        assert_eq!(
            argv[..4],
            [
                "bash",
                "-c",
                "export MUR_EC_RUNTIME_PID=$$; exec \"$@\"",
                "escape-conformance"
            ]
        );
        assert_eq!(argv[4], "systemd-run");
        let mur = argv.iter().position(|a| a.ends_with("mur")).unwrap();
        assert_eq!(argv[mur + 1], "run");

        let plain = cases::find("record-rewrite").unwrap();
        let argv = build_argv(plain, &cfg, Path::new("/tmp/m.yaml"), Path::new("/tmp/wd"));
        assert_eq!(argv[0], "systemd-run");
        assert!(!argv.iter().any(|a| a.contains("MUR_EC_")));
    }

    /// Only `runtime-kill-record` on the process harness holds its run for a host kill, and the
    /// marker it is staged with survives the scratch cleanup.
    #[test]
    fn only_the_runtime_kill_case_on_the_harness_is_killed_after_it_reports() {
        for case in std::iter::once(&cases::PREFLIGHT).chain(cases::all_cases()) {
            for route in Route::ALL {
                assert_eq!(
                    kills_runtime_after_report(case, route),
                    case.id == "runtime-kill-record" && route == Route::ProcessHarness,
                    "{} on {}",
                    case.id,
                    route.as_str()
                );
            }
        }
        let case = cases::find("runtime-kill-record").unwrap();
        assert!(staged_names(case).contains(&HOLD_FOR_HOST_KILL));
        assert!(case.body.contains(HOLD_FOR_HOST_KILL));
        assert!(cases::SIBLING_TARGET.body.contains(SIBLING_READY));
        assert!(cases::SIBLING_TARGET.body.contains(SIBLING_HOLD));
    }

    /// A host target that ends by itself before the survival check reads as one the probe
    /// stopped, so every target outlives the longest case the timeout allows.
    #[test]
    fn host_targets_outlive_the_longest_case() {
        for secs in [1, 300, 3600] {
            let timeout = Duration::from_secs(secs);
            assert!(
                target_hold(timeout) > SIBLING_READY_WAIT + timeout,
                "{secs}"
            );
        }
    }

    #[test]
    fn a_probe_file_has_reported_once_it_holds_a_verdict_line() {
        let root = crate::work_root::tests::scratch_path("reported");
        fs::create_dir_all(&root).unwrap();
        let file = root.join(probe::PROBE_FILE);
        assert!(!has_reported(&file));
        fs::write(&file, "").unwrap();
        assert!(!has_reported(&file));
        fs::write(&file, "VERDICT=REFUSED\nDETAIL=x\n").unwrap();
        assert!(has_reported(&file));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn descendants_follow_the_parent_chain_and_nothing_else() {
        let table = [
            (10, 1),
            (11, 10),
            (12, 11),
            (13, 10),
            (20, 1),
            (21, 20),
            // A cycle cannot loop the walk.
            (30, 31),
            (31, 30),
        ];
        assert_eq!(descendants(10, &table), [11, 12, 13]);
        assert_eq!(descendants(20, &table), [21]);
        assert_eq!(descendants(12, &table), Vec::<u32>::new());
        assert_eq!(descendants(30, &table), [31]);
    }

    #[test]
    fn this_process_is_alive_and_a_reaped_child_is_not() {
        assert!(pid_alive(std::process::id()));
        let mut child = Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert!(!pid_alive(pid));
    }

    #[test]
    fn the_survival_check_overrides_a_refusal_when_a_target_is_gone() {
        let (verdict, detail) = fold_survival(
            Verdict::Refused,
            "probe said".to_string(),
            &[(100, true), (101, true)],
        );
        assert_eq!(verdict, Verdict::Refused);
        assert_eq!(
            detail,
            "probe said [every target pid was alive after the case: 100 101]"
        );

        let (verdict, detail) = fold_survival(
            Verdict::Refused,
            "probe said".to_string(),
            &[(100, true), (101, false)],
        );
        assert_eq!(verdict, Verdict::Allowed);
        assert!(detail.contains("target pid(s) 101 were gone"), "{detail}");

        // An ALLOWED stays ALLOWED either way.
        let (verdict, _) = fold_survival(Verdict::Allowed, String::new(), &[(100, true)]);
        assert_eq!(verdict, Verdict::Allowed);
    }

    fn record_check(
        start: usize,
        end: usize,
        record: Result<capsule_runtime::record_seal::Verdict, &str>,
    ) -> SessionRecordCheck {
        SessionRecordCheck {
            dir: PathBuf::from("/records/sess"),
            session_start: start,
            session_end: end,
            record: record.map_err(str::to_string),
        }
    }

    #[test]
    fn the_runtime_kill_record_check_keeps_the_verdict_only_for_an_unsealed_record() {
        use capsule_runtime::record_seal::Verdict as Record;
        let fold = |killed: bool, checks: &[SessionRecordCheck]| {
            fold_runtime_kill_record(Verdict::Refused, "probe said".to_string(), killed, checks)
        };

        let (verdict, detail) = fold(true, &[record_check(1, 0, Ok(Record::Unsealed))]);
        assert_eq!(verdict, Verdict::Refused);
        assert!(
            detail.ends_with(
                "[runner killed mur from the host after the probe reported: session_start=1, \
                 session_end=0, record=unsealed]"
            ),
            "{detail}"
        );

        let (verdict, detail) = fold(true, &[record_check(1, 0, Ok(Record::Verified))]);
        assert_eq!(verdict, Verdict::Allowed);
        assert!(detail.contains("the kill is undetectable"), "{detail}");

        for record in [Record::Absent, Record::Unverifiable, Record::Changed] {
            let (verdict, detail) = fold(true, &[record_check(1, 0, Ok(record))]);
            assert_eq!(verdict, Verdict::Inconclusive, "{detail}");
            assert!(detail.contains(&format!("record={}", record.as_str())));
        }
        // Unsealed but with a session_end, or without a session_start: not a killed runtime.
        assert_eq!(
            fold(true, &[record_check(1, 1, Ok(Record::Unsealed))]).0,
            Verdict::Inconclusive
        );
        assert_eq!(
            fold(true, &[record_check(0, 0, Ok(Record::Unsealed))]).0,
            Verdict::Inconclusive
        );

        let (verdict, detail) = fold(true, &[record_check(0, 0, Err("no trace.jsonl: gone"))]);
        assert_eq!(verdict, Verdict::Inconclusive);
        assert!(
            detail.contains("not checked (no trace.jsonl: gone)"),
            "{detail}"
        );

        let (verdict, detail) = fold(true, &[]);
        assert_eq!(verdict, Verdict::Inconclusive);
        assert!(detail.contains("no session record"), "{detail}");

        let (verdict, detail) = fold(false, &[record_check(1, 0, Ok(Record::Unsealed))]);
        assert_eq!(verdict, Verdict::Inconclusive);
        assert!(detail.contains("never killed mur"), "{detail}");

        // A verifying record wins over every other session's.
        assert_eq!(
            fold(
                true,
                &[
                    record_check(1, 0, Err("no trace.jsonl: gone")),
                    record_check(1, 0, Ok(Record::Verified))
                ]
            )
            .0,
            Verdict::Allowed
        );
    }
}
