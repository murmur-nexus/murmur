//! A tool's own `murmur.yaml` pins a download of a program it runs (`programs.<p>.download`), `mur
//! install` fetches it once into `~/.murmur/programs/<sha256>/`, and a capsule that pins the tool
//! with an `operator` lock origin and grants the program in `capabilities.shell.allow` runs that
//! copy by name — unless the host has a program of that name on `PATH`. `mur doctor`, `mur run
//! --explain-scope` and `session_start.effective_grants.pinned_programs` say which copy runs.
//!
//! Every download is a zip the test builds, served from a loopback HTTP server that counts the
//! requests per path. Everything runs through the real `mur` binary with `HOME` pointed at a
//! scratch directory under `CARGO_TARGET_TMPDIR` (never `/tmp`, which the `scoped` tier replaces
//! with a private one, hiding a store kept there).

#[path = "common/mod.rs"]
mod common;

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::{Arc, Mutex};

use assert_cmd::Command;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const DRIVER: &str = "local/murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.0";
const TOOL: &str = "local/pinned-tool";
const VERSION: &str = "0.1.0";
const PROGRAM: &str = "fixture-prog";
const RUN: &str = "fixture/fixture-prog";
const PINNED_SCRIPT: &[u8] = b"#!/bin/sh\necho pinned-copy-ran\n";

// ------------------------------------------------------------------------------- the server

/// Each path's status and body.
type Routes = Arc<Mutex<HashMap<String, (u16, Vec<u8>)>>>;

/// One refusal case: its label, the `programs:` block it installs, the lines it must print.
type RefusalCase = (&'static str, Box<dyn Fn(&Fixture) -> String>, Vec<String>);

/// A loopback HTTP server answering each path from a table, counting the GETs per path.
struct DownloadServer {
    base: String,
    routes: Routes,
    hits: Arc<Mutex<HashMap<String, usize>>>,
}

impl DownloadServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let routes: Routes = Arc::default();
        let hits: Arc<Mutex<HashMap<String, usize>>> = Arc::default();
        let (served_routes, served_hits) = (routes.clone(), hits.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).is_err() {
                    continue;
                }
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).unwrap_or(0) == 0 || header == "\r\n" {
                        break;
                    }
                }
                let path = request_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .to_string();
                *served_hits.lock().unwrap().entry(path.clone()).or_default() += 1;
                let (status, body) = served_routes
                    .lock()
                    .unwrap()
                    .get(&path)
                    .cloned()
                    .unwrap_or((404, b"not found".to_vec()));
                let head = format!(
                    "HTTP/1.1 {status} X\r\ncontent-length: {}\r\ncontent-type: \
                     application/octet-stream\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body);
            }
        });
        Self { base, routes, hits }
    }

    /// Serves `body` at `path`, returning its URL.
    fn serve(&self, path: &str, body: Vec<u8>) -> String {
        self.routes
            .lock()
            .unwrap()
            .insert(path.to_string(), (200, body));
        format!("{}{path}", self.base)
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn hits(&self, path: &str) -> usize {
        self.hits.lock().unwrap().get(path).copied().unwrap_or(0)
    }

    fn total_hits(&self) -> usize {
        self.hits.lock().unwrap().values().sum()
    }
}

// ------------------------------------------------------------------------------- fixtures

