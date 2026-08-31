//! The spawn referee: a child capsule can never hold more capability than the capsule that asked
//! for it.
//!
//! The parent's envelope is seeded straight into the job store, which is exactly where a
//! registered session's envelope comes from, and a credential is minted for it from the daemon's
//! own authority — the same two facts a registered parent would leave behind. The referee runs at
//! `POST /spawn`, so [`common::Daemon::spawn`] performs the whole exchange — ask, then register —
//! and returns the first refusal it meets.
//!
//! What binds a caller to a session, and an approval to an artifact, is asserted in
//! `spawn_binding.rs`. Here the question is only what the referee compares.

#[path = "common/mod.rs"]
mod common;

use common::Daemon;

const PARENT_SESSION: &str = "ses_0000000000000000000000000000parent";

/// A child within its parent on every axis is approved, and registers.
#[test]
fn a_child_within_its_parents_envelope_launches() {
    let daemon = Daemon::new();
    daemon.seed(
        PARENT_SESSION,
        "capabilities:\n  network:\n    allow: [registry.internal, api.example.com]\n  \
         env:\n    allow: [MURMUR_TEST_ALLOWED_VAR]\n  \
         filesystem:\n    scope: data\n  \
         spawn:\n    allow: [worker]\n",
    );
    daemon.publish_body(
        "worker",
        "0.1.0",
        "artifacts: []\ncapabilities:\n  network:\n    allow: [registry.internal]\n  \
         filesystem:\n    scope: data/in\n",
        "",
    );

    let response = daemon.spawn("worker", Some(PARENT_SESSION));

    assert_eq!(response.status, 200, "{:?}", response.body);
    let credential = response.body["credential"].as_str().unwrap();
    assert!(credential.starts_with("msc1."), "{credential}");
}

/// The refusal names the manifest key and the entry, not a bare "denied" — and not the name-list
/// message, which is a different refusal about a different question.
#[test]
fn a_network_host_the_parent_does_not_hold_is_refused_by_axis_and_entry() {
    let daemon = Daemon::new();
    daemon.seed(
        PARENT_SESSION,
        "capabilities:\n  network:\n    allow: [registry.internal]\n  \
         spawn:\n    allow: [worker]\n",
    );
    daemon.publish_body(
        "worker",
        "0.1.0",
        "artifacts: []\ncapabilities:\n  network:\n    allow: [api.example.com]\n",
        "",
    );

    let response = daemon.spawn("worker", Some(PARENT_SESSION));

    assert_eq!(response.status, 403);
    let error = response.error();
    assert!(error.contains("capabilities.network.allow"), "{error}");
    assert!(error.contains("api.example.com"), "{error}");
    assert!(error.contains("its parent does not hold"), "{error}");
    assert!(!error.contains("spawn_allow"), "{error}");
}

