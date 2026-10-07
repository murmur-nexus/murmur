//! `mur token`, and the absence of any door token from what `mur run` writes.
//!
//! Every case runs the capsule as its own `mur run` process under a scratch `HOME`, which is where
//! `mur token` finds the running record it reads.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Stdio},
    thread,
    time::{Duration, Instant},
};

use common::door_capsule::{
    agent_project, authenticated_capsule_with_a_task_in_flight, driver_home, end_turn,
    files_containing, message, mur, rpc, wait_completed, MurRun, AUTHENTICATION_YAML,
    QUEUE_SLEEP_YAML,
};
use serde_json::{json, Value};

/// A `mur run` whose stdout and stderr both go to `log`, as `mur run > run.log 2>&1 &` does.
struct Redirected {
    child: Child,
    log: PathBuf,
}

impl Redirected {
    fn start(home: &Path, manifest: &Path, json: bool, log: &Path) -> Self {
        let file = fs::File::create(log).unwrap();
        let mut command = std::process::Command::new(assert_cmd::cargo::cargo_bin("mur"));
        command
            .args(["run", "--manifest"])
            .arg(manifest)
            .current_dir(manifest.parent().unwrap())
            .env("HOME", home)
            .env_remove("NEXUS_API_KEY")
            .env_remove(capsule_runtime::DOOR_TOKEN_ENV)
            .env_remove(capsule_runtime::formation::FORMATION_ID_ENV)
            .env_remove(capsule_runtime::formation::FORMATION_PEERS_ENV)
            .env_remove(capsule_runtime::FORMATION_CHANNEL_ENV)
            .stdin(Stdio::null())
            .stdout(file.try_clone().unwrap())
            .stderr(file);
        if json {
            command.arg("--json");
        }
        Self {
            child: command.spawn().expect("mur run should start"),
            log: log.to_path_buf(),
        }
    }

