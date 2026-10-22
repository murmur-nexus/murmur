//! The tool names the runtime answers itself are not available to artifacts, and a capsule that
//! declares one is refused before anything is resolved, pulled or created.
//!
//! Every case here runs the real `mur` binary against an empty artifact store. That the store is
//! empty is the measurement: a missing-artifact error would mean the refusal came from the
//! registry rather than from the reserved-name rule, and an operator sent to `mur install` for a
//! name no registry may serve would be sent nowhere.

#[path = "common/mod.rs"]
mod common;

use std::{fs, path::Path};

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

const CAPSULE_NAME: &str = "reserved-name-capsule";
const RESERVED: [&str; 7] = [
    "share-file",
    "fetch-peer-file",
    "delegate-task",
    "submit-plan",
    "end-without-answer",
    "session-status",
    "background-command",
];

/// An agent capsule declaring one tool artifact whose tool name is `tool`, under
/// [`full_name`], plus whatever extra top-level YAML the case needs. Nothing it names is
/// published anywhere.
fn manifest(project: &Path, tool: &str, blocks: &str) -> std::path::PathBuf {
    let artifact_name = common::local(tool);
    let yaml = format!(
        "name: {CAPSULE_NAME}\nversion: 0.1.0\n{blocks}artifacts:\n  - name: {artifact_name}\n    \
         version: 0.1.0\n    runtime: tool\n  - name: local/murmur-driver-anthropic\n    version: \
         0.1.0\n    runtime: driver\n    gateway:\n      endpoint: http://127.0.0.1:1\n      \
         api_key: test-key\ninference:\n  transport: http\n  model: test-model\n  driver:\n    \
         artifact: local/murmur-driver-anthropic\n",
    );
    let path = project.join("murmur.yaml");
    fs::write(&path, yaml).unwrap();
    path
}

fn run(home: &TempDir, manifest_path: &Path) -> assert_cmd::assert::Assert {
    let mut cmd = Command::cargo_bin("mur").unwrap();
    cmd.env("HOME", home.path())
        .args([
            "run",
            "--manifest",
            manifest_path.to_str().unwrap(),
            "--task",
            "anything",
        ])
        .assert()
}

/// Each reserved name is refused by code, naming the declared full name and the tool name that
/// collides, and the message carries the whole set so the operator can see what else is
/// off-limits without opening the docs.
#[test]
fn an_artifact_under_a_reserved_name_refuses_the_launch() {
    let home = tempfile::tempdir().unwrap();

    for name in RESERVED {
        let project = tempfile::tempdir().unwrap();
        let path = manifest(project.path(), name, "");

        let assert = run(&home, &path)
            .failure()
            .stderr(predicate::str::contains("error[E-CAP-013]"))
            .stderr(predicate::str::contains(format!(
                "artifact '{}' has the tool name '{name}', which collides with a tool the runtime \
                 provides itself",
                common::local(name)
            )))
            .stderr(
                predicate::str::contains("rename the artifact")
                    .or(predicate::str::contains("Rename the artifact")),
            );

        let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
        for reserved in RESERVED {
            assert!(
                stderr.contains(reserved),
                "the refusal lists the whole reserved set, missing '{reserved}': {stderr}"
            );
        }
        // The store is empty, so a check that ran after registry resolution would have produced
        // this instead. Its absence is what proves the ordering.
        assert!(
            !stderr.contains("missing artifacts") && !stderr.contains("E-REG-001"),
            "the refusal must precede registry resolution: {stderr}"
        );
        // Nothing staged, so nothing to leave behind.
        assert!(
            !project.path().join("workdir").exists(),
            "a refused launch creates no session directory: {stderr}"
        );
    }
}

/// A name is reserved whether or not this capsule would have been granted the tool. Otherwise the
/// rule would depend on grants the operator can change, and the same manifest would be legal or
/// illegal by accident.
#[test]
fn the_refusal_does_not_depend_on_the_grant_that_provides_the_tool() {
    let home = tempfile::tempdir().unwrap();
    let grants = [
        ("share-file", "exports:\n  peer_files:\n    root: out\n"),
        (
            "fetch-peer-file",
            "capabilities:\n  peer_fetch:\n    allow:\n      - localhost\n",
        ),
        (
            "delegate-task",
            "capabilities:\n  spawn:\n    allow:\n      - local/worker\n",
        ),
        ("submit-plan", "capabilities:\n  plan:\n    submit: true\n"),
    ];

    for (name, blocks) in grants {
        let project = tempfile::tempdir().unwrap();
        let path = manifest(project.path(), name, blocks);
        run(&home, &path)
            .failure()
            .stderr(predicate::str::contains("error[E-CAP-013]"))
            .stderr(predicate::str::contains(format!(
                "artifact '{}' has the tool name '{name}'",
                common::local(name)
            )));
    }
}

/// Every name the runtime answers itself, in the order `E-CAP-013` lists them.
const ALL_RESERVED: [&str; 11] = [
    "share-file",
    "fetch-peer-file",
    "delegate-task",
    "submit-plan",
    "switch-driver",
    "call-member",
    "end-without-answer",
    "pull-artifact",
    "remove-artifact",
    "session-status",
    "background-command",
];

/// The two artifact tools are reserved under any scope: an artifact a capsule declared, or one it
/// pulled, could otherwise take the name the runtime answers a pull or a removal under.
#[test]
fn the_artifact_tools_are_reserved_under_any_scope() {
    let home = tempfile::tempdir().unwrap();
    for (full_name, tool) in [
        ("local/pull-artifact", "pull-artifact"),
        ("ana/remove-artifact", "remove-artifact"),
    ] {
        let project = tempfile::tempdir().unwrap();
        let path = manifest(project.path(), tool, "");
        let yaml = fs::read_to_string(&path)
            .unwrap()
            .replace(&common::local(tool), full_name);
        fs::write(&path, yaml).unwrap();

        let assert = run(&home, &path)
            .failure()
            .stderr(predicate::str::contains("error[E-CAP-013]"))
            .stderr(predicate::str::contains(format!(
                "artifact '{full_name}' has the tool name '{tool}', which collides with a tool \
                 the runtime provides itself"
            )));
        let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
        assert!(
            stderr.contains(&ALL_RESERVED.join(", ")),
            "the refusal lists all eleven names: {stderr}"
        );
        assert!(!stderr.contains("E-REG-001"), "{stderr}");
    }
}

/// `--explain-scope` describes the launch rather than offering a second opinion about it: a
/// manifest a real run refuses is not reported as fine.
#[test]
fn explain_scope_refuses_the_same_manifest() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let path = manifest(project.path(), "delegate-task", "");

    Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .args([
            "run",
            "--manifest",
            path.to_str().unwrap(),
            "--explain-scope",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("error[E-CAP-013]"));
}

/// Shell binary names are operator-chosen, so they are not reserved: a capsule may declare an
/// artifact sharing a name with an allowlisted binary. This one fails on the empty store, which is
/// exactly the point — it gets as far as registry resolution rather than being refused by name.
#[test]
fn a_shell_binary_name_is_not_reserved() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let path = manifest(
        project.path(),
        "bash",
        "capabilities:\n  shell:\n    allow:\n      - bash\n",
    );

    let stderr =
        String::from_utf8(run(&home, &path).failure().get_output().stderr.clone()).unwrap();
    assert!(
        !stderr.contains("E-CAP-013"),
        "an operator-chosen binary name is not reserved: {stderr}"
    );
}
