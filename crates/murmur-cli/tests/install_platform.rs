//! What an install records about the platform a payload is for, end to end.
//!
//! The source-chain half runs against a mock GitHub API (`MUR_GITHUB_API_BASE`, the same
//! mechanism the unit tests in `src/source/mod.rs` use); the store half calls `mur publish` /
//! `mur install` against a temporary `HOME`. Both halves run on one host: a foreign platform is
//! whichever member of `SUPPORTED_PLATFORMS` this host is not, never a second machine.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    thread,
};

use assert_cmd::Command;
use murmur_artifact::{
    current_platform, read_lockfile, sha256_hex, write_lockfile_atomic, LocalRegistry,
    LockedArtifact, LockedSha256, MurmurLock, PlatformMatch, Registry, RuntimeType, LOCK_VERSION,
    SUPPORTED_PLATFORMS,
};
use predicates::prelude::*;
use tempfile::TempDir;

const NAME: &str = "nativetool";
const VERSION: &str = "0.1.0";

/// A `SUPPORTED_PLATFORMS` member that is not this host's, so a test can assert on a platform
/// nothing here can resolve without knowing which host it runs on.
fn foreign_platform() -> &'static str {
    SUPPORTED_PLATFORMS
        .iter()
        .copied()
        .find(|platform| *platform != current_platform())
        .expect("at least one supported platform is not this host's")
}

fn native_zip(dir: &Path, file_name: &str, filler: &str) -> PathBuf {
    let built = common::create_native_artifact(
        dir,
        NAME,
        VERSION,
        &format!("#!/bin/sh\necho {filler}\n"),
        None,
        None,
    );
    let target = dir.join(file_name);
    if built != target {
        fs::rename(&built, &target).unwrap();
    }
    target
}

fn wasm_zip(dir: &Path, name: &str, version: &str) -> PathBuf {
    let path = dir.join(format!("{name}-{version}.mur.zip"));
    let mut zip = zip::ZipWriter::new(fs::File::create(&path).unwrap());
    let options: zip::write::SimpleFileOptions =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    zip.start_file("murmur.yaml", options).unwrap();
    writeln!(zip, "name: {name}\nversion: {version}\nruntime: tool").unwrap();
    zip.start_file("tool.wasm", options).unwrap();
    zip.write_all(b"\0asm\x01\0\0\0").unwrap();
    zip.finish().unwrap();
    path
}

fn write_registry_source_config(home: &Path) {
    let config_dir = home.join(".murmur");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.yaml"),
        "registry:\n  default: official\n  sources:\n    - name: official\n      type: github\n      repo: acme/artifacts\n",
    )
    .unwrap();
}

fn mur(home: &TempDir) -> Command {
    let mut cmd = Command::cargo_bin("mur").unwrap();
    cmd.env("HOME", home.path()).env_remove("NEXUS_API_KEY");
    cmd
}

// ── Mock GitHub release ──────────────────────────────────────────────────────

/// Serves one `releases/latest` payload and the asset bytes behind it, and 404s every tag
/// lookup so resolution takes the latest-release path. Runs until the test binary exits.
struct MockRelease {
    api_base: String,
}

impl MockRelease {
    fn start(assets: Vec<(String, Vec<u8>)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let api_base = format!("http://{address}");

        let asset_json: Vec<String> = assets
            .iter()
            .enumerate()
            .map(|(index, (name, _))| format!("{{\"id\":{},\"name\":\"{name}\"}}", index + 1))
            .collect();
        let release_body = format!(
            "{{\"tag_name\":\"v{VERSION}\",\"assets\":[{}]}}",
            asset_json.join(",")
        );

        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let Some(path) = read_request_path(&mut stream) else {
                    continue;
                };

                if path.ends_with("/releases/latest") {
                    let _ = write_response(
                        &mut stream,
                        200,
                        "application/json",
                        release_body.as_bytes(),
                    );
                } else if let Some(id) = path.rsplit_once("/releases/assets/").map(|(_, id)| id) {
                    match id.parse::<usize>() {
                        Ok(id) if id >= 1 && id <= assets.len() => {
                            let _ = write_response(
                                &mut stream,
                                200,
                                "application/octet-stream",
                                &assets[id - 1].1,
                            );
                        }
                        _ => {
                            let _ =
                                write_response(&mut stream, 404, "text/plain", b"no such asset");
                        }
                    }
                } else {
                    let _ = write_response(&mut stream, 404, "text/plain", b"not found");
                }
            }
        });

        Self { api_base }
    }
}

