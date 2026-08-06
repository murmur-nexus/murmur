//! `escape-conformance` — the one-command hand-run gate.
//!
//! Exit codes are part of the interface and are deliberately distinct, because "the suite refused
//! to run" and "a boundary was crossed" are different facts and a wrapper script must be able to
//! tell them apart:
//!
//! | code | meaning | record written? |
//! |---|---|---|
//! | 0 | every asserted case matched its expected verdict | yes |
//! | 1 | usage error, or the harness itself could not proceed | no |
//! | 2 | **refused before running any case** — class gate, container, or a missing prerequisite | **no** |
//! | 3 | at least one **boundary** case failed — a containment escape | yes |
//! | 4 | boundary clean, but a **resource-exhaustion** case failed — denial of service | yes |
//!
//! 3 and 4 are separate so a resource failure can never be mistaken for an escape by a reader of
//! the exit status alone, which is the same rule the record and the stdout summary follow.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::str::FromStr;
use std::time::Duration;

use escape_conformance::cases::{self, Case};
use escape_conformance::host;
use escape_conformance::record::{Record, Stamp};
use escape_conformance::runner::{self, RunnerConfig};
use escape_conformance::verdict::Category;
use murmur_artifact::ContainmentClass;

const EXIT_OK: u8 = 0;
const EXIT_USAGE: u8 = 1;
const EXIT_REFUSED: u8 = 2;
const EXIT_BOUNDARY_FAILURE: u8 = 3;
const EXIT_RESOURCE_FAILURE: u8 = 4;

const USAGE: &str = "\
escape-conformance — hand-run containment release gate (never wired into CI)

USAGE:
    escape-conformance --class <advisory|scoped|sealed> [options]
    escape-conformance --list-cases

REQUIRED:
    --class <CLASS>        Containment class to assert: advisory, scoped or sealed.
                           The harness refuses, before running any case and without writing a
                           record, if this host cannot back the class.

OPTIONS:
    --record-dir <DIR>     Where the dated record is written. Default: the current directory.
    --work-root <DIR>      Per-case scratch (manifests, probes, mur output, traces). Kept after
                           the run; the record points at it per case.
                           Default: <record-dir>/escape-conformance-work-<stamp>.
    --mur <PATH>           The built `mur` binary. Default: $MUR_BIN, then this repository's
                           target/release/mur and target/debug/mur, then PATH.
    --probe-driver <PATH>  This package's probe-driver binary. Default: next to this binary.
    --python <NAME>        Interpreter the probes run as. Default: python3.
    --timeout-secs <N>     Wall-clock ceiling per case. Default: 300.
    --systemd-scope        Wrap each `mur run` in
                           `systemd-run --user --scope --property=Delegate=yes`.
                           Default: on when systemd-run is present, since without a delegated
                           cgroup v2 subtree `mur run` refuses every subprocess-capable capsule
                           with E-RUN-012.
    --no-systemd-scope     Turn that off (e.g. already inside a delegated scope).
    --allow-container      Run even though a container was detected. The record is stamped
                           NOT W-SEC-005 EVIDENCE and cannot be cited.
    --only <CASE-ID>       Run one case (repeatable). The record is stamped PARTIAL RUN and
                           cannot be cited. For iterating by hand, never for a release gate.
    --list-cases           Print the case registry with per-class expectations and exit. Runs
                           nothing and writes no record.
    -h, --help             This text.

See ESCAPE_CONFORMANCE_HARNESS.md at the repository root for the full procedure.
";

struct Options {
    class: Option<ContainmentClass>,
    record_dir: PathBuf,
    work_root: Option<PathBuf>,
    mur: Option<PathBuf>,
    probe_driver: Option<PathBuf>,
    python: String,
    timeout: Duration,
    systemd_scope: Option<bool>,
    allow_container: bool,
    only: Vec<String>,
    list_cases: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            class: None,
            record_dir: PathBuf::from("."),
            work_root: None,
            mur: None,
            probe_driver: None,
            python: "python3".to_string(),
            timeout: Duration::from_secs(300),
            systemd_scope: None,
            allow_container: false,
            only: Vec::new(),
            list_cases: false,
        }
    }
}

