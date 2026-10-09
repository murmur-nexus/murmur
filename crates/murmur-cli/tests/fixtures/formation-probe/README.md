# formation-probe fixture

A test-only WASM tool for `tests/formation_launch.rs`, run by a formation member to show which
callees its runtime handed it and whether each one answers through the runtime's formation egress.

`tool/formation-probe.wasm` is built against `crates/capsule-runtime/wit/guest` `world tool`. Its
input is optional:

```json
{"names": ["coder", "reviewer", "nosuch"]}
```

Without `names`, it probes the names `MURMUR_FORMATION_PEERS` lists. For each name, in order, it
sends `GET http://<name>.formation.invalid/.well-known/agent-card.json` and one JSON-RPC
`SendMessage` as `POST http://<name>.formation.invalid/`, each with `A2A-Version: 1.0`. It holds no
token and no port: the member's runtime resolves each virtual address to the callee's real door and
presents the callee's formation token. Its summary is one line per name, then the variable it was
handed:

```text
<name> card=<status|refused:<error>> send=<status|status:<code>|status:-|refused:<error>>
peers=<value of MURMUR_FORMATION_PEERS, or absent>
```

| `send=` | Meaning |
|---|---|
| `<status>` | The body carried a JSON-RPC `result`: the callee served the `SendMessage`. |
| `<status>:<code>` | The body carried a JSON-RPC `error` with that code, such as `200:-32009`. |
| `<status>:-` | The body carried neither, such as a `403` refusal. |
| `refused:<error>` | The request's failure as wasi-http reported it, for example `refused:ErrorCode::HttpRequestDenied` for a name the member may not call. |

## Rebuild

From the repository root:

```bash
scripts/rebuild-components.sh formation-probe
```
