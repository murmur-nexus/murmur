//! What the runtime itself writes inside the capsule's workdir, declared rather than discovered.
//!
//! A consumer for whom the workdir *is* the deliverable — a benchmark harness diffing it to score
//! a patch, a `git-worktree` workspace whose diff becomes a pull request — has to answer "what did
//! the *capsule* change?". Without a declaration that answer is a hand-maintained exclusion list
//! that goes stale the moment the runtime gains an entry, and silently: nothing tells the consumer
//! the set grew. [`runtime_writes`] is that declaration, carried by
//! [`crate::containment::ScopeReport`] and therefore printed by `mur run --explain-scope` and
//! recorded verbatim in `trace.jsonl`'s `session_start.effective_grants`.
//!
//! # Path form
//!
//! Every [`RuntimeWriteReport::path`] is relative to the **accessible workdir** — the directory a
//! `--workdir` run hands the capsule, and the directory a consumer diffs — and carries the
//! **literal** segment `<session-id>` ([`SESSION_ID_PLACEHOLDER`]) wherever a real session id
//! appears. `mur run --explain-scope` answers before a session exists, so there is no id to
//! substitute; keeping the placeholder in both renderings is what lets a consumer capture the
//! declaration ahead of a run and compare it byte for byte with the one the trace carries.
//! `session_start.session_id` supplies the substitution.
//!
//! Without `--workdir` there is no separate deliverable: the accessible workdir *is* the session
//! directory, under `<manifest dir>/workdir/<session id>/`, and the runtime owns all of it. The
//! declaration describes the `--workdir` layout in both cases, so that it is one fixed answer a
//! consumer can compare against rather than two.
//!
//! # What is filtered and what is labelled
//!
//! [`RuntimeWriteCondition`] names when a path is present. Only [`RuntimeWriteCondition::Sealed`]
//! filters the set: the enforcement tier is host state the report has already probed, so a report
//! that omitted the sealed rows on a host that will use them — or listed them on a host that
//! cannot — would be describing a different launch than the one it is about. Every other condition
//! turns on what the session does after it starts, which no report can know, so those rows are
//! always declared and the condition says when the path appears.
//!
//! # Adding an entry
//!
//! Add a row to [`DECLARED_WRITES`]. `every_workdir_write_is_declared_or_exempted` reads the
//! runtime's own source text and fails until a new `workdir.join(...)` is either declared here or
//! listed in that test's exempt tables with a written reason.

use serde::Serialize;

use crate::sandbox::EnforcementTier;

/// The literal segment a declared path carries in place of a real session id.
///
/// Substituted by the consumer, from `session_start.session_id`. Never expanded here: see the
/// module docs for why the two renderings have to stay identical.
pub const SESSION_ID_PLACEHOLDER: &str = "<session-id>";

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

/// Which of the two workdirs an entry lands in.
///
/// The distinction only exists under `--workdir`, where [`Self::Session`] paths sit under
/// `.murmur/<session-id>/` inside the accessible workdir. Both are reported relative to the
/// accessible workdir, so a consumer that excludes the single prefix `.murmur/` covers every
/// [`Self::Session`] row without reading their paths at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeWriteScope {
    /// Directly inside the directory the capsule was asked to produce.
    Accessible,
    /// Inside this session's own bookkeeping directory, `.murmur/<session-id>/`.
    Session,
}

impl RuntimeWriteScope {
    /// The wire name, identical to what [`Serialize`] emits.
    #[must_use]
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::Accessible => "accessible",
            Self::Session => "session",
        }
    }
}

/// When a declared entry is present on disk.
///
/// [`Self::Sealed`] is the one value that also decides whether the row appears in the report at
/// all — see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeWriteCondition {
    /// Written by every session, before the capsule runs.
    Always,
    /// Written only when the launch was given `--workdir`.
    WorkdirProvided,
    /// Written by the agent loop, so absent from a script capsule's session.
    AgentSession,
    /// Written by a script capsule, whose WASI preopen `.` is the accessible workdir itself.
    ScriptSession,
    /// Written when a shell or native tool call runs.
    Shell,
    /// Written when a peer handle is redeemed, which needs `capabilities.peer_fetch.allow`.
    PeerFetch,
    /// Written when this capsule launches a sub-capsule, which needs `capabilities.spawn.allow`.
    Spawn,
    /// Written by a capsule that was itself launched as a delegated child, into its own workdir.
    Delegated,
    /// Written only where the enforcement tier composes a sealed root.
    Sealed,
}

