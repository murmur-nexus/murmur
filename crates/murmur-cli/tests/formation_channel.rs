//! A member's formation channel, read strictly: `mur run` started as one member, on its own, with
//! an inherited pipe or file as the descriptor `MURMUR_FORMATION_CHANNEL` names. Anything it
//! cannot read as this member's credentials refuses the launch with `E-RUN-046` before a session
//! exists, naming the problem without quoting the line.

#[path = "common/mod.rs"]
mod common;

use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use capsule_runtime::formation::{FORMATION_ID_ENV, FORMATION_PEERS_ENV};
use capsule_runtime::{FormationAuthority, FormationId, FORMATION_CHANNEL_ENV};
use common::door_capsule::{agent_project, driver_home, QUEUE_SLEEP_YAML};
use serde_json::Value;
use tempfile::TempDir;

/// The descriptor number the channel is handed to `mur run` at.
const CHANNEL_FD: i32 = 47;

const MEMBER_YAML: &str = "exports:\n  peer_tasks:\n    accept: true\n\
                           network:\n  authentication:\n    scheme: bearer\n";

/// What the channel descriptor is.
enum Channel {
    /// Nothing at [`CHANNEL_FD`].
    Absent,
    /// A pipe holding `bytes`, whose write end stays open while `mur run` runs.
    Pipe(Vec<u8>),
    /// A regular file holding `bytes`.
    File(Vec<u8>),
}

struct Member {
    project: TempDir,
    home: TempDir,
    _model: common::ScriptedServer,
}

impl Member {
    fn new() -> Self {
        let model = common::ScriptedServer::start(Vec::new());
        let project = agent_project(
            &model.endpoint,
            "coder",
            "",
            &format!("{QUEUE_SLEEP_YAML}{MEMBER_YAML}"),
        );
        Self {
            project,
            home: driver_home(),
            _model: model,
        }
    }

    /// Every `ses_*` directory under the project or the scratch `HOME`.
    fn sessions(&self) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let mut stack = vec![
            self.project.path().to_path_buf(),
            self.home.path().to_path_buf(),
        ];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for path in entries.flatten().map(|entry| entry.path()) {
                if !path.is_dir() {
                    continue;
                }
                if path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("ses_"))
                {
                    found.push(path);
                } else {
                    stack.push(path);
                }
            }
        }
        found
    }

    /// `mur run --json` in the project, with `env` and `channel` at [`CHANNEL_FD`]. Returns the
    /// child and the write end of a pipe channel, which the caller keeps open.
    fn spawn(
        &self,
        env: &[(&str, &str)],
        channel: Channel,
    ) -> (std::process::Child, Option<std::io::PipeWriter>) {
        let mut command = Command::new(assert_cmd::cargo::cargo_bin("mur"));
        command
            .args(["run", "--json"])
            .current_dir(self.project.path())
            .env("HOME", self.home.path())
            .env_remove("NEXUS_API_KEY")
            .env_remove(capsule_runtime::DOOR_TOKEN_ENV)
            .env_remove(FORMATION_ID_ENV)
            .env_remove(FORMATION_PEERS_ENV)
            .env_remove(FORMATION_CHANNEL_ENV)
            .env_remove("MURMUR_SPAWNER")
            .envs(env.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let (source, writer): (Option<OwnedFd>, _) = match channel {
            Channel::Absent => (None, None),
            Channel::Pipe(bytes) => {
                let (reader, mut writer) = std::io::pipe().unwrap();
                writer.write_all(&bytes).unwrap();
                (Some(reader.into()), Some(writer))
            }
            Channel::File(bytes) => {
                let path = self.home.path().join("channel");
                std::fs::write(&path, bytes).unwrap();
                (Some(std::fs::File::open(&path).unwrap().into()), None)
            }
        };
        if let Some(source) = &source {
            hand_over(&mut command, source.as_raw_fd());
        }
        let child = command.spawn().expect("mur run starts");
        drop(source);
        (child, writer)
    }

    /// `mur run` refused before staging: its exit status and stderr.
    fn refused(&self, env: &[(&str, &str)], channel: Channel) -> String {
        let (child, writer) = self.spawn(env, channel);
        let output = child.wait_with_output().unwrap();
        drop(writer);
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert_eq!(output.status.code(), Some(1), "{stderr}");
        assert!(output.stdout.is_empty(), "{stderr}");
        assert!(stderr.contains("error[E-RUN-046]"), "{stderr}");
        assert!(stderr.contains(FORMATION_CHANNEL_ENV) || stderr.contains(FORMATION_PEERS_ENV));
        assert!(
            self.sessions().is_empty(),
            "a refused launch left a session"
        );
        stderr
    }
}

