//! `mur run`'s containment surface: `--explain-scope`, `--containment`, and the refusal when a
//! declared floor is stronger than the host.
//!
//! Every assertion here is host-independent on purpose, and staying that way took real care once
//! `sealed` became achievable. It is no longer true that no host can reach it: a Linux box with a
//! usable Landlock ABI, unprivileged user namespaces and the shipped `mur-sealed` AppArmor profile
//! does. A test that hardcodes "sealed always refuses" would therefore pass on CI (containers, no
//! profile) and fail on exactly the machine the feature was built for — the worst possible place
//! for a test to break.
//!
//! So the `sealed` cases below **ask the host first**, via `--explain-scope --json`, and assert the
//! branch that host is actually in. Both branches are asserted; neither is skipped.
//!
//! Nothing here claims that `scoped` or `sealed` actually contains anything at the kernel level —
//! that can only be checked by hand on a real Linux host, per
//! `docs/content/reference/sealed-containment-manual-verification.md`.

use std::fs;
use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

/// A script capsule: no `inference:` block, so `mur run` looks for a root `*.wasm`. The bytes are
/// a bare module header — enough to be discovered and read, and deliberately not a valid
/// component, so a run that gets *past* the containment gate fails loudly at compile time. That
/// is what lets the "no declaration does not refuse" test below prove the gate was not hit.
fn write_project(dir: &Path, capabilities_yaml: &str) {
    fs::write(
        dir.join("murmur.yaml"),
        format!("name: containment-fixture\nversion: 0.0.1\n{capabilities_yaml}"),
    )
    .unwrap();
    fs::write(dir.join("capsule.wasm"), b"\0asm\x01\0\0\0").unwrap();
}

fn mur_run(home: &TempDir, project_dir: &Path, args: &[&str]) -> assert_cmd::assert::Assert {
    Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .current_dir(project_dir)
        .arg("run")
        .arg("--manifest")
        .arg("murmur.yaml")
        .args(args)
        .assert()
}

/// What this host reports it can back, read from `--explain-scope --json` rather than assumed.
///
/// Uses a throwaway project so the caller's fixture is untouched, and a declared floor of
/// `advisory` so the probe answer is the only thing that varies.
fn host_achieved_containment() -> String {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_project(project.path(), "capabilities:\n  containment: advisory\n");

    let output = mur_run(&home, project.path(), &["--explain-scope", "--json"])
        .success()
        .get_output()
        .stdout
        .clone();
    let report: serde_json::Value =
        serde_json::from_str(String::from_utf8(output).unwrap().trim()).unwrap();
    report["achieved_containment"].as_str().unwrap().to_string()
}

/// On a host that cannot back `sealed`, the refusal is `E-CAP-003` and it names the *specific*
/// missing mechanism — the AppArmor profile, `CAP_SYS_ADMIN` inside a container, or the kernel —
/// rather than one fixed sentence. On a host that can, the same manifest launches and gets far
/// enough to fail on the deliberately-invalid component instead.
#[test]
fn sealed_refuses_with_an_actionable_reason_unless_the_host_can_back_it() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_project(project.path(), "capabilities:\n  containment: sealed\n");

    if host_achieved_containment() == "sealed" {
        // The gate let it through: it fails later, at the invalid component. Asserting the
        // *absence* of E-CAP-003 is the point — a sealed-capable host must not refuse.
        mur_run(&home, project.path(), &[])
            .failure()
            .stderr(predicate::str::contains("E-CAP-003").not());
        return;
    }

    let assertion = mur_run(&home, project.path(), &[])
        .failure()
        .stderr(predicate::str::contains("E-CAP-003"))
        .stderr(predicate::str::contains("'sealed'"));

    // Exactly one of the mechanism-specific reasons, never a generic "not supported".
    //
    // Compared against `SealedBlocker::ALL` rather than a hand-written list of substrings. The
    // hand-written version was wrong the moment a variant was added: the refusal was correct and
    // specific, the list had not heard of it, and the failure read as "this host cannot do sealed"
    // when the real defect was in the test. Deriving the expected set from the enum means a new
    // blocker can never make this assert lie again.
    let stderr = String::from_utf8(assertion.get_output().stderr.clone()).unwrap();
    let matched = capsule_runtime::sealed::SealedBlocker::ALL
        .iter()
        .find(|blocker| stderr.contains(&blocker.reason()));
    assert!(
        matched.is_some(),
        "the sealed refusal must be one of SealedBlocker's mechanism-specific reasons, got: \
         {stderr}"
    );

    // The refusal lands ahead of workdir creation, so nothing was left behind.
    assert!(
        !project.path().join("workdir").exists(),
        "a refused launch must not create a workdir"
    );
}

