//! The host side of `murmur:stream/events`: what a WASM component's mid-turn signals write to the
//! task's stream.
//!
//! [`invoke_tool_component`] registers one host function per WIT function, each a thin wrapper
//! around the matching function here, so the frames they write are testable without a component.
//! Every function is synchronous, returns nothing and never fails: a signal that has nowhere to go
//! is dropped, and the guest is never trapped for one.
//!
//! `emit-chunk` and `emit-thinking-chunk` stream text from any component. The two tool-call
//! functions write only when the dispatch carries a [`ToolCallProgress`], which only the agent
//! loop's driver turn does: from a tool, from a hook's `run-inference` and outside an A2A task they
//! do nothing.
//!
//! [`invoke_tool_component`]: crate::runtime::invoke_tool_component

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Instant,
};

use crate::{
    streaming::{
        emit_chunk_sse, emit_frame, emit_thinking_chunk_sse, SseBroadcast, SseEventBuffer,
        StreamFrame, TaskToolCallProgressEvent, TaskToolCallStartedEvent,
    },
    tool_call_progress::{SettledCall, ToolCallProgress},
};

/// Where one dispatch's `murmur:stream/events` calls go.
#[derive(Clone)]
pub(crate) struct StreamEventsTarget {
    pub(crate) sse: Option<(SseBroadcast, Arc<Mutex<SseEventBuffer>>)>,
    pub(crate) task_id: Option<String>,
    /// Set by every `emit-chunk`, so the agent loop knows the turn's text was streamed.
    pub(crate) chunks_emitted: Arc<AtomicBool>,
    /// The calls the session's driver has started this attempt. `None` on every dispatch that is
    /// not the agent loop's driver turn.
    pub(crate) tool_calls: Option<Arc<Mutex<ToolCallProgress>>>,
}

/// The broadcast, buffer and task id a frame is written with.
struct Wire<'a> {
    tx: &'a SseBroadcast,
    buf: &'a Arc<Mutex<SseEventBuffer>>,
    task_id: &'a str,
}

impl StreamEventsTarget {
    /// Where a frame goes, when this dispatch serves an A2A task whose stream has somewhere to go.
    fn wire(&self) -> Option<Wire<'_>> {
        let (tx, buf) = self.sse.as_ref()?;
        Some(Wire {
            tx,
            buf,
            task_id: self.task_id.as_deref()?,
        })
    }
}

/// `emit-chunk`: one streamed text fragment.
pub(crate) fn emit_chunk(target: &StreamEventsTarget, chunk: &str) {
    target.chunks_emitted.store(true, Ordering::Relaxed);
    if let Some(wire) = target.wire() {
        emit_chunk_sse(wire.tx, wire.buf, wire.task_id, chunk);
    }
}

/// `emit-thinking-chunk`: one streamed thinking fragment.
pub(crate) fn emit_thinking_chunk(target: &StreamEventsTarget, chunk: &str) {
    if let Some(wire) = target.wire() {
        emit_thinking_chunk_sse(wire.tx, wire.buf, wire.task_id, chunk);
    }
}

/// `tool-call-started`: the driver's model has begun writing call `id`, named `name`. Writes a
/// `tool-call-started` frame when the tracker accepts the start.
pub(crate) fn tool_call_started(target: &StreamEventsTarget, id: &str, name: &str) {
    let (Some(wire), Some(tool_calls)) = (target.wire(), target.tool_calls.as_ref()) else {
        return;
    };
    if !lock(tool_calls).start(id, name) {
        return;
    }
    write(
        wire.tx,
        wire.buf,
        StreamFrame::ToolCallStarted,
        &TaskToolCallStartedEvent {
            id: wire.task_id.to_string(),
            tool_call_id: id.to_string(),
            tool_name: name.to_string(),
        },
    );
}

