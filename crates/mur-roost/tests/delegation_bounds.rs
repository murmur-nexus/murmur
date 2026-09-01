//! How far a delegation chain may go, and how wide, refereed from the daemon's own records.
//!
//! Every case here drives the two-request exchange a real launch makes — `POST /spawn` with the
//! parent's credential, then `POST /register` with the approval it granted — against real
//! artifacts in a real registry and real MAC'd approvals. No case inspects a counter directly;
//! each asserts the response a caller receives.

#[path = "common/mod.rs"]
mod common;

use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use common::{Daemon, IDENTITY_REFUSAL, PLAIN_WORKER_BODY};
use mur_roost::authority::{now_ms, SpawnAuthority};
use murmur_artifact::{LocalRegistry, Registry};

/// A capsule whose `capabilities.spawn.allow` names itself. Under the envelope rule alone it is
/// contained by every copy of itself, so nothing but the depth budget ends the chain.
const RECURSER_BODY: &str = "artifacts: []\ncapabilities:\n  spawn:\n    allow: [recurser]\n";

const PARENT_SESSION: &str = "ses_00000000000000000000000000parent";

/// One level of a chain: ask, then register the child the approval names.
///
/// Returns the child's session id and the credential it will delegate with, or the refusal that
/// ended the chain.
fn descend(daemon: &Daemon, capsule: &str, credential: &str) -> Result<(String, String), String> {
    let permission = daemon.permission(capsule, "0.1.0", Some(credential));
    if permission.status != 200 {
        return Err(permission.error().to_string());
    }
    let approval = permission.body["approval"].as_str().unwrap().to_string();
    let child = daemon.child_session_id();
    let registered = daemon.register(&child, capsule, "0.1.0", Some(&approval));
    if registered.status != 200 {
        return Err(registered.error().to_string());
    }
    let credential = registered.body["credential"].as_str().unwrap().to_string();
    Ok((child, credential))
}

// ── 1. Depth ──────────────────────────────────────────────────────────────────

/// A capsule that names itself in `capabilities.spawn.allow` terminates at `--max-depth` instead of
/// recursing until the host runs out of processes.
#[test]
fn a_capsule_that_names_itself_terminates_at_the_declared_depth() {
    let daemon = Daemon::bounded(vec!["recurser".to_string()], 3, 4);
    daemon.publish_body("recurser", "0.1.0", RECURSER_BODY, "");

    let root = daemon.child_session_id();
    let registered = daemon.register(&root, "recurser", "0.1.0", None);
    assert_eq!(registered.status, 200, "{:?}", registered.body);
    let mut credential = registered.body["credential"].as_str().unwrap().to_string();
    let mut chain = vec![root];

    for level in 1..=3 {
        let (child, child_credential) =
            descend(&daemon, "recurser", &credential).unwrap_or_else(|refusal| {
                panic!("level {level} must be granted, got: {refusal}");
            });
        chain.push(child);
        credential = child_credential;
    }

    // The deepest level holds no budget, so its own request for one more level is refused — and
    // stays refused however many times it asks.
    for _ in 0..3 {
        let refusal = descend(&daemon, "recurser", &credential).unwrap_err();
        assert_eq!(
            refusal,
            "delegation depth bound reached: this daemon allows 3 levels of delegation below a \
             top-level capsule (--max-depth 3), and this session has none left to spend — a \
             capsule whose capabilities.spawn.allow names itself terminates here rather than \
             recursing",
        );
    }

    assert_eq!(daemon.session_ids().len(), 4, "{:?}", daemon.session_ids());
    for (level, session) in chain.iter().enumerate() {
        let status = daemon.status(session);
        assert_eq!(status.status, 200, "{:?}", status.body);
        assert_eq!(status.body["status"], "running");
        assert_eq!(
            status.body["depth_remaining"].as_u64().unwrap(),
            3 - level as u64,
            "level {level}",
        );
    }
    // Each level holds exactly the one child below it; the deepest holds none.
    assert_eq!(daemon.status(&chain[0]).body["live_children"], 1);
    assert_eq!(daemon.status(&chain[3]).body["live_children"], 0);
    assert!(
        daemon.workdir_entries().is_empty(),
        "{:?}",
        daemon.workdir_entries()
    );
}

