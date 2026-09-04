//! A parent's runtime launching an approved child as an operating-system process of its own.
//!
//! Every case here runs the real thing: `mur-roost` on a loopback port over a real registry, the
//! built `mur` binary as the child, and the child's own runtime registering itself. What the
//! daemon does is referee; what this crate does is launch.
//!
//! One daemon, one registry and one `HOME` are shared by the whole suite, because `HOME` is
//! process-wide and a child resolves its artifacts through it. Each case gets its own parent
//! workdir, so nothing a case creates is visible to another.

#[path = "common/mod.rs"]
mod common;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use capsule_runtime::{
    child_workdir_for, delegation::SPAWNER_ENV, launch_child_capsule, ChildLaunchRequest,
    LaunchedChild, SpawnApproval, Spawner,
};
use common::{component, files_under, find_in_files, mur_binary, Roost, ScriptedServer};
use serde_json::{json, Value};
use tempfile::TempDir;

/// The capsule the parent session is registered as. Its manifest is the ceiling every child in
/// this suite is refereed against.
const PARENT_CAPSULE: &str = "suite-parent";
const PARENT_SESSION: &str = "ses_suite000000000000000000000parent";
const DRIVER: &str = "murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";

/// Two host variables the parent holds and each child declares at most one of.
const A_ONLY: &str = "WORKER_A_ONLY";
const B_ONLY: &str = "WORKER_B_ONLY";

struct Suite {
    roost: Roost,
    credential: String,
    /// Kept alive for the life of the process: `HOME` points inside it.
    _home: TempDir,
    /// Held for the life of the process: the agent children address it as their inference
    /// endpoint.
    _inference: ScriptedServer,
}

/// The one daemon, registry and `HOME` every case shares.
fn suite() -> &'static Suite {
    static SUITE: OnceLock<Suite> = OnceLock::new();
    SUITE.get_or_init(|| {
        let home = TempDir::new().unwrap();
        std::env::set_var("HOME", home.path());
        std::env::set_var(A_ONLY, "a-only-value");
        std::env::set_var(B_ONLY, "b-only-value");
        std::env::set_var("GITHUB_TOKEN", "parent-github-token");

        let inference = ScriptedServer::always_replying("done");
        let registry_path = home.path().join(".murmur").join("artifacts");
        std::fs::create_dir_all(&registry_path).unwrap();

        let roost = Roost::start_at(&registry_path, &[PARENT_CAPSULE]);
        common::publish_driver(
            &registry_path,
            DRIVER,
            DRIVER_VERSION,
            &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
        );

        // The parent's ceiling: every child below is within it on every axis. `containment` is
        // left undeclared, so `child-sealed` raising the floor is a rise rather than an escape.
        common::publish_capsule(
            &registry_path,
            PARENT_CAPSULE,
            "0.1.0",
            &format!(
                "artifacts: []\ncapabilities:\n  \
                 network:\n    allow: [{endpoint}]\n  \
                 env:\n    allow: [{A_ONLY}, {B_ONLY}, GITHUB_TOKEN, MURMUR_TEST_ALLOWED_VAR]\n  \
                 spawn:\n    allow: [child-agent, child-a, child-b, child-escape, child-sealed, \
                 child-leak, child-reader, child-repeat]\n",
                endpoint = inference.authority(),
            ),
            Some(&component("capsule-env-echo.wasm")),
        );

        // Script children that write into their own directory and try to leave it.
        for name in ["child-a", "child-b"] {
            let allow = match name {
                "child-a" => format!("  env:\n    allow: [{A_ONLY}]\n"),
                _ => format!("  env:\n    allow: [{B_ONLY}]\n"),
            };
            common::publish_capsule(
                &registry_path,
                name,
                "0.1.0",
                &format!("artifacts: []\ncapabilities:\n{allow}"),
                Some(&component("capsule-filesystem-escape.wasm")),
            );
        }
        common::publish_capsule(
            &registry_path,
            "child-escape",
            "0.1.0",
            "artifacts: []\n",
            Some(&component("capsule-filesystem-escape.wasm")),
        );

        // Script children that read the paths their parent named, and report what each attempt
        // was served.
        for name in ["child-reader", "child-repeat"] {
            common::publish_capsule(
                &registry_path,
                name,
                "0.1.0",
                "artifacts: []\n",
                Some(&component("capsule-path-reader.wasm")),
            );
        }
        common::publish_capsule(
            &registry_path,
            "child-leak",
            "0.1.0",
            "artifacts: []\ncapabilities:\n  env:\n    allow: [GITHUB_TOKEN, MURMUR_TEST_ALLOWED_VAR]\n",
            Some(&component("capsule-env-echo.wasm")),
        );

        // Agent children: they bind a port and serve A2A.
        for (name, containment) in [("child-agent", ""), ("child-sealed", "  containment: sealed\n")]
        {
            common::publish_capsule(
                &registry_path,
                name,
                "0.1.0",
                &format!(
                    "artifacts:\n  - name: {DRIVER}\n    version: {DRIVER_VERSION}\n    \
                     runtime: driver\ncapabilities:\n  network:\n    allow: [{endpoint}]\n\
                     {containment}inference:\n  transport: http\n  endpoint: {url}\n  \
                     model: test-model\n  api_key: test-key\n  driver:\n    artifact: {DRIVER}\n",
                    endpoint = inference.authority(),
                    url = inference.endpoint,
                ),
                None,
            );
        }

        // The parent's own registration: this is where its credential comes from, exactly as a
        // real parent's runtime would have obtained it at launch.
        let credential = roost.register(PARENT_SESSION, PARENT_CAPSULE, "0.1.0");
        std::env::set_var(capsule_runtime::MUR_BINARY_ENV, mur_binary());

        Suite {
            roost,
            credential,
            _home: home,
            _inference: inference,
        }
    })
}