/// Hand-rolled rather than `clap`, mirroring `racecheck/`: this package's dependency set is one
/// crate, and a reviewer should be able to convince themselves the gate has no behaviour beyond
/// what is in these files.
fn parse_args(argv: &[String]) -> Result<Options, String> {
    let mut options = Options::default();
    let mut args = argv.iter().skip(1);
    while let Some(arg) = args.next() {
        let mut value = |name: &str| -> Result<String, String> {
            args.next()
                .cloned()
                .ok_or_else(|| format!("{name} requires a value"))
        };
        match arg.as_str() {
            "--class" => {
                let raw = value("--class")?;
                options.class = Some(ContainmentClass::from_str(&raw).map_err(|_| {
                    format!("--class: unknown containment class {raw:?} (expected advisory, scoped or sealed)")
                })?);
            }
            "--record-dir" => options.record_dir = PathBuf::from(value("--record-dir")?),
            "--work-root" => options.work_root = Some(PathBuf::from(value("--work-root")?)),
            "--mur" => options.mur = Some(PathBuf::from(value("--mur")?)),
            "--probe-driver" => options.probe_driver = Some(PathBuf::from(value("--probe-driver")?)),
            "--python" => options.python = value("--python")?,
            "--timeout-secs" => {
                let raw = value("--timeout-secs")?;
                let secs: u64 = raw
                    .parse()
                    .map_err(|_| format!("--timeout-secs: not a number: {raw}"))?;
                if secs == 0 {
                    return Err("--timeout-secs must be greater than 0".to_string());
                }
                options.timeout = Duration::from_secs(secs);
            }
            "--systemd-scope" => options.systemd_scope = Some(true),
            "--no-systemd-scope" => options.systemd_scope = Some(false),
            "--allow-container" => options.allow_container = true,
            "--only" => {
                let id = value("--only")?;
                if cases::find(&id).is_none() {
                    return Err(format!(
                        "--only: no case named {id:?}; run --list-cases to see the registry"
                    ));
                }
                options.only.push(id);
            }
            "--list-cases" => options.list_cases = true,
            "-h" | "--help" => return Err(USAGE.to_string()),
            other => return Err(format!("unrecognised argument: {other}")),
        }
    }
    Ok(options)
}

/// Repository root, derived from this package's own location.
fn repo_root() -> PathBuf {
    // <repo>/crates/capsule-runtime/escape-conformance
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf()
}

/// Finds `name` on `PATH`.
fn on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

fn resolve_mur(explicit: Option<PathBuf>) -> Result<PathBuf, String> {
    if let Some(path) = explicit {
        return if path.is_file() {
            Ok(path)
        } else {
            Err(format!("--mur: {} is not a file", path.display()))
        };
    }
    if let Some(raw) = std::env::var_os("MUR_BIN") {
        let path = PathBuf::from(raw);
        if path.is_file() {
            return Ok(path);
        }
    }
    let root = repo_root();
    for candidate in [
        root.join("target/release/mur"),
        root.join("target/debug/mur"),
    ] {
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    on_path("mur").ok_or_else(|| {
        "could not find a `mur` binary. Build one with `cargo build --release -p murmur-cli` from \
         the repository root, or pass --mur <path>."
            .to_string()
    })
}

fn resolve_probe_driver(explicit: Option<PathBuf>) -> Result<PathBuf, String> {
    if let Some(path) = explicit {
        return if path.is_file() {
            Ok(path)
        } else {
            Err(format!("--probe-driver: {} is not a file", path.display()))
        };
    }
    let sibling = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("probe-driver")));
    match sibling {
        Some(path) if path.is_file() => Ok(path),
        _ => Err(
            "could not find `probe-driver` next to this binary. Build both with `cargo build \
             --release` inside this package, or pass --probe-driver <path>."
                .to_string(),
        ),
    }
}

