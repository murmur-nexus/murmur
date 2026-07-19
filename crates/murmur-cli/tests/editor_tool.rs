//! Integration tests for the editor-tool native artifact.
//!
//! All five tests invoke the murmur-tool-editor binary directly (bypassing the capsule
//! runtime) — the same pattern used by the slice2_* tests in git_tool.rs.  The binary
//! reads a JSON envelope from stdin and writes a JSON ToolResult to stdout.  Each test
//! sets the binary's CWD to a temp directory so that relative paths in operation
//! payloads resolve correctly.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use serde_json::{json, Value};
use tempfile::TempDir;

// ── binary helpers ────────────────────────────────────────────────────────────

/// Locate or compile the murmur-tool-editor binary from default-artifacts.
/// Returns None if the workspace cannot be found or the build fails.
fn editor_tool_binary() -> Option<PathBuf> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // crates/murmur-cli → murmur/ → _murmur/ → default-artifacts/
    let default_artifacts = manifest_dir.ancestors().nth(3)?.join("default-artifacts");

    if !default_artifacts.exists() {
        eprintln!("[editor_tool] default-artifacts not found at {default_artifacts:?}");
        return None;
    }

    let binary_path = default_artifacts
        .join("target")
        .join("release")
        .join("murmur-tool-editor");

    if !binary_path.exists() {
        eprintln!("[editor_tool] binary not found, building...");
        let status = Command::new("cargo")
            .args(["build", "-p", "murmur-tool-editor", "--release"])
            .current_dir(&default_artifacts)
            .status()
            .ok()?;
        if !status.success() {
            eprintln!("[editor_tool] cargo build failed");
            return None;
        }
    }

    Some(binary_path)
}

/// Invoke the editor-tool binary from `workdir` with `data` as the operation payload.
/// The envelope sent on stdin is `{ "data": <data>, "log_path": null }`.
fn invoke(binary: &Path, data: Value, workdir: &Path) -> Value {
    let envelope = json!({ "data": data, "log_path": null });
    let stdin_bytes = serde_json::to_vec(&envelope).unwrap();

    let mut child = Command::new(binary)
        .current_dir(workdir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("binary should spawn");

    child.stdin.take().unwrap().write_all(&stdin_bytes).unwrap();
    let out = child.wait_with_output().unwrap();

    serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
        json!({ "ok": false, "message": "binary produced invalid JSON output",
                "raw": String::from_utf8_lossy(&out.stdout).to_string() })
    })
}

// ── tests ─────────────────────────────────────────────────────────────────────

/// Test 1: read_file returns the exact file content and a byte-count summary.
#[test]
fn editor_read_file_success() {
    let binary = match editor_tool_binary() {
        Some(b) => b,
        None => {
            eprintln!("[SKIP] editor_read_file_success: binary not available");
            return;
        }
    };

    let workdir = TempDir::new().unwrap();
    fs::write(workdir.path().join("hello.txt"), "hello world\n").unwrap();

    let result = invoke(
        &binary,
        json!({ "operation": "read_file", "path": "hello.txt" }),
        workdir.path(),
    );

    assert_eq!(result["ok"], true, "read_file should succeed; got: {result:?}");
    assert_eq!(
        result["data"]["content"],
        "hello world\n",
        "data.content must equal the file bytes exactly"
    );
    let summary = result["summary"].as_str().unwrap_or("");
    assert!(
        summary.contains("bytes"),
        "summary should contain byte count; got: {summary}"
    );
}

/// Test 2: write_file creates missing intermediate directories and writes exact content.
#[test]
fn editor_write_file_creates_dirs() {
    let binary = match editor_tool_binary() {
        Some(b) => b,
        None => {
            eprintln!("[SKIP] editor_write_file_creates_dirs: binary not available");
            return;
        }
    };

    let workdir = TempDir::new().unwrap();
    let nested_path = "sub/dir/new.txt";

    let result = invoke(
        &binary,
        json!({ "operation": "write_file", "path": nested_path, "content": "created\n" }),
        workdir.path(),
    );

    assert_eq!(result["ok"], true, "write_file should succeed; got: {result:?}");
    let summary = result["summary"].as_str().unwrap_or("");
    assert!(
        summary.contains("bytes written"),
        "summary should mention bytes written; got: {summary}"
    );

    let written = workdir.path().join("sub").join("dir").join("new.txt");
    assert!(written.exists(), "file should have been created at {written:?}");
    assert_eq!(
        fs::read_to_string(&written).unwrap(),
        "created\n",
        "file content must match what was passed to write_file"
    );
}

