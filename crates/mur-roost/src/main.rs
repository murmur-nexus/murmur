//! The `mur-roost` binary: argument parsing, process hardening, and the accept loop.
//!
//! Every request the daemon answers is handled in [`mur_roost`], so the endpoints can be driven by
//! tests without a socket.

use std::collections::HashMap;
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

use mur_roost::{handle_connection, State};

// ── CLI args ──────────────────────────────────────────────────────────────────

struct Args {
    port: u16,
    registry_path: PathBuf,
    spawn_allow: Vec<String>,
}

fn parse_args() -> Result<Args, String> {
    let raw: Vec<String> = std::env::args().collect();
    let mut port: u16 = 7700;
    let mut registry_path: Option<PathBuf> = None;
    let mut spawn_allow: Vec<String> = Vec::new();
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

    let state = Arc::new(State {
        jobs: Arc::new(Mutex::new(HashMap::new())),
        registry_path: args.registry_path,
        spawn_allow: args.spawn_allow,
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