#[test]
fn explain_scope_reports_an_unmet_floor_and_still_exits_zero() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_project(project.path(), "capabilities:\n  containment: sealed\n");

    let met = if host_achieved_containment() == "sealed" {
        "floor met: yes"
    } else {
        "floor met: no"
    };
    mur_run(&home, project.path(), &["--explain-scope"])
        .success()
        .stdout(predicate::str::contains("declared:  sealed"))
        .stdout(predicate::str::contains(met));

    assert!(
        !project.path().join("workdir").exists(),
        "--explain-scope must not create a workdir"
    );
}

#[test]
fn explain_scope_json_emits_one_machine_readable_line() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_project(
        project.path(),
        "capabilities:\n  containment: sealed\n  network:\n    allow:\n      - https://api.example.com\n",
    );

    let output = mur_run(&home, project.path(), &["--explain-scope", "--json"])
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).unwrap();
    assert_eq!(
        stdout.lines().filter(|line| !line.is_empty()).count(),
        1,
        "--json must emit exactly one line: {stdout}"
    );

    let report: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(report["declared_containment"], "sealed");
    assert_eq!(
        report["network_allow"],
        serde_json::json!(["https://api.example.com"])
    );
    // `floor_met` follows the host, and the two fields must agree with each other — that
    // consistency is the host-independent claim, not either field's value.
    let achieved_is_sealed = report["achieved_containment"] == "sealed";
    assert_eq!(report["floor_met"], achieved_is_sealed);
    assert_eq!(report["shortfall_reason"].is_null(), achieved_is_sealed);
}

#[test]
fn an_undeclared_manifest_is_not_gated() {
    if capsule_runtime::skip_without_host_support("an_undeclared_manifest_is_not_gated") {
        return;
    }
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_project(
        project.path(),
        "capabilities:\n  shell:\n    allow:\n      - echo\n",
    );

    // Fails later, at the deliberately-invalid component — proving the containment gate let it
    // through rather than refusing a manifest that declared nothing.
    mur_run(&home, project.path(), &[])
        .failure()
        .stderr(predicate::str::contains("E-RUN-001"))
        .stderr(predicate::str::contains("E-CAP-003").not());
}

#[test]
fn the_cli_flag_can_raise_a_floor_the_manifest_never_declared() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_project(
        project.path(),
        "capabilities:\n  shell:\n    allow:\n      - echo\n",
    );

    let assertion = mur_run(&home, project.path(), &["--containment", "sealed"]).failure();
    if host_achieved_containment() != "sealed" {
        assertion.stderr(predicate::str::contains("E-CAP-003"));
    }
}

#[test]
fn the_workspace_config_can_raise_a_floor_the_manifest_never_declared() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_project(
        project.path(),
        "capabilities:\n  shell:\n    allow:\n      - echo\n",
    );
    // `project_mur_config_path()` is cwd-relative, and `mur_run` runs in the project dir.
    fs::create_dir_all(project.path().join(".murmur")).unwrap();
    fs::write(
        project.path().join(".murmur").join("config.yaml"),
        "containment: sealed\n",
    )
    .unwrap();

    mur_run(&home, project.path(), &["--explain-scope"])
        .success()
        .stdout(predicate::str::contains("declared:  sealed"));
}

