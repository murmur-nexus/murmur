//! A tool's own `murmur.yaml` declares what each host program it runs needs (`programs:`), and a
//! capsule that pins the tool with an `operator` lock origin and grants the program in
//! `capabilities.shell.allow` gets those needs: staged, reported by `mur run --explain-scope`, and
//! checked by `mur doctor`.
//!
//! Staging runs in-process against a scratch store holding the fixture driver and a tool packing
//! the committed `image-tool` component with a `programs:` block. The needs name things every
//! Linux host running these tests has (`libm.so.6`, `cat`'s own folder) and one it may lack
//! (`system: fonts`), so a test asserts the host-dependent need as granted or missing.

#[path = "common/mod.rs"]
mod common;

use std::{fs, path::Path};

use assert_cmd::Command;
use capsule_runtime::{
    capability_policy_from_runtime_manifest, stage_session, ArtifactRequest, LockExpectation,
    RuntimeError, StageRequest, StagedSession,
};
use murmur_artifact::{
    load_runtime_manifest, ArtifactRuntime, ContainmentClass, LocalRegistry, LockOrigin,
};
use serde_json::{json, Value};
use tempfile::TempDir;

const DRIVER: &str = "local/murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.7";
const TOOL: &str = "local/needs-tool";
const VERSION: &str = "0.1.0";
const ENDPOINT: &str = "http://127.0.0.1:9";

/// Host-portable needs for `cat`, and `ls` named with none.
const CAT_NEEDS: &str = "programs:
  cat:
    needs:
      - library: libm.so.6
      - file: libc.so.6
        beside: libm.so.6
      - program_folder: .
      - system: fonts
  ls:
    needs: []
";

/// A scratch home holding the driver and [`TOOL`] carrying `block`, with the published zips'
/// hashes for a lock.
struct Store {
    home: TempDir,
    _artifacts: TempDir,
    driver_sha: String,
    tool_sha: String,
}

fn publish(block: &str) -> Store {
    let home = TempDir::new().unwrap();
    let artifacts = TempDir::new().unwrap();
    let driver = common::create_driver_artifact(
        artifacts.path(),
        DRIVER,
        DRIVER_VERSION,
        &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver).success();
    let tool = common::create_tool_artifact_with_auth(
        artifacts.path(),
        TOOL,
        VERSION,
        &common::fixture_path("image-tool/tool/image-tool.wasm"),
        block,
    );
    common::publish_local(&home, &tool).success();
    Store {
        driver_sha: murmur_artifact::sha256_hex(&fs::read(&driver).unwrap()),
        tool_sha: murmur_artifact::sha256_hex(&fs::read(&tool).unwrap()),
        home,
        _artifacts: artifacts,
    }
}

/// A project whose manifest pins the driver and [`TOOL`], allowlists `shell_allow`, and carries
/// `extra_shell` under `capabilities.shell`.
fn project(shell_allow: &[&str], extra_shell: &str) -> TempDir {
    let dir = TempDir::new().unwrap();
    let allow: String = shell_allow
        .iter()
        .map(|name| format!("      - {name}\n"))
        .collect();
    fs::write(
        dir.path().join("murmur.yaml"),
        format!(
            "name: needs-capsule\nversion: 0.1.0\n\
             artifacts:\n  - name: {DRIVER}\n    version: {DRIVER_VERSION}\n    runtime: driver\n    \
             gateway:\n      endpoint: {ENDPOINT}\n      api_key: test-key\n  \
             - name: {TOOL}\n    version: {VERSION}\n    runtime: tool\n\
             capabilities:\n  shell:\n    allow:\n{allow}{extra_shell}  network:\n    allow:\n      \
             - {ENDPOINT}\n\
             inference:\n  transport: http\n  model: test-model\n  driver:\n    artifact: {DRIVER}\n"
        ),
    )
    .unwrap();
    dir
}

