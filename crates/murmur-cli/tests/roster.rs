//! `roster.yaml` admission through `mur doctor`, against real stores in temp directories.
//!
//! Every case drives the `mur` binary with `HOME` pointed at a temp dir, so the global store is
//! `<home>/.murmur/artifacts` and nothing on the machine running the suite is read.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use murmur_artifact::{
    sha256_hex, write_lockfile_atomic, LockOrigin, LockedArtifact, LockedSha256, MurmurLock,
    LOCK_VERSION,
};
use tempfile::TempDir;

const SERVES: &str = "exports:\n  peer_tasks:\n    accept: true\n";
const REFUSES: &str = "exports:\n  peer_tasks:\n    accept: false\n";
const AUTHENTICATED: &str = "network:\n  authentication:\n    scheme: bearer\n";

const S1_ROSTER: &str = "roster_version: 1
members:
  - name: planner
    capsule: planner
    version: 0.3.0
    entry: true
  - name: coder
    capsule: coder
    version: 1.2.0
  - name: reviewer
    capsule: reviewer
    version: 0.9.0
reachability:
  - from: planner
    to: [coder, reviewer]
  - from: reviewer
    to: [coder]
";

// ── Fixtures ─────────────────────────────────────────────────────────────────

/// A capsule's `murmur.yaml`, serving peers and authenticating its door as asked.
fn manifest(name: &str, version: &str, serves: bool, authenticated: bool) -> String {
    let mut yaml = format!("name: {name}\nversion: {version}\nartifacts: []\n");
    if serves {
        yaml.push_str(SERVES);
    }
    if authenticated {
        yaml.push_str(AUTHENTICATED);
    }
    yaml
}

/// Install a capsule into `store_root` whose packed `murmur.yaml` is exactly `manifest_yaml`.
/// Admission reads that one entry, so no payload is needed. Returns the stored bytes' sha256.
fn install_capsule(store_root: &Path, name: &str, version: &str, manifest_yaml: &str) -> String {
    use std::io::Write;

    let mut cursor = std::io::Cursor::new(Vec::<u8>::new());
    {
        let mut zip = zip::ZipWriter::new(&mut cursor);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zip.start_file("murmur.yaml", options).unwrap();
        zip.write_all(manifest_yaml.as_bytes()).unwrap();
        zip.finish().unwrap();
    }
    let bytes = cursor.into_inner();
    let sha256 = sha256_hex(&bytes);
    murmur_artifact::LocalRegistry::new(store_root)
        .store_installed_overwrite(
            murmur_artifact::ArtifactMeta {
                name: name.to_string(),
                version: version.to_string(),
                runtime: murmur_artifact::RuntimeType::Wasm,
                artifact_runtime: "capsule".to_string(),
                platforms: Vec::new(),
                description: None,
                tags: Vec::new(),
                wit_contracts: None,
            },
            &bytes,
            &sha256,
        )
        .unwrap();
    sha256
}

fn project_store(project_dir: &Path) -> PathBuf {
    project_dir.join(".murmur").join("artifacts")
}

fn global_store(home: &TempDir) -> PathBuf {
    home.path().join(".murmur").join("artifacts")
}

/// A `murmur.yaml` declaring no artifacts: doctor's checklist is empty, so the roster is the only
/// thing that can fail it.
fn create_project(project_dir: &Path) {
    fs::write(
        project_dir.join("murmur.yaml"),
        "name: roster-fixture\nversion: 0.0.1\nartifacts: []\n",
    )
    .unwrap();
}

fn write_roster(project_dir: &Path, roster_yaml: &str) {
    fs::write(project_dir.join("roster.yaml"), roster_yaml).unwrap();
}

fn write_lock(project_dir: &Path, name: &str, resolved_version: &str, sha256: &str) {
    write_lockfile_atomic(
        &project_dir.join("murmur.lock"),
        &MurmurLock {
            lock_version: LOCK_VERSION,
            artifacts: vec![LockedArtifact {
                name: name.to_string(),
                resolved_version: resolved_version.to_string(),
                sha256: LockedSha256::any(sha256.to_string()),
                origin: LockOrigin::Operator,
            }],
        },
    )
    .unwrap();
}

