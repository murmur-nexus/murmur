//! Binding a spawn decision to the session that earned it and the artifact it was granted for.
//!
//! `spawn_envelope.rs` asserts what the referee compares. This suite asserts the three bindings
//! that make the referee's answer trustworthy: the credential that says who is asking, the
//! approval that says what was approved, and the expiry and single use that bound how long and how
//! often that answer holds.
//!
//! Every case drives [`mur_roost::route`] rather than a socket — the same entry point the
//! connection handler reaches, with only the framing removed.

use std::collections::HashMap;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use capsule_runtime::{SpawnEnvelope, SPAWN_APPROVAL_HEADER, SPAWN_CREDENTIAL_HEADER};
use mur_roost::{
    authority::{now_ms, SpawnAuthority},
    mint_session_credential, JobRecord, JobStatus, RequestHeaders, State,
};
use murmur_artifact::{ArtifactMeta, LocalRegistry, Registry, RuntimeManifest, RuntimeType};
use tempfile::TempDir;

/// A script capsule that needs no artifacts and no host grants. Read from `murmur-cli`'s fixture
/// directory, so every suite launches the same component bytes.
const CAPSULE_COMPONENT: &str = "capsule-env-echo.wasm";

/// The session every delegating case asks as. Its envelope lists `worker-a`, `worker-b` and
/// `worker-c`.
const CALLER_SESSION: &str = "ses_00000000000000000000000000caller";

/// A second, strictly wider session: it lists `worker-a` where [`POOR_SESSION`] does not. Naming
/// it is the escalation this slice closes.
const RICH_SESSION: &str = "ses_ric00000000000000000000000000rich";

/// A session whose envelope lists nothing but `worker-c`.
const POOR_SESSION: &str = "ses_poo00000000000000000000000000poor";

fn fixture_component() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("murmur-cli")
        .join("tests")
        .join("fixtures")
        .join("run")
        .join("components")
        .join(CAPSULE_COMPONENT)
}

fn manifest_yaml(name: &str, version: &str, body: &str) -> String {
    format!("name: {name}\nversion: {version}\n{body}")
}