impl RuntimeWriteCondition {
    /// The wire name, identical to what [`Serialize`] emits.
    #[must_use]
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::Always => "always",
            Self::WorkdirProvided => "workdir-provided",
            Self::AgentSession => "agent-session",
            Self::ScriptSession => "script-session",
            Self::Shell => "shell",
            Self::PeerFetch => "peer-fetch",
            Self::Spawn => "spawn",
            Self::Delegated => "delegated",
            Self::Sealed => "sealed",
        }
    }
}

/// One path the runtime writes inside the capsule's workdir.
///
/// Field names are the JSON keys verbatim: `path`, `kind`, `scope`, `condition`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuntimeWriteReport {
    /// Relative to the accessible workdir, carrying the literal [`SESSION_ID_PLACEHOLDER`].
    pub path: String,
    pub kind: RuntimeWriteKind,
    pub scope: RuntimeWriteScope,
    pub condition: RuntimeWriteCondition,
}

/// One row of [`DECLARED_WRITES`]: an entry named relative to its own scope's root.
///
/// The `.murmur/<session-id>/` prefix a [`RuntimeWriteScope::Session`] entry carries in the report
/// is composed by [`DeclaredWrite::report`] rather than written into each row, so the placeholder
/// appears in one place and a row states only the part that is about the entry itself.
struct DeclaredWrite {
    entry: &'static str,
    kind: RuntimeWriteKind,
    scope: RuntimeWriteScope,
    condition: RuntimeWriteCondition,
}

