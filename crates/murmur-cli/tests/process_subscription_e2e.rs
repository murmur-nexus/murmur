//! A coding task, end to end, on a `transport: process` capsule: the real `claude` binary, the
//! real `murmur-driver-claude-code` from the local artifact store, the real runtime bridge, the
//! real `murmur-tool-editor`, and a scratch git repository — driven over the same A2A door an
//! `http` capsule answers on.
//!
//! Every Murmur component here is the shipped one. The single substitution is the upstream model
//! endpoint: [`MockEndpoint`] is a scripted Anthropic Messages server reached through
//! `ANTHROPIC_BASE_URL`, because a 401, a 429 and a mid-turn interrupt have to happen on demand
//! and a subscription cannot be held by a test.
//!
//! Redirecting the endpoint does not redirect the credential. `ANTHROPIC_API_KEY` cannot reach a
//! capsule at all — `capabilities.env.allow` refuses every credential-shaped name with
//! `E-CAP-016` — so the harness authenticates the only way a capsule's harness ever can: out of
//! the login store under its `HOME`. [`install_login`] copies the operator's into the scratch
//! home, `ANTHROPIC_BASE_URL` alone points the turn at the endpoint, and the turn therefore costs
//! nothing while still reporting `auth: subscription`. That is why every case here asserts
//! `W-SEC-031` was **not** raised; the warning's own coverage is `process_runner.rs` S10, where a
//! fake harness reports API-key auth.
//!
//! # Running these
//!
//! Every case is `#[ignore]`d and needs five things on the host, so a clean machine's
//! `cargo test --workspace` is unaffected:
//!
//! ```text
//! cargo test -p murmur-cli --test process_subscription_e2e -- --ignored --test-threads=1
//! ```
//!
//! A missing prerequisite **panics** naming it and the command that produces it. It never skips:
//! a silently-skipped end-to-end test is the one failure mode this file exists to close, and a
//! green run that proved nothing is worse than a red one.
//!
//! `--test-threads=1` is not optional. The scratch `$HOME` and `ANTHROPIC_BASE_URL` are
//! process-global, read by the runtime out of this process's own environment when it builds the
//! harness's; and each case drives its capsule to a terminal state before it returns, which is
//! what makes rewriting them between cases safe.

#![cfg(unix)]

#[path = "common/mod.rs"]
mod common;

use std::{
    collections::HashSet,
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::OnceLock,
    time::{Duration, Instant},
};

use capsule_runtime::{
    capability_policy_from_runtime_manifest, launch_session, stage_session, AfterTask,
    ArtifactRequest, LifecycleConfig, StageRequest, TaskAcceptance,
};
use murmur_artifact::{
    load_runtime_manifest, ArtifactRuntime, ContainmentClass, ConversationMode, LocalRegistry,
    W_SEC_031,
};
use serde_json::{json, Value};
use tempfile::TempDir;

// ── What the host has to provide ──────────────────────────────────────────────

/// The process driver under test. Named here and nowhere else in `crates/`: everything that knows
/// about Claude Code lives in this artifact, and this file is the fixture that drives it.
const DRIVER_NAME: &str = "murmur-driver-claude-code";

/// The tool the capsule edits its repository through. A WASM artifact, dispatched by murmur when
/// the harness calls it over the bridge.
const EDITOR_NAME: &str = "murmur-tool-editor";

/// The harness binary the driver's `describe()` names, looked up on `PATH`.
const HARNESS_BINARY: &str = "claude";

/// Variables the driver's `describe().required-env` names. A name the driver requires and the
/// manifest omits refuses the launch with `E-CAP-019`, which names the missing one — so this list
/// being short is checked by the runtime rather than by a constant kept in step by hand.
const DRIVER_REQUIRED_ENV: &[&str] = &["HOME", "PATH"];

/// The one variable that points the harness at [`MockEndpoint`] instead of at the real upstream.
///
/// It carries no credential, so `capabilities.env.allow` accepts it. Its API-key sibling would be
/// refused with `E-CAP-016` before any capsule started, which is why the harness is logged in
/// through its `HOME` by [`install_login`] instead.
const BASE_URL_ENV: &str = "ANTHROPIC_BASE_URL";

/// Read by `claude` itself: with no value it retries a 401 or a 429 up to ten times with backoff
/// before reporting it, which would put the two failure cases minutes apart from their answers.
const MAX_RETRIES_ENV: &str = "CLAUDE_CODE_MAX_RETRIES";

/// The login store `claude` reads its subscription credential out of, relative to `HOME`.
///
/// The harness's `HOME` is the scratch one, so the operator's copy is what has to be placed
/// there. Nothing else in the operator's real home is copied: the point is a logged-in harness,
/// not a mirrored machine.
const LOGIN_FILE: &str = ".claude/.credentials.json";

/// The built-in tools `claude` ships with. None may be offered to the model: `--setting-sources ""`
/// alone leaves every one of them in the request, and it is the driver's tool flags that strip
/// them, leaving only what murmur's bridge serves.
///
/// `Agent` is here beside `Task` because the two are the same tool under two names: the harness's
/// `init` line announces it as `Task` and the request body offers it as `Agent`. That mismatch is
/// why this list is checked against the tool array the model was actually offered rather than
/// against what the harness said it had.
const HARNESS_OWN_TOOLS: &[&str] = &[
    "Read",
    "Write",
    "Edit",
    "Bash",
    "Glob",
    "Grep",
    "Task",
    "Agent",
    "WebFetch",
    "WebSearch",
    "TodoWrite",
];

/// Where the host's published artifacts are read from, and the variable that moves it.
const ARTIFACT_STORE_ENV: &str = "MURMUR_E2E_ARTIFACT_STORE";

/// Where the scripted endpoint's script is read from, and the variable that moves it.
const MOCK_SCRIPT_ENV: &str = "MURMUR_E2E_MOCK_SCRIPT";

/// Where the operator's login store is read from, and the variable that moves it.
const LOGIN_FILE_ENV: &str = "MURMUR_E2E_LOGIN_FILE";

/// The literal the `text` scenario reports having seen, used to prove the harness carried the
/// first message into the second turn.
const SECRET: &str = "PINEAPPLE";

/// One artifact resolved out of the host's local store, copied rather than read in place.
struct StoreArtifact {
    name: String,
    version: String,
    zip: PathBuf,
}

