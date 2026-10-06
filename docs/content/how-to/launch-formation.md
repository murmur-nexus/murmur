# How to launch a formation of capsules

A [formation](../concepts/formations.md) is a group of capsules on one machine that work on one
task together: one member receives the task and hands parts of it to the others. This guide
scaffolds a two-member formation, `lead` and `worker`, launches it on a task that `lead` hands to
`worker`, and follows the hand-off through the trace until the formation ends.

The relevant manifest options are:

| Option | Controls |
|---|---|
| [exports.peer_tasks.accept](../reference/manifest.md#field-exports-peer-tasks) | Whether the member serves tasks other members send it |
| [network.authentication](../reference/manifest.md#field-network-authentication) | Whether the member's door refuses callers without a token |
| [capabilities.network.allow](../reference/manifest.md#network-allow-entries) | Where the member may connect, other members' doors included |
| [lifecycle.task_acceptance](../reference/manifest.md#lifecycle-task-acceptance) | How many tasks the member takes |
| [lifecycle.after_task](../reference/manifest.md#lifecycle-after-task) | Whether the member exits or waits after a task |
| [inference.system_prompt](../reference/manifest.md#inference-system-prompt) | What each member is told it is for |

The output below is from one real run with OpenAI as the provider and `HOME` set to a scratch
directory, `/tmp/crew-run-6ad9/attempt1/home`. Your ids, ports, pids, timings and paths will differ.

---

## Step 1 — scaffold the formation

`mur new --roster` names the provider's driver, endpoint and model from `~/.murmur/config.yaml`.
With no `inference:` block it uses Anthropic. To use OpenAI, set the provider before you scaffold:

=== "Anthropic"

    Nothing to set.

=== "OpenAI"

    ```bash
    mur config set -g inference.provider openai
    ```

    ```text
    Set inference.provider in ~/.murmur/config.yaml
    ```

In an empty directory, scaffold a formation named `crew`:

```bash
mur new --roster crew
```

```text
Scaffolded formation 'crew' in ./crew
  crew/roster.yaml          lead is the entry member; lead may call worker
  crew/lead/murmur.yaml     capsule crew-lead@0.1.0
  crew/worker/murmur.yaml   capsule crew-worker@0.1.0, serves peers

Next:
  mur config set -g credentials.OPENAI_API_KEY <your key>
  mur install -g murmur-driver-openai@0.9.0
  mur build crew/lead && mur install -g crew/lead/crew-lead-0.1.0.mur.zip
  mur build crew/worker && mur install -g crew/worker/crew-worker-0.1.0.mur.zip
  mur run --roster crew --task "<your task>"
```

The scaffold writes three files and nothing else. Every key in them carries a comment and a link
to its reference entry. The `Next:` lines are the rest of this guide, in order. See
[`mur new --roster`](../reference/cli.md#mur-new-roster).

---

## Step 2 — read what the generated files declare

### `crew/roster.yaml`

The roster names the members, marks one as the entry member, and says who may call whom:

```yaml
members:
  - name: lead                      # its name in this roster, used by reachability
    capsule: crew-lead              # built from lead/murmur.yaml
    version: 0.1.0                  # the exact version installed
…
    entry: true
  - name: worker                    # its name in this roster, used by reachability
    capsule: crew-worker            # built from worker/murmur.yaml
    version: 0.1.0                  # the exact version installed
…
reachability:
  - from: lead                      # the calling member
    to: [worker]                    # the members lead may call
```

- **The entry member.** `lead` has `entry: true`, so it receives the formation's task, and its
  outcome is the formation's outcome. No rule may list it in `to`: the entry member is never
  called. See [The entry member](../reference/roster.md#entry-member).
- **The edge.** The one rule lets `lead` call `worker`. That edge is what gives `lead` the
  `call-member` tool, and what makes the launcher issue `lead` a credential for `worker`'s door.
  `worker` may call nobody. See [`reachability`](../reference/roster.md#reachability).

### `crew/worker/murmur.yaml`

`worker` consents to tasks from other members, and waits at its door between them:

```yaml
lifecycle:
  task_acceptance: queue            # take tasks one after another
  after_task: sleep                 # wait at the door for the next one
…
exports:
  peer_tasks:                       # tasks another member sends to this door
    accept: true                    # serve them; absent or false refuses them
…
network:
  authentication:                   # refuse every caller without a token this session minted
    scheme: bearer                  # Authorization: Bearer <token>, the only scheme
```

- **Consent.** A member a rule calls must declare `exports.peer_tasks.accept: true`, or admission
  refuses the roster with [`E-ROS-005`](../reference/diagnostics.md#e-ros-005).
- **Authentication.** A roster with any edge requires every member's door to require a token, or
  admission refuses it with [`E-ROS-006`](../reference/diagnostics.md#e-ros-006). The door then
  lets in only the credential the launcher issued for it. See
  [Authentication](../reference/roster.md#authentication).
- **Lifecycle.** `queue` and `sleep` keep `worker` serving for as long as the formation runs.

### `crew/lead/murmur.yaml`

`lead` is told to hand its task on, takes the formation's one task, and may reach other members'
doors:

```yaml
  system_prompt: "You are 'lead', the entry member of the formation 'crew'. You do not do the task yourself: worker does it. Your first reply is a call-member tool call that hands worker the task you are given, stated in full. Then end your turn while worker works. When worker's answer arrives in this conversation, answer with it."
…
lifecycle:
  task_acceptance: single           # one task: the formation's
  after_task: exit                  # exit when it ends, which stops the formation
…
capabilities:
  network:                          # IP destinations the capsule may reach
    allow:                          # anything not listed is denied
      - localhost                   # every member's door, at any port and scheme
…
network:
  authentication:                   # refuse every caller without a token this session minted
    scheme: bearer                  # Authorization: Bearer <token>, the only scheme
```

- **System prompt.** `lead` does not do the task itself: it hands the task to `worker` with
  `call-member`, ends its turn, and answers with what comes back.
- **Lifecycle.** `single` and `exit` make `lead` take the formation's task and exit when it ends.
  Its exit ends the formation.
- **Egress.** The roster grants `lead` a name and a credential, not network access. A member's
  door is served on loopback at a port chosen at launch, so `lead` allows the bare host
  `localhost`. Without it every call fails, and the launch prints
  [`W-RUN-008`](../reference/diagnostics.md#w-run-008). See
  [Giving a member work](../reference/roster.md#member-calls).
- **Network authentication.** `lead` declares it too: with any edge in the roster, every member
  must, whether or not it serves peers.

---

## Step 3 — store the key and install the members

Store the provider key in the global config. This comes first because, on a machine with no
`~/.murmur/config.yaml`, the config it writes carries the registry source the driver install needs:

=== "Anthropic"

    ```bash
    mur config set -g credentials.ANTHROPIC_API_KEY <your key>
    ```

=== "OpenAI"

    ```bash
    mur config set -g credentials.OPENAI_API_KEY <your key>
    ```

```text
Set credentials.OPENAI_API_KEY in ~/.murmur/config.yaml
```

Each member reads the key from [`credentials:`](../reference/config.md#credentials) at launch, and
neither prints [`W-SEC-027`](../reference/diagnostics.md#w-sec-027).

Install the driver both members declare:

=== "Anthropic"

    ```bash
    mur install -g murmur-driver-anthropic@{{ v.murmur_driver_anthropic }}
    ```

=== "OpenAI"

    ```bash
    mur install -g murmur-driver-openai@{{ v.murmur_driver_openai }}
    ```

```text
Installed murmur-driver-openai@0.9.0 from github:murmur-nexus/default-artifacts
```

--8<-- "includes/mur-pull-info.md"

Build each member and install it into the global store, where `mur run --roster` looks for it:

```bash
mur build crew/lead && mur install -g crew/lead/crew-lead-0.1.0.mur.zip
mur build crew/worker && mur install -g crew/worker/crew-worker-0.1.0.mur.zip
```

```text
Built artifact: crew/lead/crew-lead-0.1.0.mur.zip
Installed crew-lead@0.1.0 from crew/lead/crew-lead-0.1.0.mur.zip
Built artifact: crew/worker/crew-worker-0.1.0.mur.zip
Installed crew-worker@0.1.0 from crew/worker/crew-worker-0.1.0.mur.zip
```

Optionally, check the formation from the `crew` directory. With no `murmur.yaml` there,
[`mur doctor`](../reference/cli.md#doctor-roster) checks the roster against the installed members:

```bash
cd crew
mur doctor
cd ..
```

```text
No murmur.yaml in /tmp/crew-run-6ad9/attempt1/work/crew: checking its roster.yaml. Run mur doctor in a member's source directory to check that member's artifacts.

Roster
  file: /tmp/crew-run-6ad9/attempt1/work/crew/roster.yaml
  lead     crew-lead@0.1.0     entry   refuses peers   authenticated door
  worker   crew-worker@0.1.0           serves peers    authenticated door
  reachability: lead → worker

All checks passed.
```

---

## Step 4 — launch the formation

Launch the formation with the last `Next:` line, your task in place of `<your task>`:

```bash
mur run --roster crew --task "Write a four-line poem about a lighthouse keeper."
```

```text
…
formation: frm_01a10ef7dc157711be1b4edae2068392
  peer   worker  crew-worker@0.1.0  pid 2111819  ses_01a10ef7dc8f74f1b54f350429d2f28f  http://localhost:40023
                 workdir /tmp/crew-run-6ad9/attempt1/home/.murmur/formations/frm_01a10ef7dc157711be1b4edae2068392/worker
  entry  lead    starting
…
murmur: url localhost:45907
…
session: ses_01a10ef7dd337c33a0c23e71a86950b4
status:  ok
[worker] [capsule-runtime] formation lifeline closed — the formation has ended; cancelling live tasks and ending the session
```

The task is the plain task. `lead`'s system prompt already tells it to hand the task to `worker`.

---

## Step 5 — read the readiness lines

The launcher starts `worker` first, waits for its door, and only then starts `lead`:

| Line | Means |
|---|---|
| `formation: frm_…` | The formation id minted for this launch. Every member carries it, and its trace is addressed by it |
| `peer   worker …` | `worker` is ready: its process id, its session id, and the door that answered as that session |
| `workdir …` | `worker`'s own directory, under `~/.murmur/formations/<frm_id>/worker`. Its sessions are under it |
| `entry  lead    starting` | Every peer is ready, and `lead` is starting with the task |
| `murmur: url`, `session:` | `lead`'s own startup lines: its door and its session id. `lead` works in `crew`, and its sessions are under `crew/.murmur` |
| `status:  ok` | `lead`'s task ended `ok` |
| `[worker] … formation lifeline closed …` | `worker` saw the formation end and wound down |

`lead` also prints the operator token for its own door, because its door requires authentication.
A peer that does not come up within 180 seconds refuses the whole launch, and `lead` never starts.
See [Readiness](../reference/roster.md#launch-readiness) and
[Member directories](../reference/roster.md#member-directories).

---

## Step 6 — follow the hand-off from `lead` to `worker`

`lead`'s model calls `call-member` once, with `worker` and the task text. The call returns as soon
as `worker`'s door holds the task, and `lead`'s model ends its turn. `worker` runs the task in its
own directory and answers. `lead`'s same task then continues with one new message holding
`worker`'s answer, fenced as coming from `member:worker`, and `lead` answers with it.

In `lead`'s trace, `crew/.murmur/<lead session>/trace.jsonl`, the call and its outcome are two
records:

```json
{"event_type":"member_call_start","event_id":"evt_01a10ef7ec767c12bf1fa580cdd42074","parent_id":"evt_01a10ef7dd537631bcba4c8ff3646c06","session_id":"ses_01a10ef7dd337c33a0c23e71a86950b4","timestamp":1791252491382,"task_id":"tsk_01a10ef7dd5c73a3bbc8938fdbd4aed5","call_id":"mcl_01a10ef7ec687f62b54d2914c5d81e85","member":"worker","member_task_id":"tsk_01a10ef7ec6b7e008be4474317266baf"}
{"event_type":"member_call","event_id":"evt_01a10ef7f6d47703bb490e966f4659eb","parent_id":"evt_01a10ef7dd537631bcba4c8ff3646c06","session_id":"ses_01a10ef7dd337c33a0c23e71a86950b4","timestamp":1791252494036,"task_id":"tsk_01a10ef7dd5c73a3bbc8938fdbd4aed5","call_id":"mcl_01a10ef7ec687f62b54d2914c5d81e85","member":"worker","member_task_id":"tsk_01a10ef7ec6b7e008be4474317266baf","status":"completed","duration_ms":2525,"output":"He tends the lamp where restless black waves roll,  \nA steady star to sailors lost at sea.  \nThrough wind and night, he guards each drifting soul,  \nAnd keeps the dawn alive for you and me.","truncated":false,"delivered":true}
```

| Field | Here |
|---|---|
| `call_id` | `mcl_…`, the same on both records |
| `member_task_id` | `worker`'s task id for this call |
| `status` | `completed`: `worker` finished the task |
| `duration_ms` | From the call to `worker`'s answer |
| `output` | `worker`'s answer, as `lead`'s model received it |
| `delivered` | `true`: the answer reached `lead`'s task |

In `worker`'s trace, the task arrives named for the member that sent it:

```json
{"event_type":"a2a_task_received","event_id":"evt_01a10ef7ec6b7e008be44762f2430fd8","parent_id":"evt_01a10ef7dca87682be084f7dffd77c90","session_id":"ses_01a10ef7dc8f74f1b54f350429d2f28f","timestamp":1791252491371,"task_id":"tsk_01a10ef7ec6b7e008be4474317266baf","context_id":"ctx_01a10ef7ec6b7e008be4475e8e8d1800","message_id":"msg_mcl_01a10ef7ec687f62b54d2914c5d81e85","traceparent_from_caller":null,"caller_member":"lead"}
```

Its `task_id` is the call's `member_task_id`, and `caller_member` is `lead`. See
[`call-member`](../reference/runtime-provided-tools.md#call-member) and the
[`member_call`](../reference/observability-schemas.md#member-call) record.

`lead`'s answer, and so the formation's, is in its session's `out/result.txt`. From here on, work
in the `crew` directory:

```bash
cd crew
cat .murmur/ses_01a10ef7dd337c33a0c23e71a86950b4/out/result.txt
```

```text
He tends the lamp where restless black waves roll,  
A steady star to sailors lost at sea.  
Through wind and night, he guards each drifting soul,  
And keeps the dawn alive for you and me.
```

---

## Step 7 — find both members in the trace

From the `crew` directory, name the formation by its id. `mur trace show` searches `lead`'s
sessions and every member's directory under `~/.murmur/formations/`:

```bash
mur trace show frm_01a10ef7dc157711be1b4edae2068392
```

--8<-- "includes/mur-trace-show-info.md"

--8<-- "includes/mur-trace-explore.md"

```text
── Formation ────────────────────────────────────
formation:  frm_01a10ef7dc157711be1b4edae2068392
searched:   /tmp/crew-run-6ad9/attempt1/work/crew/workdir
searched:   /tmp/crew-run-6ad9/attempt1/work/crew/.murmur
searched:   /tmp/crew-run-6ad9/attempt1/home/.murmur/formations/frm_01a10ef7dc157711be1b4edae2068392/worker/.murmur
ses_01a10ef7dc8f74f1b54f350429d2f28f  crew-worker@0.1.0         ok
ses_01a10ef7dd337c33a0c23e71a86950b4  crew-lead@0.1.0           ok
```

Show `lead`'s session from its root, `.murmur`:

```bash
mur trace show ses_01a10ef7dd337c33a0c23e71a86950b4 --workdir .murmur
```

```text
── Session ──────────────────────────────────────
session:    ses_01a10ef7dd337c33a0c23e71a86950b4
formation:  frm_01a10ef7dc157711be1b4edae2068392
capsule:    crew-lead v0.1.0
model:      gpt-5.6-luna
status:     ok
duration:   7.9s
capabilities: network
tools:      call-member
…
── Tool calls ───────────────────────────────────
count:      1  (1 ok, 0 error)  success 100.0%
latency:    avg 15ms
  turn 0  call-member 15ms ✓  {"member":"worker","task":"Write a four-line poem about a lighthouse keeper. Return only the four poetic lines."}
  turn 1  end_turn
…
── Member calls ─────────────────────────────────
mcl_01a10ef7ec687f62b54d2914c5d81e85  worker  tsk_01a10ef7ec6b7e008be4474317266baf  completed in 2.5s
…
```

`tools: call-member` is there because the roster lets `lead` call `worker`. The **Member calls**
section lists each call with the member, its task id, and how it ended.

Show `worker`'s session from its own directory:

```bash
mur trace show ses_01a10ef7dc8f74f1b54f350429d2f28f --workdir ~/.murmur/formations/frm_01a10ef7dc157711be1b4edae2068392/worker/.murmur
```

```text
── Session ──────────────────────────────────────
session:    ses_01a10ef7dc8f74f1b54f350429d2f28f
formation:  frm_01a10ef7dc157711be1b4edae2068392
capsule:    crew-worker v0.1.0
model:      gpt-5.6-luna
status:     ok
duration:   8.2s
…
── Formation ended ──────────────────────────────
formation_ended  frm_01a10ef7dc157711be1b4edae2068392  the formation ended; this session wound down

── A2A ──────────────────────────────────────────
received:   1 task
sent:       0 messages
…
```

`worker` has no `tools:` line: the roster lets it call nobody, so it has no `call-member`.

---

## Step 8 — see how the formation ends

`lead` takes one task and exits when it ends. When `lead`'s process ends, the launcher closes every
other member's lifeline, and the formation is over:

1. `worker` appends `formation_ended` to its trace, before anything else its wind-down writes:

    ```json
    {"event_type":"formation_ended","event_id":"evt_01a10ef7fca77141b7c9e8cf78fc1833","parent_id":"evt_01a10ef7dca87682be084f7dffd77c90","session_id":"ses_01a10ef7dc8f74f1b54f350429d2f28f","timestamp":1791252495527,"formation_id":"frm_01a10ef7dc157711be1b4edae2068392"}
    ```

2. `worker` cancels anything still in flight, ends its session and exits.
3. The launcher exits with `lead`'s exit status: `0` here, because `lead`'s task ended `ok`.

Nothing of the formation is left running:

```bash
mur ps
```

```text
no running capsules
```

The formation lives for this one task. To run it again, launch it again with a new task: the new
launch is a new formation with a new id. `mur run --roster` refuses `--resume`, and a member's
session resumed by hand belongs to no formation. See
[How a formation ends](../reference/roster.md#launch-stop) and
[Resuming a member's session](../reference/roster.md#launch-resume).

---

## Summary

| Feature / setting | How it works |
|---|---|
| Scaffold | `mur new --roster crew` writes `roster.yaml` and one `murmur.yaml` per member, with the provider from `~/.murmur/config.yaml`, and prints the `Next:` steps |
| Entry member | `entry: true` in `roster.yaml`. It receives the task and is never called; when its task ends, the formation ends |
| Edge | A `reachability` rule. It gives the caller `call-member` and a credential for the callee's door |
| Consent | `exports.peer_tasks.accept: true` on every member a rule calls |
| Authentication | `network.authentication` on every member once the roster has any edge |
| Egress | The caller's `capabilities.network.allow` lists `localhost` |
| Key | `mur config set -g credentials.<KEY_VAR> <your key>`, first of the `Next:` steps |
| Launch | `mur run --roster crew --task "…"` starts the peers, waits for each door, then starts the entry member |
| Hand-off | `call-member` returns once the callee holds the task; the answer comes back into the caller's same task |
| Trace | `mur trace show frm_<id>` from the roster's directory lists every member's session; `member_call` in the caller's trace, `a2a_task_received` with `caller_member` in the callee's |
| End | The entry member exits after its task; every other member records `formation_ended` and exits; the launcher exits with the entry member's status |
