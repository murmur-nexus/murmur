//! Host implementation of `murmur:task-io/read@0.1.0`.
//!
//! A hook component that imports this interface can read the text of the task
//! its capsule was handed and the result text the agent loop produced for it,
//! holding no `filesystem` grant and with neither payload copied into any
//! lifecycle record. The runtime holds both in one [`TaskIoState`] and copies
//! only the window a hook explicitly asks for, so a hook that never imports the
//! interface is not charged for the bytes.
//!
//! See `wit/hook/deps/murmur-task-io/read.wit` for the contract, including the
//! table of which lifecycle events have a task in scope.

use std::sync::{Arc, Mutex, PoisonError};

use wasmtime::component::Linker;

use crate::bindings::hook::murmur::task_io::read::{IoError, TaskInputForm};

/// The versioned instance name the host provides the four read functions under.
/// Hook components that do not import it simply ignore the registration.
pub(crate) const TASK_IO_IFACE_VERSIONED: &str = "murmur:task-io/read@0.1.0";

/// The text belonging to the task currently in scope.
///
/// `original` and `as_given` are byte-identical until a hook reopens the task:
/// the reopen loop rewrites `task.md` as the original plus accumulated feedback,
/// and `as_given` is what the *next* attempt's agent loop is handed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TaskIoValues {
    pub(crate) original: String,
    pub(crate) as_given: String,
    /// The result text this attempt's agent loop produced. `None` until the loop
    /// reaches a terminal arm that writes one, and again from the start of every
    /// subsequent attempt — a hook judging attempt N never sees attempt N-1's
    /// result.
    pub(crate) output: Option<String>,
}

/// The runtime's single copy of the in-scope task's text.
///
/// Owned by `HookRuntime` for the whole launch and shared with each granted
/// hook's linker through an `Arc`, so the reopen loop's `begin_task_attempt` and
/// the agent loop's `record_task_output` are visible to a hook call already in
/// flight. `None` means no task is in scope, which is what every lifecycle event
/// outside the agent loop sees.
#[derive(Debug, Default)]
pub(crate) struct TaskIoState {
    values: Mutex<Option<TaskIoValues>>,
}

impl TaskIoState {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Put a task in scope for one attempt. Clears any previous attempt's output.
    pub(crate) fn begin_task_attempt(&self, original: String, as_given: String) {
        *self.lock() = Some(TaskIoValues {
            original,
            as_given,
            output: None,
        });
    }

    /// Record the result text the agent loop produced for the attempt in scope.
    /// A no-op when no task is in scope, which is the bypass path that runs an
    /// agent loop without entering the reopen loop.
    pub(crate) fn record_output(&self, text: &str) {
        if let Some(values) = self.lock().as_mut() {
            values.output = Some(text.to_string());
        }
    }

    /// Take the task out of scope. Every later read reports `no-task` until the
    /// next `begin_task_attempt`.
    pub(crate) fn end_task(&self) {
        *self.lock() = None;
    }

    pub(crate) fn input_len(&self, form: TaskInputForm) -> Result<u64, IoError> {
        self.with_input(form, |value| Ok(value.len() as u64))
    }

    pub(crate) fn read_input(
        &self,
        form: TaskInputForm,
        offset: u64,
        max_bytes: u64,
    ) -> Result<String, IoError> {
        self.with_input(form, |value| window(value, offset, max_bytes))
    }

    pub(crate) fn output_len(&self) -> Result<u64, IoError> {
        self.with_output(|value| Ok(value.len() as u64))
    }

    pub(crate) fn read_output(&self, offset: u64, max_bytes: u64) -> Result<String, IoError> {
        self.with_output(|value| window(value, offset, max_bytes))
    }

    fn with_input<R>(
        &self,
        form: TaskInputForm,
        f: impl FnOnce(&str) -> Result<R, IoError>,
    ) -> Result<R, IoError> {
        let guard = self.lock();
        let values = guard.as_ref().ok_or(IoError::NoTask)?;
        f(match form {
            TaskInputForm::AsGiven => &values.as_given,
            TaskInputForm::Original => &values.original,
        })
    }