/// Everything a case needs off the host, resolved once for the whole binary.
struct Prereqs {
    /// Absolute path of the `claude` binary, and what `claude --version` printed.
    harness: PathBuf,
    harness_version: String,
    python3: PathBuf,
    mock_script: PathBuf,
    driver: StoreArtifact,
    editor: StoreArtifact,
    /// The operator's login store, copied into every scratch home by [`install_login`].
    login: PathBuf,
}

/// Resolve every prerequisite, or panic naming what is absent and the command that produces it.
fn prereqs() -> &'static Prereqs {
    static PREREQS: OnceLock<Prereqs> = OnceLock::new();
    PREREQS.get_or_init(|| {
        // Resolved before `scratch_home` rewrites `$HOME`, so the store this reads is the
        // operator's real one. Nothing below writes to it: each `.mur.zip` is copied out and
        // republished into the case's own scratch home.
        let store = artifact_store();
        let mut missing: Vec<String> = Vec::new();

        let harness = which(HARNESS_BINARY);
        if harness.is_none() {
            missing.push(format!(
                "the '{HARNESS_BINARY}' CLI is not on PATH\n    install it and log in: \
                 `npm install -g @anthropic-ai/claude-code && claude login`"
            ));
        }
        let python3 = which("python3");
        if python3.is_none() {
            missing.push(
                "'python3' is not on PATH — it runs the scripted inference endpoint\n    \
                 install your platform's python3 package"
                    .to_string(),
            );
        }
        let driver = latest_in_store(&store, DRIVER_NAME);
        if driver.is_none() {
            missing.push(format!(
                "'{DRIVER_NAME}' is not published in {}\n    build and publish it from a \
                 default-artifacts checkout: `scripts/local-install.sh {DRIVER_NAME} <capsule-dir>`, \
                 or `mur build drivers/{DRIVER_NAME} -o /tmp/{DRIVER_NAME}.mur.zip && mur publish \
                 /tmp/{DRIVER_NAME}.mur.zip`",
                store.display()
            ));
        }
        let editor = latest_in_store(&store, EDITOR_NAME);
        if editor.is_none() {
            missing.push(format!(
                "'{EDITOR_NAME}' is not published in {}\n    build and publish it from a \
                 default-artifacts checkout: `cargo build -p {EDITOR_NAME} --target wasm32-wasip2 \
                 --release`, then `mur build` + `mur publish`",
                store.display()
            ));
        }
        let login = host_login();
        if !login.is_file() {
            missing.push(format!(
                "the '{HARNESS_BINARY}' CLI is not logged in: there is no {}\n    log in on this \
                 machine: `{HARNESS_BINARY} login`",
                login.display()
            ));
        }
        let mock_script = mock_script();
        if !mock_script.is_file() {
            missing.push(format!(
                "the scripted inference endpoint is not at {}\n    point {MOCK_SCRIPT_ENV} at a \
                 copy of mock.py",
                mock_script.display()
            ));
        }

        assert!(
            missing.is_empty(),
            "this end-to-end test runs against the real harness and the real driver, and \
             refuses to skip:\n  - {}",
            missing.join("\n  - ")
        );

        let harness = harness.expect("checked above");
        let harness_version = harness_version(&harness);
        println!("claude --version: {harness_version}");
        let driver = driver.expect("checked above");
        println!("{DRIVER_NAME} resolved from the local store at {}", driver.version);

        Prereqs {
            harness,
            harness_version,
            python3: python3.expect("checked above"),
            mock_script,
            driver,
            editor: editor.expect("checked above"),
            login,
        }
    })
}

/// The operator's own login store, resolved while `$HOME` still names their real home.
fn host_login() -> PathBuf {
    static LOGIN: OnceLock<PathBuf> = OnceLock::new();
    LOGIN
        .get_or_init(|| {
            if let Some(path) = std::env::var_os(LOGIN_FILE_ENV) {
                return PathBuf::from(path);
            }
            let home = std::env::var_os("HOME").expect("HOME must be set to find the login store");
            PathBuf::from(home).join(LOGIN_FILE)
        })
        .clone()
}

/// Copy the operator's login store into a scratch home, so the harness that runs under it
/// authenticates as the operator.
///
/// The endpoint the turn actually reaches is [`MockEndpoint`], so nothing is spent; what the
/// credential buys is `apiKeySource: "none"`, which is what makes the run report
/// `auth: subscription` and raise no `W-SEC-031`. Copied, never linked: `claude` rewrites this
/// file when it refreshes a token, and the operator's own login must not move under it.
fn install_login(home: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let dest = home.join(LOGIN_FILE);
    let dir = dest.parent().expect("the login file is nested");
    fs::create_dir_all(dir).unwrap();
    fs::copy(&prereqs().login, &dest).unwrap_or_else(|error| {
        panic!(
            "could not copy the login store into {}: {error}",
            home.display()
        )
    });
    // `0600` under `0700`, the terms the runtime writes its own harness session entries on. The
    // scratch home outlives the run — the capsules launched under it resolve paths against it to
    // the end of the binary — so this copy of a real credential sits in the system temp directory
    // afterwards, and nothing but the permissions keeps another account off it.
    fs::set_permissions(&dest, PermissionsExt::from_mode(0o600)).unwrap();
    fs::set_permissions(dir, PermissionsExt::from_mode(0o700)).unwrap();
}

/// The host store the driver and the tool are read out of.
///
/// Resolved once and cached: `scratch_home` rewrites `$HOME` for the rest of the binary, so a
/// later call would otherwise name the scratch home's empty store instead of the operator's.
fn artifact_store() -> PathBuf {
    static STORE: OnceLock<PathBuf> = OnceLock::new();
    STORE
        .get_or_init(|| {
            if let Some(dir) = std::env::var_os(ARTIFACT_STORE_ENV) {
                return PathBuf::from(dir);
            }
            let home =
                std::env::var_os("HOME").expect("HOME must be set to find the artifact store");
            PathBuf::from(home).join(".murmur").join("artifacts")
        })
        .clone()
}

