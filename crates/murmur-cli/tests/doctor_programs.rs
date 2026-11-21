//! `mur doctor --programs`: each program started inside the sandbox the project's capsule would get
//! on this host, through the real `mur` binary, with a scratch `HOME` and fixture project.
//!
//! Every test spawns probes, so each stands down with `[SKIP-HOST]` where this host cannot create
//! the capsule's network namespace, and where a host fact it reads (`/usr/bin/curl`,
//! a rustup `cargo`) is missing. The scratch `HOME` and the project lie outside
//! `/tmp`: the `scoped` tier maps a private `/tmp` over the host's, which would hide a probe's work
//! folder under it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use assert_cmd::Command;
use serde_json::Value;
use tempfile::TempDir;

/// The common list `mur doctor --programs` probes with no names, in order.
const COMMON: &[&str] = &[
    "sh", "bash", "git", "curl", "wget", "python3", "node", "npm", "make", "cc", "cargo", "rustc",
    "go", "jq", "rg", "unshare", "sudo",
];

/// A scratch `HOME` and a fixture project inside it, both outside `/tmp`.
struct Fixture {
    home: TempDir,
    project: PathBuf,
    /// The operator's own `HOME`, where the host's `~/.cargo` and `~/.rustup` are.
    real_home: PathBuf,
}

impl Fixture {
    /// A project whose `capabilities` block is `capabilities`, with `config` as the scratch
    /// `~/.murmur/config.yaml` when given.
    fn new(capabilities: &str, config: Option<&str>) -> Self {
        let real_home = PathBuf::from(std::env::var_os("HOME").expect("HOME is set"));
        let home = tempfile::Builder::new()
            .prefix(".doctor-programs-")
            .tempdir_in(&real_home)
            .unwrap();
        let project = home.path().join("project");
        fs::create_dir_all(&project).unwrap();
        fs::write(
            project.join("murmur.yaml"),
            format!(
                "name: probe-fixture\nversion: 0.1.0\nartifacts: []\ncapabilities:\n{capabilities}"
            ),
        )
        .unwrap();
        if let Some(config) = config {
            fs::create_dir_all(home.path().join(".murmur")).unwrap();
            fs::write(home.path().join(".murmur").join("config.yaml"), config).unwrap();
        }
        Self {
            home,
            project,
            real_home,
        }
    }

    fn rustup_home(&self) -> String {
        self.real_home.join(".rustup").display().to_string()
    }

    fn cargo_home(&self) -> String {
        self.real_home.join(".cargo").display().to_string()
    }

    /// `PATH=/usr/local/bin:/usr/bin:/bin:<real home>/.cargo/bin`, with `RUSTUP_HOME` and
    /// `CARGO_HOME` naming the operator's own.
    fn board_env(&self) -> Vec<(String, String)> {
        vec![
            (
                "PATH".to_string(),
                format!(
                    "/usr/local/bin:/usr/bin:/bin:{}",
                    self.real_home.join(".cargo/bin").display()
                ),
            ),
            ("RUSTUP_HOME".to_string(), self.rustup_home()),
            ("CARGO_HOME".to_string(), self.cargo_home()),
        ]
    }

    /// `mur doctor <args>` from the project, under the scratch `HOME`, with `env`.
    fn doctor(&self, args: &[&str], env: &[(String, String)]) -> Output {
        let mut command = Command::cargo_bin("mur").unwrap();
        command
            .env("HOME", self.home.path())
            .env_remove("MURMUR_ROOST_URL")
            .env_remove("RUSTUP_HOME")
            .env_remove("CARGO_HOME")
            .current_dir(&self.project)
            .timeout(Duration::from_secs(600))
            .arg("doctor")
            .args(args);
        for (name, value) in env {
            command.env(name, value);
        }
        command.output().unwrap()
    }

