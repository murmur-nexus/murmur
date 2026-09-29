//! `mur control`: a thin client of a running capsule's control plane.
//!
//! Every subcommand resolves the session through its running record, reads the control token the
//! capsule wrote beside that record, and makes one plain HTTP request to `/control` on the door's
//! listener. It decides nothing: what may change, and whether a value is acceptable, is the
//! capsule's answer, printed here. There is no `--url`, because the token is found only through
//! the record.
//!
//! Neither the token nor a secret value reaches stdout, stderr or an error.

use std::io::{BufRead, BufReader, IsTerminal, Read, Write};
use std::time::Duration;

use clap::Subcommand;
use serde_json::Value;

use crate::error::{CliError, E_IO_003, E_RUN_028, E_RUN_042};
use crate::live_address::{resolve_live, LATEST};

/// How long the control plane has to answer once connected. It takes a lock and answers, behind
/// whatever else the connection task is serving.
const CONTROL_READ_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Subcommand)]
pub(crate) enum ControlCommand {
    /// Show what this session lets a controller change, and the current values
    Show {
        /// Running session: @1, a ses_ id, or a 4-character suffix of one (default: @1)
        #[arg(value_name = "SESSION")]
        session: Option<String>,
        /// Print the control surface's JSON as it answered
        #[arg(long)]
        json: bool,
    },
    /// Change a setting the capsule's control: block declares, from its next inference call
    Set {
        /// The setting, e.g. inference.max_tokens
        #[arg(value_name = "SETTING")]
        setting: String,
        /// The new value
        #[arg(value_name = "VALUE", allow_hyphen_values = true)]
        value: String,
        /// Running session: @1, a ses_ id, or a 4-character suffix of one (default: @1)
        #[arg(value_name = "SESSION")]
        session: Option<String>,
    },
    /// Supply a secret the capsule's control: block declares, read from stdin
    Secret {
        /// The secret's name, as control.secrets lists it
        #[arg(value_name = "NAME")]
        name: String,
        /// Running session: @1, a ses_ id, or a 4-character suffix of one (default: @1)
        #[arg(value_name = "SESSION")]
        session: Option<String>,
    },
    /// Drop a secret the capsule holds, so its gateway's keyed requests are refused again
    Forget {
        /// The secret's name, as control.secrets lists it
        #[arg(value_name = "NAME")]
        name: String,
        /// Running session: @1, a ses_ id, or a 4-character suffix of one (default: @1)
        #[arg(value_name = "SESSION")]
        session: Option<String>,
    },
}

pub(crate) fn run_control(command: ControlCommand) -> Result<(), CliError> {
    match command {
        ControlCommand::Show { session, json } => {
            let surface = Surface::resolve(session.as_deref())?;
            let listing = surface.request("GET", "/control", None, &[])?;
            if json {
                println!("{listing}");
            } else {
                print_listing(&listing);
            }
            Ok(())
        }
        ControlCommand::Set {
            setting,
            value,
            session,
        } => {
            let surface = Surface::resolve(session.as_deref())?;
            // Sent as JSON when it parses as JSON and as a string otherwise, so the capsule, not
            // this client, decides what the setting takes.
            let value = serde_json::from_str::<Value>(&value).unwrap_or(Value::String(value));
            let body = serde_json::json!({ "value": value }).to_string();
            let answer = surface.request(
                "PUT",
                &format!("/control/settings/{setting}"),
                Some("application/json"),
                body.as_bytes(),
            )?;
            println!("setting:  {}", field(&answer, "name", &setting));
            println!("previous: {}", field(&answer, "previous", "?"));
            println!("value:    {}", field(&answer, "value", "?"));
            println!("applies:  next inference call");
            Ok(())
        }
        ControlCommand::Secret { name, session } => {
            let surface = Surface::resolve(session.as_deref())?;
            let mut value = read_secret(&name)?;
            let answer = surface.request(
                "PUT",
                &format!("/control/secrets/{name}"),
                Some("application/octet-stream"),
                &value,
            );
            overwrite(&mut value);
            let answer = answer?;
            let replaced = answer.get("replaced").and_then(Value::as_bool) == Some(true);
            println!("secret: {name}");
            println!(
                "set:    yes ({})",
                if replaced { "replaced" } else { "new" }
            );
            Ok(())
        }
        ControlCommand::Forget { name, session } => {
            let surface = Surface::resolve(session.as_deref())?;
            surface.request("DELETE", &format!("/control/secrets/{name}"), None, &[])?;
            println!("secret: {name}");
            println!("set:    no");
            Ok(())
        }
    }
}