/// One case's parent: an accessible workdir children are composed beneath, and the shared
/// credential the daemon minted for the parent session.
struct Parent {
    workdir: TempDir,
}

impl Parent {
    fn new() -> Self {
        Self {
            workdir: TempDir::new().unwrap(),
        }
    }

    fn dir(&self) -> &Path {
        self.workdir.path()
    }

    /// `POST /spawn`: the parent's runtime asking whether it may spawn this capsule.
    fn approve(&self, name: &str) -> (SpawnApproval, Value) {
        let suite = suite();
        let answer = suite.roost.permission(&suite.credential, name, "0.1.0");
        let token = answer["approval"]
            .as_str()
            .unwrap_or_else(|| panic!("spawn was refused: {answer}"))
            .to_string();
        (SpawnApproval::new(token), answer)
    }

    fn launch(&self, name: &str, env_allow: &[&str]) -> LaunchedChild {
        self.try_launch(name, env_allow)
            .unwrap_or_else(|error| panic!("launching '{name}' failed: {error}"))
    }

    /// Launch a `capsule-path-reader` child, run `place` on the directory the runtime made for
    /// it, ask it to read `targets`, and return it alongside one outcome per target.
    ///
    /// The placing runs beside the launch rather than before it because composing the child's
    /// directory, creating it and starting the child are one call: the directory does not exist
    /// until that call is under way. The child waits for `targets.txt`, which goes in last, so
    /// what it reports is what the parent placed rather than what it raced.
    fn launch_reading(
        &self,
        name: &str,
        targets: &[String],
        place: impl FnOnce(&Path),
    ) -> (LaunchedChild, Vec<String>) {
        let children = self.dir().join(".murmur").join("children");
        let before = dirs_under(&children);

        let child = std::thread::scope(|scope| {
            let launch = scope.spawn(|| self.launch(name, &[]));
            if let Some(dir) = wait_for_new_dir(&children, &before) {
                place(&dir);
                // Renamed into place, so the child that is polling for the list never reads half
                // of one.
                std::fs::write(dir.join("targets.part"), targets.join("\n")).unwrap();
                std::fs::rename(dir.join("targets.part"), dir.join("targets.txt")).unwrap();
            }
            launch.join().unwrap()
        });

        let report = std::fs::read_to_string(child.workdir.join("out").join("result.txt"))
            .unwrap_or_else(|error| panic!("'{name}' wrote no result file: {error}"));
        let outcomes = report.lines().map(str::to_string).collect();
        (child, outcomes)
    }