    fn with_output<R>(&self, f: impl FnOnce(&str) -> Result<R, IoError>) -> Result<R, IoError> {
        let guard = self.lock();
        let values = guard.as_ref().ok_or(IoError::NoTask)?;
        f(values.output.as_deref().ok_or(IoError::NoOutput)?)
    }

    /// A hook that panicked mid-call must not turn every later read into a
    /// different error than the contract names, so a poisoned lock is recovered
    /// rather than propagated — the state behind it is plain owned strings with
    /// no invariant a panic could have broken.
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<TaskIoValues>> {
        self.values.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The longest prefix of `value[offset..]` that fits in `max_bytes` and ends on a
/// UTF-8 character boundary. `offset` past the end, or inside a multi-byte
/// character, is [`IoError::OutOfRange`]; `offset` equal to the length and
/// `max_bytes: 0` both yield an empty string.
fn window(value: &str, offset: u64, max_bytes: u64) -> Result<String, IoError> {
    let offset = usize::try_from(offset).map_err(|_| IoError::OutOfRange)?;
    if offset > value.len() || !value.is_char_boundary(offset) {
        return Err(IoError::OutOfRange);
    }
    let rest = &value[offset..];
    // A `max_bytes` past `usize` cannot be a real budget on this host; clamping to
    // the remainder returns all of it, which is what asking for more than exists
    // means everywhere else here.
    let mut end = usize::try_from(max_bytes)
        .unwrap_or(usize::MAX)
        .min(rest.len());
    while !rest.is_char_boundary(end) {
        end -= 1;
    }
    Ok(rest[..end].to_string())
}

/// Register the four `murmur:task-io/read@0.1.0` functions on a hook linker.
///
/// `state` is `None` for a hook whose manifest entry does not declare
/// `capabilities.task_io.read: true`: the functions are still *defined* (so an
/// importing hook links and runs), they just all return
/// [`IoError::NotGranted`]. A denied read is a value the hook branches on, never
/// a trap.
pub(crate) fn add_task_io_to_linker<T: 'static>(
    linker: &mut Linker<T>,
    state: Option<Arc<TaskIoState>>,
) -> Result<(), String> {
    let mut instance = linker
        .instance(TASK_IO_IFACE_VERSIONED)
        .map_err(|e| format!("failed to define {TASK_IO_IFACE_VERSIONED}: {e}"))?;

    let granted = state.clone();
    instance
        .func_wrap(
            "input-len",
            move |_store: wasmtime::StoreContextMut<'_, T>, (form,): (TaskInputForm,)| {
                Ok((granted
                    .as_ref()
                    .map_or(Err(IoError::NotGranted), |s| s.input_len(form)),))
            },
        )
        .map_err(|e| register_err("input-len", &e))?;

    let granted = state.clone();
    instance
        .func_wrap(
            "read-input",
            move |_store: wasmtime::StoreContextMut<'_, T>,
                  (form, offset, max_bytes): (TaskInputForm, u64, u64)| {
                Ok((granted.as_ref().map_or(Err(IoError::NotGranted), |s| {
                    s.read_input(form, offset, max_bytes)
                }),))
            },
        )
        .map_err(|e| register_err("read-input", &e))?;

    let granted = state.clone();
    instance
        .func_wrap(
            "output-len",
            move |_store: wasmtime::StoreContextMut<'_, T>, (): ()| {
                Ok((granted
                    .as_ref()
                    .map_or(Err(IoError::NotGranted), |s| s.output_len()),))
            },
        )
        .map_err(|e| register_err("output-len", &e))?;

    let granted = state;
    instance
        .func_wrap(
            "read-output",
            move |_store: wasmtime::StoreContextMut<'_, T>, (offset, max_bytes): (u64, u64)| {
                Ok((granted.as_ref().map_or(Err(IoError::NotGranted), |s| {
                    s.read_output(offset, max_bytes)
                }),))
            },
        )
        .map_err(|e| register_err("read-output", &e))?;

    Ok(())
}

