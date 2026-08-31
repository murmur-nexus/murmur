//! Shell commands that outran their grace period and finished in the background.
//!
//! Every shell command starts in the foreground. One that has not exited by the time
//! `lifecycle.shell_grace_secs` elapses is demoted: the turn gets a handle and carries on, the
//! command keeps running on an OS thread of its own, and its output goes to a file rather than
//! into the conversation. When it finishes, the runtime enqueues a task on this capsule with
//! [`crate::origin::TaskOrigin::Completion`] carrying the exit code and that file's path.
//!
//! The handle is not addressable. Nothing takes a work id as an argument — no tool, no host
//! import, no CLI subcommand — because a pollable handle invites a loop that burns turns waiting,
//! which is the cost demotion exists to remove. A demoted command is learned about through its
//! completion and no other way.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::{
    bindings::host::murmur::tool::run::{Status, ToolResult},
    origin::{TaskOrigin, TaskProvenance},
};

/// Prefix on every work id. Distinct from `tsk_`/`ctx_`/`ses_` so a reader of `trace.jsonl` can
/// tell at a glance which id space a value belongs to.
pub const WORK_ID_PREFIX: &str = "wrk_";

/// A fresh work id, matching `^wrk_[0-9a-f]+$`.
///
/// Time-ordered like every other id the runtime mints, so two work ids sort in the order their
/// commands were demoted.
pub fn new_work_id() -> String {
    format!("{WORK_ID_PREFIX}{}", uuid::Uuid::now_v7().simple())
}

/// Where a demoted command's full output lands, relative to the capsule workdir.
///
/// One rule, used by the writer and by every reader that has to find the file again.
pub(crate) fn output_path_for(work_id: &str) -> String {
    format!("logs/{work_id}.log")
}

/// A demoted command the session is no longer waiting for.
#[derive(Debug, Clone)]
pub(crate) struct DetachedWork {
    pub work_id: String,
    /// The program that ran, resolved the same way [`crate::shell::ShellResult::binary`] is.
    pub binary: String,
    /// The command text the model supplied, without the binary name.
    pub command: String,
    pub started_at_ms: u64,
}

/// What a demoted command left behind when it finished.
///
/// Carries no output. The bytes are on disk at [`Self::output_path`] and the agent is told where,
/// never what — a completion the model reads must not be able to grow with the command's output.
#[derive(Debug, Clone)]
pub(crate) struct DetachedCompletion {
    pub work_id: String,
    pub binary: String,
    pub command: String,
    pub exit_code: i32,
    pub duration_ms: u64,
    /// Workdir-relative, always `logs/<work_id>.log`. Named even when [`Self::error`] is set, so
    /// a reader knows where the output was meant to be.
    pub output_path: String,
    /// Bytes written to that file, header included.
    pub output_bytes: u64,
    pub resource_limit: Option<String>,
    /// The conversation the command was started from. The completion task runs under this id, so
    /// the result joins the thread that asked for it rather than opening a new one.
    pub context_id: String,
    /// Stamped [`TaskOrigin::Completion`] with the starting task's trust inherited.
    pub provenance: TaskProvenance,
    /// Some only when the wait itself failed — distinct from the command exiting non-zero.
    pub error: Option<String>,
}

impl DetachedCompletion {
    /// The `IncomingTask.message_text` the agent reads: the work id, the command, the exit code,
    /// the duration and the output path. Never the output itself.
    pub(crate) fn message_text(&self) -> String {
        let mut text = format!(
            "Background shell command finished.\n\
             work_id: {}\n\
             binary: {}\n\
             command: {}\n\
             status: {}\n\
             exit_code: {}\n\
             duration_ms: {}\n\
             output: {} ({} bytes, in the capsule workdir)",
            self.work_id,
            self.binary,
            self.command,
            self.status(),
            self.exit_code,
            self.duration_ms,
            self.output_path,
            self.output_bytes,
        );
        if let Some(limit) = &self.resource_limit {
            text.push_str(&format!("\nresource_limit: {limit}"));
        }
        if let Some(error) = &self.error {
            text.push_str(&format!("\nwait_error: {error}"));
        }
        text.push_str(
            "\n\nThe full stdout and stderr are in that file and are not reproduced here.",
        );
        text
    }

