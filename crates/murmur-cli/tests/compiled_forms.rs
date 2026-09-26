//! `mur run` keeps the compiled form of every staged WASM component in `~/.murmur/compiled` and
//! loads it on the next launch, and a form it cannot trust is compiled again and rewritten without
//! changing the launch.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    io::Write,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

use assert_cmd::Command;
use murmur_artifact::{read_lockfile, sha256_hex};
use predicates::prelude::*;
use tempfile::TempDir;
use zip::{
    write::{FileOptions, SimpleFileOptions},
    CompressionMethod, ZipWriter,
};

const TOOL_NAME: &str = "echo-tool";
const TOOL_VERSION: &str = "0.1.0";

/// A project running the `capsule-allowlisted.wasm` script capsule with `echo-tool` installed
/// project-locally, and the scratch `HOME` it runs under.
struct Project {
    home: TempDir,
    project: TempDir,
    manifest: PathBuf,
}

impl Project {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let fixture = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let artifact = create_tool_artifact(
            fixture.path(),
            TOOL_NAME,
            TOOL_VERSION,
            &fixture_component("echo-tool.wasm"),
        );
        let manifest = create_project(
            project.path(),
            "capsule-allowlisted.wasm",
            &format!("  - name: {TOOL_NAME}\n    version: {TOOL_VERSION}\n"),
        );
        common::install_artifact_to_project(project.path(), &artifact).success();
        Self {
            home,
            project,
            manifest,
        }
    }

    /// Runs the capsule, requires a clean exit, and returns `out/result.txt` and stderr.
    fn run(&self) -> (String, String) {
        let output = common::run_capsule(&self.home, &self.manifest)
            .success()
            .stdout(predicate::str::contains("status:  ok"))
            .get_output()
            .clone();
        let workdir = common::parse_workdir_from_stdout(&String::from_utf8_lossy(&output.stdout));
        let result = fs::read_to_string(workdir.join("out/result.txt")).unwrap();
        (result, String::from_utf8_lossy(&output.stderr).into_owned())
    }

    fn compiled(&self) -> PathBuf {
        self.home.path().join(".murmur").join("compiled")
    }

    /// The form of `echo-tool`, named by the payload sha256 the lock pins.
    fn tool_form(&self) -> PathBuf {
        let lock = read_lockfile(&self.project.path().join("murmur.lock")).unwrap();
        let sha256 = lock
            .artifact_for(TOOL_NAME)
            .expect("lock entry for echo-tool")
            .sha256
            .any
            .clone()
            .expect("lock pins echo-tool's sha256");
        form_named(&self.compiled(), &sha256)
    }

    /// The form of the capsule component, named by the sha256 of its bytes.
    fn capsule_form(&self) -> PathBuf {
        let sha256 = sha256_hex(&fs::read(self.project.path().join("capsule.wasm")).unwrap());
        form_named(&self.compiled(), &sha256)
    }
}

/// The one `.cwasm` in `dir` named `<sha256>-<16 hex>.cwasm`.
fn form_named(dir: &Path, sha256: &str) -> PathBuf {
    let matching: Vec<PathBuf> = forms(dir)
        .into_iter()
        .filter(|path| {
            let name = path.file_name().unwrap().to_str().unwrap();
            name.strip_prefix(sha256)
                .and_then(|rest| rest.strip_prefix('-'))
                .and_then(|rest| rest.strip_suffix(".cwasm"))
                .is_some_and(|engine| {
                    engine.len() == 16 && engine.bytes().all(|b| b.is_ascii_hexdigit())
                })
        })
        .collect();
    assert_eq!(matching.len(), 1, "one form for {sha256} in {dir:?}");
    matching.into_iter().next().unwrap()
}

/// Every `.cwasm` directly in `dir`, sorted.
fn forms(dir: &Path) -> Vec<PathBuf> {
    let mut forms: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "cwasm"))
        .collect();
    forms.sort();
    forms
}

fn sidecar(form: &Path) -> PathBuf {
    form.with_extension("cwasm.sha256")
}

fn assert_matches_sidecar(form: &Path) {
    assert_eq!(
        fs::read_to_string(sidecar(form)).unwrap().trim(),
        sha256_hex(&fs::read(form).unwrap()),
        "{} does not match its sidecar",
        form.display()
    );
}

fn inode(path: &Path) -> u64 {
    fs::symlink_metadata(path).unwrap().ino()
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
}