/// A zip of `entries`: (name, unix mode, contents).
fn zip_of(entries: &[(&str, u32, &[u8])]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, mode, contents) in entries {
        let options = zip::write::SimpleFileOptions::default().unix_permissions(*mode);
        writer.start_file(*name, options).unwrap();
        writer.write_all(contents).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

/// `fixture/fixture-prog` (0755, prints `pinned-copy-ran`) and `fixture/data.txt` (0644).
fn fixture_zip() -> Vec<u8> {
    zip_of(&[
        (RUN, 0o755, PINNED_SCRIPT),
        ("fixture/data.txt", 0o644, b"data\n"),
    ])
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn platform() -> &'static str {
    murmur_artifact::host_download_platform().expect("this host has a download platform")
}

/// The `programs:` block pinning `PROGRAM` for `platform` from `url` at `sha`, with `run`.
fn download_block(platform: &str, url: &str, sha: &str, run: Option<&str>) -> String {
    let run = run.map_or_else(String::new, |run| format!("        run: {run}\n"));
    format!(
        "programs:\n  {PROGRAM}:\n    download:\n      {platform}:\n        url: {url}\n        \
         sha256: {sha}\n{run}"
    )
}

struct Fixture {
    _root: TempDir,
    home: PathBuf,
    project: PathBuf,
    artifacts: PathBuf,
    server: DownloadServer,
}

impl Fixture {
    /// A scratch home and project with the fixture driver installed, a manifest declaring the
    /// driver and [`TOOL`] and allowlisting `bash`, `sh` and [`PROGRAM`].
    fn new() -> Self {
        let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let base = root.path().canonicalize().unwrap();
        let home = base.join("home");
        let project = base.join("project");
        let artifacts = base.join("artifacts");
        for dir in [&home, &project, &artifacts] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let fixture = Self {
            _root: root,
            home,
            project,
            artifacts,
            server: DownloadServer::start(),
        };
        fixture.write_manifest("http://127.0.0.1:9", "scoped", &["bash", "sh", PROGRAM]);
        let driver = common::create_driver_artifact(
            &fixture.artifacts,
            DRIVER,
            DRIVER_VERSION,
            &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
        );
        let installed = fixture.install(&driver);
        assert!(installed.status.success(), "{}", text(&installed));
        fixture
    }

    fn mur(&self) -> Command {
        let mut command = Command::cargo_bin("mur").unwrap();
        command.env("HOME", &self.home).current_dir(&self.project);
        command
    }

    fn write_manifest(&self, endpoint: &str, containment: &str, shell_allow: &[&str]) {
        let allow: String = shell_allow
            .iter()
            .map(|program| format!("      - {program}\n"))
            .collect();
        std::fs::write(
            self.project.join("murmur.yaml"),
            format!(
                "name: pinned-capsule\nversion: 0.0.1\nartifacts:\n  - name: {DRIVER}\n    \
                 version: {DRIVER_VERSION}\n    runtime: driver\n    gateway:\n      endpoint: \
                 {endpoint}\n      api_key: test-key\n  - name: {TOOL}\n    version: {VERSION}\n    \
                 runtime: tool\ncapabilities:\n  containment: {containment}\n  network:\n    \
                 allow:\n      - {endpoint}\n  shell:\n    allow:\n{allow}inference:\n  \
                 transport: http\n  model: test-model\n  driver:\n    artifact: {DRIVER}\n"
            ),
        )
        .unwrap();
    }

    /// [`TOOL`]'s `.mur.zip` carrying `block`, packing the committed `image-tool` component.
    fn tool(&self, block: &str) -> PathBuf {
        common::create_tool_artifact_with_auth(
            &self.artifacts,
            TOOL,
            VERSION,
            &common::fixture_path("image-tool/tool/image-tool.wasm"),
            block,
        )
    }

    /// Serves [`fixture_zip`] at `/fixture-prog.zip` and returns the tool pinning it for this
    /// host, with its sha256.
    fn pinned_tool(&self) -> (PathBuf, String) {
        let bytes = fixture_zip();
        let sha = sha256_hex(&bytes);
        let url = self.server.serve("/fixture-prog.zip", bytes);
        (
            self.tool(&download_block(platform(), &url, &sha, Some(RUN))),
            sha,
        )
    }

    /// `mur install <artifact>` into the project store.
    fn install(&self, artifact: &Path) -> Output {
        self.mur()
            .args(["install", artifact.to_str().unwrap(), "--no-precompile"])
            .output()
            .unwrap()
    }

    fn store(&self) -> PathBuf {
        self.home.join(".murmur").join("programs")
    }

    fn entry(&self, sha: &str) -> PathBuf {
        self.store().join(sha)
    }

    fn doctor(&self) -> String {
        text(&self.mur().arg("doctor").output().unwrap())
    }

    fn explain_json(&self, path: Option<&str>) -> Value {
        let mut command = self.mur();
        if let Some(path) = path {
            command.env("PATH", path);
        }
        let output = command
            .args([
                "run",
                "--manifest",
                "murmur.yaml",
                "--explain-scope",
                "--json",
            ])
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", text(&output));
        serde_json::from_str(String::from_utf8(output.stdout).unwrap().trim()).unwrap()
    }

    /// One `mur run` at `containment` whose model runs `command` in `bash`, returning the
    /// command's output lines and the session's `session_start`.
    fn run_command(
        &self,
        containment: &str,
        shell_allow: &[&str],
        command: &str,
        path: Option<&str>,
    ) -> (Vec<String>, Value) {
        let server = common::ScriptedServer::start(vec![
            common::tool_use_response("toolu_prog", "bash", json!({ "command": command })),
            end_turn(),
        ]);
        self.write_manifest(&server.endpoint, containment, shell_allow);
        let mut mur = self.mur();
        if let Some(path) = path {
            mur.env("PATH", path);
        }
        let output = mur
            .args(["run", "--manifest", "murmur.yaml"])
            .args(["--task", "Run the program once.", "--verbose"])
            .write_stdin("")
            .timeout(std::time::Duration::from_secs(300))
            .output()
            .unwrap();
        assert!(
            output.status.success() && text(&output).contains("status:  ok"),
            "the run must finish ok\n{}",
            text(&output)
        );
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let records = common::parse_records_from_stdout(&stdout);
        let trace = common::read_whole_trace(&records.join("trace.jsonl"));
        let session_start = trace
            .into_iter()
            .find(|event| event["event_type"] == "session_start")
            .expect("session_start");
        let result = common::tool_result_text(&server.requests(), "toolu_prog")
            .unwrap_or_else(|| panic!("no tool result\n{}", text(&output)));
        (result.lines().map(str::to_string).collect(), session_start)
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

/// stdout then stderr.
fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn mode_of(path: &Path) -> u32 {
    std::fs::symlink_metadata(path)
        .unwrap()
        .permissions()
        .mode()
        & 0o777
}

fn no_partials(store: &Path) -> bool {
    !store.exists()
        || std::fs::read_dir(store).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".partial-")
        })
}