    /// `"ok"` only for a command that ran to a clean exit with nothing attributed against it.
    ///
    /// A non-zero exit, a signal kill (which [`crate::shell`] reports as `128 + signal`), an
    /// attributed resource limit and a wait that itself failed are all `"error"`: a failure is a
    /// completion, and it is told apart from success by this field rather than by its prose.
    pub(crate) fn status(&self) -> &'static str {
        if self.exit_code == 0 && self.error.is_none() && self.resource_limit.is_none() {
            "ok"
        } else {
            "error"
        }
    }
}

/// What a dispatch reports back when its command was demoted rather than finished.
#[derive(Debug, Clone)]
pub(crate) struct DetachedDispatchInfo {
    pub work_id: String,
    pub binary: String,
    pub command: String,
    /// The grace period this command outran, in milliseconds — the configured
    /// `lifecycle.shell_grace_secs`, recorded so a trace reader can see which setting was in
    /// force without consulting the manifest.
    pub grace_ms: u64,
}

/// The tool result a demoted command hands back to the turn.
///
/// Constant size by construction: nothing here derives from the command, its arguments or its
/// output, so no input can make the text the model reads grow. It names no path either — the
/// output location arrives with the completion, which is the only thing that can say the file is
/// finished being written.
pub(crate) fn demotion_tool_result(work_id: &str) -> ToolResult {
    ToolResult {
        status: Status::Passed,
        summary: Some(format!(
            "Command still running in the background as {work_id}."
        )),
        data: Some(format!(
            "The command did not finish in time and is now running in the background as {work_id}. \
             Its result will arrive later as a separate task; there is nothing to poll and no way \
             to ask about it. Continue with other work."
        )),
        data_path: None,
        truncated: false,
        metadata: vec![("work_id".to_string(), work_id.to_string())],
    }
}

/// The demoted commands of one session, and the channel their completions arrive on.
///
/// Shared between the agent loop (which registers work) and each demoted command's own thread
/// (which completes it), so the session-end sweep has one place to ask what is still running.
pub(crate) struct DetachedRegistry {
    outstanding: Mutex<BTreeMap<String, DetachedWork>>,
    completions: UnboundedSender<DetachedCompletion>,
}

impl DetachedRegistry {
    pub(crate) fn new() -> (Arc<Self>, UnboundedReceiver<DetachedCompletion>) {
        let (completions, receiver) = unbounded_channel();
        (
            Arc::new(Self {
                outstanding: Mutex::new(BTreeMap::new()),
                completions,
            }),
            receiver,
        )
    }

    /// Record a demoted command. Called before the dispatch returns its handle, so the
    /// session-end sweep cannot miss work registered after it looked.
    pub(crate) fn register(&self, work: DetachedWork) {
        self.outstanding
            .lock()
            .expect("detached registry mutex")
            .insert(work.work_id.clone(), work);
    }

    /// Removes the work id from [`Self::outstanding`] and sends the completion.
    ///
    /// The channel is unbounded, so the blocking thread that calls this never waits and needs no
    /// async context. A send that fails means the task loop is gone — the session is ending, and
    /// the result has nowhere to be delivered.
    pub(crate) fn complete(&self, completion: DetachedCompletion) {
        self.outstanding
            .lock()
            .expect("detached registry mutex")
            .remove(&completion.work_id);
        let _ = self.completions.send(completion);
    }

    /// Every command demoted and not yet completed, oldest work id first.
    pub(crate) fn outstanding(&self) -> Vec<DetachedWork> {
        self.outstanding
            .lock()
            .expect("detached registry mutex")
            .values()
            .cloned()
            .collect()
    }
}