/// `answer[key]` for printing: a string bare, anything else as JSON, `default` when absent.
fn field(answer: &Value, key: &str, default: &str) -> String {
    match answer.get(key) {
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
        None => default.to_string(),
    }
}

fn print_listing(listing: &Value) {
    println!("session:  {}", field(listing, "session_id", "?"));
    let settings = listing
        .get("settings")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let secrets = listing
        .get("secrets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let width = settings
        .iter()
        .chain(&secrets)
        .map(|entry| field(entry, "name", "").len())
        .max()
        .unwrap_or(0);
    if !settings.is_empty() {
        println!("settings:");
        for setting in &settings {
            println!(
                "  {:<width$}   {}",
                field(setting, "name", "?"),
                field(setting, "value", "?")
            );
        }
    }
    if !secrets.is_empty() {
        println!("secrets:");
        for secret in &secrets {
            let set = secret.get("set").and_then(Value::as_bool) == Some(true);
            println!(
                "  {:<width$}   {}",
                field(secret, "name", "?"),
                if set { "set" } else { "not set" }
            );
        }
    }
}

/// One running session's control plane: where it listens and the token it takes.
struct Surface {
    session_id: String,
    addr: String,
    token: String,
}

impl Surface {
    /// Resolves `session` (default `@1`) to a running, answering capsule and reads its token.
    ///
    /// `E-RUN-042` when the session holds no token file: it declares no `control:` block.
    fn resolve(session: Option<&str>) -> Result<Self, CliError> {
        let record = resolve_live(session.unwrap_or(LATEST))?;
        let path = capsule_runtime::running::control_token_path(&record.session_id)
            .map_err(|reason| crate::live_address::records_unreadable(&reason))?;
        let token = match std::fs::read_to_string(&path) {
            Ok(token) => token.trim().to_string(),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(CliError::with_hint(
                    E_RUN_042,
                    format!(
                        "{} has no control surface: it holds no control token, because its \
                         murmur.yaml declares no control: block",
                        record.session_id
                    ),
                    "declare control: {settings: [...], secrets: [...]} in murmur.yaml and \
                     restart the capsule — what is controllable is fixed at launch",
                ));
            }
            Err(err) => {
                return Err(CliError::new(
                    E_RUN_028,
                    format!("{} could not be read: {err}", path.display()),
                ));
            }
        };
        Ok(Self {
            session_id: record.session_id,
            addr: record.url,
            token,
        })
    }

    /// One request to the control plane, its JSON answer on `200`.
    ///
    /// Any other status is `E-RUN-042`, stating the status and the plane's one-sentence reason.
    fn request(
        &self,
        method: &str,
        path: &str,
        content_type: Option<&str>,
        body: &[u8],
    ) -> Result<Value, CliError> {
        let stream = crate::commands::cancel::connect_with_timeout(&self.addr)?;
        stream.set_read_timeout(Some(CONTROL_READ_TIMEOUT)).ok();
        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\n\
             Content-Length: {}\r\nConnection: close\r\n",
            self.addr,
            self.token,
            body.len()
        );
        if let Some(content_type) = content_type {
            head.push_str(&format!("Content-Type: {content_type}\r\n"));
        }
        head.push_str("\r\n");
        let mut writer = &stream;
        writer
            .write_all(head.as_bytes())
            .and_then(|()| writer.write_all(body))
            .and_then(|()| writer.flush())
            .map_err(|e| CliError::new(E_IO_003, format!("failed to send request: {e}")))?;

        let mut reader = BufReader::new(&stream);
        let mut status_line = String::new();
        reader
            .read_line(&mut status_line)
            .map_err(|e| CliError::new(E_IO_003, format!("failed to read response status: {e}")))?;
        let status: u16 = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .ok_or_else(|| {
                CliError::new(
                    E_IO_003,
                    format!(
                        "the capsule answered with no HTTP status: {}",
                        status_line.trim()
                    ),
                )
            })?;
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) if line.trim().is_empty() => break,
                Ok(_) => {}
            }
        }
        let mut text = String::new();
        let _ = reader.read_to_string(&mut text);
        let answer: Value = serde_json::from_str(&text).unwrap_or(Value::Null);

        if status != 200 {
            let reason = answer
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("the control surface gave no reason");
            return Err(CliError::new(
                E_RUN_042,
                format!(
                    "the control surface of {} refused {method} {path}: HTTP {status}: {reason}",
                    self.session_id
                ),
            ));
        }
        Ok(answer)
    }
}

