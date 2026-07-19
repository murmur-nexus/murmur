# Driver fixture source

These fixture WASM files are built from the `default-artifacts` repository and copied here manually.

## Build in `default-artifacts`

```bash
cd ~/default-artifacts
cargo build --workspace --target wasm32-wasip2 --release
```

## Copy outputs into this fixture

```bash
cp ~/default-artifacts/target/wasm32-wasip2/release/murmur_driver_anthropic.wasm \
  ../anthropic/driver/murmur-driver-anthropic.wasm

cp ~/default-artifacts/target/wasm32-wasip2/release/murmur_driver_openai.wasm \
  ../openai/driver/murmur-driver-openai.wasm
```
