# Formations

!!! note "Partially implemented"

    Formations are an area of Murmur that is only partly built out. Agent Card discovery and
    A2A messaging between capsules, described below, are live today.

## Agent Card & A2A messaging

While an agent session is active, the runtime serves a small HTTP endpoint exposing an Agent
Card (`/.well-known/agent-card.json`) and a JSON-RPC 2.0 task interface, so other capsules or
agents can discover the capsule and hand it a task. An incoming message reserves an active
task slot (`Empty` → `Running` → `Done`); acceptance depends on the capsule's
`lifecycle.task_acceptance` setting (see [Capsule
lifecycle](session-loop.md#capsule-lifecycle)). See [Connect two capsules with A2A
messaging](../how-to/capsules-a2a-messaging.md) for the full protocol and examples.
