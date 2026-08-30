//! The spawn referee: a child capsule can never hold more capability than the capsule that asked
//! for it.
//!
//! Every case drives [`mur_roost::route`] rather than a socket — it is the same entry point the
//! connection handler reaches, with only the framing removed, so a refusal asserted here is the
//! refusal a caller receives.
//!
//! The parent's envelope is seeded straight into the job store, which is exactly where a real
//! parent's envelope comes from, and a credential is minted for it from the daemon's own
//! authority — the same two facts a launched parent would leave behind. The referee itself now
//! runs at `POST /delegate`, so [`Daemon::spawn`] performs the whole exchange and returns the
//! first refusal it meets.
//!
//! What binds a caller to a session, and an approval to an artifact, is asserted in
//! `spawn_binding.rs`. Here the question is only what the referee compares.

use std::collections::HashMap;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use capsule_runtime::{SpawnEnvelope, SPAWN_APPROVAL_HEADER, SPAWN_CREDENTIAL_HEADER};
use mur_roost::{authority::SpawnAuthority, JobRecord, JobStatus, RequestHeaders, State};
use murmur_artifact::{ArtifactMeta, LocalRegistry, Registry, RuntimeManifest, RuntimeType};
use tempfile::TempDir;

/// A script capsule that needs no artifacts and no host grants: it probes two environment
/// variables and writes what it saw into its own preopen. Read from `murmur-cli`'s fixture
/// directory, so both suites launch the same component bytes.
const CAPSULE_COMPONENT: &str = "capsule-env-echo.wasm";

const PARENT_SESSION: &str = "ses_0000000000000000000000000000parent";

fn fixture_component(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("murmur-cli")
        .join("tests")
        .join("fixtures")
        .join("run")
        .join("components")
        .join(name)
}

/// A capsule manifest body: everything below `name:`/`version:`, indented as the operator wrote it.
fn manifest_yaml(name: &str, body: &str) -> String {
    format!("name: {name}\nversion: 0.1.0\n{body}")
}

/// Pack a `.mur.zip` with the given manifest and a root `capsule.wasm`, and publish it where
/// `resolve_with_platform` will find it.
fn publish_capsule(registry_root: &Path, name: &str, body: &str) {
    let mut cursor = Cursor::new(Vec::<u8>::new());
    {
        let mut zip = zip::ZipWriter::new(&mut cursor);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zip.start_file("murmur.yaml", options).unwrap();
        zip.write_all(manifest_yaml(name, body).as_bytes()).unwrap();
        zip.start_file("capsule.wasm", options).unwrap();
        zip.write_all(&std::fs::read(fixture_component(CAPSULE_COMPONENT)).unwrap())
            .unwrap();
        zip.finish().unwrap();
    }

    LocalRegistry::new(registry_root)
        .publish(
            ArtifactMeta {
                name: name.to_string(),
                version: "0.1.0".to_string(),
                runtime: RuntimeType::Wasm,
                artifact_runtime: "capsule".to_string(),
                platforms: Vec::new(),
                description: None,
                tags: Vec::new(),
            },
            &cursor.into_inner(),
        )
        .unwrap();
}

fn envelope_from(name: &str, body: &str) -> SpawnEnvelope {
    SpawnEnvelope::from_runtime_manifest(
        &RuntimeManifest::from_yaml_str(&manifest_yaml(name, body))
            .expect("parent manifest fixture must parse"),
    )
}

struct Daemon {
    state: Arc<State>,
    registry: TempDir,
    workdir: TempDir,
}

impl Daemon {
    /// A daemon with no `--spawn-allow` names at all, so every top-level spawn is refused and only
    /// the seeded parent can delegate.
    fn new() -> Self {
        Self::with_spawn_allow(Vec::new())
    }

    fn with_spawn_allow(spawn_allow: Vec<String>) -> Self {
        let registry = TempDir::new().unwrap();
        let workdir = TempDir::new().unwrap();
        let state = Arc::new(State {
            jobs: Arc::new(Mutex::new(HashMap::new())),
            registry_path: registry.path().to_path_buf(),
            spawn_allow,
            authority: Arc::new(SpawnAuthority::generate().unwrap()),
        });
        Self {
            state,
            registry,
            workdir,
        }
    }

    fn seed_parent(&self, body: &str) {
        self.state.jobs.lock().unwrap().insert(
            PARENT_SESSION.to_string(),
            JobRecord {
                status: JobStatus::Running,
                envelope: envelope_from("parent", body),
            },
        );
    }

    fn publish(&self, name: &str, body: &str) {
        publish_capsule(self.registry.path(), name, body);
    }

    /// A credential minted by this daemon for the named session, exactly as the session's own
    /// runtime would have received it at launch.
    fn credential(&self, session_id: &str) -> String {
        self.state
            .authority
            .mint_credential_token(session_id)
            .unwrap()
    }

