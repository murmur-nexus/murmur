//! `mur install` reports a GitHub source that refused a lookup as unanswered (`E-REG-006`), not as
//! an artifact that does not exist (`E-REG-001`), and names the fix that applies.
//!
//! The server is a plain HTTP/1.1 responder on a loopback port, reached through
//! `MUR_GITHUB_API_BASE`; no test here talks to the real GitHub API.

use std::{
    fs,
    io::{BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    sync::{Arc, Mutex},
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

use assert_cmd::Command;
use predicates::prelude::*;

const ARTIFACT: &str = "murmur-tool-git";
const VERSION: &str = "0.4.2";

const RATE_LIMIT_BODY: &str = "{\"message\":\"API rate limit exceeded for 127.0.0.1. (But here's the good news: Authenticated requests get a higher rate limit.)\",\"documentation_url\":\"https://docs.github.com/rest/overview/resources-in-the-rest-api#rate-limiting\"}";

const SAML_BODY: &str = "{\"message\":\"Resource protected by organization SAML enforcement. You must grant your Personal Access token access to this organization.\"}";

const NOT_FOUND_BODY: &str = "{\"message\":\"Not Found\"}";

/// A canned response: status line, extra header lines, body.
struct Reply {
    status: &'static str,
    headers: Vec<String>,
    body: String,
}

impl Reply {
    fn rate_limited() -> Self {
        let reset = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 600;
        Self {
            status: "403 Forbidden",
            headers: vec![
                "x-ratelimit-limit: 60".to_string(),
                "x-ratelimit-remaining: 0".to_string(),
                format!("x-ratelimit-reset: {reset}"),
            ],
            body: RATE_LIMIT_BODY.to_string(),
        }
    }

    fn not_found() -> Self {
        Self {
            status: "404 Not Found",
            headers: Vec::new(),
            body: NOT_FOUND_BODY.to_string(),
        }
    }

    fn ok(body: String) -> Self {
        Self {
            status: "200 OK",
            headers: Vec::new(),
            body,
        }
    }
}

/// A loopback GitHub API that answers each request path with `route(path)` and records the
/// `authorization` header of every request it received.
struct Stub {
    api_base: String,
    authorizations: Arc<Mutex<Vec<Option<String>>>>,
}

impl Stub {
    fn start(route: impl Fn(&str) -> Reply + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let api_base = format!("http://{}", listener.local_addr().unwrap());
        let authorizations = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&authorizations);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                respond(stream, &route, &seen);
            }
        });
        Self {
            api_base,
            authorizations,
        }
    }

    fn authorizations(&self) -> Vec<Option<String>> {
        self.authorizations.lock().unwrap().clone()
    }
}

fn respond(
    mut stream: TcpStream,
    route: &impl Fn(&str) -> Reply,
    seen: &Mutex<Vec<Option<String>>>,
) {
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(clone) => clone,
        Err(_) => return,
    });
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    let mut authorization = None;
    loop {
        let mut header = String::new();
        match reader.read_line(&mut header) {
            Ok(0) => break,
            Ok(_) if header.trim().is_empty() => break,
            Ok(_) => {
                if let Some((name, value)) = header.split_once(':') {
                    if name.trim().eq_ignore_ascii_case("authorization") {
                        authorization = Some(value.trim().to_string());
                    }
                }
            }
            Err(_) => return,
        }
    }
    seen.lock().unwrap().push(authorization);

    let path = request_line.split_whitespace().nth(1).unwrap_or("/");
    let reply = route(path);
    let mut head = format!(
        "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        reply.status,
        reply.body.len()
    );
    for header in &reply.headers {
        head.push_str(header);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(reply.body.as_bytes());
    let _ = stream.flush();
}

fn write_config(home: &Path, config: &str) {
    let config_dir = home.join(".murmur");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(config_dir.join("config.yaml"), config).unwrap();
}

fn write_single_source_config(home: &Path) {
    write_config(
        home,
        "registry:\n  default: official\n  sources:\n    - name: official\n      type: github\n      repo: acme/artifacts\n",
    );
}

/// `mur` with `HOME`, the stub's API base, and `GITHUB_TOKEN` set to `token` or removed.
fn mur(home: &Path, cwd: &Path, stub: &Stub, token: Option<&str>) -> Command {
    let mut command = Command::cargo_bin("mur").unwrap();
    command
        .env("HOME", home)
        .env("MUR_GITHUB_API_BASE", &stub.api_base)
        .env_remove("NEXUS_API_KEY")
        .current_dir(cwd);
    match token {
        Some(token) => command.env("GITHUB_TOKEN", token),
        None => command.env_remove("GITHUB_TOKEN"),
    };
    command
}

