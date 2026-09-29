//! `mur precompile` stores the compiled form of every WASM artifact file it is given, reports
//! each file in argument order, and a later launch loads what it stored.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    io::Write,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    time::SystemTime,
};

use assert_cmd::Command;
use serde_json::Value;
use tempfile::TempDir;
use zip::{
    write::{FileOptions, SimpleFileOptions},
    CompressionMethod, ZipWriter,
};

const TOOL: &str = "echo-tool";
const VERSION: &str = "0.1.0";

/// A fresh `HOME` and the five files the tests compile: a WASM tool, a native tool, a file that is
/// not a zip, a path that does not exist, and a tool whose `tool.wasm` is not wasm.
struct Files {
    home: TempDir,
    _dir: TempDir,
    echo: PathBuf,
    native: PathBuf,
    garbage: PathBuf,
    missing: PathBuf,
    bad_wasm: PathBuf,
}

impl Files {
    fn new() -> Self {
        let home = TempDir::new().unwrap();
        let dir = TempDir::new().unwrap();
        let echo = common::create_tool_artifact(
            dir.path(),
            TOOL,
            VERSION,
            &common::fixture_path("run/components/echo-tool.wasm"),
        );
        let native = common::create_native_artifact(
            dir.path(),
            "native-echo",
            VERSION,
            "#!/bin/sh\necho ok\n",
            None,
            None,
        );
        let garbage = dir.path().join("garbage.mur.zip");
        fs::write(&garbage, b"not a zip").unwrap();
        let missing = dir.path().join("missing.mur.zip");
        let bad_wasm = dir.path().join("bad-wasm.mur.zip");
        let mut zip = ZipWriter::new(fs::File::create(&bad_wasm).unwrap());
        let options: SimpleFileOptions =
            FileOptions::default().compression_method(CompressionMethod::Deflated);
        zip.start_file("murmur.yaml", options).unwrap();
        zip.write_all(b"name: bad-wasm\nversion: 0.2.0\nruntime: tool\n")
            .unwrap();
        zip.start_file("tool.wasm", options).unwrap();
        zip.write_all(b"not wasm").unwrap();
        zip.finish().unwrap();
        Self {
            home,
            _dir: dir,
            echo,
            native,
            garbage,
            missing,
            bad_wasm,
        }
    }

    fn precompile(&self, args: &[&Path], json: bool) -> std::process::Output {
        let mut command = Command::cargo_bin("mur").unwrap();
        command
            .env("HOME", self.home.path())
            .env_remove("MURMUR_MAX_ARTIFACT_DECOMPRESSED_BYTES")
            .arg("precompile");
        if json {
            command.arg("--json");
        }
        command.args(args).output().unwrap()
    }

    fn compiled(&self) -> PathBuf {
        self.home.path().join(".murmur/compiled")
    }

    fn compiled_names(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.compiled())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    /// The one stored form of the echo tool's payload.
    fn echo_form(&self) -> PathBuf {
        let prefix = format!(
            "{}-",
            murmur_artifact::sha256_hex(&fs::read(&self.echo).unwrap())
        );
        let forms: Vec<PathBuf> = fs::read_dir(self.compiled())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                let name = path.file_name().unwrap().to_str().unwrap();
                name.starts_with(&prefix) && name.ends_with(".cwasm")
            })
            .collect();
        let [form] = &forms[..] else {
            panic!("expected one echo form, found {forms:?}");
        };
        form.clone()
    }
}

fn json_report(output: &std::process::Output) -> Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    let [line] = &lines[..] else {
        panic!("expected one line on stdout, got {stdout:?}");
    };
    serde_json::from_str(line).unwrap()
}

fn inode_and_mtime(path: &Path) -> (u64, SystemTime) {
    let metadata = fs::metadata(path).unwrap();
    (metadata.ino(), metadata.modified().unwrap())
}