fn read_request_path(stream: &mut std::net::TcpStream) -> Option<String> {
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
    stream: &mut std::net::TcpStream,
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

// ── S1: two platforms, one store ─────────────────────────────────────────────

#[test]
fn all_platforms_install_files_each_platform_beside_the_other() {
    let home = tempfile::tempdir().unwrap();
    let staging = tempfile::tempdir().unwrap();
    write_registry_source_config(home.path());

    let linux = native_zip(
        staging.path(),
        &format!("{NAME}-{VERSION}-linux-x86_64.mur.zip"),
        "linux",
    );
    let darwin = native_zip(
        staging.path(),
        &format!("{NAME}-{VERSION}-darwin-aarch64.mur.zip"),
        "darwin-and-then-some",
    );
    let linux_bytes = fs::read(&linux).unwrap();
    let darwin_bytes = fs::read(&darwin).unwrap();
    let release = MockRelease::start(vec![
        (
            format!("{NAME}-{VERSION}-linux-x86_64.mur.zip"),
            linux_bytes.clone(),
        ),
        (
            format!("{NAME}-{VERSION}-darwin-aarch64.mur.zip"),
            darwin_bytes.clone(),
        ),
    ]);

    mur(&home)
        .env("MUR_GITHUB_API_BASE", &release.api_base)
        .args(["install", "--all-platforms", &format!("{NAME}@{VERSION}")])
        .assert()
        .success();

    let version_dir = home
        .path()
        .join(format!(".murmur/artifacts/{NAME}/{VERSION}"));
    let mut files: Vec<String> = fs::read_dir(&version_dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    files.sort();
    assert_eq!(
        files,
        vec![
            format!("{NAME}-{VERSION}-darwin-aarch64.meta.json"),
            format!("{NAME}-{VERSION}-darwin-aarch64.mur.zip"),
            format!("{NAME}-{VERSION}-darwin-aarch64.sha256"),
            format!("{NAME}-{VERSION}-linux-x86_64.meta.json"),
            format!("{NAME}-{VERSION}-linux-x86_64.mur.zip"),
            format!("{NAME}-{VERSION}-linux-x86_64.sha256"),
        ],
        "the store must hold a payload, a hash and metadata per platform, and no generic payload"
    );

    let store = LocalRegistry::new(home.path().join(".murmur/artifacts"));
    let resolved_linux = store
        .resolve_with_platform(NAME, VERSION, Some("linux-x86_64"))
        .unwrap();
    let resolved_darwin = store
        .resolve_with_platform(NAME, VERSION, Some("darwin-aarch64"))
        .unwrap();

    assert_eq!(resolved_linux.bytes, linux_bytes);
    assert_eq!(resolved_darwin.bytes, darwin_bytes);
    assert_ne!(resolved_linux.sha256, resolved_darwin.sha256);
    assert_eq!(resolved_linux.platform_match, PlatformMatch::Tagged);
    assert_eq!(resolved_darwin.platform_match, PlatformMatch::Tagged);
    assert_eq!(resolved_linux.meta.runtime, RuntimeType::Native);
    assert_eq!(
        resolved_linux.meta.platforms,
        vec![("linux".to_string(), "x86_64".to_string())]
    );
    assert_eq!(
        resolved_darwin.meta.platforms,
        vec![("darwin".to_string(), "aarch64".to_string())]
    );

    // `mur list` reports the artifact once, for both platforms.
    let index = store.list_index().unwrap();
    assert_eq!(index.len(), 1);
    assert_eq!(index[0].platforms.len(), 2);
}

// ── S2: a WASM artifact stays untagged ───────────────────────────────────────

#[test]
fn a_wasm_artifact_installs_untagged_and_resolves_for_any_platform() {
    let home = tempfile::tempdir().unwrap();
    let staging = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("murmur.yaml"),
        "name: platform-fixture\nversion: 0.0.1\n",
    )
    .unwrap();
    write_registry_source_config(home.path());

    let artifact = wasm_zip(staging.path(), "wasmtool", VERSION);
    let bytes = fs::read(&artifact).unwrap();
    let release = MockRelease::start(vec![(format!("wasmtool-{VERSION}.mur.zip"), bytes.clone())]);

    mur(&home)
        .env("MUR_GITHUB_API_BASE", &release.api_base)
        .current_dir(project.path())
        .args(["install", &format!("wasmtool@{VERSION}")])
        .assert()
        .success();

    let version_dir = project
        .path()
        .join(format!(".murmur/artifacts/wasmtool/{VERSION}"));
    assert!(version_dir
        .join(format!("wasmtool-{VERSION}.mur.zip"))
        .exists());
    assert!(version_dir
        .join(format!("wasmtool-{VERSION}.meta.json"))
        .exists());

    let store = LocalRegistry::new(project.path().join(".murmur/artifacts"));
    // A platform this host is not: a platform-independent payload resolves for it anyway.
    let resolved = store
        .resolve_with_platform("wasmtool", VERSION, Some(foreign_platform()))
        .unwrap();
    assert!(resolved.meta.platforms.is_empty());
    assert_eq!(resolved.platform_match, PlatformMatch::NotApplicable);

    // S7's other half: a platform-independent payload is pinned once, under `any`.
    let lock = read_lockfile(&project.path().join("murmur.lock")).unwrap();
    let entry = lock.artifact_for("wasmtool").unwrap();
    assert_eq!(
        entry.sha256.any.as_deref(),
        Some(sha256_hex(&bytes).as_str())
    );
    assert!(entry.sha256.platforms.is_empty());
}

// ── S6: a platform with no published asset ───────────────────────────────────

#[test]
fn a_release_without_this_platforms_asset_fails_naming_the_platform() {
    let home = tempfile::tempdir().unwrap();
    let staging = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("murmur.yaml"),
        "name: platform-fixture\nversion: 0.0.1\n",
    )
    .unwrap();
    write_registry_source_config(home.path());

    let other = foreign_platform();
    let artifact = native_zip(
        staging.path(),
        &format!("{NAME}-{VERSION}-{other}.mur.zip"),
        "foreign",
    );
    let release = MockRelease::start(vec![(
        format!("{NAME}-{VERSION}-{other}.mur.zip"),
        fs::read(&artifact).unwrap(),
    )]);

    mur(&home)
        .env("MUR_GITHUB_API_BASE", &release.api_base)
        .current_dir(project.path())
        .args(["install", &format!("{NAME}@{VERSION}")])
        .assert()
        .failure()
        .stderr(predicate::str::contains(current_platform()))
        .stderr(predicate::str::contains(other))
        .stderr(predicate::str::contains("this release publishes"));

    assert!(
        !project.path().join(".murmur/artifacts").join(NAME).exists(),
        "a refused install must leave no payload behind"
    );
}

