# gateway-probe fixture

A test-only WASM tool and the script capsule that drives it, for `tests/credential_gateway.rs`.

`tool/gateway-probe.wasm` is built against `crates/capsule-runtime/wit/guest` `world tool`. It
reads `MURMUR_GATEWAY_ENDPOINT` (falling back to the bare gateway authority `http://127.0.0.1:9`
when the variable is absent), POSTs a fixed JSON body with a forged `Authorization: Bearer forged`
header to `<endpoint>/v1/probe`, and returns one summary line:

```text
gateway_env=<set|absent> endpoint=<url> marker_in_env=<true|false> status=<code> sha256=<hex>
```

with `error=<what>` in place of `status` and `sha256` when the request did not complete. It is
told the key marker, hex-encoded, through its tool input — bare, or as the `marker_hex` field of a
JSON object — never through its environment, and reports only whether any environment value
contains it.

`murmur.yaml` is the bundled manifest the tests pack with the tool. It declares
`inference_auth: {header: Authorization, value: "Bearer {key}"}`.

`capsule/capsule-gateway-probe.wasm` is built against `world capsule`. It passes the task text
(`task.md`, the hex-encoded marker) as input to `gateway-probe` and `gateway-probe-b` and writes
their summaries to `out/result.txt` and `out/result-b.txt`.

## Rebuild

```bash
cd src/gateway-probe
cargo build --target wasm32-wasip2 --release
cp target/wasm32-wasip2/release/gateway_probe_fixture.wasm ../../tool/gateway-probe.wasm

cd ../capsule-gateway-probe
cargo build --target wasm32-wasip2 --release
cp target/wasm32-wasip2/release/capsule_gateway_probe_fixture.wasm ../../capsule/capsule-gateway-probe.wasm
```
