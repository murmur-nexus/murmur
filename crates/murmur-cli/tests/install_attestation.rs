//! `mur install` checks who published an artifact: the Sigstore bundle published beside each
//! release asset as `<asset file>.sigstore.json`, verified offline against the trusted root
//! `MUR_SIGSTORE_TRUSTED_ROOT` names. A bundle that verifies is kept in the store and recorded as
//! the lock entry's `signer`; one that does not is refused with `E-REG-016`; none, or one in a
//! form this `mur` does not read, installs with `W-REG-005`.
//!
//! GitHub is a loopback stub reached through `MUR_GITHUB_API_BASE`; the bundles and the trusted
//! root are minted per test by `common::attestation`. The `mur` binary, the stores and the lock are
//! real.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    net::TcpListener,
    path::{Path, PathBuf},
    time::Duration,
};

use assert_cmd::Command;
use common::attestation::{sha256_hex, signer_for, BundleSpec, Minter};
use common::github_release::{mur_index, release_assets, wasm_tool_zip, MockGitHub};
use murmur_artifact::read_lockfile;
use serde_json::Value;
use tempfile::TempDir;

const ANA: &str = "ana/artifacts";
const ANA_OWNER: u64 = 1111;
const ANA_CONFIG: &str = "registry:\n  sources:\n    - repo: ana/artifacts\n";

/// How one published version is attested.
enum Attest {
    /// A bundle that verifies, signed by this workflow file.
    By(&'static str),
    /// A bundle that verifies for a payload of other bytes.
    OtherDigest,
    /// A bundle [`BundleSpec::new`] makes for the payload, then changed by the function.
    Edited(fn(&mut BundleSpec)),
    /// No bundle asset.
    Nothing,
}

/// One test's world: the stub, a minted trust root, a `HOME` with `ana/artifacts` configured, and
/// a project directory.
struct World {
    stub: MockGitHub,
    minter: Minter,
    root: PathBuf,
    home: TempDir,
    project: TempDir,
    staging: TempDir,
    /// `tool-git-<version>.mur.zip` bytes by file name, as published.
    bundles: Vec<(String, Vec<u8>)>,
}

impl World {
    fn new() -> Self {
        let minter = Minter::new();
        let staging = tempfile::tempdir().unwrap();
        let root = minter.write_trusted_root(staging.path());
        let home = tempfile::tempdir().unwrap();
        let murmur = home.path().join(".murmur");
        fs::create_dir_all(&murmur).unwrap();
        fs::write(murmur.join("config.yaml"), ANA_CONFIG).unwrap();
        Self {
            stub: MockGitHub::start(),
            minter,
            root,
            home,
            project: tempfile::tempdir().unwrap(),
            staging,
            bundles: Vec::new(),
        }
    }

    /// Publish release `v1` of `ana/artifacts` holding each `(name, version, attest)` and an index
    /// listing them all.
    fn publish(&mut self, artifacts: &[(&str, &str, Attest)]) {
        let dir = self.staging.path().join("v1");
        fs::create_dir_all(&dir).unwrap();
        for (name, version, _) in artifacts {
            wasm_tool_zip(&dir, name, version);
        }
        let index = mur_index(&dir, ANA, "v1", None);
        let mut assets = release_assets(&dir);
        for (name, version, attest) in artifacts {
            let file = format!("{name}-{version}.mur.zip");
            let sha = sha256_hex(&fs::read(dir.join(&file)).unwrap());
            let spec = match attest {
                Attest::Nothing => continue,
                Attest::By(workflow) => BundleSpec::signed_by(ANA, ANA_OWNER, &sha, workflow),
                Attest::OtherDigest => BundleSpec::new(ANA, ANA_OWNER, &sha256_hex(b"other")),
                Attest::Edited(edit) => {
                    let mut spec = BundleSpec::new(ANA, ANA_OWNER, &sha);
                    edit(&mut spec);
                    spec
                }
            };
            let bundle = self.minter.bundle(&spec);
            self.bundles.push((file.clone(), bundle.clone()));
            assets.push((format!("{file}.sigstore.json"), bundle));
        }
        assets.push(("artifacts-index.json".to_string(), index));
        self.stub.repository(ANA, ANA_OWNER);
        self.stub.release(ANA, "v1", assets);
    }