    fn murmur(&self, sub: &str) -> PathBuf {
        self.home.path().join(".murmur").join(sub)
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The one JSON document stdout holds, and that the process exited with its `exit_code`.
fn document(output: &Output) -> Value {
    let text = stdout(output);
    let value: Value = serde_json::from_str(&text).unwrap_or_else(|error| {
        panic!(
            "stdout is not one JSON document ({error}):\n{text}\nstderr:\n{}",
            stderr(output)
        )
    });
    assert_eq!(
        Some(i64::from(output.status.code().unwrap_or(-1))),
        value["exit_code"].as_i64(),
        "{value:#}"
    );
    value
}

fn probed(report: &Value) -> &Vec<Value> {
    report["programs"]["probed"]
        .as_array()
        .unwrap_or_else(|| panic!("no programs.probed: {report:#}"))
}

fn program<'a>(report: &'a Value, name: &str) -> &'a Value {
    probed(report)
        .iter()
        .find(|probe| probe["program"] == name)
        .unwrap_or_else(|| panic!("{name} was not probed: {report:#}"))
}

fn w_run_015(report: &Value) -> Vec<&Value> {
    report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|finding| finding["code"] == "W-RUN-015")
        .collect()
}

fn skip_without_namespace(test: &str) -> bool {
    capsule_runtime::network_namespace::skip_without_egress_namespace(test)
}

fn skip_host(test: &str, reason: &str) {
    eprintln!("[SKIP-HOST] {test}: {reason}");
}

/// Whether `~/.cargo/bin/cargo` of the operator is a rustup proxy.
fn rustup_cargo(real_home: &Path) -> bool {
    let cargo = real_home.join(".cargo/bin/cargo");
    fs::canonicalize(&cargo)
        .ok()
        .is_some_and(|target| target.file_name().is_some_and(|name| name == "rustup"))
}

/// `curl` not granted, `unshare` refused by seccomp and a rustup `cargo` without its homes each
/// come out with their cause and grant, beside two granted programs that run.
#[test]
fn the_board_cases_come_out_with_their_causes_and_grants() {
    const TEST: &str = "the_board_cases_come_out_with_their_causes_and_grants";
    if skip_without_namespace(TEST) {
        return;
    }
    let fixture = Fixture::new("  shell:\n    allow: [bash, git]\n", None);
    if !rustup_cargo(&fixture.real_home) || !Path::new("/usr/bin/curl").exists() {
        return skip_host(TEST, "needs /usr/bin/curl and a rustup ~/.cargo/bin/cargo");
    }
    let output = fixture.doctor(
        &["--programs", "bash,git,curl,unshare,cargo"],
        &fixture.board_env(),
    );
    let text = stdout(&output);
    assert_eq!(output.status.code(), Some(0), "{text}\n{}", stderr(&output));
    assert!(
        text.contains("Programs (probed inside the sandbox: scoped, landlock+seccomp)"),
        "{text}"
    );
    let at = |needle: &str| {
        text.find(needle)
            .unwrap_or_else(|| panic!("{needle:?} missing:\n{text}"))
    };
    let bash = at("\u{2713}  bash      runs \u{00B7} /");
    let git = at("\u{2713}  git       runs \u{00B7} /");
    let curl = at("\u{2717}  curl      refused (not-in-shell-allow) \u{00B7} /usr/bin/curl");
    let unshare = at("\u{2717}  unshare   refused (namespaces-refused)");
    let cargo = at("\u{2717}  cargo     refused (host-path-needed)");
    assert!(
        bash < git && git < curl && curl < unshare && unshare < cargo,
        "{text}"
    );
    assert!(
        text.contains("Grant: add curl to capabilities.shell.allow\n"),
        "{text}"
    );
    assert!(text.contains("seccomp filter"), "{text}");
    assert!(text.contains("mur run --unconfined"), "{text}");
    assert!(text.contains("containment: unconfined"), "{text}");
    let rustup = fixture.rustup_home();
    assert!(text.contains(&format!("RUSTUP_HOME ({rustup})")), "{text}");
    assert!(text.contains("CARGO_HOME ("), "{text}");
    assert!(
        text.contains(&format!("{{path: {rustup}, access: read}}")),
        "{text}"
    );
    assert!(
        text.contains("RUSTUP_HOME=") && text.contains("CARGO_HOME="),
        "{text}"
    );
    assert!(
        text.contains("and add cargo to capabilities.shell.allow"),
        "{text}"
    );
    assert!(!text.contains("W-RUN-015"), "{text}");
}

