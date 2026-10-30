//! Shell commands that were moved out of the foreground and run on in the background.
//!
//! Every shell command starts in the foreground. One is demoted when it has not exited by the
//! time a declared `lifecycle.shell_grace_secs` elapses, or when the call waiting on it is
//! abandoned or its task is canceled: the turn gets a work id and carries on, the command keeps
//! running on an OS thread of its own, and its output is written to a log in the session's
//! records directory as it is read. With no `shell_grace_secs` a command is never demoted on a
//! clock.
//!
//! The work id is an address. [`DetachedRegistry`] keeps every demoted command of the session,
//! running or finished, and the runtime-provided `background-command` tool
//! ([`crate::background_command`]), offered when `shell_grace_secs` is set, reads its state,
//! waits for it or stops it by that id. When a
//! command finishes, the runtime also enqueues a task on this capsule with
//! [`crate::origin::TaskOrigin::Completion`] carrying the exit code and the log's path, unless
//! the agent already read the exit through the tool.

use std::{
    collections::{BTreeMap, VecDeque},
    io::Write as _,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use tokio::sync::{
    mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
    watch,
};

use crate::{
    bindings::host::murmur::tool::run::{Status, ToolResult},
    cancel::CancelSignal,
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

/// A shell command's full-output log in the session's records directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OutputLog {
    /// Relative to the records directory, `logs/<stem>.log`: what the writer opens and measures
    /// through the records root.
    pub relative: String,
    /// The absolute host path, the one name every text gives the log. A shell command, a plan
    /// shell step, a native tool and a process harness can all open it; a WASM tool cannot.
    pub path: String,
}

/// Where the full output of the shell command named `stem` lands: `logs/<stem>.log` in the
/// session's records directory. The stem is the work id for a demoted command and `shell-<ms>`
/// for a foreground command whose output passed [`crate::shell::MAX_SHELL_OUTPUT_BYTES`].
///
/// One rule, used by both writers — [`LiveOutput`] for a demoted command, which writes the log as
/// the command runs, and `shell::write_shell_output_log` for a foreground one, which writes it
/// once at exit in its own layout — and by every reader that has to name the file.
pub(crate) fn output_path_for(
    records_root: &crate::workdir_fs::WorkdirRoot,
    stem: &str,
) -> OutputLog {
    let relative = format!("logs/{stem}.log");
    let path = records_root
        .host_path(&relative)
        .to_string_lossy()
        .into_owned();
    OutputLog { relative, path }
}

/// A demoted command the session is no longer waiting for.
#[derive(Debug, Clone)]
pub(crate) struct DetachedWork {
    pub work_id: String,
    /// The program that ran, resolved the same way [`crate::shell::ShellResult::binary`] is.
    pub binary: String,
    /// The command text the model supplied, without the binary name.
    pub command: String,
    /// When the command was spawned, not when it was demoted — the foreground grace period counts
    /// as running time, so this lines up with the `duration_ms` a completion carries.
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
    /// Epoch milliseconds at which the command's own wait returned, before its output finished
    /// draining and before the completion was recorded.
    pub exited_at_ms: u64,
    /// The log's absolute path, [`output_path_for`]'s [`OutputLog::path`] for this work id. Named
    /// even when [`Self::error`] is set, so a reader knows where the output was meant to be.
    pub output_path: String,
    /// Bytes written to that file, header included.
    pub output_bytes: u64,
    pub resource_limit: Option<String>,
    /// Whether anything the command started was still holding its output when it exited, as on
    /// [`crate::shell::ShellResult::output_holders`].
    pub output_holders: crate::shell::OutputHolders,
    /// The conversation the command was started from. The completion task runs under this id, so
    /// the result joins the thread that asked for it rather than opening a new one.
    pub context_id: String,
    /// Stamped [`TaskOrigin::Completion`] with the starting task's trust inherited.
    pub provenance: TaskProvenance,
    /// Some only when the wait itself failed — distinct from the command exiting non-zero.
    pub error: Option<String>,
}

impl DetachedCompletion {
    /// The `IncomingTask.message_text` the agent reads: the work id, the status, the exit code,
    /// the duration and the output path. Never the output itself, nor the command, which the work
    /// id names and the tool call that started it holds.
    ///
    /// An attributed limit adds the [`crate::resources::resource_limit_line`] a foreground call
    /// would get, naming the limit and what to do about it, and output holders that were killed
    /// or left running add their [`crate::shell::OutputHolders::note`].
    pub(crate) fn message_text(&self) -> String {
        let mut text = format!(
            "Background shell command finished.\n\
             work_id: {}\n\
             status: {}\n\
             exit_code: {}\n\
             duration_ms: {}\n\
             output: {} ({} bytes)",
            self.work_id,
            self.status(),
            self.exit_code,
            self.duration_ms,
            self.output_path,
            self.output_bytes,
        );
        if let Some(limit) = &self.resource_limit {
            text.push('\n');
            text.push_str(&crate::resources::resource_limit_line(limit));
        }
        if let Some(note) = self.output_holders.note() {
            text.push('\n');
            text.push_str(&note);
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
    /// The configured `lifecycle.shell_grace_secs` in milliseconds, recorded so a trace reader
    /// can see which setting was in force without consulting the manifest. `None` when the
    /// session declares no grace, so the command was demoted by an abandoned call or a cancel.
    pub grace_ms: Option<u64>,
}

/// The tool result a demoted command hands back to the turn. `delivered` is
/// [`DetachPolicy::delivered`]: whether the completion can also reach the model as a later task.
/// `tool_offered` is whether the session offers `background-command`, which it does exactly when
/// `lifecycle.shell_grace_secs` is set; without it the text names no tool.
///
/// Constant size by construction for every combination: nothing here derives from the command,
/// its arguments or its output, so no input can make the text the model reads grow. It names no
/// path and shows no output; `background-command` reports both.
pub(crate) fn demotion_tool_result(
    work_id: &str,
    delivered: bool,
    tool_offered: bool,
) -> ToolResult {
    let tool = crate::background_command::BACKGROUND_COMMAND_TOOL;
    let mut data = if tool_offered {
        format!(
            "The command is still running and was moved to the background as {work_id}. The \
             {tool} tool reports on it, waits for it or stops it by that id. A command still \
             running when the session ends is killed."
        )
    } else {
        format!(
            "The command is still running and was moved to the background as {work_id}. A \
             command still running when the session ends is killed."
        )
    };
    match (delivered, tool_offered) {
        (true, true) => data.push_str(&format!(
            " If it exits before {tool} has reported its exit, its result also arrives later as a \
             separate task."
        )),
        (true, false) => {
            data.push_str(" When it exits, its result arrives later as a separate task.");
        }
        (false, _) => {}
    }
    ToolResult {
        status: Status::Passed,
        summary: Some(format!(
            "Command still running in the background as {work_id}."
        )),
        data: Some(data),
        data_path: None,
        truncated: false,
        metadata: vec![("work_id".to_string(), work_id.to_string())],
    }
}

/// Where a demoted command is in its life, as each registry entry broadcasts it.
///
/// Ordered: a later phase never goes back to an earlier one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Phase {
    /// The command's own process has not exited.
    Running,
    /// Its own process has exited and is about to be reaped, so its process group is no longer
    /// signalled. Its output may still be draining.
    LeaderExited,
    /// The completion is recorded: exit code, status and the log's trailer.
    Finished,
}

/// What `background-command` reports a command's state as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommandState {
    Running,
    /// Finished, with no `stop` having reached it.
    Exited,
    /// Finished after a `stop` signalled it.
    Stopped,
}

impl CommandState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Exited => "exited",
            Self::Stopped => "stopped",
        }
    }
}

/// How a finished command's completion reached the agent. Claimed once per work id, by whichever
/// path gets there first, and that path writes the work id's one `shell_completed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Delivery {
    /// A `background-command` call returned the finished state.
    BackgroundCommand,
    /// The task loop queued the completion as a `completion`-origin task.
    Task,
}

impl Delivery {
    /// `shell_completed.delivered_via`.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::BackgroundCommand => "background_command",
            Self::Task => "task",
        }
    }
}

/// The output section of a demoted command's log, written as its pipes are read.
///
/// The log is `Command: <binary> <args…>`, a blank line, the output section, and the
/// `Exit code: <n>` trailer appended once the command has exited. The output section is
/// whatever either pipe yielded, in the order the two reader threads appended whole chunks
/// under this struct's lock; the bytes read before demotion come first, stdout's then stderr's.
///
/// The last [`crate::shell::MAX_SHELL_OUTPUT_BYTES`] bytes of the section are also kept here, so
/// a status read never opens the file.
pub(crate) struct LiveOutput {
    /// `None` once a write failed, or when the log could not be opened.
    file: Option<std::fs::File>,
    tail: VecDeque<u8>,
    /// Bytes appended to the output section, counted whether or not they reached the file.
    section_bytes: u64,
    /// The first failure to open or write the log.
    write_error: Option<String>,
}

impl LiveOutput {
    /// Opens `relative` in `records_root` for appending and writes the `Command:` header.
    ///
    /// A failure is kept, not returned: the command keeps running, its tail is still kept, and
    /// the failure becomes its completion's [`DetachedCompletion::error`].
    pub(crate) fn open(
        records_root: &crate::workdir_fs::WorkdirRoot,
        relative: &str,
        command_line: &str,
    ) -> Self {
        let mut live = Self {
            file: None,
            tail: VecDeque::new(),
            section_bytes: 0,
            write_error: None,
        };
        match records_root.open_append(relative, crate::workdir_fs::DEFAULT_FILE_MODE) {
            Ok(file) => {
                live.file = Some(file);
                live.write(format!("Command: {command_line}\n\n").as_bytes());
            }
            Err(error) => live.write_error = Some(format!("failed to write shell log: {error}")),
        }
        live
    }