/// Test 3: replace_in_file replaces ALL occurrences and reports the correct count.
#[test]
fn editor_replace_in_file_success() {
    let binary = match editor_tool_binary() {
        Some(b) => b,
        None => {
            eprintln!("[SKIP] editor_replace_in_file_success: binary not available");
            return;
        }
    };

    let workdir = TempDir::new().unwrap();
    fs::write(workdir.path().join("code.txt"), "foo foo bar\n").unwrap();

    let result = invoke(
        &binary,
        json!({
            "operation": "replace_in_file",
            "path": "code.txt",
            "old_string": "foo",
            "new_string": "baz",
        }),
        workdir.path(),
    );

    assert_eq!(result["ok"], true, "replace_in_file should succeed; got: {result:?}");
    assert_eq!(result["data"]["count"], 2, "count must equal number of occurrences replaced");
    let summary = result["summary"].as_str().unwrap_or("");
    assert!(summary.contains("2"), "summary should mention count; got: {summary}");

    let after = fs::read_to_string(workdir.path().join("code.txt")).unwrap();
    assert_eq!(after, "baz baz bar\n", "file must contain all replacements");
}

/// Test 4: replace_in_file returns string_not_found when old_string is absent.
/// The file must be byte-for-byte unchanged after the failed call.
#[test]
fn editor_replace_in_file_string_not_found() {
    let binary = match editor_tool_binary() {
        Some(b) => b,
        None => {
            eprintln!("[SKIP] editor_replace_in_file_string_not_found: binary not available");
            return;
        }
    };

    let workdir = TempDir::new().unwrap();
    let original = "hello world\n";
    fs::write(workdir.path().join("stable.txt"), original).unwrap();

    let result = invoke(
        &binary,
        json!({
            "operation": "replace_in_file",
            "path": "stable.txt",
            "old_string": "xyz_not_present",
            "new_string": "abc",
        }),
        workdir.path(),
    );

    assert_eq!(result["ok"], false, "should return error when string absent; got: {result:?}");
    assert_eq!(
        result["error_kind"],
        "string_not_found",
        "error_kind must be string_not_found; got: {result:?}"
    );

    // File must be byte-for-byte unchanged.
    let after = fs::read_to_string(workdir.path().join("stable.txt")).unwrap();
    assert_eq!(
        after, original,
        "file must be unchanged when string_not_found"
    );
}

/// Test 5: find_in_files returns correct line entries for matching files;
/// zero matches is a success result with an empty list, not an error.
#[test]
fn editor_find_in_files_regex() {
    let binary = match editor_tool_binary() {
        Some(b) => b,
        None => {
            eprintln!("[SKIP] editor_find_in_files_regex: binary not available");
            return;
        }
    };

    let workdir = TempDir::new().unwrap();
    fs::write(workdir.path().join("match.txt"), "hello world\n").unwrap();
    fs::write(workdir.path().join("no_match.txt"), "goodbye\n").unwrap();

    // 5a: Pattern that matches one file.
    let result = invoke(
        &binary,
        json!({
            "operation": "find_in_files",
            "pattern": "hello",
            "dir": ".",
            "recursive": true,
        }),
        workdir.path(),
    );

    assert_eq!(result["ok"], true, "find_in_files should succeed; got: {result:?}");
    let matches = result["data"]["matches"].as_array().expect("matches must be an array");
    assert_eq!(matches.len(), 1, "should find exactly 1 match; got: {matches:?}");

    let m = &matches[0];
    assert_eq!(m["path"], "match.txt", "match path must be relative to dir");
    assert_eq!(m["line"], 1, "line number must be 1-based");
    assert_eq!(m["text"], "hello world", "text must be the full line without trailing newline");

    // 5b: Pattern that matches nothing — ok: true, empty list.
    let result_empty = invoke(
        &binary,
        json!({
            "operation": "find_in_files",
            "pattern": "xyz_no_match_anywhere",
            "dir": ".",
            "recursive": true,
        }),
        workdir.path(),
    );

    assert_eq!(
        result_empty["ok"], true,
        "zero matches must be ok:true, not an error; got: {result_empty:?}"
    );
    let empty_matches = result_empty["data"]["matches"]
        .as_array()
        .expect("matches must be an array even when empty");
    assert!(empty_matches.is_empty(), "matches list must be empty; got: {empty_matches:?}");
}