/// The same setup with `--json` carries the same results as a `programs` object after
/// `findings`, keys in order; without `--programs` there is no `programs` key.
#[test]
fn the_json_document_carries_the_programs_object() {
    const TEST: &str = "the_json_document_carries_the_programs_object";
    if skip_without_namespace(TEST) {
        return;
    }
    let fixture = Fixture::new("  shell:\n    allow: [bash, git]\n", None);
    if !rustup_cargo(&fixture.real_home) || !Path::new("/usr/bin/curl").exists() {
        return skip_host(TEST, "needs /usr/bin/curl and a rustup ~/.cargo/bin/cargo");
    }
    let plain = fixture.doctor(&["--json"], &fixture.board_env());
    let plain_report = document(&plain);
    assert!(plain_report.get("programs").is_none(), "{plain_report:#}");

    let output = fixture.doctor(
        &["--json", "--programs", "bash,git,curl,unshare,cargo"],
        &fixture.board_env(),
    );
    let report = document(&output);
    let text = stdout(&output);
    assert!(text.find("\"findings\"").unwrap() < text.find("\"programs\"").unwrap());
    assert!(
        stderr(&output).contains("Programs (probed inside the sandbox"),
        "the text report goes to stderr"
    );
    for key in [
        "verdict",
        "exit_code",
        "checks_passed",
        "errors",
        "warnings",
        "findings",
    ] {
        assert_eq!(report[key], plain_report[key], "{key}: {report:#}");
    }
    assert_eq!(report["programs"]["containment"], "scoped");
    assert_eq!(report["programs"]["mechanism"], "landlock+seccomp");
    assert!(report["programs"]["path"]
        .as_str()
        .unwrap()
        .starts_with("/usr/local/bin:/usr/bin:/bin:"));
    let names: Vec<&str> = probed(&report)
        .iter()
        .map(|probe| probe["program"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["bash", "git", "curl", "unshare", "cargo"]);

    // Each element's keys in order.
    let keys = [
        "program",
        "status",
        "in_shell_allow",
        "resolved",
        "probe",
        "exit_code",
        "cause",
        "reason",
        "grant",
        "note",
        "detail",
    ];
    let element = &text[text.find("\"probed\"").unwrap()..];
    let element = &element[..element.find('}').unwrap()];
    let positions: Vec<usize> = keys
        .iter()
        .map(|key| element.find(&format!("\"{key}\":")).unwrap())
        .collect();
    assert!(
        positions.windows(2).all(|pair| pair[0] < pair[1]),
        "{element}"
    );

    for name in ["bash", "git"] {
        let probe = program(&report, name);
        assert_eq!(probe["status"], "runs", "{probe:#}");
        assert_eq!(probe["in_shell_allow"], true);
        assert!(probe["cause"].is_null() && probe["reason"].is_null() && probe["grant"].is_null());
        assert_eq!(probe["exit_code"], 0);
    }
    let curl = program(&report, "curl");
    assert_eq!(curl["cause"], "not-in-shell-allow", "{curl:#}");
    assert_eq!(curl["in_shell_allow"], false);
    assert_eq!(curl["resolved"], "/usr/bin/curl");
    assert_eq!(curl["grant"], "add curl to capabilities.shell.allow");
    assert_eq!(
        curl["note"],
        "reaches only hosts in capabilities.network.allow (none declared)"
    );
    let unshare = program(&report, "unshare");
    assert_eq!(unshare["cause"], "namespaces-refused", "{unshare:#}");
    assert_eq!(
        unshare["probe"],
        serde_json::json!(["unshare", "--user", "--map-root-user", "true"])
    );
    let cargo = program(&report, "cargo");
    assert_eq!(cargo["cause"], "host-path-needed", "{cargo:#}");
    assert_eq!(cargo["probe"], serde_json::json!(["cargo", "--version"]));
    assert!(w_run_015(&report).is_empty(), "{report:#}");
}

/// `curl` granted but resolving to the snap launcher is refused naming the copy that would
/// run; with `/usr/bin` first it runs.
#[test]
fn curl_as_a_snap_wrapper_is_refused_naming_the_real_copy() {
    const TEST: &str = "curl_as_a_snap_wrapper_is_refused_naming_the_real_copy";
    if skip_without_namespace(TEST) {
        return;
    }
    if !Path::new("/usr/bin/curl").exists() {
        return skip_host(TEST, "needs /usr/bin/curl");
    }
    let fixture = Fixture::new("  shell:\n    allow: [curl]\n", None);
    // A snap-style bin folder of the fixture's own: `curl` is a link to `snap`, which is a
    // link to a program that starts nothing, as on a host that installs curl as a snap.
    let snap_bin = fixture.home.path().join("snap-bin");
    fs::create_dir_all(&snap_bin).unwrap();
    std::os::unix::fs::symlink("/bin/false", snap_bin.join("snap")).unwrap();
    std::os::unix::fs::symlink("snap", snap_bin.join("curl")).unwrap();
    let snap_bin = snap_bin.display().to_string();
    let path = |value: &str| vec![("PATH".to_string(), value.to_string())];
    let output = fixture.doctor(
        &["--json", "--programs", "curl"],
        &path(&format!("{snap_bin}:/usr/local/bin:/usr/bin:/bin")),
    );
    let report = document(&output);
    assert_eq!(report["exit_code"], 0, "{report:#}");
    let curl = program(&report, "curl");
    assert_eq!(curl["status"], "refused", "{curl:#}");
    assert_eq!(curl["cause"], "snap-wrapper", "{curl:#}");
    assert_eq!(curl["resolved"], format!("{snap_bin}/curl"));
    assert!(
        curl["reason"]
            .as_str()
            .unwrap()
            .contains(&format!("{snap_bin}/snap")),
        "{curl:#}"
    );
    let grant = curl["grant"].as_str().unwrap();
    assert!(
        grant.contains(&format!("put /usr/bin ahead of {snap_bin}")),
        "{grant}"
    );
    assert!(grant.contains("/usr/bin/curl"), "{grant}");
    let warnings = w_run_015(&report);
    assert_eq!(warnings.len(), 1, "{report:#}");
    assert_eq!(warnings[0]["subject"], "curl");

    let output = fixture.doctor(
        &["--json", "--programs", "curl"],
        &path(&format!("/usr/local/bin:/usr/bin:/bin:{snap_bin}")),
    );
    let report = document(&output);
    let curl = program(&report, "curl");
    assert_eq!(curl["status"], "runs", "{curl:#}");
    assert_eq!(curl["resolved"], "/usr/bin/curl");
    assert!(w_run_015(&report).is_empty(), "{report:#}");
}

/// Every entry beneath `dir`, relative, sorted.
fn tree(dir: &Path) -> Vec<String> {
    let mut entries = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        let Ok(read) = fs::read_dir(&next) else {
            continue;
        };
        for entry in read.flatten() {
            let path = entry.path();
            entries.push(path.strip_prefix(dir).unwrap().display().to_string());
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                stack.push(path);
            }
        }
    }
    entries.sort();
    entries
}

