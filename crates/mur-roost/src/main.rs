//! The `mur-roost` binary: argument parsing, process hardening, and the accept loop.
//!
//! Every request the daemon answers is handled in [`mur_roost`], so the endpoints can be driven by
//! tests without a socket.

use std::collections::HashMap;
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

use mur_roost::bounds::{DEFAULT_MAX_CONCURRENT, DEFAULT_MAX_DEPTH};
use mur_roost::{authority::SpawnAuthority, handle_connection, State};

// ── CLI args ──────────────────────────────────────────────────────────────────

struct Args {
    port: u16,
    registry_path: PathBuf,
    spawn_allow: Vec<String>,
    max_depth: u32,
    max_concurrent: u32,
}

fn parse_args() -> Result<Args, String> {
    let raw: Vec<String> = std::env::args().collect();
    let mut port: u16 = 7700;
    let mut registry_path: Option<PathBuf> = None;
    let mut spawn_allow: Vec<String> = Vec::new();
    let mut max_depth: u32 = DEFAULT_MAX_DEPTH;
    let mut max_concurrent: u32 = DEFAULT_MAX_CONCURRENT;
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
            // The installer execs each binary it staged once before renaming it onto PATH, and
            // refuses the whole install when one will not start. That needs an invocation which
            // exits 0 without binding a port or taking a registry path: every other argument this
            // daemon accepts leads to a listening socket.
            "--version" => {
                println!("mur-roost {}", env!("CARGO_PKG_VERSION"));
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
    })
}

// ── main ──────────────────────────────────────────────────────────────────────

fn main() {
    if let Err(e) = capsule_runtime::security::harden_process_dumpable() {
        eprintln!("mur-roost: warning: failed to harden process against /proc environ reads: {e}");
    }

    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("mur-roost: {e}");
            std::process::exit(1);
        }
    };

    // One authority per process. Its key never reaches disk, so every credential and approval this
    // daemon issues dies with it — restarting is a complete revocation with no revocation list.
    let authority = match SpawnAuthority::generate() {
        Ok(authority) => Arc::new(authority),
        Err(e) => {
            eprintln!("mur-roost: {e}");
            std::process::exit(1);
        }
    };

    let state = Arc::new(State {
        jobs: Arc::new(Mutex::new(HashMap::new())),
        registry_path: args.registry_path,
        spawn_allow: args.spawn_allow,
        max_depth: args.max_depth,
        max_concurrent: args.max_concurrent,
        authority,
    });

    let listener = match TcpListener::bind(format!("127.0.0.1:{}", args.port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("mur-roost: failed to bind port {}: {e}", args.port);
            std::process::exit(1);
        }
    };

    eprintln!("mur-roost: listening on 127.0.0.1:{}", args.port);

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let state = Arc::clone(&state);
                thread::spawn(move || handle_connection(stream, state));
            }
            Err(e) => eprintln!("mur-roost: accept error: {e}"),
        }
    }
}
