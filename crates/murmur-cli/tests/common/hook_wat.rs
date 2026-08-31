//! Hand-authored WAT hook components, compiled in-test.
//!
//! Every hook fixture in this suite is built here rather than pulled from a `default-artifacts`
//! checkout, so no test that drives a lifecycle arm is `#[ignore]`d on a bare host.

use std::{
    fs,
    path::{Path, PathBuf},
};

/// Interface the host links hooks against.
const LIFECYCLE_IFACE: &str = "murmur:hook/lifecycle@0.7.0";

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
    (case "seed-context" (list $message))
    (case "deny" string)))
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

// ── Policy hooks ──────────────────────────────────────────────────────────────

/// Where a policy hook's static reason string sits in guest memory.
const REASON_POOL: u32 = 512;
/// Where a policy hook assembles a reason out of the event it was handed.
const SCRATCH: u32 = 4096;

/// The seven-case `hook-output` of `murmur:hook/lifecycle@0.7.0`. `deny` is discriminant 6.
const HOOK_OUTPUT: &str = r#"  (type $hook-output (variant
    (case "none")
    (case "replace-context" (list $message))
    (case "write-manifests" (list $tool-manifest))
    (case "artifact" string)
    (case "reopen-task" string)
    (case "seed-context" (list $message))
    (case "deny" string)))"#;

/// The `hook-output` as it stood at `@0.6.0`, one case short. A component built against it
/// instantiates and then fails to lift at the call, which is what makes it the fixture for
/// "the hook returned something the host cannot read".
const HOOK_OUTPUT_UNREADABLE: &str = r#"  (type $hook-output (variant
    (case "none")
    (case "replace-context" (list $message))
    (case "write-manifests" (list $tool-manifest))
    (case "artifact" string)
    (case "reopen-task" string)
    (case "seed-context" (list $message))))"#;

/// The WIT declarations and core signature of one lifecycle event's record.
struct EventShape {
    /// Core params the canonical ABI flattens this event's record to. `shell-event` (19
    /// values) and `inference-event` (17) both exceed the 16-value flat limit, so each
    /// arrives as one pointer into guest memory.
    params: &'static str,
    /// The event's WIT type declarations, with `$event` last.
    decls: &'static str,
    /// The instance type exports those declarations require.
    type_exports: &'static str,
}

fn event_shape(fn_name: &str) -> EventShape {
    match fn_name {
        "on-shell" => EventShape {
            params: "(param $event i32)",
            decls: r#"  (type $shell-outcome (record
    (field "exit-code" s32)
    (field "stdout" string)
    (field "stderr" string)
    (field "stdout-bytes" u64)
    (field "stderr-bytes" u64)
    (field "duration-ms" u64)))
  (type $event (record
    (field "turn" u32)
    (field "binary" string)
    (field "command" string)
    (field "argv" (list string))
    (field "script" (option string))
    (field "outcome" (option $shell-outcome))))"#,
            type_exports: "    (export \"shell-outcome\" (type $shell-outcome))\n    (export \"shell-event\" (type $event))",
        },
        "on-tool-call" => EventShape {
            params: "(param $turn i32) (param $namep i32) (param $namelen i32) \
                     (param $inbytes i64) (param $inputp i32) (param $inputlen i32) \
                     (param $odisc i32) (param $obytes i64) (param $oms i64) \
                     (param $statusp i32) (param $statuslen i32)",
            decls: r#"  (type $tool-outcome (record
    (field "output-bytes" u64)
    (field "duration-ms" u64)
    (field "status" string)))
  (type $event (record
    (field "turn" u32)
    (field "tool-name" string)
    (field "input-bytes" u64)
    (field "input" string)
    (field "outcome" (option $tool-outcome))))"#,
            type_exports: "    (export \"tool-outcome\" (type $tool-outcome))\n    (export \"tool-event\" (type $event))",
        },
        "on-inference" => EventShape {
            params: "(param $event i32)",
            decls: r#"  (type $event (record
    (field "turn" u32)
    (field "input-tokens" u64)
    (field "output-tokens" u64)
    (field "decision" string)
    (field "tool-name" (option string))
    (field "prompt" (option string))
    (field "output" (option string))
    (field "tools" (option string))))"#,
            type_exports: "    (export \"inference-event\" (type $event))",
        },
        other => panic!("no event shape for {other}"),
    }
}