/// The scripted endpoint's script: the vendored copy, unless [`MOCK_SCRIPT_ENV`] names another.
fn mock_script() -> PathBuf {
    match std::env::var_os(MOCK_SCRIPT_ENV) {
        Some(path) => PathBuf::from(path),
        None => common::fixture_path("subscription-e2e/mock.py"),
    }
}

fn which(binary: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(binary))
        .find(|candidate| candidate.is_file())
}

/// What the harness printed for `--version`, as one line.
fn harness_version(harness: &Path) -> String {
    let output = Command::new(harness)
        .arg("--version")
        .output()
        .unwrap_or_else(|error| panic!("`{} --version` could not run: {error}", harness.display()));
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// The highest version of `name` published in `store`, as a path to its `.mur.zip`.
///
/// Read-only: the caller copies the zip out and republishes it into a scratch home. Nothing in
/// this file writes to the operator's store.
fn latest_in_store(store: &Path, name: &str) -> Option<StoreArtifact> {
    let mut versions: Vec<String> = fs::read_dir(store.join(name))
        .ok()?
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    versions.sort();
    let version = versions.pop()?;
    let zip = store
        .join(name)
        .join(&version)
        .join(format!("{name}-{version}.mur.zip"));
    zip.is_file().then(|| StoreArtifact {
        name: name.to_string(),
        version,
        zip,
    })
}

/// The `.mur.zip` published for `name@version` in `store`, if there is one.
///
/// A portable artifact is published as `<name>-<version>.mur.zip`; one built per platform carries
/// a target suffix (`<name>-<version>-linux-x86_64.mur.zip`), so a name-and-version match alone
/// would miss every native tool. The exact name wins when both are present.
fn published_zip(store: &Path, name: &str, version: &str) -> Option<PathBuf> {
    let dir = store.join(name).join(version);
    let exact = dir.join(format!("{name}-{version}.mur.zip"));
    if exact.is_file() {
        return Some(exact);
    }
    let prefix = format!("{name}-{version}-");
    let mut candidates: Vec<PathBuf> = fs::read_dir(&dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(&prefix) && n.ends_with(".mur.zip"))
        })
        .collect();
    candidates.sort();
    candidates.pop()
}

/// The scratch `$HOME` every capsule in this binary runs under, and the retry setting the two
/// failure cases need.
///
/// Both are process-global. `$HOME` is read twice over: the runtime resolves the harness session
/// map against this process's own home, and `capabilities.env.allow: [HOME]` hands the harness
/// that same value — so this is also the home `claude` reads its login out of and writes its own
/// state into. Set once, never per case, because a capsule an earlier case left sleeping still
/// resolves paths against it.
fn scratch_home() -> &'static Path {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        // Resolves the host store first, while `$HOME` still names the operator's home.
        prereqs();
        let dir = tempfile::tempdir().expect("a scratch home");
        let path = dir.path().to_path_buf();
        std::mem::forget(dir);
        install_login(&path);
        std::env::set_var("HOME", &path);
        std::env::set_var(MAX_RETRIES_ENV, "0");
        path
    })
}

// ── The scripted inference endpoint ───────────────────────────────────────────

/// A `mock.py` on an ephemeral port, its request log and its scenario file.
///
/// The scenario file is read by the endpoint on **every** request, so a case can change what the
/// model does mid-run by calling [`MockEndpoint::scenario`].
struct MockEndpoint {
    port: u16,
    log_dir: PathBuf,
    scen_file: PathBuf,
    child: Child,
    _dir: TempDir,
}

impl MockEndpoint {
    /// Start the endpoint with `scenario` and wait until it answers.
    fn start(scenario: &str) -> Self {
        let prereqs = prereqs();
        let dir = tempfile::tempdir().expect("a scratch dir for the endpoint");
        let log_dir = dir.path().join("log");
        let scen_file = dir.path().join("scenario");
        fs::create_dir_all(&log_dir).unwrap();
        fs::write(&scen_file, scenario).unwrap();

        let port = common::free_port();
        let child = Command::new(&prereqs.python3)
            .arg(&prereqs.mock_script)
            .arg(port.to_string())
            .env("MOCK_LOG", &log_dir)
            .env("MOCK_SCEN", &scen_file)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("the scripted endpoint should start");

        let endpoint = Self {
            port,
            log_dir,
            scen_file,
            child,
            _dir: dir,
        };
        endpoint.wait_until_listening();
        endpoint
    }

    fn wait_until_listening(&self) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if TcpStream::connect(("127.0.0.1", self.port)).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("the scripted endpoint never listened on port {}", self.port);
    }

    fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Rewrite the scenario file. Takes effect on the next request the endpoint serves.
    fn scenario(&self, scenario: &str) {
        fs::write(&self.scen_file, scenario).unwrap();
    }

    /// Every request body the endpoint logged, in the order it logged them.
    fn requests(&self) -> Vec<Value> {
        let mut files: Vec<PathBuf> = fs::read_dir(&self.log_dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect();
        files.sort();
        files
            .iter()
            .filter_map(|path| {
                let record: Value = serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
                let body = record.get("body")?.as_str()?;
                serde_json::from_str::<Value>(body).ok()
            })
            .collect()
    }

    /// The `tools` array of every logged request that carried one, as tool names.
    fn tools_offered(&self) -> Vec<Vec<String>> {
        self.requests()
            .iter()
            .filter_map(|body| body.get("tools")?.as_array().cloned())
            .map(|tools| {
                tools
                    .iter()
                    .filter_map(|tool| Some(tool.get("name")?.as_str()?.to_string()))
                    .collect()
            })
            .collect()
    }
}

