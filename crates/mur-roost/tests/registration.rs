//! What a session tells the daemon about itself, and what the daemon believes.
//!
//! A registrant names an artifact. The envelope the daemon then holds is derived from *that
//! artifact's registry manifest*, so what a session is judged to hold never depends on what it
//! said about itself — a registrant that could state its grants would be a registrant that could
//! declare its own ceiling.

#[path = "common/mod.rs"]
mod common;

use capsule_runtime::SpawnEnvelope;
use common::{Daemon, IDENTITY_REFUSAL};
use murmur_artifact::RuntimeManifest;

const PARENT_SESSION: &str = "ses_0000000000000000000000000000parent";

/// The manifest a registrant claims changes nothing: the envelope the daemon holds is
/// `SpawnEnvelope::from_runtime_manifest` of the *registry* manifest, verbatim.
#[test]
fn a_registrant_does_not_get_to_state_its_own_grants() {
    let daemon = Daemon::with_spawn_allow(vec!["worker".to_string()]);
    let published = "artifacts: []\ncapabilities:\n  network:\n    allow: [registry.internal]\n  \
                     env:\n    allow: [TZ]\n  spawn:\n    allow: [grandchild]\n";
    daemon.publish_body("worker", "0.1.0", published, "");
    let child = daemon.child_session_id();

    let response = daemon.register_body(
        &format!(
            r#"{{"session_id":"{child}","name":"worker","version":"0.1.0",
                 "capabilities":{{"network":{{"allow":["evil.example.com"]}},
                                  "shell":{{"allow":["bash"]}}}},
                 "envelope":{{"network_allow":["evil.example.com"],"shell_allow":["bash"]}}}}"#
        ),
        None,
    );

    assert_eq!(response.status, 200, "{:?}", response.body);
    let held = daemon
        .state
        .jobs
        .lock()
        .unwrap()
        .get(&child)
        .unwrap()
        .envelope
        .clone();
    let from_registry = SpawnEnvelope::from_runtime_manifest(
        &RuntimeManifest::from_yaml_str(&common::manifest_yaml("worker", "0.1.0", published))
            .unwrap(),
    );
    assert_eq!(held.network_allow, from_registry.network_allow);
    assert_eq!(held.shell_allow, from_registry.shell_allow);
    assert_eq!(held.env_allow, from_registry.env_allow);
    assert_eq!(held.spawn_allow, from_registry.spawn_allow);
    assert!(held.shell_allow.is_empty(), "{:?}", held.shell_allow);
    assert_eq!(held.network_allow, vec!["registry.internal"]);
}

/// A registration presenting no approval is admitted only for a name the operator listed.
#[test]
fn a_registration_without_an_approval_needs_the_operators_list() {
    let daemon = Daemon::with_spawn_allow(vec!["listed".to_string()]);
    daemon.publish("listed", "0.1.0");
    daemon.publish("unlisted", "0.1.0");

    let listed = daemon.register(&daemon.child_session_id(), "listed", "0.1.0", None);
    assert_eq!(listed.status, 200, "{:?}", listed.body);

    let unlisted = daemon.register(&daemon.child_session_id(), "unlisted", "0.1.0", None);
    assert_eq!(unlisted.status, 403);
    assert_eq!(
        unlisted.error(),
        "capsule 'unlisted' is not in --spawn-allow"
    );
}

/// An approval minted for one capsule does not admit another.
#[test]
fn an_approval_for_a_different_capsule_is_refused() {
    let daemon = Daemon::new();
    daemon.seed_caller(PARENT_SESSION);
    daemon.publish("worker-a", "0.1.0");
    daemon.publish("worker-b", "0.1.0");
    let credential = daemon.credential(PARENT_SESSION);
    let approval = daemon.approval("worker-a", "0.1.0", &credential);

    let response = daemon.register(
        &daemon.child_session_id(),
        "worker-b",
        "0.1.0",
        Some(&approval),
    );

    assert_eq!(response.status, 403);
    assert_eq!(
        response.error(),
        "this spawn approval was granted for 'worker-a@0.1.0', not 'worker-b@0.1.0'"
    );
}