    fn try_launch(
        &self,
        name: &str,
        env_allow: &[&str],
    ) -> Result<LaunchedChild, capsule_runtime::RuntimeError> {
        let (grant, _) = self.approve(name);
        launch_child_capsule(ChildLaunchRequest {
            parent_accessible_workdir: self.dir().to_path_buf(),
            capsule_name: name.to_string(),
            capsule_version: "0.1.0".to_string(),
            grant,
            child_env_allow: env_allow.iter().map(|name| name.to_string()).collect(),
            roost_url: suite().roost.url.clone(),
            // Nothing in this suite delegates: no handle is injected and no watcher runs.
            spawner: None,
            completion_deadline: None,
        })
    }
}

/// Wait for a predicate to hold, up to `deadline`.
fn wait_for(what: &str, timeout: Duration, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if ready() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for {what}");
}

/// The directories directly under `parent`, or none if it does not exist yet.
fn dirs_under(parent: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return Vec::new();
    };
    entries.flatten().map(|entry| entry.path()).collect()
}

/// The one directory under `parent` that was not there in `before`.
fn wait_for_new_dir(parent: &Path, before: &[PathBuf]) -> Option<PathBuf> {
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if let Some(fresh) = dirs_under(parent)
            .into_iter()
            .find(|dir| !before.contains(dir))
        {
            return Some(fresh);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

fn trace_of(child: &LaunchedChild) -> PathBuf {
    child
        .workdir
        .join(".murmur")
        .join(&child.session_id)
        .join("trace.jsonl")
}

// ── 1. The happy path ─────────────────────────────────────────────────────────

/// A spawn approved by the daemon, launched by this runtime, answering A2A from a process of its
/// own — and known to the daemon as running because it registered for itself.
#[test]
fn a_parents_runtime_launches_an_approved_child_as_a_separate_process() {
    let suite = suite();
    let parent = Parent::new();

    let (_, answer) = parent.approve("child-agent");
    // Permission, not a process: the daemon answered with an artifact and an expiry, and nothing
    // that could address anything running.
    assert_eq!(answer["name"], "child-agent");
    assert_eq!(answer["version"], "0.1.0");
    assert!(answer["sha256"].as_str().unwrap().len() == 64);
    assert!(answer.get("capsule_url").is_none());
    assert!(answer.get("session_id").is_none());

    let mut child = parent.launch("child-agent", &[]);

    assert_ne!(child.pid(), std::process::id());
    assert!(child.capsule_url.starts_with("http://"), "{child:?}");
    assert!(child.session_id.starts_with("ses_"), "{child:?}");
    #[cfg(target_os = "linux")]
    {
        // The runtime that owns the child's Wasmtime store is a different image entirely: this
        // process is a test binary, and the child is `mur`.
        let comm = std::fs::read_to_string(format!("/proc/{}/comm", child.pid())).unwrap();
        assert_eq!(comm.trim(), "mur");
    }

    // The daemon knows it, by the session id the child minted for itself.
    assert_eq!(
        suite.roost.status(&child.session_id).as_deref(),
        Some("running"),
        "the child did not register"
    );
    assert_eq!(
        suite.roost.status_over_http(&child.session_id)["status"],
        "running"
    );

    // And it answers A2A on the url it reported.
    let response = common::request(
        "POST",
        &child.capsule_url,
        Some(
            &json!({
                "jsonrpc": "2.0",
                "id": "child-launch-1",
                "method": "message/send",
                "params": {"message": {"messageId": "m-1", "role": "user",
                                       "parts": [{"text": "hello"}]}}
            })
            .to_string(),
        ),
        &[],
    )
    .expect("the child answers message/send");
    assert!(
        response["result"]["id"].as_str().is_some(),
        "message/send returned {response}"
    );

    child.shutdown().unwrap();
}

// ── 2. Two children of the same parent are independent ────────────────────────

/// Each child's environment is built from that child's own declaration, and each child's
/// directory is a sibling of the other's rather than beneath it.
#[test]
fn two_children_of_the_same_parent_are_independent() {
    let parent = Parent::new();

    let a = parent.launch("child-a", &[A_ONLY]);
    let b = parent.launch("child-b", &[B_ONLY]);

    // The environment the parent built for each, which is the environment `execve` received.
    let names = |child: &LaunchedChild| -> Vec<String> {
        child.env.iter().map(|(key, _)| key.clone()).collect()
    };
    assert!(names(&a).contains(&A_ONLY.to_string()));
    assert!(!names(&a).contains(&B_ONLY.to_string()));
    assert!(names(&b).contains(&B_ONLY.to_string()));
    assert!(!names(&b).contains(&A_ONLY.to_string()));
    // Neither was handed the parent's own environment wholesale.
    assert!(!names(&a).contains(&"GITHUB_TOKEN".to_string()));

    // The same read through the kernel's own record, on a host that allows it. `mur` marks itself
    // non-dumpable at startup (`security::harden_process_dumpable`), so this is normally
    // unreadable even to the launching parent — that is the point of the flag, and the assertion
    // above is what stands in for it.
    for (child, present, absent) in [(&a, A_ONLY, B_ONLY), (&b, B_ONLY, A_ONLY)] {
        if let Ok(raw) = std::fs::read(format!("/proc/{}/environ", child.pid())) {
            let observed = String::from_utf8_lossy(&raw);
            assert!(observed.contains(present), "{observed}");
            assert!(!observed.contains(absent), "{observed}");
        }
    }

    // Siblings, neither beneath the other, neither the parent's own.
    assert_ne!(a.workdir, b.workdir);
    assert!(!a.workdir.starts_with(&b.workdir));
    assert!(!b.workdir.starts_with(&a.workdir));
    assert_ne!(a.workdir, parent.dir());
    assert_eq!(a.workdir.parent(), b.workdir.parent());
    assert!(a.workdir.starts_with(parent.dir()));

    // Writing into one leaves the other untouched.
    std::fs::write(a.workdir.join("only-in-a.txt"), "a").unwrap();
    assert!(!b.workdir.join("only-in-a.txt").exists());

    // Each child's session artifacts are under its own directory.
    assert!(a.workdir.join(".murmur").join(&a.session_id).is_dir());
    assert!(b.workdir.join(".murmur").join(&b.session_id).is_dir());
    assert!(!a.workdir.join(".murmur").join(&b.session_id).exists());
}

/// The environment a child that declares nothing is handed: the names the runtime owns, and
/// nothing else. An empty `capabilities.env.allow` is a complete answer rather than a hole, so a
/// list that arrives empty — which is what a child declaring none of its parent's variables
/// yields — leaves the child holding the runtime-owned names alone.
#[test]
fn a_child_declaring_no_variables_gets_only_the_runtime_owned_names() {
    let parent = Parent::new();
    let keys = |child: &LaunchedChild| -> Vec<String> {
        child.env.iter().map(|(key, _)| key.clone()).collect()
    };

    let plain = parent.launch("child-a", &[]);
    assert_eq!(keys(&plain), ["PATH", "HOME", "MURMUR_ROOST_URL"]);

    // The same launch with lineage. It reports to nobody, so the handle is the whole of what the
    // spawner adds and no watcher runs.
    let (grant, _) = parent.approve("child-b");
    let delegated = launch_child_capsule(ChildLaunchRequest {
        parent_accessible_workdir: parent.dir().to_path_buf(),
        capsule_name: "child-b".to_string(),
        capsule_version: "0.1.0".to_string(),
        grant,
        child_env_allow: Vec::new(),
        roost_url: suite().roost.url.clone(),
        spawner: Some(Spawner {
            session_id: PARENT_SESSION.to_string(),
            context_id: "ctx_runtime_owned_names".to_string(),
            report_to: None,
        }),
        completion_deadline: None,
    })
    .expect("a child that declares no variables launches");
    assert_eq!(
        keys(&delegated),
        ["PATH", "HOME", "MURMUR_ROOST_URL", SPAWNER_ENV]
    );
}

// ── 3. A child cannot reach out of its own directory ──────────────────────────

/// The child's only preopen is the directory the parent made for it. A file the parent placed
/// there is inside it; the parent's own workdir root and a sibling's directory are not, and the
/// attempt to traverse out is refused rather than served.
#[test]
fn a_child_cannot_reach_its_parents_workdir_or_its_siblings() {
    let parent = Parent::new();
    std::fs::write(parent.dir().join("parent-secret.txt"), "parent only").unwrap();

    let sibling = parent.launch("child-b", &[B_ONLY]);
    std::fs::write(sibling.workdir.join("sibling-secret.txt"), "sibling only").unwrap();

    // The escaping child gets its own input from the parent, placed in its own directory before it
    // is asked to do anything with it.
    let escaping_dir = child_workdir_for(parent.dir(), "child-escape");
    let child = parent.launch("child-escape", &[]);

    // Its attempt to reach `../../outside.txt` — out of its preopen and into the parent's
    // `.murmur/` — was refused by the WASI layer rather than served.
    assert_eq!(
        std::fs::read_to_string(child.workdir.join("out").join("result.txt")).unwrap(),
        "blocked"
    );
    // Nothing it tried to write landed anywhere: not in the parent's root, not in `.murmur/`, not
    // in the sibling's directory.
    assert!(!parent.dir().join("outside.txt").exists());
    assert!(!parent.dir().join(".murmur").join("outside.txt").exists());
    assert!(!sibling.workdir.join("outside.txt").exists());
    assert_eq!(
        std::fs::read_to_string(sibling.workdir.join("sibling-secret.txt")).unwrap(),
        "sibling only"
    );
    assert_eq!(
        std::fs::read_to_string(parent.dir().join("parent-secret.txt")).unwrap(),
        "parent only"
    );

    // The child's own directory is the preopen it does reach: its output landed there, written
    // from inside the guest, while the traversal above was refused.
    assert_eq!(child.workdir.parent(), escaping_dir.parent());
    assert!(child.workdir.join("out").join("result.txt").is_file());

    // And reads run the same way round. A third child is pointed at existing files: its own input,
    // the parent's secret and the sibling's. Only the one in its own directory is served.
    let sibling_dir = sibling.workdir.file_name().unwrap().to_string_lossy();
    let mut targets = vec![
        "./input.txt".to_string(),
        "../../../parent-secret.txt".to_string(),
        format!("../{sibling_dir}/sibling-secret.txt"),
    ];
    let mut expected = vec!["served placed by the parent", "blocked", "blocked"];

    // Symlinks are the other way out of a preopen, and a trailing slash on one is the shape
    // RUSTSEC-2026-0269 escaped through, so both forms are attempted alongside the plain
    // traversals above.
    #[cfg(unix)]
    {
        targets.extend([
            "secret-link".to_string(),
            "secret-link/".to_string(),
            "root-link/parent-secret.txt".to_string(),
        ]);
        expected.extend(["blocked", "blocked", "blocked"]);
    }

    let (reader, outcomes) = parent.launch_reading("child-reader", &targets, |dir| {
        std::fs::write(dir.join("input.txt"), "placed by the parent").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(
                parent.dir().join("parent-secret.txt"),
                dir.join("secret-link"),
            )
            .unwrap();
            std::os::unix::fs::symlink(parent.dir(), dir.join("root-link")).unwrap();
        }
    });
    assert_eq!(outcomes, expected, "reader in {}", reader.workdir.display());

    // Every refused path names content that is really there: resolved from the reader's own
    // directory, or followed from the links planted in it, they are the parent's secret and the
    // sibling's. So `blocked` is a refusal and not a miss.
    assert!(reader.workdir.join("../../../parent-secret.txt").is_file());
    assert!(reader
        .workdir
        .join(format!("../{sibling_dir}/sibling-secret.txt"))
        .is_file());
    #[cfg(unix)]
    {
        assert_eq!(
            std::fs::read_to_string(reader.workdir.join("secret-link")).unwrap(),
            "parent only"
        );
        assert_eq!(
            std::fs::read_to_string(reader.workdir.join("root-link").join("parent-secret.txt"))
                .unwrap(),
            "parent only"
        );
    }
}

// ── 4. A directory per delegation ─────────────────────────────────────────────

/// Delegating the same capsule name and version twice yields two directories, both beneath the
/// parent's accessible workdir, both owner-only, and the second does not disturb the first.
#[test]
fn each_delegation_gets_a_directory_of_its_own() {
    let parent = Parent::new();

    let (first, first_read) =
        parent.launch_reading("child-repeat", &["./input.txt".to_string()], |dir| {
            std::fs::write(dir.join("input.txt"), "first only").unwrap();
        });
    std::fs::write(first.workdir.join("placed-by-the-parent.txt"), "kept").unwrap();

    let first_dir = first
        .workdir
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let (second, second_read) = parent.launch_reading(
        "child-repeat",
        &[
            "./input.txt".to_string(),
            format!("../{first_dir}/input.txt"),
        ],
        |dir| {
            std::fs::write(dir.join("input.txt"), "second only").unwrap();
        },
    );

    // What the parent put in each directory belongs to that delegation alone: each child was
    // served its own input, and the second was refused the first's.
    assert_eq!(first_read, ["served first only"]);
    assert_eq!(second_read, ["served second only", "blocked"]);
    // The first child's input is still on disk where the second was pointed at it, so the second
    // was refused a file rather than sent after one that had gone.
    assert!(second
        .workdir
        .join(format!("../{first_dir}/input.txt"))
        .is_file());

    assert_ne!(first.workdir, second.workdir);
    assert_ne!(first.workdir, parent.dir());
    assert_ne!(second.workdir, parent.dir());
    assert!(first.workdir.starts_with(parent.dir()));
    assert!(second.workdir.starts_with(parent.dir()));
    assert_eq!(
        std::fs::read_to_string(first.workdir.join("placed-by-the-parent.txt")).unwrap(),
        "kept"
    );

    // The naming rule, and its permissions.
    for dir in [&first.workdir, &second.workdir] {
        assert_eq!(
            dir.parent(),
            Some(parent.dir().join(".murmur").join("children").as_path())
        );
        let name = dir.file_name().unwrap().to_string_lossy();
        assert_eq!(name.len(), "child-repeat-".len() + 16);
        assert!(name.starts_with("child-repeat-"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(dir).unwrap().permissions().mode() & 0o777,
                0o700,
                "{}",
                dir.display()
            );
        }
    }
}

// ── 5. No token reaches anything a capsule can read ───────────────────────────

/// The credential and the approval appear in no file, no trace line, no guest environment and no
/// error string.
#[test]
fn neither_token_leaks_into_a_file_a_trace_an_environment_or_an_error() {
    let suite = suite();
    let parent = Parent::new();
    let (grant, _) = parent.approve("child-leak");
    let approval = grant.expose().to_string();

    let child = launch_child_capsule(ChildLaunchRequest {
        parent_accessible_workdir: parent.dir().to_path_buf(),
        capsule_name: "child-leak".to_string(),
        capsule_version: "0.1.0".to_string(),
        grant,
        child_env_allow: vec![
            "GITHUB_TOKEN".to_string(),
            "MURMUR_TEST_ALLOWED_VAR".to_string(),
        ],
        roost_url: suite.roost.url.clone(),
        spawner: None,
        completion_deadline: None,
    })
    .expect("the leak fixture launches");

    // Not in the argument vector, and not in the environment: the grant travelled on standard
    // input, which nothing in `/proc` exposes.
    assert!(!child
        .argv
        .iter()
        .any(|argument| argument.contains(&approval)));
    assert!(!child
        .env
        .iter()
        .any(|(key, value)| key.contains(&approval) || value.contains(&approval)));
    assert!(!child
        .env
        .iter()
        .any(|(_, value)| value.contains(&suite.credential)));

    // Not in any file either process wrote.
    for root in [parent.dir(), child.workdir.as_path()] {
        for token in [&approval, &suite.credential] {
            assert_eq!(
                find_in_files(root, token),
                None,
                "a token appears under {}",
                root.display()
            );
        }
    }
    assert!(
        files_under(&child.workdir)
            .iter()
            .any(|path| path.ends_with("out/result.txt")),
        "the fixture wrote nothing, so this proves nothing"
    );

    // The guest's own view of its environment: the fixture reports what it saw for the two names
    // it probes, and the host's `GITHUB_TOKEN` is stripped before it reaches WASI even though the
    // child declared it.
    let observed = std::fs::read_to_string(child.workdir.join("out").join("result.txt")).unwrap();
    assert!(observed.contains("GITHUB_TOKEN=absent"), "{observed}");
    assert!(!observed.contains(&approval), "{observed}");
    assert!(!observed.contains(&suite.credential), "{observed}");

    // And not in a refusal: registering with a daemon that is not there names the daemon and the
    // reason, and nothing else.
    let error = launch_child_capsule(ChildLaunchRequest {
        parent_accessible_workdir: parent.dir().to_path_buf(),
        capsule_name: "child-leak".to_string(),
        capsule_version: "0.1.0".to_string(),
        grant: SpawnApproval::new(approval.clone()),
        child_env_allow: Vec::new(),
        roost_url: "http://127.0.0.1:1".to_string(),
        spawner: None,
        completion_deadline: None,
    })
    .expect_err("a child cannot register with a daemon that is not listening");
    let message = error.to_string();
    assert!(!message.contains(&approval), "{message}");
    assert!(!message.contains(&suite.credential), "{message}");
}

/// The registration refusal a capsule that declares spawn meets when the daemon is unreachable
/// names the daemon and the reason, and carries no token.
#[test]
fn a_registration_failure_names_the_daemon_and_the_reason() {
    let error = capsule_runtime::register_session(
        "http://127.0.0.1:1",
        "ses_unreachable",
        "child-leak",
        "0.1.0",
        Some(&SpawnApproval::new("msa1.secret.token".to_string())),
    )
    .expect_err("nothing is listening on port 1");

    let message = error.to_string();
    assert!(message.contains("http://127.0.0.1:1"), "{message}");
    assert!(message.contains("mur-roost"), "{message}");
    assert!(message.contains("capabilities.spawn.allow"), "{message}");
    assert!(!message.contains("msa1.secret.token"), "{message}");
}

// ── 6. Containment, judged in the child's own process ─────────────────────────

/// A child declaring the strongest floor is judged by the host it actually runs on — which is its
/// own process, probed at its own launch. Both outcomes are asserted; neither is skipped.
#[test]
fn a_sealed_child_either_achieves_its_floor_or_is_refused() {
    let parent = Parent::new();
    let host_reaches_sealed =
        capsule_runtime::detect_achieved_containment() == murmur_artifact::ContainmentClass::Sealed;

    match parent.try_launch("child-sealed", &[]) {
        Ok(mut child) => {
            assert!(
                host_reaches_sealed,
                "a sealed child launched on a host that cannot provide sealed"
            );
            let trace = trace_of(&child);
            wait_for("the child's trace", Duration::from_secs(30), || {
                trace.is_file()
                    && std::fs::read_to_string(&trace)
                        .unwrap()
                        .contains("session_start")
            });
            let session_start: Value = std::fs::read_to_string(&trace)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str::<Value>(line).unwrap())
                .find(|event| event["event_type"] == "session_start")
                .expect("the child's trace records a session_start");
            assert_eq!(
                session_start["effective_grants"]["achieved_containment"], "sealed",
                "{session_start}"
            );
            child.shutdown().unwrap();
        }
        Err(error) => {
            assert!(
                !host_reaches_sealed,
                "a sealed child was refused on a host that provides sealed: {error}"
            );
            let message = error.to_string();
            assert!(
                message.contains("declared containment class 'sealed'"),
                "{message}"
            );
            assert!(
                message.contains("is not achievable on this host"),
                "{message}"
            );
        }
    }
}