impl DeclaredWrite {
    fn report(&self) -> RuntimeWriteReport {
        let path = match self.scope {
            RuntimeWriteScope::Accessible => self.entry.to_string(),
            RuntimeWriteScope::Session => {
                format!(".murmur/{SESSION_ID_PLACEHOLDER}/{}", self.entry)
            }
        };
        RuntimeWriteReport {
            path,
            kind: self.kind,
            scope: self.scope,
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

/// Every path the runtime writes inside a capsule's workdir, in report order.
///
/// The single source of truth: `mur run --explain-scope` reads it, `trace.jsonl` carries what that
/// produced, and `every_workdir_write_is_declared_or_exempted` fails the suite when the runtime's
/// source grows a workdir entry this table does not name.
const DECLARED_WRITES: &[DeclaredWrite] = &[
    // ── The accessible workdir's own top level ────────────────────────────────
    DeclaredWrite {
        // The one prefix a consumer excludes. Created by `stage_session` before anything else.
        entry: ".murmur/<session-id>",
        kind: RuntimeWriteKind::Directory,
        scope: RuntimeWriteScope::Accessible,
        condition: RuntimeWriteCondition::Always,
    },
    DeclaredWrite {
        // The task input the capsule reads by relative path, rewritten on every incoming A2A
        // message and removed again under `lifecycle.task_acceptance: queue`.
        entry: "task.md",
        kind: RuntimeWriteKind::File,
        scope: RuntimeWriteScope::Accessible,
        condition: RuntimeWriteCondition::Always,
    },
    DeclaredWrite {
        // Copied from the manifest directory so the agent can reference it from `.`; skipped when
        // the destination already exists, which is why a `--workdir` holding its own manifest is
        // never overwritten.
        entry: "murmur.yaml",
        kind: RuntimeWriteKind::File,
        scope: RuntimeWriteScope::Accessible,
        condition: RuntimeWriteCondition::WorkdirProvided,
    },
    DeclaredWrite {
        // `logs/shell-<ms>.log` for a foreground command whose output was truncated, and
        // `logs/<work-id>.log` for a demoted one. Stays at the top level because the shell tool
        // returns this path, workdir-relative, in the result the model reads.
        entry: "logs",
        kind: RuntimeWriteKind::Directory,
        scope: RuntimeWriteScope::Accessible,
        condition: RuntimeWriteCondition::Shell,
    },
    DeclaredWrite {
        // Where a redeemed peer handle's bytes land. A documented resource-plane path.
        entry: "peer-in",
        kind: RuntimeWriteKind::Directory,
        scope: RuntimeWriteScope::Accessible,
        condition: RuntimeWriteCondition::PeerFetch,
    },
    DeclaredWrite {
        // One directory per sub-capsule launch, each its own accessible workdir. Under `.murmur/`
        // already, but beside the session directory rather than inside it: a child outlives no
        // session but is named for the capsule, not for the parent's id.
        entry: ".murmur/children",
        kind: RuntimeWriteKind::Directory,
        scope: RuntimeWriteScope::Accessible,
        condition: RuntimeWriteCondition::Spawn,
    },
    DeclaredWrite {
        // A script capsule's WASI preopen `.` is the accessible workdir, so its `out/result.txt`
        // lands here rather than in the session directory the agent loop writes to.
        entry: "out",
        kind: RuntimeWriteKind::Directory,
        scope: RuntimeWriteScope::Accessible,
        condition: RuntimeWriteCondition::ScriptSession,
    },
    DeclaredWrite {
        // A delegated child's own record of how it ended, read by the launcher's watcher.
        entry: "completion.json",
        kind: RuntimeWriteKind::File,
        scope: RuntimeWriteScope::Accessible,
        condition: RuntimeWriteCondition::Delegated,
    },
    // ── The per-session directory ─────────────────────────────────────────────
    DeclaredWrite {
        entry: "tools",
        kind: RuntimeWriteKind::Directory,
        scope: RuntimeWriteScope::Session,
        condition: RuntimeWriteCondition::Always,
    },
    DeclaredWrite {
        entry: "trace.jsonl",
        kind: RuntimeWriteKind::File,
        scope: RuntimeWriteScope::Session,
        condition: RuntimeWriteCondition::Always,
    },
    DeclaredWrite {
        // Trace bodies too large to inline, addressed by their own sha256.
        entry: "blobs",
        kind: RuntimeWriteKind::Directory,
        scope: RuntimeWriteScope::Session,
        condition: RuntimeWriteCondition::Always,
    },
    DeclaredWrite {
        // `bootstrap.log`, `otel.log` and one `hook-<name>.log` per failing hook.
        entry: "logs",
        kind: RuntimeWriteKind::Directory,
        scope: RuntimeWriteScope::Session,
        condition: RuntimeWriteCondition::AgentSession,
    },
    DeclaredWrite {
        // `result.txt`, `result_<task-id>.txt` and `compaction-summaries.jsonl`.
        entry: "out",
        kind: RuntimeWriteKind::Directory,
        scope: RuntimeWriteScope::Session,
        condition: RuntimeWriteCondition::AgentSession,
    },
    DeclaredWrite {
        entry: "MURMUR.md",
        kind: RuntimeWriteKind::File,
        scope: RuntimeWriteScope::Session,
        condition: RuntimeWriteCondition::AgentSession,
    },
    DeclaredWrite {
        // One `plan-<n>.json` per submitted plan.
        entry: "plans",
        kind: RuntimeWriteKind::Directory,
        scope: RuntimeWriteScope::Session,
        condition: RuntimeWriteCondition::AgentSession,
    },
    DeclaredWrite {
        // `$HOME` for every shell and native tool subprocess. Reached only through the variable;
        // nothing outside the runtime names the literal path.
        entry: ".capsule-home",
        kind: RuntimeWriteKind::Directory,
        scope: RuntimeWriteScope::Session,
        condition: RuntimeWriteCondition::Shell,
    },
    DeclaredWrite {
        // The bind source the composed root mounts at `/tmp`. The capsule sees `/tmp` and never a
        // workdir path.
        entry: ".mur-tmp",
        kind: RuntimeWriteKind::Directory,
        scope: RuntimeWriteScope::Session,
        condition: RuntimeWriteCondition::Sealed,
    },
    DeclaredWrite {
        // The bind sources for the composed root's synthetic `/etc/passwd` and `/etc/group`.
        entry: ".mur-etc",
        kind: RuntimeWriteKind::Directory,
        scope: RuntimeWriteScope::Session,
        condition: RuntimeWriteCondition::Sealed,
    },
];

/// Every path the runtime writes inside this launch's workdir, in declaration order.
///
/// Pure and filesystem-free: `mur run --explain-scope` returns before a workdir exists, so a
/// declaration that touched one would make the diagnostic create the thing it describes.
///
/// `tier` is the tier the session would actually install — [`crate::sandbox::applied_tier`], the
/// host's own tier capped by the declared floor — never the declared class on its own. A capsule
/// declaring `scoped` on a sealed-capable host composes no root; under an unmet `sealed` floor the
/// launch refuses anyway. Either way the honest answer is the set the mechanism that will really
/// run writes.
pub(crate) fn runtime_writes(tier: EnforcementTier) -> Vec<RuntimeWriteReport> {
    let sealed = tier == EnforcementTier::KernelSealed;
    DECLARED_WRITES
        .iter()
        .filter(|declared| sealed || declared.condition != RuntimeWriteCondition::Sealed)
        .map(DeclaredWrite::report)
        .collect()
}

/// The `Runtime writes` block of `mur run --explain-scope`'s human rendering.
///
/// Ends with a newline. Shared with nothing else today, but kept beside the declaration for the
/// reason [`crate::containment::render_read_only`] is kept beside its own: a second renderer is a
/// second place for the wording to drift from the data.
#[must_use]
pub fn render_runtime_writes(writes: &[RuntimeWriteReport]) -> String {
    let mut out = String::new();
    out.push_str(
        "  paths are relative to the accessible workdir; `<session-id>` is a literal segment, \
         replaced by the run's own session id\n",
    );
    if writes.is_empty() {
        out.push_str("  runtime writes: <none>\n");
        return out;
    }
    out.push_str("  runtime writes:\n");
    for write in writes {
        out.push_str(&format!(
            "    - {} ({}, {}, {})\n",
            write.path,
            write.kind.wire_name(),
            write.scope.wire_name(),
            write.condition.wire_name()
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The source files carrying a production `workdir.join(...)`, each scanned by
    /// [`every_workdir_write_is_declared_or_exempted`].
    ///
    /// `plan.rs` has no production occurrence today and is scanned anyway, so a plan step that
    /// grows one is covered without anybody remembering to widen this list.
    const SCANNED: &[(&str, &str)] = &[
        ("agent.rs", include_str!("agent.rs")),
        ("agent/inventory.rs", include_str!("agent/inventory.rs")),
        ("agent/process.rs", include_str!("agent/process.rs")),
        ("child_launch.rs", include_str!("child_launch.rs")),
        ("delegation.rs", include_str!("delegation.rs")),
        ("hooks.rs", include_str!("hooks.rs")),
        ("murmur_md.rs", include_str!("murmur_md.rs")),
        ("otel.rs", include_str!("otel.rs")),
        ("plan.rs", include_str!("plan.rs")),
        ("protected_paths.rs", include_str!("protected_paths.rs")),
        ("recipes.rs", include_str!("recipes.rs")),
        ("resource_plane.rs", include_str!("resource_plane.rs")),
        ("runtime.rs", include_str!("runtime.rs")),
        ("sandbox.rs", include_str!("sandbox.rs")),
        ("sealed.rs", include_str!("sealed.rs")),
        ("shell.rs", include_str!("shell.rs")),
        ("tool_annotations.rs", include_str!("tool_annotations.rs")),
        ("trace.rs", include_str!("trace.rs")),
        ("trace_blobs.rs", include_str!("trace_blobs.rs")),
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
        ("SEALED_TMP_DIR_NAME", crate::sealed::SEALED_TMP_DIR_NAME),
        (
            "SEALED_ETC_STAGING_DIR_NAME",
            crate::sealed::SEALED_ETC_STAGING_DIR_NAME,
        ),
        (
            "SYNTHETIC_HOME_DIR_NAME",
            crate::shell::SYNTHETIC_HOME_DIR_NAME,
        ),
    ];

    /// Entry names the scanner resolves that are deliberately not declared, each with the reason.
    const EXEMPT: &[(&str, &str)] = &[
        (
            "input.txt",
            "Read, never written: the fallback name `read_task` tries after `task.md`. Source \
             text cannot tell a read from a write, so a path the runtime only ever opens for \
             reading is exempted here rather than declared as something the runtime produces.",
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
            "recipes.rs",
            "dir",
            "A directory name from the recipe detector's own candidate list, opened to see \
             whether the workdir has it.",
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
            "&relative",
            "A resource path a tool call named, resolved to check it is inside the workdir \
             before the file is read.",
        ),
        (
            "runtime.rs",
            "&stored_path",
            "Where a redeemed peer handle's bytes go, composed by `peer_handoff::stored_path_for` \
             under `peer-in/`. Declared through that directory's own entry, which is the name a \
             consumer excludes.",
        ),
        (
            "shell.rs",
            "&output_path",
            "A demoted command's own log path, `logs/<work-id>.log`, composed by \
             `detached::output_path_for` and joined here only to measure the file's size. \
             Declared through the `logs` entry.",
        ),
        (
            "delegation.rs",
            "format!( \".{COMPLETION_FILE}.{}.tmp\", uuid::Uuid::now_v7().simple() )",
            "The temporary file `completion.json` is written through and renamed from. It exists \
             only between two syscalls in the same function, so a consumer diffing a finished \
             run never sees it.",
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

    /// Every `workdir.join(<arg>)` in `source`, as the argument's source text.
    ///
    /// The receiver is matched on its `workdir` suffix rather than by name, which is what makes
    /// `accessible_workdir.join(`, `session_workdir.join(`, `child_workdir.join(` and
    /// `parent_accessible_workdir\n    .join(` all one case.
    fn workdir_join_arguments(source: &str) -> Vec<String> {
        let mut found = Vec::new();
        let bytes = source.as_bytes();
        let mut cursor = 0;
        while let Some(offset) = source[cursor..].find(".join(") {
            let start = cursor + offset;
            cursor = start + ".join(".len();
            if !source[..start].trim_end().ends_with("workdir") {
                continue;
            }
            let mut depth = 1usize;
            let mut index = cursor;
            while index < bytes.len() && depth > 0 {
                match bytes[index] {
                    b'(' => depth += 1,
                    b')' => depth -= 1,
                    _ => {}
                }
                index += 1;
            }
            assert!(
                depth == 0,
                "unbalanced parentheses after a join into a workdir"
            );
            // Collapsed to one line: an argument's line breaks and indentation are `rustfmt`'s
            // business, and a table row that had to reproduce them would fail on a reflow.
            found.push(
                source[cursor..index - 1]
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" "),
            );
        }
        found
    }

    /// The entry name a `workdir.join(...)` argument stands for, or `None` when the source text
    /// cannot say — a variable, a field access, a formatted string.
    fn resolve_entry_name(argument: &str) -> Option<String> {
        if let Some(rest) = argument.strip_prefix('"') {
            return rest.split_once('"').map(|(literal, _)| literal.to_string());
        }
        let last = argument.rsplit("::").next().unwrap_or(argument);
        let is_constant_name = !last.is_empty()
            && last
                .chars()
                .all(|character| character.is_ascii_uppercase() || character == '_');
        if !is_constant_name {
            return None;
        }
        Some(
            CONSTANTS
                .iter()
                .find(|(name, _)| *name == last)
                .unwrap_or_else(|| {
                    panic!(
                        "a workdir.join({argument}) names the constant {last}, which this test's \
                         CONSTANTS table does not pair with a value. Add \
                         (\"{last}\", <path to the constant>) so a rename or a value change is \
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
    /// The input is the runtime's own source text rather than a list somebody maintains, so a
    /// developer who adds a `workdir.join("...")` fails this test until they decide which it is.
    #[test]
    fn every_workdir_write_is_declared_or_exempted() {
        let declared: Vec<&str> = DECLARED_WRITES
            .iter()
            .map(DeclaredWrite::first_segment)
            .collect();

        let mut scanned = 0usize;
        for (file, source) in SCANNED {
            for argument in workdir_join_arguments(production_text(source)) {
                scanned += 1;
                let Some(entry) = resolve_entry_name(&argument) else {
                    assert!(
                        DYNAMIC_EXEMPT
                            .iter()
                            .any(|(exempt_file, exempt_argument, _)| exempt_file == file
                                && *exempt_argument == argument),
                        "{file} writes workdir.join({argument}) into a workdir, and the argument \
                         is neither a string literal nor a named constant, so this test cannot \
                         say what it names. Add ({file:?}, {argument:?}, <written reason>) to \
                         this test's DYNAMIC_EXEMPT table."
                    );
                    continue;
                };
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
            scanned >= 50,
            "the workdir.join scan found only {scanned} occurrences across {} files, far below \
             what the runtime has — the scanner is broken, not the runtime",
            SCANNED.len()
        );

        for (name, _) in EXEMPT {
            assert!(
                !declared.contains(name),
                "`{name}` is listed as exempt and is also declared — remove one"
            );
        }
    }

    /// Every source file with a production `workdir.join(...)` is in `SCANNED`.
    ///
    /// Without this, a new module writing into a workdir would be invisible to the scan above and
    /// the declaration would go quietly stale, which is the exact failure this whole module
    /// exists to close.
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
                    // This module's own prose and failure messages quote the pattern being
                    // searched for; it declares the writes rather than performing any.
                    if path
                        .file_name()
                        .is_some_and(|name| name == "workdir_writes.rs")
                    {
                        continue;
                    }
                    let source = std::fs::read_to_string(&path).unwrap();
                    if workdir_join_arguments(production_text(&source)).is_empty() {
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

    /// Sealed is the one condition that changes the reported set, and it adds nothing a consumer
    /// diffing the accessible workdir's top level would see.
    #[test]
    fn the_sealed_tier_adds_exactly_the_two_sealed_session_entries() {
        let scoped = runtime_writes(EnforcementTier::KernelFull);
        let sealed = runtime_writes(EnforcementTier::KernelSealed);

        assert!(
            scoped.iter().all(|write| sealed.contains(write)),
            "the sealed declaration must be a superset of the scoped one"
        );
        let added: Vec<&RuntimeWriteReport> = sealed
            .iter()
            .filter(|write| !scoped.contains(write))
            .collect();
        assert_eq!(
            added
                .iter()
                .map(|write| write.path.as_str())
                .collect::<Vec<_>>(),
            vec![
                ".murmur/<session-id>/.mur-tmp",
                ".murmur/<session-id>/.mur-etc"
            ]
        );
        for write in added {
            assert_eq!(write.condition, RuntimeWriteCondition::Sealed);
            assert_eq!(write.scope, RuntimeWriteScope::Session);
            assert_eq!(write.kind, RuntimeWriteKind::Directory);
        }
    }

    /// Every non-sealed tier reports the same set, so a host's Landlock or seccomp availability
    /// cannot quietly change what a consumer subtracts.
    #[test]
    fn only_the_sealed_tier_changes_the_declared_set() {
        let baseline = runtime_writes(EnforcementTier::EnvironmentOnly);
        assert_eq!(baseline, runtime_writes(EnforcementTier::KernelSeccompOnly));
        assert_eq!(baseline, runtime_writes(EnforcementTier::KernelFull));
        assert!(baseline
            .iter()
            .all(|write| write.condition != RuntimeWriteCondition::Sealed));
    }

    /// Serialized rows carry exactly the four documented keys, in declaration order, with the
    /// kebab-case wire values.
    ///
    /// The order is asserted off `to_string` rather than off `to_value`: `serde_json`'s `Value`
    /// holds a map that sorts its keys, so a round trip through it would prove nothing about the
    /// bytes `--explain-scope --json` and `trace.jsonl` actually carry.
    #[test]
    fn a_serialized_row_carries_exactly_path_kind_scope_and_condition() {
        for write in runtime_writes(EnforcementTier::KernelSealed) {
            let line = serde_json::to_string(&write).unwrap();
            assert_eq!(
                line,
                format!(
                    r#"{{"path":"{}","kind":"{}","scope":"{}","condition":"{}"}}"#,
                    write.path,
                    write.kind.wire_name(),
                    write.scope.wire_name(),
                    write.condition.wire_name()
                )
            );
        }
    }

    /// A session-scoped entry is reported under the one prefix a consumer excludes, and an
    /// accessible one never is.
    #[test]
    fn session_scoped_paths_carry_the_murmur_prefix_and_the_literal_session_id() {
        for write in runtime_writes(EnforcementTier::KernelSealed) {
            match write.scope {
                RuntimeWriteScope::Session => {
                    let prefix = format!(".murmur/{SESSION_ID_PLACEHOLDER}/");
                    assert!(
                        write.path.starts_with(&prefix),
                        "{} is session-scoped but not under {prefix}",
                        write.path
                    );
                }
                RuntimeWriteScope::Accessible => assert!(
                    !write
                        .path
                        .starts_with(&format!(".murmur/{SESSION_ID_PLACEHOLDER}/")),
                    "{} is accessible-scoped but sits inside the session directory",
                    write.path
                ),
            }
            assert!(
                !write.path.contains("ses_"),
                "{} carries a real session id where the placeholder belongs",
                write.path
            );
        }
    }

    /// No two rows name the same path, so a subtraction cannot silently depend on which one it
    /// matched.
    #[test]
    fn no_declared_path_is_reported_twice() {
        let writes = runtime_writes(EnforcementTier::KernelSealed);
        let mut paths: Vec<&str> = writes.iter().map(|write| write.path.as_str()).collect();
        paths.sort_unstable();
        let count = paths.len();
        paths.dedup();
        assert_eq!(paths.len(), count, "a path is declared twice");
    }

    #[test]
    fn the_rendering_names_every_declared_path_and_the_placeholder_rule() {
        let writes = runtime_writes(EnforcementTier::KernelSealed);
        let rendered = render_runtime_writes(&writes);
        assert!(rendered.contains("`<session-id>` is a literal segment"));
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