/// The lines of the doctor section headed `header`, through its blank line.
fn section<'a>(doctor: &'a str, header: &str) -> Vec<&'a str> {
    doctor
        .lines()
        .skip_while(|line| *line != header)
        .take_while(|line| !line.is_empty())
        .collect()
}

/// The warning count of doctor's closing tally.
fn warning_count(doctor: &str) -> usize {
    doctor
        .lines()
        .find(|line| line.contains(" check") && line.contains(" found"))
        .unwrap_or_default()
        .split(", ")
        .find_map(|part| {
            part.strip_suffix(" warnings.")
                .or(part.strip_suffix(" warning."))
        })
        .and_then(|count| count.parse().ok())
        .unwrap_or(0)
}

/// The error count of doctor's closing tally; a tally naming no errors counts 0. The count is
/// the tally's last part when no warning follows it (`2 checks passed, 1 error found.`), so the
/// closing period is dropped before matching.
fn error_count(doctor: &str) -> usize {
    doctor
        .lines()
        .flat_map(|line| line.split(", "))
        .find_map(|part| {
            let part = part.strip_suffix('.').unwrap_or(part);
            part.strip_suffix(" errors found")
                .or(part.strip_suffix(" error found"))
        })
        .and_then(|count| count.trim().parse().ok())
        .unwrap_or(0)
}

// ------------------------------------------------------------------------------- install