/// Every live process whose working directory lies under `dir`.
fn processes_under(dir: &Path) -> Vec<String> {
    let mut found = Vec::new();
    for entry in fs::read_dir("/proc").into_iter().flatten().flatten() {
        if let Ok(cwd) = fs::read_link(entry.path().join("cwd")) {
            if cwd.starts_with(dir) {
                found.push(format!(
                    "{} {}",
                    entry.file_name().to_string_lossy(),
                    cwd.display()
                ));
            }
        }
    }
    found
}

/// Every cgroup directory beneath the v2 hierarchy whose name contains `needle`.
fn cgroups_named(needle: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![PathBuf::from("/sys/fs/cgroup")];
    while let Some(dir) = stack.pop() {
        let Ok(read) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in read.flatten() {
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let path = entry.path();
            if path.to_string_lossy().contains(needle) {
                found.push(path.clone());
            }
            stack.push(path);
        }
    }
    found
}

/// `cargo` with its homes lent is no longer refused for them, and names what it needs
/// next; nothing is left behind in the project, the scratch folders, the process table or the
/// cgroup tree.
#[test]
fn cargo_with_its_homes_lent_gets_past_them_and_leaves_nothing_behind() {
    const TEST: &str = "cargo_with_its_homes_lent_gets_past_them_and_leaves_nothing_behind";
    if skip_without_namespace(TEST) {
        return;
    }
    let real_home = PathBuf::from(std::env::var_os("HOME").unwrap());
    if !rustup_cargo(&real_home) {
        return skip_host(TEST, "needs a rustup ~/.cargo/bin/cargo");
    }
    let config = format!(
        "host_paths:\n  - path: {}\n    access: read\n  - path: {}\n    access: read\n",
        real_home.join(".rustup").display(),
        real_home.join(".cargo").display()
    );
    let fixture = Fixture::new("  shell:\n    allow: [cargo]\n", Some(&config));
    fs::create_dir_all(fixture.project.join("workdir")).unwrap();
    fs::write(fixture.project.join("workdir").join("kept.txt"), "kept\n").unwrap();
    let project_before = tree(&fixture.project);
    let sessions_before = tree(&fixture.murmur("sessions"));

    let mut command = std::process::Command::new(assert_cmd::cargo::cargo_bin("mur"));
    command
        .env("HOME", fixture.home.path())
        .env_remove("MURMUR_ROOST_URL")
        .current_dir(&fixture.project)
        .args(["doctor", "--json", "--programs", "cargo"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for (name, value) in fixture.board_env() {
        command.env(name, value);
    }
    let child = command.spawn().unwrap();
    let pid = child.id();
    let output = child.wait_with_output().unwrap();
    let report = document(&output);

    // With both homes lent the proxy reads them; what it reaches next depends on the host: the
    // toolchain it runs lies in a host path, never executable, unless the build step is reached.
    let cargo = program(&report, "cargo");
    assert_ne!(cargo["cause"], "host-path-needed", "{cargo:#}");
    if cargo["probe"][1] == "--version" {
        assert_eq!(cargo["cause"], "interpreter-runtime-needed", "{cargo:#}");
        let grant = cargo["grant"].as_str().unwrap();
        assert!(
            grant.starts_with(
                "declare capabilities.shell.interpreter_runtime: [{binary: cargo, dirs: [{path: "
            ) && grant.contains(&format!("{}/toolchains/", fixture.rustup_home())),
            "{cargo:#}"
        );
        assert_eq!(w_run_015(&report).len(), 1, "{report:#}");
    } else {
        assert_build_links_or_names_cc(&report);
    }
    assert_eq!(
        tree(&fixture.project),
        project_before,
        "the project changed"
    );
    assert_eq!(tree(&fixture.murmur("sessions")), sessions_before);
    assert_eq!(tree(&fixture.murmur("probes")), Vec::<String>::new());
    assert_eq!(processes_under(fixture.home.path()), Vec::<String>::new());
    assert_eq!(
        cgroups_named(&format!("probe-{pid}-")),
        Vec::<PathBuf>::new()
    );
}

/// The `cargo` probe reached its hello build, which links (`runs`) or is refused naming the
/// `cc` linker with the build's stderr tail.
fn assert_build_links_or_names_cc(report: &Value) {
    let cargo = program(report, "cargo");
    assert_eq!(cargo["probe"][1], "build", "{cargo:#}");
    match cargo["status"].as_str() {
        Some("runs") => assert!(w_run_015(report).is_empty(), "{report:#}"),
        _ => {
            assert_eq!(cargo["cause"], "helper-not-allowed", "{cargo:#}");
            assert!(
                cargo["grant"]
                    .as_str()
                    .unwrap()
                    .starts_with("add cc to capabilities.shell.allow"),
                "{cargo:#}"
            );
            assert!(!cargo["detail"].as_str().unwrap().is_empty(), "{cargo:#}");
            assert_eq!(w_run_015(report).len(), 1, "{report:#}");
        }
    }
}

/// With the toolchain folder and the host library folder granted through
/// `interpreter_runtime`, the hello build is reached, and links or names the `cc` linker.
#[test]
fn cargo_with_its_toolchain_granted_reaches_the_link() {
    const TEST: &str = "cargo_with_its_toolchain_granted_reaches_the_link";
    if skip_without_namespace(TEST) {
        return;
    }
    let real_home = PathBuf::from(std::env::var_os("HOME").unwrap());
    let settings = fs::read_to_string(real_home.join(".rustup/settings.toml")).unwrap_or_default();
    let toolchain = settings.lines().find_map(|line| {
        let value = line.trim().strip_prefix("default_toolchain")?.trim_start();
        Some(
            value
                .strip_prefix('=')?
                .trim()
                .trim_matches('"')
                .to_string(),
        )
    });
    let libraries = Path::new("/usr/lib/x86_64-linux-gnu");
    let (true, Some(toolchain), true) = (rustup_cargo(&real_home), toolchain, libraries.exists())
    else {
        return skip_host(
            TEST,
            "needs a rustup ~/.cargo/bin/cargo with a default toolchain and /usr/lib/x86_64-linux-gnu",
        );
    };
    let config = format!(
        "host_paths:\n  - path: {}\n    access: read\n  - path: {}\n    access: read\n",
        real_home.join(".rustup").display(),
        real_home.join(".cargo").display()
    );
    let capabilities = format!(
        "  shell:\n    allow: [cargo]\n    interpreter_runtime:\n      - binary: cargo\n        \
         dirs:\n          - path: {}\n            list_dir: true\n          - path: {}\n            \
         list_dir: false\n",
        real_home.join(".rustup/toolchains").join(toolchain).display(),
        libraries.display()
    );
    let fixture = Fixture::new(&capabilities, Some(&config));
    let output = fixture.doctor(&["--json", "--programs", "cargo"], &fixture.board_env());
    assert_build_links_or_names_cc(&document(&output));
}

/// Probes `name` from a manifest whose `shell.allow` names only it, applies the grant each probe
/// prints, and probes again until it runs. A grant may not repeat, and may not add a program
/// `shell.allow` already names.
fn follow_grants(name: &str, config: Option<&str>, env: &[(String, String)]) {
    const MAX_STEPS: usize = 10;
    let mut allow = vec![name.to_string()];
    let mut dirs: Vec<(String, bool)> = Vec::new();
    let mut grants = Vec::new();
    for _ in 0..MAX_STEPS {
        let mut capabilities = format!("  shell:\n    allow: [{}]\n", allow.join(", "));
        if !dirs.is_empty() {
            capabilities.push_str(&format!(
                "    interpreter_runtime:\n      - binary: {name}\n        dirs:\n"
            ));
            for (path, list_dir) in &dirs {
                capabilities.push_str(&format!(
                    "          - path: {path}\n            list_dir: {list_dir}\n"
                ));
            }
        }
        let fixture = Fixture::new(&capabilities, config);
        let report = document(&fixture.doctor(&["--json", "--programs", name], env));
        let probe = program(&report, name);
        if probe["status"] == "runs" {
            return;
        }
        let grant = probe["grant"]
            .as_str()
            .unwrap_or_else(|| panic!("refused with no grant after {grants:?}: {probe:#}"))
            .to_string();
        assert!(
            !grants.contains(&grant),
            "{grant} repeated after {grants:?}"
        );
        for part in grant.split("; and ") {
            if let Some(names) = part
                .strip_prefix("add ")
                .and_then(|rest| rest.strip_suffix(" to capabilities.shell.allow"))
            {
                for added in names.split(", ") {
                    assert!(
                        !allow.iter().any(|a| a == added),
                        "{grant} names {added} again"
                    );
                    allow.push(added.to_string());
                }
            } else if let Some(rest) =
                part.strip_prefix("declare capabilities.shell.interpreter_runtime: [{binary: ")
            {
                let rest = rest
                    .strip_prefix(&format!("{name}, dirs: [{{path: "))
                    .unwrap_or_else(|| panic!("{grant} is not keyed to {name}"));
                let (path, rest) = rest.split_once(", list_dir: ").unwrap();
                dirs.push((path.to_string(), rest.starts_with("true")));
            } else {
                panic!("a grant this walk cannot apply: {grant}");
            }
        }
        grants.push(grant);
    }
    panic!("{name} still refused after {MAX_STEPS} grants: {grants:?}");
}

/// Starting from `shell.allow: [cc]`, and from a rustup `cargo` with its homes lent, every grant
/// the probe prints changes the next probe's answer until the program runs: compile helpers
/// gcc names by path get their folder, and helpers on `PATH` get a `shell.allow` entry.
#[test]
fn following_each_grant_leads_cc_and_cargo_to_run() {
    const TEST: &str = "following_each_grant_leads_cc_and_cargo_to_run";
    if skip_without_namespace(TEST) {
        return;
    }
    if !Path::new("/usr/bin/cc").exists() {
        return skip_host(TEST, "needs /usr/bin/cc");
    }
    let path_only = vec![(
        "PATH".to_string(),
        "/usr/local/bin:/usr/bin:/bin".to_string(),
    )];
    follow_grants("cc", None, &path_only);

    let real_home = PathBuf::from(std::env::var_os("HOME").unwrap());
    if !rustup_cargo(&real_home) {
        return skip_host(TEST, "needs a rustup ~/.cargo/bin/cargo for its cargo half");
    }
    let config = format!(
        "host_paths:\n  - path: {}\n    access: read\n  - path: {}\n    access: read\n",
        real_home.join(".rustup").display(),
        real_home.join(".cargo").display()
    );
    let env = Fixture::new("  shell:\n    allow: [cargo]\n", None).board_env();
    follow_grants("cargo", Some(&config), &env);
}

/// No names probes the common list then each `shell.allow` entry not in it, each once;
/// programs this host lacks are `not-installed`, not errors.
#[test]
fn the_default_list_is_the_common_programs_then_shell_allow() {
    const TEST: &str = "the_default_list_is_the_common_programs_then_shell_allow";
    if skip_without_namespace(TEST) {
        return;
    }
    let fixture = Fixture::new(
        "  shell:\n    allow: [bash, git, jq-not-a-real-binary]\n",
        None,
    );
    let env = vec![(
        "PATH".to_string(),
        "/usr/local/bin:/usr/bin:/bin".to_string(),
    )];
    let output = fixture.doctor(&["--json", "--programs"], &env);
    let report = document(&output);
    let names: Vec<&str> = probed(&report)
        .iter()
        .map(|probe| probe["program"].as_str().unwrap())
        .collect();
    let mut expected = COMMON.to_vec();
    expected.push("jq-not-a-real-binary");
    assert_eq!(names, expected);
    let missing = program(&report, "jq-not-a-real-binary");
    assert_eq!(missing["cause"], "not-installed", "{missing:#}");
    assert_eq!(
        missing["grant"],
        "install jq-not-a-real-binary on this host so it is on mur run's PATH"
    );
    assert!(missing["resolved"].is_null() && missing["exit_code"].is_null());
    let warnings = w_run_015(&report);
    assert_eq!(warnings.len(), 1, "{report:#}");
    assert_eq!(warnings[0]["subject"], "jq-not-a-real-binary");
    if Path::new("/usr/bin/sudo").exists() {
        let sudo = program(&report, "sudo");
        assert_eq!(sudo["cause"], "no-new-privileges", "{sudo:#}");
        assert!(sudo["grant"]
            .as_str()
            .unwrap()
            .contains("mur run --unconfined"));
    }
    for probe in probed(&report) {
        if probe["resolved"].is_null() {
            assert_eq!(probe["cause"], "not-installed", "{probe:#}");
        }
    }
    assert_eq!(report["errors"], 0, "{report:#}");
}

/// A program `shell.allow` grants that is refused is one counted `W-RUN-015`, with the grant
/// as its fix, and the verdict stays `ready`.
#[test]
fn a_granted_program_that_cannot_run_is_a_counted_warning() {
    const TEST: &str = "a_granted_program_that_cannot_run_is_a_counted_warning";
    if skip_without_namespace(TEST) {
        return;
    }
    if !Path::new("/usr/bin/unshare").exists() {
        return skip_host(TEST, "needs /usr/bin/unshare");
    }
    let fixture = Fixture::new("  shell:\n    allow: [bash, unshare]\n", None);
    let env = vec![(
        "PATH".to_string(),
        "/usr/local/bin:/usr/bin:/bin".to_string(),
    )];
    let output = fixture.doctor(&["--json", "--programs", "unshare"], &env);
    let report = document(&output);
    assert_eq!(report["verdict"], "ready", "{report:#}");
    assert_eq!(report["exit_code"], 0);
    let warnings = w_run_015(&report);
    assert_eq!(warnings.len(), 1, "{report:#}");
    let warning = warnings[0];
    assert_eq!(warning["severity"], "warning");
    assert_eq!(warning["block"], "programs");
    assert_eq!(warning["subject"], "unshare");
    let grant = program(&report, "unshare")["grant"].clone();
    assert_eq!(warning["fix"], grant, "{report:#}");
    assert!(warning["message"]
        .as_str()
        .unwrap()
        .starts_with("unshare is in capabilities.shell.allow and cannot run in this capsule: "));

    let output = fixture.doctor(&["--programs", "unshare"], &env);
    let text = stdout(&output);
    assert_eq!(output.status.code(), Some(0), "{text}");
    assert!(text.contains(" 1 warning."), "{text}");
    assert!(
        text.contains(&format!("Fix: {}\n", grant.as_str().unwrap())),
        "{text}"
    );
}
