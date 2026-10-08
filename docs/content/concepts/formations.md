# Formations

## What a formation is { #what }

A formation is a group of capsules on one machine that work on one task together. Its members are
declared in a `roster.yaml` in the project directory: each is a capsule at an exact version, one
of them is the **entry member** that receives the formation's task, and the roster says which
members may call which. [`mur doctor`](../reference/cli.md#doctor-roster) checks a roster against
the installed capsules. See the [Roster Schema](../reference/roster.md).

`mur run --roster` launches a roster for one task:

1. Every member runs as its own `mur run` process, and every member carries the same
   `formation_id`, minted for this launch. Each member's trace names it.
2. The other members, the peers, start first, each counted ready only once its door answers as the
   session it reported. The entry member starts last, with the formation's task.
3. When the entry member's task ends, the formation ends.

One launch is one formation, for one task. Launching the same roster again makes a new formation
with a new id. See [Launching a formation](../reference/roster.md#launch) and the
[`mur run --roster` flags and output](../reference/cli.md#mur-run-roster).
[How to launch a formation of capsules](../how-to/launch-formation.md) scaffolds a two-member
formation, launches it, and follows one member handing the other its task.

Each member works in a directory of its own. The entry member works in the roster's project
directory, with its sessions under `<project>/.murmur/`. Each peer works in
`~/.murmur/formations/<frm_id>/<member>/`, a directory no other member can reach. A formation's
directory outlives its launch; the entry member's `trace.retain` bounds how many are kept. See
[Member directories](../reference/roster.md#member-directories) and
[Removing formation directories](../reference/roster.md#formation-retention).

## How a member gives another work { #member-calls }

A member gives another member work with the runtime-provided
[`call-member`](../reference/runtime-provided-tools.md#call-member) tool, which a member has only
when the roster lets it call another:

1. The model calls `call-member` with the member's name and the whole task.
2. The call returns as soon as the other member holds the task. A member with no room for it
   turns it away busy; the call returns at once, and the runtime keeps offering it the task.
3. The other member runs the task in its own directory.
4. Its answer comes back into the caller's same task, fenced as coming from that member, and the
   caller's model continues with it. A call that ends with no answer — the member stayed busy,
   timed out or failed — is named by the runtime as giving no answer, and the caller's model is
   told not to present an answer of its own as that member's.
5. A member that has no answer to give, because a member it called gave it none, ends its task
   with [`end-without-answer`](../reference/runtime-provided-tools.md#end-without-answer) instead
   of replying. Its caller is then told, in the runtime's own words, that no answer came from it,
   and which member further down gave none. A member that replies anyway after a missing answer
   has its reply delivered with a line saying which members it answered without.

A second call to a member is refused, with nothing sent, until the first call's answer has been
delivered. A call carries text only, so whatever the other member needs goes in the task text. See
[Giving a member work](../reference/roster.md#member-calls).

## Reachability: three layers, each closed by default { #reachability }

A call from one member to another succeeds only when all three layers allow it:

| Layer | Declared by | Closed until |
|---|---|---|
| The roster edge | `roster.yaml` | A [`reachability`](../reference/roster.md#reachability) rule lets the caller call the member; only then does the caller have `call-member`, and only then does the launcher issue a credential for that edge |
| The callee's consent | The callee's `murmur.yaml` | The callee declares [`exports.peer_tasks.accept: true`](../reference/manifest.md#field-exports-peer-tasks), and its door [lets in only a credential issued for it](../reference/roster.md#enforcement-door) |
| The caller's egress | The caller's `murmur.yaml` | The caller's own [`capabilities.network.allow`](../reference/roster.md#member-calls) lists `localhost` |

The entry member is never called: it runs the formation's one task from launch until the formation
ends, and accepts no other. A rule that lists it in `to` is refused at admission with
[`E-ROS-008`](../reference/diagnostics.md#e-ros-008). To have a member report to the entry member,
let the entry member call it; the answer comes back into the entry member's own task. See
[The entry member](../reference/roster.md#entry-member).

The launcher issues every credential, and the runtime attaches a member's credential to its calls,
so no model ever sees it. A formation credential reaches exactly two methods on the door it was
issued for: `message/send`, which starts a task, and `tasks/get`, which reads the state of a task
the calling member started. It reaches nothing that cancels or stops a task. See
[What a door answers](../reference/roster.md#enforcement-door).

## How a formation ends { #lifeline }

A formation's members end with it, however it ends. Each member holds one end of a lifeline whose
other end only the launcher holds; when the launcher ends the formation, or dies — even by
`SIGKILL` — every member's lifeline closes. A member whose lifeline closes records
`formation_ended` in its trace, cancels the work it had in flight, since nothing remains to receive
the result, and ends its session in order. See [How a formation ends](../reference/roster.md#launch-stop).

A sub-capsule a member delegates to is not a member: it shares the formation's id and nothing
else. It ends with the member that started it, through a lifeline of its own whose other end only
that member's process holds — see
[A child ends with its parent](../reference/roost-api.md#spawner-lifeline). When the formation
ends, its members end, and their delegated children end with them.

## The A2A door { #door }

Every capsule with an agent session serves a door: a small HTTP endpoint exposing an A2A v1.0
[Agent Card](../reference/agent-card.md) (`/.well-known/agent-card.json`) and a JSON-RPC 2.0 task
interface, so other capsules and standard A2A clients can discover the capsule and hand it a task.
Whether an incoming message is accepted depends on the capsule's `lifecycle.task_acceptance`
setting (see [Capsule lifecycle](session-loop.md#capsule-lifecycle)).

A formation member is reached at its door like any capsule: `call-member` sends the task there. To
wire two capsules together by hand, with a pinned port and `curl`, see
[Connect two capsules with A2A messaging](../how-to/capsules-a2a-messaging.md).

## Not yet supported { #not-yet }

- **Members on more than one machine.** Every member runs on the launcher's machine, and every
  door is served on loopback. See [Launching a formation](../reference/roster.md#launch).
- **Formation calls from native processes.** Shell commands, native tools and a
  `transport: process` harness are handed no `MURMUR_FORMATION_PEERS` and cannot reach a member's
  virtual address — see
  [What a member's components are handed](../reference/roster.md#formation-peers). A
  `transport: process` harness still calls members with `call-member`, which the runtime serves to
  it through its loopback tool server; the answer reaches it when the runtime resumes the
  harness's session — see [How the answer arrives](../reference/runtime-provided-tools.md#call-member-answer).
- **Credential rotation.** A formation credential has no expiry, rotation or revocation; the
  credentials end with the formation's one task — see
  [Why a formation credential never expires](../reference/roster.md#enforcement-lifetime).
- **Cancelling a member's task.** No formation credential reaches `tasks/cancel`. A call that times
  out or is abandoned leaves the member's task running until it finishes or the formation ends —
  see [What a door answers](../reference/roster.md#enforcement-door).
- **Passing files between members.** A peer's directory is in no other member's reach, so content
  goes in the task text — see [Member directories](../reference/roster.md#member-directories).
- **Resuming a formation.** `mur run --roster` refuses `--resume`, and a member's session resumed
  by hand belongs to no formation — see
  [Resuming a member's session](../reference/roster.md#launch-resume).

## Next { #next }

- [How to launch a formation of capsules](../how-to/launch-formation.md) walks through the
  scaffold, the launch and the trace of a real run.
- The [Roster Schema](../reference/roster.md) lists every `roster.yaml` field, the admission
  checks, and how a formation is launched and enforced.