struct Doctor {
    success: bool,
    stdout: String,
    stderr: String,
}

fn mur_doctor(home: &TempDir, project_dir: &Path) -> Doctor {
    let output = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .current_dir(project_dir)
        .arg("doctor")
        .output()
        .unwrap();
    Doctor {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// A project holding S1's roster, with all three members installed in the project store, each
/// serving peers behind an authenticated door. Returns each capsule's stored sha256.
fn s1_project(project_dir: &Path) -> BTreeMap<&'static str, String> {
    create_project(project_dir);
    write_roster(project_dir, S1_ROSTER);
    let store = project_store(project_dir);
    [
        ("planner", "0.3.0"),
        ("coder", "1.2.0"),
        ("reviewer", "0.9.0"),
    ]
    .into_iter()
    .map(|(name, version)| {
        (
            name,
            install_capsule(&store, name, version, &manifest(name, version, true, true)),
        )
    })
    .collect()
}

/// Doctor refused the roster with `code`: a non-zero exit, the code on stderr, the `✗` line in
/// the block, and a `Fix:` entry. Every `needle` appears in the refusal line.
fn assert_refused(doctor: &Doctor, code: &str, needles: &[&str]) {
    assert!(
        !doctor.success,
        "doctor should fail\nstdout:\n{}\nstderr:\n{}",
        doctor.stdout, doctor.stderr
    );
    assert!(
        doctor.stderr.contains(&format!("error[{code}]")),
        "expected {code} on stderr:\n{}",
        doctor.stderr
    );
    let refusal = doctor
        .stdout
        .lines()
        .find(|line| line.trim_start().starts_with('\u{2717}') && line.contains("roster.yaml"))
        .unwrap_or_else(|| panic!("no roster refusal line in:\n{}", doctor.stdout));
    for needle in needles {
        assert!(
            refusal.contains(needle),
            "expected {needle:?} in {refusal:?}"
        );
    }
    assert!(
        doctor.stdout.contains("Fix:"),
        "a refusal adds a Fix: entry:\n{}",
        doctor.stdout
    );
}

fn assert_admitted(doctor: &Doctor) {
    assert!(
        doctor.success,
        "doctor should pass\nstdout:\n{}\nstderr:\n{}",
        doctor.stdout, doctor.stderr
    );
    assert!(
        !doctor.stderr.contains("E-ROS") && !doctor.stderr.contains("E-REG-005"),
        "{}",
        doctor.stderr
    );
    assert!(doctor.stdout.contains("Roster\n"), "{}", doctor.stdout);
}

/// Every file and directory under `root`, with each file's bytes.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    let mut entries = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(listing) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in listing.flatten() {
            let path = entry.path();
            if path.is_dir() {
                entries.insert(path.clone(), None);
                pending.push(path);
            } else {
                entries.insert(path.clone(), Some(fs::read(&path).unwrap()));
            }
        }
    }
    entries
}

// ── S1–S3: admitted rosters ──────────────────────────────────────────────────

#[test]
fn s1_doctor_prints_the_admitted_roster_and_passes() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    s1_project(project.path());

    let doctor = mur_doctor(&home, project.path());
    assert_admitted(&doctor);
    let block: Vec<&str> = doctor
        .stdout
        .lines()
        .skip_while(|line| *line != "Roster")
        .take_while(|line| !line.is_empty())
        .collect();
    assert_eq!(
        block,
        [
            "Roster".to_string(),
            format!(
                "  file: {}",
                project
                    .path()
                    .canonicalize()
                    .unwrap()
                    .join("roster.yaml")
                    .display()
            ),
            "  planner    planner@0.3.0    entry   serves peers    authenticated door".to_string(),
            "  coder      coder@1.2.0              serves peers    authenticated door".to_string(),
            "  reviewer   reviewer@0.9.0           serves peers    authenticated door".to_string(),
            "  reachability: planner \u{2192} coder, planner \u{2192} reviewer, reviewer \u{2192} coder"
                .to_string(),
        ]
    );
}

