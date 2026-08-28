//! End-to-end coverage for `seed-context`: what an `on-task-start` hook proposes, what the
//! runtime commits, and what `trace.jsonl` records about the decision.
//!
//! The hook components are hand-authored WAT compiled in-test, so nothing here depends on a
//! `default-artifacts` checkout and no case is `#[ignore]`d.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    path::{Path, PathBuf},
};

use capsule_runtime::launch_session;
use serde_json::{json, Value};

const DRIVER_NAME: &str = "murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";

/// The task the capsule is given, asserted verbatim as the message a seed must precede.
const TASK_TEXT: &str = "Report what you remember.";

/// The one word every generated seed message is padded with. One cl100k token each, so a
/// message's size is a function of how many of them it carries.
const PAD_WORD: &str = "word ";

// ── WAT hook components ──────────────────────────────────────────────────────

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
fn wat_data(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("\\{b:02x}")).collect()
}

fn le(value: u32, out: &mut Vec<u8>) {
    out.extend_from_slice(&value.to_le_bytes());
}

/// Lay out `messages` as a canonical-ABI `list<message>` plus its string pool.
///
/// A `message` is 40 bytes: `role` ptr/len, `content` ptr/len, then the two
/// `option<string>` fields as discriminant + ptr + len each. Both options are `none`, so the
/// runtime is the only thing that ever puts an `id` on these.
fn message_list(messages: &[(&str, &str)]) -> (Vec<u8>, Vec<u8>) {
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
fn hook_component(
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
fn seed_hook_wasm(messages: &[(&str, &str)]) -> Vec<u8> {
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
fn compaction_hook_wasm(summary: &str) -> Vec<u8> {
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

// ── Fixtures ─────────────────────────────────────────────────────────────────

/// Pack a hook `.mur.zip` whose bundled manifest declares the binding and commit policy the
/// runtime cross-checks at staging.
fn create_hook_zip(
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

/// Two-turn-free driver script: one `end_turn`, so the session runs exactly one request.
fn one_turn_responses() -> Vec<String> {
    vec![json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": "Done."}],
        "stop_reason": "end_turn",
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": 10, "output_tokens": 5}
    })
    .to_string()]
}

/// A capsule manifest declaring the driver, the given hooks, and a `context:` block.
fn create_manifest(
    project_dir: &Path,
    endpoint: &str,
    seed_budget: &str,
    hook_names: &[&str],
) -> PathBuf {
    fs::write(
        project_dir.join("murmur.yaml"),
        "registry:\n  default: local\n",
    )
    .unwrap();

    let hooks: String = hook_names
        .iter()
        .map(|name| format!("  - name: {name}\n    version: 0.1.0\n    runtime: hook\n"))
        .collect();

    let manifest = format!(
        concat!(
            "name: seed-capsule\n",
            "version: 0.1.0\n",
            "context:\n",
            // Wide enough that the compaction threshold is never crossed during the run, so
            // any `compaction` line in the trace can only have come from the seed path.
            "  max_tokens: 200000\n",
            "  seed_budget: {seed_budget}\n",
            "  seed_overflow_margin: 0.10\n",
            "artifacts:\n",
            "  - name: {driver_name}\n",
            "    version: {driver_version}\n",
            "    runtime: driver\n",
            "{hooks}",
            "capabilities:\n",
            "  network:\n",
            "    allow:\n",
            "      - {endpoint}\n",
            "inference:\n",
            "  transport: http\n",
            "  endpoint: {endpoint}\n",
            "  model: test-model\n",
            "  api_key: test-key\n",
            "  driver:\n",
            "    artifact: {driver_name}\n",
        ),
        seed_budget = seed_budget,
        driver_name = DRIVER_NAME,
        driver_version = DRIVER_VERSION,
        hooks = hooks,
        endpoint = endpoint,
    );

    fs::write(project_dir.join("murmur.yaml"), manifest).unwrap();
    project_dir.join("murmur.yaml")
}

/// What one launched session left behind.
struct Session {
    requests: Vec<Value>,
    trace: Vec<Value>,
    trace_raw: String,
}

impl Session {
    fn first_messages(&self) -> &Vec<Value> {
        self.requests
            .first()
            .expect("the driver must have been called at least once")["messages"]
            .as_array()
            .expect("a request carries a messages array")
    }

    fn events(&self, event_type: &str) -> Vec<&Value> {
        self.trace
            .iter()
            .filter(|e| e["event_type"] == event_type)
            .collect()
    }

    /// The one `context_seed` line, with the whole trace in the failure message — a seed
    /// decision that did not get recorded is the defect these tests exist to catch.
    fn context_seed(&self) -> &Value {
        let seeds = self.events("context_seed");
        assert_eq!(
            seeds.len(),
            1,
            "expected exactly one context_seed line; trace was:\n{}",
            self.trace_raw
        );
        seeds[0]
    }
}

/// Publish the driver and every hook, stage the capsule, run it, and collect what it left.
fn run_session(seed_budget: &str, hooks: &[(&str, &str, &str, Vec<u8>)]) -> Session {
    let server = common::ScriptedServer::start(one_turn_responses());
    let home = tempfile::tempdir().unwrap();
    let artifact_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let driver_artifact = common::create_driver_artifact(
        artifact_dir.path(),
        DRIVER_NAME,
        DRIVER_VERSION,
        &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver_artifact).success();

    let mut hook_names = Vec::new();
    for (name, binding, commit_policy, wasm) in hooks {
        let artifact = create_hook_zip(artifact_dir.path(), name, binding, commit_policy, wasm);
        common::publish_local(&home, &artifact).success();
        hook_names.push(*name);
    }

    let manifest_path = create_manifest(project.path(), &server.endpoint, seed_budget, &hook_names);
    let staged = common::stage_agent_session(&home, project.path(), &manifest_path);
    let workdir = staged.workdir.clone();
    fs::write(workdir.join("task.md"), TASK_TEXT).unwrap();

    launch_session(staged, |_| {}).expect("the session must launch whatever the seed does");

    let trace_raw = fs::read_to_string(workdir.join("trace.jsonl")).unwrap_or_default();
    let trace: Vec<Value> = trace_raw
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).expect("every trace line must be valid JSON"))
        .collect();

    Session {
        requests: server.requests(),
        trace,
        trace_raw,
    }
}

/// A seed message of roughly `tokens` cl100k tokens, tagged so the assertions can tell one
/// from another in the request body.
fn seed_text(label: &str, tokens: usize) -> String {
    format!("{label} {}", PAD_WORD.repeat(tokens))
}

fn text_of(message: &Value) -> String {
    message["content"][0]["text"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

/// The user message the task itself contributes, in full — what a message list with no seed
/// consists of, and what a seed must sit ahead of.
fn task_message() -> Value {
    json!({"role": "user", "content": [{"type": "text", "text": TASK_TEXT}]})
}

// ── Scenarios ────────────────────────────────────────────────────────────────

/// A seeding hook's messages reach the first inference request, ahead of the task, and the
/// trace records what was committed.
#[test]
fn seeds_the_first_inference_request() {
    if common::skip_without_host_support("seeds_the_first_inference_request") {
        return;
    }
    let known = "REMEMBER-THIS the operator prefers terse answers.";
    let session = run_session(
        "0.10",
        &[(
            "seed-hook",
            "on-task-start",
            "seed-context",
            seed_hook_wasm(&[("user", known)]),
        )],
    );

    let messages = session.first_messages();
    assert_eq!(messages.len(), 2, "seed then task: {messages:?}");
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(text_of(&messages[0]), known);
    assert_eq!(messages[1], task_message());

    let seed = session.context_seed();
    assert_eq!(seed["hook_name"], "seed-hook");
    assert_eq!(seed["outcome"], "seeded");
    assert!(seed["reason"].is_null(), "a committed seed names no reason");
    assert!(
        seed["tokens"].as_u64().is_some_and(|t| t > 0),
        "a committed seed carries its token count: {seed}"
    );
    assert_eq!(seed["proposed_tokens"], seed["tokens"]);
    assert_eq!(seed["budget_tokens"], 20_000);
    let ids = seed["message_ids"].as_array().unwrap();
    assert_eq!(ids.len(), 1);
    assert!(
        ids[0].as_str().unwrap().starts_with("msg_"),
        "message ids are msg_-prefixed: {seed}"
    );

    assert!(
        !session
            .events("hook_dispatch_error")
            .iter()
            .any(|e| e["arm"] == "seed-context"),
        "an honored arm must not also be reported as a fault: {}",
        session.trace_raw
    );
}

/// A capsule with no seeding hook — and one whose bound hook returns `none` — puts exactly
/// the task message in the request and records no seed.
#[test]
fn inert_without_a_seeding_hook() {
    if common::skip_without_host_support("inert_without_a_seeding_hook") {
        return;
    }
    for hooks in [
        Vec::new(),
        vec![(
            "silent-hook",
            "on-task-start",
            "seed-context",
            seed_hook_wasm(&[]),
        )],
    ] {
        let session = run_session("0.10", &hooks);

        assert_eq!(
            session.first_messages(),
            &vec![task_message()],
            "the message list must be exactly the task message"
        );
        assert!(
            session.events("context_seed").is_empty(),
            "nothing was proposed, so nothing is recorded: {}",
            session.trace_raw
        );

        let wire = serde_json::to_string(&session.requests).unwrap();
        assert!(!wire.contains("source_id"), "{wire}");
        for message in session.first_messages() {
            assert!(message.get("id").is_none(), "{message}");
            assert!(message.get("source_id").is_none(), "{message}");
        }
    }
}

/// An overflow inside the margin is trimmed from the front: the newest messages reach the
/// request and the oldest does not.
#[test]
fn trimmed_seed_reaches_the_request() {
    if common::skip_without_host_support("trimmed_seed_reaches_the_request") {
        return;
    }
    let oldest = seed_text("SEED-OLDEST", 100);
    let middle = seed_text("SEED-MIDDLE", 100);
    let newest = seed_text("SEED-NEWEST", 100);
    // Three ~122-token messages — 365 tokens all told — against a 350-token budget: 15 over,
    // inside the 35 the 10% margin allows, so the front is dropped rather than summarized.
    let session = run_session(
        "0.00175",
        &[(
            "seed-hook",
            "on-task-start",
            "seed-context",
            seed_hook_wasm(&[
                ("user", oldest.as_str()),
                ("user", middle.as_str()),
                ("user", newest.as_str()),
            ]),
        )],
    );

    let seed = session.context_seed();
    assert_eq!(seed["outcome"], "trimmed", "{seed}");
    assert!(seed["tokens"].as_u64().unwrap() < seed["proposed_tokens"].as_u64().unwrap());

    let messages = session.first_messages();
    let texts: Vec<String> = messages.iter().map(text_of).collect();
    assert!(
        texts.iter().any(|t| t.starts_with("SEED-NEWEST")),
        "the newest must survive: {texts:?}"
    );
    assert!(
        !texts.iter().any(|t| t.starts_with("SEED-OLDEST")),
        "the oldest is what gets dropped: {texts:?}"
    );
    assert_eq!(messages.last().unwrap(), &task_message());
    assert_eq!(
        seed["message_ids"].as_array().unwrap().len(),
        messages.len() - 1
    );
}

/// Past the margin the overflowing front goes to the compaction hook, and its summary leads
/// the seed. Nothing about the session's own context was compacted, so no `compaction` line
/// is written.
#[test]
fn overflow_over_margin_is_compacted() {
    if common::skip_without_host_support("overflow_over_margin_is_compacted") {
        return;
    }
    let summary = "SUMMARY-OF-THE-FRONT: two earlier notes.";
    let oldest = seed_text("SEED-OLDEST", 100);
    let middle = seed_text("SEED-MIDDLE", 100);
    let newest = seed_text("SEED-NEWEST", 100);
    // 365 proposed tokens against a 159-token budget: only the newest message fits, and the
    // 206-token overflow is far past the margin yet well inside the reject multiple.
    let session = run_session(
        "0.0008",
        &[
            (
                "seed-hook",
                "on-task-start",
                "seed-context",
                seed_hook_wasm(&[
                    ("user", oldest.as_str()),
                    ("user", middle.as_str()),
                    ("user", newest.as_str()),
                ]),
            ),
            (
                "compact-hook",
                "on-compaction",
                "replace-context",
                compaction_hook_wasm(summary),
            ),
        ],
    );

    let seed = session.context_seed();
    assert_eq!(seed["outcome"], "compacted", "{seed}");

    let messages = session.first_messages();
    assert_eq!(text_of(&messages[0]), summary, "{messages:?}");
    assert!(
        text_of(&messages[1]).starts_with("SEED-NEWEST"),
        "{messages:?}"
    );
    assert_eq!(messages.last().unwrap(), &task_message());

    assert!(
        session.events("compaction").is_empty(),
        "the seed path compacts nothing about the session's context: {}",
        session.trace_raw
    );
}

/// With no compaction hook bound there is nothing to summarize the front, so it is trimmed —
/// and the task still runs.
#[test]
fn overflow_without_a_compaction_hook_trims() {
    if common::skip_without_host_support("overflow_without_a_compaction_hook_trims") {
        return;
    }
    let oldest = seed_text("SEED-OLDEST", 100);
    let middle = seed_text("SEED-MIDDLE", 100);
    let newest = seed_text("SEED-NEWEST", 100);
    // The same 365-against-159 overflow the compacted case uses, with nothing bound to
    // summarize the front.
    let session = run_session(
        "0.0008",
        &[(
            "seed-hook",
            "on-task-start",
            "seed-context",
            seed_hook_wasm(&[
                ("user", oldest.as_str()),
                ("user", middle.as_str()),
                ("user", newest.as_str()),
            ]),
        )],
    );

    let seed = session.context_seed();
    assert_eq!(seed["outcome"], "trimmed", "{seed}");

    let messages = session.first_messages();
    assert!(
        text_of(&messages[0]).starts_with("SEED-NEWEST"),
        "{messages:?}"
    );
    assert_eq!(messages.last().unwrap(), &task_message());
}

/// One message wider than the whole budget seeds nothing, and says so in two places.
#[test]
fn oversized_message_is_rejected() {
    if common::skip_without_host_support("oversized_message_is_rejected") {
        return;
    }
    let huge = seed_text("SEED-HUGE", 200);
    // A 19-token budget against a 221-token message: no trim can help.
    let session = run_session(
        "0.0001",
        &[(
            "seed-hook",
            "on-task-start",
            "seed-context",
            seed_hook_wasm(&[("user", huge.as_str())]),
        )],
    );

    assert_eq!(
        session.first_messages(),
        &vec![task_message()],
        "a rejected seed leaves the context untouched"
    );

    let seed = session.context_seed();
    assert_eq!(seed["outcome"], "rejected");
    assert_eq!(seed["reason"], "message_over_budget");
    assert_eq!(seed["tokens"], 0);
    assert!(seed["proposed_tokens"].as_u64().unwrap() > seed["budget_tokens"].as_u64().unwrap());
    assert!(seed["message_ids"].as_array().unwrap().is_empty());

    let faults: Vec<&Value> = session
        .events("hook_dispatch_error")
        .into_iter()
        .filter(|e| e["arm"] == "seed-rejected")
        .collect();
    assert_eq!(faults.len(), 1, "trace was:\n{}", session.trace_raw);
    assert_eq!(faults[0]["hook_name"], "seed-hook");
    assert_eq!(faults[0]["event"], "on-task-start");

    // A refused seed must not cost the task its ordinary terminal records.
    assert_eq!(session.events("task_end").len(), 1);
    assert_eq!(session.events("session_end").len(), 1);
}
