# mur-roost HTTP API

`mur-roost` is a local daemon that spawns capsules on request. It listens on loopback and exposes three endpoints: a health check, one to spawn a capsule, and one to poll a spawned job.

Two things call it: a plan's `capsule` step, which the capsule runtime dispatches through the daemon, and shell tools inside a capsule that call the endpoints directly.

---

## Start the daemon

```bash
mur-roost --port 7700 --spawn-allow orchestrator --spawn-allow worker-a
```

| Flag | Default | Description |
|---|---|---|
| `--port` | `7700` | Port to bind on `127.0.0.1` |
| `--registry-path` | `$HOME/.murmur/artifacts` | Local artifact registry the daemon resolves spawned capsules from |
| `--spawn-allow` | *(empty)* | One capsule name that may be spawned at top level. Repeat the flag per name; `--spawn-allow=NAME` is also accepted |

`--spawn-allow` takes a single name per occurrence, not a comma-separated list. Started with no `--spawn-allow` at all, the daemon refuses every spawn request that omits `spawned_by`.

Any other flag is rejected and the daemon exits.

---

## Endpoints

### `GET /health`

Returns `200 OK` once the daemon is listening.

```json
{}
```

---

### `POST /spawn`

Resolve a capsule from the registry, stage it, and launch it. The response returns once the capsule has bound its port, so the caller can address it immediately.

**Request body**

```json
{
  "name":       "worker-a",
  "version":    "0.1.0",
  "workdir":    "/abs/path/to/workdir",
  "spawned_by": "optional-session-id"
}
```

| Field | Type | Required | Notes |
|---|---|---:|---|
| `name` | string | yes | Capsule name; must be in the applicable allow list |
| `version` | string | yes | Capsule version |
| `workdir` | string | yes | Absolute path to an existing directory; used as the spawned capsule's session workdir |
| `spawned_by` | string | no | Session ID of the capsule making the request. Selects which allow list applies — see [Per-session allow lists](#per-session-allow-lists) |

**Success — `200 OK`**

```json
{
  "session_id":  "ses_01a000c58eae7ca0901d5e6b7427df28",
  "capsule_url": "http://localhost:53124"
}
```

`capsule_url` is the spawned capsule's A2A endpoint. Send it a `message/send` JSON-RPC call to give it work — the daemon itself carries no task payload.

**Error responses**

| Status | Condition |
|---|---|
| `400 Bad Request` | Body is not valid JSON, or a required field is missing |
| `403 Forbidden` | `name` is not in the applicable allow list, or `spawned_by` names a job the daemon does not know |
| `500 Internal Server Error` | The capsule could not be resolved from the registry, staged or launched, or it did not bind a port within 60 seconds |

---

### `GET /status/{session_id}`

Poll a spawned capsule.

**Success — `200 OK`**

```json
{ "status": "running" }
```

| `status` | Meaning |
|---|---|
| `running` | Launched and still executing |
| `complete` | The session ended without error |
| `failed` | Staging or launch failed, or the session ended with an error |

**Error — `404 Not Found`**

```json
{ "error": "session not found" }
```

Session records are held in memory. Restarting the daemon discards them, and every session ID from before the restart then returns `404`.

---

## Per-session allow lists

`mur-roost` keeps two levels of capsule allow list, and `spawned_by` selects between them.

| Request | List consulted | Where it comes from |
|---|---|---|
| No `spawned_by` | Global | The daemon's `--spawn-allow` flags |
| `spawned_by` present | Per-session | `capabilities.spawn.allow` in the manifest of the capsule that owns that session, read when the session was staged |

A capsule that sets `spawned_by` can spawn only the names listed in its *own* manifest, even where the global list permits more:

- Daemon started with `--spawn-allow orchestrator --spawn-allow worker-a --spawn-allow worker-b`
- Capsule A's manifest: `capabilities.spawn.allow: [worker-a]`
- Capsule A is spawned; its session ID is `ses_01a0…`
- Capsule A sends `POST /spawn` with `name: worker-b` and `spawned_by: ses_01a0…` → **403**

A `spawned_by` the daemon does not recognise is refused with `403`; it falls back to no other list.

!!! note "Trust boundary"
    Within a single-machine local deployment the process boundary is the trust boundary. A capsule can claim any known session ID as `spawned_by` and receive that session's allow list.

---

## Environment variables

| Variable | Set by | Purpose |
|---|---|---|
| `MURMUR_ROOST_URL` | The environment of the process that runs the capsule | Base URL a plan's `capsule` step calls to spawn its child. When it is unset or blank, the step fails with `MURMUR_ROOST_URL is not set; capsule steps require mur-roost` |
| `MURMUR_SESSION_ID` | The runtime, in every capsule | The capsule's own session ID, which it passes as `spawned_by` so its per-session allow list applies |

A shell tool inside a capsule can call the daemon directly:

```bash
curl -s -X POST "http://127.0.0.1:7700/spawn" \
  -H 'Content-Type: application/json' \
  -d "{\"name\":\"worker-a\",\"version\":\"0.1.0\",\"workdir\":\"$PWD\",\"spawned_by\":\"$MURMUR_SESSION_ID\"}"
```
