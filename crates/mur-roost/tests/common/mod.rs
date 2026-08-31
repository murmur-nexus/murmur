//! The daemon under test, its registry, and the two exchanges every suite drives it through.
//!
//! Every case drives [`mur_roost::route`] rather than a socket — it is the same entry point the
//! connection handler reaches, with only the framing removed, so a refusal asserted here is the
//! refusal a caller receives.
//!
//! A delegated launch is two requests against this daemon, made by two different parties. The
//! parent's runtime asks `POST /spawn` for permission; the child's runtime presents the resulting
//! approval at `POST /register`. Nothing between them touches the daemon: the parent's runtime
//! creates the child's directory and starts the process itself.

#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use capsule_runtime::{SpawnEnvelope, SPAWN_APPROVAL_HEADER, SPAWN_CREDENTIAL_HEADER};
use mur_roost::{authority::SpawnAuthority, JobRecord, JobStatus, RequestHeaders, State};
use murmur_artifact::{ArtifactMeta, LocalRegistry, Registry, RuntimeManifest, RuntimeType};
use tempfile::TempDir;

/// A script capsule that needs no artifacts and no host grants: it probes two environment
/// variables and writes what it saw into its own preopen. Read from `murmur-cli`'s fixture
/// directory, so every suite packs the same component bytes.
pub const CAPSULE_COMPONENT: &str = "capsule-env-echo.wasm";

/// The refusal every identity failure answers with, written out here so every case compares
/// against a literal rather than against the daemon's own constant.
pub const IDENTITY_REFUSAL: &str = "not authorised: this daemon answers only a credential it minted for a running session, and an approval it minted for that same session";

pub fn fixture_component(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("murmur-cli")
        .join("tests")
        .join("fixtures")
        .join("run")
        .join("components")
        .join(name)
}

/// A capsule manifest: everything below `name:`/`version:`, indented as the operator wrote it.
pub fn manifest_yaml(name: &str, version: &str, body: &str) -> String {
    format!("name: {name}\nversion: {version}\n{body}")
}

