//! `mur run --unconfined` and `containment: unconfined` in `~/.murmur/config.yaml`: the capsule's
//! commands run as the operator, and only the operator can choose it.
//!
//! The model is scripted. What runs is real: `mur` itself, the staged session, the shell command
//! it spawns, and the records it writes. Every scratch directory is created under
//! `CARGO_TARGET_TMPDIR`. An unconfined run creates no namespace, so its half never skips; a
//! contained comparison half that declares `shell.allow` first asks whether this host can give it
//! one.

#[path = "common/mod.rs"]
mod common;

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Output;

use assert_cmd::Command;
use serde_json::{json, Value};
use tempfile::TempDir;

const DRIVER_NAME: &str = "local/murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";

/// A fresh scratch directory under `CARGO_TARGET_TMPDIR`, never under `/tmp`.
fn scratch() -> TempDir {
    tempfile::Builder::new()
        .prefix("unconfined-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap()
}

fn end_turn(text: &str) -> String {
    json!({
        "id": "msg_end",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

fn bash(tool_id: &str, command: &str) -> String {
    common::tool_use_response(tool_id, "bash", json!({ "command": command }))
}

/// A scratch `HOME`, a project and an artifact directory, with the anthropic fixture driver
/// published into `HOME`.
struct Fixture {
    home: TempDir,
    project: TempDir,
    artifacts: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let fixture = Self {
            home: scratch(),
            project: scratch(),
            artifacts: scratch(),
        };
        let driver = common::create_driver_artifact(
            fixture.artifacts.path(),
            DRIVER_NAME,
            DRIVER_VERSION,
            &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
        );
        common::publish_local(&fixture.home, &driver).success();
        fixture
    }

    /// Writes an agent capsule's `murmur.yaml` into the project: the driver at `endpoint`, the
    /// `artifacts` entries after it, `capabilities` (each line indented under `capabilities:`)
    /// and `inference` lines after the driver binding.
    fn manifest(&self, endpoint: &str, artifacts: &str, capabilities: &str, inference: &str) {
        write_agent_manifest(
            self.project.path(),
            endpoint,
            artifacts,
            capabilities,
            inference,
        );
    }

    /// `~/.murmur/config.yaml` in the scratch `HOME`.
    fn global_config(&self, yaml: &str) {
        let dir = self.home.path().join(".murmur");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("config.yaml"), yaml).unwrap();
    }

    /// `<project>/.murmur/config.yaml`, read because `mur` runs in the project directory.
    fn project_config(&self, yaml: &str) {
        let dir = self.project.path().join(".murmur");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("config.yaml"), yaml).unwrap();
    }

    fn mur(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut command = Command::cargo_bin("mur").unwrap();
        command
            .env("HOME", self.home.path())
            .current_dir(self.project.path())
            .args(["run", "--manifest", "murmur.yaml"])
            .args(args)
            .write_stdin("");
        for (key, value) in env {
            command.env(key, value);
        }
        command.output().unwrap()
    }

    /// `mur run --task go` with `args`, which must succeed.
    fn run(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let output = self.mur(&[&["--task", "go"], args].concat(), env);
        assert!(
            output.status.success(),
            "mur run failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    /// `mur run --explain-scope --json` with `args`.
    fn explain(&self, args: &[&str]) -> Value {
        let output = self.mur(&[&["--explain-scope", "--json"], args].concat(), &[]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).expect("one JSON object")
    }

    /// The newest session's `trace.jsonl`, one value per line.
    fn trace(&self) -> Vec<Value> {
        let records =
            common::newest_session_records(self.home.path(), &self.project.path().join("workdir"))
                .expect("a session was recorded");
        common::read_whole_trace(&records.join("trace.jsonl"))
    }

    fn records_exist(&self) -> bool {
        common::records_root(self.home.path()).exists()
    }
}

fn write_agent_manifest(
    dir: &Path,
    endpoint: &str,
    artifacts: &str,
    capabilities: &str,
    inference: &str,
) {
    let manifest = format!(
        "name: unconfined-capsule\n\
         version: 0.1.0\n\
         artifacts:\n\
         \x20 - name: {DRIVER_NAME}\n\
         \x20   version: {DRIVER_VERSION}\n\
         \x20   runtime: driver\n\
         \x20   gateway:\n\
         \x20     endpoint: {endpoint}\n\
         \x20     api_key: test-key\n\
         {artifacts}\
         capabilities:\n\
         \x20 network:\n\
         \x20   allow:\n\
         \x20     - {endpoint}\n\
         {capabilities}\
         inference:\n\
         \x20 transport: http\n\
         \x20 model: test-model\n\
         \x20 driver:\n\
         \x20   artifact: {DRIVER_NAME}\n\
         {inference}"
    );
    fs::write(dir.join("murmur.yaml"), manifest).unwrap();
}

const SHELL_ALLOW_BASH: &str = "  shell:\n    allow:\n      - bash\n";

fn first(trace: &[Value], event_type: &str) -> Value {
    trace
        .iter()
        .find(|event| event["event_type"] == event_type)
        .unwrap_or_else(|| panic!("no {event_type} in {trace:#?}"))
        .clone()
}

fn lines(text: &str) -> Vec<&str> {
    text.lines().collect()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// This process's `/proc/self/status` value for `field`, e.g. `0` for `NoNewPrivs`.
fn own_status(field: &str) -> String {
    let status = fs::read_to_string("/proc/self/status").unwrap();
    status
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{field}:")))
        .unwrap()
        .trim()
        .to_string()
}

/// This process's soft and hard `RLIMIT_NOFILE`, as `ulimit -Sn` and `ulimit -Hn` print them: a
/// child started without changing its limits inherits both.
fn own_nofile() -> String {
    let output = std::process::Command::new("bash")
        .args(["-c", "echo \"$(ulimit -Sn) $(ulimit -Hn)\""])
        .output()
        .expect("bash runs");
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn count(text: &str, needle: &str) -> usize {
    text.matches(needle).count()
}

/// Every refusal exits non-zero before staging — no records directory, no workdir — through
/// `mur run` and through `--explain-scope --json` alike.
#[test]
fn every_refusal_comes_before_staging() {
    struct Case {
        name: &'static str,
        containment: Option<&'static str>,
        global: Option<&'static str>,
        project: Option<&'static str>,
        args: &'static [&'static str],
        expect: &'static [&'static str],
        usage_error: bool,
    }
    let cases = [
        Case {
            name: "manifest unconfined",
            containment: Some("unconfined"),
            global: None,
            project: None,
            args: &[],
            expect: &["capabilities.containment", "mur run --unconfined"],
            usage_error: false,
        },
        Case {
            name: "project config unconfined",
            containment: None,
            global: None,
            project: Some("containment: unconfined\n"),
            args: &[],
            expect: &["E-CFG-005", ".murmur/config.yaml"],
            usage_error: false,
        },
        Case {
            name: "--containment unconfined",
            containment: None,
            global: None,
            project: None,
            args: &["--containment", "unconfined"],
            expect: &["E-IO-003", "--unconfined"],
            usage_error: false,
        },
        Case {
            name: "--unconfined --containment scoped",
            containment: None,
            global: None,
            project: None,
            args: &["--unconfined", "--containment", "scoped"],
            expect: &["--unconfined", "--containment"],
            usage_error: true,
        },
        Case {
            name: "manifest scoped + --unconfined",
            containment: Some("scoped"),
            global: None,
            project: None,
            args: &["--unconfined"],
            expect: &[
                "E-RUN-059",
                "--unconfined",
                "scoped in the manifest's capabilities.containment",
            ],
            usage_error: false,
        },
        Case {
            name: "manifest advisory + --unconfined",
            containment: Some("advisory"),
            global: None,
            project: None,
            args: &["--unconfined"],
            expect: &[
                "E-RUN-059",
                "--unconfined",
                "advisory in the manifest's capabilities.containment",
            ],
            usage_error: false,
        },
        Case {
            name: "project scoped + --unconfined",
            containment: None,
            global: None,
            project: Some("containment: scoped\n"),
            args: &["--unconfined"],
            expect: &["E-RUN-059", "--unconfined", "scoped in .murmur/config.yaml"],
            usage_error: false,
        },
        Case {
            name: "global scoped + --unconfined",
            containment: None,
            global: Some("containment: scoped\n"),
            project: None,
            args: &["--unconfined"],
            expect: &[
                "E-RUN-059",
                "--unconfined",
                "scoped in ~/.murmur/config.yaml",
            ],
            usage_error: false,
        },
        Case {
            name: "global unconfined + manifest sealed",
            containment: Some("sealed"),
            global: Some("containment: unconfined\n"),
            project: None,
            args: &[],
            expect: &[
                "E-RUN-059",
                "containment: unconfined in ~/.murmur/config.yaml",
                "sealed in the manifest's capabilities.containment",
            ],
            usage_error: false,
        },
        Case {
            name: "--unconfined --contained-launch",
            containment: None,
            global: None,
            project: None,
            args: &["--unconfined", "--contained-launch"],
            expect: &["E-RUN-060", "--contained-launch"],
            usage_error: false,
        },
        Case {
            name: "--unconfined --spawn-grant-stdin",
            containment: None,
            global: None,
            project: None,
            args: &["--unconfined", "--spawn-grant-stdin"],
            expect: &["E-RUN-060", "--spawn-grant-stdin"],
            usage_error: false,
        },
    ];
    for case in &cases {
        let fixture = Fixture::new();
        let capabilities = case
            .containment
            .map(|class| format!("  containment: {class}\n"))
            .unwrap_or_default();
        fixture.manifest("http://127.0.0.1:9", "", &capabilities, "");
        if let Some(yaml) = case.global {
            fixture.global_config(yaml);
        }
        if let Some(yaml) = case.project {
            fixture.project_config(yaml);
        }
        for explain in [false, true] {
            let mut args: Vec<&str> = case.args.to_vec();
            if explain {
                args.extend(["--explain-scope", "--json"]);
            } else {
                args.extend(["--task", "go"]);
            }
            let output = fixture.mur(&args, &[]);
            let err = stderr(&output);
            assert!(!output.status.success(), "{}: {err}", case.name);
            if case.usage_error {
                assert_eq!(output.status.code(), Some(2), "{}: {err}", case.name);
            }
            for needle in case.expect {
                assert!(err.contains(needle), "{}: {needle} not in {err}", case.name);
            }
            assert!(
                !fixture.records_exist(),
                "{}: a refused launch left a records directory",
                case.name
            );
            assert!(
                !fixture.project.path().join("workdir").exists(),
                "{}: a refused launch left a workdir",
                case.name
            );
        }
    }
}

/// The probe the scripted model runs: one marker line per fact, builtins only, so a contained
/// run of it is refused nothing but what its tier refuses.
fn operator_probe(secret: &Path, rw: &Path, port: u16) -> String {
    format!(
        "echo \"home=$HOME\"; echo \"token=$GITHUB_TOKEN\"; echo \"var=$MURMUR_TEST_UNCONFINED\"; \
         while read -r l; do case \"$l\" in NoNewPrivs:*) set -- $l; echo \"nnp=$2\";; esac; \
         done < /proc/self/status; \
         if read -r s < {secret}; then echo \"secret=$s\"; else echo secret=refused; fi; \
         if echo new > {rw}/new; then echo write=ok; else echo write=refused; fi; \
         echo \"nofile=$(ulimit -Sn) $(ulimit -Hn)\"; \
         if (exec 3<>/dev/tcp/127.0.0.1/{port}) 2>/dev/null; then echo tcp=ok; \
         else echo tcp=refused; fi",
        secret = secret.display(),
        rw = rw.display(),
    )
}

/// A file to read and a directory to write, both outside the workdir.
struct Outside {
    _dir: TempDir,
    secret: PathBuf,
    rw: PathBuf,
}

fn outside_targets() -> Outside {
    let dir = scratch();
    let secret = dir.path().join("secret.txt");
    fs::write(&secret, "outside-secret\n").unwrap();
    let rw = dir.path().join("rw");
    fs::create_dir(&rw).unwrap();
    Outside {
        _dir: dir,
        secret,
        rw,
    }
}

const UNCONFINED_ENV: &[(&str, &str)] = &[
    ("GITHUB_TOKEN", "unconfined-sentinel"),
    ("MURMUR_TEST_UNCONFINED", "present"),
];

/// An unconfined shell command runs as the operator — the real `HOME`, the whole environment,
/// `NoNewPrivs` as this process has it, the host filesystem and network, and the limits `mur run`
/// itself has — and every report says so. The same probe under `--containment scoped` sees the
/// session home folder and none of it.
#[test]
fn an_unconfined_command_runs_as_the_operator() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let outside = outside_targets();
    let probe = operator_probe(&outside.secret, &outside.rw, port);
    let read_md =
        "while IFS= read -r l; do echo \"md:$l\"; done < \"$MURMUR_RUNTIME_DIR/MURMUR.md\"";

    let server = common::ScriptedServer::start(vec![
        bash("toolu_probe", &probe),
        bash("toolu_md", read_md),
        end_turn("Done."),
    ]);
    let fixture = Fixture::new();
    fixture.manifest(&server.endpoint, "", SHELL_ALLOW_BASH, "");
    let output = fixture.run(&["--unconfined"], UNCONFINED_ENV);
    let requests = server.requests();
    let text = common::tool_result_text(&requests, "toolu_probe").unwrap();
    let seen = lines(&text);
    let home = fixture.home.path().display().to_string();
    for expected in [
        format!("home={home}"),
        "token=unconfined-sentinel".to_string(),
        "var=present".to_string(),
        format!("nnp={}", own_status("NoNewPrivs")),
        "secret=outside-secret".to_string(),
        "write=ok".to_string(),
        format!("nofile={}", own_nofile()),
        "tcp=ok".to_string(),
    ] {
        assert!(
            seen.contains(&expected.as_str()),
            "{expected} not in {text}"
        );
    }
    assert!(outside.rw.join("new").exists());
    assert!(listener.accept().is_ok(), "the connection was accepted");

    // MURMUR.md says who commands run as and prescribes nothing.
    let md = common::tool_result_text(&requests, "toolu_md").unwrap();
    assert!(
        md.contains(&format!(
            "md:Commands run unconfined, as the operator's own account on this host: `$HOME` is \
             {home}, the environment is the operator's, and no containment applies"
        )),
        "{md}"
    );

    // One W-SEC-036 naming what the capsule reaches, and no tier warning.
    let err = stderr(&output);
    assert_eq!(
        count(&err, "W-SEC-036"),
        1,
        "exactly one W-SEC-036 line: {err}"
    );
    let warning = err
        .lines()
        .find(|line| line.contains("warning[W-SEC-036]"))
        .unwrap();
    for needle in [
        &home,
        "environment",
        "filesystem",
        "network",
        "sudo",
        "#w-sec-036",
    ] {
        assert!(warning.contains(needle), "{needle} not in {warning}");
    }
    for code in ["W-SEC-001", "W-SEC-002", "W-SEC-005", "W-SEC-010"] {
        assert!(!err.contains(code), "{code} in {err}");
    }

    // The trace says unconfined, and its grants are `--explain-scope --json`'s.
    let trace = fixture.trace();
    let start = first(&trace, "session_start");
    assert_eq!(start["containment_declared"], "unconfined", "{start}");
    assert_eq!(start["containment_achieved"], "unconfined", "{start}");
    let grants = &start["effective_grants"];
    assert_eq!(grants["enforcement_tier"], "unconfined");
    assert_eq!(grants["achieved_containment"], "unconfined");
    assert_eq!(grants["filesystem_boundary"]["restriction"], "none");
    assert_eq!(grants["unconfined"]["home"], home.as_str());
    assert_eq!(grants["io_max"]["status"], "not-required");
    assert_eq!(*grants, fixture.explain(&["--unconfined"]));
    assert!(fixture.explain(&[])["unconfined"].is_null());
    first(&trace, "tool_call");
    first(&trace, "session_end");

    // `mur trace show` names the stance with the user and HOME.
    let records = common::newest_session_records(
        fixture.home.path(),
        &fixture.project.path().join("workdir"),
    )
    .unwrap();
    let shown = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", fixture.home.path())
        .args([
            "trace",
            "show",
            records.join("trace.jsonl").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let shown = String::from_utf8_lossy(&shown.stdout);
    let line = shown
        .lines()
        .find(|line| line.starts_with("containment:"))
        .unwrap_or_else(|| panic!("no containment line in {shown}"));
    assert!(line.contains("unconfined → unconfined"), "{line}");
    assert!(line.contains(&format!("with HOME {home}")), "{line}");

    // With --json too, exactly one W-SEC-036.
    let server = common::ScriptedServer::start(vec![end_turn("Done.")]);
    fixture.manifest(&server.endpoint, "", SHELL_ALLOW_BASH, "");
    let json_output = fixture.run(&["--unconfined", "--json"], &[]);
    assert_eq!(
        stderr(&json_output)
            .lines()
            .filter(|line| line.contains("warning[W-SEC-036]"))
            .count(),
        1
    );
    let explained = fixture.mur(&["--unconfined", "--explain-scope"], &[]);
    assert!(!stderr(&explained).contains("W-SEC-036"));
    assert!(!String::from_utf8_lossy(&explained.stdout).contains("W-SEC-036"));

    // The contained half: the session home folder, no credential, no_new_privs, and nothing
    // outside the grants.
    if capsule_runtime::skip_without_egress_namespace("an_unconfined_command_runs_as_the_operator")
    {
        return;
    }
    if fixture.explain(&["--containment", "scoped"])["floor_met"] != true {
        eprintln!("[SKIP] the contained half: this host cannot back scoped");
        return;
    }
    let contained_outside = outside_targets();
    let server = common::ScriptedServer::start(vec![
        bash(
            "toolu_probe",
            &operator_probe(&contained_outside.secret, &contained_outside.rw, port),
        ),
        end_turn("Done."),
    ]);
    fixture.manifest(&server.endpoint, "", SHELL_ALLOW_BASH, "");
    let contained = fixture.run(&["--containment", "scoped"], UNCONFINED_ENV);
    assert!(!stderr(&contained).contains("W-SEC-036"));
    let text = common::tool_result_text(&server.requests(), "toolu_probe").unwrap();
    let seen = lines(&text);
    for expected in [
        "token=",
        "var=",
        "secret=refused",
        "write=refused",
        "tcp=refused",
    ] {
        assert!(seen.contains(&expected), "{expected} not in {text}");
    }
    // Under `scoped` the command reads its own `/proc/self/status` through the session's private
    // `/proc`, and its `NoNewPrivs` is set.
    assert!(seen.contains(&"nnp=1"), "{text}");
    let home_line = seen.iter().find(|line| line.starts_with("home=")).unwrap();
    assert_ne!(*home_line, format!("home={home}"), "{text}");
    assert!(home_line.ends_with("/home"), "{text}");
    assert!(!contained_outside.rw.join("new").exists());
}

/// `containment: unconfined` in the global config chooses it; `--containment` overrides it
/// for one run.
#[test]
fn the_global_config_chooses_unconfined_and_containment_overrides_it() {
    let probe = "echo \"home=$HOME\"";
    let server = common::ScriptedServer::start(vec![bash("toolu_home", probe), end_turn("Done.")]);
    let fixture = Fixture::new();
    fixture.global_config("containment: unconfined\n");
    fixture.manifest(&server.endpoint, "", SHELL_ALLOW_BASH, "");
    let output = fixture.run(&[], &[]);
    let home = fixture.home.path().display().to_string();
    let text = common::tool_result_text(&server.requests(), "toolu_home").unwrap();
    assert!(
        lines(&text).contains(&format!("home={home}").as_str()),
        "{text}"
    );
    assert_eq!(
        first(&fixture.trace(), "session_start")["containment_achieved"],
        "unconfined"
    );
    assert!(stderr(&output).contains("W-SEC-036"));

    if capsule_runtime::skip_without_egress_namespace(
        "the_global_config_chooses_unconfined_and_containment_overrides_it",
    ) {
        return;
    }
    let server = common::ScriptedServer::start(vec![bash("toolu_home", probe), end_turn("Done.")]);
    fixture.manifest(&server.endpoint, "", SHELL_ALLOW_BASH, "");
    let output = fixture.run(&["--containment", "advisory"], &[]);
    assert!(!stderr(&output).contains("W-SEC-036"));
    let text = common::tool_result_text(&server.requests(), "toolu_home").unwrap();
    assert!(
        !lines(&text).contains(&format!("home={home}").as_str()),
        "{text}"
    );
    let start = first(&fixture.trace(), "session_start");
    assert_eq!(start["containment_declared"], "advisory");
    assert_ne!(start["containment_achieved"], "unconfined");
}

/// `mur run` works inside an unconfined capsule, and the session it starts is a fresh
/// top-level one: no spawner, no formation, and contained.
#[test]
fn mur_run_works_inside_an_unconfined_capsule() {
    let inner_server = common::ScriptedServer::start(vec![end_turn("inner-done")]);
    let fixture = Fixture::new();
    let inner = fixture.project.path().join("inner");
    fs::create_dir(&inner).unwrap();
    write_agent_manifest(&inner, &inner_server.endpoint, "", "", "");

    let command = format!(
        "\"$MUR_BIN\" run --manifest {} --task inner; echo \"inner-exit=$?\"",
        inner.join("murmur.yaml").display()
    );
    let server =
        common::ScriptedServer::start(vec![bash("toolu_inner", &command), end_turn("Done.")]);
    fixture.manifest(&server.endpoint, "", SHELL_ALLOW_BASH, "");
    let mur_bin = assert_cmd::cargo::cargo_bin("mur");
    fixture.run(&["--unconfined"], &[("MUR_BIN", mur_bin.to_str().unwrap())]);

    let text = common::tool_result_text(&server.requests(), "toolu_inner").unwrap();
    // The inner run's own output: it reached its model and succeeded.
    assert!(lines(&text).contains(&"status:  ok"), "{text}");
    assert!(lines(&text).contains(&"inner-exit=0"), "{text}");
    assert_eq!(
        inner_server.requests().len(),
        1,
        "the inner session asked its model"
    );

    let inner_records = common::newest_session_records(fixture.home.path(), &inner.join("workdir"))
        .expect("the inner session's records are under the scratch HOME");
    let trace = common::read_whole_trace(&inner_records.join("trace.jsonl"));
    let start = first(&trace, "session_start");
    assert!(
        start.get("spawned_by").is_none_or(Value::is_null),
        "{start}"
    );
    assert!(
        start.get("formation_id").is_none_or(Value::is_null),
        "{start}"
    );
    assert_ne!(start["containment_achieved"], "unconfined", "{start}");
}

/// An unconfined run still ends at its turn limit, and its trace is written.
#[test]
fn an_unconfined_run_still_ends_at_the_turn_limit() {
    let server = common::ScriptedServer::start(vec![
        bash("toolu_1", "echo one"),
        bash("toolu_2", "echo two"),
        bash("toolu_3", "echo three"),
        end_turn("Done."),
    ]);
    let fixture = Fixture::new();
    fixture.manifest(&server.endpoint, "", SHELL_ALLOW_BASH, "  max_turns: 2\n");
    let output = fixture.mur(&["--unconfined", "--task", "go"], &[]);
    let trace = fixture.trace();
    let end = first(&trace, "session_end");
    assert_eq!(
        end["exit_status"],
        "max_turns_reached",
        "{end}\n{}",
        stderr(&output)
    );
    assert!(!output.status.success());
    // The model was asked twice, so its third call never ran.
    assert_eq!(server.requests().len(), 2);
    assert!(common::find_tool_result(&server.requests(), "toolu_3").is_none());
    assert_eq!(
        first(&trace, "session_start")["containment_achieved"],
        "unconfined"
    );
    first(&trace, "tool_call");
    let fenced = common::find_tool_result(&server.requests(), "toolu_1")
        .map(|block| common::extract_result_text(&block))
        .unwrap();
    assert!(
        fenced.starts_with("<untrusted-content source=tool:"),
        "a permitted result is fenced: {fenced}"
    );
}

/// A declared `max_open_files` is the one limit an unconfined command gets.
#[test]
fn an_unconfined_command_gets_only_the_declared_limits() {
    let server = common::ScriptedServer::start(vec![
        bash(
            "toolu_limits",
            "echo \"nofile=$(ulimit -Sn) $(ulimit -Hn)\"",
        ),
        end_turn("Done."),
    ]);
    let fixture = Fixture::new();
    fixture.manifest(
        &server.endpoint,
        "",
        &format!("{SHELL_ALLOW_BASH}  resources:\n    max_open_files: 256\n"),
        "",
    );
    fixture.run(&["--unconfined"], &[]);
    let text = common::tool_result_text(&server.requests(), "toolu_limits").unwrap();
    assert!(lines(&text).contains(&"nofile=256 256"), "{text}");
    let grants = &first(&fixture.trace(), "session_start")["effective_grants"];
    assert_eq!(
        grants["unconfined"]["resource_limits"],
        json!(["max_open_files"])
    );
}

/// `mur ps` names an unconfined capsule waiting at its door, below the table, and `--json`
/// says `"unconfined": true` for it; its running record carries the key.
#[test]
fn mur_ps_names_an_unconfined_capsule() {
    let fixture = Fixture::new();
    fixture.manifest("http://127.0.0.1:9", "", SHELL_ALLOW_BASH, "");
    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("mur"))
        .env("HOME", fixture.home.path())
        .current_dir(fixture.project.path())
        .args([
            "run",
            "--manifest",
            "murmur.yaml",
            "--unconfined",
            "--lifecycle-task-acceptance",
            "queue",
            "--lifecycle-after-task",
            "sleep",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let running = fixture.home.path().join(".murmur").join("running");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    let record = loop {
        let found = fs::read_dir(&running).ok().and_then(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .find(|path| path.extension().is_some_and(|ext| ext == "json"))
        });
        if let Some(path) = found {
            if let Ok(text) = fs::read_to_string(&path) {
                if let Ok(value) = serde_json::from_str::<Value>(&text) {
                    break value;
                }
            }
        }
        if std::time::Instant::now() > deadline || child.try_wait().unwrap().is_some() {
            let _ = child.kill();
            let mut err = String::new();
            child
                .stderr
                .take()
                .unwrap()
                .read_to_string(&mut err)
                .unwrap();
            panic!("no running record appeared: {err}");
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    };
    assert_eq!(record["unconfined"], true, "{record}");
    let session = record["session_id"].as_str().unwrap().to_string();

    let ps = |args: &[&str]| {
        Command::cargo_bin("mur")
            .unwrap()
            .env("HOME", fixture.home.path())
            .arg("ps")
            .args(args)
            .output()
            .unwrap()
    };
    let table = String::from_utf8_lossy(&ps(&[]).stdout).into_owned();
    assert!(
        table
            .lines()
            .any(|line| line.starts_with(&format!("{session} runs unconfined"))),
        "{table}"
    );
    let listing: Value = serde_json::from_slice(&ps(&["--json"]).stdout).unwrap();
    let row = listing["capsules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["session_id"] == session.as_str())
        .unwrap()
        .clone();
    assert_eq!(row["unconfined"], true, "{row}");

    let _ = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", fixture.home.path())
        .args(["stop", &session])
        .output();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while child.try_wait().unwrap().is_none() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// `mur doctor` with the global config's `unconfined` states the stance and where it came
/// from; with a manifest floor too, it fails with the `E-RUN-059` refusal `mur run` will make.
#[test]
fn mur_doctor_states_the_unconfined_stance() {
    let fixture = Fixture::new();
    fixture.global_config("containment: unconfined\n");
    fixture.manifest("http://127.0.0.1:9", "", "", "");
    let doctor = || {
        Command::cargo_bin("mur")
            .unwrap()
            .env("HOME", fixture.home.path())
            .current_dir(fixture.project.path())
            .arg("doctor")
            .output()
            .unwrap()
    };
    let output = doctor();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout
        .lines()
        .find(|line| line.starts_with("Containment stance: unconfined"))
        .unwrap_or_else(|| panic!("no stance line in {stdout}{}", stderr(&output)));
    assert!(
        line.contains("containment: unconfined in ~/.murmur/config.yaml"),
        "{line}"
    );
    assert!(line.contains("sudo"), "{line}");

    fixture.manifest("http://127.0.0.1:9", "", "  containment: scoped\n", "");
    let output = doctor();
    let err = stderr(&output);
    let out = String::from_utf8_lossy(&output.stdout);
    assert!(err.contains("[mur doctor] error[E-RUN-059]:"), "{err}");
    assert!(!err.contains("warning[E-RUN-059]"), "{err}");
    assert!(err.contains("`mur run` will refuse this capsule"), "{err}");
    assert_eq!(output.status.code(), Some(1), "{out}{err}");
    assert!(out.contains("\nFirst refusal: error[E-RUN-059]: "), "{out}");
}

/// A WASM tool's environment, network allowlist and state stores are the same with and without
/// `--unconfined`, and so are its preopens but for one key: an unscoped WASM tool's `extra_roots`
/// gain the host's `/` at `murmur-host`, while a tool narrowed to a subtree keeps exactly it.
#[test]
fn wasm_tool_grants_differ_only_in_extra_roots_when_unconfined() {
    let fixture = Fixture::new();
    fixture.manifest(
        "http://127.0.0.1:9",
        "  - name: local/scoped-tool\n\
         \x20   version: 0.1.0\n\
         \x20   runtime: tool\n\
         \x20   capabilities:\n\
         \x20     filesystem:\n\
         \x20       scope: data\n\
         \x20     state: {}\n\
         \x20 - name: local/whole-tool\n\
         \x20   version: 0.1.0\n\
         \x20   runtime: tool\n",
        "  env:\n    allow:\n      - MURMUR_TEST_UNCONFINED\n",
        "",
    );
    let contained = fixture.explain(&[]);
    let unconfined = fixture.explain(&["--unconfined"]);
    for key in ["env_allow", "network_allow", "state_stores"] {
        assert_eq!(contained[key], unconfined[key], "{key}");
    }
    let without_roots = |report: &Value| {
        let mut preopens = report["preopens"].clone();
        for preopen in preopens.as_array_mut().unwrap() {
            preopen.as_object_mut().unwrap().remove("extra_roots");
        }
        preopens
    };
    assert_eq!(without_roots(&contained), without_roots(&unconfined));
    let roots_of = |report: &Value, artifact: &str| {
        report["preopens"]
            .as_array()
            .unwrap()
            .iter()
            .find(|preopen| preopen["artifact"] == artifact)
            .unwrap_or_else(|| panic!("no {artifact} in {report}"))["extra_roots"]
            .clone()
    };
    for report in [&contained, &unconfined] {
        assert_eq!(roots_of(report, "local/scoped-tool"), json!([]), "{report}");
    }
    assert_eq!(roots_of(&contained, "local/whole-tool"), json!([]));
    assert_eq!(
        roots_of(&unconfined, "local/whole-tool"),
        json!([{
            "guest_path": "murmur-host",
            "path": "/",
            "access": "read-write",
            "source": "unconfined",
        }])
    );
    assert!(
        contained["preopens"]
            .as_array()
            .unwrap()
            .iter()
            .any(|preopen| preopen.to_string().contains("data")),
        "{contained}"
    );
}
