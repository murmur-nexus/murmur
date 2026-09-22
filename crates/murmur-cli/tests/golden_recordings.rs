//! The recorded Claude Code runs under `.nexus/roadmap/fixtures/claude-code-2.1.278/` still say
//! what a driver would be written against.
//!
//! Nothing in `crates/` reads those recordings: the driver that consumes this format lives in the
//! separate `default-artifacts` repository, and this repository holds them only as the record of
//! what a real harness emitted. This file is what keeps them honest — that every line is still
//! JSON, and that the `usage` members `murmur:driver/process@0.2.0`'s `usage` record names are
//! still readable in them.
//!
//! The directory is not tracked by git (`.gitignore` excludes `.nexus`), so a clean checkout has
//! nothing to check and this test skips. It runs where the recordings are.

use std::path::{Path, PathBuf};

use serde_json::Value;

fn recordings_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(".nexus/roadmap/fixtures/claude-code-2.1.278")
}

/// The Claude Code spellings of the members the `usage` record names, each as the path to
/// read it by. Only a driver knows these names — see the invariant in the runtime: nothing under
/// `crates/` outside this test file may spell one.
const USAGE_MEMBERS: [&[&str]; 5] = [
    &["input_tokens"],
    &["output_tokens"],
    &["cache_read_input_tokens"],
    &["cache_creation_input_tokens"],
    &["output_tokens_details", "thinking_tokens"],
];

#[test]
fn every_recorded_line_is_json_and_still_carries_the_usage_members() {
    let dir = recordings_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        println!(
            "skipped: no recordings at {} — the .nexus tree is not tracked by git",
            dir.display()
        );
        return;
    };

    let mut recordings: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.to_string_lossy().ends_with(".stdout.jsonl"))
        .collect();
    recordings.sort();
    assert!(!recordings.is_empty(), "no .stdout.jsonl under {dir:?}");

    let mut found = [false; USAGE_MEMBERS.len()];
    for path in &recordings {
        let body = std::fs::read_to_string(path).unwrap();
        for (number, line) in body.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let event: Value = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("{path:?} line {}: {e}", number + 1));
            // `usage` rides on the `result` line and on the `message` of an assistant line, so
            // both are looked into rather than only the top level.
            for usage in [event.get("usage"), event.pointer("/message/usage")]
                .into_iter()
                .flatten()
            {
                for (index, member) in USAGE_MEMBERS.iter().enumerate() {
                    let mut at = usage;
                    let readable = member.iter().all(|key| match at.get(key) {
                        Some(next) => {
                            at = next;
                            true
                        }
                        None => false,
                    });
                    found[index] |= readable && at.is_u64();
                }
            }
        }
    }

    for (index, member) in USAGE_MEMBERS.iter().enumerate() {
        assert!(
            found[index],
            "no recording under {dir:?} carries a readable {}",
            member.join(".")
        );
    }
    println!(
        "{} recordings checked; every usage member the interface names is readable",
        recordings.len()
    );
}
