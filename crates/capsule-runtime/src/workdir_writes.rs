//! What the runtime itself writes inside the capsule's work folder, declared rather than
//! discovered.
//!
//! A consumer for whom the work folder *is* the deliverable — a benchmark harness diffing it to
//! score a patch, a `git-worktree` workspace whose diff becomes a pull request — has to answer
//! "what did the *capsule* change?". Without a declaration that answer is a hand-maintained
//! exclusion list that goes stale the moment the runtime gains an entry, and silently: nothing
//! tells the consumer the set grew. [`runtime_writes`] is that declaration, carried by
//! [`crate::containment::ScopeReport`] and therefore printed by `mur run --explain-scope` and
//! recorded verbatim in `trace.jsonl`'s `session_start.effective_grants`.
//!
//! # Path form
//!
//! Every [`RuntimeWriteReport::path`] is relative to the **work folder**: the `--workdir`
//! directory, or `<manifest dir>/workdir/<session id>/` for a launch without one. The runtime
//! creates nothing else in it, so the declaration is the same fixed answer for both.
//!
//! # What is labelled
//!
//! [`RuntimeWriteCondition`] names when a path is present. Each turns on what the capsule does
//! after it starts — a peer handle the agent redeems, a script capsule's own write to its `.` —
//! which no report can know, so every row is always declared, whatever the tier, and the
//! condition says when the path appears. Both are written only at the capsule's own request.
//!
//! # What is not here
//!
//! Every file the runtime writes for its own use — `MURMUR.md`, `task.md`, the manifest copy,
//! `tools/` and `plans/` — is written into the session's runtime directory,
//! `~/.murmur/sessions/<session-id>/runtime`, and the synthetic `/etc` files and a
//! `transport: process` harness's files go beside it. Everything else it writes about or for the
//! session goes into the session's records directory, `~/.murmur/records/<workdir
//! root>/<session-id>`: `trace.jsonl`, `blobs/`, `logs/bootstrap.log`, `logs/otel.log`,
//! `logs/hook-<name>.log`, a shell command's full-output log (`logs/shell-<ms>.log` for a
//! truncated foreground command, `logs/<work-id>.log` for a demoted one), a delegated child's
//! `completion.json`, the `hooks/` a records-granted hook writes in, an agent's `out/result.txt`,
//! `out/result_<task-id>.txt` and `out/compaction-summaries.jsonl`, the commands' `tmp/` and
//! `home/`, and `children/<capsule>-<16 hex>/`, each delegated child's work folder. None of them is
//! a runtime write inside the work folder, so none is declared here; [`crate::runtime_dir`],
//! [`crate::records_dir`] and [`crate::temp_dir`] describe them.
//!
//! # Adding an entry
//!
//! A file the runtime writes for its own use goes through the runtime directory's root and is
//! declared in the test module's `RUNTIME_DIR_WRITES`; anything else it writes goes through the
//! records directory's root, `records_root`, and is declared in its `RECORDS_DIR_WRITES`.
//! `every_workdir_write_is_declared_or_exempted` reads the runtime's own source text and fails
//! until a new write is declared in one of those tables, in [`DECLARED_WRITES`] or in an exempt
//! table with a written reason. A row in [`DECLARED_WRITES`] is a path the runtime writes into
//! the operator's folder, which needs a reason the capsule asked for it.

use serde::Serialize;

/// Whether a declared entry is a single file or a directory whose whole subtree the runtime owns.
///
/// A consumer subtracting the declaration treats [`Self::Directory`] as a path prefix and
/// [`Self::File`] as an exact match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeWriteKind {
    File,
    Directory,
}

impl RuntimeWriteKind {
    /// The wire name, identical to what [`Serialize`] emits.
    #[must_use]
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Directory => "directory",
        }
    }
}

/// When a declared entry is present on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeWriteCondition {
    /// Written by a script capsule, whose WASI preopen `.` is the work folder itself.
    ScriptSession,
    /// Written when a peer handle is redeemed, which needs `capabilities.peer_fetch.allow`.
    PeerFetch,
}

impl RuntimeWriteCondition {
    /// The wire name, identical to what [`Serialize`] emits.
    #[must_use]
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::ScriptSession => "script-session",
            Self::PeerFetch => "peer-fetch",
        }
    }
}

/// One path the runtime writes inside the capsule's work folder.
///
/// Field names are the JSON keys verbatim: `path`, `kind`, `condition`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuntimeWriteReport {
    /// Relative to the work folder.
    pub path: String,
    pub kind: RuntimeWriteKind,
    pub condition: RuntimeWriteCondition,
}

/// One row of [`DECLARED_WRITES`].
struct DeclaredWrite {
    entry: &'static str,
    kind: RuntimeWriteKind,
    condition: RuntimeWriteCondition,
}

impl DeclaredWrite {
    fn report(&self) -> RuntimeWriteReport {
        RuntimeWriteReport {
            path: self.entry.to_string(),
            kind: self.kind,
            condition: self.condition,
        }
    }

    /// The first path segment of this entry — the name a `workdir.join(...)` in the runtime's own
    /// source has to resolve to for the entry to count as declared.
    #[cfg(test)]
    fn first_segment(&self) -> &'static str {
        self.entry.split('/').next().unwrap_or(self.entry)
    }
}

/// Every path the runtime writes inside a capsule's work folder, in report order.
///
/// The single source of truth: `mur run --explain-scope` reads it, `trace.jsonl` carries what that
/// produced, and `every_workdir_write_is_declared_or_exempted` fails the suite when the runtime's
/// source grows a work folder entry this table does not name.
const DECLARED_WRITES: &[DeclaredWrite] = &[
    DeclaredWrite {
        // Where a redeemed peer handle's bytes land, at the agent's own fetch call. A documented
        // resource-plane path.
        entry: "peer-in",
        kind: RuntimeWriteKind::Directory,
        condition: RuntimeWriteCondition::PeerFetch,
    },
    DeclaredWrite {
        // A script capsule's WASI preopen `.` is the work folder, so its own `out/result.txt`
        // lands here. The runtime only reads it.
        entry: "out",
        kind: RuntimeWriteKind::Directory,
        condition: RuntimeWriteCondition::ScriptSession,
    },
];

/// Every path the runtime writes inside this launch's work folder, in declaration order.
///
/// Pure and filesystem-free: `mur run --explain-scope` returns before a work folder exists, so a
/// declaration that touched one would make the diagnostic create the thing it describes. The same
/// set on every tier.
pub(crate) fn runtime_writes() -> Vec<RuntimeWriteReport> {
    DECLARED_WRITES.iter().map(DeclaredWrite::report).collect()
}

