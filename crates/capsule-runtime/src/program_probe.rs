//! Which programs a capsule can run on this host, found by running each one inside the sandbox a
//! launch of the capsule would get here: what `mur doctor --programs` reports.
//!
//! [`ProgramProbeRunner`] derives the staging-time policy fields from the capsule's policy through
//! the functions `runtime::stage_session` calls, decides containment with
//! [`ContainmentDecision::decide`] from one [`HostProbe`], and builds each probe's
//! [`ShellEnforcement`] with [`ShellEnforcement::resolve`], bounded by a cgroup scope and given a
//! runtime directory, home and temporary folder of its own. Each probe step runs through
//! [`crate::shell::run_shell_until_stopped`], the path a session's foreground command takes, in a
//! scratch work folder under `~/.murmur/probes/<id>/`. Nothing is granted: the program is added to
//! `capabilities.shell.allow` in the probe's own copy of the policy only, so the reported grant is
//! complete, and whether the manifest already grants it is recorded beside the result.
//!
//! [`classify`] turns what a step did, read against static facts about the program (where it
//! resolves on the capsule's `PATH`, a snap launcher, a rustup proxy, a `#!` interpreter chain),
//! into a cause and the grant that would lift it.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use murmur_artifact::ContainmentClass;
use serde::Serialize;

use crate::{
    containment::ContainmentDecision,
    declared_needs::DeclaredToolNeeds,
    pinned_programs::DeclaredToolDownloads,
    sandbox::{EnforcementTier, HostProbe, ShellEnforcement},
    types::CapabilityPolicy,
    workdir_fs::WorkdirRoot,
};

/// How long a probe step runs before it is killed, unless its [`ProbeStep`] says otherwise.
pub const DEFAULT_STEP_TIMEOUT: Duration = Duration::from_secs(30);

/// How many of the deciding step's last stderr lines [`ProgramProbe::detail`] keeps.
pub const DETAIL_LINES: usize = 3;

/// How many characters of each [`ProgramProbe::detail`] line are kept; a longer line ends in `…`.
pub const DETAIL_LINE_CHARS: usize = 300;

/// The folder beneath `~/.murmur` holding one `<id>/` per running probe.
pub const PROBES_DIR_NAME: &str = "probes";

/// The programs probed when no names are given, in report order. Each `capabilities.shell.allow`
/// entry not listed here follows them.
pub const COMMON_PROGRAMS: &[&str] = &[
    "sh", "bash", "git", "curl", "wget", "python3", "node", "npm", "make", "cc", "cargo", "rustc",
    "go", "jq", "rg", "unshare", "sudo",
];

/// The seccomp-denied programs whose refusal no manifest grant lifts.
const NAMESPACE_PROGRAMS: &[&str] = &["unshare", "bwrap", "nsenter", "firejail", "newuidmap"];

/// The programs that gain privilege by a setuid bit or a helper, which `no_new_privs` refuses.
const PRIVILEGE_PROGRAMS: &[&str] = &["sudo", "su", "doas", "pkexec", "newgrp"];

/// The programs whose probe gets a [`ProgramProbe::note`] about the network allowlist.
const NETWORK_PROGRAMS: &[&str] = &["curl", "wget", "git"];

/// The grant text for a refusal only an unconfined launch lifts.
pub const UNCONFINED_ONLY_GRANT: &str = "no manifest grant lifts this; only an unconfined launch \
     does (mur run --unconfined, or containment: unconfined in ~/.murmur/config.yaml)";

const E_CAP_005: &str = "E-CAP-005";
const E_RUN_012: &str = "E-RUN-012";
const E_RUN_014: &str = "E-RUN-014";

const E_CAP_005_GRANT: &str = concat!(
    "make this host able to create the capsule's network namespace \u{2014} see ",
    murmur_artifact::docs_reference_url!("diagnostics/#e-cap-005")
);
const E_RUN_012_GRANT: &str = concat!(
    "give `mur` a delegated cgroup v2 scope: run it under a systemd user unit with \
     `Delegate=yes` for memory, pids, cpu and io (for example `systemd-run --user --scope -p \
     Delegate=yes mur run`) \u{2014} see ",
    murmur_artifact::docs_reference_url!("resource-limits/#platform-behavior")
);
const SANDBOX_UNPREPARED_GRANT: &str =
    "see the Host and Containment sections of mur doctor for what this host can back";

/// A file a probe step writes into the scratch work folder before it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeFile {
    /// Relative to the work folder.
    pub path: String,
    pub contents: String,
}

/// One command a probe runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeStep {
    /// The program first, then its arguments, run with no shell.
    pub argv: Vec<String>,
    pub timeout: Duration,
    /// Written into the work folder before the step runs.
    pub files: Vec<ProbeFile>,
}

impl ProbeStep {
    fn new(argv: &[&str]) -> Self {
        Self {
            argv: argv.iter().map(|word| (*word).to_string()).collect(),
            timeout: DEFAULT_STEP_TIMEOUT,
            files: Vec::new(),
        }
    }

    fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    fn with_file(mut self, path: &str, contents: &str) -> Self {
        self.files.push(ProbeFile {
            path: path.to_string(),
            contents: contents.to_string(),
        });
        self
    }
}

/// What to run to find out whether `program` runs: its steps, in order. The first step that does
/// not exit 0 decides the result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeSpec {
    pub program: String,
    pub steps: Vec<ProbeStep>,
}

const HELLO_C: &str = "int main(void) { return 0; }\n";
const HELLO_CARGO_TOML: &str = "[package]\nname = \"hello\"\nversion = \"0.1.0\"\nedition = \
     \"2021\"\n\n[dependencies]\n\n[workspace]\n";
const HELLO_MAIN_RS: &str = "fn main() {}\n";

/// The probe for `program`: a fixed one for each [`COMMON_PROGRAMS`] entry, `<program> --version`
/// with [`DEFAULT_STEP_TIMEOUT`] for any other name.
#[must_use]
pub fn probe_spec(program: &str) -> ProbeSpec {
    let steps = match program {
        "sh" => vec![ProbeStep::new(&["sh", "-c", ":"])],
        "python3" => vec![ProbeStep::new(&["python3", "-c", "pass"])],
        "node" => vec![ProbeStep::new(&["node", "-e", "0"])],
        "go" => vec![ProbeStep::new(&["go", "version"])],
        "unshare" => vec![ProbeStep::new(&[
            "unshare",
            "--user",
            "--map-root-user",
            "true",
        ])],
        "sudo" => vec![ProbeStep::new(&["sudo", "-n", "true"])],
        "cc" => vec![ProbeStep::new(&["cc", "hello.c", "-o", "hello"])
            .with_timeout(Duration::from_secs(120))
            .with_file("hello.c", HELLO_C)],
        "cargo" => vec![
            ProbeStep::new(&["cargo", "--version"]),
            ProbeStep::new(&[
                "cargo",
                "build",
                "--offline",
                "--quiet",
                "--manifest-path",
                "hello/Cargo.toml",
                "--target-dir",
                "hello/target",
            ])
            .with_timeout(Duration::from_secs(180))
            .with_file("hello/Cargo.toml", HELLO_CARGO_TOML)
            .with_file("hello/src/main.rs", HELLO_MAIN_RS),
        ],
        other => vec![ProbeStep::new(&[other, "--version"])],
    };
    ProbeSpec {
        program: program.to_string(),
        steps,
    }
}

/// The programs to probe, each once in first-seen order: `names` when given, else
/// [`COMMON_PROGRAMS`] then each `shell_allow` entry.
#[must_use]
pub fn programs_to_probe(names: Option<&[String]>, shell_allow: &[String]) -> Vec<String> {
    let listed: Vec<String> = match names {
        Some(names) => names.to_vec(),
        None => COMMON_PROGRAMS
            .iter()
            .map(|name| (*name).to_string())
            .chain(shell_allow.iter().cloned())
            .collect(),
    };
    let mut programs: Vec<String> = Vec::new();
    for name in listed {
        let name = name.trim().to_string();
        if !name.is_empty() && !programs.contains(&name) {
            programs.push(name);
        }
    }
    programs
}

/// Whether a probed program can run in this capsule as its manifest stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProbeStatus {
    Runs,
    Refused,
}

/// Why a probed program cannot run in this capsule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProbeCause {
    /// Not found on the capsule's `PATH`; nothing was started.
    NotInstalled,
    /// It runs once added to `capabilities.shell.allow`, which does not name it.
    NotInShellAllow,
    /// It resolves to the snap launcher, which cannot start a snap inside the capsule.
    SnapWrapper,
    /// It needs a namespace syscall the capsule's seccomp filter refuses.
    NamespacesRefused,
    /// It needs to gain privilege, which `no_new_privs` refuses.
    NoNewPrivileges,
    /// A rustup proxy whose `RUSTUP_HOME` or `CARGO_HOME` no `host_paths` entry lends.
    HostPathNeeded,
    /// A program it runs or a library it loads lies outside every executable grant:
    /// `capabilities.shell.interpreter_runtime` naming its folder lifts it.
    InterpreterRuntimeNeeded,
    /// A `#!` script whose interpreter chain is not all in `capabilities.shell.allow`.
    InterpreterNotAllowed,
    /// A compile or link step that needs a helper program the capsule cannot run.
    HelperNotAllowed,
    /// The sandbox could not be prepared, so nothing was started.
    NoSandbox,
    /// A step outran its timeout and was killed.
    TimedOut,
    /// It failed for a reason none of the above names.
    Failed,
}

