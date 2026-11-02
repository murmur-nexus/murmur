//! Host folders an operator lends contained capsules through `host_paths:` in
//! `~/.murmur/config.yaml`, driven through the real `mur` binary on a scripted model.
//!
//! Every test runs `mur` with `HOME` set to a scratch directory under `CARGO_TARGET_TMPDIR`, never
//! under `/tmp`: a host path may not lie under `/tmp`, which a composed root and the private `/tmp`
//! replace.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    path::{Path, PathBuf},
    process::Output,
};

use assert_cmd::Command;
use serde_json::{json, Value};
use tempfile::TempDir;

const DRIVER: &str = "local/murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.0";
const TOOL_VERSION: &str = "0.1.0";
const CAPSULE_NAME: &str = "host-paths-capsule";
const REACH_TOOL: &str = "local/reach";

/// Whether this host reaches `sealed`, printing a `[SKIP-HOST]` line when it does not.
fn host_reaches_sealed(test: &str) -> bool {
    let (decision, shortfall) =
        capsule_runtime::detect_containment_decision(murmur_artifact::ContainmentClass::Sealed);
    if decision.achieved() == murmur_artifact::ContainmentClass::Sealed {
        return true;
    }
    eprintln!(
        "[SKIP-HOST] {test}: this host cannot back sealed ({})",
        shortfall
            .map(|shortfall| shortfall.reason())
            .unwrap_or_else(|| format!("it reaches {}", decision.achieved()))
    );
    false
}

fn end_turn_response() -> String {
    json!({
        "id": "msg_end",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": "done"}],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {"input_tokens": 1, "output_tokens": 1},
    })
    .to_string()
}

/// A native tool's script: drain stdin, run `body`, which sets `$v`, then report `$v` between
/// `<<<` and `>>>`, escaped for a JSON string. Builtins only, plus what `shell.allow` names.
fn native_script(body: &str) -> String {
    format!(
        "#!/bin/bash\nwhile read -r _; do :; done\n{body}\nv=${{v//\\\\/\\\\\\\\}}\nv=${{v//\\\"/\\\\\\\"}}\n\
         printf '{{\"status\":\"passed\",\"summary\":\"<<<%s>>>\"}}' \"$v\"\n"
    )
}

/// A canonical scratch directory outside `/tmp`.
fn scratch() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let canonical = dir.path().canonicalize().unwrap();
    assert!(
        !canonical.starts_with("/tmp"),
        "CARGO_TARGET_TMPDIR resolves under /tmp ({}); these tests need a directory outside it",
        canonical.display()
    );
    (dir, canonical)
}

/// One capsule on a scripted model: a scratch `HOME`, a project, and the driver (and optionally
/// a native tool) installed into the project under that `HOME`.
struct Capsule {
    _root: TempDir,
    base: PathBuf,
    home: PathBuf,
    project: PathBuf,
    /// The capsule-wide `capabilities:` lines beyond `network.allow`, indented two spaces.
    capabilities: String,
    /// Artifact entries beyond the driver.
    entries: String,
}

impl Capsule {
    fn new(project_in_home: bool, native_tool: Option<&str>) -> Self {
        let (root, base) = scratch();
        let home = base.join("home");
        let project = if project_in_home {
            home.join("project")
        } else {
            base.join("project")
        };
        let fixture = base.join("fixture");
        for dir in [&home, &project, &fixture] {
            fs::create_dir_all(dir).unwrap();
        }
        let capsule = Self {
            _root: root,
            base,
            home,
            project,
            capabilities: String::new(),
            entries: String::new(),
        };
        capsule.write_manifest("http://127.0.0.1:9");
        let driver = common::create_driver_artifact(
            &fixture,
            DRIVER,
            DRIVER_VERSION,
            &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
        );
        capsule.install(&driver, &["--no-precompile"]).success();
        if let Some(body) = native_tool {
            let artifact = common::create_native_artifact(
                &fixture,
                REACH_TOOL,
                TOOL_VERSION,
                &native_script(body),
                Some("Reports what it can reach."),
                None,
            );
            capsule.install(&artifact, &[]).success();
        }
        capsule
    }