/// A valid approval replayed at a second registration is refused, and the second session is never
/// recorded.
#[test]
fn a_replayed_approval_is_refused() {
    let daemon = Daemon::new();
    daemon.seed_caller(PARENT_SESSION);
    daemon.publish("worker", "0.1.0");
    let credential = daemon.credential(PARENT_SESSION);
    let approval = daemon.approval("worker", "0.1.0", &credential);

    let first_child = daemon.child_session_id();
    let second_child = daemon.child_session_id();
    assert_eq!(
        daemon
            .register(&first_child, "worker", "0.1.0", Some(&approval))
            .status,
        200
    );
    let replay = daemon.register(&second_child, "worker", "0.1.0", Some(&approval));

    assert_eq!(replay.status, 403);
    assert_eq!(
        replay.error(),
        "this spawn approval has already been redeemed; an approval covers one launch, so ask \
         POST /spawn for another"
    );
    assert!(!daemon.session_ids().contains(&second_child));
}

/// One registration, then the session is visible and delegating: registering is the only way the
/// daemon comes to know a session at all.
#[test]
fn a_registered_session_is_running_and_can_delegate() {
    let daemon = Daemon::with_spawn_allow(vec!["parent".to_string()]);
    daemon.publish_body(
        "parent",
        "0.1.0",
        "artifacts: []\ncapabilities:\n  network:\n    allow: [registry.internal]\n  \
         spawn:\n    allow: [worker]\n",
        "",
    );
    daemon.publish("worker", "0.1.0");
    let session = daemon.child_session_id();

    let registered = daemon.register(&session, "parent", "0.1.0", None);
    assert_eq!(registered.status, 200, "{:?}", registered.body);
    let credential = registered.body["credential"].as_str().unwrap().to_string();

    assert_eq!(daemon.status(&session).body["status"], "running");
    assert_eq!(
        daemon
            .permission("worker", "0.1.0", Some(&credential))
            .status,
        200
    );
}

/// Deregistration retires the record and, with it, everything the credential could still do.
#[test]
fn deregistering_ends_what_the_credential_can_do() {
    let daemon = Daemon::with_spawn_allow(vec!["parent".to_string()]);
    daemon.publish_body(
        "parent",
        "0.1.0",
        "artifacts: []\ncapabilities:\n  spawn:\n    allow: [worker]\n",
        "",
    );
    daemon.publish("worker", "0.1.0");
    let session = daemon.child_session_id();
    let credential = daemon.register(&session, "parent", "0.1.0", None).body["credential"]
        .as_str()
        .unwrap()
        .to_string();

    assert_eq!(daemon.deregister(&credential, "complete").status, 200);
    assert_eq!(daemon.status(&session).body["status"], "complete");

    let after = daemon.permission("worker", "0.1.0", Some(&credential));
    assert_eq!(after.status, 403);
    assert_eq!(after.error(), IDENTITY_REFUSAL);

    // And a second deregistration has nothing left to retire.
    assert_eq!(daemon.deregister(&credential, "complete").status, 403);
}

#[test]
fn a_failed_outcome_is_recorded_as_failed() {
    let daemon = Daemon::with_spawn_allow(vec!["worker".to_string()]);
    daemon.publish("worker", "0.1.0");
    let session = daemon.child_session_id();
    let credential = daemon.register(&session, "worker", "0.1.0", None).body["credential"]
        .as_str()
        .unwrap()
        .to_string();

    assert_eq!(daemon.deregister(&credential, "failed").status, 200);
    assert_eq!(daemon.status(&session).body["status"], "failed");
}

