//! An installed capsule reads the files its manifest names by relative path from its own package,
//! never from the workdir, the current directory or the store root.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    path::{Path, PathBuf},
};

use assert_cmd::Command;
use serde_json::{json, Value};
use tempfile::TempDir;

use common::ScriptedServer;

const DRIVER_NAME: &str = "local/murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";
const VERSION: &str = "0.1.0";
const PACKAGE_TEXT: &str = "PACKAGE-MARKER-5d1e: answer as the installed package says.\n";
const DECOY_TEXT: &str = "DECOY-MARKER-a07c: this file sits in the workdir.\n";

/// A capsule source folder built with `mur build` and published, with its driver, into a scratch
/// `HOME`'s global store, so `mur run --capsule` needs nothing in the workdir.
struct Installed {
    home: TempDir,
    _staging: TempDir,
    source: TempDir,
    server: ScriptedServer,
    name: &'static str,
}

impl Installed {
    /// `system_prompt_file` names `prompt_file`; `requires_files: [instructions.md]` is declared
    /// only when `packed`, so without it the package lacks the file.
    fn new(name: &'static str, responses: Vec<String>, prompt_file: &str, packed: bool) -> Self {
        let home = TempDir::new().unwrap();
        let staging = TempDir::new().unwrap();
        let source = TempDir::new().unwrap();
        let server = ScriptedServer::start(responses);
        let endpoint = &server.endpoint;
        let requires = if packed {
            "requires_files: [instructions.md]\n"
        } else {
            ""
        };
        fs::write(
            source.path().join("murmur.yaml"),
            format!(
                "name: {name}\nversion: {VERSION}\nruntime: capsule\nexecution: static\n\
                 {requires}artifacts:\n  - name: {DRIVER_NAME}\n    version: {DRIVER_VERSION}\n    \
                 runtime: driver\n    gateway:\n      endpoint: {endpoint}\n      api_key: test-key\n\
                 capabilities:\n  network:\n    allow:\n      - {endpoint}\n\
                 inference:\n  transport: http\n  model: test-model\n  \
                 system_prompt_file: {prompt_file}\n  driver:\n    artifact: {DRIVER_NAME}\n"
            ),
        )
        .unwrap();
        fs::write(source.path().join("instructions.md"), PACKAGE_TEXT).unwrap();

        let driver = common::create_driver_artifact(
            staging.path(),
            DRIVER_NAME,
            DRIVER_VERSION,
            &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
        );
        common::publish_local(&home, &driver).success();
        let package = staging.path().join(format!("{name}-{VERSION}.mur.zip"));
        mur(home.path())
            .args([
                "build",
                source.path().to_str().unwrap(),
                "--output",
                package.to_str().unwrap(),
            ])
            .assert()
            .success();
        common::publish_local(&home, &package).success();
        Self {
            home,
            _staging: staging,
            source,
            server,
            name,
        }
    }

    fn artifact(&self) -> String {
        format!("local/{}", self.name)
    }

    /// `mur run --capsule local/<name> --capsule-version 0.1.0 --workdir <workdir>`, then `args`.
    fn run(&self, workdir: &Path, args: &[&str]) -> assert_cmd::assert::Assert {
        mur(self.home.path())
            .args([
                "run",
                "--capsule",
                &self.artifact(),
                "--capsule-version",
                VERSION,
                "--workdir",
                workdir.to_str().unwrap(),
                "--no-env-file",
            ])
            .args(args)
            .assert()
    }

    /// A one-task launch that succeeds; its records directory.
    fn launch(&self, workdir: &Path, args: &[&str]) -> PathBuf {
        let mut all = vec!["--task", "Say hello.", "--json"];
        all.extend_from_slice(args);
        let assert = self.run(workdir, &all).success();
        records_dir(&String::from_utf8_lossy(&assert.get_output().stdout))
    }

    /// `--explain-scope --json` for the installed capsule.
    fn explain_scope_json(&self, workdir: &Path, args: &[&str]) -> Value {
        let mut all = vec!["--explain-scope", "--json"];
        all.extend_from_slice(args);
        let assert = self.run(workdir, &all).success();
        serde_json::from_slice(&assert.get_output().stdout).unwrap()
    }

    fn first_system(&self) -> String {
        let requests = self.server.requests();
        assert!(!requests.is_empty(), "the provider received no request");
        common::system_text(&requests[0]["system"]).expect("a system field")
    }
}

fn mur(home: &Path) -> Command {
    let mut command = Command::cargo_bin("mur").unwrap();
    command.env("HOME", home);
    command
}

