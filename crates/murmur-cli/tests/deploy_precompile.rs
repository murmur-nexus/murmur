//! `mur deploy run` compiles the WASM artifacts it uploads on the target, with the target's own
//! `mur` and under the target's environment, before it starts the capsule, so the capsule's first
//! launch loads compiled forms.
//!
//! Every deploy here goes to a [`LoopbackTarget`]: the real `mur deploy run` and the real target
//! `mur`, with `ssh` and `scp` shimmed onto a temp dir. The capsule is an agent capsule whose
//! anthropic driver reaches an in-test upstream, with one WASM tool and one native tool.
#![cfg(feature = "beta-mur-deploy")]

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    io::Write,
    net::TcpStream,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use assert_cmd::Command;
use common::{
    loopback_target::{now_ns, LoopbackTarget},
    recording_upstream::{RecordingUpstream, Reply},
};
use tempfile::TempDir;

const DRIVER: &str = "murmur-driver-anthropic";
const TOOL: &str = "echo-tool";
const NATIVE: &str = "native-echo";
const VERSION: &str = "0.1.0";
const KEY: &str = "sk-loopback-deploy";
const CEILING_ENV: &str = "MURMUR_MAX_ARTIFACT_DECOMPRESSED_BYTES";
const RAISED_CEILING: &str = "600000000";

const END_TURN: &str = r#"{"id":"msg_1","type":"message","role":"assistant","model":"test-model","content":[{"type":"text","text":"done"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#;

// ─── the deploying machine ───────────────────────────────────────────────────

/// The deploying machine: a `HOME` with `mur-deploy` enabled and the capsule's three artifacts in
/// its global store, installed without precompiling, and the capsule's project directory.
struct Deployer {
    home: TempDir,
    project: TempDir,
    manifest: PathBuf,
    upstream: RecordingUpstream,
    /// When each upstream request arrived, on the shims' clock.
    arrivals: Arc<Mutex<Vec<u128>>>,
}

impl Deployer {
    fn new() -> Self {
        let home = TempDir::new().unwrap();
        let project = TempDir::new().unwrap();
        let artifacts = TempDir::new().unwrap();
        fs::create_dir_all(home.path().join(".murmur")).unwrap();
        fs::write(
            home.path().join(".murmur/config.yaml"),
            "beta:\n  enabled:\n    - mur-deploy\n",
        )
        .unwrap();

        let zips = [
            common::create_driver_artifact(
                artifacts.path(),
                DRIVER,
                VERSION,
                &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
            ),
            common::create_tool_artifact(
                artifacts.path(),
                TOOL,
                VERSION,
                &common::fixture_path("run/components/echo-tool.wasm"),
            ),
            common::create_native_artifact(
                artifacts.path(),
                NATIVE,
                VERSION,
                "#!/bin/sh\necho '{\"ok\":true}'\n",
                Some("prints ok"),
                None,
            ),
        ];
        for zip in &zips {
            Command::cargo_bin("mur")
                .unwrap()
                .env("HOME", home.path())
                .env_remove("NEXUS_API_KEY")
                .env_remove(CEILING_ENV)
                .args(["install", "-g", "--no-precompile", zip.to_str().unwrap()])
                .assert()
                .success();
        }
        assert!(!home.path().join(".murmur/compiled").exists());

        let arrivals = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&arrivals);
        let upstream = RecordingUpstream::with_handler(move |_, _| {
            recorded.lock().unwrap().push(now_ns());
            Reply::json(END_TURN)
        });

        let manifest = project.path().join("murmur.yaml");
        fs::write(
            &manifest,
            format!(
                "name: deployed-capsule\nversion: 0.1.0\nartifacts:\n  - name: {DRIVER}\n    \
                 version: {VERSION}\n    runtime: driver\n    gateway:\n      \
                 endpoint: {}\n      api_key: ${{GATEWAY_TEST_KEY}}\n  - name: {TOOL}\n    \
                 version: {VERSION}\n    runtime: tool\n  - name: {NATIVE}\n    \
                 version: {VERSION}\n    runtime: tool\ninference:\n  transport: http\n  \
                 model: test-model\n  driver:\n    artifact: {DRIVER}\n",
                upstream.endpoint
            ),
        )
        .unwrap();

