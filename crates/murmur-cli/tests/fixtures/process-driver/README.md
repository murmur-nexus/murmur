# process-driver fixture

A test-only process driver, and a fake harness for it to drive, for `tests/process_driver.rs`,
`tests/process_runner.rs` and the process driver runner's own tests. The driver exports
`murmur:driver/process@0.1.0` from the `process-driver` world and imports nothing but WASI.

The committed `tool/process-driver.wasm` is built from `src/process-driver`. `fake-harness` is a
committed executable bash script; a test copies it and `chmod 755`es its copy.

## The driver

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

`err("empty task")` when `task` is empty, and `err("launch refused by config")` when `config`
contains `refuse-launch`. Otherwise `ok` with:

| Field | Value |
| --- | --- |
| `args` | `["--session", <session.id>, "--mode", "new" or "resume"]`, then `["--model", <model>]` when `model` is set |
| `env-set` | `[("FIXTURE_FILES", "{files_dir}")]`, then the three bridge variables below when `bridge` is set |
| `files` | one file, `config.json`, holding `config`, or `{}` when `config` is `none` |
| `stdin` | the task's bytes followed by `\n` |
| `keep-stdin-open` | `true` |
| `interrupt-stdin` | `interrupt\n` |

When `bridge` is set, `env-set` also carries:

| Variable | Value |
| --- | --- |
| `FIXTURE_BRIDGE_URL` | `bridge.url` |
| `FIXTURE_BRIDGE_TOKEN` | `bridge.bearer-token` |
| `FIXTURE_BRIDGE_TOOLS` | `<server-name>__<tool>` for each bare name in `bridge.tool-names`, comma-joined |

The driver, not the runtime, builds those qualified names, and remembers the `<server-name>__`
prefix so `parse` can strip it off a `tool-call` again. The runtime never spells a harness tool
name in either direction.

### `parse(lines)`

One event per line, read from the first word:

| Line | Event |
| --- | --- |
| `started <id> [<auth>]` | `session-started { id, auth, model: none }`; `auth` defaults to `subscription` |
| `delta <text>` | `text-delta(<text>)` |
| `text <text>` | `text(<text>)` |
| `thinking <text>` | `thinking(<text>)` |
| `tool <id> <name> <json>` | `tool-call { id, name with the bridge prefix stripped, input: <json> }` |
| `result <id> ok\|error <output>` | `tool-result { id, output, is-error }` |
| `retry <n> <reason>` | `retry { attempt: <n>, reason }` |
| `end <result>` | `turn-end(<result>)` |
| `fail <kind> <message>` | `turn-failed { kind, message }`, `kind` one of `auth`, `quota`, `max-turns`, `canceled`, `harness-error`, `other` |
| anything else | `note(<line>)` |

A word with nothing after it, an unknown failure kind, a `retry` whose attempt is not a number, and
a `tool` / `result` line with too few fields are all "anything else".

### `classify-exit(exit)`

| Exit | Event |
| --- | --- |
| `interrupted` is true | `turn-failed { kind: canceled, message: "interrupted" }` |
| otherwise, `code` is `0` | `turn-end("")` |
| otherwise | `turn-failed { kind: harness-error, message: <stderr-tail> }` |

## The fake harness

`fake-harness` answers `--version` with `fixture-harness 1.0.0` (or `fixture-harness 0.9.0` under
the `old-version` profile) and exits. Otherwise it reads one task line from stdin and then follows
`$FIXTURE_HARNESS_PROFILE`:

| Profile | What it does |
| --- | --- |
| `happy` | One `text` and one `end`, result `HAPPY-RESULT` |
| `tool` | One `tool` call, one bridge request, the response as the `result`, the `text` and the `end` |
| `fail-auth`, `fail-quota`, `fail-max-turns`, `fail-canceled`, `fail-harness-error`, `fail-other` | One `fail <kind> the harness says so` |
| `exit-0` | Output with no terminal event, then exit `0` |
| `exit-3` | `boom-on-stderr` on stderr, then exit `3` |
| `silent` | Says nothing and sleeps, so the inactivity window kills it |
| `chatty` | Six lines half a second apart, then `end CHATTY-RESULT` |
| `bridge-busy` | Six bridge requests half a second apart with nothing on stdout, then `end BRIDGE-BUSY-RESULT` |
| `env` | Notes its environment's variable names and its files directory's mode, then `end ENV-RESULT` |
| `old-version` | Reports an untested version to `--version`, then `end OLD-VERSION-RESULT` |
| `turns` | Two turns closed by tool results, then a third opened by `text`, then sleeps |
| `thinking` | `thinking` and `delta` events around one `text`, then `end THINKING-RESULT` |
| `api-key` | `started fixture-session-1 api-key`, then `end API-KEY-RESULT` |
| `linger` | `end LINGER`, then ignores stdin closing and sleeps, so the exit grace kills it |

`silent`, `turns` and `linger` write their pid to `harness.pid` in their working directory, which
is how a test proves the harness is dead.

The bridge request is made with bash's own `/dev/tcp` — no `curl` — and the harness's environment
is only what the capsule declared plus the driver's `env-set`, so a test that wants `sleep`, `ls`
or `env` to resolve has to declare `PATH`.

## Rebuild

```bash
cd src/process-driver
cargo build --target wasm32-wasip2 --release
cp target/wasm32-wasip2/release/process_driver_fixture.wasm ../../tool/process-driver.wasm
```