/// The budget is the daemon's, sealed into the approval. Nothing the registrant sends can raise it,
/// and an approval whose payload is edited to raise it fails its MAC.
#[test]
fn a_child_cannot_claim_more_depth_than_its_parent_had() {
    let daemon = Daemon::new();
    daemon.seed_at_depth(
        PARENT_SESSION,
        "capabilities:\n  network:\n    allow: [registry.internal]\n  \
         spawn:\n    allow: [worker-a]\n",
        1,
    );
    daemon.publish("worker-a", "0.1.0");
    let credential = daemon.credential(PARENT_SESSION);

    let ordinary = daemon.child_session_id();
    let approval = daemon.approval("worker-a", "0.1.0", &credential);
    assert_eq!(
        daemon
            .register(&ordinary, "worker-a", "0.1.0", Some(&approval))
            .status,
        200,
    );
    assert_eq!(daemon.status(&ordinary).body["depth_remaining"], 0);

    // The same request with a depth of its own in the body. `POST /register` has no such field,
    // so the extra key is read by nothing and the session registers with the sealed budget.
    let claimant = daemon.child_session_id();
    let approval = daemon.approval("worker-a", "0.1.0", &credential);
    let claimed = daemon.register_body(
        &format!(
            r#"{{"session_id":"{claimant}","name":"worker-a","version":"0.1.0","depth_remaining":99}}"#
        ),
        Some(&approval),
    );
    assert_eq!(claimed.status, 200, "{:?}", claimed.body);
    assert_eq!(daemon.status(&claimant).body["depth_remaining"], 0);

    // The same approval with one character of its payload segment altered.
    let approval = daemon.approval("worker-a", "0.1.0", &credential);
    let mut segments: Vec<String> = approval.split('.').map(str::to_string).collect();
    let payload = &mut segments[1];
    let first = payload.remove(0);
    payload.insert(0, if first == 'a' { 'b' } else { 'a' });
    let forged = daemon.register(
        &daemon.child_session_id(),
        "worker-a",
        "0.1.0",
        Some(&segments.join(".")),
    );
    assert_eq!(forged.status, 403);
    assert_eq!(forged.error(), IDENTITY_REFUSAL);
}

// ── 2. Concurrency ────────────────────────────────────────────────────────────

/// A concurrency refusal names the bound and the current count, so the operator reading it knows
/// what to raise and what it is being raised past.
#[test]
fn a_concurrency_cap_refuses_naming_the_bound_and_the_count() {
    let daemon = Daemon::bounded(Vec::new(), 3, 2);
    daemon.seed_caller(PARENT_SESSION);
    daemon.publish("worker-a", "0.1.0");
    let credential = daemon.credential(PARENT_SESSION);

    let (_, first_child) = descend(&daemon, "worker-a", &credential).unwrap();
    descend(&daemon, "worker-a", &credential).unwrap();
    assert_eq!(daemon.status(PARENT_SESSION).body["live_children"], 2);

    let refusal = daemon.permission("worker-a", "0.1.0", Some(&credential));
    assert_eq!(refusal.status, 403);
    assert_eq!(
        refusal.error(),
        "delegation concurrency bound reached: this daemon allows a capsule 2 live children at a \
         time (--max-concurrent 2), and this session already holds 2 — wait for one to finish, or \
         raise --max-concurrent",
    );

    // A child that ends frees its slot.
    assert_eq!(daemon.deregister(&first_child, "complete").status, 200);
    assert_eq!(daemon.status(PARENT_SESSION).body["live_children"], 1);
    let granted = daemon.permission("worker-a", "0.1.0", Some(&credential));
    assert_eq!(granted.status, 200, "{:?}", granted.body);
}

