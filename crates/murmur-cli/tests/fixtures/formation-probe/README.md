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
`message/send` as `POST http://<name>.formation.invalid/`. It holds no token and no port: the
member's runtime resolves each virtual address to the callee's real door and presents the callee's
formation token. Its summary is one line per name, then the variable it was handed:

```text
<name> card=<status|refused:<error>> send=<status|refused:<error>>
peers=<value of MURMUR_FORMATION_PEERS, or absent>
```

`refused:<error>` is the request's failure as wasi-http reported it, for example
`refused:HttpRequestDenied` for a name the member may not call.

## Rebuild

```bash
cd src/formation-probe
cargo build --target wasm32-wasip2 --release
cp target/wasm32-wasip2/release/formation_probe_fixture.wasm ../../tool/formation-probe.wasm
```
