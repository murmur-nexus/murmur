//! The `Memory bound` block `mur doctor` prints: the cgroup files a launch writes from
//! `capabilities.resources.cgroup_memory_bytes`.
//!
//! Driven through the real binary, so the values are the ones a launch on this host writes: a
//! declared `cgroup_memory_bytes` lands in `memory.max`, and swap is bounded at zero wherever this
//! kernel exposes `memory.swap.max`.

use std::{fs, path::Path};

use assert_cmd::Command;
use tempfile::TempDir;

fn doctor_stdout(home: &TempDir, project_dir: &Path) -> String {
    let output = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .current_dir(project_dir)
        .arg("doctor")
        .assert()
        .get_output()
        .stdout
        .clone();
    String::from_utf8(output).unwrap()
}

/// The `Memory bound` block's lines, up to the blank line that ends it.
fn memory_block(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .skip_while(|line| *line != "Memory bound")
        .skip(1)
        .take_while(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

#[test]
fn doctor_reports_the_declared_memory_ceiling_and_a_zero_swap_bound() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    fs::write(
        project.path().join("murmur.yaml"),
        "name: demo\nversion: 0.1.0\ncapabilities:\n  resources:\n    cgroup_memory_bytes: 268435456\n",
    )
    .unwrap();
    fs::write(project.path().join("capsule.wasm"), b"\0asm\x01\0\0\0").unwrap();

    let stdout = doctor_stdout(&home, project.path());
    let block = memory_block(&stdout);
    assert_eq!(
        block.first().map(String::as_str),
        Some("  cgroup_memory_bytes: 268435456"),
        "stdout was:\n{stdout}"
    );

    if cfg!(target_os = "linux") {
        assert!(
            block.iter().any(|line| line == "  memory.max 268435456"),
            "stdout was:\n{stdout}"
        );
        match capsule_runtime::probe_swap_control() {
            capsule_runtime::SwapControl::Absent => assert!(
                block
                    .iter()
                    .any(|line| line.contains("swap is not bounded on this host")),
                "stdout was:\n{stdout}"
            ),
            capsule_runtime::SwapControl::Exposed => assert!(
                block.iter().any(|line| line == "  memory.swap.max 0"),
                "stdout was:\n{stdout}"
            ),
            capsule_runtime::SwapControl::Unknown => assert!(
                block.iter().any(
                    |line| line.starts_with("  memory.swap.max 0 where this kernel exposes it")
                ),
                "stdout was:\n{stdout}"
            ),
        }
    } else {
        assert!(
            block
                .iter()
                .any(|line| line.contains("cgroup bounds are Linux-only")),
            "stdout was:\n{stdout}"
        );
    }
}

/// Where this kernel exposes `memory.swap.max`, the block prints both cgroup values exactly. A
/// host whose probe answers otherwise is covered by the test above.
#[cfg(target_os = "linux")]
#[test]
fn a_linux_host_with_a_memory_controller_shows_both_values() {
    if capsule_runtime::probe_swap_control() != capsule_runtime::SwapControl::Exposed {
        eprintln!(
            "[SKIP-HOST] a_linux_host_with_a_memory_controller_shows_both_values: this kernel \
             exposes no memory.swap.max to this process's cgroups"
        );
        return;
    }
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    fs::write(
        project.path().join("murmur.yaml"),
        "name: demo\nversion: 0.1.0\ncapabilities:\n  resources:\n    cgroup_memory_bytes: 268435456\n",
    )
    .unwrap();
    fs::write(project.path().join("capsule.wasm"), b"\0asm\x01\0\0\0").unwrap();
    let block = memory_block(&doctor_stdout(&home, project.path()));
    assert_eq!(
        block,
        [
            "  cgroup_memory_bytes: 268435456",
            "  memory.max 268435456",
            "  memory.swap.max 0"
        ]
    );
}