/// Stages `project` from `store` in-process. With `tool_origin`, the lock pins both artifacts and
/// the tool's pin has that origin; with `None` there is no lock.
fn stage(
    store: &Store,
    project: &TempDir,
    tool_origin: Option<LockOrigin>,
) -> Result<StagedSession, RuntimeError> {
    let manifest_path = project.path().join("murmur.yaml");
    let runtime_manifest = load_runtime_manifest(&manifest_path).unwrap();
    let mut allowlisted_tools = std::collections::HashSet::new();
    let mut requested_artifacts = Vec::new();
    for artifact in &runtime_manifest.artifacts {
        if matches!(artifact.runtime, ArtifactRuntime::Tool) {
            allowlisted_tools.insert(artifact.name.clone());
        }
        requested_artifacts.push(ArtifactRequest {
            name: artifact.name.clone(),
            version: artifact.version.clone(),
            runtime: artifact.runtime.clone(),
            source: artifact.source.clone(),
            on_overflow: artifact.on_overflow,
            config: artifact.config.clone(),
            gateway: artifact.gateway.clone(),
            capabilities: artifact.capabilities.clone(),
        });
    }
    let lock_expectations = tool_origin.map(|origin| {
        vec![
            LockExpectation {
                name: DRIVER.to_string(),
                resolved_version: DRIVER_VERSION.to_string(),
                sha256: store.driver_sha.clone(),
                origin: LockOrigin::Operator,
            },
            LockExpectation {
                name: TOOL.to_string(),
                resolved_version: VERSION.to_string(),
                sha256: store.tool_sha.clone(),
                origin,
            },
        ]
    });
    let local_registry = LocalRegistry::new(store.home.path().join(".murmur").join("artifacts"));
    common::process_home();
    stage_session(
        std::sync::Arc::new(local_registry),
        StageRequest {
            source_supply: None,
            expected_workdir_identity: None,
            parent_workdir_snapshot: None,
            session_deadline: None,
            manifest_file: Some(manifest_path.clone()),
            credentials_file: None,
            manifest_dir: project.path().to_path_buf(),
            capsule_name: runtime_manifest.name.clone(),
            capsule_version: runtime_manifest.version.clone(),
            capsule_artifact: None,
            capsule_component_bytes: Vec::new(),
            artifacts: requested_artifacts,
            allowlisted_tools,
            lock_expectations,
            capability_policy: capability_policy_from_runtime_manifest(&runtime_manifest),
            inference: runtime_manifest.inference.clone(),
            system_prompt_overridden: false,
            context: runtime_manifest.context.clone(),
            context_id: None,
            resume: None,
            forget_session: false,
            otel_endpoint: None,
            eval_config_json: None,
            case_id: None,
            dataset_id: None,
            lifecycle: None,
            lifecycle_override: None,
            trace: None,
            workdir: None,
            bind_addr: "127.0.0.1".to_string(),
            internal_port: None,
            declared_containment_floor: ContainmentClass::Advisory,
            exports: None,
            control: None,
            door_authentication: None,
            spawn_grant: None,
            machine_tokens_per_day: None,
            formation_id: None,
            formation_member: None,
        },
    )
}

/// The staged report's `declared_needs`, as `session_start.effective_grants` serializes it.
fn staged_needs(staged: &StagedSession) -> Value {
    serde_json::to_value(staged.scope_report()).unwrap()["declared_needs"].clone()
}

/// Asserts `needs` is [`CAT_NEEDS`]'s `cat` entry: `libm.so.6`, `libc.so.6` beside it and `cat`'s
/// folder granted, `system fonts` granted or missing.
fn assert_cat_needs(needs: &Value) {
    let entries = needs.as_array().expect("declared_needs is an array");
    assert_eq!(entries.len(), 1, "{needs}");
    let entry = &entries[0];
    assert_eq!(entry["artifact"], TOOL, "{needs}");
    assert_eq!(entry["program"], "cat", "{needs}");
    let granted: Vec<&str> = entry["granted"]
        .as_array()
        .unwrap()
        .iter()
        .map(|need| need["need"].as_str().unwrap())
        .collect();
    let missing: Vec<&str> = entry["missing"]
        .as_array()
        .unwrap()
        .iter()
        .map(|need| need["need"].as_str().unwrap())
        .collect();
    for need in [
        "library libm.so.6",
        "file libc.so.6 beside libm.so.6",
        "program_folder .",
    ] {
        assert!(granted.contains(&need), "{need} granted: {needs}");
    }
    assert!(
        granted.contains(&"system fonts") ^ missing.contains(&"system fonts"),
        "system fonts is granted or missing: {needs}"
    );
    let library = &entry["granted"][0];
    assert!(library["paths"]
        .as_array()
        .unwrap()
        .iter()
        .any(|path| path.as_str().unwrap().ends_with("/libm.so.6")));
    let cat_dir = fs::canonicalize(capsule_runtime::find_on_path("cat").unwrap())
        .unwrap()
        .parent()
        .unwrap()
        .display()
        .to_string();
    assert_eq!(
        entry["granted"][2]["paths"],
        json!([format!("{cat_dir} (list_dir)")]),
        "{needs}"
    );
}