impl ProbeCause {
    /// The wire name, as `mur doctor --json` spells it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotInstalled => "not-installed",
            Self::NotInShellAllow => "not-in-shell-allow",
            Self::SnapWrapper => "snap-wrapper",
            Self::NamespacesRefused => "namespaces-refused",
            Self::NoNewPrivileges => "no-new-privileges",
            Self::HostPathNeeded => "host-path-needed",
            Self::InterpreterRuntimeNeeded => "interpreter-runtime-needed",
            Self::InterpreterNotAllowed => "interpreter-not-allowed",
            Self::HelperNotAllowed => "helper-not-allowed",
            Self::NoSandbox => "no-sandbox",
            Self::TimedOut => "timed-out",
            Self::Failed => "failed",
        }
    }
}

/// One probed program. Serialized keys keep the field order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProgramProbe {
    pub program: String,
    pub status: ProbeStatus,
    /// Whether the manifest's `capabilities.shell.allow` names it.
    pub in_shell_allow: bool,
    /// The first match on the capsule's `PATH`, as found there (a link is not followed); `None`
    /// when there is none.
    pub resolved: Option<String>,
    /// The deciding step's argv: the first that did not exit 0, else the last.
    pub probe: Vec<String>,
    /// The deciding step's exit code; `None` when it was not started, did not start, or was
    /// killed at its timeout.
    pub exit_code: Option<i32>,
    /// `None` when [`ProbeStatus::Runs`].
    pub cause: Option<ProbeCause>,
    /// Why it cannot run; `None` when it runs.
    pub reason: Option<String>,
    /// What would lift the refusal; `None` when it runs, and for [`ProbeCause::Failed`] and
    /// [`ProbeCause::TimedOut`], which no known grant lifts.
    pub grant: Option<String>,
    /// A fact about running it that is not a refusal, such as which hosts it can reach.
    pub note: Option<String>,
    /// The deciding step's last [`DETAIL_LINES`] stderr lines, trimmed; empty when it printed
    /// none.
    pub detail: String,
}

/// Every probed program and the sandbox they were probed in. Serialized keys keep the field order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProgramsReport {
    /// The class the probes ran under, as `session_start` names an achieved class.
    pub containment: String,
    /// The tier's mechanism, as `--explain-scope` spells `enforcement_tier`.
    pub mechanism: String,
    /// The `PATH` each probe resolved its program on: the one a `mur run` shell command gets.
    pub path: String,
    pub probed: Vec<ProgramProbe>,
}

/// What a rustup proxy reads its toolchains from, and whether the capsule can read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RustupHomes {
    pub rustup_home: PathBuf,
    pub cargo_home: PathBuf,
    /// Whether a `host_paths` entry is or contains each.
    pub rustup_home_lent: bool,
    pub cargo_home_lent: bool,
    /// `<RUSTUP_HOME>/toolchains/<default_toolchain>`, from rustup's `settings.toml`, when it
    /// names one.
    pub toolchain_dir: Option<PathBuf>,
}

/// What is known about a program without running it: [`classify`]'s first input.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StaticFacts {
    pub program: String,
    /// Whether the manifest's `capabilities.shell.allow` names it.
    pub in_shell_allow: bool,
    /// The manifest's `capabilities.shell.allow`.
    pub shell_allow: Vec<String>,
    /// The first match on the capsule's `PATH`.
    pub resolved: Option<PathBuf>,
    /// [`Self::resolved`] with every link followed.
    pub canonical: Option<PathBuf>,
    /// The snap launcher [`Self::resolved`] reaches through its links (`/snap/bin/curl` →
    /// `/usr/bin/snap`): the first hop named `snap` or `snapd`.
    pub snap_launcher: Option<PathBuf>,
    /// The first match later on the same `PATH` that is not the snap launcher.
    pub non_snap_copy: Option<PathBuf>,
    /// Set when [`Self::resolved`] is a rustup proxy.
    pub rustup: Option<RustupHomes>,
    /// The `#!` line's program and, through `env`, the interpreter it runs: `["env", "node"]`.
    pub interpreter_chain: Vec<String>,
    /// `capabilities.network.allow`.
    pub network_allow: Vec<String>,
    /// The probes run unconfined: no sandbox refuses anything.
    pub unconfined: bool,
    /// The probes run on the sealed tier.
    pub sealed: bool,
    /// The shared library the deciding step's output says could not be loaded, and the host
    /// folder holding it, when the host has it.
    pub missing_library: Option<(String, PathBuf)>,
}

/// What one probe step did: [`classify`]'s second input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepOutcome {
    /// It ran to an exit.
    Exited {
        exit_code: i32,
        stdout: String,
        stderr: String,
    },
    /// The program could not be started: `execve` refused it.
    NotStarted { message: String },
    /// It outran its timeout and was killed.
    TimedOut { timeout: Duration, stderr: String },
    /// The sandbox could not be prepared; nothing was started.
    NoSandbox {
        /// The refusal's code, when it has one.
        code: Option<&'static str>,
        reason: String,
        /// What lifts the refusal.
        hint: String,
    },
}

/// What [`classify`] decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classified {
    pub status: ProbeStatus,
    pub cause: Option<ProbeCause>,
    pub reason: Option<String>,
    pub grant: Option<String>,
}

impl Classified {
    fn runs() -> Self {
        Self {
            status: ProbeStatus::Runs,
            cause: None,
            reason: None,
            grant: None,
        }
    }

    fn refused(cause: ProbeCause, reason: String, grant: Option<String>) -> Self {
        Self {
            status: ProbeStatus::Refused,
            cause: Some(cause),
            reason: Some(reason),
            grant,
        }
    }
}