/// `mur install` fetches the pinned download once, stores it with its record, modes and link,
/// leaves no partial folder; a second install and a manifest install find it present without a
/// request; a file changed since install makes the next install fetch it again.
#[test]
#[allow(clippy::print_stdout)]
fn install_fetches_once_and_checks() {
    let fixture = Fixture::new();
    let (tool, sha) = fixture.pinned_tool();
    let zip_len = fixture_zip().len();

    let first = fixture.install(&tool);
    let first_text = text(&first);
    println!("{first_text}");
    assert!(first.status.success(), "{first_text}");
    assert!(
        first_text.contains(&format!(
            "  fetched {PROGRAM} for {} ({TOOL}): {zip_len} B, sha256 verified",
            platform()
        )),
        "{first_text}"
    );
    assert_eq!(fixture.server.hits("/fixture-prog.zip"), 1);

    let entry = fixture.entry(&sha);
    let record: Value =
        serde_json::from_str(&std::fs::read_to_string(entry.join("record.json")).unwrap()).unwrap();
    println!("record.json: {record}");
    assert_eq!(record["url"], fixture.server.url("/fixture-prog.zip"));
    assert_eq!(record["sha256"], sha.as_str());
    assert_eq!(record["platform"], platform());
    assert_eq!(record["run"], RUN);
    assert_eq!(record["tree_sha256"].as_str().unwrap().len(), 64);
    assert!(record["fetched_at"].as_str().unwrap().ends_with('Z'));
    let listing = std::process::Command::new("ls")
        .args(["-lR"])
        .arg(&entry)
        .output()
        .unwrap();
    println!(
        "ls -lR {}:\n{}",
        entry.display(),
        String::from_utf8_lossy(&listing.stdout)
    );
    assert_eq!(mode_of(&entry), 0o700);
    assert_eq!(mode_of(&entry.join("files/fixture/fixture-prog")), 0o700);
    assert_eq!(mode_of(&entry.join("files/fixture/data.txt")), 0o600);
    assert_eq!(mode_of(&entry.join("record.json")), 0o600);
    assert_eq!(
        std::fs::read_link(entry.join("bin").join(PROGRAM)).unwrap(),
        PathBuf::from("../files/fixture/fixture-prog")
    );
    assert_eq!(
        std::fs::read(entry.join("bin").join(PROGRAM)).unwrap(),
        PINNED_SCRIPT
    );
    assert!(no_partials(&fixture.store()));

    // Present: no request, by reference and through the manifest install (cached artifact).
    let second = text(&fixture.install(&tool));
    println!("{second}");
    let present = format!(
        "  {PROGRAM} for {} ({TOOL}): already in the store",
        platform()
    );
    assert!(second.contains(&present), "{second}");
    let manifest_install = fixture
        .mur()
        .args(["install", "--no-precompile"])
        .output()
        .unwrap();
    let manifest_text = text(&manifest_install);
    println!("{manifest_text}");
    assert!(manifest_install.status.success(), "{manifest_text}");
    assert!(manifest_text.contains(&present), "{manifest_text}");
    assert_eq!(fixture.server.hits("/fixture-prog.zip"), 1);

    // A file changed since install: removed and fetched again.
    std::fs::write(entry.join("files/fixture/data.txt"), "changed\n").unwrap();
    let third = text(&fixture.install(&tool));
    println!("{third}");
    assert!(
        third.contains(&format!(
            "  {PROGRAM} for {} ({TOOL}): the stored copy failed its checksum; fetched again",
            platform()
        )),
        "{third}"
    );
    assert_eq!(fixture.server.hits("/fixture-prog.zip"), 2);
    assert_eq!(
        std::fs::read_to_string(entry.join("files/fixture/data.txt")).unwrap(),
        "data\n"
    );
    let fourth = text(&fixture.install(&tool));
    assert!(fourth.contains(&present), "{fourth}");
    assert_eq!(fixture.server.hits("/fixture-prog.zip"), 2);
    assert!(no_partials(&fixture.store()));

    // The home audit is quiet about the store.
    let doctor = fixture.doctor();
    assert!(
        !doctor
            .lines()
            .any(|line| line.contains("W-SEC-028") && line.contains(".murmur/programs")),
        "{doctor}"
    );
}

