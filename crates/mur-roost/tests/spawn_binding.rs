//! Binding a spawn decision to the session that earned it and the artifact it was granted for.
//!
//! `spawn_envelope.rs` asserts what the referee compares. This suite asserts the three bindings
//! that make the referee's answer trustworthy: the credential that says who is asking, the
//! approval that says what was approved, and the expiry and single use that bound how long and how
//! often that answer holds.
//!
//! The two halves of a delegated launch are made by two different parties. The parent's runtime
//! presents its credential at `POST /spawn`; the child's runtime presents the resulting approval
//! at `POST /register`. A credential is therefore never presented alongside an approval, and the
//! approval's binding to the session that earned it is checked against the job store rather than
//! against a second token.

#[path = "common/mod.rs"]
mod common;

use capsule_runtime::SPAWN_APPROVAL_HEADER;
use common::{Daemon, IDENTITY_REFUSAL};
use mur_roost::{authority::now_ms, authority::SpawnAuthority, JobStatus};
use murmur_artifact::{LocalRegistry, Registry};

/// The session every delegating case asks as. Its envelope lists `worker-a`, `worker-b` and
/// `worker-c`.
const CALLER_SESSION: &str = "ses_00000000000000000000000000caller";

/// A second, strictly wider session: it lists `worker-a` where [`POOR_SESSION`] does not. Naming
/// it without holding its credential is the escalation the bindings refuse.
const RICH_SESSION: &str = "ses_ric00000000000000000000000000rich";

/// A session whose envelope lists nothing but `worker-c`.
const POOR_SESSION: &str = "ses_poo00000000000000000000000000poor";

// ── 1. Happy path ─────────────────────────────────────────────────────────────

/// The whole exchange, end to end: an approval for a capsule the caller's envelope contains, then
/// a registration of exactly that artifact.
#[test]
fn a_delegated_spawn_registers_the_artifact_its_approval_names() {
    let daemon = Daemon::new();
    daemon.seed_caller(CALLER_SESSION);
    daemon.publish("worker-a", "0.1.0");
    let credential = daemon.credential(CALLER_SESSION);

    let permission = daemon.permission("worker-a", "0.1.0", Some(&credential));
    assert_eq!(permission.status, 200, "{:?}", permission.body);
    let approval = permission.body["approval"].as_str().unwrap();
    assert!(approval.starts_with("msa1."), "{approval}");
    assert_eq!(permission.body["name"], "worker-a");
    assert_eq!(permission.body["version"], "0.1.0");
    assert_eq!(
        permission.body["sha256"].as_str().unwrap(),
        LocalRegistry::new(daemon.registry.path())
            .resolve("worker-a", "0.1.0")
            .unwrap()
            .sha256
    );
    assert!(
        permission.body["expires_at_ms"].as_u64().unwrap() > now_ms(),
        "{:?}",
        permission.body
    );
    // Permission, not a process.
    assert!(permission.body.get("capsule_url").is_none());
    assert!(permission.body.get("session_id").is_none());
    assert_eq!(daemon.session_ids(), vec![CALLER_SESSION]);

    let child = daemon.child_session_id();
    let response = daemon.register(&child, "worker-a", "0.1.0", Some(approval));

    assert_eq!(response.status, 200, "{:?}", response.body);
    assert!(response.body["credential"]
        .as_str()
        .unwrap()
        .starts_with("msc1."));
    assert!(
        daemon.session_ids().contains(&child),
        "the registered session has no job record: {:?}",
        daemon.session_ids()
    );
    assert_eq!(daemon.status(&child).body["status"], "running");
}

// ── 2. The escalation is closed ───────────────────────────────────────────────