fn register_err(func: &str, err: &wasmtime::Error) -> String {
    format!("failed to register {TASK_IO_IFACE_VERSIONED}#{func}: {err}")
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::TASK_IO_IFACE_VERSIONED;
    use wasmtime::component::Component;

    /// One row per `murmur:hook/lifecycle@0.7.0` function: the WIT name, the WIT type of its
    /// event parameter, and the core-function signature the canonical ABI flattens that type
    /// to. `on-inference`'s record flattens to 17 values and `on-shell`'s to 19, both past
    /// the 16-parameter limit, so each is passed indirectly as a single pointer.
    const LIFECYCLE_FNS: &[(&str, &str, &str)] = &[
        ("on-stage", "$stage-event", "i32 i32"),
        (
            "on-session-start",
            "$session-context",
            "i32 i32 i32 i32 i32 i32 i32 i32 i32 i32",
        ),
        (
            "on-task-start",
            "$task-start-event",
            "i32 i32 i32 i32 i32 i32 i64 i64 i64 i64",
        ),
        ("on-inference", "$inference-event", "i32"),
        (
            "on-tool-call",
            "$tool-event",
            "i32 i32 i32 i64 i32 i32 i32 i64 i64 i32 i32",
        ),
        ("on-shell", "$shell-event", "i32"),
        (
            "on-compaction",
            "$compaction-event",
            "i32 i32 i64 f64 i32 i32 i32 i32 i32 i32",
        ),
        ("on-task-end", "$task-end-event", "i32 i32 i32 i32"),
        (
            "on-session-end",
            "$session-end-event",
            "i32 i64 i64 i32 i32 i64 i32 i32",
        ),
    ];

    /// The import declaration for `murmur:task-io/read@0.1.0`, plus the memory and `realloc`
    /// the four lowered imports need. Memory lives in its own core module so a lowered import
    /// can reference it without a cyclic instantiation — the same arrangement
    /// `hooks::tests::hook_inference_caller_double` uses.
    fn preamble() -> String {
        format!(
            r#"  (import "{TASK_IO_IFACE_VERSIONED}" (instance $tio
    (type (enum "as-given" "original"))
    (export "task-input-form" (type (eq 0)))
    (type (enum "not-granted" "no-task" "no-output" "out-of-range"))
    (export "io-error" (type (eq 2)))
    (type (result u64 (error 3)))
    (type (result string (error 3)))
    (export "input-len" (func (param "form" 1) (result 4)))
    (export "read-input"
      (func (param "form" 1) (param "offset" u64) (param "max-bytes" u64) (result 5)))
    (export "output-len" (func (result 4)))
    (export "read-output" (func (param "offset" u64) (param "max-bytes" u64) (result 5)))
  ))
  (alias export $tio "input-len" (func $ilen))
  (alias export $tio "read-input" (func $rin))
  (alias export $tio "output-len" (func $olen))
  (alias export $tio "read-output" (func $rout))

  (core module $libc
    (memory (export "memory") 4)
    (global $bump (mut i32) (i32.const 32768))
    (func (export "realloc") (param i32 i32 i32 i32) (result i32)
      (local $p i32)
      (local.set $p (i32.and (i32.add (global.get $bump) (i32.const 7)) (i32.const -8)))
      (global.set $bump (i32.add (local.get $p) (i32.add (local.get 3) (i32.const 8))))
      (local.get $p))
  )
  (core instance $li (instantiate $libc))
  (alias core export $li "memory" (core memory $mem))
  (alias core export $li "realloc" (core func $realloc))
  (core func $ilen_l
    (canon lower (func $ilen) (memory $mem) (realloc $realloc) string-encoding=utf8))
  (core func $rin_l
    (canon lower (func $rin) (memory $mem) (realloc $realloc) string-encoding=utf8))
  (core func $olen_l
    (canon lower (func $olen) (memory $mem) (realloc $realloc) string-encoding=utf8))
  (core func $rout_l
    (canon lower (func $rout) (memory $mem) (realloc $realloc) string-encoding=utf8))"#
        )
    }

    /// Core-module boilerplate: the four lowered imports, a bump cursor over the report
    /// buffer, and one decoder per result shape the interface returns.
    ///
    /// Both decoders render an `err` as `!` followed by the `io-error` case index — `0`
    /// `not-granted`, `1` `no-task`, `2` `no-output`, `3` `out-of-range` — so a test asserts
    /// which error the host chose rather than merely that one occurred. `result<u64, io-error>`
    /// has align 8, so its payload sits at byte 8; `result<string, io-error>` has align 4, so
    /// its payload sits at byte 4.
    const CORE_HELPERS: &str = r#"
    (import "libc" "memory" (memory 4))
    (import "tio" "ilen" (func $ilen (param i32 i32)))
    (import "tio" "rin" (func $rin (param i32 i64 i64 i32)))
    (import "tio" "olen" (func $olen (param i32)))
    (import "tio" "rout" (func $rout (param i64 i64 i32)))
    (global $cur (mut i32) (i32.const 0))
    (func $put (param $b i32)
      (i32.store8 (global.get $cur) (local.get $b))
      (global.set $cur (i32.add (global.get $cur) (i32.const 1))))
    (func $append (param $src i32) (param $n i32)
      (memory.copy (global.get $cur) (local.get $src) (local.get $n))
      (global.set $cur (i32.add (global.get $cur) (local.get $n))))
    (func $dec (param $v i32)
      (if (i32.ge_u (local.get $v) (i32.const 10))
        (then (call $dec (i32.div_u (local.get $v) (i32.const 10)))))
      (call $put (i32.add (i32.const 48) (i32.rem_u (local.get $v) (i32.const 10)))))
    (func $emit_len (param $rp i32)
      (if (i32.eqz (i32.load8_u (local.get $rp)))
        (then (call $dec (i32.wrap_i64 (i64.load (i32.add (local.get $rp) (i32.const 8))))))
        (else
          (call $put (i32.const 33))
          (call $put (i32.add (i32.const 48)
                              (i32.load8_u (i32.add (local.get $rp) (i32.const 8))))))))
    (func $emit_str (param $rp i32)
      (if (i32.eqz (i32.load8_u (local.get $rp)))
        (then (call $append (i32.load (i32.add (local.get $rp) (i32.const 4)))
                            (i32.load (i32.add (local.get $rp) (i32.const 8)))))
        (else
          (call $put (i32.const 33))
          (call $put (i32.add (i32.const 48)
                              (i32.load8_u (i32.add (local.get $rp) (i32.const 4))))))))