#[test]
fn s2_a_single_public_member_is_admitted_with_no_reachability() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    create_project(project.path());
    install_capsule(
        &project_store(project.path()),
        "solo",
        "1.0.0",
        &manifest("solo", "1.0.0", false, false),
    );
    write_roster(
        project.path(),
        "roster_version: 1\nmembers:\n  - name: solo\n    capsule: solo\n    version: 1.0.0\n    entry: true\n",
    );

    let doctor = mur_doctor(&home, project.path());
    assert_admitted(&doctor);
    assert!(
        doctor.stdout.contains("refuses peers   public door"),
        "{}",
        doctor.stdout
    );
    assert!(
        doctor.stdout.contains("  reachability: none\n"),
        "{}",
        doctor.stdout
    );
}

#[test]
fn s3_all_expands_to_the_members_that_both_serve_peers() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    create_project(project.path());
    let store = project_store(project.path());
    for (name, serves) in [("a", true), ("b", true), ("c", false), ("d", false)] {
        install_capsule(
            &store,
            name,
            "1.0.0",
            &manifest(name, "1.0.0", serves, true),
        );
    }
    write_roster(
        project.path(),
        "roster_version: 1\nmembers:\n  - {name: a, capsule: a, version: 1.0.0, entry: true}\n  \
         - {name: b, capsule: b, version: 1.0.0}\n  - {name: c, capsule: c, version: 1.0.0}\n  \
         - {name: d, capsule: d, version: 1.0.0}\nreachability: all\n",
    );

    let doctor = mur_doctor(&home, project.path());
    assert_admitted(&doctor);
    assert!(
        doctor
            .stdout
            .contains("  reachability (all): a \u{2192} b\n"),
        "{}",
        doctor.stdout
    );

    // With `a` refusing peers too, `all` expands to nothing, so public doors are admitted.
    for (name, serves) in [("a", false), ("b", true), ("c", false), ("d", false)] {
        install_capsule(
            &store,
            name,
            "1.0.0",
            &manifest(name, "1.0.0", serves, false),
        );
    }
    let doctor = mur_doctor(&home, project.path());
    assert_admitted(&doctor);
    assert!(
        doctor.stdout.contains("  reachability: none\n"),
        "{}",
        doctor.stdout
    );
}

// ── S4–S10: refusals ─────────────────────────────────────────────────────────

#[test]
fn s4_no_entry_member_or_several_is_e_ros_002() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    s1_project(project.path());

    for roster in [
        S1_ROSTER.replace("    entry: true\n", ""),
        S1_ROSTER
            .replace("    entry: true\n", "    entry: false\n")
            .replace("version: 1.2.0\n", "version: 1.2.0\n    entry: false\n")
            .replace("version: 0.9.0\n", "version: 0.9.0\n    entry: false\n"),
    ] {
        write_roster(project.path(), &roster);
        assert_refused(
            &mur_doctor(&home, project.path()),
            "E-ROS-002",
            &["no member"],
        );
    }

    write_roster(
        project.path(),
        &S1_ROSTER.replace("version: 1.2.0\n", "version: 1.2.0\n    entry: true\n"),
    );
    assert_refused(
        &mur_doctor(&home, project.path()),
        "E-ROS-002",
        &["'planner'", "'coder'"],
    );
}