/// One case per axis, each child exceeding on exactly one of them. Every case asserts the axis it
/// exceeded *and* that no other axis's manifest key appears, so a refusal can never blame a
/// neighbouring axis.
#[test]
fn every_axis_refuses_naming_its_own_manifest_key_and_entry() {
    // `(child name, parent capabilities, child capabilities, manifest key, offending entry)`.
    let cases: &[(&str, &str, &str, &str, &str)] = &[
        (
            "child-network",
            "  network:\n    allow: [registry.internal]\n",
            "  network:\n    allow: [api.example.com]\n",
            "capabilities.network.allow",
            "api.example.com",
        ),
        (
            "child-unix-sockets",
            "  network:\n    allow: [registry.internal]\n    unix_sockets: false\n",
            "  network:\n    allow: [registry.internal]\n    unix_sockets: true\n",
            "capabilities.network.unix_sockets",
            "true",
        ),
        (
            "child-peer-fetch",
            "  peer_fetch:\n    allow: [peer.internal]\n",
            "  peer_fetch:\n    allow: [other.internal]\n",
            "capabilities.peer_fetch.allow",
            "other.internal",
        ),
        (
            "child-shell",
            "  shell:\n    allow: [git]\n",
            "  shell:\n    allow: [curl]\n",
            "capabilities.shell.allow",
            "curl",
        ),
        (
            "child-spawn",
            "",
            "  spawn:\n    allow: [grandchild]\n",
            "capabilities.spawn.allow",
            "grandchild",
        ),
        (
            "child-env",
            "  env:\n    allow: [HOME]\n",
            "  env:\n    allow: [GITHUB_TOKEN]\n",
            "capabilities.env.allow",
            "GITHUB_TOKEN",
        ),
        (
            "child-scope-sibling",
            "  filesystem:\n    scope: data\n",
            "  filesystem:\n    scope: other\n",
            "capabilities.filesystem.scope",
            "other",
        ),
        (
            "child-scope-escape",
            "  filesystem:\n    scope: data\n",
            "  filesystem:\n    scope: data/../other\n",
            "capabilities.filesystem.scope",
            "data/../other",
        ),
        (
            "child-scope-absent",
            "  filesystem:\n    scope: data\n",
            "",
            "capabilities.filesystem.scope",
            "data",
        ),
        (
            "child-workdir-exec",
            "  filesystem:\n    scope: data\n    workdir_exec: false\n",
            "  filesystem:\n    scope: data\n    workdir_exec: true\n",
            "capabilities.filesystem.workdir_exec",
            "true",
        ),
        (
            "child-containment",
            "  containment: scoped\n",
            "  containment: advisory\n",
            "capabilities.containment",
            "advisory",
        ),
    ];

    // A store is granted per artifact, so this case declares one on each side rather than at
    // capsule level, where a `state:` block grants nothing at all.
    let state_case = (
        "child-state",
        "artifacts:\n  - name: writer\n    version: 0.1.0\n    runtime: tool\n    \
         capabilities:\n      state:\n        store: parent-notes\n",
        "artifacts:\n  - name: writer\n    version: 0.1.0\n    runtime: tool\n    \
         capabilities:\n      state:\n        store: child-notes\n",
        "capabilities.state.store",
        "child-notes",
    );

    let every_key = [
        "capabilities.network.allow",
        "capabilities.network.unix_sockets",
        "capabilities.peer_fetch.allow",
        "capabilities.shell.allow",
        "capabilities.spawn.allow",
        "capabilities.env.allow",
        "capabilities.filesystem.scope",
        "capabilities.filesystem.workdir_exec",
        "capabilities.state.store",
        "capabilities.containment",
    ];

    let capability_cases = cases.iter().map(|(name, parent, child, key, entry)| {
        (
            *name,
            format!("artifacts: []\ncapabilities:\n  spawn:\n    allow: [{name}]\n{parent}"),
            format!("artifacts: []\ncapabilities:\n{child}"),
            *key,
            *entry,
        )
    });
    let (state_name, state_parent, state_child, state_key, state_entry) = state_case;
    let all_cases = capability_cases.chain(std::iter::once((
        state_name,
        format!("{state_parent}capabilities:\n  spawn:\n    allow: [{state_name}]\n"),
        state_child.to_string(),
        state_key,
        state_entry,
    )));

    for (name, parent_body, child_body, key, entry) in all_cases {
        let daemon = Daemon::new();
        daemon.seed(PARENT_SESSION, &parent_body);
        daemon.publish_body(name, "0.1.0", &child_body, "");

        let response = daemon.spawn(name, Some(PARENT_SESSION));

        assert_eq!(response.status, 403, "{name}");
        let error = response.error();
        assert!(error.contains(key), "{name}: {error}");
        assert!(error.contains(entry), "{name}: {error}");
        for other in every_key {
            // `capabilities.filesystem.scope` is a prefix of nothing, but
            // `capabilities.network.allow` is not a prefix of `…unix_sockets`, so a plain
            // containment check is enough to catch a refusal blaming the wrong axis.
            if other != key {
                assert!(!error.contains(other), "{name} named '{other}': {error}");
            }
        }
    }
}

/// A floor may rise, and the referee is not the thing that judges whether the host can back it.
///
/// The daemon holds no capsule runtime and takes no host probe, so a `sealed`-declaring child
/// clears this endpoint on every host. Whether the floor is actually achievable is decided in the
/// child's own process, at its own launch — see
/// `capsule-runtime`'s `child_launch::a_sealed_child_either_achieves_its_floor_or_is_refused`.
#[test]
fn a_raised_containment_floor_clears_the_referee_and_is_judged_by_the_child_itself() {
    let daemon = Daemon::new();
    daemon.seed(
        PARENT_SESSION,
        "capabilities:\n  containment: scoped\n  spawn:\n    allow: [worker]\n",
    );
    daemon.publish_body(
        "worker",
        "0.1.0",
        "artifacts: []\ncapabilities:\n  containment: sealed\n",
        "",
    );

    let response = daemon.spawn("worker", Some(PARENT_SESSION));

    assert!(
        !format!("{:?}", response.body).contains("capabilities.containment"),
        "the referee must not treat a raised floor as an escalation: {:?}",
        response.body,
    );
    assert_eq!(response.status, 200, "{:?}", response.body);
}