    fn bundle_for(&self, file: &str) -> Vec<u8> {
        self.bundles
            .iter()
            .find(|(name, _)| name == file)
            .map(|(_, bundle)| bundle.clone())
            .unwrap()
    }

    /// `mur <args>` in `cwd` under `home`, against the stub and the minted root.
    fn mur_in(&self, home: &Path, cwd: &Path, args: &[&str]) -> Command {
        let mut command = Command::cargo_bin("mur").unwrap();
        command
            .env("HOME", home)
            .env("MUR_GITHUB_API_BASE", &self.stub.api_base)
            .env("MUR_SIGSTORE_TRUSTED_ROOT", &self.root)
            .env_remove("GITHUB_TOKEN")
            .env_remove("MURMUR_ROOST_URL")
            .current_dir(cwd)
            .timeout(Duration::from_secs(180))
            .args(args);
        command
    }

    fn mur(&self, args: &[&str]) -> Command {
        self.mur_in(self.home.path(), self.project.path(), args)
    }

    fn declare(&self, artifacts: &[(&str, &str)]) {
        write_manifest(self.project.path(), artifacts);
    }

    fn lock(&self) -> murmur_artifact::MurmurLock {
        read_lockfile(&self.project.path().join("murmur.lock")).unwrap()
    }

    fn project_store(&self) -> PathBuf {
        self.project.path().join(".murmur").join("artifacts")
    }

