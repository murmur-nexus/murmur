# Graduation fixture source crates

Recompile the prebuilt fixture components from this directory:

```bash
cargo build --target wasm32-wasip2 --release --manifest-path jsonl-line-count/Cargo.toml
cargo build --target wasm32-wasip2 --release --manifest-path graduation-capsule/Cargo.toml
```

Copy the outputs into the committed fixture paths:

```bash
cp jsonl-line-count/target/wasm32-wasip2/release/jsonl_line_count_fixture.wasm ../tool/jsonl-line-count.wasm
cp graduation-capsule/target/wasm32-wasip2/release/graduation_capsule_fixture.wasm ../capsule/capsule.wasm
```
