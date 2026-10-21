//! Fixture native tool for murmur's own integration tests.
//!
//! It speaks the protocol `capsule_runtime::dispatch_native_tool` expects, so the
//! test suite can exercise native tool staging, dispatch and schema plumbing
//! without depending on a binary built in a sibling artifact checkout.
//!
//! stdin (one JSON object, EOF-terminated):
//!   {"data": <string | object>, "log_path": <string | null>}
//!   `data` is required. The runtime forwards the model's tool arguments as a
//!   JSON *string*; a direct caller may pass the object itself. Both are accepted.
//!   `log_path` is optional; when it is a string, one line per invocation is
//!   appended to that file.
//!
//! stdout (one JSON object):
//!   {"status": "passed" | "failed", "summary": <string>, "data": <object | null>}
//!   `status` and `summary` are always written. `data` is present on success and
//!   null on failure, which is what makes the runtime fall back to `summary` as
//!   the tool result text.
//!
//! Operations:
//!   create_dir   — repo?, path (required), label?  -> {path, label}
//!   list_entries — repo?, path?                    -> {dir, entries}
//!
//! Probe operations, for the sandbox tests. Each reports its outcome in `data`
//! with `status: passed` whether the attempt succeeded or not, so a test reads
//! the OS error text rather than a bare failure:
//!   read        — path                    -> {ok, contents} | {ok: false, error}
//!   write       — path, text              -> {ok}           | {ok: false, error}
//!   connect     — host, port, send?       -> {ok, reply}    | {ok: false, error}
//!   unix_socket —                         -> {ok}           | {ok: false, error}
//!   exec        — path, args?             -> {ok, code, stdout} | {ok: false, error}
//!   netns       —                         -> {ok, netns}    | {ok: false, error}
//!   rlimit_nofile —                       -> {ok, soft, hard} | {ok: false, error}
//!   open_until_refused — max? (default 200) -> {ok, opened, error: <text> | null}
//! A relative `path` resolves against the working directory.

use std::{
    fs,
    io::{Read, Write},
    net::{TcpStream, ToSocketAddrs},
    path::{Path, PathBuf},
    time::Duration,
};

use serde_json::{json, Value};

fn main() {
    let mut raw = String::new();
    if std::io::stdin().read_to_string(&mut raw).is_err() {
        eprintln!("fatal: failed to read stdin");
        std::process::exit(1);
    }
    let result = run(&raw);
    let encoded = serde_json::to_string(&result).unwrap_or_else(|_| {
        r#"{"status":"failed","summary":"failed to serialize output","data":null}"#.to_string()
    });
    println!("{encoded}");
}

fn run(raw: &str) -> Value {
    if raw.trim().is_empty() {
        return failed("missing input on stdin");
    }

    let envelope: Value = match serde_json::from_str(raw) {
        Ok(value) => value,
        Err(err) => return failed(format!("invalid stdin JSON: {err}")),
    };

    let data = match envelope.get("data") {
        None | Some(Value::Null) => return failed("missing data field"),
        Some(value) => value.clone(),
    };

    // The runtime double-encodes: `data` arrives as a JSON string holding the
    // arguments object. Direct callers pass the object itself.
    let op: Value = match &data {
        Value::String(text) => match serde_json::from_str(text) {
            Ok(value) => value,
            Err(err) => return failed(format!("invalid data JSON string: {err}")),
        },
        Value::Object(_) => data.clone(),
        _ => return failed("data must be a JSON string or object"),
    };

    if let Some(log_path) = envelope.get("log_path").and_then(Value::as_str) {
        let operation = op.get("operation").and_then(Value::as_str).unwrap_or("");
        if let Ok(mut file) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
        {
            let _ = writeln!(file, "tool-fixture: {operation}");
        }
    }

    let base = match resolve_base(&op) {
        Ok(base) => base,
        Err(message) => return failed(message),
    };

    match op.get("operation").and_then(Value::as_str).unwrap_or("") {
        "create_dir" => op_create_dir(&op, &base),
        "list_entries" => op_list_entries(&op, &base),
        "read" => probe(op_read(&op)),
        "write" => probe(op_write(&op)),
        "connect" => probe(op_connect(&op)),
        "unix_socket" => probe(op_unix_socket()),
        "exec" => probe(op_exec(&op)),
        "netns" => probe(op_netns()),
        "rlimit_nofile" => probe(op_rlimit_nofile()),
        "open_until_refused" => probe(Ok(op_open_until_refused(&op))),
        other => failed(format!("unknown operation '{other}'")),
    }
}

/// The base directory the operation runs against: the explicit `repo` field when
/// given, and the process working directory (the capsule workdir, under the
/// runtime) otherwise.
fn resolve_base(op: &Value) -> Result<PathBuf, String> {
    match op.get("repo").and_then(Value::as_str) {
        Some(repo) => Ok(PathBuf::from(repo)),
        None => std::env::current_dir().map_err(|err| format!("cannot read working dir: {err}")),
    }
}

