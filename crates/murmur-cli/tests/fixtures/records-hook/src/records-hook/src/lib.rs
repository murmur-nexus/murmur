// Test-only `on-session-end` hook that writes the records a scoring hook writes, through the one
// directory its manifest entry grants it as `"."`.
//
// At `on-session-end` it:
//
// 1. writes `./eval.jsonl`, replacing it, with one `dataset_run` line whose `overall` is `pass`
//    and whose one score is `records-hook: 0.75`, carrying `MURMUR_DATASET_ID` and
//    `MURMUR_CASE_ID` when the runtime set them;
// 2. appends `records-hook wrote eval.jsonl` to `./logs/hook-records-hook.log`.
//
// Declared `capabilities.filesystem.records: true`, its `"."` is the session records directory's
// `hooks/`; declared `scope: .`, the workdir. Every other event is a no-op, and every outcome is
// `none`; a write that fails is returned as the hook's error, which the runtime logs.

wit_bindgen::generate!({
    path: "../../../../../../capsule-runtime/wit/hook",
    world: "hook",
    generate_all,
});

use std::io::Write;

use exports::murmur::hook::lifecycle::{
    CompactionEvent, Guest, HookOutput, InferenceEvent, SessionContext, SessionEndEvent,
    ShellEvent, StageEvent, TaskEndEvent, TaskStartEvent, ToolEvent,
};

struct RecordsHook;

fn write_records() -> Result<(), String> {
    let id = |name: &str| {
        std::env::var(name)
            .map(serde_json::Value::from)
            .unwrap_or(serde_json::Value::Null)
    };
    let line = serde_json::json!({
        "record_type": "dataset_run",
        "ts": 1,
        "dataset_id": id("MURMUR_DATASET_ID"),
        "case_id": id("MURMUR_CASE_ID"),
        "overall": "pass",
        "scores": {"records-hook": 0.75},
    });
    std::fs::write("eval.jsonl", format!("{line}\n")).map_err(|e| format!("eval.jsonl: {e}"))?;
    std::fs::create_dir_all("logs").map_err(|e| format!("logs: {e}"))?;
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("logs/hook-records-hook.log")
        .map_err(|e| format!("log: {e}"))?;
    writeln!(log, "records-hook wrote eval.jsonl").map_err(|e| format!("log: {e}"))
}

impl Guest for RecordsHook {
    fn on_stage(_event: StageEvent) -> Result<HookOutput, String> {
        Ok(HookOutput::None)
    }
    fn on_session_start(_ctx: SessionContext) -> Result<HookOutput, String> {
        Ok(HookOutput::None)
    }
    fn on_task_start(_event: TaskStartEvent) -> Result<HookOutput, String> {
        Ok(HookOutput::None)
    }
    fn on_inference(_event: InferenceEvent) -> Result<HookOutput, String> {
        Ok(HookOutput::None)
    }
    fn on_tool_call(_event: ToolEvent) -> Result<HookOutput, String> {
        Ok(HookOutput::None)
    }
    fn on_shell(_event: ShellEvent) -> Result<HookOutput, String> {
        Ok(HookOutput::None)
    }
    fn on_compaction(_event: CompactionEvent) -> Result<HookOutput, String> {
        Ok(HookOutput::None)
    }
    fn on_task_end(_event: TaskEndEvent) -> Result<HookOutput, String> {
        Ok(HookOutput::None)
    }
    fn on_session_end(_event: SessionEndEvent) -> Result<HookOutput, String> {
        write_records()?;
        Ok(HookOutput::None)
    }
}

export!(RecordsHook);
