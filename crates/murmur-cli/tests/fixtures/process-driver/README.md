# process-driver fixture

A test-only process driver for `tests/process_driver.rs` and the process driver runner's tests. It
exports `murmur:driver/process@0.1.0` from the `process-driver` world and imports nothing but WASI.
It drives a harness that does not exist, so every answer below is fixed.

The committed `tool/process-driver.wasm` is built from `src/process-driver`.

## What each call returns

### `describe()`

| Field | Value |
| --- | --- |
| `harness` | `fixture-harness` |
| `binary` | `fixture-cli` |
| `version-args` | `["--version"]` |
| `tested-versions` | `["1.0.0"]` |
| `interrupt` | `stdin-message` |
| `required-env` | `["HOME", "FIXTURE_HARNESS_PROFILE"]` |
| `streams-text` | `true` |

### `launch(request)`

`err("empty task")` when `task` is empty. Otherwise `ok` with:

| Field | Value |
| --- | --- |
| `args` | `["--session", <session.id>, "--mode", "new" or "resume"]`, then `["--model", <model>]` when `model` is set |
| `env-set` | `[("FIXTURE_FILES", "{files_dir}")]` |
| `files` | one file, `config.json`, holding `config`, or `{}` when `config` is `none` |
| `stdin` | the task's bytes followed by `\n` |
| `keep-stdin-open` | `true` |
| `interrupt-stdin` | `interrupt\n` |

### `parse(lines)`

One event per line, read from the first word:

| Line | Event |
| --- | --- |
| `started <id>` | `session-started { id: <id>, auth: "subscription", model: none }` |
| `delta <text>` | `text-delta(<text>)` |
| `text <text>` | `text(<text>)` |
| `end <result>` | `turn-end(<result>)` |
| `fail <message>` | `turn-failed { kind: other, message: <message> }` |
| anything else | `note(<line>)` |

A word with nothing after it is "anything else".

### `classify-exit(exit)`

| Exit | Event |
| --- | --- |
| `interrupted` is true | `turn-failed { kind: canceled, message: "interrupted" }` |
| otherwise, `code` is `0` | `turn-end("")` |
| otherwise | `turn-failed { kind: harness-error, message: <stderr-tail> }` |

## Rebuild

```bash
cd src/process-driver
cargo build --target wasm32-wasip2 --release
cp target/wasm32-wasip2/release/process_driver_fixture.wasm ../../tool/process-driver.wasm
```