/// Supplied by a dispatch that is allowed to demote.
///
/// `None` at a call site means the command runs to completion in the foreground, exactly as
/// every call did before demotion existed. That is what keeps `plan.rs` and the script-capsule
/// store state foreground-only: neither runs a task loop, so neither has anywhere to deliver a
/// completion.
pub(crate) struct DetachPolicy {
    pub grace: Duration,
    pub registry: Arc<DetachedRegistry>,
    /// The command text as the model wrote it, without the binary name — the same string a
    /// foreground `shell` trace record carries. `run_shell` only sees the argv it was handed, so
    /// the caller that knows what was asked for supplies it here.
    pub command: String,
    pub context_id: String,
    /// The provenance of the task that started the command. The completion is stamped
    /// `TaskProvenance::derive(TaskOrigin::Completion, Some(that trust))`, so untrust survives
    /// the round trip instead of being reset by the runtime enqueuing its own task.
    pub provenance: Option<TaskProvenance>,
}

impl DetachPolicy {
    /// The provenance the completion task carries.
    pub(crate) fn completion_provenance(&self) -> TaskProvenance {
        TaskProvenance::derive(
            TaskOrigin::Completion,
            self.provenance.map(|task| task.trust()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::{
        path::Path,
        time::{Duration, Instant},
    };

    use crate::{
        origin::TrustClass,
        sandbox::ShellEnforcement,
        shell::{run_shell, ShellOutcome},
        types::CapabilityPolicy,
    };

    fn bash_policy() -> CapabilityPolicy {
        CapabilityPolicy {
            shell_allow: vec!["bash".to_string()],
            ..Default::default()
        }
    }

    /// `run_shell` against `bash -c <script>` with a grace period, returning the outcome and the
    /// registry the completion (if any) will arrive on.
    fn detached_run(
        workdir: &Path,
        script: &str,
        grace: Duration,
    ) -> (
        ShellOutcome,
        Arc<DetachedRegistry>,
        UnboundedReceiver<DetachedCompletion>,
    ) {
        let (registry, receiver) = DetachedRegistry::new();
        let outcome = run_shell(
            "bash",
            &["-c", script],
            &[],
            workdir,
            &bash_policy(),
            &ShellEnforcement::environment_only(),
            Some(DetachPolicy {
                grace,
                registry: Arc::clone(&registry),
                command: script.to_string(),
                context_id: "ctx_detached_tests".to_string(),
                provenance: Some(TaskProvenance::derive(TaskOrigin::User, None)),
            }),
        )
        .expect("a declared binary runs");
        (outcome, registry, receiver)
    }

    /// Block until the completion lands, so the test fails with a timeout rather than hanging.
    fn await_completion(
        receiver: &mut UnboundedReceiver<DetachedCompletion>,
    ) -> DetachedCompletion {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match receiver.try_recv() {
                Ok(completion) => return completion,
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    assert!(
                        Instant::now() < deadline,
                        "timed out waiting for the detached completion"
                    );
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(error) => panic!("the completion channel closed early: {error}"),
            }
        }
    }

    /// The whole argument of demotion: the call returns long before the command does, and the
    /// result turns up afterwards with the output on disk rather than in hand.
    #[test]
    fn a_command_outrunning_the_grace_period_is_demoted() {
        let workdir = tempfile::tempdir().unwrap();
        let started = Instant::now();
        // The output word is spelled nowhere in the command, so finding it in the completion
        // would mean the output leaked into the turn rather than merely being echoed back.
        let (outcome, registry, mut receiver) = detached_run(
            workdir.path(),
            "sleep 3; printf 'fin%s' 'ished-marker'",
            Duration::from_secs(1),
        );

        let info = match outcome {
            ShellOutcome::Detached(info) => info,
            ShellOutcome::Finished(result) => {
                panic!(
                    "a three-second command must not finish inside a one-second grace: {result:?}"
                )
            }
        };
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "the call returned only after the command did"
        );
        assert!(
            info.work_id.starts_with(WORK_ID_PREFIX)
                && info.work_id[WORK_ID_PREFIX.len()..]
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "work id {} must match ^wrk_[0-9a-f]+$",
            info.work_id
        );
        assert_eq!(info.grace_ms, 1_000);
        assert_eq!(
            registry
                .outstanding()
                .iter()
                .map(|work| work.work_id.clone())
                .collect::<Vec<_>>(),
            vec![info.work_id.clone()],
            "the work is registered before the handle is returned"
        );

        let completion = await_completion(&mut receiver);
        assert_eq!(completion.work_id, info.work_id);
        assert_eq!(completion.exit_code, 0);
        assert_eq!(completion.status(), "ok");
        assert_eq!(
            completion.output_path,
            format!("logs/{}.log", info.work_id),
            "the completion names logs/<work_id>.log"
        );
        assert_eq!(completion.provenance.origin(), TaskOrigin::Completion);
        assert_eq!(completion.provenance.trust(), TrustClass::Trusted);
        assert_eq!(completion.context_id, "ctx_detached_tests");
        assert!(
            registry.outstanding().is_empty(),
            "a completed command is no longer outstanding"
        );

        let log = std::fs::read_to_string(workdir.path().join(&completion.output_path))
            .expect("the completion's output file exists");
        assert!(
            log.contains("finished-marker"),
            "the output is in the log: {log}"
        );
        assert!(
            !completion.message_text().contains("finished-marker"),
            "the output never reaches the agent: {}",
            completion.message_text()
        );
    }

    /// A command that beats the grace period behaves exactly as it did before demotion existed.
    #[test]
    fn a_fast_command_finishes_in_the_foreground() {
        let workdir = tempfile::tempdir().unwrap();
        let (outcome, registry, mut receiver) =
            detached_run(workdir.path(), "echo hi", Duration::from_secs(10));

        let result = match outcome {
            ShellOutcome::Finished(result) => result,
            ShellOutcome::Detached(info) => {
                panic!("`echo hi` must not be demoted: {}", info.work_id)
            }
        };
        assert_eq!(result.stdout.trim(), "hi");
        assert_eq!(result.exit_code, 0);
        assert!(!result.truncated);
        assert!(result.full_output_path.is_none());
        assert!(registry.outstanding().is_empty());
        assert!(
            matches!(
                receiver.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ),
            "a foreground command produces no completion"
        );
    }

    /// A failure is a completion, told apart from success by a field.
    #[test]
    fn a_failing_detached_command_completes_with_its_exit_code() {
        let workdir = tempfile::tempdir().unwrap();
        let (outcome, _registry, mut receiver) = detached_run(
            workdir.path(),
            "sleep 3; echo to-stderr >&2; exit 7",
            Duration::from_secs(1),
        );
        assert!(matches!(outcome, ShellOutcome::Detached(_)));

        let completion = await_completion(&mut receiver);
        assert_eq!(completion.exit_code, 7);
        assert_eq!(completion.status(), "error");
        assert!(
            completion.message_text().contains("exit_code: 7"),
            "the message names the exit code: {}",
            completion.message_text()
        );

        let log = std::fs::read_to_string(workdir.path().join(&completion.output_path)).unwrap();
        let stderr_section = log
            .split("Stderr:")
            .nth(1)
            .expect("the log has a stderr section");
        assert!(
            stderr_section.contains("to-stderr"),
            "stderr is in the log's Stderr section: {log}"
        );

        let (outcome, _registry, mut receiver) =
            detached_run(workdir.path(), "sleep 3; exit 0", Duration::from_secs(1));
        assert!(matches!(outcome, ShellOutcome::Detached(_)));
        assert_eq!(await_completion(&mut receiver).status(), "ok");
    }

    /// No input can make the demotion text grow: two work ids of equal length cost the same, and
    /// the cost is small.
    #[test]
    fn the_demotion_result_is_a_constant_size() {
        let reduce = |work_id: &str| {
            let result = demotion_tool_result(work_id);
            result
                .data
                .or(result.summary)
                .unwrap_or_else(|| "tool returned no data".to_string())
        };
        let first = reduce(&new_work_id());
        let second = reduce(&new_work_id());

        assert_eq!(
            first.len(),
            second.len(),
            "two work ids of equal length must produce texts of equal size"
        );
        // Not token equality: `cl100k_base` splits one random hex id into a different number of
        // pieces than another of the same length, which says nothing about this text. The claim
        // that matters is that the cost is bounded and no input moves it.
        for text in [&first, &second] {
            let tokens = crate::agent::count_tokens(text);
            assert!(
                tokens < 120,
                "the demotion result costs {tokens} tokens, over the 120-token ceiling"
            );
        }
        assert!(
            !first.contains("logs/"),
            "the demotion result names no path: {first}"
        );
    }

    /// The rule the slice is built on, checked against the source rather than trusted.
    ///
    /// A demoted command is learned about through its completion and no other way: no tool name,
    /// host import or manifest-visible entry point takes a work id as an argument, and
    /// `build_tool_inventory` gains no entry for one.
    #[test]
    fn no_surface_takes_a_work_id() {
        const RULE: &str = "a demoted command is learned about through its completion and no \
                            other way: nothing the model or a host embedder can reach may take a \
                            work id as an argument";

        // Split so the needles are not spelled literally on the lines that use them — this test
        // sweeps its own file too, and a self-match would be a permanent false positive.
        let work_id = concat!("work", "_id");
        let parameter = format!("{work_id}:");
        let json_key = format!(".get(\"{work_id}\")");
        let schema = concat!("input", "_schema");

        let sources = crate_sources();
        let mut offences = Vec::new();
        for (path, source) in &sources {
            for (number, line) in source.lines().enumerate() {
                let line = line.trim();
                let at = format!("{path}:{}", number + 1);

                // A public signature naming one would be a lookup surface for anything outside
                // this crate — a CLI subcommand, a host embedder.
                if (line.starts_with("pub fn ") || line.starts_with("pub async fn "))
                    && line.contains(&parameter)
                {
                    offences.push(format!("{at}: public function takes a work id — {line}"));
                }
                // A tool's input parser reading the key would let the model name one.
                if line.contains(&json_key) {
                    offences.push(format!("{at}: tool input is parsed for a work id — {line}"));
                }
                // A tool manifest declaring it would put one in front of the model.
                if line.contains(schema) && line.contains(work_id) {
                    offences.push(format!("{at}: a tool schema declares a work id — {line}"));
                }
            }
        }

        let inventory = sources
            .iter()
            .find(|(path, _)| path.ends_with("inventory.rs"))
            .map(|(_, source)| source)
            .expect("agent/inventory.rs is part of this crate");
        if inventory.contains(work_id) || inventory.contains(WORK_ID_PREFIX) {
            offences.push("agent/inventory.rs: the tool inventory mentions a work id".to_string());
        }

        assert!(offences.is_empty(), "{RULE}\n{}", offences.join("\n"));
    }

    /// Every `.rs` file under this crate's `src`, as `(path relative to src, contents)`.
    fn crate_sources() -> Vec<(String, String)> {
        let root = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src"));
        let mut sources = Vec::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(&dir).expect("the crate's src directory is readable") {
                let entry = entry.expect("a readable directory entry");
                let path = entry.path();
                if path.is_dir() {
                    pending.push(path);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    let relative = path
                        .strip_prefix(root)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .into_owned();
                    sources.push((
                        relative,
                        std::fs::read_to_string(&path).expect("a readable source file"),
                    ));
                }
            }
        }
        assert!(
            sources.len() > 10,
            "the source sweep found only {} files, so it is not sweeping the crate",
            sources.len()
        );
        sources
    }
}