    fn install(&self, artifact: &Path, extra: &[&str]) -> assert_cmd::assert::Assert {
        Command::cargo_bin("mur")
            .unwrap()
            .env("HOME", &self.home)
            .current_dir(&self.project)
            .args(["install", artifact.to_str().unwrap()])
            .args(extra)
            .assert()
    }

    /// Sets the capsule-wide capabilities and rewrites the manifest, so `--explain-scope` and
    /// `mur doctor` read it before any run.
    fn with(mut self, capabilities: &str) -> Self {
        self.capabilities = capabilities.to_string();
        self.write_manifest("http://127.0.0.1:9");
        self
    }

    fn with_native_tool(mut self) -> Self {
        self.entries =
            format!("  - name: {REACH_TOOL}\n    version: {TOOL_VERSION}\n    runtime: tool\n");
        self.write_manifest("http://127.0.0.1:9");
        self
    }

    fn manifest(&self) -> PathBuf {
        self.project.join("murmur.yaml")
    }

    fn write_manifest(&self, endpoint: &str) {
        fs::write(
            self.manifest(),
            format!(
                "name: {CAPSULE_NAME}\nversion: 0.0.1\nartifacts:\n  - name: {DRIVER}\n    version: \
                 {DRIVER_VERSION}\n    runtime: driver\n    gateway:\n      endpoint: {endpoint}\n      \
                 api_key: test-key\n{}capabilities:\n  network:\n    allow:\n      - {endpoint}\n{}\
                 inference:\n  transport: http\n  model: test-model\n  driver:\n    artifact: {DRIVER}\n",
                self.entries, self.capabilities,
            ),
        )
        .unwrap();
    }

    /// Writes the global `~/.murmur/config.yaml` with `entries` under `host_paths:`.
    fn global_config(&self, entries: &str) {
        fs::create_dir_all(self.home.join(".murmur")).unwrap();
        fs::write(
            self.home.join(".murmur/config.yaml"),
            format!("host_paths:\n{entries}"),
        )
        .unwrap();
    }

    fn mur(&self, env: &[(&str, &str)]) -> Command {
        let mut cmd = Command::cargo_bin("mur").unwrap();
        cmd.env("HOME", &self.home).current_dir(&self.project);
        for (key, value) in env {
            cmd.env(key, value);
        }
        cmd
    }

    /// One `mur run --task` on a model scripted with `responses` then `end_turn`. Returns the
    /// output and every request the model received.
    fn run(&self, responses: Vec<String>, env: &[(&str, &str)]) -> (Output, Vec<Value>) {
        let mut responses = responses;
        responses.push(end_turn_response());
        let server = common::ScriptedServer::start(responses);
        self.write_manifest(&server.endpoint);
        let output = self
            .mur(env)
            .args([
                "run",
                "--manifest",
                self.manifest().to_str().unwrap(),
                "--task",
                "Run each call once.",
                "--verbose",
            ])
            .timeout(std::time::Duration::from_secs(300))
            .output()
            .unwrap();
        (output, server.requests())
    }

    fn explain(&self, json: bool, env: &[(&str, &str)]) -> Output {
        let mut cmd = self.mur(env);
        cmd.args([
            "run",
            "--manifest",
            self.manifest().to_str().unwrap(),
            "--explain-scope",
        ]);
        if json {
            cmd.arg("--json");
        }
        cmd.output().unwrap()
    }

    fn explain_json(&self, env: &[(&str, &str)]) -> Value {
        let output = self.explain(true, env);
        assert!(output.status.success(), "{}", stderr(&output));
        serde_json::from_slice(&output.stdout).expect("one JSON object")
    }

    fn doctor(&self) -> Output {
        self.mur(&[]).arg("doctor").output().unwrap()
    }