/// A hook component implementing exactly one lifecycle function with `body` as its core
/// function body, and stubbing the rest.
///
/// `body` is raw WAT: it may declare locals, must leave one `i32` on the stack, and reaches
/// the return area at [`RETURN_AREA`], a static reason string at [`REASON_POOL`] and a scratch
/// buffer at [`SCRATCH`]. `reason` is laid down at `REASON_POOL` for a body that returns a
/// fixed reason; pass `""` for a body that does not.
fn policy_component(fn_name: &str, hook_output: &str, reason: &str, body: &str) -> Vec<u8> {
    let shape = event_shape(fn_name);
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
    ;; Bump allocator over the upper half of memory, so what the host lowers into the guest
    ;; never lands on the statically laid out areas below.
    (global $bump (mut i32) (i32.const 65536))
    ;; Write cursor into the scratch buffer, for a body that assembles its own reason.
    (global $n (mut i32) (i32.const 0))
    (data (i32.const {REASON_POOL}) "{reason}")
    (func (export "realloc") (param $old i32) (param $oldsz i32) (param $align i32) (param $newsz i32) (result i32)
      (local $p i32)
      (global.set $bump (i32.and (i32.add (global.get $bump) (i32.const 7)) (i32.const -8)))
      (local.set $p (global.get $bump))
      (global.set $bump (i32.add (local.get $p) (local.get $newsz)))
      (local.get $p))
    (func $app (param $p i32) (param $l i32)
      (memory.copy (i32.add (i32.const {SCRATCH}) (global.get $n)) (local.get $p) (local.get $l))
      (global.set $n (i32.add (global.get $n) (local.get $l))))
    (func $ch (param $c i32)
      (i32.store8 (i32.add (i32.const {SCRATCH}) (global.get $n)) (local.get $c))
      (global.set $n (i32.add (global.get $n) (i32.const 1))))
    (func (export "handler") {params} (result i32)
{body})
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
{hook_output}
{decls}
  (type $ft (func (param "event" $event) (result (result $hook-output (error string)))))

  (func $impl (type $ft)
    (canon lift (core func $i "handler") (memory $mem) (realloc $realloc) string-encoding=utf8))
  (func $noop (canon lift (core func $i "noop")))

  (instance $lc
    (export "message" (type $message))
    (export "tool-manifest" (type $tool-manifest))
    (export "hook-output" (type $hook-output))
{type_exports}
    (export "{fn_name}" (func $impl))
{stubs}
  )
  (export "{LIFECYCLE_IFACE}" (instance $lc))
)"#,
        reason = wat_data(reason.as_bytes()),
        params = shape.params,
        decls = shape.decls,
        type_exports = shape.type_exports,
    );
    wat::parse_str(&wat).expect("policy hook component WAT parses")
}

/// The WAT that returns `ok(<arm_disc>(REASON_POOL, len))` from the return area.
fn return_string_arm(arm_disc: u32, len: usize) -> String {
    format!(
        "      (i32.store (i32.const {RETURN_AREA}) (i32.const 0))\n      \
         (i32.store (i32.const {}) (i32.const {arm_disc}))\n      \
         (i32.store (i32.const {}) (i32.const {REASON_POOL}))\n      \
         (i32.store (i32.const {}) (i32.const {len}))\n      \
         (i32.const {RETURN_AREA})",
        RETURN_AREA + 4,
        RETURN_AREA + 8,
        RETURN_AREA + 12,
    )
}

/// A hook whose `fn_name` returns `deny(reason)`. With `reason: ""` this is the empty-reason
/// case the runtime refuses with a reason of its own.
pub fn deny_hook_wasm(fn_name: &str, reason: &str) -> Vec<u8> {
    policy_component(
        fn_name,
        HOOK_OUTPUT,
        reason,
        &return_string_arm(6, reason.len()),
    )
}