impl Drop for MockEndpoint {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ── The capsule under test ────────────────────────────────────────────────────

/// A launched `transport: process` capsule, its scratch git repository, and the addresses a case
/// reads it back through.
struct E2eCapsule {
    /// `host:port` of the capsule's A2A door.
    url: String,
    /// The internal session workdir — where `trace.jsonl` and `out/` are written.
    workdir: PathBuf,
    /// The git repository the capsule edits, mounted as its accessible workdir.
    repo: PathBuf,
    /// The context every task in this capsule is sent under, so they share one harness session.
    context_id: String,
    _home: TempDir,
    _project: TempDir,
}

impl E2eCapsule {
    /// Stand up a scratch git repository and launch a capsule over it, in-process.
    ///
    /// `name` must be unique within this binary: it keys the harness session map under the
    /// shared scratch `$HOME`.
    fn launch(name: &str, mock: &MockEndpoint) -> Self {
        let prereqs = prereqs();
        scratch_home();

        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        publish_copy(&home, &prereqs.driver);
        publish_copy(&home, &prereqs.editor);

        let repo = init_git_repo(project.path());

        let manifest_path = project.path().join("murmur.yaml");
        fs::write(&manifest_path, manifest_yaml(name, prereqs)).unwrap();

        // Written into this process's environment, not the child's: the runtime builds the
        // harness's environment from `capabilities.env.allow` read against its own. Safe because
        // the suite runs single-threaded and each case ends its tasks before the next starts.
        std::env::set_var(BASE_URL_ENV, mock.base_url());

        let staged = stage(&home, &manifest_path, &repo);
        let workdir = staged.workdir.clone();
        let (url_tx, url_rx) = std::sync::mpsc::channel::<String>();
        std::thread::spawn(move || {
            let _ = launch_session(staged, move |url| {
                let _ = url_tx.send(url.to_string());
            });
        });
        let url = url_rx
            .recv_timeout(Duration::from_secs(120))
            .expect("timed out waiting for the capsule URL");

        Self {
            url,
            workdir,
            repo,
            context_id: format!("ctx-{name}"),
            _home: home,
            _project: project,
        }
    }

    /// Submit a task with `message/send` under this capsule's one context, returning its id.
    fn submit(&self, message_id: &str, text: &str) -> String {
        let response = http_post_json(
            &self.url,
            &json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "message/send",
                "params": {"message": {
                    "messageId": message_id,
                    "contextId": self.context_id,
                    "role": "user",
                    "parts": [{"text": text}]
                }}
            })
            .to_string(),
        );
        response["result"]["id"]
            .as_str()
            .unwrap_or_else(|| panic!("message/send did not return a task id: {response}"))
            .to_string()
    }

    fn tasks_get(&self, task_id: &str) -> Value {
        http_post_json(
            &self.url,
            &json!({"jsonrpc": "2.0", "id": 2, "method": "tasks/get", "params": {"id": task_id}})
                .to_string(),
        )
    }

    fn tasks_cancel(&self, task_id: &str) -> Value {
        http_post_json(
            &self.url,
            &json!({"jsonrpc": "2.0", "id": 3, "method": "tasks/cancel", "params": {"id": task_id}})
                .to_string(),
        )
    }

    /// Poll `tasks/get` until the task reaches a terminal state, and return that response.
    fn wait_for_terminal(&self, task_id: &str, timeout: Duration) -> Value {
        let deadline = Instant::now() + timeout;
        loop {
            let response = self.tasks_get(task_id);
            let state = response["result"]["status"]["state"].as_str().unwrap_or("");
            if matches!(state, "completed" | "failed" | "canceled" | "rejected") {
                return response;
            }
            assert!(
                Instant::now() < deadline,
                "task {task_id} never reached a terminal state; last response: {response}"
            );
            std::thread::sleep(Duration::from_millis(150));
        }
    }

    /// Every `harness_start` record the session's trace holds, in order.
    fn harness_starts(&self) -> Vec<Value> {
        self.trace_records("harness_start")
    }

    fn trace_records(&self, event_type: &str) -> Vec<Value> {
        let trace = fs::read_to_string(self.workdir.join("trace.jsonl")).unwrap_or_default();
        trace
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|record| record["event_type"] == event_type)
            .collect()
    }

    /// The harness session entry this capsule's context maps to, or `None` before one is written.
    fn harness_session_entry(&self) -> Option<Value> {
        let root = scratch_home().join(".murmur").join("conversations");
        let path = common::find_file(&root, "harness-session.json")?;
        serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
    }

    /// Every path of repository content, relative, sorted — what a case compares before and after.
    ///
    /// `.git` and `.murmur` are skipped. Neither is content a coding task edits: the first is the
    /// repository's own store, and the second is the session workdir the runtime creates under
    /// the capsule's accessible directory, holding `trace.jsonl` and `out/`. Counting either
    /// would make "the run wrote only what the tool call asked for" a statement about murmur's
    /// bookkeeping rather than about the repository.
    fn repo_tree(&self) -> Vec<String> {
        let mut found = Vec::new();
        let mut stack = vec![self.repo.clone()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
                let path = entry.path();
                if path
                    .file_name()
                    .is_some_and(|name| name == ".git" || name == ".murmur")
                {
                    continue;
                }
                if path.is_dir() {
                    stack.push(path.clone());
                }
                if let Ok(relative) = path.strip_prefix(&self.repo) {
                    found.push(relative.display().to_string());
                }
            }
        }
        found.sort();
        found
    }
}

/// Copy a `.mur.zip` out of the host store and publish it into `home`.
///
/// Copied rather than published in place, because `mur publish` writes into the home it is given
/// and this file must never write into the operator's `~/.murmur/artifacts`.
fn publish_copy(home: &TempDir, artifact: &StoreArtifact) {
    let staging = home.path().join("incoming");
    fs::create_dir_all(&staging).unwrap();
    let local = staging.join(format!("{}-{}.mur.zip", artifact.name, artifact.version));
    fs::copy(&artifact.zip, &local).unwrap_or_else(|error| {
        panic!(
            "could not copy {} into the scratch home: {error}",
            artifact.zip.display()
        )
    });
    common::publish_local(home, &local).success();
}

/// The manifest every case writes: this transport, this driver, this tool, and the environment
/// that points the harness at the scripted endpoint.
fn manifest_yaml(name: &str, prereqs: &Prereqs) -> String {
    let mut allow: Vec<&str> = DRIVER_REQUIRED_ENV.to_vec();
    allow.push(BASE_URL_ENV);
    allow.push(MAX_RETRIES_ENV);
    let allow = allow
        .iter()
        .map(|name| format!("      - {name}\n"))
        .collect::<String>();
    format!(
        "name: {name}\n\
         version: 0.1.0\n\
         artifacts:\n  \
         - name: {DRIVER_NAME}\n    version: \"{driver_version}\"\n    runtime: driver\n  \
         - name: {EDITOR_NAME}\n    version: \"{editor_version}\"\n    runtime: tool\n\
         capabilities:\n  env:\n    allow:\n{allow}\
         inference:\n  transport: process\n  max_turns: 10\n  driver:\n    artifact: {DRIVER_NAME}\n\
         lifecycle:\n  task_acceptance: queue\n  queue_depth: 8\n  after_task: sleep\n  \
         conversation: threaded\n",
        driver_version = prereqs.driver.version,
        editor_version = prereqs.editor.version,
    )
}