#[test]
fn s5_a_duplicate_name_is_e_ros_003_installed_or_not() {
    let roster = "roster_version: 1\nmembers:\n  - {name: coder, capsule: coder, version: 1.2.0, entry: true}\n  \
                  - {name: coder, capsule: coder, version: 1.2.0, entry: true}\n";
    let home = tempfile::tempdir().unwrap();

    let installed = tempfile::tempdir().unwrap();
    s1_project(installed.path());
    write_roster(installed.path(), roster);
    let empty = tempfile::tempdir().unwrap();
    create_project(empty.path());
    write_roster(empty.path(), roster);

    let refusals: Vec<String> = [installed.path(), empty.path()]
        .into_iter()
        .map(|project| {
            let doctor = mur_doctor(&home, project);
            assert_refused(&doctor, "E-ROS-003", &["'coder'"]);
            assert!(!doctor.stderr.contains("E-ROS-002"), "{}", doctor.stderr);
            doctor
                .stderr
                .lines()
                .find(|line| line.contains("E-ROS-003"))
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(refusals[0], refusals[1]);
}

#[test]
fn s6_a_rule_naming_no_member_is_e_ros_004() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    s1_project(project.path());

    write_roster(
        project.path(),
        &S1_ROSTER.replace("to: [coder, reviewer]", "to: [coder, tester]"),
    );
    assert_refused(
        &mur_doctor(&home, project.path()),
        "E-ROS-004",
        &["'tester'"],
    );

    write_roster(
        project.path(),
        &S1_ROSTER.replace("from: reviewer", "from: ghost"),
    );
    assert_refused(
        &mur_doctor(&home, project.path()),
        "E-ROS-004",
        &["'ghost'"],
    );
}

#[test]
fn s7_a_callee_that_does_not_serve_peers_is_e_ros_005() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    s1_project(project.path());
    let store = project_store(project.path());

    for reviewer in [
        manifest("reviewer", "0.9.0", false, true),
        format!("name: reviewer\nversion: 0.9.0\nartifacts: []\n{REFUSES}{AUTHENTICATED}"),
    ] {
        install_capsule(&store, "reviewer", "0.9.0", &reviewer);
        write_roster(project.path(), S1_ROSTER);
        assert_refused(
            &mur_doctor(&home, project.path()),
            "E-ROS-005",
            &["'reviewer'"],
        );

        // A caller need not serve peers.
        write_roster(
            project.path(),
            &S1_ROSTER.replace("to: [coder, reviewer]", "to: [coder]"),
        );
        assert_admitted(&mur_doctor(&home, project.path()));
    }
}

#[test]
fn s8_a_public_door_in_a_roster_with_traffic_is_e_ros_006() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    s1_project(project.path());
    let store = project_store(project.path());

    install_capsule(
        &store,
        "coder",
        "1.2.0",
        &manifest("coder", "1.2.0", true, false),
    );
    assert_refused(
        &mur_doctor(&home, project.path()),
        "E-ROS-006",
        &["'coder'"],
    );

    install_capsule(
        &store,
        "coder",
        "1.2.0",
        &manifest("coder", "1.2.0", true, true),
    );
    install_capsule(
        &store,
        "notes",
        "1.0.0",
        &manifest("notes", "1.0.0", false, false),
    );
    write_roster(
        project.path(),
        &S1_ROSTER.replace(
            "reachability:",
            "  - name: notes\n    capsule: notes\n    version: 1.0.0\nreachability:",
        ),
    );
    assert_refused(
        &mur_doctor(&home, project.path()),
        "E-ROS-006",
        &["'notes'"],
    );
}

#[test]
fn s9_an_uninstalled_or_unreadable_member_is_e_ros_007() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    s1_project(project.path());

    write_roster(
        project.path(),
        &S1_ROSTER.replace("version: 1.2.0", "version: 9.9.9"),
    );
    assert_refused(
        &mur_doctor(&home, project.path()),
        "E-ROS-007",
        &["'coder'", "coder@9.9.9"],
    );

    write_roster(project.path(), S1_ROSTER);
    install_capsule(
        &project_store(project.path()),
        "coder",
        "1.2.0",
        "name: [unclosed\n",
    );
    assert_refused(
        &mur_doctor(&home, project.path()),
        "E-ROS-007",
        &["'coder'", "coder@1.2.0"],
    );
}

#[test]
fn s9_a_member_installed_only_in_the_global_store_resolves() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    s1_project(project.path());
    fs::remove_dir_all(project_store(project.path()).join("coder")).unwrap();
    install_capsule(
        &global_store(&home),
        "coder",
        "1.2.0",
        &manifest("coder", "1.2.0", true, true),
    );

    assert_admitted(&mur_doctor(&home, project.path()));
}