/// A wrong checksum (`E-REG-014`), a failed fetch (`E-REG-013`) and a bad archive (`E-REG-015`)
/// fail the install with exit 1, store nothing under `~/.murmur/programs`, and leave the artifact
/// itself stored and pinned.
#[test]
#[allow(clippy::print_stdout)]
fn install_refuses_a_wrong_checksum_a_failed_fetch_and_a_bad_archive() {
    let platform = platform();
    let wrong_sha = "ab".repeat(32);
    let cases: Vec<RefusalCase> = vec![
        (
            "wrong checksum",
            Box::new(|fixture: &Fixture| {
                let url = fixture.server.serve("/fixture-prog.zip", fixture_zip());
                download_block(platform, &url, &"ab".repeat(32), Some(RUN))
            }),
            vec![
                "error[E-REG-014]:".to_string(),
                format!(
                    "the pinned download of {PROGRAM} for {platform} ({TOOL}@{VERSION}) from \
                     URL has sha256 {}, and the artifact pins {wrong_sha}; nothing was stored",
                    sha256_hex(&fixture_zip())
                ),
            ],
        ),
        (
            "404",
            Box::new(|fixture: &Fixture| {
                download_block(
                    platform,
                    &fixture.server.url("/absent.zip"),
                    &sha256_hex(b"x"),
                    Some(RUN),
                )
            }),
            vec![
                "error[E-REG-013]:".to_string(),
                format!(
                    "could not fetch the pinned download of {PROGRAM} for {platform} \
                     ({TOOL}@{VERSION}) from URL: HTTP 404"
                ),
            ],
        ),
        (
            "missing run",
            Box::new(|fixture: &Fixture| {
                let bytes = fixture_zip();
                let sha = sha256_hex(&bytes);
                let url = fixture.server.serve("/fixture-prog.zip", bytes);
                download_block(platform, &url, &sha, Some("fixture/absent"))
            }),
            vec![
                "error[E-REG-015]:".to_string(),
                format!(
                    "the pinned download of {PROGRAM} for {platform} ({TOOL}@{VERSION}) cannot \
                     be unpacked: run names fixture/absent, and the zip holds no such file"
                ),
            ],
        ),
        (
            "escaping entry",
            Box::new(|fixture: &Fixture| {
                let bytes = zip_of(&[("../escape", 0o644, b"x"), (RUN, 0o755, PINNED_SCRIPT)]);
                let sha = sha256_hex(&bytes);
                let url = fixture.server.serve("/escape.zip", bytes);
                download_block(platform, &url, &sha, Some(RUN))
            }),
            vec![
                "error[E-REG-015]:".to_string(),
                format!(
                    "the pinned download of {PROGRAM} for {platform} ({TOOL}@{VERSION}) cannot \
                     be unpacked: zip entry '../escape' escapes the archive's folder"
                ),
            ],
        ),
    ];
    for (label, block, expected) in cases {
        let fixture = Fixture::new();
        let block = block(&fixture);
        let url = block
            .lines()
            .find_map(|line| line.trim().strip_prefix("url: "))
            .unwrap()
            .to_string();
        let declared_sha = block
            .lines()
            .find_map(|line| line.trim().strip_prefix("sha256: "))
            .unwrap()
            .to_string();
        let installed = fixture.install(&fixture.tool(&block));
        let output = text(&installed);
        println!("── {label}\n{output}");
        assert_eq!(installed.status.code(), Some(1), "{label}: {output}");
        for line in &expected {
            let line = line.replace("URL", &url);
            assert!(output.contains(&line), "{label}: expected {line}\n{output}");
        }
        assert!(
            output.contains("https://docs.murmur.nexus/reference/diagnostics/#e-reg-01"),
            "{output}"
        );
        assert!(!fixture.entry(&declared_sha).exists(), "{label}");
        assert!(no_partials(&fixture.store()), "{label}");
        // The artifact itself is stored and pinned.
        let lock = std::fs::read_to_string(fixture.project.join("murmur.lock")).unwrap();
        assert!(lock.contains(TOOL), "{label}: {lock}");
        assert!(fixture
            .project
            .join(".murmur/artifacts/local/pinned-tool")
            .exists());
    }
}

/// A tool pinning a download only for another platform installs with one line, no request and
/// no store entry.
#[test]
#[allow(clippy::print_stdout)]
fn install_without_this_platform_is_not_an_error() {
    let fixture = Fixture::new();
    let other = murmur_artifact::PINNED_DOWNLOAD_PLATFORMS
        .into_iter()
        .find(|candidate| *candidate != platform())
        .unwrap();
    let bytes = fixture_zip();
    let sha = sha256_hex(&bytes);
    let url = fixture.server.serve("/fixture-prog.zip", bytes);
    let installed = fixture.install(&fixture.tool(&download_block(other, &url, &sha, Some(RUN))));
    let output = text(&installed);
    println!("{output}");
    assert!(installed.status.success(), "{output}");
    assert!(
        output.contains(&format!(
            "  {PROGRAM} ({TOOL}): no pinned download for {}; a host program of that name is \
             used if the capsule grants it",
            platform()
        )),
        "{output}"
    );
    assert_eq!(fixture.server.total_hits(), 0);
    assert!(!fixture.entry(&sha).exists());
}

// ------------------------------------------------------------------------------- the session