#[test]
fn every_file_is_reported_in_order_and_only_the_wasm_tool_is_stored() {
    let files = Files::new();
    let output = files.precompile(
        &[
            &files.echo,
            &files.native,
            &files.garbage,
            &files.missing,
            &files.bad_wasm,
        ],
        true,
    );
    assert_eq!(output.status.code(), Some(1));
    let report = json_report(&output);
    assert_eq!(report["mur_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(report["decompression_ceiling"], 524_288_000u64);
    let artifacts = report["artifacts"].as_array().unwrap();
    let expected = [
        (&files.echo, Some(TOOL), Some(VERSION), "compiled"),
        (
            &files.native,
            Some("native-echo"),
            Some(VERSION),
            "not_wasm",
        ),
        (&files.garbage, None, None, "failed"),
        (&files.missing, None, None, "failed"),
        (&files.bad_wasm, Some("bad-wasm"), Some("0.2.0"), "failed"),
    ];
    assert_eq!(artifacts.len(), expected.len());
    for (artifact, (path, name, version, outcome)) in artifacts.iter().zip(expected) {
        assert_eq!(
            artifact,
            &serde_json::json!({
                "path": path.to_str().unwrap(),
                "name": name,
                "version": version,
                "outcome": outcome,
            })
        );
    }

    let form = files.echo_form();
    let form_name = form.file_name().unwrap().to_str().unwrap().to_string();
    assert_eq!(
        files.compiled_names(),
        [form_name.clone(), format!("{form_name}.sha256")]
    );
    assert_eq!(
        fs::metadata(files.compiled()).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(&form).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn a_stored_form_is_already_stored_and_left_as_it_is() {
    let files = Files::new();
    assert!(files.precompile(&[&files.echo], true).status.success());
    let form = files.echo_form();
    let before = inode_and_mtime(&form);
    for _ in 0..2 {
        let output = files.precompile(&[&files.echo], true);
        assert_eq!(output.status.code(), Some(0));
        assert_eq!(
            json_report(&output)["artifacts"][0]["outcome"],
            "already_stored"
        );
        assert_eq!(inode_and_mtime(&form), before);
    }
}

#[test]
fn the_text_report_is_one_line_per_file() {
    let files = Files::new();
    let output = files.precompile(&[&files.echo, &files.native], false);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "compiled        echo-tool@0.1.0\nnot wasm        native-echo@0.1.0\n"
    );

    let output = files.precompile(&[&files.echo, &files.missing], false);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!(
            "already stored  echo-tool@0.1.0\nfailed          {}\n",
            files.missing.display()
        )
    );
}

#[test]
fn the_command_and_its_flags_are_in_the_help() {
    let help = |args: &[&str]| {
        let output = Command::cargo_bin("mur")
            .unwrap()
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap()
    };
    let own = help(&["precompile", "--help"]);
    assert!(own.contains("--workdir"), "{own}");
    assert!(own.contains("--json"), "{own}");
    let top = help(&["--help"]);
    assert!(
        top.lines()
            .any(|line| line.trim_start().starts_with("precompile ")),
        "{top}"
    );
}

#[test]
fn a_launch_loads_the_form_precompile_stored() {
    let files = Files::new();
    assert!(files.precompile(&[&files.echo], true).status.success());
    let form = files.echo_form();
    let before = inode_and_mtime(&form);

    let project = TempDir::new().unwrap();
    fs::write(
        project.path().join("murmur.yaml"),
        format!(
            "name: capsule\nversion: 0.0.1\nartifacts:\n  - name: {TOOL}\n    version: {VERSION}\n"
        ),
    )
    .unwrap();
    fs::copy(
        common::fixture_path("run/components/capsule-allowlisted.wasm"),
        project.path().join("capsule.wasm"),
    )
    .unwrap();
    common::install_artifact_to_project_with_home(
        project.path(),
        &files.home,
        &files.echo,
        &["--no-precompile"],
    )
    .success();
    assert_eq!(inode_and_mtime(&form), before);

    common::run_capsule(&files.home, &project.path().join("murmur.yaml")).success();
    assert_eq!(
        inode_and_mtime(&form),
        before,
        "the launch compiled the tool instead of loading the stored form"
    );
}
