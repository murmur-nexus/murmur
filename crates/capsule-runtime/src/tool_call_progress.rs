//! The tool calls a model is writing, from the driver's report that one has started to the
//! complete call, and the coalescing that decides which of their progress reports become frames.
//!
//! Both transports keep one [`ToolCallProgress`] and ask it what to write: the process transport
//! from its driver's `tool-call-started` and `tool-call-progress` events, the http transport from
//! its driver's `murmur:stream/events` calls. It writes nothing itself and knows nothing of either
//! transport, so the same reports produce the same frames on both.
//!
//! Progress is coalesced to at most one frame per call per [`TOOL_CALL_PROGRESS_INTERVAL`], plus
//! one when the complete call arrives if the largest count reported has not been written yet, so a
//! driver reporting every few bytes cannot crowd a task's earlier frames out of the replay buffer.

use std::{
    collections::HashSet,
    time::{Duration, Instant},
};

/// The shortest time between two `tool-call-progress` frames for one call. The frame the complete
/// call flushes is not held to it.
pub(crate) const TOOL_CALL_PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

/// A tool call the model is writing: started, its complete call not yet arrived.
struct StartedCall {
    /// The name its `tool-call-started` frame carried.
    name: String,
    /// The largest input size the driver has reported for it.
    reported: u64,
    /// The size its last `tool-call-progress` frame carried, and when that frame was written.
    sent: Option<(u64, Instant)>,
}

impl StartedCall {
    fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            reported: 0,
            sent: None,
        }
    }

    /// Take a report of `input_bytes` at `now`, and return the size to write a frame for, if one
    /// is due. A report no larger than an earlier one is ignored. The first is written at once; a
    /// later one only once [`TOOL_CALL_PROGRESS_INTERVAL`] has passed since the last frame, and
    /// until then it is held.
    fn report(&mut self, input_bytes: u64, now: Instant) -> Option<u64> {
        if input_bytes <= self.reported {
            return None;
        }
        self.reported = input_bytes;
        let due = self
            .sent
            .is_none_or(|(_, at)| now.saturating_duration_since(at) >= TOOL_CALL_PROGRESS_INTERVAL);
        if !due {
            return None;
        }
        self.sent = Some((input_bytes, now));
        Some(input_bytes)
    }

    /// The held size, if a report larger than the last frame is still unwritten, whatever the
    /// interval says: the call's input is complete and no later report will carry it.
    fn flush(&mut self, now: Instant) -> Option<u64> {
        let written = self.sent.map_or(0, |(size, _)| size);
        if self.reported <= written {
            return None;
        }
        self.sent = Some((self.reported, now));
        Some(self.reported)
    }
}

/// A started call whose complete call has arrived.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SettledCall {
    /// The name its start carried.
    pub(crate) name: String,
    /// The size still held for it, to be written as its last `tool-call-progress` frame.
    pub(crate) held: Option<u64>,
}

/// Every call one task attempt has started, being written or settled.
#[derive(Default)]
pub(crate) struct ToolCallProgress {
    /// The calls the model is writing, by id, in the order they started.
    writing: Vec<(String, StartedCall)>,
    /// The ids of calls whose complete call or whose result has arrived. Nothing more is written
    /// for them before their `artifact`, and a start arriving now is not believed.
    settled: HashSet<String>,
}

impl ToolCallProgress {
    /// The model has begun writing call `id`, named `name`. Returns whether to write its
    /// `tool-call-started` frame: a start with no id, or for a call already being written or
    /// settled, writes nothing and is not tracked.
    pub(crate) fn start(&mut self, id: &str, name: &str) -> bool {
        if id.is_empty() || self.writing(id).is_some() || self.settled.contains(id) {
            return false;
        }
        self.writing.push((id.to_string(), StartedCall::new(name)));
        true
    }

    /// The model has written `input_bytes` of call `id`'s input, as of `now`. Returns the size to
    /// write a `tool-call-progress` frame for, if one is due; never one for a call not being
    /// written.
    pub(crate) fn report(&mut self, id: &str, input_bytes: u64, now: Instant) -> Option<u64> {
        self.writing(id)?.report(input_bytes, now)
    }

