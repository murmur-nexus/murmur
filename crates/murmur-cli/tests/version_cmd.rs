//! `mur version`: this mur's version, the WIT level it serves and that level's package table.

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
        "mur {}\nWIT level {}\n\n{:<width$}VERSION\n",
        env!("CARGO_PKG_VERSION"),
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
    let keys: Vec<&str> = value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys.len(), 3, "{keys:?}");
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