fn canonicalize_or_keep(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn op_create_dir(op: &Value, base: &Path) -> Value {
    let relative = match op.get("path").and_then(Value::as_str) {
        Some(path) => path,
        None => return failed("create_dir requires a 'path' field"),
    };
    let target = base.join(relative);

    if target.exists() {
        return failed(format!(
            "'{}' already exists — refusing to overwrite it",
            target.display()
        ));
    }
    if let Err(err) = fs::create_dir_all(&target) {
        return failed(format!("failed to create '{}': {err}", target.display()));
    }

    let label = op.get("label").and_then(Value::as_str);
    if let Some(label) = label {
        if let Err(err) = fs::write(target.join("label.txt"), label) {
            return failed(format!("failed to write label.txt: {err}"));
        }
    }

    let target = canonicalize_or_keep(&target);
    json!({
        "status": "passed",
        "summary": format!("created {}", target.display()),
        "data": {
            "path": target.to_string_lossy(),
            "label": label,
        },
    })
}

fn op_list_entries(op: &Value, base: &Path) -> Value {
    let dir = match op.get("path").and_then(Value::as_str) {
        Some(relative) => base.join(relative),
        None => base.to_path_buf(),
    };

    let read = match fs::read_dir(&dir) {
        Ok(read) => read,
        Err(err) => return failed(format!("failed to read '{}': {err}", dir.display())),
    };
    let mut entries: Vec<String> = read
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    entries.sort();

    json!({
        "status": "passed",
        "summary": format!("listed {} entries in {}", entries.len(), dir.display()),
        "data": {
            "dir": canonicalize_or_keep(&dir).to_string_lossy(),
            "entries": entries,
        },
    })
}

/// A probe's outcome as a passed result: `{ok: true, ..fields}` or `{ok: false, error}`.
fn probe(outcome: Result<Value, String>) -> Value {
    let data = match outcome {
        Ok(Value::Object(mut fields)) => {
            fields.insert("ok".to_string(), Value::Bool(true));
            Value::Object(fields)
        }
        Ok(_) => json!({"ok": true}),
        Err(error) => json!({"ok": false, "error": error}),
    };
    json!({"status": "passed", "summary": "probe ran", "data": data})
}

fn required_str<'a>(op: &'a Value, field: &str) -> Result<&'a str, String> {
    op.get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing '{field}'"))
}

fn op_read(op: &Value) -> Result<Value, String> {
    let path = required_str(op, "path")?;
    let contents = fs::read_to_string(path).map_err(|err| err.to_string())?;
    Ok(json!({"contents": contents}))
}

fn op_write(op: &Value) -> Result<Value, String> {
    let path = required_str(op, "path")?;
    let text = op.get("text").and_then(Value::as_str).unwrap_or_default();
    fs::write(path, text).map_err(|err| err.to_string())?;
    Ok(json!({}))
}

fn op_connect(op: &Value) -> Result<Value, String> {
    let host = required_str(op, "host")?;
    let port = op
        .get("port")
        .and_then(Value::as_u64)
        .and_then(|port| u16::try_from(port).ok())
        .ok_or("missing 'port'")?;
    let address = (host, port)
        .to_socket_addrs()
        .map_err(|err| err.to_string())?
        .next()
        .ok_or_else(|| format!("'{host}' resolved to no address"))?;
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(5))
        .map_err(|err| err.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|err| err.to_string())?;
    if let Some(send) = op.get("send").and_then(Value::as_str) {
        stream
            .write_all(send.as_bytes())
            .map_err(|err| err.to_string())?;
    }
    let mut reply = String::new();
    stream
        .read_to_string(&mut reply)
        .map_err(|err| err.to_string())?;
    Ok(json!({"reply": reply}))
}

fn op_unix_socket() -> Result<Value, String> {
    std::os::unix::net::UnixDatagram::unbound().map_err(|err| err.to_string())?;
    Ok(json!({}))
}

fn op_exec(op: &Value) -> Result<Value, String> {
    let path = required_str(op, "path")?;
    let args: Vec<&str> = op
        .get("args")
        .and_then(Value::as_array)
        .map(|args| args.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let output = std::process::Command::new(path)
        .args(args)
        .output()
        .map_err(|err| err.to_string())?;
    Ok(json!({
        "code": output.status.code(),
        "stdout": String::from_utf8_lossy(&output.stdout),
    }))
}

fn op_netns() -> Result<Value, String> {
    let link = fs::read_link("/proc/self/ns/net").map_err(|err| err.to_string())?;
    Ok(json!({"netns": link.to_string_lossy()}))
}

/// `RLIMIT_NOFILE` in `<sys/resource.h>`, the same on Linux x86_64 and aarch64.
const RLIMIT_NOFILE: i32 = 7;

/// `struct rlimit` on 64-bit Linux: two `rlim_t` (`u64`) fields.
#[repr(C)]
struct Rlimit {
    cur: u64,
    max: u64,
}

extern "C" {
    fn getrlimit(resource: i32, rlim: *mut Rlimit) -> i32;
}

/// The process's own descriptor bound, read with `getrlimit` rather than from `/proc/self/limits`,
/// which Landlock may not grant the tool.
fn op_rlimit_nofile() -> Result<Value, String> {
    let mut limit = Rlimit { cur: 0, max: 0 };
    // SAFETY: `limit` is a live, writable `struct rlimit` with the C layout `getrlimit` fills, and
    // the call writes nothing beyond it.
    let rc = unsafe { getrlimit(RLIMIT_NOFILE, &mut limit) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(json!({"soft": limit.cur, "hard": limit.max}))
}

/// Opens `/dev/null` until an open fails or `max` descriptors are held, all of them kept open
/// until the count is taken.
fn op_open_until_refused(op: &Value) -> Value {
    let max = op.get("max").and_then(Value::as_u64).unwrap_or(200);
    let mut held = Vec::new();
    let mut error = None;
    while (held.len() as u64) < max {
        match fs::File::open("/dev/null") {
            Ok(file) => held.push(file),
            Err(err) => {
                error = Some(err.to_string());
                break;
            }
        }
    }
    json!({"opened": held.len(), "error": error})
}

fn failed(summary: impl Into<String>) -> Value {
    json!({
        "status": "failed",
        "summary": summary.into(),
        "data": Value::Null,
    })
}