/// A git repository with one commit, mounted as the capsule's accessible workdir.
fn init_git_repo(dir: &Path) -> PathBuf {
    let repo = dir.join("repo");
    fs::create_dir_all(&repo).unwrap();
    let run = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(&repo)
            .status()
            .expect("git should run");
        assert!(status.success(), "git {args:?} failed");
    };
    // `-b main` rather than the host's `init.defaultBranch`, so the repository reads the same on
    // a stock git as on a configured one.
    run(&["init", "-b", "main"]);
    run(&["config", "user.email", "e2e@example.com"]);
    run(&["config", "user.name", "Process E2E"]);
    fs::write(repo.join("README.md"), "scratch repository\n").unwrap();
    run(&["add", "README.md"]);
    run(&["commit", "-m", "initial"]);
    repo
}

fn stage(home: &TempDir, manifest_path: &Path, repo: &Path) -> capsule_runtime::StagedSession {
    let runtime_manifest = load_runtime_manifest(manifest_path).unwrap();
    let mut allowlisted_tools = HashSet::new();
    let mut requested_artifacts = Vec::new();
    for artifact in &runtime_manifest.artifacts {
        if matches!(artifact.runtime, ArtifactRuntime::Tool) {
            allowlisted_tools.insert(artifact.name.clone());
        }
        requested_artifacts.push(ArtifactRequest {
            name: artifact.name.clone(),
            version: artifact.version.clone(),
            runtime: artifact.runtime.clone(),
            source: artifact.source.clone(),
            on_overflow: artifact.on_overflow,
            config: artifact.config.clone(),
            gateway: artifact.gateway.clone(),
            capabilities: artifact.capabilities.clone(),
        });
    }
    let local_registry = LocalRegistry::new(home.path().join(".murmur").join("artifacts"));
    stage_session(
        std::sync::Arc::new(local_registry),
        StageRequest {
            credentials_file: None,
            manifest_dir: manifest_path.parent().unwrap().to_path_buf(),
            capsule_name: runtime_manifest.name.clone(),
            capsule_version: runtime_manifest.version.clone(),
            capsule_component_bytes: Vec::new(),
            artifacts: requested_artifacts,
            allowlisted_tools,
            lock_expectations: None,
            capability_policy: capability_policy_from_runtime_manifest(&runtime_manifest),
            inference: runtime_manifest.inference.clone(),
            system_prompt_overridden: false,
            context: runtime_manifest.context.clone(),
            context_id: None,
            resume: None,
            forget_session: false,
            otel_endpoint: None,
            eval_config_json: None,
            case_id: None,
            dataset_id: None,
            lifecycle: Some(LifecycleConfig {
                task_acceptance: TaskAcceptance::Queue,
                after_task: AfterTask::Sleep,
                queue_depth: 8,
                conversation_mode: ConversationMode::Threaded,
                ..Default::default()
            }),
            lifecycle_override: None,
            trace: None,
            // The git repository is the capsule's accessible workdir: WASM tools see it at ".",
            // so the editor's relative `dest_path` lands inside the repository and nowhere else.
            workdir: Some(repo.to_path_buf()),
            bind_addr: "127.0.0.1".to_string(),
            internal_port: None,
            declared_containment_floor: ContainmentClass::Advisory,
            exports: None,
            spawn_grant: None,
            machine_tokens_per_day: None,
        },
    )
    .unwrap()
}

// ── Talking to the door ───────────────────────────────────────────────────────

fn http_post_json(addr: &str, body: &str) -> Value {
    let stream = TcpStream::connect(addr).expect("should connect to the capsule");
    stream.set_read_timeout(Some(Duration::from_secs(60))).ok();
    let mut writer = &stream;
    writer
        .write_all(
            format!(
                "POST / HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .unwrap();
    writer.flush().unwrap();
    let mut reader = BufReader::new(&stream);
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
            break;
        }
    }
    let mut response = String::new();
    reader.read_to_string(&mut response).ok();
    serde_json::from_str(response.trim()).unwrap_or_else(|_| json!({"_raw": response}))
}

/// An open `stream/watch` connection, read frame by frame.
struct Watch {
    stream: TcpStream,
    pending: Vec<u8>,
    event: String,
    frames: Vec<(String, Value)>,
}

impl Watch {
    fn open(addr: &str) -> Self {
        let stream = common::idle_capsule::open_watch(addr, 0);
        Self {
            stream,
            pending: Vec::new(),
            event: String::new(),
            frames: Vec::new(),
        }
    }

    /// Read whatever has arrived, appending to [`Self::frames`], and return them all.
    fn pump(&mut self) -> &[(String, Value)] {
        let mut buf = [0u8; 4096];
        let mut source: &TcpStream = &self.stream;
        loop {
            match source.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => self.pending.extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
        }
        while let Some(pos) = self.pending.iter().position(|byte| *byte == b'\n') {
            let raw: Vec<u8> = self.pending.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&raw[..raw.len() - 1])
                .trim_end_matches('\r')
                .to_string();
            if let Some(rest) = line.strip_prefix("event: ") {
                self.event = rest.to_string();
            } else if let Some(rest) = line.strip_prefix("data: ") {
                if let Ok(data) = serde_json::from_str::<Value>(rest) {
                    self.frames.push((self.event.clone(), data));
                }
            }
        }
        &self.frames
    }

    /// Pump until `predicate` holds over the frames read so far, or `timeout` passes.
    fn wait_for(
        &mut self,
        timeout: Duration,
        predicate: impl Fn(&[(String, Value)]) -> bool,
    ) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if predicate(self.pump()) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// The `status` object of the terminal frame for `task_id`, once it has arrived.
    ///
    /// The answer a turn produced and the reason one failed are carried here and nowhere else:
    /// `tasks/get` reports a task's state but none of its text, so a case that wants either has
    /// to be watching the feed while the turn runs.
    fn terminal_status(&mut self, task_id: &str, timeout: Duration) -> Value {
        let is_terminal = |frames: &[(String, Value)]| {
            frames.iter().any(|(event, data)| {
                event == "status" && data["id"] == json!(task_id) && data["final"] == json!(true)
            })
        };
        assert!(
            self.wait_for(timeout, is_terminal),
            "no terminal status frame arrived for {task_id}; frames: {:#?}",
            self.frames
        );
        self.frames
            .iter()
            .find(|(event, data)| {
                event == "status" && data["id"] == json!(task_id) && data["final"] == json!(true)
            })
            .map(|(_, data)| data["status"].clone())
            .expect("just waited for it")
    }

    fn of_type(&self, event_type: &str) -> Vec<&Value> {
        self.frames
            .iter()
            .filter(|(event, _)| event == event_type)
            .map(|(_, data)| data)
            .collect()
    }
}