    /// The whole exchange a runtime performs: `/delegate` for an approval, then `/spawn` to redeem
    /// it. A refusal at either step is the response, so a test asserting the referee's wording
    /// asserts what a caller actually receives.
    ///
    /// `spawned_by: None` is the operator's top-level path, which carries no headers at all.
    fn spawn(&self, name: &str, spawned_by: Option<&str>) -> Response {
        let Some(session_id) = spawned_by else {
            return self.post_spawn(&self.spawn_body(name, None), None, None);
        };
        let credential = self.credential(session_id);
        let delegation = self.delegate(name, "0.1.0", Some(&credential));
        if delegation.status != 200 {
            return delegation;
        }
        let approval = delegation.body["approval"].as_str().unwrap().to_string();
        self.post_spawn(
            &self.spawn_body(name, Some(session_id)),
            Some(&credential),
            Some(&approval),
        )
    }

    fn spawn_body(&self, name: &str, spawned_by: Option<&str>) -> String {
        let spawned_by = spawned_by
            .map(|id| format!(r#","spawned_by":"{id}""#))
            .unwrap_or_default();
        format!(
            r#"{{"name":"{name}","version":"0.1.0","workdir":{}{spawned_by}}}"#,
            serde_json::to_string(&self.workdir.path().display().to_string()).unwrap(),
        )
    }

    fn delegate(&self, name: &str, version: &str, credential: Option<&str>) -> Response {
        self.post_delegate(
            &format!(r#"{{"name":"{name}","version":"{version}"}}"#),
            credential,
        )
    }

    fn post_delegate(&self, body: &str, credential: Option<&str>) -> Response {
        let mut headers = RequestHeaders::new();
        if let Some(credential) = credential {
            headers.insert(SPAWN_CREDENTIAL_HEADER, credential);
        }
        Response::parse(&mur_roost::route(
            "POST",
            "/delegate",
            &headers,
            body,
            &self.state,
        ))
    }

    fn post_spawn(&self, body: &str, credential: Option<&str>, approval: Option<&str>) -> Response {
        let mut headers = RequestHeaders::new();
        if let Some(credential) = credential {
            headers.insert(SPAWN_CREDENTIAL_HEADER, credential);
        }
        if let Some(approval) = approval {
            headers.insert(SPAWN_APPROVAL_HEADER, approval);
        }
        Response::parse(&mur_roost::route(
            "POST",
            "/spawn",
            &headers,
            body,
            &self.state,
        ))
    }
}

struct Response {
    status: u16,
    body: serde_json::Value,
}

impl Response {
    fn parse(raw: &str) -> Self {
        let (head, body) = raw.split_once("\r\n\r\n").expect("response has a body");
        let status = head
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .expect("status line carries a code");
        Self {
            status,
            body: serde_json::from_str(body).expect("body is JSON"),
        }
    }

    fn error(&self) -> &str {
        self.body["error"]
            .as_str()
            .expect("refusal carries an error")
    }
}

/// A child within its parent on every axis is approved, and launches.
#[test]
fn a_child_within_its_parents_envelope_launches() {
    let daemon = Daemon::new();
    daemon.seed_parent(
        "capabilities:\n  network:\n    allow: [registry.internal, api.example.com]\n  \
         env:\n    allow: [MURMUR_TEST_ALLOWED_VAR]\n  \
         filesystem:\n    scope: data\n  \
         spawn:\n    allow: [worker]\n",
    );
    daemon.publish(
        "worker",
        "artifacts: []\ncapabilities:\n  network:\n    allow: [registry.internal]\n  \
         filesystem:\n    scope: data/in\n",
    );

    let response = daemon.spawn("worker", Some(PARENT_SESSION));

    assert_eq!(response.status, 200, "{:?}", response.body);
    let session_id = response.body["session_id"].as_str().unwrap();
    assert!(session_id.starts_with("ses_"), "{session_id}");
    assert!(response.body.get("capsule_url").is_some());
}

/// The refusal names the manifest key and the entry, not a bare "denied" — and not the name-list
/// message, which is a different refusal about a different question.
#[test]
fn a_network_host_the_parent_does_not_hold_is_refused_by_axis_and_entry() {
    let daemon = Daemon::new();
    daemon.seed_parent(
        "capabilities:\n  network:\n    allow: [registry.internal]\n  \
         spawn:\n    allow: [worker]\n",
    );
    daemon.publish(
        "worker",
        "artifacts: []\ncapabilities:\n  network:\n    allow: [api.example.com]\n",
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
        daemon.seed_parent(&parent_body);
        daemon.publish(name, &child_body);

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

/// A floor may rise. A child raising it clears the referee, and is then judged by the runtime's own
/// containment gate against what this host can actually back — a different, and correct, refusal.
#[test]
fn a_raised_containment_floor_clears_the_referee_and_meets_the_hosts_own_gate() {
    let daemon = Daemon::new();
    daemon.seed_parent("capabilities:\n  containment: scoped\n  spawn:\n    allow: [worker]\n");
    daemon.publish(
        "worker",
        "artifacts: []\ncapabilities:\n  containment: sealed\n",
    );

    let response = daemon.spawn("worker", Some(PARENT_SESSION));

    assert!(
        !format!("{:?}", response.body).contains("capabilities.containment"),
        "the referee must not treat a raised floor as an escalation: {:?}",
        response.body,
    );

    if capsule_runtime::detect_achieved_containment() == murmur_artifact::ContainmentClass::Sealed {
        assert_eq!(response.status, 200, "{:?}", response.body);
    } else {
        assert_eq!(response.status, 500, "{:?}", response.body);
        let error = response.error();
        assert!(
            error.contains("declared containment class 'sealed'"),
            "{error}"
        );
        assert!(error.contains("is not achievable on this host"), "{error}");
    }
}

/// The name check runs first and is a separate refusal: a child within its parent on every axis is
/// still refused when its name is absent from the parent's own list, and no axis is named.
#[test]
fn the_name_check_runs_first_and_names_no_axis() {
    let daemon = Daemon::new();
    daemon.seed_parent(
        "capabilities:\n  network:\n    allow: [registry.internal]\n  \
         spawn:\n    allow: [some-other-worker]\n",
    );
    daemon.publish("worker", "artifacts: []\n");

    let response = daemon.spawn("worker", Some(PARENT_SESSION));

    assert_eq!(response.status, 403);
    let error = response.error();
    assert_eq!(error, "capsule 'worker' is not in parent's spawn_allow");
}

/// With no `spawned_by` there is no parent to be within, so the global list is the only gate — even
/// for a manifest whose grants would exceed any parent.
#[test]
fn a_top_level_spawn_has_no_parent_to_be_within() {
    let daemon = Daemon::with_spawn_allow(vec!["worker".to_string()]);
    daemon.publish(
        "worker",
        "artifacts: []\ncapabilities:\n  network:\n    allow: [api.example.com]\n    \
         unix_sockets: true\n  filesystem:\n    workdir_exec: true\n  \
         env:\n    allow: [GITHUB_TOKEN]\n",
    );

    let response = daemon.spawn("worker", None);

    assert_eq!(response.status, 200, "{:?}", response.body);
    assert!(response.body["session_id"]
        .as_str()
        .unwrap()
        .starts_with("ses_"));
}

#[test]
fn a_top_level_spawn_of_an_unlisted_name_is_still_refused_by_the_flag() {
    let daemon = Daemon::new();
    daemon.publish("worker", "artifacts: []\n");

    let response = daemon.spawn("worker", None);

    assert_eq!(response.status, 403);
    assert_eq!(response.error(), "capsule 'worker' is not in --spawn-allow");
}

/// The referee reads the registry manifest, never the request. A body carrying its own narrow,
/// within-envelope declaration changes nothing.
#[test]
fn a_manifest_supplied_in_the_request_is_not_consulted() {
    let daemon = Daemon::new();
    daemon.seed_parent(
        "capabilities:\n  network:\n    allow: [registry.internal]\n  \
         spawn:\n    allow: [worker]\n",
    );
    daemon.publish(
        "worker",
        "artifacts: []\ncapabilities:\n  network:\n    allow: [api.example.com]\n",
    );
    let credential = daemon.credential(PARENT_SESSION);

    let response = daemon.post_delegate(
        r#"{"name":"worker","version":"0.1.0",
            "manifest":{"name":"worker","version":"0.1.0","capabilities":{"network":{"allow":["registry.internal"]}}},
            "capabilities":{"network":{"allow":["registry.internal"]}}}"#,
        Some(&credential),
    );

    assert_eq!(response.status, 403);
    assert!(
        response.error().contains("api.example.com"),
        "{}",
        response.error()
    );
}

/// A refused spawn creates nothing: no job record, no session directory under the requested
/// workdir, and therefore no trace.
#[test]
fn a_refused_spawn_creates_nothing() {
    let daemon = Daemon::new();
    daemon.seed_parent(
        "capabilities:\n  network:\n    allow: [registry.internal]\n  \
         spawn:\n    allow: [worker]\n",
    );
    daemon.publish(
        "worker",
        "artifacts: []\ncapabilities:\n  network:\n    allow: [api.example.com]\n",
    );

    assert_eq!(daemon.spawn("worker", Some(PARENT_SESSION)).status, 403);

    let jobs = daemon.state.jobs.lock().unwrap();
    assert_eq!(jobs.len(), 1);
    assert!(jobs.contains_key(PARENT_SESSION));
    drop(jobs);

    let entries: Vec<_> = std::fs::read_dir(daemon.workdir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert!(entries.is_empty(), "workdir is not empty: {entries:?}");
}