/// The four controls: a need reaches the session only for a granted program of an operator pin.
#[test]
#[allow(clippy::print_stdout)]
fn declared_needs_reach_a_session_only_through_its_own_declarations() {
    let store = publish(CAT_NEEDS);

    // (a) The program with needs is not granted.
    let staged = stage(&store, &project(&["ls"], ""), None).unwrap();
    println!(
        "(a) shell.allow [ls]: declared_needs = {}",
        staged_needs(&staged)
    );
    assert_eq!(staged_needs(&staged), json!([]));

    // (b) Both granted: only cat's needs, under cat.
    let staged = stage(&store, &project(&["cat", "ls"], ""), None).unwrap();
    println!(
        "(b) shell.allow [cat, ls]: declared_needs = {}",
        staged_needs(&staged)
    );
    assert_cat_needs(&staged_needs(&staged));

    // (c) A runtime pin stages, and contributes nothing.
    let staged = stage(
        &store,
        &project(&["cat"], ""),
        Some(LockOrigin::Runtime {
            session: "ses_x".to_string(),
        }),
    )
    .unwrap();
    println!(
        "(c) origin runtime (ses_x): declared_needs = {}",
        staged_needs(&staged)
    );
    assert_eq!(staged_needs(&staged), json!([]));

    // (d) An operator pin and a granted program.
    let staged = stage(&store, &project(&["cat"], ""), Some(LockOrigin::Operator)).unwrap();
    println!(
        "(d) origin operator: declared_needs = {}",
        staged_needs(&staged)
    );
    assert_cat_needs(&staged_needs(&staged));
}

/// `mur run --explain-scope` names each need under its artifact and program, text and JSON, and
/// its JSON is the value staging reports.
#[test]
#[allow(clippy::print_stdout)]
fn explain_scope_reports_the_declared_needs_staging_grants() {
    let store = publish(CAT_NEEDS);
    let project = project(&["cat"], "");
    let explain = |json: bool| {
        let mut args = vec!["run", "--manifest", "murmur.yaml", "--explain-scope"];
        if json {
            args.push("--json");
        }
        let output = Command::cargo_bin("mur")
            .unwrap()
            .env("HOME", store.home.path())
            .current_dir(project.path())
            .args(&args)
            .assert()
            .success()
            .get_output()
            .clone();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(!stderr.contains("W-SEC-009"), "{stderr}");
        String::from_utf8(output.stdout).unwrap()
    };
    let rendered = explain(false);
    let block: Vec<&str> = rendered
        .lines()
        .skip_while(|line| line.trim_start() != "declared needs:")
        .take_while(|line| {
            let line = line.trim_start();
            line == "declared needs:" || line.starts_with("- ")
        })
        .collect();
    println!("{}", block.join("\n"));
    assert_eq!(block.first(), Some(&"  declared needs:"), "{rendered}");
    let lines = &block[1..];
    assert_eq!(lines.len(), 4, "{rendered}");
    let owner = format!("    - {TOOL} (cat): ");
    assert!(
        lines[0].starts_with(&format!("{owner}library libm.so.6 -> /")),
        "{rendered}"
    );
    assert!(lines[0].contains("/libm.so.6"), "{rendered}");
    assert!(
        lines[1].starts_with(&format!("{owner}file libc.so.6 beside libm.so.6 -> /")),
        "{rendered}"
    );
    assert!(
        lines[2].starts_with(&format!("{owner}program_folder . -> /")),
        "{rendered}"
    );
    assert!(lines[2].ends_with(" (list_dir)"), "{rendered}");
    let fonts = lines
        .iter()
        .find(|line| line.contains("system fonts"))
        .expect("system fonts is listed");
    assert!(
        fonts.contains("system fonts -> missing (not found on this host)")
            || fonts.contains("system fonts -> /"),
        "{rendered}"
    );
    let trust_store = rendered.find("  trust store:").unwrap();
    let declared = rendered.find("  declared needs:").unwrap();
    assert!(
        trust_store < declared,
        "declared needs follow the trust store"
    );

    let report: Value = serde_json::from_str(explain(true).trim()).unwrap();
    println!("declared_needs: {}", report["declared_needs"]);
    assert_cat_needs(&report["declared_needs"]);

    let staged = stage(&store, &project, None).unwrap();
    assert_eq!(staged_needs(&staged), report["declared_needs"]);
}

