//! Hand-authored WAT hook components, compiled in-test.
//!
//! Every hook fixture in this suite is built here rather than pulled from a `default-artifacts`
//! checkout, so no test that drives a lifecycle arm is `#[ignore]`d on a bare host.

use std::{
    fs,
    path::{Path, PathBuf},
};

/// Interface the host links hooks against.
const LIFECYCLE_IFACE: &str = "murmur:hook/lifecycle@0.6.0";

/// Every lifecycle export a hook component must carry to instantiate.
const HOOK_FNS: [&str; 8] = [
    "on-session-start",
    "on-task-start",
    "on-inference",
    "on-tool-call",
    "on-shell",
    "on-compaction",
    "on-task-end",
    "on-session-end",
];

/// Where the lifted `result<hook-output, string>` return area sits in guest memory.
const RETURN_AREA: u32 = 128;
/// Where the `list<message>` records sit. One record is 40 bytes.
const MESSAGE_RECORDS: u32 = 256;
/// Where the string bytes the records point at sit.
const STRING_POOL: u32 = 1024;

/// Encode `bytes` as a WAT data-segment string literal.
pub fn wat_data(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("\\{b:02x}")).collect()
}

pub fn le(value: u32, out: &mut Vec<u8>) {
    out.extend_from_slice(&value.to_le_bytes());
}

/// Lay out `messages` as a canonical-ABI `list<message>` plus its string pool.
///
/// A `message` is 40 bytes: `role` ptr/len, `content` ptr/len, then the two
/// `option<string>` fields as discriminant + ptr + len each. Both options are `none`, so the
/// runtime is the only thing that ever puts an `id` on these.
pub fn message_list(messages: &[(&str, &str)]) -> (Vec<u8>, Vec<u8>) {
    let mut records = Vec::new();
    let mut pool = Vec::new();
    for (role, content) in messages {
        let role_ptr = STRING_POOL + pool.len() as u32;
        pool.extend_from_slice(role.as_bytes());
        let content_ptr = STRING_POOL + pool.len() as u32;
        pool.extend_from_slice(content.as_bytes());

        le(role_ptr, &mut records);
        le(role.len() as u32, &mut records);
        le(content_ptr, &mut records);
        le(content.len() as u32, &mut records);
        for _ in 0..6 {
            le(0, &mut records);
        }
    }
    (records, pool)
}

/// A hook component that implements exactly one lifecycle function and stubs the rest.
///
/// `arm_disc` selects the returned `hook-output` case — `0` = `none`, `1` = `replace-context`,
/// `5` = `seed-context` — and `messages` is the list that case carries. `core_params` is the
/// canonical flat lowering of the implemented function's event record; the body ignores it and
/// returns the statically laid out result area.
pub fn hook_component(
    fn_name: &str,
    core_params: &str,
    event_type: &str,
    event_type_name: &str,
    arm_disc: u32,
    messages: &[(&str, &str)],
) -> Vec<u8> {
    let (records, pool) = message_list(messages);
    let mut ret = Vec::new();
    le(0, &mut ret); // result: ok
    le(arm_disc, &mut ret);
    le(MESSAGE_RECORDS, &mut ret);
    le(messages.len() as u32, &mut ret);

    let stubs = HOOK_FNS
        .iter()
        .filter(|n| **n != fn_name)
        .map(|n| format!("    (export \"{n}\" (func $noop))"))
        .collect::<Vec<_>>()
        .join("\n");

    let wat = format!(
        r#"(component
  (core module $m
    (memory (export "memory") 4)
    ;; Bump allocator over the upper half of memory, so the strings the host lowers into
    ;; the guest never land on the statically laid out result area below.
    (global $bump (mut i32) (i32.const 65536))
    (data (i32.const {RETURN_AREA}) "{ret}")
    (data (i32.const {MESSAGE_RECORDS}) "{records}")
    (data (i32.const {STRING_POOL}) "{pool}")
    (func (export "realloc") (param $old i32) (param $oldsz i32) (param $align i32) (param $newsz i32) (result i32)
      (local $p i32)
      (global.set $bump (i32.and (i32.add (global.get $bump) (i32.const 7)) (i32.const -8)))
      (local.set $p (global.get $bump))
      (global.set $bump (i32.add (local.get $p) (local.get $newsz)))
      (local.get $p))
    (func (export "handler") {core_params} (result i32) (i32.const {RETURN_AREA}))
    (func (export "noop"))
  )
  (core instance $i (instantiate $m))
  (alias core export $i "memory" (core memory $mem))
  (alias core export $i "realloc" (core func $realloc))

  (type $message (record
    (field "role" string)
    (field "content" string)
    (field "id" (option string))
    (field "source-id" (option string))))
  (type $tool-manifest (record (field "binary-name" string) (field "content" string)))
  (type $hook-output (variant
    (case "none")
    (case "replace-context" (list $message))
    (case "write-manifests" (list $tool-manifest))
    (case "artifact" string)
    (case "reopen-task" string)
    (case "seed-context" (list $message))))
{event_type}
  (type $ft (func (param "event" $event) (result (result $hook-output (error string)))))

  (func $impl (type $ft)
    (canon lift (core func $i "handler") (memory $mem) (realloc $realloc) string-encoding=utf8))
  (func $noop (canon lift (core func $i "noop")))

  (instance $lc
    (export "message" (type $message))
    (export "tool-manifest" (type $tool-manifest))
    (export "hook-output" (type $hook-output))
    (export "{event_type_name}" (type $event))
    (export "{fn_name}" (func $impl))
{stubs}
  )
  (export "{LIFECYCLE_IFACE}" (instance $lc))
)"#,
        ret = wat_data(&ret),
        records = wat_data(&records),
        pool = wat_data(&pool),
    );
    wat::parse_str(&wat).expect("hook component WAT parses")
}

