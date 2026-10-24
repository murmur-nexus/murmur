//! `W-SEC-019`: what an operator sees when `murmur.yaml` carries a key this build does not
//! recognize.
//!
//! Driven through the real `mur` binary on both surfaces, because the claim is about two stderr
//! streams reading identically — an in-process assertion about one message builder cannot tell you
//! whether `mur run` and `mur doctor` actually print the same bytes.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;
use zip::{
    write::{FileOptions, SimpleFileOptions},
    CompressionMethod, ZipWriter,
};

const CODE: &str = "W-SEC-019";

fn project(dir: &Path, manifest: &str) {
    fs::write(dir.join("murmur.yaml"), manifest).unwrap();
    fs::write(dir.join("capsule.wasm"), b"\0asm\x01\0\0\0").unwrap();
}

/// `--explain-scope` returns ahead of every side effect, so these need no installed artifact and
/// no registry.
fn explain_scope_stderr(home: &TempDir, project_dir: &Path) -> String {
    let output = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .current_dir(project_dir)
        .args(["run", "--manifest", "murmur.yaml", "--explain-scope"])
        .assert()
        .success()
        .get_output()
        .stderr
        .clone();
    String::from_utf8(output).unwrap()
}

/// Doctor's exit status depends on the artifact checklist, which these fixtures deliberately leave
/// empty, so the stream is taken without asserting on it.
fn doctor_stderr(home: &TempDir, project_dir: &Path) -> String {
    let output = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .current_dir(project_dir)
        .arg("doctor")
        .assert()
        .get_output()
        .stderr
        .clone();
    String::from_utf8(output).unwrap()
}

fn warning_lines(stderr: &str) -> Vec<&str> {
    stderr.lines().filter(|line| line.contains(CODE)).collect()
}

/// The hyphen-for-underscore typo, named with the block that held it and with the key it should
/// have been — and the run still exits 0, because an unrecognized key refuses nothing.
#[test]
fn a_typo_is_named_with_its_path_and_the_nearest_key() {
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    project(
        dir.path(),
        "name: typo-capsule\nversion: 0.1.0\ncapabilities:\n  filesystem:\n    read-only:\n      \
         - tests\n",
    );

    let stderr = explain_scope_stderr(&home, dir.path());
    let lines = warning_lines(&stderr);
    assert_eq!(lines.len(), 1, "{stderr}");
    assert!(lines[0].contains("unrecognized key 'read-only' in capabilities.filesystem"));
    assert!(lines[0].contains("did you mean 'read_only'?"));
    assert!(lines[0].contains("spelling problem"));
}

/// The two surfaces print one identical line. Asserted on the bytes as well as structurally
/// (both call `warn_on_unknown_manifest_keys`), because that is what an operator comparing the two
/// actually reads.
#[test]
fn run_and_doctor_print_the_same_bytes() {
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    project(
        dir.path(),
        "name: typo-capsule\nversion: 0.1.0\ncapabilities:\n  filesystem:\n    read-only:\n      \
         - tests\n",
    );

    assert_eq!(
        warning_lines(&explain_scope_stderr(&home, dir.path())),
        warning_lines(&doctor_stderr(&home, dir.path()))
    );
}

/// A manifest of recognized keys is silent on both surfaces. A warning that fires on correct input
/// is noise, and noise is what every operator learns to ignore.
#[test]
fn a_correct_manifest_warns_on_neither_surface() {
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    project(
        dir.path(),
        r#"name: clean-capsule
version: 0.1.0
artifacts:
  - name: local/notes-tool
    version: 0.1.0
    runtime: tool
    capabilities:
      network:
        allow:
          - example.com:443
capabilities:
  filesystem:
    read_only:
      - tests
      - bench/fixtures
  shell:
    allow:
      - git
  network:
    allow:
      - example.com:443
"#,
    );

    assert!(warning_lines(&explain_scope_stderr(&home, dir.path())).is_empty());
    assert!(warning_lines(&doctor_stderr(&home, dir.path())).is_empty());
}

