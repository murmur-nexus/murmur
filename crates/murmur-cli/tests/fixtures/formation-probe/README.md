# formation-probe fixture

A test-only WASM tool for `tests/formation_launch.rs`, run by a formation's entry member to show
which peers it was handed and whether each one's door answers.

`tool/formation-probe.wasm` is built against `crates/capsule-runtime/wit/guest` `world tool`. It
reads `MURMUR_FORMATION_PEERS` and, for each `name=url` pair, sends
`GET <url>/.well-known/agent-card.json` and one unauthenticated JSON-RPC `message/send` as
`POST <url>/`. It returns one line per peer, in the order the variable names them:

```text
<name> card=<status> card_name=<card's "name"> send=<status>
```

with `card_error=<what>` or `send_error=<what>` in place of a status when a request does not
complete, and the single line `peers=absent` when the variable is not set. It holds no token, so an
authenticated peer answers its public card with `200` and `message/send` with `401`.

## Rebuild

```bash
cd src/formation-probe
cargo build --target wasm32-wasip2 --release
cp target/wasm32-wasip2/release/formation_probe_fixture.wasm ../../tool/formation-probe.wasm
```
