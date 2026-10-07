//! `mur run --roster`: launch the formation a project's `roster.yaml` declares, for one task.
//!
//! The launcher admits the roster, mints one formation id, starts every peer and waits for each
//! door to answer, announces the formation, starts the entry member last, and waits on the entry
//! member's process. It stages no session of its own and contacts no daemon: every member is a
//! `mur run` subprocess, owned by [`capsule_runtime::formation_launch`].

// The formation line goes to stdout ahead of the entry member's own output, which the entry member
// writes to the same inherited stream; a reader may close it at any point after that.

use std::path::{Path, PathBuf};

use capsule_runtime::formation_launch::{
    self, render_stopped, signal_name, FormationEnding, FormationLaunchOptions,
    MemberLaunchFailure, RunningFormation,
};
use capsule_runtime::{admit_roster_file, FormationId};
use murmur_artifact::{LocalRegistry, TraceRetainConfig, ROSTER_FILENAME};

use crate::error::{CliError, E_IO_003, E_ROS_001, E_RUN_045, E_RUN_046};
use crate::registry_client::FallbackRegistry;

use super::read_optional_lockfile;

/// What `mur run --roster` was given: the roster, and the flags it passes through.
pub(crate) struct RosterLaunch<'a> {
    /// A project directory or its `roster.yaml`.
    pub(crate) roster: &'a Path,
    pub(crate) task: Option<&'a str>,
    pub(crate) json: bool,
    pub(crate) verbose: bool,
    pub(crate) no_env_file: bool,
    pub(crate) containment: Option<&'a str>,
}

/// Launch the formation and run it to its end. `Ok` carries the launcher's exit status, and is
/// returned only once every member has been stopped and reaped.
pub(crate) fn run_roster(launch: RosterLaunch<'_>) -> Result<i32, CliError> {
    refuse_inside_a_formation()?;
    let project_dir = roster_project_dir(launch.roster)?;

    // The stores and the lock `mur run --capsule` resolves a member from, in the same order.
    let registry = FallbackRegistry {
        primary: LocalRegistry::new(project_dir.join(".murmur").join("artifacts")),
        secondary: LocalRegistry::from_default_home().map_err(CliError::from)?,
    };
    let lock = read_optional_lockfile(&project_dir)?;
    let roster =
        admit_roster_file(&project_dir, &registry, lock.as_ref()).map_err(CliError::from)?;
    // A warning only: calls may never overlap, so the formation launches regardless.
    for overflow in roster.caller_overflows() {
        capsule_runtime::runtime_err!(
            "[mur run] {}",
            capsule_runtime::caller_overflow_warning(&overflow)
        );
    }

    // The entry member's `trace.retain` bounds this project's formation directories as it bounds
    // the entry member's own sessions. Without one, nothing is removed.
    let retain = roster
        .entry()
        .manifest
        .trace
        .as_ref()
        .and_then(|trace| trace.retain);
    let formation_id = FormationId::mint();
    let prune = || {
        if let Some(policy) = &retain {
            prune_formation_directories(&formation_id, &project_dir, policy);
        }
    };

    let mut options = FormationLaunchOptions::new(formation_id.clone(), project_dir.clone());
    options.task = launch.task.map(str::to_string);
    options.json = launch.json;
    options.verbose = launch.verbose;
    options.no_env_file = launch.no_env_file;
    options.containment = launch.containment.map(str::to_string);

    formation_launch::catch_launcher_signals().map_err(|error| {
        CliError::new(
            E_IO_003,
            format!("failed to catch SIGINT, SIGTERM and SIGHUP for the formation: {error}"),
        )
    })?;

    // Every return below runs the pass once, after every member of this launch is reaped: a
    // refused launch has stopped what it started, and `wait` stops every member before it returns.
    let mut formation = match formation_launch::launch_formation(&roster, options) {
        Ok(formation) => formation,
        Err(failure) => {
            prune();
            return refused(&failure);
        }
    };
    announce(&formation, launch.json);
    if let Err(failure) = formation.start_entry() {
        prune();
        return refused(&failure);
    }
    let exit = formation.wait();
    if let FormationEnding::Signalled(signal) = exit.ending {
        capsule_runtime::runtime_err!(
            "[mur run] {}: the formation was stopped; stopped: {}",
            signal_name(signal),
            render_stopped(&exit.stopped)
        );
    }
    prune();
    Ok(exit.exit_code())
}

/// Remove the ended formation directories of `project_dir` that `policy` expires, naming each on
/// stderr, newest first. `current` and every formation minted after it are kept.
fn prune_formation_directories(
    current: &FormationId,
    project_dir: &Path,
    policy: &TraceRetainConfig,
) {
    for pruned in formation_launch::prune_ended_formations(current, project_dir, policy) {
        capsule_runtime::runtime_err!(
            "[mur run] retention: removed formation {} ({})",
            pruned.formation_id,
            pruned.reason
        );
    }
}