/// Asking for a capsule only a better-provisioned session may spawn, with nothing to back the
/// claim, is refused — and that session's envelope is never reached, so nothing about it can be
/// launched.
#[test]
fn asking_without_a_credential_is_refused() {
    let daemon = Daemon::new();
    daemon.seed_caller(RICH_SESSION);
    daemon.seed(
        POOR_SESSION,
        "capabilities:\n  spawn:\n    allow: [worker-c]\n",
    );
    daemon.publish("worker-a", "0.1.0");

    let response = daemon.permission("worker-a", "0.1.0", None);

    assert_eq!(response.status, 403);
    assert_eq!(response.error(), IDENTITY_REFUSAL);
    assert_eq!(daemon.session_ids(), vec![POOR_SESSION, RICH_SESSION]);
    assert!(
        daemon.workdir_entries().is_empty(),
        "{:?}",
        daemon.workdir_entries()
    );
}

// ── 3. No existence oracle ────────────────────────────────────────────────────

/// Two requests differing only in whether the session they name exists get byte-identical
/// responses, on both endpoints.
#[test]
fn a_refusal_does_not_say_whether_a_session_exists() {
    let daemon = Daemon::new();
    daemon.seed_caller(CALLER_SESSION);
    daemon.publish("worker-a", "0.1.0");

    // A credential this daemon did not mint: well-formed, and unverifiable.
    let foreign_authority = SpawnAuthority::generate().unwrap();
    let known = daemon.permission(
        "worker-a",
        "0.1.0",
        Some(
            &foreign_authority
                .mint_credential_token(CALLER_SESSION)
                .unwrap(),
        ),
    );
    let unknown = daemon.permission(
        "worker-a",
        "0.1.0",
        Some(
            &foreign_authority
                .mint_credential_token("ses_no-such-session")
                .unwrap(),
        ),
    );
    assert_eq!(known.raw, unknown.raw);
    assert_eq!(known.status, 403);
    assert_eq!(known.error(), IDENTITY_REFUSAL);

    // And the same at registration: an approval this daemon did not mint says nothing about which
    // session it names.
    let foreign_known = daemon.register(
        &daemon.child_session_id(),
        "worker-a",
        "0.1.0",
        Some(
            &foreign_authority
                .mint_approval_token(CALLER_SESSION, "worker-a", "0.1.0", "d", now_ms() + 60_000)
                .unwrap(),
        ),
    );
    let foreign_unknown = daemon.register(
        &daemon.child_session_id(),
        "worker-a",
        "0.1.0",
        Some(
            &foreign_authority
                .mint_approval_token(
                    "ses_no-such-session",
                    "worker-a",
                    "0.1.0",
                    "d",
                    now_ms() + 60_000,
                )
                .unwrap(),
        ),
    );
    assert_eq!(foreign_known.raw, foreign_unknown.raw);
    assert_eq!(foreign_known.status, 403);
    assert_eq!(foreign_known.error(), IDENTITY_REFUSAL);
}

// ── 4. A credential cannot claim another session ──────────────────────────────

/// The session a credential names is the session judged, and the session bound into the approval
/// it earns. Neither can be redirected by the request body, and an approval does not outlive the
/// session that earned it.
#[test]
fn a_credential_is_judged_and_bound_as_the_session_it_names() {
    let daemon = Daemon::new();
    daemon.seed_caller(RICH_SESSION);
    daemon.seed(
        POOR_SESSION,
        "capabilities:\n  spawn:\n    allow: [worker-c]\n",
    );
    daemon.publish("worker-a", "0.1.0");
    let poor = daemon.credential(POOR_SESSION);
    let rich = daemon.credential(RICH_SESSION);

    // Judged against the poor session's own list, not the one it would like to be. The body's
    // extra keys naming the richer session select nothing.
    let redirected = daemon.post(
        "/spawn",
        &format!(
            r#"{{"name":"worker-a","version":"0.1.0","spawned_by":"{RICH_SESSION}","session_id":"{RICH_SESSION}"}}"#
        ),
        &[(capsule_runtime::SPAWN_CREDENTIAL_HEADER, &poor)],
    );
    assert_eq!(redirected.status, 403);
    assert_eq!(
        redirected.error(),
        "capsule 'worker-a' is not in parent's spawn_allow"
    );

    // And an approval the rich session earned stops being redeemable the moment that session is no
    // longer running: the `sid` bound into the token is checked against the job store.
    let approval = daemon.approval("worker-a", "0.1.0", &rich);
    daemon
        .state
        .jobs
        .lock()
        .unwrap()
        .get_mut(RICH_SESSION)
        .unwrap()
        .status = JobStatus::Complete;

    let response = daemon.register(
        &daemon.child_session_id(),
        "worker-a",
        "0.1.0",
        Some(&approval),
    );

    assert_eq!(response.status, 403);
    assert_eq!(response.error(), IDENTITY_REFUSAL);
    assert_eq!(daemon.session_ids(), vec![POOR_SESSION, RICH_SESSION]);
    assert!(
        daemon.workdir_entries().is_empty(),
        "{:?}",
        daemon.workdir_entries()
    );
}