"#;

    /// Every WIT type a lifecycle export in these doubles names.
    const LIFECYCLE_TYPES: &str = r#"
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
  (type $stage-event (record (field "shell-allow" (list string))))
  (type $session-context (record
    (field "capsule-name" string)
    (field "capsule-version" string)
    (field "session-id" string)
    (field "model" string)
    (field "capabilities" (list string))))
  (type $task-start-event (record
    (field "task-id" string)
    (field "context-id" string)
    (field "source" string)
    (field "input-bytes" u64)
    (field "budget-tokens" u64)
    (field "context-window" u64)
    (field "prior-tokens" u64)))
  (type $inference-event (record
    (field "turn" u32)
    (field "input-tokens" u64)
    (field "output-tokens" u64)
    (field "decision" string)
    (field "tool-name" (option string))
    (field "prompt" (option string))
    (field "output" (option string))
    (field "tools" (option string))))
  (type $tool-outcome (record
    (field "output-bytes" u64)
    (field "duration-ms" u64)
    (field "status" string)))
  (type $tool-event (record
    (field "turn" u32)
    (field "tool-name" string)
    (field "input-bytes" u64)
    (field "input" string)
    (field "outcome" (option $tool-outcome))))
  (type $shell-outcome (record
    (field "exit-code" s32)
    (field "stdout" string)
    (field "stderr" string)
    (field "stdout-bytes" u64)
    (field "stderr-bytes" u64)
    (field "duration-ms" u64)))
  (type $shell-event (record
    (field "turn" u32)
    (field "binary" string)
    (field "command" string)
    (field "argv" (list string))
    (field "script" (option string))
    (field "outcome" (option $shell-outcome))))
  (type $compaction-event (record
    (field "messages" (list $message))
    (field "session-tokens" u64)
    (field "threshold" float64)
    (field "model" (option string))
    (field "system-prompt" (option string))))
  (type $task-end-event (record
    (field "task-id" string)
    (field "exit-status" string)))
  (type $session-end-event (record
    (field "total-turns" u32)
    (field "total-input-tokens" u64)
    (field "total-output-tokens" u64)
    (field "total-tool-calls" u32)
    (field "total-shell-calls" u32)
    (field "duration-ms" u64)
    (field "exit-status" string)))