/// The secret's bytes from stdin, with exactly one trailing `\n` or `\r\n` removed.
///
/// On a terminal, prompts on stderr and turns echo off for the read, restoring it on return and
/// on `SIGINT`, `SIGTERM` or `SIGHUP`; from a pipe or file, reads to end of input.
fn read_secret(name: &str) -> Result<Vec<u8>, CliError> {
    let stdin = std::io::stdin();
    let mut value = Vec::new();
    let read = if stdin.is_terminal() {
        eprint!("value for {name}: ");
        let _ = std::io::stderr().flush();
        let echo = EchoOff::new();
        let read = stdin.lock().read_until(b'\n', &mut value);
        drop(echo);
        eprintln!();
        read
    } else {
        stdin.lock().read_to_end(&mut value)
    };
    read.map_err(|e| {
        CliError::new(
            E_IO_003,
            format!("failed to read the secret from stdin: {e}"),
        )
    })?;
    if value.ends_with(b"\r\n") {
        value.truncate(value.len() - 2);
    } else if value.ends_with(b"\n") {
        value.truncate(value.len() - 1);
    }
    Ok(value)
}

/// Overwrites `bytes` so the value does not outlive the request in this process's heap.
fn overwrite(bytes: &mut [u8]) {
    bytes.fill(0);
    std::hint::black_box(bytes);
}

/// The terminal's attributes before echo was turned off, for the signal handler to put back.
static SAVED_TERMIOS: std::sync::OnceLock<libc::termios> = std::sync::OnceLock::new();

/// Terminal echo turned off on stdin for as long as this lives.
struct EchoOff {
    active: bool,
}

impl EchoOff {
    #[allow(unsafe_code)]
    fn new() -> Self {
        // SAFETY: `termios` is plain old data, so a zeroed value is a valid one for `tcgetattr`
        // to overwrite; it and `tcsetattr` read and write only the struct passed by pointer, which
        // lives on this stack frame for the duration of each call.
        unsafe {
            let mut original: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut original) != 0 {
                return Self { active: false };
            }
            let _ = SAVED_TERMIOS.set(original);
            for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
                libc::signal(
                    signal,
                    restore_and_reraise as extern "C" fn(libc::c_int) as libc::sighandler_t,
                );
            }
            let mut silent = original;
            silent.c_lflag &= !libc::ECHO;
            let active = libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &silent) == 0;
            Self { active }
        }
    }
}

impl Drop for EchoOff {
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        if let (true, Some(original)) = (self.active, SAVED_TERMIOS.get()) {
            // SAFETY: `original` is a `termios` `tcgetattr` filled in, borrowed from a static
            // that is never written again, and `tcsetattr` only reads it.
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, original);
            }
        }
    }
}

/// Puts the terminal's saved attributes back, then lets `signal` end the process as it would have.
#[allow(unsafe_code)]
extern "C" fn restore_and_reraise(signal: libc::c_int) {
    // SAFETY: `tcsetattr`, `signal` and `raise` are async-signal-safe. The static is set before
    // this handler is installed and never written afterwards, so reading it here races nothing.
    unsafe {
        if let Some(original) = SAVED_TERMIOS.get() {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, original);
        }
        libc::signal(signal, libc::SIG_DFL);
        libc::raise(signal);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_prints_strings_bare_and_numbers_as_json() {
        let answer = serde_json::json!({"name": "inference.max_tokens", "value": 2048});
        assert_eq!(field(&answer, "name", "?"), "inference.max_tokens");
        assert_eq!(field(&answer, "value", "?"), "2048");
        assert_eq!(field(&answer, "previous", "?"), "?");
    }
}