/// Whether any frame read so far is a non-final text frame — the proof that text streamed.
fn has_streamed_text(frames: &[(String, Value)]) -> bool {
    frames
        .iter()
        .any(|(event, data)| event == "text" && data["final"] != json!(true))
}

/// How long one scripted turn is given to finish. Generous, because it spans a real harness
/// start-up, a version probe and a WASM tool dispatch.
const TURN_TIMEOUT: Duration = Duration::from_secs(180);

// ── Cases ─────────────────────────────────────────────────────────────────────

/// S1 + S2. A coding task edits a file in the repository through the bridge and completes — and
/// the harness was offered only the bridge's tool, never one of its own.
///
/// The two facts are asserted together because they are one run: the tool array the model was
/// offered is only observable from the request bodies of a run that actually happened.
#[test]
#[ignore = "needs the real claude CLI, the real driver in the local store, and python3"]
fn a_coding_task_edits_a_file_through_the_bridge_and_completes() {
    let marker = "MURMUR-E2E-WROTE-THIS";
    let target = "notes/e2e.md";
    let mock = MockEndpoint::start(&format!(
        r#"call:{{"name":"{EDITOR_NAME}","args":{{"operation":"write_file","dest_path":"{target}","content":"{marker}\n"}}}}"#
    ));
    let capsule = E2eCapsule::launch("e2e-edit", &mock);
    let before = capsule.repo_tree();

    let mut watch = Watch::open(&capsule.url);
    let task = capsule.submit("msg-edit", "Write the marker into notes/e2e.md.");
    let response = capsule.wait_for_terminal(&task, TURN_TIMEOUT);
    assert_eq!(
        response["result"]["status"]["state"], "completed",
        "the task should have completed: {response}"
    );
    watch.wait_for(Duration::from_secs(10), |frames| {
        frames
            .iter()
            .any(|(event, data)| event == "status" && data["final"] == json!(true))
    });

    // The file changed on disk, inside the capsule's filesystem scope.
    let written = fs::read_to_string(capsule.repo.join(target))
        .unwrap_or_else(|error| panic!("{target} was not written: {error}"));
    assert!(
        written.contains(marker),
        "{target} does not hold the marker: {written:?}"
    );

    // And nothing outside it was: the only new paths are the ones the tool call created.
    let after = capsule.repo_tree();
    let new: Vec<&String> = after.iter().filter(|path| !before.contains(path)).collect();
    assert!(
        new.iter().all(|path| path.starts_with("notes")),
        "the run wrote paths the tool call did not ask for: {new:?}"
    );

    // The tool feed carries the call and its output, under the bare capsule tool name.
    let artifacts = watch.of_type("artifact");
    let editor_calls: Vec<&&Value> = artifacts
        .iter()
        .filter(|frame| frame["artifact"]["tool_name"] == json!(EDITOR_NAME))
        .collect();
    assert!(
        !editor_calls.is_empty(),
        "no artifact frame named the bare tool '{EDITOR_NAME}'; frames: {artifacts:#?}"
    );
    let call = &editor_calls[0]["artifact"];
    assert_eq!(
        call["is_error"],
        json!(false),
        "the tool call failed: {call}"
    );
    assert!(
        call["content"].as_str().is_some_and(|out| !out.is_empty()),
        "the tool call carried no output: {call}"
    );

    // Text streamed before the terminal frame.
    assert!(
        has_streamed_text(watch.pump()),
        "no text frame arrived before the terminal one"
    );

    // And the trace says this was the host's own harness, at the version the run recorded, with
    // the bridge offering exactly the capsule's one tool, bare.
    let prereqs = prereqs();
    let starts = capsule.harness_starts();
    assert_eq!(starts.len(), 1, "expected one harness_start: {starts:#?}");
    // Compared through `canonicalize`, because the runtime resolves the harness to the real file
    // and an installed `claude` is routinely a symlink from `PATH` into a versioned directory.
    let spawned = PathBuf::from(starts[0]["binary"].as_str().unwrap_or_default());
    assert_eq!(
        spawned.canonicalize().ok(),
        prereqs.harness.canonicalize().ok(),
        "the run spawned a binary other than the one on PATH: {}",
        starts[0]
    );
    assert_eq!(
        starts[0]["harness_version"],
        json!(prereqs.harness_version),
        "the version probe read something other than `{HARNESS_BINARY} --version`: {}",
        starts[0]
    );
    assert_eq!(
        starts[0]["bridge_tools"],
        json!([EDITOR_NAME]),
        "the bridge offered something other than the capsule's one tool: {}",
        starts[0]
    );

    // The harness authenticated out of the login store, not out of an API key — which is what a
    // subscription run does, and the thing this whole file exists to prove is reachable. The
    // endpoint it then reached is scripted, so the turn cost nothing either way.
    let warnings = capsule.trace_records("harness_warning");
    let auth_warnings: Vec<&Value> = warnings
        .iter()
        .filter(|record| record["code"] == json!(W_SEC_031))
        .collect();
    assert!(
        auth_warnings.is_empty(),
        "the run raised {W_SEC_031}, so the harness did not report subscription auth: \
         {auth_warnings:#?}"
    );

    // S2. Every request the harness made offered the bridge's tool and none of its own.
    let offered = mock.tools_offered();
    assert!(
        !offered.is_empty(),
        "the harness made no request that carried a tool array"
    );
    for tools in &offered {
        assert!(
            tools
                .iter()
                .any(|name| name == EDITOR_NAME || name.ends_with(&format!("__{EDITOR_NAME}"))),
            "a request did not offer the bridge's tool: {tools:?}"
        );
        for tool in tools {
            assert!(
                !HARNESS_OWN_TOOLS
                    .iter()
                    .any(|own| tool == own || tool.ends_with(own)),
                "the harness offered the model one of its own tools: {tool} (from {tools:?})"
            );
        }
    }
}