#[test]
fn s10_murmur_lock_verifies_a_member_and_is_never_written() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let hashes = s1_project(project.path());
    let lock_path = project.path().join("murmur.lock");

    assert_admitted(&mur_doctor(&home, project.path()));
    assert!(!lock_path.exists(), "admission never creates murmur.lock");

    write_lock(project.path(), "coder", "1.2.0", &hashes["coder"]);
    let before = fs::read(&lock_path).unwrap();
    assert_admitted(&mur_doctor(&home, project.path()));
    assert_eq!(fs::read(&lock_path).unwrap(), before);

    for (version, sha256) in [
        ("1.2.0", "0".repeat(64)),
        ("1.1.0", hashes["coder"].clone()),
    ] {
        write_lock(project.path(), "coder", version, &sha256);
        let before = fs::read(&lock_path).unwrap();
        let doctor = mur_doctor(&home, project.path());
        assert_refused(&doctor, "E-REG-005", &["'coder'", "murmur.lock conflict"]);
        assert_eq!(fs::read(&lock_path).unwrap(), before);
    }
}

// ── S11: malformed ───────────────────────────────────────────────────────────

#[test]
fn s11_a_malformed_roster_is_e_ros_001_naming_the_key_path() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    s1_project(project.path());

    for (roster, needles) in [
        ("members: [\n".to_string(), &["YAML syntax error"][..]),
        (
            S1_ROSTER.replace("roster_version: 1", "roster_version: 2"),
            &["roster_version"][..],
        ),
        (
            S1_ROSTER.replace("reachability:", "reachabilty:"),
            &["reachabilty", "unknown key"][..],
        ),
        (
            S1_ROSTER.replace("version: 1.2.0", "version: 1.2.0\n    sha: abc"),
            &["members[1].sha", "member 'coder'"][..],
        ),
        (
            S1_ROSTER.replace("version: 1.2.0", "version: latest"),
            &["members[1].version", "member 'coder'"][..],
        ),
        (
            S1_ROSTER.replace("name: planner", "name: Planner"),
            &["members[0].name", "'Planner'"][..],
        ),
    ] {
        write_roster(project.path(), &roster);
        assert_refused(&mur_doctor(&home, project.path()), "E-ROS-001", needles);
    }
}

// ── S12: no roster, no change ────────────────────────────────────────────────

#[test]
fn s12_without_a_roster_doctor_prints_no_roster_block() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    create_project(project.path());

    let without = mur_doctor(&home, project.path());
    assert!(without.success, "{}", without.stderr);
    assert!(!without.stdout.contains("Roster"), "{}", without.stdout);
    assert!(without.stdout.contains("All checks passed."));

    // A roster that admits adds its block and nothing else.
    s1_project(project.path());
    let with = mur_doctor(&home, project.path());
    assert_admitted(&with);
    let block_free: String = {
        let mut lines = Vec::new();
        let mut in_block = false;
        for line in with.stdout.lines() {
            if line == "Roster" {
                in_block = true;
            }
            if !in_block {
                lines.push(line);
            }
            if in_block && line.is_empty() {
                in_block = false;
            }
        }
        lines.join("\n") + "\n"
    };
    fs::remove_file(project.path().join("roster.yaml")).unwrap();
    let after = mur_doctor(&home, project.path());
    assert_eq!(after.stdout, block_free);
    assert_eq!(after.stdout, without.stdout);
}