        Self {
            home,
            project,
            manifest,
            upstream,
            arrivals,
        }
    }

    /// `mur deploy run` onto `target` with `mur_binary` as the target's `mur`, `extra` after the
    /// fixed arguments, and `ceiling` as this machine's `MURMUR_MAX_ARTIFACT_DECOMPRESSED_BYTES`.
    fn deploy(
        &self,
        target: &LoopbackTarget,
        mur_binary: &Path,
        extra: &[&str],
        ceiling: Option<&str>,
    ) -> Deployed {
        let mut command = Command::cargo_bin("mur").unwrap();
        command
            .env("HOME", self.home.path())
            .env("PATH", target.path_env())
            .env_remove("NEXUS_API_KEY")
            .env_remove(CEILING_ENV)
            .current_dir(self.project.path())
            .timeout(Duration::from_secs(600))
            .args(["deploy", "run", "--host", "127.0.0.1", "--mur-binary"])
            .arg(mur_binary)
            .args([
                "--deploy-platform",
                murmur_artifact::current_platform(),
                "--manifest",
                self.manifest.to_str().unwrap(),
                "--env",
                &format!("GATEWAY_TEST_KEY={KEY}"),
            ])
            .args(extra);
        if let Some(ceiling) = ceiling {
            command.env(CEILING_ENV, ceiling);
        }
        let started = Instant::now();
        let output = command.output().unwrap();
        let wall = started.elapsed();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(
            output.status.success(),
            "deploy failed ({}):\n{stderr}\ncapsule stderr:\n{}",
            output.status,
            fs::read_to_string(target.path().join("tmp/mur-start.err")).unwrap_or_default()
        );
        Deployed {
            url: self.last_url(),
            stderr,
            wall,
        }
    }

    /// The URL `deployments.json` records for the latest deploy.
    fn last_url(&self) -> String {
        let records: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(self.home.path().join(".murmur/deployments.json")).unwrap(),
        )
        .unwrap();
        records.as_array().unwrap().last().unwrap()["url"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn arrivals(&self) -> Vec<u128> {
        self.arrivals.lock().unwrap().clone()
    }

    /// Posts one task to the capsule at `url` and waits for the upstream request it causes, the
    /// first after `seen` earlier ones. Returns when it arrived.
    fn serve_one_task(&self, url: &str, seen: usize) -> u128 {
        let address = url.trim_start_matches("http://").to_string();
        std::thread::spawn(move || post_task(&address));
        let deadline = Instant::now() + Duration::from_secs(300);
        loop {
            if let Some(&arrived) = self.arrivals().get(seen) {
                assert_eq!(
                    self.upstream.requests()[seen].header_values("x-api-key"),
                    [KEY],
                    "the capsule sent the key its `.env` gave it"
                );
                return arrived;
            }
            assert!(
                Instant::now() < deadline,
                "the upstream saw no request for the posted task"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

struct Deployed {
    url: String,
    stderr: String,
    wall: Duration,
}

impl Deployed {
    fn warnings(&self, code: &str) -> Vec<&str> {
        let needle = format!("warning[{code}]");
        self.stderr
            .lines()
            .filter(|line| line.contains(&needle))
            .collect()
    }
}

/// One JSON-RPC `SendMessage` to the capsule's door. The reply is not needed: the upstream
/// request the task causes is what the tests wait on.
fn post_task(address: &str) {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "SendMessage",
        "params": {"message": {
            "messageId": "msg-1",
            "role": "user",
            "parts": [{"text": "Say hello."}]
        }}
    })
    .to_string();
    let Ok(mut stream) = TcpStream::connect(address) else {
        return;
    };
    let _ = write!(
        stream,
        "POST / HTTP/1.0\r\nHost: {address}\r\nContent-Type: application/json\r\nA2A-Version: 1.0\r\n\
         Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.set_read_timeout(Some(Duration::from_secs(300)));
    let _ = std::io::Read::read_to_end(&mut stream, &mut Vec::new());
}

fn real_mur() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_mur"))
}

// ─── the target ──────────────────────────────────────────────────────────────

/// The sha256 of the payload deploy uploaded for `name`.
fn uploaded_sha256(target: &LoopbackTarget, name: &str) -> String {
    let zip = target.root().join(format!(
        ".murmur/artifacts/{name}/{VERSION}/{name}-{VERSION}.mur.zip"
    ));
    murmur_artifact::sha256_hex(&fs::read(&zip).unwrap())
}

/// The `.cwasm` entries of a compiled-forms listing, by file name.
fn form_names(listing: &str) -> Vec<String> {
    listing
        .lines()
        .filter_map(|line| line.split(' ').next())
        .filter(|name| name.ends_with(".cwasm"))
        .map(str::to_string)
        .collect()
}

/// The engine key of the one form of `sha256` in `listing`.
fn engine_key(listing: &str, sha256: &str) -> String {
    let prefix = format!("{sha256}-");
    let names: Vec<String> = form_names(listing)
        .into_iter()
        .filter(|name| name.starts_with(&prefix))
        .collect();
    let [name] = &names[..] else {
        panic!("expected one form of {sha256} in:\n{listing}");
    };
    let key = name
        .strip_prefix(&prefix)
        .and_then(|rest| rest.strip_suffix(".cwasm"))
        .unwrap();
    assert!(
        key.len() == 16 && key.chars().all(|c| c.is_ascii_hexdigit()),
        "{name}"
    );
    key.to_string()
}

/// Asserts `listing` holds exactly one form and sidecar each for the tool and the driver as
/// uploaded, owner-only in an owner-only directory, and nothing for the native tool.
fn assert_forms_of_tool_and_driver(target: &LoopbackTarget, listing: &str) {
    let tool = uploaded_sha256(target, TOOL);
    let driver = uploaded_sha256(target, DRIVER);
    let native = uploaded_sha256(target, NATIVE);
    let mut lines = listing.lines();
    assert_eq!(lines.next(), Some("dir 700"), "{listing}");
    let entries: Vec<(&str, &str)> = lines
        .map(|line| {
            let fields: Vec<&str> = line.split(' ').collect();
            (fields[0], fields[4])
        })
        .collect();
    assert_eq!(entries.len(), 4, "{listing}");
    for sha256 in [&tool, &driver] {
        let key = engine_key(listing, sha256);
        for name in [
            format!("{sha256}-{key}.cwasm"),
            format!("{sha256}-{key}.cwasm.sha256"),
        ] {
            assert!(
                entries.contains(&(name.as_str(), "600")),
                "{name} 600 missing from:\n{listing}"
            );
        }
    }
    assert!(!listing.contains(&native), "{listing}");
}

/// `commands` with the deploy's `/root/mur-<6 hex>` and the capsule port of `url` replaced by
/// placeholders, and the step-6 artifact `mkdir`s, which run in parallel and so arrive in any
/// order, sorted.
fn normalised(commands: &[String], url: &str) -> Vec<String> {
    let port = url.rsplit(':').next().unwrap();
    let mut commands: Vec<String> = commands
        .iter()
        .map(|command| {
            let mut out = String::new();
            let mut rest = command.as_str();
            while let Some(at) = rest.find("/root/mur-") {
                let (head, tail) = rest.split_at(at + "/root/mur-".len());
                out.push_str(head);
                out.push_str("<id>");
                rest = &tail[tail.len().min(6)..];
            }
            out.push_str(rest);
            out.replace(port, "<port>")
        })
        .collect();
    let mut start = 0;
    while start < commands.len() {
        let is_store_mkdir = |c: &String| c.starts_with("mkdir -p /root/.murmur/artifacts/");
        if is_store_mkdir(&commands[start]) {
            let end = start
                + commands[start..]
                    .iter()
                    .take_while(|c| is_store_mkdir(c))
                    .count();
            commands[start..end].sort();
            start = end;
        } else {
            start += 1;
        }
    }
    commands
}

fn position(commands: &[String], what: &str, matches: impl Fn(&str) -> bool) -> usize {
    let found: Vec<usize> = commands
        .iter()
        .enumerate()
        .filter(|(_, command)| matches(command))
        .map(|(i, _)| i)
        .collect();
    let [index] = found[..] else {
        panic!("expected exactly one {what} command, found {found:?} in {commands:#?}");
    };
    index
}

fn is_env_write(command: &str) -> bool {
    command.starts_with("printf '%s' ") && command.contains("/.env && chmod 600 ")
}

fn is_start(command: &str) -> bool {
    command.contains("/usr/local/bin/mur run ")
}

fn is_precompile(command: &str) -> bool {
    command.contains("/usr/local/bin/mur precompile ")
}

/// The one `mur run --workdir` of the start script: `/root/mur-<6 hex>`.
fn remote_deploy_dir(commands: &[String]) -> String {
    let start = &commands[position(commands, "start", is_start)];
    let dir = start
        .split(" --workdir ")
        .nth(1)
        .and_then(|rest| rest.split(' ').next())
        .unwrap()
        .to_string();
    let id = dir.strip_prefix("/root/mur-").unwrap();
    assert!(
        id.len() == 6 && id.chars().all(|c| c.is_ascii_hexdigit()),
        "{dir}"
    );
    dir
}

// ─── a default deploy ────────────────────────────────────────────────────────

#[test]
fn a_deploy_compiles_on_the_target_and_the_first_launch_loads_the_forms() {
    let deployer = Deployer::new();
    let target = LoopbackTarget::new();
    let deployed = deployer.deploy(&target, &real_mur(), &[], None);

    let commands = target.commands();
    let precompile = position(&commands, "precompile", is_precompile);
    assert!(position(&commands, ".env write", is_env_write) < precompile);
    assert!(precompile < position(&commands, "start", is_start));
    let dir = remote_deploy_dir(&commands);
    let command = &commands[precompile];
    assert!(command.contains(" precompile --json "), "{command}");
    assert!(command.contains(&format!(" --workdir {dir} ")), "{command}");
    for name in [DRIVER, TOOL, NATIVE] {
        let path = format!(" /root/.murmur/artifacts/{name}/{VERSION}/{name}-{VERSION}.mur.zip");
        assert!(command.contains(&path), "{path} missing from {command}");
    }

    let before = target.compiled_before_start();
    assert_forms_of_tool_and_driver(&target, &before);

    deployer.serve_one_task(&deployed.url, 0);
    assert_eq!(
        target.compiled_listing(),
        before,
        "the launch compiled or rewrote a form instead of loading the stored one"
    );
    assert!(
        !deployed.stderr.contains("W-DEPLOY-"),
        "{}",
        deployed.stderr
    );
    target.stop_capsules();
}

// ─── --no-precompile ─────────────────────────────────────────────────────────

#[test]
fn no_precompile_sends_every_command_but_the_compile() {
    let deployer = Deployer::new();

    let compiled = LoopbackTarget::new();
    let compiled_url = deployer.deploy(&compiled, &real_mur(), &[], None).url;
    compiled.stop_capsules();

    let target = LoopbackTarget::new();
    let deployed = deployer.deploy(&target, &real_mur(), &["--no-precompile"], None);

    let mut expected = compiled.commands();
    expected.remove(position(&expected, "precompile", is_precompile));
    assert_eq!(
        normalised(&target.commands(), &deployed.url),
        normalised(&expected, &compiled_url)
    );
    assert_eq!(target.compiled_before_start(), "absent\n");

    deployer.serve_one_task(&deployed.url, 0);
    assert_forms_of_tool_and_driver(&target, &target.compiled_listing());
    assert!(
        !deployed.stderr.contains("W-DEPLOY-"),
        "{}",
        deployed.stderr
    );
    target.stop_capsules();

    let help = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", deployer.home.path())
        .args(["deploy", "run", "--help"])
        .output()
        .unwrap();
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("--no-precompile"));
}

// ─── the decompression ceiling ───────────────────────────────────────────────

/// Deploys under the given ceilings, serves one task, and returns the deploy and the listing the
/// capsule started with, having checked the launch loaded every form in it.
fn deploy_under_ceilings(
    deployer: &Deployer,
    target: &LoopbackTarget,
    deployer_ceiling: Option<&str>,
    target_ceiling: Option<&str>,
) -> (Deployed, String) {
    let env = target_ceiling.map(|ceiling| format!("{CEILING_ENV}={ceiling}"));
    let extra: Vec<&str> = match &env {
        Some(env) => vec!["--env", env],
        None => Vec::new(),
    };
    let deployed = deployer.deploy(target, &real_mur(), &extra, deployer_ceiling);
    let before = target.compiled_before_start();
    assert_forms_of_tool_and_driver(target, &before);
    deployer.serve_one_task(&deployed.url, 0);
    assert_eq!(target.compiled_listing(), before);
    target.stop_capsules();
    (deployed, before)
}

#[test]
fn a_ceiling_set_only_for_the_target_warns_and_the_launch_is_warm() {
    let deployer = Deployer::new();
    let target = LoopbackTarget::new();
    let (deployed, listing) = deploy_under_ceilings(&deployer, &target, None, Some(RAISED_CEILING));
    assert_eq!(
        deployed.warnings("W-DEPLOY-002"),
        [
            "warning[W-DEPLOY-002]: the artifact decompression ceiling is 524288000 bytes on this \
          machine and 600000000 bytes on the target; the capsule runs, warm, under the target's \
          value. To align them, pass --env MURMUR_MAX_ARTIFACT_DECOMPRESSED_BYTES=524288000"
        ],
        "{}",
        deployed.stderr
    );
    assert!(deployed.warnings("W-DEPLOY-001").is_empty());

    let default = LoopbackTarget::new();
    let (_, default_listing) = deploy_under_ceilings(&deployer, &default, None, None);
    let tool = uploaded_sha256(&target, TOOL);
    assert_ne!(
        engine_key(&listing, &tool),
        engine_key(&default_listing, &tool),
        "a raised ceiling keys its own forms"
    );
}

#[test]
fn a_ceiling_set_only_on_this_machine_warns_and_the_launch_is_warm() {
    let deployer = Deployer::new();
    let target = LoopbackTarget::new();
    let (deployed, _) = deploy_under_ceilings(&deployer, &target, Some(RAISED_CEILING), None);
    assert_eq!(
        deployed.warnings("W-DEPLOY-002"),
        [
            "warning[W-DEPLOY-002]: the artifact decompression ceiling is 600000000 bytes on this \
          machine and 524288000 bytes on the target; the capsule runs, warm, under the target's \
          value. To align them, pass --env MURMUR_MAX_ARTIFACT_DECOMPRESSED_BYTES=600000000"
        ],
        "{}",
        deployed.stderr
    );
}

#[test]
fn an_aligned_ceiling_warns_nothing_and_the_launch_is_warm() {
    let deployer = Deployer::new();
    let target = LoopbackTarget::new();
    let (deployed, _) = deploy_under_ceilings(
        &deployer,
        &target,
        Some(RAISED_CEILING),
        Some(RAISED_CEILING),
    );
    assert!(
        !deployed.stderr.contains("W-DEPLOY-"),
        "{}",
        deployed.stderr
    );
}

// ─── a target mur without precompile ─────────────────────────────────────────

#[test]
fn a_target_mur_without_precompile_warns_and_the_capsule_still_serves() {
    let deployer = Deployer::new();
    let scripts = TempDir::new().unwrap();
    let old_mur = scripts.path().join("mur");
    fs::write(
        &old_mur,
        format!(
            "#!/bin/sh\nif [ \"$1\" = precompile ]; then\n  \
             printf \"error: unrecognized subcommand 'precompile'\\n\\nUsage: mur <COMMAND>\\n\\n\
             For more information, try '--help'.\\n\" >&2\n  exit 2\nfi\nexec '{}' \"$@\"\n",
            real_mur().display()
        ),
    )
    .unwrap();
    fs::set_permissions(&old_mur, fs::Permissions::from_mode(0o755)).unwrap();

    let target = LoopbackTarget::new();
    let deployed = deployer.deploy(&target, &old_mur, &[], None);
    assert_eq!(
        deployed.warnings("W-DEPLOY-001"),
        [
            "warning[W-DEPLOY-001]: the target did not precompile (error: unrecognized subcommand \
          'precompile'); the capsule compiles its artifacts on its first launch"
        ],
        "{}",
        deployed.stderr
    );
    assert!(deployed.warnings("W-DEPLOY-002").is_empty());
    assert_eq!(target.compiled_before_start(), "absent\n");

    deployer.serve_one_task(&deployed.url, 0);
    assert_forms_of_tool_and_driver(&target, &target.compiled_listing());
    target.stop_capsules();
}

// ─── measured, run by hand ───────────────────────────────────────────────────

struct Sample {
    first_launch: Duration,
    warm_launch: Duration,
    precompile: Option<Duration>,
    deploy_wall: Duration,
}

fn ns(from: u128, to: u128) -> Duration {
    Duration::from_nanos(u64::try_from(to.saturating_sub(from)).unwrap())
}

/// One fresh target: deploy, first launch to the first upstream request; then the capsule is
/// stopped, the logged start script is run again through the shim, and the same measurement is
/// the warm launch.
fn measure(deployer: &Deployer, mur: &Path, no_precompile: bool) -> Sample {
    let target = LoopbackTarget::new();
    let extra: &[&str] = if no_precompile {
        &["--no-precompile"]
    } else {
        &[]
    };
    let seen = deployer.arrivals().len();
    let deployed = deployer.deploy(&target, mur, extra, None);
    let first_arrival = deployer.serve_one_task(&deployed.url, seen);
    let first_launch = ns(target.start_times()[0], first_arrival);
    target.stop_capsules();

    let start = target
        .commands()
        .into_iter()
        .find(|command| is_start(command))
        .unwrap();
    let output = target.ssh(&start);
    let line = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let ready: serde_json::Value =
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("warm start printed {line:?}: {e}"));
    let port = ready["url"].as_str().unwrap().rsplit(':').next().unwrap();
    let warm_arrival = deployer.serve_one_task(&format!("http://127.0.0.1:{port}"), seen + 1);
    let warm_launch = ns(target.start_times()[1], warm_arrival);
    target.stop_capsules();

    Sample {
        first_launch,
        warm_launch,
        precompile: target
            .precompile_times()
            .first()
            .map(|&(from, to)| ns(from, to)),
        deploy_wall: deployed.wall,
    }
}

