//! The `mur-roost` binary: argument parsing, process hardening, and the accept loop.
//!
//! Every request the daemon answers is handled in [`mur_roost`], so the endpoints can be driven by
//! tests without a socket.

// Every write to standard output or standard error goes through `capsule_runtime::diagnostic`,
// whose report path drops a line the stream refuses instead of panicking.
#![cfg_attr(not(test), deny(clippy::print_stdout, clippy::print_stderr))]

use std::collections::HashMap;
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

use mur_roost::bounds::{default_max_live_capsules, DEFAULT_MAX_CONCURRENT, DEFAULT_MAX_DEPTH};
use mur_roost::{authority::SpawnAuthority, handle_connection, State};

/// The `mur` CLI's code for a standard-output write that failed for a reason other than a gone
/// reader. Defined in `murmur-cli`, which this crate does not depend on.
const E_IO_003: &str = "E-IO-003";

// ── CLI args ──────────────────────────────────────────────────────────────────

struct Args {
    port: u16,
    registry_path: PathBuf,
    spawn_allow: Vec<String>,
    max_depth: u32,
    max_concurrent: u32,
    max_live_capsules: u32,
}

fn parse_args() -> Result<Args, String> {
    let raw: Vec<String> = std::env::args().collect();
    let mut port: u16 = 7700;
    let mut registry_path: Option<PathBuf> = None;
    let mut spawn_allow: Vec<String> = Vec::new();
    let mut max_depth: u32 = DEFAULT_MAX_DEPTH;
    let mut max_concurrent: u32 = DEFAULT_MAX_CONCURRENT;
    // Derived from this host's core count rather than fixed, so a laptop and a build VM run under
    // different ceilings.
    let mut max_live_capsules: u32 = default_max_live_capsules();
    let mut i = 1;
    while i < raw.len() {
        match raw[i].as_str() {
            "--port" => {
                i += 1;
                port = raw
                    .get(i)
                    .ok_or("--port requires a value")?
                    .parse::<u16>()
                    .map_err(|e| format!("invalid --port: {e}"))?;
            }
            "--registry-path" => {
                i += 1;
                registry_path = Some(PathBuf::from(
                    raw.get(i).ok_or("--registry-path requires a value")?,
                ));
            }
            "--spawn-allow" => {
                i += 1;
                let val = raw.get(i).ok_or("--spawn-allow requires a value")?;
                spawn_allow.push(val.clone());
            }
            "--max-depth" => {
                i += 1;
                max_depth = raw
                    .get(i)
                    .ok_or("--max-depth requires a value")?
                    .parse::<u32>()
                    .map_err(|e| format!("invalid --max-depth: {e}"))?;
            }
            "--max-concurrent" => {
                i += 1;
                max_concurrent = raw
                    .get(i)
                    .ok_or("--max-concurrent requires a value")?
                    .parse::<u32>()
                    .map_err(|e| format!("invalid --max-concurrent: {e}"))?;
            }
            "--max-live-capsules" => {
                i += 1;
                max_live_capsules = raw
                    .get(i)
                    .ok_or("--max-live-capsules requires a value")?
                    .parse::<u32>()
                    .map_err(|e| format!("invalid --max-live-capsules: {e}"))?;
            }
            // The installer execs each binary it staged once before renaming it onto PATH, and
            // refuses the whole install when one will not start. That needs an invocation which
            // exits 0 without binding a port or taking a registry path: every other argument this
            // daemon accepts leads to a listening socket.
            "--version" => {
                capsule_runtime::report_println!("mur-roost {}", env!("CARGO_PKG_VERSION"));
                // A reader that went away is not a failure; any other refusal, such as `ENOSPC`
                // on a redirect to a full disk, means the version line was never written.
                if let Some(failure) = capsule_runtime::diagnostic::stdout_failure() {
                    capsule_runtime::report_eprintln!(
                        "error[{E_IO_003}]: failed to write to standard output: {failure}"
                    );
                    std::process::exit(1);
                }
                std::process::exit(0);
            }
            other if other.starts_with("--spawn-allow=") => {
                spawn_allow.push(other["--spawn-allow=".len()..].to_string());
            }
            other => return Err(format!("unknown argument: {other}")),
        }
        i += 1;
    }
    let registry_path = registry_path
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".murmur").join("artifacts"))
        })
        .ok_or("--registry-path is required")?;
    Ok(Args {
        port,
        registry_path,
        spawn_allow,
        max_depth,
        max_concurrent,
        max_live_capsules,
    })
}

// ── main ──────────────────────────────────────────────────────────────────────

fn main() {
    if let Err(e) = capsule_runtime::security::harden_process_dumpable() {
        capsule_runtime::report_eprintln!(
            "mur-roost: warning: failed to harden process against /proc environ reads: {e}"
        );
    }

    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            capsule_runtime::report_eprintln!("mur-roost: {e}");
            std::process::exit(1);
        }
    };

    // Counted before the bind, so no request is ever answered from a census missing the capsules a
    // previous daemon left running. A directory that exists and cannot be listed stops the daemon:
    // starting from zero would over-admit a host it cannot see.
    let inherited = match capsule_runtime::running::running_dir_location()
        .and_then(|dir| mur_roost::census::inherit(&dir))
    {
        Ok((inherited, report)) => {
            capsule_runtime::report_eprintln!("{}", report.startup_line());
            inherited
        }
        Err(reason) => {
            capsule_runtime::report_eprintln!(
                "mur-roost: cannot count the capsules already running on this host: {reason}"
            );
            std::process::exit(1);
        }
    };

    // One authority per process. Its key never reaches disk, so every credential and approval this
    // daemon issues dies with it — restarting is a complete revocation with no revocation list.
    let authority = match SpawnAuthority::generate() {
        Ok(authority) => Arc::new(authority),
        Err(e) => {
            capsule_runtime::report_eprintln!("mur-roost: {e}");
            std::process::exit(1);
        }
    };

    let state = Arc::new(State {
        jobs: Arc::new(Mutex::new(HashMap::new())),
        registry_path: args.registry_path,
        spawn_allow: args.spawn_allow,
        max_depth: args.max_depth,
        max_concurrent: args.max_concurrent,
        max_live_capsules: args.max_live_capsules,
        inherited: Arc::new(Mutex::new(inherited)),
        authority,
    });

    let listener = match TcpListener::bind(format!("127.0.0.1:{}", args.port)) {
        Ok(l) => l,
        Err(e) => {
            capsule_runtime::report_eprintln!("mur-roost: failed to bind port {}: {e}", args.port);
            std::process::exit(1);
        }
    };

    capsule_runtime::report_eprintln!("mur-roost: listening on 127.0.0.1:{}", args.port);
    // Printed whether the ceiling was set or derived, so an operator who never passed the flag can
    // still read the number they are running under.
    capsule_runtime::report_eprintln!(
        "mur-roost: machine ceiling {} live capsules (--max-live-capsules)",
        args.max_live_capsules,
    );

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let state = Arc::clone(&state);
                thread::spawn(move || handle_connection(stream, state));
            }
            Err(e) => capsule_runtime::report_eprintln!("mur-roost: accept error: {e}"),
        }
    }
}
