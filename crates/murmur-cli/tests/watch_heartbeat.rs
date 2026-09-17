//! Integration tests for the `stream/watch` liveness signal against a real launched capsule.
//!
//! These read raw bytes rather than parsed events: the heartbeat is an SSE comment, and the
//! SSE clients elsewhere in the tree (and `mur watch` itself) discard comment lines by design,
//! so they cannot see what these tests measure.

#[path = "common/mod.rs"]
mod common;

use std::time::{Duration, Instant};

use common::idle_capsule::{
    end_turn_server, launch_idle_capsule, open_watch, read_lines_until, sse_body, submit_task,
    wait_for_task_ends,
};

/// The interval `handle_stream_watch` writes `:heartbeat` at. Mirrored from
/// `capsule_runtime::streaming::SSE_HEARTBEAT_INTERVAL`, which is crate-private.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

/// When each `:heartbeat` line arrived, measured from `since`.
fn heartbeat_arrivals(body: &[(Instant, String)], since: Instant) -> Vec<Duration> {
    body.iter()
        .filter(|(_, line)| line == ":heartbeat")
        .map(|(at, _)| at.duration_since(since))
        .collect()
}

// ── tests ──────────────────────────────────────────────────────────────────────

/// An observer on an idle capsule can tell the connection is alive by reading bytes.
///
/// One heartbeat is the expected count in a 25 s window: the interval is 15 s and the first
/// tick is consumed immediately. Two is scheduling slack; three would mean a burst of missed
/// ticks.
#[test]
fn watch_idle_observer_receives_heartbeat() {
    let capsule = launch_idle_capsule(end_turn_server(&["idle capsule reply"]));

    let started = Instant::now();
    let stream = open_watch(&capsule.url, 0);
    let lines = read_lines_until(&stream, started + Duration::from_secs(25));
    let body = sse_body(&lines);

    assert!(
        body.iter().any(|(_, line)| line == "event: connection-ack"),
        "stream/watch should open with a connection-ack; body was: {:?}",
        body.iter().map(|(_, l)| l).collect::<Vec<_>>()
    );

    let arrivals = heartbeat_arrivals(&body, started);
    assert!(
        arrivals.iter().any(|d| *d <= Duration::from_secs(20)),
        "expected a :heartbeat within 20 s of connecting; arrivals were {arrivals:?}"
    );
    assert!(
        arrivals.len() <= 2,
        "expected at most 2 heartbeats in a 25 s window at a {HEARTBEAT_INTERVAL:?} interval, \
         got {}: {arrivals:?}",
        arrivals.len()
    );
}

/// The heartbeat is a comment, not a frame: it consumes no event id and never enters the
/// replay buffer.
///
/// Three replays of the same capsule state from the same `Last-Event-ID` — one too short to
/// see a heartbeat, one held across one, one opened afterwards — must return the identical
/// `id:` sequence and the identical body. The third is what catches a heartbeat that reached
/// the replay buffer; the second is what catches one that took an id or arrived as a frame.
#[test]
fn watch_heartbeat_does_not_perturb_event_ids() {
    let capsule = launch_idle_capsule(end_turn_server(&["replayable reply"]));

    // Put real events in the replay buffer before observing.
    submit_task(&capsule.url, "heartbeat-ids-1", "produce some events");
    wait_for_task_ends(&capsule.workdir, 1);

    // Before: too short a connection to have seen a heartbeat.
    let before_conn = open_watch(&capsule.url, 0);
    let before_body = sse_body(&read_lines_until(
        &before_conn,
        Instant::now() + Duration::from_secs(3),
    ));
    drop(before_conn);

    let before_ids = event_ids(&before_body);
    assert!(
        !before_ids.is_empty(),
        "expected the replay to carry event ids; body was: {:?}",
        text(&before_body)
    );
    assert_eq!(
        heartbeat_count(&before_body),
        0,
        "a 3 s window should be far too short for a {HEARTBEAT_INTERVAL:?} heartbeat"
    );

    // Across: a connection held past one heartbeat interval.
    let started = Instant::now();
    let across_conn = open_watch(&capsule.url, 0);
    let across_body = sse_body(&read_lines_until(
        &across_conn,
        started + HEARTBEAT_INTERVAL + Duration::from_secs(5),
    ));
    drop(across_conn);

    assert!(
        heartbeat_count(&across_body) > 0,
        "expected at least one heartbeat while holding the connection open; body was: {:?}",
        text(&across_body)
    );
    // Every heartbeat stands alone between blank lines — no id:, no event: of its own.
    for block in blocks(&across_body) {
        if block.iter().any(|line| line.starts_with(':')) {
            assert_eq!(
                block,
                vec![":heartbeat".to_string()],
                "a heartbeat shared a frame with other lines: {block:?}"
            );
        }
    }
    assert_eq!(
        event_ids(&across_body),
        before_ids,
        "a heartbeat added or consumed an event id on the connection carrying it"
    );

    // After: the replay a later observer gets must be untouched by the heartbeat.
    let after_conn = open_watch(&capsule.url, 0);
    let after_body = sse_body(&read_lines_until(
        &after_conn,
        Instant::now() + Duration::from_secs(3),
    ));
    drop(after_conn);

    assert_eq!(
        heartbeat_count(&after_body),
        0,
        "a heartbeat was replayed, so it entered the replay buffer; body was: {:?}",
        text(&after_body)
    );
    assert_eq!(
        event_ids(&after_body),
        before_ids,
        "the id sequence replayed from Last-Event-ID: 0 changed once a heartbeat had been written"
    );
    assert_eq!(
        text(&after_body),
        text(&before_body),
        "the replayed body changed once a heartbeat had been written"
    );
}

fn text(body: &[(Instant, String)]) -> Vec<String> {
    body.iter().map(|(_, line)| line.clone()).collect()
}

fn heartbeat_count(body: &[(Instant, String)]) -> usize {
    body.iter().filter(|(_, line)| line == ":heartbeat").count()
}

/// Split SSE body lines into blank-line-delimited frames.
fn blocks(body: &[(Instant, String)]) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    let mut current: Vec<String> = Vec::new();
    for (_, line) in body {
        if line.is_empty() {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
        } else {
            current.push(line.clone());
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn event_ids(body: &[(Instant, String)]) -> Vec<u64> {
    body.iter()
        .filter_map(|(_, line)| line.strip_prefix("id: "))
        .filter_map(|rest| rest.trim().parse::<u64>().ok())
        .collect()
}