/// S3. A second task in the same context resumes the same harness session, and the harness — not
/// murmur, which writes no conversation record here — carried the first message into it.
#[test]
#[ignore = "needs the real claude CLI, the real driver in the local store, and python3"]
fn a_second_task_in_the_same_context_resumes_the_same_session() {
    let mock = MockEndpoint::start("text");
    let capsule = E2eCapsule::launch("e2e-resume", &mock);

    let mut watch = Watch::open(&capsule.url);
    let first = capsule.submit("msg-1", &format!("Remember the word {SECRET}."));
    let response = capsule.wait_for_terminal(&first, TURN_TIMEOUT);
    assert_eq!(
        response["result"]["status"]["state"], "completed",
        "the first task should have completed: {response}"
    );

    let second = capsule.submit("msg-2", "What word did I ask you to remember?");
    let response = capsule.wait_for_terminal(&second, TURN_TIMEOUT);
    assert_eq!(
        response["result"]["status"]["state"], "completed",
        "the second task should have completed: {response}"
    );

    // The endpoint reports whether the request body it was sent held the secret. The second task
    // never names it, so a `True` here can only have come from the harness replaying the first.
    let status = watch.terminal_status(&second, Duration::from_secs(15));
    let reply = status["response"].as_str().unwrap_or_default().to_string();
    assert!(
        reply.contains("saw_secret=True"),
        "the second turn did not carry the first message; reply was {reply:?}"
    );

    let starts = capsule.harness_starts();
    assert_eq!(
        starts.len(),
        2,
        "expected two harness_start records: {starts:#?}"
    );
    assert_eq!(starts[0]["session_mode"], json!("new"), "{}", starts[0]);
    assert_eq!(starts[1]["session_mode"], json!("resume"), "{}", starts[1]);
    assert_eq!(
        starts[0]["harness_session_id"], starts[1]["harness_session_id"],
        "the second run resumed a different session"
    );

    let entry = capsule
        .harness_session_entry()
        .expect("a harness-session.json should have been written");
    assert_eq!(
        entry["session_id"], starts[0]["harness_session_id"],
        "the stored entry holds a different session id: {entry}"
    );

    // Murmur keeps no message list on this transport, so there is nothing of its own to carry.
    assert!(
        !capsule.workdir.join("conversation.jsonl").exists(),
        "transport: process wrote a conversation record"
    );
}

/// S4. A cancel mid-turn ends the task `canceled`, and the conversation survives it.
#[test]
#[ignore = "needs the real claude CLI, the real driver in the local store, and python3"]
fn a_cancel_mid_turn_ends_canceled_and_the_next_task_still_resumes() {
    let mock = MockEndpoint::start("slow");
    let capsule = E2eCapsule::launch("e2e-cancel", &mock);

    let mut watch = Watch::open(&capsule.url);
    let task = capsule.submit("msg-slow", "Count slowly.");
    assert!(
        watch.wait_for(TURN_TIMEOUT, has_streamed_text),
        "no text arrived before the cancel, so the cancel would not have been mid-turn"
    );

    capsule.tasks_cancel(&task);
    let response = capsule.wait_for_terminal(&task, TURN_TIMEOUT);
    assert_eq!(
        response["result"]["status"]["state"], "canceled",
        "the canceled task ended in another state: {response}"
    );

    watch.wait_for(Duration::from_secs(15), |frames| {
        frames
            .iter()
            .any(|(event, data)| event == "status" && data["final"] == json!(true))
    });
    let terminal: Vec<&Value> = watch
        .of_type("status")
        .into_iter()
        .filter(|frame| frame["final"] == json!(true))
        .collect();
    assert_eq!(
        terminal.len(),
        1,
        "the canceled task got more than one terminal status: {terminal:#?}"
    );

    // The harness is gone: the runtime reaps it before it writes the terminal status.
    assert!(
        harness_pids().is_empty(),
        "a harness survived the cancel: {:?}",
        harness_pids()
    );

    // And the conversation was not cost. The next task in the same context resumes it.
    mock.scenario("text");
    let next = capsule.submit("msg-after-cancel", "Are you still there?");
    let response = capsule.wait_for_terminal(&next, TURN_TIMEOUT);
    assert_eq!(
        response["result"]["status"]["state"], "completed",
        "the task after the cancel should have completed: {response}"
    );

    let starts = capsule.harness_starts();
    assert_eq!(
        starts.len(),
        2,
        "expected two harness_start records: {starts:#?}"
    );
    assert_eq!(starts[1]["session_mode"], json!("resume"), "{}", starts[1]);
    assert_eq!(
        starts[0]["harness_session_id"], starts[1]["harness_session_id"],
        "the task after the cancel resumed a different session"
    );
}

/// S5. A 401 from the endpoint ends the task `failed`, naming `E-RUN-033` and the kind `auth`.
#[test]
#[ignore = "needs the real claude CLI, the real driver in the local store, and python3"]
fn a_mocked_401_ends_failed_with_an_auth_reason() {
    assert_turn_failure("e2e-401", "401", "auth");
}

/// S6. A 429 ends the task `failed` naming the kind `quota`.
///
/// Distinct from [`a_mocked_401_ends_failed_with_an_auth_reason`] because the harness reports both
/// as `subtype: "success"` with an `api_error_status`: a driver reading `subtype` alone would
/// report a rate limit as a successful reply.
#[test]
#[ignore = "needs the real claude CLI, the real driver in the local store, and python3"]
fn a_mocked_429_ends_failed_with_a_quota_reason() {
    assert_turn_failure("e2e-429", "429", "quota");
}

