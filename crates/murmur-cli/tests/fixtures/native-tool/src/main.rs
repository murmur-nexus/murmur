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

use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

use serde_json::{json, Value};

/// Colon-separated list of directory prefixes this tool may operate under.
///
/// Same name and same self-enforcement pattern the real native tools use: the
/// capsule runtime does not police path arguments, so a native tool that wants a
/// filesystem boundary has to check one itself.
const ALLOW_ENV: &str = "MURMUR_FILESYSTEM_ALLOW";

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
            let _ = writeln!(file, "murmur-tool-fixture: {operation}");
        }
    }

    let base = match resolve_base(&op) {
        Ok(base) => base,
        Err(message) => return failed(message),
    };

    match op.get("operation").and_then(Value::as_str).unwrap_or("") {
        "create_dir" => op_create_dir(&op, &base),
        "list_entries" => op_list_entries(&op, &base),
        other => failed(format!("unknown operation '{other}'")),
    }
}

/// Resolve the base directory the operation runs against, then validate it
/// against `MURMUR_FILESYSTEM_ALLOW`.
///
/// The base is the explicit `repo` field when given, and the process working
/// directory (the capsule workdir, under the runtime) otherwise.
fn resolve_base(op: &Value) -> Result<PathBuf, String> {
    let base = match op.get("repo").and_then(Value::as_str) {
        Some(repo) => PathBuf::from(repo),
        None => std::env::current_dir().map_err(|err| format!("cannot read working dir: {err}"))?,
    };

    let allow = match std::env::var(ALLOW_ENV) {
        Ok(value) => value,
        Err(_) => return Ok(base),
    };
    let prefixes: Vec<&str> = allow.split(':').filter(|part| !part.is_empty()).collect();
    if prefixes.is_empty() {
        return Ok(base);
    }

    // Compare canonical paths component-wise: a textual prefix test would let
    // `/tmp/ab` stand in for `/tmp/abc`.
    let canonical = canonicalize_or_keep(&base);
    let permitted = prefixes
        .iter()
        .any(|prefix| canonical.starts_with(canonicalize_or_keep(Path::new(prefix))));

    if permitted {
        Ok(base)
    } else {
        Err(format!(
            "repo path '{}' is not within any allowed filesystem path. \
             Add the path to capabilities.filesystem.allow in the manifest.",
            base.display()
        ))
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

fn failed(summary: impl Into<String>) -> Value {
    json!({
        "status": "failed",
        "summary": summary.into(),
        "data": Value::Null,
    })
}