// ── 5. An approval names a resolved artifact ──────────────────────────────────

/// An approval covers one artifact, identified by name, version and content hash. A different
/// name, a different version, or the same coordinate resolving to different bytes is refused.
#[test]
fn an_approval_covers_one_artifact_by_name_version_and_digest() {
    let daemon = Daemon::new();
    daemon.seed_caller(CALLER_SESSION);
    daemon.publish("worker-a", "0.1.0");
    daemon.publish("worker-a", "0.2.0");
    daemon.publish("worker-b", "0.1.0");
    let credential = daemon.credential(CALLER_SESSION);

    let other_name = daemon.register(
        &daemon.child_session_id(),
        "worker-b",
        "0.1.0",
        Some(&daemon.approval("worker-a", "0.1.0", &credential)),
    );
    assert_eq!(other_name.status, 403);
    assert_eq!(
        other_name.error(),
        "this spawn approval was granted for 'worker-a@0.1.0', not 'worker-b@0.1.0'"
    );

    let other_version = daemon.register(
        &daemon.child_session_id(),
        "worker-a",
        "0.2.0",
        Some(&daemon.approval("worker-a", "0.1.0", &credential)),
    );
    assert_eq!(other_version.status, 403);
    assert_eq!(
        other_version.error(),
        "this spawn approval was granted for 'worker-a@0.1.0', not 'worker-a@0.2.0'"
    );

    // Same coordinate, different bytes: the manifest the referee read is no longer the manifest
    // that would be launched.
    let approval = daemon.approval("worker-a", "0.1.0", &credential);
    daemon.publish_body(
        "worker-a",
        "0.1.0",
        common::PLAIN_WORKER_BODY,
        "republished with different bytes",
    );
    let other_bytes = daemon.register(
        &daemon.child_session_id(),
        "worker-a",
        "0.1.0",
        Some(&approval),
    );
    assert_eq!(other_bytes.status, 403);
    assert!(
        other_bytes
            .error()
            .starts_with("'worker-a@0.1.0' now resolves to a different artifact than the one this spawn approval was granted for"),
        "{}",
        other_bytes.error(),
    );

    assert_eq!(daemon.session_ids(), vec![CALLER_SESSION]);
    assert!(
        daemon.workdir_entries().is_empty(),
        "{:?}",
        daemon.workdir_entries()
    );
}

// ── 6. Expiry ─────────────────────────────────────────────────────────────────

/// The same fixture registers with a future expiry and is refused with a past one, so the refusal
/// is attributable to the expiry and to nothing else about the approval.
#[test]
fn an_approval_past_its_expiry_is_refused() {
    let daemon = Daemon::new();
    daemon.seed_caller(CALLER_SESSION);
    daemon.publish("worker-a", "0.1.0");
    let digest = LocalRegistry::new(daemon.registry.path())
        .resolve("worker-a", "0.1.0")
        .unwrap()
        .sha256;

    let mint = |expires_at_ms: u64| {
        daemon
            .state
            .authority
            .mint_approval_token(CALLER_SESSION, "worker-a", "0.1.0", &digest, expires_at_ms)
            .unwrap()
    };

    let expired = daemon.register(
        &daemon.child_session_id(),
        "worker-a",
        "0.1.0",
        Some(&mint(now_ms() - 1)),
    );
    assert_eq!(expired.status, 403);
    assert_eq!(
        expired.error(),
        "this spawn approval has passed its expiry; an approval is valid for 60 seconds from the \
         POST /spawn that granted it"
    );

    let live = daemon.register(
        &daemon.child_session_id(),
        "worker-a",
        "0.1.0",
        Some(&mint(now_ms() + 60_000)),
    );
    assert_eq!(live.status, 200, "{:?}", live.body);
}

