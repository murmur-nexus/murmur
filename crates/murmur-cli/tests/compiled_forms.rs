//! `mur run` keeps the compiled form of every staged WASM component in `~/.murmur/compiled` and
//! loads it on the next launch, and a form it cannot trust is compiled again and rewritten without
//! changing the launch. `mur install` writes the same forms for the artifacts it installs, so the
//! first launch loads them, and in a project keeps the host facts of the manifest's
//! `capabilities.shell.allow` binaries, so that launch reports every one kept.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs::{self, FileTimes},
    io::Write,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use assert_cmd::Command;
use murmur_artifact::{read_lockfile, sha256_hex};
use predicates::prelude::*;
use serde_json::{json, Value};
use tempfile::TempDir;
use zip::{
    write::{FileOptions, SimpleFileOptions},
    CompressionMethod, ZipWriter,
};

/// Full name of the tool every scenario installs from a `.mur.zip` file, so in the `local` scope.
const TOOL_NAME: &str = "local/echo-tool";
/// The bare part of [`TOOL_NAME`]: the name the zip is packed under and its store files carry.
const TOOL_BARE: &str = "echo-tool";
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

    /// The payload sha256 the lock pins for the installed artifact `name`.
    fn artifact_sha256(&self, name: &str) -> String {
        let lock = read_lockfile(&self.project.path().join("murmur.lock")).unwrap();
        lock.artifact_for(name)
            .unwrap_or_else(|| panic!("lock entry for {name}"))
            .sha256
            .any
            .clone()
            .unwrap_or_else(|| panic!("lock pins {name}'s sha256"))
    }

    /// The payload sha256 the lock pins for `echo-tool`.
    fn tool_sha256(&self) -> String {
        self.artifact_sha256(TOOL_NAME)
    }

    /// The form of `echo-tool`, named by the payload sha256 the lock pins.
    fn tool_form(&self) -> PathBuf {
        self.artifact_form(TOOL_NAME)
    }

    /// The form of the installed artifact `name`, named by the payload sha256 the lock pins.
    fn artifact_form(&self, name: &str) -> PathBuf {
        form_named(&self.compiled(), &self.artifact_sha256(name))
    }

    /// Every form in [`Self::compiled`] keyed on `echo-tool`'s payload, under any engine key.
    fn tool_forms(&self) -> Vec<PathBuf> {
        let prefix = format!("{}-", self.tool_sha256());
        if !self.compiled().exists() {
            return Vec::new();
        }
        forms(&self.compiled())
            .into_iter()
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(&prefix))
            })
            .collect()
    }

    /// Runs the capsule under `MURMUR_MAX_ARTIFACT_DECOMPRESSED_BYTES=ceiling`, requires it to
    /// fail, and returns stderr.
    fn run_failing_under_ceiling(&self, ceiling: &str) -> String {
        let output = common::run_capsule_with_env(
            &self.home,
            &self.manifest,
            &[("MURMUR_MAX_ARTIFACT_DECOMPRESSED_BYTES", ceiling)],
        )
        .failure()
        .get_output()
        .clone();
        String::from_utf8_lossy(&output.stderr).into_owned()
    }

    /// Declares the WASM tool with full name `name`, built from the `fixture` component, and
    /// installs it project-locally.
    fn add_tool(&self, name: &str, fixture: &str) {
        let dir = tempfile::tempdir().unwrap();
        let artifact =
            create_tool_artifact(dir.path(), name, TOOL_VERSION, &fixture_component(fixture));
        let mut manifest = fs::read_to_string(&self.manifest).unwrap();
        manifest.push_str(&format!("  - name: {name}\n    version: {TOOL_VERSION}\n"));
        fs::write(&self.manifest, manifest).unwrap();
        common::install_artifact_to_project(self.project.path(), &artifact).success();
        // A local-file install pins nothing, and a lock that lacks a declared artifact refuses
        // the run. With no lock, the next `mur run` pins every declared artifact after staging.
        fs::remove_file(self.project.path().join("murmur.lock")).unwrap();
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
        sidecar_line(form, 0),
        sha256_hex(&fs::read(form).unwrap()),
        "{} does not match its sidecar",
        form.display()
    );
}

/// Line `index` of `form`'s sidecar, trimmed, or `""` when it has no such line.
fn sidecar_line(form: &Path, index: usize) -> String {
    fs::read_to_string(sidecar(form))
        .unwrap()
        .lines()
        .nth(index)
        .unwrap_or_default()
        .trim()
        .to_owned()
}

