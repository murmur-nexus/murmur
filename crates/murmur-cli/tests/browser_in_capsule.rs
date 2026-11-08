//! A headless Chrome started from a shell command inside a capsule, driven through the real `mur`
//! binary on a scripted model with `transport: http`.
//!
//! The browser is the host's: `MUR_TEST_CHROME`, else the newest Playwright
//! `chrome-headless-shell` under the real `HOME`, else `chrome-headless-shell` or `chromium` on
//! `PATH`. A host with none skips with a `[SKIP-HOST]` line. Scratch directories live under
//! `CARGO_TARGET_TMPDIR`, never `/tmp`.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    path::{Path, PathBuf},
    process::Output,
};

use assert_cmd::Command;
use serde_json::{json, Value};
use tempfile::TempDir;

const DRIVER: &str = "local/murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.0";

/// The page's marker, on a line of its own, which `--dump-dom` prints back when the page loaded.
const MARKER: &str = "murmur-chrome-marker";

/// What Chrome prints when its GPU process dies at start.
const GPU_EXITED: &str = "GPU process exited unexpectedly";

/// The flags Playwright starts Chrome with that bear on containment, and `--dump-dom`, which
/// prints the loaded page and exits.
const CHROME_FLAGS: &[&str] = &[
    "--headless",
    "--no-sandbox",
    "--disable-dev-shm-usage",
    "--disable-gpu",
    "--no-first-run",
    "--no-default-browser-check",
    "--dump-dom",
];

