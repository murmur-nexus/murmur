# Control Surface

A capsule whose manifest declares [`control:`](manifest.md#field-control) can be changed while it
runs by whoever holds its session's control token. It serves a control surface under `/control` on
the same listener as its A2A door: plain HTTP with JSON bodies, separate from the JSON-RPC methods
on `POST /`, and absent from the [agent card](agent-card.md).

| What a controller changes | Declared by | Takes effect | Command |
|---|---|---|---|
| A setting | `control.settings` | From the agent loop's next inference call | `mur control set` |
| A secret | `control.secrets` | On the credential gateway's next keyed request | `mur control secret`, `mur control forget` |

A capsule with no `control:` block — or with `control: {}`, or both lists empty — has no control
surface. It mints no token, and every request under `/control` is answered `404`.

The agent is never a controller. No manifest key hands the token to a tool, hook, driver or shell
command. Where the session's filesystem restriction is `advisory`, a shell command or native tool
can read any file you can, the [token file](#token) included: on such a host, do not grant
`capabilities.shell` to a capsule whose controls the agent must not reach. `mur run --explain-scope
--json` reports the restriction as
[`filesystem_boundary.restriction`](containment.md#testing-containment).

---

## Controllable settings { #settings }

| Setting | Value | Valid under | Read by |
|---|---|---|---|
| `inference.max_tokens` | Integer, `1` to `4294967295` | `inference.transport: http` | Every agent-loop inference call: the `max_tokens` sent to the driver, the spend admission, the context-occupancy count and the truncation warning |

Each inference call reads the setting once, so one call never mixes two values. Compaction calls
and a hook's `run-inference` do not read it.

## Controllable secrets { #secrets }

A name in `control.secrets` backs an artifact's `gateway.api_key: ${NAME}`. The capsule launches
without a value for it.

| State | A keyed request from that artifact |
|---|---|
| No value injected, or the value forgotten | Refused inside the runtime; nothing is sent upstream. The tool sees an error naming the credential and saying it has not been injected |
| A value injected | Sent with the value, rendered as the artifact's `upstream_auth:` says |

The gateway reads the value on every keyed request, so a replacement reaches the next one. The
global config's `credentials:` map and the environment are never consulted for a name in
`control.secrets`.

The value is held in memory only. It is never written to disk, the trace, the conversation record,
stderr or an error, and it is never placed in the agent's context or a tool's environment.

---

## Endpoints { #endpoints }

| Method | Path | Body | Answers `200` with |
|---|---|---|---|
| `GET` | `/control` | — | `{"session_id", "settings": [{"name", "value"}], "secrets": [{"name", "set"}]}` |
| `PUT` | `/control/settings/<name>` | `content-type: application/json`, `{"value": <n>}` | `{"name", "previous", "value", "applies_from": "next_inference_call"}` |
| `PUT` | `/control/secrets/<name>` | The raw value, exactly as sent: no JSON, no trimming | `{"name", "set": true, "replaced": <bool>}` |
| `DELETE` | `/control/secrets/<name>` | — | `{"name", "set": false}` |

`GET /control` never carries a secret value. Every request carries
`Authorization: Bearer <token>`.

### Refusals { #refusals }

Every refusal has the body `{"error": "<one sentence>"}`. A secret's value never appears in one.

| Status | `reason` in the trace | When |
|---|---|---|
| 404 | — | The capsule declares no `control:` block. Nothing is recorded |
| 401 | `unauthenticated` | `Authorization: Bearer` is missing or its token does not verify for this session. Carries `WWW-Authenticate: Bearer realm="murmur-control"` |
| 404 | `unknown_path` | No control resource at the path |
| 404 | `undeclared_setting` | The setting is not in `control.settings` |
| 404 | `undeclared_secret` | The secret is not in `control.secrets` |
| 405 | `method_not_allowed` | A method the resource does not answer. Carries `allow` |
| 403 | `not_loopback` | A secret `PUT` or `DELETE` from a peer that is not loopback |
| 413 | `body_too_large` | `content-length` over 1024 bytes for a setting, or 8192 bytes for a secret |
| 422 | `missing_body` | A `PUT` with no body, or `content-length: 0` |
| 415 | `unsupported_media_type` | A setting `PUT` without `content-type: application/json` |
| 422 | `invalid_value` | A setting value outside its range or not an integer, or a secret that is empty or holds any byte other than visible ASCII and space |
| 400 | `truncated_body` | The connection closed before `content-length` bytes arrived |

### Order of checks { #order-of-checks }

The checks run in this order, and a request stops at the first that refuses it:

1. The capsule declares `control:`.
2. The token verifies.
3. The path, the name and the method.
4. For a secret, the peer is loopback: `127.0.0.0/8`, `::1`, or an IPv4-mapped `::ffff:127.0.0.0/104`.
5. The declared `content-length`.
6. The content type and the value.

---

## The control token { #token }

| Property | Value |
|---|---|
| File | `~/.murmur/running/<session_id>.control`, mode `0600`, in the `0700` running directory |
| Written | At launch, before the session's [running record](cli.md#mur-ps), and only when `control:` is declared |
| Removed | With the running record: when the session ends, on `mur stop`, and when a reader prunes a dead session's record |
| Shape | `ctl1.<base64url payload>.<base64url MAC>` |
| Valid for | This session only. The key that verifies it is generated in memory at launch and never written, so a token from a stopped session is refused with `401` — including by a new session of the same capsule on the same port |

`mur run` never prints the token. The trace carries only its `token_id`, the first 16 hex characters
of its SHA-256.

A capsule that declares `control:` and cannot write its token file refuses to launch with
[`E-RUN-041`](diagnostics.md#e-run-041).

### Reaching a capsule on another host { #remote }

`mur control` reads the token file on the host it runs on. A controller on another host gets the
token from whoever launched the capsule, who reads the file on the capsule's host and hands it over,
then sends requests to the capsule's address with `Authorization: Bearer <token>`.

| Request | Over a non-loopback address |
|---|---|
| A setting `PUT` | Accepted. The door has no TLS, so the token crosses the network in clear |
| A secret `PUT` or `DELETE` | Refused `403`. A secret is accepted only over loopback, because the door has no TLS |

A capsule is reachable from other hosts only when `mur run --bind` names a non-loopback address.

---

## What is recorded { #recorded }

| Event | Written when |
|---|---|
| `control_change` | A change is accepted |
| `control_refused` | A request to a declared surface is refused |
| `control_applied` | The first agent-loop inference call to use a changed setting value, before the call is sent |
| `session_start.control` | At launch: the declared setting and secret names |
| `gateway_credential` with `source: "injected"` | An injected credential's keyed request found no value: once per period with no value |

Field lists are in [Observability schemas](observability-schemas.md#control-events).
`mur trace show` prints one line per `control_change` under **Control**, with the turn a setting
applied from.

Nothing is added to the conversation record, the A2A stream or `mur watch`: a control change is not
a message to the model.

## What is not persisted { #persistence }

Nothing a controller sets survives the process. A restart or `mur run --resume` starts every setting
from the manifest, holds no secret, and mints a new token.
