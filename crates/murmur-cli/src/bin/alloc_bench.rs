//! `mur-alloc-bench` — the harness that chose `mur`'s global allocator.
//!
//! Built once per candidate allocator and run round-robin by `scripts/alloc-bench.sh`, which
//! is where medians, deltas and the per-round sign count are computed. This binary parses no
//! output and aggregates nothing across allocators: it prints one line per round and lets the
//! driver decide what the rounds mean.
//!
//! It shares `mur`'s allocator selection by including the same source file, because
//! `murmur-cli` has no `[lib]` target for a second binary to `use`.

#[path = "../allocator.rs"]
mod allocator;

use std::path::PathBuf;
use std::process::ExitCode;

use capsule_runtime::alloc_bench::{compile_component_nanos, store_churn_nanos};

/// The largest single compile a session performs: the driver component, already in the tree as
/// a `murmur-cli` test fixture. Resolved at compile time so the harness runs from any cwd.
const DEFAULT_COMPONENT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/drivers/anthropic/driver/murmur-driver-anthropic.wasm"
);

const USAGE: &str = "\
mur-alloc-bench [--rounds N] [--compiles N] [--stores N] [--component PATH]

  --rounds N       measured rounds to run                    (default 15)
  --compiles N     component compiles per round              (default 8)
  --stores N       Store create-and-drop pairs per round     (default 40000)
  --component PATH component to compile          (default: the anthropic driver fixture)

Every line is tab-separated and opens with the allocator name.";

struct Args {
    rounds: usize,
    compiles: usize,
    stores: usize,
    component: PathBuf,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            rounds: 15,
            compiles: 8,
            stores: 40_000,
            component: PathBuf::from(DEFAULT_COMPONENT),
        }
    }
}

/// `Ok(None)` means `--help` was asked for, which is a successful run that measures nothing
/// rather than a usage error.
fn parse_args() -> Result<Option<Args>, String> {
    let mut args = Args::default();
    let mut argv = std::env::args().skip(1);
    while let Some(flag) = argv.next() {
        let mut value = || {
            argv.next()
                .ok_or_else(|| format!("{flag} needs a value\n\n{USAGE}"))
        };
        match flag.as_str() {
            "--rounds" => args.rounds = parse_count(&value()?)?,
            "--compiles" => args.compiles = parse_count(&value()?)?,
            "--stores" => args.stores = parse_count(&value()?)?,
            "--component" => args.component = PathBuf::from(value()?),
            "-h" | "--help" => return Ok(None),
            other => return Err(format!("unknown flag {other}\n\n{USAGE}")),
        }
    }
    Ok(Some(args))
}

fn parse_count(raw: &str) -> Result<usize, String> {
    match raw.parse::<usize>() {
        Ok(0) | Err(_) => Err(format!("expected a positive integer, got {raw:?}")),
        Ok(n) => Ok(n),
    }
}

/// The live thread count of this process, which is what `RLIMIT_NPROC` is enforced against and
/// what an allocator's background threads would move. Reported at both ends of the run so a
/// thread started lazily on first allocation is still caught.
fn thread_count() -> String {
    let status = match std::fs::read_to_string("/proc/self/status") {
        Ok(status) => status,
        Err(_) => return "unknown".to_string(),
    };
    status
        .lines()
        .find_map(|line| line.strip_prefix("Threads:"))
        .map(|count| count.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(Some(args)) => args,
        Ok(None) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::FAILURE;
        }
    };
    let name = allocator::ALLOCATOR_NAME;

    let wasm = match std::fs::read(&args.component) {
        Ok(wasm) => wasm,
        Err(err) => {
            eprintln!("cannot read {}: {err}", args.component.display());
            return ExitCode::FAILURE;
        }
    };

    println!("{name}\tthreads_before\t{}", thread_count());
    println!(
        "{name}\tconfig\trounds={}\tcompiles={}\tstores={}\tcomponent={}\tbytes={}",
        args.rounds,
        args.compiles,
        args.stores,
        args.component.display(),
        wasm.len()
    );

    for round in 1..=args.rounds {
        let compile_ns: u128 = compile_component_nanos(&wasm, args.compiles).iter().sum();
        let store_ns = store_churn_nanos(args.stores);
        println!("{name}\t{round}\t{compile_ns}\t{store_ns}");
    }

    println!("{name}\tthreads_after\t{}", thread_count());
    ExitCode::SUCCESS
}
