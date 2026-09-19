# gateway-probe-hook fixture

A test-only `on-stage` hook for `tests/credential_gateway.rs`, built against
`crates/capsule-runtime/wit/hook` `world hook`.

At `on-stage` it POSTs a fixed JSON body with a forged `Authorization: Bearer forged` header to
`<MURMUR_GATEWAY_ENDPOINT>/v1/hook`, then sends a GET to the unlisted loopback host named by its
operator `config:` block (`{"unlisted": "127.0.0.1:<port>"}`), then reports both outcomes by
POSTing one line to `<MURMUR_GATEWAY_ENDPOINT>/v1/hook-report`:

```text
gateway_env=<set|absent> first=<status|error> unlisted=<status|error> env_sha256=<hex,...>
```

`env_sha256` lists the sha256 of every environment value, so a test can show the key is not among
them without the hook being told the key. A hook holds no preopened directory by default, so the
report travels through the gateway rather than a file. Every lifecycle event returns `none`.

`murmur.yaml` is the bundled manifest the tests pack with the hook: `binding: on-stage`,
`execution_mode: blocking`, and `upstream_auth: {header: Authorization, value: "Bearer {key}"}`.

## Rebuild

```bash
cd src/gateway-probe-hook
cargo build --target wasm32-wasip2 --release
cp target/wasm32-wasip2/release/gateway_probe_hook_fixture.wasm ../../hook/gateway-probe-hook.wasm
```
