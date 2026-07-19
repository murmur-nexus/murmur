# How to read your agent's trace and compare two runs

Every Murmur agent session produces a `trace.jsonl` file that captures the complete record of what happened: every inference call, tool invocation, shell command, and compaction event. This guide shows how to read a single session's trace, spot problems, and compare two runs side by side.

The commands in this guide are read-only — they do not modify any file and do not require a running capsule.

---

## Where trace.jsonl lives

After `mur run -v`, the session workdir is printed to stdout:

```text
murmur: url localhost:52222
session: ses_6801f81dd28b4a9daf434e8324c4793e
workdir: /path/to/workdir/ses_6801f81dd28b4a9daf434e8324c4793e
manifest: my-agent v0.1.0
driver: murmur-driver-anthropic (claude-sonnet-4-6)
```

The trace lives at `<workdir>/trace.jsonl`. It is always written — even if the session failed — and cannot be suppressed by the capsule.

---

## Step 1 — print a session summary

With no argument, `mur trace show` picks the most recent session automatically:

```bash
mur trace show
```

--8<-- "includes/mur-trace-show-info.md"

--8<-- "includes/mur-trace-explore.md"

Output:

```text
── Session ──────────────────────────────────────
session:    ses_6801f81dd28b4a9daf434e8324c4793e
capsule:    my-agent v0.1.0
model:      claude-sonnet-4-6
status:     ok
duration:   3.2s

── Turns ────────────────────────────────────────
count:      4  (max: 10)

── Tokens ───────────────────────────────────────
input:      5,840  (avg 1460/turn)
output:     612  (avg 153/turn)
total:      6,452

── Tool calls ───────────────────────────────────
count:      3  (3 ok, 0 error)  success 100.0%
latency:    avg 28ms
  turn 0  bash 22ms ✓  {"command":"cargo build --release"}
  turn 1  bash 41ms ✓  {"command":"cargo test --workspace"}
  turn 2  bash 21ms ✓  {"command":"cargo clippy"}

── Shell calls ──────────────────────────────────
count:      2
exit codes: 2 ok
latency:    avg 18ms

── Compaction ───────────────────────────────────
fired:      no
```

Each section maps to a class of events in `trace.jsonl`. The turn count and token totals give you the most immediate signal — if a session is taking more turns than expected, the turn count shows it immediately. Under **Tool calls**, each turn row shows the tool that ran, its duration, a `✓`/`✗` status icon, and — when the trace recorded one — the tool's input as compact JSON (truncated to 120 characters).

---

## Step 2 — spot problems in the output

**Too many turns** — if count is close to the maximum (set in `murmur.yaml`, default 10 turns), the agent hit the turn cap. Use `mur trace steps --verbose` to see what it called each turn and what each tool received:

```bash
mur trace steps --verbose
```

```text
Session ses_6801f81dd28b4a9daf434e8324c4793e  (4 turns)

  0  tool_call    bash        820ms   "cargo build --release"
  1  tool_call    bash        1.2s    "cargo test --workspace"
  2  tool_call    bash        340ms   "cargo clippy"
  3  end_turn     —           —
```

Repeated calls to the same tool with similar inputs is the most common sign of a retry loop.

**Tool errors** — if the tool calls line shows errors, the model called a tool that returned a failure. Use `mur trace show` to see which tool failed and what input triggered it.

**Latency spikes** — `avg tool latency` and `avg shell latency` are per-call averages. `mur trace show` lists every tool call with its individual duration in the Tool calls section, so you can identify the slow call without inspecting the raw file.

**Shell failures** — when the agent runs shell commands, `mur trace show` tracks the exit code for each one:

```text
── Shell calls ──────────────────────────────────
count:      3
exit codes: 2 ok, 1 failed (exit 1)
```

Non-zero exits are passed back to the model as output, not failures — but if the same exit code repeats across turns, the model is retrying a command that keeps failing.

---

## Step 3 — compare two runs with diff

When you change a manifest, system prompt, or model and want to know if the change helped, use `mur trace diff`. With no arguments it compares the two most recent sessions automatically:

```bash
mur trace diff
```

Output:

```text
Metric                 Run A            Run B            Delta
────────────────────── ──────────────── ──────────────── ──────────────────────────
turns                  4                2                -2 (B better)
duration               3.2s             1.4s             -1.8s (B better)
input tokens           5,840            2,910            -2930 (B better)
output tokens          612              298              -314 (B better)
input/turn (avg)       1460             1455             -5.0 (B better)
output/turn (avg)      153              149              -4.0 (B better)
tool calls             3                1                -2 (B better)
tool success rate      100.0%           100.0%           =
avg tool latency       28ms             31ms             +3ms (A better)
shell calls            2                1                -1 (B better)
avg shell latency      18ms             22ms             +4ms (A better)
compaction             none             none             —
exit status            ok               ok               —
```

Lower-is-better metrics (turns, tokens, latency) flag the lower run as better. `tool success rate` is higher-is-better. Non-comparable fields like `exit status` show `—` in the Delta column.

This is the fastest way to confirm that a prompt or model change actually reduced turn count or token usage.

---

## Step 4 — aggregate across sessions

After multiple runs, `mur trace report` automatically picks up every session in the workdir and aggregates them into a single report:

```bash
mur trace report
```

Output:

```text
Sessions: 5  (./workdir/)

Metric                 Mean           StdDev         Min            Max
────────────────────── ────────────── ────────────── ────────────── ──────────────
turns                  3.2            1.1            2.0            5.0
duration (ms)          2,640          880            1,400          4,100
input tokens           4,720          1,380          2,910          6,850
output tokens          498            142            298            712
tool calls             2.4            0.9            1.0            4.0
tool success (%)       96.0           8.9            80.0           100.0
shell calls            1.8            0.8            1.0            3.0

Exit status:
  ok                       4  (80.0%)
  max_turns_reached        1  (20.0%)
```

The exit status distribution immediately shows how often the agent hit the turn cap. `tool success (%)` stddev above 10–15 points usually means the agent is making inconsistent tool choices across runs.

---

## Step 5 — read the raw events

`trace.jsonl` is plain JSONL — each line is one JSON object. You can inspect it directly for detailed debugging:

```bash
# All events in order
cat workdir/ses_.../trace.jsonl | jq .

# Only shell events
grep '"event_type":"shell"' workdir/ses_.../trace.jsonl | jq .

# Session summary fields
grep '"event_type":"session_end"' workdir/ses_.../trace.jsonl | jq '{turns: .total_turns, tokens: (.total_input_tokens + .total_output_tokens), status: .exit_status}'
```

For persistent capsules (queue mode), each task produces its own `task_start` / `session_start` / `session_end` / `task_end` block inside a single `trace.jsonl`. `mur trace show` displays a per-task breakdown in the **Tasks** section.

To visualise the same trace data as Grafana spans, see [Work with capsule trace spans in Grafana](grafana-tempo-spans.md).

---

## Summary

| Command | Use it for |
|---|---|
| `mur trace show [session]` | Quick health check of a single session: turns, tokens, tool errors, latency |
| `mur trace steps [session]` | Turn-by-turn breakdown of what the agent did, with per-call latency |
| `mur trace diff [a] [b]` | Confirm a change improved (or didn't regress) key metrics; defaults to last two sessions |
| `mur trace report` | Spot high variance across runs, find outliers, check exit status distribution |
| Raw `grep` + `jq` | Deep inspection of specific events — which tool calls failed, what commands ran, when compaction fired |