/// Pack a `.mur.zip` with the given manifest and a root `capsule.wasm`, and publish it where
/// `resolve_with_platform` will find it.
///
/// `salt` lands in an extra zip entry the runtime never reads, so a coordinate can be republished
/// with different bytes — and therefore a different sha256 — while staying the same capsule.
pub fn publish_capsule(registry_root: &Path, name: &str, version: &str, body: &str, salt: &str) {
    let mut cursor = Cursor::new(Vec::<u8>::new());
    {
        let mut zip = zip::ZipWriter::new(&mut cursor);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zip.start_file("murmur.yaml", options).unwrap();
        zip.write_all(manifest_yaml(name, version, body).as_bytes())
            .unwrap();
        zip.start_file("capsule.wasm", options).unwrap();
        zip.write_all(&std::fs::read(fixture_component(CAPSULE_COMPONENT)).unwrap())
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

pub fn envelope_from(name: &str, body: &str) -> SpawnEnvelope {
    SpawnEnvelope::from_runtime_manifest(
        &RuntimeManifest::from_yaml_str(&manifest_yaml(name, "0.1.0", body))
            .expect("session manifest fixture must parse"),
    )
}

/// The manifest body every suite publishes when it only needs *some* capsule to exist.
pub const PLAIN_WORKER_BODY: &str =
    "artifacts: []\ncapabilities:\n  network:\n    allow: [registry.internal]\n";

pub struct Daemon {
    pub state: Arc<State>,
    pub registry: TempDir,
    /// A directory the daemon is never told about and must never write to. Asserted empty by
    /// every case that used to watch a launch create a session directory here.
    pub workdir: TempDir,
    next_child: AtomicUsize,
}

impl Daemon {
    /// A daemon with no `--spawn-allow` names at all, so every registration that presents no
    /// approval is refused and only a seeded parent can delegate.
    pub fn new() -> Self {
        Self::with_spawn_allow(Vec::new())
    }

    pub fn with_spawn_allow(spawn_allow: Vec<String>) -> Self {
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
            next_child: AtomicUsize::new(0),
        }
    }

    /// A running session with the given capabilities, seeded straight into the job store — which
    /// is exactly where a registered session's envelope comes from.
    pub fn seed(&self, session_id: &str, body: &str) {
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
    pub fn seed_caller(&self, session_id: &str) {
        self.seed(
            session_id,
            "capabilities:\n  network:\n    allow: [registry.internal]\n  \
             spawn:\n    allow: [worker-a, worker-b, worker-c, worker]\n",
        );
    }

    pub fn publish(&self, name: &str, version: &str) {
        self.publish_body(name, version, PLAIN_WORKER_BODY, "");
    }

    /// Publishes a coordinate, replacing whatever was there.
    ///
    /// The local registry refuses to overwrite a published coordinate, so the directory goes
    /// first: what this stands in for is a registry that *can* serve different bytes under a
    /// coordinate the referee already read, which is the case the approval's digest exists to
    /// catch.
    pub fn publish_body(&self, name: &str, version: &str, body: &str, salt: &str) {
        let published = self.registry.path().join(name).join(version);
        if published.exists() {
            std::fs::remove_dir_all(&published).unwrap();
        }
        publish_capsule(self.registry.path(), name, version, body, salt);
    }

    pub fn credential(&self, session_id: &str) -> String {
        self.state
            .authority
            .mint_credential_token(session_id)
            .unwrap()
    }

    /// A fresh session id for a child, of the shape a runtime mints.
    pub fn child_session_id(&self) -> String {
        format!(
            "ses_child{:023}",
            self.next_child.fetch_add(1, Ordering::SeqCst)
        )
    }

    pub fn post(&self, path: &str, body: &str, headers: &[(&str, &str)]) -> Response {
        let mut request_headers = RequestHeaders::new();
        for (name, value) in headers {
            request_headers.insert(name, value);
        }
        Response::parse(&mur_roost::route(
            "POST",
            path,
            &request_headers,
            body,
            &self.state,
        ))
    }

    pub fn get(&self, path: &str) -> Response {
        Response::parse(&mur_roost::route(
            "GET",
            path,
            &RequestHeaders::new(),
            "",
            &self.state,
        ))
    }

    /// `POST /spawn`: ask whether the session holding `credential` may spawn this capsule.
    pub fn permission(&self, name: &str, version: &str, credential: Option<&str>) -> Response {
        let headers: Vec<(&str, &str)> = credential
            .map(|token| vec![(SPAWN_CREDENTIAL_HEADER, token)])
            .unwrap_or_default();
        self.post(
            "/spawn",
            &format!(r#"{{"name":"{name}","version":"{version}"}}"#),
            &headers,
        )
    }

    /// The approval `POST /spawn` grants for a coordinate the caller may spawn.
    pub fn approval(&self, name: &str, version: &str, credential: &str) -> String {
        let response = self.permission(name, version, Some(credential));
        assert_eq!(response.status, 200, "{:?}", response.body);
        response.body["approval"].as_str().unwrap().to_string()
    }

    /// `POST /register`: a launched session announcing itself.
    pub fn register(
        &self,
        session_id: &str,
        name: &str,
        version: &str,
        approval: Option<&str>,
    ) -> Response {
        self.register_body(
            &format!(r#"{{"session_id":"{session_id}","name":"{name}","version":"{version}"}}"#),
            approval,
        )
    }

    pub fn register_body(&self, body: &str, approval: Option<&str>) -> Response {
        let headers: Vec<(&str, &str)> = approval
            .map(|token| vec![(SPAWN_APPROVAL_HEADER, token)])
            .unwrap_or_default();
        self.post("/register", body, &headers)
    }

    pub fn deregister(&self, credential: &str, outcome: &str) -> Response {
        self.post(
            "/deregister",
            &format!(r#"{{"outcome":"{outcome}"}}"#),
            &[(SPAWN_CREDENTIAL_HEADER, credential)],
        )
    }

    pub fn status(&self, session_id: &str) -> Response {
        self.get(&format!("/status/{session_id}"))
    }

    /// The whole exchange a parent's runtime performs for one child: ask, then launch — where
    /// "launch" is, from this daemon's side, the child registering itself. The first refusal met
    /// is the response, so a case asserting the referee's wording asserts what a caller receives.
    ///
    /// `parent: None` is the operator's top-level path, which asks nobody and presents no
    /// approval.
    pub fn spawn(&self, name: &str, parent: Option<&str>) -> Response {
        self.spawn_version(name, "0.1.0", parent)
    }

    pub fn spawn_version(&self, name: &str, version: &str, parent: Option<&str>) -> Response {
        let child = self.child_session_id();
        let Some(parent) = parent else {
            return self.register(&child, name, version, None);
        };
        let credential = self.credential(parent);
        let permission = self.permission(name, version, Some(&credential));
        if permission.status != 200 {
            return permission;
        }
        let approval = permission.body["approval"].as_str().unwrap().to_string();
        self.register(&child, name, version, Some(&approval))
    }

    /// Every session id the job store holds, seeded or registered.
    pub fn session_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.state.jobs.lock().unwrap().keys().cloned().collect();
        ids.sort();
        ids
    }

    pub fn workdir_entries(&self) -> Vec<String> {
        std::fs::read_dir(self.workdir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
            .collect()
    }
}

pub struct Response {
    pub raw: String,
    pub status: u16,
    pub body: serde_json::Value,
}

impl Response {
    pub fn parse(raw: &str) -> Self {
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

    pub fn error(&self) -> &str {
        self.body["error"]
            .as_str()
            .expect("refusal carries an error")
    }
}