/// The last [`DETAIL_LINES`] non-empty lines of `text`, each trimmed and cut to
/// [`DETAIL_LINE_CHARS`], joined by newlines.
#[must_use]
pub fn tail_lines(text: &str) -> String {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    lines[lines.len().saturating_sub(DETAIL_LINES)..]
        .iter()
        .map(|line| match line.char_indices().nth(DETAIL_LINE_CHARS) {
            Some((cut, _)) => format!("{}\u{2026}", &line[..cut]),
            None => (*line).to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Decides whether a program runs, and if not why and what would lift it, from `facts` and the
/// deciding step's `outcome` (`None` when nothing was started because the program is not on the
/// capsule's `PATH`).
///
/// An exit 0 always means the program runs; it is refused only when the manifest does not grant
/// it. A failure is read against the facts in a fixed order: the snap launcher, a refused
/// namespace syscall, a refused privilege gain, a library or rustup toolchain outside every
/// executable grant, a missing compile helper, a rustup proxy whose homes are not lent, an
/// interpreter chain not in `shell.allow`, then `failed`. Under an unconfined launch no failure is
/// put down to a sandbox.
#[must_use]
pub fn classify(facts: &StaticFacts, outcome: Option<&StepOutcome>) -> Classified {
    let program = facts.program.as_str();
    let add_program = format!("add {program} to capabilities.shell.allow");
    // A grant a manifest edit gives, with the program's own `shell.allow` entry when it is
    // missing too.
    let with_allow = |grant: String| {
        if facts.in_shell_allow {
            grant
        } else {
            format!("{grant}; and {add_program}")
        }
    };

    if let Some(StepOutcome::NoSandbox { code, reason, hint }) = outcome {
        let reason = match code {
            Some(code) => format!("{code}: {reason}"),
            None => reason.clone(),
        };
        return Classified::refused(ProbeCause::NoSandbox, reason, Some(hint.clone()));
    }
    let Some(outcome) = outcome.filter(|_| facts.resolved.is_some()) else {
        return Classified::refused(
            ProbeCause::NotInstalled,
            format!("{program} is not on the capsule's PATH"),
            Some(format!(
                "install {program} on this host so it is on mur run's PATH"
            )),
        );
    };

    let (exit_code, stderr) = match outcome {
        StepOutcome::Exited { exit_code: 0, .. } => {
            return if facts.in_shell_allow {
                Classified::runs()
            } else {
                Classified::refused(
                    ProbeCause::NotInShellAllow,
                    format!("{program} runs only once capabilities.shell.allow names it"),
                    Some(add_program),
                )
            };
        }
        StepOutcome::Exited {
            exit_code, stderr, ..
        } => (Some(*exit_code), stderr.as_str()),
        StepOutcome::NotStarted { message } => (None, message.as_str()),
        StepOutcome::TimedOut { timeout, .. } => {
            return Classified::refused(
                ProbeCause::TimedOut,
                format!(
                    "the probe did not finish within {} s and was killed",
                    timeout.as_secs()
                ),
                None,
            );
        }
        StepOutcome::NoSandbox { .. } => unreachable!("handled above"),
    };
    let failure = match exit_code {
        Some(code) => format!("exited {code}"),
        None => format!("did not start: {}", tail_lines(stderr).replace('\n', " ")),
    };
    let lower = stderr.to_ascii_lowercase();

    // The snap launcher: the program is a link to `snap`, which starts nothing inside a capsule.
    if let Some(launcher) = facts.snap_launcher.as_ref() {
        let resolved = facts
            .resolved
            .as_ref()
            .map_or_else(String::new, |path| path.display().to_string());
        let grant = match &facts.non_snap_copy {
            Some(copy) => {
                let dir = copy.parent().unwrap_or(Path::new("/"));
                let snap_dir = facts
                    .resolved
                    .as_ref()
                    .and_then(|path| path.parent())
                    .unwrap_or(Path::new("/snap/bin"));
                format!(
                    "put {} ahead of {} on mur run's PATH, so {program} resolves to {}",
                    dir.display(),
                    snap_dir.display(),
                    copy.display()
                )
            }
            None => format!("install {program} from the distribution's package"),
        };
        return Classified::refused(
            ProbeCause::SnapWrapper,
            format!(
                "{resolved} is the snap launcher {}, which cannot start a snap inside the \
                 capsule; it {failure}",
                launcher.display()
            ),
            Some(with_allow(grant)),
        );
    }

    if !facts.unconfined {
        let namespace_failure = lower.contains("operation not permitted")
            && (NAMESPACE_PROGRAMS.contains(&program)
                || lower.contains("unshare")
                || lower.contains("setns")
                || lower.contains("user namespace"));
        if namespace_failure {
            return Classified::refused(
                ProbeCause::NamespacesRefused,
                format!(
                    "the capsule's seccomp filter refuses unshare, setns, mount, pivot_root and \
                     chroot at every contained floor; {program} {failure}"
                ),
                Some(UNCONFINED_ONLY_GRANT.to_string()),
            );
        }
        let privilege_failure = lower.contains("no new privileges")
            || lower.contains("effective uid is not 0")
            || lower.contains("nosuid")
            || (PRIVILEGE_PROGRAMS.contains(&program)
                && (lower.contains("operation not permitted")
                    || lower.contains("permission denied")));
        if privilege_failure {
            return Classified::refused(
                ProbeCause::NoNewPrivileges,
                format!(
                    "the capsule's commands run with no_new_privs, so a setuid program gains no \
                     privilege; {program} {failure}"
                ),
                Some(UNCONFINED_ONLY_GRANT.to_string()),
            );
        }
    }

    let runtime_grant = |dir: &Path, list_dir: bool| {
        with_allow(format!(
            "declare capabilities.shell.interpreter_runtime: [{{binary: {program}, dirs: \
             [{{path: {}, list_dir: {list_dir}}}]}}]",
            dir.display()
        ))
    };
    if let Some((library, dir)) = facts.missing_library.as_ref().filter(|_| !facts.unconfined) {
        return Classified::refused(
            ProbeCause::InterpreterRuntimeNeeded,
            format!(
                "{program}, or a program it runs, loads {library} from {}, which no grant \
                 reaches; it {failure}",
                dir.display()
            ),
            Some(runtime_grant(dir, false)),
        );
    }
    if let Some((homes, toolchain)) = facts
        .rustup
        .as_ref()
        .filter(|homes| homes.rustup_home_lent && homes.cargo_home_lent && !facts.unconfined)
        .and_then(|homes| Some((homes, homes.toolchain_dir.as_ref()?)))
    {
        if lower.contains("command failed") && lower.contains("permission denied") {
            return Classified::refused(
                ProbeCause::InterpreterRuntimeNeeded,
                format!(
                    "{program} is a rustup proxy, and the toolchain program it runs lies under \
                     {} in RUSTUP_HOME ({}), a host path, which is never executable; it \
                     {failure}",
                    toolchain.display(),
                    homes.rustup_home.display()
                ),
                Some(runtime_grant(toolchain, true)),
            );
        }
    }

    if let Some(helper) = missing_helper(&lower).filter(|_| !facts.unconfined) {
        let mut grant = format!("add {helper} to capabilities.shell.allow");
        if facts.sealed {
            if let Some(path) = crate::sandbox::find_on_path(&helper) {
                grant.push_str(&format!(
                    " (on sealed the composed root binds only granted programs' folders; \
                     W-SEC-012 names {})",
                    path.display()
                ));
            }
        }
        return Classified::refused(
            ProbeCause::HelperNotAllowed,
            format!("{program} needs the helper program {helper}, which the capsule cannot run; it {failure}"),
            Some(with_allow(grant)),
        );
    }

    if let Some(homes) = facts.rustup.as_ref().filter(|_| !facts.unconfined) {
        let rustup_said = lower.contains("rustup could not choose a version")
            || lower.contains("no default toolchain")
            || lower.contains("toolchain '")
            || lower.contains("rustup");
        if !(homes.rustup_home_lent && homes.cargo_home_lent) || rustup_said {
            let mut missing = Vec::new();
            if !homes.rustup_home_lent {
                missing.push(format!("RUSTUP_HOME ({})", homes.rustup_home.display()));
            }
            if !homes.cargo_home_lent {
                missing.push(format!("CARGO_HOME ({})", homes.cargo_home.display()));
            }
            if !missing.is_empty() {
                let entries = [
                    (homes.rustup_home_lent, &homes.rustup_home),
                    (homes.cargo_home_lent, &homes.cargo_home),
                ]
                .iter()
                .filter(|(lent, _)| !lent)
                .map(|(_, path)| format!("{{path: {}, access: read}}", path.display()))
                .collect::<Vec<_>>()
                .join(", ");
                return Classified::refused(
                    ProbeCause::HostPathNeeded,
                    format!(
                        "{program} is a rustup proxy that reads its toolchains from RUSTUP_HOME \
                         and CARGO_HOME, and no host_paths entry lends {}; it {failure}",
                        missing.join(" or ")
                    ),
                    Some(with_allow(format!(
                        "lend them in ~/.murmur/config.yaml host_paths: [{entries}], and start \
                         mur run with RUSTUP_HOME={} and CARGO_HOME={} set",
                        homes.rustup_home.display(),
                        homes.cargo_home.display()
                    ))),
                );
            }
        }
    }

    let refused_exec = exit_code == Some(126) || lower.contains("permission denied");
    if !facts.unconfined && refused_exec && !facts.interpreter_chain.is_empty() {
        let missing: Vec<&str> = facts
            .interpreter_chain
            .iter()
            .map(String::as_str)
            .filter(|name| *name != program && !facts.shell_allow.iter().any(|a| a == name))
            .collect();
        if !missing.is_empty() {
            let names = missing.join(", ");
            return Classified::refused(
                ProbeCause::InterpreterNotAllowed,
                format!(
                    "{program} is a #! script run by {}, and capabilities.shell.allow does not \
                     name {names}; it {failure}",
                    facts.interpreter_chain.join(" \u{2192} ")
                ),
                Some(with_allow(format!(
                    "add {names} to capabilities.shell.allow"
                ))),
            );
        }
    }

    Classified::refused(ProbeCause::Failed, failure, None)
}

/// The helper program a compile or link step's output says it could not run, if any.
fn missing_helper(lower: &str) -> Option<String> {
    // rustc: "linker `cc` not found" / "linking with `cc` failed".
    for marker in ["linker `", "linking with `"] {
        if let Some(start) = lower.find(marker) {
            let rest = &lower[start + marker.len()..];
            if let Some(end) = rest.find('`') {
                let tail = &rest[end..];
                if lower.contains("not found")
                    || tail.contains("failed")
                    || lower.contains("permission denied")
                {
                    return Some(rest[..end].to_string());
                }
            }
        }
    }
    // gcc: "cannot execute 'cc1': execvp: Permission denied", "collect2: fatal error: cannot
    // find 'ld'".
    let denied = lower.contains("permission denied")
        || lower.contains("cannot execute")
        || lower.contains("cannot find");
    if denied {
        for helper in ["cc1plus", "cc1", "collect2", "ld", "as"] {
            if lower.contains(&format!("'{helper}'"))
                || lower.contains(&format!("{helper}: permission denied"))
            {
                return Some(helper.to_string());
            }
        }
    }
    None
}

/// What [`ProgramProbeRunner::new`] needs: the capsule's policy as a launch here would start
/// from, and what staging derives the rest of its policy from.
#[derive(Debug, Clone)]
pub struct ProbeInputs {
    /// `capability_policy_from_runtime_manifest`, with `host_paths` set as `mur run` sets them.
    pub policy: CapabilityPolicy,
    /// The combined declared floor `mur run` with no flags resolves.
    pub declared_floor: ContainmentClass,
    /// The workdir root a launch would use: the containment decision, `/tmp` mapping and every
    /// staging derivation are made for it, as for that launch. Probes never run in it.
    pub workdir: PathBuf,
    /// Each pinned tool's program downloads, for `derive_pinned_programs`.
    pub pinned_tools: Vec<DeclaredToolDownloads>,
    /// Each pinned tool's declared program needs, for `derive_declared_needs`.
    pub declared_tools: Vec<DeclaredToolNeeds>,
}

/// Why no probe can start.
#[derive(Debug, Clone, PartialEq, Eq)]
struct NoSandbox {
    code: Option<&'static str>,
    reason: String,
    hint: String,
}

impl NoSandbox {
    fn outcome(&self) -> StepOutcome {
        StepOutcome::NoSandbox {
            code: self.code,
            reason: self.reason.clone(),
            hint: self.hint.clone(),
        }
    }
}

/// The scratch folders, scope and runtime directory every probe of one runner shares.
struct Prepared {
    id: String,
    /// `~/.murmur/probes`, held; `<id>/` is removed through it.
    probes: WorkdirRoot,
    work: WorkdirRoot,
    roots: crate::runtime_dir::RuntimeDirRoots,
    scope: Option<Arc<crate::cgroup::CgroupScope>>,
    guard: Option<Arc<crate::resources::WorkdirGuard>>,
    netns: Arc<crate::network_namespace::SessionNetns>,
    /// Dropped after every process is killed, removing `<sessions>/<id>/`.
    runtime_dir: Option<crate::runtime_dir::RuntimeDir>,
}

/// Runs [`ProbeSpec`]s inside the sandbox a launch of one capsule would get on this host. Every
/// process it started is killed, and its scratch folder, runtime directory and cgroup scope
/// removed, when it drops.
pub struct ProgramProbeRunner {
    inputs: ProbeInputs,
    host_probe: HostProbe,
    decision: ContainmentDecision,
    placement: Option<crate::runtime_dir::RuntimeDirPlacement>,
    temp_mapping: crate::temp_dir::TempMapping,
    prepared: Result<Prepared, NoSandbox>,
    /// The first spawn refused for want of a sandbox, which every probe then reports.
    refused: Mutex<Option<NoSandbox>>,
}

impl std::fmt::Debug for ProgramProbeRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProgramProbeRunner")
            .field("decision", &self.decision)
            .finish_non_exhaustive()
    }
}

impl ProgramProbeRunner {
    /// Decides containment from `host_probe` for `inputs`, as `stage_session` does, and prepares
    /// the probes' cgroup scope, scratch folder `~/.murmur/probes/<id>/` (`work/`, and `records/`
    /// holding `home/` and `tmp/`) and runtime directory. A part that cannot be prepared makes
    /// every probe [`ProbeCause::NoSandbox`]; nothing fails here.
    #[must_use]
    pub fn new(mut inputs: ProbeInputs, host_probe: HostProbe) -> Self {
        inputs.policy.containment_floor = inputs.declared_floor;
        let placement = crate::runtime_dir::runtime_dir_placement(&inputs.workdir).ok();
        let decision = ContainmentDecision::decide(
            host_probe.tier(),
            inputs.declared_floor,
            inputs.policy.workdir_exec_allowed,
            placement
                .as_ref()
                .is_some_and(crate::runtime_dir::RuntimeDirPlacement::inside_workdir),
        );
        let temp_mapping = crate::temp_dir::temp_mapping(
            decision.applied_tier(),
            host_probe.mount_namespace_usable(),
            &inputs.workdir,
        );
        let prepared = match &placement {
            Some(placement) => prepare(&inputs, &host_probe, &decision, placement),
            None => Err(NoSandbox {
                code: None,
                reason: "HOME cannot hold ~/.murmur, where a launch keeps its runtime directory"
                    .to_string(),
                hint: SANDBOX_UNPREPARED_GRANT.to_string(),
            }),
        };
        Self {
            inputs,
            host_probe,
            decision,
            placement,
            temp_mapping,
            prepared,
            refused: Mutex::new(None),
        }
    }

    /// The class the probes run under.
    #[must_use]
    pub fn containment(&self) -> ContainmentClass {
        self.decision.achieved()
    }

    /// The tier's mechanism, as `--explain-scope` spells it.
    #[must_use]
    pub fn mechanism(&self) -> &'static str {
        self.decision.enforcement_tier()
    }

    /// Whether the probes run unconfined.
    #[must_use]
    pub fn unconfined(&self) -> bool {
        self.decision.applied_tier() == EnforcementTier::Unconfined
    }

    /// Whether no kernel sandbox restricts which programs a probe starts (the `EnvironmentOnly`
    /// tier of a host with no kernel containment).
    #[must_use]
    pub fn environment_only(&self) -> bool {
        self.decision.applied_tier() == EnforcementTier::EnvironmentOnly
    }

    /// Probes every spec, in order. When a spawn is refused for want of a sandbox, every program
    /// is reported [`ProbeCause::NoSandbox`] with that refusal.
    pub fn report(&self, specs: &[ProbeSpec]) -> ProgramsReport {
        let mut probed: Vec<ProgramProbe> = specs.iter().map(|spec| self.probe(spec)).collect();
        let refused = self
            .refused
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(refusal) = refused {
            for (probe, spec) in probed.iter_mut().zip(specs) {
                if probe.cause != Some(ProbeCause::NoSandbox) {
                    *probe = self.no_sandbox_probe(spec, &refusal);
                }
            }
        }
        ProgramsReport {
            containment: self.containment().as_str().to_string(),
            mechanism: self.mechanism().to_string(),
            path: self.probe_path(&self.inputs.policy),
            probed,
        }
    }

    /// Probes `spec.program` under the capsule's policy with the program added to
    /// `capabilities.shell.allow`, step by step until one does not exit 0, and classifies what
    /// the deciding step did.
    pub fn probe(&self, spec: &ProbeSpec) -> ProgramProbe {
        if let Err(refusal) = &self.prepared {
            return self.no_sandbox_probe(spec, refusal);
        }
        if let Some(refusal) = self
            .refused
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            return self.no_sandbox_probe(spec, &refusal);
        }
        let policy = self.probe_policy(&spec.program);
        let facts = self.static_facts(&spec.program, &policy);
        let first_argv = spec
            .steps
            .first()
            .map(|step| step.argv.clone())
            .unwrap_or_else(|| vec![spec.program.clone()]);
        if facts.resolved.is_none() {
            return self.finish(spec, &facts, first_argv, None);
        }
        let enforcement = match self.enforcement(&policy) {
            Ok(enforcement) => enforcement,
            Err(refusal) => return self.no_sandbox_probe(spec, &refusal),
        };
        let mut deciding: Option<(Vec<String>, StepOutcome)> = None;
        for step in &spec.steps {
            let outcome = self.run_step(step, &policy, &enforcement);
            let succeeded = matches!(outcome, StepOutcome::Exited { exit_code: 0, .. });
            deciding = Some((step.argv.clone(), outcome));
            if !succeeded {
                break;
            }
        }
        // Whatever a step left running is killed before the next program is probed.
        enforcement.kill_session_processes();
        match deciding {
            Some((_, StepOutcome::NoSandbox { code, reason, hint })) => {
                let refusal = NoSandbox { code, reason, hint };
                *self
                    .refused
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(refusal.clone());
                self.no_sandbox_probe(spec, &refusal)
            }
            Some((argv, outcome)) => {
                let mut facts = facts;
                facts.missing_library = missing_library(&outcome);
                self.finish(spec, &facts, argv, Some(outcome))
            }
            None => self.finish(spec, &facts, first_argv, None),
        }
    }

    fn finish(
        &self,
        spec: &ProbeSpec,
        facts: &StaticFacts,
        argv: Vec<String>,
        outcome: Option<StepOutcome>,
    ) -> ProgramProbe {
        let classified = classify(facts, outcome.as_ref());
        let (exit_code, detail) = match &outcome {
            Some(StepOutcome::Exited {
                exit_code, stderr, ..
            }) => (Some(*exit_code), tail_lines(stderr)),
            Some(StepOutcome::NotStarted { message }) => (None, tail_lines(message)),
            Some(StepOutcome::TimedOut { stderr, .. }) => (None, tail_lines(stderr)),
            Some(StepOutcome::NoSandbox { .. }) | None => (None, String::new()),
        };
        let note = (facts.resolved.is_some() && NETWORK_PROGRAMS.contains(&spec.program.as_str()))
            .then(|| self.network_note())
            .flatten();
        ProgramProbe {
            program: spec.program.clone(),
            status: classified.status,
            in_shell_allow: facts.in_shell_allow,
            resolved: facts
                .resolved
                .as_ref()
                .map(|path| path.display().to_string()),
            probe: argv,
            exit_code,
            cause: classified.cause,
            reason: classified.reason,
            grant: classified.grant,
            note,
            detail,
        }
    }

    fn no_sandbox_probe(&self, spec: &ProbeSpec, refusal: &NoSandbox) -> ProgramProbe {
        let facts = StaticFacts {
            program: spec.program.clone(),
            in_shell_allow: self.in_shell_allow(&spec.program),
            ..StaticFacts::default()
        };
        let argv = spec
            .steps
            .first()
            .map(|step| step.argv.clone())
            .unwrap_or_default();
        let mut probe = self.finish(spec, &facts, argv, Some(refusal.outcome()));
        probe.resolved = self
            .resolve_on_path(&spec.program, &self.inputs.policy)
            .map(|path| path.display().to_string());
        probe
    }

    fn network_note(&self) -> Option<String> {
        if self.unconfined() || self.environment_only() {
            return None;
        }
        let allow = &self.inputs.policy.network_allow;
        Some(if allow.is_empty() {
            "reaches only hosts in capabilities.network.allow (none declared)".to_string()
        } else {
            format!(
                "reaches only hosts in capabilities.network.allow ({})",
                allow.join(", ")
            )
        })
    }

    fn in_shell_allow(&self, program: &str) -> bool {
        self.inputs
            .policy
            .shell_allow
            .iter()
            .any(|allowed| allowed == program)
    }

    /// The capsule's policy with `program` in `shell.allow`, and every staging-time field derived
    /// from it through the functions `stage_session` calls, in its order.
    fn probe_policy(&self, program: &str) -> CapabilityPolicy {
        let mut policy = self.inputs.policy.clone();
        if !policy.shell_allow.iter().any(|allowed| allowed == program) {
            policy.shell_allow.push(program.to_string());
        }
        let Some(placement) = &self.placement else {
            return policy;
        };
        let writable = crate::interpreter_runtime::capsule_writable_roots(placement);
        let interpreter_runtimes = crate::interpreter_runtime::derive_interpreter_runtimes(
            &policy.shell_allow,
            &policy.shell_staged_runtime,
            &writable,
            &crate::host_facts::HostFactsCache::in_murmur_home(),
        );
        policy.shell_derived_interpreter_runtime = interpreter_runtimes.runtimes;
        policy.shell_trust_store = crate::trust_store::derive_trust_store(
            &policy.shell_allow,
            &policy.network_allow,
            &writable,
        );
        policy.shell_pinned_programs = crate::pinned_programs::derive_pinned_programs(
            &self.inputs.pinned_tools,
            &policy.shell_allow,
        );
        policy.shell_declared_needs = crate::declared_needs::derive_declared_needs(
            &self.inputs.declared_tools,
            &policy.shell_allow,
            &writable,
            &policy.shell_pinned_programs,
        );
        policy
    }

    /// `policy`'s enforcement on the runner's decision, scope, runtime directory, temporary
    /// mapping and network namespace.
    fn enforcement(&self, policy: &CapabilityPolicy) -> Result<ShellEnforcement, NoSandbox> {
        let prepared = self.prepared.as_ref().map_err(std::clone::Clone::clone)?;
        if let Err(refusal) = crate::host_paths::check_policy_execute_roots(
            policy,
            crate::sandbox::host_path_mechanism(self.decision.applied_tier()),
        ) {
            return Err(NoSandbox {
                code: Some("E-CFG-004"),
                reason: refusal.to_string(),
                hint: "fix or remove that host_paths entry in ~/.murmur/config.yaml".to_string(),
            });
        }
        let enforcement =
            ShellEnforcement::resolve(policy, &self.decision, self.host_probe.clone()).map_err(
                |reason| NoSandbox {
                    code: None,
                    reason: format!("the capsule's enforcement could not be resolved: {reason}"),
                    hint: SANDBOX_UNPREPARED_GRANT.to_string(),
                },
            )?;
        Ok(enforcement
            .with_host_bounding(prepared.scope.clone(), prepared.guard.clone())
            .with_runtime_dir(prepared.roots.clone(), None)
            .with_temp_mapping(self.temp_mapping)
            .with_session_netns(Arc::clone(&prepared.netns)))
    }

    /// The `PATH` a command spawned under `policy` gets.
    fn probe_path(&self, policy: &CapabilityPolicy) -> String {
        let env = self.probe_env(policy);
        env.get(&OsString::from("PATH"))
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// The environment a command spawned under `policy` gets, from [`crate::shell::subprocess_env`]
    /// where an enforcement can be built, else this process's own with the pinned folders
    /// appended to `PATH`.
    fn probe_env(&self, policy: &CapabilityPolicy) -> BTreeMap<OsString, OsString> {
        if let Ok(enforcement) = self.enforcement(policy) {
            if let Ok(env) = crate::shell::subprocess_env(policy, &[], &enforcement) {
                return env;
            }
        }
        let mut env: BTreeMap<OsString, OsString> = std::env::vars_os().collect();
        let _ = crate::shell::append_pinned_program_path(&mut env, policy);
        env
    }

    fn resolve_on_path(&self, program: &str, policy: &CapabilityPolicy) -> Option<PathBuf> {
        let path = self.probe_path(policy);
        path_matches(program, &path).into_iter().next()
    }

    fn static_facts(&self, program: &str, policy: &CapabilityPolicy) -> StaticFacts {
        let env = self.probe_env(policy);
        let path = env
            .get(&OsString::from("PATH"))
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default();
        let matches = path_matches(program, &path);
        let resolved = matches.first().cloned();
        let canonical = resolved
            .as_ref()
            .and_then(|path| std::fs::canonicalize(path).ok());
        let snap_launcher = resolved.as_deref().and_then(snap_launcher_of);
        let non_snap_copy = matches
            .iter()
            .skip(1)
            .find(|candidate| snap_launcher_of(candidate).is_none())
            .cloned();
        let rustup = resolved
            .as_ref()
            .filter(|path| is_rustup_proxy(path))
            .map(|_| self.rustup_homes(&env, policy));
        let interpreter_chain = resolved
            .as_deref()
            .map(interpreter_chain)
            .unwrap_or_default();
        StaticFacts {
            program: program.to_string(),
            in_shell_allow: self.in_shell_allow(program),
            shell_allow: self.inputs.policy.shell_allow.clone(),
            resolved,
            canonical,
            snap_launcher,
            non_snap_copy,
            rustup,
            interpreter_chain,
            network_allow: self.inputs.policy.network_allow.clone(),
            unconfined: self.unconfined(),
            sealed: self.decision.applied_tier() == EnforcementTier::KernelSealed,
            missing_library: None,
        }
    }

    /// Where a rustup proxy run under `env` reads its toolchains: `RUSTUP_HOME` and `CARGO_HOME`
    /// from `env`, else `~/.rustup` and `~/.cargo` of the operator's own `HOME`.
    fn rustup_homes(
        &self,
        env: &BTreeMap<OsString, OsString>,
        policy: &CapabilityPolicy,
    ) -> RustupHomes {
        let operator_home = std::env::var_os("HOME").map(PathBuf::from);
        let home_of = |variable: &str, default: &str| {
            env.get(&OsString::from(variable))
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .or_else(|| operator_home.as_ref().map(|home| home.join(default)))
                .unwrap_or_else(|| PathBuf::from(default))
        };
        let rustup_home = home_of("RUSTUP_HOME", ".rustup");
        let cargo_home = home_of("CARGO_HOME", ".cargo");
        let lent = |dir: &Path| {
            let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
            policy
                .host_paths
                .iter()
                .any(|host_path| dir.starts_with(&host_path.path))
        };
        RustupHomes {
            rustup_home_lent: lent(&rustup_home),
            cargo_home_lent: lent(&cargo_home),
            toolchain_dir: default_toolchain(&rustup_home)
                .map(|toolchain| rustup_home.join("toolchains").join(toolchain)),
            rustup_home,
            cargo_home,
        }
    }

    fn run_step(
        &self,
        step: &ProbeStep,
        policy: &CapabilityPolicy,
        enforcement: &ShellEnforcement,
    ) -> StepOutcome {
        let Ok(prepared) = self.prepared.as_ref() else {
            return StepOutcome::NotStarted {
                message: "no sandbox".to_string(),
            };
        };
        for file in &step.files {
            if let Err(error) = prepared.work.write_file_exact_mode(
                &file.path,
                file.contents.as_bytes(),
                crate::murmur_home::PRIVATE_FILE_MODE,
            ) {
                return StepOutcome::NotStarted {
                    message: format!("could not write the probe's {}: {error}", file.path),
                };
            }
        }
        let Some((binary, args)) = step.argv.split_first() else {
            return StepOutcome::NotStarted {
                message: "the probe step is empty".to_string(),
            };
        };
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let stop = crate::shell::ForegroundStop {
            canceled: crate::cancel::CancelSignal::new(),
            timeout: step.timeout,
        };
        match crate::shell::run_shell_until_stopped(
            binary,
            &args,
            &[],
            &prepared.work,
            policy,
            enforcement,
            stop,
        ) {
            Ok(run) => match run.stopped {
                Some(_) => StepOutcome::TimedOut {
                    timeout: step.timeout,
                    stderr: run.result.stderr,
                },
                None => StepOutcome::Exited {
                    exit_code: run.result.exit_code,
                    stdout: run.result.stdout,
                    stderr: run.result.stderr,
                },
            },
            Err(crate::shell::ShellExecError::SealedRootConstructionFailed { detail }) => {
                StepOutcome::NoSandbox {
                    code: Some(E_RUN_014),
                    reason: format!("the sealed composed root could not be built: {detail}"),
                    hint: SANDBOX_UNPREPARED_GRANT.to_string(),
                }
            }
            Err(crate::shell::ShellExecError::Failed(message)) => spawn_failure(message),
        }
    }
}