/// A key with no near neighbour is worded as one this build does not know rather than as a
/// misspelling: sending an operator to hunt for a typo in a correct manifest is the failure this
/// warning exists to stop.
#[test]
fn a_key_with_no_near_neighbour_is_not_called_a_typo() {
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    project(
        dir.path(),
        "name: newer-capsule\nversion: 0.1.0\nquantum_teleport: true\n",
    );

    let stderr = explain_scope_stderr(&home, dir.path());
    let lines = warning_lines(&stderr);
    assert_eq!(lines.len(), 1, "{stderr}");
    assert!(lines[0].contains("unrecognized key 'quantum_teleport' at the top level"));
    assert!(lines[0].contains("may come from a newer mur"));
    assert!(!lines[0].contains("did you mean"));
    assert!(!lines[0].contains("spelling"));
}

/// A typo under `inference.retry` is named at `inference.retry`, with the key it should have been.
#[test]
fn a_typo_under_inference_retry_is_named_at_its_block() {
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    project(
        dir.path(),
        "name: retry-capsule\nversion: 0.1.0\nartifacts:\n  - name: local/drv\n    version: 0.1.0\n    \
         runtime: driver\n    gateway:\n      endpoint: http://127.0.0.1:8080\n      api_key: \
         sk-test\ninference:\n  transport: http\n  model: m\n  driver:\n    artifact: local/drv\n  \
         retry:\n    max_retrys: 2\n",
    );

    let stderr = explain_scope_stderr(&home, dir.path());
    let lines = warning_lines(&stderr);
    assert_eq!(lines.len(), 1, "{stderr}");
    assert!(
        lines[0].contains("unrecognized key 'max_retrys' in inference.retry"),
        "{}",
        lines[0]
    );
    assert!(
        lines[0].contains("did you mean 'max_retries'?"),
        "{}",
        lines[0]
    );
}

/// A top-level `mur_version` is refused before anything is staged, on every surface that loads the
/// project manifest, and both the message and the hint name `deploy.mur_version`.
#[test]
fn a_top_level_mur_version_is_refused_with_a_hint_naming_deploy_mur_version() {
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    project(
        dir.path(),
        "name: c\nversion: 0.1.0\nmur_version: \"0.4.5\"\n",
    );

    for args in [
        &["run", "--manifest", "murmur.yaml", "--explain-scope"][..],
        &["doctor"],
        &["install"],
    ] {
        let output = Command::cargo_bin("mur")
            .unwrap()
            .env("HOME", home.path())
            .current_dir(dir.path())
            .args(args)
            .assert()
            .failure()
            .get_output()
            .stderr
            .clone();
        let stderr = String::from_utf8(output).unwrap();

        assert!(stderr.contains("error[E-MAN-003]"), "{args:?}: {stderr}");
        assert!(
            stderr.contains(
                "murmur.yaml: 'mur_version' is not a top-level key — the mur release a deploy \
                 installs is pinned as 'deploy.mur_version'"
            ),
            "{args:?}: {stderr}"
        );
        assert!(
            stderr
                .lines()
                .any(|line| line.contains("hint:") && line.contains("deploy.mur_version")),
            "{args:?}: {stderr}"
        );
    }
}

