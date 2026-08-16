# `murmur-tool-fixture` — native tool fixture

A small host-native tool binary used by `crates/murmur-cli/tests/git_tool.rs` and
`crates/murmur-cli/tests/native_tool_packaging.rs`.

Unlike the other crates under `tests/fixtures/`, this one is **not** a
`wasm32-wasip2` component and its build output is **not** checked in. Native
tools are dispatched by `capsule_runtime::dispatch_native_tool` as ordinary
subprocesses, so the fixture has to be compiled for whichever host runs the
tests — a single committed blob could not serve linux/x86_64, linux/aarch64 and
darwin/aarch64 at once. `common::fixture_native_tool_binary()` builds it on
demand and caches the result in the workspace target directory.

Build it by hand the same way the tests do:

```bash
cargo build --release \
  --manifest-path crates/murmur-cli/tests/fixtures/native-tool/Cargo.toml \
  --target-dir target/native-tool-fixture
```

`murmur.yaml` next to this README is the artifact manifest the tests pack into
the `.mur.zip`; its `input_schema` is what the inventory → `input_schema`
mapping test asserts on, so keep the `repo` property in it.

See the module docs in `src/main.rs` for the stdin/stdout protocol.