fn median(mut values: Vec<Duration>) -> Duration {
    values.sort();
    let mid = values.len() / 2;
    if values.len() % 2 == 1 {
        values[mid]
    } else {
        (values[mid - 1] + values[mid]) / 2
    }
}

/// The 1, 5 and 15 minute load averages.
fn load_average() -> String {
    fs::read_to_string("/proc/loadavg")
        .unwrap_or_default()
        .split(' ')
        .take(3)
        .collect::<Vec<_>>()
        .join(" ")
}

fn host_line(path: &str, key: &str) -> String {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .find(|line| line.starts_with(key))
        .and_then(|line| line.split_once(':'))
        .map(|(_, value)| value.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// The first-launch and warm-launch times of five fresh targets per arm, alternating the default
/// deploy and `--no-precompile`. `MURMUR_BENCH_TARGET_MUR` names the target `mur` to measure (a
/// release build, for figures a user would see); the test's own debug `mur` otherwise.
#[test]
#[ignore = "measurement, run by hand with --ignored --nocapture"]
fn measured_first_launch_after_a_deploy() {
    const TARGETS_PER_ARM: usize = 5;
    let mur = std::env::var_os("MURMUR_BENCH_TARGET_MUR")
        .map(PathBuf::from)
        .unwrap_or_else(real_mur);
    let deployer = Deployer::new();
    let load_before = load_average();
    let mut default = Vec::new();
    let mut skipped = Vec::new();
    for _ in 0..TARGETS_PER_ARM {
        default.push(measure(&deployer, &mur, false));
        skipped.push(measure(&deployer, &mur, true));
    }

    let ms = |d: Duration| format!("{:.0} ms", d.as_secs_f64() * 1000.0);
    println!("target mur: {}", mur.display());
    println!(
        "host: {} · {} logical CPUs · load average before {load_before}",
        host_line("/proc/cpuinfo", "model name"),
        std::thread::available_parallelism().map_or(0, |n| n.get()),
    );
    println!("| arm | target | first launch | warm launch | precompile step | deploy wall time |");
    println!("|---|---|---|---|---|---|");
    for (arm, samples) in [("default", &default), ("--no-precompile", &skipped)] {
        for (i, sample) in samples.iter().enumerate() {
            println!(
                "| {arm} | {} | {} | {} | {} | {} |",
                i + 1,
                ms(sample.first_launch),
                ms(sample.warm_launch),
                sample.precompile.map_or("—".to_string(), ms),
                ms(sample.deploy_wall)
            );
        }
    }
    let first_default = median(default.iter().map(|s| s.first_launch).collect());
    let first_skipped = median(skipped.iter().map(|s| s.first_launch).collect());
    let warm = median(
        default
            .iter()
            .chain(&skipped)
            .map(|s| s.warm_launch)
            .collect(),
    );
    let precompile = median(default.iter().filter_map(|s| s.precompile).collect());
    let wall_default = median(default.iter().map(|s| s.deploy_wall).collect());
    let wall_skipped = median(skipped.iter().map(|s| s.deploy_wall).collect());
    let ratio_default = first_default.as_secs_f64() / warm.as_secs_f64();
    let ratio_skipped = first_skipped.as_secs_f64() / warm.as_secs_f64();
    println!("median first launch, default: {}", ms(first_default));
    println!(
        "median first launch, --no-precompile: {}",
        ms(first_skipped)
    );
    println!("median warm launch: {}", ms(warm));
    println!("median precompile step (default): {}", ms(precompile));
    println!(
        "median deploy wall time: default {}, --no-precompile {}",
        ms(wall_default),
        ms(wall_skipped)
    );
    println!("first(default) / warm = {ratio_default:.2} (pass ≤ 1.25)");
    println!("first(--no-precompile) / warm = {ratio_skipped:.2} (pass ≥ 2)");
    println!("load average after: {}", load_average());
    assert!(
        ratio_default <= 1.25,
        "first(default)/warm = {ratio_default:.2}"
    );
    assert!(
        ratio_skipped >= 2.0,
        "first(--no-precompile)/warm = {ratio_skipped:.2}"
    );
}