// ── 7. One use ────────────────────────────────────────────────────────────────

#[test]
fn an_approval_covers_exactly_one_launch() {
    let daemon = Daemon::new();
    daemon.seed_caller(CALLER_SESSION);
    daemon.publish("worker-a", "0.1.0");
    let credential = daemon.credential(CALLER_SESSION);
    let approval = daemon.approval("worker-a", "0.1.0", &credential);

    let first = daemon.register(
        &daemon.child_session_id(),
        "worker-a",
        "0.1.0",
        Some(&approval),
    );
    let second = daemon.register(
        &daemon.child_session_id(),
        "worker-a",
        "0.1.0",
        Some(&approval),
    );

    assert_eq!(first.status, 200, "{:?}", first.body);
    assert_eq!(second.status, 403);
    assert_eq!(
        second.error(),
        "this spawn approval has already been redeemed; an approval covers one launch, so ask \
         POST /spawn for another"
    );
    // The seeded caller, plus exactly one registered session.
    assert_eq!(daemon.session_ids().len(), 2, "{:?}", daemon.session_ids());
}

// ── 8. The top-level path ─────────────────────────────────────────────────────

/// The operator's path needs no tokens, and a failed delegated registration never falls back to
/// it — not even for a name the operator listed.
#[test]
fn the_top_level_path_is_unchanged_and_needs_no_tokens() {
    let daemon = Daemon::with_spawn_allow(vec!["worker-a".to_string()]);
    daemon.seed_caller(CALLER_SESSION);
    daemon.publish("worker-a", "0.1.0");
    let credential = daemon.credential(CALLER_SESSION);

    let bare = daemon.register(&daemon.child_session_id(), "worker-a", "0.1.0", None);
    assert_eq!(bare.status, 200, "{:?}", bare.body);
    assert!(bare.body["credential"]
        .as_str()
        .unwrap()
        .starts_with("msc1."));

    // An approval that does not verify is a failed exchange, not a request to be judged by the
    // operator's list instead.
    let unusable = daemon.register(
        &daemon.child_session_id(),
        "worker-a",
        "0.1.0",
        Some("msa1.e30.e30"),
    );
    assert_eq!(unusable.status, 403);
    assert_eq!(unusable.error(), IDENTITY_REFUSAL);

    // And a credential is not an approval, whatever header it is placed in.
    let credential_as_approval = daemon.register(
        &daemon.child_session_id(),
        "worker-a",
        "0.1.0",
        Some(&credential),
    );
    assert_eq!(credential_as_approval.status, 403);
    assert_eq!(credential_as_approval.error(), IDENTITY_REFUSAL);
}

/// A credential is not an approval and an approval is not a credential: the two MAC domains keep
/// each family from verifying as the other.
#[test]
fn a_credential_cannot_be_presented_as_its_own_approval() {
    let daemon = Daemon::new();
    daemon.seed_caller(CALLER_SESSION);
    daemon.publish("worker-a", "0.1.0");
    let credential = daemon.credential(CALLER_SESSION);
    let approval = daemon.approval("worker-a", "0.1.0", &credential);

    // The credential, in the approval header.
    let as_approval = daemon.post(
        "/register",
        &format!(
            r#"{{"session_id":"{}","name":"worker-a","version":"0.1.0"}}"#,
            daemon.child_session_id()
        ),
        &[(SPAWN_APPROVAL_HEADER, &credential)],
    );
    assert_eq!(as_approval.status, 403);
    assert_eq!(as_approval.error(), IDENTITY_REFUSAL);

    // The approval, in the credential header.
    let as_credential = daemon.permission("worker-a", "0.1.0", Some(&approval));
    assert_eq!(as_credential.status, 403);
    assert_eq!(as_credential.error(), IDENTITY_REFUSAL);
}