fn stderr_of(assert: &assert_cmd::assert::Assert) -> String {
    String::from_utf8(assert.get_output().stderr.clone()).unwrap()
}

fn hint_line(stderr: &str) -> &str {
    stderr
        .lines()
        .find(|line| line.trim_start().starts_with("hint:"))
        .unwrap_or_else(|| panic!("no hint line in:\n{stderr}"))
}

fn assert_nothing_installed(home: &Path) {
    let store = home.join(".murmur").join("artifacts").join(ARTIFACT);
    assert!(!store.exists(), "{} was written", store.display());
}

/// S1: an unauthenticated rate limit is reported as unanswered, with the token hint.
#[test]
fn an_unauthenticated_rate_limit_is_unanswered_and_names_the_token_fix() {
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    write_single_source_config(home.path());
    let stub = Stub::start(|_| Reply::rate_limited());

    let assert = mur(home.path(), cwd.path(), &stub, None)
        .args(["install", &format!("{ARTIFACT}@{VERSION}"), "-g"])
        .assert()
        .failure();
    let stderr = stderr_of(&assert);

    for needle in [
        "E-REG-006",
        "github:acme/artifacts",
        "GitHub rate-limited the lookup",
        "rate limited by GitHub (HTTP 403, x-ratelimit-remaining: 0)",
        "API rate limit exceeded",
        "minute",
        "GITHUB_TOKEN",
        "gh auth token",
        "token:",
    ] {
        assert!(stderr.contains(needle), "{needle} missing from:\n{stderr}");
    }
    for absent in [
        "E-REG-001",
        "could not resolve",
        "mur doctor",
        "mur config set",
        "documentation_url",
    ] {
        assert!(!stderr.contains(absent), "{absent} present in:\n{stderr}");
    }
    assert_nothing_installed(home.path());
}