#[test]
fn deregistering_needs_a_credential_and_a_known_outcome() {
    let daemon = Daemon::with_spawn_allow(vec!["worker".to_string()]);
    daemon.publish("worker", "0.1.0");
    let session = daemon.child_session_id();
    let credential = daemon.register(&session, "worker", "0.1.0", None).body["credential"]
        .as_str()
        .unwrap()
        .to_string();

    let no_credential = daemon.post("/deregister", r#"{"outcome":"complete"}"#, &[]);
    assert_eq!(no_credential.status, 403);
    assert_eq!(no_credential.error(), IDENTITY_REFUSAL);

    let unknown_outcome = daemon.deregister(&credential, "abandoned");
    assert_eq!(unknown_outcome.status, 400);
    assert_eq!(
        unknown_outcome.error(),
        "outcome 'abandoned' is not 'complete' or 'failed'"
    );
    // Refused, so the session is still running.
    assert_eq!(daemon.status(&session).body["status"], "running");
}

#[test]
fn a_registration_of_an_unknown_artifact_is_a_server_error() {
    let daemon = Daemon::with_spawn_allow(vec!["worker".to_string()]);

    let response = daemon.register(&daemon.child_session_id(), "worker", "0.1.0", None);

    assert_eq!(response.status, 500);
    assert!(response.error().starts_with("registry error for"));
}

#[test]
fn a_registration_with_an_empty_session_id_is_a_bad_request() {
    let daemon = Daemon::with_spawn_allow(vec!["worker".to_string()]);
    daemon.publish("worker", "0.1.0");

    let response = daemon.register("", "worker", "0.1.0", None);

    assert_eq!(response.status, 400);
    assert_eq!(response.error(), "session_id must not be empty");
}

/// The manifest of a capsule that talks to a provider: the key is an environment reference, and
/// the variable it names is deliberately one no test process sets.
const PROVIDER_WORKER_BODY: &str = "artifacts: []\ncapabilities:\n  network:\n    allow: [api.provider.internal]\ninference:\n  transport: http\n  endpoint: https://api.provider.internal/v1\n  model: test-model\n  api_key: ${ROOST_MUST_NEVER_READ}\n  driver:\n    artifact: murmur-driver-anthropic\n";

/// The referee holds no provider credential, so it reads a capsule's manifest for its capability
/// policy without resolving the key that manifest references.
#[test]
fn a_capsule_whose_key_the_daemon_does_not_hold_still_registers() {
    let daemon = Daemon::with_spawn_allow(vec!["listed".to_string()]);
    daemon.publish_body("listed", "0.1.0", PROVIDER_WORKER_BODY, "");
    let session = daemon.child_session_id();

    let response = daemon.register(&session, "listed", "0.1.0", None);

    assert_eq!(response.status, 200, "{:?}", response.body);
    assert_eq!(daemon.status(&session).body["status"], "running");
    // Nothing the daemon says names a variable out of a capsule it does not own the secrets for.
    assert!(!response.raw.contains("ROOST_MUST_NEVER_READ"));
    assert!(!response.raw.contains("${"));
}

/// The operator's list is consulted before the registry is read, so a name nobody listed is
/// refused for not being listed — whatever the registry would have said about it.
#[test]
fn an_unlisted_name_is_refused_before_the_registry_is_read() {
    let daemon = Daemon::with_spawn_allow(vec!["listed".to_string()]);
    daemon.publish_body("unlisted", "0.1.0", PROVIDER_WORKER_BODY, "");

    let published = daemon.register(&daemon.child_session_id(), "unlisted", "0.1.0", None);
    assert_eq!(published.status, 403, "{:?}", published.body);
    assert_eq!(
        published.error(),
        "capsule 'unlisted' is not in --spawn-allow"
    );

    // The same refusal for a name that is in neither the list nor the registry: the allow list
    // answers first, so this is not a registry error.
    let absent = daemon.register(&daemon.child_session_id(), "unlisted", "9.9.9", None);
    assert_eq!(absent.status, 403, "{:?}", absent.body);
    assert_eq!(absent.error(), "capsule 'unlisted' is not in --spawn-allow");
}

/// The whole delegated exchange, between two capsules that both reference a provider key the
/// daemon does not hold.
#[test]
fn a_delegation_between_two_provider_capsules_needs_no_key() {
    let daemon = Daemon::with_spawn_allow(vec!["listed".to_string()]);
    let parent_body = "artifacts: []\ncapabilities:\n  network:\n    allow: [api.provider.internal]\n  spawn:\n    allow: [child]\ninference:\n  transport: http\n  endpoint: https://api.provider.internal/v1\n  model: test-model\n  api_key: ${ROOST_MUST_NEVER_READ}\n  driver:\n    artifact: murmur-driver-anthropic\n";
    daemon.publish_body("listed", "0.1.0", parent_body, "");
    daemon.publish_body("child", "0.1.0", PROVIDER_WORKER_BODY, "");

    let parent_session = daemon.child_session_id();
    let registered = daemon.register(&parent_session, "listed", "0.1.0", None);
    assert_eq!(registered.status, 200, "{:?}", registered.body);
    let credential = registered.body["credential"].as_str().unwrap().to_string();

    let permission = daemon.permission("child", "0.1.0", Some(&credential));
    assert_eq!(permission.status, 200, "{:?}", permission.body);
    assert!(!permission.raw.contains("ROOST_MUST_NEVER_READ"));

    let child_session = daemon.child_session_id();
    let approval = permission.body["approval"].as_str().unwrap().to_string();
    let child = daemon.register(&child_session, "child", "0.1.0", Some(&approval));
    assert_eq!(child.status, 200, "{:?}", child.body);
    assert_eq!(daemon.status(&child_session).body["status"], "running");
}