/// One failing-turn case: the scenario, the failure kind it must be reported as.
fn assert_turn_failure(name: &str, scenario: &str, kind: &str) {
    let mock = MockEndpoint::start(scenario);
    let capsule = E2eCapsule::launch(name, &mock);

    let mut watch = Watch::open(&capsule.url);
    let task = capsule.submit("msg-fail", "Do the thing.");
    let response = capsule.wait_for_terminal(&task, TURN_TIMEOUT);
    assert_eq!(
        response["result"]["status"]["state"], "failed",
        "a {scenario} should fail the task, never complete it: {response}"
    );

    let status = watch.terminal_status(&task, Duration::from_secs(15));
    assert_eq!(
        status["state"],
        json!("failed"),
        "the feed reported another state than the registry: {status}"
    );
    let reason = status["message"].as_str().unwrap_or_default().to_string();
    assert!(
        reason.contains("E-RUN-033"),
        "the failure did not name E-RUN-033: {reason:?}"
    );
    assert!(
        reason.contains(kind),
        "the failure did not name the kind '{kind}': {reason:?}"
    );

    // The endpoint's error body is not an answer, and must not have been written as one.
    let result = capsule.workdir.join("out").join("result.txt");
    if let Ok(text) = fs::read_to_string(&result) {
        assert!(
            !text.contains("MOCK:"),
            "the endpoint's error body was written to out/result.txt as if it were an answer: \
             {text:?}"
        );
    }
}

/// S7. `mur install` resolves the driver for the nexus manifest out of the local store, with no
/// network fetch, and `mur doctor` then passes over the same directory.
#[test]
#[ignore = "needs the real driver and every other declared artifact in the local store"]
fn mur_install_resolves_the_driver_from_the_local_store() {
    let prereqs = prereqs();
    scratch_home();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    // Every artifact the manifest declares, copied out of the host store and republished into a
    // scratch home. A name absent from the host store fails here naming itself.
    let manifest_source = common::fixture_path("subscription-e2e/murmur.yaml");
    let manifest_text = fs::read_to_string(&manifest_source).unwrap();
    let manifest: serde_yaml::Value = serde_yaml::from_str(&manifest_text).unwrap();
    let store = artifact_store();
    let declared: Vec<(String, String)> = manifest["artifacts"]
        .as_sequence()
        .expect("the manifest declares artifacts")
        .iter()
        .map(|entry| {
            (
                entry["name"].as_str().unwrap().to_string(),
                entry["version"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    for (name, version) in &declared {
        let zip = published_zip(&store, name, version).unwrap_or_else(|| {
            panic!(
                "the nexus manifest declares {name}@{version}, which is not published in {}\n  \
                 build and publish it from a default-artifacts checkout before running this case",
                store.display()
            )
        });
        publish_copy(
            &home,
            &StoreArtifact {
                name: name.clone(),
                version: version.clone(),
                zip,
            },
        );
    }

    // The nexus manifest, byte for byte, beside the files it names.
    fs::copy(&manifest_source, project.path().join("murmur.yaml")).unwrap();
    fs::write(
        project.path().join("instructions.md"),
        "You are the nexus capsule.\n",
    )
    .unwrap();

    let install = assert_cmd::Command::cargo_bin("mur")
        .unwrap()
        .arg("install")
        .current_dir(project.path())
        .env("HOME", home.path())
        // No registry to reach: a resolution that left the local store would fail here rather
        // than quietly fetching.
        .env("MURMUR_REGISTRY_URL", "http://127.0.0.1:9")
        .assert()
        .success();
    let installed = String::from_utf8_lossy(&install.get_output().stdout).to_string();
    let driver_dir = project
        .path()
        .join(".murmur")
        .join("artifacts")
        .join(DRIVER_NAME)
        .join(&prereqs.driver.version);
    assert!(
        driver_dir.is_dir(),
        "mur install left no {DRIVER_NAME}@{} in the project store; it printed:\n{installed}",
        prereqs.driver.version
    );

    let doctor = assert_cmd::Command::cargo_bin("mur")
        .unwrap()
        .arg("doctor")
        .current_dir(project.path())
        .env("HOME", home.path())
        .assert();
    let output = doctor.get_output();
    let printed = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    // `mur doctor` also reports host findings this case says nothing about, so the assertion is
    // narrowed to the inference block rather than to the command's exit code.
    let inference_errors: Vec<&str> = printed
        .lines()
        .filter(|line| line.contains("error") && line.contains("inference"))
        .collect();
    assert!(
        inference_errors.is_empty(),
        "mur doctor reported an error against the inference block: {inference_errors:?}\n\
         full output:\n{printed}"
    );
}

/// Pids of every `claude` this process is an ancestor of.
///
/// Read from `/proc` rather than from a pid the runtime handed out: the property is that no
/// harness outlives the cancel, which is a statement about the process table. Ancestry is part of
/// the question, not a refinement of it — a developer machine routinely runs `claude` for its own
/// reasons, and matching on the name alone would report those as survivors of this cancel.
fn harness_pids() -> Vec<i32> {
    let own = std::process::id() as i32;
    let mut pids = Vec::new();
    for entry in fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        let Ok(cmdline) = fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let argv0 = cmdline.split(|byte| *byte == 0).next().unwrap_or(&[]);
        let argv0 = String::from_utf8_lossy(argv0);
        if Path::new(argv0.as_ref())
            .file_name()
            .is_some_and(|name| name == HARNESS_BINARY)
            && descends_from(pid, own)
        {
            pids.push(pid);
        }
    }
    pids
}

/// Whether `ancestor` appears anywhere up `pid`'s parent chain.
///
/// The walk is bounded: a parent chain that has not reached pid 1 within this many steps means the
/// table shifted under the read, and no answer from it is worth trusting.
fn descends_from(pid: i32, ancestor: i32) -> bool {
    let mut current = pid;
    for _ in 0..64 {
        if current == ancestor {
            return true;
        }
        if current <= 1 {
            return false;
        }
        let Some(parent) = parent_pid(current) else {
            return false;
        };
        current = parent;
    }
    false
}

/// The parent pid out of `/proc/<pid>/stat`.
///
/// Read from the last `)` rather than by splitting on whitespace: the second field is the
/// executable name, and it can hold both spaces and parentheses.
fn parent_pid(pid: i32) -> Option<i32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let comm_end = stat.rfind(')')?;
    let mut fields = stat.get(comm_end + 1..)?.split_whitespace();
    let _state = fields.next()?;
    fields.next()?.parse().ok()
}