/// A failed spawn: a refusal to prepare the sandbox (`sandbox: …`, an `E-CAP-005` namespace
/// creation among them) is [`StepOutcome::NoSandbox`]; anything else, `execve` refusing the
/// program, is [`StepOutcome::NotStarted`].
fn spawn_failure(message: String) -> StepOutcome {
    if message.contains(E_CAP_005) {
        return StepOutcome::NoSandbox {
            code: Some(E_CAP_005),
            reason: message
                .trim_start_matches("sandbox: ")
                .trim_start_matches("E-CAP-005: ")
                .to_string(),
            hint: E_CAP_005_GRANT.to_string(),
        };
    }
    if message.starts_with("sandbox:") {
        return StepOutcome::NoSandbox {
            code: None,
            reason: message,
            hint: SANDBOX_UNPREPARED_GRANT.to_string(),
        };
    }
    StepOutcome::NotStarted { message }
}

impl Drop for ProgramProbeRunner {
    /// Kills every process the probes started and ends their network namespace, then removes the
    /// runtime directory, the scratch folder and the cgroup scope.
    fn drop(&mut self) {
        let Ok(prepared) = &mut self.prepared else {
            return;
        };
        // `ShellEnforcement::end_session`'s two halves, on the parts every probe's enforcement
        // shared: the scope's processes killed and waited for, then the namespace ended.
        if let Some(scope) = &prepared.scope {
            let _ = scope.kill_all();
            scope.wait_until_empty(crate::sandbox::SESSION_END_KILL_WAIT);
        }
        prepared.netns.end();
        prepared.guard = None;
        prepared.runtime_dir = None;
        if let Err(error) = prepared.probes.remove_dir_all(&prepared.id) {
            if !error.is_not_found() {
                crate::runtime_err!(
                    "[capsule-runtime] warning: failed to remove the probe folder {}: {error}",
                    prepared.probes.host_path(&prepared.id).display()
                );
            }
        }
        prepared.scope = None;
    }
}