/// The `Runtime writes` block of `mur run --explain-scope`'s human rendering.
///
/// Ends with a newline. Shared with nothing else today, but kept beside the declaration for the
/// reason [`crate::containment::render_read_only`] is kept beside its own: a second renderer is a
/// second place for the wording to drift from the data.
#[must_use]
pub fn render_runtime_writes(writes: &[RuntimeWriteReport]) -> String {
    let mut out = String::new();
    out.push_str("  paths are relative to the work folder\n");
    if writes.is_empty() {
        out.push_str("  runtime writes: <none>\n");
        return out;
    }
    out.push_str("  runtime writes:\n");
    for write in writes {
        out.push_str(&format!(
            "    - {} ({}, {})\n",
            write.path,
            write.kind.wire_name(),
            write.condition.wire_name()
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The source files carrying a production `workdir.join(...)` or a [`WorkdirRoot`] write,
    /// each scanned by [`every_workdir_write_is_declared_or_exempted`] and by
    /// [`no_workdir_file_bypasses_the_no_follow_helper`].
    ///
    /// `plan.rs` and `delegation_plane.rs` have no production write of their own and are scanned
    /// anyway: both read a workdir for a decision, so a raw call either grows is caught without
    /// anybody remembering to widen this list.
    ///
    /// [`WorkdirRoot`]: crate::workdir_fs::WorkdirRoot
    const SCANNED: &[(&str, &str)] = &[
        ("agent.rs", include_str!("agent.rs")),
        ("agent/inventory.rs", include_str!("agent/inventory.rs")),
        ("agent/process.rs", include_str!("agent/process.rs")),
        ("child_launch.rs", include_str!("child_launch.rs")),
        ("delegation.rs", include_str!("delegation.rs")),
        ("delegation_plane.rs", include_str!("delegation_plane.rs")),
        ("detached.rs", include_str!("detached.rs")),
        ("hooks.rs", include_str!("hooks.rs")),
        ("instruction_files.rs", include_str!("instruction_files.rs")),
        ("murmur_md.rs", include_str!("murmur_md.rs")),
        ("otel.rs", include_str!("otel.rs")),
        ("network_policy.rs", include_str!("network_policy.rs")),
        ("plan.rs", include_str!("plan.rs")),
        ("protected_paths.rs", include_str!("protected_paths.rs")),
        ("recipes.rs", include_str!("recipes.rs")),
        ("records_dir.rs", include_str!("records_dir.rs")),
        ("resource_plane.rs", include_str!("resource_plane.rs")),
        ("retention.rs", include_str!("retention.rs")),
        ("runtime.rs", include_str!("runtime.rs")),
        (
            "runtime/artifact_change.rs",
            include_str!("runtime/artifact_change.rs"),
        ),
        ("runtime_dir.rs", include_str!("runtime_dir.rs")),
        ("sandbox.rs", include_str!("sandbox.rs")),
        ("sealed.rs", include_str!("sealed.rs")),
        ("shell.rs", include_str!("shell.rs")),
        ("session_check.rs", include_str!("session_check.rs")),
        ("tool_annotations.rs", include_str!("tool_annotations.rs")),
        ("trace.rs", include_str!("trace.rs")),
        ("trace_blobs.rs", include_str!("trace_blobs.rs")),
        ("types.rs", include_str!("types.rs")),
        ("workdir_snapshot.rs", include_str!("workdir_snapshot.rs")),
    ];

    /// Constant names a `workdir.join(...)` may name, paired with the constant itself.
    ///
    /// The pairing is the point: the scanner reads the identifier out of source text, and this
    /// table is what turns that text into the value it stands for, so renaming the constant or
    /// changing what it holds is caught rather than silently accepted.
    const CONSTANTS: &[(&str, &str)] = &[
        ("MANIFEST_FILENAME", murmur_artifact::MANIFEST_FILENAME),
        ("COMPLETION_FILE", crate::delegation::COMPLETION_FILE),
        ("BLOB_DIR_NAME", crate::trace_blobs::BLOB_DIR_NAME),
        ("TASK_INPUT_FILE", crate::runtime::TASK_INPUT_FILE),
        (
            "LEGACY_TASK_INPUT_FILE",
            crate::runtime::LEGACY_TASK_INPUT_FILE,
        ),
        ("TRACE_FILE_NAME", crate::trace::TRACE_FILE_NAME),
        ("CHILDREN_DIR", crate::child_launch::CHILDREN_DIR),
        ("DELEGATIONS_SUBDIR", crate::runtime_dir::DELEGATIONS_SUBDIR),
        ("HOOKS_SUBDIR", crate::records_dir::HOOKS_SUBDIR),
        ("HOME_SUBDIR", crate::records_dir::HOME_SUBDIR),
        ("CHECK_LOG_FILE", crate::session_check::CHECK_LOG_FILE),
        ("RESULTS_SUBDIR", crate::records_dir::RESULTS_SUBDIR),
        ("RESULT_FILE", crate::records_dir::RESULT_FILE),
        (
            "SESSION_TEMP_DIR_NAME",
            crate::temp_dir::SESSION_TEMP_DIR_NAME,
        ),
    ];

    /// Entry names the scanner resolves that are deliberately not declared, each with the reason.
    const EXEMPT: &[(&str, &str)] = &[
        (
            "input.txt",
            "Read, never written: the operator's task file `import_task_input` reads once when \
             the workdir has no `task.md`. Source text cannot tell a read from a write, so a \
             path the runtime only ever opens for reading is exempted here rather than declared \
             as something the runtime produces.",
        ),
        (
            "package.json",
            "Read, never written: the recipe detector opens it to learn which scripts a workdir \
             already declares. Exempt on `input.txt`'s terms.",
        ),
    ];

    /// `workdir.join(...)` calls whose argument is not a literal or a named constant, each with
    /// the reason no entry name can be resolved from the source text.
    const DYNAMIC_EXEMPT: &[(&str, &str, &str)] = &[
        (
            "protected_paths.rs",
            "&rule.relative",
            "A `capabilities.filesystem.read_only` path from the manifest, resolved to decide \
             whether a call is refused. The runtime reads it and writes nothing.",
        ),
        (
            "protected_paths.rs",
            "&lexical",
            "The lexically normalised form of a path a tool call named, resolved for the same \
             refusal decision.",
        ),
        (
            "resource_plane.rs",
            "declared_root",
            "The `exports.files` / `exports.peer_files` root a manifest declares. The plane \
             serves what the capsule put there; it creates nothing of its own.",
        ),
        (
            "resource_plane.rs",
            "&export.root",
            "The same declared root, resolved when a request is served. Exempt on \
             `declared_root`'s terms.",
        ),
        (
            "runtime.rs",
            "&stored_path",
            "Where a redeemed peer handle's bytes go, composed by `peer_handoff::stored_path_for` \
             under `peer-in/`. Declared through that directory's own entry, which is the name a \
             consumer excludes.",
        ),
        (
            "runtime.rs",
            "rel",
            "`write_staged_file`'s path, `tools/<name>/<file>` in the runtime directory, composed \
             by each of its callers. Declared through RUNTIME_DIR_WRITES' `tools` entry.",
        ),
        (
            "session_check.rs",
            "rel.as_ref()",
            "A file or directory of an eval case's `start` or `check.files`, copied into the work \
             folder by `copy_tree_into`: `start` by `mur eval run` into a freshly staged work \
             folder before launch, as an input it stages for the capsule; `check.files` by \
             `CheckRunner::run` after the session has ended and before the check's command runs. \
             No session writes either.",
        ),
        (
            "types.rs",
            "rel",
            "`StagedSession::write_workdir_file`'s path, named by the embedding caller for an \
             input it stages for the capsule before launch. A launch writes nothing there of its \
             own.",
        ),
        (
            "trace.rs",
            "trace",
            "A resumed trace's `trace.jsonl`, in the directory `mur run --resume` named it in \
             (`ResumeRequest::trace_dir`): the prior session's records directory, or the folder \
             its trace was copied to. Only `shell_lost` markers are appended.",
        ),
        (
            "runtime.rs",
            "&name",
            "The launched manifest's own file name, copied into the runtime directory through \
             `runtime_root`. Declared through RUNTIME_DIR_WRITES' `murmur.yaml` entry, the \
             project manifest's name.",
        ),
        (
            "records_dir.rs",
            "&rel",
            "`<workdir root mirrored>/<session-id>` beneath `~/.murmur/records`, created at staging \
             outside every workdir.",
        ),
    ];

    /// Production text of `source`: everything above its test module.
    ///
    /// A test fixture writing `workdir.join("scratch")` says nothing about what the runtime
    /// produces, so scanning below this marker would fill the tables with names no capsule ever
    /// sees.
    fn production_text(source: &str) -> &str {
        match source.find("#[cfg(test)]\nmod tests {") {
            Some(index) => &source[..index],
            None => source,
        }
    }

    /// The source text between the `(` that ends at `open` and its matching `)`.
    fn parenthesised(source: &str, open: usize) -> &str {
        let bytes = source.as_bytes();
        let mut depth = 1usize;
        let mut index = open;
        while index < bytes.len() && depth > 0 {
            match bytes[index] {
                b'(' => depth += 1,
                b')' => depth -= 1,
                _ => {}
            }
            index += 1;
        }
        assert!(depth == 0, "unbalanced parentheses in a scanned call");
        &source[open..index - 1]
    }

    /// `text` collapsed to one line: an argument's line breaks and indentation are `rustfmt`'s
    /// business, and a table row that had to reproduce them would fail on a reflow.
    fn one_line(text: &str) -> String {
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// The first top-level argument of an argument list.
    fn first_argument(arguments: &str) -> &str {
        let mut depth = 0i32;
        let mut in_string = false;
        for (index, character) in arguments.char_indices() {
            match character {
                '"' => in_string = !in_string,
                '(' | '[' | '{' if !in_string => depth += 1,
                ')' | ']' | '}' if !in_string => depth -= 1,
                ',' if !in_string && depth == 0 => return &arguments[..index],
                _ => {}
            }
        }
        arguments
    }

    /// Every `workdir.join(<arg>)` in `source`, as the argument's source text.
    ///
    /// The receiver is matched on its `workdir` suffix rather than by name, which is what makes
    /// `accessible_workdir.join(`, `session_workdir.join(`, `child_workdir.join(` and
    /// `parent_accessible_workdir\n    .join(` all one case.
    fn workdir_join_arguments(source: &str) -> Vec<String> {
        let mut found = Vec::new();
        let mut cursor = 0;
        while let Some(offset) = source[cursor..].find(".join(") {
            let start = cursor + offset;
            cursor = start + ".join(".len();
            if !source[..start].trim_end().ends_with("workdir") {
                continue;
            }
            found.push(one_line(parenthesised(source, cursor)));
        }
        found
    }

    /// The [`crate::workdir_fs::WorkdirRoot`] methods that create or change something beneath a
    /// root.
    const HELPER_WRITES: &[&str] = &[
        ".write_file(",
        ".write_file_exact_mode(",
        ".overwrite_in_place(",
        ".open_append(",
        ".create_dir_all(",
        ".set_dir_mode(",
    ];

    /// The relative-path argument of every [`HELPER_WRITES`] call in `source`, as source text.
    ///
    /// A `.create_dir_all(` straight after `fs` is `std::fs`'s, a raw call
    /// [`no_workdir_file_bypasses_the_no_follow_helper`] judges instead.
    fn helper_write_arguments(source: &str) -> Vec<String> {
        helper_calls(source, HELPER_WRITES)
            .into_iter()
            .map(|(_, argument)| argument)
            .collect()
    }

    /// Every call of one of `methods` in `source`, as `(receiver, first argument)` source text,
    /// skipping `std::fs` functions of the same name. The receiver is the identifier the call
    /// chain ends in — `runtime_root` for `self.runtime_root\n    .write_file(` — or `""` when
    /// the text before the call does not end in one.
    fn helper_calls(source: &str, methods: &[&str]) -> Vec<(String, String)> {
        helper_calls_at(source, methods)
            .into_iter()
            .map(|(_, receiver, argument)| (receiver, argument))
            .collect()
    }

    /// [`helper_calls`], with the byte offset of each call.
    fn helper_calls_at(source: &str, methods: &[&str]) -> Vec<(usize, String, String)> {
        let mut found = Vec::new();
        for method in methods {
            let mut cursor = 0;
            while let Some(offset) = source[cursor..].find(method) {
                let start = cursor + offset;
                cursor = start + method.len();
                if source[..start].ends_with("fs") {
                    continue;
                }
                let receiver: String = source[..start]
                    .trim_end()
                    .chars()
                    .rev()
                    .take_while(|character| character.is_ascii_alphanumeric() || *character == '_')
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                found.push((
                    start,
                    receiver,
                    one_line(first_argument(parenthesised(source, cursor))),
                ));
            }
        }
        found
    }

    /// The receivers that name the session's runtime directory, through which every file the
    /// runtime owns is written and read.
    const RUNTIME_DIR_ROOT: &str = "runtime_root";

    /// The receivers that name the session's records directory, through which every record the
    /// runtime writes about the session goes.
    const RECORDS_DIR_ROOT: &str = "records_root";

    /// Every entry the runtime writes into the records directory, with what it is.
    const RECORDS_DIR_WRITES: &[(&str, &str)] = &[
        (
            "trace.jsonl",
            "The session's trace: every appender, and a resumed session's `shell_lost` markers.",
        ),
        (
            "blobs",
            "Trace bodies too large to inline, by their sha256.",
        ),
        (
            "logs",
            "`bootstrap.log`, `otel.log`, one `hook-<name>.log` per failing hook, and a shell \
             command's full output: `shell-<ms>.log` for a truncated foreground command and \
             `<work-id>.log` for a demoted one, and `check.log`, an eval check's output, written \
             after the session ended.",
        ),
        (
            "completion.json",
            "A delegated child's record of how it ended, written by the child and by its \
             parent's watcher.",
        ),
        (
            "hooks",
            "The `\".\"` of every hook declared `capabilities.filesystem.records: true`.",
        ),
        (
            "tmp",
            "The session's temporary folder, `/tmp` and `$TMPDIR` to its commands. Created \
             empty at staging; the runtime writes nothing in it.",
        ),
        (
            "home",
            "The session's home folder, `$HOME` to its commands. Created empty at staging; the \
             runtime writes nothing in it.",
        ),
        (
            "out",
            "An agent's `result.txt`, `result_<task-id>.txt` and `compaction-summaries.jsonl`, \
             written by the runtime alone.",
        ),
        (
            "children",
            "One `<capsule>-<16 hex>` work folder per delegated child, the child's `--workdir`.",
        ),
    ];

    /// Writes through `records_root` whose argument is not a literal or a named constant, each
    /// with the [`RECORDS_DIR_WRITES`] entry it lies under and the reason. Matched only through
    /// `records_root`: the same argument through a workdir root resolves to nothing and fails.
    const RECORDS_DIR_DYNAMIC: &[(&str, &str, &str, &str)] = &[
        (
            "child_launch.rs",
            "rel",
            "children",
            "One delegated child's work folder, `children/<name>-<16 hex>`, composed by \
             `child_workdir_rel`.",
        ),
        (
            "shell.rs",
            "&log.relative",
            "logs",
            "A foreground shell command's full-output log, `logs/shell-<ms>.log`, composed by \
             `detached::output_path_for`.",
        ),
        (
            "detached.rs",
            "relative",
            "logs",
            "A demoted shell command's log, `logs/<work-id>.log`, composed by \
             `detached::output_path_for` and written as the command runs.",
        ),
    ];

    /// The literal text a path argument opens with — a string literal, or a `format!` string up
    /// to its first `{` — or `None` when it opens with no literal.
    fn literal_prefix(argument: &str) -> Option<&str> {
        let argument = argument.trim_start_matches('&');
        let argument = argument
            .strip_prefix("format!(")
            .map(str::trim_start)
            .unwrap_or(argument);
        let (literal, _) = argument.strip_prefix('"')?.split_once('"')?;
        Some(literal.split('{').next().unwrap_or(literal))
    }

    /// Whether a write of `argument` names a log file. Every log is a record.
    fn names_a_log(argument: &str) -> bool {
        literal_prefix(argument)
            .is_some_and(|literal| literal == "logs" || literal.starts_with("logs/"))
    }

    /// The receivers that name a directory beside the runtime directory, outside every workdir:
    /// `sealed-etc/`, `harness/` and the session directory holding them. No capsule command is
    /// granted any of them, so what is written there is no runtime write inside a workdir.
    const OUTSIDE_WORKDIR_ROOTS: &[&str] = &["sealed_etc_root", "harness_root", "session_dir_root"];

    /// Every entry the runtime writes into the runtime directory, with what it is. The files a
    /// capsule's commands read and cannot change.
    const RUNTIME_DIR_WRITES: &[(&str, &str)] = &[
        (
            "tools",
            "Every staged and pulled artifact's manifest, `skill.md` and native entrypoint, and \
             every runtime-provided tool's manifest.",
        ),
        (
            "MURMUR.md",
            "The environment reference, written at staging and after port binding.",
        ),
        (
            "task.md",
            "The task: `mur run --task`, the import from the workdir, every A2A message and \
             every reopen's rewrite.",
        ),
        (
            "plans",
            "One `plan-<n>.json` per submitted plan, read back when it runs.",
        ),
        (
            "murmur.yaml",
            "The project manifest, copied on every launch.",
        ),
        (
            "delegations",
            "One `<delegation-id>.txt` per sub-capsule whose answer was cut in the report its \
             parent was handed: the whole answer, written once by `hand_over_answer`.",
        ),
    ];

    /// Entry names of files the runtime owns and of the records it keeps: a write or read under
    /// one of them reaches a runtime-directory or records-directory root, never a workdir root.
    const MOVED_OUT_OF_THE_WORKDIR: &[&str] = &[
        "tools",
        "MURMUR.md",
        "task.md",
        "plans",
        "murmur.yaml",
        ".mur-etc",
        ".process-driver",
        "trace.jsonl",
        "blobs",
        "completion.json",
        "eval.jsonl",
    ];

    /// The root a moved name belongs to: `records_root` for a record, `runtime_root` for every
    /// other.
    fn root_of(entry: &str) -> &'static str {
        if RECORDS_DIR_WRITES.iter().any(|(name, _)| *name == entry) || entry == "eval.jsonl" {
            RECORDS_DIR_ROOT
        } else {
            RUNTIME_DIR_ROOT
        }
    }

    /// Functions that read the workdir's own copy of one of those names, each with the reason.
    const MOVED_NAME_WORKDIR_READS: &[(&str, &str, &str)] = &[(
        "runtime.rs",
        "import_task_input",
        "The one read of the operator's `task.md` from the workdir, at session start, copied \
         into the runtime directory and never read again.",
    )];

    /// [`crate::workdir_fs::WorkdirRoot`] methods that read, list, inspect or remove.
    const HELPER_READS: &[&str] = &[
        ".read_file(",
        ".read_to_string(",
        ".list_dir(",
        ".file_len(",
        ".entry_kind(",
        ".remove_file(",
        ".remove_dir_all(",
        ".open_beneath(",
    ];

    /// The entry name — the first path segment — a path argument stands for, or `None` when the
    /// source text cannot say: a variable, a field access, a format string that opens with a
    /// value rather than a name.
    ///
    /// A literal, a `format!` whose string opens with a literal segment or with a named constant
    /// (`format!("{BLOB_DIR_NAME}/{sha}")`), and a named constant all resolve.
    fn resolve_entry_name(argument: &str) -> Option<String> {
        let argument = argument.trim_start_matches('&');
        let argument = argument
            .strip_prefix("format!(")
            .map(str::trim_start)
            .unwrap_or(argument);
        let resolved = if let Some(rest) = argument.strip_prefix('"') {
            let (literal, _) = rest.split_once('"')?;
            match literal.strip_prefix('{') {
                Some(inner) => resolve_constant(argument, inner.split_once('}')?.0)?,
                None if literal.starts_with('{') => return None,
                None => literal.split('{').next()?.to_string(),
            }
        } else {
            resolve_constant(argument, argument.rsplit("::").next().unwrap_or(argument))?
        };
        let first = resolved.split('/').next()?.to_string();
        (!first.is_empty()).then_some(first)
    }

    /// The value of the constant `name` in [`CONSTANTS`], or `None` when `name` is not a constant
    /// name at all. Panics on a constant name the table does not pair with a value.
    fn resolve_constant(argument: &str, name: &str) -> Option<String> {
        let is_constant_name = !name.is_empty()
            && name
                .chars()
                .all(|character| character.is_ascii_uppercase() || character == '_');
        if !is_constant_name {
            return None;
        }
        Some(
            CONSTANTS
                .iter()
                .find(|(constant, _)| *constant == name)
                .unwrap_or_else(|| {
                    panic!(
                        "a workdir write ({argument}) names the constant {name}, which this \
                         test's CONSTANTS table does not pair with a value. Add \
                         (\"{name}\", <path to the constant>) so a rename or a value change is \
                         caught here rather than silently accepted."
                    )
                })
                .1
                .to_string(),
        )
    }

    /// Every entry the runtime writes into a workdir is declared by [`DECLARED_WRITES`] or
    /// exempted here with a written reason.
    ///
    /// The input is the runtime's own source text rather than a list somebody maintains: every
    /// `workdir.join(...)` and the path of every [`HELPER_WRITES`] call. A developer who adds
    /// either fails this test until they decide which it is.
    #[test]
    fn every_workdir_write_is_declared_or_exempted() {
        let declared: Vec<&str> = DECLARED_WRITES
            .iter()
            .map(DeclaredWrite::first_segment)
            .collect();

        let mut scanned = 0usize;
        let mut dynamic_rows_used = vec![false; DYNAMIC_EXEMPT.len()];
        let mut records_dynamic_rows_used = vec![false; RECORDS_DIR_DYNAMIC.len()];
        for (file, source) in SCANNED {
            let text = production_text(source);
            let arguments = workdir_join_arguments(text)
                .into_iter()
                .map(|argument| (String::new(), argument))
                .chain(helper_calls(text, HELPER_WRITES));
            for (receiver, argument) in arguments {
                scanned += 1;
                if OUTSIDE_WORKDIR_ROOTS.contains(&receiver.as_str()) {
                    continue;
                }
                let runtime_dir = receiver == RUNTIME_DIR_ROOT;
                let records_dir = receiver == RECORDS_DIR_ROOT;
                assert!(
                    records_dir || !names_a_log(&argument),
                    "{file} writes ({argument}) through `{receiver}`, a workdir root. Every log \
                     the runtime writes is a record: it goes through the records directory's root \
                     (`records_root`), which no capsule command can write."
                );
                if records_dir {
                    if let Some(row) = RECORDS_DIR_DYNAMIC.iter().position(
                        |(dynamic_file, dynamic_argument, _, _)| {
                            dynamic_file == file && *dynamic_argument == argument
                        },
                    ) {
                        let entry = RECORDS_DIR_DYNAMIC[row].2;
                        assert!(
                            RECORDS_DIR_WRITES.iter().any(|(name, _)| *name == entry),
                            "RECORDS_DIR_DYNAMIC row ({file:?}, {argument:?}) names `{entry}`, \
                             which RECORDS_DIR_WRITES does not declare."
                        );
                        records_dynamic_rows_used[row] = true;
                        continue;
                    }
                }
                let Some(entry) = resolve_entry_name(&argument) else {
                    let row =
                        DYNAMIC_EXEMPT
                            .iter()
                            .position(|(exempt_file, exempt_argument, _)| {
                                exempt_file == file && *exempt_argument == argument
                            });
                    assert!(
                        row.is_some(),
                        "{file} writes ({argument}) into a workdir, and the argument is neither \
                         a string literal nor a named constant, so this test cannot say what it \
                         names. Add ({file:?}, {argument:?}, <written reason>) to this test's \
                         DYNAMIC_EXEMPT table."
                    );
                    dynamic_rows_used[row.unwrap()] = true;
                    continue;
                };
                if runtime_dir {
                    assert!(
                        RUNTIME_DIR_WRITES.iter().any(|(name, _)| *name == entry),
                        "{file} writes `{entry}` into the runtime directory, and \
                         RUNTIME_DIR_WRITES does not declare it. Add (\"{entry}\", <what it is>)."
                    );
                    continue;
                }
                if records_dir {
                    assert!(
                        RECORDS_DIR_WRITES.iter().any(|(name, _)| *name == entry),
                        "{file} writes `{entry}` into the records directory, and \
                         RECORDS_DIR_WRITES does not declare it. Add (\"{entry}\", <what it is>)."
                    );
                    continue;
                }
                assert!(
                    !MOVED_OUT_OF_THE_WORKDIR.contains(&entry.as_str()),
                    "{file} writes `{entry}` through `{receiver}` ({argument}), a workdir root. \
                     `{entry}` is a file the runtime owns or a record it keeps: it goes through \
                     `{}`, which no capsule command can write.",
                    root_of(&entry)
                );
                assert!(
                    declared.contains(&entry.as_str())
                        || EXEMPT.iter().any(|(name, _)| *name == entry),
                    "{file} writes `{entry}` into a workdir, and no --explain-scope report \
                     declares it. Either add an entry for `{entry}` to DECLARED_WRITES in \
                     workdir_writes.rs, so a consumer diffing the workdir can subtract it, or add \
                     it to this test's EXEMPT table with a written reason — a path the runtime \
                     only reads is the usual one."
                );
            }
        }

        // A scanner that silently stopped matching would otherwise pass by finding nothing.
        assert!(
            scanned >= 40,
            "the workdir write scan found only {scanned} occurrences across {} files, far below \
             what the runtime has — the scanner is broken, not the runtime",
            SCANNED.len()
        );

        for ((file, argument, _, _), used) in
            RECORDS_DIR_DYNAMIC.iter().zip(records_dynamic_rows_used)
        {
            assert!(
                used,
                "RECORDS_DIR_DYNAMIC row ({file:?}, {argument:?}) matches no write through \
                 `records_root` in {file}; remove it so the table names only what the runtime does"
            );
        }

        for ((file, argument, _), used) in DYNAMIC_EXEMPT.iter().zip(dynamic_rows_used) {
            assert!(
                used,
                "DYNAMIC_EXEMPT row ({file:?}, {argument:?}) matches no write in {file}; remove \
                 it so the table names only what the runtime does"
            );
        }

        for (name, _) in EXEMPT {
            assert!(
                !declared.contains(name),
                "`{name}` is listed as exempt and is also declared — remove one"
            );
        }
    }

    /// No production read, listing, check or removal of a file the runtime owns goes through a
    /// workdir root: every one reaches the runtime directory, whose files no capsule command can
    /// change, apart from the rows of [`MOVED_NAME_WORKDIR_READS`].
    #[test]
    fn no_runtime_owned_file_is_reached_through_a_workdir_root() {
        let mut checked = 0usize;
        let mut rows_used = vec![false; MOVED_NAME_WORKDIR_READS.len()];
        for (file, source) in SCANNED {
            let text = production_text(source);
            let methods: Vec<&str> = HELPER_READS.iter().chain(HELPER_WRITES).copied().collect();
            for (start, receiver, argument) in helper_calls_at(text, &methods) {
                let Some(entry) = resolve_entry_name(&argument) else {
                    continue;
                };
                if !MOVED_OUT_OF_THE_WORKDIR.contains(&entry.as_str()) {
                    continue;
                }
                checked += 1;
                if receiver == root_of(&entry) {
                    continue;
                }
                let function = enclosing_fn(text, start);
                let row = MOVED_NAME_WORKDIR_READS
                    .iter()
                    .position(|(row_file, row_fn, _)| row_file == file && *row_fn == function);
                let Some(row) = row else {
                    panic!(
                        "{file}: `{function}` reaches `{entry}` through `{receiver}` ({argument}), \
                         a workdir root. `{entry}` is a file the runtime owns or a record it \
                         keeps, and lives outside the workdir: go through `{}`.",
                        root_of(&entry)
                    );
                };
                rows_used[row] = true;
            }
        }
        assert!(
            checked >= 20,
            "found only {checked} reaches of a runtime-owned file — the scanner is broken"
        );
        for ((file, function, _), used) in MOVED_NAME_WORKDIR_READS.iter().zip(rows_used) {
            assert!(
                used,
                "MOVED_NAME_WORKDIR_READS row ({file:?}, {function:?}) matches no read; remove it"
            );
        }
    }

    /// Every source file with a production `workdir.join(...)` or [`HELPER_WRITES`] call is in
    /// `SCANNED`.
    ///
    /// Without this, a new module writing into a workdir escapes the scans above.
    #[test]
    fn every_source_file_that_joins_a_workdir_is_scanned() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut stack = vec![root.clone()];
        let mut checked = 0usize;
        while let Some(dir) = stack.pop() {
            let entries = std::fs::read_dir(&dir)
                .unwrap_or_else(|error| panic!("reading {}: {error}", dir.display()));
            for entry in entries {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().is_some_and(|extension| extension == "rs") {
                    checked += 1;
                    // This module's own prose and failure messages quote the patterns being
                    // searched for, and `workdir_fs.rs` is the helper itself.
                    if path
                        .file_name()
                        .is_some_and(|name| name == "workdir_writes.rs" || name == "workdir_fs.rs")
                    {
                        continue;
                    }
                    let source = std::fs::read_to_string(&path).unwrap();
                    let text = production_text(&source);
                    if workdir_join_arguments(text).is_empty()
                        && helper_write_arguments(text).is_empty()
                    {
                        continue;
                    }
                    let relative = path
                        .strip_prefix(&root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/");
                    assert!(
                        SCANNED.iter().any(|(file, _)| *file == relative),
                        "src/{relative} writes into a workdir but is not in \
                         every_workdir_write_is_declared_or_exempted's SCANNED list, so nothing \
                         checks its entries against the declaration. Add \
                         (\"{relative}\", include_str!(\"{relative}\")) to it."
                    );
                }
            }
        }
        assert!(
            checked >= 40,
            "walked only {checked} source files under src/ — the walk is broken"
        );
    }

    /// Every raw filesystem call in a scanned file, with the reason its path is not beneath the
    /// accessible workdir or the session directory, keyed by file and enclosing function.
    ///
    /// A row covers every listed call in that function. Anything a capsule's commands can reach
    /// goes through [`crate::workdir_fs::WorkdirRoot`] instead, and has no row here.
    const RAW_FS_EXEMPT: &[(&str, &str, &str)] = &[
        (
            "session_check.rs",
            "copy_tree_into",
            "Reads the files of an eval case's `start` or `check.files`, a directory of the \
             operator's outside every work folder, walked by `walk_tree` with no link followed: a \
             link or special file in it refuses the copy. What it reads is written into the work \
             folder through `WorkdirRoot`.",
        ),
        (
            "network_policy.rs",
            "resolve_scoped_dir",
            "Creates a declared `capabilities.filesystem.scope` beneath the root a WASI guest is \
             preopened at. The preopen itself is opened by path by `wasmtime-wasi`, so a root \
             here would hold nothing the guest then gets; the scope was validated as relative and \
             non-escaping at staging.",
        ),
        (
            "resource_plane.rs",
            "open_no_follow",
            "The resource plane's own reads of files the capsule exported, under its own \
             `SymlinkPolicy`; it resolves and confines every path itself and writes nothing.",
        ),
        (
            "instruction_files.rs",
            "walk_nested",
            "Lists the accessible work folder at staging, before any capsule command runs, for \
             files named like an `inference.instruction_files` entry. It reads directory entries \
             only, follows and enters no link, and writes nothing; the listed files are read \
             through `resolve_relpath_beneath_root` and `open_no_follow`, never here.",
        ),
        (
            "resource_plane.rs",
            "list_export_files",
            "The resource plane's listing of a declared export root, on `open_no_follow`'s \
             terms.",
        ),
        (
            "retention.rs",
            "prune_formations",
            "A formation directory under `~/.murmur/formations` and its records subtree under `~/.murmur/records`, outside every workdir.",
        ),
        (
            "retention.rs",
            "prune_records",
            "A conversation record directory under `~/.murmur/conversations`, outside every workdir.",
        ),
        (
            "retention.rs",
            "remove_record",
            "A conversation record directory, on `prune_records`'s terms.",
        ),
        (
            "retention.rs",
            "list_records",
            "The conversation record root, on `prune_records`'s terms.",
        ),
        (
            "retention.rs",
            "child_dirs",
            "Directories beneath the conversation record root, on `prune_records`'s terms.",
        ),
        (
            "retention.rs",
            "locate_message",
            "A conversation record file, on `prune_records`'s terms.",
        ),
        (
            "retention.rs",
            "find_context_dirs_in",
            "Context directories beneath the conversation record root, on `prune_records`'s terms.",
        ),
        (
            "retention.rs",
            "read_context_dir",
            "A context directory's record file, on `prune_records`'s terms.",
        ),
        (
            "retention.rs",
            "count_messages",
            "A conversation record file, on `prune_records`'s terms.",
        ),
        (
            "retention.rs",
            "stage_truncation",
            "A conversation record file being truncated, on `prune_records`'s terms.",
        ),
        (
            "retention.rs",
            "stage_with_mode",
            "`StagedRewrite`, the sibling-file rewrite of a file under `~/.murmur` — a conversation record or a private file — outside every workdir.",
        ),
        (
            "retention.rs",
            "commit",
            "`StagedRewrite`'s rename into place, on `stage_with_mode`'s terms.",
        ),
        (
            "retention.rs",
            "drop",
            "`StagedRewrite`'s removal of an uncommitted sibling file, on `stage_with_mode`'s terms.",
        ),
        (
            "runtime.rs",
            "check_resume_launchable",
            "A conversation record or harness-session file under `~/.murmur/conversations`, \
             outside every workdir.",
        ),
        (
            "runtime.rs",
            "copy_manifest_into_runtime_dir",
            "Reads the project's `murmur.yaml` from the manifest directory; the copy into the \
             runtime directory goes through `WorkdirRoot`.",
        ),
        (
            "runtime.rs",
            "load_local_skill_files",
            "A local skill source named relative to the manifest directory, read before any \
             workdir exists.",
        ),
        (
            "runtime.rs",
            "read_prompt_file",
            "A prompt file named relative to the manifest directory.",
        ),
        (
            "runtime.rs",
            "stage_session",
            "The operator's `--workdir` itself, checked by path before it is opened as the \
             accessible root, and `<manifest dir>/workdir/<session id>`, created before any root \
             exists. Nothing beneath either is touched by path.",
        ),
        (
            "sandbox.rs",
            "test_runtime_dir",
            "Test-only: creates a runtime directory's three subdirectories under a temporary \
             directory the test owns.",
        ),
        (
            "sandbox.rs",
            "build_sealed_root",
            "Host directories offered as the composed root's base, outside every workdir.",
        ),
        (
            "sandbox.rs",
            "open_o_path",
            "An `O_PATH` handle for a Landlock rule, opened from a host path the grant names; it \
             reads and writes nothing.",
        ),
        (
            "sandbox.rs",
            "open_host",
            "The ELF images of host binaries and libraries a shell grant or a derived interpreter \
             runtime names, opened to read their headers and dependencies.",
        ),
        (
            "sandbox.rs",
            "resolve_soname",
            "Host library search directories, outside every workdir.",
        ),
        (
            "sealed.rs",
            "inspect_installed_profile",
            "The host's installed AppArmor profile under `/etc/apparmor.d`.",
        ),
        (
            "sealed.rs",
            "inspect_profile_attachment",
            "`/proc/self/attr/current`, the runtime's own AppArmor attachment.",
        ),
        (
            "sealed.rs",
            "read_trimmed",
            "Host settings under `/sys` and `/proc`.",
        ),
    ];

    /// `std::fs` / `tokio::fs` functions that touch a path, matched only straight after `fs::`
    /// so a [`crate::workdir_fs::WorkdirRoot`] method of the same name is never mistaken for one.
    const RAW_FS_FUNCTIONS: &[&str] = &[
        "write(",
        "copy(",
        "rename(",
        "create_dir_all(",
        "create_dir(",
        "set_permissions(",
        "remove_file(",
        "remove_dir_all(",
        "remove_dir(",
        "hard_link(",
        "symlink(",
        "read_to_string(",
        "read(",
    ];

    /// Raw opens and existence checks, matched wherever they appear.
    const RAW_FS_CALLS: &[&str] = &[
        "File::create(",
        "File::options(",
        "OpenOptions::new(",
        "File::open(",
        "try_exists(",
    ];

    /// Path predicates, matched unless the receiver is a metadata or file-type value, which has
    /// already been read from a descriptor or a directory entry.
    const RAW_FS_PREDICATES: &[&str] = &[".exists()", ".is_file()", ".is_dir()"];

    /// The function the byte at `at` sits in: the nearest preceding `fn <name>`.
    fn enclosing_fn(source: &str, at: usize) -> &str {
        let before = &source[..at];
        let mut cursor = before.len();
        while let Some(index) = before[..cursor].rfind("fn ") {
            cursor = index;
            let boundary = index == 0
                || !before.as_bytes()[index - 1].is_ascii_alphanumeric()
                    && before.as_bytes()[index - 1] != b'_';
            let name: String = before[index + 3..]
                .chars()
                .take_while(|character| character.is_ascii_alphanumeric() || *character == '_')
                .collect();
            if boundary && !name.is_empty() {
                return &before[index + 3..index + 3 + name.len()];
            }
        }
        "<file level>"
    }

    /// Every raw filesystem call in `source`, as `(enclosing fn, call)`.
    fn raw_fs_calls(source: &str) -> Vec<(String, String)> {
        let mut found = Vec::new();
        for function in RAW_FS_FUNCTIONS {
            let pattern = format!("fs::{function}");
            for (at, _) in source.match_indices(&pattern) {
                found.push((enclosing_fn(source, at).to_string(), pattern.clone()));
            }
        }
        for call in RAW_FS_CALLS {
            for (at, _) in source.match_indices(call) {
                found.push((enclosing_fn(source, at).to_string(), call.to_string()));
            }
        }
        for predicate in RAW_FS_PREDICATES {
            for (at, _) in source.match_indices(predicate) {
                let receiver = source[..at]
                    .trim_end()
                    .rsplit(|character: char| {
                        !(character.is_ascii_alphanumeric() || "_().".contains(character))
                    })
                    .next()
                    .unwrap_or_default();
                let last = receiver
                    .trim_end_matches("()")
                    .rsplit('.')
                    .next()
                    .unwrap_or_default();
                if matches!(last, "meta" | "metadata" | "file_type") {
                    continue;
                }
                found.push((enclosing_fn(source, at).to_string(), predicate.to_string()));
            }
        }
        found
    }

    /// No scanned file touches a path with a raw filesystem call unless [`RAW_FS_EXEMPT`] says
    /// why that path is outside every workdir — and no row of that table outlives the call it
    /// excuses.
    ///
    /// A raw call resolves every component of its path, so a link the capsule planted anywhere on
    /// the way redirects it; [`crate::workdir_fs::WorkdirRoot`] is the one way the runtime
    /// touches a path beneath a workdir.
    #[test]
    fn no_workdir_file_bypasses_the_no_follow_helper() {
        let mut rows_used = vec![false; RAW_FS_EXEMPT.len()];
        for (file, source) in SCANNED {
            for (function, call) in raw_fs_calls(production_text(source)) {
                let row = RAW_FS_EXEMPT
                    .iter()
                    .position(|(exempt_file, exempt_fn, _)| {
                        exempt_file == file && *exempt_fn == function
                    });
                let Some(row) = row else {
                    panic!(
                        "{file}: `{function}` calls `{call}` directly. A path beneath the \
                         accessible workdir or the session directory goes through WorkdirRoot \
                         (crate::workdir_fs), which follows no link; a path outside every \
                         workdir gets a ({file:?}, {function:?}, <why it is not beneath a \
                         workdir>) row in RAW_FS_EXEMPT."
                    );
                };
                rows_used[row] = true;
            }
        }
        for ((file, function, _), used) in RAW_FS_EXEMPT.iter().zip(rows_used) {
            assert!(
                used,
                "RAW_FS_EXEMPT row ({file:?}, {function:?}) matches no raw filesystem call — \
                 the call is gone or moved; remove or re-key the row"
            );
        }
    }

    #[test]
    fn the_raw_call_scanner_names_the_function_and_the_call() {
        let source = "fn staged() {\n    let _ = std::fs::write(self.workdir.join(\"plans\").join(\"x\"), b\"\");\n}\nfn helper(root: &WorkdirRoot) {\n    root.write_file(\"plans/x\", b\"\", 0o644);\n    if metadata.is_file() {}\n}\n";
        assert_eq!(
            raw_fs_calls(source),
            vec![("staged".to_string(), "fs::write(".to_string())]
        );
        assert_eq!(
            helper_write_arguments(source),
            vec!["\"plans/x\"".to_string()]
        );
        assert_eq!(resolve_entry_name("\"plans/x\"").as_deref(), Some("plans"));
        assert_eq!(
            resolve_entry_name("format!(\"{BLOB_DIR_NAME}/{sha256}\")").as_deref(),
            Some("blobs")
        );
        assert_eq!(
            resolve_entry_name("format!(\"tools/{name}/x\")").as_deref(),
            Some("tools")
        );
        assert_eq!(
            resolve_entry_name("CHILDREN_DIR").as_deref(),
            Some("children")
        );
        assert_eq!(resolve_entry_name("&rel"), None);
        assert_eq!(resolve_entry_name("format!(\"{}/x\", a)"), None);
    }

    /// The files the runtime owns are in the runtime directory, so the workdir declaration names
    /// none of them.
    #[test]
    fn runtime_writes_lists_none_of_the_runtime_directory_files() {
        for write in runtime_writes() {
            let last = write.path.rsplit('/').next().unwrap_or_default();
            assert!(
                !MOVED_OUT_OF_THE_WORKDIR.contains(&last),
                "{} is declared as a workdir write",
                write.path
            );
        }
    }

    /// Every shell log is a record in the session's records directory, so no row names `logs`.
    #[test]
    fn no_declared_path_is_a_shell_log() {
        for write in runtime_writes() {
            assert!(
                write.path != "logs" && !write.path.starts_with("logs/"),
                "{} is declared as a workdir write",
                write.path
            );
        }
    }

    /// The declaration is exactly the two paths the capsule itself asks for: a redeemed peer
    /// handle's `peer-in` and a script capsule's own `out`. Nothing the runtime keeps for itself —
    /// a session directory, a home, an agent's results, a child's work folder — is in the work
    /// folder.
    #[test]
    fn runtime_writes_are_exactly_peer_in_and_a_script_capsules_out() {
        assert_eq!(
            runtime_writes(),
            vec![
                RuntimeWriteReport {
                    path: "peer-in".to_string(),
                    kind: RuntimeWriteKind::Directory,
                    condition: RuntimeWriteCondition::PeerFetch,
                },
                RuntimeWriteReport {
                    path: "out".to_string(),
                    kind: RuntimeWriteKind::Directory,
                    condition: RuntimeWriteCondition::ScriptSession,
                },
            ]
        );
        for write in runtime_writes() {
            for old in [".murmur", ".capsule-home", "children", "home"] {
                assert!(!write.path.contains(old), "{}", write.path);
            }
        }
    }

    /// The session's temporary folder is in the records directory, so no row names a temp store
    /// inside the workdir.
    #[test]
    fn no_declared_path_is_a_temp_store() {
        for write in runtime_writes() {
            let last = write.path.rsplit('/').next().unwrap_or_default();
            assert!(
                !last.contains("tmp"),
                "{} is declared as a workdir write",
                write.path
            );
        }
    }

    /// Serialized rows carry exactly the three documented keys, in declaration order, with the
    /// kebab-case wire values.
    ///
    /// The order is asserted off `to_string` rather than off `to_value`: `serde_json`'s `Value`
    /// holds a map that sorts its keys, so a round trip through it would prove nothing about the
    /// bytes `--explain-scope --json` and `trace.jsonl` actually carry.
    #[test]
    fn a_serialized_row_carries_exactly_path_kind_and_condition() {
        for write in runtime_writes() {
            let line = serde_json::to_string(&write).unwrap();
            assert_eq!(
                line,
                format!(
                    r#"{{"path":"{}","kind":"{}","condition":"{}"}}"#,
                    write.path,
                    write.kind.wire_name(),
                    write.condition.wire_name()
                )
            );
        }
    }

    /// No two rows name the same path, so a subtraction cannot silently depend on which one it
    /// matched.
    #[test]
    fn no_declared_path_is_reported_twice() {
        let writes = runtime_writes();
        let mut paths: Vec<&str> = writes.iter().map(|write| write.path.as_str()).collect();
        paths.sort_unstable();
        let count = paths.len();
        paths.dedup();
        assert_eq!(paths.len(), count, "a path is declared twice");
    }

    #[test]
    fn the_rendering_names_every_declared_path_relative_to_the_work_folder() {
        let writes = runtime_writes();
        let rendered = render_runtime_writes(&writes);
        assert!(rendered.contains("paths are relative to the work folder"));
        for write in &writes {
            assert!(
                rendered.contains(&write.path),
                "{} is missing from the rendering",
                write.path
            );
        }
        assert!(rendered.ends_with('\n'));
    }
}
