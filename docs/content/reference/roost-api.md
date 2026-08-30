# mur-roost HTTP API

`mur-roost` is a local daemon that spawns capsules on request. It listens on loopback and exposes four endpoints: a health check, one to ask permission to spawn a capsule, one to spawn it, and one to poll a spawned job.

One thing calls it: a plan's `capsule` step, which the capsule runtime dispatches through the daemon. A spawn on behalf of a running capsule requires a credential the daemon minted for that capsule's runtime, which is held in runtime memory and is not readable from inside the capsule, so a shell tool cannot make the call itself.

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

`--spawn-allow` takes a single name per occurrence, not a comma-separated list. It gates the top-level path only — the requests that carry no credential. Started with no `--spawn-allow` at all, the daemon refuses every such request.

Any other flag is rejected and the daemon exits.

---

## Endpoints

### `GET /health`

Returns `200 OK` once the daemon is listening.

```json
{}
```

---

### `POST /delegate`

Ask permission to spawn a capsule. This is where the allow list and the [spawn envelope](#spawn-envelope) are checked, and the only place they are checked.

**Request headers**

| Header | Required | Notes |
|---|---:|---|
| `x-murmur-spawn-credential` | yes | The credential the daemon minted for the calling session — see [Credentials and approvals](#credentials-and-approvals) |

**Request body**

```json
{
  "name":    "worker-a",
  "version": "0.1.0"
}
```

| Field | Type | Required | Notes |
|---|---|---:|---|
| `name` | string | yes | Capsule name; must be in the calling session's `capabilities.spawn.allow` |
| `version` | string | yes | Capsule version |

**Success — `200 OK`**

```json
{
  "approval":      "msa1.eyJ2IjoxLCJz…",
  "expires_at_ms": 1756531200000
}
```

`approval` is an opaque token naming the artifact the daemon resolved, by name, version and content hash. `expires_at_ms` is its absolute expiry in unix milliseconds, 60 seconds after it was granted.

**Error responses**

| Status | Condition |
|---|---|
| `400 Bad Request` | Body is not valid JSON, or a required field is missing |
| `403 Forbidden` | The credential is absent or not valid — see [Refusals](#refusals) |
| `403 Forbidden` | `name` is not in the calling session's allow list |
| `403 Forbidden` | The capsule's manifest declares more capability than the calling session holds — see [Spawn envelope](#spawn-envelope) |
| `500 Internal Server Error` | The capsule could not be resolved from the registry |

Nothing is created here. A delegation, granted or refused, leaves no session directory, no trace and no job record.

---

### `POST /spawn`

Resolve a capsule from the registry, stage it, and launch it. The response returns once the capsule has bound its port, so the caller can address it immediately.

**Request headers**

| Header | Required | Notes |
|---|---:|---|
| `x-murmur-spawn-credential` | on the delegated path | The credential the daemon minted for the calling session |
| `x-murmur-spawn-approval` | on the delegated path | An approval from `POST /delegate`, unexpired and not yet redeemed |

Both headers together are the delegated path; neither is the top-level operator path. Exactly one of the two is refused, and does not fall back to `--spawn-allow`.

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
| `name` | string | yes | Capsule name; must resolve to the artifact the approval names, or be in `--spawn-allow` on the top-level path |
| `version` | string | yes | Capsule version |
| `workdir` | string | yes | Absolute path to an existing directory; used as the spawned capsule's session workdir |
| `spawned_by` | string | no | Session ID of the capsule making the request. Selects nothing: the credential names the calling session. Present on the delegated path it must equal that session ID, and present without a credential the request is refused |

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
| `403 Forbidden` | The credential or the approval is absent or not valid, or `spawned_by` disagrees with the credential — see [Refusals](#refusals) |
| `403 Forbidden` | The approval has expired, has already been redeemed, or names a different artifact |
| `403 Forbidden` | `name` is not in `--spawn-allow`, on the top-level path |
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

`mur-roost` keeps two levels of capsule allow list, and the credential selects between them.

| Request | List consulted | Where it comes from |
|---|---|---|
| No credential | Global | The daemon's `--spawn-allow` flags |
| Credential present | Per-session | `capabilities.spawn.allow` in the manifest of the capsule that owns the session the credential names, read when the session was staged |

A capsule that delegates can spawn only the names listed in its *own* manifest, even where the global list permits more:

- Daemon started with `--spawn-allow orchestrator --spawn-allow worker-a --spawn-allow worker-b`
- Capsule A's manifest: `capabilities.spawn.allow: [worker-a]`
- Capsule A is spawned; its session ID is `ses_01a0…`
- Capsule A's runtime sends `POST /delegate` with `name: worker-b` → **403**

---

## Spawn envelope

The credential selects two decisions, not one. The allow list above answers *which* capsules the spawning capsule may spawn. The envelope answers *how much* any of them may hold: the daemon lowers the child's registry manifest and refuses the request when the child would hold more capability than its parent on any axis.

The comparison runs at `POST /delegate`, after the name check and before the child's workload is staged, created or launched. A refused delegation leaves no session directory, no trace and no job record. It runs once: the approval it grants names the resolved artifact by content hash, and that hash determines the manifest the comparison read.

| Axis | Manifest key | Rule |
|---|---|---|
| Network allow | `capabilities.network.allow` | Every child entry is covered by a parent entry. A bare `example.com` covers `https://example.com`; a parent entry of `https://example.com` covers only that exact form, because the bare form spans both schemes and every port |
| Unix sockets | `capabilities.network.unix_sockets` | A child `true` requires a parent `true` |
| Peer-fetch allow | `capabilities.peer_fetch.allow` | The coverage rule above, applied to the separate list |
| Shell allow | `capabilities.shell.allow` | Every child binary name appears in the parent's list |
| Spawn allow | `capabilities.spawn.allow` | Every child capsule name appears in the parent's list |
| Env allow | `capabilities.env.allow` | Every child variable name appears in the parent's list |
| Filesystem scope | `capabilities.filesystem.scope` | A parent that declares no scope holds the whole workdir and covers anything. A parent that declares one covers a child scope equal to it or beneath it, and refuses a child that declares none |
| Workdir exec | `capabilities.filesystem.workdir_exec` | A child `true` requires a parent `true` |
| State stores | `capabilities.state.store` | Every store the child's artifacts would open is one the parent's artifacts also open. An artifact that declares `state:` without a `store:` opens a store named after its own capsule |
| Containment | `capabilities.containment` | The child's floor is at or above the parent's |

Containment is the one axis where a difference in the child's favour is allowed. A floor is a requirement rather than a grant, so it may only rise: a `scoped` parent may spawn a `sealed` child, and a child that declares `advisory` under a `scoped` parent is refused.

A mismatch on any other axis is refused rather than narrowed to fit. Fix it by widening the parent's declaration or narrowing the child's.

A refusal names the manifest key and the child declaration that exceeded:

```json
{
  "error": "capabilities.network.allow: the child declares 'api.example.com', which its parent does not hold — a spawned capsule can never hold more capability than the capsule that spawned it"
}
```

The grants compared are the ones in the manifest the daemon resolves from the registry. The request body carries no manifest and no capability declaration, and any extra keys in it are ignored.

A request with no credential has no parent to be within: the global `--spawn-allow` list is the only gate, and the capsule's own grants are compared against nothing.

---

## Credentials and approvals

A delegated spawn is a two-step exchange against two opaque, MAC'd tokens.

| Token | Minted | Names | Lifetime |
|---|---|---|---|
| Credential | When a session whose manifest declares `capabilities.spawn.allow` is staged | The session it was minted for | The daemon process |
| Approval | By `POST /delegate`, once the referee has passed | One session, and one artifact by name, version and content hash | 60 seconds, one redemption |

The credential is handed to the session's *runtime* and stays in memory there. Nothing the capsule can read carries it: not the workdir, not an environment variable, not a tool result, not an error message. A capsule therefore cannot call the daemon itself; its runtime makes the two requests on its behalf.

The approval binds a launch to the artifact the referee actually judged. A different name, a different version, or the same coordinates resolving to different bytes is refused. An approval is marked spent as soon as it verifies, before the artifact is compared, so presenting one for the wrong artifact consumes it.

Both keys live only in memory. Restarting the daemon invalidates every outstanding credential and approval at once.

### Refusals

Refusals split into two classes.

*Identity* failures — an absent, malformed or unverifiable credential, a credential naming a session that is not running, a `spawned_by` disagreeing with the credential, an approval bound to a different session, or exactly one of the two headers — all answer `403` with one message:

```json
{
  "error": "not authorised: a spawn must present a credential and an approval minted for the same running session"
}
```

Two requests differing only in whether the session they name exists get byte-identical responses — same status line, same headers, same body — so the endpoint cannot be used to discover which sessions are running.

*Approval-state* failures answer `403` and say what went wrong, because reaching one requires already holding a valid credential:

| Condition | Message |
|---|---|
| Expired | `this spawn approval has passed its expiry; an approval is valid for 60 seconds from the POST /delegate that granted it` |
| Replayed | `this spawn approval has already been redeemed; an approval covers one launch, so ask POST /delegate for another` |
| Different coordinates | `this spawn approval was granted for 'worker-a@0.1.0', not 'worker-b@0.1.0'` |
| Different bytes | `'worker-a@0.1.0' now resolves to a different artifact than the one this spawn approval was granted for (approved sha256 …, resolved sha256 …)` |

!!! note "Trust boundary"
    Within a single-machine local deployment the process boundary is the trust boundary. The daemon judges the session its credential names, not the session a request body claims, so a caller that reaches the loopback port without a credential can use only the top-level `--spawn-allow` path. The credential is a bearer token over loopback: anything that can read another session's runtime memory can present that session's credential.

---

## Environment variables

| Variable | Set by | Purpose |
|---|---|---|
| `MURMUR_ROOST_URL` | The environment of the process that runs the capsule | Base URL a plan's `capsule` step calls to spawn its child. When it is unset or blank, the step fails with `MURMUR_ROOST_URL is not set; capsule steps require mur-roost` |
| `MURMUR_SESSION_ID` | The runtime, in every capsule | The capsule's own session ID, which its traces carry and which `mur run` prints |

The spawn credential has no environment variable. `POST /delegate` and the delegated `POST /spawn` are made by the capsule's runtime, which holds the credential; `MURMUR_SESSION_ID` authorises nothing on its own.