#[test]
fn an_unknown_containment_flag_value_names_the_accepted_set() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_project(
        project.path(),
        "capabilities:\n  shell:\n    allow:\n      - echo\n",
    );

    mur_run(&home, project.path(), &["--containment", "paranoid"])
        .failure()
        .stderr(predicate::str::contains("E-IO-003"))
        .stderr(predicate::str::contains(
            "--containment must be one of: advisory, scoped, sealed; got 'paranoid'",
        ));
}

#[test]
fn an_unknown_manifest_containment_value_fails_at_parse_time() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_project(project.path(), "capabilities:\n  containment: paranoid\n");

    mur_run(&home, project.path(), &[])
        .failure()
        .stderr(predicate::str::contains("capabilities.containment"))
        .stderr(predicate::str::contains(
            "must be one of: advisory, scoped, sealed",
        ));
}

/// The `runtime_writes` declaration, read the way a consumer for whom the workdir is the
/// deliverable reads it: before anything is staged, off a command that creates nothing.
///
/// The row set itself is asserted in `capsule-runtime`'s own tests, against the declaration
/// function. What is asserted here is the part only the real binary can show — that the key
/// reaches `--explain-scope --json` at all, that each row is the four-key object a consumer
/// parses, and that asking the question leaves no directory behind.
#[test]
fn explain_scope_declares_what_the_runtime_writes_and_creates_nothing() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_project(
        project.path(),
        "capabilities:\n  shell:\n    allow:\n      - bash\n",
    );

    let report = explain_scope_report(&home, project.path(), &[]);
    let writes = report["runtime_writes"]
        .as_array()
        .expect("runtime_writes is an array");
    assert!(!writes.is_empty(), "the runtime writes something");

    for write in writes {
        let object = write.as_object().expect("each row is an object");
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["condition", "kind", "path", "scope"], "{write}");
        assert!(
            ["file", "directory"].contains(&object["kind"].as_str().unwrap()),
            "{write}"
        );
        assert!(
            ["accessible", "session"].contains(&object["scope"].as_str().unwrap()),
            "{write}"
        );
        let condition = object["condition"].as_str().unwrap();
        assert!(
            condition
                .chars()
                .all(|c| c.is_ascii_lowercase() || c == '-'),
            "conditions are kebab-case wire names: {write}"
        );
    }

    let declared: Vec<&str> = writes
        .iter()
        .map(|write| write["path"].as_str().unwrap())
        .collect();
    assert!(declared.contains(&".murmur/<session-id>"), "{declared:?}");
    assert!(declared.contains(&"task.md"), "{declared:?}");

    assert!(
        !project.path().join(".murmur").exists(),
        "--explain-scope answers before a session exists and must create nothing"
    );
    assert!(!project.path().join("workdir").exists());
}

/// Enabling `sealed` changes the set of paths the runtime writes, and the change is visible in
/// the declaration rather than only on disk.
///
/// Host-independent in the same way the rest of this file is: on a host that can back `sealed`
/// the two reports differ by exactly the sealed rows, and on a host that cannot they are equal,
/// because the declaration is keyed on the tier that will actually be used rather than on the
/// class the manifest asked for. Either way, no added row is a new top-level entry in the
/// accessible workdir — which is the property a consumer diffing that directory depends on.
#[test]
fn asking_for_sealed_adds_only_paths_under_the_session_directory() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    write_project(
        project.path(),
        "capabilities:\n  shell:\n    allow:\n      - bash\n",
    );

    let scoped = explain_scope_report(&home, project.path(), &[]);
    let sealed = explain_scope_report(&home, project.path(), &["--containment", "sealed"]);

    let rows = |report: &serde_json::Value| -> Vec<serde_json::Value> {
        report["runtime_writes"].as_array().unwrap().clone()
    };
    let (scoped_rows, sealed_rows) = (rows(&scoped), rows(&sealed));
    for row in &scoped_rows {
        assert!(
            sealed_rows.contains(row),
            "the sealed declaration must keep every scoped row: {row}"
        );
    }

    let added: Vec<&serde_json::Value> = sealed_rows
        .iter()
        .filter(|row| !scoped_rows.contains(row))
        .collect();
    if sealed["achieved_containment"] == "sealed" {
        assert_eq!(sealed["floor_met"], true);
        assert_eq!(
            added.len(),
            2,
            "sealed adds /tmp and /etc staging: {added:?}"
        );
    } else {
        assert!(
            added.is_empty(),
            "a host that cannot seal runs the non-sealed set, whatever the manifest declared: \
             {added:?}"
        );
    }
    for row in added {
        assert_eq!(row["condition"], "sealed");
        assert_eq!(row["scope"], "session");
        assert!(
            row["path"]
                .as_str()
                .unwrap()
                .starts_with(".murmur/<session-id>/"),
            "enabling sealed must add nothing to the top level of the accessible workdir: {row}"
        );
    }
}

