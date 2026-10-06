//! Which remote source a `name@version` install falls back to when the local store misses.
//!
//! With no config file at all, every form of `mur install` resolves against the built-in
//! `official` source, `murmur-nexus/default-artifacts`, without writing a config file. A config
//! file with `registry.sources: []` names no source, so the same miss fails with `E-REG-001`
//! and opens no connection.
//!
//! The GitHub API is a loopback stub reached through `MUR_GITHUB_API_BASE`.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    io::{ErrorKind, Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use assert_cmd::Command;
use murmur_artifact::{read_lockfile, sha256_hex};
use predicates::prelude::*;
use tempfile::TempDir;

const NAME: &str = "default-source-tool";
const VERSION: &str = "0.3.0";
const ABSENT: &str = "absent-artifact@1.0.0";

fn wasm_zip(dir: &Path) -> Vec<u8> {
    let path = dir.join(format!("{NAME}-{VERSION}.mur.zip"));
    let mut zip = zip::ZipWriter::new(fs::File::create(&path).unwrap());
    let options: zip::write::SimpleFileOptions =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    zip.start_file("murmur.yaml", options).unwrap();
    writeln!(zip, "name: {NAME}\nversion: {VERSION}\nruntime: tool").unwrap();
    zip.start_file("tool.wasm", options).unwrap();
    zip.write_all(b"\0asm\x01\0\0\0").unwrap();
    zip.finish().unwrap();
    fs::read(path).unwrap()
}

/// `mur` under the scratch `home`, run from `cwd`, with no GitHub token in reach.
fn mur(home: &TempDir, cwd: &Path, api_base: &str) -> Command {
    let mut cmd = Command::cargo_bin("mur").unwrap();
    cmd.env("HOME", home.path())
        .env("MUR_GITHUB_API_BASE", api_base)
        .env_remove("NEXUS_API_KEY")
        .env_remove("GITHUB_TOKEN")
        .current_dir(cwd)
        .timeout(Duration::from_secs(60));
    cmd
}

fn write_project(dir: &Path, name: &str, version: &str) {
    fs::write(
        dir.join("murmur.yaml"),
        format!(
            "name: default-source-project\nversion: 0.0.1\nartifacts:\n  - name: {name}\n    \
             version: {version}\n    runtime: tool\n"
        ),
    )
    .unwrap();
}

/// Serves one `releases/latest` payload holding `asset`, and 404s every tag lookup so
/// resolution takes the latest-release path. Records every request path it is sent.
struct MockRelease {
    api_base: String,
    paths: Arc<Mutex<Vec<String>>>,
}

impl MockRelease {
    fn start(asset: Vec<u8>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let api_base = format!("http://{}", listener.local_addr().unwrap());
        let paths = Arc::new(Mutex::new(Vec::new()));
        let release_body = format!(
            "{{\"tag_name\":\"v{VERSION}\",\"assets\":[{{\"id\":1,\"name\":\"{NAME}-{VERSION}.mur.zip\"}}]}}"
        );

        let recorded = Arc::clone(&paths);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let Some(path) = read_request_path(&mut stream) else {
                    continue;
                };
                recorded.lock().unwrap().push(path.clone());
                let _ = if path.ends_with("/releases/latest") {
                    write_response(
                        &mut stream,
                        200,
                        "application/json",
                        release_body.as_bytes(),
                    )
                } else if path.ends_with("/releases/assets/1") {
                    write_response(&mut stream, 200, "application/octet-stream", &asset)
                } else {
                    write_response(&mut stream, 404, "text/plain", b"not found")
                };
            }
        });

        Self { api_base, paths }
    }

    fn paths(&self) -> Vec<String> {
        self.paths.lock().unwrap().clone()
    }
}

fn read_request_path(stream: &mut TcpStream) -> Option<String> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let request = String::from_utf8_lossy(&buffer);
    Some(
        request
            .lines()
            .next()?
            .split_whitespace()
            .nth(1)?
            .to_string(),
    )
}

fn write_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let reason = if status == 200 { "OK" } else { "Not Found" };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()
}