/// The cgroup scope, scratch folder and runtime directory for `inputs`' probes, created in the
/// order `CheckRunner` creates a check's.
fn prepare(
    inputs: &ProbeInputs,
    host_probe: &HostProbe,
    decision: &ContainmentDecision,
    placement: &crate::runtime_dir::RuntimeDirPlacement,
) -> Result<Prepared, NoSandbox> {
    let unprepared = |reason: String| NoSandbox {
        code: None,
        reason,
        hint: SANDBOX_UNPREPARED_GRANT.to_string(),
    };
    let contained = decision.applied_tier() != EnforcementTier::Unconfined;
    if let Err(error) = crate::network_namespace::check_egress_namespace(
        contained,
        host_probe.egress_namespace_failure().as_ref(),
    ) {
        return Err(NoSandbox {
            code: Some(E_CAP_005),
            reason: error.to_string(),
            hint: E_CAP_005_GRANT.to_string(),
        });
    }
    let id = format!(
        "probe-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos())
    );

    // Every probe is a subprocess, so the scope is decided as for a capsule that runs one.
    let mut scope_policy = inputs.policy.clone();
    if !crate::runtime::has_subprocess_route_at_staging(&scope_policy) {
        scope_policy.shell_allow.push("sh".to_string());
    }

    let home = crate::murmur_home::ensure_murmur_home().map_err(unprepared)?;
    let probes_path = home.join(PROBES_DIR_NAME);
    crate::murmur_home::hold_private_dir(&probes_path)
        .map_err(|reason| unprepared(format!("{}: {reason}", probes_path.display())))?;
    let probes = WorkdirRoot::open(&probes_path).map_err(|error| unprepared(error.to_string()))?;
    let mode = crate::runtime_dir::PRIVATE_DIR_MODE;
    for sub in [
        format!("{id}/work"),
        format!("{id}/records/{}", crate::records_dir::HOME_SUBDIR),
        format!("{id}/records/{}", crate::temp_dir::SESSION_TEMP_DIR_NAME),
    ] {
        if let Err(error) = probes.create_dir_all(&sub, mode) {
            let _ = probes.remove_dir_all(&id);
            return Err(unprepared(error.to_string()));
        }
    }
    let opened = (|| {
        let work = probes
            .open_beneath(&format!("{id}/work"))?
            .ok_or_else(|| missing(&probes_path, &id, "work"))?;
        let records = probes
            .open_beneath(&format!("{id}/records"))?
            .ok_or_else(|| missing(&probes_path, &id, "records"))?;
        let home = records
            .open_beneath(crate::records_dir::HOME_SUBDIR)?
            .ok_or_else(|| missing(&probes_path, &id, "records/home"))?;
        let temp = records
            .open_beneath(crate::temp_dir::SESSION_TEMP_DIR_NAME)?
            .ok_or_else(|| missing(&probes_path, &id, "records/tmp"))?;
        Ok::<_, crate::workdir_fs::WorkdirError>((work, records, home, temp))
    })();
    let (work, records, home_root, temp) = match opened {
        Ok(opened) => opened,
        Err(error) => {
            let _ = probes.remove_dir_all(&id);
            return Err(unprepared(error.to_string()));
        }
    };

    let cleanup = |probes: &WorkdirRoot| {
        let _ = probes.remove_dir_all(&id);
    };
    let scope = match crate::runtime::prepare_launch_scope(&scope_policy, &id, work.path()) {
        Ok(prepared) => prepared.scope,
        Err(error) => {
            cleanup(&probes);
            return Err(NoSandbox {
                code: Some(E_RUN_012),
                reason: error.to_string(),
                hint: E_RUN_012_GRANT.to_string(),
            });
        }
    };
    let guard = crate::resources::workdir_guard_max_bytes(&inputs.policy).map(|max_bytes| {
        crate::resources::WorkdirGuard::spawn(
            &work,
            Some(&temp),
            Some(&home_root),
            Some(&records),
            max_bytes,
        )
    });
    let runtime_dir =
        match crate::runtime_dir::RuntimeDir::create(&placement.sessions_dir, &id, &work) {
            Ok(runtime_dir) => runtime_dir,
            Err(reason) => {
                cleanup(&probes);
                return Err(unprepared(format!(
                    "the probes' runtime directory: {reason}"
                )));
            }
        };
    let roots = runtime_dir
        .roots()
        .clone()
        .with_records(records)
        .with_temp(temp)
        .with_home(home_root);
    Ok(Prepared {
        id,
        probes,
        work,
        roots,
        scope,
        guard,
        netns: Arc::new(crate::network_namespace::SessionNetns::new()),
        runtime_dir: Some(runtime_dir),
    })
}

