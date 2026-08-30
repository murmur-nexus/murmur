# Access Control

A capsule's `capabilities:` block is where access control is declared — what the capsule and
its artifacts may reach.

Every grant is declared: filesystem and network access start closed, and shell access is
allowlist-based. The same applies to hooks — a hook's grant comes from its entry in the
capsule's manifest and starts empty.

## Per-tool narrowing

By default every WASM tool, and the inference driver, runs on the
capsule-wide `capabilities:` ceiling: the same allow-list and the same preopened workdir. A
`runtime: tool` or `runtime: driver` entry in the capsule's own `murmur.yaml` may optionally
declare its own `capabilities:` block to run *below* that ceiling — a narrower host list, a
subdirectory instead of the whole workdir:

```yaml
# in the capsule's own murmur.yaml
artifacts:
  - name: murmur-tool-fetch
    version: 1.0.0
    runtime: tool
    capabilities:
      network:
        allow: [https://api.example.com]
      filesystem:
        scope: cache
```

This uses the same key and vocabulary as hooks, with the opposite starting point: a hook with
no block gets nothing, a tool with no block keeps the full ceiling. A per-artifact block can
only subtract — an entry naming a host the capsule itself may not reach is dropped, with a
[`W-SEC-007`](../reference/diagnostics.md#w-sec-007) warning. Like a hook's, the grant is
read only from the capsule operator's entry, never from the tool's own bundled manifest. The
artifact named by `inference.driver.artifact` narrows the same way. See [Tool and driver
capabilities](../reference/manifest.md#tool-capabilities) for the full rules.

## Hook capabilities { #hook-capabilities }

A hook runs default-deny, and only the capsule operator can widen it. A hook artifact's own
`murmur.yaml` declares the hook's behavioral contract and nothing else. Capability grants are
read exclusively from the artifact entry in the **capsule's** `murmur.yaml`, so a hook pulled
from a registry can never grant itself anything:

```yaml
# in the capsule's own murmur.yaml
artifacts:
  - name: murmur-hook-telemetry
    version: 1.0.0
    runtime: hook
    capabilities:
      network:
        allow: [https://telemetry.example.com]
      filesystem:
        scope: hook-state
```

With no `capabilities:` block a hook has no network, no directory, and no sight of the task at
all — every outbound request is denied, it cannot read or write any file, and asking for the
task's text or the agent's result returns `not-granted`. A granted hook reaches its declared
hosts through the same allow-list gate a capsule's or tool's outbound HTTP goes through, sees
exactly one directory, `<workdir>/<scope>`, as its current directory, and with
`task_io.read: true` can read the task text and the result the agent produced. Every hook gets
its grant the same way, whatever its binding or execution mode. See
[Hook capabilities](../reference/manifest.md#hook-capabilities) for the full rules.

## Untrusted publisher text

Tool and skill descriptions rendered into [`MURMUR.md`](session-loop.md#murmurmd) are
publisher-controlled, untrusted text: the runtime
sanitizes them (collapsing newlines, stripping control characters, truncating length) before
rendering, and `MURMUR.md` states explicitly that this file is machine-generated inventory
data, never instructions.

## Task origin and trust class

Every task the runtime runs carries an **origin** — why it woke the capsule — set by whoever
enqueued it, and a **trust class** derived from that origin. A sender never declares its own
class.

| Origin | Set by | Trust |
|---|---|---|
| `user` | A person handing the capsule an instruction — a local `mur run`, a `task.md` | `trusted` |
| `schedule` | A timer | `trusted` |
| `system` | The runtime enqueuing work for itself, with no person in the loop | `trusted` |
| `event` | A webhook, a chat message, a PR comment — third-party text | `untrusted` |
| `peer` | A message from another capsule | The sending capsule's own class |
| `completion` | A sub-capsule or detached shell reporting that its work finished | The sending capsule's own class |

`peer` and `completion` inherit, so untrust cannot launder itself at the first hop: an untrusted
webhook payload that reaches capsule A and is forwarded to capsule B arrives at B still
untrusted. A message that carries no class at all is `untrusted`.

The origin travels between capsules as two request headers, stamped by the sending runtime:

| Header | Values |
|---|---|
| `x-murmur-task-origin` | `peer` or `completion` |
| `x-murmur-task-trust` | `trusted` or `untrusted` |

`murmur:message/send` carries no origin or trust field, so a capsule author has nothing to assert
a class with. Only `peer` and `completion` are accepted from the wire; the other four origins are
enqueued locally and never legitimately arrive over HTTP. An inbound `x-murmur-task-origin`
naming one of them, naming anything unrecognised, or absent entirely yields `event` /
`untrusted`. This is not authentication: a caller that claims `peer` + `trusted` gets what a
genuine trusted peer gets, and nothing on the A2A path tells the two apart. What it closes is
untrust laundering across an honest chain.

The class is recorded, not enforced: no task is refused, delayed or reordered because it is
`untrusted`. An `untrusted` payload reaches the model inside the
[untrusted fence](#threat-model), marked as data. Treat the class as the answer to "why did this
run", and keep authoring manifests on the assumption that any task's text may be hostile. The
origin does more than the class: it also picks the [queue lane](session-loop.md#queue-lanes) a
task waits in, so two tasks that are both `untrusted` can still run in a different order.

Both values are recorded on the [`task_start`](../reference/observability-schemas.md#session-trace-tracejsonl)
trace event and shown on the task row of `mur trace steps <session>`, alongside the lane:

```
task tsk_0a1b2c3d…  ctx_3c4d5e6f…  (a2a, peer/untrusted, lane peer)
task tsk_0a1b2c3d…  ctx_3c4d5e6f…  (task_md, user/trusted, lane user)
```

## Prompt injection and network-bypass posture { #threat-model }

Two gaps shape how you should author manifests, not just what the runtime enforces. The first
has no structural fix on any platform; the second is closed on Linux where kernel enforcement is
available, and permanently open elsewhere.

- **Prompt injection.** No manifest setting or runtime mechanism fully prevents a model
  from being manipulated by instructions smuggled inside data it reads (a fetched web page, a
  tool result, output from a binary declared under `capabilities.shell.allow`). The runtime's
  mitigation is a marker — the untrusted fence — applied at two boundaries and stated in the
  system prompt on both `transport: http` and `transport: process`.

    | Content reaching the model | Fenced |
    |---|---|
    | A tool result — from a WASM tool, a native subprocess tool, a shell binary, or one of the runtime's own peer-handoff tools | Yes |
    | A task whose [trust class](#task-origin-and-trust-class) is `untrusted` | Yes |
    | A declared skill's `skill.md` | No. It is the capsule author's own guidance, staged inside the capsule at install; fencing it as data would make the skill inert |
    | A `user` or `schedule` task | No. It is the operator instructing their own capsule, for the same reason |

    Fenced content arrives between `<untrusted-content source=NAME>` and `</untrusted-content>`,
    where NAME is `tool:<artifact name>` for a tool result and `task:<origin>` for a task payload.
    A marker found inside the content itself is rewritten to
    `<!MURMUR-NEUTRALISED!/untrusted-content>` before the fence closes, so content cannot end its
    own block; the rewrite inserts and deletes nothing, so an operator reading the trace sees the
    forged marker as rewritten text. The system prompt tells the model that everything between the
    markers is data and that a closing marker appearing anywhere inside a block — including one
    drawn inside an image — is a forgery.

    The fence marks content; it does not control capability. It sits inside the boundary
    `capabilities:` draws and replaces no part of it — nothing is refused or delayed for being
    fenced, and a model that acts on injected instructions can still do everything the manifest
    allows. Manifest authors
    are still responsible for not combining broad tool authority with exposure to untrusted
    content (see the phase-separation pattern below).

- **Network allowlist bypass via subprocess.** `capabilities.network.allow` constrains
  requests the *runtime itself* makes (HTTP calls from tool and driver components); by itself it
  does **not** constrain a subprocess spawned via `capabilities.shell.allow`. Closing that gap
  needs kernel-level subprocess enforcement, which only exists on Linux. The runtime warns at
  capsule launch whenever this gap is live on the current host — see
  [Subprocess enforcement tiers](../reference/containment.md#subprocess-enforcement-tiers) for the
  breakdown and [`W-SEC-001`](../reference/diagnostics.md#w-sec-001) through
  [`W-SEC-003`](../reference/diagnostics.md#w-sec-003) for each warning code.

**Maximum-injection-risk combination:** `capabilities.shell.allow` containing `bash` *combined
with* any external-fetch capability — either a declared `capabilities.network.allow`, or a
tool/driver artifact that performs its own outbound fetches independent of `network.allow`. This
pairing gives a capsule both the ability to ingest attacker-influenced content from the network
and the shell authority to act on it unchecked: injected instructions with a bypassable
enforcement boundary underneath them.

### Recommended pattern: data/action phase separation

For any capsule that ingests untrusted external content (fetched web pages, third-party file
contents, output from a tool the operator didn't author), split the capsule's work into two
phases instead of giving one phase both fetch and shell/write authority at once:

1. **Data-gathering phase** — only fetch/read tools are active (network calls, file reads). No
   `bash`/shell/write capability is available during this phase.
2. **Action phase** — the fetched content is summarized or extracted into a bounded structure
   (a short JSON object, a fixed set of fields) *before* it is handed to a phase that has
   bash/shell/write authority. The action phase never receives the raw untrusted bytes directly —
   only the bounded, already-extracted structure.

The point of the split is that raw untrusted content and tool-calling/shell authority should never
be present in the same context window at the same time. Summarizing/extracting first means an
injected instruction inside the fetched content has nothing to act through by the time a
shell-capable phase is reading it.
