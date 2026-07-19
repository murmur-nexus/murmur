use std::{
    path::Path,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use serde::Serialize;
use serde_json::Value;
use tokio::{
    fs::{File, OpenOptions},
    io::{AsyncWriteExt, BufWriter},
};

pub(crate) struct TraceWriter {
    writer: BufWriter<File>,
    session_id: String,
    capsule_name: String,
    capsule_version: String,
    model: String,
    capabilities: Vec<String>,
    include_tool_output: bool,
    session_start_time: Instant,
    session_started: bool,
    session_ended: bool,
    // Running totals — updated as events are written, used for fallback session_end on error exit
    total_turns: u32,
    total_input_tokens: u64,
    total_output_tokens: u64,
    total_tool_calls: u32,
    total_shell_calls: u32,
    // Per-task counters — reset on each write_task_start; consumed by write_task_end
    task_turns: u32,
    task_input_tokens: u64,
    task_output_tokens: u64,
    task_tool_calls: u32,
    task_shell_calls: u32,
    task_start_instant: Option<Instant>,
    pub(crate) active_task_id: Option<String>,
}

// ── Event structs (Serialize → JSONL lines) ──────────────────────────────────

#[derive(Serialize)]
struct SessionStartEvent {
    event_type: &'static str,
    session_id: String,
    timestamp: u64,
    capsule_name: String,
    capsule_version: String,
    model: String,
    max_turns: u32,
    capabilities: Vec<String>,
    tools_declared: Vec<String>,
}

#[derive(Serialize)]
struct InferenceEvent {
    event_type: &'static str,
    session_id: String,
    timestamp: u64,
    turn: u32,
    input_tokens: u64,
    output_tokens: u64,
    decision: String,
    tool_name: Option<String>,
}

#[derive(Serialize)]
struct ToolCallEvent {
    event_type: &'static str,
    session_id: String,
    timestamp: u64,
    turn: u32,
    tool_name: String,
    input: Value,
    input_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<String>,
    output_bytes: u64,
    duration_ms: u64,
    status: String,
    /// The tool's self-declared `state_effect` for this call (`read`/`mutate`),
    /// lifted verbatim from `tool-result.metadata`. Absent when the tool declared
    /// nothing; consumers treat absence conservatively. See `wit/tool.wit`.
    #[serde(skip_serializing_if = "Option::is_none")]
    state_effect: Option<String>,
    /// The resource this call addressed, as declared by the tool and lifted verbatim
    /// from `tool-result.metadata`. An opaque, tool-defined string — never parsed here.
    /// Absent when the tool declared nothing, in which case consumers fall back to
    /// guessing the resource from the call's input. See `wit/tool.wit`.
    #[serde(skip_serializing_if = "Option::is_none")]
    resource_id: Option<String>,
}

#[derive(Serialize)]
struct SkillCallEvent {
    event_type: &'static str,
    session_id: String,
    timestamp: u64,
    turn: u32,
    skill_name: String,
    output_bytes: u64,
    duration_ms: u64,
    status: String,
}

#[derive(Serialize)]
struct ShellEvent {
    event_type: &'static str,
    session_id: String,
    timestamp: u64,
    turn: u32,
    command: String,
    exit_code: i32,
    stdout_bytes: u64,
    stderr_bytes: u64,
    duration_ms: u64,
}

#[derive(Serialize)]
struct CompactionEvent {
    event_type: &'static str,
    session_id: String,
    timestamp: u64,
    turn: u32,
    tokens_before: u64,
    tokens_after: u64,
}

#[derive(Serialize)]
struct A2aTaskReceivedEvent {
    event_type: &'static str,
    session_id: String,
    timestamp: u64,
    task_id: String,
    context_id: String,
    message_id: String,
    traceparent_from_caller: Option<String>,
}

#[derive(Serialize)]
struct A2aSendEvent {
    event_type: &'static str,
    session_id: String,
    timestamp: u64,
    peer_url: String,
    message_id: String,
    task_id: String,
    context_id: String,
    traceparent: Option<String>,
}

#[derive(Serialize)]
struct SessionEndEvent {
    event_type: &'static str,
    session_id: String,
    timestamp: u64,
    total_turns: u32,
    total_input_tokens: u64,
    total_output_tokens: u64,
    total_tool_calls: u32,
    total_shell_calls: u32,
    duration_ms: u64,
    exit_status: String,
}

#[derive(Serialize)]
struct TaskStartEvent {
    event_type: &'static str,
    session_id: String,
    timestamp: u64,
    task_id: String,
    context_id: String,
    source: String,
    message_parts_bytes: u64,
}

#[derive(Serialize)]
struct TaskEndEvent {
    event_type: &'static str,
    session_id: String,
    timestamp: u64,
    task_id: String,
    exit_status: String,
    duration_ms: u64,
    turns: u32,
    input_tokens: u64,
    output_tokens: u64,
    tool_calls: u32,
    shell_calls: u32,
}

// ── TraceWriter impl ─────────────────────────────────────────────────────────

impl TraceWriter {
    pub(crate) async fn open(
        workdir: &Path,
        session_id: String,
        capsule_name: String,
        capsule_version: String,
        model: String,
        capabilities: Vec<String>,
        include_tool_output: bool,
    ) -> std::io::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(workdir.join("trace.jsonl"))
            .await?;
        Ok(Self {
            writer: BufWriter::new(file),
            session_id,
            capsule_name,
            capsule_version,
            model,
            capabilities,
            include_tool_output,
            session_start_time: Instant::now(),
            session_started: false,
            session_ended: false,
            total_turns: 0,
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_tool_calls: 0,
            total_shell_calls: 0,
            task_turns: 0,
            task_input_tokens: 0,
            task_output_tokens: 0,
            task_tool_calls: 0,
            task_shell_calls: 0,
            task_start_instant: None,
            active_task_id: None,
        })
    }

    pub(crate) async fn write_session_start(
        &mut self,
        max_turns: u32,
        tools_declared: Vec<String>,
    ) -> std::io::Result<()> {
        let event = SessionStartEvent {
            event_type: "session_start",
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            capsule_name: self.capsule_name.clone(),
            capsule_version: self.capsule_version.clone(),
            model: self.model.clone(),
            max_turns,
            capabilities: self.capabilities.clone(),
            tools_declared,
        };
        self.write_event(&event).await?;
        self.session_started = true;
        Ok(())
    }

    pub(crate) async fn write_inference(
        &mut self,
        turn: u32,
        input_tokens: u64,
        output_tokens: u64,
        decision: String,
        tool_name: Option<String>,
    ) -> std::io::Result<()> {
        let event = InferenceEvent {
            event_type: "inference",
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            turn,
            input_tokens,
            output_tokens,
            decision,
            tool_name,
        };
        self.write_event(&event).await?;
        self.total_turns = self.total_turns.saturating_add(1);
        self.total_input_tokens = self.total_input_tokens.saturating_add(input_tokens);
        self.total_output_tokens = self.total_output_tokens.saturating_add(output_tokens);
        self.task_turns = self.task_turns.saturating_add(1);
        self.task_input_tokens = self.task_input_tokens.saturating_add(input_tokens);
        self.task_output_tokens = self.task_output_tokens.saturating_add(output_tokens);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn write_tool_call(
        &mut self,
        turn: u32,
        tool_name: String,
        input: Value,
        input_bytes: u64,
        output: &str,
        output_bytes: u64,
        duration_ms: u64,
        status: String,
        state_effect: Option<String>,
        resource_id: Option<String>,
    ) -> std::io::Result<()> {
        let event = ToolCallEvent {
            event_type: "tool_call",
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            turn,
            tool_name,
            input,
            input_bytes,
            output: self.include_tool_output.then(|| output.to_string()),
            output_bytes,
            duration_ms,
            status,
            state_effect,
            resource_id,
        };
        self.write_event(&event).await?;
        self.total_tool_calls = self.total_tool_calls.saturating_add(1);
        self.task_tool_calls = self.task_tool_calls.saturating_add(1);
        Ok(())
    }

    /// Records a skill invocation as a `skill_call` event. Skill calls are NOT counted in
    /// `total_tool_calls` — they appear in a separate `── Skill calls ──` section in `mur trace show`.
    pub(crate) async fn write_skill_call(
        &mut self,
        turn: u32,
        skill_name: String,
        output_bytes: u64,
        duration_ms: u64,
        status: String,
    ) -> std::io::Result<()> {
        let event = SkillCallEvent {
            event_type: "skill_call",
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            turn,
            skill_name,
            output_bytes,
            duration_ms,
            status,
        };
        self.write_event(&event).await
        // Intentionally does not increment total_tool_calls or task_tool_calls.
    }

    pub(crate) async fn write_shell(
        &mut self,
        turn: u32,
        command: String,
        exit_code: i32,
        stdout_bytes: u64,
        stderr_bytes: u64,
        duration_ms: u64,
    ) -> std::io::Result<()> {
        let event = ShellEvent {
            event_type: "shell",
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            turn,
            command,
            exit_code,
            stdout_bytes,
            stderr_bytes,
            duration_ms,
        };
        self.write_event(&event).await?;
        self.total_shell_calls = self.total_shell_calls.saturating_add(1);
        self.task_shell_calls = self.task_shell_calls.saturating_add(1);
        Ok(())
    }

    pub(crate) async fn write_a2a_task_received(
        &mut self,
        task_id: &str,
        context_id: &str,
        message_id: &str,
        traceparent_from_caller: Option<&str>,
    ) -> std::io::Result<()> {
        let event = A2aTaskReceivedEvent {
            event_type: "a2a_task_received",
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            task_id: task_id.to_string(),
            context_id: context_id.to_string(),
            message_id: message_id.to_string(),
            traceparent_from_caller: traceparent_from_caller.map(str::to_string),
        };
        self.write_event(&event).await
    }

    pub(crate) async fn write_a2a_send(
        &mut self,
        peer_url: &str,
        message_id: &str,
        task_id: &str,
        context_id: &str,
        traceparent: Option<&str>,
    ) -> std::io::Result<()> {
        let event = A2aSendEvent {
            event_type: "a2a_send",
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            peer_url: peer_url.to_string(),
            message_id: message_id.to_string(),
            task_id: task_id.to_string(),
            context_id: context_id.to_string(),
            traceparent: traceparent.map(str::to_string),
        };
        self.write_event(&event).await
    }

    pub(crate) async fn write_task_start(
        &mut self,
        task_id: &str,
        context_id: &str,
        source: &str,
        message_parts_bytes: u64,
    ) -> std::io::Result<()> {
        // Reset per-task counters for this task
        self.task_turns = 0;
        self.task_input_tokens = 0;
        self.task_output_tokens = 0;
        self.task_tool_calls = 0;
        self.task_shell_calls = 0;
        self.task_start_instant = Some(Instant::now());
        self.active_task_id = Some(task_id.to_string());

        let event = TaskStartEvent {
            event_type: "task_start",
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            task_id: task_id.to_string(),
            context_id: context_id.to_string(),
            source: source.to_string(),
            message_parts_bytes,
        };
        self.write_event(&event).await
    }

    pub(crate) async fn write_task_end(
        &mut self,
        task_id: &str,
        exit_status: &str,
    ) -> std::io::Result<()> {
        let duration_ms = self
            .task_start_instant
            .take()
            .map(|t| t.elapsed().as_millis().try_into().unwrap_or(u64::MAX))
            .unwrap_or(0);

        let event = TaskEndEvent {
            event_type: "task_end",
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            task_id: task_id.to_string(),
            exit_status: exit_status.to_string(),
            duration_ms,
            turns: self.task_turns,
            input_tokens: self.task_input_tokens,
            output_tokens: self.task_output_tokens,
            tool_calls: self.task_tool_calls,
            shell_calls: self.task_shell_calls,
        };
        self.active_task_id = None;
        self.write_event(&event).await
    }

    pub(crate) async fn write_compaction(
        &mut self,
        turn: u32,
        tokens_before: u64,
        tokens_after: u64,
    ) -> std::io::Result<()> {
        let event = CompactionEvent {
            event_type: "compaction",
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            turn,
            tokens_before,
            tokens_after,
        };
        self.write_event(&event).await
    }

    pub(crate) async fn write_session_end(&mut self, exit_status: &str) -> std::io::Result<()> {
        let duration_ms = self
            .session_start_time
            .elapsed()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX);
        let event = SessionEndEvent {
            event_type: "session_end",
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            total_turns: self.total_turns,
            total_input_tokens: self.total_input_tokens,
            total_output_tokens: self.total_output_tokens,
            total_tool_calls: self.total_tool_calls,
            total_shell_calls: self.total_shell_calls,
            duration_ms,
            exit_status: exit_status.to_string(),
        };
        self.write_event(&event).await?;
        self.session_ended = true;
        Ok(())
    }

    /// Writes session_end with the accumulated totals if session_start was written but
    /// session_end was not (error exit paths that bypass the normal Ok() return sites).
    pub(crate) async fn write_session_end_if_not_ended(
        &mut self,
        exit_status: &str,
    ) -> std::io::Result<()> {
        if self.session_started && !self.session_ended {
            self.write_session_end(exit_status).await?;
        }
        Ok(())
    }

    pub(crate) async fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush().await
    }

    async fn write_event(&mut self, event: &impl Serialize) -> std::io::Result<()> {
        let line = serde_json::to_string(event).map_err(std::io::Error::other)?;
        self.writer.write_all(line.as_bytes()).await?;
        self.writer.write_all(b"\n").await
    }
}