    fn log(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// The session id and `host:port` the log announces, once it does.
    fn announced(&self, json: bool) -> (String, String) {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let log = self.log();
            let mut session_id = None;
            let mut url = None;
            for line in log.lines() {
                if json {
                    if let Ok(ready) = serde_json::from_str::<Value>(line) {
                        session_id = ready["session_id"].as_str().map(str::to_string);
                        url = ready["url"].as_str().map(str::to_string);
                    }
                } else if let Some(rest) = line.strip_prefix("murmur: url ") {
                    url = Some(rest.trim().to_string());
                } else if let Some(rest) = line.strip_prefix("session: ") {
                    session_id = Some(rest.trim().to_string());
                }
            }
            if let (Some(session_id), Some(url)) = (session_id, url) {
                return (session_id, url);
            }
            assert!(
                Instant::now() < deadline,
                "the door was never announced; the log so far:\n{log}"
            );
            thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Redirected {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `mur token <args>` under `home`: its exit status, stdout and stderr.
fn token(home: &Path, args: &[&str]) -> (bool, String, String) {
    let output = mur(home, &[]).arg("token").args(args).output().unwrap();
    (
        output.status.success(),
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

fn record_path(home: &Path, session_id: &str) -> PathBuf {
    home.join(".murmur")
        .join("running")
        .join(format!("{session_id}.json"))
}

/// The acceptance case, in both output modes: a run redirected to a file writes no token there,
/// and `mur token` hands out every one from the running record.
#[test]
fn a_redirected_run_writes_no_token_and_mur_token_reads_each_from_the_record() {
    for json in [false, true] {
        let server = common::ScriptedServer::start(vec![end_turn(1, "done")]);
        let home = driver_home();
        let project = agent_project(
            &server.endpoint,
            "door-token",
            "",
            &format!("{QUEUE_SLEEP_YAML}{AUTHENTICATION_YAML}"),
        );
        let log = project.path().join("run.log");
        let mut run =
            Redirected::start(home.path(), &project.path().join("murmur.yaml"), json, &log);
        let (session_id, addr) = run.announced(json);
        if json {
            let ready: Value = serde_json::from_str(run.log().lines().last().unwrap()).unwrap();
            assert_eq!(ready["auth"], "bearer", "{ready}");
        } else {
            assert!(
                run.log()
                    .contains(&format!("murmur: auth bearer (mur token {session_id})\n")),
                "{}",
                run.log()
            );
        }

        // Omitted, `@1` and the full id name the same session, and print exactly one line.
        let (ok, operator, stderr) = token(home.path(), &[]);
        assert!(ok, "{stderr}");
        assert!(operator.starts_with("mdt1."), "{operator}");
        assert!(operator.ends_with('\n') && operator.lines().count() == 1);
        assert_eq!(stderr, "");
        for address in ["@1", session_id.as_str()] {
            assert_eq!(token(home.path(), &[address]).1, operator, "{address}");
        }
        assert_eq!(
            token(home.path(), &[&session_id, "--credential", "operator"]).1,
            operator
        );
        let operator = operator.trim_end();

        // The operator token drives the door.
        let sent = rpc(&addr, Some(operator), "message/send", message("m-1", "hi"));
        assert_eq!(sent.status, 200, "{sent:?}");
        let task_id = sent.json()["result"]["id"].as_str().unwrap().to_string();
        wait_completed(&addr, Some(operator), &task_id);

        // A declared credential reads and is refused what its scopes do not grant.
        let (ok, watcher, stderr) = token(home.path(), &["--credential", "watcher"]);
        assert!(ok, "{stderr}");
        let watcher = watcher.trim_end();
        assert!(watcher.starts_with("mdt1.") && watcher != operator);
        let got = rpc(&addr, Some(watcher), "tasks/get", json!({"id": task_id}));
        assert_eq!(got.status, 200, "{got:?}");
        let refused = rpc(&addr, Some(watcher), "message/send", message("m-2", "no"));
        assert_eq!(refused.status, 403, "{refused:?}");

        // The record holds every token, at 0600.
        let path = record_path(home.path(), &session_id);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let record: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(record["door_token"], operator);
        assert_eq!(record["credentials"]["watcher"], watcher);
        let reader = token(home.path(), &["--credential", "reader"]).1;
        assert_eq!(record["credentials"]["reader"], reader.trim_end());
        let session_dir = PathBuf::from(record["workdir"].as_str().unwrap());

        // `mur ps` names it and prints no token; `mur stop` ends it by address.
        let ps = mur(home.path(), &[]).arg("ps").assert().success();
        let ps = String::from_utf8_lossy(&ps.get_output().stdout).to_string();
        assert!(ps.contains(&session_id[session_id.len() - 4..]), "{ps}");
        assert!(!ps.contains("mdt1."), "{ps}");
        mur(home.path(), &[])
            .args(["stop", "@1"])
            .assert()
            .success();
        let deadline = Instant::now() + Duration::from_secs(30);
        while run.child.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "the capsule did not exit");
            thread::sleep(Duration::from_millis(100));
        }

        let written = run.log();
        assert!(
            !written.contains("mdt1."),
            "json={json}: the run log holds a token:\n{written}"
        );
        for root in [project.path(), session_dir.as_path()] {
            assert_eq!(
                files_containing(root, "mdt1."),
                Vec::<PathBuf>::new(),
                "json={json}: a file the session wrote holds a token"
            );
        }
    }
}

/// A public door minted no token: `E-RUN-048`, and nothing on stdout.
#[test]
fn mur_token_of_a_public_session_is_e_run_048() {
    let server = common::ScriptedServer::start(vec![]);
    let home = driver_home();
    let project = agent_project(&server.endpoint, "door-public", "", QUEUE_SLEEP_YAML);
    let run = MurRun::start(
        home.path(),
        &project.path().join("murmur.yaml"),
        true,
        &[],
        &[],
    );
    let session_id = run.session_id();
    let (ok, stdout, stderr) = token(home.path(), &[]);
    assert!(!ok);
    assert_eq!(stdout, "");
    assert!(stderr.contains("E-RUN-048"), "{stderr}");
    assert!(
        stderr.contains(&format!(
            "{session_id} has a public door: its capsule declares no network.authentication, so \
             it minted no token"
        )),
        "{stderr}"
    );
}

/// A credential the session did not mint is `E-RUN-049`, naming the ones it did.
#[test]
fn mur_token_of_an_undeclared_credential_is_e_run_049() {
    let server = common::ScriptedServer::start(vec![]);
    let home = driver_home();
    let project = agent_project(
        &server.endpoint,
        "door-names",
        "",
        &format!("{QUEUE_SLEEP_YAML}{AUTHENTICATION_YAML}"),
    );
    let run = MurRun::start(
        home.path(),
        &project.path().join("murmur.yaml"),
        true,
        &[],
        &[],
    );
    let session_id = run.session_id();
    let (ok, stdout, stderr) = token(home.path(), &["--credential", "admin"]);
    assert!(!ok);
    assert_eq!(stdout, "");
    assert!(stderr.contains("E-RUN-049"), "{stderr}");
    assert!(
        stderr.contains(&format!(
            "{session_id} minted no credential 'admin'; it has: operator, reader, watcher"
        )),
        "{stderr}"
    );
    assert!(!stderr.contains("mdt1."), "{stderr}");
}

/// No running session: the address error every live-address command gives.
#[test]
fn mur_token_with_nothing_running_is_e_run_022() {
    let home = tempfile::tempdir().unwrap();
    let (ok, stdout, stderr) = token(home.path(), &[]);
    assert!(!ok);
    assert_eq!(stdout, "");
    assert!(stderr.contains("E-RUN-022"), "{stderr}");
}

/// `mur watch`, `mur cancel` and `mur stop` by session address read the operator token from the
/// record, as they did when `mur run` printed it.
#[test]
fn watch_cancel_and_stop_by_address_still_reach_an_authenticated_door() {
    let home = driver_home();
    let (run, _server, _project, task_id) =
        authenticated_capsule_with_a_task_in_flight(&home, "door-address");

    let watched = mur(home.path(), &[])
        .args(["watch", "@1"])
        .timeout(Duration::from_secs(5))
        .output()
        .unwrap();
    let watched_stderr = String::from_utf8_lossy(&watched.stderr).to_string();
    assert!(
        !watched_stderr.contains("401") && !watched_stderr.contains("E-RUN-0"),
        "{watched_stderr}"
    );
    assert!(
        !String::from_utf8_lossy(&watched.stdout).contains("mdt1."),
        "mur watch printed a token"
    );

    let canceled = mur(home.path(), &[])
        .args(["cancel", "@1", &task_id])
        .assert()
        .success();
    let canceled = String::from_utf8_lossy(&canceled.get_output().stdout).to_string();
    assert!(canceled.contains("state:   canceled"), "{canceled}");

    mur(home.path(), &[])
        .args(["stop", &run.session_id()])
        .assert()
        .success();
}