fn missing(probes: &Path, id: &str, sub: &str) -> crate::workdir_fs::WorkdirError {
    crate::workdir_fs::WorkdirError::Io {
        path: probes.join(id).join(sub).display().to_string(),
        source: std::io::Error::new(std::io::ErrorKind::NotFound, "vanished after its creation"),
    }
}

/// Every executable file named `program` on `path`, in `PATH` order, as found there.
fn path_matches(program: &str, path: &str) -> Vec<PathBuf> {
    if program.contains('/') {
        let candidate = PathBuf::from(program);
        return if crate::sandbox::is_executable_file(&candidate) {
            vec![candidate]
        } else {
            Vec::new()
        };
    }
    let mut matches: Vec<PathBuf> = Vec::new();
    for dir in std::env::split_paths(path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join(program);
        if crate::sandbox::is_executable_file(&candidate) && !matches.contains(&candidate) {
            matches.push(candidate);
        }
    }
    matches
}

/// The shared library a failed step's output says the dynamic loader could not find, and the
/// first standard library folder on this host holding it.
fn missing_library(outcome: &StepOutcome) -> Option<(String, PathBuf)> {
    let StepOutcome::Exited { stderr, .. } = outcome else {
        return None;
    };
    const MARKER: &str = "error while loading shared libraries: ";
    let rest = &stderr[stderr.find(MARKER)? + MARKER.len()..];
    let library = rest.split(':').next()?.trim().to_string();
    if library.is_empty() || library.contains('/') {
        return None;
    }
    crate::sandbox::DEFAULT_LIBRARY_SEARCH_DIRS
        .iter()
        .map(PathBuf::from)
        .find(|dir| dir.join(&library).exists())
        .map(|dir| (library, std::fs::canonicalize(&dir).unwrap_or(dir)))
}

/// The `default_toolchain` rustup's `settings.toml` in `rustup_home` names.
fn default_toolchain(rustup_home: &Path) -> Option<String> {
    let settings = std::fs::read_to_string(rustup_home.join("settings.toml")).ok()?;
    settings.lines().find_map(|line| {
        let value = line.trim().strip_prefix("default_toolchain")?.trim_start();
        let value = value.strip_prefix('=')?.trim().trim_matches('"');
        (!value.is_empty() && !value.contains('/')).then(|| value.to_string())
    })
}

/// The first hop named `snap` or `snapd` on the link chain from `path`, `path` itself included.
fn snap_launcher_of(path: &Path) -> Option<PathBuf> {
    let is_snap = |hop: &Path| {
        hop.file_name()
            .is_some_and(|name| name == "snap" || name == "snapd")
    };
    let mut hop = path.to_path_buf();
    // A bound on the walk, so a link cycle ends it.
    for _ in 0..16 {
        if is_snap(&hop) {
            return Some(hop);
        }
        let Ok(target) = std::fs::read_link(&hop) else {
            return None;
        };
        hop = match hop.parent() {
            Some(dir) if target.is_relative() => dir.join(target),
            _ => target,
        };
    }
    None
}

/// Whether `path` is a rustup proxy: the same file as the `rustup` beside it, or a file named
/// `rustup` once every link is followed.
fn is_rustup_proxy(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    let Ok(canonical) = std::fs::canonicalize(path) else {
        return false;
    };
    if canonical.file_name().is_some_and(|name| name == "rustup") {
        return true;
    }
    let Some(rustup) = path.parent().map(|dir| dir.join("rustup")) else {
        return false;
    };
    match (std::fs::metadata(path), std::fs::metadata(rustup)) {
        (Ok(program), Ok(rustup)) => program.dev() == rustup.dev() && program.ino() == rustup.ino(),
        _ => false,
    }
}

/// The `#!` line's program and, through `env`, the interpreter it execs.
fn interpreter_chain(path: &Path) -> Vec<String> {
    let Some(line) = crate::reachability::shebang_line(path, 256) else {
        return Vec::new();
    };
    let mut tokens = line.split_whitespace();
    let Some(first) = tokens.next().and_then(base_name) else {
        return Vec::new();
    };
    let mut chain = vec![first.clone()];
    if first == "env" {
        if let Some(interpreter) = tokens
            .filter(|token| !token.starts_with('-') && !token.contains('='))
            .find_map(base_name)
        {
            chain.push(interpreter);
        }
    }
    chain
}