/// This host's os-release `ID`.
fn host_os_id() -> Option<String> {
    ["/etc/os-release", "/usr/lib/os-release"]
        .iter()
        .find_map(|path| fs::read_to_string(path).ok())
        .and_then(|text| {
            text.lines().find_map(|line| {
                line.strip_prefix("ID=")
                    .map(|id| id.trim_matches('"').trim_matches('\'').to_string())
            })
        })
}

/// `mur doctor` warns for each declared library this host lacks, naming the package the tool
/// declares for this host's distribution, and counts each warning.
#[test]
#[allow(clippy::print_stdout)]
fn doctor_names_a_missing_declared_library_and_its_package() {
    let Some(id) = host_os_id() else {
        eprintln!("SKIP — PROVES NOTHING: this host has no os-release ID");
        return;
    };
    let block = format!(
        "programs:\n  cat:\n    needs:\n      - library: libmurmur-absent-fixture.so.9\n        \
         packages:\n          {id}: murmur-fixture-pkg\n      - library: libmurmur-absent-two.so.3\n        \
         packages:\n          not-{id}: other-pkg\n      - library: libm.so.6\n"
    );
    let store = publish(&block);
    let project = project(&["cat"], "");
    let output = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", store.home.path())
        .current_dir(project.path())
        .arg("doctor")
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    let section: Vec<&str> = stdout
        .lines()
        .skip_while(|line| *line != "Declared needs:")
        .take_while(|line| !line.is_empty())
        .collect();
    println!("{}", section.join("\n"));
    let tally = stdout
        .lines()
        .find(|line| line.contains(" check") && line.contains(" found"))
        .unwrap_or_default();
    println!("{tally}");
    assert_eq!(
        section.get(1).copied(),
        Some(format!("  \u{26A0}  {TOOL} (cat): 1 granted, 2 missing").as_str()),
        "{stdout}\n{stderr}"
    );
    let warnings: Vec<&str> = stdout
        .lines()
        .filter(|line| line.contains("warning[W-RUN-011]"))
        .collect();
    assert_eq!(
        warnings,
        [
            format!(
                "  warning[W-RUN-011]: {TOOL} declares library libmurmur-absent-fixture.so.9 for \
                 cat, and this host has none; install murmur-fixture-pkg ({id})"
            )
            .as_str(),
            format!(
                "  warning[W-RUN-011]: {TOOL} declares library libmurmur-absent-two.so.3 for cat, \
                 and this host has none; the artifact names no package for {}",
                capsule_runtime::declared_needs::host_distributions().join(", ")
            )
            .as_str(),
        ],
        "{stdout}"
    );
    assert!(!stdout.contains("declares library libm.so.6"), "{stdout}");
    // The tally counts every warning; these two are among them, each with its `Fix:` line.
    let fixes: Vec<&str> = stdout
        .lines()
        .filter(|line| line.starts_with("Fix: install") && line.contains("libmurmur-absent"))
        .collect();
    assert_eq!(fixes.len(), 2, "{stdout}");
    let counted = tally
        .split(", ")
        .find_map(|part| {
            part.strip_suffix(" warnings.")
                .or(part.strip_suffix(" warning."))
        })
        .and_then(|count| count.parse::<usize>().ok())
        .expect("the tally counts warnings");
    assert!(counted >= 2, "{tally}");

    // Without the block, the section is absent and the two warnings with it.
    let store = publish("");
    let output = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", store.home.path())
        .current_dir(project.path())
        .arg("doctor")
        .output()
        .unwrap();
    let bare = String::from_utf8(output.stdout).unwrap();
    assert!(!bare.contains("Declared needs:"), "{bare}");
    let bare_tally = bare
        .lines()
        .find(|line| line.contains(" check") && line.contains(" found"))
        .unwrap_or_default()
        .to_string();
    let bare_count = bare_tally
        .split(", ")
        .find_map(|part| {
            part.strip_suffix(" warnings.")
                .or(part.strip_suffix(" warning."))
        })
        .and_then(|count| count.parse::<usize>().ok())
        .unwrap_or(0);
    assert_eq!(counted, bare_count + 2, "{tally} vs {bare_tally}");
}