"#;

    /// The types every lifecycle instance re-exports alongside its functions.
    const TYPE_EXPORTS: &str = "    (export \"message\" (type $message))\n    \
                                (export \"tool-manifest\" (type $tool-manifest))\n    \
                                (export \"hook-output\" (type $hook-output))\n    \
                                (export \"tool-outcome\" (type $tool-outcome))\n    \
                                (export \"shell-outcome\" (type $shell-outcome))\n";

    /// Assemble a component from a core body plus the lift and instance-export lines its
    /// lifecycle functions need.
    fn build(engine: &wasmtime::Engine, core_body: &str, lifts: &str, exports: &str) -> Component {
        let wat = format!(
            "(component\n{preamble}\n\n  (core module $m{CORE_HELPERS}{core_body}  )\n  \
             (core instance $i (instantiate $m\n    (with \"libc\" (instance $li))\n    \
             (with \"tio\" (instance\n      (export \"ilen\" (func $ilen_l))\n      \
             (export \"rin\" (func $rin_l))\n      (export \"olen\" (func $olen_l))\n      \
             (export \"rout\" (func $rout_l))))))\n{LIFECYCLE_TYPES}\n{lifts}\n  \
             (instance $lc\n{TYPE_EXPORTS}{exports}  )\n  \
             (export \"murmur:hook/lifecycle@0.7.0\" (instance $lc))\n)",
            preamble = preamble(),
        );
        let bytes = wat::parse_str(&wat).expect("task-io double WAT parses");
        Component::new(engine, &bytes).expect("task-io double compiles")
    }

    /// The `canon lift` for one lifecycle function, naming the core export `core_name`.
    fn lift_line(wit_name: &str, core_name: &str) -> String {
        let event_type = LIFECYCLE_FNS
            .iter()
            .find(|(name, _, _)| *name == wit_name)
            .map(|(_, ty, _)| *ty)
            .expect("every lifted function is a declared lifecycle function");
        format!(
            "  (type ${core_name}-ft (func (param \"event\" {event_type}) \
             (result (result $hook-output (error string)))))\n  \
             (func ${core_name} (type ${core_name}-ft)\n    (canon lift (core func $i \
             \"{core_name}\") (memory $mem) (realloc $realloc) string-encoding=utf8))\n"
        )
    }

    /// The field separator in [`reader_double`]'s report. ASCII unit separator rather than a
    /// printable character because a reopened attempt's `as-given` contains the previous
    /// attempt's whole report, so any character the report itself uses would make the fields
    /// ambiguous to split.
    pub(crate) const REPORT_SEP: char = '\u{1f}';

    /// A hook double that, on `on-task-end`, calls all four `murmur:task-io/read` functions
    /// and returns everything it read as `reopen-task`.
    ///
    /// The reason string is `A=<as-given>`, `O=<original>`, `R=<output>`, `LI=<input-len>`,
    /// `LO=<output-len>` joined by [`REPORT_SEP`], a failed call rendered as `!<io-error case
    /// index>`. Only `on-task-end` has a real body; the other
    /// required exports are bare stubs whose signature does not match, exactly as the existing
    /// single-event doubles in `hooks.rs` do, so this double is only ever usefully dispatched
    /// for `on-task-end`.
    pub(crate) fn reader_double(engine: &wasmtime::Engine) -> Component {
        let core_body = r#"
    (func (export "ontaskend") (param i32 i32 i32 i32) (result i32)
      (global.set $cur (i32.const 2048))
      (call $put (i32.const 65)) (call $put (i32.const 61))
      (call $rin (i32.const 0) (i64.const 0) (i64.const 8192) (i32.const 1024))
      (call $emit_str (i32.const 1024))
      (call $put (i32.const 31))
      (call $put (i32.const 79)) (call $put (i32.const 61))
      (call $rin (i32.const 1) (i64.const 0) (i64.const 8192) (i32.const 1024))
      (call $emit_str (i32.const 1024))
      (call $put (i32.const 31))
      (call $put (i32.const 82)) (call $put (i32.const 61))
      (call $rout (i64.const 0) (i64.const 8192) (i32.const 1024))
      (call $emit_str (i32.const 1024))
      (call $put (i32.const 31))
      (call $put (i32.const 76)) (call $put (i32.const 73)) (call $put (i32.const 61))
      (call $ilen (i32.const 0) (i32.const 1024))
      (call $emit_len (i32.const 1024))
      (call $put (i32.const 31))
      (call $put (i32.const 76)) (call $put (i32.const 79)) (call $put (i32.const 61))
      (call $olen (i32.const 1024))
      (call $emit_len (i32.const 1024))
      (i32.store (i32.const 128) (i32.const 0))
      (i32.store (i32.const 132) (i32.const 4))
      (i32.store (i32.const 136) (i32.const 2048))
      (i32.store (i32.const 140) (i32.sub (global.get $cur) (i32.const 2048)))
      (i32.const 128))
    (func (export "noop"))
"#;
        let lifts = format!(
            "{}  (func $noop (canon lift (core func $i \"noop\")))\n",
            lift_line("on-task-end", "ontaskend")
        );
        let mut exports = String::from(
            "    (export \"task-end-event\" (type $task-end-event))\n    \
             (export \"on-task-end\" (func $ontaskend))\n",
        );
        for (name, _, _) in LIFECYCLE_FNS {
            if !matches!(*name, "on-task-end" | "on-stage" | "on-task-start") {
                exports.push_str(&format!("    (export \"{name}\" (func $noop))\n"));
            }
        }
        build(engine, core_body, &lifts, &exports)
    }

    /// A hook double that answers **every** lifecycle function by reporting what
    /// `murmur:task-io/read` says about scope, as `err("<fn> in=<…>,out=<…>")`.
    ///
    /// A returned `err` is the one channel available at every lifecycle event: the runtime
    /// appends it verbatim to `logs/hook-<name>.log` and carries on. That is what lets a test
    /// read one line per dispatch, in dispatch order, for events like `on-session-end` which
    /// honor no `hook-output` arm at all.
    pub(crate) fn probe_double(engine: &wasmtime::Engine) -> Component {
        // Each export names itself from a label in a data segment, so the report body exists
        // once rather than nine times.
        let mut data = String::new();
        let mut labels = Vec::new();
        let mut offset = 512usize;
        for (name, _, _) in LIFECYCLE_FNS {
            labels.push((offset, name.len()));
            data.push_str(name);
            offset += name.len();
        }

        let mut core_body = format!(
            r#"
    (data (i32.const 512) "{data}")
    (global $lp (mut i32) (i32.const 0))
    (global $ll (mut i32) (i32.const 0))
    (func $probe (result i32)
      (global.set $cur (i32.const 8192))
      (call $append (global.get $lp) (global.get $ll))
      (call $put (i32.const 32))
      (call $put (i32.const 105)) (call $put (i32.const 110)) (call $put (i32.const 61))
      (call $ilen (i32.const 0) (i32.const 1024))
      (call $emit_len (i32.const 1024))
      (call $put (i32.const 44))
      (call $put (i32.const 111)) (call $put (i32.const 117)) (call $put (i32.const 116))
      (call $put (i32.const 61))
      (call $olen (i32.const 1024))
      (call $emit_len (i32.const 1024))
      (i32.store (i32.const 128) (i32.const 1))
      (i32.store (i32.const 132) (i32.const 8192))
      (i32.store (i32.const 136) (i32.sub (global.get $cur) (i32.const 8192)))
      (i32.const 128))
"#
        );
        let mut lifts = String::new();
        // An instance export naming a type must precede the function exports that use it.
        let mut exports = String::new();
        let mut func_exports = String::new();
        for ((name, event_type, params), (label_offset, label_len)) in
            LIFECYCLE_FNS.iter().zip(&labels)
        {
            let core_name = name.replace('-', "");
            core_body.push_str(&format!(
                "    (func (export \"{core_name}\") (param {params}) (result i32)\n      \
                 (global.set $lp (i32.const {label_offset}))\n      \
                 (global.set $ll (i32.const {label_len}))\n      (call $probe))\n"
            ));
            lifts.push_str(&lift_line(name, &core_name));
            exports.push_str(&format!(
                "    (export \"{}\" (type {event_type}))\n",
                event_type.trim_start_matches('$')
            ));
            func_exports.push_str(&format!("    (export \"{name}\" (func ${core_name}))\n"));
        }
        exports.push_str(&func_exports);
        build(engine, &core_body, &lifts, &exports)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two hand-authored guest doubles are hand-laid-out against the canonical ABI, so
    /// their signatures are only checked when Wasmtime validates them. Building both here
    /// fails loudly on a layout slip rather than inside whichever suite happens to use one.
    #[test]
    fn the_guest_doubles_compile() {
        let mut config = wasmtime::Config::new();
        config.wasm_component_model(true);
        let engine = wasmtime::Engine::new(&config).expect("engine builds");
        test_support::reader_double(&engine);
        test_support::probe_double(&engine);
    }

    /// A value whose multi-byte characters sit at byte offsets that a naive
    /// fixed-size chunk would split: `é` is 2 bytes, `字` is 3, `𝄞` is 4.
    const MIXED: &str = "abéc字d𝄞efg";

    fn in_scope() -> TaskIoState {
        let state = TaskIoState::new();
        state.begin_task_attempt("original text".to_string(), MIXED.to_string());
        state
    }

    /// Reading in a loop with a `max_bytes` smaller than the value reassembles it
    /// byte for byte, and no window ever splits a character.
    #[test]
    fn windowed_reads_reassemble_the_value_without_splitting_a_character() {
        let state = in_scope();
        for max_bytes in 1..=6u64 {
            let mut assembled = String::new();
            let mut offset = 0u64;
            let len = state.input_len(TaskInputForm::AsGiven).unwrap();
            while offset < len {
                let chunk = state
                    .read_input(TaskInputForm::AsGiven, offset, max_bytes)
                    .expect("an offset produced by advancing over whole characters stays in range");
                if chunk.is_empty() {
                    // `max_bytes` is smaller than the next character; a real caller
                    // grows its budget rather than looping forever.
                    assert!(
                        max_bytes < 4,
                        "only a budget below the widest character can stall"
                    );
                    break;
                }
                assert!(
                    chunk.len() as u64 <= max_bytes,
                    "a window must never exceed the caller's budget"
                );
                offset += chunk.len() as u64;
                assembled.push_str(&chunk);
            }
            if max_bytes >= 4 {
                assert_eq!(assembled, MIXED, "max_bytes={max_bytes}");
            } else {
                assert!(MIXED.starts_with(&assembled), "max_bytes={max_bytes}");
            }
        }
    }

    /// `input-len` is the byte length, not the character count — the whole point
    /// of the size-first shape is that a caller can size a buffer from it.
    #[test]
    fn input_len_is_the_byte_length() {
        let state = in_scope();
        assert_eq!(
            state.input_len(TaskInputForm::AsGiven).unwrap(),
            MIXED.len() as u64
        );
        assert_ne!(MIXED.len(), MIXED.chars().count());
    }

    /// The three boundary offsets: at the end is an empty string, past the end is
    /// `out-of-range`, and inside a multi-byte character is `out-of-range` rather
    /// than a silently realigned read.
    #[test]
    fn offset_boundaries_are_reported_rather_than_realigned() {
        let state = in_scope();
        let len = MIXED.len() as u64;

        assert_eq!(
            state.read_input(TaskInputForm::AsGiven, len, 16).unwrap(),
            ""
        );
        assert_eq!(
            state.read_input(TaskInputForm::AsGiven, len + 1, 16),
            Err(IoError::OutOfRange)
        );
        // `abé…` — byte 3 is the second byte of `é`.
        assert!(!MIXED.is_char_boundary(3));
        assert_eq!(
            state.read_input(TaskInputForm::AsGiven, 3, 16),
            Err(IoError::OutOfRange)
        );
    }

    /// `max_bytes: 0` is a legal, empty read, not an error: a caller that has run
    /// out of budget gets a value to branch on.
    #[test]
    fn zero_max_bytes_is_an_empty_read() {
        let state = in_scope();
        assert_eq!(state.read_input(TaskInputForm::AsGiven, 0, 0).unwrap(), "");
    }

    /// The two input forms are independent values, and `original` is unaffected by
    /// whatever the attempt was actually handed.
    #[test]
    fn the_two_input_forms_read_different_values() {
        let state = in_scope();
        assert_eq!(
            state.read_input(TaskInputForm::Original, 0, 1024).unwrap(),
            "original text"
        );
        assert_eq!(
            state.read_input(TaskInputForm::AsGiven, 0, 1024).unwrap(),
            MIXED
        );
    }

    /// Scope transitions: nothing before a task starts, `no-output` while the loop
    /// is running, the recorded text once it produces one, and `no-task` again
    /// after the task ends.
    #[test]
    fn scope_transitions_report_no_task_and_no_output() {
        let state = TaskIoState::new();
        assert_eq!(
            state.input_len(TaskInputForm::AsGiven),
            Err(IoError::NoTask)
        );
        assert_eq!(state.output_len(), Err(IoError::NoTask));

        state.begin_task_attempt("t".to_string(), "t".to_string());
        assert_eq!(state.input_len(TaskInputForm::AsGiven), Ok(1));
        assert_eq!(state.output_len(), Err(IoError::NoOutput));
        assert_eq!(state.read_output(0, 16), Err(IoError::NoOutput));

        state.record_output("result");
        assert_eq!(state.output_len(), Ok(6));
        assert_eq!(state.read_output(0, 16), Ok("result".to_string()));

        state.end_task();
        assert_eq!(
            state.input_len(TaskInputForm::AsGiven),
            Err(IoError::NoTask)
        );
        assert_eq!(state.output_len(), Err(IoError::NoTask));
    }

    /// A second attempt clears the first attempt's result: this is what stops a
    /// gate hook from judging attempt 2 against attempt 1's output.
    #[test]
    fn beginning_an_attempt_clears_the_previous_attempts_output() {
        let state = TaskIoState::new();
        state.begin_task_attempt("t".to_string(), "t".to_string());
        state.record_output("attempt one");
        assert_eq!(state.output_len(), Ok(11));

        state.begin_task_attempt("t".to_string(), "t + feedback".to_string());
        assert_eq!(state.output_len(), Err(IoError::NoOutput));
        assert_eq!(
            state.read_input(TaskInputForm::AsGiven, 0, 1024).unwrap(),
            "t + feedback"
        );
        assert_eq!(
            state.read_input(TaskInputForm::Original, 0, 1024).unwrap(),
            "t"
        );
    }

    /// Recording an output with no task in scope is dropped rather than putting a
    /// task in scope: the bypass agent-loop path writes a result without ever
    /// entering the reopen loop, and must still read as `no-task`.
    #[test]
    fn recording_output_with_no_task_in_scope_is_a_no_op() {
        let state = TaskIoState::new();
        state.record_output("orphan result");
        assert_eq!(state.output_len(), Err(IoError::NoTask));
    }
}