/// `mur --version`, for the record. A gate's evidence has to name the binary it graded.
fn mur_version(mur: &Path) -> String {
    Command::new(mur)
        .arg("--version")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn list_cases() {
    println!("Escape-conformance case registry — {} cases", cases::all_cases().len());
    println!(
        "  {} boundary, {} resource_exhaustion\n",
        cases::in_category(Category::Boundary).count(),
        cases::in_category(Category::ResourceExhaustion).count()
    );
    println!(
        "{:<34} {:<20} {:<14} {:<14} SEALED",
        "CASE", "CATEGORY", "ADVISORY", "SCOPED"
    );
    for case in cases::all_cases() {
        println!(
            "{:<34} {:<20} {:<14} {:<14} {}",
            case.id,
            // `as_str`, not the Display impl: a custom Display ignores the width specifier, so
            // the columns would not line up.
            case.category.as_str(),
            case.advisory.as_str(),
            case.scoped.as_str(),
            case.sealed.as_str()
        );
    }
    println!(
        "\n`not-asserted` is not a skip: the case still runs and its verdict is still recorded, \
         but the declared class provides no mechanism that could back a claim, so the result \
         cannot pass or fail.\n\
         `not graded` is also not a skip: the `sealed` column records the class's intended \
         verdict, and the case still runs and is still recorded, but nobody has validated those \
         expectations against a real composed root yet, so they gate nothing. See \
         docs/content/reference/sealed-containment-manual-verification.md."
    );
}

/// Everything that must hold before the first case runs. Any failure here is a refusal (exit 2),
/// never a skipped case.
fn gate(options: &Options, facts: &host::HostFacts, class: ContainmentClass) -> Result<(), String> {
    let achieved = capsule_runtime::detect_achieved_containment();
    // The third argument is what turns "sealed is unavailable here" into a specific, actionable
    // sentence — the missing AppArmor profile, a container without CAP_SYS_ADMIN, a kernel
    // without user namespaces. It probes the same cached host facts `detect_achieved_containment`
    // above does.
    let sealed_blocker = capsule_runtime::detect_sealed_blocker();
    if let Some(reason) =
        capsule_runtime::containment_shortfall_reason(class, achieved, sealed_blocker)
    {
        return Err(format!(
            "REFUSED — this host cannot back the class under test.\n\
             \x20 declared: {class}\n\
             \x20 achieved: {achieved}\n\
             \x20 reason:   {reason}\n\n\
             No record file was written. An absent record is not a passing one: nothing about \
             the `{class}` class was asserted on this machine, and nothing here may be cited as \
             W-SEC-005 evidence. Run the suite on a host that provides `{class}`."
        ));
    }

    if facts.container.detected && !options.allow_container {
        return Err(format!(
            "REFUSED — a container was detected ({}).\n\n\
             The suite must run on bare metal. During the original investigation a container \
             masked three separate findings: the raw-disk escape, the `docker.sock` escape, and \
             the entire syscall surface all looked closed inside Docker and were wide open \
             outside it. A run in here would certify the container's boundary, not this \
             runtime's.\n\n\
             No record file was written. Pass --allow-container to run anyway; the record is \
             then stamped NOT W-SEC-005 EVIDENCE and cannot be cited.",
            facts.container.firing()
        ));
    }

    Ok(())
}

fn run() -> Result<u8, String> {
    let argv: Vec<String> = std::env::args().collect();
    let options = parse_args(&argv)?;

    if options.list_cases {
        list_cases();
        return Ok(EXIT_OK);
    }

    let Some(class) = options.class else {
        return Err(format!("--class is required.\n\n{USAGE}"));
    };

    let facts = host::probe();
    let achieved = capsule_runtime::detect_achieved_containment();

    println!("escape-conformance — hand-run containment gate");
    println!("  host:      {} ({})", facts.kernel_system, facts.kernel_release);
    println!("  container: {}", facts.container);
    println!("  declared:  {class}");
    println!("  achieved:  {achieved}");

    if let Err(refusal) = gate(&options, &facts, class) {
        eprintln!("\n{refusal}");
        return Ok(EXIT_REFUSED);
    }

    // Prerequisites are resolved *after* the class gate so the most fundamental refusal is the
    // one a reader sees first, but still before any case runs — a missing prerequisite is a
    // refusal too, never a run with holes in it.
    let mur = resolve_mur(options.mur.clone())?;
    let probe_driver = resolve_probe_driver(options.probe_driver.clone())?;
    let interpreter_dirs = runner::derive_interpreter_dirs(&options.python).map_err(|err| {
        format!(
            "REFUSED — {err}\n\n\
             Every probe runs as `{}`, because under the full enforcement tier a capsule may only \
             exec the binaries named in capabilities.shell.allow. Without a working interpreter \
             no case can report anything, and a suite that ran anyway would report INCONCLUSIVE \
             for everything. No record file was written.",
            options.python
        )
    })?;

    let systemd_scope = options
        .systemd_scope
        .unwrap_or_else(|| cfg!(target_os = "linux") && on_path("systemd-run").is_some());

    let stamp = Stamp::now();
    let work_root = options.work_root.clone().unwrap_or_else(|| {
        options
            .record_dir
            .join(format!("escape-conformance-work-{}", stamp.compact()))
    });
    std::fs::create_dir_all(&work_root)
        .map_err(|err| format!("could not create {}: {err}", work_root.display()))?;
    // Absolute from here on. Each case's `mur run` is spawned with its own capsule workdir as
    // cwd, so a relative scratch path would resolve against the wrong directory and every case
    // would fail to find its manifest — an infrastructure fault that would read as 28 escapes.
    let work_root = work_root
        .canonicalize()
        .map_err(|err| format!("could not resolve {}: {err}", work_root.display()))?;

    println!("  mur:       {} ({})", mur.display(), mur_version(&mur));
    println!("  driver:    {}", probe_driver.display());
    println!("  scratch:   {}", work_root.display());
    println!(
        "  python:    {} (interpreter_runtime grants: {})",
        options.python,
        interpreter_dirs
            .iter()
            .map(|(p, _)| p.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    if !facts.is_root() {
        println!(
            "  note:      non-root run (euid {}). `mknod`, `bpf` and `open_by_handle_at` refuse \
             for an ordinary uid regardless of this runtime, so those three cases are recorded \
             but not attributable — see each case's attribution note in the record.",
            facts.euid
        );
    }

    let config = RunnerConfig {
        mur: mur.clone(),
        probe_driver,
        work_root,
        timeout: options.timeout,
        systemd_scope,
        interpreter_dirs,
    };

    // Last gate before the first case. A host where no probe can start is refused, not run with
    // every case reporting INCONCLUSIVE — see `runner::preflight`.
    print!("\npreflight … ");
    let _ = std::io::Write::flush(&mut std::io::stdout());
    match runner::preflight(&config, class) {
        Ok(evidence) => println!("ok — {evidence}"),
        Err(refusal) => {
            println!("REFUSED");
            eprintln!("\n{refusal}");
            return Ok(EXIT_REFUSED);
        }
    }

    let selected: Vec<&'static Case> = if options.only.is_empty() {
        cases::all_cases().iter().collect()
    } else {
        options
            .only
            .iter()
            .filter_map(|id| cases::find(id))
            .collect()
    };

    println!("\nrunning {} case(s)\n", selected.len());
    let mut outcomes = Vec::with_capacity(selected.len());
    for case in &selected {
        print!("  {:<34} … ", case.id);
        let _ = std::io::Write::flush(&mut std::io::stdout());
        let outcome = runner::run_case(case, &config, class);
        let status = if !outcome.expectation.gates() {
            "recorded (not asserted at this class)".to_string()
        } else if outcome.passed {
            "PASS".to_string()
        } else {
            "FAIL".to_string()
        };
        println!(
            "{:<12} expected {:<14} → {}",
            outcome.verdict.as_str(),
            outcome.expectation.as_str(),
            status
        );
        outcomes.push(outcome);
    }

    // ── The two rollups, never merged ─────────────────────────────────────────────────────
    let failed_in = |category: Category| -> usize {
        outcomes
            .iter()
            .filter(|o| o.case.category == category && o.expectation.gates() && !o.passed)
            .count()
    };
    let boundary_failures = failed_in(Category::Boundary);
    let resource_failures = failed_in(Category::ResourceExhaustion);

    let record = Record {
        stamp,
        declared: class,
        achieved,
        host: &facts,
        mur_binary: mur.display().to_string(),
        mur_version: mur_version(&mur),
        outcomes: &outcomes,
        partial: !options.only.is_empty(),
        container_override: options.allow_container && facts.container.detected,
        invocation: argv.join(" "),
    };
    let record_path = record
        .write_to(&options.record_dir)
        .map_err(|err| format!("could not write the record: {err}"))?;

    println!("\n── summary ─────────────────────────────────────────────────────────────");
    println!(
        "  boundary:            {}",
        if boundary_failures == 0 {
            "no boundary was crossed".to_string()
        } else {
            format!("A BOUNDARY WAS CROSSED — {boundary_failures} case(s) failed")
        }
    );
    println!(
        "  resource exhaustion: {}",
        if resource_failures == 0 {
            "every declared ceiling held".to_string()
        } else {
            format!(
                "{resource_failures} ceiling(s) did not hold — denial of service, NOT an escape"
            )
        }
    );
    println!("  record:              {}", record_path.display());
    if !record.is_citable() {
        println!("  NOTE:                this record is stamped NOT W-SEC-005 EVIDENCE");
    }

    Ok(if boundary_failures > 0 {
        EXIT_BOUNDARY_FAILURE
    } else if resource_failures > 0 {
        EXIT_RESOURCE_FAILURE
    } else {
        EXIT_OK
    })
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(message) => {
            eprintln!("{message}");
            ExitCode::from(EXIT_USAGE)
        }
    }
}
