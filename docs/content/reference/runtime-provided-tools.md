# Runtime-provided tools

A runtime-provided tool is an entry in the agent's tool inventory with no artifact behind it. The
runtime writes its `murmur.yaml` into `workdir/tools/<name>/` while the session is staged, and
answers the call itself.

The model sees no difference. A runtime-provided tool has the same manifest shape, appears in the
same inventory, and is called the same way as a tool artifact. What differs is where it comes from
and what decides whether it may run.

## The four

Each one appears only when the manifest declaration in the second column is present.

| Tool | Gated on | Does |
|---|---|---|
| One per shell binary | [`capabilities.shell.allow`](manifest.md#shell-allow) | Runs that binary as a subprocess in the accessible workdir |
| `share-file` | [`exports.peer_files`](manifest.md#field-exports-peer-files) | Mints a [peer-file handle](resource-plane.md#peer-plane) for one file under the declared export root |
| `fetch-peer-file` | [`capabilities.peer_fetch`](manifest.md#field-peer-fetch) | Redeems a handle a peer sent and stores the file in this capsule's workdir |
| `delegate-task` | [`capabilities.spawn.allow`](manifest.md#field-capabilities) | Hands one task to one sub-capsule and waits for its answer, up to [`lifecycle.delegation_deadline_secs`](manifest.md#lifecycle-delegation-deadline-secs) — see [The delegation tool](roost-api.md#the-delegation-tool) |

Every one of their manifests carries `version: 0.0.0`, `runtime: tool` and
`implementation: native`. Nothing was fetched, so nothing is version-pinned, nothing is
hash-verified, and no entry appears for them in `murmur.lock` or in `mur list`.

## The grant is the tool's existence

`share-file`, `fetch-peer-file` and `delegate-task` are answered before the tool allowlist is
consulted. The allowlist governs which tool *artifacts* may run; it has no say over these three.

**The gate is whether the manifest file was written at all.** With the grant absent, staging writes
nothing under `workdir/tools/<name>/`, so:

- The tool is absent from the inventory the model is sent.
- It is absent from `session_start`'s `tools_declared` in [`trace.jsonl`](observability-schemas.md).
- A call naming it anyway is refused with a message naming the declaration that is missing.

So `capabilities.spawn.allow` decides whether `delegate-task` exists, rather than whether a call to
it succeeds. The same holds for the two peer-handoff tools and their grants.

Shell binaries are gated the same way — one manifest per name in `capabilities.shell.allow` — and
are additionally checked against that list again at dispatch.

## Reserved names

`share-file`, `fetch-peer-file` and `delegate-task` are reserved. A capsule declaring an artifact
under one of them is refused at staging, before any artifact is resolved, pulled or hash-verified,
with [`E-CAP-013`](diagnostics.md#e-cap-013). The same refusal covers an in-session
`manage.pull()` of that name.

A name is reserved whether or not the capsule declares the grant that would provide the tool, so
adding or removing a capability never changes whether a manifest is accepted.

Shell binary names are not reserved. They come from `capabilities.shell.allow`, which is the
operator's own list, so there is no fixed set to reserve — and a capsule may declare both an
artifact named `bash` and `bash` in its shell allowlist. Staging writes the artifact's manifest
first and the shell manifest yields to it, so the inventory describes the artifact.

Dispatch resolves such a pair by [precedence](../concepts/tools.md#tool-dispatch): a native
artifact answers ahead of the shell binary, and the shell binary answers ahead of a WASM artifact
of the same name.