/// A slot is taken at approval, not at registration: two rapid asks under a cap of one do not both
/// pass because neither child has registered yet.
#[test]
fn an_approved_but_unlaunched_child_holds_its_slot() {
    let daemon = Daemon::bounded(Vec::new(), 3, 1);
    daemon.seed_caller(PARENT_SESSION);
    daemon.publish("worker-a", "0.1.0");
    let credential = daemon.credential(PARENT_SESSION);

    let first = daemon.permission("worker-a", "0.1.0", Some(&credential));
    assert_eq!(first.status, 200, "{:?}", first.body);
    assert_eq!(daemon.status(PARENT_SESSION).body["live_children"], 1);

    let second = daemon.permission("worker-a", "0.1.0", Some(&credential));
    assert_eq!(second.status, 403);
    assert_eq!(
        second.error(),
        "delegation concurrency bound reached: this daemon allows a capsule 1 live children at a \
         time (--max-concurrent 1), and this session already holds 1 — wait for one to finish, or \
         raise --max-concurrent",
    );
    // Nothing registered, and the daemon created nothing for either ask.
    assert_eq!(daemon.session_ids(), vec![PARENT_SESSION.to_string()]);
    assert!(
        daemon.workdir_entries().is_empty(),
        "{:?}",
        daemon.workdir_entries()
    );
}

// ── 3. Neither bound has a value meaning unlimited ────────────────────────────

