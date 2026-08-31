//! `POST /spawn` answers *may you*, and nothing else.
//!
//! The daemon holds no capsule runtime: it stages nothing, launches nothing, and takes no host
//! probe. What a granted request leaves behind is an approval and no other trace of itself.

#[path = "common/mod.rs"]
mod common;

use common::{Daemon, IDENTITY_REFUSAL};

const PARENT_SESSION: &str = "ses_0000000000000000000000000000parent";

/// A granted request answers with permission — an approval and the artifact it names — and with
/// nothing that could address a process.
#[test]
fn a_granted_spawn_returns_permission_and_no_process() {
    let daemon = Daemon::new();
    daemon.seed_caller(PARENT_SESSION);
    daemon.publish("worker", "0.1.0");
    let credential = daemon.credential(PARENT_SESSION);

    let response = daemon.permission("worker", "0.1.0", Some(&credential));

    assert_eq!(response.status, 200, "{:?}", response.body);
    let object = response.body.as_object().unwrap();
    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort();
    assert_eq!(
        keys,
        vec!["approval", "expires_at_ms", "name", "sha256", "version"]
    );
}

/// After the response: the job map holds only the parent's record, no directory was created
/// anywhere the daemon knows about, and no `trace.jsonl` exists under either.
#[test]
fn a_granted_spawn_creates_no_workdir_no_session_and_no_trace() {
    let daemon = Daemon::new();
    daemon.seed_caller(PARENT_SESSION);
    daemon.publish("worker", "0.1.0");
    let credential = daemon.credential(PARENT_SESSION);

    daemon.approval("worker", "0.1.0", &credential);

    assert_eq!(daemon.session_ids(), vec![PARENT_SESSION]);
    assert!(
        daemon.workdir_entries().is_empty(),
        "{:?}",
        daemon.workdir_entries()
    );
    assert!(traces_under(daemon.registry.path()).is_empty());
    assert!(traces_under(daemon.workdir.path()).is_empty());
    // `GET /status` for a session nothing registered is a 404, not a launched job.
    assert_eq!(daemon.status("ses_anything").status, 404);
}

/// The request body carries a capsule name and a version and nothing else. A workdir path in it
/// is ignored rather than used, because the daemon has no directory to make.
#[test]
fn the_spawn_body_carries_no_workdir() {
    let daemon = Daemon::new();
    daemon.seed_caller(PARENT_SESSION);
    daemon.publish("worker", "0.1.0");
    let credential = daemon.credential(PARENT_SESSION);
    let claimed = daemon.workdir.path().join("claimed-by-the-request");

    let response = daemon.post(
        "/spawn",
        &format!(
            r#"{{"name":"worker","version":"0.1.0","workdir":{}}}"#,
            serde_json::to_string(&claimed.display().to_string()).unwrap()
        ),
        &[(capsule_runtime::SPAWN_CREDENTIAL_HEADER, &credential)],
    );

    assert_eq!(response.status, 200, "{:?}", response.body);
    assert!(
        !claimed.exists(),
        "the daemon created {}",
        claimed.display()
    );
}

/// Every identity failure on this endpoint answers with the one refusal, byte for byte.
#[test]
fn every_identity_failure_answers_with_one_refusal() {
    let daemon = Daemon::new();
    daemon.seed_caller(PARENT_SESSION);
    daemon.publish("worker", "0.1.0");

    let absent = daemon.permission("worker", "0.1.0", None);
    let malformed = daemon.permission("worker", "0.1.0", Some("not-a-token"));
    let wrong_family = daemon.permission("worker", "0.1.0", Some("msa1.e30.e30"));
    let unknown_session = daemon.permission(
        "worker",
        "0.1.0",
        Some(&daemon.credential("ses_never-registered")),
    );

    for response in [&absent, &malformed, &wrong_family, &unknown_session] {
        assert_eq!(response.status, 403);
        assert_eq!(response.error(), IDENTITY_REFUSAL);
    }
    assert_eq!(absent.raw, malformed.raw);
    assert_eq!(absent.raw, wrong_family.raw);
    assert_eq!(absent.raw, unknown_session.raw);
}

#[test]
fn a_malformed_body_is_a_bad_request() {
    let daemon = Daemon::new();
    daemon.seed_caller(PARENT_SESSION);
    let credential = daemon.credential(PARENT_SESSION);

    let response = daemon.post(
        "/spawn",
        "{not json",
        &[(capsule_runtime::SPAWN_CREDENTIAL_HEADER, &credential)],
    );

    assert_eq!(response.status, 400);
    assert!(response.error().starts_with("invalid JSON:"));
}

#[test]
fn health_is_unchanged() {
    let daemon = Daemon::new();
    let response = daemon.get("/health");
    assert_eq!(response.status, 200);
    assert_eq!(response.body, serde_json::json!({}));
}

fn traces_under(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().is_some_and(|name| name == "trace.jsonl") {
                found.push(path);
            }
        }
    }
    found
}
