# `tool-fixture` — native tool fixture

A small host-native tool binary used by `crates/murmur-cli/tests/git_tool.rs`,
`crates/murmur-cli/tests/native_tool_packaging.rs` and
`crates/murmur-cli/tests/native_tool_confinement.rs`. Its probe operations
(`read`, `write`, `connect`, `unix_socket`, `exec`, `netns`, `rlimit_nofile`,
`open_until_refused`) are what the confinement tests drive to check what the
sandbox refuses.

Unlike the other crates under `tests/fixtures/`, this one is **not** a
`wasm32-wasip2` component and its build output is **not** checked in. Native
tools are dispatched by `capsule_runtime::dispatch_native_tool` as ordinary
subprocesses, so the fixture has to be compiled for whichever host runs the
tests — a single committed blob could not serve linux/x86_64, linux/aarch64 and
darwin/aarch64 at once. `common::fixture_native_tool_binary()` runs an
incremental `cargo build` of it once per test process, into the workspace target
directory, so an edit here is always picked up and an unchanged crate costs only
cargo's freshness check.

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
