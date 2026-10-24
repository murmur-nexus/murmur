//! `mur version`: this mur's version, the commit it was built from, the WIT level it serves,
//! that level's package table and the machine-readable commands it serves.

use std::process::Command as StdCommand;

use assert_cmd::Command;
use capsule_runtime::{served_wit_level, SERVED_WIT_PACKAGES};
use serde_json::Value;
use tempfile::TempDir;

fn mur_version(args: &[&str]) -> String {
    let home = TempDir::new().unwrap();
    let output = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .arg("version")
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).unwrap()
}

/// The commit of the checkout this test binary was built in, which `mur` was built from too.
fn head_commit() -> String {
    let output = StdCommand::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let commit = String::from_utf8(output.stdout).unwrap().trim().to_string();
    assert_eq!(commit.len(), 40, "{commit}");
    commit
}

#[test]
fn version_prints_the_mur_version_the_served_level_and_its_packages() {
    let stdout = mur_version(&[]);
    let level = served_wit_level();
    let width = SERVED_WIT_PACKAGES
        .iter()
        .map(|(package, _)| package.len())
        .max()
        .unwrap()
        + 2;
    let mut expected = format!(
        "mur {}\ncommit {}\nWIT level {}\n\n{:<width$}VERSION\n",
        env!("CARGO_PKG_VERSION"),
        head_commit(),
        level.level,
        "PACKAGE"
    );
    for (package, version) in SERVED_WIT_PACKAGES {
        expected.push_str(&format!("{package:<width$}{version}\n"));
    }
    assert_eq!(stdout, expected);
}

#[test]
fn version_json_is_one_object_naming_the_same_level_and_packages() {
    let stdout = mur_version(&["--json"]);
    assert!(stdout.ends_with("}\n"), "{stdout}");
    let value: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(value["mur_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(value["commit"], head_commit());
    assert_eq!(value["wit_level"], served_wit_level().level);
    let packages: Vec<(String, String)> = value["packages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            (
                entry["package"].as_str().unwrap().to_string(),
                entry["version"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    let served: Vec<(String, String)> = SERVED_WIT_PACKAGES
        .iter()
        .map(|(package, version)| (package.to_string(), version.to_string()))
        .collect();
    assert_eq!(packages, served);
    // `Value` sorts its keys, so their order is read off the text.
    let keys: Vec<&str> = stdout
        .lines()
        .filter_map(|line| line.strip_prefix("  \""))
        .filter_map(|line| line.split_once('"').map(|(key, _)| key))
        .collect();
    assert_eq!(
        keys,
        ["mur_version", "commit", "wit_level", "packages", "commands"]
    );
    let commands = concat!(
        "  \"commands\": {\n",
        "    \"control show --json\": 1,\n",
        "    \"conversation ls --json\": 1,\n",
        "    \"conversation show --json\": 1,\n",
        "    \"eval diff --run --json\": 1,\n",
        "    \"eval show --json\": 1,\n",
        "    \"precompile --json\": 1,\n",
        "    \"ps --json\": 1,\n",
        "    \"run --explain-scope --json\": 1,\n",
        "    \"run --json\": 1,\n",
        "    \"run --roster --json\": 1,\n",
        "    \"token\": 1,\n",
        "    \"trace list --json\": 1,\n",
        "    \"trace rm --json\": 1,\n",
        "    \"version --json\": 1\n",
        "  }\n",
        "}\n",
    );
    assert!(stdout.ends_with(commands), "{stdout}");
}

#[test]
fn dash_dash_version_is_unchanged() {
    let home = TempDir::new().unwrap();
    let output = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .arg("--version")
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("murmur-cli {}\n", env!("CARGO_PKG_VERSION"))
    );
}