/// S2: when the rate-limited requests carried a token, the hint says that token is exhausted.
#[test]
fn a_rate_limit_with_a_token_gives_no_token_advice() {
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    write_single_source_config(home.path());
    let stub = Stub::start(|_| Reply::rate_limited());

    let assert = mur(home.path(), cwd.path(), &stub, Some("test-token"))
        .args(["install", &format!("{ARTIFACT}@{VERSION}"), "-g"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("E-REG-006"))
        .stderr(predicate::str::contains("rate limited"));
    let stderr = stderr_of(&assert);
    let hint = hint_line(&stderr);

    assert!(!hint.contains("export GITHUB_TOKEN"), "{hint}");
    assert!(!hint.contains("gh auth token"), "{hint}");
    assert!(hint.contains("exhausted"), "{hint}");

    let authorizations = stub.authorizations();
    assert!(!authorizations.is_empty());
    assert!(
        authorizations
            .iter()
            .all(|value| value.as_deref() == Some("Bearer test-token")),
        "{authorizations:?}"
    );
}

/// S3: a 403 that is not a rate limit is unanswered, but not rate limited and gets no token hint.
#[test]
fn a_saml_refusal_is_unanswered_without_rate_limit_advice() {
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    write_single_source_config(home.path());
    let stub = Stub::start(|_| Reply {
        status: "403 Forbidden",
        headers: vec!["x-ratelimit-remaining: 4999".to_string()],
        body: SAML_BODY.to_string(),
    });

    let assert = mur(home.path(), cwd.path(), &stub, Some("test-token"))
        .args(["install", &format!("{ARTIFACT}@{VERSION}"), "-g"])
        .assert()
        .failure();
    let stderr = stderr_of(&assert);

    for needle in ["E-REG-006", "HTTP 403", "SAML"] {
        assert!(stderr.contains(needle), "{needle} missing from:\n{stderr}");
    }
    for absent in [
        "E-REG-001",
        "rate limited",
        "rate-limited",
        "gh auth token",
        "mur doctor",
    ] {
        assert!(!stderr.contains(absent), "{absent} present in:\n{stderr}");
    }
}

/// S5: a rate limit on the asset download, after the release lookup answered, is unanswered too.
#[test]
fn a_rate_limited_asset_download_is_unanswered() {
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    write_single_source_config(home.path());
    let stub = Stub::start(|path| {
        if path.ends_with("/releases/latest") {
            Reply::ok(format!(
                "{{\"tag_name\":\"v9.9.9\",\"assets\":[{{\"id\":1,\"name\":\"{ARTIFACT}-{VERSION}.mur.zip\"}}]}}"
            ))
        } else if path.ends_with("/releases/assets/1") {
            Reply::rate_limited()
        } else {
            Reply::not_found()
        }
    });

    let assert = mur(home.path(), cwd.path(), &stub, None)
        .args(["install", &format!("{ARTIFACT}@{VERSION}"), "-g"])
        .assert()
        .failure();
    let stderr = stderr_of(&assert);

    for needle in ["E-REG-006", "rate limited", "GITHUB_TOKEN"] {
        assert!(stderr.contains(needle), "{needle} missing from:\n{stderr}");
    }
    assert!(!stderr.contains("E-REG-001"), "{stderr}");
    assert_nothing_installed(home.path());
}

/// S6: one rate-limited source and one that answered "not found" still cannot claim the artifact
/// is absent, and both sources get a line.
#[test]
fn a_chain_with_one_unanswered_source_is_unanswered() {
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    write_config(
        home.path(),
        "registry:\n  default: first\n  sources:\n    - name: first\n      type: github\n      repo: acme/first\n    - name: second\n      type: github\n      repo: acme/second\n",
    );
    let stub = Stub::start(|path| {
        if path.starts_with("/repos/acme/first/") {
            Reply::rate_limited()
        } else {
            Reply::not_found()
        }
    });

    let assert = mur(home.path(), cwd.path(), &stub, None)
        .args(["install", &format!("{ARTIFACT}@{VERSION}"), "-g"])
        .assert()
        .failure();
    let stderr = stderr_of(&assert);

    assert!(stderr.contains("E-REG-006"), "{stderr}");
    assert!(!stderr.contains("E-REG-001"), "{stderr}");
    let first: Vec<&str> = stderr
        .lines()
        .filter(|line| line.contains("github:acme/first"))
        .collect();
    assert_eq!(first.len(), 1, "{stderr}");
    assert!(first[0].contains("rate limited"), "{stderr}");
    assert_eq!(
        stderr
            .lines()
            .filter(|line| line.contains("github:acme/second"))
            .count(),
        1,
        "{stderr}"
    );
}

/// S7: a bare `mur install` in a project reports each rate-limited artifact as unanswered.
#[test]
fn a_manifest_install_reports_every_rate_limited_artifact_as_unanswered() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write_single_source_config(home.path());
    fs::write(
        project.path().join("murmur.yaml"),
        format!(
            "name: dep-project\nversion: 0.0.1\nartifacts:\n  - name: murmur-tool-git\n    version: {VERSION}\n    runtime: tool\n  - name: murmur-tool-fs\n    version: {VERSION}\n    runtime: tool\n"
        ),
    )
    .unwrap();
    let stub = Stub::start(|_| Reply::rate_limited());

    let assert = mur(home.path(), project.path(), &stub, None)
        .arg("install")
        .assert()
        .failure();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let stderr = stderr_of(&assert);

    assert!(
        stdout.contains(&format!("murmur-tool-git@{VERSION}")),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!("murmur-tool-fs@{VERSION}")),
        "{stdout}"
    );
    assert_eq!(stdout.matches("error[E-REG-006]:").count(), 2, "{stdout}");
    assert_eq!(stdout.matches("error[E-REG-001]:").count(), 0, "{stdout}");
    assert!(!stdout.contains("mur doctor"), "{stdout}");
    assert!(!stderr.contains("mur doctor"), "{stderr}");
}

/// S8: an explicit `github:` URI that is rate limited is unanswered, not a general I/O error.
#[test]
fn a_rate_limited_explicit_uri_is_unanswered() {
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    write_single_source_config(home.path());
    let stub = Stub::start(|_| Reply::rate_limited());

    let assert = mur(home.path(), cwd.path(), &stub, None)
        .args(["install", "github:acme/artifacts@v1.0.0", "-g"])
        .assert()
        .failure();
    let stderr = stderr_of(&assert);

    for needle in ["E-REG-006", "GITHUB_TOKEN"] {
        assert!(stderr.contains(needle), "{needle} missing from:\n{stderr}");
    }
    assert!(!stderr.contains("E-IO-003"), "{stderr}");
}