    /// Call `id`'s complete call arrived at `now`. Settles it, whether or not it was started, and
    /// returns its start's name and the size still held for it, for a call that was started.
    pub(crate) fn complete(&mut self, id: &str, now: Instant) -> Option<SettledCall> {
        self.settled.insert(id.to_string());
        let index = self.writing.iter().position(|(started, _)| started == id)?;
        let (_, mut call) = self.writing.remove(index);
        Some(SettledCall {
            held: call.flush(now),
            name: call.name,
        })
    }

    /// [`Self::complete`] for every call still being written, in the order they started.
    pub(crate) fn complete_all(&mut self, now: Instant) -> Vec<(String, SettledCall)> {
        std::mem::take(&mut self.writing)
            .into_iter()
            .map(|(id, mut call)| {
                self.settled.insert(id.clone());
                let settled = SettledCall {
                    held: call.flush(now),
                    name: call.name,
                };
                (id, settled)
            })
            .collect()
    }

    /// Call `id`'s result arrived: it is settled, and anything still held for it is dropped.
    pub(crate) fn settle(&mut self, id: &str) {
        self.writing.retain(|(started, _)| started != id);
        self.settled.insert(id.to_string());
    }

    /// Forget every call still being written, leaving the settled ones settled.
    pub(crate) fn clear_writing(&mut self) {
        self.writing.clear();
    }

    /// Forget every call, settled or not, for a new task attempt.
    pub(crate) fn reset(&mut self) {
        self.writing.clear();
        self.settled.clear();
    }

