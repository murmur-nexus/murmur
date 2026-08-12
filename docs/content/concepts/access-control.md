# Access Control

A capsule's `capabilities:` block is where access control is declared — what the capsule and
its artifacts may reach.

Every grant is declared: filesystem and network access start closed, and shell access is
allowlist-based. The same applies to hooks — a hook's grant comes from its entry in the
capsule's manifest and starts empty.

## Per-tool narrowing

By default every WASM tool, and the inference driver, runs on the
capsule-wide `capabilities:` ceiling: the same allow-list and the same preopened workdir. A
`runtime: tool` or `runtime: driver` entry in the capsule's own `murmur.yaml` may optionally
declare its own `capabilities:` block to run *below* that ceiling — a narrower host list, a
subdirectory instead of the whole workdir:

```yaml
# in the capsule's own murmur.yaml
artifacts:
  - name: murmur-tool-fetch
    version: 1.0.0
    runtime: tool
    capabilities:
      network:
        allow: [https://api.example.com]
      filesystem:
        scope: cache
```

This uses the same key and vocabulary as hooks, with the opposite starting point: a hook with
no block gets nothing, a tool with no block keeps the full ceiling. A per-artifact block can
only subtract — an entry naming a host the capsule itself may not reach is dropped, with a
[`W-SEC-007`](../reference/security-warnings.md#w-sec-007) warning. Like a hook's, the grant is
read only from the capsule operator's entry, never from the tool's own bundled manifest. The
artifact named by `inference.driver.artifact` narrows the same way. See [Tool and driver
capabilities](../reference/manifest-schema.md#tool-capabilities) for the full rules.

## Hook capabilities { #hook-capabilities }

A hook runs default-deny, and only the capsule operator can widen it. A hook artifact's own
`murmur.yaml` declares the hook's behavioral contract and nothing else. Capability grants are
read exclusively from the artifact entry in the **capsule's** `murmur.yaml`, so a hook pulled
from a registry can never grant itself anything:

```yaml
# in the capsule's own murmur.yaml
artifacts:
  - name: murmur-hook-telemetry
    version: 1.0.0
    runtime: hook
    capabilities:
      network:
        allow: [https://telemetry.example.com]
      filesystem:
        scope: hook-state
```

With no `capabilities:` block a hook has no network and no directory at all — every outbound
request is denied, and it cannot read or write any file. A granted hook reaches its declared
hosts through the same allow-list gate a capsule's or tool's outbound HTTP goes through, and
sees exactly one directory, `<workdir>/<scope>`, as its current directory. Every hook gets its
grant the same way, whatever its binding or execution mode. See
[Hook capabilities](../reference/manifest-schema.md#hook-capabilities) for the full rules.

## Untrusted publisher text

Tool and skill descriptions rendered into [`MURMUR.md`](session-loop.md#murmurmd) are
publisher-controlled, untrusted text: the runtime
sanitizes them (collapsing newlines, stripping control characters, truncating length) before
rendering, and `MURMUR.md` states explicitly that this file is machine-generated inventory
data, never instructions.
