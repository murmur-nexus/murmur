# mur-roost HTTP API

`mur-roost` is the local orchestration daemon that manages spawned capsules. It exposes a small HTTP API consumed by `mur run` (auto-start path) and by capsule shell tools that make direct `curl` calls.

---

## Start the daemon

```bash
mur-roost \
  --port 7700 \
  --nexus-url http://localhost:7800 \
  --spawn-allow orchestrator,worker-a,worker-b \
  [--scoped] \
  [--max-concurrent 4]
```

| Flag | Default | Notes |
|---|---|---|
| `--port` | `7700` | Port to bind |
| `--nexus-url` | `http://localhost:7800` | Nexus registry base URL |
| `--spawn-allow` | *(required)* | Comma-separated list of capsule names that may be spawned |
| `--scoped` | false | Stage each spawned worker in its own isolated job root |
| `--max-concurrent` | unlimited | Maximum simultaneous scoped workers (only used with `--scoped`) |

---

## Endpoints

### `GET /health`

Returns `200 OK` when the daemon is ready.

```json
{ "status": "ok" }
```

---

### `POST /spawn`

Enqueue a capsule for execution and return a job ID immediately.

**Request body**

```json
{
  "name":       "worker-a",
  "version":    "0.1.0",
  "workdir":    "/abs/path/to/workdir",
  "input":      "{\"schema\":\"murmur.message.v1\",\"type\":\"murmur.code_task.request.v1\",\"job_id\":null,\"payload\":{\"objective\":\"Do the worker task\"}}",
  "spawned_by": "optional-job-id"
}
```

| Field | Type | Required | Notes |
|---|---|---|---|
| `name` | string | yes | Capsule name; must be in the active allow list |
| `version` | string | yes | Capsule version |
| `workdir` | string | yes | Absolute path to an existing directory; seeded into the worker's session |
| `input` | string | no | Serialized structured task input. When present and non-empty, roost writes it to the worker staging root as `input.json` before launch |
| `spawned_by` | string | no | Job ID of the capsule making this request (see [Per-job allow lists](#per-job-allow-lists)) |

`input` is usually a serialized [`input.json`](workdir.md#inputjson-task-input) `murmur.code_task.request.v1` envelope. If both the caller seed and the spawn request provide `input.json`, the request `input` overwrites the copied seed file.

**Success — `202 Accepted`**

```json
{ "job_id": "job_550e8400e29b41d4a716446655440000" }
```

**Error responses**

| Status | Condition |
|---|---|
| `400 Bad Request` | Missing or invalid fields |
| `403 Forbidden` | Capsule name not in the relevant allow list |

---

### `GET /status/{job_id}`

Poll the status of a previously spawned job.

**Success — `200 OK`**

```json
{ "status": "queued" }
{ "status": "running" }
{ "status": "complete", "output_path": "/abs/path/to/workdir/workdir/<session-id>" }
{ "status": "failed",   "error": "..." }
```

The `output_path` directory contains `out/result.txt` (and optionally `out/result.json`) written by the capsule.

**Error — `404 Not Found`**

```json
{ "error": "job '<id>' was not found" }
```

---

## Per-job allow lists

`mur-roost` enforces two levels of capsule allow lists:

**Global list** (`--spawn-allow`): set at daemon start. Applies to all spawn requests that do **not** carry `spawned_by`.

**Per-job list**: derived from the spawning capsule's own `capabilities.spawn.allow` manifest field at job creation time. Applies when `spawned_by` is present and refers to an existing job.

A capsule that sets `spawned_by` can only spawn names listed in its *own* manifest — even if the global `--spawn-allow` list would permit more. Example:

- Global allow list: `[orchestrator, worker-a, worker-b]`
- Capsule A manifest: `capabilities.spawn.allow: [worker-a]`
- Capsule A is spawned; its job ID is `job-123`
- Capsule A sends `POST /spawn` with `name: worker-b` and `spawned_by: job-123` → **403**

This ensures each capsule in a spawn hierarchy is limited to the names its own author declared.

!!! note "Trust boundary"
    Within a single-machine local deployment the process boundary is the trust boundary. A capsule can claim any known job ID as `spawned_by` and receive that job's allow list. Per-job spawn tokens are deferred to post-v1.0.0.

---

## MURMUR_ROOST_URL propagation

`mur-roost` injects `MURMUR_ROOST_URL=http://127.0.0.1:<port>` into every capsule it launches via the `shell_baseline_env` capability. Shell tools inside the capsule can use this to make further spawn calls without hard-coding the port:

```bash
curl -s -X POST "$MURMUR_ROOST_URL/spawn" \
  -H 'Content-Type: application/json' \
  -d "{\"name\":\"worker-a\",\"version\":\"0.1.0\",\"workdir\":\"$PWD\",\"spawned_by\":\"$MURMUR_JOB_ID\"}"
```

This propagation is transitive: a capsule spawned by `mur-roost` that itself spawns another capsule will also see `MURMUR_ROOST_URL` set.

---

## Source references

- `crates/mur-roost/src/server.rs` — route handlers
- `crates/mur-roost/src/job_store.rs` — job and allow-list storage
- `crates/mur-roost/src/worker.rs` — capsule resolution, env injection, and execution
- `crates/mur-roost/src/main.rs` — CLI and daemon startup