/// A hook whose `fn_name` returns `artifact(payload)` — an arm no decision point honors.
pub fn artifact_hook_wasm(fn_name: &str, payload: &str) -> Vec<u8> {
    policy_component(
        fn_name,
        HOOK_OUTPUT,
        payload,
        &return_string_arm(3, payload.len()),
    )
}

/// A hook whose `fn_name` returns `none`.
pub fn none_hook_wasm(fn_name: &str) -> Vec<u8> {
    policy_component(
        fn_name,
        HOOK_OUTPUT,
        "",
        &format!(
            "      (i32.store (i32.const {RETURN_AREA}) (i32.const 0))\n      \
             (i32.store (i32.const {}) (i32.const 0))\n      (i32.const {RETURN_AREA})",
            RETURN_AREA + 4
        ),
    )
}

/// A hook whose `fn_name` traps.
pub fn trap_hook_wasm(fn_name: &str) -> Vec<u8> {
    policy_component(fn_name, HOOK_OUTPUT, "", "      unreachable")
}

/// A hook whose `fn_name` never returns. The epoch deadline is what ends the call.
pub fn spin_hook_wasm(fn_name: &str) -> Vec<u8> {
    policy_component(
        fn_name,
        HOOK_OUTPUT,
        "",
        "      (block $never (loop $l (br $l)))\n      (i32.const 0)",
    )
}

/// A hook whose `fn_name` returns `none` against a `hook-output` the host cannot read: the
/// six-case variant of `@0.6.0`. The typed call fails at the lift.
pub fn unreadable_output_hook_wasm(fn_name: &str) -> Vec<u8> {
    policy_component(
        fn_name,
        HOOK_OUTPUT_UNREADABLE,
        "",
        &format!(
            "      (i32.store (i32.const {RETURN_AREA}) (i32.const 0))\n      \
             (i32.store (i32.const {}) (i32.const 0))\n      (i32.const {RETURN_AREA})",
            RETURN_AREA + 4
        ),
    )
}

/// An `on-shell` policy hook that denies with the identity it was handed:
/// `<binary>|<script>|<argv joined by spaces>`.
///
/// `shell-event` arrives indirectly, so the record is read out of guest memory at the
/// canonical-ABI offsets: `binary` ptr/len 4/8, `argv` ptr/count 20/24, `script`
/// discriminant/ptr/len 28/32/36.
pub fn shell_echo_deny_hook_wasm() -> Vec<u8> {
    let body = format!(
        r#"      (local $i i32)
      (local $e i32)
      (call $app
        (i32.load (i32.add (local.get $event) (i32.const 4)))
        (i32.load (i32.add (local.get $event) (i32.const 8))))
      (call $ch (i32.const 124))
      (if (i32.eq (i32.load (i32.add (local.get $event) (i32.const 28))) (i32.const 1))
        (then (call $app
                (i32.load (i32.add (local.get $event) (i32.const 32)))
                (i32.load (i32.add (local.get $event) (i32.const 36))))))
      (call $ch (i32.const 124))
      (block $done
        (loop $l
          (br_if $done
            (i32.ge_u (local.get $i) (i32.load (i32.add (local.get $event) (i32.const 24)))))
          (local.set $e
            (i32.add
              (i32.load (i32.add (local.get $event) (i32.const 20)))
              (i32.mul (local.get $i) (i32.const 8))))
          (call $app (i32.load (local.get $e)) (i32.load (i32.add (local.get $e) (i32.const 4))))
          (call $ch (i32.const 32))
          (local.set $i (i32.add (local.get $i) (i32.const 1)))
          (br $l)))
      (i32.store (i32.const {RETURN_AREA}) (i32.const 0))
      (i32.store (i32.const {disc}) (i32.const 6))
      (i32.store (i32.const {ptr}) (i32.const {SCRATCH}))
      (i32.store (i32.const {len}) (global.get $n))
      (i32.const {RETURN_AREA})"#,
        disc = RETURN_AREA + 4,
        ptr = RETURN_AREA + 8,
        len = RETURN_AREA + 12,
    );
    policy_component("on-shell", HOOK_OUTPUT, "", &body)
}
