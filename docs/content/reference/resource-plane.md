# Resource Plane

A capsule that declares [`exports.files`](manifest.md#field-exports) opens a read-only view onto
part of its session workdir. An external process lists and reads the files under that subtree over
the capsule's HTTP listener, under the `/resources/` path prefix.

The runtime serves these reads itself, off the host path it already holds for the workdir. No WASM
is instantiated, no lock is taken and no running turn is consulted, so the plane answers whether
the capsule is idle or mid-task — and a read costs no inference turn.

A manifest with no `exports.files` has no resource plane: every request to it is refused with
`no_resource_plane`.

---

## Endpoints { #endpoints }

| Method | Path | Answers |
|---|---|---|
| `GET` | `/resources/files` | The listing |
| `GET` | `/resources/files/<relpath>` | One file's bytes |

Every other method under `/resources/` returns `405` with an `allow: GET` header. There is no write
path: no `PUT`, `POST`, `PATCH`, `DELETE`, mkdir or delete verb exists.

### `GET /resources/files` { #list }

Recursive over the whole subtree, regular files only, sorted lexicographically by `path`. `path` is
relative to the export root, `/`-separated. `size_bytes`, `mtime_ms` and `sha256` all come from a
single open of that file.

```json
{"root":"out/","mode":"read-only","max_bytes":10485760,"generation":0,"containment_achieved":"sealed","entries":[{"path":"report.md","size_bytes":32,"mtime_ms":1787577327015,"sha256":"868db1c52a946cae0278871d41b60fcdef206b933946bf8668a42ac1ea443412"}]}
```

A file larger than `max_bytes` is listed with its real size and refused on read. A root that does
not exist returns `200` with `entries: []` — `exports.files.root` need not exist when the capsule
launches, and only an undeclared plane refuses a listing.

### `GET /resources/files/<relpath>` { #read }

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

## Errors { #errors }

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

## Path resolution { #path-resolution }

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

An `exports.files.root` that already resolves outside the session workdir, because it exists as a
symlink pointing out of it, refuses the launch with
[`E-CAP-007`](diagnostics.md#e-cap-007) before any session runs.

---

## Generation { #generation }

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

## Writing files an external reader will read { #authoring-convention }

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

## Reading after the runtime exits { #after-teardown }

The runtime is the only reader. Nothing else may serve these bytes — a second reader would be a
second authoriser, bypassing the export declaration, the resolve-beneath discipline and the trace
record alike.

Session workdirs persist past teardown, so a later read means relaunching the runtime against the
same workdir and re-requesting. A relaunched process reports `generation: 0`, because no task has
completed in it; `etag` remains the validator that tracks content.
