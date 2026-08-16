//! Zip layout test for native tool artifacts.
//!
//! Verifies the canonical .mur.zip layout for native tools:
//!   murmur.yaml         — at archive root
//!   bin/<tool-name>       — compiled binary with executable permissions
//!
//! No source files, Cargo.toml, target/, or other build artifacts may appear.
//!
//! The binary packed here is the local fixture tool from
//! `tests/fixtures/native-tool/`, built on demand by
//! `common::fixture_native_tool_binary()`. Any executable would satisfy these
//! structural assertions, but using the same fixture the dispatch tests use
//! keeps one native tool in the suite instead of two.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use zip::{
    write::{FileOptions, SimpleFileOptions},
    CompressionMethod, ZipArchive, ZipWriter,
};

const ARTIFACT_NAME: &str = common::FIXTURE_NATIVE_TOOL_NAME;
const ARTIFACT_VERSION: &str = "0.1.0";

/// Create a zip with the canonical native tool layout:
///   murmur.yaml
///   bin/<ARTIFACT_NAME>
fn create_canonical_native_zip(dir: &Path, binary_path: &Path) -> PathBuf {
    let zip_path = dir.join(format!("{ARTIFACT_NAME}-{ARTIFACT_VERSION}.mur.zip"));
    let file = fs::File::create(&zip_path).unwrap();
    let mut zip = ZipWriter::new(file);

    let text_opts: SimpleFileOptions =
        FileOptions::default().compression_method(CompressionMethod::Deflated);
    let exec_opts: SimpleFileOptions = FileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(0o755);

    zip.start_file("murmur.yaml", text_opts).unwrap();
    writeln!(zip, "name: {ARTIFACT_NAME}").unwrap();
    writeln!(zip, "version: \"{ARTIFACT_VERSION}\"").unwrap();
    writeln!(zip, "runtime: tool").unwrap();
    writeln!(zip, "implementation: native").unwrap();

    zip.start_file(format!("bin/{ARTIFACT_NAME}"), exec_opts)
        .unwrap();
    zip.write_all(&fs::read(binary_path).unwrap()).unwrap();

    zip.finish().unwrap();
    zip_path
}

/// Verify the canonical .mur.zip layout for a native tool.
///
/// Asserts:
/// - Exactly two entries: `murmur.yaml` and `bin/<tool-name>`
/// - No source files, Cargo.toml, target/, .cargo/ present
/// - `bin/<tool-name>` has Unix executable permissions (mode includes 0o111)
#[test]
fn native_tool_zip_layout() {
    let Some(binary) = common::fixture_native_tool_binary() else {
        eprintln!("[SKIP] native_tool_zip_layout: fixture native tool binary not available");
        return;
    };

    let dir = tempfile::tempdir().unwrap();
    let zip_path = create_canonical_native_zip(dir.path(), &binary);

    let file = fs::File::open(&zip_path).unwrap();
    let mut archive = ZipArchive::new(file).expect("zip should open");

    // Collect all entry names.
    let names: Vec<String> = (0..archive.len())
        .map(|i| archive.by_index(i).unwrap().name().to_string())
        .collect();

    // Must have exactly two entries.
    assert_eq!(
        names.len(),
        2,
        "expected exactly 2 entries in the zip, got {}: {:?}",
        names.len(),
        names
    );

    // Required entries.
    assert!(
        names.contains(&"murmur.yaml".to_string()),
        "zip must contain murmur.yaml; got: {:?}",
        names
    );
    let bin_entry = format!("bin/{ARTIFACT_NAME}");
    assert!(
        names.contains(&bin_entry),
        "zip must contain {bin_entry}; got: {:?}",
        names
    );

    // Prohibited patterns.
    for name in &names {
        assert!(
            !name.starts_with("src/"),
            "source directory must not appear in zip: {name}"
        );
        assert!(
            !name.starts_with("target/"),
            "target directory must not appear in zip: {name}"
        );
        assert!(
            !name.starts_with(".cargo/"),
            ".cargo directory must not appear in zip: {name}"
        );
        assert!(name != "Cargo.toml", "Cargo.toml must not appear in zip");
    }

    // Binary must have executable permission bits set (Unix mode 0o755 → mode & 0o111 != 0).
    #[cfg(unix)]
    {
        let bin_file = archive.by_name(&bin_entry).unwrap();
        let unix_mode = bin_file.unix_mode().unwrap_or(0);
        assert_ne!(
            unix_mode & 0o111,
            0,
            "bin/{ARTIFACT_NAME} must have executable bits set, got mode {unix_mode:#o}"
        );
    }
}