fn set_mode(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

#[test]
fn a_second_run_loads_the_forms_the_first_run_wrote() {
    let project = Project::new();
    let (first, _) = project.run();

    let compiled = project.compiled();
    assert!(fs::symlink_metadata(&compiled).unwrap().is_dir());
    assert_eq!(mode(&compiled), 0o700);
    let tool_form = project.tool_form();
    let capsule_form = project.capsule_form();
    assert_eq!(forms(&compiled), {
        let mut expected = vec![tool_form.clone(), capsule_form.clone()];
        expected.sort();
        expected
    });
    for form in [&tool_form, &capsule_form] {
        assert_eq!(mode(form), 0o600, "{}", form.display());
        assert_matches_sidecar(form);
    }
    let inodes = (inode(&tool_form), inode(&capsule_form));

    let (second, _) = project.run();
    assert_eq!(second, first);
    assert_eq!((inode(&tool_form), inode(&capsule_form)), inodes);
}

#[test]
fn a_corrupted_form_is_recompiled_and_rewritten() {
    let project = Project::new();
    let (first, _) = project.run();
    let tool_form = project.tool_form();
    let mut bytes = fs::read(&tool_form).unwrap();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0xff;
    fs::write(&tool_form, &bytes).unwrap();
    let corrupted = inode(&tool_form);

    let (second, _) = project.run();
    assert_eq!(second, first);
    assert_ne!(inode(&tool_form), corrupted);
    assert_matches_sidecar(&tool_form);
}

#[test]
fn a_form_wider_than_owner_only_is_rewritten_owner_only() {
    let project = Project::new();
    let (first, _) = project.run();
    let tool_form = project.tool_form();
    set_mode(&tool_form, 0o644);
    let wide = inode(&tool_form);

    let (second, _) = project.run();
    assert_eq!(second, first);
    assert_eq!(mode(&tool_form), 0o600);
    assert_ne!(inode(&tool_form), wide);
}

#[test]
fn a_symlinked_compiled_dir_is_neither_read_nor_written() {
    let project = Project::new();
    let (first, _) = project.run();
    let compiled = project.compiled();
    let target = project.home.path().join("elsewhere");
    fs::rename(&compiled, &target).unwrap();
    set_mode(&target, 0o755);
    std::os::unix::fs::symlink(&target, &compiled).unwrap();

    let entries = |dir: &Path| -> Vec<(String, u64, Vec<u8>)> {
        let mut entries: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                (
                    path.file_name().unwrap().to_string_lossy().into_owned(),
                    inode(&path),
                    fs::read(&path).unwrap(),
                )
            })
            .collect();
        entries.sort();
        entries
    };
    let before = entries(&target);
    assert_eq!(before.len(), 4, "two forms and their sidecars");

    let (second, _) = project.run();
    assert_eq!(second, first);
    assert!(fs::symlink_metadata(&compiled).unwrap().is_symlink());
    assert_eq!(mode(&target), 0o755);
    assert_eq!(entries(&target), before);
}

#[test]
fn a_widened_compiled_dir_is_narrowed_and_its_forms_rewritten() {
    let project = Project::new();
    let (first, _) = project.run();
    let compiled = project.compiled();
    let (tool_form, capsule_form) = (project.tool_form(), project.capsule_form());
    let inodes = (inode(&tool_form), inode(&capsule_form));
    set_mode(&compiled, 0o755);

    let (second, _) = project.run();
    assert_eq!(second, first);
    assert_eq!(mode(&compiled), 0o700);
    assert_ne!(inode(&tool_form), inodes.0);
    assert_ne!(inode(&capsule_form), inodes.1);
}

#[test]
fn a_home_inside_the_capsule_workdir_gets_no_compiled_forms() {
    let project = Project::new();
    let home = project.project.path().join("workdir").join("home");
    fs::create_dir_all(&home).unwrap();

    Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", &home)
        .env_remove("NEXUS_API_KEY")
        .args([
            "run",
            "--manifest",
            project.manifest.to_str().unwrap(),
            "--verbose",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("status:  ok"));

    assert!(!home.join(".murmur").join("compiled").exists());
}

#[test]
fn an_unusable_compiled_path_never_fails_a_launch() {
    let project = Project::new();
    project.run();
    let compiled = project.compiled();
    fs::remove_dir_all(&compiled).unwrap();
    fs::write(&compiled, b"not a directory").unwrap();

    let (blocked, blocked_stderr) = project.run();
    assert_eq!(fs::read(&compiled).unwrap(), b"not a directory");

    fs::remove_file(&compiled).unwrap();
    let (empty, empty_stderr) = project.run();
    assert_eq!(blocked, empty);
    assert_eq!(
        blocked_stderr.lines().collect::<Vec<_>>(),
        empty_stderr.lines().collect::<Vec<_>>()
    );
}

#[test]
fn concurrent_first_runs_both_succeed() {
    let project = Project::new();
    project.run();
    fs::remove_dir_all(project.compiled()).unwrap();

    let spawn = || {
        std::process::Command::new(assert_cmd::cargo::cargo_bin("mur"))
            .env("HOME", project.home.path())
            .env_remove("NEXUS_API_KEY")
            .args([
                "run",
                "--manifest",
                project.manifest.to_str().unwrap(),
                "--verbose",
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    };
    let (a, b) = (spawn(), spawn());
    for child in [a, b] {
        let output = child.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("status:  ok"),
            "{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    project.run();
    let written = forms(&project.compiled());
    assert!(!written.is_empty());
    for form in &written {
        assert_matches_sidecar(form);
    }
}

fn fixture_component(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("run")
        .join("components")
        .join(name)
}

fn create_project(project_dir: &Path, capsule_fixture: &str, artifacts_yaml: &str) -> PathBuf {
    fs::write(
        project_dir.join("murmur.yaml"),
        format!("name: capsule\nversion: 0.0.1\nartifacts:\n{artifacts_yaml}"),
    )
    .unwrap();
    fs::copy(
        fixture_component(capsule_fixture),
        project_dir.join("capsule.wasm"),
    )
    .unwrap();
    project_dir.join("murmur.yaml")
}

fn create_tool_artifact(
    dir: &Path,
    name: &str,
    version: &str,
    tool_component_path: &Path,
) -> PathBuf {
    let artifact_path = dir.join(format!("{name}-{version}.mur.zip"));
    let file = fs::File::create(&artifact_path).unwrap();
    let mut zip = ZipWriter::new(file);
    let options: SimpleFileOptions =
        FileOptions::default().compression_method(CompressionMethod::Deflated);

    zip.start_file("murmur.yaml", options).unwrap();
    writeln!(zip, "name: {name}").unwrap();
    writeln!(zip, "version: {version}").unwrap();
    writeln!(zip, "runtime: wasm").unwrap();

    zip.start_file("tool.wasm", options).unwrap();
    zip.write_all(&fs::read(tool_component_path).unwrap())
        .unwrap();

    zip.finish().unwrap();
    artifact_path
}