// ── 7. The naming rule ────────────────────────────────────────────────────────

#[test]
fn the_child_directory_naming_rule_is_stable_and_unique() {
    let parent = TempDir::new().unwrap();
    let first = child_workdir_for(parent.path(), "worker");
    let second = child_workdir_for(parent.path(), "worker");

    assert_ne!(first, second);
    assert_eq!(
        first.parent(),
        Some(parent.path().join(".murmur").join("children").as_path())
    );
    let suffix = first
        .file_name()
        .unwrap()
        .to_string_lossy()
        .strip_prefix("worker-")
        .unwrap()
        .to_string();
    assert_eq!(suffix.len(), 16);
    assert!(suffix.chars().all(|c| c.is_ascii_hexdigit()));
}

// ── 8. A parent that returns early leaves no orphan ───────────────────────────

/// `LaunchedChild`'s `Drop` terminates and reaps, so a parent that returns — or panics — before
/// shutting a child down leaves no process holding a port and a directory.
#[test]
fn dropping_a_launched_child_terminates_and_reaps_it() {
    let parent = Parent::new();
    let pid = {
        let child = parent.launch("child-agent", &[]);
        let pid = child.pid();
        assert!(process_is_alive(pid));
        pid
    };

    wait_for(
        "the dropped child to be reaped",
        Duration::from_secs(10),
        || !process_is_alive(pid),
    );
}

/// `release` is the one way out of that: the parent stops owning the child's lifetime, and the
/// handle is consumed rather than dropped normally. The process keeps running, and this suite ends
/// it itself, because these launches name no completion address and so start no watcher — after a
/// release nothing else in the runtime will.
#[test]
fn a_released_child_survives_the_handle_that_launched_it() {
    let parent = Parent::new();
    let pid = {
        let child = parent.launch("child-agent", &[]);
        let pid = child.pid();
        assert!(process_is_alive(pid));

        child.release();
        pid
    };

    // Long enough that a `Drop` which killed would have been observed.
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        process_is_alive(pid),
        "a released child is not signalled when its launch handle is dropped"
    );

    // Nothing owns this process now, so the test ends it rather than leaving it holding a daemon
    // slot for the life of the binary.
    let _ = std::process::Command::new("kill")
        .arg(pid.to_string())
        .status();
}

#[cfg(target_os = "linux")]
fn process_is_alive(pid: u32) -> bool {
    // A reaped child has no `/proc` entry at all; a zombie would still have one, which is exactly
    // what `Drop`'s `wait` exists to prevent.
    Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(not(target_os = "linux"))]
fn process_is_alive(_pid: u32) -> bool {
    false
}