/// `E-RUN-046` when this process is itself a formation member: a child joins its parent's
/// formation and never starts one, and a member is no different.
fn refuse_inside_a_formation() -> Result<(), CliError> {
    match FormationId::from_env() {
        Ok(None) => Ok(()),
        Ok(Some(_)) | Err(_) => Err(CliError::with_hint(
            E_RUN_046,
            "MURMUR_FORMATION_ID is set, so this process is already a member of a formation, and \
             a formation member cannot launch a formation",
            "a member joins the formation it was launched into and never starts one; unset \
             MURMUR_FORMATION_ID to launch this roster as a formation of its own",
        )),
    }
}

/// The project directory `--roster` names: the directory itself, or the one holding the
/// `roster.yaml` it names. A file under any other name is refused, because admission reads only
/// `<project>/roster.yaml`.
fn roster_project_dir(arg: &Path) -> Result<PathBuf, CliError> {
    let path = if arg.is_absolute() {
        arg.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|source| {
                CliError::new(
                    E_IO_003,
                    format!("failed to determine current working directory: {source}"),
                )
            })?
            .join(arg)
    };
    if path.is_dir() {
        return Ok(path);
    }
    if path.file_name().and_then(|name| name.to_str()) == Some(ROSTER_FILENAME) {
        if let Some(project_dir) = path.parent() {
            return Ok(project_dir.to_path_buf());
        }
    }
    Err(CliError::with_hint(
        E_ROS_001,
        format!(
            "{}: a roster is read from roster.yaml in its project directory, and this path is \
             neither a directory nor a file named roster.yaml",
            arg.display()
        ),
        "pass the project directory, or the roster.yaml inside it, to --roster; a bare --roster \
         means ./roster.yaml",
    ))
}

/// The formation line on stdout under `--json`, or the formation block on stderr. Printed once
/// every peer is ready and before the entry member starts. No door token appears in either.
fn announce(formation: &RunningFormation, json: bool) {
    if json {
        let peers: Vec<serde_json::Value> = formation
            .peers()
            .iter()
            .map(|peer| {
                serde_json::json!({
                    "name": peer.name,
                    "pid": peer.pid,
                    "session_id": peer.session_id,
                    "url": peer.url,
                    "workdir": peer.workdir.display().to_string(),
                })
            })
            .collect();
        let line = serde_json::json!({
            "entry": formation.entry_name(),
            "formation_id": formation.formation_id().as_str(),
            "peers": peers,
        });
        capsule_runtime::runtime_out!("{line}");
        return;
    }
    let width = formation
        .peers()
        .iter()
        .map(|peer| peer.name.len())
        .chain([formation.entry_name().len()])
        .max()
        .unwrap_or_default();
    let mut block = format!("formation: {}", formation.formation_id());
    for peer in formation.peers() {
        block.push_str(&format!(
            "\n  peer   {name:<width$}  {coordinate}  pid {pid}  {session}  {url}\n         \
             {blank:<width$}  workdir {workdir}",
            name = peer.name,
            coordinate = format_args!("{}@{}", peer.capsule, peer.version),
            pid = peer.pid,
            session = peer.session_id,
            url = peer.url,
            blank = "",
            workdir = peer.workdir.display(),
        ));
    }
    block.push_str(&format!(
        "\n  entry  {name:<width$}  starting",
        name = formation.entry_name()
    ));
    capsule_runtime::runtime_err!("{block}");
}

/// The launcher's answer to a launch that did not complete: `E-RUN-045`, or — when what ended it
/// was a signal the launcher caught — the signal's exit status.
fn refused(failure: &MemberLaunchFailure) -> Result<i32, CliError> {
    if let Some(signal) = failure.interrupted_by {
        capsule_runtime::runtime_err!("[mur run] {failure}");
        return Ok(128 + signal);
    }
    Err(CliError::with_hint(
        E_RUN_045,
        failure.to_string(),
        "the formation was not launched, and nothing it started is still running; fix the member \
         named above and launch again. `mur run --capsule <capsule> --capsule-version <version>` \
         runs one member on its own and shows its whole output",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_roster_path_names_its_project_directory() {
        let project = tempfile::tempdir().unwrap();
        assert_eq!(roster_project_dir(project.path()).unwrap(), project.path());
        assert_eq!(
            roster_project_dir(&project.path().join("roster.yaml")).unwrap(),
            project.path()
        );
        let other = roster_project_dir(&project.path().join("other.yaml")).unwrap_err();
        assert_eq!(other.code, E_ROS_001);
        assert!(
            other
                .message
                .contains("a roster is read from roster.yaml in its project directory"),
            "{}",
            other.message
        );
    }
}
