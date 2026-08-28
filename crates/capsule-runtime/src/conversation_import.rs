//! Host implementation of `murmur:conversation/read@0.1.0`.
//!
//! A hook component that imports this interface reads the capsule's durable conversation record —
//! every message the runtime put in front of the model, newest first, paged — holding no
//! `filesystem` grant and with no other artifact involved. Nothing is preopened under
//! `conversations/`, so this is the only way in.
//!
//! See `wit/hook/deps/murmur-conversation/read.wit` for the contract and
//! [`crate::conversation`] for the record itself.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use wasmtime::component::Linker;

use crate::bindings::hook::murmur::conversation::read::MessagePage;

/// The versioned instance name the host provides `read-messages` under. Hook components that do
/// not import it ignore the registration.
pub(crate) const CONVERSATION_IFACE_VERSIONED: &str = "murmur:conversation/read@0.1.0";

/// The runtime's view of which record is in scope.
///
/// Owned by `HookRuntime` for the whole launch and shared with each granted hook's linker through
/// an `Arc`. `root` is fixed for the launch; the context id moves with the task loop, so a hook
/// dispatched during task N reads task N's conversation.
pub(crate) struct ConversationState {
    /// `<home>/.murmur/conversations/<record>`, or `None` when this session writes no record —
    /// `context.record: off`, a `process`-transport capsule, or a host whose home could not be
    /// resolved. A read then returns an empty page, never an error.
    root: Option<PathBuf>,
    /// Session directory a malformed record line is reported into, beside stderr.
    workdir: PathBuf,
    /// Context id of the task in scope. `None` before the first task of a launch.
    context_id: Mutex<Option<String>>,
}

impl ConversationState {
    pub(crate) fn new(root: Option<PathBuf>, workdir: &Path) -> Self {
        Self {
            root,
            workdir: workdir.to_path_buf(),
            context_id: Mutex::new(None),
        }
    }

    /// Put one task's conversation in scope. Called for every task the runtime starts, from the
    /// same dispatch that tells hooks the task's `context-id`.
    pub(crate) fn set_context(&self, context_id: Option<String>) {
        *self.lock() = context_id;
    }

    /// One page of the record in scope, newest first.
    ///
    /// A context id that is not a usable path segment reads as no record rather than as an error:
    /// the id came from an A2A client, and a conversation the runtime declined to record has
    /// nothing to report but its emptiness.
    pub(crate) fn read_messages(
        &self,
        cursor: Option<String>,
        limit: u32,
    ) -> Result<MessagePage, String> {
        let context_id = self.lock().clone();
        let path = match (self.root.as_deref(), context_id.as_deref()) {
            (Some(root), Some(context_id))
                if crate::state_store::validate_store_name(context_id).is_ok() =>
            {
                Some(root.join(context_id).join("conversation.jsonl"))
            }
            _ => None,
        };
        crate::conversation::page(path.as_deref(), &self.workdir, cursor.as_deref(), limit)
    }