/// The capsule names the program in `shell.allow` and nothing else: no `PATH` entry, no host path.
/// Its command runs the fetched copy, at `scoped` and, where the host reaches it, `sealed`, and
/// `session_start` records the choice.
#[test]
#[allow(clippy::print_stdout)]
fn a_capsule_granting_the_program_runs_the_fetched_copy() {
    let test = "a_capsule_granting_the_program_runs_the_fetched_copy";
    if common::skip_without_host_support(test)
        || capsule_runtime::skip_without_egress_namespace(test)
    {
        return;
    }
    let fixture = Fixture::new();
    let (tool, sha) = fixture.pinned_tool();
    assert!(fixture.install(&tool).status.success());
    let tree = fixture.entry(&sha).canonicalize().unwrap();
    for containment in common::tiers(test) {
        let (lines, session_start) =
            fixture.run_command(containment, &["bash", "sh", PROGRAM], PROGRAM, None);
        let pinned = &session_start["effective_grants"]["pinned_programs"];
        println!(
            "── {containment}\n{}\npinned_programs: {pinned}",
            lines.join("\n")
        );
        assert!(
            lines.iter().any(|line| line == "pinned-copy-ran"),
            "{containment}: {lines:?}"
        );
        assert_eq!(
            pinned,
            &json!([{
                "artifact": TOOL,
                "program": PROGRAM,
                "platform": platform(),
                "source": "pinned",
                "path": tree.join("files/fixture/fixture-prog").display().to_string(),
                "url": fixture.server.url("/fixture-prog.zip"),
                "sha256": sha,
            }]),
            "{containment}"
        );
    }
}

/// The copy reaches a session only through both declarations: the program in `shell.allow` and
/// an operator pin. A program missing from the store is reported and the launch goes on.
#[test]
#[allow(clippy::print_stdout)]
fn pinned_programs_reach_a_session_only_through_its_own_declarations() {
    let test = "pinned_programs_reach_a_session_only_through_its_own_declarations";
    if common::skip_without_host_support(test)
        || capsule_runtime::skip_without_egress_namespace(test)
    {
        return;
    }
    let fixture = Fixture::new();
    let (tool, sha) = fixture.pinned_tool();
    assert!(fixture.install(&tool).status.success());
    let probe = format!("{PROGRAM}; echo exit=$?");

    // (a) Not granted: nothing recorded, and the name is not found.
    let (lines, session_start) = fixture.run_command("scoped", &["bash", "sh"], &probe, None);
    println!("(a) {lines:?}");
    assert_eq!(
        session_start["effective_grants"]["pinned_programs"],
        json!([])
    );
    assert!(lines.iter().any(|line| line == "exit=127"), "{lines:?}");
    assert!(!lines.iter().any(|line| line == "pinned-copy-ran"));

    // (b) A runtime pin of the tool: nothing.
    let lock_path = fixture.project.join("murmur.lock");
    let operator_lock = std::fs::read_to_string(&lock_path).unwrap();
    let mut lock = murmur_artifact::read_lockfile(&lock_path).unwrap();
    for entry in &mut lock.artifacts {
        if entry.name == TOOL {
            entry.origin = murmur_artifact::LockOrigin::Runtime {
                session: "ses_x".to_string(),
            };
        }
    }
    murmur_artifact::write_lockfile_atomic(&lock_path, &lock).unwrap();
    fixture.write_manifest("http://127.0.0.1:9", "scoped", &["bash", "sh", PROGRAM]);
    let runtime = fixture.explain_json(None);
    println!("(b) pinned_programs: {}", runtime["pinned_programs"]);
    assert_eq!(runtime["pinned_programs"], json!([]));
    std::fs::write(&lock_path, operator_lock).unwrap();

    // (c) The program key spelled differently from the shell.allow entry: nothing.
    fixture.write_manifest(
        "http://127.0.0.1:9",
        "scoped",
        &["bash", "sh", "Fixture-prog"],
    );
    let spelled = fixture.explain_json(None);
    println!("(c) pinned_programs: {}", spelled["pinned_programs"]);
    assert_eq!(spelled["pinned_programs"], json!([]));

    // (d) The store has no entry for the declared sha256: missing, and the launch is not refused.
    std::fs::rename(fixture.entry(&sha), fixture.store().join("moved-away")).unwrap();
    let (lines, session_start) =
        fixture.run_command("scoped", &["bash", "sh", PROGRAM], &probe, None);
    let pinned = &session_start["effective_grants"]["pinned_programs"];
    println!("(d) {lines:?}\npinned_programs: {pinned}");
    assert_eq!(
        pinned,
        &json!([{
            "artifact": TOOL,
            "program": PROGRAM,
            "platform": platform(),
            "source": "missing",
            "reason": format!("the store holds no copy for sha256 {sha}; run mur install"),
        }])
    );
    assert!(lines.iter().any(|line| line == "exit=127"), "{lines:?}");
}

