# escape-conformance-driver

The process driver the escape-conformance gate runs every case through. It exports
`murmur:driver/process@0.3.0` from the `process-driver` world, imports nothing but WASI, and
drives one harness: the gate's own `probe-driver` binary
(`../src/probe_driver.rs`), which makes predetermined tool calls over the capsule's tool bridge.

The gate embeds `escape_conformance_driver.wasm` and `murmur.yaml` from this directory, packs them
with `mur build` at startup, and installs the artifact into each case's project store with
`mur install`. Nothing is fetched from a registry, and the driver is never published.

## The driver

### `describe()`

| Field | Value |
| --- | --- |
| `harness` | `escape-conformance-probe` |
| `binary` | `probe-driver` (every generated manifest overrides it with `inference.command`) |
| `version-args` | `["--version"]` |
| `tested-versions` | `["1.0.0"]` — `PROBE_HARNESS_VERSION` |
| `interrupt` | `unsupported` |
| `required-env` | `[]` |
| `streams-text` | `false` |
| `reports-usage` | `true` |

### `launch(request)`

`err("the capsule exposes no tools, so the probe has nothing to call")` when `bridge` is `none`,
and `err("inference.driver.config is required: it names the case")` when `config` is `none`.
Otherwise `ok` with:

| Field | Value |
| --- | --- |
| `args` | `[]` |
| `env-set` | `MURMUR_EC_BRIDGE_URL` = `bridge.url`, `MURMUR_EC_BRIDGE_TOKEN` = `bridge.bearer-token`, `MURMUR_EC_PROBE` = `config` verbatim |
| `files` | `[]` |
| `stdin` | `none` |
| `keep-stdin-open` | `false` |
| `interrupt-stdin` | `none` |

`MURMUR_EC_PROBE` is the manifest's `inference.driver.config` as JSON:

| Key | Required | Meaning |
| --- | --- | --- |
| `tool` | yes | Bridge tool to call |
| `script` | yes | The tool's `command` argument |
| `script2` | no | A second call's `command` |
| `case` | yes | Case id, echoed into the summary |
| `log` | yes | Absolute path `probe-driver` writes its summary line to |

The driver passes the object through without reading it. `probe-driver` reads it, and reports a
missing required key as a `fail` line.

### `parse(lines)`

One event per line, read from the first word:

| Line | Event |
| --- | --- |
| `tool <id> <name> <json>` | `tool-call { id, name, input: <json> }`; bridge names are bare, so nothing is stripped |
| `result <id> ok\|error [<text>]` | `tool-result { id, output: <text>, is-error }`; no text is empty output |
| `usage in=<n> out=<n>` | `usage { input: <n>, output: <n> }`, every other member `none` |
| `end [<summary>]` | `turn-end(<summary>)` |
| `fail [<message>]` | `turn-failed { harness-error, <message> }` |
| anything else, or one of the above with too few fields, an unknown status or a non-numeric count | `note(<line>)` |

### `classify-exit(exit)`

| Exit | Event |
| --- | --- |
| `interrupted` | `turn-failed { canceled, "interrupted" }` |
| `code == 0` | `turn-failed { harness-error, "probe-driver exited without an end line" }` |
| otherwise | `turn-failed { harness-error, <stderr-tail> }` |

A probe that exits without an `end` line never reads as a finished run.

## Rebuilding

From this directory:

```bash
cargo build --target wasm32-wasip2 --release
cp target/wasm32-wasip2/release/escape_conformance_driver.wasm escape_conformance_driver.wasm
cargo test
```

`cargo test` runs the line reader's unit tests on the host. Rebuild after any change to
`src/lib.rs`, and after any version bump of `crates/capsule-runtime/wit/process-driver/`: until
then `committed_driver_exports_the_process_interface_this_runtime_accepts` in the gate package
fails, and the gate refuses to start. A change to the line protocol in
`../src/harness_protocol.rs` that this driver does not read fails
`committed_driver_reads_every_line_the_probe_writes`.