/// `source` at [`CHANNEL_FD`] in the started process alone, as a formation launcher hands a member
/// its channel.
#[allow(unsafe_code)]
fn hand_over(command: &mut Command, source: i32) {
    use std::os::unix::process::CommandExt;
    // SAFETY: the closure runs between `fork` and `exec` and makes only `dup2` and `fcntl` calls
    // on integer descriptors, which are async-signal-safe.
    unsafe {
        command.pre_exec(move || {
            if source == CHANNEL_FD {
                let flags = libc::fcntl(source, libc::F_GETFD);
                libc::fcntl(source, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
            } else if libc::dup2(source, CHANNEL_FD) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// One refused launch: what it is, its environment, its channel, and what the refusal says.
type Case<'a> = (&'a str, Vec<(&'a str, &'a str)>, Channel, &'a str);

fn line(text: &str) -> Vec<u8> {
    format!("{text}\n").into_bytes()
}

/// Every channel `mur run` cannot read as this member's credentials is `E-RUN-046` before any
/// session exists, and the refusal never quotes what the channel carried.
#[test]
fn an_unreadable_formation_channel_refuses_the_launch() {
    let member = Member::new();
    let id = FormationId::mint();
    let authority = FormationAuthority::generate(&id).unwrap();
    let good = authority
        .member_bundle("coder", &["reviewer"])
        .render_line();
    let fd = CHANNEL_FD.to_string();
    let with_id = [
        (FORMATION_ID_ENV, id.as_str()),
        (FORMATION_CHANNEL_ENV, fd.as_str()),
    ];
    let other = FormationAuthority::generate(&FormationId::mint()).unwrap();
    let mut bad_key: Value = serde_json::from_str(&good).unwrap();
    bad_key["verify_key"] = Value::from("bm90IGEga2V5");

    let cases: Vec<Case> = vec![
        (
            "(a) a channel without a formation id",
            vec![(FORMATION_CHANNEL_ENV, fd.as_str())],
            Channel::Pipe(line(&good)),
            "MURMUR_FORMATION_ID is not",
        ),
        (
            "(b) a descriptor that is not open",
            vec![
                (FORMATION_ID_ENV, id.as_str()),
                (FORMATION_CHANNEL_ENV, "987"),
            ],
            Channel::Absent,
            "descriptor 987 is not open",
        ),
        (
            "(c) a first line that is not JSON",
            with_id.to_vec(),
            Channel::File(line("this is not the line you are looking for")),
            "its first line is not JSON",
        ),
        (
            "(d) another formation's line",
            with_id.to_vec(),
            Channel::Pipe(line(&other.member_bundle("coder", &[]).render_line())),
            "names another formation",
        ),
        (
            "(e) a verify key that is not one",
            with_id.to_vec(),
            Channel::Pipe(line(&bad_key.to_string())),
            "verify_key is not a formation key",
        ),
        (
            "(f) a member name that is not one",
            with_id.to_vec(),
            Channel::Pipe(line(&authority.member_bundle("Coder", &[]).render_line())),
            "member is not a member name",
        ),
        (
            "(h) peers in the process environment",
            vec![
                (FORMATION_ID_ENV, id.as_str()),
                (FORMATION_PEERS_ENV, "coder=http://localhost:1"),
            ],
            Channel::Absent,
            "set in this process's environment",
        ),
    ];
    for (case, env, channel, wording) in cases {
        let stderr = member.refused(&env, channel);
        assert!(stderr.contains(wording), "{case}: {stderr}");
        for secret in ["mft1.", "looking for", "bm90IGEga2V5", "localhost:1"] {
            assert!(
                !stderr.contains(secret),
                "{case} quotes the channel: {stderr}"
            );
        }
        assert!(
            !stderr.contains(&authority.verify_key().render()),
            "{case}: {stderr}"
        );
    }

    // (g) Nothing arrives: refused at the 10 s deadline.
    let started = Instant::now();
    let stderr = member.refused(&with_id, Channel::Pipe(Vec::new()));
    assert!(
        stderr.contains("no first line arrived within 10s"),
        "{stderr}"
    );
    assert!(started.elapsed() >= Duration::from_secs(10));
}

/// A valid first line launches the member, and its trace names it. (That the descriptor is
/// then close-on-exec is `capsule-runtime`'s unit test: `mur` makes itself non-dumpable, so its
/// `/proc/<pid>/fdinfo` is not readable from here.)
#[test]
fn a_valid_channel_launches_the_member() {
    if common::skip_without_host_support("a_valid_channel_launches_the_member") {
        return;
    }
    let member = Member::new();
    let id = FormationId::mint();
    let authority = FormationAuthority::generate(&id).unwrap();
    let fd = CHANNEL_FD.to_string();
    let (mut child, writer) = member.spawn(
        &[
            (FORMATION_ID_ENV, id.as_str()),
            (FORMATION_CHANNEL_ENV, fd.as_str()),
        ],
        Channel::Pipe(line(
            &authority
                .member_bundle("coder", &["reviewer"])
                .render_line(),
        )),
    );
    let stdout = child.stdout.take().unwrap();
    let mut first = String::new();
    BufReader::new(stdout).read_line(&mut first).unwrap();
    let ready: Value = serde_json::from_str(&first).unwrap_or_else(|_| {
        let _ = child.kill();
        panic!("no readiness line: {first:?}")
    });
    assert_eq!(ready["formation_id"], id.as_str(), "{ready}");
    assert!(!first.contains("mft1."), "{first}");

    // SAFETY: `kill` takes two integers; the pid is this test's own child.
    #[allow(unsafe_code)]
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
    }
    let _ = child.wait();
    drop(writer);
    let sessions = member.sessions();
    assert_eq!(sessions.len(), 1, "{sessions:?}");
    let session = &sessions[0];
    assert!(session.ends_with(ready["session_id"].as_str().unwrap()));
    let start = first_trace_line(session);
    assert_eq!(start["formation_member"], "coder", "{start}");
    assert_eq!(start["formation_callees"], serde_json::json!(["reviewer"]));
    assert!(common::door_capsule::files_containing(member.project.path(), "mft1.").is_empty());
    assert!(common::door_capsule::files_containing(member.home.path(), "mft1.").is_empty());
}

fn first_trace_line(session: &Path) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(line) = std::fs::read_to_string(session.join("trace.jsonl"))
            .ok()
            .and_then(|trace| trace.lines().next().map(str::to_string))
        {
            return serde_json::from_str(&line).unwrap();
        }
        assert!(Instant::now() < deadline, "no session_start");
        std::thread::sleep(Duration::from_millis(50));
    }
}