/// The name check runs first and is a separate refusal: a child within its parent on every axis is
/// still refused when its name is absent from the parent's own list, and no axis is named.
#[test]
fn the_name_check_runs_first_and_names_no_axis() {
    let daemon = Daemon::new();
    daemon.seed(
        PARENT_SESSION,
        "capabilities:\n  network:\n    allow: [registry.internal]\n  \
         spawn:\n    allow: [some-other-worker]\n",
    );
    daemon.publish_body("worker", "0.1.0", "artifacts: []\n", "");

    let response = daemon.spawn("worker", Some(PARENT_SESSION));

    assert_eq!(response.status, 403);
    let error = response.error();
    assert_eq!(error, "capsule 'worker' is not in parent's spawn_allow");
}

/// With no parent there is no envelope to be within, so the global list is the only gate — even
/// for a manifest whose grants would exceed any parent.
#[test]
fn a_top_level_spawn_has_no_parent_to_be_within() {
    let daemon = Daemon::with_spawn_allow(vec!["worker".to_string()]);
    daemon.publish_body(
        "worker",
        "0.1.0",
        "artifacts: []\ncapabilities:\n  network:\n    allow: [api.example.com]\n    \
         unix_sockets: true\n  filesystem:\n    workdir_exec: true\n  \
         env:\n    allow: [GITHUB_TOKEN]\n",
        "",
    );

    let response = daemon.spawn("worker", None);

    assert_eq!(response.status, 200, "{:?}", response.body);
    assert!(response.body["credential"]
        .as_str()
        .unwrap()
        .starts_with("msc1."));
}

#[test]
fn a_top_level_spawn_of_an_unlisted_name_is_still_refused_by_the_flag() {
    let daemon = Daemon::new();
    daemon.publish("worker", "0.1.0");

    let response = daemon.spawn("worker", None);

    assert_eq!(response.status, 403);
    assert_eq!(response.error(), "capsule 'worker' is not in --spawn-allow");
}

/// The referee reads the registry manifest, never the request. A body carrying its own narrow,
/// within-envelope declaration changes nothing.
#[test]
fn a_manifest_supplied_in_the_request_is_not_consulted() {
    let daemon = Daemon::new();
    daemon.seed(
        PARENT_SESSION,
        "capabilities:\n  network:\n    allow: [registry.internal]\n  \
         spawn:\n    allow: [worker]\n",
    );
    daemon.publish_body(
        "worker",
        "0.1.0",
        "artifacts: []\ncapabilities:\n  network:\n    allow: [api.example.com]\n",
        "",
    );
    let credential = daemon.credential(PARENT_SESSION);

    let response = daemon.post(
        "/spawn",
        r#"{"name":"worker","version":"0.1.0",
            "manifest":{"name":"worker","version":"0.1.0","capabilities":{"network":{"allow":["registry.internal"]}}},
            "capabilities":{"network":{"allow":["registry.internal"]}}}"#,
        &[(capsule_runtime::SPAWN_CREDENTIAL_HEADER, &credential)],
    );

    assert_eq!(response.status, 403);
    assert!(
        response.error().contains("api.example.com"),
        "{}",
        response.error()
    );
}

/// A refused spawn creates nothing: no job record, no session directory anywhere, and therefore no
/// trace.
#[test]
fn a_refused_spawn_creates_nothing() {
    let daemon = Daemon::new();
    daemon.seed(
        PARENT_SESSION,
        "capabilities:\n  network:\n    allow: [registry.internal]\n  \
         spawn:\n    allow: [worker]\n",
    );
    daemon.publish_body(
        "worker",
        "0.1.0",
        "artifacts: []\ncapabilities:\n  network:\n    allow: [api.example.com]\n",
        "",
    );

    assert_eq!(daemon.spawn("worker", Some(PARENT_SESSION)).status, 403);

    let jobs = daemon.state.jobs.lock().unwrap();
    assert_eq!(jobs.len(), 1);
    assert!(jobs.contains_key(PARENT_SESSION));
    drop(jobs);

    assert!(
        daemon.workdir_entries().is_empty(),
        "workdir is not empty: {:?}",
        daemon.workdir_entries()
    );
}