    fn writing(&mut self, id: &str) -> Option<&mut StartedCall> {
        self.writing
            .iter_mut()
            .find_map(|(started, call)| (started == id).then_some(call))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ms` milliseconds after `base`.
    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn tool_call_progress_the_first_report_is_written_at_once() {
        let now = Instant::now();
        let mut call = StartedCall::new("write_file");
        assert_eq!(call.report(12, now), Some(12));
    }

    #[test]
    fn tool_call_progress_a_report_within_the_interval_is_held() {
        let now = Instant::now();
        let mut call = StartedCall::new("write_file");
        call.report(12, now);
        assert_eq!(call.report(40, at(now, 100)), None);
        assert_eq!(call.report(60, at(now, 249)), None);
        assert_eq!(call.reported, 60);
    }

    #[test]
    fn tool_call_progress_a_report_at_the_interval_writes_the_largest_size() {
        let now = Instant::now();
        let mut call = StartedCall::new("write_file");
        call.report(12, now);
        call.report(40, at(now, 100));
        assert_eq!(call.report(60, at(now, 250)), Some(60));
        // The interval runs from the last frame written, not from the last report.
        assert_eq!(call.report(80, at(now, 400)), None);
        assert_eq!(call.report(90, at(now, 500)), Some(90));
    }

    #[test]
    fn tool_call_progress_an_equal_or_lower_size_is_never_written() {
        let now = Instant::now();
        let mut call = StartedCall::new("write_file");
        assert_eq!(call.report(0, now), None);
        call.report(40, now);
        assert_eq!(call.report(40, at(now, 1_000)), None);
        assert_eq!(call.report(12, at(now, 2_000)), None);
        assert_eq!(call.flush(at(now, 3_000)), None);
    }

    #[test]
    fn tool_call_progress_the_complete_call_flushes_the_held_size_once_whatever_the_interval() {
        let now = Instant::now();
        let mut call = StartedCall::new("write_file");
        call.report(12, now);
        call.report(40, at(now, 1));
        assert_eq!(call.flush(at(now, 2)), Some(40));
        assert_eq!(call.flush(at(now, 3)), None);
    }

    #[test]
    fn tool_call_progress_the_complete_call_flushes_nothing_when_nothing_is_held() {
        let now = Instant::now();
        let mut call = StartedCall::new("write_file");
        assert_eq!(call.flush(now), None);
        call.report(12, now);
        assert_eq!(call.flush(at(now, 1)), None);
    }

    #[test]
    fn tool_call_progress_a_start_with_an_empty_id_is_not_tracked() {
        let mut calls = ToolCallProgress::default();
        assert!(!calls.start("", "write_file"));
        assert_eq!(calls.report("", 12, Instant::now()), None);
    }

    #[test]
    fn tool_call_progress_a_second_start_for_a_call_being_written_writes_nothing() {
        let mut calls = ToolCallProgress::default();
        assert!(calls.start("c1", "write_file"));
        assert!(!calls.start("c1", "write_file"));
        assert!(!calls.start("c1", "read_file"));
    }

    #[test]
    fn tool_call_progress_a_start_for_a_settled_call_writes_nothing() {
        let now = Instant::now();
        let mut calls = ToolCallProgress::default();
        assert_eq!(calls.complete("c1", now), None);
        assert!(!calls.start("c1", "write_file"));
        calls.settle("c2");
        assert!(!calls.start("c2", "write_file"));
    }

    #[test]
    fn tool_call_progress_a_report_for_a_call_not_being_written_writes_nothing() {
        let now = Instant::now();
        let mut calls = ToolCallProgress::default();
        assert_eq!(calls.report("ghost", 5, now), None);
        calls.start("c1", "write_file");
        calls.complete("c1", now);
        assert_eq!(calls.report("c1", 40, at(now, 1_000)), None);
    }

    #[test]
    fn tool_call_progress_reports_are_coalesced_per_call() {
        let now = Instant::now();
        let mut calls = ToolCallProgress::default();
        calls.start("c1", "write_file");
        calls.start("c2", "read_file");
        assert_eq!(calls.report("c1", 12, now), Some(12));
        assert_eq!(calls.report("c2", 3, at(now, 1)), Some(3));
        assert_eq!(calls.report("c1", 30, at(now, 2)), None);
        assert_eq!(calls.report("c1", 20, at(now, 300)), None);
        assert_eq!(calls.report("c1", 31, at(now, 300)), Some(31));
    }

    #[test]
    fn tool_call_progress_the_complete_call_returns_the_start_s_name_and_held_size() {
        let now = Instant::now();
        let mut calls = ToolCallProgress::default();
        calls.start("c1", "write_file");
        calls.report("c1", 12, now);
        calls.report("c1", 40, at(now, 1));
        assert_eq!(
            calls.complete("c1", at(now, 2)),
            Some(SettledCall {
                name: "write_file".to_string(),
                held: Some(40),
            })
        );
        assert_eq!(calls.complete("c1", at(now, 3)), None);
    }

    #[test]
    fn tool_call_progress_clearing_the_in_flight_set_keeps_settled_ids_settled() {
        let now = Instant::now();
        let mut calls = ToolCallProgress::default();
        calls.start("c1", "write_file");
        calls.complete("c1", now);
        calls.start("c2", "write_file");
        calls.clear_writing();
        assert!(!calls.start("c1", "write_file"));
        assert_eq!(calls.report("c2", 12, now), None);
        assert!(calls.start("c2", "write_file"));
    }

    #[test]
    fn tool_call_progress_the_flush_all_on_dispatch_return_flushes_every_in_flight_call_once() {
        let now = Instant::now();
        let mut calls = ToolCallProgress::default();
        calls.start("c1", "write_file");
        calls.start("c2", "read_file");
        calls.start("c3", "list_dir");
        calls.report("c1", 12, now);
        calls.report("c1", 40, at(now, 1));
        calls.report("c2", 7, now);
        let settled = calls.complete_all(at(now, 2));
        assert_eq!(
            settled,
            [
                (
                    "c1".to_string(),
                    SettledCall {
                        name: "write_file".to_string(),
                        held: Some(40),
                    }
                ),
                (
                    "c2".to_string(),
                    SettledCall {
                        name: "read_file".to_string(),
                        held: None,
                    }
                ),
                (
                    "c3".to_string(),
                    SettledCall {
                        name: "list_dir".to_string(),
                        held: None,
                    }
                ),
            ]
        );
        assert!(calls.complete_all(at(now, 3)).is_empty());
        assert!(!calls.start("c1", "write_file"));
        assert_eq!(calls.report("c2", 90, at(now, 1_000)), None);
    }

    #[test]
    fn tool_call_progress_a_reset_forgets_settled_calls() {
        let mut calls = ToolCallProgress::default();
        calls.settle("c1");
        calls.reset();
        assert!(calls.start("c1", "write_file"));
    }

    #[test]
    fn tool_call_progress_interval_is_the_documented_one() {
        let page = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/content/reference/streaming-protocol.md"
        ))
        .expect("the streaming protocol reference page");
        let section = page
            .split("\n## ")
            .find(|section| section.contains("{ #event-tool-call-progress }"))
            .expect("a tool-call-progress section");
        let stated = format!("{} ms", TOOL_CALL_PROGRESS_INTERVAL.as_millis());
        assert!(
            section.contains(&stated),
            "the tool-call-progress section does not state {stated}"
        );
    }
}