/// A host program of the same name on `PATH` runs instead of the store's copy, the report says
/// `host`, the store entry gets no grant, and doctor names the host copy without a warning.
#[test]
#[allow(clippy::print_stdout)]
fn a_host_program_of_that_name_is_used_and_doctor_says_so() {
    let test = "a_host_program_of_that_name_is_used_and_doctor_says_so";
    if common::skip_without_host_support(test)
        || capsule_runtime::skip_without_egress_namespace(test)
    {
        return;
    }
    let fixture = Fixture::new();
    let (tool, sha) = fixture.pinned_tool();
    assert!(fixture.install(&tool).status.success());
    let host_bin = fixture.home.parent().unwrap().join("host-bin");
    std::fs::create_dir_all(&host_bin).unwrap();
    let host_program = host_bin.join(PROGRAM);
    std::fs::write(&host_program, "#!/bin/sh\necho host-copy-ran\n").unwrap();
    std::fs::set_permissions(&host_program, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        host_bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let (lines, session_start) =
        fixture.run_command("scoped", &["bash", "sh", PROGRAM], PROGRAM, Some(&path));
    let pinned = &session_start["effective_grants"]["pinned_programs"];
    println!("{lines:?}\npinned_programs: {pinned}");
    assert!(
        lines.iter().any(|line| line == "host-copy-ran"),
        "{lines:?}"
    );
    assert!(!lines.iter().any(|line| line == "pinned-copy-ran"));
    assert_eq!(pinned[0]["source"], "host");
    assert_eq!(
        pinned[0]["path"],
        host_program.canonicalize().unwrap().display().to_string()
    );
    assert!(pinned[0].get("url").is_none() && pinned[0].get("sha256").is_none());

    let explain = fixture.explain_json(Some(&path));
    let entry = fixture.entry(&sha).canonicalize().unwrap();
    let grants = explain.to_string();
    assert!(
        !grants.contains(&entry.display().to_string()),
        "no grant on the store entry: {explain}"
    );

    let doctor = text(
        &fixture
            .mur()
            .env("PATH", &path)
            .arg("doctor")
            .output()
            .unwrap(),
    );
    let lines = section(&doctor, "Pinned programs:");
    println!("{}", lines.join("\n"));
    assert_eq!(
        lines,
        [
            "Pinned programs:".to_string(),
            format!(
                "  \u{2713}  {TOOL} ({PROGRAM}): pinned copy present; runs {} (host)",
                host_program.canonicalize().unwrap().display()
            ),
        ]
    );
    assert!(!doctor.contains("W-RUN-012"), "{doctor}");
}

// ------------------------------------------------------------------------------- reporting

/// Doctor's `Pinned programs` section in its three states, each failing one adding one warning
/// and no error; a tool pinning nothing prints no section.
#[test]
#[allow(clippy::print_stdout)]
fn doctor_shows_present_missing_and_failing() {
    let fixture = Fixture::new();
    fixture.write_manifest("http://127.0.0.1:9", "scoped", &[PROGRAM]);
    let (tool, sha) = fixture.pinned_tool();
    assert!(fixture.install(&tool).status.success());
    let entry = fixture.entry(&sha);
    let pinned_path = entry
        .canonicalize()
        .unwrap()
        .join("files/fixture/fixture-prog");

    let present = fixture.doctor();
    let lines = section(&present, "Pinned programs:");
    println!("── present\n{}", lines.join("\n"));
    assert_eq!(
        lines,
        [
            "Pinned programs:".to_string(),
            format!(
                "  \u{2713}  {TOOL} ({PROGRAM}): pinned copy present; runs {} (pinned)",
                pinned_path.display()
            ),
        ]
    );
    assert!(!present.contains("W-RUN-012"), "{present}");
    let base_warnings = warning_count(&present);
    let base_errors = error_count(&present);

    // Missing.
    let aside = fixture.store().join("aside");
    std::fs::rename(&entry, &aside).unwrap();
    let missing = fixture.doctor();
    let lines = section(&missing, "Pinned programs:");
    println!(
        "── missing\n{}\n{}",
        lines.join("\n"),
        error_count(&missing)
    );
    assert_eq!(
        lines,
        [
            "Pinned programs:".to_string(),
            format!("  \u{26A0}  {TOOL} ({PROGRAM}): pinned copy missing; nothing runs"),
            format!(
                "  warning[W-RUN-012]: {TOOL} pins {PROGRAM} for {}, and the store holds no copy; \
                 run mur install",
                platform()
            ),
        ]
    );
    assert!(
        missing.contains(&format!(
            "Fix: run mur install in this project to fetch {PROGRAM} for {} ({TOOL})",
            platform()
        )),
        "{missing}"
    );
    assert_eq!(warning_count(&missing), base_warnings + 1, "{missing}");
    assert_eq!(error_count(&missing), base_errors);

    // Failing its checksum.
    std::fs::rename(&aside, &entry).unwrap();
    std::fs::write(entry.join("files/fixture/data.txt"), "changed\n").unwrap();
    let failing = fixture.doctor();
    let lines = section(&failing, "Pinned programs:");
    println!("── failing\n{}", lines.join("\n"));
    assert_eq!(
        lines,
        [
            "Pinned programs:".to_string(),
            format!(
                "  \u{26A0}  {TOOL} ({PROGRAM}): pinned copy failing its checksum; nothing runs"
            ),
            format!(
                "  warning[W-RUN-012]: {TOOL}'s pinned copy of {PROGRAM} fails its checksum: \
                 files/fixture/data.txt changed since install; run mur install to fetch it again"
            ),
        ]
    );
    assert!(
        failing.contains("Fix: run mur install to fetch"),
        "{failing}"
    );
    assert_eq!(warning_count(&failing), base_warnings + 1, "{failing}");
    assert_eq!(error_count(&failing), base_errors);

    // A tool pinning nothing: no section.
    let bare = Fixture::new();
    bare.write_manifest("http://127.0.0.1:9", "scoped", &[PROGRAM]);
    assert!(bare.install(&bare.tool("")).status.success());
    let doctor = bare.doctor();
    assert!(!doctor.contains("Pinned programs:"), "{doctor}");
}

/// `mur run --explain-scope` prints the `pinned programs` block after `declared needs`, and its
/// JSON is the value a launch records.
#[test]
#[allow(clippy::print_stdout)]
fn explain_scope_reports_pinned_programs() {
    let test = "explain_scope_reports_pinned_programs";
    if common::skip_without_host_support(test)
        || capsule_runtime::skip_without_egress_namespace(test)
    {
        return;
    }
    let fixture = Fixture::new();
    let (tool, sha) = fixture.pinned_tool();
    assert!(fixture.install(&tool).status.success());
    let rendered = text(
        &fixture
            .mur()
            .args(["run", "--manifest", "murmur.yaml", "--explain-scope"])
            .output()
            .unwrap(),
    );
    let block: Vec<&str> = rendered
        .lines()
        .skip_while(|line| line.trim_start() != "pinned programs:")
        .take_while(|line| {
            let line = line.trim_start();
            line == "pinned programs:" || line.starts_with("- ")
        })
        .collect();
    println!("{}", block.join("\n"));
    let path = fixture
        .entry(&sha)
        .canonicalize()
        .unwrap()
        .join("files/fixture/fixture-prog");
    assert_eq!(
        block,
        [
            "  pinned programs:".to_string(),
            format!(
                "    - {TOOL} ({PROGRAM}): pinned {} (sha256 {sha}, from {})",
                path.display(),
                fixture.server.url("/fixture-prog.zip")
            ),
        ]
    );
    assert!(
        rendered.find("  declared needs:").unwrap() < rendered.find("  pinned programs:").unwrap()
    );

    let report = fixture.explain_json(None);
    println!("pinned_programs: {}", report["pinned_programs"]);
    let (_, session_start) = fixture.run_command("scoped", &["bash", "sh", PROGRAM], "true", None);
    assert_eq!(
        session_start["effective_grants"]["pinned_programs"],
        report["pinned_programs"]
    );
    assert_eq!(report["pinned_programs"][0]["source"], "pinned");
}