    fn write(&mut self, bytes: &[u8]) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        if let Err(error) = file.write_all(bytes) {
            self.file = None;
            self.write_error
                .get_or_insert_with(|| format!("failed to write shell log: {error}"));
        }
    }

    /// Appends `bytes` to the output section and to the kept tail.
    pub(crate) fn append(&mut self, bytes: &[u8]) {
        self.write(bytes);
        self.section_bytes = self.section_bytes.saturating_add(bytes.len() as u64);
        let keep = crate::shell::MAX_SHELL_OUTPUT_BYTES;
        if bytes.len() >= keep {
            self.tail.clear();
            self.tail.extend(&bytes[bytes.len() - keep..]);
        } else {
            let excess = (self.tail.len() + bytes.len()).saturating_sub(keep);
            self.tail.drain(..excess);
            self.tail.extend(bytes);
        }
    }

    /// Appends the `Exit code:` trailer and closes the log. Returns the first write failure, if
    /// any write to this log failed.
    pub(crate) fn finish(&mut self, exit_code: i32) -> Option<String> {
        self.write(format!("\n\nExit code: {exit_code}\n").as_bytes());
        self.file = None;
        self.write_error.clone()
    }
}

/// One demoted command as the registry holds it for the rest of the session.
struct DetachedEntry {
    work: DetachedWork,
    started: Instant,
    /// The command's process group: its own pid, since every shell command leads its group.
    pgid: i32,
    /// The log's absolute path, as [`OutputLog::path`].
    output_path: String,
    output: Arc<Mutex<LiveOutput>>,
    /// Whether the command's own process is unreaped. Its group is signalled only while this is
    /// set, and only under the registry lock, because the demoted thread clears it under that
    /// lock before it reaps: a reaped pid can name an unrelated process group.
    leader_running: bool,
    /// Set when `stop` signalled the command, which makes a finished command `stopped`.
    stop_requested: bool,
    /// Signals sent to the group by `stop` and `kill_running`.
    signals_sent: u32,
    finished: Option<DetachedCompletion>,
    /// How the completion reached the agent, once one path has claimed it. Only a finished entry
    /// is claimed.
    delivery: Option<Delivery>,
    phase: watch::Sender<Phase>,
}

/// A demoted command's state at one moment, as `background-command` renders it.
#[derive(Debug, Clone)]
pub(crate) struct CommandSnapshot {
    pub work_id: String,
    pub state: CommandState,
    /// From spawn to now, or to the end of its completion for a finished command.
    pub elapsed_ms: u64,
    /// The completion, for a finished command.
    pub finished: Option<DetachedCompletion>,
    pub output_path: String,
    /// Bytes in the log's output section.
    pub output_bytes: u64,
    /// The last [`crate::shell::MAX_SHELL_OUTPUT_BYTES`] bytes of the output section, or all of
    /// it when it is shorter.
    pub tail: Vec<u8>,
    /// Whether this read claimed the completion for [`Delivery::BackgroundCommand`], which makes
    /// its caller the writer of the work id's `shell_completed`.
    pub delivered_now: bool,
}

/// How a `wait` ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WaitEnd {
    Finished,
    TimedOut,
    Canceled,
    /// The work id names no command in this session.
    Unknown,
}

/// The demoted commands of one session, and the channel their completions arrive on.
///
/// Shared between the agent loop (which registers work and answers `background-command`) and
/// each demoted command's own thread (which completes it). Entries are never removed, so a work
/// id stays valid for the rest of the session.
pub(crate) struct DetachedRegistry {
    entries: Mutex<BTreeMap<String, DetachedEntry>>,
    /// Every report the task loop turns into a `completion`-origin task, not only a demoted
    /// command's. A delegation that outran its deadline arrives here too, so one channel feeds one
    /// enqueue path and the admission rule cannot differ between them.
    reports: UnboundedSender<DetachedReport>,
}

