// Test-only hook that waits on the host at `on-session-start`: it sleeps 600 seconds — a
// `wasi:clocks` wait, during which no guest code runs — before returning `none`. Every other
// lifecycle function returns `none` at once.

wit_bindgen::generate!({
    path: "../../../../../../capsule-runtime/wit/hook",
    world: "hook",
    generate_all,
});

use exports::murmur::hook::lifecycle::{
    CompactionEvent, Guest, HookOutput, InferenceEvent, SessionContext, SessionEndEvent,
    ShellEvent, StageEvent, TaskEndEvent, TaskStartEvent, ToolEvent,
};

struct SleepHook;

impl Guest for SleepHook {
    fn on_stage(_event: StageEvent) -> Result<HookOutput, String> {
        Ok(HookOutput::None)
    }
    fn on_session_start(_ctx: SessionContext) -> Result<HookOutput, String> {
        std::thread::sleep(std::time::Duration::from_secs(600));
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
        Ok(HookOutput::None)
    }
}

export!(SleepHook);
