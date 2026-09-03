# mur-roost HTTP API

`mur-roost` is a local daemon that decides which capsules may spawn which other capsules. It
listens on loopback and exposes five endpoints: a health check, one to ask permission to spawn a
capsule, one for a session to announce itself, one to retire it, and one to poll a session.

The daemon runs nothing. It holds no capsule runtime, stages no session, creates no directory,
takes no host probe and starts no process — it resolves manifests from its registry, referees them
against one another, and mints tokens. The launching is done by the runtime of the capsule that
asked, in a process of the child's own.

One thing calls it: a capsule's runtime. A spawn on behalf of a running capsule requires a
credential the daemon minted for that capsule's runtime, which is held in runtime memory and is not
readable from inside the capsule, so a shell tool cannot make the call itself.

---

## Start the daemon

The installer ships `mur-roost` beside `mur`, in the same directory and at the same version:

```bash
curl -fsSL https://install.murmur.rs | sh
```

Start it on loopback:

```bash
mur-roost --port 7700 --spawn-allow orchestrator --spawn-allow worker-a
```

| Flag | Default | Description |
|---|---|---|
| `--port` | `7700` | Port to bind on `127.0.0.1` |
| `--registry-path` | `$HOME/.murmur/artifacts` | Local artifact registry the daemon resolves manifests from |
| `--spawn-allow` | *(empty)* | One capsule name that may register without an approval. Repeat the flag per name; `--spawn-allow=NAME` is also accepted |
| `--max-depth` | `3` | Levels of delegation allowed below a top-level capsule — see [Delegation bounds](#delegation-bounds) |
| `--max-concurrent` | `4` | Children one session may hold live at once |

`--spawn-allow` takes a single name per occurrence, not a comma-separated list. It gates the
top-level path only — the registrations that present no approval. Started with no `--spawn-allow`
at all, the daemon admits only capsules launched under an approval it granted.

`--max-depth` and `--max-concurrent` each take a whole number, and neither has a value meaning
unlimited: `0` refuses every delegation.

Any other flag is rejected and the daemon exits. There is no `--max-total` — see
[No total cap](#no-total-cap).

---

## Endpoints

### `GET /health`

Returns `200 OK` once the daemon is listening.

```json
{}
```

---

### `POST /spawn`

Ask permission to spawn a capsule. This is where the allow list and the
[spawn envelope](#spawn-envelope) are checked, and the only place they are checked.

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

The body carries no manifest and no capability declaration, and any extra keys in it are ignored.

**Success — `200 OK`**

```json
{
  "approval":      "msa1.eyJ2IjoxLCJz…",
  "name":          "worker-a",
  "version":       "0.1.0",
  "sha256":        "9f2c…",
  "expires_at_ms": 1756531200000
}
```

`approval` is an opaque token naming the artifact the daemon resolved, by name, version and content
hash. `name`, `version` and `sha256` are that artifact, echoed so the caller launches the same one
the referee judged. `expires_at_ms` is the approval's absolute expiry in unix milliseconds, 60
seconds after it was granted.

The response carries no `capsule_url` and no `session_id`, because nothing was started.

**Error responses**

| Status | Condition |
|---|---|
| `400 Bad Request` | Body is not valid JSON, or a required field is missing |
| `403 Forbidden` | The credential is absent or not valid, or names a session that is not running — see [Refusals](#refusals) |
| `403 Forbidden` | `name` is not in the calling session's allow list |
| `403 Forbidden` | The calling session has no delegation depth left — see [Delegation bounds](#delegation-bounds) |
| `403 Forbidden` | The calling session already holds `--max-concurrent` live children |
| `403 Forbidden` | The capsule's manifest declares more capability than the calling session holds — see [Spawn envelope](#spawn-envelope) |
| `500 Internal Server Error` | The capsule could not be resolved from the registry |

---

### `POST /register` { #post-register }

A session announcing itself, and taking the credential it will delegate with.

The registrant names an artifact. The daemon resolves *that* artifact from its own registry and
lowers the manifest into a spawn envelope itself; the request never states what the session holds.
A request that could state its grants would be a request that could declare its own ceiling.

A session registers when its manifest declares `capabilities.spawn.allow`, and when it was launched
under an approval — the second because presenting the approval is what marks it spent. Every other
capsule never calls this endpoint, needs no daemon, and runs with nothing listening.

**Request headers**

| Header | Required | Notes |
|---|---:|---|
| `x-murmur-spawn-approval` | no | The approval this session was launched with. Absent means the top-level path, gated by `--spawn-allow` |

**Request body**

```json
{
  "session_id": "ses_01a000c58eae7ca0901d5e6b7427df28",
  "name":       "worker-a",
  "version":    "0.1.0"
}
```

| Field | Type | Required | Notes |
|---|---|---:|---|
| `session_id` | string | yes | The registering session's own id, minted by its runtime. Must not already be registered |
| `name` | string | yes | Capsule name, as published |
| `version` | string | yes | Capsule version |

Extra keys are accepted and change nothing. A body carrying its own `capabilities` or `envelope`
block registers exactly the same grants as one carrying neither: the ones in the registry manifest.

**Success — `200 OK`**

```json
{ "credential": "msc1.eyJ2IjoxLCJz…" }
```

The session is now `running`, and the credential is what makes its `POST /spawn` answerable.

**Error responses**

| Status | Condition |
|---|---|
| `400 Bad Request` | Body is not valid JSON, or `session_id` is empty |
| `403 Forbidden` | The approval is not valid, or the session that earned it is no longer running, or `session_id` is already registered — see [Refusals](#refusals) |
| `403 Forbidden` | The approval has expired, has already been redeemed, or names a different artifact |
| `403 Forbidden` | No approval was presented and `name` is not in `--spawn-allow` |
| `500 Internal Server Error` | The capsule could not be resolved from the registry |

An approval that does not verify is a failed exchange, not a request to be judged by the operator's
list instead: it is refused rather than falling back to `--spawn-allow`.

---

### `POST /deregister`

A session reporting that it has ended.

**Request headers**

| Header | Required | Notes |
|---|---:|---|
| `x-murmur-spawn-credential` | yes | The credential this session was minted at registration |

**Request body**

```json
{ "outcome": "complete" }
```

| Field | Type | Required | Notes |
|---|---|---:|---|
| `outcome` | string | yes | `complete` or `failed`. Becomes the session's `GET /status` value |

**Success — `200 OK`**

```json
{}
```

The credential still carries a valid MAC afterwards — nothing can un-mint one — but every endpoint
that means anything requires the session it names to be running, so it authorises nothing from
here on.

**Error responses**

| Status | Condition |
|---|---|
| `400 Bad Request` | Body is not valid JSON, or `outcome` is neither `complete` nor `failed` |
| `403 Forbidden` | The credential is absent or not valid, or names a session that is not running |

The call is best effort on the runtime's side: a session that has already finished is not failed
because the daemon it was reporting to had gone away.

---

### `GET /status/{session_id}`

Poll a registered session.

**Success — `200 OK`**

```json
{
  "status":          "running",
  "depth_remaining": 2,
  "live_children":   1
}
```

| Field | Type | Meaning |
|---|---|---|
| `status` | string | `running` — registered and not yet retired; `complete` or `failed` — the session deregistered reporting that outcome |
| `depth_remaining` | number | Levels of delegation still available below this session, against `--max-depth` |
| `live_children` | number | Children this session holds right now, counting one it has been approved to launch and has not launched yet |

**Error — `404 Not Found`**

```json
{ "error": "session not found" }
```

Session records are held in memory. Restarting the daemon discards them, and every session ID from
before the restart then returns `404`.

---

## Who launches what

| Step | Who does it |
|---|---|
| Ask whether a child may be spawned | The parent capsule's runtime, at `POST /spawn` |
| Referee the child's manifest against the parent's | The daemon |
| Create the child's directory and place its inputs | The parent capsule's runtime |
| Start the child process | The parent capsule's runtime, as a `mur` subprocess |
| Probe the host for its containment class | The child's own runtime, in the child's own process |
| Record what the child holds | The daemon, at the child's `POST /register` |
| Stop the child | The parent capsule's runtime |

Three problems are absent rather than solved by this split. A daemon crash takes no child with it,
because nothing a child needs lives in the daemon's address space. Each child has its own process
environment and working directory, so a native subprocess started by one child inherits nothing a
sibling shares. And a child declaring the `sealed` containment floor enters a mount namespace of
its own, because every containment mechanism is installed per process and the child *is* a process.

### The child's directory

The parent's runtime composes the child's directory at
`<parent accessible workdir>/.murmur/children/<capsule name>-<16 hex>`, creates it owner-only
(`0700` on Unix), and passes it as the child's `--workdir`. The 16 hex characters are fresh per
delegation, so spawning the same capsule twice yields two directories rather than one shared one.

**The parent retains write access to a running child's directory.** That is deliberate: the parent
creates the directory and places the child's inputs in it before launch, and the directory sits
beneath the parent's own accessible workdir, which is a single preopen the WASI layer cannot carve
a hole in. It is convenient — it is how a parent streams inputs to a child that is already running
— and it is a channel the spawn envelope does not cover. A child cannot reach out of its own
directory, so nothing flows the other way, but a parent can write into a running child's workspace
without any grant saying so.

### The child's environment

A delegated child starts from a cleared environment. Its parent's runtime builds the whole of what
the child holds:

| Name | Present when |
|---|---|
| `PATH` | Always, from the parent's own value |
| `HOME` | Always, from the parent's own value |
| `MURMUR_ROOST_URL` | Always, so the child registers at the same daemon its parent asked |
| `MURMUR_SPAWNER` | The child was given a spawner handle — see [The completion path](#the-completion-path) |
| Any other name | The name appears in both the child's and the parent's `capabilities.env.allow`, and the parent's own process environment holds it |

Every other variable the parent holds is absent from the child. A name a child declares that its
parent does not is refused at `POST /spawn` on the [`capabilities.env.allow` axis](#spawn-envelope).
The four names above are the runtime's, and a child that lists one of them under
`capabilities.env.allow` does not displace the runtime's value.

The value copied is the value the parent's process holds at launch. A variable the parent itself
does not hold is absent from the child even where both manifests declare it, and a child whose
`inference.api_key` references it fails at manifest load with
[`E-MAN-003`](diagnostics.md#index).

The daemon resolves a child manifest's `${VARIABLE}` references against its own process
environment when it referees a spawn. Start `mur-roost` in an environment holding every variable
the capsules it referees reference: a reference the daemon cannot resolve fails the spawn, for a
manifest the child itself would have loaded.

---

## The delegation tool

A capsule whose manifest names at least one capsule in `capabilities.spawn.allow` gains one tool,
`delegate-task`. The grant governs the tool's existence: where the list is empty, the tool is
missing from the capsule's directory, from `session_start`'s `tools_declared` and from the
inventory its model is shown, so the model has nothing to call.

| Argument | Required | Meaning |
|---|---|---|
| `capsule` | yes | The sub-capsule's name. The schema's `enum` holds exactly this capsule's `capabilities.spawn.allow`, so the granted names are the only names the model can supply |
| `version` | yes | That capsule's exact version. `latest`, `stable` and `edge` are reserved words that resolve to no artifact |
| `task` | yes | The whole of what the sub-capsule is told. It arrives as the child's first user message |

Those three strings are the whole of what the agent supplies. The daemon's address, this session's
credential, the approval, the child's directory, the child's process and the A2A conversation with
it are composed by the capsule's own runtime, so **a delegating capsule needs no
`capabilities.network.allow` entry for the daemon** and never sees a token.

### What the call returns

**The call returns when the child starts, not when it finishes.** A result is a JSON object:

| Field | Type | Meaning |
|---|---|---|
| `delegation_id` | string | `dlg_…`, the id this delegation is named by in `trace.jsonl` and on the outcome that arrives later |
| `session_id` | string | `ses_…`, the child's own session |
| `capsule`, `version` | string | The artifact that was launched |
| `status` | string | `started`, `failed` or `refused` |
| `child_workdir` | string | The child's directory, relative to the delegating capsule's accessible workdir — where its own `trace.jsonl` is, and where its result will be. Present only on `started` |
| `output` | string | Why there is no delegation, on `failed`. Absent on `started`, which has produced nothing yet |

`started` means the sub-capsule is running **and holding its task**, not merely that a process
exists. Everything short of that fails the call in the same turn: a daemon refusal, a child that
could not be launched, one that bound no address, and one that did not accept its task within the
delivery bound. A child that fails at any of those points is stopped and reaped rather than left
running.

A refusal comes back instead as a plain sentence naming the manifest key and the entry that failed,
with no JSON around it. The delegating capsule's own run carries on: a refused delegation is a
failed tool call, and the session continues.

### How the outcome arrives

What the sub-capsule did reaches the delegating capsule afterwards, as its own task with
[origin](../concepts/access-control.md#task-origin-and-trust-class) `completion` in the `bg` lane,
carrying the same `dlg_` id the call returned. That is the whole of what a turn issuing several
delegations has to do differently: issue them, end the turn, and handle each outcome as it lands.
See [The completion path](#the-completion-path) for what one carries.

**A delegating capsule has to be able to receive one.** Under the default `lifecycle` block the
session ends with the task that made the delegation, and every outcome is posted to a session that
is already gone. A capsule that delegates declares:

| Key | Value | Why |
|---|---|---|
| [`lifecycle.task_acceptance`](manifest.md#lifecycle-task-acceptance) | `queue` | An outcome is an inbound task; a capsule accepting one task at a time never takes it |
| [`lifecycle.queue_depth`](manifest.md#field-lifecycle) | at least the number of delegations one turn issues | Each outcome in flight occupies a slot |
| [`lifecycle.after_task`](manifest.md#lifecycle-after-task) | `sleep` | The session has to outlive the task that delegated |

A capsule that declares `capabilities.spawn.allow` and leaves that block unable to receive a
completion is warned at launch with [`W-SEC-020`](diagnostics.md#w-sec-020). The launch is not
refused: a capsule that delegates and deliberately does not wait is legitimate.

### Bounds

| Bound | Value | What it covers |
|---|---|---|
| Launch | 180s | From starting the child process to its first `--json` line |
| Delivery | 30s | Retrying the task delivery while the child's listener comes up |
| Child watch | [`lifecycle.delegation_deadline_secs`](manifest.md#lifecycle-delegation-deadline-secs), default 600s, or `MURMUR_DELEGATION_TIMEOUT_SECS` | How long the started child runs. Counted from the moment it reported itself ready, so the launch bound above is not spent out of it. On expiry the child is ended and a `terminated` completion is posted to the delegating capsule, naming the bound in seconds |

The child-watch bound is the delegating capsule's own runtime's clock. No request is made to the
daemon to decide or enforce it, so no daemon has to be reachable for it to fire.

How deep a chain of delegations may go and how many a capsule may have running at once are the
daemon's, not this tool's — see [Delegation bounds](#delegation-bounds). A delegation the daemon
refuses comes back as a failed tool call carrying the refusal.

**A delegated capsule reports when its session ends.** A sub-capsule that sleeps between tasks
never ends its session, so nothing about it reaches its parent until the child-watch bound stops it.
Declare `lifecycle.after_task: exit` on a capsule meant to be delegated to with `delegate-task`.

A plan's `capsule` step is the exception, and it runs the other way: that step waits on the
connection it opened and reads the answer with an A2A `tasks/get`, so a capsule meant to be used as
a plan step must still be listening when the read happens and declares
`lifecycle.after_task: sleep` instead.

---

## The completion path

A delegated child tells its parent that it finished. The parent's runtime injects one variable at
launch, the child posts one message back at the end of its session, and the outcome arrives at the
parent as a task with `completion` origin in the background lane — behind anything a person or a
peer is waiting for.

### What is injected

| Field of `MURMUR_SPAWNER` | Meaning |
|---|---|
| `url` | The parent's own A2A endpoint, `http://host:port` |
| `session_id` | The parent's session. A completion addressed anywhere else is refused |
| `context_id` | The conversation the delegation was made from. The completion task runs under it |
| `trust` | `trusted` or `untrusted` — the trust class of the parent task that made the delegation |
| `delegation_id` | `dlg_…`, minted by the parent's launcher, one per launch |

The value is compact JSON, applied last in the child's environment alongside `MURMUR_ROOST_URL`, so
a child cannot displace it by listing the name in `capabilities.env.allow`. A capsule nobody
delegated has no `MURMUR_SPAWNER` at all and contacts nobody. A capsule whose `MURMUR_SPAWNER`
cannot be read refuses to launch with [`E-RUN-020`](diagnostics.md#e-run-020).

### What the completion carries

The delegation's identity, the outcome and where the result is — never the child's output. The
result stays in the child's own directory, which is inside the parent's single preopen, so a parent
that wants it reads the file deliberately through an ordinary tool call.

| Field | Meaning |
|---|---|
| `delegation_id` | The id the parent's launcher minted, echoed back |
| `capsule_name`, `capsule_version` | Which capsule ran |
| `session_id` | The child's own session, so its trace is findable |
| `status` | `ok`, `error`, `crashed` or `terminated` |
| `result_path` | Workdir-relative path to the result, absent when the child wrote none |
| `workdir` | The child's directory, absolute — the root `result_path` is relative to |
| `duration_ms` | How long the child ran |
| `detail` | The exit status and the child's last stderr lines, on a `crashed` or `terminated` outcome |

The same fields are written to `completion.json` in the child's own directory, with three more that
describe the delivery rather than the outcome.

| Field of `completion.json` only | Meaning |
|---|---|
| `reported_by` | `child` or `launcher` — which of the two reporters built the record |
| `delivered` | Whether the notification reached the parent's door |
| `delivery_error` | Why delivery failed, when one was attempted and refused |

### How it travels

One JSON-RPC `message/send` to the parent's `POST /`, carrying the fields above as its message
text, with four request headers.

| Header | Value |
|---|---|
| `x-murmur-task-origin` | `completion` |
| `x-murmur-task-trust` | The `trust` of the injected handle |
| `x-murmur-delegation-id` | The `delegation_id` of the injected handle |
| `x-murmur-completion-session` | The `session_id` of the injected handle |

The parent's door refuses a completion whose `x-murmur-completion-session` is not the session
running there — the shape a parent that restarted onto the same address leaves behind — with the
JSON-RPC error `completion is addressed to session <id>, which is not the session running here`.
Both delegation headers are read only for a request classified `completion`, and ignored on every
other path.

### Who reports, and what happens when nobody can

| Situation | Reporter | `status` |
|---|---|---|
| The child's session ended | The child, at the end of its own session | `ok` or `error` |
| The child's process ended without recording a completion | The parent's launcher | `crashed` |
| The parent ended the delegation itself | The parent's launcher, recorded and posted to nobody | `terminated` |

Both reporters write the outcome to `completion.json` before posting it, and rewrite the file with
what the posting did. Between those two writes the record reads `delivered: false` with no
`delivery_error`, so a reader polling the file should wait for one of the two to be set. A
completion that could not be delivered is recorded with `delivered: false` and the refusal's
reason, and one line goes to stderr; the launcher retries an undelivered completion once, and after
that the file and the line are the record. A failed delivery does not fail the child's own session:
`status` still records how that session ended.

A delegation is never reported twice, whichever reporter gets there first.

!!! note "A host restart loses a delegation in flight"
    A parent that sleeps while a child works loses the child entirely if the host restarts: no
    completion arrives at the parent, and none is written to `completion.json` afterwards.

---

## Per-session allow lists

`mur-roost` keeps two levels of capsule allow list, and how a session was launched selects between
them.

| Registration | List consulted | Where it comes from |
|---|---|---|
| No approval | Global | The daemon's `--spawn-allow` flags |
| Approval present | Per-session | `capabilities.spawn.allow` in the manifest of the capsule that owns the session the approval was granted to |

A capsule that delegates can spawn only the names listed in its *own* manifest, even where the
global list permits more:

- Daemon started with `--spawn-allow orchestrator --spawn-allow worker-a --spawn-allow worker-b`
- Capsule A's manifest: `capabilities.spawn.allow: [worker-a]`
- Capsule A registers; its session ID is `ses_01a0…`
- Capsule A's runtime sends `POST /spawn` with `name: worker-b` → **403**

---

## Spawn envelope

The credential selects two decisions, not one. The allow list above answers *which* capsules the
spawning capsule may spawn. The envelope answers *how much* any of them may hold: the daemon lowers
the child's registry manifest and refuses the request when the child would hold more capability
than its parent on any axis.

The comparison runs at `POST /spawn`, after the name check, and it runs exactly once per
delegation: the approval it grants names the resolved artifact by content hash, and that hash
determines the manifest the comparison read.

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

Containment is the one axis where a difference in the child's favour is allowed. A floor is a
requirement rather than a grant, so it may only rise: a `scoped` parent may spawn a `sealed` child,
and a child that declares `advisory` under a `scoped` parent is refused. Whether the host can
actually deliver a raised floor is not this daemon's question — it takes no host probe. That is
decided in the child's own process, at its own launch, and reported as
[`E-CAP-003`](diagnostics.md#e-cap-003) when the host falls short.

A mismatch on any other axis is refused rather than narrowed to fit. Fix it by widening the
parent's declaration or narrowing the child's.

A refusal names the manifest key and the child declaration that exceeded:

```json
{
  "error": "capabilities.network.allow: the child declares 'api.example.com', which its parent does not hold — a spawned capsule can never hold more capability than the capsule that spawned it"
}
```

The grants compared are the ones in the manifest the daemon resolves from the registry.

A registration with no approval has no parent to be within: the global `--spawn-allow` list is the
only gate, and the capsule's own grants are compared against nothing.

---

## Credentials and approvals

A delegated spawn travels on two opaque, MAC'd tokens, presented by two different parties.

| Token | Minted | Names | Presented by | Lifetime |
|---|---|---|---|---|
| Credential | At `POST /register` | The session it was minted for | That session's own runtime, at `POST /spawn` and `POST /deregister` | The daemon process |
| Approval | At `POST /spawn`, once the referee has passed | One session, and one artifact by name, version and content hash | The launched child's runtime, at `POST /register` | 60 seconds, one redemption |

The credential is handed to the session's *runtime* and stays in memory there. Nothing the capsule
can read carries it: not the workdir, not an environment variable, not a tool result, not an error
message. A capsule therefore cannot call the daemon itself; its runtime makes the requests on its
behalf.

The approval reaches the child on the child process's standard input, written by the parent and
closed immediately — not on the argument vector and not in the environment, both of which any
process running as the same user can read out of `/proc`.

The approval binds a launch to the artifact the referee actually judged. A different name, a
different version, or the same coordinates resolving to different bytes is refused. An approval is
marked spent as soon as it verifies, before the artifact is compared, so presenting one for the
wrong artifact consumes it.

Both keys live only in memory. Restarting the daemon invalidates every outstanding credential and
approval at once.

### Refusals

Refusals split into two classes.

*Identity* failures — an absent, malformed or unverifiable token, a credential naming a session
that is not running, an approval whose granting session has ended, a credential presented as an
approval or the reverse, and a `session_id` that is already registered — all answer `403` with one
message, on every endpoint:

```json
{
  "error": "not authorised: this daemon answers only a credential it minted for a running session, and an approval it minted for that same session"
}
```

Two requests differing only in whether the session they name exists get byte-identical responses —
same status line, same headers, same body — so no endpoint can be used to discover which sessions
are running.

*Approval-state* failures answer `403` and say what went wrong, because reaching one requires
already holding a verifiable approval:

| Condition | Message |
|---|---|
| Expired | `this spawn approval has passed its expiry; an approval is valid for 60 seconds from the POST /spawn that granted it` |
| Replayed | `this spawn approval has already been redeemed; an approval covers one launch, so ask POST /spawn for another` |
| Different coordinates | `this spawn approval was granted for 'worker-a@0.1.0', not 'worker-b@0.1.0'` |
| Different bytes | `'worker-a@0.1.0' now resolves to a different artifact than the one this spawn approval was granted for (approved sha256 …, resolved sha256 …)` |

The name-list refusals are their own, and name the list an operator has to edit:
`capsule 'worker-a' is not in parent's spawn_allow` at `POST /spawn`, and
`capsule 'worker-a' is not in --spawn-allow` at `POST /register`.

!!! note "Trust boundary"
    Within a single-machine local deployment the process boundary is the trust boundary. The daemon
    judges the session its credential names, not the session a request body claims, so a caller that
    reaches the loopback port without a credential can use only the top-level `--spawn-allow` path.
    The credential is a bearer token over loopback: anything that can read another session's runtime
    memory can present that session's credential.

---

## Delegation bounds

Two bounds on delegation, both the operator's and both decided from the daemon's own records rather
than from anything a capsule says about itself.

| Bound | Flag | Default | What it counts |
|---|---|---|---|
| Depth | `--max-depth` | `3` | Levels of delegation below a capsule that registered with no approval |
| Concurrency | `--max-concurrent` | `4` | Children one session holds live at once |

Both are checked at `POST /spawn`, after the name check, and a refused spawn leaves no record
anywhere — no session, no workdir and no trace.

### The depth budget

Each session holds a number: how many further levels may hang below it. A session that registered
with no approval holds `--max-depth`. Every other session holds the number sealed into the approval
it registered with, which is one less than what its parent held. A session at `0` is refused every
spawn it asks for.

The budget rides the approval. `POST /spawn` computes the child's number, seals it into the MAC'd
approval it mints, and reads it back out when that child presents the approval at `POST /register`.
Between those two moments no party states it: the parent's runtime never sees the number, the
`POST /register` body has no field it could arrive in, and an extra `depth_remaining` key in that
body is read by nothing. An approval whose payload was edited fails its MAC and is refused with the
[identity refusal](#refusals).

This is what terminates a capsule whose `capabilities.spawn.allow` names itself. The
[spawn envelope](#spawn-envelope) narrows a chain but never ends one — a capsule that names itself
narrows to itself — so with `--max-depth 3` such a capsule runs at four levels and the fourth
level's `POST /spawn` is refused:

```json
{
  "error": "delegation depth bound reached: this daemon allows 3 levels of delegation below a top-level capsule (--max-depth 3), and this session has none left to spend — a capsule whose capabilities.spawn.allow names itself terminates here rather than recursing"
}
```

### The concurrency cap

A session may hold `--max-concurrent` children at once. The refusal names the bound and the count,
so an operator reading it knows what to raise and what it is being raised past:

```json
{
  "error": "delegation concurrency bound reached: this daemon allows a capsule 2 live children at a time (--max-concurrent 2), and this session already holds 2 — wait for one to finish, or raise --max-concurrent"
}
```

A child occupies a slot from the moment its parent is approved to launch it, so `live_children`
can exceed the number of sessions that have registered.

| Event | Effect on the parent's count |
|---|---|
| `POST /spawn` is granted | The approval reserves a slot |
| The child redeems the approval at `POST /register` | The reservation becomes a running child; the count is unchanged |
| The child's `POST /deregister` | The slot is released |
| The approval's 60-second expiry passes unredeemed | The slot is released, whether or not anybody ever presented it |
| The parent's own `POST /deregister` | Every reservation it holds is released, because an approval it earned can no longer be redeemed by anybody |

A session's current figures are readable at [`GET /status/{session_id}`](#get-statussession_id) as
`depth_remaining` and `live_children`.

### A bound that cannot be evaluated refuses

Every figure a bound is decided from is read from the record of a *running* session. Where that
record cannot be read the request is refused rather than granted on a default, and the refusal is
the identity one. A `POST /spawn` whose credential names a session that has deregistered, and a
`POST /register` whose approval was granted by a session that has since deregistered, are both
refused on that ground.

### No total cap { #no-total-cap }

There is no cap on the total number of delegations, and no `--max-total` flag. An operator who
tries to set one gets `unknown argument: --max-total` and the daemon exits.

The job store is held in memory, so restarting the daemon discards every registration and every
count a total would be kept in. `--max-depth` and `--max-concurrent` are unaffected: both are
decided from the sessions the daemon is currently tracking, and a restart that discards those
sessions also discards every credential and approval that could delegate under them.

---

## Environment variables

| Variable | Set by | Purpose |
|---|---|---|
| `MURMUR_ROOST_URL` | The environment of the process that runs the capsule; set on a child by its parent's runtime | Base URL the runtime registers at, and the base URL a plan's `capsule` step asks permission at. When it is unset or blank, a capsule that declares `capabilities.spawn.allow` refuses to launch with [`E-RUN-019`](diagnostics.md#e-run-019), and a `capsule` step fails with `MURMUR_ROOST_URL is not set; capsule steps require mur-roost` |
| `MURMUR_SESSION_ID` | The runtime, in every capsule | The capsule's own session ID, which its traces carry and which `mur run` prints |
| `MURMUR_SPAWNER` | The parent capsule's runtime, on a delegated child only | Where the child reports its outcome, and under which delegation id — see [The completion path](#the-completion-path). A value that is not a spawner handle refuses the launch with [`E-RUN-020`](diagnostics.md#e-run-020) |
| `MURMUR_MUR_BINARY` | The environment of the process that runs the capsule | The `mur` binary a parent starts its children from. Defaults to the running executable, which in production is `mur` itself |
| `MURMUR_DELEGATION_TIMEOUT_SECS` | The environment of the process that runs the capsule | The single delegation bound, in whole seconds: how long a started sub-capsule is watched, and how long a plan `capsule` step waits for its answer. Sets [`lifecycle.delegation_deadline_secs`](manifest.md#lifecycle-delegation-deadline-secs) for the whole process. Default 600. A value that is not a positive integer is ignored |

The spawn credential and the spawn approval have no environment variable. Every request to this
daemon is made by the capsule's runtime, which holds them; `MURMUR_SESSION_ID` authorises nothing
on its own.