/// `--max-depth 0` and `--max-concurrent 0` each refuse every delegation.
#[test]
fn a_zero_bound_refuses_every_delegation() {
    let no_depth = Daemon::bounded(vec!["recurser".to_string()], 0, 4);
    no_depth.publish_body("recurser", "0.1.0", RECURSER_BODY, "");
    let root = no_depth.child_session_id();
    let credential = no_depth.register(&root, "recurser", "0.1.0", None).body["credential"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(no_depth.status(&root).body["depth_remaining"], 0);
    let refusal = no_depth.permission("recurser", "0.1.0", Some(&credential));
    assert_eq!(refusal.status, 403);
    assert!(
        refusal.error().contains("--max-depth 0"),
        "{}",
        refusal.error()
    );

    let no_children = Daemon::bounded(Vec::new(), 3, 0);
    no_children.seed_caller(PARENT_SESSION);
    no_children.publish("worker-a", "0.1.0");
    let refusal = no_children.permission(
        "worker-a",
        "0.1.0",
        Some(&no_children.credential(PARENT_SESSION)),
    );
    assert_eq!(refusal.status, 403);
    assert!(
        refusal.error().contains("--max-concurrent 0"),
        "{}",
        refusal.error()
    );
    assert_eq!(no_children.session_ids(), vec![PARENT_SESSION.to_string()]);
}

// ── 4. A bound that cannot be evaluated refuses ───────────────────────────────

/// Every figure a bound is decided from is read from the record of a *running* session. Where that
/// record cannot be read, the request is refused rather than granted on a default.
#[test]
fn a_bound_that_cannot_be_evaluated_refuses() {
    // (a) The asking session ended, and its credential still carries a valid MAC.
    let daemon = Daemon::bounded(vec!["worker".to_string()], 3, 4);
    daemon.publish_body("worker", "0.1.0", PLAIN_WORKER_BODY, "");
    daemon.publish("worker-a", "0.1.0");
    let root = daemon.child_session_id();
    let credential = daemon.register(&root, "worker", "0.1.0", None).body["credential"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(daemon.deregister(&credential, "complete").status, 200);
    let ended = daemon.permission("worker-a", "0.1.0", Some(&credential));
    assert_eq!(ended.status, 403);
    assert_eq!(ended.error(), IDENTITY_REFUSAL);

    // (b) The granting session ended between the ask and the registration.
    let daemon = Daemon::bounded(vec!["worker".to_string()], 3, 4);
    daemon.publish_body(
        "worker",
        "0.1.0",
        "artifacts: []\ncapabilities:\n  network:\n    allow: [registry.internal]\n  \
         spawn:\n    allow: [worker-a]\n",
        "",
    );
    daemon.publish("worker-a", "0.1.0");
    let parent = daemon.child_session_id();
    let credential = daemon.register(&parent, "worker", "0.1.0", None).body["credential"]
        .as_str()
        .unwrap()
        .to_string();
    let approval = daemon.approval("worker-a", "0.1.0", &credential);
    assert_eq!(daemon.deregister(&credential, "complete").status, 200);
    let orphaned = daemon.register(
        &daemon.child_session_id(),
        "worker-a",
        "0.1.0",
        Some(&approval),
    );
    assert_eq!(orphaned.status, 403);
    assert_eq!(orphaned.error(), IDENTITY_REFUSAL);
    assert_eq!(daemon.session_ids(), vec![parent]);

    // (c) An approval carrying a well-formed depth, minted by a key this daemon has
    // never held.
    let daemon = Daemon::new();
    daemon.seed_caller(PARENT_SESSION);
    daemon.publish("worker-a", "0.1.0");
    let digest = LocalRegistry::new(daemon.registry.path())
        .resolve("worker-a", "0.1.0")
        .unwrap()
        .sha256;
    let foreign = SpawnAuthority::generate().unwrap();
    let foreign_approval = foreign
        .mint_approval_token(
            PARENT_SESSION,
            "worker-a",
            "0.1.0",
            &digest,
            99,
            now_ms() + 60_000,
        )
        .unwrap();
    let refused = daemon.register(
        &daemon.child_session_id(),
        "worker-a",
        "0.1.0",
        Some(&foreign_approval),
    );
    assert_eq!(refused.status, 403);
    assert_eq!(refused.error(), IDENTITY_REFUSAL);
    assert_eq!(daemon.session_ids(), vec![PARENT_SESSION.to_string()]);
}

// ── 5. No total cap ───────────────────────────────────────────────────────────

/// `--max-total` is not a flag: the daemon holds no total cap, and rejects the argument at
/// startup.
#[test]
fn there_is_no_total_cap_to_set() {
    let registry = tempfile::TempDir::new().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_mur-roost"))
        .args([
            "--max-total",
            "10",
            "--registry-path",
            registry.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unknown argument: --max-total"), "{stderr}");
}

/// `--max-depth` and `--max-concurrent` reject a value that is not a number, as `--port` does.
#[test]
fn the_bound_flags_reject_a_value_that_is_not_a_number() {
    let registry = tempfile::TempDir::new().unwrap();
    for flag in ["--max-depth", "--max-concurrent"] {
        let output = Command::new(env!("CARGO_BIN_EXE_mur-roost"))
            .args([
                flag,
                "deep",
                "--registry-path",
                registry.path().to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(!output.status.success(), "{output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(&format!("invalid {flag}")), "{stderr}");
    }
}

/// A registration presenting no approval is the top of a chain, and starts from the operator's own
/// `--max-depth`.
#[test]
fn a_registration_with_no_approval_starts_from_max_depth() {
    let daemon = Daemon::bounded(vec!["worker".to_string()], 2, 4);
    daemon.publish_body("worker", "0.1.0", PLAIN_WORKER_BODY, "");
    let root = daemon.child_session_id();
    assert_eq!(daemon.register(&root, "worker", "0.1.0", None).status, 200);
    let status = daemon.status(&root);
    assert_eq!(status.body["status"], "running");
    assert_eq!(status.body["depth_remaining"], 2);
    assert_eq!(status.body["live_children"], 0);
}

/// The count and the reservation happen under one lock hold, so a parent asking from several
/// threads at once cannot take more slots than the operator allows.
///
/// The daemon serves each connection on its own thread. A check that released the lock before
/// recording its reservation would let every one of these asks read the same count and pass.
#[test]
fn concurrent_asks_cannot_take_more_slots_than_the_bound() {
    let daemon = Arc::new(Daemon::bounded(Vec::new(), 3, 1));
    daemon.seed_caller(PARENT_SESSION);
    daemon.publish("worker-a", "0.1.0");
    let credential = daemon.credential(PARENT_SESSION);

    let granted = Arc::new(AtomicUsize::new(0));
    let start = Arc::new(Barrier::new(8));
    let mut threads = Vec::new();
    for _ in 0..8 {
        let daemon = Arc::clone(&daemon);
        let granted = Arc::clone(&granted);
        let start = Arc::clone(&start);
        let credential = credential.clone();
        threads.push(thread::spawn(move || {
            start.wait();
            if daemon
                .permission("worker-a", "0.1.0", Some(&credential))
                .status
                == 200
            {
                granted.fetch_add(1, Ordering::SeqCst);
            }
        }));
    }
    for thread in threads {
        thread.join().unwrap();
    }

    assert_eq!(granted.load(Ordering::SeqCst), 1);
    assert_eq!(daemon.status(PARENT_SESSION).body["live_children"], 1);
}