/// `tool-call-input-bytes`: the driver's model has written `bytes` of call `id`'s input as of
/// `now`. Writes a `tool-call-progress` frame when one is due.
pub(crate) fn tool_call_input_bytes(
    target: &StreamEventsTarget,
    id: &str,
    bytes: u64,
    now: Instant,
) {
    let (Some(wire), Some(tool_calls)) = (target.wire(), target.tool_calls.as_ref()) else {
        return;
    };
    let Some(size) = lock(tool_calls).report(id, bytes, now) else {
        return;
    };
    write_progress(wire.tx, wire.buf, wire.task_id, id, size);
}

/// The driver's dispatch returned at `now`: settle every call it started and has not settled,
/// writing the size still held for each. Returns each settled call's id and the name its start
/// carried, in the order they started. Outside an A2A task no call was started, so it writes and
/// returns nothing.
pub(crate) fn settle_started_calls(
    sse: &Option<(SseBroadcast, Arc<Mutex<SseEventBuffer>>)>,
    task_id: Option<&str>,
    tool_calls: &Mutex<ToolCallProgress>,
    now: Instant,
) -> Vec<(String, String)> {
    let settled = lock(tool_calls).complete_all(now);
    settled
        .into_iter()
        .map(|(id, SettledCall { name, held })| {
            if let (Some((tx, buf)), Some(task_id), Some(size)) = (sse, task_id, held) {
                write_progress(tx, buf, task_id, &id, size);
            }
            (id, name)
        })
        .collect()
}

fn write_progress(
    tx: &SseBroadcast,
    buf: &Arc<Mutex<SseEventBuffer>>,
    task_id: &str,
    tool_call_id: &str,
    input_bytes: u64,
) {
    write(
        tx,
        buf,
        StreamFrame::ToolCallProgress,
        &TaskToolCallProgressEvent {
            id: task_id.to_string(),
            tool_call_id: tool_call_id.to_string(),
            input_bytes,
        },
    );
}

fn write(
    tx: &SseBroadcast,
    buf: &Arc<Mutex<SseEventBuffer>>,
    frame: StreamFrame,
    data: &impl serde::Serialize,
) {
    if let Ok(data) = serde_json::to_string(data) {
        emit_frame(tx, buf, frame, &data);
    }
}