fn assert_asked_default_artifacts(release: &MockRelease) {
    let paths = release.paths();
    assert!(
        paths
            .iter()
            .any(|path| path.contains("/repos/murmur-nexus/default-artifacts/")),
        "the lookup went to the built-in source; requests: {paths:?}"
    );
}

fn assert_no_config_written(home: &TempDir) {
    assert!(
        !home.path().join(".murmur/config.yaml").exists(),
        "install writes no config file"
    );
}

/// A listener that is bound and never accepted. Returns it with its `MUR_GITHUB_API_BASE`.
fn silent_listener() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let api_base = format!("http://{}", listener.local_addr().unwrap());
    (listener, api_base)
}

/// Whether anything connected to `listener`: a connection the kernel completed sits in the
/// backlog until accepted.
fn was_connected(listener: &TcpListener) -> bool {
    listener.set_nonblocking(true).unwrap();
    match listener.accept() {
        Ok(_) => true,
        Err(err) if err.kind() == ErrorKind::WouldBlock => false,
        Err(err) => panic!("accept on the silent listener failed: {err}"),
    }
}

#[test]
fn a_global_install_with_no_config_file_uses_the_built_in_source() {
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let staging = tempfile::tempdir().unwrap();
    let release = MockRelease::start(wasm_zip(staging.path()));

    mur(&home, cwd.path(), &release.api_base)
        .args(["install", "-g", &format!("{NAME}@{VERSION}")])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "Installed {NAME}@{VERSION}"
        )));

    assert_asked_default_artifacts(&release);
    assert!(home
        .path()
        .join(format!(
            ".murmur/artifacts/{NAME}/{VERSION}/{NAME}-{VERSION}.mur.zip"
        ))
        .exists());
    assert_no_config_written(&home);
}

#[test]
fn a_manifest_install_with_no_config_file_uses_the_built_in_source() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let staging = tempfile::tempdir().unwrap();
    let bytes = wasm_zip(staging.path());
    let sha256 = sha256_hex(&bytes);
    let release = MockRelease::start(bytes);
    write_project(project.path(), NAME, VERSION);

    mur(&home, project.path(), &release.api_base)
        .arg("install")
        .assert()
        .success();

    assert_asked_default_artifacts(&release);
    assert!(project
        .path()
        .join(format!(
            ".murmur/artifacts/{NAME}/{VERSION}/{NAME}-{VERSION}.mur.zip"
        ))
        .exists());
    let lock = read_lockfile(&project.path().join("murmur.lock")).unwrap();
    let entry = lock.artifact_for(NAME).unwrap();
    assert_eq!(entry.resolved_version, VERSION);
    assert_eq!(entry.sha256.any.as_deref(), Some(sha256.as_str()));
    assert_no_config_written(&home);
}

#[test]
fn a_global_install_with_an_empty_source_list_fails_without_a_lookup() {
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    common::write_no_source_config(home.path());
    let (listener, api_base) = silent_listener();

    mur(&home, cwd.path(), &api_base)
        .args(["install", "-g", ABSENT])
        .assert()
        .failure()
        .stderr(predicate::str::contains("error[E-REG-001]"))
        .stderr(predicate::str::contains(format!(
            "artifact {ABSENT} not found in registry"
        )))
        .stderr(predicate::str::contains("E-REG-006").not());

    assert!(!was_connected(&listener), "no source was asked");
}

#[test]
fn a_manifest_install_with_an_empty_source_list_fails_without_a_lookup() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    common::write_no_source_config(home.path());
    let (name, version) = ABSENT.split_once('@').unwrap();
    write_project(project.path(), name, version);
    let (listener, api_base) = silent_listener();

    let assert = mur(&home, project.path(), &api_base)
        .arg("install")
        .assert()
        .failure();
    let output = assert.get_output();
    let printed = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(printed.contains("error[E-REG-001]"), "{printed}");
    assert!(
        printed.contains(&format!("artifact {ABSENT} not found in registry")),
        "{printed}"
    );
    assert!(!printed.contains("E-REG-006"), "{printed}");

    assert!(!was_connected(&listener), "no source was asked");
}