/// The `Not protected here` block, read off the real binary on whatever host runs the suite.
///
/// The expectation is derived from `enforcement_tier` and `declared_containment` — the host
/// mechanism and the floor the manifest asked for — rather than from `achieved_containment`, which
/// answers a different question and cannot tell a Landlock-scoped session on a sealed-capable host
/// apart from one running inside a composed root. Deriving it from the field under test would make
/// the assertion hold under any derivation at all.
///
/// The strings themselves are asserted in `capsule-runtime`'s own tests, against the function that
/// derives them and against what a subprocess in a real session actually reaches. What is asserted
/// here is the part only the binary can show: that the JSON key reaches the report, that the human
/// rendering carries the same statements in the same words, and that the block sits between
/// `Containment` and `Effective grants`.
#[test]
fn explain_scope_discloses_what_this_host_does_not_protect() {
    for capabilities in [
        "capabilities:\n  shell:\n    allow:\n      - bash\n",
        "capabilities:\n  containment: sealed\n  shell:\n    allow:\n      - bash\n",
    ] {
        let home = TempDir::new().unwrap();
        let project = TempDir::new().unwrap();
        write_project(project.path(), capabilities);

        let report = explain_scope_report(&home, project.path(), &[]);
        let boundary = &report["filesystem_boundary"];
        let statements = boundary["not_protected"]
            .as_array()
            .expect("filesystem_boundary.not_protected is always an array");

        // The mechanism this session installs: the host's, dropped to Landlock whenever the
        // manifest asked for less than a composed root.
        let composes_a_root = report["enforcement_tier"] == "mountns+pivot_root+landlock+seccomp"
            && report["declared_containment"] == "sealed";
        let expected_restriction = match report["enforcement_tier"].as_str().unwrap() {
            "mountns+pivot_root+landlock+seccomp" if composes_a_root => "absent",
            "mountns+pivot_root+landlock+seccomp" | "landlock+seccomp" => "enforced",
            _ => "advisory",
        };
        assert_eq!(boundary["restriction"], expected_restriction, "{report}");

        let rendered = String::from_utf8(
            mur_run(&home, project.path(), &["--explain-scope"])
                .success()
                .get_output()
                .stdout
                .clone(),
        )
        .unwrap();

        assert_eq!(
            composes_a_root,
            statements.is_empty(),
            "not_protected is empty exactly when a composed root makes the paths absent: {report}"
        );
        assert_eq!(
            composes_a_root,
            !rendered.contains("Not protected here"),
            "the heading is printed exactly when there is something to disclaim:\n{rendered}"
        );

        if composes_a_root {
            continue;
        }

        let heading = rendered.find("\nNot protected here\n").unwrap();
        assert!(
            heading > rendered.find("Containment\n").unwrap(),
            "{rendered}"
        );
        assert!(
            heading < rendered.find("\nEffective grants\n").unwrap(),
            "{rendered}"
        );
        for statement in statements {
            let statement = statement.as_str().unwrap();
            assert!(
                rendered.contains(statement),
                "the human report must carry the JSON statement verbatim: {statement}\n{rendered}"
            );
        }
    }
}

/// One `--explain-scope --json` report, parsed.
fn explain_scope_report(home: &TempDir, project_dir: &Path, extra: &[&str]) -> serde_json::Value {
    let mut args = vec!["--explain-scope", "--json"];
    args.extend_from_slice(extra);
    let output = mur_run(home, project_dir, &args)
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_str(String::from_utf8(output).unwrap().trim()).unwrap()
}
