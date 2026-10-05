# Formations

## Agent Card & A2A messaging

While an agent session is active, the runtime serves a small HTTP endpoint exposing an A2A v1.0
[Agent Card](../reference/agent-card.md) (`/.well-known/agent-card.json`) and a JSON-RPC 2.0 task
interface, so other capsules and standard A2A clients can discover the capsule and hand it a task.
Whether an incoming message is accepted depends on the capsule's `lifecycle.task_acceptance`
setting (see [Capsule lifecycle](session-loop.md#capsule-lifecycle)). See [Connect two capsules
with A2A messaging](../how-to/capsules-a2a-messaging.md) for the full protocol and examples.

## Rosters

A formation is declared in a `roster.yaml` in its project directory: its members, each a
capsule at an exact version, the entry member that receives the formation's task, and which
members may call which. [`mur doctor`](../reference/cli.md#doctor-roster) checks a roster against
the installed capsules. See the [Roster Schema](../reference/roster.md).

`mur run --roster` launches a roster for one task. Every member runs as its own `mur run` process
under one formation id: the peers start first, each counted ready only once its door answers as the
session it reported, and the entry member starts last. When the entry member's task ends, every
member is stopped. See [Launching a formation](../reference/roster.md#launch) and the
[`mur run --roster` flags and output](../reference/cli.md#mur-run-roster).

The roster is enforced. The launcher is the one authority that issues credentials: it
signs a token for each edge the roster allows, hands each member only the tokens for the members it
may call, and keeps the signing key in its own memory. A member's components call another member by
name; the runtime attaches the token, so no model ever sees it. Every member's door lets in only a
token issued for that door, answers any other caller `401` or `403`, and the whole set of
credentials ends with the formation. See
[How reachability is enforced](../reference/roster.md#enforcement).

A member gives another member work with the runtime-provided
[`call-member`](../reference/runtime-provided-tools.md#call-member) tool, which a member has only
when the roster lets it call another. The call returns as soon as the other member holds the task.
The other member runs the task in its own directory, and its answer comes back into the caller's
same task, marked as coming from that member. Members share no files, so whatever the other member
needs goes in the task text. The roster grants the name and the credential, not the network: the
caller's own `capabilities.network.allow` must list `localhost`. See
[Giving a member work](../reference/roster.md#member-calls).

The entry member is never called, because it runs the formation's own task from launch to end;
members report to it by answering its calls. See [The entry member](../reference/roster.md#entry-member).

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