fn timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    async fn make_writer(dir: &std::path::Path) -> TraceWriter {
        make_writer_with_opts(dir, false).await
    }

    async fn make_writer_with_opts(dir: &std::path::Path, include_tool_output: bool) -> TraceWriter {
        TraceWriter::open(
            dir,
            "test-session-id".to_string(),
            "test-capsule".to_string(),
            "1.0.0".to_string(),
            "claude-test".to_string(),
            vec!["shell".to_string()],
            include_tool_output,
        )
        .await
        .unwrap()
    }

    fn read_events(dir: &std::path::Path) -> Vec<Value> {
        let content = std::fs::read_to_string(dir.join("trace.jsonl")).unwrap();
        content
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn session_start_fields() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_session_start(10, vec!["bash".to_string()])
            .await
            .unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e["event_type"], "session_start");
        assert_eq!(e["session_id"], "test-session-id");
        assert_eq!(e["capsule_name"], "test-capsule");
        assert_eq!(e["capsule_version"], "1.0.0");
        assert_eq!(e["model"], "claude-test");
        assert_eq!(e["max_turns"], 10);
        assert_eq!(e["capabilities"], serde_json::json!(["shell"]));
        assert_eq!(e["tools_declared"], serde_json::json!(["bash"]));
        assert!(e["timestamp"].as_u64().unwrap() > 0);
    }

    #[tokio::test]
    async fn inference_fields_and_snake_case() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_inference(
            0,
            100,
            50,
            "tool_call".to_string(),
            Some("bash".to_string()),
        )
        .await
        .unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        let e = &events[0];
        assert_eq!(e["event_type"], "inference");
        assert_eq!(e["turn"], 0);
        assert_eq!(e["input_tokens"], 100);
        assert_eq!(e["output_tokens"], 50);
        assert_eq!(e["decision"], "tool_call");
        assert_eq!(e["tool_name"], "bash");
        // Confirm snake_case (not camelCase)
        assert!(e.get("inputTokens").is_none(), "must use snake_case");
        assert!(e.get("toolName").is_none(), "must use snake_case");
    }

    #[tokio::test]
    async fn tool_call_fields() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_tool_call(
            1,
            "bash".to_string(),
            serde_json::json!({"command": "echo hi"}),
            42,
            "hi\n",
            100,
            55,
            "ok".to_string(),
            None,
            None,
        )
        .await
        .unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        let e = &events[0];
        assert_eq!(e["event_type"], "tool_call");
        assert_eq!(e["turn"], 1);
        assert_eq!(e["tool_name"], "bash");
        assert_eq!(e["input_bytes"], 42);
        assert_eq!(e["output_bytes"], 100);
        assert_eq!(e["duration_ms"], 55);
        assert_eq!(e["status"], "ok");
        assert!(
            e.get("state_effect").is_none(),
            "state_effect must be omitted when the tool declared nothing"
        );
    }

    #[tokio::test]
    async fn tool_call_records_declared_state_effect() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_tool_call(
            0,
            "murmur-tool-editor".to_string(),
            serde_json::json!({"operation": "read_file", "path": "a.txt"}),
            10,
            "hi\n",
            3,
            5,
            "ok".to_string(),
            Some("read".to_string()),
            None,
        )
        .await
        .unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        assert_eq!(
            events[0]["state_effect"], "read",
            "declared state_effect must be recorded verbatim on the tool_call event"
        );
    }

    #[tokio::test]
    async fn tool_call_records_declared_resource_id() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_tool_call(
            0,
            "murmur-tool-code-graph".to_string(),
            serde_json::json!({"symbol": "Foo::bar"}),
            10,
            "hi\n",
            3,
            5,
            "ok".to_string(),
            Some("read".to_string()),
            Some("sym:Foo".to_string()),
        )
        .await
        .unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        assert_eq!(
            events[0]["resource_id"], "sym:Foo",
            "declared resource_id must reach trace.jsonl verbatim, unparsed"
        );
    }

    #[tokio::test]
    async fn tool_call_omits_undeclared_resource_id() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_tool_call(
            0,
            "bash".to_string(),
            serde_json::json!({"command": "echo hi"}),
            10,
            "hi\n",
            3,
            5,
            "ok".to_string(),
            None,
            None,
        )
        .await
        .unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        assert!(
            events[0].get("resource_id").is_none(),
            "resource_id must be omitted entirely (not null) when the tool declared nothing"
        );
    }

    #[tokio::test]
    async fn shell_fields() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_shell(0, "echo hi".to_string(), 0, 7, 0, 10)
            .await
            .unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        let e = &events[0];
        assert_eq!(e["event_type"], "shell");
        assert_eq!(e["command"], "echo hi");
        assert_eq!(e["exit_code"], 0);
        assert_eq!(e["stdout_bytes"], 7);
        assert_eq!(e["stderr_bytes"], 0);
        assert_eq!(e["duration_ms"], 10);
    }

    #[tokio::test]
    async fn compaction_fields() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_compaction(3, 80000, 20000).await.unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        let e = &events[0];
        assert_eq!(e["event_type"], "compaction");
        assert_eq!(e["turn"], 3);
        assert_eq!(e["tokens_before"], 80000);
        assert_eq!(e["tokens_after"], 20000);
    }

    #[tokio::test]
    async fn session_end_fields_and_snake_case() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_inference(0, 100, 50, "tool_call".to_string(), None)
            .await
            .unwrap();
        w.write_tool_call(
            0,
            "bash".to_string(),
            serde_json::json!({"command": "ls"}),
            10,
            "file.txt\n",
            20,
            5,
            "ok".to_string(),
            None,
            None,
        )
        .await
        .unwrap();
        w.write_shell(0, "ls".to_string(), 0, 3, 0, 2)
            .await
            .unwrap();
        w.write_session_end("ok").await.unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        let se = events.last().unwrap();
        assert_eq!(se["event_type"], "session_end");
        assert_eq!(se["total_turns"], 1);
        assert_eq!(se["total_input_tokens"], 100);
        assert_eq!(se["total_output_tokens"], 50);
        assert_eq!(se["total_tool_calls"], 1);
        assert_eq!(se["total_shell_calls"], 1);
        assert_eq!(se["exit_status"], "ok");
        assert!(se["duration_ms"].as_u64().is_some());
        // snake_case checks
        assert!(se.get("exitStatus").is_none(), "must use snake_case");
        assert!(se.get("totalTurns").is_none(), "must use snake_case");
    }

    #[tokio::test]
    async fn write_session_end_if_not_ended_is_noop_when_already_ended() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_session_start(10, vec![]).await.unwrap();
        w.write_session_end("ok").await.unwrap();
        w.write_session_end_if_not_ended("failed").await.unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        assert_eq!(
            events
                .iter()
                .filter(|e| e["event_type"] == "session_end")
                .count(),
            1,
            "session_end should appear exactly once"
        );
    }

    #[tokio::test]
    async fn write_session_end_if_not_ended_writes_when_started_not_ended() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_session_start(10, vec![]).await.unwrap();
        // Simulate error path — session_end_if_not_ended in launch_session
        w.write_session_end_if_not_ended("failed").await.unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        let se = events
            .iter()
            .find(|e| e["event_type"] == "session_end")
            .unwrap();
        assert_eq!(se["exit_status"], "failed");
    }

    #[tokio::test]
    async fn no_session_end_written_when_session_never_started() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        // session_start never written — fallback should be no-op
        w.write_session_end_if_not_ended("failed").await.unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        assert!(
            events.iter().all(|e| e["event_type"] != "session_end"),
            "session_end must not appear when session_start was never written"
        );
    }

    #[tokio::test]
    async fn session_id_on_every_event() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_session_start(10, vec!["bash".to_string()])
            .await
            .unwrap();
        w.write_inference(0, 10, 5, "end_turn".to_string(), None)
            .await
            .unwrap();
        w.write_session_end("ok").await.unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        for e in &events {
            assert_eq!(e["session_id"], "test-session-id");
        }
    }

    #[tokio::test]
    async fn counts_match_per_event_sum() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_session_start(10, vec![]).await.unwrap();
        w.write_inference(0, 50, 25, "tool_call".to_string(), Some("bash".to_string()))
            .await
            .unwrap();
        w.write_tool_call(
            0,
            "bash".to_string(),
            serde_json::json!({"command": "echo"}),
            5,
            "ok",
            10,
            3,
            "ok".to_string(),
            None,
            None,
        )
        .await
        .unwrap();
        w.write_shell(0, "echo".to_string(), 0, 4, 0, 1)
            .await
            .unwrap();
        w.write_inference(1, 60, 30, "end_turn".to_string(), None)
            .await
            .unwrap();
        w.write_session_end("ok").await.unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        let inference_count = events
            .iter()
            .filter(|e| e["event_type"] == "inference")
            .count();
        let tool_count = events
            .iter()
            .filter(|e| e["event_type"] == "tool_call")
            .count();
        let shell_count = events.iter().filter(|e| e["event_type"] == "shell").count();

        let se = events
            .iter()
            .find(|e| e["event_type"] == "session_end")
            .unwrap();
        assert_eq!(
            se["total_turns"].as_u64().unwrap() as usize,
            inference_count
        );
        assert_eq!(
            se["total_tool_calls"].as_u64().unwrap() as usize,
            tool_count
        );
        assert_eq!(
            se["total_shell_calls"].as_u64().unwrap() as usize,
            shell_count
        );
    }

    #[tokio::test]
    async fn tool_call_input_always_present() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await; // include_tool_output = false
        w.write_tool_call(
            0,
            "bash".to_string(),
            serde_json::json!({"command": "ls -la"}),
            20,
            "total 8\n",
            80,
            12,
            "ok".to_string(),
            None,
            None,
        )
        .await
        .unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        let e = &events[0];
        assert_eq!(e["event_type"], "tool_call");
        assert_eq!(e["input"]["command"], "ls -la");
        assert!(e.get("input").is_some(), "input must always be present");
    }

    #[tokio::test]
    async fn tool_call_output_absent_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await; // include_tool_output = false
        w.write_tool_call(
            0,
            "bash".to_string(),
            serde_json::json!({"command": "ls"}),
            10,
            "file.txt\n",
            90,
            8,
            "ok".to_string(),
            None,
            None,
        )
        .await
        .unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        let e = &events[0];
        assert!(
            e.get("output").is_none(),
            "output must be absent when include_tool_output is false"
        );
        assert_eq!(e["output_bytes"], 90, "output_bytes must still be recorded");
    }

    #[tokio::test]
    async fn skill_call_writes_skill_call_event() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_skill_call(2, "my-skill".to_string(), 1024, 8, "ok".to_string())
            .await
            .unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        let e = &events[0];
        assert_eq!(e["event_type"], "skill_call");
        assert_eq!(e["turn"], 2);
        assert_eq!(e["skill_name"], "my-skill");
        assert_eq!(e["output_bytes"], 1024);
        assert_eq!(e["duration_ms"], 8);
        assert_eq!(e["status"], "ok");
    }

    #[tokio::test]
    async fn skill_call_does_not_increment_total_tool_calls() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_session_start(10, vec![]).await.unwrap();
        w.write_tool_call(
            0,
            "real-tool".to_string(),
            serde_json::json!({}),
            2,
            "data",
            4,
            5,
            "ok".to_string(),
            None,
            None,
        )
        .await
        .unwrap();
        w.write_skill_call(1, "my-skill".to_string(), 512, 3, "ok".to_string())
            .await
            .unwrap();
        w.write_session_end("ok").await.unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        let se = events.iter().find(|e| e["event_type"] == "session_end").unwrap();
        assert_eq!(se["total_tool_calls"], 1, "skill_call must not inflate total_tool_calls");
    }

    #[tokio::test]
    async fn tool_call_output_present_when_opted_in() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer_with_opts(dir.path(), true).await;
        w.write_tool_call(
            0,
            "bash".to_string(),
            serde_json::json!({"command": "echo hello"}),
            22,
            "hello\n",
            60,
            5,
            "ok".to_string(),
            None,
            None,
        )
        .await
        .unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        let e = &events[0];
        assert_eq!(
            e["output"].as_str().unwrap(),
            "hello\n",
            "output must be present when include_tool_output is true"
        );
        assert_eq!(e["output_bytes"], 60);
    }
}