fn one_turn() -> String {
    json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": "done"}],
        "stop_reason": "end_turn",
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

fn sha256_hex(text: &str) -> String {
    murmur_artifact::sha256_hex(text.as_bytes())
}

/// The `records_dir` of the `--json` launch line, the first JSON object on stdout.
fn records_dir(stdout: &str) -> PathBuf {
    let line = stdout
        .lines()
        .find_map(|line| serde_json::from_str::<Value>(line).ok())
        .unwrap_or_else(|| panic!("no JSON launch line in stdout:\n{stdout}"));
    PathBuf::from(line["records_dir"].as_str().expect("records_dir"))
}

/// The one `session_start` record in `records`.
fn session_start(records: &Path) -> Value {
    let trace = fs::read_to_string(records.join("trace.jsonl")).unwrap();
    let mut starts = trace
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| event["event_type"] == "session_start");
    let start = starts.next().expect("a session_start record");
    assert!(starts.next().is_none(), "one session_start record");
    start
}

/// Every file under `dir`, relative to it.
fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(
                files_under(&path)
                    .into_iter()
                    .map(|child| PathBuf::from(entry.file_name()).join(child)),
            );
        } else {
            found.push(PathBuf::from(entry.file_name()));
        }
    }
    found
}

#[test]
fn an_installed_capsule_reads_its_system_prompt_file_from_its_package() {
    if common::skip_without_host_support(
        "an_installed_capsule_reads_its_system_prompt_file_from_its_package",
    ) {
        return;
    }
    let installed = Installed::new("imf-happy", vec![one_turn()], "./instructions.md", true);
    let workdir = TempDir::new().unwrap();

    let records = installed.launch(workdir.path(), &[]);

    let system = installed.first_system();
    assert!(system.contains("PACKAGE-MARKER-5d1e"), "{system}");
    let start = session_start(&records);
    assert_eq!(start["system_prompt_source"], "manifest");
    assert_eq!(start["system_prompt_sha256"], sha256_hex(PACKAGE_TEXT));
    assert!(!workdir.path().join("instructions.md").exists());

    // The launch records what `--explain-scope --json` reports for the same capsule.
    let empty = TempDir::new().unwrap();
    let explained = installed.explain_scope_json(empty.path(), &[]);
    assert_eq!(
        start["effective_grants"]["manifest_files"],
        explained["manifest_files"]
    );
    assert_eq!(
        start["effective_grants"]["manifest_files"],
        json!([{
            "key": "inference.system_prompt_file",
            "declared": "./instructions.md",
            "resolved": "local/imf-happy@0.1.0!/instructions.md",
            "base": "installed-package",
        }])
    );
}

#[test]
fn the_same_capsule_from_its_source_folder_sends_the_same_prompt() {
    if common::skip_without_host_support(
        "the_same_capsule_from_its_source_folder_sends_the_same_prompt",
    ) {
        return;
    }
    let installed = Installed::new("imf-control", vec![one_turn()], "./instructions.md", true);
    let workdir = TempDir::new().unwrap();

    let assert = mur(installed.home.path())
        .args([
            "run",
            "--manifest",
            installed
                .source
                .path()
                .join("murmur.yaml")
                .to_str()
                .unwrap(),
            "--workdir",
            workdir.path().to_str().unwrap(),
            "--no-env-file",
            "--task",
            "Say hello.",
            "--json",
        ])
        .assert()
        .success();
    let records = records_dir(&String::from_utf8_lossy(&assert.get_output().stdout));

    assert!(installed.first_system().contains("PACKAGE-MARKER-5d1e"));
    let start = session_start(&records);
    assert_eq!(start["system_prompt_source"], "manifest");
    assert_eq!(start["system_prompt_sha256"], sha256_hex(PACKAGE_TEXT));
}

#[test]
fn an_installed_capsule_ignores_an_instructions_file_in_its_workdir() {
    if common::skip_without_host_support(
        "an_installed_capsule_ignores_an_instructions_file_in_its_workdir",
    ) {
        return;
    }
    let installed = Installed::new("imf-decoy", vec![one_turn()], "./instructions.md", true);
    let workdir = TempDir::new().unwrap();
    fs::write(workdir.path().join("instructions.md"), DECOY_TEXT).unwrap();

    let records = installed.launch(workdir.path(), &[]);

    let system = installed.first_system();
    assert!(system.contains("PACKAGE-MARKER-5d1e"), "{system}");
    assert!(!system.contains("DECOY-MARKER-a07c"), "{system}");
    assert_eq!(
        session_start(&records)["system_prompt_sha256"],
        sha256_hex(PACKAGE_TEXT)
    );
}