impl DetachedRegistry {
    pub(crate) fn new() -> (Arc<Self>, UnboundedReceiver<DetachedReport>) {
        let (reports, receiver) = unbounded_channel();
        (
            Arc::new(Self {
                entries: Mutex::new(BTreeMap::new()),
                reports,
            }),
            receiver,
        )
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, DetachedEntry>> {
        self.entries.lock().expect("detached registry mutex")
    }

    /// Hand the task loop one report. A send that fails means the loop is gone.
    pub(crate) fn report(&self, report: DetachedReport) {
        let _ = self.reports.send(report);
    }

    /// Record a demoted command spawned at `started`, whose process group is `pgid`, whose log is
    /// at `output_path` and whose output is being written through `output`. Called before the
    /// dispatch returns its handle, so the session-end sweep cannot miss work registered after it
    /// looked.
    pub(crate) fn register(
        &self,
        work: DetachedWork,
        started: Instant,
        pgid: i32,
        output_path: String,
        output: Arc<Mutex<LiveOutput>>,
    ) {
        let (phase, _) = watch::channel(Phase::Running);
        self.lock().insert(
            work.work_id.clone(),
            DetachedEntry {
                work,
                started,
                pgid,
                output_path,
                output,
                leader_running: true,
                stop_requested: false,
                signals_sent: 0,
                finished: None,
                delivery: None,
                phase,
            },
        );
    }

    /// The command's own process has exited and is about to be reaped: from here on its group is
    /// never signalled. Called by the demoted thread after `waitid(WNOWAIT)` and before
    /// `Child::wait`.
    pub(crate) fn leader_exited(&self, work_id: &str) {
        if let Some(entry) = self.lock().get_mut(work_id) {
            entry.leader_running = false;
            entry.phase.send_replace(Phase::LeaderExited);
        }
    }

    /// Records the completion on its entry and sends it to the task loop.
    ///
    /// A command a `stop` reached has no resource limit attributed: the stop is why it ended.
    ///
    /// The channel is unbounded, so the blocking thread that calls this never waits and needs no
    /// async context. A send that fails means the task loop is gone — the session is ending, and
    /// the result has nowhere to be delivered.
    pub(crate) fn complete(&self, mut completion: DetachedCompletion) {
        {
            let mut entries = self.lock();
            if let Some(entry) = entries.get_mut(&completion.work_id) {
                if entry.stop_requested {
                    completion.resource_limit = None;
                }
                entry.leader_running = false;
                entry.finished = Some(completion.clone());
                entry.phase.send_replace(Phase::Finished);
            }
        }
        self.report(DetachedReport::Completed(completion));
    }

    /// Every command demoted and not yet finished, oldest work id first.
    pub(crate) fn outstanding(&self) -> Vec<DetachedWork> {
        self.lock()
            .values()
            .filter(|entry| entry.finished.is_none())
            .map(|entry| entry.work.clone())
            .collect()
    }

    /// `work_id`'s state now, or `None` for a work id this session never issued.
    ///
    /// A finished state read here counts as the agent having read the exit: an unclaimed entry is
    /// claimed for [`Delivery::BackgroundCommand`], so its completion is not also enqueued as a
    /// task, and [`CommandSnapshot::delivered_now`] says this read made the claim. A running
    /// entry, and one already claimed either way, claims nothing.
    pub(crate) fn read(&self, work_id: &str) -> Option<CommandSnapshot> {
        let mut entries = self.lock();
        let entry = entries.get_mut(work_id)?;
        let delivered_now = entry.finished.is_some() && entry.delivery.is_none();
        if delivered_now {
            entry.delivery = Some(Delivery::BackgroundCommand);
        }
        let state = match (&entry.finished, entry.stop_requested) {
            (None, _) => CommandState::Running,
            (Some(_), false) => CommandState::Exited,
            (Some(_), true) => CommandState::Stopped,
        };
        let elapsed_ms = match &entry.finished {
            Some(completion) => completion.duration_ms,
            None => millis(entry.started.elapsed()),
        };
        let (output_bytes, tail) = {
            let output = entry
                .output
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (output.section_bytes, output.tail.iter().copied().collect())
        };
        Some(CommandSnapshot {
            work_id: entry.work.work_id.clone(),
            state,
            elapsed_ms,
            finished: entry.finished.clone(),
            output_path: entry.output_path.clone(),
            output_bytes,
            tail,
            delivered_now,
        })
    }

    /// Claims `work_id`'s completion for [`Delivery::Task`], under the same lock [`Self::read`]
    /// claims under. `false` when a read already claimed it, so the task loop enqueues nothing and
    /// writes nothing. A work id this registry does not hold has nothing to race and is `true`.
    pub(crate) fn claim_for_task(&self, work_id: &str) -> bool {
        let mut entries = self.lock();
        let Some(entry) = entries.get_mut(work_id) else {
            return true;
        };
        if entry.delivery.is_some() {
            return false;
        }
        entry.delivery = Some(Delivery::Task);
        true
    }

    /// Whether `background-command` has returned `work_id`'s finished state: its completion was
    /// claimed for [`Delivery::BackgroundCommand`].
    pub(crate) fn exit_reported(&self, work_id: &str) -> bool {
        self.lock()
            .get(work_id)
            .is_some_and(|entry| entry.delivery == Some(Delivery::BackgroundCommand))
    }

    fn phase(&self, work_id: &str) -> Option<watch::Receiver<Phase>> {
        self.lock()
            .get(work_id)
            .map(|entry| entry.phase.subscribe())
    }

    /// Sends `stop`'s `signal` to `work_id`'s process group if its leader is unreaped, and marks
    /// the entry stop-requested. `true` when sent.
    fn signal_stop(&self, work_id: &str, signal: libc::c_int) -> bool {
        let mut entries = self.lock();
        let Some(entry) = entries
            .get_mut(work_id)
            .filter(|entry| entry.leader_running)
        else {
            return false;
        };
        let sent = crate::formation_launch::signal_group(i64::from(entry.pgid), signal);
        if sent {
            entry.signals_sent += 1;
            entry.stop_requested = true;
        }
        sent
    }

    /// `SIGKILL` to the process group of every command whose leader is unreaped. The session's
    /// end calls this, so no demoted command outlives its session on a host with no cgroup
    /// scope.
    pub(crate) fn kill_running(&self) {
        for entry in self.lock().values_mut() {
            if entry.leader_running
                && crate::formation_launch::signal_group(i64::from(entry.pgid), libc::SIGKILL)
            {
                entry.signals_sent += 1;
            }
        }
    }

    /// Waits until `work_id` finishes, `bound` passes or `cancel` is raised, whichever is first.
    ///
    /// Holds no thread while it waits, and dropping the future leaves nothing behind.
    pub(crate) async fn wait(
        &self,
        work_id: &str,
        bound: Duration,
        cancel: Option<&crate::cancel::CancelSignal>,
    ) -> WaitEnd {
        let Some(mut phase) = self.phase(work_id) else {
            return WaitEnd::Unknown;
        };
        let canceled = async {
            match cancel {
                Some(signal) => signal.canceled().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            finished = phase.wait_for(|phase| *phase == Phase::Finished) => {
                // The sender lives in the entry, which is never removed, so the wait cannot see
                // a closed channel; a `false` here would mean the registry itself was dropped.
                if finished.is_ok() { WaitEnd::Finished } else { WaitEnd::TimedOut }
            }
            () = tokio::time::sleep(bound) => WaitEnd::TimedOut,
            () = canceled => WaitEnd::Canceled,
        }
    }

    /// Stops `work_id`: `SIGTERM` to its process group, then `SIGKILL` if its leader has not
    /// exited [`crate::background_command::STOP_TERM_GRACE`] later, then up to
    /// [`crate::background_command::STOP_KILL_WAIT`] more for the completion. Returns once the
    /// command has finished or that time is up, whichever is first; a command already finished
    /// is sent nothing.
    ///
    /// `false` for a work id this session never issued.
    pub(crate) async fn stop(&self, work_id: &str) -> bool {
        let Some(mut phase) = self.phase(work_id) else {
            return false;
        };
        if self.signal_stop(work_id, libc::SIGTERM) {
            let exited = tokio::time::timeout(
                crate::background_command::STOP_TERM_GRACE,
                phase.wait_for(|phase| *phase >= Phase::LeaderExited),
            )
            .await;
            if !matches!(exited, Ok(Ok(_))) {
                self.signal_stop(work_id, libc::SIGKILL);
            }
        }
        let _ = tokio::time::timeout(
            crate::background_command::STOP_KILL_WAIT,
            phase.wait_for(|phase| *phase == Phase::Finished),
        )
        .await;
        true
    }

    /// Signals sent to `work_id`'s process group so far.
    #[cfg(test)]
    pub(crate) fn signals_sent(&self, work_id: &str) -> u32 {
        self.lock()
            .get(work_id)
            .map_or(0, |entry| entry.signals_sent)
    }
}

fn millis(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

/// Kills the process group of every command the registry still runs when dropped, so a failing
/// test leaves nothing behind.
#[cfg(test)]
pub(crate) struct KillRunningOnDrop(pub(crate) Arc<DetachedRegistry>);

#[cfg(test)]
impl Drop for KillRunningOnDrop {
    fn drop(&mut self) {
        self.0.kill_running();
    }
}

/// Supplied by a dispatch that is allowed to demote.
///
/// `None` at a call site means the command runs to completion in the foreground. That is what
/// keeps `plan.rs` and the script-capsule store state foreground-only: neither runs a task loop,
/// so neither has anywhere to deliver a completion.
pub(crate) struct DetachPolicy {
    /// How long the command runs in the foreground before it is demoted: the declared
    /// `lifecycle.shell_grace_secs`. `None` sets no deadline, so the command stays in the
    /// foreground until it exits unless `abandoned` or `canceled` demotes it.
    pub grace: Option<Duration>,
    /// Whether the completion reaches the model as a later task, which only `task_acceptance:
    /// queue` with `after_task: sleep` does. Decides what [`demotion_tool_result`] tells it.
    pub delivered: bool,
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
    /// Raised when whoever was waiting for the foreground result has gone away. The command is
    /// then demoted at once, whether or not `grace` is set. `None` for every caller that
    /// cannot go away mid-call; the process-transport tool bridge is the one that can.
    pub abandoned: Option<AbandonSignal>,
    /// The cancel of the task whose turn made the call. Raised before the spawn, the command is
    /// not run; raised while it runs in the foreground, it is demoted at once, as `abandoned`
    /// demotes it, and the cancel is claimed for [`crate::cancel::PHASE_SHELL`]. `None` for a
    /// caller whose own wait answers the cancel: a plan step, and the process-transport bridge.
    pub canceled: Option<CancelSignal>,
}

/// Set once by a caller that stopped waiting for a call's result.
///
/// Clones share one flag. Raising it never stops anything already running: a shell command still
/// in the foreground reads it and demotes, which is the only effect it has.
#[derive(Debug, Clone, Default)]
pub(crate) struct AbandonSignal(Arc<AtomicBool>);

impl AbandonSignal {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn raise(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub(crate) fn is_raised(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
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

// -- Work a clean exit discarded ---------------------------------------------

/// What the teardown sweep knows about one discarded command.
///
/// The two arms are the two places a command can be caught by the sweep, and they are told apart
/// because what is known differs: a command still running has nothing on disk and never will,
/// while one that finished between the task loop's last read and the sweep has an exit code and a
/// written output log that no task will ever carry back.
#[derive(Debug, Clone)]
pub(crate) enum AbandonedDisposition {
    /// Running when the session ended, and killed with it. No exit code reaches any task. Its
    /// log holds what it printed up to the kill; whether the `Exit code:` trailer lands depends
    /// on its runtime thread reaping it before the process exits.
    StillRunning {
        /// From spawn to the sweep, foreground portion included.
        running_ms: u64,
    },
    /// Finished during teardown, after the task loop stopped reading completions. Everything a
    /// `shell_completed` would have carried exists; only the task that would have carried it does
    /// not.
    FinishedTooLate {
        exit_code: i32,
        /// From spawn to exit, foreground portion included.
        duration_ms: u64,
        /// As [`DetachedCompletion::output_path`], and on disk by the time this arm is built.
        output_path: String,
        output_bytes: u64,
        resource_limit: Option<String>,
        /// Set only when the wait itself failed, as on [`DetachedCompletion::error`]. Without it a
        /// report can read `status: error` with an exit code of `0` and no reason for either.
        wait_error: Option<String>,
        /// [`DetachedCompletion::status`] of the completion this was built from.
        status: &'static str,
    },
}

/// One discarded command, as the operator report and the `shell_abandoned` record both read it.
#[derive(Debug, Clone)]
pub(crate) struct AbandonedWork {
    pub work_id: String,
    /// As [`DetachedWork::binary`].
    pub binary: String,
    /// As [`DetachedWork::command`] — the text the model wrote, which the operator needs to know
    /// what was actually left running.
    pub command: String,
    pub disposition: AbandonedDisposition,
}

impl AbandonedWork {
    /// How long the command ran, whichever arm it was caught in — the `running_ms` of its
    /// `shell_abandoned` line.
    pub(crate) fn running_ms(&self) -> u64 {
        match &self.disposition {
            AbandonedDisposition::StillRunning { running_ms } => *running_ms,
            AbandonedDisposition::FinishedTooLate { duration_ms, .. } => *duration_ms,
        }
    }

    /// The exit code, output path and byte count, or `None` for a command still running.
    ///
    /// The three travel together because they exist together: a command with an exit code has a
    /// written log, and one without has neither.
    pub(crate) fn recovered(&self) -> Option<(i32, &str, u64)> {
        match &self.disposition {
            AbandonedDisposition::StillRunning { .. } => None,
            AbandonedDisposition::FinishedTooLate {
                exit_code,
                output_path,
                output_bytes,
                ..
            } => Some((*exit_code, output_path.as_str(), *output_bytes)),
        }
    }
}

/// The grouped operator report for every command one session discarded, as it lands on stderr and
/// in the records directory's `logs/bootstrap.log`. `records_root` is the session's records
/// directory, which a still-running command's log path is named in.
///
/// One block for the whole session rather than a line per command: a session that discarded ten
/// commands must state the condition once and then enumerate, or the condition is lost in the
/// enumeration. Deliberately **not** [`LostReport::message_text`] — that text asserts no exit
/// code was recorded and that whether the command finished is unknown, which is false here: this
/// session's registry was alive, so the running time is known, and a command that finished during
/// teardown has its exit code on disk.
pub(crate) fn abandonment_report_text(
    session_id: &str,
    records_root: &crate::workdir_fs::WorkdirRoot,
    abandoned: &[AbandonedWork],
) -> String {
    let count = abandoned.len();
    let plural = if count == 1 { "command" } else { "commands" };
    let mut text = format!(
        "[capsule-runtime] {count} background shell {plural} discarded at session end.\n\n\
         Session {session_id} ended while {count} demoted shell {plural} {} unaccounted for. \
         Every process the session started was killed when it ended; what is known about each \
         command is below.\n",
        if count == 1 { "was" } else { "were" }
    );
    for work in abandoned {
        text.push_str(&format!(
            "\nwork_id: {}\nbinary: {}\ncommand: {}\n",
            work.work_id, work.binary, work.command
        ));
        match &work.disposition {
            AbandonedDisposition::StillRunning { running_ms } => {
                text.push_str(&format!(
                    "state: killed at session end after {running_ms} ms\n\
                     result: none — it was killed before it finished, so no task carries an exit \
                     code. What it printed before the kill is in {}.\n",
                    output_path_for(records_root, &work.work_id).path
                ));
            }
            AbandonedDisposition::FinishedTooLate {
                exit_code,
                duration_ms,
                output_path,
                output_bytes,
                resource_limit,
                wait_error,
                status,
            } => {
                text.push_str(&format!(
                    "state: finished during teardown after {duration_ms} ms\n\
                     status: {status}\n\
                     exit_code: {exit_code}\n"
                ));
                if let Some(limit) = resource_limit {
                    text.push_str(&format!("resource_limit: {limit}\n"));
                }
                if let Some(error) = wait_error {
                    text.push_str(&format!("wait_error: {error}\n"));
                }
                text.push_str(&format!(
                    "output: {output_path} ({output_bytes} bytes)\n\
                     result: on disk — it finished too late for any task to carry it back to the \
                     agent.\n"
                ));
            }
        }
    }
    text.push_str(
        "\nA session's end kills every command it demoted and every process its shell calls \
         started in the background. Declare lifecycle.task_acceptance: queue with \
         lifecycle.after_task: sleep for a session that outlives its task and can be told the \
         result instead.",
    );
    text
}

// -- Work a dead session left unaccounted ------------------------------------

/// A demoted command whose session died before anything could be recorded about it.
///
/// Reconstructed from the prior session's `shell_detached` line, which is the only trace of it
/// that survives: [`DetachedRegistry`] lives in process memory, and the `Exit code:` trailer of
/// the output log is written by a runtime thread after the command exits, so a runtime killed
/// with `SIGKILL` leaves a log with no exit code even for a command that ran to a clean exit.
#[derive(Debug, Clone)]
pub(crate) struct LostWork {
    pub work_id: String,
    /// As [`DetachedWork::binary`].
    pub binary: String,
    /// As [`DetachedWork::command`].
    pub command: String,
    /// The `shell_detached` line's own timestamp — the demotion instant.
    pub detached_at_ms: u64,
}

/// Every unaccounted command of one prior session, reported as one task.
///
/// One report per resume rather than one per work id: enqueuing raises `pending_count` and so
/// counts against `lifecycle.queue_depth`, and a session that lost ten commands must not cost ten
/// pending slots to say so.
#[derive(Debug, Clone)]
pub(crate) struct LostReport {
    /// The session that started the work, and whose `trace.jsonl` holds both the unmatched
    /// `shell_detached` lines and the `shell_lost` lines that account for them.
    pub started_in_session: String,
    /// Non-empty by construction: a report with nothing in it is never built.
    pub lost: Vec<LostWork>,
    /// The conversation the resume runs under, so the report joins that thread.
    pub context_id: String,
    /// [`TaskOrigin::Completion`] carrying the lowest trust across [`Self::lost`] — one untrusted
    /// command makes the whole report untrusted.
    pub provenance: TaskProvenance,
    /// The task this report is enqueued as. Minted before the `shell_lost` markers are appended,
    /// because each marker names it as `reconciled_task_id`.
    pub task_id: String,
}

impl LostReport {
    /// The `IncomingTask.message_text` the agent reads: the work id and the demotion instant of
    /// each lost command, and the statement that no exit code was recorded. The work id names the
    /// command, which the tool call that started it holds.
    ///
    /// Shares no opening line with [`DetachedCompletion::message_text`]. A model that cannot tell
    /// "it failed" from "nobody knows" retries, and the retry is the cost demotion exists to
    /// remove. Names no output path, quotes no byte count and asserts no exit code: no exit code
    /// exists, and the log the earlier session wrote stops wherever that session died.
    pub(crate) fn message_text(&self) -> String {
        let mut text = format!(
            "Background shell commands from an earlier session were never accounted for.\n\
             \n\
             Session {} ended without recording what became of the commands below; they were \
             running in the background at the time. No exit code was recorded for any of them, \
             and whether each command finished at all is unknown.\n",
            self.started_in_session
        );
        for work in &self.lost {
            text.push_str(&format!(
                "\nwork_id: {}\ndetached_at_ms: {}\n",
                work.work_id, work.detached_at_ms
            ));
        }
        text.push_str(
            "\nNone of these is known to have succeeded and none is known to have failed. Treat \
             each as unknown, and start the work again only if it still matters.",
        );
        text
    }
}

/// What the task loop turns into a `completion`-origin task.
///
/// One enum rather than two enqueue paths: an authority reporting on work it started is the same
/// shape whether the work finished under this runtime or was left unaccounted by one that died.
#[derive(Debug, Clone)]
pub(crate) enum DetachedReport {
    Completed(DetachedCompletion),
    Lost(LostReport),
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::{
        path::{Path, PathBuf},
        time::{Duration, Instant},
    };

    use crate::{
        origin::TrustClass,
        sandbox::ShellEnforcement,
        shell::{
            run_shell,
            test_support::{process_dead_within, read_pid, KillPidFileOnDrop},
            ShellOutcome,
        },
        types::CapabilityPolicy,
    };

    fn bash_policy() -> CapabilityPolicy {
        CapabilityPolicy {
            shell_allow: vec!["bash".to_string()],
            ..Default::default()
        }
    }

    /// The records directory [`crate::sandbox::test_runtime_dir`] creates under `base`.
    fn records_dir(base: &Path) -> PathBuf {
        base.join(crate::records_dir::RECORDS_DIR_NAME)
    }

    /// `run_shell` against `bash -c <script>` with a grace period, returning the outcome and the
    /// registry the completion (if any) will arrive on. The session's runtime directory roots,
    /// its records directory among them, are created under `runtime_base`.
    fn detached_run(
        workdir: &Path,
        runtime_base: &Path,
        script: &str,
        grace: impl Into<Option<Duration>>,
    ) -> (
        ShellOutcome,
        Arc<DetachedRegistry>,
        UnboundedReceiver<DetachedReport>,
    ) {
        detached_run_with(
            workdir,
            &ShellEnforcement::environment_only()
                .with_runtime_dir(crate::sandbox::test_runtime_dir(runtime_base), None),
            script,
            grace,
        )
    }

    /// [`detached_run`] under `enforcement`.
    fn detached_run_with(
        workdir: &Path,
        enforcement: &ShellEnforcement,
        script: &str,
        grace: impl Into<Option<Duration>>,
    ) -> (
        ShellOutcome,
        Arc<DetachedRegistry>,
        UnboundedReceiver<DetachedReport>,
    ) {
        let (registry, receiver) = DetachedRegistry::new();
        let outcome = run_shell(
            "bash",
            &["-c", script],
            &[],
            &crate::workdir_fs::WorkdirRoot::open(workdir).unwrap(),
            &bash_policy(),
            enforcement,
            Some(DetachPolicy {
                grace: grace.into(),
                delivered: true,
                registry: Arc::clone(&registry),
                command: script.to_string(),
                context_id: "ctx_detached_tests".to_string(),
                provenance: Some(TaskProvenance::derive(TaskOrigin::User, None)),
                abandoned: None,
                canceled: None,
            }),
        )
        .expect("a declared binary runs");
        (outcome, registry, receiver)
    }

    /// Block until the completion lands, so the test fails with a timeout rather than hanging.
    ///
    /// The channel carries every report the task loop enqueues, so a shell command's completion is
    /// taken off it by variant: anything else on this receiver means a delegation report reached a
    /// test that started no delegation.
    fn await_completion(receiver: &mut UnboundedReceiver<DetachedReport>) -> DetachedCompletion {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match receiver.try_recv() {
                Ok(DetachedReport::Completed(completion)) => return completion,
                Ok(other) => panic!("a shell command's completion was expected; got {other:?}"),
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

    fn completion_provenance() -> TaskProvenance {
        TaskProvenance::derive(TaskOrigin::Completion, Some(TrustClass::Untrusted))
    }

    /// Every completion an agent can be handed opens with a line no other one opens with, so a
    /// model can tell a delegation's outcome from a demoted command's without reading further.
    #[test]
    fn no_two_completions_share_an_opening_line() {
        let outcome = crate::delegation::DelegationOutcome {
            delegation_id: "dlg_0001".to_string(),
            capsule_name: "worker".to_string(),
            capsule_version: "0.1.0".to_string(),
            session_id: "ses_child".to_string(),
            status: crate::delegation::DelegationStatus::Ok,
            result_path: None,
            workdir: "/tmp/child".to_string(),
            duration_ms: 1,
            detail: None,
            reported_by: crate::delegation::Reporter::Launcher,
            delivered: true,
            delivery_error: None,
        };
        let completion = DetachedCompletion {
            work_id: "wrk_0001".to_string(),
            binary: "bash".to_string(),
            command: "sleep 3".to_string(),
            exit_code: 0,
            duration_ms: 3_000,
            exited_at_ms: 1_000,
            output_path: "/home/test/.murmur/records/work/ses_1/logs/wrk_0001.log".to_string(),
            output_bytes: 0,
            resource_limit: None,
            output_holders: crate::shell::OutputHolders::Closed,
            context_id: "ctx_1".to_string(),
            provenance: completion_provenance(),
            error: None,
        };
        let texts = [
            ("delegation outcome", outcome.message_text()),
            ("detached shell completion", completion.message_text()),
        ];
        for (index, (name, text)) in texts.iter().enumerate() {
            let opening = text.lines().next().unwrap_or_default();
            assert!(!opening.is_empty(), "{name} opens with nothing");
            for (other_name, other) in texts.iter().skip(index + 1) {
                assert_ne!(
                    opening,
                    other.lines().next().unwrap_or_default(),
                    "{name} and {other_name} open with the same line"
                );
            }
        }
    }

    /// A demoted command's limit reaches the model in the same words a foreground call's does,
    /// without displacing the opening line or the closing pointer at the output file.
    #[test]
    fn a_completion_names_its_limit_in_the_shared_wording() {
        let completion = DetachedCompletion {
            work_id: "wrk_0001".to_string(),
            binary: "bash".to_string(),
            command: "make -j64".to_string(),
            exit_code: 1,
            duration_ms: 3_000,
            exited_at_ms: 1_000,
            output_path: "/home/test/.murmur/records/work/ses_1/logs/wrk_0001.log".to_string(),
            output_bytes: 0,
            resource_limit: Some("cgroup_pids_max".into()),
            output_holders: crate::shell::OutputHolders::Closed,
            context_id: "ctx_1".to_string(),
            provenance: completion_provenance(),
            error: None,
        };
        let text = completion.message_text();
        let expected = crate::resources::resource_limit_line("cgroup_pids_max");

        assert!(
            text.lines().any(|line| line == expected),
            "no limit line in:\n{text}"
        );
        assert_eq!(
            text.lines().next(),
            Some("Background shell command finished.")
        );
        assert_eq!(completion.status(), "error");
        assert!(text.ends_with("are not reproduced here."), "{text}");
    }

    /// A completion names its command by the work id alone: the tool call that started it holds
    /// the command, and the trace's `shell_completed` line keeps it.
    #[test]
    fn a_completion_names_the_work_id_and_not_the_command() {
        let completion = DetachedCompletion {
            work_id: "wrk_0001".to_string(),
            binary: "/usr/bin/bash".to_string(),
            command: "make -j64".to_string(),
            exit_code: 0,
            duration_ms: 3_000,
            exited_at_ms: 1_000,
            output_path: "/home/test/.murmur/records/work/ses_1/logs/wrk_0001.log".to_string(),
            output_bytes: 12,
            resource_limit: None,
            output_holders: crate::shell::OutputHolders::Closed,
            context_id: "ctx_1".to_string(),
            provenance: completion_provenance(),
            error: Some("the wait failed".to_string()),
        };
        assert_eq!(
            completion.message_text(),
            "Background shell command finished.\n\
             work_id: wrk_0001\n\
             status: error\n\
             exit_code: 0\n\
             duration_ms: 3000\n\
             output: /home/test/.murmur/records/work/ses_1/logs/wrk_0001.log (12 bytes)\n\
             wait_error: the wait failed\n\
             \n\
             The full stdout and stderr are in that file and are not reproduced here."
        );
    }

    /// A lost-work report names each command by its work id and demotion instant; the binary
    /// and the command text stay in the `shell_detached` and `shell_lost` trace lines.
    #[test]
    fn a_lost_report_names_each_work_id_and_not_the_command() {
        let lost = |work_id: &str, command: &str, detached_at_ms| LostWork {
            work_id: work_id.to_string(),
            binary: "/usr/bin/bash".to_string(),
            command: command.to_string(),
            detached_at_ms,
        };
        let report = LostReport {
            started_in_session: "ses_prior".to_string(),
            lost: vec![
                lost("wrk_one", "sleep 60", 1750),
                lost("wrk_two", "make test", 2500),
            ],
            context_id: "ctx_1".to_string(),
            provenance: completion_provenance(),
            task_id: "tsk_1".to_string(),
        };
        let text = report.message_text();
        assert!(
            text.contains(
                "\nwork_id: wrk_one\ndetached_at_ms: 1750\n\nwork_id: wrk_two\ndetached_at_ms: 2500\n"
            ),
            "{text}"
        );
        assert!(text.starts_with(
            "Background shell commands from an earlier session were never accounted for.\n\n\
             Session ses_prior ended without recording what became of the commands below;"
        ));
        assert!(
            text.ends_with("start the work again only if it still matters."),
            "{text}"
        );
        for absent in [
            "binary:",
            "command:",
            "/usr/bin/bash",
            "sleep 60",
            "make test",
        ] {
            assert!(!text.contains(absent), "{absent} in:\n{text}");
        }
    }

    /// A demoted command that exits while a process it started still holds its output completes
    /// anyway: the holder is killed, the completion says so, and the log has what was printed.
    #[test]
    fn a_demoted_command_completes_when_its_background_holders_are_killed() {
        let workdir = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let _sleeper = KillPidFileOnDrop(workdir.path().join("sleeper.pid"));
        let (outcome, _registry, mut receiver) = detached_run(
            workdir.path(),
            runtime.path(),
            "sleep 2; sleep 300 & echo $! > sleeper.pid; echo started",
            Duration::from_secs(1),
        );
        let ShellOutcome::Detached(info) = outcome else {
            panic!("a two-second command must not finish inside a one-second grace: {outcome:?}");
        };

        let waiting = Instant::now();
        let completion = await_completion(&mut receiver);
        let waited = waiting.elapsed();
        assert!(
            waited < Duration::from_secs(15),
            "the completion took {waited:?}"
        );
        assert_eq!(completion.work_id, info.work_id);
        assert_eq!(completion.exit_code, 0);
        assert_eq!(
            completion.output_holders,
            crate::shell::OutputHolders::Killed
        );
        let note = crate::shell::OutputHolders::Killed.note().unwrap();
        let text = completion.message_text();
        assert!(text.lines().any(|line| line == note), "{text}");
        assert!(text.ends_with("are not reproduced here."), "{text}");
        let log = std::fs::read_to_string(&completion.output_path).unwrap();
        assert!(log.contains("started"), "{log}");

        let sleeper = read_pid(&workdir.path().join("sleeper.pid"));
        assert!(
            process_dead_within(sleeper, Duration::from_secs(2)),
            "the background sleeper {sleeper} is still running"
        );
    }

    /// The whole argument of demotion: the call returns long before the command does, and the
    /// result turns up afterwards with the output on disk rather than in hand.
    #[test]
    fn a_command_outrunning_the_grace_period_is_demoted() {
        let workdir = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let started = Instant::now();
        // The output word is spelled nowhere in the command, so finding it in the completion
        // can only mean the output leaked into the turn.
        let (outcome, registry, mut receiver) = detached_run(
            workdir.path(),
            runtime.path(),
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
        assert_eq!(info.grace_ms, Some(1_000));
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
        let expected_path = records_dir(runtime.path())
            .join("logs")
            .join(format!("{}.log", info.work_id));
        assert!(expected_path.is_absolute());
        assert_eq!(
            completion.output_path,
            expected_path.to_string_lossy(),
            "the completion names <records dir>/logs/<work_id>.log by its absolute path"
        );
        assert_eq!(completion.provenance.origin(), TaskOrigin::Completion);
        assert_eq!(completion.provenance.trust(), TrustClass::Trusted);
        assert_eq!(completion.context_id, "ctx_detached_tests");
        assert!(
            registry.outstanding().is_empty(),
            "a completed command is no longer outstanding"
        );

        let log = std::fs::read_to_string(&completion.output_path)
            .expect("the completion's output file exists");
        assert!(
            log.contains("finished-marker"),
            "the output is in the log: {log}"
        );
        assert_eq!(completion.output_bytes, log.len() as u64);
        let text = completion.message_text();
        assert!(
            !text.contains("finished-marker"),
            "the output never reaches the agent: {text}"
        );
        assert!(
            text.contains(&format!(
                "output: {} ({} bytes)",
                completion.output_path, completion.output_bytes
            )),
            "{text}"
        );
        assert!(!text.contains("in the capsule workdir"), "{text}");
        assert!(
            !workdir.path().join("logs").exists(),
            "nothing is written into the accessible workdir"
        );
    }

    /// With no records directory there is nowhere to keep a demoted command's output, so a
    /// command that outruns its grace period runs to completion in the foreground instead.
    #[test]
    fn with_no_records_directory_a_slow_command_is_not_demoted() {
        let workdir = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let (outcome, registry, mut receiver) = detached_run_with(
            workdir.path(),
            &ShellEnforcement::environment_only().with_runtime_dir(
                crate::runtime_dir::RuntimeDirRoots {
                    records: None,
                    ..crate::sandbox::test_runtime_dir(base.path())
                },
                None,
            ),
            "sleep 2; echo done",
            Duration::from_millis(200),
        );

        let result = match outcome {
            ShellOutcome::Finished(result) => result,
            ShellOutcome::Detached(info) => {
                panic!(
                    "no records directory, yet the command was demoted as {}",
                    info.work_id
                )
            }
        };
        assert!(started.elapsed() >= Duration::from_secs(2));
        assert_eq!(result.stdout.trim(), "done");
        assert!(registry.outstanding().is_empty());
        assert!(matches!(
            receiver.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        assert!(!workdir.path().join("logs").exists());
    }

    /// With no grace there is no clock: a command that runs for seconds stays in the foreground
    /// and returns its output to the call, and nothing is registered.
    #[test]
    fn with_no_grace_a_slow_command_finishes_in_the_foreground() {
        let workdir = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let (outcome, registry, mut receiver) = detached_run(
            workdir.path(),
            runtime.path(),
            "sleep 3; echo finished",
            None,
        );

        let result = match outcome {
            ShellOutcome::Finished(result) => result,
            ShellOutcome::Detached(info) => {
                panic!("no grace, yet the command was demoted as {}", info.work_id)
            }
        };
        assert!(started.elapsed() >= Duration::from_secs(3));
        assert_eq!(result.stdout.trim(), "finished");
        assert_eq!(result.exit_code, 0);
        assert!(registry.outstanding().is_empty());
        assert!(matches!(
            receiver.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
    }

    /// A grace of zero demotes at the first poll after the spawn.
    #[test]
    fn a_zero_grace_demotes_at_the_first_poll() {
        let workdir = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let (outcome, registry, _receiver) = detached_run(
            workdir.path(),
            runtime.path(),
            "sleep 5",
            Some(Duration::ZERO),
        );

        let ShellOutcome::Detached(info) = outcome else {
            panic!("a zero grace must demote a five-second command: {outcome:?}");
        };
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "demotion took {:?}",
            started.elapsed()
        );
        assert_eq!(info.grace_ms, Some(0));
        registry.kill_running();
    }

    /// A command that beats the grace period returns its result to the turn and registers nothing.
    #[test]
    fn a_fast_command_finishes_in_the_foreground() {
        let workdir = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let (outcome, registry, mut receiver) = detached_run(
            workdir.path(),
            runtime.path(),
            "echo hi",
            Duration::from_secs(10),
        );

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
        let runtime = tempfile::tempdir().unwrap();
        let (outcome, _registry, mut receiver) = detached_run(
            workdir.path(),
            runtime.path(),
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

        let log = std::fs::read_to_string(&completion.output_path).unwrap();
        assert!(log.starts_with("Command: bash -c "), "{log}");
        assert!(
            log.contains("\n\nto-stderr\n"),
            "stderr is in the output section: {log}"
        );
        assert!(log.ends_with("\n\nExit code: 7\n"), "{log}");

        let (outcome, _registry, mut receiver) = detached_run(
            workdir.path(),
            runtime.path(),
            "sleep 3; exit 0",
            Duration::from_secs(1),
        );
        assert!(matches!(outcome, ShellOutcome::Detached(_)));
        assert_eq!(await_completion(&mut receiver).status(), "ok");
    }

    /// The demotion text names the work id, names the tool that takes it only when the session
    /// offers that tool, says what becomes of the command at session end, and stays small for
    /// either lifecycle; only a lifecycle that delivers completions mentions the separate task.
    #[test]
    fn the_demotion_result_names_the_tool_and_stays_small() {
        for (delivered, tool_offered) in
            [(true, true), (false, true), (true, false), (false, false)]
        {
            let result = demotion_tool_result("wrk_0a1b", delivered, tool_offered);
            let data = result.data.as_deref().unwrap();
            assert!(data.len() < 400, "{} bytes: {data}", data.len());
            assert!(data.contains("wrk_0a1b"), "{data}");
            assert_eq!(data.contains("background-command"), tool_offered, "{data}");
            assert_eq!(data.contains(" tool"), tool_offered, "{data}");
            assert!(data.contains("is killed"), "{data}");
            for retired in [concat!("nothing to ", "poll"), concat!("no way ", "to ask")] {
                assert!(!data.contains(retired), "{retired}: {data}");
            }
            assert_eq!(data.contains("separate task"), delivered, "{data}");
            assert_eq!(
                result.summary.as_deref(),
                Some("Command still running in the background as wrk_0a1b.")
            );
            assert_eq!(
                result.metadata,
                vec![("work_id".to_string(), "wrk_0a1b".to_string())]
            );
        }
        assert_eq!(
            demotion_tool_result("wrk_1", false, true).data.as_deref(),
            Some(
                "The command is still running and was moved to the background as wrk_1. The \
                 background-command tool reports on it, waits for it or stops it by that id. A \
                 command still running when the session ends is killed."
            )
        );
        assert_eq!(
            demotion_tool_result("wrk_1", true, true).data.as_deref(),
            Some(
                "The command is still running and was moved to the background as wrk_1. The \
                 background-command tool reports on it, waits for it or stops it by that id. A \
                 command still running when the session ends is killed. If it exits before \
                 background-command has reported its exit, its result also arrives later as a \
                 separate task."
            )
        );
        assert_eq!(
            demotion_tool_result("wrk_1", false, false).data.as_deref(),
            Some(
                "The command is still running and was moved to the background as wrk_1. A \
                 command still running when the session ends is killed."
            )
        );
        assert_eq!(
            demotion_tool_result("wrk_1", true, false).data.as_deref(),
            Some(
                "The command is still running and was moved to the background as wrk_1. A \
                 command still running when the session ends is killed. When it exits, its \
                 result arrives later as a separate task."
            )
        );
    }

    // ── background-command: status, wait, stop ──────────────────────────────

    fn demoted(outcome: ShellOutcome) -> DetachedDispatchInfo {
        match outcome {
            ShellOutcome::Detached(info) => info,
            ShellOutcome::Finished(result) => {
                panic!("the command finished inside its grace period: {result:?}")
            }
        }
    }

    /// The command's state now, and the text `background-command` renders for it.
    fn read_text(
        registry: &DetachedRegistry,
        work_id: &str,
        waited: Option<Duration>,
    ) -> (CommandSnapshot, ToolResult) {
        let snapshot = registry.read(work_id).expect("the work id was issued");
        let (result, _) = crate::background_command::render(&snapshot, waited);
        (snapshot, result)
    }

    fn log_of(snapshot: &CommandSnapshot) -> String {
        std::fs::read_to_string(&snapshot.output_path).expect("the log exists while it runs")
    }

    /// Output printed after demotion, on both streams, is in the status and already in the log
    /// while the command runs; the trailer lands once it ends.
    #[tokio::test]
    async fn status_shows_output_so_far_while_the_command_runs() {
        let workdir = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let (outcome, registry, _receiver) = detached_run(
            workdir.path(),
            runtime.path(),
            "echo first; sleep 2; echo second >&2; sleep 30",
            Duration::from_secs(1),
        );
        let _guard = KillRunningOnDrop(Arc::clone(&registry));
        let info = demoted(outcome);

        tokio::time::sleep(Duration::from_millis(2_500).saturating_sub(started.elapsed())).await;
        let (snapshot, result) = read_text(&registry, &info.work_id, None);
        let data = result.data.unwrap();
        assert_eq!(snapshot.state, CommandState::Running, "{data}");
        assert!(data.contains("state: running\n"), "{data}");
        assert!(data.contains("output_shown: all\n"), "{data}");
        assert!(data.ends_with("\n\nfirst\nsecond\n"), "{data}");
        let log = log_of(&snapshot);
        assert!(log.contains("first") && log.contains("second"), "{log}");
        assert!(!log.contains("Exit code:"), "{log}");

        assert!(registry.stop(&info.work_id).await);
        let (snapshot, _) = read_text(&registry, &info.work_id, None);
        assert_eq!(snapshot.state, CommandState::Stopped);
        let log = log_of(&snapshot);
        assert!(log.contains("\n\nExit code: "), "{log}");
    }

    /// Past 16 KiB the status shows exactly the last 16384 output bytes and says so, while the
    /// log holds every byte.
    #[tokio::test]
    async fn status_shows_the_last_16_kib_and_the_log_holds_everything() {
        let workdir = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let (outcome, registry, _receiver) = detached_run(
            workdir.path(),
            runtime.path(),
            "sleep 1.5; head -c 50000 /dev/zero | tr '\\0' a; echo; echo TAILMARK; sleep 30",
            Duration::from_secs(1),
        );
        let _guard = KillRunningOnDrop(Arc::clone(&registry));
        let info = demoted(outcome);

        let expected_bytes = 50_000 + "\nTAILMARK\n".len() as u64;
        let deadline = Instant::now() + Duration::from_secs(15);
        let (snapshot, result) = loop {
            let (snapshot, result) = read_text(&registry, &info.work_id, None);
            if snapshot.output_bytes >= expected_bytes {
                break (snapshot, result);
            }
            assert!(
                Instant::now() < deadline,
                "only {} bytes",
                snapshot.output_bytes
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        assert_eq!(snapshot.output_bytes, expected_bytes);
        assert!(result.truncated);
        let data = result.data.unwrap();
        assert!(data.contains("output_bytes: 50010\n"), "{}", &data[..300]);
        assert!(
            data.contains("output_shown: last 16384 bytes\n\n"),
            "{}",
            &data[..300]
        );
        let shown = data.split_once("\n\n").unwrap().1;
        let mut expected = "a".repeat(16_384 - "\nTAILMARK\n".len());
        expected.push_str("\nTAILMARK\n");
        assert_eq!(shown.len(), 16_384);
        assert_eq!(shown, expected);
        let log = log_of(&snapshot);
        assert!(
            log.contains(&format!("\n\n{}\nTAILMARK\n", "a".repeat(50_000))),
            "the log holds all 50000 bytes"
        );

        assert!(registry.stop(&info.work_id).await);
        let log = log_of(&registry.read(&info.work_id).unwrap());
        assert!(
            log.contains("TAILMARK\n\n\nExit code: "),
            "{}",
            &log[log.len() - 40..]
        );
    }

    #[tokio::test]
    async fn wait_returns_when_the_command_exits() {
        let workdir = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let (outcome, registry, _receiver) = detached_run(
            workdir.path(),
            runtime.path(),
            "sleep 3; echo done; exit 7",
            Duration::from_secs(1),
        );
        let _guard = KillRunningOnDrop(Arc::clone(&registry));
        let info = demoted(outcome);

        let waiting = Instant::now();
        let end = registry
            .wait(
                &info.work_id,
                crate::background_command::effective_wait(Some(30)),
                None,
            )
            .await;
        let waited = waiting.elapsed();
        assert_eq!(end, WaitEnd::Finished);
        assert!(waited < Duration::from_secs(6), "the wait took {waited:?}");
        let (snapshot, result) = read_text(&registry, &info.work_id, Some(waited));
        let data = result.data.unwrap();
        assert_eq!(snapshot.state, CommandState::Exited);
        for line in ["state: exited\n", "exit_code: 7\n", "status: error\n"] {
            assert!(data.contains(line), "{line}: {data}");
        }
        assert!(data.ends_with("\n\ndone\n"), "{data}");
        assert!(registry.exit_reported(&info.work_id));
    }

    // ── delivery: one claim per completion ─────────────────────────────────────

    /// A registry holding `work_id`, registered with no process behind it: these cases complete it
    /// by hand and never signal its group.
    fn registry_holding(work_id: &str) -> Arc<DetachedRegistry> {
        let (registry, _receiver) = DetachedRegistry::new();
        registry.register(
            DetachedWork {
                work_id: work_id.to_string(),
                binary: "/usr/bin/bash".to_string(),
                command: "sleep 3".to_string(),
                started_at_ms: 1_000,
            },
            Instant::now(),
            0,
            format!("/records/logs/{work_id}.log"),
            Arc::new(Mutex::new(LiveOutput {
                file: None,
                tail: VecDeque::new(),
                section_bytes: 0,
                write_error: None,
            })),
        );
        registry
    }

    fn completion_of(work_id: &str) -> DetachedCompletion {
        DetachedCompletion {
            work_id: work_id.to_string(),
            binary: "/usr/bin/bash".to_string(),
            command: "sleep 3".to_string(),
            exit_code: 0,
            duration_ms: 3_000,
            exited_at_ms: 4_000,
            output_path: format!("/records/logs/{work_id}.log"),
            output_bytes: 0,
            resource_limit: None,
            output_holders: crate::shell::OutputHolders::Closed,
            context_id: "ctx_1".to_string(),
            provenance: completion_provenance(),
            error: None,
        }
    }

    /// The first read of a finished command claims it for `background-command`: that read alone
    /// says so, and the task loop's later claim fails, so the completion is neither enqueued nor
    /// recorded twice.
    #[test]
    fn a_read_claims_a_finished_command_before_the_task_loop_does() {
        let registry = registry_holding("wrk_read");
        registry.complete(completion_of("wrk_read"));

        let first = registry.read("wrk_read").unwrap();
        assert_eq!(first.state, CommandState::Exited);
        assert!(first.delivered_now, "the first read of the exit claims it");
        assert!(
            !registry.read("wrk_read").unwrap().delivered_now,
            "a second read claims nothing"
        );
        assert!(registry.exit_reported("wrk_read"));
        assert!(
            !registry.claim_for_task("wrk_read"),
            "the task loop finds the completion already delivered"
        );
    }

    /// The task loop claiming first leaves nothing for a later read to deliver.
    #[test]
    fn the_task_loop_claims_a_completion_no_read_has_delivered() {
        let registry = registry_holding("wrk_task");
        registry.complete(completion_of("wrk_task"));

        assert!(registry.claim_for_task("wrk_task"));
        assert!(!registry.claim_for_task("wrk_task"), "claimed once");
        let read = registry.read("wrk_task").unwrap();
        assert_eq!(read.state, CommandState::Exited);
        assert!(
            !read.delivered_now,
            "the queued task delivers it, not the read"
        );
        assert!(
            !registry.exit_reported("wrk_task"),
            "a task delivery is not a background-command one"
        );
    }

    /// A running command has no completion to claim, so reading it leaves both paths open.
    #[test]
    fn a_read_of_a_running_command_claims_nothing() {
        let registry = registry_holding("wrk_running");
        let read = registry.read("wrk_running").unwrap();
        assert_eq!(read.state, CommandState::Running);
        assert!(!read.delivered_now);
        assert!(!registry.exit_reported("wrk_running"));

        registry.complete(completion_of("wrk_running"));
        assert!(registry.read("wrk_running").unwrap().delivered_now);
    }

    /// `exited_at_ms` is when the command's wait returned, stamped by its own thread before the
    /// completion is sent, not when anything later read it.
    #[test]
    fn exited_at_ms_is_stamped_at_the_exit_not_at_the_read() {
        let workdir = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let before = crate::trace::timestamp_ms();
        let (outcome, registry, mut receiver) = detached_run(
            workdir.path(),
            runtime.path(),
            "sleep 2; exit 3",
            Duration::from_millis(500),
        );
        let _guard = KillRunningOnDrop(Arc::clone(&registry));
        let info = demoted(outcome);

        let completion = await_completion(&mut receiver);
        let received = crate::trace::timestamp_ms();
        assert!(
            completion.exited_at_ms >= before + 2_000 && completion.exited_at_ms <= received,
            "exited at {} for a command started after {before} and received at {received}",
            completion.exited_at_ms
        );

        std::thread::sleep(Duration::from_millis(400));
        let read = registry.read(&info.work_id).unwrap();
        assert!(read.delivered_now);
        assert_eq!(
            read.finished.unwrap().exited_at_ms,
            completion.exited_at_ms,
            "the read hands back the exit time the command's thread stamped"
        );
    }

    #[tokio::test]
    async fn wait_returns_at_its_bound_with_the_command_still_running() {
        let workdir = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let (outcome, registry, _receiver) = detached_run(
            workdir.path(),
            runtime.path(),
            "sleep 60",
            Duration::from_secs(1),
        );
        let _guard = KillRunningOnDrop(Arc::clone(&registry));
        let info = demoted(outcome);

        let waiting = Instant::now();
        let end = registry
            .wait(
                &info.work_id,
                crate::background_command::effective_wait(Some(1)),
                None,
            )
            .await;
        let waited = waiting.elapsed();
        assert_eq!(end, WaitEnd::TimedOut);
        assert!(
            waited >= Duration::from_secs(1) && waited < Duration::from_secs(3),
            "the wait took {waited:?}"
        );
        let (snapshot, result) = read_text(&registry, &info.work_id, Some(waited));
        let data = result.data.unwrap();
        assert_eq!(snapshot.state, CommandState::Running);
        assert!(data.contains("waited_secs: 1\n"), "{data}");
        assert!(!data.contains("exit_code"), "{data}");
        assert!(!registry.exit_reported(&info.work_id));
    }

    /// `stop` takes the command and the process it started in its group, and the command reads
    /// `stopped` with the exit code the signal gave it.
    #[tokio::test]
    async fn stop_ends_the_command_and_its_children() {
        let workdir = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let _leader = KillPidFileOnDrop(workdir.path().join("leader.pid"));
        let _child = KillPidFileOnDrop(workdir.path().join("child.pid"));
        let (outcome, registry, _receiver) = detached_run(
            workdir.path(),
            runtime.path(),
            "echo $$ > leader.pid; sleep 300 > /dev/null 2>&1 & echo $! > child.pid; sleep 300",
            Duration::from_secs(1),
        );
        let _guard = KillRunningOnDrop(Arc::clone(&registry));
        let info = demoted(outcome);

        let stopping = Instant::now();
        assert!(registry.stop(&info.work_id).await);
        let took = stopping.elapsed();
        assert!(took < Duration::from_secs(5), "the stop took {took:?}");
        let (snapshot, result) = read_text(&registry, &info.work_id, None);
        let data = result.data.unwrap();
        assert_eq!(snapshot.state, CommandState::Stopped, "{data}");
        let exit_code = snapshot.finished.as_ref().unwrap().exit_code;
        assert!([143, 137].contains(&exit_code), "{data}");
        assert!(data.contains("state: stopped\n"), "{data}");
        assert_eq!(snapshot.finished.unwrap().resource_limit, None);
        for file in ["leader.pid", "child.pid"] {
            let pid = read_pid(&workdir.path().join(file));
            assert!(
                process_dead_within(pid, Duration::from_secs(2)),
                "{file}'s process {pid} is still running"
            );
        }
    }

    /// A command that ignores `SIGTERM` is killed with `SIGKILL` once the grace has passed.
    #[tokio::test]
    async fn stop_escalates_to_sigkill_when_sigterm_is_ignored() {
        let workdir = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let _leader = KillPidFileOnDrop(workdir.path().join("leader.pid"));
        let (outcome, registry, _receiver) = detached_run(
            workdir.path(),
            runtime.path(),
            "echo $$ > leader.pid; trap '' TERM; while :; do sleep 1; done",
            Duration::from_secs(1),
        );
        let _guard = KillRunningOnDrop(Arc::clone(&registry));
        let info = demoted(outcome);

        let stopping = Instant::now();
        assert!(registry.stop(&info.work_id).await);
        let took = stopping.elapsed();
        assert!(took < Duration::from_secs(5), "the stop took {took:?}");
        assert!(
            took >= crate::background_command::STOP_TERM_GRACE,
            "{took:?}"
        );
        let (snapshot, result) = read_text(&registry, &info.work_id, None);
        assert_eq!(snapshot.state, CommandState::Stopped, "{:?}", result.data);
        assert_eq!(snapshot.finished.unwrap().exit_code, 137);
        assert_eq!(registry.signals_sent(&info.work_id), 2);
        let pid = read_pid(&workdir.path().join("leader.pid"));
        assert!(
            process_dead_within(pid, Duration::from_secs(2)),
            "{pid} still runs"
        );
    }

    /// A command that already exited is reported as it is, and nothing is signalled.
    #[tokio::test]
    async fn stop_on_a_finished_command_reports_it_unchanged() {
        let workdir = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let (outcome, registry, mut receiver) = detached_run(
            workdir.path(),
            runtime.path(),
            "sleep 1.5; exit 0",
            Duration::from_secs(1),
        );
        let info = demoted(outcome);
        await_completion(&mut receiver);

        assert!(registry.stop(&info.work_id).await);
        let (snapshot, result) = read_text(&registry, &info.work_id, None);
        let data = result.data.unwrap();
        assert_eq!(snapshot.state, CommandState::Exited, "{data}");
        assert!(data.contains("exit_code: 0\nstatus: ok\n"), "{data}");
        assert_eq!(registry.signals_sent(&info.work_id), 0);
        assert!(
            !registry.stop(&new_work_id()).await,
            "an unknown id is not stopped"
        );
    }

    /// With no cgroup scope, the registry's own kill is what ends a demoted command at session
    /// end, the process it started in its group included.
    #[test]
    fn kill_running_leaves_nothing_without_a_cgroup_scope() {
        let workdir = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let _leader = KillPidFileOnDrop(workdir.path().join("leader.pid"));
        let _child = KillPidFileOnDrop(workdir.path().join("child.pid"));
        let enforcement = ShellEnforcement::environment_only()
            .with_runtime_dir(crate::sandbox::test_runtime_dir(runtime.path()), None);
        assert!(enforcement.cgroup_scope.is_none());
        let (outcome, registry, _receiver) = detached_run_with(
            workdir.path(),
            &enforcement,
            "echo $$ > leader.pid; sleep 300 > /dev/null 2>&1 & echo $! > child.pid; sleep 300",
            Duration::from_secs(1),
        );
        let info = demoted(outcome);
        assert_eq!(registry.outstanding().len(), 1);

        registry.kill_running();
        for file in ["leader.pid", "child.pid"] {
            let pid = read_pid(&workdir.path().join(file));
            assert!(
                process_dead_within(pid, Duration::from_secs(2)),
                "{file}'s process {pid} outlived kill_running"
            );
        }
        assert_eq!(registry.signals_sent(&info.work_id), 1);
    }

    /// No input can make the demotion text grow: two work ids of equal length cost the same, and
    /// the cost is small.
    #[test]
    fn the_demotion_result_is_a_constant_size() {
        for delivered in [true, false] {
            for tool_offered in [true, false] {
                the_demotion_result_is_a_constant_size_when(delivered, tool_offered);
            }
        }
    }

    fn the_demotion_result_is_a_constant_size_when(delivered: bool, tool_offered: bool) {
        let reduce = |work_id: &str| {
            let result = demotion_tool_result(work_id, delivered, tool_offered);
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

    // ── The operator's abandonment report ────────────────────────────────────

    fn still_running(work_id: &str, running_ms: u64) -> AbandonedWork {
        AbandonedWork {
            work_id: work_id.to_string(),
            binary: "/usr/bin/bash".to_string(),
            command: "sleep 45; echo done".to_string(),
            disposition: AbandonedDisposition::StillRunning { running_ms },
        }
    }

    /// A records directory for the report tests, which name paths in it and write nothing.
    fn report_records() -> (tempfile::TempDir, crate::workdir_fs::WorkdirRoot) {
        let dir = tempfile::tempdir().unwrap();
        let root = crate::workdir_fs::WorkdirRoot::open(dir.path()).unwrap();
        (dir, root)
    }

    fn finished_too_late(
        records_root: &crate::workdir_fs::WorkdirRoot,
        work_id: &str,
    ) -> AbandonedWork {
        AbandonedWork {
            work_id: work_id.to_string(),
            binary: "/usr/bin/bash".to_string(),
            command: "sleep 2; echo done".to_string(),
            disposition: AbandonedDisposition::FinishedTooLate {
                exit_code: 0,
                duration_ms: 2_014,
                output_path: output_path_for(records_root, work_id).path,
                output_bytes: 91,
                resource_limit: None,
                wait_error: None,
                status: "ok",
            },
        }
    }

    /// A command still running is reported with the four things that are known about it and as
    /// killed, which the session's end did to it, and the report names the log that holds what
    /// it printed.
    #[test]
    fn a_still_running_command_is_reported_as_killed_with_what_is_known() {
        let (records, records_root) = report_records();
        let text = abandonment_report_text(
            "ses_abc",
            &records_root,
            &[still_running("wrk_0a1b", 45_012)],
        );

        assert!(
            text.contains("1 background shell command discarded"),
            "{text}"
        );
        assert!(text.contains("Session ses_abc"), "{text}");
        assert!(text.contains("work_id: wrk_0a1b"), "{text}");
        assert!(text.contains("binary: /usr/bin/bash"), "{text}");
        assert!(text.contains("command: sleep 45; echo done"), "{text}");
        assert!(
            text.contains("state: killed at session end after 45012 ms"),
            "{text}"
        );
        let path = records.path().join("logs/wrk_0a1b.log");
        assert!(
            text.contains(&format!("before the kill is in {}.", path.display())),
            "the report names the log by its absolute path: {text}"
        );
        assert!(!text.contains("exit_code:"), "no exit code exists: {text}");
        for retired in [
            "nothing was killed",
            "Nothing was waited for",
            "keeps running",
        ] {
            assert!(
                !text.contains(retired),
                "the session's end kills what it started, so the report must not say {retired:?}: \
                 {text}"
            );
        }
    }

    /// The `FinishedTooLate` arm names the exit code, the status, the duration and the real
    /// output file, rather than being flattened into "still running, result lost". This is the
    /// fidelity a clean exit has and a killed runtime does not.
    #[test]
    fn a_command_that_finished_during_teardown_is_reported_with_its_result() {
        let (records, records_root) = report_records();
        let text = abandonment_report_text(
            "ses_abc",
            &records_root,
            &[finished_too_late(&records_root, "wrk_0c2d")],
        );

        assert!(
            text.contains("state: finished during teardown after 2014 ms"),
            "{text}"
        );
        assert!(text.contains("status: ok"), "{text}");
        assert!(text.contains("exit_code: 0"), "{text}");
        let path = records.path().join("logs/wrk_0c2d.log");
        assert!(
            text.contains(&format!("output: {} (91 bytes)\n", path.display())),
            "{text}"
        );
        assert!(!text.contains("in the capsule workdir"), "{text}");
        assert!(!text.contains("killed at session end"), "{text}");
    }

    /// One block for the session, then one entry per command: a session that discarded several
    /// states the condition once and enumerates below it.
    #[test]
    fn the_report_states_the_condition_once_and_enumerates_every_command() {
        let (_records, records_root) = report_records();
        let text = abandonment_report_text(
            "ses_abc",
            &records_root,
            &[
                still_running("wrk_0a1b", 12),
                finished_too_late(&records_root, "wrk_0c2d"),
            ],
        );

        assert_eq!(
            text.matches("2 background shell commands discarded")
                .count(),
            1
        );
        assert_eq!(text.matches("work_id: ").count(), 2);
        assert!(text.contains("wrk_0a1b"), "{text}");
        assert!(text.contains("wrk_0c2d"), "{text}");
    }

    /// An attributed resource limit rides along, on the same terms
    /// [`DetachedCompletion::message_text`] carries it.
    #[test]
    fn an_attributed_resource_limit_is_named_in_the_report() {
        let (_records, records_root) = report_records();
        let mut work = finished_too_late(&records_root, "wrk_0c2d");
        if let AbandonedDisposition::FinishedTooLate {
            resource_limit,
            exit_code,
            status,
            ..
        } = &mut work.disposition
        {
            *resource_limit = Some("memory".to_string());
            *exit_code = 137;
            *status = "error";
        }
        let text = abandonment_report_text("ses_abc", &records_root, &[work]);

        assert!(text.contains("resource_limit: memory"), "{text}");
        assert!(text.contains("status: error"), "{text}");
        assert!(text.contains("exit_code: 137"), "{text}");
    }

    /// `running_ms` and `recovered` are what the `shell_abandoned` line is written from, so the
    /// two arms must disagree on `recovered` and agree on carrying a duration.
    #[test]
    fn only_a_command_that_finished_carries_a_recovered_result() {
        assert_eq!(still_running("wrk_0a1b", 45_012).running_ms(), 45_012);
        assert!(still_running("wrk_0a1b", 45_012).recovered().is_none());

        let (records, records_root) = report_records();
        let finished = finished_too_late(&records_root, "wrk_0c2d");
        let path = records.path().join("logs/wrk_0c2d.log");
        assert_eq!(finished.running_ms(), 2_014);
        assert_eq!(
            finished.recovered(),
            Some((0, path.to_str().unwrap(), 91)),
            "the exit code, the output path and its byte count travel together"
        );
    }

    /// A wait that itself failed is named. It is what turns `status` to `"error"` on a command
    /// that exited zero, so a report carrying the status without the reason states a failure and
    /// withholds the only thing that explains it.
    #[test]
    fn a_failed_wait_is_named_beside_the_status_it_explains() {
        let (_records, records_root) = report_records();
        let mut work = finished_too_late(&records_root, "wrk_0c2d");
        if let AbandonedDisposition::FinishedTooLate {
            wait_error, status, ..
        } = &mut work.disposition
        {
            *wait_error = Some("no child processes".to_string());
            *status = "error";
        }
        let text = abandonment_report_text("ses_abc", &records_root, &[work]);

        assert!(text.contains("status: error"), "{text}");
        assert!(text.contains("wait_error: no child processes"), "{text}");
    }

    /// The clean-exit report must not read like [`LostReport::message_text`]: that text asserts
    /// no exit code was recorded, which is false here.
    #[test]
    fn the_report_does_not_claim_nothing_is_recoverable() {
        let (_records, records_root) = report_records();
        let text =
            abandonment_report_text("ses_abc", &records_root, &[still_running("wrk_0a1b", 12)]);
        assert!(
            !text.contains("No exit code was recorded"),
            "the clean-exit report is not the crash report: {text}"
        );
    }
}