/// An `on-task-start` hook returning `seed-context(messages)`, or `none` when `messages` is
/// empty.
pub fn seed_hook_wasm(messages: &[(&str, &str)]) -> Vec<u8> {
    let task_start_event = r#"  (type $event (record
    (field "task-id" string)
    (field "context-id" string)
    (field "source" string)
    (field "input-bytes" u64)
    (field "budget-tokens" u64)
    (field "context-window" u64)
    (field "prior-tokens" u64)))"#;
    hook_component(
        "on-task-start",
        "(param i32 i32 i32 i32 i32 i32 i64 i64 i64 i64)",
        task_start_event,
        "task-start-event",
        if messages.is_empty() { 0 } else { 5 },
        messages,
    )
}

/// An `on-compaction` hook returning `replace-context([summary])`.
pub fn compaction_hook_wasm(summary: &str) -> Vec<u8> {
    let compaction_event = r#"  (type $event (record
    (field "messages" (list $message))
    (field "session-tokens" u64)
    (field "threshold" f64)
    (field "model" (option string))
    (field "system-prompt" (option string))))"#;
    hook_component(
        "on-compaction",
        "(param i32 i32 i64 f64 i32 i32 i32 i32 i32 i32)",
        compaction_event,
        "compaction-event",
        1,
        &[("user", summary)],
    )
}

/// Pack a hook `.mur.zip` whose bundled manifest declares the binding and commit policy the
/// runtime cross-checks at staging.
pub fn create_hook_zip(
    dir: &Path,
    name: &str,
    binding: &str,
    commit_policy: &str,
    wasm: &[u8],
) -> PathBuf {
    use std::io::Write;
    use zip::{write::SimpleFileOptions, CompressionMethod, ZipWriter};

    let artifact_path = dir.join(format!("{name}-0.1.0.mur.zip"));
    let file = fs::File::create(&artifact_path).unwrap();
    let mut zip = ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    zip.start_file("murmur.yaml", options).unwrap();
    writeln!(zip, "name: {name}").unwrap();
    writeln!(zip, "version: 0.1.0").unwrap();
    writeln!(zip, "runtime: hook").unwrap();
    writeln!(zip, "binding: {binding}").unwrap();
    writeln!(zip, "commit_policy: {commit_policy}").unwrap();

    zip.start_file("hook.wasm", options).unwrap();
    zip.write_all(wasm).unwrap();

    zip.finish().unwrap();
    artifact_path
}