#[test]
fn a_package_without_its_prompt_file_fails_naming_the_package_path() {
    let installed = Installed::new("imf-unpacked", Vec::new(), "./instructions.md", false);
    let workdir = TempDir::new().unwrap();
    fs::write(workdir.path().join("instructions.md"), DECOY_TEXT).unwrap();

    let assert = installed
        .run(workdir.path(), &["--task", "Say hello."])
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    println!("── stderr\n{stderr}");

    assert!(stderr.contains("E-RUN-009"), "{stderr}");
    assert!(
        stderr.contains("local/imf-unpacked@0.1.0!/instructions.md"),
        "{stderr}"
    );
    assert!(stderr.contains("requires_files"), "{stderr}");
    assert!(
        !stderr.contains(workdir.path().to_str().unwrap()),
        "{stderr}"
    );
    assert!(installed.server.requests().is_empty());
    assert_eq!(
        files_under(workdir.path()),
        [PathBuf::from("instructions.md")],
        "nothing is created in the workdir"
    );
    let records = installed.home.path().join(".murmur").join("records");
    assert!(
        !files_under(&records)
            .iter()
            .any(|file| file.ends_with("trace.jsonl")),
        "no session records: {:?}",
        files_under(&records)
    );
}

#[test]
fn a_package_path_that_climbs_out_is_refused() {
    let installed = Installed::new("imf-climb", Vec::new(), "../outside.md", true);
    let parent = TempDir::new().unwrap();
    let workdir = parent.path().join("work");
    fs::create_dir(&workdir).unwrap();
    fs::write(parent.path().join("outside.md"), DECOY_TEXT).unwrap();

    let assert = installed.run(&workdir, &["--task", "Say hello."]).failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    println!("── stderr\n{stderr}");

    assert!(stderr.contains("E-RUN-009"), "{stderr}");
    assert!(
        stderr.contains("local/imf-climb@0.1.0!/../outside.md"),
        "{stderr}"
    );
    assert!(stderr.contains("outside the installed package"), "{stderr}");
    assert!(installed.server.requests().is_empty());
}

#[test]
fn explain_scope_shows_where_each_manifest_file_resolves() {
    let installed = Installed::new("imf-explain", Vec::new(), "./instructions.md", true);
    let workdir = TempDir::new().unwrap();

    let assert = installed
        .run(workdir.path(), &["--explain-scope"])
        .success();
    let human = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    println!("── --explain-scope\n{human}");
    assert!(human.contains("\nManifest files\n"), "{human}");
    assert!(
        human.contains(
            "\n  inference.system_prompt_file: ./instructions.md -> \
             local/imf-explain@0.1.0!/instructions.md\n"
        ),
        "{human}"
    );

    let explained = installed.explain_scope_json(workdir.path(), &[]);
    assert_eq!(
        explained["manifest_files"],
        json!([{
            "key": "inference.system_prompt_file",
            "declared": "./instructions.md",
            "resolved": "local/imf-explain@0.1.0!/instructions.md",
            "base": "installed-package",
        }])
    );

    let manifest = installed.source.path().join("murmur.yaml");
    let from_source = common::explain_scope_json(&installed.home, &manifest);
    assert_eq!(
        from_source["manifest_files"],
        json!([{
            "key": "inference.system_prompt_file",
            "declared": "./instructions.md",
            "resolved": installed
                .source
                .path()
                .join("./instructions.md")
                .display()
                .to_string(),
            "base": "manifest-folder",
        }])
    );

    assert!(files_under(workdir.path()).is_empty(), "nothing is staged");
    assert!(installed.server.requests().is_empty());
}

#[test]
fn system_prompt_override_drops_the_file_row() {
    if common::skip_without_host_support("system_prompt_override_drops_the_file_row") {
        return;
    }
    let installed = Installed::new("imf-override", vec![one_turn()], "./instructions.md", true);
    let workdir = TempDir::new().unwrap();

    let explained =
        installed.explain_scope_json(workdir.path(), &["--system-prompt", "CLI prompt"]);
    assert_eq!(explained["manifest_files"], json!([]));

    let records = installed.launch(workdir.path(), &["--system-prompt", "CLI prompt"]);
    let start = session_start(&records);
    assert_eq!(start["effective_grants"]["manifest_files"], json!([]));
    assert_eq!(start["system_prompt_source"], "cli");
    let system = installed.first_system();
    assert!(system.contains("CLI prompt"), "{system}");
    assert!(!system.contains("PACKAGE-MARKER-5d1e"), "{system}");
}