/// The stamp mur records on sidecar line 2: device, inode, size, owner, mode, mtime and ctime.
fn stamp(path: &Path) -> String {
    let metadata = fs::metadata(path).unwrap();
    format!(
        "{} {} {} {} {:o} {}.{:09} {}.{:09}",
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.uid(),
        metadata.mode(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ctime(),
        metadata.ctime_nsec(),
    )
}

/// Overwrites line 1 of `form`'s sidecar with 64 zeros in place, keeping line 2 and the file's
/// inode and mode.
fn break_recorded_sha256(form: &Path) {
    let stamp = sidecar_line(form, 1);
    assert!(
        !stamp.is_empty(),
        "no stamp recorded for {}",
        form.display()
    );
    fs::write(sidecar(form), format!("{}\n{stamp}\n", "0".repeat(64))).unwrap();
}

/// Inverts the middle byte of `form` with a positioned write, then sets its atime and mtime back
/// to what they were, so only its ctime shows the change.
fn flip_middle_byte_keeping_times(form: &Path) {
    use std::os::unix::fs::FileExt;

    let earlier = fs::metadata(form).unwrap();
    let bytes = fs::read(form).unwrap();
    let middle = bytes.len() / 2;
    let file = fs::OpenOptions::new().write(true).open(form).unwrap();
    file.write_all_at(&[!bytes[middle]], middle as u64).unwrap();
    file.set_times(
        FileTimes::new()
            .set_accessed(earlier.accessed().unwrap())
            .set_modified(earlier.modified().unwrap()),
    )
    .unwrap();
    drop(file);
    let later = fs::metadata(form).unwrap();
    assert_eq!(later.ino(), earlier.ino());
    assert_eq!(later.len(), earlier.len());
    assert_eq!(later.modified().unwrap(), earlier.modified().unwrap());
    assert_ne!(fs::read(form).unwrap(), bytes);
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

const DAY: Duration = Duration::from_secs(24 * 60 * 60);
const HOUR: Duration = Duration::from_secs(60 * 60);

/// Writes `name` into `dir` at `0600`, with its atime and mtime `age` ago.
fn plant(dir: &Path, name: &str, age: Duration) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, name).unwrap();
    set_mode(&path, 0o600);
    let time = SystemTime::now() - age;
    fs::File::open(&path)
        .unwrap()
        .set_times(FileTimes::new().set_accessed(time).set_modified(time))
        .unwrap();
    path
}

/// Plants a form of a 64-hex key under `engine` and its sidecar, both `age` old.
fn plant_set(dir: &Path, key: char, engine: &str, age: Duration) -> [PathBuf; 2] {
    let form = format!("{}-{engine}.cwasm", key.to_string().repeat(64));
    [
        plant(dir, &format!("{form}.sha256"), age),
        plant(dir, &form, age),
    ]
}

/// The engine key a form name carries.
fn engine_key(form: &Path) -> String {
    let name = form.file_name().unwrap().to_str().unwrap();
    name.strip_suffix(".cwasm").unwrap()[65..].to_string()
}

/// Every entry directly in `dir` as (name, inode, size, mtime), sorted.
fn snapshot(dir: &Path) -> Vec<(String, u64, u64, SystemTime)> {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            let metadata = fs::symlink_metadata(&path).unwrap();
            (
                path.file_name().unwrap().to_string_lossy().into_owned(),
                metadata.ino(),
                metadata.len(),
                metadata.modified().unwrap(),
            )
        })
        .collect();
    entries.sort();
    entries
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
fn a_launch_that_stores_a_form_prunes_a_stale_set_and_keeps_the_current_one() {
    let project = Project::new();
    project.run();
    let compiled = project.compiled();
    let (tool_form, capsule_form) = (project.tool_form(), project.capsule_form());
    let current = [
        sidecar(&tool_form),
        tool_form.clone(),
        sidecar(&capsule_form),
        capsule_form.clone(),
    ];
    let inodes: Vec<u64> = current.iter().map(|path| inode(path)).collect();
    let engine = engine_key(&tool_form);
    let (stale_engine, fresh_engine) = ("0000000000000000", "1111111111111111");
    assert!(engine != stale_engine && engine != fresh_engine);

    let stale_set = plant_set(&compiled, 'a', stale_engine, 31 * DAY);
    let fresh_set = plant_set(&compiled, 'b', fresh_engine, DAY);
    let temp_name = |pid: u32| {
        format!(
            ".{}.{pid}.0192d3f4a5b67c8d9e0f1a2b3c4d5e6f.tmp",
            tool_form.file_name().unwrap().to_string_lossy()
        )
    };
    let crashed_temp = plant(&compiled, &temp_name(4242), 2 * HOUR);
    let live_temp = plant(&compiled, &temp_name(4243), Duration::ZERO);

    project.add_tool("local/config-echo", "config-echo.wasm");
    project.run();

    for path in stale_set.iter().chain([&crashed_temp]) {
        assert!(!path.exists(), "{} survived", path.display());
    }
    for path in fresh_set.iter().chain([&live_temp]) {
        assert!(path.exists(), "{} was removed", path.display());
    }
    assert_eq!(
        current.iter().map(|path| inode(path)).collect::<Vec<_>>(),
        inodes
    );
    let new_form = project.artifact_form("local/config-echo");
    assert_matches_sidecar(&new_form);
    assert_eq!(forms(&compiled), {
        let mut expected = vec![tool_form, capsule_form, new_form, fresh_set[1].clone()];
        expected.sort();
        expected
    });
}

#[test]
fn a_warm_launch_prunes_nothing() {
    let project = Project::new();
    project.run();
    let compiled = project.compiled();
    plant_set(&compiled, 'a', "0000000000000000", 31 * DAY);
    plant(
        &compiled,
        ".a.cwasm.4242.0192d3f4a5b67c8d9e0f1a2b3c4d5e6f.tmp",
        2 * HOUR,
    );
    let before = snapshot(&compiled);

    project.run();
    assert_eq!(snapshot(&compiled), before);
}

