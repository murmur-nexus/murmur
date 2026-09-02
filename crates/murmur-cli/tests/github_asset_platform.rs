//! `mur install` refuses another platform's release asset rather than downloading it.
//!
//! The stub release publishes two platforms picked from `SUPPORTED_PLATFORMS` with this host's
//! excluded, so the case under test is a real miss on whatever machine runs the suite. The server
//! is a plain HTTP/1.1 responder on a loopback port: `ureq` needs no TLS for `http://`, so no test
//! dependency is added for it.

use std::{
    fs,
    io::{BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    thread,
};

use assert_cmd::Command;
use murmur_artifact::{current_platform, SUPPORTED_PLATFORMS};
use predicates::prelude::*;

const ARTIFACT: &str = "murmur-tool-git";
const VERSION: &str = "0.4.2";
const RELEASE_TAG: &str = "v9.9.9";

/// Two platform tags that are not this host's, so no asset in the stub release can legitimately
/// resolve here.
fn published_platforms() -> Vec<&'static str> {
    SUPPORTED_PLATFORMS
        .into_iter()
        .filter(|platform| *platform != current_platform())
        .take(2)
        .collect()
}

/// The `releases/latest` body: platform-tagged assets for two foreign platforms and nothing else.
fn latest_release_json(platforms: &[&str]) -> String {
    let assets: Vec<String> = platforms
        .iter()
        .enumerate()
        .map(|(index, platform)| {
            format!(
                "{{\"id\":{},\"name\":\"{ARTIFACT}-{VERSION}-{platform}.mur.zip\"}}",
                index + 1
            )
        })
        .collect();
    format!(
        "{{\"tag_name\":\"{RELEASE_TAG}\",\"assets\":[{}]}}",
        assets.join(",")
    )
}

/// Serve the canned release on a loopback port until the process exits.
///
/// Only `releases/latest` has a body; every other path is a 404, which is how the real API reports
/// a release tag that does not exist and drives the resolver onto its latest-release fallback.
fn serve_release(listener: TcpListener, body: String) {
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            respond(stream, &body);
        }
    });
}

fn respond(mut stream: TcpStream, body: &str) {
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(clone) => clone,
        Err(_) => return,
    });
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    // Drain the headers so the client sees a complete exchange rather than a reset.
    loop {
        let mut header = String::new();
        match reader.read_line(&mut header) {
            Ok(0) => break,
            Ok(_) if header.trim().is_empty() => break,
            Ok(_) => {}
            Err(_) => return,
        }
    }

    let (status, payload) = if request_line.contains("/releases/latest") {
        ("200 OK", body)
    } else {
        ("404 Not Found", "{\"message\":\"Not Found\"}")
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

fn write_source_config(home: &Path) {
    let config_dir = home.join(".murmur");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.yaml"),
        "registry:\n  default: official\n  sources:\n    - name: official\n      type: github\n      repo: acme/artifacts\n",
    )
    .unwrap();
}

#[test]
fn install_refuses_a_release_that_publishes_no_asset_for_this_platform() {
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    write_source_config(home.path());

    let platforms = published_platforms();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let api_base = format!("http://{}", listener.local_addr().unwrap());
    serve_release(listener, latest_release_json(&platforms));

    let mut assertion = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .env("MUR_GITHUB_API_BASE", &api_base)
        .env_remove("GITHUB_TOKEN")
        .current_dir(cwd.path())
        .args(["install", &format!("{ARTIFACT}@{VERSION}"), "-g"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("E-REG-001"))
        .stderr(predicate::str::contains(current_platform()))
        .stderr(predicate::str::contains("mur build"))
        .stderr(predicate::str::contains("ask the publisher"));
    for platform in &platforms {
        assertion = assertion.stderr(predicate::str::contains(*platform));
    }
    drop(assertion);

    // A refusal writes nothing: no asset download is issued at all.
    let store = home.path().join(".murmur").join("artifacts").join(ARTIFACT);
    assert!(!store.exists(), "{} was written", store.display());
}