/// A tool whose bundled block names a library by host path (packed without `mur build`) refuses
/// the launch with `E-RUN-062`, naming the artifact, its version and the parser's reason.
#[test]
#[allow(clippy::print_stdout)]
fn a_malformed_programs_block_refuses_the_launch() {
    let store = publish("programs:\n  cat:\n    needs:\n      - library: /usr/lib/libx.so\n");
    let project = project(&["cat"], "");
    let Err(error) = stage(&store, &project, None) else {
        panic!("staging must refuse the block");
    };
    let message = error.to_string();
    println!("{message}");
    assert!(
        matches!(error, RuntimeError::ProgramNeedsMalformed { .. }),
        "{error:?}"
    );
    assert!(
        message.contains(&format!("artifact '{TOOL}@{VERSION}'")),
        "{message}"
    );
    assert!(
        message.contains(
            "field 'programs.cat.needs[0].library' is '/usr/lib/libx.so', an absolute host path"
        ),
        "{message}"
    );

    // `--explain-scope` refuses as the launch would.
    let output = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", store.home.path())
        .current_dir(project.path())
        .args(["run", "--manifest", "murmur.yaml", "--explain-scope"])
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8(output.stderr).unwrap();
    println!("{stderr}");
    assert!(stderr.contains("error[E-RUN-062]:"), "{stderr}");
    assert!(stderr.contains(&format!("{TOOL}@{VERSION}")), "{stderr}");
}

/// A capsule that names the paths itself keeps its own grant and its `W-SEC-009`, and gets the
/// declared needs joined to it.
#[test]
#[allow(clippy::print_stdout)]
fn a_manifest_naming_its_own_paths_keeps_them_beside_the_declared_needs() {
    let store = publish(CAT_NEEDS);
    let scratch = TempDir::new().unwrap();
    let own = fs::canonicalize(scratch.path()).unwrap();
    let project = project(
        &["cat"],
        &format!(
            "    interpreter_runtime:\n      - binary: cat\n        dirs:\n          - path: {}\n            \
             list_dir: false\n",
            own.display()
        ),
    );
    let staged = stage(&store, &project, None).unwrap();
    let report = serde_json::to_value(staged.scope_report()).unwrap();
    println!(
        "interpreter_runtime_grants: {}\ndeclared_needs: {}",
        report["interpreter_runtime_grants"], report["declared_needs"]
    );
    assert_eq!(
        report["interpreter_runtime_grants"],
        json!([format!("cat: {}", own.display())])
    );
    assert_cat_needs(&report["declared_needs"]);

    let doctor = |dir: &Path| {
        let output = Command::cargo_bin("mur")
            .unwrap()
            .env("HOME", store.home.path())
            .current_dir(dir)
            .arg("doctor")
            .output()
            .unwrap();
        format!(
            "{}{}",
            String::from_utf8(output.stdout).unwrap(),
            String::from_utf8(output.stderr).unwrap()
        )
    };
    let own_grant = doctor(project.path());
    assert!(own_grant.contains("warning[W-SEC-009]"), "{own_grant}");
    let needs_only = doctor(self::project(&["cat"], "").path());
    assert!(!needs_only.contains("W-SEC-009"), "{needs_only}");
}