    fn sidecar(&self, full: &str, version: &str) -> PathBuf {
        let bare = full.split_once('/').unwrap().1;
        self.project_store()
            .join(full)
            .join(version)
            .join(format!("{bare}-{version}.sigstore.json"))
    }
}

fn write_manifest(dir: &Path, artifacts: &[(&str, &str)]) {
    let mut manifest = "name: attestation-project\nversion: 0.0.1\nartifacts:\n".to_string();
    for (name, version) in artifacts {
        manifest.push_str(&format!(
            "  - name: {name}\n    version: {version}\n    runtime: tool\n"
        ));
    }
    fs::write(dir.join("murmur.yaml"), manifest).unwrap();
}

struct Ran {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Ran {
    fn all(&self) -> String {
        format!("{}\n{}", self.stdout, self.stderr)
    }
}

fn run(mut command: Command) -> Ran {
    let output = command.output().unwrap();
    Ran {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn ok(command: Command) -> Ran {
    let ran = run(command);
    assert_eq!(ran.code, Some(0), "{}", ran.all());
    ran
}

/// Every file under `dir`, relative to it.
fn files_under(dir: &Path) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                out.push(path.strip_prefix(root).unwrap().display().to_string());
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

/// An API base nothing listens on.
fn closed_port() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    format!("http://127.0.0.1:{port}")
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn release_signer() -> String {
    signer_for(ANA, "release.yml")
}

// ─── Scenario 1 ──────────────────────────────────────────────────────────────

#[test]
fn an_attested_artifact_installs_verified_and_keeps_its_bundle() {
    let mut world = World::new();
    world.publish(&[("tool-git", "1.0.0", Attest::By("release.yml"))]);
    world.declare(&[("ana/tool-git", "1.0.0")]);

    let ran = ok(world.mur(&["install", "--no-precompile"]));
    assert!(
        ran.stdout.contains(&format!(
            "ana/tool-git@1.0.0 attested by {}",
            release_signer()
        )),
        "{}",
        ran.all()
    );
    assert!(!ran.all().contains("W-REG-005"), "{}", ran.all());
    assert_eq!(
        fs::read(world.sidecar("ana/tool-git", "1.0.0")).unwrap(),
        world.bundle_for("tool-git-1.0.0.mur.zip")
    );
    let lock = world.lock();
    assert_eq!(lock.lock_version, 3);
    let entry = lock.artifact_for("ana/tool-git").unwrap();
    assert_eq!(entry.owner_id, Some(ANA_OWNER));
    assert_eq!(entry.signer.as_deref(), Some(release_signer().as_str()));
    let raw = fs::read_to_string(world.project.path().join("murmur.lock")).unwrap();
    assert!(
        raw.contains(&format!("signer: {}", release_signer())),
        "{raw}"
    );

    // A named install from the source prints it on its `Installed` line.
    let ran = ok(world.mur(&["install", "--no-precompile", "ana/tool-git@1.0.0"]));
    assert!(
        ran.stdout.contains(&format!(
            "Installed ana/tool-git@1.0.0 from github:ana/artifacts, attested by {}",
            release_signer()
        )),
        "{}",
        ran.all()
    );
}

// ─── Scenario 2 ──────────────────────────────────────────────────────────────

#[test]
fn a_payload_that_does_not_match_its_attestation_is_refused() {
    let mut world = World::new();
    world.publish(&[("tool-git", "1.0.0", Attest::OtherDigest)]);
    world.declare(&[("ana/tool-git", "1.0.0")]);
    let payload_sha =
        sha256_hex(&fs::read(world.staging.path().join("v1/tool-git-1.0.0.mur.zip")).unwrap());

    let ran = run(world.mur(&["install", "--no-precompile", "ana/tool-git@1.0.0"]));
    assert_ne!(ran.code, Some(0), "{}", ran.all());
    assert!(
        ran.stderr.contains(&format!(
            "error[E-REG-016]: ana/tool-git@1.0.0 from github:ana/artifacts does not match its \
             attestation: the attestation does not cover the payload's sha256 {payload_sha}"
        )),
        "{}",
        ran.all()
    );
    assert!(ran.stderr.contains("nothing was stored"), "{}", ran.stderr);
    assert!(files_under(&world.project_store()).is_empty());
    assert!(!world.project.path().join("murmur.lock").exists());

    // The manifest install reports it per artifact and writes nothing either.
    let ran = run(world.mur(&["install", "--no-precompile"]));
    assert_ne!(ran.code, Some(0), "{}", ran.all());
    assert!(ran.all().contains("error[E-REG-016]"), "{}", ran.all());
    assert!(files_under(&world.project_store()).is_empty());
    assert!(!world.project.path().join("murmur.lock").exists());
}

// ─── Scenario 3 ──────────────────────────────────────────────────────────────

#[test]
fn a_forged_or_foreign_attestation_is_refused() {
    let mut world = World::new();
    world.publish(&[
        (
            "tool-forged",
            "1.0.0",
            Attest::Edited(|spec| spec.flip_signature = true),
        ),
        (
            "tool-foreign",
            "1.0.0",
            Attest::Edited(|spec| {
                spec.source_repository_uri = Some("https://github.com/mallory/artifacts".into());
                spec.owner_id = Some("2222".into());
            }),
        ),
        (
            "tool-rogue-ca",
            "1.0.0",
            Attest::Edited(|spec| spec.foreign_ca = true),
        ),
    ]);
    world.declare(&[]);
    for (name, reason) in [
        (
            "ana/tool-forged@1.0.0",
            "its DSSE signature does not verify",
        ),
        (
            "ana/tool-foreign@1.0.0",
            "its certificate was issued for https://github.com/mallory/artifacts with owner id \
             2222, not github.com/ana/artifacts with owner id 1111",
        ),
        (
            "ana/tool-rogue-ca@1.0.0",
            "its signing certificate does not chain to a certificate authority in the trusted root",
        ),
    ] {
        let ran = run(world.mur(&["install", "--no-precompile", name]));
        assert_ne!(ran.code, Some(0), "{}", ran.all());
        assert!(
            ran.stderr.contains(&format!(
                "error[E-REG-016]: {name} from github:ana/artifacts does not match its \
                 attestation: {reason}"
            )),
            "{name}: {}",
            ran.all()
        );
    }
    assert!(files_under(&world.project_store()).is_empty());
    assert!(!world.project.path().join("murmur.lock").exists());
}

// ─── Scenario 4 ──────────────────────────────────────────────────────────────

#[test]
fn an_unattested_artifact_installs_with_a_warning_naming_it() {
    let mut world = World::new();
    world.publish(&[("tool-git", "1.0.0", Attest::Nothing)]);
    world.declare(&[("ana/tool-git", "1.0.0")]);

    let ran = ok(world.mur(&["install", "--no-precompile"]));
    assert_eq!(
        ran.stderr.matches("warning[W-REG-005]:").count(),
        1,
        "{}",
        ran.all()
    );
    let warning = "warning[W-REG-005]: ana/tool-git@1.0.0 from github:ana/artifacts has no \
                   attestation; installed without proof of who published it\n  \u{2192} \
                   https://docs.murmur.nexus/reference/diagnostics/#w-reg-005";
    assert!(ran.stderr.contains(warning), "{}", ran.all());
    assert!(!ran.all().contains("attested by"), "{}", ran.all());
    assert!(!world.sidecar("ana/tool-git", "1.0.0").exists());
    assert_eq!(
        world.lock().artifact_for("ana/tool-git").unwrap().signer,
        None
    );
    assert!(
        !world
            .stub
            .paths()
            .iter()
            .any(|path| path.contains(".sigstore.json")),
        "{:?}",
        world.stub.paths()
    );
}

// ─── Scenario 5 ──────────────────────────────────────────────────────────────

#[test]
fn an_attestation_in_an_unknown_form_warns_and_installs() {
    let mut world = World::new();
    world.publish(&[(
        "tool-git",
        "1.0.0",
        Attest::Edited(|spec| {
            spec.media_type = "application/vnd.dev.sigstore.bundle.v9.9+json".into()
        }),
    )]);
    world.declare(&[("ana/tool-git", "1.0.0")]);

    let ran = ok(world.mur(&["install", "--no-precompile"]));
    assert!(
        ran.stderr.contains(
            "warning[W-REG-005]: ana/tool-git@1.0.0 from github:ana/artifacts has an attestation \
             in a form this mur does not verify (media type \
             'application/vnd.dev.sigstore.bundle.v9.9+json' is not a bundle form this mur \
             reads); installed without proof of who published it"
        ),
        "{}",
        ran.all()
    );
    assert!(!ran.all().contains("attested by"), "{}", ran.all());
    assert!(!world.sidecar("ana/tool-git", "1.0.0").exists());
    assert_eq!(
        world.lock().artifact_for("ana/tool-git").unwrap().signer,
        None
    );
}

// ─── Scenario 6 ──────────────────────────────────────────────────────────────

#[test]
fn local_artifacts_are_neither_checked_nor_warned() {
    let world = World::new();
    let file = wasm_tool_zip(world.staging.path(), "echo-tool", "0.1.0");
    world.declare(&[("local/echo-tool", "0.1.0")]);

    let outputs = vec![
        ok(world.mur(&["install", "--no-precompile", file.to_str().unwrap()])).all(),
        ok(world.mur(&["install", "--no-precompile"])).all(),
        run(world.mur(&["doctor"])).all(),
    ];
    for output in &outputs {
        assert!(!output.contains("attest"), "{output}");
        assert!(!output.contains("W-REG-005"), "{output}");
        assert!(!output.contains("W-REG-006"), "{output}");
    }
    assert!(
        !files_under(&world.project_store())
            .iter()
            .any(|file| file.ends_with(".sigstore.json")),
        "{:?}",
        files_under(&world.project_store())
    );
    assert_eq!(
        world.lock().artifact_for("local/echo-tool").unwrap().signer,
        None
    );
}

// ─── Scenario 7 ──────────────────────────────────────────────────────────────

#[test]
fn a_copied_store_verifies_with_no_network() {
    let mut world = World::new();
    world.publish(&[("tool-git", "1.0.0", Attest::By("release.yml"))]);
    world.declare(&[("ana/tool-git", "1.0.0")]);
    ok(world.mur(&["install", "--no-precompile"]));

    let copy = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    for file in ["murmur.yaml", "murmur.lock"] {
        fs::copy(world.project.path().join(file), copy.path().join(file)).unwrap();
    }
    copy_dir(
        &world.project_store(),
        &copy.path().join(".murmur").join("artifacts"),
    );
    world.stub.clear_requests();
    let offline = |args: &[&str]| {
        let mut command = world.mur_in(home.path(), copy.path(), args);
        command.env("MUR_GITHUB_API_BASE", closed_port());
        command
    };

    let ran = ok(offline(&["install", "--no-precompile"]));
    assert!(
        ran.stdout.contains(&format!(
            "ana/tool-git@1.0.0 attested by {}",
            release_signer()
        )),
        "{}",
        ran.all()
    );
    let doctor = run(offline(&["doctor"]));
    assert!(
        doctor.all().contains(&format!(
            "ana/tool-git@1.0.0   installed \u{00B7} attested by {}",
            release_signer()
        )),
        "{}",
        doctor.all()
    );
    assert!(world.stub.requests().is_empty(), "{:?}", world.stub.paths());

    // A sidecar swapped for another payload's bundle is refused, offline.
    let sidecar = copy
        .path()
        .join(".murmur/artifacts/ana/tool-git/1.0.0/tool-git-1.0.0.sigstore.json");
    let foreign = world
        .minter
        .bundle(&BundleSpec::new(ANA, ANA_OWNER, &sha256_hex(b"other")));
    fs::write(&sidecar, foreign).unwrap();
    let ran = run(offline(&["install", "--no-precompile"]));
    assert_ne!(ran.code, Some(0), "{}", ran.all());
    assert!(
        ran.all().contains(
            "error[E-REG-016]: ana/tool-git@1.0.0 from the project store does not match its \
             attestation: the attestation does not cover the payload's sha256"
        ),
        "{}",
        ran.all()
    );
    assert!(world.stub.requests().is_empty(), "{:?}", world.stub.paths());
}

// ─── Scenario 8 ──────────────────────────────────────────────────────────────

#[test]
fn the_global_store_carries_the_bundle_into_a_project() {
    let mut world = World::new();
    world.publish(&[("tool-git", "1.0.0", Attest::By("release.yml"))]);
    let ran = ok(world.mur(&["install", "--no-precompile", "-g", "ana/tool-git@1.0.0"]));
    assert!(
        ran.stdout.contains(&format!(
            "Installed ana/tool-git@1.0.0 from github:ana/artifacts, attested by {}",
            release_signer()
        )),
        "{}",
        ran.all()
    );
    let global_sidecar = world
        .home
        .path()
        .join(".murmur/artifacts/ana/tool-git/1.0.0/tool-git-1.0.0.sigstore.json");
    assert_eq!(
        fs::read(&global_sidecar).unwrap(),
        world.bundle_for("tool-git-1.0.0.mur.zip")
    );

    world.declare(&[("ana/tool-git", "1.0.0")]);
    world.stub.clear_requests();
    let ran = ok(world.mur(&["install", "--no-precompile", "ana/tool-git@1.0.0"]));
    assert!(world.stub.requests().is_empty(), "{:?}", world.stub.paths());
    assert!(
        ran.stdout
            .contains(&format!(", attested by {}", release_signer())),
        "{}",
        ran.all()
    );
    assert_eq!(
        fs::read(world.sidecar("ana/tool-git", "1.0.0")).unwrap(),
        world.bundle_for("tool-git-1.0.0.mur.zip")
    );
    assert_eq!(
        world.lock().artifact_for("ana/tool-git").unwrap().signer,
        Some(release_signer())
    );
}

// ─── Scenario 9 ──────────────────────────────────────────────────────────────

#[test]
fn a_changed_signer_is_surfaced() {
    let mut world = World::new();
    world.publish(&[
        ("tool-git", "1.0.0", Attest::By("release.yml")),
        ("tool-git", "1.1.0", Attest::By("other.yml")),
        ("tool-git", "1.2.0", Attest::Nothing),
    ]);
    let other = signer_for(ANA, "other.yml");

    world.declare(&[("ana/tool-git", "1.0.0")]);
    let ran = ok(world.mur(&["install", "--no-precompile"]));
    assert!(!ran.stderr.contains("W-REG-006"), "{}", ran.all());

    world.declare(&[("ana/tool-git", "1.1.0")]);
    let ran = ok(world.mur(&["install", "--no-precompile"]));
    assert!(
        ran.stderr.contains(&format!(
            "warning[W-REG-006]: ana/tool-git@1.1.0 is attested by {other}; murmur.lock recorded \
             {}",
            release_signer()
        )),
        "{}",
        ran.all()
    );
    assert_eq!(
        world.lock().artifact_for("ana/tool-git").unwrap().signer,
        Some(other.clone())
    );

    world.declare(&[("ana/tool-git", "1.2.0")]);
    let ran = ok(world.mur(&["install", "--no-precompile"]));
    assert!(
        ran.stderr.contains(&format!(
            "warning[W-REG-006]: ana/tool-git@1.2.0 has no attestation; murmur.lock recorded it \
             as attested by {other}"
        )),
        "{}",
        ran.all()
    );
    assert!(ran.stderr.contains("warning[W-REG-005]"), "{}", ran.all());
    let entry = world.lock().artifact_for("ana/tool-git").unwrap().clone();
    assert_eq!(entry.resolved_version, "1.2.0");
    assert_eq!(entry.signer, None);
    let raw = fs::read_to_string(world.project.path().join("murmur.lock")).unwrap();
    assert!(!raw.contains("signer"), "{raw}");
}

// ─── Scenario 10 ─────────────────────────────────────────────────────────────

#[test]
fn a_github_release_install_checks_each_asset() {
    let world = World::new();
    let dir = world.staging.path().join("v3");
    fs::create_dir_all(&dir).unwrap();
    let good = wasm_tool_zip(&dir, "tool-good", "1.0.0");
    wasm_tool_zip(&dir, "tool-bad", "1.0.0");
    let mut assets = release_assets(&dir);
    let good_sha = sha256_hex(&fs::read(&good).unwrap());
    assets.push((
        "tool-good-1.0.0.mur.zip.sigstore.json".to_string(),
        world
            .minter
            .bundle(&BundleSpec::new(ANA, ANA_OWNER, &good_sha)),
    ));
    assets.push((
        "tool-bad-1.0.0.mur.zip.sigstore.json".to_string(),
        world
            .minter
            .bundle(&BundleSpec::new(ANA, ANA_OWNER, &sha256_hex(b"other"))),
    ));
    world.stub.repository(ANA, ANA_OWNER);
    world.stub.release(ANA, "v3", assets);
    world.declare(&[]);

    let ran = run(world.mur(&["install", "--no-precompile", "github:ana/artifacts@v3"]));
    assert_ne!(ran.code, Some(0), "{}", ran.all());
    assert!(
        ran.stdout.contains(&format!(
            "Installed ana/tool-good@1.0.0 from github:ana/artifacts, attested by {}",
            release_signer()
        )),
        "{}",
        ran.all()
    );
    assert!(
        ran.stdout
            .contains("1 of 2 assets of github:ana/artifacts@v3 failed to install:"),
        "{}",
        ran.all()
    );
    assert!(
        ran.stdout.contains(
            "error[E-REG-016]: ana/tool-bad@1.0.0 from github:ana/artifacts does not match its \
             attestation"
        ),
        "{}",
        ran.all()
    );
    assert!(world.sidecar("ana/tool-good", "1.0.0").exists());
    assert!(!world.project_store().join("ana/tool-bad").exists());
    let lock = world.lock();
    assert_eq!(
        lock.artifact_for("ana/tool-good").unwrap().signer,
        Some(release_signer())
    );
    assert!(lock.artifact_for("ana/tool-bad").is_none());
}

// ─── Scenario 11 ─────────────────────────────────────────────────────────────

#[test]
fn doctor_shows_each_artifacts_attestation_state() {
    let mut world = World::new();
    world.publish(&[
        ("tool-git", "1.0.0", Attest::By("release.yml")),
        ("tool-plain", "1.0.0", Attest::Nothing),
        ("tool-tamper", "1.0.0", Attest::By("release.yml")),
    ]);
    world.declare(&[
        ("ana/tool-git", "1.0.0"),
        ("ana/tool-plain", "1.0.0"),
        ("ana/tool-tamper", "1.0.0"),
        ("local/echo-tool", "0.1.0"),
    ]);
    let local = wasm_tool_zip(world.staging.path(), "echo-tool", "0.1.0");
    ok(world.mur(&["install", "--no-precompile", local.to_str().unwrap()]));
    ok(world.mur(&["install", "--no-precompile"]));
    fs::write(
        world.sidecar("ana/tool-tamper", "1.0.0"),
        world
            .minter
            .bundle(&BundleSpec::new(ANA, ANA_OWNER, &sha256_hex(b"other"))),
    )
    .unwrap();

    let offline = |json: bool| {
        let mut command = world.mur(&["doctor"]);
        if json {
            command.arg("--json");
        }
        command.env("MUR_GITHUB_API_BASE", closed_port());
        run(command)
    };
    let text = offline(false).all();
    for line in [
        format!(
            "\u{2713}  ana/tool-git@1.0.0      installed \u{00B7} attested by {}",
            release_signer()
        ),
        "\u{26A0}  ana/tool-plain@1.0.0    installed \u{00B7} no attestation".to_string(),
        "\u{26A0}  ana/tool-tamper@1.0.0   installed \u{00B7} attestation does not verify: the \
         attestation does not cover the payload's sha256"
            .to_string(),
        "\u{2713}  local/echo-tool@0.1.0   installed\n".to_string(),
    ] {
        assert!(text.contains(&line), "{line:?} missing from:\n{text}");
    }
    assert!(text.contains("MUR_SIGSTORE_TRUSTED_ROOT"), "{text}");

    let json = offline(true);
    let report: Value = serde_json::from_str(&json.stdout).unwrap();
    assert_eq!(report["verdict"], "ready", "{report:#}");
    let attestation: Vec<&Value> = report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|finding| {
            finding["code"]
                .as_str()
                .is_some_and(|code| code == "W-REG-005" || code == "W-REG-006")
        })
        .collect();
    assert_eq!(attestation.len(), 2, "{report:#}");
    for (finding, subject) in attestation
        .iter()
        .zip(["ana/tool-plain@1.0.0", "ana/tool-tamper@1.0.0"])
    {
        assert_eq!(finding["code"], "W-REG-005", "{finding}");
        assert_eq!(finding["severity"], "warning", "{finding}");
        assert_eq!(finding["block"], "artifacts", "{finding}");
        assert_eq!(finding["subject"], subject, "{finding}");
    }
    assert_eq!(attestation[1]["fix"], "mur install ana/tool-tamper@1.0.0");
}
