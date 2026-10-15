//! `mur install` refuses another platform's release asset rather than downloading it.
//!
//! The stub source's index lists two platforms picked from `SUPPORTED_PLATFORMS` with this host's
//! excluded, so the case under test is a real miss on whatever machine runs the suite.

#[path = "common/mod.rs"]
mod common;

use std::{fs, path::Path};

use assert_cmd::Command;
use common::github_release::{native_tool_zip, publish_dir, MockGitHub};
use murmur_artifact::{current_platform, SUPPORTED_PLATFORMS};
use predicates::prelude::*;

/// The bare name the release publishes its assets under.
const ARTIFACT: &str = "tool-git";
/// The full name installed: the scope of the `acme/artifacts` source.
const FULL_NAME: &str = "acme/tool-git";
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

fn write_source_config(home: &Path) {
    let config_dir = home.join(".murmur");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.yaml"),
        "registry:\n  sources:\n    - repo: acme/artifacts\n",
    )
    .unwrap();
}

#[test]
fn install_refuses_a_release_that_publishes_no_asset_for_this_platform() {
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let staging = tempfile::tempdir().unwrap();
    write_source_config(home.path());

    let platforms = published_platforms();
    for platform in &platforms {
        native_tool_zip(staging.path(), ARTIFACT, VERSION, platform);
    }
    let stub = MockGitHub::start();
    stub.repository("acme/artifacts", 4242);
    publish_dir(&stub, "acme/artifacts", RELEASE_TAG, staging.path());

    let mut assertion = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .env("MUR_GITHUB_API_BASE", &stub.api_base)
        .env_remove("GITHUB_TOKEN")
        .current_dir(cwd.path())
        .args(["install", &format!("{FULL_NAME}@{VERSION}"), "-g"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("E-REG-001"))
        .stderr(predicate::str::contains(current_platform()))
        .stderr(predicate::str::contains("mur build"))
        .stderr(predicate::str::contains("ask the publisher"))
        .stderr(predicate::str::contains("mur doctor").not())
        .stderr(predicate::str::contains("E-REG-006").not());
    for platform in &platforms {
        assertion = assertion.stderr(predicate::str::contains(*platform));
    }
    drop(assertion);

    // A refusal writes nothing, and no payload download is issued at all.
    let store = home
        .path()
        .join(".murmur")
        .join("artifacts")
        .join(FULL_NAME);
    assert!(!store.exists(), "{} was written", store.display());
    assert!(!stub.downloaded_a_payload(), "{:?}", stub.paths());
}