#[test]
fn s12_mur_run_never_reads_roster_yaml() {
    const DRIVER_NAME: &str = "murmur-driver-anthropic";
    const DRIVER_VERSION: &str = "0.1.4";

    let server = common::ScriptedServer::start(vec![serde_json::json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": "done"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()]);
    let home = tempfile::tempdir().unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let driver_artifact = common::create_driver_artifact(
        artifacts.path(),
        DRIVER_NAME,
        DRIVER_VERSION,
        &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    let endpoint = &server.endpoint;
    fs::write(
        project.path().join("murmur.yaml"),
        format!(
            "name: roster-run\nversion: 0.1.0\nartifacts:\n  - name: {DRIVER_NAME}\n    version: {DRIVER_VERSION}\n    runtime: driver\n    gateway:\n      endpoint: {endpoint}\n      api_key: test-key\ncapabilities:\n  network:\n    allow:\n      - {endpoint}\ninference:\n  transport: http\n  model: test-model\n  driver:\n    artifact: {DRIVER_NAME}\n"
        ),
    )
    .unwrap();
    common::install_artifact_to_project(project.path(), &driver_artifact).success();
    write_roster(project.path(), "roster_version: 9\nmembers: nonsense\n");
    let task = project.path().join("task.md");
    fs::write(&task, "say done").unwrap();

    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("mur"))
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .current_dir(project.path())
        .args(["run", "--manifest", "murmur.yaml", "--task"])
        .arg(&task)
        .arg("--json")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = child.stderr.take().unwrap();
    let stderr = std::thread::spawn(move || {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut BufReader::new(stderr), &mut text).ok();
        text
    });
    let mut readiness = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut readiness)
        .unwrap();
    let status = child.wait().unwrap();
    let stderr = stderr.join().unwrap();

    assert!(!stderr.contains("E-ROS"), "{stderr}");
    let parsed: serde_json::Value = serde_json::from_str(readiness.trim())
        .unwrap_or_else(|_| panic!("readiness line {readiness:?}; stderr:\n{stderr}"));
    assert!(!parsed["url"].as_str().unwrap_or("").is_empty(), "{parsed}");
    assert_eq!(status.code(), Some(0), "{stderr}");
}

// ── S13: no side effects, fixed order ────────────────────────────────────────

#[test]
fn s13_admission_creates_changes_and_removes_nothing() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let hashes = s1_project(project.path());
    fs::remove_dir_all(project_store(project.path()).join("reviewer")).unwrap();
    install_capsule(
        &global_store(&home),
        "reviewer",
        "0.9.0",
        &manifest("reviewer", "0.9.0", true, true),
    );
    write_lock(project.path(), "coder", "1.2.0", &hashes["coder"]);

    let project_before = snapshot(project.path());
    let global_before = snapshot(&global_store(&home));
    let registry = capsule_runtime_registry(&home, project.path());
    let lock = murmur_artifact::read_lockfile(&project.path().join("murmur.lock")).unwrap();
    let admitted =
        capsule_runtime::admit_roster_file(project.path(), &registry, Some(&lock)).unwrap();
    assert_eq!(admitted.members().len(), 3);
    assert_eq!(snapshot(project.path()), project_before);
    assert_eq!(snapshot(&global_store(&home)), global_before);

    // A refused roster leaves everything as it was too.
    write_roster(
        project.path(),
        &S1_ROSTER.replace("version: 1.2.0", "version: 1.1.0"),
    );
    let project_before = snapshot(project.path());
    capsule_runtime::admit_roster_file(project.path(), &registry, Some(&lock)).unwrap_err();
    assert_eq!(snapshot(project.path()), project_before);
    assert_eq!(snapshot(&global_store(&home)), global_before);
}

#[test]
fn s13_a_duplicate_and_an_unknown_rule_name_is_e_ros_003_every_time() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    s1_project(project.path());
    write_roster(
        project.path(),
        "roster_version: 1\nmembers:\n  - {name: coder, capsule: coder, version: 1.2.0, entry: true}\n  \
         - {name: coder, capsule: coder, version: 1.2.0}\nreachability:\n  - from: coder\n    \
         to: [ghost]\n",
    );
    for _ in 0..3 {
        let doctor = mur_doctor(&home, project.path());
        assert_refused(&doctor, "E-ROS-003", &["'coder'"]);
        assert!(!doctor.stderr.contains("E-ROS-004"), "{}", doctor.stderr);
    }
}