/// A `capabilities.shell.allow` entry that is not a bare binary name is refused before anything is
/// staged or installed, on every surface that loads the project manifest, and nothing is created
/// at the path it names.
#[test]
fn a_shell_allow_entry_that_is_not_a_bare_name_is_refused_on_every_surface() {
    let home = TempDir::new().unwrap();
    let scratch = TempDir::new().unwrap();
    let absolute = scratch.path().join("bin/uv").display().to_string();
    for (entry, reason) in [
        (absolute.as_str(), "is a path"),
        ("bin/uv", "is a path"),
        ("..", "names a directory"),
        (".", "names a directory"),
        ("", "is empty"),
        ("a b", "contains whitespace"),
    ] {
        let dir = TempDir::new().unwrap();
        project(
            dir.path(),
            &format!(
                "name: c\nversion: 0.1.0\ncapabilities:\n  shell:\n    allow: [bash, {}]\n",
                serde_json::to_string(entry).unwrap()
            ),
        );
        for args in [
            &["run", "--manifest", "murmur.yaml", "--explain-scope"][..],
            &["doctor"],
            &["install"],
        ] {
            let output = Command::cargo_bin("mur")
                .unwrap()
                .env("HOME", home.path())
                .current_dir(dir.path())
                .args(args)
                .assert()
                .code(1)
                .get_output()
                .stderr
                .clone();
            let stderr = String::from_utf8(output).unwrap();
            assert!(stderr.contains("error[E-MAN-003]"), "{args:?}: {stderr}");
            assert!(
                stderr.contains(&format!(
                    "invalid capability config for 'capabilities.shell.allow[1]': '{entry}' \
                     {reason}; an entry is a bare binary name"
                )),
                "{args:?} {entry:?}: {stderr}"
            );
            assert!(
                stderr.contains("hint: name the binary bare")
                    && stderr.contains("PATH=/opt/uv/bin:$PATH mur run"),
                "{args:?}: {stderr}"
            );
        }
        assert!(
            !scratch.path().join("bin").exists(),
            "{entry:?} created a path"
        );
        assert!(
            !dir.path().join(".murmur").exists(),
            "{entry:?} created .murmur"
        );
    }
}

/// `deploy:` is read by `mur deploy` alone: a pin under it, higher or lower than the running
/// binary, adds nothing to either surface, and the per-key lines are unchanged beside it.
#[test]
fn a_deploy_pin_warns_nothing_on_either_surface() {
    let home = TempDir::new().unwrap();
    let higher = TempDir::new().unwrap();
    project(
        higher.path(),
        "name: pin-capsule\nversion: 0.1.0\ndeploy:\n  mur_version: \"99.0.0\"\nquantum_teleport: true\n",
    );
    let lower = TempDir::new().unwrap();
    project(
        lower.path(),
        "name: pin-capsule\nversion: 0.1.0\ndeploy:\n  mur_version: \"0.0.1\"\n",
    );

    let higher_streams = [
        explain_scope_stderr(&home, higher.path()),
        doctor_stderr(&home, higher.path()),
    ];
    for stderr in &higher_streams {
        let lines = warning_lines(stderr);
        assert_eq!(lines.len(), 1, "{stderr}");
        assert!(
            lines[0].contains("unrecognized key 'quantum_teleport' at the top level"),
            "{stderr}"
        );
    }

    let lower_streams = [
        explain_scope_stderr(&home, lower.path()),
        doctor_stderr(&home, lower.path()),
    ];
    for stderr in &lower_streams {
        assert!(warning_lines(stderr).is_empty(), "{stderr}");
    }

    // No version line on either surface, in either wording, and neither pinned version quoted.
    let mut absent = ["requires", "pins"]
        .map(|verb| format!("{verb} mur"))
        .to_vec();
    absent.extend(["99.0.0", "0.0.1"].map(str::to_string));
    for stderr in higher_streams.iter().chain(&lower_streams) {
        for absent in &absent {
            assert!(!stderr.contains(absent.as_str()), "{absent:?} in {stderr}");
        }
    }
}

/// The least a delegated sub-capsule needs to be publishable — `runtime:` plus `execution:` —
/// is silent on both surfaces. `POST /register` derives each registrant's ceiling from the
/// registry manifest, so every sub-capsule of a delegating formation carries these keys.
#[test]
fn a_publishable_capsule_warns_on_neither_surface() {
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    project(
        dir.path(),
        "name: probe\nversion: 0.1.0\nruntime: capsule\nexecution: static\n",
    );

    assert_eq!(
        warning_lines(&explain_scope_stderr(&home, dir.path())),
        Vec::<&str>::new()
    );
    assert!(warning_lines(&doctor_stderr(&home, dir.path())).is_empty());
}

