# Formations

!!! note "Partially implemented"

    Formations are an area of Murmur that is only partly built out. Agent Card discovery and
    A2A messaging between capsules, described below, are implemented.

## Agent Card & A2A messaging

While an agent session is active, the runtime serves a small HTTP endpoint exposing an A2A v1.0
[Agent Card](../reference/agent-card.md) (`/.well-known/agent-card.json`) and a JSON-RPC 2.0 task
interface, so other capsules and standard A2A clients can discover the capsule and hand it a task.
Whether an incoming message is accepted depends on the capsule's `lifecycle.task_acceptance`
setting (see [Capsule lifecycle](session-loop.md#capsule-lifecycle)). See [Connect two capsules
with A2A messaging](../how-to/capsules-a2a-messaging.md) for the full protocol and examples.

## Rosters

A formation is declared in a `roster.yaml` beside the project's `murmur.yaml`: its members, each a
capsule at an exact version, the entry member that receives the formation's task, and which
members may call which. [`mur doctor`](../reference/cli.md#doctor-roster) checks a roster against
the installed capsules. See the [Roster Schema](../reference/roster.md).

`mur run --roster` launches a roster for one task. Every member runs as its own `mur run` process
under one formation id: the peers start first, each counted ready only once its door answers as the
session it reported, and the entry member starts last. When the entry member's task ends, every
member is stopped. See [Launching a formation](../reference/roster.md#launch) and the
[`mur run --roster` flags and output](../reference/cli.md#mur-run-roster).

The roster is enforced, not advisory. The launcher is the one authority that issues credentials: it
signs a token for each edge the roster allows, hands each member only the tokens for the members it
may call, and keeps the signing key in its own memory. A member's components call another member by
name; the runtime attaches the token, so no model ever sees it. Every member's door lets in only a
token issued for that door, answers any other caller `401` or `403`, and the whole set of
credentials ends with the formation. See
[How reachability is enforced](../reference/roster.md#enforcement).
