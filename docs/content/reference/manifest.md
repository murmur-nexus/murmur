# Manifest Schema

`murmur.yaml` serves two roles, each with its own set of fields. An artifact's own manifest
declares its identity and what `mur build` packages. A capsule's manifest declares what `mur run`
launches: the artifacts it installs, the capabilities it grants, and how it reaches a model.

---

## Artifact manifest { #artifact-manifest }

The manifest that ships inside a `.mur.zip`, read by `mur build` and `mur publish`.

| Field | Type | Required | Notes |
|---|---|---:|---|
| `name` | string | yes | The artifact's bare name, without a scope. Lowercase letters, digits and inner hyphens (`[a-z0-9-]`, no leading or trailing `-`), non-empty, at most 100 characters. Anything else, a `/` included, fails the build with [`E-BLD-001`](diagnostics.md#e-bld-001). The artifact installs as `<scope>/<name>` — see [Artifact names](../concepts/registry.md#artifact-names). |
| `version` | string | yes | `latest`, `stable` and `edge` are reserved and cannot be published (`E-REG-004`). |
| `description` | string | no | What the artifact does. [`mur index`](cli.md#mur-index) records its first sentence as the artifact's description in the [artifact index](artifact-index.md). |
| `runtime` | string | yes | The artifact's role. Two values change how it is packaged when `execution` is absent: `skill` packages as `static`, and `tool` with `implementation: native` packages as `native`. Everything else packages as `wasm`. |
| `implementation` | `wasm \| native` | no | How a `runtime: tool` artifact is implemented. Default: `wasm`. |
| `execution` | `wasm \| native \| static` | no | Declares the registry packaging type directly. When set it is authoritative for `mur publish` and overrides the derivation from `runtime` and `implementation`. Case-insensitive. An unrecognized value is a parse error. |
| `requires_files` | list<string> | no | Companion files that must sit beside `murmur.yaml` — and the complete list of what `mur build` packages besides the manifest itself. Paths are relative to the source directory and may be nested (`assets/logo.png`). Default: `["skill.md"]` for `runtime: skill`, empty for every other role; an explicit value, including `[]`, always overrides that default. A missing file fails the build with `E-IO-003`, naming the first missing entry. Entries must be plain relative paths to real files: absolute paths, `..` components and symlinks are rejected with [`E-BLD-002`](diagnostics.md#e-bld-002). An artifact with a compiled payload must declare it here, or the built `.mur.zip` contains nothing but `murmur.yaml` — for a wasm artifact that is [`E-BLD-003`](diagnostics.md#e-bld-003). |
| `input_schema` | string (JSON) or map | no | A `runtime: tool` artifact's parameters, as a JSON Schema object. Offered to the model as the tool's input; every name in its top-level `required` must be present in a call's input or the call is refused before the tool runs. Default: an object with no properties, and no field required. `input` is an accepted older spelling. See [`input_schema`](#input-schema). |
| `upstream_auth` | map | no | How the artifact's upstream takes its key: `header` and a `value` template with `{key}` exactly once. Read only when the capsule's entry for this artifact declares [`gateway:`](#artifact-gateway); absent or malformed then refuses the launch with [`E-RUN-025`](diagnostics.md#e-run-025). See [`upstream_auth:` block](#upstream-auth). |
| `models` | map | no | `runtime: driver` only. Each model id the driver serves, with its `max_output_tokens`, `context_window` and optionally `max_input_tokens`. Default: empty. A capsule on the driver takes its output cap, context window and input ceiling from here. See [`models:` block](#driver-models). |

A hook artifact's own manifest carries three more fields — see
[Hook contract fields](#hook-contract-fields).

```yaml
name: my-tool
version: "0.1.0"
runtime: tool
execution: static
requires_files:
  - config.json
```

### `upstream_auth:` block { #upstream-auth }

How the artifact's upstream takes its key. Any artifact whose capsule entry may declare
[`gateway:`](#artifact-gateway) — a tool, hook or driver — declares this block in its own
`murmur.yaml`. The runtime renders the header from the entry's `gateway.api_key` and attaches it to
each request the artifact sends to `MURMUR_GATEWAY_ENDPOINT` (for the inference driver, also
`MURMUR_INFERENCE_ENDPOINT`). The artifact itself never receives the key.

A web search tool whose upstream takes a bearer token:

```yaml
name: my-web-search
version: "0.1.0"
runtime: tool
requires_files:
  - tool.wasm
upstream_auth:
  header: Authorization
  value: "Bearer {key}"
```

| Field | Type | Required | Notes |
|---|---|---:|---|
| `upstream_auth.header` | string | yes | A valid HTTP header name. Any header of the same name the artifact sets, in any case, is replaced. Cannot be `Host`, `Origin`, `Referer`, `Cookie`, `Connection`, `Content-Length`, `Content-Type`, `Content-Encoding`, `Expect`, `Keep-Alive`, `TE`, `Trailer`, `Transfer-Encoding` or `Upgrade` |
| `upstream_auth.value` | string | yes | The header value. Must contain `{key}` exactly once, which is replaced by the entry's `gateway.api_key`. Under `gateway.keyless: true`, no header is sent |

When the block is read and refused:

| Case | Result |
|---|---|
| Capsule entry declares no `gateway:` | The block is never read |
| Capsule entry declares `gateway:`, block absent or malformed | `mur run` refuses with [`E-RUN-025`](diagnostics.md#e-run-025) |
| Artifact manifest declares `inference_auth:`, the block's former name, with or without `upstream_auth:` | `mur build` refuses with [`E-BLD-004`](diagnostics.md#e-bld-004); an artifact already packed refuses the launch with [`E-RUN-025`](diagnostics.md#e-run-025) when its entry declares `gateway:` |

### `models:` block { #driver-models }

The limits of each model a driver serves, declared in the driver's own `murmur.yaml` beside
`upstream_auth:`. A `transport: http` capsule on the driver takes each driver choice's
[output cap](#inference-max-tokens) and context window from the entry for its model, unless the
capsule's manifest sets them, and its input ceiling from the entry alone.

```yaml
name: driver-glm
version: "0.1.0"
runtime: driver
requires_files:
  - driver.wasm
models:
  glm-5.3:
    max_output_tokens: 131072
    context_window: 200000
  gpt-5.6:
    max_output_tokens: 128000
    context_window: 1050000
    max_input_tokens: 922000
```

| Field | Type | Required | Notes |
|---|---|---:|---|
| `models.<id>` | map | — | One entry per model id. The id is matched by exact string equality with `inference.model` or an alternate's `model`: no prefix, alias or case folding, so a driver that accepts a dated id and an alias lists both. Must not be empty. No key besides the three below |
| `models.<id>.max_output_tokens` | integer | yes | The most output tokens the model produces in one completion. `1` to `4294967295` |
| `models.<id>.context_window` | integer | yes | The model's context window in tokens. `1` to `4294967295` |
| `models.<id>.max_input_tokens` | integer | no | The most input tokens the provider accepts in one request for the model. `1` to `4294967295`, and at most `context_window`. With it, every call's [input budget](#inference-max-tokens) is at most this number |

| Case | Result |
|---|---|
| `models:` malformed — a `max_input_tokens` above its `context_window` included — or declared on an artifact whose `runtime` is not `driver` | `mur build` refuses with [`E-BLD-005`](diagnostics.md#e-bld-005), naming the field |
| A packed `murmur.yaml` whose `models:` is malformed | `mur install` and `mur publish` refuse with [`E-REG-011`](diagnostics.md#e-reg-011), naming `name@version` and the field, and write nothing to the store or `murmur.lock` |
| A stored driver whose `models:` is malformed | `mur run` refuses with [`E-RUN-052`](diagnostics.md#e-run-052), naming the driver and the field |
| The table lists no entry for a choice's model | The capsule's own `inference.max_tokens` and `context.max_tokens` apply; see [Output cap](#inference-max-tokens) |

### `input_schema` { #input-schema }

The JSON Schema object a tool offers the model as its parameters. Written as a YAML string holding
JSON, or as a YAML mapping:

```yaml
input_schema: |
  {"type":"object","properties":{"operation":{"type":"string","enum":["write_file","replace_in_file"]},"dest_path":{"type":"string"},"content":{"type":"string"}},"required":["operation","dest_path"]}
```

**Required fields are enforced.** Before a call to the tool runs, the runtime checks that every
name in the schema's top-level `required` is a key of the call's input. A call missing one or more
of them is refused without running the tool — on `transport: http`, on `transport: process` and in
a [plan](#field-plan) `tool` step alike. The model is handed a failed tool result:

```text
tool-editor: missing required field "operation"

This call did not run. tool-editor's input_schema requires: "operation", "dest_path". Call it again with every one of them present. Only the presence of required fields is checked before a call runs — not their types, allowed values or nested contents.
```

With several fields missing, the first line names all of them: `missing required fields
"operation", "dest_path"`. The trace records a
[`tool_input_refused`](observability-schemas.md#tool-input-refused) beside a `tool_call` with
`status: "error"`; a plan step records the same text in its `plan_step` record's `error` instead. A
refusal is an ordinary tool result: the turn continues, and every turn the model spends retrying
counts against `inference.max_turns`.

| Checked | Not checked |
|---|---|
| Each name in the top-level `required` is a key of the input object | Value types, `enum` values, nested objects, `$ref` |
| An input that is not a JSON object is missing every required name | A `null` or empty value — the key is there, so the field is present |

| Path | Order of the pre-dispatch checks |
|---|---|
| A direct call, either transport | Required fields, then [`capabilities.filesystem.read_only`](#read-only-paths), then an `on-tool-call` policy hook. A call missing a field never reaches the hook |
| A plan `tool` step | `read_only` and the policy hook when the step is reached, then required fields when it is invoked |

There is no opt-out. A field the tool supplies a default for belongs out of `required`.

| Schema | Calls |
|---|---|
| No `input_schema`, no `required`, or `required: []` | Not checked |
| A string that is not JSON | Not checked |
| A `runtime: skill` artifact's schema | Not checked |
| Root is not a JSON object | Not checked; [`W-RUN-004`](diagnostics.md#w-run-004) once per tool per session |
| `required` is not an array of strings | Not checked; [`W-RUN-004`](diagnostics.md#w-run-004) once per tool per session |

The schema is read when each call is made, so a tool pulled into the session at runtime is checked
against its own schema from its first call.

---

## Capsule manifest { #capsule-manifest }

The manifest `mur run` reads.

One `murmur.yaml` can be both manifests at once: a capsule you also publish carries the
[artifact manifest](#artifact-manifest) fields beside the capsule fields below. `mur run` and
`mur doctor` accept `runtime`, `implementation`, `execution` and `requires_files`, take no
run-time meaning from them, and do not report them as unrecognized keys
([`W-SEC-019`](diagnostics.md#w-sec-019)).

Where artifacts come from is set in `~/.murmur/config.yaml`, not in a manifest — see
[Resolution order](config.md#resolution-order). Both manifests refuse a top-level `registry:` key,
whatever it holds, before any other field is checked, so `mur install`, `mur run`, `mur doctor`,
`mur eval`, `mur build` and `mur publish` all fail on it with `E-MAN-003`:

| Field | Refused with |
|---|---|
| `registry` | `murmur.yaml: 'registry' is not a manifest key` |

### Supported shape

```yaml
name: my-capsule
version: "0.1.0"

artifacts:
  - name: ana/some-tool   # <scope>/<name>
    version: "1.2.3"
    runtime: tool  # optional, defaults to tool
    gateway:       # optional: the runtime presents this tool's third-party key for it
      endpoint: https://api.tavily.com
      api_key: ${TAVILY_API_KEY}   # optional; literal value or ${ENV_VAR}
  - name: murmur/driver-anthropic
    version: "{{ v.murmur_driver_anthropic }}"
    runtime: driver
    gateway:       # required on the transport: http driver
      endpoint: https://api.anthropic.com
      api_key: ${ANTHROPIC_API_KEY}
  - name: murmur/hook-debug
    version: "{{ v.murmur_hook_debug }}"
    runtime: hook  # lifecycle observer; hidden from the model

capabilities:
  network:
    allow:
      - https://api.github.com  # hosts tools and shell subprocesses may reach; not needed for a gateway's upstream
    unix_sockets: false  # optional, defaults to false: may shell subprocesses create AF_UNIX sockets?
  peer_fetch:            # optional: peers this capsule may redeem a peer-file handle against
    allow:
      - localhost:41234
  filesystem:
    scope: ./workdir
  shell:
    allow:
      - bash           # binaries the agent may invoke as shell tools
    strip_env:         # optional: env var patterns to strip from subprocess environment
      - AWS_*
    baseline_env:      # optional: env var patterns to keep after stripping
      - PATH
    interpreter_runtime:            # optional: host dirs a path-based interpreter needs
      - binary: python3             # MUST already appear in allow above
        dirs:
          - path: /usr/lib/python3.11
            list_dir: true          # the dir's entries are enumerable
          - path: /usr/lib/python3.11/lib-dynload
            list_dir: false         # files openable by exact name, dir not listable
    staged_runtime:                 # optional: pinned host runtime trees to bind-mount into a sealed root
      - binary: python3             # MUST already appear in allow above; MUST NOT also have an interpreter_runtime grant
        source_path: /opt/testbed/conda/envs/django__django   # absolute host path to an already-pinned tree
        pin: conda-4.10.3/python-3.9.19/testbed-2024-05-01    # required; never inferred
  env:
    allow:             # optional: host env vars a WASM component (capsule/tool/driver) may observe
      - MY_APP_REGION

network:
  internal_port: 14159  # optional; omit to let the OS assign a free port
  authentication:       # optional; omit for a door that answers every caller
    scheme: bearer      # required in the block; the only scheme
    credentials:        # optional; tokens narrower than the operator token
      watcher:
        scopes: [GetTask, murmur.WatchSession]

context:
  max_tokens: 200000   # optional; overrides the context_window the driver declares for the model
  record: on           # on (default) | off — the durable conversation record
  record_store: shey   # optional; directory under ~/.murmur/conversations/, default: capsule name
  retain:              # optional; omit to keep every record whole and forever
    max_messages: 2000 # truncate the front of the record this launch opens beyond this
    max_age: 90d       # drop a record this capsule owns and has not written to for this long

observability:
  otel_endpoint: http://localhost:4318  # OTLP/HTTP endpoint; absent = no external span export
  eval:
    dataset_id: my-dataset              # optional; labels dataset_run records
    scorers:
      - type: exit_ok
        name: success_check
      - type: max_turns
        name: turn_limit
        max: 5
      - type: max_tokens
        name: token_budget
        max: 100000
      - type: tool_sequence
        name: tool_order
        expected: [bash, python]

trace:
  capture: content    # optional; default meta: also store the bodies behind the trace's hashes
  retain:             # optional; omit to keep every session's records forever
    max_sessions: 50  # keep the newest N sessions, this launch's own included
    max_age: 14d      # and/or drop anything whose ses_ id was minted longer ago than this

inference:
  transport: http
  model: claude-opus-4-5
  driver:
    artifact: murmur/driver-anthropic  # its entry above carries the gateway
    config:
      some_flag: true             # optional free-form JSON object
  compaction:
    threshold: 0.85              # optional; default 0.98
    model: claude-haiku-4-5      # optional; defaults to primary inference model
    system_prompt: |             # optional; defaults to the compaction hook's own prompt
      task = X, currently editing Y, already tried Z.
    # system_prompt_file: compaction-instructions.md  # alternative: load from file
    # dump_summaries: true       # optional; default false
    # The compaction hook is selected by binding, not named here: declare an
    # artifact with `runtime: hook` and `binding: on-compaction` (see artifacts:).
  system_prompt: |              # optional; injected as `system` on every API call
    Always begin responses with CONFIRMED:
  # system_prompt_file: conventions.md  # alternative: load from file
  # max_turns: 50               # optional; default: no cap

# Alternative: run a harness CLI as a subprocess (no ANTHROPIC_API_KEY required)
# inference:
#   transport: process
#   driver:
#     artifact: local/my-process-driver  # the process driver that knows this harness
#   max_turns: 50

lifecycle:
  task_acceptance: queue  # none | single (default) | queue
  after_task: sleep       # exit (default) | sleep
  queue_depth: 2          # only meaningful for queue mode; default: 1
  input_timeout_secs: 60  # optional; absent = wait indefinitely for request-input reply
  conversation: threaded  # stateless (default) | threaded

exports:
  files:
    root: out/            # required; relative to the work folder
    mode: read-only       # required; the only accepted value
    max_bytes: 10Mi       # optional; per-file read ceiling, default 10Mi
  peer_files:
    root: out/handoff/    # required; relative to the work folder
    max_ttl: 15m          # optional when after_task: exit (default 1h); required, max 15m, when sleep
    max_bytes: 10Mi       # optional; per-file ceiling on a redeemed read, default 10Mi
  peer_tasks:
    accept: true          # required; whether the door serves tasks from other capsules

control:                  # optional: what a controller may change while the capsule runs
  settings: [inference.max_tokens]
  secrets: [TAVILY_API_KEY]   # each backs some artifact's gateway.api_key: ${NAME}

deploy:                   # optional: read by mur deploy only
  mur_version: "1.0.0"    # optional; the mur release installed on the target, default the running mur
```

### Field reference

#### Identity { #field-identity }

| Field | Type | Required | Notes |
|---|---|---:|---|
| `name` | string | yes | Capsule identity: the capsule's bare name, as for any [artifact](#artifact-manifest). |
| `version` | string | yes | Capsule version. |

#### `artifacts` { #field-artifacts }

| Field | Type | Required | Notes |
|---|---|---:|---|
| `artifacts` | list | no | Defaults to empty. |
| `artifacts[].name` | string | yes (per entry) | The artifact's full name, `<scope>/<name>`, such as `murmur/tool-git` — see [Artifact names](../concepts/registry.md#artifact-names). A bare name is refused with `E-MAN-003` and a hint naming its scoped forms. An entry that declares `source:` takes the bare name instead, and a scope on it is refused. The bare part is the tool name the model calls, so two entries may not share one ([`E-CAP-020`](diagnostics.md#e-cap-020)). |
| `artifacts[].version` | string | yes (per entry) | Optional when `source` is set — see [Local-source artifacts](#local-source-skills). |
| `artifacts[].runtime` | `tool \| driver \| hook \| skill` | no | Default: `tool`. `tool` and `skill` artifacts are model-visible; `driver` and `hook` artifacts are hidden from the model. `wasm` and `native` are rejected here — those belong in the artifact's own manifest as `implementation`. |
| `artifacts[].source` | string | no | Local path the runtime resolves this artifact from instead of the registry. Requires `local_source: true`. See [Local-source artifacts](#local-source-skills). |
| `artifacts[].local_source` | bool | no | Opts this artifact into `source:` resolution. Default: `true` for `runtime: skill`, `false` for every other role; an explicit value overrides that default in both directions. See [Local-source artifacts](#local-source-skills). |
| `artifacts[].prompt_payload` | bool | no | Opts this artifact into being named by `inference.system_prompt_artifact`. Default: `true` for `runtime: skill`, `false` for every other role; an explicit value overrides that default. See [`inference.system_prompt_artifact`](#inference-system-prompt-artifact). |
| `artifacts[].capabilities` | map | no | Per-artifact capability grant, recognized on `runtime: hook`, `runtime: tool` and `runtime: driver`. The baseline differs by role: on a hook, absent means no network and no filesystem at all (see [Hook capabilities](#hook-capabilities)); on a tool or driver, absent means the unchanged capsule-wide ceiling, and a declared block *narrows* below it (see [Tool and driver capabilities](#tool-capabilities)). `capabilities.state` is the exception to both baselines: absent means no durable store for any role, and a declared block opens one directory outside every workdir. Declaring it on `runtime: skill` fails with `E-MAN-003`. |
| `artifacts[].config` | map | no | Operator-authored configuration delivered to this artifact alone as the `MURMUR_ARTIFACT_CONFIG` environment variable, serialized as compact JSON. Recognized on `runtime: hook`, `runtime: tool` and `runtime: driver`; declaring it on `runtime: skill`, or at the top level of the manifest, fails with `E-MAN-003`. Absent, the variable is absent from that artifact's environment. See [Artifact config](#artifact-config) and [Choosing a config block](#which-config-block). |
| `artifacts[].gateway` | map | no | The third-party upstream this artifact reaches through the credential gateway, and the key the runtime presents there. Required on the `transport: http` driver `inference.driver.artifact` names. Which other entries accept it is in [Credential gateway](#artifact-gateway). |
| `artifacts[].gateway.endpoint` | string | yes (when `gateway` is set) | Upstream URL. A `gateway:` without it fails with `E-MAN-003`. See [`gateway.endpoint` validation](#gateway-endpoint-validation). |
| `artifacts[].gateway.api_key` | string | yes, unless `keyless: true` | Literal value or `${NAME}` reference. A gateway with neither a non-blank `api_key` nor `keyless: true` fails with [`E-CAP-018`](diagnostics.md#e-cap-018). See [`gateway.api_key` resolution](#gateway-api-key). |
| `artifacts[].gateway.keyless` | boolean | no | `true` declares an upstream that takes no key: requests go out with no credential header. Default: `false`. Writing it with `api_key` fails with `E-MAN-003`. See [Keyless upstreams](#gateway-keyless). |
| `artifacts[].on_overflow` | `drop \| block` | no | Default: `drop`. Recognized only on `runtime: hook`; declaring it on any other role fails with `E-MAN-003`. Governs what happens when an `execution_mode: async` hook's job queue is full — see [Async hook execution](#hook-overflow). Legal but inert on a hook that turns out to be `execution_mode: blocking`, which has no queue. |

##### Hook capabilities { #hook-capabilities }

A hook runs default-deny: a `runtime: hook` entry with no `capabilities:` block gets no network
capability (no raw WASI sockets, and an empty outbound allow-list, so every HTTP request is
denied) and no directory at all — it cannot read or write any file, not even in its own working
directory. Network and filesystem are granted back one hook at a time, from that hook's entry in
**your own** `murmur.yaml`:

```yaml
artifacts:
  # default-deny: no network, no filesystem
  - name: local/hook-observe
    version: 0.1.0
    runtime: hook

  # granted exactly one host and exactly one directory
  - name: local/hook-telemetry
    version: 0.1.0
    runtime: hook
    capabilities:
      network:
        allow:
          - https://telemetry.example.com
      filesystem:
        scope: hook-state

  # granted the task text and the agent's result, and nothing else
  - name: local/hook-output-gate
    version: 0.1.0
    runtime: hook
    capabilities:
      task_io:
        read: true

  # granted the capsule's conversation record, and nothing else
  - name: local/hook-recall
    version: 0.1.0
    runtime: hook
    capabilities:
      conversation:
        read: true

  # granted a durable store and nothing else — no project directory at all
  - name: local/hook-notes
    version: 0.1.0
    runtime: hook
    capabilities:
      state: {}

  # writes its records beside the session's trace, outside the workdir
  - name: murmur/hook-eval
    version: 1.0.0
    runtime: hook
    capabilities:
      filesystem:
        records: true

  # each of its calls may run for up to two minutes
  - name: murmur/hook-compact
    version: 1.0.0
    runtime: hook
    capabilities:
      limits:
        deadline_seconds: 120
```

| Key | Type | Required | Description |
|---|---|---|---|
| `task_io.read` | bool | yes | Whether this hook may read the task's input text and the agent's result text through [`murmur:task-io/read`](wit-interfaces.md#murmurtask-ioread). Never inferred: a `task_io:` block that omits it fails with `E-MAN-003`. |
| `conversation.read` | bool | yes | Whether this hook may read the capsule's [conversation record](workdir.md#the-conversation-record) through [`murmur:conversation/read`](wit-interfaces.md#murmurconversationread). Never inferred: a `conversation:` block that omits it fails with `E-MAN-003`. |
| `state.store` | string | no | Directory name under `~/.murmur/state/`. Omitted, the capsule name is used. See [Durable state](workdir.md#state-store). |
| `filesystem.scope` | string | no | A directory of the workdir mounted as the hook's current directory. See the rules below. |
| `filesystem.records` | bool | no | Mount the session [records directory](workdir.md#records-directory)'s `hooks/` as the hook's current directory, read-write, in place of a `scope`. Default: `false`. Recognized only on a `runtime: hook` entry; declared `true` together with `filesystem.scope`, on any other role, or in the capsule-wide [`capabilities`](#field-capabilities) block, it fails with `E-MAN-003`. |
| `limits.deadline_seconds` | integer | no | Wall-clock budget for each of this hook's lifecycle calls, in seconds, `on-stage` included. Default: the capsule-wide [`capabilities.limits.deadline_seconds`](#field-capabilities), else 30. Must be > 0. A call past it ends with a message naming this key on this entry. |

Rules:

- **Only the capsule operator can grant.** The grant is read from your capsule manifest's artifact
  entry. A `capabilities:` key inside a published hook artifact is inert.
- **The grant is per-hook.** The capsule-wide [`capabilities`](#field-capabilities) block does not
  reach hooks, and a hook's grant does not widen the capsule.
- **`network.allow` takes the same entries and the same enforcement as the capsule-wide block** —
  the `host`, `host:port` and `scheme://host[:port]` forms of
  [Network allow entries](#network-allow-entries). Anything not listed is denied.
- **`filesystem.scope` is a real directory grant.** Exactly one directory — `<workdir>/<scope>` —
  is mounted as the hook's current directory, and is created if missing. Paths outside that subtree
  are unreachable. An absolute scope, or one that escapes the workdir via `..`, fails at launch with
  [`E-CAP-002`](diagnostics.md#e-cap-002) before any hook component is instantiated.
- **`filesystem.records` keeps what a hook writes out of the commands' reach.** A hook that
  writes records — `murmur/hook-eval`'s `eval.jsonl` and its own log — declared
  `records: true` writes them into `hooks/` beside the session's `trace.jsonl`, where the capsule's
  commands can read them and cannot change them, and where [`mur eval`](cli.md#mur-eval) reads
  `hooks/eval.jsonl`. The same hook declared `scope: .` writes them into the workdir. It works on
  every hook binding, on-stage included.
- **`state` is a second, independent directory grant.** A hook holding one reaches
  `~/.murmur/state/<store>/` as `state/`, alongside — or instead of — the
  `filesystem.scope` directory mounted as `.`. The two do not imply each other in either
  direction, so a hook can hold durable state without being handed the project directory. The
  store is keyed by capsule, so it survives a launch that gets a fresh work folder. See
  [Durable state](workdir.md#state-store).
- **`task_io.read` grants a host import, not a directory.** A granted hook reads the task text
  and the result text from the runtime itself, so it needs no `filesystem.scope` for either. An
  ungranted hook still loads and still runs; every read returns `not-granted`.
- **`task_io` is recognized only on a `runtime: hook` entry.** Declaring it on a `runtime: tool`,
  `runtime: driver` or `runtime: skill` entry, or in the capsule-wide
  [`capabilities`](#field-capabilities) block, fails with `E-MAN-003` — nothing there could
  enforce it.
- **`conversation.read` grants a host import, not a directory.** No artifact ever gets a
  filesystem path into `~/.murmur/conversations/`, so a granted hook reads the record through the
  interface and an ungranted one gets `not-granted` from the call rather than failing to load. Declaring the key on
  any role other than `runtime: hook` fails with `E-MAN-003`; declaring it in the capsule-wide
  [`capabilities`](#field-capabilities) block is inert and prints
  [`W-SEC-016`](diagnostics.md#w-sec-016).
- **Only `network`, `filesystem`, `state`, `task_io`, `conversation` and
  `limits.deadline_seconds` govern a hook.** `shell`, `spawn`, `env`, `limits.memory_bytes`,
  `limits.table_elements`, `limits.instances`, `resources` and `containment` parse here but are
  capsule-wide concerns the runtime does not apply per-hook; declaring one prints
  [`W-SEC-006`](diagnostics.md#w-sec-006).

##### Tool and driver capabilities { #tool-capabilities }

A `runtime: tool` or `runtime: driver` entry takes the same `capabilities:` key, but the baseline
is inherit-and-clamp rather than default-deny. An entry with no `capabilities:` block runs on the
full capsule-wide [`capabilities`](#field-capabilities) ceiling. Declaring a block *narrows* that
one artifact:

```yaml
capabilities:            # the capsule-wide ceiling
  network:
    allow:
      - https://api.example.com
      - https://other.example.com

artifacts:
  # unchanged: reaches both ceiling hosts, sees the whole workdir
  - name: local/broad-tool
    version: 0.1.0
    runtime: tool

  # narrowed: one host, and only the `cache/` subtree of the workdir
  - name: local/scoped-tool
    version: 0.1.0
    runtime: tool
    capabilities:
      network:
        allow:
          - https://api.example.com
      filesystem:
        scope: cache

  # widened by exactly one directory: a durable store outside every workdir
  - name: murmur/tool-corpus
    version: 0.1.0
    runtime: tool
    capabilities:
      state:
        store: shey
```

| Key | Type | Required | Description |
|---|---|---|---|
| `state.store` | string | no | Directory name under `~/.murmur/state/`. Omitted, the capsule name is used. See [Durable state](workdir.md#state-store). |

Rules:

- **Narrowing only ever subtracts.** The effective grant is `declaration ∩ ceiling`. An entry
  naming a host the ceiling does not itself allow is dropped, and prints
  [`W-SEC-007`](diagnostics.md#w-sec-007) naming the artifact and the dropped entry. Staging
  continues.
- **A bare host does not fit under a scheme-bound ceiling entry.** `api.example.com` spans both
  schemes and every port, so a ceiling of `https://api.example.com` does not cover it and it is
  dropped. Write the narrowing at least as specific as the ceiling entry it sits under.
- **`network.allow: []` is a real narrowing to zero**, distinct from omitting the key: an explicit
  empty list denies that artifact all outbound HTTP while its siblings keep the ceiling.
- **`filesystem.scope` is a real directory grant.** `<work folder>/<scope>` is mounted as
  that artifact's current directory instead of the whole workdir, and is created if missing.
  `scope: "."` is the explicit "whole workdir" grant. An absolute scope, or one escaping via `..`,
  fails at staging with [`E-CAP-002`](diagnostics.md#e-cap-002). It is this scope, and not the
  capsule's containment class, that bounds what a WASM artifact reaches —
  [What bounds a WASM artifact](containment.md#artifact-boundary).
- **Only the capsule operator can grant**, exactly as for hooks: the block is read from your
  manifest's artifact entry, never from the artifact's own bundled `murmur.yaml`.
- **Drivers narrow identically.** The artifact named by `inference.driver.artifact` dispatches
  through the same path as any WASM tool, so a `capabilities:` block on its entry applies to every
  driver call — including one made by a hook's `run-inference`.
- **`state` is the one sub-block that widens rather than narrows.** It grants a second directory,
  `~/.murmur/state/<store>/`, mounted in the artifact as `state/` beside the workdir mounted as `.`.
  It opens exactly that one directory: never a workdir path, and never another capsule's store.
  Declaring it does not change the capsule's achieved containment class. A store name must be a
  single path segment — see [State store name](#state-store-name). See
  [Durable state](workdir.md#state-store).
- **Only `network`, `filesystem` and `state` apply to a tool or driver.** `shell`, `spawn`, `env`,
  `limits`, `resources` and `containment` parse but are inert here and print
  [`W-SEC-008`](diagnostics.md#w-sec-008), as does any block on a tool with a **native** (non-WASM)
  implementation. A native tool runs under the capsule-wide `capabilities.filesystem`,
  `capabilities.network` and `capabilities.shell.allow`, enforced by the same sandbox as a shell
  command (see [Native tools](containment.md#native-tools)); the per-artifact block does not
  narrow them.

##### Artifact config { #artifact-config }

`config:` on an artifact entry carries operator-authored settings to that one artifact. The runtime
serializes the block to compact JSON and sets it as `MURMUR_ARTIFACT_CONFIG` in that artifact's
environment:

```yaml
artifacts:
  - name: murmur/tool-corpus
    version: 0.1.0
    runtime: tool
    capabilities:
      state: {}
    config:
      types:
        utterance:
          schema: { type: object, required: [text] }
      read_recent: { default: 20, max: 100 }
```

The tool reads `MURMUR_ARTIFACT_CONFIG` and gets:

```json
{"types":{"utterance":{"schema":{"type":"object","required":["text"]}}},"read_recent":{"default":20,"max":100}}
```

Rules:

- **Scoped to the declaring artifact.** Each entry's block reaches that artifact and no other.
  An entry with no `config:` key gets no `MURMUR_ARTIFACT_CONFIG` at all, which an artifact reads
  as an unset variable rather than as an empty object.
- **`MURMUR_ARTIFACT_CONFIG` is set by the runtime.** Naming it in
  [`capabilities.env.allow`](#field-capabilities) does not pass the host's value through, and does
  not override the block.
- **Types survive the translation.** Numbers stay numbers, sequences stay arrays, nested mappings
  stay objects, and declaration order is preserved, so the same block always produces the same
  bytes.
- **Config grants nothing.** Declaring it opens no directory, reaches no host and leaves the
  capsule's containment class unchanged.
- **Shape is validated at launch; meaning is not.** A block that breaks one of the rules under
  [Artifact config shape](#artifact-config-shape) fails with
  [`E-CAP-010`](diagnostics.md#e-cap-010). Which keys a given artifact requires is that artifact's
  own business, and a missing one surfaces as that artifact's error.
- **A native tool reads no config.** A `runtime: tool` entry whose artifact ships a native
  (non-WASM) implementation runs as a host subprocess, so a `config:` block there delivers nothing
  and prints [`W-SEC-015`](diagnostics.md#w-sec-015). The launch continues.
- **Only the capsule operator can configure**, exactly as for capabilities: the block is read from
  your manifest's artifact entry, never from the artifact's own bundled `murmur.yaml`.
- **Config is per artifact.** A top-level `config:` key fails with `E-MAN-003`.

`mur run --explain-scope` lists the artifacts that declare a block, and never what any of them
declared:

```
  artifact config:
    - murmur/tool-corpus
```

`--json` emits the same names as `configured_artifacts`, and `trace.jsonl`'s `session_start`
carries them verbatim as `effective_grants.configured_artifacts`. Both read `artifact config:
<none>` and `[]` when nothing declares a block.

###### Secrets do not belong in a `config:` block { #artifact-config-secrets }

`murmur.yaml` is an audit record of what a capsule was allowed to do, and a `config:` block is
plaintext inside it. Give an artifact its credential through a [`gateway:`](#artifact-gateway)
block with a `${NAME}` reference, which leaves only the name in the manifest — see
[`gateway.api_key` resolution](#gateway-api-key). Names that look credential-shaped are
stripped from every environment the runtime builds; a literal in a manifest field is reported as
[`W-SEC-004`](diagnostics.md#w-sec-004).

##### Credential gateway { #artifact-gateway }

A `gateway:` block on an artifact entry lets that artifact call a third-party API without holding
its key. The runtime keeps the key; the artifact sends plain HTTP to the address the runtime hands
it, and the runtime sends the request on to `gateway.endpoint` with the key attached.

```yaml
artifacts:
  - name: local/tool-web-search
    version: "0.1.0"
    runtime: tool
    gateway:
      endpoint: https://api.tavily.com
      api_key: ${TAVILY_API_KEY}
```

The fields are in the [`artifacts:` table](#field-artifacts). The block is read only from the
capsule manifest's artifact entry, never from the artifact's own bundled `murmur.yaml`. How the
key is presented comes from the artifact's bundled manifest, its
[`upstream_auth:`](#upstream-auth) block. An artifact whose entry declares
`gateway:` and whose manifest has no usable `upstream_auth:` refuses the launch with
[`E-RUN-025`](diagnostics.md#e-run-025); an artifact without `gateway:` needs no such block.

Which entries accept the block:

| Entry | `gateway:` |
|---|---|
| `runtime: tool` (WASM) | Accepted |
| `runtime: tool` with a native implementation | Refused at launch with [`E-CAP-017`](diagnostics.md#e-cap-017) |
| `runtime: hook` | Accepted |
| `runtime: driver` named by `inference.driver.artifact`, `transport: http` | Required |
| `runtime: driver` named by an [`inference.alternates`](#inference-alternates) entry, `transport: http` | Required, unless it is also the primary's driver |
| Any other `runtime: driver` entry, or any driver entry under `transport: process` | `E-MAN-003` |
| `runtime: skill` | `E-MAN-003` |

What the artifact sees and what the runtime does:

| Aspect | Behaviour |
|---|---|
| Environment | `MURMUR_GATEWAY_ENDPOINT`, set only for the artifact whose entry declares the block: `http://127.0.0.1:9` plus the path of `gateway.endpoint`, without a trailing `/`. A driver serving an agent-loop call also receives it as `MURMUR_INFERENCE_ENDPOINT`. |
| Request | A request to `127.0.0.1:9` is readdressed at `gateway.endpoint`'s scheme, host and port, keeping its own path and query; `https` upstreams get TLS. Every header named like [`upstream_auth.header`](#upstream-auth) is removed and exactly one is attached, rendered from the key. |
| Network grant | `gateway.endpoint` is the grant for these requests: they are not checked against `capabilities.network.allow` or the entry's own `capabilities.network`. Every other request from the artifact is checked as usual. An allow-list entry naming the upstream warns [`W-SEC-025`](diagnostics.md#w-sec-025). |
| Scope | Only the declaring artifact's own calls use its gateway. A request another artifact sends to `127.0.0.1:9` carries no key and is checked against the allow-list. |
| Spend | Only the gateways of the configured inference driver and of each [`inference.alternates`](#inference-alternates) driver are metered by [`inference.max_session_tokens`](#inference-max-session-tokens) and [`spend.machine_tokens_per_day`](config.md#spend). Every other gateway is unmetered and warns [`W-SEC-030`](diagnostics.md#w-sec-030) at launch and from `mur doctor`. |
| Rejection | A `401` from the upstream of the driver choice serving an agent-loop turn fails the task with [`E-RUN-027`](diagnostics.md#e-run-027). A `401` from any other gateway's upstream goes back to the artifact as the response, and is recorded in the trace. |
| Trace | Each gateway is listed in [`session_start.gateways`](observability-schemas.md#session-trace-tracejsonl). |

###### `gateway.endpoint` validation { #gateway-endpoint-validation }

`gateway.endpoint` alone decides where the key is sent, so it is validated when the manifest is
parsed, before any capsule launches or any network call is made.

| Value | Result |
|---|---|
| `https://` with a host, optional port and path (`https://api.anthropic.com/v1`) | Accepted. The upstream must present a certificate for that host that chains to a public root; a server with a private-CA certificate fails with a TLS error and receives nothing |
| `http://` with host `localhost` or a loopback IP literal (`http://localhost:11434`, `http://127.0.0.1:8080`, `http://[::1]:8080`) | Accepted. `localhost` is resolved by the host running `mur` |
| `http://` with any other host, including `0.0.0.0`, private ranges and `[::ffff:127.0.0.1]` | Rejected: the key would cross a network unencrypted |
| Userinfo before `@` (`https://user:pass@api.example.com`, `https://api.example.com@127.0.0.1`) | Rejected. The URL's host is the part after `@`, so the second example addresses `127.0.0.1`; the message names that host |
| `${` anywhere (`https://${PROVIDER_HOST}/v1`) | Rejected: `gateway.endpoint` is not interpolated |
| A query or fragment (`https://api.example.com/v1?x=1`) | Rejected: each request keeps its own query |
| A schemeless or malformed value (`api.anthropic.com`, `"not a url"`) | Rejected |
| Any scheme other than `http`/`https` (`ftp://example.com`) | Rejected |

Each rejection is `E-MAN-003` naming `gateway.endpoint`, the artifact entry, the endpoint and the
reason.

The endpoint confines the key to its scheme, host and port. Within that origin:

- **The artifact chooses the path.** It sets the whole path and query of every request; the
  endpoint's path only seeds `MURMUR_GATEWAY_ENDPOINT`.
- **The response reaches the artifact.** An upstream path that echoes request headers back hands
  the key to the artifact. Bind keys only to API hosts.
- **Redirects are returned, not followed.** A `3xx` goes back to the artifact as the response, and
  the key is never sent to its `Location`.

###### `gateway.api_key` resolution { #gateway-api-key }

`api_key` accepts two forms:

| Form | Example |
|---|---|
| Credential reference | `api_key: ${ANTHROPIC_API_KEY}` |
| Literal string | `api_key: sk-ant-xxxx` |

Only `${UPPER_SNAKE_CASE}` is a reference. Anything else is a literal value.

`mur run` resolves a reference before any session directory exists:

| Order | Where | Rotation |
|---|---|---|
| 1 | `credentials.ANTHROPIC_API_KEY` in `~/.murmur/config.yaml`, when non-empty | Re-read while the capsule runs: a key replaced with `mur config set -g` is used on the artifact's next request |
| 2 | The environment variable `ANTHROPIC_API_KEY` | Read once at launch, with [`W-SEC-027`](diagnostics.md#w-sec-027) |
| — | Neither | `mur run` refuses with `E-MAN-003`, naming the artifact, `gateway.api_key` and both places |

A literal is read once at launch, also with `W-SEC-027`. The re-read cadence, the single retry
after a `401`, and what removing an entry does are in [`credentials:`](config.md#credentials).
They apply to every gateway.

###### Keyless upstreams { #gateway-keyless }

An upstream that takes no key is declared with `keyless: true`, such as a local Ollama server
behind the inference driver:

```yaml
artifacts:
  - name: murmur/driver-openai
    version: "{{ v.murmur_driver_openai }}"
    runtime: driver
    gateway:
      endpoint: http://localhost:11434/v1
      keyless: true
```

| `gateway:` writes | Result |
|---|---|
| A non-blank `api_key` | Keyed: the runtime attaches the key |
| `keyless: true`, no `api_key` | Keyless: every header named like [`upstream_auth.header`](#upstream-auth) is removed and none is attached |
| `api_key` (any value, blank included) and `keyless: true` | `E-MAN-003` |
| No `api_key`, or one that is blank or null, without `keyless: true` | [`E-CAP-018`](diagnostics.md#e-cap-018) |

A loopback endpoint needs `keyless: true` like any other: the address is never read as a
declaration. A keyless artifact still needs its own [`upstream_auth:`](#upstream-auth) block
([`E-RUN-025`](diagnostics.md#e-run-025)), which names the header to remove. A keyless gateway is
recorded as `credential_source: "keyless"` in
[`session_start.gateways`](observability-schemas.md#session-trace-tracejsonl).

##### Local-source artifacts { #local-source-skills }

Any artifact can be sourced from a local path instead of the registry by setting `source:` and
declaring `local_source: true`. This is the authoring loop for skills, and for any other role that
opts in: edit the file, relaunch the capsule, and the change is live — no build/publish round-trip.

```yaml
artifacts:
  # source points directly at a skill.md file — skill defaults local_source to true
  - name: house-style
    source: ./skills/house-style/skill.md
    runtime: skill

  # source points at a directory containing skill.md (any casing)
  - name: review-conventions
    source: ./skills/review-conventions/
    runtime: skill

  # a non-skill role must opt in explicitly
  - name: my-tool
    source: ./local/my-tool
    local_source: true
    runtime: tool
```

Rules:

- **A `source:` entry is named by its bare name**, such as `house-style`. A scope says where an
  installed artifact came from, and a `source:` entry is not installed: `name: ana/house-style`
  with `source:` is refused with `E-MAN-003` and the hint `drop the scope: name it 'house-style'`.
- **`local_source` gates `source:`, not the role directly.** When `local_source` is absent its
  value is derived from the role: `true` for `runtime: skill`, `false` for everything else.
  Declaring `local_source: true` on a `tool`, `driver` or `hook` artifact opts it in; declaring
  `local_source: false` on a skill opts it back out.
- **`version` is optional when `source` is set.** A local file has no registry version, so the
  runtime substitutes the literal `local` wherever a version is shown (the installed-path label and
  the `MURMUR.md` listing). A `version` set anyway is ignored, and a warning is printed to stderr.
- **Relative paths resolve against the directory containing `murmur.yaml`**, not the current
  working directory. Absolute paths are used as-is.
- **For a skill, `source` may point at a `skill.md` file or at a directory.** For a directory, the
  runtime finds `skill.md` case-insensitively (`SKILL.md`, `Skill.md`, … all match) and installs it
  as lowercase `skill.md`. A file path is read directly regardless of its name.
- Everything downstream is identical to a registry skill: the file is installed to
  `tools/<name>/skill.md` in the session's [runtime directory](workdir.md#runtime-directory) and
  listed under `## Installed Skills` in `MURMUR.md`.
- A non-skill artifact declaring `local_source: true` passes manifest validation, but only a
  skill-shaped local payload — a `skill.md` file, or a directory containing one — resolves at
  launch.

Failure modes (all exit non-zero before the workdir is created):

| Condition | Code | Message |
|---|---|---|
| `source` declared without `local_source: true` | `E-MAN-003` | `artifact '<name>' declares 'source:' but does not declare 'local_source: true' (runtime: <role>)` |
| `source` path does not exist | `E-IO-001` | `skill source path not found: <path>` |
| `source` directory has no `skill.md` | `E-IO-001` | `skill source directory '<path>' contains no skill.md` |

Local-source artifacts are never written to `murmur.lock` and are skipped by `mur install` — they
are resolved fresh from disk on every `mur run`.

##### `inference.system_prompt_artifact` { #inference-system-prompt-artifact }

Names an artifact declared in `artifacts:`, by its entry's `name` as written, whose content is
read once at launch and bound as the system prompt, instead of being called on demand. The named artifact must declare
`prompt_payload: true`, or default to it as `runtime: skill` does; naming an artifact where
`prompt_payload` is `false` fails with `E-MAN-003`. A skill bound this
way is excluded from the callable tool inventory, because it is already in the system prompt. See
[Shape agent behavior with a system prompt](../how-to/capsule-system-prompt.md#step-4-load-the-prompt-from-a-skill-artifact).

Overriding the prompt with [`mur run --system-prompt`](cli.md#mur-run) releases the named artifact
back into the callable inventory for that run, and `MURMUR.md` stops describing it as bound: it is
no longer in the system prompt, so there is nothing to double-inject.

#### `capabilities` { #field-capabilities }

| Field | Type | Required | Notes |
|---|---|---:|---|
| `capabilities.network.allow` | list<string> | no | Host/URL entries the capsule may reach — see [Network allow entries](#network-allow-entries) for the accepted forms. Governs IP destinations only, TCP and UDP alike. It has no effect on unix-domain sockets, which `capabilities.network.unix_sockets` governs separately, and none on `AF_NETLINK`/`AF_PACKET`, which are always refused. An empty or absent list means no IP destination is reachable. |
| `capabilities.network.unix_sockets` | bool | no | Default: `false`. When false, the capsule's shell subprocess tree cannot create an `AF_UNIX` socket at all: `socket(AF_UNIX, ...)` fails with `EACCES`. Set `true` only if a shell tool genuinely needs a local daemon socket — the grant is capsule-wide, not a per-socket-path allowlist, so it re-exposes **every** unix socket the process can reach, `/var/run/docker.sock` (host root) included. No effect on non-Linux hosts, which have no kernel enforcement — see [`W-SEC-001`](diagnostics.md#w-sec-001). |
| `capabilities.peer_fetch.allow` | list<string> | see notes | Peers this capsule may redeem a [peer-file handle](resource-plane.md#peer-plane) against — see [`capabilities.peer_fetch`](#field-peer-fetch). Required and non-empty when the `peer_fetch:` block is present. Absent block means no fetching is possible and the `fetch-peer-file` tool does not exist. |
| `capabilities.filesystem.scope` | string | no | Relative scope under the workdir; see [Filesystem scope](#filesystem-scope). Omitted, the capsule sees the whole workdir. |
| `capabilities.filesystem.read_only` | list<string> | no | Workdir-relative subtrees the capsule may read but must not write, in the same vocabulary as `scope`. The runtime refuses, before dispatch, any tool call or shell command it can identify as writing one, records it as `protected_path_denied` and tells the model why. Omitted or `[]`, the whole workdir is writable. See [Read-only paths](#read-only-paths). |
| `capabilities.filesystem.workdir_exec` | bool | no | Default: `false`. When false, nothing the capsule writes into the work folder, its temporary folder or its home folder can be executed — under any name, including one that appears in `capabilities.shell.allow`. Set `true` only for compile-and-run workflows (the capsule builds a binary in its workdir and then runs it); doing so makes `shell.allow` unenforceable for anything inside the workdir, caps the capsule's achieved containment class at `advisory` on **every** host, and fires [`W-SEC-011`](diagnostics.md#w-sec-011) at staging. See [Executable workdirs](containment.md#field-workdir-exec). |
| `capabilities.shell.allow` | list<string> | no | Shell binaries the agent may invoke (e.g. `bash`); see [Shell allow](#shell-allow). |
| `capabilities.shell.strip_env` | list<string> | no | Glob patterns for env vars to strip from the subprocess environment (e.g. `AWS_*`). |
| `capabilities.shell.baseline_env` | list<string> | no | Glob patterns for env vars to keep after stripping (e.g. `PATH`). |
| `capabilities.shell.interpreter_runtime` | list<grant> | no | Widens an allowlisted binary's filesystem scope to specific host directories its import machinery needs outside the workdir (a path-based interpreter's stdlib). A recognised CPython or Perl interpreter needs none for its own import path — see [Derived interpreter runtimes](containment.md#derived-interpreter-runtime). Fires [`W-SEC-009`](diagnostics.md#w-sec-009) at staging. The directories are host-specific, so declaring them ties the capsule to one image's layout; where the command has a module form, allowlisting the interpreter and invoking the module through it stays correct across images — see [`E-CAP-006`](diagnostics.md#e-cap-006). |
| `capabilities.shell.interpreter_runtime[].binary` | string | yes | A binary that must already appear in `capabilities.shell.allow` — this widens filesystem access alongside an existing exec grant, it never grants exec. |
| `capabilities.shell.interpreter_runtime[].dirs` | list<dir> | yes | The host directories to grant; must name at least one. |
| `capabilities.shell.interpreter_runtime[].dirs[].path` | string | yes | An absolute host path (must start with `/`) outside the workdir. |
| `capabilities.shell.interpreter_runtime[].dirs[].list_dir` | bool | yes | `true` grants execute, read and directory listing; `false` grants execute and read only, so files are openable by exact name but the directory cannot be listed. Never inferred — must be written explicitly. |
| `capabilities.shell.staged_runtime` | list<grant> | no | Names a pinned host runtime tree to bind-mount read-only into a `sealed` capsule's composed root, so the interpreter is present inside the root rather than reachable outside it. Requires an effective `sealed` containment floor ([`E-CAP-004`](diagnostics.md#e-cap-004)), and is mutually exclusive per binary with `interpreter_runtime`. See [Staged runtime](containment.md#field-staged-runtime). |
| `capabilities.shell.staged_runtime[].binary` | string | yes | A binary that must already appear in `capabilities.shell.allow`, and must not also have a `capabilities.shell.interpreter_runtime` grant. This says where an existing exec grant's runtime comes from; it never grants exec. |
| `capabilities.shell.staged_runtime[].source_path` | string | yes | Absolute host path (must start with `/`) of an already-pinned runtime tree — a vendored toolchain directory, a baked-in conda env. Not resolved, discovered or version-sniffed by the runtime, and not required to exist on the machine that merely *parses* the manifest. |
| `capabilities.shell.staged_runtime[].pin` | string | yes | Non-empty, opaque identifier of which build the tree is. Never inferred: it exists so a human can compare the declared pin across two hosts and confirm the same runtime shipped to both. |
| `capabilities.env.allow` | list<string> | no | Host env var names a WASM component (capsule, tool or driver) may observe. Omitted or `[]` grants nothing beyond the runtime's own `MURMUR_*` injections. |
| `capabilities.limits.memory_bytes` | integer | no | Cap on how much memory a component may allocate, in bytes. Default: 536870912 (512 MiB). Must be > 0. |
| `capabilities.limits.table_elements` | integer | no | Cap on a component's table growth, in elements. Default: 100000. Must be > 0. |
| `capabilities.limits.instances` | integer | no | Cap on the number of component instances one call may create. Default: 1000. Must be > 0. |
| `capabilities.limits.deadline_seconds` | integer | no | Wall-clock budget for a single component call, in seconds. Default: 600 for a capsule, tool or driver call; 30 for a hook lifecycle call. An explicit value overrides both defaults, hooks included. A hook's own entry can set its deadline ahead of this value — see [Hook capabilities](#hook-capabilities). Must be > 0. |
| `capabilities.resources.max_processes` | integer | no | `RLIMIT_NPROC` headroom for a native subprocess with no cgroup scope: the account's count at that spawn plus this value, in threads on Linux and processes on macOS. No effect on Linux when the subprocess runs in a cgroup scope, where `cgroup_pids_max` is the bound. Default: 128. Must be > 0. See [`max_processes` by platform](resource-limits.md#max-processes-headroom). |
| `capabilities.resources.max_open_files` | integer | no | `RLIMIT_NOFILE` hard ceiling on each native subprocess. Default: 1024. Must be > 0. |
| `capabilities.resources.max_file_size_bytes` | integer | no | `RLIMIT_FSIZE` hard ceiling — largest single file a subprocess may write, in bytes. Default: 4294967296 (4 GiB). Must be > 0. |
| `capabilities.resources.cpu_seconds` | integer | no | The CPU time, in CPU-seconds, after which each native subprocess gets `SIGXCPU`, and `SIGKILL` one second later. Default: 3600 (1 hour). Must be > 0. |
| `capabilities.resources.memory_bytes` | integer | no | `RLIMIT_AS` (Linux) hard ceiling on each native subprocess's address space, in bytes. Default: 2147483648 (2 GiB). Must be > 0. macOS maps this to `RLIMIT_DATA`, which its kernel does not enforce — see the [platform note](resource-limits.md#host-resource-limits). |
| `capabilities.resources.cgroup_memory_bytes` | integer | no | cgroup v2 `memory.max` — aggregate memory across the whole subprocess tree, in bytes. Swap is bounded too: the scope's `memory.swap.max` is `0`, so a tree over this ceiling is OOM-killed rather than pushed out to swap. Default: 4294967296 (4 GiB). Must be > 0. Linux only. On a kernel that exposes no `memory.swap.max`, swap is not bounded and [`mur doctor`](cli.md#mur-doctor) says so. |
| `capabilities.resources.cgroup_pids_max` | integer | no | cgroup v2 `pids.max` — aggregate task count across the whole subprocess tree. Default: 256. Must be > 0. Linux only. |
| `capabilities.resources.cgroup_cpu_percent` | integer | no | cgroup v2 `cpu.max` quota as a percentage of one core (200 = two cores' worth). Default: 200. Must be > 0. Linux only. |
| `capabilities.resources.cgroup_io_bytes_per_sec` | integer | no | cgroup v2 `io.max` read+write throughput on the workdir's backing device, in bytes/sec. Default: 104857600 (100 MiB/s). Must be > 0. Linux only, best-effort — a workdir whose backing device cannot be resolved (overlayfs, tmpfs, device-mapper) reports [`io_max.status: unavailable`](resource-limits.md#io-max-report) with [`W-SEC-021`](diagnostics.md#w-sec-021) and keeps the other three controllers. |
| `capabilities.resources.workdir_max_bytes` | integer | no | Ceiling on the work folder, the [session's temporary folder](containment.md#temp-folder) and its [home folder](containment.md#home-folder) together, in bytes, checked every 10 seconds; crossing it ends the session with `E-RUN-013`. See [Size ceiling](workdir.md#work-folder-ceiling). Default: 10737418240 (10 GiB). Must be > 0. Every platform. |
| `capabilities.containment` | `advisory \| scoped \| sealed` | no | Minimum containment class this capsule requires, in ascending strength. Omitted, the capsule states no requirement — see [Containment class](containment.md#field-containment). Capsule-wide only; declaring it on a per-artifact entry has no effect and warns at staging — [What bounds a WASM artifact](containment.md#artifact-boundary) names the grant that scopes one artifact. |
| `capabilities.conversation.read` | bool | no | Grant of the `murmur:conversation/read` import, applied **per hook only**. Declaring it in this capsule-wide block reaches nothing — no artifact can read the conversation record — and prints [`W-SEC-016`](diagnostics.md#w-sec-016) at staging. Put it on the hook entry that needs it: [Hook capabilities](#hook-capabilities). |
| `capabilities.state.store` | string | no | Durable store name, applied **per artifact only**. Declaring it in this capsule-wide block reaches nothing — no store is created and no `state` directory is mounted — and prints [`W-SEC-014`](diagnostics.md#w-sec-014) at staging. Put it on the tool, driver or hook entry that needs it: [Tool and driver capabilities](#tool-capabilities), [Hook capabilities](#hook-capabilities). See [Durable state](workdir.md#state-store). |
| `capabilities.plan.submit` | bool | see notes | Default: absent, which is deny. `true` puts the [runtime-provided tool](runtime-provided-tools.md) `submit-plan` in the capsule's inventory; a capsule that declares nothing is not offered it. Required when the `plan:` block is present — a block that omits it is refused at parse. It grants no reach of its own: every step of a plan runs through this capsule's own tools, `capabilities.shell.allow` and `capabilities.spawn.allow`. See [Plans](plans.md). `mur run --explain-scope` reports it as `plan submit`. |
| `capabilities.install.skill` | list<string> | see notes | Default: absent, which is deny. Skills that a pull — `manage.pull`, or the [`pull-artifact`](runtime-provided-tools.md#pull-artifact) tool — may install into a running session, as [install entries](#install-entries): an exact full name, `<scope>/*`, `<scope>/<prefix>*`, or `*`. A skill is an artifact whose payload is a `skill.md` and nothing else. Capsule-wide only — declaring `install` on an `artifacts[]` entry is refused at parse. The same entries let `manage.remove` and `remove-artifact` uninstall a skill the capsule pulled. With both install lists empty or absent, every pull and every removal is refused. See [`capabilities.install`](#field-install). `mur run --explain-scope` reports it as `install skill`, and `trace.jsonl`'s `session_start` carries it as `effective_grants.install_skill`. |
| `capabilities.install.tool` | list<string> | see notes | Default: absent, which is deny. Tools, WASM or native, that a pull may install, as [install entries](#install-entries). A pulled tool joins the dispatch allowlist. The same entries let a removal uninstall a tool the capsule pulled. Drivers and hooks are never pullable or removable, under any entry. At least one of `skill` and `tool` must be non-empty when the `install:` block is present — an empty block is refused at parse. Capsule-wide only. `mur run --explain-scope` reports it as `install tool`, and `trace.jsonl`'s `session_start` carries it as `effective_grants.install_tool`. |
| `capabilities.install.from` | `[stores] \| [stores, sources]` | no | Where a pull finds what it installs. Default: `[stores]`, the project store then the global store. `[stores, sources]` (in either order) also fetches a version no store holds from the GitHub source of the artifact's scope; the runtime fetches it, records the owner id in `murmur.lock` and writes it to no store. An empty list, a list without `stores`, a duplicate and any other value are refused with `E-MAN-003`. `mur run --explain-scope` and `mur doctor` report it as `install from`, and `trace.jsonl`'s `session_start` carries it as `effective_grants.install_from`. A spawned child declaring `sources` under a parent without it is refused. |
| `capabilities.spawn.allow` | list<string> | no | Full names, `<scope>/<name>`, of the capsules this capsule may spawn as sub-capsules. A bare name is refused with `E-MAN-003`, and so are two entries with the same bare part, because `delegate-task` offers each capsule to the model by its bare part. `mur-roost` matches each spawn request's capsule name against this list and refuses a name that is absent from it — see [Per-session allow lists](roost-api.md#per-session-allow-lists) for the worked example. `capabilities.shell.allow` governs the executables the capsule runs itself. A non-empty list means the capsule has a subprocess tree, so it is bound by `capabilities.resources` and needs a network namespace on Linux ([`E-CAP-005`](diagnostics.md#e-cap-005)). It also means the session registers with `mur-roost` at launch, so the daemon holds the ceiling it referees against: with no daemon reachable at `MURMUR_ROOST_URL` the launch is refused with [`E-RUN-019`](diagnostics.md#e-run-019). A non-empty list is also what puts the [runtime-provided tool](runtime-provided-tools.md) `delegate-task` in the capsule's inventory, with the bare part of each name as the tool's `capsule` argument — see [The delegation tool](roost-api.md#the-delegation-tool). A capsule that declares none is offered no such tool. How deep a chain of delegations may go and how many children one session may hold at once are the daemon's, not this field's — see [Delegation bounds](roost-api.md#delegation-bounds). `mur run --explain-scope` reports it as `spawn allow`, and `trace.jsonl`'s `session_start` carries it as `effective_grants.spawn_allow`. |

A `capabilities.network.allow` host that fails DNS resolution at launch is skipped rather than
treated as an error: the run proceeds with that host contributing no addresses to the launch-time
IP allowlist a shell subprocess falls back to when it reaches a destination by address rather than
by name. This only ever shrinks what a shell binary can reach. When the failure was a resolver that
did not answer within five seconds — as opposed to a name that does not exist — the runtime says so
on stderr and carries on:

```
[capsule-runtime] warning: the network allowlist host 'api.example.com' could not be resolved at
launch: the resolver did not answer within 5s (a subprocess reaching it by literal address is
denied for this run)
```

Each unresolvable host costs up to that five seconds at launch. Names are still resolved for the
capsule while it runs, so a host named here stays reachable *by name* whatever happened at launch;
see [Capsule name resolution](containment.md#capsule-name-resolution). Malformed host *syntax*, as
opposed to a resolution failure, is still rejected outright with
[`E-CAP-001`](diagnostics.md#e-cap-001).

A WASM component never inherits the host process's environment. `capabilities.env.allow` is the only
way to expose a host variable. A declared-but-unset host variable is omitted rather than reported.
A declared name is judged by name alone:

| The name | Result |
|---|---|
| Matches a credential backstop pattern (see [Lock down a capsule's capabilities](../how-to/lock-down-capsule.md#step-2-manage-the-subprocess-environment) for the list) or a `capabilities.shell.strip_env` pattern | The capsule is refused with [`E-CAP-016`](diagnostics.md#e-cap-016) |
| Credential-shaped: contains `api_key`, `token`, `secret` or `password`, or has a segment such as `KEY`, `PASS`, `CREDENTIALS`, `DSN` or `AUTH` | Reaches every WASM component, reported once by [`W-SEC-024`](diagnostics.md#w-sec-024), which lists the full rule |
| Anything else | Reaches every WASM component |

The field governs WASM components and capsules delegated to through
[`capabilities.spawn.allow`](#field-capabilities) — never the capsule process itself, which is
started by the operator's own shell and inherits that shell wholesale. So a root capsule needs no
`env.allow` entry for a variable only its own process reads; the field is about what the capsule
hands onward.

The runtime omits an unset name at launch rather than refusing it, so a run reaches the delegation
before an absent variable shows itself. [`mur doctor`](cli.md#mur-doctor) reports the whole
`spawn.allow` closure's requirement ahead of a run, marking every declared name set or unset — by
name only, never by value.

#### Process environment { #process-environment }

A process the runtime starts on the host — a `transport: process` harness, a shell command or a
native tool — begins with an empty environment and receives the variables below, in order. A later
row wins over an earlier one for the same name. A WASM component receives only what
`capabilities.env.allow` declares, as described above.

A [`transport: process`](#transport-process) harness:

| Order | Source | Variables |
|---|---|---|
| 1 | `capabilities.env.allow` | The host's value of each declared name the host sets, after the credential backstop |
| 2 | The driver's launch plan | Each name its `env-set` lists |
| 3 | The host account | `USER` and `LOGNAME`, each with the host's value when the host sets it and absent when it does not. No `env.allow` entry or launch plan names a different account. [`harness_start`](observability-schemas.md#harness-records)'s `account_env_unset` lists the names the host did not set |
| 4 | The session | `MURMUR_RUNTIME_DIR`, the session's [runtime directory](workdir.md#runtime-directory), and `MURMUR_RECORDS_DIR`, its [records directory](workdir.md#records-directory) |

The harness's version check runs with rows 1 and 3 only.

A shell tool call, a plan's `shell` step, or a native tool:

| Order | Source | Variables |
|---|---|---|
| 1 | Baseline | The host's value of each of `PATH`, `HOME`, `USER`, `LOGNAME`, `LANG`, `LC_ALL`, `CARGO_HOME`, `RUSTUP_HOME` and `TERM` the host sets |
| 2 | The session's identity | A shell tool call in a session with `inference:` only: `MURMUR_CAPSULE_NAME`, `MURMUR_CAPSULE_VERSION`, `MURMUR_SESSION_ID`, `MURMUR_CAPSULE_URL` and the [`MURMUR_INFERENCE_*` variables](default-artifacts.md#driver-environment) |
| 3 | `capabilities.shell.baseline_env` | The host's value of each listed name the host sets |
| 4 | Credential backstop and `capabilities.shell.strip_env` | Removes every name that matches a [credential pattern](../how-to/lock-down-capsule.md#step-2-manage-the-subprocess-environment) or a `strip_env` pattern, baseline names such as `USER` and `LOGNAME` included |
| 5 | The session's home folder | `HOME` and `USERPROFILE`, both the session's [home folder](containment.md#home-folder), `home/` in its records directory, never the host's home |
| 6 | The session | `MURMUR_RUNTIME_DIR` and `MURMUR_RECORDS_DIR`, as for the harness |
| 7 | The session's temporary folder | `TMPDIR`, `TMP` and `TEMP`: `/tmp` where the session maps `/tmp` onto its [temporary folder](containment.md#temp-folder), and the folder's absolute path where it does not. No earlier row sets them, `capabilities.shell.baseline_env` included |

A delegated child `mur` starts from an empty environment and receives the parent's `USER` and
`LOGNAME` beside its `PATH` and `HOME`, each only when the parent holds it.
[The child's environment](roost-api.md#the-childs-environment) lists every variable it receives.

#### `capabilities.plan` { #field-plan }

Grants the capsule's agent one [runtime-provided tool](runtime-provided-tools.md), `submit-plan`,
which runs a plan of steps and returns every step's result in one reply. The plan JSON format is
[Plans](plans.md).

```yaml
capabilities:
  plan:
    submit: true
```

`submit` is never inferred: a `plan:` block that omits it is a parse error naming
`capabilities.plan.submit`, and `submit: false` is the same grant as an absent block.

The grant is capsule-wide. Declaring it on a per-artifact entry reaches nothing — a plan is
submitted by the agent, not by an artifact.

A plan opens no path this capsule does not already have. Each step is dispatched by the session
that submitted it: a `tool` step reaches the tools in the capsule's own inventory, a `shell` step
the binaries in `capabilities.shell.allow`, and a `capsule` step the names in
`capabilities.spawn.allow`. What the grant changes is how much of that one model turn can reach
without another turn in between.

#### `capabilities.install` { #field-install }

Grants a capsule permission to install skills and tools while the session runs, and to
uninstall them again:

| Capsule | Pulls with | Removes with |
|---|---|---|
| A script capsule | [`murmur:artifact-manager/manage.pull`](wit-interfaces.md#murmurartifact-managermanage) | [`manage.remove`](wit-interfaces.md#manage-remove) |
| An agent capsule, `transport: http` | The [`pull-artifact`](runtime-provided-tools.md#pull-artifact) tool | The [`remove-artifact`](runtime-provided-tools.md#remove-artifact) tool |

Under `transport: process` the block is refused at parse with `E-MAN-003`: the harness owns its
tool list, so a pulled artifact could never reach it. Both paths make the same checks.

```yaml
capabilities:
  install:
    skill: [ana/code-review, "ana/style-*"]
    tool: [local/jq]
    from: [stores, sources]
```

| Key | Admits |
|---|---|
| `skill` | Artifacts whose payload is a `skill.md`, declared `runtime: skill` |
| `tool` | Artifacts whose payload is a WASM component or a native binary, declared `runtime: tool` |

An artifact matches only the list for its own kind: `skill: ["*"]` admits no tool. A `driver` or
`hook` is refused under every entry, and so is an artifact whose declared role does not match its
payload.

A pull resolves from the project artifact store, falling back to the global store under
`~/.murmur`: what [`mur install`](cli.md#mur-install) or [`mur publish`](cli.md#mur-publish) has
already placed on this host. With `from: [stores, sources]`, a version neither store holds is
fetched from the scope's [source](config.md) with the checks `mur install` makes, and pinned in
`murmur.lock` with the scope's owner id; no store gains it. A pull names a full artifact name and
a version that is a single path segment; a bare name, a `github:` reference, a path, or a version
containing `/` or `..` is refused. A pull with no version takes the newest version, by semver
precedence, that fits the [WIT level](wit-levels.md#fit) this `mur` serves and has a payload for
this platform; on a tie the store wins. A version that does not fit is refused with
[`E-WIT-LEVEL-001`](diagnostics.md#e-wit-level-001), naming the newest that does.

A pulled artifact runs on this capsule's own grants. A pulled tool joins the dispatch allowlist,
so it can be called; it gains no per-artifact capabilities and no gateway, and the
`capabilities:` block in its own bundled `murmur.yaml` is ignored. It is marked untrusted in the
tool list and its results are fenced — see [Artifact origin](../concepts/access-control.md#artifact-origin).

`manage.remove` uninstalls only an artifact the capsule pulled:

| Artifact | `manage.remove`, `remove-artifact` |
|---|---|
| Pulled, and pinned in `murmur.lock` as `origin: runtime` | Removed |
| Declared in this `murmur.yaml`'s `artifacts:` | Refused, whatever its pin origin |
| Pinned in `murmur.lock` as `origin: operator` | Refused |
| A `hook` or a `driver` | Refused |

A capsule spawned through [`capabilities.spawn.allow`](#field-capabilities) holds only the
`install` block its own manifest declares. `mur-roost` refuses a child whose entries its parent's
do not cover, or that declares `from: [stores, sources]` under a parent that does not — see
[Spawn envelope](roost-api.md#spawn-envelope).

#### `capabilities.peer_fetch` { #field-peer-fetch }

Names the peers this capsule may redeem a [peer-file handle](resource-plane.md#peer-plane) against.
Declaring it gives the agent one
[runtime-provided tool](runtime-provided-tools.md), `fetch-peer-file`.

It sits beside `capabilities.network` rather than inside it because fetching a peer's bytes lands a
file in this capsule's own workdir: that is an ingestion path, and therefore a prompt-injection
surface, which deserves its own operator control.

`allow` uses the same syntax and the same matcher as
[`capabilities.network.allow`](#network-allow-entries), and is a **separate list**:

- Declaring a destination here does not widen `capabilities.network.allow`.
- A destination in `capabilities.network.allow` is not redeemable unless it also appears here.

An empty `allow: []` is a parse error rather than a silent deny — `E-MAN-003`, naming
`capabilities.peer_fetch.allow`. The check runs before any connection is opened, so a refused peer
is never contacted.

#### `network` { #field-network }

| Field | Type | Required | Notes |
|---|---|---:|---|
| `network.internal_port` | integer | no | The port the capsule's A2A HTTP server binds. When set, the runtime binds exactly that port and fails with `E-RUN-010` if it is already in use. When omitted, the OS assigns a free port. Either way the bound port is the one `mur run` prints and the one `MURMUR_CAPSULE_URL` carries. |
| `network.authentication` | object | no | Makes the A2A door refuse every caller that presents no token this session minted. Default: absent, a public door that answers every caller that reaches the port. See [`network.authentication`](#field-network-authentication). |

The top-level `network:` block configures the capsule's A2A door. It is unrelated to
[`capabilities.network`](#network-allow-entries), which is where the capsule itself may connect.

##### `network.authentication` { #field-network-authentication }

| Field | Type | Required | Notes |
|---|---|---:|---|
| `network.authentication.scheme` | `bearer` | yes | How a caller authenticates: `Authorization: Bearer <token>`. `bearer` is the only value; any other is `E-MAN-003`. |
| `network.authentication.credentials` | map of name → object | no | Default: none. Each entry mints one more token beside the operator token, reaching only its `scopes`. |
| `network.authentication.credentials.<name>` | object | — | `<name>` matches `^[a-z][a-z0-9_-]{0,31}$`. `operator` is reserved. |
| `network.authentication.credentials.<name>.scopes` | list<string> | yes | Non-empty. Each entry once, from the scopes below. |

The runtime mints one `operator` token holding every scope, and one token per declared credential.
[`mur run`](cli.md#mur-run) prints them. A credential exists to hand out less than the operator
token.

| Scope | Reaches |
|---|---|
| `SendMessage` | Starting a task with `SendMessage` |
| `SendStreamingMessage` | Starting or continuing a task with `SendStreamingMessage`; the connection carries [that task's events](streaming-protocol.md#endpoints) while it is open |
| `murmur.WatchSession` | [Watching every task](agent-card.md#watch-session) on the session |
| `GetTask` | Reading a task the credential started |
| `ListTasks` | Listing the tasks the credential started, a page at a time |
| `CancelTask` | Cancelling a task the credential started |
| `SubscribeToTask` | Streaming the rest of a live task the credential started |
| `murmur.StopSession` | [Cancelling every task](agent-card.md#stop-session) on the session and reporting the residue |
| `resources/files` | The [operator resource plane](resource-plane.md#operator-plane) under `/resources/files` |

A scope names a method. A declared credential's token reaches only the tasks a request carrying it
started; the `operator` token reaches every task. `murmur.WatchSession` and `murmur.StopSession`
reach every task whoever holds them — see [Agent Card: Who sees a task](agent-card.md#task-access).
`GetExtendedAgentCard` takes no scope: every valid token reaches it. What the door answers each
caller, and the public and extended cards, are in [Agent Card: Security](agent-card.md#security).

A scope that is a retired name of a door method is refused with
[`E-MAN-003`](diagnostics.md#e-man-003), and the hint names the scope to write in its place:

| Retired scope | Write instead |
|---|---|
| `message/send` | `SendMessage` |
| `message/stream` | `SendStreamingMessage` |
| `tasks/get` | `GetTask` |
| `tasks/cancel` | `CancelTask` |
| `agent/getAuthenticatedExtendedCard` | Nothing: remove it. Every valid token may call `GetExtendedAgentCard` |
| `stream/watch` | `murmur.WatchSession` |
| `session/stop` | `murmur.StopSession` |

See [Agent Card: Migrating from A2A 0.3](agent-card.md#migrating-a2a-0-3).

A capsule declaring both `network.authentication` and a non-empty `capabilities.spawn.allow` is
refused with `E-MAN-003`: a delegated child posts its outcome to its parent's door, and a child
cannot be issued a token for that door.

#### `inference` { #field-inference }

The `inference` block is optional; a capsule with no inference block runs its tools without a
model. These fields are read under both transports:

| Field | Type | Required | Notes |
|---|---|---:|---|
| `inference.transport` | `http \| process` | no | Default: `http`. `http` routes every call through a WASM driver artifact; `process` runs a harness CLI as a subprocess. See [Inference configuration](#inference-config). |
| `inference.max_turns` | integer | no | Maximum LLM inference calls per task. Default: no cap. Must be > 0. A hook's counted `run-inference` call is one of them: it is in `task_end.turns`, and an attempt [reopened](../concepts/session-loop.md#task-reopening-commit_policy-reopen-task) after it is handed only the turns the task has left. See [Turn cap](#inference-max-turns). |
| `inference.system_prompt` | string | no | Text injected verbatim as the `system` parameter on every inference call. At most one of `system_prompt`, `system_prompt_file` and `system_prompt_artifact` may be set. |
| `inference.system_prompt_file` | string | no | Path to a file whose content is injected as the system prompt, relative to the manifest directory. |
| `inference.system_prompt_artifact` | string | no | The `name`, as written, of an artifact declared in `artifacts:` whose payload is bound as the system prompt — see [`inference.system_prompt_artifact`](#inference-system-prompt-artifact). |

These fields are read under `transport: http`. Setting any of them except `inference.model` under
`transport: process` is a manifest error:

| Field | Type | Required | Notes |
|---|---|---:|---|
| `inference.model` | string | yes | Model identifier passed to the driver. |
| `inference.max_tokens` | integer | no | Maximum output tokens the model may generate **per turn**, for every driver choice. Default: the `max_output_tokens` the driver's [`models:`](#driver-models) table declares for the choice's model; a choice with neither refuses the launch with [`E-RUN-052`](diagnostics.md#e-run-052). Must be > 0; not clamped at the top end. A value at or above a choice's context window leaves it no input budget and refuses the launch with [`E-RUN-055`](diagnostics.md#e-run-055). Distinct from [`context.max_tokens`](#field-context) — see [Output cap](#inference-max-tokens). |
| `inference.tool_refresh` | `compaction \| immediate` | no | When an artifact installed mid-session, other than by the model's own `pull-artifact`, reaches the tool list the model is offered. Default: `compaction`. See [Tool refresh](#inference-tool-refresh). |
| `inference.alternates` | list | no | Default: empty. Other driver choices the agent loop may be switched to while the capsule runs. Requires `inference.driver` in `control.settings` or `control.agent_settings`. See [Driver alternates](#inference-alternates). |
| `inference.retry` | map | no | How a driver call that failed transiently is made again: `max_retries` (0–20, default `6`), `initial_delay_ms` (1–60000, default `1000`) and `max_delay_ms` (1–3600000 and at least `initial_delay_ms`, default `60000`), each an optional integer. Refused under `transport: process`, where the harness retries its own provider calls. See [Retrying transient failures](#inference-retry). |

This field is read under both transports, against a different measurement on each:

| Field | Type | Required | Notes |
|---|---|---:|---|
| `inference.max_session_tokens` | integer | no | Most tokens this session may spend on inference in total. No default: absent sets no session ceiling. Must be > 0. See [Session spend ceiling](#inference-max-session-tokens). |

The driver's upstream and key are `gateway.endpoint` and `gateway.api_key` on its `artifacts:`
entry. A manifest that writes either under `inference:`, under any transport, fails with
`E-MAN-003`:

| Field | Refused with |
|---|---|
| `inference.endpoint` | `was removed; set gateway.endpoint on the artifacts: entry '<driver>' instead` |
| `inference.api_key` | `was removed; set gateway.api_key on the artifacts: entry '<driver>' instead` |

With no driver named, the message reads `on the artifacts: entry inference.driver.artifact names`.

`inference.driver` is read under both transports, with a meaning per transport. Under either, the
artifact it names must be declared in `artifacts:` with `runtime: driver`, and a value of `config`
that is not a mapping fails the manifest parse with `E-MAN-003`.

`inference.driver.artifact`, `inference.alternates[].driver.artifact` and
[`inference.system_prompt_artifact`](#inference-system-prompt-artifact) name an `artifacts:` entry
by its `name` exactly as written: `murmur/driver-anthropic` for a resolved entry, the bare name for
a `source:` entry. A name no entry declares is refused with `E-MAN-003`; when it is the bare part of
exactly one declared entry, the message names that entry:
`artifact 'driver-anthropic' is not declared in artifacts: (the declared 'murmur/driver-anthropic' has that name; refer to it as 'murmur/driver-anthropic')`.

| Field | `transport: http` | `transport: process` |
|---|---|---|
| `inference.driver.artifact` | Required. The inference driver. Its entry must carry [`gateway.endpoint`](#artifact-gateway). | Required. The [process driver](#process-driver). Its entry may carry none of `gateway:`, `capabilities:` or `config:`. |
| `inference.driver.config` | Optional object. Settings any driver of this role would act on, serialized to compact JSON and set as `MURMUR_INFERENCE_DRIVER_CONFIG` for the driver, every WASM tool and every shell tool in the session. See [Choosing a config block](#which-config-block). | Optional object, handed to the process driver as JSON. |

##### Driver alternates { #inference-alternates }

A **driver choice** is a named pair of one driver artifact and one model. The choice named
`primary` is `inference.driver.artifact` with `inference.model`; each `inference.alternates` entry
is another. The agent loop runs on `primary` at launch, and a controller
(`control.settings: [inference.driver]`) or the agent (`control.agent_settings: [inference.driver]`)
switches it to another declared choice by name. Nothing is pulled or installed: every alternate's
driver is an `artifacts:` entry.

```yaml
artifacts:
  - name: murmur/driver-anthropic
    version: "{{ v.murmur_driver_anthropic }}"
    runtime: driver
    gateway: { endpoint: https://api.anthropic.com, api_key: ${ANTHROPIC_API_KEY} }
  - name: murmur/driver-openai
    version: "{{ v.murmur_driver_openai }}"
    runtime: driver
    gateway: { endpoint: https://api.openai.com/v1, api_key: ${OPENAI_API_KEY} }
inference:
  model: claude-sonnet-4-5
  driver:
    artifact: murmur/driver-anthropic
  alternates:
    - name: haiku
      model: claude-haiku-4-5
      driver:
        artifact: murmur/driver-anthropic
    - name: gpt
      model: gpt-5
      driver:
        artifact: murmur/driver-openai
control:
  settings: [inference.driver]
```

| Field | Type | Required | Notes |
|---|---|---:|---|
| `inference.alternates[].name` | string | yes | Matches `^[a-z][a-z0-9-]{0,62}$`. Unique among the alternates. `primary` is reserved. |
| `inference.alternates[].model` | string | yes | Model identifier the driver sends. Must not be empty. |
| `inference.alternates[].driver.artifact` | string | yes | An `artifacts:` entry with `runtime: driver`. The entry must carry its own [`gateway:`](#artifact-gateway), unless it is the primary's driver. |
| `inference.alternates[].driver.config` | — | — | Refused. `inference.driver.config` applies to every declared driver; per-driver settings go in that driver's `artifacts[].config`. |

Every refusal is `E-MAN-003`, naming the field:

| Manifest | Field named |
|---|---|
| `inference.alternates` under `transport: process` | `inference.alternates` |
| Alternates with `inference.driver` in neither `control.settings` nor `control.agent_settings` | `inference.alternates` |
| A name that is `primary`, repeated, or outside the pattern | `inference.alternates[i].name` |
| An empty or missing model | `inference.alternates[i].model` |
| A driver that is not a declared `runtime: driver` entry, or whose entry has no `gateway:` | `inference.alternates[i].driver.artifact` |
| A `driver.config` on an alternate | `inference.alternates[i].driver.config` |
| The same driver and model as the primary or another alternate | `inference.alternates[i]` |

What follows a switch:

| Inference call | Served by |
|---|---|
| Agent-loop turns | The choice in use: its driver, its model, its gateway and credential |
| Compaction, a hook's `run-inference`, seed summarization | The primary. `inference.compaction.model` names a model of the primary's provider |
| `MURMUR_INFERENCE_*` as tools and hooks see it | The primary, for the whole session |

A driver-choice call sets `MURMUR_INFERENCE_ENDPOINT`, `MURMUR_INFERENCE_MODEL` and
`MURMUR_INFERENCE_DRIVER` in that driver's environment to the choice's own values. Each choice has
its own [output cap and context window](#inference-max-tokens), resolved at launch, and an
agent-loop call uses the cap and window of the choice it is served by. A controller's change to
`inference.max_tokens` applies to every later call whatever the choice. Every choice's gateway is metered like the primary's. An alternate whose
credential is found nowhere at launch does not refuse the launch: it is staged unavailable and
warns [`W-RUN-003`](diagnostics.md#w-run-003). Nothing a controller or the agent selects survives a
restart. What the conversation carries across a switch is in
[Switching drivers](../concepts/context.md#switching-drivers).

These fields are read under `transport: process`:

| Field | Type | Required | Notes |
|---|---|---:|---|
| `inference.command` | string | no | The executable to run instead of the one the [process driver](#process-driver)'s `describe()` names. A value containing `/` is used as a path; a bare name is looked up on `PATH`. Invalid for `transport: http`. |
| `inference.model` | string | no | Model identifier handed to the driver. Omitted, the harness uses its own configured default. |

The `inference.compaction` block takes effect only under `transport: http`; the process transport
has no compaction step. Under `transport: process`, `threshold`, `model`, `system_prompt`,
`system_prompt_file` and `dump_summaries` are accepted and inert, and `max_tokens` and
`driver_config` are refused with "is not valid with transport: process".

| Field | Type | Required | Notes |
|---|---|---:|---|
| `inference.compaction.threshold` | float (0.0–1.0] | no | Fraction of the call's [input budget](#inference-max-tokens) at which compaction fires. Default: `0.98`. Checked after each response against the provider's count of the call when the driver reports usage, and the runtime's estimate otherwise — see [Context compaction](../concepts/context.md). |
| `inference.compaction.model` | string | no | Model override for compaction calls. Defaults to the primary inference model. |
| `inference.compaction.system_prompt` | string | no | System prompt override for compaction calls, passed verbatim to the compaction hook. No trimming, length limit or format check. Omitted, the compaction hook picks its own default prompt. Mutually exclusive with `inference.compaction.system_prompt_file`. |
| `inference.compaction.system_prompt_file` | string | no | Path to a file whose content is passed verbatim as the compaction system prompt. Relative to the manifest directory; read when the session launches. Mutually exclusive with `inference.compaction.system_prompt`. |
| `inference.compaction.dump_summaries` | bool | no | Default: `false`. When `true`, every committed compaction appends one JSON line to `out/compaction-summaries.jsonl` in the session's [records directory](workdir.md#compaction-summaries), recording the verbatim summary text plus token counts. |
| `inference.compaction.max_tokens` | integer | no | Output cap of every `run-inference` call a hook makes while it serves `on-compaction` — the threshold trigger, [`mur run --resume … --resume-mode compact`](cli.md#mur-run) and a seed's overflow summary — and the amount a [spend ceiling](#inference-max-session-tokens) reserves for each. Default: the cap any other hook call sends, under [Output cap sent](#inference-max-tokens). Must be > 0. A value above the `max_output_tokens` the primary driver's [`models:`](#driver-models) table declares for the compaction model (`inference.compaction.model`, else `inference.model`) refuses the launch with [`E-RUN-054`](diagnostics.md#e-run-054); a model the table does not list takes any value. A value at or above the compaction model's window (`context.max_tokens`, else that table entry's `context_window`) leaves compaction calls no input budget and refuses the launch with [`E-RUN-055`](diagnostics.md#e-run-055). Invalid for `transport: process`. |
| `inference.compaction.driver_config` | mapping | no | Laid over [`inference.driver.config`](#inference-config) for the same calls `inference.compaction.max_tokens` caps, and no others. Shallow: each top-level key replaces the same-named key of `inference.driver.config` or is added beside it, and every other key is kept; a nested mapping replaces the whole value. With no `inference.driver.config`, the mapping alone is sent. The driver validates the merged object as it validates `inference.driver.config`, and names it `inference.driver.config` in its refusal. Default: none, so compaction calls get `inference.driver.config` unchanged. A non-mapping value is refused. Invalid for `transport: process`. |

#### `context` { #field-context }

| Field | Type | Required | Notes |
|---|---|---:|---|
| `context.max_tokens` | integer | no | The context window every driver choice's [input budget](#inference-max-tokens) is cut from: compaction and the seed budget measure against that budget. Default: the `context_window` the driver's [`models:`](#driver-models) table declares for the choice's model; with neither, the choice has no window, and compaction does not run on it. Must be > 0. A value at or below a choice's output cap leaves it no input budget and refuses the launch with [`E-RUN-055`](diagnostics.md#e-run-055). Read only under `transport: http`, like the [`inference.compaction`](#field-inference) block it drives. Distinct from [`inference.max_tokens`](#field-inference), the per-turn output cap. |
| `context.seed_budget` | float (0.0–1.0) | no | Default: `0.10`. Fraction of the primary choice's [input budget](#inference-max-tokens) at launch an `on-task-start` hook's `seed-context` may occupy. The product, rounded down, is sent to the hook as `task-start-event.budget-tokens`; `task-start-event.context-window` carries the window. Requires a window: with none there is no ceiling, and a returned seed is refused with `reason: "no_budget"`. Inert under `transport: process`, where a seed is refused with `reason: "unsupported_transport"`. |
| `context.seed_overflow_margin` | float (0.0–1.0) | no | Default: `0.10`. Slack above `context.seed_budget`, as a fraction of it, within which an over-budget seed has its oldest messages dropped rather than being handed to the compaction hook. Requires a window and is inert under `transport: process`, exactly like `context.seed_budget`. |
| `context.record` { #context-record } | `on \| off` | no | Default: `on`. Whether the runtime keeps a [durable conversation record](workdir.md#the-conversation-record) for this capsule. `off` turns the mechanism off: nothing is created under `~/.murmur/conversations/`, and a hook granted `capabilities.conversation.read` reads an empty page. Under `transport: process` no record is written either way, and `off` additionally turns off the [harness session map](workdir.md#harness-session-map). |
| `context.record_store` | string | no | Default: the capsule name. Directory under `~/.murmur/conversations/` this capsule's records live in. One path segment: no `/`, no `.` or `..`, not absolute, not starting with a dot — anything else refuses the launch with [`E-CAP-011`](diagnostics.md#e-cap-011). Accepted and inert alongside `record: off`. |
| `context.retain` { #context-retain } | block | no | What bounds this capsule's [conversation records](workdir.md#the-conversation-record). Omitted, nothing is ever deleted. See [Retention](#retention). |
| `context.retain.max_messages` | integer ≥ 1 | no | Messages to keep. At each launch, the record that launch opens — the context named by `mur run --context` — is truncated to its newest N; the older ones are dropped and the [header line](workdir.md#record-header) records the drop. A launch with no `--context` mints a context per task and opens no record to truncate; bound those with `context.retain.max_age`. |
| `context.retain.max_age` | duration | no | Age beyond which a record this capsule owns is removed whole, measured from the last write to its `conversation.jsonl`. |

#### `observability` { #field-observability }

| Field | Type | Required | Notes |
|---|---|---:|---|
| `observability.otel_endpoint` | string | no | OTLP/HTTP endpoint for OTel span export (e.g. `http://localhost:4318`). When set, the runtime exports one span per session event as the event happens, and injects `MURMUR_OTEL_ENDPOINT` into every hook component's environment — see [OTel span emission](observability-schemas.md#otel-span-emission). When absent, no outbound telemetry is sent and `trace.jsonl` is the only output. An empty string counts as absent. |
| `observability.eval.dataset_id` | string | no | Labels the `dataset_run` record in `eval.jsonl` and is forwarded to hooks as `MURMUR_DATASET_ID`. Useful when diffing runs from multiple datasets. |
| `observability.eval.scorers` | list | no | Scorer configurations. When present with at least one valid scorer, `murmur/hook-eval` writes `eval.jsonl`. When absent or empty, the hook is a no-op and logs a warning. |
| `observability.eval.scorers[].type` | string | yes (per entry) | One of `exit_ok`, `max_turns`, `max_tokens`, `tool_sequence`, `llm_judge`. An unrecognized type is skipped with a message on stderr. `llm_judge` is unimplemented: it parses, logs a warning and emits no score. |
| `observability.eval.scorers[].name` | string | no | Key this scorer's records appear under in `eval.jsonl`. Defaults to the scorer's `type`. |
| `observability.eval.scorers[].max` | integer | no | Upper bound for `max_turns` and `max_tokens`. Default: `10` for `max_turns`, `100000` for `max_tokens`. Ignored by every other type. |
| `observability.eval.scorers[].expected` | list<string> | no | Ordered list of tool names that must appear as a subsequence of observed calls, for `tool_sequence`. Defaults to empty. Ignored by every other type. |

#### `trace` { #field-trace }

| Field | Type | Required | Notes |
|---|---|---:|---|
| `trace.capture` | `none \| meta \| content` | no | Default: `meta`. How much of each turn's driver request `trace.jsonl` keeps — see the table below. |
| `trace.retain` { #trace-retain } | block | no | What bounds the sessions beside the running one: each session's [records directory](workdir.md#records-directory), and, for a launch without `--workdir`, the work folder the runtime created for it, removed together. A `--workdir` folder is never touched. On a roster's entry member, it also bounds the roster's [formation directories](roster.md#formation-retention). Omitted, nothing is ever deleted. See [Retention](#retention). |
| `trace.retain.max_sessions` | integer ≥ 1 | no | Sessions to keep in the running session's records group, counting the running session itself. The rest are removed whole: the records directory, with its `trace.jsonl`, `blobs/`, `out/`, `home/`, `tmp/` and `children/`, and, for a launch without `--workdir`, its work folder `<manifest-dir>/workdir/<session-id>`. On an entry member, also the project's formation directories to keep, counting the current formation's. |
| `trace.retain.max_age` | duration | no | Age beyond which a session is removed, measured from the millisecond timestamp inside its own uuid-v7 `ses_` id. No file metadata is read. On an entry member, also the age beyond which an ended formation directory is removed, measured from its `frm_` id. |

| `trace.capture` | `inference` content hashes | `blobs/` | `tool_call.output` |
|---|---|---|---|
| `none` | — | — | — |
| `meta` | written | — | — |
| `content` | written | written | written |

The hashes are `system_sha`, `tools_sha`, `response_sha` and `message_shas` on each
[`inference` event](observability-schemas.md#wire-hashes). Under `content` the body behind each
one is also written to [`<session_id>/blobs/<sha256>`](observability-schemas.md#trace-blobs),
verbatim and unredacted — bodies can be large, and a blob holds the wire payload as sent.

#### `lifecycle` { #field-lifecycle }

| Field | Type | Required | Notes |
|---|---|---:|---|
| `lifecycle.task_acceptance` | `none \| single \| queue` | no | Default: `single`. How the capsule accepts incoming A2A tasks — see [`lifecycle.task_acceptance`](#lifecycle-task-acceptance). |
| `lifecycle.after_task` | `exit \| sleep` | no | Default: `exit`. What the capsule does after completing a task — see [`lifecycle.after_task`](#lifecycle-after-task). |
| `lifecycle.queue_depth` | integer | no | Default: `1`. Maximum number of pending, not-yet-started tasks the capsule holds under `task_acceptance: queue`, where it is at least `1`: `0` there is refused with [`E-MAN-003`](diagnostics.md#e-man-003). Under `single` and `none` it has no effect. A task beyond this limit is answered in [`TASK_STATE_REJECTED`](agent-card.md#task-states). For a [roster](roster.md) member, [`W-ROS-001`](diagnostics.md#w-ros-001) warns when more members may call it than it holds at once. |
| `lifecycle.input_timeout_secs` | integer | no | Maximum seconds to wait for a `SendMessage` reply after a tool component calls `request-input`. Absent means wait indefinitely — see [`lifecycle.input_timeout_secs`](#lifecycle-input-timeout-secs). |
| `lifecycle.conversation` | `stateless \| threaded` | no | Default: `stateless`. Whether tasks sharing a `contextId` accumulate history — see [`lifecycle.conversation`](#lifecycle-conversation). |
| `lifecycle.max_task_reopens` | integer | no | Default: `1`. Maximum times an `on-task-end` hook (`commit_policy: reopen-task`) may reopen a single task. `0` is a valid explicit value and disables reopening. Reopening never grants turns past `inference.max_turns`; see [Task reopening](../concepts/session-loop.md#task-reopening-commit_policy-reopen-task). |
| `lifecycle.shell_grace_secs` | integer | no | Default: `10`. Seconds a shell command runs in the foreground before it is demoted to the background — see [`lifecycle.shell_grace_secs`](#lifecycle-shell-grace-secs). |
| `lifecycle.delegation_deadline_secs` | integer | no | Default: `600`. Seconds a delegated sub-capsule may run before it is ended, and a [`call-member`](runtime-provided-tools.md#call-member) call may spend offering a busy member the task and waiting for its answer — see [`lifecycle.delegation_deadline_secs`](#lifecycle-delegation-deadline-secs). |
| `lifecycle.deadline_notice_secs` | integer | no | Default: five minutes, or a tenth of the deadline when that is shorter, and at least `1`. Seconds before a [`mur run --deadline`](cli.md#mur-run-deadline) at which the agent is told, once, that the session is about to end — see [The deadline notice](runtime-provided-tools.md#deadline-notice). `0` sends no notice. Must be ≥ 0. Inert without `--deadline`. |
| `lifecycle.keep_working` | object | no | Absent: a task ends at its first turn end. Sends `message` after each turn end, up to `limit` times per task — see [`lifecycle.keep_working`](#lifecycle-keep-working). |
| `lifecycle.keep_working.message` | string | yes, in the block | The text sent as the next user message, verbatim. Must not be blank. |
| `lifecycle.keep_working.limit` | integer | yes, in the block | Most continuations one task receives. Must be ≥ 1. |

#### `exports` { #field-exports }

Opens surfaces of the capsule to processes outside it, on the capsule's HTTP listener:

| Block | Opens |
|---|---|
| `exports.files` | A read-only view of one workdir subtree for the operator, addressed by path — see [Resource plane](resource-plane.md) |
| `exports.peer_files` | A read-only view of one workdir subtree for other capsules, addressed by handle — see [`exports.peer_files`](#field-exports-peer-files) |
| `exports.peer_tasks` | The A2A door to tasks sent by other capsules — see [`exports.peer_tasks`](#field-exports-peer-tasks) |

Each block is its own authoriser: declaring one grants nothing about another, and a capsule may
declare any of them or none.

| Field | Type | Required | Notes |
|---|---|---:|---|
| `exports.files` | block | no | The operator-facing file surface, addressed by path. Absent means every request to it is refused with `no_resource_plane`. |
| `exports.peer_files` | block | no | The peer-facing file surface, addressed by handle — see [`exports.peer_files`](#field-exports-peer-files). Absent means the capsule mints nothing and every redeem is refused with `no_peer_plane`. |
| `exports.peer_tasks` | block | no | Whether the door serves tasks from other capsules — see [`exports.peer_tasks`](#field-exports-peer-tasks). Absent means it does not. |
| `exports.peer_tasks.accept` | boolean | yes | `true` serves peer tasks; `false` refuses them, the same as an absent block. Omitting it inside the block is `E-MAN-003`. |
| `exports.files.root` | string | yes | Subtree of the [work folder](workdir.md#work-folder) the export opens — the directory the agent's tools see as `.`. Must be relative, non-empty and free of `..`. Need not exist when the capsule launches. A root that resolves outside the workdir — because it already exists as a symlink pointing out of it — refuses the launch with `E-CAP-007`. |
| `exports.files.mode` | `read-only` | yes | `read-only` is the only accepted value. |
| `exports.files.max_bytes` | integer or suffixed string | no | Default: `10Mi` (10485760). Per-file read ceiling: a file above it is still listed, with its real size, and refused on read with `too_large`. Accepts a bare byte count or one suffixed `Ki`, `Mi` or `Gi`. Must be greater than zero. |

Declaring an export leaves the achieved containment class unchanged. Containment bounds what the
capsule reaches outward; an export widens what an operator reaches inward, and hands the agent no
capability at all.

#### `exports.peer_files` { #field-exports-peer-files }

Names the one subtree a [peer-file handle](resource-plane.md#peer-plane) may address. Declaring it
gives the agent one [runtime-provided tool](runtime-provided-tools.md), `share-file`, and opens
`GET /resources/peer/<handle>` on the capsule's listener.

| Field | Type | Required | Notes |
|---|---|---:|---|
| `exports.peer_files.root` | string | yes | Subtree of the [work folder](workdir.md#work-folder) a handle may name. Must be relative, non-empty and free of `..`. Need not exist when the capsule launches. A root that resolves outside the workdir refuses the launch with `E-CAP-007`. Independent of `exports.files.root`, and neither is derived from the other. |
| `exports.peer_files.max_ttl` | integer or suffixed string | see notes | Ceiling on a minted handle's lifetime. A bare integer is seconds; `s`, `m` and `h` suffixes are accepted. Must be greater than zero. Optional under `lifecycle.after_task: exit`, where it defaults to `1h`; **required and at most `15m`** under `lifecycle.after_task: sleep`, which otherwise refuses the launch with [`E-CAP-008`](diagnostics.md#e-cap-008). |
| `exports.peer_files.max_bytes` | integer or suffixed string | no | Default: `10Mi` (10485760). Per-file ceiling on a redeemed read; a larger file is refused with `too_large`. Accepts a bare byte count or one suffixed `Ki`, `Mi` or `Gi`. Must be greater than zero. |

There is no `list` verb and no path addressing on this plane. `share-file` clamps a requested `ttl`
down to `max_ttl` and never up.

#### `exports.peer_tasks` { #field-exports-peer-tasks }

States whether the capsule's A2A door serves a task another capsule sends it.

```yaml
exports:
  peer_tasks:
    accept: true
```

| Field | Type | Required | Notes |
|---|---|---:|---|
| `exports.peer_tasks.accept` | boolean | yes | `true` serves peer tasks. `false` refuses them. Default when the block is absent: refuse. `peer_tasks: {}` is `E-MAN-003` naming `exports.peer_tasks.accept`; a value that is not a boolean, or a `peer_tasks` that is not a block, is `E-MAN-002` naming the field. |

A peer task is a request whose `x-murmur-task-origin` header is `peer`. The sending capsule's
runtime stamps it on every message sent through
[`murmur:message/send`](wit-interfaces.md#message-send), and the sending agent cannot set it. A
capsule that does not accept peer tasks answers every such request on its door with:

```http
HTTP/1.1 403 Forbidden
content-type: application/json

{"error":"peer_not_accepted","message":"this capsule does not accept tasks from peers"}
```

The response carries no `www-authenticate` header and is the same for every method, path, task id
and body. The [agent card](agent-card.md#murmur-door-v1) states the setting as `peerTasks`, so a
caller can read it before sending.

| Request | Governed by `exports.peer_tasks` |
|---|---|
| `x-murmur-task-origin: peer`, any method or path on the door | yes |
| No origin header, or any origin but `peer` and `completion` | no — classified `event` and governed by [`network.authentication`](#field-network-authentication) alone |
| A completion a [delegated child](roost-api.md#the-delegation-tool) posts back to this session | no — handed to the task that made the delegation |
| A task delivered to this capsule as a delegated child | no — it carries no origin header |
| `/resources/peer/` | no — governed by [`exports.peer_files`](#field-exports-peer-files) and the handle |
| The [control surface](control-surface.md) | no — governed by the control token |
| `GET /.well-known/agent-card.json` | no — always served |

The origin header is the caller's claim: a caller that does not identify as a peer is not refused
by this setting. On a door declaring `network.authentication` the token is checked first, so a
caller without one is answered `401` before this setting is read.

`exports.peer_tasks` is independent of `network.authentication`: accepting peer tasks neither
requires nor implies a token, and leaves the door's `securitySchemes` and its achieved containment
class unchanged. `murmur:message/send` presents no token, so a door that declares
`network.authentication` answers a message sent through it `401`, whatever `accept` says.

Only the capsule's own `murmur.yaml` sets it: there is no `mur run` flag, environment variable or
[`control.settings`](#field-control) entry for it. It names no peer, so `accept: true` serves every
caller that identifies as a peer.

#### `control` { #field-control }

What a controller holding the session's control token may change while the capsule runs, over the
[control surface](control-surface.md), and what the agent may change about itself. Absent,
`control: {}`, or all three lists empty means the capsule has no control surface and mints no
token. A block that lists only `control.agent_settings` also mints no token.

| Field | Type | Required | Notes |
|---|---|---:|---|
| `control.settings` | list of strings | no | Default: empty. Settings a controller may change, each listed once. Accepted: `inference.max_tokens` and `inference.driver`, both valid only under `inference.transport: http`; `inference.driver` also requires at least one [`inference.alternates`](#inference-alternates) entry. |
| `control.secrets` | list of strings | no | Default: empty. Credential names a controller supplies at run time. Each must be a credential name (uppercase letters, digits and `_`, not starting with a digit), listed once, and referenced by some artifact's `gateway.api_key` as `${NAME}` — but not by the configured `transport: http` driver's. A name keying an alternate driver's gateway is accepted. The global config and the environment are never consulted for a listed name, and `mur doctor` reports it as supplied by a controller rather than as unset. |
| `control.agent_settings` | list of strings | no | Default: empty. Settings the agent may change about itself, each listed once. Accepted: `inference.driver`, on the same terms as in `control.settings`. The grant is the [`switch-driver`](runtime-provided-tools.md#switch-driver) tool. |

`control:` requires an `inference:` block: only an agent capsule serves the listener the control
surface is on. Every refusal above is `E-MAN-003`, naming `control.settings`, `control.secrets`,
`control.agent_settings` or `control` and quoting the entry. An unknown key under `control:` is reported as
[`W-SEC-019`](diagnostics.md#w-sec-019). Nothing a controller sets is persisted: a restart starts
from these values again.

#### `deploy` { #field-deploy }

Settings read by `mur deploy run` alone. `mur run` and `mur doctor` read nothing in `deploy:`.

```yaml
deploy:
  mur_version: "1.0.0"
```

| Field | Type | Required | Notes |
|---|---|---:|---|
| `deploy` | map | no | Absent or `deploy: {}`, every field takes its default. |
| `deploy.mur_version` | string | no | The `mur` release `mur deploy run` downloads from GitHub releases, caches at `~/.murmur/bin/mur-{version}-{platform}` and installs on the target. Default: the version of the `mur` running `mur deploy run`. A blank value takes the default. [`--mur-binary`](cli.md#mur-deploy-run) takes precedence over it. A `mur` built from `main` reports an `X.Y.Z-dev` [version](cli.md#mur-version) that has no release to download, so a deploy run from it needs `deploy.mur_version` or `--mur-binary`, and otherwise fails with `E-DEPLOY-006`. |

A top-level `mur_version` key is refused with [`E-MAN-003`](diagnostics.md#e-man-003), whatever its
value; the pin is written as `deploy.mur_version`.

---

## Choosing a config block { #which-config-block }

Two blocks carry operator-authored settings into an artifact: `inference.driver.config`, and
`config:` on that artifact's own entry in `artifacts:`. One question decides which one a setting
belongs in.

**Does this setting mean anything to a *different* implementation of the same role?**

| Answer | Block | The setting is about |
|---|---|---|
| Yes | [`inference.driver.config`](#field-inference) | Being a driver — endpoints, timeouts, retry behaviour, anything the runtime or any driver would act on. |
| No | [`config:` on that artifact's own entry in `artifacts:`](#artifact-config) | Being *this* artifact — a provider quirk, a feature only this implementation has, a knob whose name would be meaningless to a sibling. |

A driver may use both blocks. A tool or a hook has no `inference:` block, so it only ever uses
`config:`.

The two blocks reach different environments:

| Block | Environment variable | Reaches |
|---|---|---|
| `inference.driver.config` | `MURMUR_INFERENCE_DRIVER_CONFIG` | The driver, every WASM tool and every shell tool in the session. |
| `artifacts[].config` | `MURMUR_ARTIFACT_CONFIG` | The declaring artifact alone. |

For the rest of what a driver or a tool is handed, see
[Driver and tool environment](default-artifacts.md#driver-environment).

### Yes — any driver would act on it { #which-config-block-yes }

A retry budget means the same thing behind any provider, so it describes the role:

```yaml
inference:
  model: claude-opus-4-5
  driver:
    artifact: murmur/driver-anthropic
    config:
      max_retries: 3
      request_timeout_seconds: 60
```

Name a driver in front of another provider instead and both keys still read the same way. These
key names illustrate the question; each driver documents the keys it reads.

### No — the key belongs to one artifact { #which-config-block-no }

`murmur/driver-anthropic` reads `prompt_cache` and `prompt_cache_ttl` from its own entry in
`artifacts:`:

```yaml
artifacts:
  - name: murmur/driver-anthropic
    version: "{{ v.murmur_driver_anthropic }}"
    runtime: driver
    config:
      prompt_cache: enabled
      prompt_cache_ttl: 1h
```

A `cache_control` breakpoint is an Anthropic construct: no sibling driver has one to place, and
other providers cache with no marker at all. Under `inference.driver.config` these two keys would
read as settings every driver understands, which none of them do.

A tool or a hook has only this block, so every setting either role reads arrives through it:

```yaml
artifacts:
  - name: murmur/tool-corpus
    version: 1.0.0
    runtime: tool
    capabilities:
      state: {}
    config:
      read_recent: { default: 20, max: 100 }
```

### The line is convention { #which-config-block-convention }

The runtime validates the shape of both blocks and never their meaning. `inference.driver.config`
must be a mapping or the manifest fails to parse with `E-MAN-003`; `artifacts[].config` must satisfy
[Artifact config shape](#artifact-config-shape) or the launch fails with
[`E-CAP-010`](diagnostics.md#e-cap-010). Neither check can read what a key means, so a setting
written into the wrong block still reaches its artifact and still works. The line holds because
operators keep it, not because the host refuses to cross it.

---

## Inference configuration { #inference-config }

### `transport: http` — WASM driver { #transport-http }

The default transport. Murmur loads a WASM driver artifact and routes every inference call through
it. The driver must be declared in `artifacts:` with `runtime: driver` and a
[`gateway:`](#artifact-gateway) block naming the provider; a driver that is named but not installed
fails with `E-RUN-006`.

```yaml
artifacts:
  - name: murmur/driver-anthropic
    version: "{{ v.murmur_driver_anthropic }}"
    runtime: driver
    gateway:
      endpoint: https://api.anthropic.com
      api_key: ${ANTHROPIC_API_KEY}

inference:
  transport: http
  model: claude-opus-4-5
  max_tokens: 4096        # optional per-turn output cap; default: the driver's models: entry
  max_session_tokens: 2000000   # optional session spend ceiling; default none
  driver:
    artifact: murmur/driver-anthropic
```

#### Turn cap: `inference.max_turns` { #inference-max-turns }

`inference.max_turns` caps the inference turns one task may take, across its reopens and
continuations. A task that spends them ends `max_turns_reached`. Omitted, a task runs until the
model ends it. What bounds a task with no turn cap:

| Bound | Ends |
|---|---|
| [`inference.max_session_tokens`](#inference-max-session-tokens) | The session's inference, once its tokens are spent |
| [`spend.machine_tokens_per_day`](config.md#spend) | Every run on the machine, once the day's tokens are spent |
| [`mur stop`](cli.md#mur-stop), or [`CancelTask`](../how-to/capsules-a2a-messaging.md#cancelling-a-running-task) | The session, or the one task |
| [`lifecycle.delegation_deadline_secs`](#lifecycle-delegation-deadline-secs) of the capsule that delegated the task | A delegated sub-capsule |
| [`mur run --deadline`](cli.md#mur-run-deadline), its formation's, or its parent's | The session, and every sub-capsule it delegates to |
| A deadline the launcher imposes, such as a CI job's timeout | Whatever the launcher kills |

Without a deadline, mur sets no wall-clock limit on a session you launch with `mur run`.

#### Output cap: `inference.max_tokens` { #inference-max-tokens }

`inference.max_tokens` is the per-turn **output** cap — the `max_tokens` field of the payload the
runtime hands the driver, which every driver forwards verbatim to its provider API. It is
provider-agnostic and reaches every driver through the same wire field, so no `driver.config` entry
is needed for it.

[`context.max_tokens`](#field-context) is the context window; `inference.max_tokens` bounds a
single response.

At launch, each driver choice — the primary and every [alternate](#inference-alternates) — resolves
its limits once, from the manifest or else from the [`models:`](#driver-models) entry its driver
declares for its model:

| Limit | Manifest sets it | Else the driver's table lists the model | Else |
|---|---|---|---|
| Output cap | `inference.max_tokens`, for every choice | `max_output_tokens` | The launch is refused with [`E-RUN-052`](diagnostics.md#e-run-052), naming the choice, the model, the driver and `inference.max_tokens` |
| Context window | `context.max_tokens`, for every choice | `context_window` | No window: compaction does not run for that choice, and no seed budget applies |
| Input ceiling | — | `max_input_tokens`, when the entry declares it | None |

Every inference call has one **input budget**: the input ceiling, or the context window less the
output cap that call sends, whichever is smaller. Compaction's
[`threshold`](#field-inference) and the [`context.seed_budget`](#field-context) are fractions of it.

| Term | Value |
|---|---|
| Window | `context.max_tokens`, else the serving driver's `context_window` for the model |
| Output cap | The cap the call sends, from the table below |
| Ceiling | The serving driver's `max_input_tokens` for the model, when declared |
| Budget | `min(ceiling, window − output cap)`; a tie goes to the ceiling |

| Driver entry (window / cap / ceiling) | Manifest | Budget | Basis | Fires at threshold `0.5` |
|---|---|---|---|---|
| 1050000 / 128000 / 922000 | — | 922000 | `max_input_tokens` | 461000 |
| 1050000 / 128000 / — | `inference.max_tokens: 6000` | 1044000 | `context_window_less_output_cap` | 522000 |
| 1050000 / 128000 / 922000 | `context.max_tokens: 200000`, `inference.max_tokens: 8192` | 191808 | `context_window_less_output_cap` | 95904 |
| 1000000 / 8192 / — | `context.max_tokens: 200000`, `inference.max_tokens: 200000` | — | refused, [`E-RUN-055`](diagnostics.md#e-run-055) | — |

| Case | Result |
|---|---|
| No window | No budget: compaction does not run and no seed budget applies |
| A choice's output cap at or above its window | The launch is refused with [`E-RUN-055`](diagnostics.md#e-run-055), naming the window, the cap and the ceiling |
| `inference.compaction.max_tokens` at or above the compaction model's window | The launch is refused with [`E-RUN-055`](diagnostics.md#e-run-055) |
| A controller sets `inference.max_tokens` at or above any choice's window | Refused `422 no_input_budget` on the [control surface](control-surface.md#refusals); the cap in force is unchanged |

A choice whose model the table does not list draws [`W-RUN-009`](diagnostics.md#w-run-009) once at
launch, unless the manifest sets both keys. `session_start` records each choice's numbers, its input
budget and where each came from in
[`model_limits`](observability-schemas.md#session-trace-tracejsonl), every `inference` line records
its call's `input_budget`, and [`mur doctor`](cli.md#mur-doctor) prints them.

Validation is one-sided: `0` is rejected at parse time, but a value larger than a given model's
documented maximum is neither rejected nor clamped — an over-large cap surfaces as the provider's
own error at request time.

| Call | Output cap sent |
|---|---|
| An agent-loop turn | The cap of the choice serving it. A controller's change to `inference.max_tokens` through the [control surface](control-surface.md) is the cap of every later turn, whatever the choice; until one is made, `GET /control` shows the cap the next turn uses |
| A hook's `run-inference` while the hook serves `on-compaction`: the threshold trigger, `mur run --resume … --resume-mode compact`, seed summarization | [`inference.compaction.max_tokens`](#field-inference) when set; otherwise as any other hook's call |
| Any other hook's `run-inference` | The `max_output_tokens` the primary driver's table declares for the model the hook calls — its own model, `inference.compaction.model`, or `inference.model`. When the table lists none, the primary choice's cap |

The anthropic driver caps extended thinking's `budget_tokens` to `max_tokens - 1`, so lowering
this value also squeezes a configured thinking budget.

##### When a turn hits the cap { #inference-max-tokens-reached }

A turn the provider stopped at the cap still produces a result: the session ends `ok`, the runtime
attempts no continuation turn, and every surface carrying the reply marks it as a fragment.
[`W-RUN-001`](diagnostics.md#w-run-001) lists what the turn leaves behind.

A driver reaches this path by reporting `stop_reason: "max_tokens"` in its response. A driver whose
provider names the same condition differently normalizes it to that value.

#### Session spend ceiling: `inference.max_session_tokens` { #inference-max-session-tokens }

`inference.max_session_tokens` caps the tokens one session spends on inference. It counts every
driver call the session makes — each agent turn, and each hook's `run-inference` call, including
compaction and seed summarization. What each call is charged depends on the transport and on what
the provider reported, and each line of the trace's
[`inference`](observability-schemas.md#session-trace-tracejsonl) records shows which:

| Call | Charged |
|---|---|
| `transport: http`, the line carries both `input_tokens_actual` and `output_tokens_actual` | The provider's request, `input_tokens_actual + cached_tokens + cache_write_tokens`, plus its completion, `output_tokens_actual`. Thinking is never added |
| `transport: http`, any other line | `input_tokens + output_tokens`, the runtime's own counts of the request it built and the response it read |
| `transport: http`, a [failed call](observability-schemas.md#inference-failed) | The provider's request plus completion when its error payload reported both, otherwise the runtime's estimate of the request alone |
| `transport: process` | `input_tokens + output_tokens`, the counts the harness reports and its driver relays — see [`transport: process`](#transport-process) |

Under `transport: http`, before each driver call the runtime admits the call only if these three
fit under the ceiling together:

| Part | Value |
|---|---|
| Used | What every earlier call was charged, plus what calls still in flight reserved |
| The call's input | `input_tokens` of the request about to be sent: the runtime's estimate, anchored on the provider's last count of the conversation when there is one |
| The call's most output | The [output cap](#inference-max-tokens) the call sends |

A session with `inference.max_session_tokens` set therefore reserves the whole output cap for every
call: with the cap taken from the driver's table, that is the model's full output ceiling. Set
`inference.max_tokens` to reserve less.

A call that does not fit is never sent. The refusal appears as:

| Surface | What it says |
|---|---|
| `trace.jsonl` | A [`spend_ceiling_reached`](observability-schemas.md#spend-ceiling-reached) line naming the limit, the ceiling, what was used and what the call needed |
| `out/result.txt` in the [records directory](workdir.md#records-directory) | `stopped: spend ceiling reached: …`, ending with `retrying will not get past it` |
| A2A terminal status | `TASK_STATE_FAILED`, with the same `spend ceiling reached: …` text as its message |
| A hook's `run-inference` | `err` with the same text |
| `task_end`, `session_end` | `exit_status: "spend_ceiling_reached"` |
| [`mur run`](cli.md#mur-run-status) | `status:  spend_ceiling_reached`, [`E-RUN-040`](diagnostics.md#e-run-040), exit code `1` |

The first refusal latches. That task ends, and every later call and task in the session is refused
without reaching the provider; the session itself stays up.

A compaction hook whose call is refused decides what happens next:

| The hook returns | Outcome |
|---|---|
| `none` or `replace-context` | The session continues, and the next agent turn's admission decides |
| An error | The task and session end with `exit_status: "spend_ceiling_reached"`, not `failed`, and `out/result.txt` in the records directory reads `stopped: spend ceiling reached: …` |

A compaction hook error that follows no refused call ends the session as `failed`.

A call that is cancelled, fails to dispatch or returns a failed status is charged its input alone.
The sum of a session's `inference` lines never exceeds the ceiling, except by how far a response's
measured `output_tokens` runs past the call's reserved output — the runtime's count of a response
can be larger than the cap the provider applied.

| Setting | Bounds |
|---|---|
| `inference.max_session_tokens` | Tokens across the whole session, input and output |
| [`inference.max_tokens`](#inference-max-tokens) | One response's output |
| [`context.max_tokens`](#field-context) | The conversation's size against the context window, which drives compaction |
| [`spend.machine_tokens_per_day`](config.md#spend) | Tokens across every run on the machine per UTC day |

Under `transport: process` there is nothing to admit: the harness reaches its provider itself, and
the runtime learns what a turn cost only once that turn is over. Each closed turn is charged and
the ceiling is asked afterwards, so a ceiling stops the *next* turn rather than the one that
reached it, and the session's total can overshoot by at most one harness turn. The run stops the
same way it does under `http` — the same sentence, the same `spend_ceiling_reached` line, the same
`exit_status` — and the harness is killed at once rather than given its exit grace. A ceiling set
against a process driver whose `describe()` reports no usage is refused at launch with
[`E-RUN-038`](diagnostics.md#e-run-038).

A delegated child is bounded by its own `inference.max_session_tokens` and by the machine ceiling,
not by its parent's session ceiling.

#### Retrying transient failures: `inference.retry` { #inference-retry }

A driver call that fails transiently is made again: the same request, through the same driver
choice, after a wait. This covers every agent turn and every hook's
[`run-inference`](wit-interfaces.md#murmurruntimeinference), which is how compaction calls the
model. The model never sees the failed attempt, no hook fires for it, and nothing from it is
committed to the conversation, the continuation or `out/result.txt`.

```yaml
inference:
  retry:
    max_retries: 6          # 0–20; 0 disables retrying
    initial_delay_ms: 1000  # 1–60000
    max_delay_ms: 60000     # 1–3600000, and >= initial_delay_ms
```

| Key | Type | Default | Bounds |
|---|---|---:|---|
| `max_retries` | integer | `6` | 0–20. The most retries one call makes after its first attempt; `0` disables retrying |
| `initial_delay_ms` | integer | `1000` | 1–60000. The backoff before the first retry |
| `max_delay_ms` | integer | `60000` | 1–3600000, and at least `initial_delay_ms`. The longest backoff, and the longest wait a provider may ask for |

A value outside its bounds fails the manifest parse with `E-MAN-003` naming the key, as
`inference.retry.<key>`; a `max_delay_ms` below `initial_delay_ms` names `inference.retry.max_delay_ms`.

A failure is transient when one of these holds, checked in order:

| Rule | Retried |
|---|---|
| The provider rejected the credential | Never |
| The driver's result carries [`transient: "false"`](wit-interfaces.md#murmurtoolrun) | No |
| The driver's result carries `transient: "true"`, as a driver reports a stream that broke after the provider answered `200` | Yes |
| The connection broke before a response arrived: reset, terminated, timed out, or the response was incomplete | Yes |
| The provider answered `408`, `429`, or a `5xx` other than `501` and `505` | Yes |
| Anything else, including a refused connection, a DNS or TLS failure, and every other `4xx` | No |

The wait before retry *k* (counting from 0) is drawn uniformly from `[d/2, d]`, where
`d = min(max_delay_ms, initial_delay_ms × 2^k)`. When the provider's response carried
`retry-after-ms` or `retry-after` (seconds, or an HTTP date), the wait is exactly that value.

The runtime stops retrying, and the call fails as it would have without retries, in four cases:

| Case | `retry_stopped` on the call's `inference` record |
|---|---|
| `max_retries` retries were already made | `max_retries` |
| The provider asked for a wait longer than `max_delay_ms` | `retry_after` |
| The wait would end at or after the session's [deadline](cli.md#mur-run-deadline) | `deadline` |
| The [spend ceiling](#inference-max-session-tokens) refuses the next attempt | None: the ordinary `spend_ceiling_reached` line and ending are written |

When a failed call was retried, or one of the first three cases stopped its retrying, the task's
failure reason ends with the number of attempts and the stop. The reason is what
[`task_failed`](observability-schemas.md#task-failed) records, `out/result.txt` holds and
[`E-RUN-040`](diagnostics.md#e-run-040) prints:

| The call | Reason |
|---|---|
| Took 3 attempts, then stopped at `max_retries` | `HTTP 503: … (3 attempts; retry stopped: max_retries)` |
| Was not retried, because the wait would pass the deadline | `HTTP 429: … (1 attempt; retry stopped: deadline)` |
| Was retried once, then failed on a failure that is not transient | `HTTP 400: … (2 attempts)` |

The failed call's `inference` record keeps the provider's error text alone in `error`, and names the
same facts in `attempts` and `retry_stopped`.

Every attempt is admitted against the spend ceilings before it is sent, and no admission is held
during a wait. What a failed attempt that is retried costs:

| The attempt failed on | Charged |
|---|---|
| A provider status, or a connection that broke before a response | Nothing |
| A failure the driver marked transient | What any [failed call](observability-schemas.md#inference-failed) is charged: the provider's report from its error payload, otherwise the request's estimate |

A cancel during a wait ends the task as canceled, in phase `inference`. A hook's call has no cancel
of its own and does not consult the hook's `limits.deadline_seconds`: a wait that outlasts it ends
the hook as a slow provider call would.

Text a failed attempt already streamed to an A2A client stays on the stream, followed by a final
empty text frame; the answer the task commits is the retry's.

Each retry writes one [`inference_retry`](observability-schemas.md#inference-retry) line to
`trace.jsonl`. The call keeps one `inference` record, which carries `attempts` when it took more
than one. Retries do not count as turns, against `inference.max_turns`, or in the turn and token
totals.

Refused under `transport: process` with `E-MAN-003`: `is not valid with transport: process; the
harness retries its own provider calls`.

#### Tool refresh: `inference.tool_refresh` { #inference-tool-refresh }

The tool list the model is offered is built from the session's installed artifacts at the start of
each task, and every inference call of that task sends it unchanged until the installed set
changes. A change the model made itself, with [`pull-artifact`](runtime-provided-tools.md#pull-artifact)
or [`remove-artifact`](runtime-provided-tools.md#remove-artifact), is in the tool list of its next
inference call, whatever this field says: the model asked for it and calls it next. That call
misses the provider's prompt cache for its whole prompt, and its `tools_refreshed` line names the
trigger `agent`.

Any other change, such as a script capsule's [`manage.pull`](wit-interfaces.md#murmurartifact-managermanage),
is added at the boundary `inference.tool_refresh` names:

| Value | The installed artifact is offered from | Cost |
|---|---|---|
| `compaction` (default) | The first inference call after a compaction replaces the context, or the start of the next task, whichever comes first | The tool list and system prompt miss the provider's prompt cache on that call. The conversation already misses it after a compaction |
| `immediate` | The next inference call | That call misses the provider's prompt cache for its whole prompt |

The tool list sits at the head of the prompt the provider caches, so any change to it is a cache
miss for everything after it — see [Measured cost](#inference-tool-refresh-cost).

Compaction runs only when the conversation reaches the [context window](#field-context), which
may be many turns away or never within a task.

A session that installs nothing sends the same tool list on every call under either value.

What the model can do with an artifact once it is offered depends on its runtime:

| Pulled artifact | Offered | Callable |
|---|---|---|
| `runtime: skill` | Yes, with no input parameters | Yes: calling it returns its `skill.md` |
| The skill named by [`inference.system_prompt_artifact`](#inference-system-prompt-artifact) | No | — |
| A WASM or native tool not declared in `artifacts:` | Yes | Yes: a pulled tool joins the dispatch allowlist |
| `runtime: driver` or `runtime: hook` | No | — |

Each refresh writes a [`tools_refreshed`](observability-schemas.md#tools-refreshed) line to
`trace.jsonl` just before the `inference` line of the call that carries the new list, and
`session_start.tool_refresh` records the value in effect.

Refused under `transport: process` with `E-MAN-003`: the harness owns its tool list.

##### Measured cost { #inference-tool-refresh-cost }

Three consecutive calls of one conversation against OpenAI `gpt-5.6-luna` through
`murmur/driver-openai`, each within seconds of the last, with the same system prompt and each
call's messages beginning with every message of the call before:

| Call | Tool list | Input tokens | Read from cache |
|---|---|---:|---:|
| Before the install | One skill | 3,325 | 3,303 |
| First call carrying the new list | One skill added, sorting first | 3,359 | 0 |
| The call after | Unchanged since the previous call | 3,378 | 3,356 |

The same session's tool list and system prompt count 347 tokens by the runtime's own estimate: that
is what a `compaction` refresh gives up, against the whole prompt for an `immediate` one.

### `transport: process` — harness subprocess { #transport-process }

Murmur runs a harness CLI as a subprocess and communicates over stdin/stdout, driving it through
the [process driver](#process-driver) the manifest names. Authentication is whatever that harness
is already configured with: credentials under this transport are the harness's own, and murmur
neither holds nor controls them.

```yaml
artifacts:
  - name: local/my-process-driver
    version: "0.1.0"
    runtime: driver
capabilities:
  env:
    allow: [HOME]        # every variable the driver's describe() requires
inference:
  transport: process
  driver:
    artifact: local/my-process-driver
  max_turns: 10
```

| Behaviour | Details |
|---|---|
| Binary | `inference.command` when set, otherwise the executable the driver's `describe()` names, looked up on `PATH`. A binary that cannot be resolved refuses the launch with [`E-RUN-006`](diagnostics.md#e-run-006). |
| Version check | The binary is run with the driver's version arguments. A version the driver was not tested against, or one that cannot be read, is [`W-RUN-002`](diagnostics.md#w-run-002) and the run proceeds. |
| Tool dispatch | When the capsule declares tool artifacts, murmur stands up a loopback tool server and hands the driver its URL, its token and the capsule's tool names; the driver points its harness at them, the model calls those tools, and murmur executes them. With no tools declared, the harness runs with no tools. |
| Environment | The harness starts with an empty environment, then receives the host values of the names [`capabilities.env.allow`](#field-capabilities) declares, whatever the driver's launch plan sets, the host's `USER` and `LOGNAME`, and the session's runtime and records directories. See [Process environment](#process-environment). |
| Turn limit | One model action — a message or a tool call — is one turn, bounded by `inference.max_turns`. A turn past the limit ends the run with [`E-RUN-033`](diagnostics.md#e-run-033). |
| Inactivity limit | A run with neither a line of harness output nor a tool call for 600 seconds is killed with [`E-RUN-035`](diagnostics.md#e-run-035). A harness that is working is never interrupted for taking a long time. |
| Interrupt grace | A harness sent a graceful interrupt has 10 seconds to end on its own before it is killed. Both this and the inactivity limit are fixed, not manifest settings. |
| Cancellation | [`CancelTask`](../how-to/capsules-a2a-messaging.md#cancelling-a-running-task) interrupts the harness the way the driver's `describe()` declares: `stdin-message` writes the launch plan's interrupt bytes to its stdin, `signal-int` sends it `SIGINT`, and `unsupported` kills it outright. A graceful interrupt gets the interrupt grace above. The task is `TASK_STATE_CANCELED` either way, and the capsule, its queue and its conversation keep going. |
| Result | The harness's final result text is written to `out/result.txt` in the session's [records directory](workdir.md#records-directory). A turn the harness reports as failed is [`E-RUN-033`](diagnostics.md#e-run-033), naming why. |
| Observability | Session, inference and tool hooks, `trace.jsonl` and OTel spans are all emitted normally. |
| Token counts | The harness's own, as its driver reports them, in the `inference` line's `input_tokens` and `output_tokens` — this transport has no runtime estimate, so `input_tokens_actual` and `output_tokens_actual` stay absent. A driver that reports no usage leaves both counts absent, which is not the same as zero. |
| Spend ceilings | [`inference.max_session_tokens`](#inference-max-session-tokens) and [`spend.machine_tokens_per_day`](config.md#spend) are enforced against those reported counts. |
| Hook `run-inference` | Unavailable: a hook's [`run-inference`](wit-interfaces.md#murmurruntimeinference) call returns an error, sends nothing, counts toward no spend ceiling, and writes nothing to the trace. |
| Compaction | Does not run. `context.max_tokens` and `inference.compaction` parse but are inert under this transport, except `inference.compaction.max_tokens` and `inference.compaction.driver_config`, which are refused; the harness manages its own context. |
| Tool refresh | The harness's. [`inference.tool_refresh`](#inference-tool-refresh) is refused with `E-MAN-003`; the tool list is bound once per task. |
| Context seeding | Does not run. The `context.seed_budget` keys parse but are inert, and a `seed-context` an `on-task-start` hook returns is recorded as a rejected [`context_seed`](observability-schemas.md#context-seed) with `reason: "unsupported_transport"`. |
| Conversation | The harness's. Murmur keeps no message list and writes no `conversation.jsonl`; it maps each context to the harness's own session id in a [harness session map](workdir.md#harness-session-map), which is what [`lifecycle.conversation: threaded`](#lifecycle-conversation) and [`mur run --resume`](cli.md#mur-run) continue. A session the harness cannot find fails the turn with [`E-RUN-036`](diagnostics.md#e-run-036). |

#### Process driver { #process-driver }

A process driver is a `runtime: driver` artifact that exports the
[`murmur:driver/process`](wit-interfaces.md#murmurdriverprocess) interface: it knows how to drive
one harness. It is granted nothing: no environment, no files, no network, and no Murmur host
interface.
It needs no `inference_auth:` block.

```yaml
capabilities:
  env:
    allow: [HOME]            # every variable the driver's describe() requires
artifacts:
  - name: local/my-process-driver
    version: "0.1.0"
    runtime: driver
inference:
  transport: process
  driver:
    artifact: local/my-process-driver
    config: {}               # optional; handed to the driver as JSON
```

`mur run` loads the named driver and checks it in this order. The first check that fails refuses the
launch:

| Check | Refused with |
|---|---|
| The artifact exports `murmur:driver/process` at the version the served [WIT level](wit-levels.md) lists | [`E-WIT-LEVEL-001`](diagnostics.md#e-wit-level-001) when it exports only another version; [`E-RUN-029`](diagnostics.md#e-run-029) when it exports none |
| It instantiates with no grants, and `describe()` returns usable variable names | [`E-RUN-032`](diagnostics.md#e-run-032) |
| Every variable `describe()` requires is declared in `capabilities.env.allow` | [`E-CAP-019`](diagnostics.md#e-cap-019) |
| `describe()` reports usage, or no spend ceiling is in effect | [`E-RUN-038`](diagnostics.md#e-run-038) |

A `transport: http` manifest whose driver exports the process interface is refused with
[`E-RUN-030`](diagnostics.md#e-run-030).

### `inference.system_prompt` / `system_prompt_file` { #inference-system-prompt }

`inference.system_prompt` and `inference.system_prompt_file` inject a static prompt as the
top-level `system` parameter on every API call, including the first turn and every subsequent turn
in a multi-turn session.

```yaml
# Option A: inline text
inference:
  system_prompt: |
    You are a strict code reviewer.
    Always respond in JSON.

# Option B: load from a file next to murmur.yaml
inference:
  system_prompt_file: conventions.md
```

Rules:

- **At most one prompt source.** Setting more than one of `system_prompt`, `system_prompt_file`
  and [`system_prompt_artifact`](#inference-system-prompt-artifact) fails with `E-MAN-003`.
- **File paths are relative to the directory containing `murmur.yaml`**, not to the work folder.
- **The file is read once at launch**, before any inference call. If it is missing or unreadable,
  `mur run` exits with `E-RUN-009` before making any API call.
- **File content is used verbatim** — whitespace is preserved and no trimming is applied. Inline
  `system_prompt` text is trimmed at parse time.
- With no prompt source set, no `system` parameter is emitted.
- **A run can override whatever is declared here.** [`mur run --system-prompt`](cli.md#mur-run)
  replaces all three sources for that invocation only and leaves `murmur.yaml` untouched.

---

## Hook artifacts { #hook-artifacts }

Declare hook artifacts with `runtime: hook` in `artifacts:`. Hook artifacts are WASM components
that implement `murmur:hook/lifecycle`; the runtime calls them at fixed lifecycle points, and what
it does with a successful return value is set by the hook's own `commit_policy`.

```yaml
artifacts:
  - name: murmur/hook-debug
    version: "{{ v.murmur_hook_debug }}"
    runtime: hook

observability:
  otel_endpoint: "http://localhost:4318"
```

| Behaviour | Details |
|---|---|
| Model visibility | Hook artifacts are not included in the tool inventory or the `MURMUR.md` installed-tool list. |
| Invocation order | Multiple hooks are invoked in manifest declaration order for each event. |
| Failure handling | A hook that returns an error does not abort the agent loop. The error is appended to `logs/hook-<name>.log` in the session's [records directory](workdir.md#records-directory). For an `execution_mode: async` hook it is also recorded as a `hook_dispatch_error` event in `trace.jsonl`, since the log is otherwise the only place it appears. A blocking hook's error is additionally surfaced to the agent loop, which is fatal only for compaction. |
| Workdir access | A hook sees one directory, and only where its entry in the capsule manifest grants a `filesystem.scope` — see [Hook capabilities](#hook-capabilities). |
| Reference hook | `murmur/hook-debug` writes one JSON object per event to `workdir/hook-debug.jsonl`. |
| Call deadline | Each hook lifecycle call gets its own wall-clock budget: `capabilities.limits.deadline_seconds` on the hook's own `artifacts:` entry, else the capsule-wide `capabilities.limits.deadline_seconds`, else 30 seconds — well below the capsule-wide 600-second default, so one wedged hook cannot stall a session for most of ten minutes per event. Time a hook spends waiting on its own `run-inference` calls counts against it. |

### Hook contract fields { #hook-contract-fields }

These three fields live in the **hook artifact's own** `murmur.yaml`, not in the capsule manifest
that installs it. They are the hook author's declaration of what the hook does, so a capsule
operator cannot change them by editing their own manifest.

| Field | Values | Default | Meaning |
|---|---|---|---|
| `binding` | `on-stage`, `on-session-start`, `on-task-start`, `on-inference`, `on-tool-call`, `on-shell`, `on-compaction`, `on-task-end`, `on-session-end` | absent — the hook receives every event | Which lifecycle event(s) the hook is dispatched for. |
| `execution_mode` | `blocking`, `async` | `blocking` | Whether the agent loop waits for the hook. A binding that commits an arm must be `blocking`, so `async` requires `commit_policy: none`. `on-stage` must be `blocking`. |
| `commit_policy` | `none`, `write-manifests`, `replace-context`, `reopen-task`, `seed-context`, `deny` | `none` | What the runtime does with the hook's successful output. |

**`binding` is the single source of truth for what a hook can commit**, and `commit_policy` is
checked against it when the capsule is staged. Each binding honors exactly one output, so it admits
exactly one `commit_policy` — plus `none`, which is always valid and means the hook only observes:

| `binding` | Valid `commit_policy` |
|---|---|
| `on-stage` | `write-manifests` or `none` |
| `on-compaction` | `replace-context` or `none` |
| `on-task-end` | `reopen-task` or `none` |
| `on-task-start` | `seed-context` or `none` |
| `on-shell` | `deny` or `none` |
| `on-tool-call` | `deny` or `none` |
| `on-inference` | `none` only — `on-inference` commits an `artifact` output, which has no `commit_policy` spelling |
| `on-session-start`, `on-session-end` | `none` only — these events commit nothing |
| absent (all events) | any value except `deny` |

Declaring a `commit_policy` the `binding` cannot honor is an error at capsule-staging time, before
the hook component is compiled or run. For example `binding: on-task-end` with
`commit_policy: replace-context` fails with:

```
hook my-hook@1.0.0 invalid config: commit_policy 'replace-context' is not valid for binding
'on-task-end'; binding 'on-task-end' honors commit_policy 'reopen-task'
```

`commit_policy: deny` additionally requires an explicit `binding:` of `on-shell` or
`on-tool-call`. It is the one policy an omitted `binding:` cannot carry: `deny` is answered at a
decision point standing in front of a call, and a hook that does not name which of the two events
it gates would be asked to decide on calls it was never written to judge. Omitting `binding:` with
`commit_policy: deny` fails with:

```
hook my-hook@1.0.0 invalid config: commit_policy 'deny' requires an explicit binding: 'on-shell'
or 'on-tool-call'; a hook with no binding: is dispatched at every event, including decision
points it was not written to decide
```

A hook declaring `commit_policy: deny` is a **policy hook**. It is called immediately before the
call it gates is dispatched, is handed the resolved identity of what is about to run, and returning
`deny(reason)` means the call does not happen. See
[Policy hooks](../concepts/hooks.md#policy-hooks) for what a policy hook decides on and what
happens when one fails.

See [What each handler can commit](wit-interfaces.md#what-each-handler-can-commit) for the runtime
side of the same table.

### Async hook execution { #hook-overflow }

An `execution_mode: async` hook is instantiated once for the session and reused for every event:
state the hook keeps in memory — a running counter, a buffered span, an open client — survives
across calls. Each async hook has its own bounded, ordered job queue and a dedicated worker that
drains it one call at a time, so dispatching an event to an async hook returns immediately and
calls to that hook are never reordered or run concurrently with each other. Every async hook's
queue is drained, and its in-flight call awaited, before the session ends, so a queued
`on-session-end` call — a final metrics export, for example — is not lost when the session tears
down. A hook still working when the drain's bounded budget runs out is abandoned and reported the
same way any other hook fault is.

`on_overflow:` on the capsule's own `artifacts:` entry controls what happens when that hook's queue
is full, which only happens when the hook is falling behind the rate of lifecycle events:

| Value | Behaviour |
|---|---|
| `drop` (default) | The event is discarded and counted. Dispatch never waits, so a slow or stuck async hook cannot delay the agent loop. |
| `block` | Dispatch waits for the hook's worker to make room. No event is lost, at the cost of putting a slow hook back on the critical path. |

```yaml
artifacts:
  - name: murmur/hook-grafana
    version: "{{ v.murmur_hook_grafana }}"
    runtime: hook
    on_overflow: block   # wait for room instead of dropping events under load
```

An async hook's output is always discarded, even an arm that would be honored for a blocking hook
on the same binding: nothing waits for its answer, so there is nowhere to apply it. That is why
`execution_mode: async` is only valid with `commit_policy: none`.

---

## Capability validation { #capability-validation }

### Network allow entries

Accepted forms:

- full URL: `https://api.example.com`
- host only: `api.example.com`
- host + port: `localhost:11434`

A URL entry must carry no path, query or fragment, and its scheme must be `http` or `https`.
Anything else fails with [`E-CAP-001`](diagnostics.md#e-cap-001).

A request an artifact sends through its [credential gateway](#artifact-gateway) does not consult
this list, so a gateway's upstream need not appear in it.

### Filesystem scope

- must be relative, not absolute
- cannot escape the workdir via `..`

A scope that breaks either rule fails with [`E-CAP-002`](diagnostics.md#e-cap-002). See
[What bounds a WASM artifact](containment.md#artifact-boundary).

### Read-only paths { #read-only-paths }

`capabilities.filesystem.read_only` lists workdir-relative subtrees the capsule reads but does not
write:

```yaml
capabilities:
  filesystem:
    read_only:
      - tests
      - bench/fixtures
```

Each entry follows the same two rules `scope` does — relative, and no `..` that escapes the
workdir — and an entry that breaks either fails with [`E-CAP-012`](diagnostics.md#e-cap-012) at
staging, before any registry pull, workdir creation or component instantiation. An empty or
whitespace-only entry is dropped at parse.

An entry names a subtree of the workdir root, matched a path component at a time:

| Entry | Covers | Does not cover |
|---|---|---|
| `tests` | `tests`, `tests/a`, `tests/a/b`, `./tests/a`, `<workdir>/tests/a` | `tests2`, `testsuite/a`, `atests`, `build/tests` |

A candidate path is resolved against the workdir before it is matched: an absolute path, a
relative one and a symlink into the subtree all produce the same rule and the same recorded path.
A path that resolves outside the workdir is not covered by any entry — what reaches it is decided
by the directory the artifact is granted and, on a kernel-enforcing host, by Landlock.

**What the runtime refuses.** The check runs on the resolved call, before dispatch and before any
[policy hook](../concepts/hooks.md#policy-hooks) is asked. It refuses what it can positively
identify as a write:

| Call | Identified as a write when |
|---|---|
| Shell (`-c` script body) | A redirection — `>`, `>>`, `>\|`, `&>`, `&>>`, `N>`, `N>>` — names a covered path |
| Shell | A covered path is in a write-target position of `tee`, `rm`, `rmdir`, `unlink`, `shred`, `truncate`, `mkdir`, `touch`, `chmod`, `chown`, `patch` (any non-flag argument), `mv`, `cp`, `install`, `ln` (the last one), `sed` (with `-i`/`--in-place`), or `dd` (the `of=` value) |
| Tool | A covered path is the string value of `dest`, `dest_path`, `destination`, `destination_path`, `target_path`, `output_path`, `out_path`, `new_path` or `to` |
| Tool | A covered path is the string value of `path`, `file_path`, `filepath`, `filename` or `file`, **and** the same JSON object also carries `content`, `contents`, `text`, `data`, `body`, `new_str`, `new_string`, `replacement`, `patch` or `diff` |

Tool-input keys are matched case-insensitively with `-` and `_` folded, and the pairing rule is
evaluated per JSON object, so a nested edit list is read the same way a flat input is. A path with
no content key beside it is a read.

A refused call does not run: no `shell` record and no `tool_call` record is written, and the model
receives an error naming the path, the rule, that nothing ran, and that the path is still
readable. See [`protected_path_denied`](observability-schemas.md#protected-path-denied).

**What a tool declares about its own input.** A tool artifact says which of its inputs are
filesystem destinations, which are payload it only stores, and which destinations it derives from
an input rather than reading whole — with JSON Schema's `format` keyword on its own properties and
one keyword at the root of its own [`input_schema`](#input-schema):

| Declaration | Written on | Effect |
|---|---|---|
| `"format": "murmur-destination"` | A string property | The value at that location is checked against every `read_only` entry, wherever in the input it sits |
| `"format": "murmur-opaque"` | An object or array property | The key-name rules above do not descend into that subtree |
| `"murmur-destinations": [...]` | The schema root, beside `type` and `properties` | Every entry is resolved against the input and checked the same way |

```yaml
input_schema: |
  {"type":"object","properties":{
    "edits":{"type":"array","items":{"type":"object","properties":{
      "path":{"type":"string","format":"murmur-destination"}}}},
    "note":{"type":"object","format":"murmur-opaque"}}}
```

An annotation refines where the runtime looks, never whether it refuses: nothing in a schema
permits a path, and every refusal is still decided by the operator's own `read_only` entries.

| Case | Behaviour |
|---|---|
| A tool that declares nothing | Judged by key name, exactly as the table above describes |
| `murmur-opaque` on a string property | Ignored; the key-name rules keep running on the object that carries it |
| `murmur-destination` inside a subtree marked `murmur-opaque` | Still checked |
| `murmur-opaque` on the schema's top level | The key-name rules do not run on that tool's input at all; only its declared destinations are checked |
| An annotation behind a `$ref` | Not resolved, so that tool keeps the key-name rules |

A refusal a declared destination triggered names the location in the model's error and in the
trace record: `edits[].path` for the schema above.

**Destinations derived from an input.** A tool whose real write is a fixed path *under* one of its
inputs — a graph database under `<repo_path>/.murmur`, where `repo_path` is a read source —
declares it at the schema root, in a `murmur-destinations` array:

```yaml
input_schema: |
  {"type":"object",
   "properties":{"repo_path":{"type":"string"},"file":{"type":"string"}},
   "murmur-destinations":["{repo_path}/.murmur"]}
```

Each entry is a braced input location, optionally followed by `/` and a relative suffix:

| Entry | Means |
|---|---|
| `{repo_path}/.murmur` | `.murmur` under every string at `repo_path` |
| `{repo_path}` | Every string at `repo_path`, unmodified |
| `{paths[]}` | Every string in the `paths` array |
| `{edits[].path}` | Every `path` string in the `edits` array |

The empty list, `"murmur-destinations": []`, declares that the tool writes nothing named by or
derived from its input.

The location is spelled the way a refusal names it — object keys joined with `.`, `[]` for an array
step. The braces are mandatory, so `repo_path/.murmur` is malformed. A suffix is relative and stays
inside what the location named: a leading `/`, an empty component, a `.` or `..` component and a
`\` anywhere are each rejected, as is an entry naming a location the schema does not declare.

Validity is all-or-nothing. One malformed or dangling entry, or a value that is not an array of
strings, discards the whole list: no derived destination is checked and the tool is judged exactly
as one that declared no list at all.

A capsule that declares `read_only` and installs a tool whose schema leaves a path-shaped or
destination-shaped property to the key-name rules fires
[`W-SEC-018`](diagnostics.md#w-sec-018) at staging, naming the tool and every such property. Which
declaration silences which property:

| Declaration | Silences the warning for |
|---|---|
| `"format": "murmur-destination"` | The property it sits on, and no sibling |
| `"format": "murmur-opaque"` | The property it sits on, only where the schema declares that property an object or array — the only place it takes effect — and everything inside that subtree |
| `"murmur-destinations": [...]`, the empty list included | The whole tool |

**What no declaration can say.** A property that is a destination under some input values and a
read source under others: a `repo` a tool writes under `checkout`, `reset --hard`, `stash pop`,
`pull`, `merge` and `cherry_pick`, and reads under `log`, `diff`, `show` and `status`. Neither a
`format` value nor a `murmur-destinations` entry takes a condition, so such a property stays
undeclared and falls back on its name: it is judged by the key-name rules if its name is in the
tables above, and not checked at all if it is not. `repo` is in neither table, so a tool with that
shape is contained there only by `capabilities.filesystem.scope`.

**What it does not refuse.** Everything the dispatch check cannot positively identify — command
substitution, `eval`, a binary outside the table above, and an allowlisted interpreter's own file
I/O. Declaring `read_only` alongside an interpreter in `capabilities.shell.allow` fires
[`W-SEC-017`](diagnostics.md#w-sec-017) at staging, naming that binary. Nor does it descend into a
subtree a tool declared `murmur-opaque`, so a destination the same tool did not declare is not
seen there. The declaration is also not a defence against a malicious artifact: see
[Access control](../concepts/access-control.md#read-only-paths).

`mur run --explain-scope` and `mur doctor` both print the declared subtrees and whether the
protection is enforced for every call the runtime can read as a write or advisory against a named
interpreter, so it can be checked before a capsule is launched.

### State store name { #state-store-name }

`capabilities.state.store` names one directory under `~/.murmur/state/`, so it is a single path
segment:

- non-empty
- contains no `/`
- is not `.` or `..`, and does not begin with `.`
- is not absolute

A name that breaks any of these fails with [`E-CAP-009`](diagnostics.md#e-cap-009) at staging,
before any registry pull, workdir creation or component instantiation, and creates nothing under
`~/.murmur/state/`. `mur run --explain-scope` refuses the same names with the same code. Omitting
`store:` uses the capsule name, which is read from your own manifest and never from the artifact's
bundled one.

### Artifact config shape { #artifact-config-shape }

`artifacts[].config` is delivered as one environment variable holding JSON, so the runtime checks
that the block can travel that way:

| Rule | Refusal |
|---|---|
| The block is a mapping | [`E-CAP-010`](diagnostics.md#e-cap-010) |
| Every key in that mapping is a string | [`E-CAP-010`](diagnostics.md#e-cap-010) |
| The block serializes to JSON | [`E-CAP-010`](diagnostics.md#e-cap-010) |
| The serialized JSON is at most 65536 bytes | [`E-CAP-010`](diagnostics.md#e-cap-010) |

A block that breaks any of these fails at staging, before any registry pull, workdir creation or
component instantiation, and leaves no work folder or records directory behind. `mur run --explain-scope` refuses
the same blocks with the same code. An oversized block is refused rather than truncated, and the
message names both the size it serialized to and the limit.

`config:` written with no value under it is an empty block, not an absent key, and is refused on
the same terms. Omit the key to deliver no variable.

### Shell allow

- Each entry is a bare binary name (`bash`, `jq`): one path component, with no `/`, no
  whitespace, and not `.` or `..`. The session runs the binary of that name on the `PATH` `mur run`
  starts with.
- An entry that is not a bare name is refused with
  [`E-MAN-003`](diagnostics.md#e-man-003) when the manifest is read, before anything is staged or
  installed. To allow a binary outside the usual `PATH`, name it bare and put its directory on
  `PATH` for `mur run`:

  ```bash
  PATH=/opt/uv/bin:$PATH mur run --task "…"
  ```

- A `capabilities.shell` block present with an empty `allow` list is rejected at parse time.
- A synthetic tool manifest is written to `tools/<binary>/murmur.yaml` in the session's
  [runtime directory](workdir.md#runtime-directory) at session start for
  each listed binary; the agent discovers these alongside artifact-backed tools.
- The tool's `command` argument carries the arguments alone — the runtime supplies the binary. A
  shell interpreter (`bash`, `sh`, `zsh`, `fish`, `dash`, `ksh`) is the exception: its `command` is
  a whole shell line, run as the interpreter's `-c` argument.
- The result the agent reads back opens with the command line that ran, then the exit code and the
  two output streams:

  ```text
  $ ls -d .
  Exit code: 0
  Stdout:
  .

  Stderr:
  ```

  The name on the `$` line is the allowlist entry, so an interpreter's line shows the shell line
  on its own (`$ echo hi`). When the runtime attributes the command's failure to one
  `capabilities.resources` limit, a line naming that limit and what to do follows the result; see
  [Which limit a subprocess hit](resource-limits.md#which-limit).

### Install entries { #install-entries }

Each entry in `capabilities.install.skill` and `capabilities.install.tool` takes one of four
forms:

| Form | Example | Matches |
|---|---|---|
| Exact full name | `ana/code-review` | That artifact only |
| Scope and prefix | `ana/style-*` | Every artifact in the scope whose bare name begins with the prefix |
| Scope | `ana/*` | Every artifact in the scope |
| Wildcard | `*` | Every artifact of every scope |

- An exact name is a full name, `<scope>/<name>` — see
  [Artifact names](../concepts/registry.md#artifact-names).
- A prefix uses the same characters as a name, does not begin with `-`, and ends in a single `*`.
  It may end in `-` before the `*`.
- An entry with no scope, such as `code-review` or `style-*`, is refused at parse with a hint
  naming its scoped forms. Anything else is refused too, naming the key and the entry: a `*`
  anywhere but the end, a second `*`, uppercase, a second `/`, `:`, whitespace, or an empty string.
- Only `*` reaches across scopes, so a child capsule's entries are covered by its parent's only
  when they stay within the scopes its parent's entries name.
- A block that names no entry in either list is refused at parse, naming `capabilities.install`.
  Omit the block to grant no install.
- An unknown key inside the block, such as `driver:`, grants nothing and warns with
  [`W-SEC-019`](diagnostics.md#w-sec-019).

---

## Retention { #retention }

Three stores grow as capsules run:

| Store | Bounded by | Enforced | Each deletion is reported as |
|---|---|---|---|
| Sessions: each [records directory](workdir.md#records-directory) under `~/.murmur/records/`, and the work folder the runtime created for a launch without `--workdir` | `trace.retain` | At launch, by the runtime | A [`retention` event](observability-schemas.md#retention) in the running session's trace |
| [Conversation records](workdir.md#the-conversation-record) under `~/.murmur/conversations/` | `context.retain` | At launch, by the runtime | A `retention` event in the running session's trace |
| [Formation directories](roster.md#formation-retention) under `~/.murmur/formations/`, each with its records subtree under `~/.murmur/records/` | The roster entry member's `trace.retain` | At the end of every `mur run --roster`, by the launcher | A `[mur run] retention:` line on the launcher's stderr |

**There are no defaults. A capsule with no `retain:` block deletes nothing, ever.**

| Rule | Effect |
|---|---|
| The block is omitted | Nothing is deleted. The stores it would bound grow without limit. |
| The block is present but empty (`retain: {}`, or `retain:` with nothing under it) | Refused at parse time, naming the block. Omitting the block is how a capsule declares no policy. |
| Both keys are present | ANDed. A session, record or formation directory survives only if it is inside both limits. |
| A key is `0` | Refused at parse time, naming the key. |

Durations are written as an integer optionally suffixed `s`, `m`, `h` or `d`; a bare integer is
seconds.

### What each key measures { #retention-measures }

| Key | Age is measured from | Unit removed |
|---|---|---|
| `trace.retain.max_sessions` | — | One session, whole: its records directory and, without `--workdir`, its work folder |
| `trace.retain.max_age` | The uuid-v7 timestamp inside the `ses_` id | One session, whole |
| `trace.retain.max_sessions`, on an entry member | — | One formation directory and its records subtree, whole |
| `trace.retain.max_age`, on an entry member | The uuid-v7 timestamp inside the `frm_` id | One formation directory and its records subtree, whole |
| `context.retain.max_messages` | — | The oldest messages of one record |
| `context.retain.max_age` | The last write to `conversation.jsonl` | One context directory, whole |

A session is an independent unit — nothing references it and nothing spans two of them — so it is
removed whole: its records directory, with its `trace.jsonl`, `blobs/`, `out/`, `home/`, `tmp/` and `children/`, and, for a launch without `--workdir`, its work folder `<manifest-dir>/workdir/<session-id>`. The
decision is made on the records group's `ses_*` names. A record is one unit
that grows, so it is trimmed from the front: the newest `max_messages` stay, each keeping the `id`
it has always carried, and the [header line](workdir.md#record-header) records what went.

### What retention never touches { #retention-never }

| Never removed | Why |
|---|---|
| The running session's own directory, or any `ses_` id at or after it | A capsule launched while this one is running is inside the same workdir, and its session is not this session's to delete. |
| The launch's own formation directory or a later one, one still running, another project's, or one with no `formation.json` | An entry member's policy reaches only its own project's ended formations. See [Removing formation directories](roster.md#formation-retention). |
| The context the launch is using | Retention must not delete the conversation it is about to continue. |
| A record whose header names another capsule | Two capsules can share a `context.record_store`; neither prunes the other's history. |
| A record with no header line | A record written by a capsule that declares no `context.retain` is unowned, and the age sweep never removes it. It is adopted — and the policy starts applying — on the next launch that opens it under `--context`. [`mur conversation rm`](cli.md#mur-conversation-rm) is what reaches an abandoned one. |

---

## Lifecycle { #lifecycle }

The `lifecycle:` block controls how long a capsule runs and how many tasks it accepts. Omitting it
is equivalent to:

```yaml
lifecycle:
  task_acceptance: single
  after_task: exit
  queue_depth: 1
  conversation: stateless
  max_task_reopens: 1
  shell_grace_secs: 10
  delegation_deadline_secs: 600
  # keep_working: absent, so a task ends at its first turn end — see lifecycle.keep_working
```

### `lifecycle.task_acceptance` { #lifecycle-task-acceptance }

| Value | Behaviour |
|---|---|
| `none` | Capsule runs the launch's own task — `--task`, or a `task.md` or `input.txt` in the workdir — if it has one, then exits. The door answers `SendMessage` with JSON-RPC error `-32601`, and `SendStreamingMessage` and `SubscribeToTask` with `-32004` — see [Agent Card: Errors](agent-card.md#errors). |
| `single` (default) | Capsule runs one task — the launch's own, or else the first A2A task — then exits. A message while a task is queued or active, the launch's own task included, is answered with a task in [`TASK_STATE_REJECTED`](agent-card.md#task-states): `task rejected: capsule is busy`. |
| `queue` | Capsule accepts up to `queue_depth` pending tasks simultaneously. Tasks are processed serially; new tasks are accepted as soon as the pending queue drops below `queue_depth`. Under `after_task: exit`, a task still queued when the session ends is never started: it ends `TASK_STATE_REJECTED` — see [Tasks queued when the session ends](#queued-tasks-at-session-end). |

### `lifecycle.after_task` { #lifecycle-after-task }

| Value | Behaviour |
|---|---|
| `exit` (default) | The session ends as soon as the task finishes, whether the task came from `--task` or over A2A. A launch that receives no task ends when the [idle timeout](#idle-timeout) fires. A task the door accepted that the session never started ends `TASK_STATE_REJECTED` — see [Tasks queued when the session ends](#queued-tasks-at-session-end). |
| `sleep` | The capsule loops back to wait for the next task. Only useful with `task_acceptance: queue`; with `single` it behaves like `exit`. |

### Tasks queued when the session ends { #queued-tasks-at-session-end }

Every lifecycle except `task_acceptance: queue` with `after_task: sleep` stops taking work once its
task is done. A task the door accepted before that point but the session never started is refused,
not run: running it would contradict `after_task: exit`.

| Task | What it gets |
|---|---|
| Accepted, never started, when the session stops taking work | `TASK_STATE_REJECTED` over [`GetTask`](agent-card.md#tasks-get), a terminal `TASK_STATE_REJECTED` [status on the stream](streaming-protocol.md#one-final-status), and one [`task_rejected`](observability-schemas.md#task-rejected) trace line. No `task_start`, `task_end`, hook dispatch or model request |
| Cancelled while still queued | Stays `TASK_STATE_CANCELED`, with its `task_canceled` trace line. Never `TASK_STATE_REJECTED` |
| A message arriving after the session stops taking work | Answered with a task in `TASK_STATE_REJECTED` at the door. Nothing is recorded in the trace |

A refused task does not change the session's own outcome: `session_end.exit_status`, the
`on-session-end` hook's `exit_status` and the `mur run` exit code are what the tasks that ran make
them. To have queued work run, declare `task_acceptance: queue` with `after_task: sleep`.

### `lifecycle.input_timeout_secs` { #lifecycle-input-timeout-secs }

Controls how long the capsule waits for a `SendMessage` reply after a WASM tool component calls
`murmur:task/task#request-input`.

| Value | Behaviour |
|---|---|
| absent (default) | Wait indefinitely — the task stays in [`TASK_STATE_INPUT_REQUIRED`](agent-card.md#task-states) until a reply arrives, the task is cancelled, or the process is killed. |
| `N` (positive integer) | If no `SendMessage` arrives within `N` seconds, the tool call fails and the attempt ends. The task ends `TASK_STATE_FAILED`, and its [final status](streaming-protocol.md#one-final-status) carries message `input-timeout`, unless an `on-task-end` hook reopens it. |

Example — require a reply within 5 minutes:

```yaml
lifecycle:
  task_acceptance: single
  input_timeout_secs: 300
```

See [request-input WIT import](wit-interfaces.md#murmurtasktask) and
[Pause the agent loop for human input](../how-to/hitl-request-input.md).

### `lifecycle.shell_grace_secs` { #lifecycle-shell-grace-secs }

How long a shell command runs in the foreground before the runtime moves it to the background.
Every command starts in the foreground and the clock decides.

| Value | Behaviour |
|---|---|
| `10` (default) | A command exiting within 10 seconds returns its output to the turn. One that runs longer moves to the background. |
| `N` (positive integer) | The same, with an `N`-second window. |
| `0` | The first check after the spawn moves the command to the background, unless it has already exited by then. |

A backgrounded command hands the turn a work id of the form `wrk_<hex>` and the fact that it is
still running. The work id is valid for the rest of the session. The runtime-provided
[`background-command`](runtime-provided-tools.md#background-command) tool takes it and reports the
command's state and output so far, waits for it to exit, or stops it.

From the moment it moves to the background, the command's stdout and stderr are written to
`logs/<work_id>.log` in the session's [records directory](workdir.md#records-directory) as they
are read:

```text
Command: <binary> <arguments>

<output: what was read before the move, stdout then stderr, then both streams in the order read>

Exit code: <n>
```

The `Exit code:` line is appended when the command exits. A foreground command whose output passed
16 KiB writes `logs/shell-<ms>.log` instead, once, with `Stdout:` and `Stderr:` sections.

How the exit reaches the agent:

| Lifecycle | Delivery |
|---|---|
| Any | `background-command` returns the exit code, `status` and the last 16 KiB of output once the command has exited |
| `task_acceptance: queue` with `after_task: sleep` | Also a task with [origin](../concepts/access-control.md#task-origin-and-trust-class) `completion` in the `bg` lane, carrying the exit code and the log's path. Not enqueued when `background-command` already returned the exit |

A non-zero exit, a signal kill and a [resource limit](resource-limits.md#which-limit) are told
apart by the `status` field on the command's
[`shell_completed`](observability-schemas.md#session-trace-tracejsonl) record, which is written
either way.

A task that ends while background commands are still running names them in its A2A result, as
[`metadata.murmur.runningCommands`](agent-card.md#no-answer-metadata), and in the `running_commands`
field of its `task_end` record. The session's end kills them.

#### When a command exits while something it started holds its output { #shell-output-holders }

Each shell command starts as the leader of its own process group, with `/dev/null` as its stdin.
A process it starts in the background with `&` stays in that group and, unless its output is
redirected, holds the call's stdout and stderr open after the command itself has exited. Once the
command's own process exits, the call:

1. Waits up to 1 second for its stdout and stderr to close, and never past the end of the
   `lifecycle.shell_grace_secs` window. Output a background process prints and exits within that
   second is part of the result.
2. Kills every process still in the command's process group with `SIGKILL`, if the output is
   still open, and returns what was read, including what the dying processes wrote.
3. Stops waiting 2 seconds after the kill if the output is still open, and returns what was read
   by then.

A command that moved to the background is finished the same way when it exits, so its exit is
recorded even when it left processes holding its output.

When step 2 or 3 is reached, the model reads a note after the output saying what happened:

| Holder | Note |
|---|---|
| Processes in the command's process group | They were still holding the output and were killed |
| A process that left the group, such as one started with `setsid` | It still holds the output and keeps running until the session ends, which kills it |

A process meant to keep running after its call returns redirects its output away from the call:

```bash
server >server.log 2>&1 &
```

It holds neither pipe, so the call returns as soon as the command exits and later calls in the
same session can reach it. The session's end kills it with everything else the session started.

The kill in step 2 takes the whole process group. If one command starts a redirected server and
also leaves another process holding its output, the server is killed with it, so redirect every
background process a command starts.

#### When a session ends with a command still running

The session does not wait for it. It kills it, together with every other process the session
started — a server a shell call put in the background with `&` included. The command's log keeps
what it printed before the kill.
One report goes to stderr and to `logs/bootstrap.log` in the session's
[records directory](workdir.md#records-directory), naming every such command's work id, binary,
command text and how long it had been running, and each command also gets a
[`shell_abandoned`](observability-schemas.md#session-trace-tracejsonl) line in `trace.jsonl`.

```text
[capsule-runtime] 1 background shell command discarded at session end.

Session ses_01k4m2y7xq8f9v3n5r7t9w1c2e ended while 1 demoted shell command was unaccounted for. Every process the session started was killed when it ended; what is known about each command is below.

work_id: wrk_0199a3f1c2d47e8ab5c6d7e8f9a0b1c2
binary: /usr/bin/bash
command: sleep 45; echo done
state: killed at session end after 45012 ms
result: none — it was killed before it finished, so no task carries an exit code. What it printed before the kill is in /home/dev/.murmur/records/home/dev/proj/ses_01k4m2y7xq8f9v3n5r7t9w1c2e/logs/wrk_0199a3f1c2d47e8ab5c6d7e8f9a0b1c2.log.

A session's end kills every command it demoted and every process its shell calls started in the background. Declare lifecycle.task_acceptance: queue with lifecycle.after_task: sleep for a session that outlives its task and can be told the result instead.
```

A command that finishes during teardown, after the agent last read it, is reported with its exit
code instead:

```text
state: finished during teardown after 2014 ms
status: ok
exit_code: 0
output: /home/dev/.murmur/records/home/dev/proj/ses_01k4m2y7xq8f9v3n5r7t9w1c2e/logs/wrk_0199a3f1c2d47e8ab5c6d7e8f9a0b1c2.log (91 bytes)
result: on disk — it finished too late for any task to carry it back to the agent.
```

`status` is `ok` only for a clean, zero exit with nothing attributed against it. A `resource_limit`
line names the ceiling a command was stopped against, and a `wait_error` line names a failure in
waiting for the command rather than in the command itself; either explains a `status: error` that
the exit code alone does not.

#### What a backgrounded command costs when the host dies

Every way a session ends — its task finishing, `SIGTERM`, a spawner or formation ending it — kills
the commands it moved to the background. A runtime that is itself killed — `SIGKILL` or an
out-of-memory kill — takes them with it on Linux: the session's cgroup scope has a watcher process
that kills everything in it once the runtime is gone (see
[Network reach limits](containment.md#network-reach-limits) for when they outlive it). Either way, no
exit code survives: `logs/<work_id>.log` keeps the output written before the kill, with no
`Exit code:` line. The work is a full run's compute spent for nothing, so
`lifecycle.shell_grace_secs` is also a decision about how much compute one lost host can waste.

The record of the demotion does survive. The `shell_detached` line is written and flushed at the
moment the command moves to the background, so a `SIGKILL` or a process crash keeps it, and
`mur run --resume <session>` reports every such command that has no matching completion as a loss
— once, naming the work id, the binary, the command and when it was detached. A host power loss
is the case that does not hold: the line is flushed, not synced to the disk, so the last few
seconds of the file can go with the machine.

A demotion whose own record cannot be written does not fail the command or the session. The
command runs, the turn keeps its handle, and the failure is announced on stderr — with the
consequence that a later resume has nothing to find and that command's loss is never reported.

### `lifecycle.delegation_deadline_secs` { #lifecycle-delegation-deadline-secs }

The single bound on work a capsule hands off: delegations and
[`call-member`](runtime-provided-tools.md#call-member) calls. The capsule's own runtime holds the
clock; no daemon has to be reachable for the deadline to fire.

It covers every wait on handed-off work, one per caller:

| Caller | What the deadline bounds |
|---|---|
| [`delegate-task`](runtime-provided-tools.md) | How long the started sub-capsule is watched. On expiry the sub-capsule is ended and the delegating task continues with a `terminated` outcome |
| A plan's `capsule` step | How long the step waits for the sub-capsule's answer. On expiry the step fails and the sub-capsule is stopped |
| [`call-member`](runtime-provided-tools.md#call-member) | How long a call may spend offering a busy formation member the task and waiting for its answer, together. On expiry the calling task gets a `timed_out` answer from a member that held the task, which is not stopped, or a `rejected` answer from one that [stayed busy](runtime-provided-tools.md#call-member-busy) |

| Value | Behaviour |
|---|---|
| `600` (default) | A sub-capsule has 10 minutes from the moment it reports itself ready. A `call-member` call has 10 minutes from the call |
| `N` (positive integer) | The same, with an `N`-second window |
| `0` | For a plan step, the first poll after the task is delivered gives up. A `call-member` call times out before its first read of the member's task, and a busy member is never offered the task again |

A capsule that does not declare it hands off work under the 600-second default. The `MURMUR_DELEGATION_TIMEOUT_SECS` environment variable sets the same
bound for a whole process when it names a positive integer; the declared value applies otherwise,
and `600` when neither is given. Getting the sub-capsule *started* is bounded separately — see
[Bounds](roost-api.md#bounds) — so a slow host does not spend this window on staging.

A sub-capsule also runs under its parent's own [deadline](cli.md#mur-run-deadline), as its session
deadline. Whichever of the two falls first ends the sub-capsule.

**Reaching the deadline ends the sub-capsule.** Under `delegate-task` the delegating task, which
waits for every sub-capsule it started on any `lifecycle`, continues with an outcome carrying the
delegation's `dlg_` id, `status: terminated` and a `detail` naming the bound in seconds — the same
shape every delegation outcome takes, described in
[How the outcome arrives](roost-api.md#how-the-outcome-arrives). A task that has heard nothing 30
seconds past the deadline ends the sub-capsule itself.

```yaml
lifecycle:
  delegation_deadline_secs: 120
```

### `lifecycle.keep_working` { #lifecycle-keep-working }

Keeps a task's agent working past the point where the model ends its turn. Each time an attempt
ends at a turn end — the model ended its turn with no tool call and nothing it handed off is
outstanding — the runtime sends `message` as the next user message in the same conversation and
the task goes on. Absent, a task ends at its first turn end.

```yaml
lifecycle:
  keep_working:
    message: "Keep working on the task. Check your work against what was asked."
    limit: 20
```

| Field | Rule |
|---|---|
| `message` | Required. Sent verbatim and unfenced. Must not be blank after trimming; the text is sent as written, surrounding whitespace included. |
| `limit` | Required. At least `1`. To turn the option off, omit the block. |

A missing or blank `message`, a missing `limit` and `limit: 0` are refused with
[`E-MAN-003`](diagnostics.md#e-man-003), naming the field:
`murmur.yaml: invalid lifecycle config for 'lifecycle.keep_working.limit': must be at least 1;
omit lifecycle.keep_working to turn the option off`.

The runtime adds no wording of its own: nothing in the system prompt, `MURMUR.md` or the
`[Capsule]` block mentions the option, and the model reads only `message` and the `keep_working`
field of [`session-status`](runtime-provided-tools.md#session-status). Every continuation resends
the whole context, so each one costs at least as much input as the turn before it.

What stops the task, what it ends with, and how the deadline treats it are in
[Keeping a task working](../concepts/session-loop.md#keep-working).

### `lifecycle.conversation` { #lifecycle-conversation }

Controls whether a task starts from the conversation its `contextId` already has. It governs what
a task *loads*, never what is recorded: both modes append every message to the conversation
record, which [`context.record`](#context-record) is what turns off.

| Value | Behaviour |
|---|---|
| `stateless` (default) | Every task starts with an empty message history. The `contextId` on the incoming message is recorded but has no effect on context. |
| `threaded` | A task that arrives with a `contextId` starts from the whole [conversation record](workdir.md#the-conversation-record) for that context, including messages an earlier session wrote. Each completed task also writes a per-task result to `out/result_<taskId>.txt` in the session's [records directory](workdir.md#records-directory), alongside the shared `out/result.txt`. |

Under [`transport: process`](#transport-process) the harness holds the conversation, so the setting
decides what the harness is handed rather than what the runtime loads: `stateless` launches it on a
new session every task, and `threaded` launches it on the session the
[harness session map](workdir.md#harness-session-map) already holds for that context.

This setting is the capsule's own policy, and
[`mur run --resume <session>`](cli.md#mur-run) overrides it for one launch: that launch loads the
record even under `stateless`.

A conversation outlives the session that started it. Threaded tasks reaching one capsule over A2A
need a long-running capsule (`task_acceptance: queue`, `after_task: sleep`), since with
`task_acceptance: single` the capsule exits after the first task; two separate
[`mur run --context <id>`](cli.md#mur-run) launches continue one conversation without it.

```yaml
lifecycle:
  task_acceptance: queue
  after_task: sleep
  queue_depth: 2
  conversation: threaded
```

### Idle timeout

How long a capsule waits for the next A2A message depends on the lifecycle it declares:

| Lifecycle | Behaviour when no message arrives |
|---|---|
| `task_acceptance: queue` with `after_task: sleep` | Waits indefinitely. Shutdown is the host's responsibility. |
| Every other combination | Waits 30 seconds for a task when the launch was given none — no `--task`, and no `task.md` or `input.txt` in the work folder when it started. If none arrives, the session ends without calling the model. |

Set the `MURMUR_A2A_TIMEOUT_SECS` environment variable to change the 30-second window.

### CLI overrides

`mur run` accepts two flags that override the manifest's lifecycle without editing the file:

```bash
mur run --manifest murmur.yaml --lifecycle-task-acceptance queue --lifecycle-after-task sleep
```

Values follow the same `snake_case` names as the manifest fields.
