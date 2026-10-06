# Roster Schema

A `roster.yaml` declares a formation: the capsules that run together as its **members**, the one
**entry member** that receives the formation's task, and which members may call which. It sits in
the project directory `mur run --roster` names, which needs no `murmur.yaml` of its own.

```yaml
roster_version: 1
members:
  - name: planner
    capsule: planner
    version: 0.3.0
    entry: true
  - name: coder
    capsule: coder
    version: 1.2.0
  - name: reviewer
    capsule: reviewer
    version: 0.9.0
reachability:
  - from: planner
    to: [coder, reviewer]
  - from: reviewer
    to: [coder]
```

[`mur run --roster`](cli.md#mur-run-roster) admits a roster and launches it — see
[Launching a formation](#launch). [`mur doctor`](cli.md#mur-doctor) admits it and reports the
result. `mur run` without `--roster` does not read `roster.yaml`.
[`mur new --roster <NAME>`](cli.md#mur-new-roster) writes a two-member roster, with a source
directory for each member, that admission accepts as written.

## Fields { #fields }

Every key is listed below. Any other key, at any level, is refused with
[`E-ROS-001`](diagnostics.md#e-ros-001).

| Field | Type | Required | Notes |
|---|---|---:|---|
| `roster_version` | integer | yes | Must be `1`. |
| `members` | list | yes | At least one entry. Order matters: admission reports faults in this order. |
| `members[].name` | string | yes | The member's name within this roster. Matches `^[a-z][a-z0-9-]{0,31}$`. Unique within the roster. |
| `members[].capsule` | string | yes | The installed capsule the member runs. A valid artifact name. Two members may run the same capsule. |
| `members[].version` | string | yes | The exact version of `capsule`. Not blank, and not `latest`, `stable` or `edge`. An unquoted number such as `1.10` reads as its text, as `artifacts[].version` does in `murmur.yaml`. |
| `members[].entry` | boolean | no | `true` on exactly one member. Default: `false`. |
| `reachability` | `all` or list | no | Which members may call which. Default: no member may call another. An empty list means the same. |
| `reachability[].from` | string | yes | The calling member. |
| `reachability[].to` | list of strings | yes | The members `from` may call. At least one, never `from` itself, and never the entry member. |

A key written with no value, such as `entry:`, is the same as an absent key.

## Members and versions { #members }

A member is a capsule at an exact version. Admission looks it up in the project store
(`.murmur/artifacts/`) and then the global store (`~/.murmur/artifacts/`), the order
`mur run --capsule` uses, and binds the member to the sha256 of the artifact it found. Install
each member first with `mur install`.

The `murmur.yaml` beside the roster is a member only when a member names its capsule.

### `murmur.lock` { #murmur-lock }

When the project's `murmur.lock` pins a member's capsule, the version and hash admission resolved
must match the pin, or the roster is refused with [`E-REG-005`](diagnostics.md#index). A capsule
the lock does not pin is admitted at the version the roster names. Admission never creates or
changes `murmur.lock`.

## The entry member { #entry-member }

The entry member receives the formation's task, splits it up and hands the parts to the members
it may call. Its task outcome is the formation's outcome.

The entry member is never called. It runs the formation's one task from launch until the
formation ends, and accepts no other, so there is no task slot for a peer's call to fill. A rule
that lists it in `to` is refused with [`E-ROS-008`](diagnostics.md#e-ros-008).

| Reachability | The entry member calls | The entry member is called by |
|---|---|---|
| `all` | Every other member that serves peers, if it serves peers itself | No member |
| list of rules | The members its own rules list in `to` | No member |

To have a member report to the entry member, let the entry member call it: write a rule
`from: <entry>` with that member in `to`. The member's answer comes back into the entry member's
own task — see [Giving a member work](#member-calls).

## `reachability` { #reachability }

A rule means "`from` may call each member in `to`". Rules add together, and the same edge written
twice is one edge.

| Value | Edges |
|---|---|
| absent, or `[]` | None |
| `all` | Every ordered pair of distinct members that both declare [`exports.peer_tasks.accept: true`](manifest.md#field-exports-peer-tasks), except pairs into the entry member |
| list of rules | Each rule's `from` to each member in its `to` |

A rule names members only. Every member a rule lists in `to` must declare
`exports.peer_tasks.accept: true`, or the roster is refused with
[`E-ROS-005`](diagnostics.md#e-ros-005). A caller does not need to. A rule that lists the
[entry member](#entry-member) in `to` is refused with [`E-ROS-008`](diagnostics.md#e-ros-008).

## Authentication { #authentication }

When the roster has at least one edge, **every** member must declare
[`network.authentication`](manifest.md#field-network-authentication), including members on no
edge. A member's door is where the roster is enforced: a call from another member carries the
formation token its launcher issued for that edge, and the door checks it — see
[How reachability is enforced](#enforcement). A public door checks nothing and takes a task from
any caller. A missing declaration is refused with [`E-ROS-006`](diagnostics.md#e-ros-006).

A roster with one member, no `reachability`, or an `all` that pairs no members has no edges and
needs no authentication.

`murmur.yaml` refuses `network.authentication` beside a non-empty `capabilities.spawn.allow`
(`E-MAN-003`), so a member of a roster with edges cannot also delegate to child capsules.

## Admission order { #admission }

Admission admits the whole roster or refuses it with one code. It checks, in this order, and stops
at the first failure. Within a check, members are taken in roster order.

| Order | Check | Code |
|---:|---|---|
| 1 | `roster.yaml` reads and has the shape above | [`E-ROS-001`](diagnostics.md#e-ros-001) |
| 2 | Member names are unique | [`E-ROS-003`](diagnostics.md#e-ros-003) |
| 3 | Exactly one member has `entry: true` | [`E-ROS-002`](diagnostics.md#e-ros-002) |
| 4 | Every rule's `from` and `to` names a member | [`E-ROS-004`](diagnostics.md#e-ros-004) |
| 5 | No rule lists the entry member in `to` | [`E-ROS-008`](diagnostics.md#e-ros-008) |
| 6 | Each member is installed at its version, and its packed `murmur.yaml` parses | [`E-ROS-007`](diagnostics.md#e-ros-007) |
| 6 | `murmur.lock` agrees with each member it pins | [`E-REG-005`](diagnostics.md#index) |
| 7 | Every member a rule calls serves peers | [`E-ROS-005`](diagnostics.md#e-ros-005) |
| 8 | With any edge, every member's door requires authentication | [`E-ROS-006`](diagnostics.md#e-ros-006) |

Checks 1–5 read no store, so a roster with a structural fault is refused even when no member is
installed. Check 6 takes each member in turn through both rows before the next.

## Launching a formation { #launch }

[`mur run --roster`](cli.md#mur-run-roster) launches the admitted roster for one task:

1. **Admit.** The roster is admitted as above. A refusal starts nothing.
2. **Mint.** One formation id and one signing key are minted for this launch. The key exists only
   in the launcher's memory.
3. **Start the peers.** Every member except the entry member starts at once, as its own
   `mur run --capsule <capsule> --capsule-version <version> --json` process, bound to `127.0.0.1`,
   in a [directory of its own](#member-directories), with its own
   [formation channel](#formation-channel) already holding its credentials. A peer takes work only
   at its door.
4. **Wait for each door.** A peer is ready when the door at the URL its readiness line reported
   serves an agent card naming the session id that line reported.
5. **Hand out addresses.** Each peer that may call other peers is sent their door URLs on its
   channel.
6. **Start the entry member.** The entry member starts last, with
   `--lifecycle-task-acceptance single --lifecycle-after-task exit`. Its channel holds its
   credentials and its callees' door URLs before it starts.
7. **End.** When the entry member's process ends, for any reason, the formation ends: every peer
   is stopped and the signing key is dropped. See [How a formation ends](#launch-stop).

| Every member | Value |
|---|---|
| `MURMUR_FORMATION_ID` | The formation id minted in step 2 |
| `MURMUR_FORMATION_CHANNEL` | The number of the inherited file descriptor its [formation channel](#formation-channel) is read from |
| `MURMUR_FORMATION_LIFELINE` | The descriptor of the member's own [lifeline](#launch-stop). Set by the launcher; not for operators |
| Directory | The entry member runs in the roster's project directory, where its `--task` is written. Each peer runs in its own directory under `~/.murmur/formations/` — see [Member directories](#member-directories) |
| Stores | The roster's project, for every member: the project store, then the global store, and the project's `murmur.lock` — the stores admission read |
| Current directory | The launcher's, so a relative `--task` path names the same file |
| The installed artifact | Exactly the bytes admission bound it to. A member whose installed artifact changed since admission refuses with [`E-RUN-047`](diagnostics.md#e-run-047) |

### Member directories { #member-directories }

| Member | Accessible directory | Sessions |
|---|---|---|
| Entry member | The roster's project directory | `<project>/.murmur/<ses_id>/` |
| Each peer | `~/.murmur/formations/<frm_id>/<member>/` | `~/.murmur/formations/<frm_id>/<member>/.murmur/<ses_id>/` |

`~/.murmur/formations/<frm_id>/` also holds `formation.json`, the formation's ownership marker.
The launcher writes it at mode `0600` before the first peer's directory is made. A roster with no
peers makes no formation directory and no marker.

| Key | Value |
|---|---|
| `project_dir` | The roster's project directory, with symlinks resolved |
| `launcher_pid` | The launcher's process id |
| `launcher_start` | When the launcher's process started, as the host reports it. Empty when it could not be read |

A launcher that cannot write the marker launches anyway and prints
`[mur run] warning: formation <frm_id> has no ownership marker (<reason>); its directory will never be removed automatically`.

- The launcher makes `~/.murmur/formations/` and `~/.murmur/formations/<frm_id>/` owner-only, and
  makes each peer's directory with mode `0700`. A path already there is never reused: the peer is
  refused with [`E-RUN-045`](diagnostics.md#e-run-045) naming it, and nothing is launched.
- Every member resolves its artifacts and its lock from the roster's project, wherever it runs, so
  a peer installed only in `<project>/.murmur/artifacts` launches.
- Files do not cross between members. A peer's directory is in no other member's reach, and each
  member's `task.md` is its own: two members working at once never touch each other's. A member
  that needs a file's content sends it in the task text.
- The [formation line](cli.md#mur-run-roster) names each peer's directory as `workdir`.

### Removing formation directories { #formation-retention }

The entry member's [`trace.retain`](manifest.md#retention) bounds its roster's formation
directories, as it bounds the entry member's own sessions. Without it, no formation directory is
ever removed.

| Key | For formation directories |
|---|---|
| `max_sessions` | Keep this many of the project's formation directories, newest first, the current formation's included |
| `max_age` | Remove a formation directory older than this, measured from the time in its `frm_` id |

Both keys apply together: a directory over either limit is removed, whole. The pass runs at the end
of every `mur run --roster`, after every member of that launch has stopped, whether the entry
member ended, the launcher was signalled, or the launch was refused after it started. Each removal
is one line on stderr, newest first:

```text
[mur run] retention: removed formation frm_0199c4e2f1b7712a9d3e4f5061728394 (max_sessions)
```

The reason is `max_age` for a directory over both limits. Nothing is written to stdout.

A formation directory is never removed when:

- it is the current launch's formation, or one minted after it — so a formation's directory always
  outlives the launch that made it;
- its launcher is still running, or a [running record](cli.md#mur-ps) names the formation and that
  member is still running. When `~/.murmur/running/` cannot be read, every formation counts as
  running;
- its `formation.json` names another project;
- it has no `formation.json`, or one that does not parse. Directories made by a `mur` that wrote no
  marker are in this case.

Removing an ended formation's directory by hand stops nothing that is running, and takes its peers'
traces with it: [`mur trace show frm_<id>`](cli.md#mur-trace-show) then finds only the entry
member's session.

### Readiness { #launch-readiness }

Each member has 180 seconds from its own start to become ready. A peer refuses the whole launch
with [`E-RUN-045`](diagnostics.md#e-run-045) when it:

- exits before it reports;
- prints a first line that is not a readiness line;
- reports no door;
- reports a formation other than the one it was launched with;
- does not report within the deadline;
- exits after it reports, before its door answers;
- reports a door that does not answer as its session within the deadline.

A refused launch stops every member already started, and never starts the entry member. Readiness
is the door: it is probed every 100 ms until it answers as the reported session.

### How a formation ends { #launch-stop }

Every member, the entry member included, is handed a **lifeline**: the read end of a pipe of its
own, whose other end only the launcher holds. Nothing is ever written to it. The member reads end
of file on it in exactly one case: the formation has ended. That happens when the launcher closes
it, or when the launcher's process dies by any means, `SIGKILL` and the OOM killer included.

| The formation ends because | The launcher |
|---|---|
| The entry member's process ended | Closes every peer's lifeline |
| The launcher received `SIGINT`, `SIGTERM` or `SIGHUP` | Closes every lifeline, the entry member's included |
| A peer did not come up | Closes the lifeline of every member already started |
| The launcher was killed | Is gone; the kernel closes every lifeline |

The launcher sends no member `SIGTERM`. A member whose lifeline closes winds down:

1. It appends one [`formation_ended`](observability-schemas.md#formation-ended) record to its
   `trace.jsonl`, naming the formation id, before anything else its wind-down writes.
2. It ends the way a first [`SIGTERM`](cli.md#mur-run-sigterm) ends it: every live task is
   cancelled, queued tasks are refused, live delegations are cancelled, asynchronous hooks are
   drained, and `session_end` is written.
3. 20 seconds after its lifeline closed, it exits with status 143 wherever its teardown is.

Work in flight is cancelled, not finished: the formation's task is over, and nothing remains to
receive a result.

| When | What happens |
|---|---|
| 20 seconds after the lifeline closed | The member exits with status 143 |
| 25 seconds after the launcher closed the lifeline | A launcher still running sends `SIGKILL` to the member's process group, then reaps it |
| A `SIGTERM` after the lifeline closed | Changes nothing; the wind-down is already under way |
| A second `SIGTERM` | The member exits at once, with status 143 |
| The lifeline closes after a `SIGTERM` | Changes nothing, and no `formation_ended` is written |

`kill -KILL` of the launcher, of the entry member, or of the launcher's whole process group leaves
no member of that formation running within 30 seconds. Every member that was not itself killed
winds down as above, and its trace ends in `session_end`. Each peer runs in a process group of its
own; the entry member stays in the launcher's, so a `kill -KILL` of that group takes the entry
member with it.

A peer's diagnostics go to the launcher's stderr behind its `[<member>] ` prefix while the launcher
runs, and to the peer's own `logs/bootstrap.log` once the launcher is gone. The entry member writes
to the stderr it shares with the launcher.

**A member started by hand.** A `mur run` whose environment carries `MURMUR_FORMATION_ID` but no
lifeline, and which is not a delegated child, runs as any other `mur run` does: nothing winds it
down when its formation ends. It prints [`W-RUN-007`](diagnostics.md#w-run-007) once at launch.
End it with [`mur stop`](cli.md#mur-stop). A delegated child carries its parent's formation id and
no formation lifeline, and prints nothing. It holds a
[spawner lifeline](roost-api.md#spawner-lifeline) instead, so it winds down when the
member that delegated to it ends, however that member ends.

A process that already carries `MURMUR_FORMATION_ID` is a formation member, and refuses
`mur run --roster` with [`E-RUN-046`](diagnostics.md#e-run-046).

### Resuming a member's session { #launch-resume }

A member's session resumed with [`mur run --resume`](cli.md#mur-run) continues that session's
conversation and nothing else of its formation. A `mur run` belongs to a formation only when its
own environment places it in one, so a session resumed by hand never re-adopts a formation:

| Formation part | In a resumed session |
|---|---|
| Formation id | None: `session_start` has no `formation_id`, `formation_member` or `formation_callees`, and the `--json` readiness line has no `formation_id` |
| Credentials and callees' addresses | None: no [formation channel](#formation-channel) is handed to it |
| [`call-member`](runtime-provided-tools.md#call-member) | Not offered |
| Lifeline | None: nothing winds it down, and its trace has no `formation_ended` |
| [`W-RUN-007`](diagnostics.md#w-run-007) | Not printed |

`mur run --roster` refuses `--resume` with exit status 2. A formation lives for one task, so to run
it again, launch it again with a new task.

## How reachability is enforced { #enforcement }

In a formation launched by `mur run --roster`, a member can call exactly the members the roster
lets it call, and no other member answers it. No member's model ever sees a credential.

### The formation credential { #formation-token }

The launcher is the only principal that issues credentials. For every edge `from → to` it
signs one **formation token** with the formation's signing key:

```text
mft1.<payload>.<signature>
payload: {"formation":"frm_…","from":"<member>","to":"<member>"}
```

Every member's door is handed the key's public half, which verifies a token and cannot sign one,
so no member can mint a token. A token names the one door it is for: `to` is part of what is
signed.

### What a door answers { #enforcement-door }

A member's door checks the `Authorization` header before anything else, in this order:

| Request | Status | `error` |
|---|---|---|
| No `Authorization` header | `401` | `unauthenticated` |
| A formation token that does not verify: forged, altered, another formation's, or any formation token at a door in no formation | `401` | `invalid_token` — the same body as any other invalid token |
| A valid formation token issued for another member's door | `403` | `not_permitted` — the message names only the member that presented it |
| A valid formation token issued for this door, calling a method outside its two scopes | `403` | `insufficient_scope`, naming the credential `member:<caller>` |
| A valid formation token issued for this door, within its scopes | Served, as the credential `member:<caller>` | — |

A formation token carries exactly these scopes:

| Scope | Lets the caller |
|---|---|
| `message/send` | Start a task |
| `tasks/get` | Read the state of a task the calling member started; any other id is `-32001 Task not found`, as [Agent Card: `tasks/get`](agent-card.md#tasks-get) describes |

A formation token does not reach `message/stream`: a `message/stream` connection carries the frames
of every task the door runs, other members' included, as
[Streaming Protocol: Endpoints](streaming-protocol.md#endpoints) describes.

The operator token and declared credentials work as they do outside a formation. A peer task is
still refused with `403 peer_not_accepted` by a member that does not declare
[`exports.peer_tasks.accept: true`](manifest.md#field-exports-peer-tasks).

### The formation channel { #formation-channel }

The launcher hands each member its credentials on a pipe that only that member's `mur run`
inherits. `MURMUR_FORMATION_CHANNEL` names the pipe's file descriptor. It is set by
`mur run --roster` and by nothing else.

| Line | Carries | Written |
|---|---|---|
| First | The formation id, the member's name, the verification key, and one token for each member it may call | Before the member starts |
| Later | The door URLs of the members it may call | Once every peer's door answers; for the entry member, before it starts |

`mur run` reads the first line before it stages the session, and refuses with
[`E-RUN-046`](diagnostics.md#e-run-046) when:

- `MURMUR_FORMATION_CHANNEL` is set without `MURMUR_FORMATION_ID`;
- the descriptor is not open, or is not a pipe or a file;
- no first line arrives within 10 seconds;
- the first line is not a member's credentials, or a token in it was not issued to this member;
- the first line names another formation than `MURMUR_FORMATION_ID`.

No token is put in an environment variable, a command line, a file, the trace, a log line or
stdout.

### What a member's components are handed { #formation-peers }

Every WASM component a member runs — its capsule, its tools, a `transport: http` driver and its
hooks — is handed `MURMUR_FORMATION_PEERS`, naming each member it may call at a virtual address,
in roster order:

```text
MURMUR_FORMATION_PEERS=coder=http://coder.formation.invalid reviewer=http://reviewer.formation.invalid
```

A component calls a member at its virtual address. `.invalid` never resolves; the member's runtime
recognises the address and sends the request on:

1. It resolves the name to the member's real door URL. A door URL that has not arrived on the
   channel yet is waited for up to 10 seconds.
2. It checks the real URL against the sending component's
   [`capabilities.network.allow`](manifest.md#field-capabilities), per-artifact narrowing
   included. The roster grants a credential and a name, never network access: a member whose
   components may not reach `localhost` cannot call anyone.
3. It removes any `Authorization`, `x-murmur-task-origin` and `x-murmur-task-trust` header the
   component set, attaches `Authorization: Bearer <token>`, and stamps the origin `peer` with the
   trust class of the task the component is running for (`untrusted` when there is none).

[`murmur:message/send`](wit-interfaces.md#message-send) resolves a virtual `peer-url` the same way, and the
trace records the virtual address.

A request to any other name under `formation.invalid` — a member it may not call, or no member at
all — is refused before a connection opens, with the same refusal either way.

| Property | Value |
|---|---|
| Pair | `<member name>=http://<member name>.formation.invalid` |
| Absent | When the member may call nobody. The variable is never set empty |
| Runtime-owned | `capabilities.env.allow` and `capabilities.shell.baseline_env` neither supply nor replace it |
| Shell commands, native tools, a `transport: process` harness | Do not receive it. A virtual address works only through the runtime |
| Set in `mur run`'s own environment | The launch is refused with [`E-RUN-046`](diagnostics.md#e-run-046) |

A shell-script component reads it without a parser:

```sh
for peer in $MURMUR_FORMATION_PEERS; do
  echo "${peer%%=*} answers at ${peer#*=}"
done
```

### Giving a member work { #member-calls }

A member's agent calls another member with the runtime-provided
[`call-member`](runtime-provided-tools.md#call-member) tool, which exists exactly when this roster
lets it call someone. A call is made from a running task, and names the member and states the task
in full:

1. The member's door gets the task as a `message/send`, carrying the caller's formation token. The
   tool call returns as soon as the door holds the task.
2. The member runs it in its own directory, as any task at its door.
3. When the caller's turn ends with an [`inference.max_turns`](manifest.md#field-inference) turn
   still left, the caller's same task waits for the answer and continues with it, fenced under
   `member:<name>`. A task with no turn left does not wait — see
   [How the answer arrives](runtime-provided-tools.md#call-member-answer).

The call reaches the member's real door only if the caller's own
[`capabilities.network.allow`](manifest.md#network-allow-entries) does. A door is served on
loopback `http` at a port chosen at launch, so the caller declares the bare host:

```yaml
capabilities:
  network:
    allow:
      - localhost
```

A member the roster lets call others, whose grant reaches no such door, prints
[`W-RUN-008`](diagnostics.md#w-run-008) at launch, and every call it makes fails.

The caller's runtime reads the answer from the member's [`tasks/get`](agent-card.md#tasks-get).
On a formation token, `tasks/get` answers only the tasks the calling member submitted.

### What each member learns { #enforcement-learns }

| A member | Learns |
|---|---|
| About a member it may call | The member's name, in its components' environment. Only its runtime holds the door URL and the token |
| About a member it may not call | Nothing: no name, no address, no token |
| Its delegated children | Inherit `MURMUR_FORMATION_ID` and nothing else of the formation: no channel, no token, no `MURMUR_FORMATION_PEERS`, no formation lifeline. A child's door belongs to no formation, so it refuses every formation token with `401`. Each child holds a [spawner lifeline](roost-api.md#spawner-lifeline) of its own, which ends it when the member does |

Each member's trace names it and its callees on `session_start`, and names the calling member on
each `a2a_task_received` a formation token let in — see the
[trace schema](observability-schemas.md#session-trace-tracejsonl).

### Why a formation credential never expires { #enforcement-lifetime }

A formation credential has no expiry, no rotation and no revocation list, because it cannot outlive
the one task it was issued for. The signing key exists only in the launcher's memory and is dropped
when the formation is torn down; a new launch of the same roster has a new formation id and a new
key, so a token from an earlier launch is `401` at every door of the new one. Rotation would solve
a problem that cannot occur.