    /// A hook that panicked mid-call must not turn every later read into a different error than
    /// the contract names, so a poisoned lock is recovered rather than propagated — the state
    /// behind it is one owned string with no invariant a panic could have broken.
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<String>> {
        self.context_id
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// Register `murmur:conversation/read@0.1.0` on a hook linker.
///
/// `state` is `None` for a hook whose manifest entry does not declare
/// `capabilities.conversation.read: true`: the function is still *defined* (so an importing hook
/// links and runs), it just returns `not-granted`. A denied read is a value the hook branches on,
/// never a trap, and it is distinguishable from the `Ok` empty page a session with no record
/// returns.
pub(crate) fn add_conversation_to_linker<T: 'static>(
    linker: &mut Linker<T>,
    state: Option<Arc<ConversationState>>,
) -> Result<(), String> {
    let mut instance = linker
        .instance(CONVERSATION_IFACE_VERSIONED)
        .map_err(|e| format!("failed to define {CONVERSATION_IFACE_VERSIONED}: {e}"))?;

    instance
        .func_wrap(
            "read-messages",
            move |_store: wasmtime::StoreContextMut<'_, T>,
                  (cursor, limit): (Option<String>, u32)| {
                Ok((state.as_ref().map_or_else(
                    || Err("not-granted".to_string()),
                    |state| state.read_messages(cursor, limit),
                ),))
            },
        )
        .map_err(|e| {
            format!("failed to register {CONVERSATION_IFACE_VERSIONED}#read-messages: {e}")
        })?;

    Ok(())
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::CONVERSATION_IFACE_VERSIONED;
    use wasmtime::component::Component;

    /// The import declaration for `murmur:conversation/read@0.1.0`, plus the memory and `realloc`
    /// the lowered import needs.
    ///
    /// Two things about the shape, both of which a `wit-bindgen` guest produces as well:
    ///
    /// * `read`'s `message` is `use`d from `murmur:hook/lifecycle`, so a component importing it
    ///   also imports that interface for the type alone. The host defines no such instance and
    ///   does not have to: a types-only import carries no runtime value to link against.
    /// * An import instance must *export* every named type it uses, and everything referencing
    ///   one has to name the exported alias rather than the definition behind it — hence the
    ///   numeric type indices below, which are the exports at 2 (`message`) and 5
    ///   (`message-page`).
    fn preamble() -> String {
        format!(
            r#"  (import "murmur:hook/lifecycle@0.6.0" (instance $lct
    (type $o (option string))
    (type $m (record (field "role" string) (field "content" string) (field "id" $o) (field "source-id" $o)))
    (export "message" (type (eq $m)))
  ))
  (import "{CONVERSATION_IFACE_VERSIONED}" (instance $conv
    (type $optstr (option string))
    (type $message (record
      (field "role" string)
      (field "content" string)
      (field "id" $optstr)
      (field "source-id" $optstr)))
    (export "message" (type (eq $message)))
    (type $msglist (list 2))
    (type $page (record
      (field "messages" $msglist)
      (field "next-cursor" $optstr)
      (field "total" u32)))
    (export "message-page" (type (eq $page)))
    (type $ret (result 5 (error string)))
    (export "read-messages"
      (func (param "cursor" $optstr) (param "limit" u32) (result $ret)))
  ))
  (alias export $conv "read-messages" (func $read))

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
  (core func $read_l
    (canon lower (func $read) (memory $mem) (realloc $realloc) string-encoding=utf8))"#
        )
    }

    /// The field separator in [`reader_double`]'s report — ASCII unit separator, for the reason
    /// `task_io_import::test_support::REPORT_SEP` is one: message content is arbitrary text.
    pub(crate) const REPORT_SEP: char = '\u{1f}';

    /// Core-module boilerplate: the lowered import, a bump cursor over the report buffer, and the
    /// decoder that renders one `result<message-page, string>`.
    ///
    /// The report is `T=<total>` then, per message, `<id>=<role>` and the first bytes of its
    /// content, joined by [`REPORT_SEP`], and ends with `N=<next-cursor>` (`N=-` for `none`). An
    /// `err` renders as `!<error string>`, so a test asserts which error the host chose.
    ///
    /// `result<message-page, string>` has align 4: the discriminant is at byte 0, the page's
    /// three fields at 4/8 (messages ptr/len), 12/16/20 (`next-cursor` option), and 24 (`total`);
    /// an `err`'s string sits at 4/8. One `message` record is 40 bytes.
    const CORE_HELPERS: &str = r#"
    (import "libc" "memory" (memory 4))
    (import "conv" "read" (func $read (param i32 i32 i32 i32 i32)))
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
    (func $emit_page (param $rp i32) (local $i i32) (local $rec i32) (local $n i32)
      (if (i32.eqz (i32.load8_u (local.get $rp)))
        (then
          (call $put (i32.const 84)) (call $put (i32.const 61))
          (call $dec (i32.load (i32.add (local.get $rp) (i32.const 24))))
          (local.set $n (i32.load (i32.add (local.get $rp) (i32.const 8))))
          (local.set $i (i32.const 0))
          (block $done
            (loop $next
              (br_if $done (i32.ge_u (local.get $i) (local.get $n)))
              (local.set $rec (i32.add (i32.load (i32.add (local.get $rp) (i32.const 4)))
                                       (i32.mul (local.get $i) (i32.const 40))))
              (call $put (i32.const 31))
              ;; id, when the option carries one
              (if (i32.load8_u (i32.add (local.get $rec) (i32.const 16)))
                (then (call $append (i32.load (i32.add (local.get $rec) (i32.const 20)))
                                    (i32.load (i32.add (local.get $rec) (i32.const 24))))))
              (call $put (i32.const 61))
              (call $append (i32.load (local.get $rec)) (i32.load (i32.add (local.get $rec) (i32.const 4))))
              (local.set $i (i32.add (local.get $i) (i32.const 1)))
              (br $next)))
          (call $put (i32.const 31))
          (call $put (i32.const 78)) (call $put (i32.const 61))
          (if (i32.load8_u (i32.add (local.get $rp) (i32.const 12)))
            (then (call $append (i32.load (i32.add (local.get $rp) (i32.const 16)))
                                (i32.load (i32.add (local.get $rp) (i32.const 20)))))
            (else (call $put (i32.const 45)))))
        (else
          (call $put (i32.const 33))
          (call $append (i32.load (i32.add (local.get $rp) (i32.const 4)))
                        (i32.load (i32.add (local.get $rp) (i32.const 8)))))))