#[test]
fn deleting_the_compiled_dir_between_runs_is_safe() {
    let project = Project::new();
    let (first, first_stderr) = project.run();
    let compiled = project.compiled();
    fs::remove_dir_all(&compiled).unwrap();

    let (second, second_stderr) = project.run();
    assert_eq!(second, first);
    assert_eq!(
        second_stderr.lines().collect::<Vec<_>>(),
        first_stderr.lines().collect::<Vec<_>>()
    );
    assert_eq!(mode(&compiled), 0o700);
    let written = forms(&compiled);
    assert_eq!(written.len(), 2);
    for form in &written {
        assert_matches_sidecar(form);
    }
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
    let tool_form_name = project.tool_form().file_name().unwrap().to_owned();
    plant(&target, "stale.cwasm", 31 * DAY);
    plant(
        &target,
        &format!(
            ".{}.4242.0192d3f4a5b67c8d9e0f1a2b3c4d5e6f.tmp",
            tool_form_name.to_string_lossy()
        ),
        2 * HOUR,
    );
    let before = entries(&target);
    assert_eq!(
        before.len(),
        6,
        "two forms, their sidecars, a stale entry and a temp file"
    );

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
    // The narrowing deletes each form before writing its replacement, and a filesystem may hand
    // the freed inode number straight to the new file. A link held outside the directory keeps
    // the old inode allocated, so a replacement cannot carry its number.
    let held = project.home.path().join("held");
    fs::create_dir(&held).unwrap();
    fs::hard_link(&tool_form, held.join("tool")).unwrap();
    fs::hard_link(&capsule_form, held.join("capsule")).unwrap();
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
fn a_lowered_decompression_ceiling_is_enforced_on_a_warm_launch() {
    let project = Project::new();
    let (first, _) = project.run();
    let tool_form = project.tool_form();
    let stored = inode(&tool_form);

    let warm = project.run_failing_under_ceiling("60000");
    assert!(warm.contains("E-IO-003"), "{warm}");
    assert!(
        warm.contains("zip entry 'tool.wasm' exceeds the 60000-byte decompression ceiling"),
        "{warm}"
    );
    assert_eq!(project.tool_forms(), vec![tool_form.clone()]);

    let aside = project.home.path().join("compiled-aside");
    fs::rename(project.compiled(), &aside).unwrap();
    let cold = project.run_failing_under_ceiling("60000");
    assert_eq!(
        cold.lines().collect::<Vec<_>>(),
        warm.lines().collect::<Vec<_>>()
    );
    assert_eq!(project.tool_forms(), Vec::<PathBuf>::new());

    if project.compiled().exists() {
        fs::remove_dir_all(project.compiled()).unwrap();
    }
    fs::rename(&aside, project.compiled()).unwrap();
    let (restored, _) = project.run();
    assert_eq!(restored, first);
    assert_eq!(inode(&tool_form), stored);
}

#[test]
fn concurrent_first_runs_both_succeed() {
    let project = Project::new();
    project.run();
    fs::remove_dir_all(project.compiled()).unwrap();

    let spawn = || {
        std::process::Command::new(assert_cmd::cargo::cargo_bin("mur"))
            .env("HOME", project.home.path())
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

#[test]
fn a_stored_form_records_its_digest_and_stamp() {
    let project = Project::new();
    project.run();
    for form in [project.tool_form(), project.capsule_form()] {
        let recorded = fs::read_to_string(sidecar(&form)).unwrap();
        assert_eq!(
            recorded,
            format!(
                "{}\n{}\n",
                sha256_hex(&fs::read(&form).unwrap()),
                stamp(&form)
            ),
            "{}",
            form.display()
        );
    }
}

#[test]
fn a_warm_run_trusts_an_unchanged_stamp_without_hashing() {
    let project = Project::new();
    let (first, _) = project.run();
    let forms = [project.tool_form(), project.capsule_form()];
    for form in &forms {
        break_recorded_sha256(form);
        assert_eq!(mode(&sidecar(form)), 0o600);
    }
    let inodes: Vec<u64> = forms.iter().map(|form| inode(form)).collect();

    let (second, _) = project.run();
    assert_eq!(second, first);
    assert_eq!(
        forms.iter().map(|form| inode(form)).collect::<Vec<_>>(),
        inodes
    );
}

#[test]
fn a_flipped_byte_with_its_times_restored_is_rebuilt() {
    let project = Project::new();
    let (first, _) = project.run();
    let tool_form = project.tool_form();
    flip_middle_byte_keeping_times(&tool_form);
    let flipped = inode(&tool_form);

    let (second, _) = project.run();
    assert_eq!(second, first);
    assert_ne!(inode(&tool_form), flipped);
    assert_matches_sidecar(&tool_form);
    assert_eq!(sidecar_line(&tool_form, 1), stamp(&tool_form));
}

#[test]
fn a_touch_or_narrowing_chmod_is_rehashed_once_and_not_rebuilt() {
    let project = Project::new();
    let (first, _) = project.run();
    let (tool_form, capsule_form) = (project.tool_form(), project.capsule_form());
    let forms = [&tool_form, &capsule_form];
    fs::File::open(&tool_form)
        .unwrap()
        .set_times(FileTimes::new().set_modified(SystemTime::now()))
        .unwrap();
    set_mode(&capsule_form, 0o400);
    let form_inodes: Vec<u64> = forms.iter().map(|form| inode(form)).collect();
    let sidecar_inodes = |forms: &[&PathBuf]| -> Vec<u64> {
        forms.iter().map(|form| inode(&sidecar(form))).collect()
    };
    let stamped = sidecar_inodes(&forms);

    let (second, _) = project.run();
    assert_eq!(second, first);
    assert_eq!(
        forms.iter().map(|form| inode(form)).collect::<Vec<_>>(),
        form_inodes
    );
    assert_eq!(mode(&capsule_form), 0o400);
    let restamped = sidecar_inodes(&forms);
    for (form, (before, after)) in forms.iter().zip(stamped.iter().zip(&restamped)) {
        assert_ne!(before, after, "{} was not restamped", form.display());
        assert_eq!(sidecar_line(form, 1), stamp(form));
    }

    let (third, _) = project.run();
    assert_eq!(third, first);
    assert_eq!(sidecar_inodes(&forms), restamped);
}

#[test]
fn a_digest_only_sidecar_is_stamped_on_the_next_run() {
    let project = Project::new();
    let (first, _) = project.run();
    let forms = [project.tool_form(), project.capsule_form()];
    for form in &forms {
        fs::write(sidecar(form), sidecar_line(form, 0)).unwrap();
        assert_eq!(sidecar_line(form, 1), "");
    }
    let inodes: Vec<u64> = forms.iter().map(|form| inode(form)).collect();

    let (second, _) = project.run();
    assert_eq!(second, first);
    assert_eq!(
        forms.iter().map(|form| inode(form)).collect::<Vec<_>>(),
        inodes
    );
    for form in &forms {
        assert_matches_sidecar(form);
        assert_eq!(sidecar_line(form, 1), stamp(form));
    }
}

/// A [`Project`] whose `echo-tool` is installed from a local `.mur.zip` under the project's own
/// scratch `HOME`, so install precompiles unless told not to.
struct Installed {
    project: Project,
    /// The `.mur.zip` installed, kept so a repeat install reads the same path.
    artifact: PathBuf,
    /// Stdout and stderr of the first install.
    first_output: (String, String),
    _fixture: TempDir,
}

impl Installed {
    /// Installs `echo-tool` with `extra_args` after the path, and requires a clean exit.
    fn new(extra_args: &[&str]) -> Self {
        let fixture = tempfile::tempdir().unwrap();
        let artifact = create_tool_artifact(
            fixture.path(),
            TOOL_NAME,
            TOOL_VERSION,
            &fixture_component("echo-tool.wasm"),
        );
        Self::from_artifact(fixture, artifact, extra_args)
    }

    /// Installs `artifact` as `echo-tool` into a fresh project and `HOME`.
    fn from_artifact(fixture: TempDir, artifact: PathBuf, extra_args: &[&str]) -> Self {
        let project = tempfile::tempdir().unwrap();
        let manifest = create_project(
            project.path(),
            "capsule-allowlisted.wasm",
            &format!("  - name: {TOOL_NAME}\n    version: {TOOL_VERSION}\n"),
        );
        let mut installed = Self {
            project: Project {
                home: tempfile::tempdir().unwrap(),
                project,
                manifest,
            },
            artifact,
            first_output: Default::default(),
            _fixture: fixture,
        };
        installed.first_output = installed.install(extra_args);
        installed
    }

    /// Runs `mur install <artifact>` again with `extra_args`, requires a clean exit, and returns
    /// stdout and stderr.
    fn install(&self, extra_args: &[&str]) -> (String, String) {
        output(
            common::install_artifact_to_project_with_home(
                self.project.project.path(),
                &self.project.home,
                &self.artifact,
                extra_args,
            )
            .success(),
        )
    }

    /// The payload sha256 of the installed `.mur.zip`, which names its form before any run has
    /// pinned it in `murmur.lock`.
    fn payload_sha256(&self) -> String {
        sha256_hex(&fs::read(&self.artifact).unwrap())
    }

    /// The form install wrote for `echo-tool`.
    fn tool_form(&self) -> PathBuf {
        form_named(&self.project.compiled(), &self.payload_sha256())
    }
}

/// Stdout and stderr of a finished command.
fn output(assert: assert_cmd::assert::Assert) -> (String, String) {
    let output = assert.get_output();
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// What `mur install --no-precompile <artifact>` prints into a fresh project and `HOME`.
fn no_precompile_output(artifact: &Path) -> (String, String) {
    fresh_install_output(artifact, &["--no-precompile"])
}

/// What `mur install <artifact>` with `extra_args` prints into a fresh project and `HOME`.
fn fresh_install_output(artifact: &Path, extra_args: &[&str]) -> (String, String) {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    create_project(
        project.path(),
        "capsule-allowlisted.wasm",
        &format!("  - name: {TOOL_NAME}\n    version: {TOOL_VERSION}\n"),
    );
    output(
        common::install_artifact_to_project_with_home(project.path(), &home, artifact, extra_args)
            .success(),
    )
}

/// The inode and mtime of `path`.
fn identity(path: &Path) -> (u64, SystemTime) {
    let metadata = fs::symlink_metadata(path).unwrap();
    (metadata.ino(), metadata.modified().unwrap())
}

#[test]
fn install_precompiles_so_the_first_run_compiles_no_installed_artifact() {
    let installed = Installed::new(&[]);
    assert_eq!(
        installed.first_output,
        no_precompile_output(&installed.artifact),
        "install printed something --no-precompile does not"
    );

    let compiled = installed.project.compiled();
    assert_eq!(mode(&compiled), 0o700);
    let tool_form = installed.tool_form();
    assert_eq!(forms(&compiled), vec![tool_form.clone()]);
    assert_eq!(mode(&tool_form), 0o600);
    assert_eq!(mode(&sidecar(&tool_form)), 0o600);
    assert_eq!(
        sidecar_line(&tool_form, 0),
        sha256_hex(&fs::read(&tool_form).unwrap())
    );
    let before = (identity(&tool_form), identity(&sidecar(&tool_form)));

    let (result, _) = installed.project.run();
    assert!(!result.is_empty());
    assert_eq!(
        (identity(&tool_form), identity(&sidecar(&tool_form))),
        before,
        "the first run rewrote the form install wrote"
    );
    let mut expected = vec![tool_form, installed.project.capsule_form()];
    expected.sort();
    assert_eq!(forms(&compiled), expected);
    assert_eq!(installed.project.tool_sha256(), installed.payload_sha256());
}

#[test]
fn no_precompile_writes_no_form_and_the_first_run_compiles() {
    let installed = Installed::new(&["--no-precompile"]);
    let compiled = installed.project.compiled();
    assert!(!compiled.exists(), "{} was created", compiled.display());

    let (stdout, stderr) = installed.first_output.clone();
    assert_eq!(
        stdout,
        format!(
            "Installed {TOOL_NAME}@{TOOL_VERSION} from {}\n",
            installed.artifact.display()
        )
    );
    assert_eq!(
        fresh_install_output(&installed.artifact, &[]),
        (stdout, stderr)
    );
    assert!(!compiled.exists(), "{} was created", compiled.display());

    installed.project.run();
    assert_matches_sidecar(&installed.tool_form());

    Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", installed.project.home.path())
        .args(["install", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--no-precompile"));
}

#[test]
fn a_repeat_install_with_every_form_present_compiles_nothing() {
    let installed = Installed::new(&[]);
    let compiled = installed.project.compiled();
    let before = snapshot(&compiled);

    assert_eq!(installed.install(&[]), installed.first_output);
    assert_eq!(snapshot(&compiled), before);
}

#[test]
fn a_manifest_install_fills_in_missing_forms_for_cached_artifacts() {
    let home = tempfile::tempdir().unwrap();
    let fixture = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let artifact = create_tool_artifact(
        fixture.path(),
        TOOL_NAME,
        TOOL_VERSION,
        &fixture_component("echo-tool.wasm"),
    );
    let sha256 = sha256_hex(&fs::read(&artifact).unwrap());
    common::publish_local(&home, &artifact).success();
    create_project(
        project.path(),
        "capsule-allowlisted.wasm",
        &format!("  - name: {TOOL_NAME}\n    version: {TOOL_VERSION}\n"),
    );
    let install = |extra_args: &[&str]| {
        output(
            Command::cargo_bin("mur")
                .unwrap()
                .env("HOME", home.path())
                .current_dir(project.path())
                .arg("install")
                .args(extra_args)
                .assert()
                .success(),
        )
    };
    let compiled = home.path().join(".murmur").join("compiled");

    install(&["--no-precompile"]);
    let lock = read_lockfile(&project.path().join("murmur.lock")).unwrap();
    assert_eq!(
        lock.artifact_for(TOOL_NAME).unwrap().sha256.any.as_deref(),
        Some(sha256.as_str())
    );
    assert!(project
        .path()
        .join(format!(
            ".murmur/artifacts/{TOOL_NAME}/{TOOL_VERSION}/{TOOL_BARE}-{TOOL_VERSION}.mur.zip"
        ))
        .is_file());
    assert!(!compiled.exists(), "{} was created", compiled.display());

    let warmed = install(&[]);
    let form = form_named(&compiled, &sha256);
    assert_matches_sidecar(&form);
    assert_eq!(install(&["--no-precompile"]), warmed);

    // A form under another engine key is not this build's: it stays, and the current one is
    // written beside it.
    let stale = compiled.join(format!("{sha256}-0000000000000000.cwasm"));
    fs::rename(&form, &stale).unwrap();
    fs::rename(sidecar(&form), sidecar(&stale)).unwrap();
    assert_eq!(install(&[]), warmed);
    assert_matches_sidecar(&form);
    assert!(stale.is_file());
}

#[test]
fn a_failed_precompile_never_fails_the_install() {
    // An unusable compiled-forms path.
    let blocked = Installed::new(&["--no-precompile"]);
    let murmur_home = blocked.project.home.path().join(".murmur");
    let compiled = blocked.project.compiled();
    fs::create_dir_all(&murmur_home).unwrap();
    set_mode(&murmur_home, 0o700);
    fs::write(&compiled, b"not a directory").unwrap();
    assert_eq!(
        blocked.install(&[]),
        no_precompile_output(&blocked.artifact)
    );
    assert_eq!(fs::read(&compiled).unwrap(), b"not a directory");
    blocked.project.run();
    assert_eq!(fs::read(&compiled).unwrap(), b"not a directory");

    // A root wasm that does not compile.
    let fixture = tempfile::tempdir().unwrap();
    let artifact = fixture
        .path()
        .join(format!("{TOOL_BARE}-{TOOL_VERSION}.mur.zip"));
    let mut zip = ZipWriter::new(fs::File::create(&artifact).unwrap());
    let options: SimpleFileOptions =
        FileOptions::default().compression_method(CompressionMethod::Deflated);
    zip.start_file("murmur.yaml", options).unwrap();
    write!(
        zip,
        "name: {TOOL_BARE}\nversion: {TOOL_VERSION}\nruntime: tool\n"
    )
    .unwrap();
    zip.start_file("tool.wasm", options).unwrap();
    zip.write_all(b"not wasm").unwrap();
    zip.finish().unwrap();
    let expected = no_precompile_output(&artifact);

    let broken = Installed::from_artifact(fixture, artifact, &[]);
    assert_eq!(broken.first_output, expected);
    let compiled = broken.project.compiled();
    assert!(
        !compiled.exists() || forms(&compiled).is_empty(),
        "a form was stored for a root wasm that does not compile"
    );
}

#[test]
fn install_written_forms_are_checked_like_launch_written_ones() {
    // A byte flipped in place, with its times restored, is rebuilt by the first run.
    let flipped = Installed::new(&[]);
    let form = flipped.tool_form();
    let written = inode(&form);
    flip_middle_byte_keeping_times(&form);
    flipped.project.run();
    assert_ne!(inode(&form), written, "the flipped form was not rebuilt");
    assert_matches_sidecar(&form);

    // A form readable beyond its owner is rewritten owner-only.
    let widened = Installed::new(&[]);
    let form = widened.tool_form();
    set_mode(&form, 0o644);
    widened.project.run();
    assert_eq!(mode(&form), 0o600);
    assert_matches_sidecar(&form);

    // Install stamped the form as launch does: with the digest line broken and the stamp line
    // kept, the unchanged stamp is trusted and the form is not rebuilt.
    let stamped = Installed::new(&[]);
    let form = stamped.tool_form();
    assert_eq!(sidecar_line(&form, 1), stamp(&form));
    let written = inode(&form);
    break_recorded_sha256(&form);
    stamped.project.run();
    assert_eq!(inode(&form), written, "the stamped form was rebuilt");
}

#[test]
fn a_tampered_payload_fails_even_with_an_install_written_form() {
    let installed = Installed::new(&[]);
    let form = installed.tool_form();
    installed.project.run();
    let installed_zip = installed.project.project.path().join(format!(
        ".murmur/artifacts/{TOOL_NAME}/{TOOL_VERSION}/{TOOL_BARE}-{TOOL_VERSION}.mur.zip"
    ));
    fs::write(installed_zip, b"tampered").unwrap();

    common::run_capsule(&installed.project.home, &installed.project.manifest)
        .failure()
        .stderr(predicate::str::contains("E-REG-002"));
    assert!(form.is_file());
}

/// The binaries the host facts scenarios allow, all on any host that runs them.
const ALLOWED: &[&str] = &["python3", "bash", "cat"];

/// The driver an [`Allowed`] project's agent infers through, published into its `HOME` before
/// the first run.
const DRIVER: &str = "local/murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.0";

/// Whether a host facts scenario named `test` must skip: its capsule allows a shell, which needs
/// an egress namespace, and it needs a `python3` to be an interpreter whose facts are kept.
fn skip_host_facts(test: &str) -> bool {
    if capsule_runtime::skip_without_egress_namespace(test) {
        return true;
    }
    if capsule_runtime::find_on_path("python3").is_none() {
        println!("[SKIP] no python3 on PATH");
        return true;
    }
    false
}

fn host_facts(home: &Path) -> PathBuf {
    home.join(".murmur").join("host_facts")
}

/// Each entry in `home`'s host facts directory, by file name, with its bytes.
fn host_facts_entries(home: &Path) -> Vec<(String, Vec<u8>)> {
    let mut entries: Vec<_> = fs::read_dir(host_facts(home))
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (
                entry.file_name().into_string().unwrap(),
                fs::read(entry.path()).unwrap(),
            )
        })
        .collect();
    entries.sort();
    entries
}

/// `session_start.host_facts` reporting `source` for each of [`ALLOWED`], in the order a staging
/// examines them: by canonical path.
fn every_allowed_binary(source: Value) -> Value {
    let mut binaries: Vec<String> = ALLOWED
        .iter()
        .map(|name| {
            fs::canonicalize(capsule_runtime::find_on_path(name).unwrap())
                .unwrap()
                .display()
                .to_string()
        })
        .collect();
    binaries.sort();
    binaries
        .into_iter()
        .map(|binary| {
            let mut object = json!({ "binary": binary });
            object
                .as_object_mut()
                .unwrap()
                .extend(source.as_object().unwrap().clone());
            object
        })
        .collect()
}

fn kept() -> Value {
    every_allowed_binary(json!({"source": "kept"}))
}

fn derived(reason: &str) -> Value {
    every_allowed_binary(json!({"source": "derived", "reason": reason}))
}

fn end_turn_response() -> String {
    json!({
        "id": "msg_et",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": "done"}],
        "stop_reason": "end_turn",
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": 8, "output_tokens": 3},
    })
    .to_string()
}

/// An http agent on a scripted model, declaring `echo-tool` and allowing `allow` in its shell,
/// with a scratch `HOME` that holds nothing until the first install or run.
struct Allowed {
    home: TempDir,
    project: TempDir,
    manifest: PathBuf,
    /// The `echo-tool` `.mur.zip` every install installs.
    artifact: PathBuf,
    server: common::ScriptedServer,
    fixture: TempDir,
}

impl Allowed {
    fn new(allow: &[&str]) -> Self {
        let server = common::ScriptedServer::start(vec![end_turn_response(); 4]);
        let fixture = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let artifact = create_tool_artifact(
            fixture.path(),
            TOOL_NAME,
            TOOL_VERSION,
            &fixture_component("echo-tool.wasm"),
        );
        let allow: String = allow
            .iter()
            .map(|name| format!("      - {name}\n"))
            .collect();
        let endpoint = &server.endpoint;
        let manifest = project.path().join("murmur.yaml");
        fs::write(
            &manifest,
            format!(
                "name: host-facts\nversion: 0.1.0\nartifacts:\n  - name: {DRIVER}\n    version: \
                 {DRIVER_VERSION}\n    runtime: driver\n    gateway:\n      endpoint: {endpoint}\n      \
                 api_key: test-key\n  - name: {TOOL_NAME}\n    version: {TOOL_VERSION}\ncapabilities:\n  \
                 network:\n    allow:\n      - {endpoint}\n  shell:\n    allow:\n{allow}inference:\n  \
                 transport: http\n  model: test-model\n  driver:\n    artifact: {DRIVER}\n"
            ),
        )
        .unwrap();
        Self {
            home: tempfile::tempdir().unwrap(),
            project,
            manifest,
            artifact,
            server,
            fixture,
        }
    }

    /// `mur install <echo-tool zip>` in the project with `extra_args`, required to exit 0.
    fn install(&self, extra_args: &[&str]) -> (String, String) {
        output(
            common::install_artifact_to_project_with_home(
                self.project.path(),
                &self.home,
                &self.artifact,
                extra_args,
            )
            .success(),
        )
    }

    /// What [`Self::install`] prints holding facts for `n` binaries.
    fn installed_and_kept(&self, n: usize) -> (String, String) {
        (
            format!(
                "Installed {TOOL_NAME}@{TOOL_VERSION} from {}\nKept host facts for {n} allowlisted \
                 binaries\n",
                self.artifact.display()
            ),
            String::new(),
        )
    }

    /// What [`Self::install`] prints holding no facts.
    fn installed_only(&self) -> (String, String) {
        (
            format!(
                "Installed {TOOL_NAME}@{TOOL_VERSION} from {}\n",
                self.artifact.display()
            ),
            String::new(),
        )
    }

    /// `mur run` of the agent with `extra_args`, required to finish `ok`. The driver is installed
    /// into the project first, with `--no-precompile` so it writes nothing under `~/.murmur`, if
    /// no run has installed it.
    fn run(&self, extra_args: &[&str]) -> Run {
        let driver = self
            .fixture
            .path()
            .join("murmur-driver-anthropic-0.1.0.mur.zip");
        if !driver.exists() {
            common::create_driver_artifact(
                self.fixture.path(),
                DRIVER,
                DRIVER_VERSION,
                &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
            );
            common::install_artifact_to_project_with_home(
                self.project.path(),
                &self.home,
                &driver,
                &["--no-precompile"],
            )
            .success();
        }
        let output = Command::cargo_bin("mur")
            .unwrap()
            .env("HOME", self.home.path())
            .current_dir(self.project.path())
            .args([
                "run",
                "--manifest",
                self.manifest.to_str().unwrap(),
                "--task",
                "Report and stop.",
                "--verbose",
            ])
            .args(extra_args)
            .timeout(Duration::from_secs(300))
            .assert()
            .success()
            .stdout(predicate::str::contains("status:  ok"))
            .get_output()
            .clone();
        Run {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }
}

/// One finished `mur run` of an [`Allowed`] agent.
struct Run {
    stdout: String,
    stderr: String,
}

impl Run {
    fn session_start(&self) -> Value {
        let records = common::parse_records_from_stdout(&self.stdout);
        common::read_whole_trace(&records.join("trace.jsonl"))
            .into_iter()
            .find(|record| record["event_type"] == "session_start")
            .unwrap_or_else(|| panic!("no session_start: {}", self.stdout))
    }
}

#[test]
fn install_keeps_host_facts_so_the_first_run_keeps_every_binary() {
    if skip_host_facts("install_keeps_host_facts_so_the_first_run_keeps_every_binary") {
        return;
    }
    let allowed = Allowed::new(ALLOWED);
    assert_eq!(allowed.install(&[]), allowed.installed_and_kept(3));
    let dir = host_facts(allowed.home.path());
    assert_eq!(mode(&dir), 0o700);
    let entries: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(entries.len(), 3, "{entries:?}");
    for entry in &entries {
        assert!(
            entry.extension().is_some_and(|ext| ext == "json"),
            "{entry:?}"
        );
        assert_eq!(mode(entry), 0o600, "{entry:?}");
    }

    assert_eq!(allowed.run(&[]).session_start()["host_facts"], kept());
}

#[test]
fn install_kept_host_facts_hold_for_a_run_with_another_workdir() {
    if skip_host_facts("install_kept_host_facts_hold_for_a_run_with_another_workdir") {
        return;
    }
    let allowed = Allowed::new(ALLOWED);
    allowed.install(&[]);
    let workdir = tempfile::tempdir().unwrap();
    let run = allowed.run(&["--workdir", workdir.path().to_str().unwrap()]);
    assert_eq!(run.session_start()["host_facts"], kept());
}

#[test]
fn install_written_host_facts_are_the_launchs_own_and_change_no_grant() {
    if skip_host_facts("install_written_host_facts_are_the_launchs_own_and_change_no_grant") {
        return;
    }
    let installed = Allowed::new(ALLOWED);
    installed.install(&[]);
    let after_install = host_facts_entries(installed.home.path());
    let launched = Allowed::new(ALLOWED);
    launched.install(&["--no-precompile"]);

    let kept_run = installed.run(&[]);
    let derived_run = launched.run(&[]);
    assert_eq!(after_install.len(), 3);
    assert_eq!(after_install, host_facts_entries(launched.home.path()));

    let kept_start = kept_run.session_start();
    let derived_start = derived_run.session_start();
    assert_eq!(kept_start["host_facts"], kept());
    assert_eq!(derived_start["host_facts"], derived("absent"));
    // Each run's own home, project, model endpoint and session id named alike.
    let neutral = |text: String, allowed: &Allowed, session_start: &Value| {
        let mut text = text;
        for (path, placeholder) in [
            (allowed.project.path(), "<PROJECT>"),
            (allowed.home.path(), "<HOME>"),
        ] {
            let canonical = fs::canonicalize(path).unwrap();
            text = text.replace(&canonical.display().to_string(), placeholder);
            text = text.replace(&path.display().to_string(), placeholder);
        }
        text.replace(&allowed.server.endpoint, "<ENDPOINT>")
            .replace(session_start["session_id"].as_str().unwrap(), "<SESSION>")
    };
    assert_ne!(kept_start["effective_grants"], Value::Null);
    assert_eq!(
        neutral(
            kept_start["effective_grants"].to_string(),
            &installed,
            &kept_start
        ),
        neutral(
            derived_start["effective_grants"].to_string(),
            &launched,
            &derived_start
        )
    );
    assert_eq!(
        neutral(kept_run.stderr, &installed, &kept_start),
        neutral(derived_run.stderr, &launched, &derived_start)
    );
}

#[test]
fn install_skips_an_allowlisted_binary_it_cannot_examine_without_a_word() {
    if skip_host_facts("install_skips_an_allowlisted_binary_it_cannot_examine_without_a_word") {
        return;
    }
    let allowed = Allowed::new(&["python3", "cat", "murmur-test-no-such-binary"]);
    assert_eq!(allowed.install(&[]), allowed.installed_and_kept(2));
    assert_eq!(host_facts_entries(allowed.home.path()).len(), 2);
}

#[test]
fn install_with_no_precompile_keeps_no_host_facts() {
    if skip_host_facts("install_with_no_precompile_keeps_no_host_facts") {
        return;
    }
    let allowed = Allowed::new(ALLOWED);
    assert_eq!(
        allowed.install(&["--no-precompile"]),
        allowed.installed_only()
    );
    assert!(!allowed.home.path().join(".murmur").exists());
    assert_eq!(
        allowed.run(&[]).session_start()["host_facts"],
        derived("absent")
    );

    Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", allowed.home.path())
        .args(["install", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "keeping host facts for the manifest's shell.allow binaries",
        ));
}

#[test]
fn an_install_with_nothing_allowlisted_or_into_the_global_store_keeps_no_host_facts() {
    let unallowed = Installed::new(&[]);
    assert!(unallowed.project.compiled().exists());
    assert!(!host_facts(unallowed.project.home.path()).exists());

    let global = Allowed::new(ALLOWED);
    let (stdout, stderr) = output(
        Command::cargo_bin("mur")
            .unwrap()
            .env("HOME", global.home.path())
            .current_dir(global.project.path())
            .args(["install", "-g", global.artifact.to_str().unwrap()])
            .assert()
            .success(),
    );
    assert!(!stdout.contains("host facts"), "{stdout}");
    assert_eq!(stderr, "");
    assert!(global.home.path().join(".murmur").exists());
    assert!(!host_facts(global.home.path()).exists());
}

#[test]
fn a_repeat_install_keeps_every_host_facts_entry_as_it_is() {
    if skip_host_facts("a_repeat_install_keeps_every_host_facts_entry_as_it_is") {
        return;
    }
    let allowed = Allowed::new(ALLOWED);
    let first = allowed.install(&[]);
    let dir = host_facts(allowed.home.path());
    let before = snapshot(&dir);
    assert_eq!(before.len(), 3);
    assert_eq!(allowed.install(&[]), first);
    assert_eq!(snapshot(&dir), before);
}

#[test]
fn an_unusable_host_facts_path_never_fails_an_install() {
    if skip_host_facts("an_unusable_host_facts_path_never_fails_an_install") {
        return;
    }
    let elsewhere = tempfile::tempdir().unwrap();
    type Plant = fn(&Path, &Path);
    let plants: [(&str, Plant); 2] = [
        ("a regular file", |dir, _| {
            fs::write(dir, b"not a directory").unwrap()
        }),
        ("a symlink to a directory", |dir, target| {
            std::os::unix::fs::symlink(target, dir).unwrap()
        }),
    ];
    for (case, plant) in plants {
        let allowed = Allowed::new(ALLOWED);
        let murmur_home = allowed.home.path().join(".murmur");
        fs::create_dir_all(&murmur_home).unwrap();
        set_mode(&murmur_home, 0o700);
        let dir = host_facts(allowed.home.path());
        plant(&dir, elsewhere.path());
        let planted = || (fs::read_link(&dir).ok(), fs::read(&dir).ok());
        let before = planted();

        assert_eq!(allowed.install(&[]), allowed.installed_only(), "{case}");
        assert_eq!(planted(), before, "{case}");
        assert_eq!(fs::read_dir(elsewhere.path()).unwrap().count(), 0, "{case}");

        assert_eq!(
            allowed.run(&[]).session_start()["host_facts"],
            derived("not-kept"),
            "{case}"
        );
        assert_eq!(planted(), before, "{case}");
    }
}

#[test]
fn a_manifest_install_keeps_host_facts_once_after_its_roll_up() {
    if skip_host_facts("a_manifest_install_keeps_host_facts_once_after_its_roll_up") {
        return;
    }
    let allowed = Allowed::new(ALLOWED);
    common::publish_local(&allowed.home, &allowed.artifact).success();
    let driver = common::create_driver_artifact(
        allowed.fixture.path(),
        DRIVER,
        DRIVER_VERSION,
        &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&allowed.home, &driver).success();

    let (stdout, stderr) = output(
        Command::cargo_bin("mur")
            .unwrap()
            .env("HOME", allowed.home.path())
            .current_dir(allowed.project.path())
            .arg("install")
            .assert()
            .success(),
    );
    assert_eq!(stderr, "");
    let line = "Kept host facts for 3 allowlisted binaries\n";
    assert_eq!(stdout.matches(line).count(), 1, "{stdout}");
    assert!(stdout.ends_with(line), "{stdout}");
    assert_eq!(host_facts_entries(allowed.home.path()).len(), 3);
    assert_eq!(allowed.run(&[]).session_start()["host_facts"], kept());
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

/// Packs a WASM tool `.mur.zip` for the full name `name`, under its bare part.
fn create_tool_artifact(
    dir: &Path,
    name: &str,
    version: &str,
    tool_component_path: &Path,
) -> PathBuf {
    let name = murmur_artifact::tool_name_of(name);
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
