use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::tempdir;
use zip::ZipArchive;

#[test]
fn build_valid_fixture_creates_mur_zip_with_expected_layout() {
    let fixture = fixture_path("happy");
    let dir = tempdir().unwrap();
    copy_dir_all(&fixture, dir.path());

    let artifact = dir.path().join("out.mur.zip");

    Command::cargo_bin("mur")
        .unwrap()
        .args([
            "build",
            dir.path().to_str().unwrap(),
            "--output",
            artifact.to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Built artifact:"));

    assert!(artifact.exists());

    let file = fs::File::open(&artifact).unwrap();
    let mut archive = ZipArchive::new(file).unwrap();

    let mut names = Vec::new();
    for i in 0..archive.len() {
        let file = archive.by_index(i).unwrap();
        names.push(file.name().unwrap().into_owned());
    }
    names.sort();

    // The fixture also carries a README.md, which the manifest does not declare. A built
    // artifact ships what `requires_files:` names, plus the `murmur-build.json` `mur build`
    // writes itself, so the README stays behind.
    assert_eq!(
        names,
        vec![
            "murmur-build.json".to_string(),
            "murmur.yaml".to_string(),
            "tool.wasm".to_string()
        ]
    );

    let mut manifest = String::new();
    archive
        .by_name("murmur.yaml")
        .unwrap()
        .read_to_string(&mut manifest)
        .unwrap();
    assert!(manifest.contains("name: hello-slice"));
}

#[test]
fn missing_name_is_non_zero_with_actionable_error() {
    let fixture = fixture_path("missing-name");

    Command::cargo_bin("mur")
        .unwrap()
        .args(["build", fixture.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("missing required field 'name'"));
}

#[test]
fn missing_version_is_non_zero_with_actionable_error() {
    let fixture = fixture_path("missing-version");

    Command::cargo_bin("mur")
        .unwrap()
        .args(["build", fixture.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("missing required field 'version'"));
}

#[test]
fn malformed_yaml_is_non_zero_with_line_info() {
    let fixture = fixture_path("malformed");

    Command::cargo_bin("mur")
        .unwrap()
        .args(["build", fixture.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("line"));
}

#[test]
fn literal_api_key_warns_but_build_succeeds() {
    let dir = tempdir().unwrap();
    // Manifest written at test time with a runtime-assembled key so the repo
    // never contains a credential-shaped literal that secret scanners could flag.
    let key = ["sk-", "ant-", "abc123456789"].concat();
    fs::write(
        dir.path().join("murmur.yaml"),
        format!(
            "name: secret-demo\nversion: 0.0.1\nruntime: wasm\napi_key: {key}\nrequires_files:\n  - tool.wasm\n"
        ),
    )
    .unwrap();
    fs::write(dir.path().join("tool.wasm"), b"\0asm").unwrap();

    Command::cargo_bin("mur")
        .unwrap()
        .args(["build", dir.path().to_str().unwrap()])
        .assert()
        .success()
        .stderr(
            predicate::str::contains("appears to contain a literal secret value")
                .and(predicate::str::contains("api_key")),
        );
}

#[test]
fn skill_artifact_build_succeeds_with_skill_md() {
    let fixture = fixture_path("skill-happy");
    let dir = tempdir().unwrap();
    copy_dir_all(&fixture, dir.path());

    let artifact = dir.path().join("out.mur.zip");

    Command::cargo_bin("mur")
        .unwrap()
        .args([
            "build",
            dir.path().to_str().unwrap(),
            "--output",
            artifact.to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Built artifact:"));

    assert!(artifact.exists());

    let file = fs::File::open(&artifact).unwrap();
    let mut archive = ZipArchive::new(file).unwrap();

    let mut names: Vec<String> = (0..archive.len())
        .map(|i| archive.by_index(i).unwrap().name().unwrap().into_owned())
        .collect();
    names.sort();

    assert!(
        names.contains(&"murmur.yaml".to_string()),
        "murmur.yaml must be present in skill artifact zip"
    );
    assert!(
        names.contains(&"skill.md".to_string()),
        "skill.md must be present in skill artifact zip"
    );
}

#[test]
fn skill_artifact_build_fails_without_skill_md() {
    let fixture = fixture_path("skill-missing-skill-md");

    Command::cargo_bin("mur")
        .unwrap()
        .args(["build", fixture.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("skill.md"));
}

/// A wasm artifact whose payload the runtime could not select is a build failure, with the
/// message the runtime itself would have printed at launch — and nothing left on disk.
#[test]
fn two_root_wasm_files_fail_the_build_with_no_artifact_written() {
    let dir = tempdir().unwrap();
    fs::write(
        dir.path().join("murmur.yaml"),
        "name: ambiguous-tool\nversion: 0.1.0\nruntime: wasm\nrequires_files:\n  - alpha.wasm\n  - zeta.wasm\n",
    )
    .unwrap();
    fs::write(dir.path().join("alpha.wasm"), b"\0asm").unwrap();
    fs::write(dir.path().join("zeta.wasm"), b"\0asm").unwrap();

    Command::cargo_bin("mur")
        .unwrap()
        .args(["build", dir.path().to_str().unwrap()])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("error[E-BLD-003]:").and(predicate::str::contains(
                "multiple root .wasm files found: alpha.wasm, zeta.wasm",
            )),
        );

    assert!(!dir.path().join("ambiguous-tool-0.1.0.mur.zip").exists());
}

/// A manifest still declaring `inference_auth:` is refused before anything is written: the
/// runtime would refuse the packed artifact at launch. The same source under the new name builds.
#[test]
fn a_retired_auth_block_fails_the_build_with_no_artifact_written() {
    let dir = tempdir().unwrap();
    let manifest = |block: &str| {
        format!(
            "name: search-tool\nversion: 0.1.0\nruntime: wasm\nrequires_files:\n  - tool.wasm\n\
             {block}:\n  header: Authorization\n  value: \"Bearer {{key}}\"\n"
        )
    };
    fs::write(dir.path().join("murmur.yaml"), manifest("inference_auth")).unwrap();
    fs::write(dir.path().join("tool.wasm"), b"\0asm").unwrap();
    let artifact = dir.path().join("out.mur.zip");
    let build = || {
        Command::cargo_bin("mur")
            .unwrap()
            .args([
                "build",
                dir.path().to_str().unwrap(),
                "--output",
                artifact.to_str().unwrap(),
            ])
            .assert()
    };

    build().failure().stderr(
        predicate::str::contains("error[E-BLD-004]:")
            .and(predicate::str::contains(
                "field 'inference_auth' was renamed to 'upstream_auth'",
            ))
            .and(predicate::str::contains(
                "hint: rename the block to upstream_auth:",
            )),
    );
    assert!(!artifact.exists());

    fs::write(dir.path().join("murmur.yaml"), manifest("upstream_auth")).unwrap();
    build().success();
    assert!(artifact.exists());
}

/// Builds the source in `dir` to `dir/out.mur.zip`.
fn build_in(dir: &Path) -> assert_cmd::assert::Assert {
    Command::cargo_bin("mur")
        .unwrap()
        .args([
            "build",
            dir.to_str().unwrap(),
            "--output",
            dir.join("out.mur.zip").to_str().unwrap(),
        ])
        .assert()
}

/// A driver manifest with `models` as written, packing one `driver.wasm`.
fn models_driver_manifest(runtime: &str, models: &str) -> String {
    format!(
        "name: model-driver\nversion: 0.1.0\nruntime: {runtime}\nrequires_files:\n  - driver.wasm\n\
         {models}"
    )
}

/// Every malformed `models:` table, and one on an artifact that is not a driver, is refused before
/// anything is written, naming the field.
#[test]
fn models_malformed_tables_fail_the_build_naming_the_field() {
    let entry = |body: &str| format!("models:\n  glm-5.3:\n{body}");
    for (runtime, models, field) in [
        ("driver", "models: [glm-5.3]\n".to_string(), "'models'"),
        (
            "driver",
            entry("    max_output_tokens: 131072\n"),
            "'models.glm-5.3.context_window'",
        ),
        (
            "driver",
            entry("    max_output_tokens: 0\n    context_window: 200000\n"),
            "'models.glm-5.3.max_output_tokens'",
        ),
        (
            "driver",
            entry("    max_output_tokens: 1\n    context_window: 2\n    tokenizer: x\n"),
            "'models.glm-5.3.tokenizer'",
        ),
        (
            "driver",
            entry(
                "    max_output_tokens: 128000\n    context_window: 1050000\n    \
                 max_input_tokens: 1050001\n",
            ),
            "'models.glm-5.3.max_input_tokens' is 1050001, above this entry's context_window of \
             1050000",
        ),
        (
            "driver",
            "models:\n  \"\":\n    max_output_tokens: 1\n    context_window: 2\n".to_string(),
            "'models' has an empty model id",
        ),
        (
            "tool",
            entry("    max_output_tokens: 1\n    context_window: 2\n"),
            "'models' is only valid on an artifact whose runtime is driver",
        ),
    ] {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("murmur.yaml"),
            models_driver_manifest(runtime, &models),
        )
        .unwrap();
        fs::write(dir.path().join("driver.wasm"), b"\0asm").unwrap();
        build_in(dir.path()).failure().stderr(
            predicate::str::contains("error[E-BLD-005]:")
                .and(predicate::str::contains(field))
                .and(predicate::str::contains(
                    "hint: models: is a driver's table",
                ))
                .and(predicate::str::contains("optionally max_input_tokens")),
        );
        assert!(!dir.path().join("out.mur.zip").exists(), "{models}");
    }
}

/// A well-formed `models:` table builds, and the manifest is packed byte for byte.
#[test]
fn models_a_good_table_builds_and_is_packed_byte_for_byte() {
    let dir = tempdir().unwrap();
    let manifest = models_driver_manifest(
        "driver",
        "# limits for each model this driver serves\nmodels:\n  glm-5.3:\n    \
         max_output_tokens: 131072\n    context_window: 200000\n  gpt-5.6:\n    \
         max_output_tokens: 128000\n    context_window: 1050000\n    max_input_tokens: 922000\n",
    );
    fs::write(dir.path().join("murmur.yaml"), &manifest).unwrap();
    fs::write(dir.path().join("driver.wasm"), b"\0asm").unwrap();
    build_in(dir.path()).success();

    let mut archive =
        ZipArchive::new(fs::File::open(dir.path().join("out.mur.zip")).unwrap()).unwrap();
    let mut packed = String::new();
    archive
        .by_name("murmur.yaml")
        .unwrap()
        .read_to_string(&mut packed)
        .unwrap();
    assert_eq!(packed, manifest);
}

/// A tool manifest with `provider_tool` as written, packing one `tool.wasm`.
fn provider_tool_manifest(runtime: &str, block: &str) -> String {
    format!(
        "name: computer\nversion: 0.1.0\nruntime: {runtime}\nrequires_files:\n  - tool.wasm\n\
         {block}"
    )
}

/// A malformed `provider_tool:` block, and one on an artifact that is not a tool, is refused
/// before anything is written, naming the field.
#[test]
fn provider_tool_malformed_blocks_fail_the_build_naming_the_field() {
    let computer = |width: &str| {
        format!(
            "provider_tool:\n  kind: computer\n  display_width: {width}\n  display_height: 800\n"
        )
    };
    for (runtime, block, field) in [
        (
            "tool",
            computer("0"),
            "'provider_tool.display_width' must be an integer from 1 to 65535",
        ),
        (
            "tool",
            computer("65536"),
            "'provider_tool.display_width' must be an integer from 1 to 65535",
        ),
        (
            "tool",
            computer("wide"),
            "'provider_tool.display_width' has invalid type",
        ),
        (
            "tool",
            "provider_tool:\n  kind: phone\n  display_width: 1\n  display_height: 1\n".to_string(),
            "the known kinds are: computer",
        ),
        (
            "hook",
            computer("1280"),
            "'provider_tool' is only valid on an artifact whose runtime is tool",
        ),
    ] {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("murmur.yaml"),
            provider_tool_manifest(runtime, &block),
        )
        .unwrap();
        fs::write(dir.path().join("tool.wasm"), b"\0asm").unwrap();
        build_in(dir.path()).failure().stderr(
            predicate::str::contains("error[E-BLD-006]:")
                .and(predicate::str::contains(field))
                .and(predicate::str::contains(
                    "hint: provider_tool: goes on a runtime: tool",
                )),
        );
        assert!(!dir.path().join("out.mur.zip").exists(), "{block}");
    }
}

/// A well-formed `provider_tool:` block builds, and the manifest is packed byte for byte.
#[test]
fn provider_tool_a_good_block_builds_and_is_packed_byte_for_byte() {
    let dir = tempdir().unwrap();
    let manifest = provider_tool_manifest(
        "tool",
        "provider_tool:\n  kind: computer\n  display_width: 1280\n  display_height: 800\n",
    );
    fs::write(dir.path().join("murmur.yaml"), &manifest).unwrap();
    fs::write(dir.path().join("tool.wasm"), b"\0asm").unwrap();
    build_in(dir.path()).success();

    let mut archive =
        ZipArchive::new(fs::File::open(dir.path().join("out.mur.zip")).unwrap()).unwrap();
    let mut packed = String::new();
    archive
        .by_name("murmur.yaml")
        .unwrap()
        .read_to_string(&mut packed)
        .unwrap();
    assert_eq!(packed, manifest);
}

#[test]
fn an_invalid_artifact_name_fails_the_build() {
    let dir = tempdir().unwrap();
    fs::write(
        dir.path().join("murmur.yaml"),
        "name: My Tool\nversion: 0.1.0\nruntime: skill\nrequires_files: []\n",
    )
    .unwrap();

    Command::cargo_bin("mur")
        .unwrap()
        .args(["build", dir.path().to_str().unwrap()])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("error[E-BLD-001]:")
                .and(predicate::str::contains("invalid artifact name 'My Tool'")),
        );
}

/// An artifact's own name is bare: `mur build` refuses `name: ana/tool-x`, naming where the scope
/// comes from, and packs `name: tool-x` as written.
#[test]
fn a_scoped_artifact_name_is_refused_and_the_bare_name_builds() {
    let dir = tempdir().unwrap();
    let manifest = |name: &str| {
        fs::write(
            dir.path().join("murmur.yaml"),
            format!(
                "name: {name}\nversion: 0.1.0\nruntime: wasm\nrequires_files:\n  - tool.wasm\n"
            ),
        )
        .unwrap();
    };
    fs::write(dir.path().join("tool.wasm"), b"\0asm").unwrap();
    let build = || {
        Command::cargo_bin("mur")
            .unwrap()
            .current_dir(dir.path())
            .args(["build", dir.path().to_str().unwrap()])
            .assert()
    };

    manifest("ana/tool-x");
    build()
        .failure()
        .stderr(predicate::str::contains(
            "error[E-BLD-001]: invalid artifact name 'ana/tool-x': an artifact's own name carries \
             no scope, so it may not contain '/'",
        ))
        .stderr(predicate::str::contains(
            "hint: the scope belongs to the source the artifact is published from, not to the \
             artifact: name it 'tool-x', and it installs as <owner>/tool-x from \
             github.com/<owner> or as local/tool-x from its .mur.zip file",
        ));
    assert!(!dir.path().join("tool-x-0.1.0.mur.zip").exists());

    manifest("tool-x");
    build().success();
    let built = dir.path().join("tool-x-0.1.0.mur.zip");
    let mut packed = String::new();
    ZipArchive::new(fs::File::open(&built).unwrap())
        .unwrap()
        .by_name("murmur.yaml")
        .unwrap()
        .read_to_string(&mut packed)
        .unwrap();
    assert!(packed.contains("name: tool-x\n"), "{packed}");
}

#[test]
fn a_declared_cargo_toml_warns_but_the_build_still_succeeds() {
    let dir = tempdir().unwrap();
    fs::write(
        dir.path().join("murmur.yaml"),
        "name: sources-shipped\nversion: 0.1.0\nruntime: wasm\nrequires_files:\n  - tool.wasm\n  - Cargo.toml\n",
    )
    .unwrap();
    fs::write(dir.path().join("tool.wasm"), b"\0asm").unwrap();
    fs::write(dir.path().join("Cargo.toml"), "[package]\n").unwrap();

    Command::cargo_bin("mur")
        .unwrap()
        .args(["build", dir.path().to_str().unwrap()])
        .assert()
        .success()
        .stderr(
            predicate::str::contains("warning[W-BLD-003]:")
                .and(predicate::str::contains("Cargo.toml")),
        );

    assert!(dir.path().join("sources-shipped-0.1.0.mur.zip").exists());
}

/// The lints are silent on a well-formed artifact: the in-workspace fixture builds with an
/// empty stderr, so a warning line means something actually changed.
#[test]
fn the_happy_fixture_builds_without_any_warning() {
    let fixture = fixture_path("happy");
    let dir = tempdir().unwrap();
    copy_dir_all(&fixture, dir.path());

    Command::cargo_bin("mur")
        .unwrap()
        .args(["build", dir.path().to_str().unwrap()])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());
}

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn copy_dir_all(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let ty = entry.file_type().unwrap();
        let target = dst.join(entry.file_name());

        if ty.is_dir() {
            copy_dir_all(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}
