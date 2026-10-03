# Roster Schema

A `roster.yaml` declares a formation: the capsules that run together as its **members**, the one
**entry member** that receives the formation's task, and which members may call which. It sits in
the project directory, beside `murmur.yaml`.

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
| `reachability[].to` | list of strings | yes | The members `from` may call. At least one, and never `from` itself. |

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

The entry member gets no edge of its own. Under `reachability: all` it calls and is called only
if it serves peers, like any other member. To let it call members without being callable, write
a rule for it.

## `reachability` { #reachability }

A rule means "`from` may call each member in `to`". Rules add together, and the same edge written
twice is one edge.

| Value | Edges |
|---|---|
| absent, or `[]` | None |
| `all` | Every ordered pair of distinct members that both declare [`exports.peer_tasks.accept: true`](manifest.md#field-exports-peer-tasks) |
| list of rules | Each rule's `from` to each member in its `to` |

A rule names members only. Every member a rule lists in `to` must declare
`exports.peer_tasks.accept: true`, or the roster is refused with
[`E-ROS-005`](diagnostics.md#e-ros-005). A caller does not need to.

## Authentication { #authentication }

When the roster has at least one edge, **every** member must declare
[`network.authentication`](manifest.md#field-network-authentication), including members on no
edge. A peer task carries no credential of its own, so a public door in a formation takes a task
from any member. A missing declaration is refused with [`E-ROS-006`](diagnostics.md#e-ros-006).

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
| 5 | Each member is installed at its version, and its packed `murmur.yaml` parses | [`E-ROS-007`](diagnostics.md#e-ros-007) |
| 5 | `murmur.lock` agrees with each member it pins | [`E-REG-005`](diagnostics.md#index) |
| 6 | Every member a rule calls serves peers | [`E-ROS-005`](diagnostics.md#e-ros-005) |
| 7 | With any edge, every member's door requires authentication | [`E-ROS-006`](diagnostics.md#e-ros-006) |

Checks 1–4 read no store, so a roster with a structural fault is refused even when no member is
installed. Check 5 takes each member in turn through both rows before the next.

## Launching a formation { #launch }

[`mur run --roster`](cli.md#mur-run-roster) launches the admitted roster for one task:

1. **Admit.** The roster is admitted as above. A refusal starts nothing.
2. **Mint.** One formation id is minted for this launch.
3. **Start the peers.** Every member except the entry member starts at once, as its own
   `mur run --capsule <capsule> --capsule-version <version> --json` process, bound to `127.0.0.1`.
   A peer takes work only at its door: it never runs a `task.md` in the project directory, where
   the entry member's task is written.
4. **Wait for each door.** A peer is ready when the door at the URL its readiness line reported
   serves an agent card naming the session id that line reported.
5. **Start the entry member.** The entry member starts last, with
   `--lifecycle-task-acceptance single --lifecycle-after-task exit`, and with
   [`MURMUR_FORMATION_PEERS`](#formation-peers) naming the members it may call.
6. **Stop.** When the entry member's process ends, for any reason, every peer is stopped.

| Every member | Value |
|---|---|
| `MURMUR_FORMATION_ID` | The formation id minted in step 2 |
| `--workdir` | The roster's project directory, so every member resolves from the stores admission read, and sessions land under `<project>/.murmur/` |
| Current directory | The launcher's, so a relative `--task` path names the same file |
| The installed artifact | Exactly the bytes admission bound it to. A member whose installed artifact changed since admission refuses with [`E-RUN-047`](diagnostics.md#e-run-047) |

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

### Stopping { #launch-stop }

A member is stopped with `SIGTERM`, then 25 seconds for its session to end in order, then `SIGKILL`
to its process group. Members are stopped at the same time. 25 seconds outlasts a member's own
[`SIGTERM` teardown bound](cli.md#mur-run-sigterm), so a member ending in order finishes before it
is killed and its `trace.jsonl` stays whole.

| The launcher ends because | It first |
|---|---|
| The entry member's process ended | Stops every peer |
| It received `SIGINT`, `SIGTERM` or `SIGHUP` | Sends `SIGTERM` to the entry member, then stops every member |
| A peer did not come up | Stops every member already started |

Each peer runs in a process group of its own; the entry member stays in the launcher's. A
`SIGKILL` of the launcher itself leaves the members running.

### What the entry member is handed { #formation-peers }

The entry member is the only member handed addresses. `MURMUR_FORMATION_PEERS` names the door of
every member the roster lets it call, as `name=url` pairs separated by single spaces, in roster
order:

```text
MURMUR_FORMATION_PEERS=coder=http://localhost:41873 reviewer=http://localhost:41874
```

| Property | Value |
|---|---|
| Pair | `<member name>=http://<host>:<port>` |
| Absent | When the entry member may call nobody. The variable is never set empty |
| Reaches | The entry member's capsule, its tools, a `transport: http` driver, its hooks, its shell commands, its native tools and a `transport: process` harness |
| Runtime-owned | `capabilities.env.allow` and `capabilities.shell.baseline_env` neither supply nor replace it |
| Delegated children | Do not receive it |
| Malformed, or set without `MURMUR_FORMATION_ID` | The launch is refused with [`E-RUN-046`](diagnostics.md#e-run-046) |

A shell reads it without a parser:

```sh
for peer in $MURMUR_FORMATION_PEERS; do
  echo "${peer%%=*} answers at ${peer#*=}"
done
```

A peer's door that requires authentication answers its public agent card to anyone, and answers
`message/send` from the entry member with `401`: no member is handed another's token.

A roster edge between two members neither of which is the entry member is admitted and both members
are launched, but no address is handed for it, and the launch prints
[`W-RUN-005`](diagnostics.md#w-run-005) once per such edge.

A process that already carries `MURMUR_FORMATION_ID` is a formation member, and refuses
`mur run --roster` with [`E-RUN-046`](diagnostics.md#e-run-046).