    fn records_entries(&self) -> usize {
        fs::read_dir(self.home.join(".murmur/records"))
            .map(|dir| dir.count())
            .unwrap_or(0)
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn bash(id: &str, command: &str) -> String {
    common::tool_use_response(id, "bash", json!({ "command": command }))
}

/// The text of tool call `id`'s result, which must exist.
fn result(requests: &[Value], id: &str, output: &Output) -> String {
    common::tool_result_text(requests, id)
        .unwrap_or_else(|| panic!("no tool result for {id}; stderr: {}", stderr(output)))
}

fn has_line(text: &str, line: &str) -> bool {
    text.lines().any(|candidate| candidate.trim() == line)
}

fn assert_ok(output: &Output) {
    assert!(
        output.status.success() && stdout(output).contains("status:  ok"),
        "the run must finish ok\nstdout: {}\nstderr: {}",
        stdout(output),
        stderr(output)
    );
}

fn session_start(output: &Output) -> Value {
    let records = common::parse_records_from_stdout(&stdout(output));
    common::read_whole_trace(&records.join("trace.jsonl"))
        .into_iter()
        .find(|event| event["event_type"] == "session_start")
        .expect("session_start")
}

/// The tier `--explain-scope --json` names, printed so the record says what each test ran on.
fn report_tier(capsule: &Capsule, test: &str, expected: &str) {
    let report = capsule.explain_json(&[]);
    let tier = report["enforcement_tier"].as_str().unwrap().to_string();
    eprintln!("{test}: enforcement_tier {tier}");
    assert_eq!(tier, expected, "{test} must run on {expected}");
}

const SCOPED_TIER: &str = "landlock+seccomp";
const SEALED_TIER: &str = "mountns+pivot_root+landlock+seccomp";

// ── S2, S3, S4: refusals ───────────────────────────────────────────────────────────────

#[test]
fn a_project_config_naming_host_paths_is_refused_before_staging() {
    for project_config in [
        "host_paths:\n  - path: /srv\n    access: read\n",
        "host_paths: []\n",
    ] {
        let capsule = Capsule::new(false, None);
        fs::create_dir_all(capsule.project.join(".murmur")).unwrap();
        let project_file = capsule.project.join(".murmur/config.yaml");
        fs::write(&project_file, project_config).unwrap();

        let (run, _) = capsule.run(Vec::new(), &[]);
        let explain = capsule.explain(true, &[]);
        for output in [&run, &explain] {
            assert!(!output.status.success());
            let text = format!("{}{}", stdout(output), stderr(output));
            assert!(text.contains("E-CFG-004"), "{text}");
            assert!(text.contains(&project_file.display().to_string()), "{text}");
            assert!(text.contains("~/.murmur/config.yaml) only"), "{text}");
        }
        assert_eq!(capsule.records_entries(), 0, "nothing is staged");
    }
}

#[test]
fn a_manifest_naming_host_paths_is_refused_with_e_man_003() {
    let capsule = Capsule::new(false, None).with("  host_paths: []\n");
    let (output, _) = capsule.run(Vec::new(), &[]);
    assert!(!output.status.success());
    let text = format!("{}{}", stdout(&output), stderr(&output));
    assert!(text.contains("E-MAN-003"), "{text}");
    assert!(text.contains("'capabilities.host_paths'"), "{text}");
    assert!(text.contains("~/.murmur/config.yaml"), "{text}");
    assert!(!text.contains("W-SEC-019"), "{text}");
}

#[test]
fn a_root_or_missing_host_path_refuses_the_run_and_explain_scope_naming_it() {
    let capsule = Capsule::new(false, None);
    let missing = capsule.base.join("does-not-exist");
    for (entry, written, reason) in [
        ("/", "/".to_string(), "names /"),
        (
            missing.to_str().unwrap(),
            missing.display().to_string(),
            "does not exist",
        ),
    ] {
        capsule.global_config(&format!("  - path: {entry}\n    access: read\n"));
        let (run, _) = capsule.run(Vec::new(), &[]);
        let explain = capsule.explain(false, &[]);
        for output in [&run, &explain] {
            assert!(!output.status.success());
            let text = format!("{}{}", stdout(output), stderr(output));
            assert!(text.contains("E-CFG-004"), "{text}");
            assert!(
                text.contains(&format!("host_paths[0] ({written}")),
                "{text}"
            );
            assert!(text.contains(reason), "{text}");
        }
        assert_eq!(capsule.records_entries(), 0, "nothing is staged");
    }
}

// ── S5: credential locations warn ──────────────────────────────────────────────────────

#[test]
fn a_credential_location_warns_and_launches() {
    const TEST: &str = "a_credential_location_warns_and_launches";
    if capsule_runtime::skip_without_egress_namespace(TEST) {
        return;
    }
    let capsule =
        Capsule::new(false, None).with("  containment: scoped\n  shell:\n    allow: [bash]\n");
    fs::create_dir_all(capsule.home.join(".ssh")).unwrap();
    fs::create_dir_all(capsule.home.join(".cargo")).unwrap();
    fs::write(capsule.home.join(".cargo/credentials.toml"), "").unwrap();
    fs::create_dir_all(capsule.home.join("plain")).unwrap();
    capsule.global_config(
        "  - path: ~/.ssh\n    access: read\n  - path: ~/.cargo\n    access: read-write\n  \
         - path: ~/plain\n    access: read\n",
    );

    let (output, _) = capsule.run(Vec::new(), &[]);
    assert_ok(&output);
    let warnings: Vec<String> = stderr(&output)
        .lines()
        .filter(|line| line.contains("W-SEC-035"))
        .map(str::to_string)
        .collect();
    assert_eq!(warnings.len(), 2, "{warnings:#?}");
    assert!(
        warnings[0].contains("host_paths entry ~/.ssh (read)"),
        "{}",
        warnings[0]
    );
    assert!(
        warnings[0].contains("credential location ~/.ssh"),
        "{}",
        warnings[0]
    );
    assert!(
        warnings[1].contains("host_paths entry ~/.cargo (read-write)"),
        "{}",
        warnings[1]
    );
    assert!(
        warnings[1].contains("credential location ~/.cargo/credentials.toml"),
        "{}",
        warnings[1]
    );
    for warning in &warnings {
        assert!(warning.contains("diagnostics/#w-sec-035"), "{warning}");
    }

    let grants = &session_start(&output)["effective_grants"]["host_paths"];
    assert_eq!(grants[0]["credential_location"], json!("~/.ssh"));
    assert_eq!(
        grants[1]["credential_location"],
        json!("~/.cargo/credentials.toml")
    );
    assert_eq!(grants[2]["credential_location"], Value::Null);

    let doctor = capsule.doctor();
    let doctor_text = format!("{}{}", stdout(&doctor), stderr(&doctor));
    for warning in &warnings {
        let body = &warning[warning.find("warning[W-SEC-035]").unwrap()..];
        assert!(
            doctor_text.contains(body),
            "doctor must print {body}\n{doctor_text}"
        );
    }
}

// ── S6, S7, S8, S12: reach ─────────────────────────────────────────────────────────────

/// `ro/` holding `in.txt`, `rw/` and `outside/` holding `secret.txt`, beside the project.
struct Dirs {
    ro: PathBuf,
    rw: PathBuf,
    outside: PathBuf,
}

impl Dirs {
    fn under(base: &Path) -> Self {
        let dirs = Self {
            ro: base.join("ro"),
            rw: base.join("rw"),
            outside: base.join("outside"),
        };
        for dir in [&dirs.ro, &dirs.rw, &dirs.outside] {
            fs::create_dir_all(dir).unwrap();
        }
        fs::write(dirs.ro.join("in.txt"), "lent\n").unwrap();
        fs::write(dirs.outside.join("secret.txt"), "secret\n").unwrap();
        dirs
    }

    fn config(&self) -> String {
        format!(
            "  - path: {}\n    access: read\n  - path: {}\n    access: read-write\n",
            self.ro.display(),
            self.rw.display()
        )
    }
}

/// What the native reach tool does, reporting `read:<line> <ro> <rename> <outside>`.
fn reach_body(dirs: &Dirs) -> String {
    format!(
        "if read -r l < '{ro}/in.txt'; then a=\"read:$l\"; else a=read-refused; fi\n\
         if echo x > '{ro}/tool-new' 2>/dev/null; then b=ro-written; else b=ro-refused; fi\n\
         if echo y > '{rw}/tool-a' && mv '{rw}/tool-a' '{rw}/tool-b'; then c=renamed; else c=rename-failed; fi\n\
         if read -r s < '{out}/secret.txt' 2>/dev/null; then d=\"leak:$s\"; else d=outside-refused; fi\n\
         v=\"$a $b $c $d\"",
        ro = dirs.ro.display(),
        rw = dirs.rw.display(),
        out = dirs.outside.display(),
    )
}

/// S6, S7 and S12 on one containment class: the runtime's `bash` tool and a native tool reach
/// `ro/` read-only, `rw/` read-write and not `outside/`; `HOME` is the session's and
/// `CARGO_HOME` the value `mur run` was started with.
fn reach_on(test: &str, containment: &str, tier: &str) {
    if capsule_runtime::skip_without_egress_namespace(test) {
        return;
    }
    if containment == "sealed" && !host_reaches_sealed(test) {
        return;
    }
    let (_scratch, base) = scratch();
    let dirs = Dirs::under(&base);
    let capsule = Capsule::new(false, Some(&reach_body(&dirs)))
        .with_native_tool()
        .with(&format!(
            "  containment: {containment}\n  shell:\n    allow: [bash, mv]\n"
        ));
    capsule.global_config(&dirs.config());
    report_tier(&capsule, test, tier);

    let ro = dirs.ro.display();
    let rw = dirs.rw.display();
    let out = dirs.outside.display();
    let calls = [
        format!("read -r l < '{ro}/in.txt' && echo \"read:$l\""),
        format!("echo x > '{ro}/new' && echo wrote || echo write-refused"),
        format!("echo written > '{rw}/new' && echo wrote-rw"),
        format!("read -r s < '{out}/secret.txt' && echo \"leak:$s\" || echo outside-refused"),
        format!("[ -e '{out}/secret.txt' ] && echo outside-present || echo outside-absent"),
        "echo \"HOME=$HOME\"; echo \"CARGO_HOME=$CARGO_HOME\"".to_string(),
    ];
    let mut responses: Vec<String> = calls
        .iter()
        .enumerate()
        .map(|(index, command)| bash(&format!("call_{index}"), command))
        .collect();
    responses.push(common::tool_use_response(
        "call_tool",
        murmur_artifact::tool_name_of(REACH_TOOL),
        json!({}),
    ));
    let cargo_home = dirs.rw.display().to_string();
    let (output, requests) = capsule.run(responses, &[("CARGO_HOME", &cargo_home)]);
    assert_ok(&output);

    let read = result(&requests, "call_0", &output);
    assert!(has_line(&read, "read:lent"), "{read}");

    let write_ro = result(&requests, "call_1", &output);
    assert!(has_line(&write_ro, "write-refused"), "{write_ro}");
    if containment == "scoped" {
        assert!(write_ro.contains("Permission denied"), "{write_ro}");
    }
    assert!(!dirs.ro.join("new").exists());

    let write_rw = result(&requests, "call_2", &output);
    assert!(has_line(&write_rw, "wrote-rw"), "{write_rw}");
    assert_eq!(
        fs::read_to_string(dirs.rw.join("new")).unwrap(),
        "written\n"
    );

    let outside = result(&requests, "call_3", &output);
    assert!(has_line(&outside, "outside-refused"), "{outside}");
    assert!(!outside.contains("leak:"), "{outside}");

    let exists = result(&requests, "call_4", &output);
    if containment == "sealed" {
        assert!(has_line(&exists, "outside-absent"), "{exists}");
    }

    let env = result(&requests, "call_5", &output);
    let home_line = env
        .lines()
        .find(|line| line.starts_with("HOME="))
        .unwrap_or_else(|| panic!("{env}"));
    let session_home = &home_line["HOME=".len()..];
    assert_ne!(Path::new(session_home), capsule.home.as_path(), "{env}");
    assert!(
        session_home.starts_with(&capsule.home.join(".murmur/records").display().to_string()),
        "HOME is the session's home folder: {env}"
    );
    assert!(has_line(&env, &format!("CARGO_HOME={cargo_home}")), "{env}");

    let tool = result(&requests, "call_tool", &output);
    assert!(
        tool.contains("<<<read:lent ro-refused renamed outside-refused>>>"),
        "{tool}"
    );
    assert!(!dirs.ro.join("tool-new").exists());
    assert_eq!(fs::read_to_string(dirs.rw.join("tool-b")).unwrap(), "y\n");
    assert!(!dirs.rw.join("tool-a").exists());

    let grants = &session_start(&output)["effective_grants"]["host_paths"];
    let mechanism = if containment == "sealed" {
        "bind"
    } else {
        "landlock"
    };
    assert_eq!(grants[0]["mechanism"], json!(mechanism));
    assert_eq!(grants[1]["access"], json!("read-write"));
}

#[test]
fn scoped_host_paths_read_write_and_outside() {
    reach_on(
        "scoped_host_paths_read_write_and_outside",
        "scoped",
        SCOPED_TIER,
    );
}

#[test]
fn sealed_host_paths_are_bound_at_their_own_path() {
    reach_on(
        "sealed_host_paths_are_bound_at_their_own_path",
        "sealed",
        SEALED_TIER,
    );
}

/// The test `HOME` itself lent `read`, with the project (and so the workdir) inside it: the run
/// writes its workdir and is refused a write into its own records directory.
fn home_lent_read_on(test: &str, containment: &str, tier: &str) {
    if capsule_runtime::skip_without_egress_namespace(test) {
        return;
    }
    if containment == "sealed" && !host_reaches_sealed(test) {
        return;
    }
    let capsule = Capsule::new(true, None).with(&format!(
        "  containment: {containment}\n  shell:\n    allow: [bash]\n"
    ));
    capsule.global_config("  - path: \"~\"\n    access: read\n");
    report_tier(&capsule, test, tier);
    let (output, requests) = capsule.run(
        vec![
            bash("call_0", "echo w > workfile && echo wrote-workdir"),
            bash(
                "call_1",
                "echo x > \"$MURMUR_RECORDS_DIR/intruder\" && echo wrote-records || echo records-refused",
            ),
        ],
        &[],
    );
    assert_ok(&output);
    let workdir = result(&requests, "call_0", &output);
    assert!(has_line(&workdir, "wrote-workdir"), "{workdir}");
    let records = result(&requests, "call_1", &output);
    assert!(has_line(&records, "records-refused"), "{records}");
    let records_dir = common::parse_records_from_stdout(&stdout(&output));
    assert!(!records_dir.join("intruder").exists());
}

#[test]
fn scoped_home_lent_read_keeps_the_workdir_writable_and_the_records_read_only() {
    home_lent_read_on(
        "scoped_home_lent_read_keeps_the_workdir_writable_and_the_records_read_only",
        "scoped",
        SCOPED_TIER,
    );
}

#[test]
fn sealed_home_lent_read_keeps_the_workdir_writable_and_the_records_read_only() {
    home_lent_read_on(
        "sealed_home_lent_read_keeps_the_workdir_writable_and_the_records_read_only",
        "sealed",
        SEALED_TIER,
    );
}

// ── S9: a read-write entry over something the session may run ──────────────────────────

/// `tools/bin/tool`, an executable `#!/bin/bash` script, and the `PATH` that finds it first.
struct Tools {
    tools: PathBuf,
    bin: PathBuf,
    path_env: String,
}

impl Tools {
    fn under(base: &Path) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let tools = base.join("tools");
        let bin = tools.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let tool = bin.join("tool");
        fs::write(&tool, "#!/bin/bash\necho tool ran\n").unwrap();
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
        let path_env = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        Self {
            tools,
            bin,
            path_env,
        }
    }
}

#[test]
fn scoped_refuses_a_read_write_host_path_holding_what_it_may_run() {
    let (_scratch, base) = scratch();
    let tools = Tools::under(&base);
    let capsule = Capsule::new(false, None)
        .with("  containment: scoped\n  shell:\n    allow: [bash, tool]\n");
    capsule.global_config(&format!(
        "  - path: {}\n    access: read-write\n",
        tools.tools.display()
    ));
    let env = [("PATH", tools.path_env.as_str())];
    let (run, _) = capsule.run(Vec::new(), &env);
    let explain = capsule.explain(true, &env);
    for output in [&run, &explain] {
        assert!(!output.status.success());
        let text = format!("{}{}", stdout(output), stderr(output));
        assert!(text.contains("E-CFG-004"), "{text}");
        assert!(
            text.contains(&format!("host_paths[0] ({}", tools.tools.display())),
            "{text}"
        );
        assert!(
            text.contains(&tools.bin.join("tool").display().to_string()),
            "names the binary: {text}"
        );
    }
}

#[test]
fn sealed_binds_what_it_may_run_read_only_inside_a_read_write_host_path() {
    const TEST: &str = "sealed_binds_what_it_may_run_read_only_inside_a_read_write_host_path";
    if capsule_runtime::skip_without_egress_namespace(TEST) || !host_reaches_sealed(TEST) {
        return;
    }
    let (_scratch, base) = scratch();
    let tools = Tools::under(&base);
    let capsule = Capsule::new(false, None).with(&format!(
        "  containment: sealed\n  shell:\n    allow: [bash, tool]\n    interpreter_runtime:\n      \
         - binary: tool\n        dirs:\n          - path: {}\n            list_dir: true\n",
        tools.bin.display()
    ));
    capsule.global_config(&format!(
        "  - path: {}\n    access: read-write\n",
        tools.tools.display()
    ));
    let env = [("PATH", tools.path_env.as_str())];
    let report = capsule.explain_json(&env);
    eprintln!("{TEST}: enforcement_tier {}", report["enforcement_tier"]);
    assert_eq!(report["enforcement_tier"], json!(SEALED_TIER));

    let bin = tools.bin.display();
    let (output, requests) = capsule.run(
        vec![
            bash("call_0", "tool"),
            bash(
                "call_1",
                &format!("echo x > '{bin}/tool' && echo rewrote || echo rewrite-refused"),
            ),
            bash(
                "call_2",
                &format!("echo x > '{bin}/new' && echo added || echo add-refused"),
            ),
            bash(
                "call_3",
                &format!(
                    "echo kept > '{}/notes' && echo wrote-notes",
                    tools.tools.display()
                ),
            ),
        ],
        &env,
    );
    assert_ok(&output);
    assert!(has_line(&result(&requests, "call_0", &output), "tool ran"));
    for (id, line) in [("call_1", "rewrite-refused"), ("call_2", "add-refused")] {
        let text = result(&requests, id, &output);
        assert!(has_line(&text, line), "{text}");
        assert!(text.contains("Read-only file system"), "{text}");
    }
    assert_eq!(
        fs::read_to_string(tools.bin.join("tool")).unwrap(),
        "#!/bin/bash\necho tool ran\n"
    );
    assert!(!tools.bin.join("new").exists());
    assert!(has_line(
        &result(&requests, "call_3", &output),
        "wrote-notes"
    ));
    assert_eq!(
        fs::read_to_string(tools.tools.join("notes")).unwrap(),
        "kept\n"
    );
}

#[test]
fn a_read_write_host_path_inside_what_it_may_run_is_refused_on_both_tiers() {
    const TEST: &str = "a_read_write_host_path_inside_what_it_may_run_is_refused_on_both_tiers";
    let (_scratch, base) = scratch();
    let tools = Tools::under(&base);
    let mut classes = vec!["scoped"];
    if host_reaches_sealed(TEST) {
        classes.push("sealed");
    }
    for containment in classes {
        let capsule = Capsule::new(false, None).with(&format!(
            "  containment: {containment}\n  shell:\n    allow: [bash, tool]\n    \
             interpreter_runtime:\n      - binary: tool\n        dirs:\n          - path: {}\n            \
             list_dir: true\n",
            tools.bin.display()
        ));
        capsule.global_config(&format!(
            "  - path: {}\n    access: read-write\n",
            tools.bin.display()
        ));
        let output = capsule.explain(true, &[("PATH", tools.path_env.as_str())]);
        assert!(!output.status.success(), "{containment}");
        let text = format!("{}{}", stdout(&output), stderr(&output));
        assert!(text.contains("E-CFG-004"), "{containment}: {text}");
        assert!(
            text.contains(&format!("inside {}", tools.bin.display())),
            "{containment}: names the execute root: {text}"
        );
    }
}

// ── S10, S11: reports ──────────────────────────────────────────────────────────────────

#[test]
fn host_paths_appear_in_explain_scope_text_json_and_doctor() {
    let (_scratch, base) = scratch();
    let dirs = Dirs::under(&base);
    let capsule = Capsule::new(false, None).with("  containment: scoped\n");

    // No key: `[]`, always serialized.
    assert_eq!(capsule.explain_json(&[])["host_paths"], json!([]));

    capsule.global_config(&dirs.config());
    let report = capsule.explain_json(&[]);
    let mechanism = match report["enforcement_tier"].as_str().unwrap() {
        SEALED_TIER => "bind",
        SCOPED_TIER => "landlock",
        _ => "none",
    };
    assert_eq!(
        report["host_paths"],
        json!([
            {
                "path": dirs.ro.display().to_string(),
                "declared": dirs.ro.display().to_string(),
                "access": "read",
                "mechanism": mechanism,
                "credential_location": null,
            },
            {
                "path": dirs.rw.display().to_string(),
                "declared": dirs.rw.display().to_string(),
                "access": "read-write",
                "mechanism": mechanism,
                "credential_location": null,
            },
        ])
    );

    let text = stdout(&capsule.explain(false, &[]));
    assert!(text.contains("  host paths:\n"), "{text}");
    assert!(
        text.contains(&format!("    - {ro} → {ro} (read)", ro = dirs.ro.display())),
        "{text}"
    );

    let doctor = capsule.doctor();
    let doctor_out = stdout(&doctor);
    assert!(doctor_out.contains("Host paths\n"), "{doctor_out}");
    assert!(
        doctor_out.contains(&format!(
            "  {rw} → {rw} (read-write): ",
            rw = dirs.rw.display()
        )),
        "{doctor_out}"
    );

    // A refusal is a doctor warning, and the checklist carries on to its tally.
    capsule.global_config(&format!(
        "  - path: {}\n    access: read\n",
        base.join("missing").display()
    ));
    let doctor = capsule.doctor();
    let doctor_text = format!("{}{}", stdout(&doctor), stderr(&doctor));
    assert!(
        doctor_text.contains("[mur doctor] warning[E-CFG-004]"),
        "{doctor_text}"
    );
    assert!(
        doctor_text.contains("`mur run` will refuse"),
        "{doctor_text}"
    );
    assert!(
        stdout(&doctor).contains("passed") || stdout(&doctor).contains("All checks"),
        "the checklist continues: {doctor_text}"
    );
}
