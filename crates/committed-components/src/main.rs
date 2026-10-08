//! `scripts/rebuild-components.sh` runs this binary from the repository root.
//!
//! ```text
//! committed-components [<name>]          rebuild every non-frozen component, or one
//! committed-components --check [<name>]  name the components the WIT tree has left stale
//! ```
//!
//! Exit 0 when everything asked for is done and current; 1 when a component is stale, missing,
//! unlisted, not a component, or failed to build; 2 when the run could not start as asked.

use std::io;
use std::path::Path;
use std::process::ExitCode;

use committed_components::{check, read_list, rebuild, rebuild_selection, wasm_target, LIST_PATH};

const USAGE: &str = "usage: scripts/rebuild-components.sh [<name>]
       scripts/rebuild-components.sh --check [<name>]

  <name>    one component, by the first column of crates/capsule-runtime/wit/components.list
  --check   write nothing; exit 1 naming every component whose murmur:* interface versions
            differ from the ones its WIT subtree declares";

const RAN_CLEAN: u8 = 0;
const FOUND_A_PROBLEM: u8 = 1;
const COULD_NOT_RUN: u8 = 2;

fn main() -> ExitCode {
    ExitCode::from(run())
}

fn run() -> u8 {
    let mut check_mode = false;
    let mut name = None;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--check" if !check_mode => check_mode = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return RAN_CLEAN;
            }
            _ if arg.starts_with('-') || name.is_some() => {
                eprintln!("error: unexpected argument `{arg}`\n\n{USAGE}");
                return COULD_NOT_RUN;
            }
            _ => name = Some(arg),
        }
    }

    let repo_root = Path::new(".");
    if !repo_root.join(LIST_PATH).is_file() {
        eprintln!("error: {LIST_PATH} not found — run this from the repository root");
        return COULD_NOT_RUN;
    }
    let list = match read_list(repo_root) {
        Ok(list) => list,
        Err(err) => {
            eprintln!("error: {err}");
            return COULD_NOT_RUN;
        }
    };

    if check_mode {
        return match check(repo_root, &list, name.as_deref()) {
            Ok(report) => {
                print!("{}", report.render());
                if report.failed() {
                    FOUND_A_PROBLEM
                } else {
                    RAN_CLEAN
                }
            }
            Err(err) => {
                eprintln!("error: {err}");
                COULD_NOT_RUN
            }
        };
    }

    // Unknown and frozen names are refused before the toolchain is looked at, so the refusal
    // names the real problem on a host without the target.
    if let Some(name) = &name {
        if let Err(err) = rebuild_selection(&list, Some(name)) {
            eprintln!("error: {err}");
            return COULD_NOT_RUN;
        }
    }
    let toolchain = match wasm_target() {
        Ok(toolchain) => toolchain,
        Err(err) => {
            eprintln!("error: {err}");
            return COULD_NOT_RUN;
        }
    };
    let repo_root = match std::env::current_dir() {
        Ok(dir) => dir,
        Err(err) => {
            eprintln!("error: could not resolve the current directory: {err}");
            return COULD_NOT_RUN;
        }
    };
    println!("{}", toolchain.version);
    let report = match rebuild(&repo_root, &list, name.as_deref(), &mut io::stdout()) {
        Ok(report) => report,
        Err(err) => {
            eprintln!("error: {err}");
            return COULD_NOT_RUN;
        }
    };
    for component in &report.frozen {
        println!(
            "skipped {}: frozen — {}",
            component.name,
            component.frozen.as_deref().unwrap_or_default()
        );
    }
    println!("{}", report.summary());
    if report.failed() {
        FOUND_A_PROBLEM
    } else {
        RAN_CLEAN
    }
}