/// The tracker, recovered from a poisoned lock: a panic elsewhere must not trap the guest here.
fn lock(tool_calls: &Mutex<ToolCallProgress>) -> std::sync::MutexGuard<'_, ToolCallProgress> {
    tool_calls
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::Value;

    use super::*;
    use crate::streaming::ReplayResult;

    /// A target writing into a real buffer, carrying a tracker when `tracked`, and that buffer.
    fn target(tracked: bool) -> (StreamEventsTarget, Arc<Mutex<SseEventBuffer>>) {
        let (tx, _rx) = tokio::sync::broadcast::channel(16);
        let frames = Arc::new(Mutex::new(SseEventBuffer::new(64)));
        let target = StreamEventsTarget {
            sse: Some((tx, Arc::clone(&frames))),
            task_id: Some("tsk_test".to_string()),
            chunks_emitted: Arc::new(AtomicBool::new(false)),
            tool_calls: tracked.then(|| Arc::new(Mutex::new(ToolCallProgress::default()))),
        };
        (target, frames)
    }

    /// The `event:` value and data of every frame the buffer holds, in order.
    fn frames_in(frames: &Arc<Mutex<SseEventBuffer>>) -> Vec<(String, Value)> {
        let ReplayResult::Complete(frames) = frames.lock().unwrap().replay_from(0) else {
            panic!("a gap in a test buffer");
        };
        frames
            .iter()
            .map(|frame| {
                let event = frame
                    .lines()
                    .find_map(|line| line.strip_prefix("event: "))
                    .unwrap();
                let data = frame
                    .lines()
                    .find_map(|line| line.strip_prefix("data: "))
                    .unwrap();
                (event.to_string(), serde_json::from_str(data).unwrap())
            })
            .collect()
    }

    fn kinds(frames: &Arc<Mutex<SseEventBuffer>>) -> Vec<String> {
        frames_in(frames)
            .into_iter()
            .map(|(kind, _)| kind)
            .collect()
    }

    #[test]
    fn stream_events_without_a_tracker_a_tool_call_writes_no_frame() {
        let (target, frames) = target(false);
        tool_call_started(&target, "c1", "echo-tool");
        tool_call_input_bytes(&target, "c1", 12, Instant::now());
        assert!(kinds(&frames).is_empty());
        assert!(!target.chunks_emitted.load(Ordering::Relaxed));
    }

    #[test]
    fn stream_events_with_a_tracker_a_start_writes_one_tool_call_started_frame() {
        let (target, frames) = target(true);
        tool_call_started(&target, "c1", "echo-tool");
        tool_call_started(&target, "c1", "echo-tool");
        tool_call_started(&target, "", "echo-tool");
        assert_eq!(
            frames_in(&frames),
            [(
                "tool-call-started".to_string(),
                serde_json::json!({
                    "id": "tsk_test",
                    "tool_call_id": "c1",
                    "tool_name": "echo-tool",
                })
            )]
        );
        // A tool-call signal is not streamed text.
        assert!(!target.chunks_emitted.load(Ordering::Relaxed));
    }

    #[test]
    fn stream_events_progress_is_coalesced_and_the_rest_is_flushed_on_return() {
        let (target, frames) = target(true);
        let now = Instant::now();
        tool_call_input_bytes(&target, "ghost", 5, now);
        tool_call_started(&target, "c1", "echo-tool");
        tool_call_input_bytes(&target, "c1", 15, now);
        tool_call_input_bytes(&target, "c1", 30, now + Duration::from_millis(1));
        tool_call_input_bytes(&target, "c1", 10, now + Duration::from_millis(2));
        let settled = settle_started_calls(
            &target.sse,
            target.task_id.as_deref(),
            target.tool_calls.as_ref().unwrap(),
            now + Duration::from_millis(3),
        );
        assert_eq!(settled, [("c1".to_string(), "echo-tool".to_string())]);
        let progress: Vec<Value> = frames_in(&frames)
            .into_iter()
            .filter(|(kind, _)| kind == "tool-call-progress")
            .map(|(_, data)| data["input_bytes"].clone())
            .collect();
        assert_eq!(progress, [15, 30]);
        // Settled: a late report writes nothing.
        tool_call_input_bytes(&target, "c1", 90, now + Duration::from_secs(1));
        assert_eq!(
            kinds(&frames),
            [
                "tool-call-started",
                "tool-call-progress",
                "tool-call-progress"
            ]
        );
    }

    #[test]
    fn stream_events_without_sse_nothing_is_written_or_tracked() {
        let tool_calls = Arc::new(Mutex::new(ToolCallProgress::default()));
        let target = StreamEventsTarget {
            sse: None,
            task_id: None,
            chunks_emitted: Arc::new(AtomicBool::new(false)),
            tool_calls: Some(Arc::clone(&tool_calls)),
        };
        emit_chunk(&target, "hello");
        emit_thinking_chunk(&target, "hmm");
        tool_call_started(&target, "c1", "echo-tool");
        tool_call_input_bytes(&target, "c1", 12, Instant::now());
        assert!(settle_started_calls(&None, None, &tool_calls, Instant::now()).is_empty());
        // The start never reached the tracker, so the same id may still start in a task.
        assert!(tool_calls.lock().unwrap().start("c1", "echo-tool"));
    }

    #[test]
    fn stream_events_emit_chunk_without_a_tracker_still_writes_text() {
        let (target, frames) = target(false);
        emit_chunk(&target, "hello");
        emit_thinking_chunk(&target, "hmm");
        assert_eq!(
            frames_in(&frames),
            [
                (
                    "text".to_string(),
                    serde_json::json!({"id": "tsk_test", "text": "hello", "final": false})
                ),
                (
                    "thinking".to_string(),
                    serde_json::json!({"id": "tsk_test", "text": "hmm", "final": false})
                ),
            ]
        );
        assert!(target.chunks_emitted.load(Ordering::Relaxed));
    }
}
