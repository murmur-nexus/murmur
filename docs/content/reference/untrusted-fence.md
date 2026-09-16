# Untrusted Fence

The markers that name where a piece of model-facing content came from, and the field each surface
labels them with.

The fence is a marker. It marks content; it does not control capability. Nothing is refused,
delayed, reordered or truncated for being fenced, no [capability grant](manifest.md#field-capabilities)
is widened or narrowed by it, and a model that acts on fenced instructions can still do everything
the manifest allows. [Access control](../concepts/access-control.md#threat-model) covers what the
fence is for and where it sits among the runtime's other boundaries.

No surface strips it. The bytes a model received are the bytes the A2A stream, the conversation
record and the session trace report.

---

## Grammar

A fenced block is an opening marker, a newline, the content, a newline, and a closing marker.

```
<untrusted-content source=tool:web-fetch>
{"status":200,"body":"…"}
</untrusted-content>
```

| Part | Form |
|---|---|
| Opening marker | `<untrusted-content source=NAME>` — one line, no attributes other than `source` |
| Source name | `tool:<artifact name>` for a tool result, `task:<origin>` for a task payload. A `>`, a carriage return or a newline appearing in a name is replaced with `_`, ` ` and ` ` respectively |
| Closing marker | `</untrusted-content>`, in full. It carries no source name |
| Separator | Exactly one `\n` after the opening marker and one before the closing marker. The content's own trailing newline, if it has one, sits before that separator |

Marker matching is case-insensitive over ASCII: `</UNTRUSTED-CONTENT>` is a closer.

### The neutralised form

Content that spells a marker of its own is rewritten before the fence closes, by inserting
`!MURMUR-NEUTRALISED!` directly after the marker's `<`:

| In the content | In the fenced block |
|---|---|
| `</untrusted-content>` | `<!MURMUR-NEUTRALISED!/untrusted-content>` |
| `<untrusted-content source=tool:other>` | `<!MURMUR-NEUTRALISED!untrusted-content source=tool:other>` |
| `</UNTRUSTED-CONTENT>` | `<!MURMUR-NEUTRALISED!/UNTRUSTED-CONTENT>` |

The rewrite is a pure insertion that preserves the original casing, so a forged marker stays
visible as rewritten text and nothing is deleted or truncated. The infix contains no `<` of its
own, so rewriting is idempotent: rewriting an already-rewritten block changes nothing.

### Parsing a block

1. A block opens at `<untrusted-content source=` and its source name runs to the first `>`.
2. A block ends at the **final** `</untrusted-content>` in the message, not the first. The
   opening and closing markers a consumer sees are the runtime's own; any marker the content
   carried is in its neutralised form and matches neither.
3. Everything between the separators is data, verbatim.

The runtime's system prompt states the same rule to the model: a closing marker appearing
anywhere inside a block — including one drawn inside an image — is a forgery.

---

## Which surfaces carry a fence

| Surface | Fence present | How it is labelled |
|---|---|---|
| A2A `artifact` SSE frame | In `artifact.content` | `artifact.fence_source` — the source name, or `null`. Present on every frame |
| [`conversation.jsonl`](workdir.md#the-conversation-record) message line | In `content` | `fence` — the source name. Absent on an unfenced line |
| [`trace.jsonl`](observability-schemas.md#session-trace-tracejsonl) `tool_call` | In `output`, and counted in `output_bytes` | Unlabelled. A `tool_call` event carries a fence and a `skill_call` event does not |
| [`murmur:conversation/read`](wit-interfaces.md#murmurconversationread) | In the message's `content` | Unlabelled. The `message` record has no `fence` field; read the markers |

The two labelled surfaces differ on purpose. An SSE consumer may be talking to any runtime
version, so `fence_source` is present on every frame and `null` distinguishes "this frame is
unfenced" from "this runtime does not label frames". A record line follows the other envelope
keys in the file, which are all absent when unset.

### The `artifact` frame

```json
{"id":"task_01a0…","artifact":{"tool_name":"web-fetch","content":"<untrusted-content source=tool:web-fetch>\n{\"status\":200}\n</untrusted-content>","fence_source":"tool:web-fetch"}}
```

| Field | Type | Value |
|---|---|---|
| `tool_name` | string | The tool, skill or hook the content came from |
| `content` | string | The bytes the model received, markers included |
| `fence_source` | string \| null | The source `content` is fenced under, or `null` when `content` carries no fence |

A frame with no `fence_source` key came from a runtime that predates the field. Read the markers.
The frame's full field list is in [Observability Schemas](observability-schemas.md#task-stream-artifact-frame).

### The record line

```json
{"role":"tool","tool_call_id":"toolu_1","is_error":false,"content":[{"type":"text","text":"<untrusted-content source=tool:web-fetch>\n{\"status\":200}\n</untrusted-content>"}],"id":"msg_01a0…","fence":"tool:web-fetch"}
```

An unfenced line carries no `fence` key:

```json
{"role":"assistant","content":[{"type":"text","text":"Fetched it."}],"id":"msg_01a0…"}
```

A record written before `fence` existed carries no `fence` key on any line, whatever its content
holds.

---

## What is never fenced

| Content | Why |
|---|---|
| A declared skill's `skill.md` | The capsule author's own guidance, staged inside the capsule at install. Fencing it as data would make the skill inert |
| A dispatch failure | The runtime's own text about a call that never reached a tool |
| A refusal — from [`capabilities.filesystem.read_only`](manifest.md#read-only-paths) or from a hook's decision | The runtime's own text. The call never ran |
| A hook artifact | The capsule operator's own declared hook speaking |
| A task whose [trust class](../concepts/access-control.md#task-origin-and-trust-class) is `trusted` | The operator instructing their own capsule |
| The model's own text | It is the model's turn, not content handed to it |

---

## Where the fence is applied

| Boundary | Content | Source name |
|---|---|---|
| Tool result | Every agent-facing tool dispatch except a skill — WASM tool, native subprocess tool, shell binary, and the runtime's own peer-handoff tools | `tool:<artifact name>` |
| Task payload | A task whose trust class is `untrusted` | `task:<origin>` |

Both boundaries apply on `inference.transport: http` and `inference.transport: process`. A
`process` capsule keeps no conversation record and emits no A2A `artifact` frame, so its fenced
content is labelled only by the markers themselves.