/// The host's headless Chrome, in the documented search order, or `None`.
fn find_chrome() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("MUR_TEST_CHROME") {
        return Some(PathBuf::from(path));
    }
    if let Some(home) = std::env::var_os("HOME") {
        let cache = Path::new(&home).join(".cache/ms-playwright");
        let mut found: Vec<PathBuf> = fs::read_dir(&cache)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("chromium_headless_shell-")
            })
            .map(|entry| {
                entry
                    .path()
                    .join("chrome-headless-shell-linux64/chrome-headless-shell")
            })
            .filter(|path| path.is_file())
            .collect();
        // `chromium_headless_shell-<build>`: the highest build is the newest.
        found.sort_by_key(|path| {
            path.parent()
                .and_then(Path::parent)
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
                .and_then(|name| name.rsplit('-').next())
                .and_then(|build| build.parse::<u64>().ok())
                .unwrap_or(0)
        });
        if let Some(newest) = found.pop() {
            return Some(newest);
        }
    }
    let path = std::env::var_os("PATH")?;
    for name in ["chrome-headless-shell", "chromium"] {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// One scripted call: the call id, the tool and its input.
struct Call {
    id: &'static str,
    tool: &'static str,
    input: Value,
}

struct Fixture {
    _root: TempDir,
    home: PathBuf,
    project: PathBuf,
    /// `PATH` for `mur`: the browser's folder first, so `shell.allow` names it.
    path: std::ffi::OsString,
}

impl Fixture {
    fn new(chrome: &Path) -> Self {
        let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let base = root.path().canonicalize().unwrap();
        let home = base.join("home");
        let project = base.join("project");
        let artifacts = base.join("artifacts");
        for dir in [&home, &project, &artifacts] {
            fs::create_dir_all(dir).unwrap();
        }
        let mut dirs = vec![chrome.parent().unwrap().to_path_buf()];
        dirs.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        let fixture = Self {
            _root: root,
            home,
            project,
            path: std::env::join_paths(dirs).unwrap(),
        };
        let driver = common::create_driver_artifact(
            &artifacts,
            DRIVER,
            DRIVER_VERSION,
            &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
        );
        fixture.write_manifest("http://127.0.0.1:9", "scoped", "");
        fixture
            .mur()
            .args(["install", driver.to_str().unwrap(), "--no-precompile"])
            .assert()
            .success();
        fixture
    }

    /// The capsule at `containment` with `capabilities` lines, its driver pointed at `endpoint`.
    fn write_manifest(&self, endpoint: &str, containment: &str, capabilities: &str) -> PathBuf {
        let manifest = self.project.join("murmur.yaml");
        fs::write(
            &manifest,
            format!(
                "name: browser-in-capsule\nversion: 0.0.1\nartifacts:\n  - name: {DRIVER}\n    \
                 version: {DRIVER_VERSION}\n    runtime: driver\n    gateway:\n      endpoint: \
                 {endpoint}\n      api_key: test-key\ncapabilities:\n  containment: \
                 {containment}\n  network:\n    allow:\n      - {endpoint}\n{capabilities}\
                 inference:\n  transport: http\n  model: test-model\n  driver:\n    artifact: \
                 {DRIVER}\n"
            ),
        )
        .unwrap();
        manifest
    }

    fn mur(&self) -> Command {
        let mut command = Command::cargo_bin("mur").unwrap();
        command
            .env("HOME", &self.home)
            .env("PATH", &self.path)
            .current_dir(&self.project);
        command
    }

    /// One `mur run` of a capsule at `containment` with `capabilities` lines, in `workdir`, the
    /// model making each call in order.
    fn run(&self, containment: &str, capabilities: &str, workdir: &Path, calls: Vec<Call>) -> Run {
        let total = calls.len() + 1;
        let mut next = 0usize;
        let server = common::ScriptedServer::start_answering(total, move |_| {
            let index = next;
            next += 1;
            match calls.get(index) {
                Some(call) => common::tool_use_response(call.id, call.tool, call.input.clone()),
                None => end_turn(),
            }
        });
        let manifest = self.write_manifest(&server.endpoint, containment, capabilities);
        let output = self
            .mur()
            .args(["run", "--manifest", manifest.to_str().unwrap()])
            .args(["--workdir", workdir.to_str().unwrap()])
            .args(["--task", "Run each call once.", "--verbose"])
            .write_stdin("")
            .timeout(std::time::Duration::from_secs(300))
            .output()
            .unwrap();
        assert!(
            output.status.success() && stdout(&output).contains("status:  ok"),
            "the run must finish ok\nstdout: {}\nstderr: {}",
            stdout(&output),
            stderr(&output)
        );
        Run {
            output,
            requests: server.requests(),
        }
    }
}

struct Run {
    output: Output,
    requests: Vec<Value>,
}

impl Run {
    fn result_text(&self, id: &str) -> String {
        common::tool_result_text(&self.requests, id)
            .unwrap_or_else(|| panic!("no tool result for {id}; stderr: {}", stderr(&self.output)))
    }
}

fn end_turn() -> String {
    json!({
        "id": "msg_end",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": "done"}],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {"input_tokens": 1, "output_tokens": 1},
    })
    .to_string()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The system library folder. Chrome loads its NSS modules from it with `dlopen`, so they are
/// not in the `DT_NEEDED` closure the runtime grants from the binary, and a capsule that runs
/// native Node addons grants it too. It puts `libpci.so.3` in reach of every command.
const LIBRARY_FOLDER: &str = "/usr/lib/x86_64-linux-gnu";

/// What `libpci`, which Chrome's GPU process loads with `dlopen`, reads: the PCI bus and the
/// device folders its entries link to. Refused, `libpci` exits the GPU process with status 1.
const PCI_SYSFS: &[&str] = &["/sys/bus/pci", "/sys/devices"];

/// `capabilities:` lines granting `bash` and `chrome` (by file name) to shell commands, with the
/// grants headless Chrome reads outside the work folder, the library folder, and `resources` V8
/// runs under.
fn chrome_capabilities(chrome: &Path) -> String {
    let name = chrome.file_name().unwrap().to_str().unwrap();
    let folder = chrome.canonicalize().unwrap();
    let folder = folder.parent().unwrap().to_path_buf();
    let mut dirs: Vec<(PathBuf, bool)> = vec![(folder, true)];
    for dir in ["/proc", "/etc/fonts", "/usr/share/fonts"] {
        dirs.push((PathBuf::from(dir), true));
    }
    dirs.push((PathBuf::from(LIBRARY_FOLDER), false));
    for dir in PCI_SYSFS {
        dirs.push((PathBuf::from(dir), true));
    }
    let mut yaml = format!(
        "  shell:\n    allow:\n      - bash\n      - {name}\n    interpreter_runtime:\n      - \
         binary: {name}\n        dirs:\n"
    );
    // A host without one of these paths (another architecture's library folder, a container
    // without `/sys/bus/pci`) has nothing there to grant.
    for (path, list_dir) in dirs.into_iter().filter(|(path, _)| path.exists()) {
        yaml.push_str(&format!(
            "          - path: {}\n            list_dir: {list_dir}\n",
            path.display()
        ));
    }
    // V8 reserves tens of GiB of address space per process; the cgroup caps what it uses.
    yaml.push_str(
        "  resources:\n    memory_bytes: 274877906944\n    cgroup_pids_max: 1024\n    \
         cgroup_memory_bytes: 4294967296\n",
    );
    yaml
}

/// A work folder holding the page Chrome loads.
fn workdir_with_page(fixture: &Fixture, label: &str) -> PathBuf {
    let workdir = fixture.project.join(format!("work-{label}"));
    let _ = fs::remove_dir_all(&workdir);
    fs::create_dir_all(&workdir).unwrap();
    fs::write(
        workdir.join("page.html"),
        format!("<html><body><p id=\"m\">\n{MARKER}\n</p></body></html>\n"),
    )
    .unwrap();
    workdir
}

/// The `bash` command that starts `chrome` on the work folder's page, its output in
/// `<label>.out` and `<label>.err` in the work folder.
fn chrome_command(chrome: &Path, workdir: &Path, label: &str) -> String {
    let name = chrome.file_name().unwrap().to_str().unwrap();
    format!(
        "{name} {} file://{}/page.html >{label}.out 2>{label}.err; echo chrome-exit=$?",
        CHROME_FLAGS.join(" "),
        workdir.display()
    )
}

/// Chrome started from a `bash` shell command in a capsule keeps its GPU process and loads a page
/// from the work folder, at every tier the host reaches.
#[test]
fn chrome_from_a_shell_command_loads_a_page_in_a_capsule() {
    let name = "chrome_from_a_shell_command_loads_a_page_in_a_capsule";
    if common::skip_without_host_support(name) {
        return;
    }
    let Some(chrome) = find_chrome() else {
        eprintln!(
            "[SKIP-HOST] {name}: no chrome-headless-shell or chromium found (set MUR_TEST_CHROME)"
        );
        return;
    };
    let fixture = Fixture::new(&chrome);
    for tier in common::tiers(name) {
        let workdir = workdir_with_page(&fixture, tier);
        let run = fixture.run(
            tier,
            &chrome_capabilities(&chrome),
            &workdir,
            vec![Call {
                id: "chrome",
                tool: "bash",
                input: json!({"command": chrome_command(&chrome, &workdir, "chrome")}),
            }],
        );
        let result = run.result_text("chrome");
        let out = fs::read_to_string(workdir.join("chrome.out")).unwrap_or_default();
        let err = fs::read_to_string(workdir.join("chrome.err")).unwrap_or_default();
        assert!(
            result.lines().any(|line| line == "chrome-exit=0"),
            "{tier}: Chrome must exit 0\nresult: {result}\nstderr: {err}"
        );
        assert!(
            out.lines().any(|line| line == MARKER),
            "{tier}: the page's marker line must be printed\nstdout: {out}\nstderr: {err}"
        );
        assert!(
            !err.contains(GPU_EXITED),
            "{tier}: the GPU process must stay up\nstderr: {err}"
        );
    }
}
