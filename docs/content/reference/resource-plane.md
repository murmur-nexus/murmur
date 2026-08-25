# Resource Plane

A capsule serves two read-only file surfaces over its HTTP listener, under the `/resources/` path
prefix. Each is declared separately, addresses a separate subtree, and answers a different
audience.

| Plane | Declared by | Audience | Addressed by | Verbs |
|---|---|---|---|---|
| [Operator plane](#endpoints) | `exports.files` | An external process the operator runs | Path | `list`, `read` |
| [Peer plane](#peer-plane) | `exports.peer_files` | One named peer capsule | An opaque handle | `read` |

Declaring one grants nothing about the other. A capsule may declare either, both or neither, and
both default to deny.

## Operator plane { #operator-plane }

A capsule that declares [`exports.files`](manifest.md#field-exports) opens a read-only view onto
part of its [accessible workdir](workdir.md) — the directory the agent's own tools see as `.`. An
external process lists and reads the files under that subtree.

The runtime serves these reads itself, off the host path it already holds for the workdir. The
agent is not involved, so a read costs no inference turn and is answered whether the capsule is
idle or in the middle of a task.

A manifest with no `exports.files` has no resource plane: every request to it is refused with
`no_resource_plane`.

---

### Endpoints { #endpoints }

| Method | Path | Answers |
|---|---|---|
| `GET` | `/resources/files` | The listing |
| `GET` | `/resources/files/<relpath>` | One file's bytes |

Every other method under `/resources/` returns `405` with an `allow: GET` header. There is no write
path: no `PUT`, `POST`, `PATCH`, `DELETE`, mkdir or delete verb exists.

#### `GET /resources/files` { #list }

Recursive over the whole subtree, regular files only, sorted lexicographically by `path`. `path` is
relative to the export root, `/`-separated. `size_bytes`, `mtime_ms` and `sha256` all come from a
single open of that file.

```json
{"root":"out/","mode":"read-only","max_bytes":10485760,"generation":0,"containment_achieved":"sealed","entries":[{"path":"report.md","size_bytes":32,"mtime_ms":1787577327015,"sha256":"868db1c52a946cae0278871d41b60fcdef206b933946bf8668a42ac1ea443412"}]}
```

A file larger than `max_bytes` is listed with its real size and refused on read. A root that does
not exist returns `200` with `entries: []` — `exports.files.root` need not exist when the capsule
launches, and only an undeclared plane refuses a listing.

#### `GET /resources/files/<relpath>` { #read }

`200`, `content-type: application/octet-stream`, body is the raw bytes:

```
HTTP/1.1 200 OK
content-type: application/octet-stream
etag: "sha256:868db1c52a946cae0278871d41b60fcdef206b933946bf8668a42ac1ea443412"
x-murmur-mtime-ms: 1787577327015
x-murmur-generation: 0
x-murmur-containment: sealed
x-murmur-export-root: out/
connection: close
```

| Header | Value |
|---|---|
| `etag` | `"sha256:<64 hex>"` of the bytes in this response |
| `content-length` | Bytes in this response |
| `x-murmur-mtime-ms` | The file's modification time, Unix milliseconds |
| `x-murmur-generation` | Completed tasks in this runtime process — see [Generation](#generation) |
| `x-murmur-containment` | `advisory`, `scoped` or `sealed` — the class this session achieved |
| `x-murmur-export-root` | `exports.files.root` verbatim |

A read always serves the file's current bytes. No request parameter selects an earlier version, and
no earlier version is retained.

---

### Errors { #errors }

Every refusal returns a JSON body `{"error": "<code>", "message": "<text>"}`.

| Status | `error` | When |
|---|---|---|
| 404 | `no_resource_plane` | The manifest declares no `exports.files` |
| 404 | `not_found` | The path names nothing under the root, or the root does not exist |
| 403 | `outside_root` | The requested path, or a symlink's target, resolves outside the export root |
| 403 | `symlink_refused` | A symlink was encountered and the achieved containment class is `scoped` |
| 403 | `not_a_regular_file` | A directory, fifo, socket or device node |
| 413 | `too_large` | The file exceeds `exports.files.max_bytes` |
| 405 | `method_not_allowed` | Any method other than `GET` under `/resources/`. The response carries `allow: GET` |
| 500 | `io_error` | The open or read failed for any other reason |

Every request lands in [`trace.jsonl`](observability-schemas.md#session-trace-tracejsonl) as a
`resource_list` or `resource_read` event carrying the same code as its `outcome` — refusals as well
as successes.

---

### Path resolution { #path-resolution }

Shared by both planes: a redeem on the peer plane resolves its handle's path by exactly these
rules, against `exports.peer_files.root`.

Path resolution belongs to the runtime. A caller never sanitises a path: a request that would leave
the export root is refused, never normalised into something servable.

For every read, in this order and before any byte is served:

1. Percent-decode the path.
2. Refuse, without touching the filesystem: an empty path, an absolute path, a NUL byte, and any
   path component equal to `""`, `"."` or `".."`.
3. Canonicalise the export root and the target, and require the target to be the root or beneath
   it.
4. Open the final component with `O_NOFOLLOW` and confirm from that descriptor that it is a regular
   file.

Symlinks are decided by the achieved containment class — see
[Symlinks under an export root](containment.md#export-symlinks).

An `exports.files.root` that already resolves outside the accessible workdir, because it exists as a
symlink pointing out of it, refuses the launch with
[`E-CAP-007`](diagnostics.md#e-cap-007) before any session runs.

---

### Generation { #generation }

Shared by both planes: a peer redeem reports the same counter.

`x-murmur-generation` on a read, and `generation` in a listing, count the tasks that have reached a
terminal state in this runtime process. It starts at `0` and increments by one per completed task.

It is provenance: it answers *these bytes are as of turn N*. It is not a selector. No request
parameter chooses a generation, no response is refused because the generation moved on, and no
superseded bytes are kept. A capsule that is finished but still alive and rewrites a file two turns
later serves the newer file; `etag` is how a caller notices the content changed.

The generation and the validator move independently: two reads of an unchanged file across a
completed task return the same `etag` and different generations.

A runtime process that has completed no task reports `0`, which is the correct answer whether the
session has just started or the process was launched fresh over an existing workdir.

---

### Writing files an external reader will read { #authoring-convention }

Shared by both planes: a peer redeem calls the same reader.

A read is atomic against a concurrent rewrite in the way that matters for the response itself: the
file is opened once and its bytes, size and mtime all come from that one descriptor, and the
`etag` is computed over the buffer actually served. The `etag` therefore always describes the body
it was sent with, whatever the agent is doing to the file at the time.

Whether the *body* is one whole version of the file is up to how the capsule writes it:

| The agent writes by | A concurrent read sees |
|---|---|
| Writing a temp file and `rename`ing it over the target | One whole version. The reader's open descriptor stays on the old inode for the whole read; the next request opens the new one |
| Truncating and rewriting the target in place | Possibly a mix of the old and new bytes, or a short body |

Write a temp file and rename it into position. An agent that rewrites in place can be read
mid-write, and the reader has no way to tell a torn body from a whole one — the `etag` matches
either way, because it describes exactly what was served.

---

### Reading after the runtime exits { #after-teardown }

Read these files through the resource plane rather than off disk. A gateway that opens the workdir
directly gets no export check, no path checks and no trace record.

Workdirs persist past teardown, so a later read means relaunching the runtime against the
same workdir and re-requesting. A relaunched process reports `generation: 0`, because no task has
completed in it; `etag` remains the validator that tracks content.

---

## Peer plane { #peer-plane }

A capsule that declares [`exports.peer_files`](manifest.md#field-exports-peer-files) can hand one
named file to one named peer without a filesystem path crossing the wire. The runtime mints an
opaque **handle**; the agent puts that handle in an ordinary A2A message; the peer redeems it.

A capsule that declares [`capabilities.peer_fetch`](manifest.md#field-peer-fetch) can redeem such a
handle and lands the bytes as a file in its own workdir.

Both halves are separate operator decisions, and neither implies the other:

| Declaration | What the capsule gains |
|---|---|
| `exports.peer_files` | The `share-file` tool, and a peer plane that answers redeems |
| `capabilities.peer_fetch` | The `fetch-peer-file` tool |
| Neither | No tool exists, and `/resources/peer/` answers `no_peer_plane` |

Declaring either leaves the [achieved containment class](containment.md) unchanged.

### The agent's two tools { #peer-tools }

Each tool appears in the model's inventory only when its own grant is declared. An undeclared
capsule's model never sees that the tool exists.

**`share-file`**

| Field | Required | Meaning |
|---|---|---|
| `path` | yes | Relative to `exports.peer_files.root` |
| `peer` | yes | The peer's address, as `host:port` or an `http(s)` URL |
| `ttl` | no | A [duration](manifest.md#field-exports-peer-files). May only narrow: a value above `max_ttl` is clamped down to it. Absent means `max_ttl` |

```json
{"handle":"mh1.eyJ2IjoxLCJpc3M...","handle_id":"3f2a91c40b7e5d68","expires_at_ms":1755953600123,"audience":"reporter@localhost:41234"}
```

No field of the result is a filesystem path. A `path` that escapes the root — `..`, absolute,
percent-encoded, or a symlink leaving the root — fails the mint and is refused, never normalised
into something mintable.

**`fetch-peer-file`**

| Field | Required | Meaning |
|---|---|---|
| `peer` | yes | The peer's address. Checked against `capabilities.peer_fetch.allow` before any connection opens |
| `handle` | yes | The handle the peer sent |

```json
{"path":"peer-in/3f2a91c40b7e5d68-report.md","bytes":812,"sha256":"6b86b273ff34fce1...","generation":3,"peer":"localhost:41234"}
```

The bytes arrive as a **file** and never as text. No field of the result holds the file's contents,
so ingesting a peer's bytes is a file the agent must decide to read rather than context it is
handed. The stored path is chosen by the runtime: `peer-in/<handle_id>-<sanitised basename>` under
the accessible workdir.

### The handle { #handle }

```
mh1.<base64url-nopad(payload JSON)>.<base64url-nopad(HMAC-SHA256)>
```

The payload is in the clear and is not the secret:

```json
{"v":1,"iss":"ses_01a033ec2c1f7eb2a5922f84e012e89d","p":"report.md","exp":1755950300000,"n":"7f3c…"}
```

| Field | Meaning |
|---|---|
| `v` | Payload version. Only `1` is minted or accepted |
| `iss` | The minting **session's** id — the capsule instance, not the capsule |
| `p` | Path relative to `exports.peer_files.root`, canonicalised at mint time |
| `exp` | Absolute expiry, Unix milliseconds |
| `n` | 16 random bytes, lowercase hex, so two handles for the same file and audience are distinct |

There is no generation field. **A handle authorises a file, not a version of one**: a redeem always
serves the file's current bytes, so a file rewritten two turns later serves the newer content and
the `etag` is how the holder notices. No `409` is reachable on this plane.

**Redeem is idempotent, not single-use.** The same handle redeems as many times as the holder likes
until it expires or the minting session ends. There is no used-set and no per-handle server state.

`handle_id` is the first 16 lowercase hex characters of `sha256(<token>)`. It is what appears in
traces and error messages; the token itself never does.

### The audience { #audience }

The MAC covers the audience, but the token does not carry it:

```
HMAC-SHA256(instance key, "murmur-peer-handle-v1" ‖ 0x1f ‖ <payload base64url> ‖ 0x1f ‖ <audience>)
```

Both sides compute the audience without exchanging it, because both compute it from the *fetching*
capsule's own advertised identity. At mint, the minter fetches the peer's agent card from
`GET /.well-known/agent-card.json` and reads its `name` and `url`; at redeem, the fetcher asserts
the same two fields of its own identity in an `x-murmur-audience` header. In both cases the string
is `<name>@<host:port>`, lowercased.

That card fetch is an ordinary outbound request and is enforced against
`capabilities.network.allow`. **Minting grants no new outbound authority.** A peer that cannot be
reached, or whose card carries no `name` and `url`, fails the mint with `peer_unreachable`.

!!! warning "Audience binding is not peer authentication"

    Nothing proves that the process asserting `x-murmur-audience: reporter@localhost:41234` *is*
    that capsule. An attacker who both intercepts a handle and knows which peer it was minted for
    can assert that identity and redeem.

    What the binding buys is that a handle is not a credential for whoever finds it: a third
    capsule with its own identity is refused, and a token alone is insufficient.

### The minting key { #minting-key }

32 random bytes, generated at launch and only when `exports.peer_files` is declared. Held in
memory, never written to disk, never placed in an environment variable, and overwritten when the
session ends.

When the session ends every outstanding handle becomes unverifiable at once — revoke-all with no
revocation list. A handle minted by one session of a capsule does not redeem against a second
session of the same capsule over the same workdir.

That is also why a persistent capsule must declare a short handle lifetime:

| `lifecycle.after_task` | `exports.peer_files.max_ttl` |
|---|---|
| `exit` (the default) | Optional. Defaults to `1h`. No ceiling — teardown is the real bound |
| `sleep` | **Required**, and at most `15m`. Absent or longer refuses the launch with [`E-CAP-008`](diagnostics.md#e-cap-008) |

A handle's lifetime is not a durability mechanism. A consumer that needs these bytes after the
capsule is gone should have the operator relaunch the runtime against the still-present workdir and
request again.

### `GET /resources/peer/<handle>` { #redeem }

One verb, and no way to enumerate. `GET /resources/peer` and `GET /resources/peer/` return `404
not_found`; there is no `list`, no path addressing and no write path.

The request requires an `x-murmur-audience` header. A `200` carries the raw bytes and the same
validator headers a read on the operator plane carries, plus `x-murmur-handle-id`:

```
HTTP/1.1 200 OK
content-type: application/octet-stream
etag: "sha256:868db1c52a946cae0278871d41b60fcdef206b933946bf8668a42ac1ea443412"
x-murmur-mtime-ms: 1787577327015
x-murmur-generation: 3
x-murmur-containment: sealed
x-murmur-handle-id: 3f2a91c40b7e5d68
content-length: 32
connection: close
```

It never carries `x-murmur-export-root`: the peer plane discloses no path structure.

A redeem calls the operator plane's reader, so it inherits that reader's guarantees unchanged — see
[Path resolution](#path-resolution) and [Writing files an external reader will
read](#authoring-convention).

Checks run in a fixed order, and nothing downstream of the MAC is evaluated before it:

1. Parse the token's shape.
2. Require the `x-murmur-audience` header.
3. Verify the MAC, in constant time.
4. Check the expiry.
5. Resolve the path and serve.

| Status | `error` | When |
|---|---|---|
| 404 | `no_peer_plane` | The manifest declares no `exports.peer_files` |
| 400 | `malformed_handle` | Not `mh1.<b64>.<b64>`, or the payload is not JSON, or `v != 1` |
| 400 | `missing_audience` | No `x-murmur-audience` header, or it is empty |
| 403 | `handle_not_valid` | MAC verification failed — for **any** reason |
| 410 | `handle_expired` | `exp` is in the past |
| 404 | `not_found` | The named path no longer exists under the root |
| 403 | `outside_root` | The named path resolves outside `exports.peer_files.root` |
| 403 | `symlink_refused` | A symlink was refused under the [class-keyed rule](containment.md#export-symlinks) |
| 403 | `not_a_regular_file` | A directory, fifo, socket or device node |
| 413 | `too_large` | The file exceeds `exports.peer_files.max_bytes` |
| 405 | `method_not_allowed` | Any method other than `GET`. The response carries `allow: GET` |
| 500 | `io_error` | The open or read failed for any other reason |

`handle_not_valid` is one code on purpose. A tampered payload, a handle minted by a different
capsule instance, and a correct handle presented with the wrong audience produce the same status,
the same code and the same message text. Splitting them would build an oracle that tells a prober
which field to change next.

### What lands in the trace { #peer-trace }

Three event types, written at the moment of the event and described in full under
[Observability Schemas](observability-schemas.md#session-trace-tracejsonl):

| Event | Written by | Side |
|---|---|---|
| `peer_handle_mint` | The `share-file` tool | The minter |
| `peer_handle_redeem` | The listener, concurrently with any running task | The minter |
| `peer_file_fetch` | The `fetch-peer-file` tool | The fetcher |

All three carry a `handle_id`, and a mint's and its redeems' are equal. Refusals are recorded as
well as successes. **The token itself never appears in a trace on either side** — anywhere it would
otherwise reach one, including as a recorded `fetch-peer-file` argument, it is replaced with
`<handle:<handle_id>>`.

### What this plane does not do { #peer-non-goals }

| Not built | Why |
|---|---|
| Re-delegation | A handle is terminal at the audience it was minted for. A recipient cannot mint a derived handle from one it received, and the redeem path has no notion of a delegation chain |
| Peer authentication | See [The audience](#audience) |
| A `list` verb | Enumeration is the thing this plane exists to prevent |
| A write path | Every method other than `GET` returns `405` |
| A script-capsule interface | `share-file` and `fetch-peer-file` are agent-loop tools. No WIT import exposes either to a wasm component |