"#;

    /// Every WIT type the lifecycle exports in this double name.
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
    (case "seed-context" (list $message))))
  (type $task-end-event (record
    (field "task-id" string)
    (field "exit-status" string)))
"#;

    /// A hook double that, on `on-task-end`, calls `read-messages` twice — a first page with the
    /// caller's `limit`, then the page its `next-cursor` names — and returns both reports as
    /// `reopen-task`, joined by `|`.
    ///
    /// Only `on-task-end` has a real body; the other required exports are bare stubs, exactly as
    /// the single-event doubles in `hooks.rs` are, so this double is only ever usefully
    /// dispatched for `on-task-end`.
    pub(crate) fn reader_double(engine: &wasmtime::Engine, limit: u32) -> Component {
        let core_body = format!(
            r#"
    (func (export "ontaskend") (param i32 i32 i32 i32) (result i32)
      (global.set $cur (i32.const 4096))
      (call $read (i32.const 0) (i32.const 0) (i32.const 0) (i32.const {limit}) (i32.const 1024))
      (call $emit_page (i32.const 1024))
      (call $put (i32.const 124))
      ;; The second call reuses the first page's `next-cursor`, when it had one.
      (if (i32.and (i32.eqz (i32.load8_u (i32.const 1024)))
                   (i32.load8_u (i32.const 1036)))
        (then (call $read (i32.const 1) (i32.load (i32.const 1040)) (i32.load (i32.const 1044))
                          (i32.const {limit}) (i32.const 2048)))
        (else (call $read (i32.const 0) (i32.const 0) (i32.const 0)
                          (i32.const {limit}) (i32.const 2048))))
      (call $emit_page (i32.const 2048))
      (i32.store (i32.const 128) (i32.const 0))
      (i32.store (i32.const 132) (i32.const 4))
      (i32.store (i32.const 136) (i32.const 4096))
      (i32.store (i32.const 140) (i32.sub (global.get $cur) (i32.const 4096)))
      (i32.const 128))
    (func (export "noop"))
"#
        );
        let wat = format!(
            "(component\n{preamble}\n\n  (core module $m{CORE_HELPERS}{core_body}  )\n  \
             (core instance $i (instantiate $m\n    (with \"libc\" (instance $li))\n    \
             (with \"conv\" (instance (export \"read\" (func $read_l))))))\n\
             {LIFECYCLE_TYPES}\n  \
             (type $ontaskend-ft (func (param \"event\" $task-end-event) \
             (result (result $hook-output (error string)))))\n  \
             (func $ontaskend (type $ontaskend-ft)\n    (canon lift (core func $i \"ontaskend\") \
             (memory $mem) (realloc $realloc) string-encoding=utf8))\n  \
             (func $noop (canon lift (core func $i \"noop\")))\n  \
             (instance $lc\n    (export \"message\" (type $message))\n    \
             (export \"tool-manifest\" (type $tool-manifest))\n    \
             (export \"hook-output\" (type $hook-output))\n    \
             (export \"task-end-event\" (type $task-end-event))\n    \
             (export \"on-task-end\" (func $ontaskend))\n{stubs}  )\n  \
             (export \"murmur:hook/lifecycle@0.6.0\" (instance $lc))\n)",
            preamble = preamble(),
            stubs = [
                "on-session-start",
                "on-inference",
                "on-tool-call",
                "on-shell",
                "on-compaction",
                "on-session-end",
            ]
            .iter()
            .map(|name| format!("    (export \"{name}\" (func $noop))\n"))
            .collect::<String>(),
        );
        let bytes = wat::parse_str(&wat).expect("conversation double WAT parses");
        Component::new(engine, &bytes).expect("conversation double compiles")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hand-authored guest double is laid out against the canonical ABI, so its layout is
    /// only checked when Wasmtime validates it. Building it here fails loudly on a slip rather
    /// than inside whichever suite happens to use it.
    #[test]
    fn the_guest_double_compiles() {
        let mut config = wasmtime::Config::new();
        config.wasm_component_model(true);
        let engine = wasmtime::Engine::new(&config).expect("engine builds");
        test_support::reader_double(&engine, 2);
    }

    /// An importing component links whether or not it is granted: the import is defined either
    /// way, so an ungranted hook runs and gets `not-granted` from the call rather than failing to
    /// instantiate.
    #[test]
    fn an_importing_component_links_with_and_without_the_grant() {
        let mut config = wasmtime::Config::new();
        config.wasm_component_model(true);
        let engine = wasmtime::Engine::new(&config).expect("engine builds");
        let component = test_support::reader_double(&engine, 2);
        let workdir = tempfile::tempdir().unwrap();

        for state in [
            None,
            Some(Arc::new(ConversationState::new(None, workdir.path()))),
        ] {
            let mut linker: Linker<()> = Linker::new(&engine);
            add_conversation_to_linker(&mut linker, state).unwrap();
            let mut store = wasmtime::Store::new(&engine, ());
            linker
                .instantiate(&mut store, &component)
                .expect("the import is defined on every linker");
        }
    }

    /// A session with no record reads as an empty page, and so does one whose task never entered
    /// scope — never as an error, so a hook can tell "nothing recorded" from "not granted".
    #[test]
    fn no_record_and_no_context_both_read_as_an_empty_page() {
        let workdir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();

        let no_record = ConversationState::new(None, workdir.path());
        no_record.set_context(Some("ctx_1".to_string()));
        let no_context = ConversationState::new(Some(home.path().to_path_buf()), workdir.path());

        for state in [&no_record, &no_context] {
            let page = state.read_messages(None, 10).expect("an Ok empty page");
            assert!(page.messages.is_empty());
            assert_eq!(page.next_cursor, None);
            assert_eq!(page.total, 0);
        }
    }

    /// A context id an A2A client could send is checked before it becomes a path: the read is
    /// empty rather than reaching outside the record root.
    #[test]
    fn a_context_id_that_escapes_reads_as_no_record() {
        let workdir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let state =
            ConversationState::new(Some(home.path().join("conversations/c")), workdir.path());
        state.set_context(Some("../../escape".to_string()));

        assert_eq!(state.read_messages(None, 10).unwrap().total, 0);
    }
}