#[test]
fn s14_a_rule_calling_the_entry_member_is_e_ros_008() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    s1_project(project.path());
    let calls_entry = S1_ROSTER.replace(
        "from: reviewer\n    to: [coder]",
        "from: reviewer\n    to: [coder, planner]",
    );
    assert_ne!(calls_entry, S1_ROSTER);

    write_roster(project.path(), &calls_entry);
    let doctor = mur_doctor(&home, project.path());
    assert_refused(&doctor, "E-ROS-008", &["'reviewer'", "'planner'"]);
    assert!(
        doctor.stdout.contains("- from: planner"),
        "{}",
        doctor.stdout
    );
    assert!(!doctor.stderr.contains("E-ROS-005"), "{}", doctor.stderr);

    // The turned-around edge, `planner → reviewer`, is S1's own roster.
    write_roster(project.path(), S1_ROSTER);
    assert_admitted(&mur_doctor(&home, project.path()));

    // Refused before any lookup: no member need be installed.
    let empty = tempfile::tempdir().unwrap();
    create_project(empty.path());
    write_roster(empty.path(), &calls_entry);
    let doctor = mur_doctor(&home, empty.path());
    assert_refused(&doctor, "E-ROS-008", &["'reviewer'", "'planner'"]);
    assert!(!doctor.stderr.contains("E-ROS-007"), "{}", doctor.stderr);

    // An entry member that does not serve peers is still refused as the entry member.
    install_capsule(
        &project_store(project.path()),
        "planner",
        "0.3.0",
        &manifest("planner", "0.3.0", false, true),
    );
    write_roster(project.path(), &calls_entry);
    let doctor = mur_doctor(&home, project.path());
    assert_refused(&doctor, "E-ROS-008", &["'reviewer'", "'planner'"]);
    assert!(!doctor.stderr.contains("E-ROS-005"), "{}", doctor.stderr);

    // A name that is no member is refused first.
    write_roster(
        project.path(),
        &calls_entry.replace("to: [coder, reviewer]", "to: [coder, tester]"),
    );
    let doctor = mur_doctor(&home, project.path());
    assert_refused(&doctor, "E-ROS-004", &["'tester'"]);
    assert!(!doctor.stderr.contains("E-ROS-008"), "{}", doctor.stderr);
}

/// The project store, then the global store: the order `mur doctor` and `mur run --capsule` resolve
/// in, with only a miss in the first falling through.
struct ProjectThenGlobal {
    project: murmur_artifact::LocalRegistry,
    global: murmur_artifact::LocalRegistry,
}

impl murmur_artifact::Registry for ProjectThenGlobal {
    fn resolve(
        &self,
        name: &str,
        version: &str,
    ) -> Result<murmur_artifact::ResolvedArtifact, murmur_artifact::RegistryError> {
        self.resolve_with_platform(name, version, None)
    }

    fn resolve_with_platform(
        &self,
        name: &str,
        version: &str,
        platform: Option<&str>,
    ) -> Result<murmur_artifact::ResolvedArtifact, murmur_artifact::RegistryError> {
        match self.project.resolve_with_platform(name, version, platform) {
            Err(murmur_artifact::RegistryError::NotFound { .. }) => {
                self.global.resolve_with_platform(name, version, platform)
            }
            other => other,
        }
    }

    fn publish(
        &self,
        _: murmur_artifact::ArtifactMeta,
        _: &[u8],
    ) -> Result<murmur_artifact::PublishResult, murmur_artifact::RegistryError> {
        unreachable!("admission never publishes")
    }

    fn list_index(
        &self,
    ) -> Result<Vec<murmur_artifact::ArtifactMeta>, murmur_artifact::RegistryError> {
        unreachable!("admission never lists")
    }
}

fn capsule_runtime_registry(home: &TempDir, project_dir: &Path) -> ProjectThenGlobal {
    ProjectThenGlobal {
        project: murmur_artifact::LocalRegistry::new(project_store(project_dir)),
        global: murmur_artifact::LocalRegistry::new(global_store(home)),
    }
}