/// Pack a `.mur.zip` and publish it where `resolve_with_platform` will find it.
///
/// `salt` lands in an extra zip entry the runtime never reads, so a coordinate can be republished
/// with different bytes — and therefore a different sha256 — while staying the same capsule.
fn publish_capsule(registry_root: &Path, name: &str, version: &str, body: &str, salt: &str) {
    let mut cursor = Cursor::new(Vec::<u8>::new());
    {
        let mut zip = zip::ZipWriter::new(&mut cursor);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zip.start_file("murmur.yaml", options).unwrap();
        zip.write_all(manifest_yaml(name, version, body).as_bytes())
            .unwrap();
        zip.start_file("capsule.wasm", options).unwrap();
        zip.write_all(&std::fs::read(fixture_component()).unwrap())
            .unwrap();
        if !salt.is_empty() {
            zip.start_file("README.txt", options).unwrap();
            zip.write_all(salt.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
    }

    LocalRegistry::new(registry_root)
        .publish(
            ArtifactMeta {
                name: name.to_string(),
                version: version.to_string(),
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
        &RuntimeManifest::from_yaml_str(&manifest_yaml(name, "0.1.0", body))
            .expect("session manifest fixture must parse"),
    )
}

struct Daemon {
    state: Arc<State>,
    registry: TempDir,
    workdir: TempDir,
}

impl Daemon {
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

    fn seed(&self, session_id: &str, body: &str) {
        self.state.jobs.lock().unwrap().insert(
            session_id.to_string(),
            JobRecord {
                status: JobStatus::Running,
                envelope: envelope_from("session", body),
            },
        );
    }

    /// A session that may spawn `worker-a`, `worker-b` and `worker-c`, holding enough network to
    /// contain any of them.
    fn seed_caller(&self, session_id: &str) {
        self.seed(
            session_id,
            "capabilities:\n  network:\n    allow: [registry.internal]\n  \
             spawn:\n    allow: [worker-a, worker-b, worker-c]\n",
        );
    }

    fn publish(&self, name: &str, version: &str) {
        self.publish_salted(name, version, "");
    }

    /// Publishes a coordinate, replacing whatever was there.
    ///
    /// The local registry refuses to overwrite a published coordinate, so the directory goes first:
    /// what this stands in for is a registry that *can* serve different bytes under a coordinate
    /// the referee already read, which is the case the approval's digest exists to catch.
    fn publish_salted(&self, name: &str, version: &str, salt: &str) {
        let published = self.registry.path().join(name).join(version);
        if published.exists() {
            std::fs::remove_dir_all(&published).unwrap();
        }
        publish_capsule(
            self.registry.path(),
            name,
            version,
            "artifacts: []\ncapabilities:\n  network:\n    allow: [registry.internal]\n",
            salt,
        );
    }

    fn credential(&self, session_id: &str) -> String {
        self.state
            .authority
            .mint_credential_token(session_id)
            .unwrap()
    }

    fn delegate(&self, name: &str, version: &str, credential: Option<&str>) -> Response {
        let mut headers = RequestHeaders::new();
        if let Some(credential) = credential {
            headers.insert(SPAWN_CREDENTIAL_HEADER, credential);
        }
        Response::parse(&mur_roost::route(
            "POST",
            "/delegate",
            &headers,
            &format!(r#"{{"name":"{name}","version":"{version}"}}"#),
            &self.state,
        ))
    }

    /// The approval `/delegate` grants for a coordinate the caller may spawn.
    fn approval(&self, name: &str, version: &str, credential: &str) -> String {
        let response = self.delegate(name, version, Some(credential));
        assert_eq!(response.status, 200, "{:?}", response.body);
        response.body["approval"].as_str().unwrap().to_string()
    }

    fn spawn(
        &self,
        name: &str,
        version: &str,
        spawned_by: Option<&str>,
        credential: Option<&str>,
        approval: Option<&str>,
    ) -> Response {
        let spawned_by = spawned_by
            .map(|id| format!(r#","spawned_by":"{id}""#))
            .unwrap_or_default();
        let body = format!(
            r#"{{"name":"{name}","version":"{version}","workdir":{}{spawned_by}}}"#,
            serde_json::to_string(&self.workdir.path().display().to_string()).unwrap(),
        );
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
            &body,
            &self.state,
        ))
    }

    /// Every session id the job store holds, seeded or launched.
    fn session_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.state.jobs.lock().unwrap().keys().cloned().collect();
        ids.sort();
        ids
    }

    fn workdir_entries(&self) -> Vec<String> {
        std::fs::read_dir(self.workdir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
            .collect()
    }
}

struct Response {
    raw: String,
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
            raw: raw.to_string(),
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

/// The refusal every identity failure answers with, asserted here once so every case below can
/// compare against a literal rather than against the daemon's own constant.
const IDENTITY_REFUSAL: &str = "not authorised: a spawn must present a credential and an approval minted for the same running session";

// ── 1. Happy path ─────────────────────────────────────────────────────────────

/// The whole exchange, end to end: an approval for a capsule the caller's envelope contains, then
/// a launch of exactly that artifact.
#[test]
fn a_delegated_spawn_launches_the_artifact_its_approval_names() {
    let daemon = Daemon::new();
    daemon.seed_caller(CALLER_SESSION);
    daemon.publish("worker-a", "0.1.0");
    let credential = daemon.credential(CALLER_SESSION);

    let delegation = daemon.delegate("worker-a", "0.1.0", Some(&credential));
    assert_eq!(delegation.status, 200, "{:?}", delegation.body);
    let approval = delegation.body["approval"].as_str().unwrap();
    assert!(approval.starts_with("msa1."), "{approval}");
    assert!(
        delegation.body["expires_at_ms"].as_u64().unwrap() > now_ms(),
        "{:?}",
        delegation.body
    );

    let response = daemon.spawn(
        "worker-a",
        "0.1.0",
        Some(CALLER_SESSION),
        Some(&credential),
        Some(approval),
    );

    assert_eq!(response.status, 200, "{:?}", response.body);
    let session_id = response.body["session_id"].as_str().unwrap();
    assert!(session_id.starts_with("ses_"), "{session_id}");
    assert!(response.body.get("capsule_url").is_some());
    assert!(
        daemon.session_ids().contains(&session_id.to_string()),
        "the launched session has no job record: {:?}",
        daemon.session_ids()
    );
}

// ── 2. The escalation is closed ───────────────────────────────────────────────

/// Naming a better-provisioned session in `spawned_by`, with nothing to back the claim, is refused
/// — and that session's envelope is never reached, so nothing about it can be launched.
#[test]
fn naming_another_session_without_a_credential_is_refused() {
    let daemon = Daemon::new();
    daemon.seed_caller(RICH_SESSION);
    daemon.seed(
        POOR_SESSION,
        "capabilities:\n  spawn:\n    allow: [worker-c]\n",
    );
    daemon.publish("worker-a", "0.1.0");

    let response = daemon.spawn("worker-a", "0.1.0", Some(RICH_SESSION), None, None);

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
/// responses, with and without a credential header.
#[test]
fn a_refusal_does_not_say_whether_a_session_exists() {
    let daemon = Daemon::new();
    daemon.seed_caller(CALLER_SESSION);
    daemon.publish("worker-a", "0.1.0");

    // A credential this daemon did not mint: well-formed, and unverifiable.
    let foreign = SpawnAuthority::generate()
        .unwrap()
        .mint_credential_token(CALLER_SESSION)
        .unwrap();
    let approval = "msa1.e30.e30";

    let known = daemon.spawn(
        "worker-a",
        "0.1.0",
        Some(CALLER_SESSION),
        Some(&foreign),
        Some(approval),
    );
    let unknown = daemon.spawn(
        "worker-a",
        "0.1.0",
        Some("ses_no-such-session"),
        Some(&foreign),
        Some(approval),
    );
    assert_eq!(known.raw, unknown.raw);
    assert_eq!(known.status, 403);
    assert_eq!(known.error(), IDENTITY_REFUSAL);

    let known = daemon.spawn("worker-a", "0.1.0", Some(CALLER_SESSION), None, None);
    let unknown = daemon.spawn("worker-a", "0.1.0", Some("ses_no-such-session"), None, None);
    assert_eq!(known.raw, unknown.raw);
    assert_eq!(known.status, 403);
}

// ── 4. A credential cannot claim another session ──────────────────────────────

/// The session a credential names is the session judged, and the session bound into the approval
/// it earns. Neither can be redirected by the request body.
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

    // Judged against the poor session's own list, not the one it would like to be.
    let delegation = daemon.delegate("worker-a", "0.1.0", Some(&poor));
    assert_eq!(delegation.status, 403);
    assert_eq!(
        delegation.error(),
        "capsule 'worker-a' is not in parent's spawn_allow"
    );

    // And an approval the rich session earned is not redeemable by the poor session's credential,
    // whatever the body claims.
    let approval = daemon.approval("worker-a", "0.1.0", &rich);
    let response = daemon.spawn(
        "worker-a",
        "0.1.0",
        Some(RICH_SESSION),
        Some(&poor),
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

    let other_name = daemon.spawn(
        "worker-b",
        "0.1.0",
        Some(CALLER_SESSION),
        Some(&credential),
        Some(&daemon.approval("worker-a", "0.1.0", &credential)),
    );
    assert_eq!(other_name.status, 403);
    assert_eq!(
        other_name.error(),
        "this spawn approval was granted for 'worker-a@0.1.0', not 'worker-b@0.1.0'"
    );

    let other_version = daemon.spawn(
        "worker-a",
        "0.2.0",
        Some(CALLER_SESSION),
        Some(&credential),
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
    daemon.publish_salted("worker-a", "0.1.0", "republished with different bytes");
    let other_bytes = daemon.spawn(
        "worker-a",
        "0.1.0",
        Some(CALLER_SESSION),
        Some(&credential),
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

/// The same fixture launches with a future expiry and is refused with a past one, so the refusal
/// is attributable to the expiry and to nothing else about the approval.
#[test]
fn an_approval_past_its_expiry_is_refused() {
    let daemon = Daemon::new();
    daemon.seed_caller(CALLER_SESSION);
    daemon.publish("worker-a", "0.1.0");
    let credential = daemon.credential(CALLER_SESSION);
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

    let expired = daemon.spawn(
        "worker-a",
        "0.1.0",
        Some(CALLER_SESSION),
        Some(&credential),
        Some(&mint(now_ms() - 1)),
    );
    assert_eq!(expired.status, 403);
    assert_eq!(
        expired.error(),
        "this spawn approval has passed its expiry; an approval is valid for 60 seconds from the \
         POST /delegate that granted it"
    );

    let live = daemon.spawn(
        "worker-a",
        "0.1.0",
        Some(CALLER_SESSION),
        Some(&credential),
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

    let first = daemon.spawn(
        "worker-a",
        "0.1.0",
        Some(CALLER_SESSION),
        Some(&credential),
        Some(&approval),
    );
    let second = daemon.spawn(
        "worker-a",
        "0.1.0",
        Some(CALLER_SESSION),
        Some(&credential),
        Some(&approval),
    );

    assert_eq!(first.status, 200, "{:?}", first.body);
    assert_eq!(second.status, 403);
    assert_eq!(
        second.error(),
        "this spawn approval has already been redeemed; an approval covers one launch, so ask \
         POST /delegate for another"
    );
    // The seeded caller, plus exactly one launched session.
    assert_eq!(daemon.session_ids().len(), 2, "{:?}", daemon.session_ids());
}

// ── 9. A non-delegating capsule is unaffected ─────────────────────────────────

/// A capsule with nothing in `capabilities.spawn.allow` is handed no credential at all, so nothing
/// about its launch changes.
#[test]
fn only_a_session_that_can_delegate_is_minted_a_credential() {
    let authority = SpawnAuthority::generate().unwrap();
    let delegating = envelope_from(
        "session",
        "capabilities:\n  spawn:\n    allow: [worker-a]\n",
    );
    let ordinary = envelope_from("session", "capabilities:\n  network:\n    allow: [host]\n");

    assert!(mint_session_credential(&authority, CALLER_SESSION, &delegating).is_some());
    assert!(mint_session_credential(&authority, CALLER_SESSION, &ordinary).is_none());
}

// ── 10. The top-level path ────────────────────────────────────────────────────

/// The operator's path is unchanged, and half an exchange never reaches it — not even for a name
/// the operator listed.
#[test]
fn the_top_level_path_is_unchanged_and_needs_no_tokens() {
    let daemon = Daemon::with_spawn_allow(vec!["worker-a".to_string()]);
    daemon.seed_caller(CALLER_SESSION);
    daemon.publish("worker-a", "0.1.0");
    let credential = daemon.credential(CALLER_SESSION);
    let approval = daemon.approval("worker-a", "0.1.0", &credential);

    let bare = daemon.spawn("worker-a", "0.1.0", None, None, None);
    assert_eq!(bare.status, 200, "{:?}", bare.body);
    assert!(bare.body["session_id"]
        .as_str()
        .unwrap()
        .starts_with("ses_"));

    let credential_only = daemon.spawn("worker-a", "0.1.0", None, Some(&credential), None);
    assert_eq!(credential_only.status, 403);
    assert_eq!(credential_only.error(), IDENTITY_REFUSAL);

    let approval_only = daemon.spawn("worker-a", "0.1.0", None, None, Some(&approval));
    assert_eq!(approval_only.status, 403);
    assert_eq!(approval_only.error(), IDENTITY_REFUSAL);
}

/// A credential is not an approval and an approval is not a credential: the two MAC domains keep
/// each family from verifying as the other.
#[test]
fn a_credential_cannot_be_presented_as_its_own_approval() {
    let daemon = Daemon::new();
    daemon.seed_caller(CALLER_SESSION);
    daemon.publish("worker-a", "0.1.0");
    let credential = daemon.credential(CALLER_SESSION);

    let response = daemon.spawn(
        "worker-a",
        "0.1.0",
        Some(CALLER_SESSION),
        Some(&credential),
        Some(&credential),
    );

    assert_eq!(response.status, 403);
    assert_eq!(response.error(), IDENTITY_REFUSAL);
}

/// A `spawned_by` that disagrees with the credential is refused rather than believed — and, being
/// an identity failure, is refused in the same words as every other.
#[test]
fn a_spawned_by_that_disagrees_with_the_credential_is_refused() {
    let daemon = Daemon::new();
    daemon.seed_caller(CALLER_SESSION);
    daemon.seed_caller(RICH_SESSION);
    daemon.publish("worker-a", "0.1.0");
    let credential = daemon.credential(CALLER_SESSION);
    let approval = daemon.approval("worker-a", "0.1.0", &credential);

    let disagreeing = daemon.spawn(
        "worker-a",
        "0.1.0",
        Some(RICH_SESSION),
        Some(&credential),
        Some(&approval),
    );
    assert_eq!(disagreeing.status, 403);
    assert_eq!(disagreeing.error(), IDENTITY_REFUSAL);

    // Omitting it entirely is fine: the credential is what names the session.
    let omitted = daemon.spawn(
        "worker-a",
        "0.1.0",
        None,
        Some(&credential),
        Some(&daemon.approval("worker-a", "0.1.0", &credential)),
    );
    assert_eq!(omitted.status, 200, "{:?}", omitted.body);
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

    let response = daemon.delegate("worker-a", "0.1.0", Some(&credential));

    assert_eq!(response.status, 403);
    assert_eq!(response.error(), IDENTITY_REFUSAL);
}

/// `POST /delegate` decides and creates nothing: a refusal there, and an approval that is never
/// redeemed, both leave the workdir and the job store as they were.
#[test]
fn delegating_creates_no_session() {
    let daemon = Daemon::new();
    daemon.seed_caller(CALLER_SESSION);
    daemon.publish("worker-a", "0.1.0");
    let credential = daemon.credential(CALLER_SESSION);

    daemon.approval("worker-a", "0.1.0", &credential);
    assert_eq!(daemon.delegate("worker-z", "0.1.0", None).status, 403);

    assert_eq!(daemon.session_ids(), vec![CALLER_SESSION]);
    assert!(
        daemon.workdir_entries().is_empty(),
        "{:?}",
        daemon.workdir_entries()
    );
}