/// The register body names the *child's* session id and nothing else. It cannot redirect which
/// session earned the approval, and it cannot overwrite a session already in the store.
#[test]
fn a_register_body_cannot_redirect_or_overwrite_a_session() {
    let daemon = Daemon::new();
    daemon.seed_caller(CALLER_SESSION);
    daemon.seed_caller(RICH_SESSION);
    daemon.publish("worker-a", "0.1.0");
    let credential = daemon.credential(CALLER_SESSION);

    // Registering under an id the store already holds is refused, so a later registrant cannot
    // replace a running session's envelope with one of its own choosing.
    let overwrite = daemon.register(
        RICH_SESSION,
        "worker-a",
        "0.1.0",
        Some(&daemon.approval("worker-a", "0.1.0", &credential)),
    );
    assert_eq!(overwrite.status, 403);
    assert_eq!(overwrite.error(), IDENTITY_REFUSAL);
    assert_eq!(
        daemon
            .state
            .jobs
            .lock()
            .unwrap()
            .get(RICH_SESSION)
            .unwrap()
            .envelope
            .spawn_allow
            .len(),
        4,
        "the seeded envelope was replaced"
    );

    // A fresh id succeeds, and is what the credential returned names.
    let child = daemon.child_session_id();
    let ok = daemon.register(
        &child,
        "worker-a",
        "0.1.0",
        Some(&daemon.approval("worker-a", "0.1.0", &credential)),
    );
    assert_eq!(ok.status, 200, "{:?}", ok.body);
    assert_eq!(
        daemon
            .state
            .authority
            .verify_credential(ok.body["credential"].as_str().unwrap())
            .unwrap(),
        child
    );
}

/// A session the job store no longer holds as running cannot delegate, whatever it still holds.
#[test]
fn a_credential_for_a_finished_session_is_refused() {
    let daemon = Daemon::new();
    daemon.seed_caller(CALLER_SESSION);
    daemon.publish("worker-a", "0.1.0");
    let credential = daemon.credential(CALLER_SESSION);
    daemon
        .state
        .jobs
        .lock()
        .unwrap()
        .get_mut(CALLER_SESSION)
        .unwrap()
        .status = JobStatus::Complete;

    let response = daemon.permission("worker-a", "0.1.0", Some(&credential));

    assert_eq!(response.status, 403);
    assert_eq!(response.error(), IDENTITY_REFUSAL);
}

/// `POST /spawn` decides and creates nothing: an approval, granted or refused, leaves the job
/// store and every directory as they were.
#[test]
fn asking_permission_creates_no_session() {
    let daemon = Daemon::new();
    daemon.seed_caller(CALLER_SESSION);
    daemon.publish("worker-a", "0.1.0");
    let credential = daemon.credential(CALLER_SESSION);

    daemon.approval("worker-a", "0.1.0", &credential);
    assert_eq!(daemon.permission("worker-z", "0.1.0", None).status, 403);

    assert_eq!(daemon.session_ids(), vec![CALLER_SESSION]);
    assert!(
        daemon.workdir_entries().is_empty(),
        "{:?}",
        daemon.workdir_entries()
    );
    // The registry holds published artifacts only — no session directory was created beside them.
    let registry_entries: Vec<String> = std::fs::read_dir(daemon.registry.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(registry_entries, vec!["worker-a"]);
}

/// `POST /delegate` is gone: the endpoint that granted an approval is now the one that names the
/// launch.
#[test]
fn the_delegate_endpoint_no_longer_exists() {
    let daemon = Daemon::new();
    daemon.seed_caller(CALLER_SESSION);
    daemon.publish("worker-a", "0.1.0");
    let credential = daemon.credential(CALLER_SESSION);

    let response = daemon.post(
        "/delegate",
        r#"{"name":"worker-a","version":"0.1.0"}"#,
        &[(capsule_runtime::SPAWN_CREDENTIAL_HEADER, &credential)],
    );

    assert_eq!(response.status, 404);
    assert_eq!(response.error(), "not found");
}