/// All four keys `mur build` and `mur publish` read beyond `name:` and `version:`, not just the
/// two a publishable agent capsule cannot omit.
#[test]
fn every_build_side_key_warns_on_neither_surface() {
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    project(
        dir.path(),
        "name: probe\nversion: 0.1.0\nruntime: capsule\nimplementation: wasm\nexecution: \
         static\nrequires_files:\n  - capsule.wasm\n  - assets/logo.png\n",
    );

    assert_eq!(
        warning_lines(&explain_scope_stderr(&home, dir.path())),
        Vec::<&str>::new()
    );
    assert!(warning_lines(&doctor_stderr(&home, dir.path())).is_empty());
}

/// `W-SEC-019` still covers every key no command reads: one sitting beside the four build-side
/// keys reports alone, and both surfaces print the same bytes for it.
#[test]
fn a_genuinely_unknown_key_survives_beside_the_build_side_keys() {
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    project(
        dir.path(),
        "name: probe\nversion: 0.1.0\nruntime: capsule\nimplementation: wasm\nexecution: \
         static\nrequires_files:\n  - capsule.wasm\nquantum_teleport: true\n",
    );

    let stderr = explain_scope_stderr(&home, dir.path());
    let lines = warning_lines(&stderr);
    assert_eq!(lines.len(), 1, "{stderr}");
    assert!(lines[0].contains("unrecognized key 'quantum_teleport' at the top level"));
    assert!(lines[0].contains("may come from a newer mur"));
    for key in ["runtime", "implementation", "execution", "requires_files"] {
        assert!(!lines[0].contains(&format!("'{key}'")), "{stderr}");
    }

    assert_eq!(lines, warning_lines(&doctor_stderr(&home, dir.path())));
}

// ── A full launch, not just the diagnostic ────────────────────────────────────

const TOOL_NAME: &str = "echo-tool";
const TOOL_VERSION: &str = "0.1.0";

fn fixture_component(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("run")
        .join("components")
        .join(name)
}

fn tool_artifact(dir: &Path) -> PathBuf {
    let artifact_path = dir.join(format!("{TOOL_NAME}-{TOOL_VERSION}.mur.zip"));
    let mut zip = ZipWriter::new(fs::File::create(&artifact_path).unwrap());
    let options: SimpleFileOptions =
        FileOptions::default().compression_method(CompressionMethod::Deflated);

    zip.start_file("murmur.yaml", options).unwrap();
    writeln!(zip, "name: {TOOL_NAME}").unwrap();
    writeln!(zip, "version: {TOOL_VERSION}").unwrap();
    writeln!(zip, "runtime: wasm").unwrap();

    zip.start_file("tool.wasm", options).unwrap();
    zip.write_all(&fs::read(fixture_component("echo-tool.wasm")).unwrap())
        .unwrap();
    zip.finish().unwrap();
    artifact_path
}

/// An unrecognized key changes no exit code and refuses nothing: the capsule stages, launches and
/// completes with the warning printed beside it. This is the invariant the whole design rests on —
/// `deny_unknown_fields` is set nowhere, so a manifest written for a newer `mur` still runs here.
#[test]
fn a_capsule_with_an_unrecognized_key_still_launches() {
    let home = TempDir::new().unwrap();
    let fixture = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();

    fs::write(
        dir.path().join("murmur.yaml"),
        format!(
            "name: capsule\nversion: 0.0.1\nartifacts:\n  - name: local/{TOOL_NAME}\n    version: \
             {TOOL_VERSION}\ncapabilities:\n  filesystem:\n    read-only:\n      - tests\n"
        ),
    )
    .unwrap();
    fs::copy(
        fixture_component("capsule-allowlisted.wasm"),
        dir.path().join("capsule.wasm"),
    )
    .unwrap();
    common::install_artifact_to_project(dir.path(), &tool_artifact(fixture.path())).success();

    common::run_capsule(&home, &dir.path().join("murmur.yaml"))
        .success()
        .stdout(predicate::str::contains("status:  ok"))
        .stderr(predicate::str::contains(
            "unrecognized key 'read-only' in capabilities.filesystem",
        ));
}
