//! The host trust store as `mur run --explain-scope` reports it: granted to a capsule whose
//! allowlisted commands reach the network, and to no other.
//!
//! Driven through the real binary, as `read_only_scope.rs` is: the claim is that an operator reads
//! the paths a launch grants before launching anything.

#[path = "common/mod.rs"]
mod common;

use std::{fs, path::Path};

use assert_cmd::Command;
use tempfile::TempDir;

fn project(dir: &Path, capabilities_yaml: &str) {
    fs::write(
        dir.join("murmur.yaml"),
        format!("name: trust-store-capsule\nversion: 0.1.0\n{capabilities_yaml}"),
    )
    .unwrap();
    fs::write(dir.join("capsule.wasm"), b"\0asm\x01\0\0\0").unwrap();
}

/// `mur run --explain-scope [extra]`'s stdout, asserting it succeeded and that neither stream
/// carries `W-SEC-009`: no capsule here declares an `interpreter_runtime` grant.
fn explain_scope(home: &TempDir, project_dir: &Path, extra: &[&str]) -> String {
    let mut args = vec!["run", "--manifest", "murmur.yaml", "--explain-scope"];
    args.extend_from_slice(extra);
    let output = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .current_dir(project_dir)
        .args(&args)
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(!stdout.contains("W-SEC-009"), "{stdout}");
    assert!(!stderr.contains("W-SEC-009"), "{stderr}");
    stdout
}

fn explain_scope_json(home: &TempDir, project_dir: &Path) -> serde_json::Value {
    serde_json::from_str(explain_scope(home, project_dir, &["--json"]).trim()).unwrap()
}

const SHELL_CURL: &str = "  shell:\n    allow:\n      - curl\n";
const NETWORK_EXAMPLE: &str = "  network:\n    allow:\n      - example.com\n";

/// The `trust store` block of a text report: its label line and the entries under it.
fn trust_store_block(rendered: &str) -> String {
    rendered
        .lines()
        .skip_while(|line| !line.trim_start().starts_with("trust store:"))
        .take_while(|line| {
            let trimmed = line.trim_start();
            trimmed.starts_with("trust store:") || trimmed.starts_with("- ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `shell.allow` and `network.allow` together grant every trust-store path this host has, and
/// the text and JSON reports name the same set `derive_trust_store` finds.
#[test]
#[allow(clippy::print_stdout)]
fn a_networked_shell_capsule_reports_the_host_trust_store() {
    let found = capsule_runtime::derive_trust_store(
        &["curl".to_string()],
        &["example.com".to_string()],
        &[],
    );
    if found.is_empty() {
        eprintln!(
            "SKIP — PROVES NOTHING: this host has none of the trust-store locations the runtime \
             checks, so there is no grant to report"
        );
        return;
    }
    let expected: Vec<String> = found
        .iter()
        .map(|entry| {
            let listable = if entry.directory { " (list_dir)" } else { "" };
            format!("{}{listable}", entry.path.display())
        })
        .collect();

    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    project(
        dir.path(),
        &format!("capabilities:\n{SHELL_CURL}{NETWORK_EXAMPLE}"),
    );

    let rendered = explain_scope(&home, dir.path(), &[]);
    let block = trust_store_block(&rendered);
    println!("{block}");
    assert!(rendered.contains("  trust store:\n"), "{rendered}");
    let listed: Vec<&str> = block
        .lines()
        .skip(1)
        .map(|line| line.trim_start().trim_start_matches("- "))
        .collect();
    assert_eq!(listed, expected, "{rendered}");
    if fs::metadata("/etc/ssl/certs").is_ok_and(|m| m.is_dir()) {
        assert!(
            rendered.contains("    - /etc/ssl/certs (list_dir)\n"),
            "{rendered}"
        );
    }

    let json = explain_scope_json(&home, dir.path());
    println!("trust_store: {}", json["trust_store"]);
    assert_eq!(json["trust_store"], serde_json::json!(expected));
}

/// Without `network.allow`, or without `shell.allow`, nothing is granted and the report says so.
#[test]
fn a_capsule_without_both_allowlists_reports_no_trust_store() {
    let home = TempDir::new().unwrap();
    for capabilities in [
        format!("capabilities:\n{SHELL_CURL}"),
        format!("capabilities:\n{NETWORK_EXAMPLE}"),
    ] {
        let dir = TempDir::new().unwrap();
        project(dir.path(), &capabilities);

        let rendered = explain_scope(&home, dir.path(), &[]);
        assert!(
            rendered.contains("  trust store: <none>\n"),
            "{capabilities}\n{rendered}"
        );
        let json = explain_scope_json(&home, dir.path());
        assert_eq!(json["trust_store"], serde_json::json!([]), "{capabilities}");
    }
}