fn base_name(token: &str) -> Option<String> {
    Path::new(token)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(program: &str) -> StaticFacts {
        StaticFacts {
            program: program.to_string(),
            in_shell_allow: true,
            shell_allow: vec![program.to_string()],
            resolved: Some(PathBuf::from(format!("/usr/bin/{program}"))),
            canonical: Some(PathBuf::from(format!("/usr/bin/{program}"))),
            ..StaticFacts::default()
        }
    }

    fn exited(exit_code: i32, stderr: &str) -> StepOutcome {
        StepOutcome::Exited {
            exit_code,
            stdout: String::new(),
            stderr: stderr.to_string(),
        }
    }

    fn cause_of(facts: &StaticFacts, outcome: Option<&StepOutcome>) -> Classified {
        classify(facts, outcome)
    }

    #[test]
    fn exit_zero_always_runs_whatever_stderr_says() {
        for stderr in [
            "",
            "unshare failed: Operation not permitted",
            "error: linker `cc` not found",
            "sudo: effective uid is not 0",
        ] {
            let classified = cause_of(&facts("unshare"), Some(&exited(0, stderr)));
            assert_eq!(classified, Classified::runs(), "{stderr}");
        }
    }

    #[test]
    fn not_on_path_is_not_installed_with_the_install_grant() {
        let mut facts = facts("jq-not-a-real-binary");
        facts.resolved = None;
        facts.canonical = None;
        let classified = cause_of(&facts, None);
        assert_eq!(classified.status, ProbeStatus::Refused);
        assert_eq!(classified.cause, Some(ProbeCause::NotInstalled));
        assert_eq!(
            classified.grant.as_deref(),
            Some("install jq-not-a-real-binary on this host so it is on mur run's PATH")
        );
    }

    #[test]
    fn a_program_that_runs_only_once_added_is_not_in_shell_allow() {
        let mut facts = facts("curl");
        facts.in_shell_allow = false;
        facts.shell_allow = vec!["bash".to_string()];
        let classified = cause_of(&facts, Some(&exited(0, "")));
        assert_eq!(classified.cause, Some(ProbeCause::NotInShellAllow));
        assert_eq!(
            classified.grant.as_deref(),
            Some("add curl to capabilities.shell.allow")
        );
    }

    #[test]
    fn the_snap_launcher_names_the_copy_later_on_path() {
        let mut facts = facts("curl");
        facts.resolved = Some(PathBuf::from("/snap/bin/curl"));
        facts.canonical = Some(PathBuf::from("/usr/lib/snapd/snapd"));
        facts.snap_launcher = Some(PathBuf::from("/usr/bin/snap"));
        facts.non_snap_copy = Some(PathBuf::from("/usr/bin/curl"));
        let classified = cause_of(
            &facts,
            Some(&exited(1, "cannot create user data directory")),
        );
        assert_eq!(classified.cause, Some(ProbeCause::SnapWrapper));
        let reason = classified.reason.unwrap();
        assert!(reason.contains("/snap/bin/curl"), "{reason}");
        assert!(reason.contains("/usr/bin/snap"), "{reason}");
        assert_eq!(
            classified.grant.as_deref(),
            Some(
                "put /usr/bin ahead of /snap/bin on mur run's PATH, so curl resolves to \
                 /usr/bin/curl"
            )
        );

        facts.non_snap_copy = None;
        let classified = cause_of(&facts, Some(&exited(1, "")));
        assert_eq!(
            classified.grant.as_deref(),
            Some("install curl from the distribution's package")
        );
    }

    #[test]
    fn the_snap_launcher_is_found_on_the_link_chain() {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path().join("lib");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(lib.join("snapd"), "").unwrap();
        std::os::unix::fs::symlink("lib/snapd", dir.path().join("snap")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("snap"), dir.path().join("curl")).unwrap();
        std::fs::write(dir.path().join("real-curl"), "").unwrap();
        assert_eq!(
            snap_launcher_of(&dir.path().join("curl")),
            Some(dir.path().join("snap"))
        );
        assert_eq!(snap_launcher_of(&dir.path().join("real-curl")), None);
    }

    #[test]
    fn a_refused_unshare_is_the_seccomp_filter_and_only_unconfined_lifts_it() {
        let outcome = exited(1, "unshare: unshare failed: Operation not permitted\n");
        let classified = cause_of(&facts("unshare"), Some(&outcome));
        assert_eq!(classified.cause, Some(ProbeCause::NamespacesRefused));
        assert!(classified.reason.unwrap().contains("seccomp filter"));
        assert_eq!(classified.grant.as_deref(), Some(UNCONFINED_ONLY_GRANT));

        // Unconfined, nothing refused it: the same failure is the program's own.
        let mut unconfined = facts("unshare");
        unconfined.unconfined = true;
        let classified = cause_of(&unconfined, Some(&outcome));
        assert_eq!(classified.cause, Some(ProbeCause::Failed));
        assert_eq!(classified.grant, None);
    }

    #[test]
    fn sudo_under_no_new_privs_is_no_new_privileges() {
        let outcome = exited(
            1,
            "sudo: The \"no new privileges\" flag is set, which prevents sudo from running as root.",
        );
        let classified = cause_of(&facts("sudo"), Some(&outcome));
        assert_eq!(classified.cause, Some(ProbeCause::NoNewPrivileges));
        assert_eq!(classified.grant.as_deref(), Some(UNCONFINED_ONLY_GRANT));

        let classified = cause_of(
            &facts("sudo"),
            Some(&exited(1, "sudo: effective uid is not 0, is /usr/bin/sudo on a file system with the 'nosuid' option set")),
        );
        assert_eq!(classified.cause, Some(ProbeCause::NoNewPrivileges));

        let mut unconfined = facts("sudo");
        unconfined.unconfined = true;
        let classified = cause_of(&unconfined, Some(&outcome));
        assert_eq!(classified.cause, Some(ProbeCause::Failed));
    }

    #[test]
    fn a_rustup_proxy_without_its_homes_lent_needs_host_paths() {
        let mut facts = facts("cargo");
        facts.in_shell_allow = false;
        facts.shell_allow = vec!["bash".to_string()];
        facts.resolved = Some(PathBuf::from("/home/u/.cargo/bin/cargo"));
        facts.canonical = Some(PathBuf::from("/home/u/.cargo/bin/rustup"));
        facts.rustup = Some(RustupHomes {
            rustup_home: PathBuf::from("/home/u/.rustup"),
            cargo_home: PathBuf::from("/home/u/.cargo"),
            rustup_home_lent: false,
            cargo_home_lent: false,
            toolchain_dir: None,
        });
        let outcome = exited(
            1,
            "error: rustup could not choose a version of cargo to run, because one wasn't \
             specified explicitly, and no default is configured.",
        );
        let classified = cause_of(&facts, Some(&outcome));
        assert_eq!(classified.cause, Some(ProbeCause::HostPathNeeded));
        let reason = classified.reason.unwrap();
        assert!(reason.contains("RUSTUP_HOME (/home/u/.rustup)"), "{reason}");
        assert!(reason.contains("CARGO_HOME (/home/u/.cargo)"), "{reason}");
        let grant = classified.grant.unwrap();
        assert!(
            grant.contains(
                "{path: /home/u/.rustup, access: read}, {path: /home/u/.cargo, access: read}"
            ),
            "{grant}"
        );
        assert!(
            grant.contains("RUSTUP_HOME=/home/u/.rustup and CARGO_HOME=/home/u/.cargo set"),
            "{grant}"
        );
        assert!(
            grant.ends_with("and add cargo to capabilities.shell.allow"),
            "{grant}"
        );

        // Both lent, and the failure is something else: not put down to the homes.
        if let Some(homes) = facts.rustup.as_mut() {
            homes.rustup_home_lent = true;
            homes.cargo_home_lent = true;
        }
        let classified = cause_of(&facts, Some(&exited(101, "error: could not compile")));
        assert_eq!(classified.cause, Some(ProbeCause::Failed));
    }

    #[test]
    fn an_env_node_script_needs_its_interpreters_in_shell_allow() {
        let mut facts = facts("npm");
        facts.resolved = Some(PathBuf::from("/home/u/.nvm/bin/npm"));
        facts.interpreter_chain = vec!["env".to_string(), "node".to_string()];
        let classified = cause_of(
            &facts,
            Some(&StepOutcome::NotStarted {
                message: "Permission denied (os error 13)".to_string(),
            }),
        );
        assert_eq!(classified.cause, Some(ProbeCause::InterpreterNotAllowed));
        assert_eq!(
            classified.grant.as_deref(),
            Some("add env, node to capabilities.shell.allow")
        );
        let classified = cause_of(&facts, Some(&exited(126, "")));
        assert_eq!(classified.cause, Some(ProbeCause::InterpreterNotAllowed));

        // With the chain allowed, the same failure names no interpreter.
        facts.shell_allow = vec!["npm".into(), "env".into(), "node".into()];
        let classified = cause_of(&facts, Some(&exited(126, "")));
        assert_eq!(classified.cause, Some(ProbeCause::Failed));
    }

    #[test]
    fn a_program_or_library_outside_every_grant_needs_interpreter_runtime() {
        let mut cargo = facts("cargo");
        cargo.rustup = Some(RustupHomes {
            rustup_home: PathBuf::from("/home/u/.rustup"),
            cargo_home: PathBuf::from("/home/u/.cargo"),
            rustup_home_lent: true,
            cargo_home_lent: true,
            toolchain_dir: Some(PathBuf::from("/home/u/.rustup/toolchains/stable")),
        });
        let classified = cause_of(
            &cargo,
            Some(&exited(
                1,
                "error: command failed: 'cargo': Permission denied (os error 13)",
            )),
        );
        assert_eq!(classified.cause, Some(ProbeCause::InterpreterRuntimeNeeded));
        assert_eq!(
            classified.grant.as_deref(),
            Some(
                "declare capabilities.shell.interpreter_runtime: [{binary: cargo, dirs: [{path: \
                 /home/u/.rustup/toolchains/stable, list_dir: true}]}]"
            )
        );

        cargo.missing_library = Some((
            "libz.so.1".to_string(),
            PathBuf::from("/usr/lib/x86_64-linux-gnu"),
        ));
        let classified = cause_of(
            &cargo,
            Some(&exited(
                101,
                "rustc: error while loading shared libraries: libz.so.1: cannot open shared \
                 object file: No such file or directory",
            )),
        );
        assert_eq!(classified.cause, Some(ProbeCause::InterpreterRuntimeNeeded));
        let grant = classified.grant.unwrap();
        assert!(
            grant.contains("{path: /usr/lib/x86_64-linux-gnu, list_dir: false}"),
            "{grant}"
        );
        assert!(missing_library(&exited(
            127,
            "x: error while loading shared libraries: libc.so.6: cannot open shared object file"
        ))
        .is_some_and(|(library, _)| library == "libc.so.6"));
    }

    #[test]
    fn a_missing_linker_or_compiler_helper_is_helper_not_allowed() {
        for (program, stderr, helper) in [
            ("cargo", "error: linker `cc` not found\n  |\n  = note: No such file or directory (os error 2)", "cc"),
            ("cargo", "error: linking with `cc` failed: exit status: 1", "cc"),
            ("cc", "cc: fatal error: cannot execute 'cc1': execvp: Permission denied", "cc1"),
            ("cc", "collect2: fatal error: cannot find 'ld'", "ld"),
        ] {
            let classified = cause_of(&facts(program), Some(&exited(1, stderr)));
            assert_eq!(classified.cause, Some(ProbeCause::HelperNotAllowed), "{stderr}");
            assert_eq!(
                classified.grant.as_deref(),
                Some(format!("add {helper} to capabilities.shell.allow").as_str()),
                "{stderr}"
            );
        }
    }

    #[test]
    fn no_sandbox_carries_the_code_and_its_hint() {
        for (code, hint) in [(E_CAP_005, E_CAP_005_GRANT), (E_RUN_012, E_RUN_012_GRANT)] {
            let outcome = StepOutcome::NoSandbox {
                code: Some(code),
                reason: "the host refused".to_string(),
                hint: hint.to_string(),
            };
            let classified = cause_of(&facts("bash"), Some(&outcome));
            assert_eq!(classified.cause, Some(ProbeCause::NoSandbox));
            assert_eq!(
                classified.reason.as_deref(),
                Some(format!("{code}: the host refused").as_str())
            );
            assert_eq!(classified.grant.as_deref(), Some(hint));
        }
    }

    #[test]
    fn a_step_past_its_timeout_is_timed_out_with_no_grant() {
        let outcome = StepOutcome::TimedOut {
            timeout: Duration::from_secs(30),
            stderr: String::new(),
        };
        let classified = cause_of(&facts("make"), Some(&outcome));
        assert_eq!(classified.cause, Some(ProbeCause::TimedOut));
        assert_eq!(classified.grant, None);
    }

    #[test]
    fn anything_else_failed_keeps_its_detail_and_no_grant() {
        let classified = cause_of(&facts("make"), Some(&exited(2, "make: *** oops")));
        assert_eq!(classified.cause, Some(ProbeCause::Failed));
        assert_eq!(classified.reason.as_deref(), Some("exited 2"));
        assert_eq!(classified.grant, None);
        assert_eq!(tail_lines("a\n\n b \nc\nd\n"), "b\nc\nd");
        let long = "x".repeat(DETAIL_LINE_CHARS + 5);
        assert_eq!(
            tail_lines(&long),
            format!("{}\u{2026}", "x".repeat(DETAIL_LINE_CHARS))
        );
    }

    #[test]
    fn a_spawn_refused_for_its_sandbox_is_no_sandbox() {
        let refused = spawn_failure(
            "sandbox: E-CAP-005: this host cannot give the capsule's subprocess tree its own \
             network namespace"
                .to_string(),
        );
        assert!(
            matches!(&refused, StepOutcome::NoSandbox { code: Some(E_CAP_005), reason, .. } if reason.starts_with("this host")),
            "{refused:?}"
        );
        assert!(matches!(
            spawn_failure("sandbox: kernel enforcement setup failed".to_string()),
            StepOutcome::NoSandbox { code: None, .. }
        ));
        assert!(matches!(
            spawn_failure("Permission denied (os error 13)".to_string()),
            StepOutcome::NotStarted { .. }
        ));
    }

    #[test]
    fn the_programs_to_probe_are_listed_once_in_order() {
        let allow = vec!["bash".to_string(), "jq-not-a-real-binary".to_string()];
        let default = programs_to_probe(None, &allow);
        assert_eq!(&default[..COMMON_PROGRAMS.len()], COMMON_PROGRAMS);
        assert_eq!(
            default.last().map(String::as_str),
            Some("jq-not-a-real-binary")
        );
        assert_eq!(default.len(), COMMON_PROGRAMS.len() + 1);
        let named = programs_to_probe(Some(&["curl".into(), "bash".into(), "curl".into()]), &allow);
        assert_eq!(named, ["curl", "bash"]);
    }

    #[test]
    fn the_common_probes_have_their_steps_and_timeouts() {
        assert_eq!(probe_spec("sh").steps[0].argv, ["sh", "-c", ":"]);
        assert_eq!(probe_spec("cc").steps[0].timeout, Duration::from_secs(120));
        assert_eq!(probe_spec("cc").steps[0].files[0].path, "hello.c");
        let cargo = probe_spec("cargo");
        assert_eq!(cargo.steps[0].argv, ["cargo", "--version"]);
        assert_eq!(cargo.steps[1].timeout, Duration::from_secs(180));
        assert_eq!(
            probe_spec("unshare").steps[0].argv,
            ["unshare", "--user", "--map-root-user", "true"]
        );
        let other = probe_spec("zig");
        assert_eq!(other.steps[0].argv, ["zig", "--version"]);
        assert_eq!(other.steps[0].timeout, DEFAULT_STEP_TIMEOUT);
    }

    #[test]
    fn a_probe_serializes_its_keys_in_order() {
        let probe = ProgramProbe {
            program: "curl".into(),
            status: ProbeStatus::Refused,
            in_shell_allow: false,
            resolved: Some("/usr/bin/curl".into()),
            probe: vec!["curl".into(), "--version".into()],
            exit_code: Some(0),
            cause: Some(ProbeCause::NotInShellAllow),
            reason: Some("r".into()),
            grant: Some("g".into()),
            note: None,
            detail: String::new(),
        };
        let text = serde_json::to_string(&probe).unwrap();
        let keys = [
            "program",
            "status",
            "in_shell_allow",
            "resolved",
            "probe",
            "exit_code",
            "cause",
            "reason",
            "grant",
            "note",
            "detail",
        ];
        let positions: Vec<usize> = keys
            .iter()
            .map(|key| text.find(&format!("\"{key}\":")).unwrap())
            .collect();
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]), "{text}");
        assert!(text.contains("\"cause\":\"not-in-shell-allow\""), "{text}");
        assert!(text.contains("\"status\":\"refused\""), "{text}");
    }

    fn scoped_inputs(shell_allow: &[&str]) -> ProbeInputs {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap();
        ProbeInputs {
            policy: CapabilityPolicy {
                shell_allow: shell_allow.iter().map(|name| (*name).to_string()).collect(),
                ..CapabilityPolicy::default()
            },
            declared_floor: ContainmentClass::Scoped,
            workdir: home.join("project").join("workdir"),
            pinned_tools: Vec::new(),
            declared_tools: Vec::new(),
        }
    }

    fn entries(dir: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .map(|read| read.flatten().map(|entry| entry.path()).collect())
            .unwrap_or_default()
    }

    fn under_scratch_home(test: &str) {
        if crate::network_namespace::skip_without_egress_namespace(test) {
            return;
        }
        let home =
            tempfile::tempdir_in(std::env::var_os("HOME").map(PathBuf::from).unwrap()).unwrap();
        crate::murmur_home::run_with_home(&format!("program_probe::tests::{test}"), home.path());
    }

    #[test]
    fn a_step_past_its_timeout_is_killed() {
        under_scratch_home("inner_a_step_past_its_timeout_is_killed");
    }

    #[test]
    #[ignore = "run by a_step_past_its_timeout_is_killed under a scratch HOME"]
    fn inner_a_step_past_its_timeout_is_killed() {
        if !crate::murmur_home::in_scratch_home() {
            return;
        }
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        let runner = ProgramProbeRunner::new(scoped_inputs(&["sh"]), HostProbe::probe());
        let spec = ProbeSpec {
            program: "sleep".to_string(),
            steps: vec![ProbeStep::new(&["sleep", "60"]).with_timeout(Duration::from_secs(1))],
        };
        let started = std::time::Instant::now();
        let report = runner.report(&[probe_spec("sh"), spec]);
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(report.probed[0].status, ProbeStatus::Runs, "{report:#?}");
        let slept = &report.probed[1];
        assert_eq!(slept.cause, Some(ProbeCause::TimedOut), "{report:#?}");
        assert_eq!(slept.grant, None);
        assert_eq!(slept.exit_code, None);
        assert_eq!(slept.probe, ["sleep", "60"]);
        assert!(!entries(&home.join(".murmur").join(PROBES_DIR_NAME)).is_empty());
        drop(runner);
        assert_eq!(
            entries(&home.join(".murmur").join(PROBES_DIR_NAME)),
            Vec::<PathBuf>::new()
        );
        assert_eq!(
            entries(&home.join(".murmur").join("sessions")),
            Vec::<PathBuf>::new()
        );
    }

    #[test]
    fn no_cgroup_scope_makes_every_program_no_sandbox() {
        under_scratch_home("inner_no_cgroup_scope_makes_every_program_no_sandbox");
    }

    #[test]
    #[ignore = "run by no_cgroup_scope_makes_every_program_no_sandbox under a scratch HOME"]
    fn inner_no_cgroup_scope_makes_every_program_no_sandbox() {
        if !crate::murmur_home::in_scratch_home() {
            return;
        }
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        crate::cgroup::FORCE_DELEGATION_FAILURE.with(|flag| flag.set(true));
        let runner = ProgramProbeRunner::new(scoped_inputs(&["bash"]), HostProbe::probe());
        crate::cgroup::FORCE_DELEGATION_FAILURE.with(|flag| flag.set(false));
        let report = runner.report(&[probe_spec("bash"), probe_spec("sh")]);
        for probe in &report.probed {
            assert_eq!(probe.cause, Some(ProbeCause::NoSandbox), "{report:#?}");
            assert!(
                probe.reason.as_deref().unwrap().starts_with("E-RUN-012: "),
                "{probe:?}"
            );
            assert_eq!(probe.grant.as_deref(), Some(E_RUN_012_GRANT));
            assert_eq!(probe.exit_code, None);
        }
        // Nothing was created to spawn into.
        assert_eq!(
            entries(&home.join(".murmur").join(PROBES_DIR_NAME)),
            Vec::<PathBuf>::new()
        );
        assert_eq!(
            entries(&home.join(".murmur").join("sessions")),
            Vec::<PathBuf>::new()
        );
    }

    #[test]
    fn a_failed_enforcement_setup_makes_every_program_no_sandbox() {
        under_scratch_home("inner_a_failed_enforcement_setup_makes_every_program_no_sandbox");
    }

    #[test]
    #[ignore = "run by a_failed_enforcement_setup_makes_every_program_no_sandbox under a scratch HOME"]
    fn inner_a_failed_enforcement_setup_makes_every_program_no_sandbox() {
        if !crate::murmur_home::in_scratch_home() {
            return;
        }
        let runner = ProgramProbeRunner::new(scoped_inputs(&["bash"]), HostProbe::probe());
        crate::sandbox::FORCE_PREPARE_FAILURE.with(|flag| flag.set(true));
        let report = runner.report(&[probe_spec("bash"), probe_spec("sh"), probe_spec("git")]);
        crate::sandbox::FORCE_PREPARE_FAILURE.with(|flag| flag.set(false));
        for probe in &report.probed {
            assert_eq!(probe.cause, Some(ProbeCause::NoSandbox), "{report:#?}");
            assert!(
                probe
                    .reason
                    .as_deref()
                    .unwrap()
                    .contains("forced by test seam"),
                "{probe:?}"
            );
            assert_eq!(probe.grant.as_deref(), Some(SANDBOX_UNPREPARED_GRANT));
            assert_eq!(probe.exit_code, None);
            assert_eq!(probe.detail, "");
        }
    }
}