// ── S7 / S8: the lockfile carries a hash per platform ────────────────────────

/// Publish a native artifact into `home`'s store, the way a build host does, and return its
/// sha256. `mur publish` auto-detects this host's platform for a native payload.
fn publish_native(home: &TempDir, staging: &TempDir) -> String {
    let artifact = native_zip(
        staging.path(),
        &format!("{NAME}-{VERSION}.mur.zip"),
        "this-host",
    );
    mur(home)
        .args(["publish", artifact.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("(auto-detected)"));
    sha256_hex(&fs::read(&artifact).unwrap())
}

fn init_project(dir: &Path) {
    fs::write(
        dir.join("murmur.yaml"),
        format!(
            "name: platform-fixture\nversion: 0.0.1\nartifacts:\n  - name: {NAME}\n    version: {VERSION}\n    runtime: tool\n"
        ),
    )
    .unwrap();
}

#[test]
fn installing_a_native_artifact_pins_a_hash_under_this_platform() {
    let home = tempfile::tempdir().unwrap();
    let staging = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    init_project(project.path());

    let sha256 = publish_native(&home, &staging);

    mur(&home)
        .current_dir(project.path())
        .args(["install", &format!("{NAME}@{VERSION}")])
        .assert()
        .success();

    let lock_path = project.path().join("murmur.lock");
    let raw = fs::read_to_string(&lock_path).unwrap();
    assert!(raw.contains("lock_version: 2"), "{raw}");
    assert!(
        !raw.contains("wasm:"),
        "a v1 sha256.wasm key survived: {raw}"
    );

    let lock = read_lockfile(&lock_path).unwrap();
    let entry = lock.artifact_for(NAME).unwrap();
    assert!(entry.sha256.any.is_none());
    assert_eq!(
        entry.sha256.for_platform(current_platform()),
        Some(sha256.as_str())
    );

    // Re-running changes nothing.
    mur(&home)
        .current_dir(project.path())
        .args(["install", &format!("{NAME}@{VERSION}")])
        .assert()
        .success();
    assert_eq!(read_lockfile(&lock_path).unwrap(), lock);
}

#[test]
fn a_lock_written_for_another_platform_names_the_platform_and_is_repaired_by_install() {
    let home = tempfile::tempdir().unwrap();
    let staging = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    init_project(project.path());

    let sha256 = publish_native(&home, &staging);
    // Install first so the store holds the correct payload for this host, then replace the lock
    // with one written on a machine this is not.
    mur(&home)
        .current_dir(project.path())
        .args(["install", &format!("{NAME}@{VERSION}")])
        .assert()
        .success();

    let lock_path = project.path().join("murmur.lock");
    let other = foreign_platform();
    fs::remove_file(&lock_path).unwrap();
    write_lockfile_atomic(
        &lock_path,
        &MurmurLock {
            lock_version: LOCK_VERSION,
            artifacts: vec![LockedArtifact {
                name: NAME.to_string(),
                resolved_version: VERSION.to_string(),
                sha256: LockedSha256::for_one_platform(other, "hash-from-the-other-machine"),
            }],
        },
    )
    .unwrap();

    mur(&home)
        .current_dir(project.path())
        .args(["run"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("E-RUN-003"))
        .stderr(predicate::str::contains(current_platform()))
        .stderr(predicate::str::contains(other))
        .stderr(predicate::str::contains("artifact integrity check failed").not())
        .stderr(predicate::str::contains("re-publish or delete the lock").not());

    // Installing on this host adds this platform's hash and leaves the other one alone.
    mur(&home)
        .current_dir(project.path())
        .args(["install", &format!("{NAME}@{VERSION}")])
        .assert()
        .success();

    let entry = read_lockfile(&lock_path)
        .unwrap()
        .artifact_for(NAME)
        .cloned()
        .unwrap();
    assert_eq!(
        entry.sha256.for_platform(current_platform()),
        Some(sha256.as_str())
    );
    assert_eq!(
        entry.sha256.for_platform(other),
        Some("hash-from-the-other-machine")
    );

    // And `mur run` gets past the lock check: whatever it fails on next, it is not E-RUN-003.
    mur(&home)
        .current_dir(project.path())
        .args(["run"])
        .assert()
        .stderr(predicate::str::contains("E-RUN-003").not());
}

// ── S9: a version-1 lockfile ─────────────────────────────────────────────────

#[test]
fn a_version_one_lockfile_is_refused_by_doctor_and_run() {
    let home = tempfile::tempdir().unwrap();
    let staging = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    init_project(project.path());
    publish_native(&home, &staging);
    mur(&home)
        .current_dir(project.path())
        .args(["install", &format!("{NAME}@{VERSION}")])
        .assert()
        .success();

    fs::write(
        project.path().join("murmur.lock"),
        format!(
            "lock_version: 1\nartifacts:\n  - name: {NAME}\n    resolved_version: {VERSION}\n    sha256:\n      wasm: whatever\n"
        ),
    )
    .unwrap();

    for command in [["doctor"], ["run"]] {
        mur(&home)
            .current_dir(project.path())
            .args(command)
            .assert()
            .failure()
            .stderr(predicate::str::contains("lock_version 1"))
            .stderr(predicate::str::contains("expected 2"))
            .stderr(predicate::str::contains("delete murmur.lock"))
            .stderr(predicate::str::contains("mur install"));
    }

    // Nothing upgraded the file in place.
    let raw = fs::read_to_string(project.path().join("murmur.lock")).unwrap();
    assert!(raw.contains("lock_version: 1"), "{raw}");
}

// ── S3: an old-shape store resolves and says so ──────────────────────────────

/// A store as an install written before this change left it: a native payload and its hash at
/// the generic paths, and one untagged metadata file claiming no platform at all.
fn write_old_shape_store(root: &Path, artifact: &Path) {
    let version_dir = root.join(NAME).join(VERSION);
    fs::create_dir_all(&version_dir).unwrap();
    let bytes = fs::read(artifact).unwrap();
    fs::write(
        version_dir.join(format!("{NAME}-{VERSION}.mur.zip")),
        &bytes,
    )
    .unwrap();
    fs::write(
        version_dir.join(format!("{NAME}-{VERSION}.sha256")),
        sha256_hex(&bytes),
    )
    .unwrap();
    fs::write(
        version_dir.join(format!("{NAME}-{VERSION}.meta.json")),
        format!(
            r#"{{"meta":{{"name":"{NAME}","version":"{VERSION}","runtime":"native","artifact_runtime":"tool","platforms":[],"description":null,"tags":[]}}}}"#
        ),
    )
    .unwrap();
}

#[test]
fn an_old_shape_store_still_resolves_and_is_reported() {
    let home = tempfile::tempdir().unwrap();
    let staging = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    init_project(project.path());

    let artifact = native_zip(
        staging.path(),
        &format!("{NAME}-{VERSION}.mur.zip"),
        "pre-upgrade",
    );
    write_old_shape_store(&project.path().join(".murmur/artifacts"), &artifact);

    let store = LocalRegistry::new(project.path().join(".murmur/artifacts"));
    let resolved = store
        .resolve_with_platform(NAME, VERSION, Some(current_platform()))
        .unwrap();
    assert_eq!(resolved.platform_match, PlatformMatch::UntaggedFallback);

    // Doctor marks the line and names the repair, and still exits 0: the artifact resolves.
    mur(&home)
        .current_dir(project.path())
        .args(["doctor"])
        .assert()
        .success()
        .stdout(predicate::str::contains("W-REG-001"))
        .stdout(predicate::str::contains(format!(
            "Fix: mur install {NAME}@{VERSION}"
        )));

    // `mur run` says the same thing on stderr. It goes on to fail for its own reasons — this
    // project has no capsule component — but the warning is printed before staging starts.
    let output = mur(&home)
        .current_dir(project.path())
        .args(["run"])
        .assert()
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("warning[W-REG-001]"),
        "expected a W-REG-001 warning, got: {stderr}"
    );
    assert!(stderr.contains(&format!("{NAME}@{VERSION}")), "{stderr}");
    assert!(
        stderr.contains(&format!("mur install {NAME}@{VERSION}")),
        "{stderr}"
    );
}

// ── S10: a locally built native artifact ─────────────────────────────────────

#[test]
fn a_local_native_file_records_the_platform_its_name_carries_or_this_host() {
    let home = tempfile::tempdir().unwrap();
    let staging = tempfile::tempdir().unwrap();

    // No platform in the file name: auto-detected, and announced.
    let untagged = native_zip(
        staging.path(),
        &format!("{NAME}-{VERSION}.mur.zip"),
        "auto-detected",
    );
    mur(&home)
        .args(["install", untagged.to_str().unwrap(), "-g"])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "Platform: {} (auto-detected)",
            current_platform()
        )));

    let version_dir = home
        .path()
        .join(format!(".murmur/artifacts/{NAME}/{VERSION}"));
    let host = current_platform();
    assert!(version_dir
        .join(format!("{NAME}-{VERSION}-{host}.mur.zip"))
        .exists());
    let meta: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(version_dir.join(format!("{NAME}-{VERSION}-{host}.meta.json")))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(meta["meta"]["runtime"], "native");
    assert_eq!(
        meta["meta"]["platforms"][0][0],
        host.split('-').next().unwrap()
    );

    // The file name wins over auto-detection.
    let other = foreign_platform();
    let tagged = native_zip(
        staging.path(),
        &format!("{NAME}-{VERSION}-{other}.mur.zip"),
        "named-for-elsewhere",
    );
    mur(&home)
        .args(["install", tagged.to_str().unwrap(), "-g"])
        .assert()
        .success();
    assert!(version_dir
        .join(format!("{NAME}-{VERSION}-{other}.mur.zip"))
        .exists());
    assert!(version_dir
        .join(format!("{NAME}-{VERSION}-{other}.meta.json"))
        .exists());
}
