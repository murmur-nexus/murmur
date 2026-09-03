# Manifest Schema

`murmur.yaml` serves two roles, each with its own set of fields. An artifact's own manifest
declares its identity and what `mur build` packages. A capsule's manifest declares what `mur run`
launches: the artifacts it installs, the capabilities it grants, and how it reaches a model.

---

## Artifact manifest { #artifact-manifest }

The manifest that ships inside a `.mur.zip`, read by `mur build` and `mur publish`.

| Field | Type | Required | Notes |
|---|---|---:|---|
| `name` | string | yes | Lowercase letters, digits and inner hyphens (`[a-z0-9-]`, no leading or trailing `-`), non-empty, at most 100 characters. Anything else fails the build with [`E-BLD-001`](diagnostics.md#e-bld-001). |
| `version` | string | yes | `latest`, `stable` and `edge` are reserved and cannot be published (`E-REG-004`). |
| `runtime` | string | yes | The artifact's role. Two values change how it is packaged when `execution` is absent: `skill` packages as `static`, and `tool` with `implementation: native` packages as `native`. Everything else packages as `wasm`. |
| `implementation` | `wasm \| native` | no | How a `runtime: tool` artifact is implemented. Default: `wasm`. |
| `execution` | `wasm \| native \| static` | no | Declares the registry packaging type directly. When set it is authoritative for `mur publish` and overrides the derivation from `runtime` and `implementation`. Case-insensitive. An unrecognized value is a parse error. |
| `requires_files` | list<string> | no | Companion files that must sit beside `murmur.yaml` — and the complete list of what `mur build` packages besides the manifest itself. Paths are relative to the source directory and may be nested (`assets/logo.png`). Default: `["skill.md"]` for `runtime: skill`, empty for every other role; an explicit value, including `[]`, always overrides that default. A missing file fails the build with `E-IO-003`, naming the first missing entry. Entries must be plain relative paths to real files: absolute paths, `..` components and symlinks are rejected with [`E-BLD-002`](diagnostics.md#e-bld-002). An artifact with a compiled payload must declare it here, or the built `.mur.zip` contains nothing but `murmur.yaml` — for a wasm artifact that is [`E-BLD-003`](diagnostics.md#e-bld-003). |

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

---

## Capsule manifest { #capsule-manifest }

The manifest `mur run` reads.

### Supported shape

```yaml
name: my-capsule
version: "0.1.0"

artifacts:
  - name: some-tool
    version: "1.2.3"
    runtime: tool  # optional, defaults to tool
  - name: murmur-hook-debug
    version: "{{ v.murmur_hook_debug }}"
    runtime: hook  # lifecycle observer; hidden from the model

capabilities:
  network:
    allow:
      - https://api.anthropic.com
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
    allow:             # optional: host env vars a WASM guest (capsule/tool/driver) may observe
      - MY_APP_REGION

network:
  internal_port: 14159  # optional; omit to let the OS assign a free port

context:
  max_tokens: 200000   # enables context compaction; omit to disable
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
  retain:             # optional; omit to keep every session directory forever
    max_sessions: 50  # keep the newest N session directories, this launch's own included
    max_age: 14d      # and/or drop anything whose ses_ id was minted longer ago than this

inference:
  transport: http
  endpoint: https://api.anthropic.com
  model: claude-opus-4-5
  api_key: ${ANTHROPIC_API_KEY}  # optional; literal value or ${ENV_VAR}
  driver:
    artifact: murmur-driver-anthropic
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
  # max_turns: 10               # optional; default 10

# Alternative: spawn a CLI as a subprocess (no ANTHROPIC_API_KEY required)
# inference:
#   transport: process
#   command: claude              # CLI binary name; must be on PATH
#   model: claude-haiku-4-5-20251001
#   max_turns: 10

lifecycle:
  task_acceptance: queue  # none | single (default) | queue
  after_task: sleep       # exit (default) | sleep
  queue_depth: 2          # only meaningful for queue mode; default: 1
  input_timeout_secs: 60  # optional; absent = wait indefinitely for request-input reply
  conversation: threaded  # stateless (default) | threaded

exports:
  files:
    root: out/            # required; relative to the accessible workdir
    mode: read-only       # required; the only accepted value
    max_bytes: 10Mi       # optional; per-file read ceiling, default 10Mi
  peer_files:
    root: out/handoff/    # required; relative to the accessible workdir
    max_ttl: 15m          # optional when after_task: exit (default 1h); required, max 15m, when sleep
    max_bytes: 10Mi       # optional; per-file ceiling on a redeemed read, default 10Mi
```

### Field reference

#### Identity { #field-identity }

| Field | Type | Required | Notes |
|---|---|---:|---|
| `name` | string | yes | Capsule identity. |
| `version` | string | yes | Capsule version. |
| `mur_version` | string | no | Pins the `mur` runtime version this capsule requires — see [`mur_version`](#field-mur-version). |

#### `mur_version` { #field-mur-version }

Pins the exact version of the `mur` binary that must run this capsule.

```yaml
mur_version: "1.0.0"
```

| Command | Behaviour |
|---|---|
| `mur deploy` | Downloads `mur-{version}-{platform}` from GitHub releases and installs it on the target VM, regardless of which `mur` version is running locally. The binary is cached at `~/.murmur/bin/mur-{version}-{platform}` and reused on subsequent deploys. Omitted, the running `mur` binary's version is used. |
| `mur run` | Prints a warning to stderr when the running `mur` version does not match. The run continues. |

#### `artifacts` { #field-artifacts }

| Field | Type | Required | Notes |
|---|---|---:|---|
| `artifacts` | list | no | Defaults to empty. |
| `artifacts[].name` | string | yes (per entry) | Artifact name. |
| `artifacts[].version` | string | yes (per entry) | Optional when `source` is set — see [Local-source artifacts](#local-source-skills). |
| `artifacts[].runtime` | `tool \| driver \| hook \| skill` | no | Default: `tool`. `tool` and `skill` artifacts are model-visible; `driver` and `hook` artifacts are hidden from the model. `wasm` and `native` are rejected here — those belong in the artifact's own manifest as `implementation`. |
| `artifacts[].source` | string | no | Local path the runtime resolves this artifact from instead of the registry. Requires `local_source: true`. See [Local-source artifacts](#local-source-skills). |
| `artifacts[].local_source` | bool | no | Opts this artifact into `source:` resolution. Default: `true` for `runtime: skill`, `false` for every other role; an explicit value overrides that default in both directions. See [Local-source artifacts](#local-source-skills). |
| `artifacts[].prompt_payload` | bool | no | Opts this artifact into being named by `inference.system_prompt_artifact`. Default: `true` for `runtime: skill`, `false` for every other role; an explicit value overrides that default. See [`inference.system_prompt_artifact`](#inference-system-prompt-artifact). |
| `artifacts[].capabilities` | map | no | Per-artifact capability grant, recognized on `runtime: hook`, `runtime: tool` and `runtime: driver`. The baseline differs by role: on a hook, absent means no network and no filesystem at all (see [Hook capabilities](#hook-capabilities)); on a tool or driver, absent means the unchanged capsule-wide ceiling, and a declared block *narrows* below it (see [Tool and driver capabilities](#tool-capabilities)). `capabilities.state` is the exception to both baselines: absent means no durable store for any role, and a declared block opens one directory outside every workdir. Declaring it on `runtime: skill` fails with `E-MAN-003`. |
| `artifacts[].config` | map | no | Operator-authored configuration delivered to this artifact alone as the `MURMUR_ARTIFACT_CONFIG` environment variable, serialized as compact JSON. Recognized on `runtime: hook`, `runtime: tool` and `runtime: driver`; declaring it on `runtime: skill`, or at the top level of the manifest, fails with `E-MAN-003`. Absent, the variable is absent from that artifact's environment. See [Artifact config](#artifact-config) and [Choosing a config block](#which-config-block). |
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
  - name: murmur-hook-observe
    version: 0.1.0
    runtime: hook

  # granted exactly one host and exactly one directory
  - name: murmur-hook-telemetry
    version: 0.1.0
    runtime: hook
    capabilities:
      network:
        allow:
          - https://telemetry.example.com
      filesystem:
        scope: hook-state

  # granted the task text and the agent's result, and nothing else
  - name: murmur-hook-output-gate
    version: 0.1.0
    runtime: hook
    capabilities:
      task_io:
        read: true

  # granted the capsule's conversation record, and nothing else
  - name: murmur-hook-recall
    version: 0.1.0
    runtime: hook
    capabilities:
      conversation:
        read: true

  # granted a durable store and nothing else — no project directory at all
  - name: murmur-hook-notes
    version: 0.1.0
    runtime: hook
    capabilities:
      state: {}
```

| Key | Type | Required | Description |
|---|---|---|---|
| `task_io.read` | bool | yes | Whether this hook may read the task's input text and the agent's result text through [`murmur:task-io/read`](wit-interfaces.md#murmurtask-ioread). Never inferred: a `task_io:` block that omits it fails with `E-MAN-003`. |
| `conversation.read` | bool | yes | Whether this hook may read the capsule's [conversation record](workdir.md#the-conversation-record) through [`murmur:conversation/read`](wit-interfaces.md#murmurconversationread). Never inferred: a `conversation:` block that omits it fails with `E-MAN-003`. |
| `state.store` | string | no | Directory name under `~/.murmur/state/`. Omitted, the capsule name is used. See [Durable state](workdir.md#state-store). |

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
- **`state` is a second, independent directory grant.** A hook holding one reaches
  `~/.murmur/state/<store>/` as `state/` in its guest, alongside — or instead of — the
  `filesystem.scope` directory mounted as `.`. The two do not imply each other in either
  direction, so a hook can hold durable state without being handed the project directory. The
  store is keyed by capsule, so it survives a launch that gets a fresh session workdir. See
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
- **Only `network`, `filesystem`, `state`, `task_io` and `conversation` govern a hook.** `shell`,
  `spawn`, `env`, `limits`, `resources` and `containment` parse here but are capsule-wide concerns
  the runtime does not apply per-hook; declaring one prints
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
  - name: broad-tool
    version: 0.1.0
    runtime: tool

  # narrowed: one host, and only the `cache/` subtree of the workdir
  - name: scoped-tool
    version: 0.1.0
    runtime: tool
    capabilities:
      network:
        allow:
          - https://api.example.com
      filesystem:
        scope: cache

  # widened by exactly one directory: a durable store outside every workdir
  - name: murmur-tool-corpus
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
- **`filesystem.scope` is a real directory grant.** `<accessible workdir>/<scope>` is mounted as
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
- **`state` is the one sub-block that widens rather than narrows.** It grants a second preopen,
  `~/.murmur/state/<store>/`, mounted in the guest as `state/` beside the workdir mounted as `.`.
  It opens exactly that one directory: never a workdir path, and never another capsule's store.
  Declaring it does not change the capsule's achieved containment class. A store name must be a
  single path segment — see [State store name](#state-store-name). See
  [Durable state](workdir.md#state-store).
- **Only `network`, `filesystem` and `state` apply to a tool or driver.** `shell`, `spawn`, `env`,
  `limits`, `resources` and `containment` parse but are inert here and print
  [`W-SEC-008`](diagnostics.md#w-sec-008), as does a grant on a tool with a **native** (non-WASM)
  implementation, which never runs through the WASI tool path at all.

##### Artifact config { #artifact-config }

`config:` on an artifact entry carries operator-authored settings to that one artifact. The runtime
serializes the block to compact JSON and sets it as `MURMUR_ARTIFACT_CONFIG` in that artifact's
environment:

```yaml
artifacts:
  - name: murmur-tool-corpus
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
    - murmur-tool-corpus
```

`--json` emits the same names as `configured_artifacts`, and `trace.jsonl`'s `session_start`
carries them verbatim as `effective_grants.configured_artifacts`. Both read `artifact config:
<none>` and `[]` when nothing declares a block.

###### Secrets do not belong in a `config:` block { #artifact-config-secrets }

`murmur.yaml` is an audit record of what a capsule was allowed to do, and a `config:` block is
plaintext inside it. Pass credentials with a `${VAR}` reference, which resolves from the
environment at launch and leaves only the variable name in the manifest — see
[`inference.api_key` resolution](#inference-api-key). Names that look credential-shaped are
stripped from every environment the runtime builds; a literal in a manifest field is reported as
[`W-SEC-004`](diagnostics.md#w-sec-004).

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
  `workdir/tools/<name>/skill.md` and listed under `## Installed Skills` in `MURMUR.md`.
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

Names an artifact declared in `artifacts:` whose content is read once at launch and bound as the
system prompt, instead of being called on demand. The named artifact must declare
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
| `capabilities.filesystem.workdir_exec` | bool | no | Default: `false`. When false, nothing the capsule writes into the session workdir can be executed — under any name, including one that appears in `capabilities.shell.allow`. Set `true` only for compile-and-run workflows (the capsule builds a binary in its workdir and then runs it); doing so makes `shell.allow` unenforceable for anything inside the workdir, caps the capsule's achieved containment class at `advisory` on **every** host, and fires [`W-SEC-011`](diagnostics.md#w-sec-011) at staging. See [Executable workdirs](containment.md#field-workdir-exec). |
| `capabilities.shell.allow` | list<string> | no | Shell binaries the agent may invoke (e.g. `bash`); see [Shell allow](#shell-allow). |
| `capabilities.shell.strip_env` | list<string> | no | Glob patterns for env vars to strip from the subprocess environment (e.g. `AWS_*`). |
| `capabilities.shell.baseline_env` | list<string> | no | Glob patterns for env vars to keep after stripping (e.g. `PATH`). |
| `capabilities.shell.interpreter_runtime` | list<grant> | no | Widens an allowlisted binary's filesystem scope to specific host directories its import machinery needs outside the workdir (a path-based interpreter's stdlib). Fires [`W-SEC-009`](diagnostics.md#w-sec-009) at staging. |
| `capabilities.shell.interpreter_runtime[].binary` | string | yes | A binary that must already appear in `capabilities.shell.allow` — this widens filesystem access alongside an existing exec grant, it never grants exec. |
| `capabilities.shell.interpreter_runtime[].dirs` | list<dir> | yes | The host directories to grant; must name at least one. |
| `capabilities.shell.interpreter_runtime[].dirs[].path` | string | yes | An absolute host path (must start with `/`) outside the workdir. |
| `capabilities.shell.interpreter_runtime[].dirs[].list_dir` | bool | yes | `true` grants execute, read and directory listing; `false` grants execute and read only, so files are openable by exact name but the directory cannot be listed. Never inferred — must be written explicitly. |
| `capabilities.shell.staged_runtime` | list<grant> | no | Names a pinned host runtime tree to bind-mount read-only into a `sealed` capsule's composed root, so the interpreter is present inside the root rather than reachable outside it. Requires an effective `sealed` containment floor ([`E-CAP-004`](diagnostics.md#e-cap-004)), and is mutually exclusive per binary with `interpreter_runtime`. See [Staged runtime](containment.md#field-staged-runtime). |
| `capabilities.shell.staged_runtime[].binary` | string | yes | A binary that must already appear in `capabilities.shell.allow`, and must not also have a `capabilities.shell.interpreter_runtime` grant. This says where an existing exec grant's runtime comes from; it never grants exec. |
| `capabilities.shell.staged_runtime[].source_path` | string | yes | Absolute host path (must start with `/`) of an already-pinned runtime tree — a vendored toolchain directory, a baked-in conda env. Not resolved, discovered or version-sniffed by the runtime, and not required to exist on the machine that merely *parses* the manifest. |
| `capabilities.shell.staged_runtime[].pin` | string | yes | Non-empty, opaque identifier of which build the tree is. Never inferred: it exists so a human can compare the declared pin across two hosts and confirm the same runtime shipped to both. |
| `capabilities.env.allow` | list<string> | no | Host env var names a WASM guest (capsule, tool or driver component) may observe. Omitted or `[]` grants nothing beyond the runtime's own `MURMUR_*` injections. |
| `capabilities.limits.memory_bytes` | integer | no | Cap on how much memory a component may allocate, in bytes. Default: 536870912 (512 MiB). Must be > 0. |
| `capabilities.limits.table_elements` | integer | no | Cap on a component's table growth, in elements. Default: 100000. Must be > 0. |
| `capabilities.limits.instances` | integer | no | Cap on the number of component instances one call may create. Default: 1000. Must be > 0. |
| `capabilities.limits.deadline_seconds` | integer | no | Wall-clock budget for a single component call, in seconds. Default: 600 for a capsule, tool or driver call; 30 for a hook lifecycle call. An explicit value overrides both defaults, hooks included. Must be > 0. |
| `capabilities.resources.max_processes` | integer | no | `RLIMIT_NPROC` headroom for each native subprocess — how much past the runtime's own uid baseline its tree may add, in the unit the kernel enforces against (threads on Linux, processes on macOS). Default: 128. Must be > 0. See the [per-uid note](resource-limits.md#host-resource-limits). |
| `capabilities.resources.max_open_files` | integer | no | `RLIMIT_NOFILE` hard ceiling on each native subprocess. Default: 1024. Must be > 0. |
| `capabilities.resources.max_file_size_bytes` | integer | no | `RLIMIT_FSIZE` hard ceiling — largest single file a subprocess may write, in bytes. Default: 4294967296 (4 GiB). Must be > 0. |
| `capabilities.resources.cpu_seconds` | integer | no | `RLIMIT_CPU` hard ceiling on each native subprocess, in CPU-seconds. Default: 3600 (1 hour). Must be > 0. |
| `capabilities.resources.memory_bytes` | integer | no | `RLIMIT_AS` (Linux) hard ceiling on each native subprocess's address space, in bytes. Default: 2147483648 (2 GiB). Must be > 0. macOS maps this to `RLIMIT_DATA`, which its kernel does not enforce — see the [platform note](resource-limits.md#host-resource-limits). |
| `capabilities.resources.cgroup_memory_bytes` | integer | no | cgroup v2 `memory.max` — aggregate memory across the whole subprocess tree, in bytes. Default: 4294967296 (4 GiB). Must be > 0. Linux only. |
| `capabilities.resources.cgroup_pids_max` | integer | no | cgroup v2 `pids.max` — aggregate task count across the whole subprocess tree. Default: 256. Must be > 0. Linux only. |
| `capabilities.resources.cgroup_cpu_percent` | integer | no | cgroup v2 `cpu.max` quota as a percentage of one core (200 = two cores' worth). Default: 200. Must be > 0. Linux only. |
| `capabilities.resources.cgroup_io_bytes_per_sec` | integer | no | cgroup v2 `io.max` read+write throughput on the workdir's backing device, in bytes/sec. Default: 104857600 (100 MiB/s). Must be > 0. Linux only, best-effort — a workdir whose backing device cannot be resolved (overlayfs, tmpfs, device-mapper) logs a note and keeps the other three controllers. |
| `capabilities.resources.workdir_max_bytes` | integer | no | Ceiling on total session-workdir size, in bytes, enforced by a periodic check. Default: 10737418240 (10 GiB). Must be > 0. Every platform. Under the `sealed` containment class this also bounds `/tmp`, which is backed by a directory inside the session workdir — see [the fixed capsule device set](containment.md#capsule-device-set). |
| `capabilities.containment` | `advisory \| scoped \| sealed` | no | Minimum containment class this capsule requires, in ascending strength. Omitted, the capsule states no requirement — see [Containment class](containment.md#field-containment). Capsule-wide only; declaring it on a per-artifact entry has no effect and warns at staging — [What bounds a WASM artifact](containment.md#artifact-boundary) names the grant that scopes one artifact. |
| `capabilities.conversation.read` | bool | no | Grant of the `murmur:conversation/read` import, applied **per hook only**. Declaring it in this capsule-wide block reaches nothing — no artifact can read the conversation record — and prints [`W-SEC-016`](diagnostics.md#w-sec-016) at staging. Put it on the hook entry that needs it: [Hook capabilities](#hook-capabilities). |
| `capabilities.state.store` | string | no | Durable store name, applied **per artifact only**. Declaring it in this capsule-wide block reaches nothing — no store is created and no `state` preopen exists — and prints [`W-SEC-014`](diagnostics.md#w-sec-014) at staging. Put it on the tool, driver or hook entry that needs it: [Tool and driver capabilities](#tool-capabilities), [Hook capabilities](#hook-capabilities). See [Durable state](workdir.md#state-store). |
| `capabilities.plan.submit` | bool | see notes | Default: absent, which is deny. `true` puts the [runtime-provided tool](runtime-provided-tools.md) `submit-plan` in the capsule's inventory, and the runtime's guidance on when to plan in its system prompt; a capsule that declares nothing is offered neither. Required when the `plan:` block is present — a block that omits it is refused at parse. It grants no reach of its own: every step of a plan runs through this capsule's own tools, `capabilities.shell.allow` and `capabilities.spawn.allow`. See [Plans](plans.md). `mur run --explain-scope` reports it as `plan submit`. |
| `capabilities.spawn.allow` | list<string> | no | Capsule names this capsule may spawn as sub-capsules. `mur-roost` matches each spawn request's capsule name against this list and refuses a name that is absent from it — see [Per-session allow lists](roost-api.md#per-session-allow-lists) for the worked example. `capabilities.shell.allow` governs the executables the capsule runs itself. A non-empty list means the capsule has a subprocess tree, so it is bound by `capabilities.resources` and needs a network namespace on Linux ([`E-CAP-005`](diagnostics.md#e-cap-005)). It also means the session registers with `mur-roost` at launch, so the daemon holds the ceiling it referees against: with no daemon reachable at `MURMUR_ROOST_URL` the launch is refused with [`E-RUN-019`](diagnostics.md#e-run-019). A non-empty list is also what puts the [runtime-provided tool](runtime-provided-tools.md) `delegate-task` in the capsule's inventory, with these names as the tool's `capsule` argument — see [The delegation tool](roost-api.md#the-delegation-tool). A capsule that declares none is offered no such tool. How deep a chain of delegations may go and how many children one session may hold at once are the daemon's, not this field's — see [Delegation bounds](roost-api.md#delegation-bounds). `mur run --explain-scope` reports it as `spawn allow`, and `trace.jsonl`'s `session_start` carries it as `effective_grants.spawn_allow`. |

A `capabilities.network.allow` host that fails DNS resolution at launch is skipped rather than
treated as an error: the run proceeds with that host contributing no addresses to the launch-time
IP allowlist a shell subprocess falls back to when it reaches a destination by address rather than
by name. This only ever shrinks what a shell binary can reach. Malformed host *syntax*, as opposed
to a resolution failure, is still rejected outright with
[`E-CAP-001`](diagnostics.md#e-cap-001).

A WASM guest never inherits the host process's environment. `capabilities.env.allow` is the only
way to expose a host variable, and even a name declared there is dropped if it is credential-shaped
(see [Lock down a capsule's capabilities](../how-to/lock-down-capsule.md#step-2-manage-the-subprocess-environment)
for the pattern list) or matches `capabilities.shell.strip_env`. A declared-but-unset host variable
is omitted rather than reported.

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

#### `inference` { #field-inference }

The `inference` block is optional; a capsule with no inference block runs its tools without a
model. These fields are read under both transports:

| Field | Type | Required | Notes |
|---|---|---:|---|
| `inference.transport` | `http \| process` | no | Default: `http`. `http` routes every call through a WASM driver artifact; `process` spawns a CLI subprocess. See [Inference configuration](#inference-config). |
| `inference.max_turns` | integer | no | Maximum LLM inference calls per task. Default: `10`. Must be > 0. |
| `inference.system_prompt` | string | no | Text injected verbatim as the `system` parameter on every inference call. At most one of `system_prompt`, `system_prompt_file` and `system_prompt_artifact` may be set. |
| `inference.system_prompt_file` | string | no | Path to a file whose content is injected as the system prompt, relative to the manifest directory. |
| `inference.system_prompt_artifact` | string | no | Name of an artifact declared in `artifacts:` whose payload is bound as the system prompt — see [`inference.system_prompt_artifact`](#inference-system-prompt-artifact). |

These fields are read under `transport: http`, and setting any of them under
`transport: process` is a manifest error:

| Field | Type | Required | Notes |
|---|---|---:|---|
| `inference.endpoint` | string | yes | Base URL for inference API requests. Must be `https://` (any host) or `http://` with a loopback host — see [Endpoint scheme and host validation](#endpoint-validation). |
| `inference.model` | string | yes | Model identifier passed to the driver. |
| `inference.driver.artifact` | string | yes | Inference driver artifact name; must be declared in `artifacts:` with `runtime: driver`. |
| `inference.driver.config` | object | no | Settings any driver of this role would act on, serialized to compact JSON and set as `MURMUR_INFERENCE_DRIVER_CONFIG` for the driver, every WASM tool and every shell tool in the session. A value that is not a mapping fails the manifest parse with `E-MAN-003`. See [Choosing a config block](#which-config-block). |
| `inference.provider.artifact` | string | no | Accepted older spelling of `inference.driver.artifact`; `inference.driver.artifact` wins when both are set. |
| `inference.api_key` | string | no | Literal value or `${ENV_VAR}` reference — see [`inference.api_key` resolution](#inference-api-key). |
| `inference.max_tokens` | integer | no | Maximum output tokens the model may generate **per turn**. Default: `8192`. Must be > 0; not clamped at the top end. Distinct from [`context.max_tokens`](#field-context) — see [Output cap](#inference-max-tokens). |

These fields are read under `transport: process`:

| Field | Type | Required | Notes |
|---|---|---:|---|
| `inference.command` | string | yes | CLI binary to spawn; must be on `PATH`. A command whose base name is `codex` selects the codex wire protocol; anything else selects the Claude Code protocol. Invalid for `transport: http`. |
| `inference.model` | string | no | Model identifier passed to the CLI. Omitted, the CLI uses its own configured default. |

The `inference.compaction` block parses under either transport but takes effect only under
`transport: http`; the CLI subprocess loop has no compaction step, so under `transport: process`
these fields are accepted and inert:

| Field | Type | Required | Notes |
|---|---|---:|---|
| `inference.compaction.threshold` | float (0.0–1.0] | no | Fraction of `context.max_tokens` at which compaction fires. Default: `0.98`. |
| `inference.compaction.model` | string | no | Model override for compaction calls. Defaults to the primary inference model. |
| `inference.compaction.system_prompt` | string | no | System prompt override for compaction calls, passed verbatim to the compaction hook. No trimming, length limit or format check. Omitted, the compaction hook picks its own default prompt. Mutually exclusive with `inference.compaction.system_prompt_file`. |
| `inference.compaction.system_prompt_file` | string | no | Path to a file whose content is passed verbatim as the compaction system prompt. Relative to the manifest directory; read when the session launches. Mutually exclusive with `inference.compaction.system_prompt`. |
| `inference.compaction.dump_summaries` | bool | no | Default: `false`. When `true`, every committed compaction appends one JSON line to `out/compaction-summaries.jsonl` in the session workdir, recording the verbatim summary text plus token counts. |

#### `context` { #field-context }

| Field | Type | Required | Notes |
|---|---|---:|---|
| `context.max_tokens` | integer | no | Token budget for the session. Required to enable compaction; omit to disable it. Must be > 0. Read only under `transport: http`, like the [`inference.compaction`](#field-inference) block it drives. Distinct from [`inference.max_tokens`](#field-inference), the per-turn output cap. |
| `context.seed_budget` | float (0.0–1.0) | no | Default: `0.10`. Fraction of `context.max_tokens` an `on-task-start` hook's `seed-context` may occupy. The product, rounded down, is sent to the hook as `task-start-event.budget-tokens`. Requires `context.max_tokens`: without it there is no ceiling, and a returned seed is refused with `reason: "no_budget"`. Inert under `transport: process`, where a seed is refused with `reason: "unsupported_transport"`. |
| `context.seed_overflow_margin` | float (0.0–1.0) | no | Default: `0.10`. Slack above `context.seed_budget`, as a fraction of it, within which an over-budget seed has its oldest messages dropped rather than being handed to the compaction hook. Requires `context.max_tokens` and is inert under `transport: process`, exactly like `context.seed_budget`. |
| `context.record` { #context-record } | `on \| off` | no | Default: `on`. Whether the runtime keeps a [durable conversation record](workdir.md#the-conversation-record) for this capsule. `off` turns the mechanism off: nothing is created under `~/.murmur/conversations/`, and a hook granted `capabilities.conversation.read` reads an empty page. Inert under `transport: process`, which writes no record either way. |
| `context.record_store` | string | no | Default: the capsule name. Directory under `~/.murmur/conversations/` this capsule's records live in. One path segment: no `/`, no `.` or `..`, not absolute, not starting with a dot — anything else refuses the launch with [`E-CAP-011`](diagnostics.md#e-cap-011). Accepted and inert alongside `record: off`. |
| `context.retain` { #context-retain } | block | no | What bounds this capsule's [conversation records](workdir.md#the-conversation-record). Omitted, nothing is ever deleted. See [Retention](#retention). |
| `context.retain.max_messages` | integer ≥ 1 | no | Messages to keep. At each launch, the record that launch opens — the context named by `mur run --context` — is truncated to its newest N; the older ones are dropped and the [header line](workdir.md#record-header) records the drop. A launch with no `--context` mints a context per task and opens no record to truncate; bound those with `context.retain.max_age`. |
| `context.retain.max_age` | duration | no | Age beyond which a record this capsule owns is removed whole, measured from the last write to its `conversation.jsonl`. |

#### `observability` { #field-observability }

| Field | Type | Required | Notes |
|---|---|---:|---|
| `observability.otel_endpoint` | string | no | OTLP/HTTP endpoint for OTel span export (e.g. `http://localhost:4318`). When set, the runtime exports one span per session event as the event happens, and injects `MURMUR_OTEL_ENDPOINT` into every hook component's environment — see [OTel span emission](observability-schemas.md#otel-span-emission). When absent, no outbound telemetry is sent and `trace.jsonl` is the only output. An empty string counts as absent. |
| `observability.eval.dataset_id` | string | no | Labels the `dataset_run` record in `eval.jsonl` and is forwarded to hooks as `MURMUR_DATASET_ID`. Useful when diffing runs from multiple datasets. |
| `observability.eval.scorers` | list | no | Scorer configurations. When present with at least one valid scorer, `murmur-hook-eval` writes `eval.jsonl`. When absent or empty, the hook is a no-op and logs a warning. |
| `observability.eval.scorers[].type` | string | yes (per entry) | One of `exit_ok`, `max_turns`, `max_tokens`, `tool_sequence`, `llm_judge`. An unrecognized type is skipped with a message on stderr. `llm_judge` is unimplemented: it parses, logs a warning and emits no score. |
| `observability.eval.scorers[].name` | string | no | Key this scorer's records appear under in `eval.jsonl`. Defaults to the scorer's `type`. |
| `observability.eval.scorers[].max` | integer | no | Upper bound for `max_turns` and `max_tokens`. Default: `10` for `max_turns`, `100000` for `max_tokens`. Ignored by every other type. |
| `observability.eval.scorers[].expected` | list<string> | no | Ordered list of tool names that must appear as a subsequence of observed calls, for `tool_sequence`. Defaults to empty. Ignored by every other type. |

#### `trace` { #field-trace }

| Field | Type | Required | Notes |
|---|---|---:|---|
| `trace.capture` | `none \| meta \| content` | no | Default: `meta`. How much of each turn's driver request `trace.jsonl` keeps — see the table below. |
| `trace.include_tool_output` | bool | no | Retired; use `trace.capture`. Accepted as an alias — `true` for `capture: content`, `false` for `capture: meta` — and its use prints a warning. Setting it alongside `trace.capture` is an error, even when the two agree. |
| `trace.retain` { #trace-retain } | block | no | What bounds the [session directories](workdir.md) beside the running one. Omitted, nothing is ever deleted. See [Retention](#retention). |
| `trace.retain.max_sessions` | integer ≥ 1 | no | Session directories to keep, counting the running session itself. The rest are removed whole, taking their `trace.jsonl` and `blobs/` with them. |
| `trace.retain.max_age` | duration | no | Age beyond which a session directory is removed, measured from the millisecond timestamp inside its own uuid-v7 `ses_` id. No file metadata is read. |

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
| `lifecycle.queue_depth` | integer | no | Default: `1`. Maximum number of pending, not-yet-started tasks the capsule holds under `task_acceptance: queue`. Tasks beyond this limit receive `state: "rejected"`. |
| `lifecycle.input_timeout_secs` | integer | no | Maximum seconds to wait for a `message/send` reply after a tool component calls `request-input`. Absent means wait indefinitely — see [`lifecycle.input_timeout_secs`](#lifecycle-input-timeout-secs). |
| `lifecycle.conversation` | `stateless \| threaded` | no | Default: `stateless`. Whether tasks sharing a `contextId` accumulate history — see [`lifecycle.conversation`](#lifecycle-conversation). |
| `lifecycle.max_task_reopens` | integer | no | Default: `1`. Maximum times an `on-task-end` hook (`commit_policy: reopen-task`) may reopen a single task. `0` is a valid explicit value and disables reopening. Reopening never grants turns past `inference.max_turns`; see [Task reopening](../concepts/session-loop.md#task-reopening-commit_policy-reopen-task). |
| `lifecycle.shell_grace_secs` | integer | no | Default: `10`. Seconds a shell command runs in the foreground before it is demoted to the background — see [`lifecycle.shell_grace_secs`](#lifecycle-shell-grace-secs). |
| `lifecycle.delegation_deadline_secs` | integer | no | Default: `600`. Seconds a delegated sub-capsule may run before it is ended — see [`lifecycle.delegation_deadline_secs`](#lifecycle-delegation-deadline-secs). |

#### `exports` { #field-exports }

Opens read-only views onto parts of the accessible workdir, served over the capsule's HTTP listener
without an inference turn — see [Resource plane](resource-plane.md). The two blocks are separate
authorisers over separate subtrees: declaring one grants nothing about the other, and a capsule may
declare either, both or neither.

| Field | Type | Required | Notes |
|---|---|---:|---|
| `exports.files` | block | no | The operator-facing file surface, addressed by path. Absent means every request to it is refused with `no_resource_plane`. |
| `exports.peer_files` | block | no | The peer-facing file surface, addressed by handle — see [`exports.peer_files`](#field-exports-peer-files). Absent means the capsule mints nothing and every redeem is refused with `no_peer_plane`. |
| `exports.files.root` | string | yes | Subtree of the [accessible workdir](workdir.md) the export opens — the directory the agent's tools see as `.`. Must be relative, non-empty and free of `..`. Need not exist when the capsule launches. A root that resolves outside the workdir — because it already exists as a symlink pointing out of it — refuses the launch with `E-CAP-007`. |
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
| `exports.peer_files.root` | string | yes | Subtree of the [accessible workdir](workdir.md) a handle may name. Must be relative, non-empty and free of `..`. Need not exist when the capsule launches. A root that resolves outside the workdir refuses the launch with `E-CAP-007`. Independent of `exports.files.root`, and neither is derived from the other. |
| `exports.peer_files.max_ttl` | integer or suffixed string | see notes | Ceiling on a minted handle's lifetime. A bare integer is seconds; `s`, `m` and `h` suffixes are accepted. Must be greater than zero. Optional under `lifecycle.after_task: exit`, where it defaults to `1h`; **required and at most `15m`** under `lifecycle.after_task: sleep`, which otherwise refuses the launch with [`E-CAP-008`](diagnostics.md#e-cap-008). |
| `exports.peer_files.max_bytes` | integer or suffixed string | no | Default: `10Mi` (10485760). Per-file ceiling on a redeemed read; a larger file is refused with `too_large`. Accepts a bare byte count or one suffixed `Ki`, `Mi` or `Gi`. Must be greater than zero. |

There is no `list` verb and no path addressing on this plane. `share-file` clamps a requested `ttl`
down to `max_ttl` and never up.

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
  endpoint: https://api.anthropic.com
  model: claude-opus-4-5
  driver:
    artifact: murmur-driver-anthropic
    config:
      max_retries: 3
      request_timeout_seconds: 60
```

Name a driver in front of another provider instead and both keys still read the same way. These
key names illustrate the question; each driver documents the keys it reads.

### No — the key belongs to one artifact { #which-config-block-no }

`murmur-driver-anthropic` reads `prompt_cache` and `prompt_cache_ttl` from its own entry in
`artifacts:`:

```yaml
artifacts:
  - name: murmur-driver-anthropic
    version: 1.0.0
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
  - name: murmur-tool-corpus
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
it. The driver must be declared in `artifacts:` with `runtime: driver`; a driver that is named but
not installed fails with `E-RUN-006`.

```yaml
inference:
  transport: http
  endpoint: https://api.anthropic.com
  model: claude-opus-4-5
  api_key: ${ANTHROPIC_API_KEY}
  max_tokens: 4096        # optional per-turn output cap; default 8192
  driver:
    artifact: murmur-driver-anthropic
```

#### Output cap: `inference.max_tokens` { #inference-max-tokens }

`inference.max_tokens` is the per-turn **output** cap — the `max_tokens` field of the payload the
runtime hands the driver, which every driver forwards verbatim to its provider API. Omit it and the
runtime sends `8192`. It is provider-agnostic and reaches every driver through the same wire field,
so no `driver.config` entry is needed for it.

[`context.max_tokens`](#field-context) is the session-wide token budget that decides when
compaction fires, counted across the whole conversation; `inference.max_tokens` bounds a single
response. They are parsed and validated independently and never share a default.

Validation is one-sided: `0` is rejected at parse time, but a value larger than a given model's
documented maximum is neither rejected nor clamped — an over-large cap surfaces as the provider's
own error at request time.

Two interactions worth knowing:

- The anthropic driver caps extended thinking's `budget_tokens` to `max_tokens - 1`, so lowering
  this value also squeezes a configured thinking budget.
- Hook-initiated completions, such as the compaction hook's own summarization call, always use the
  built-in `8192` default. This field caps the agent's own responses, not the runtime's internal
  calls.

#### Endpoint scheme and host validation { #endpoint-validation }

`inference.endpoint` is validated when the manifest is parsed, before any capsule launches or any
network call is made.

| Value | Result |
|---|---|
| Any `https://` URL, any host (`https://api.anthropic.com`) | Accepted |
| `http://` with host `localhost`, or a loopback IP literal (`http://127.0.0.1:11434`, `http://[::1]`) | Accepted — this covers local model servers such as Ollama |
| `http://` with a non-loopback host (`http://api.attacker.example.com`) | Rejected |
| A schemeless or malformed value (`api.anthropic.com`, `"not a url"`) | Rejected |
| Any scheme other than `http`/`https` (`ftp://example.com`) | Rejected |

Each rejection names the endpoint and the reason. The check runs only for `transport: http`, which
is the only transport that accepts `endpoint` at all.

### `transport: process` — CLI subprocess { #transport-process }

Murmur spawns `inference.command` as a subprocess and communicates over stdin/stdout. No
`ANTHROPIC_API_KEY` is required — authentication uses whatever the CLI is already configured with.
The base name of `command` selects the wire protocol: `codex` speaks the codex-exec dialect,
anything else speaks the Claude Code dialect. No WASM driver artifact is needed or staged.

```yaml
inference:
  transport: process
  command: claude        # must be on PATH
  model: claude-haiku-4-5-20251001
  max_turns: 10
```

| Behaviour | Details |
|---|---|
| Pre-flight check | `mur run` verifies `command` is on `PATH` before staging the session. If it is not found the run exits with `E-RUN-006` and a hint naming that CLI's install page. |
| Tool dispatch | When the capsule declares tool artifacts, murmur stands up a loopback MCP server and points the CLI at it, so the model calls the capsule's own tools and murmur executes them. The CLI's built-in host tools are disabled. With no tools declared, the CLI runs as pure inference. |
| Turn limit | Each assistant response counts as one turn, bounded by `inference.max_turns`. |
| Wall-clock limit | One subprocess run — all turns and tool calls — is capped at 10 minutes. |
| Result | The CLI's final result text is written to `out/result.txt`. |
| Observability | Session, inference and tool hooks, `trace.jsonl` and OTel spans are all emitted normally. Token counts are reported as 0, which the subprocess protocol does not carry. |
| Compaction | Does not run. `context.max_tokens` and `inference.compaction` parse but are inert under this transport; the CLI manages its own context. |
| Context seeding | Does not run. The `context.seed_budget` keys parse but are inert, and a `seed-context` an `on-task-start` hook returns is recorded as a rejected [`context_seed`](observability-schemas.md#context-seed) with `reason: "unsupported_transport"`. |

### `inference.api_key` resolution { #inference-api-key }

`api_key` accepts two forms:

- **Literal string:** `api_key: sk-ant-xxxx`
- **Environment variable reference:** `api_key: ${ANTHROPIC_API_KEY}` — resolved at parse time from
  the host environment. If the variable is not set, `mur run` exits with an error before launch.

Only `${UPPER_SNAKE_CASE}` references are expanded. Anything else is treated as a literal value.

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
- **File paths are relative to the directory containing `murmur.yaml`**, not to the session workdir.
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
  - name: murmur-hook-debug
    version: "{{ v.murmur_hook_debug }}"
    runtime: hook

observability:
  otel_endpoint: "http://localhost:4318"
```

| Behaviour | Details |
|---|---|
| Model visibility | Hook artifacts are not included in the tool inventory or the `MURMUR.md` installed-tool list. |
| Invocation order | Multiple hooks are invoked in manifest declaration order for each event. |
| Failure handling | A hook that returns an error does not abort the agent loop. The error is appended to `workdir/logs/hook-<name>.log`. For an `execution_mode: async` hook it is also recorded as a `hook_dispatch_error` event in `trace.jsonl`, since the log is otherwise the only place it appears. A blocking hook's error is additionally surfaced to the agent loop, which is fatal only for compaction. |
| Workdir access | A hook sees one directory, and only where its entry in the capsule manifest grants a `filesystem.scope` — see [Hook capabilities](#hook-capabilities). |
| Reference hook | `murmur-hook-debug` writes one JSON object per event to `workdir/hook-debug.jsonl`. |
| Call deadline | Each hook lifecycle call gets its own wall-clock budget: `capabilities.limits.deadline_seconds` when the manifest sets it, otherwise 30 seconds — well below the capsule-wide 600-second default, so one wedged hook cannot stall a session for most of ten minutes per event. |

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
  - name: murmur-hook-grafana
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
by the preopen and, on a kernel-enforcing host, by Landlock.

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
filesystem destinations and which are payload it only stores, with JSON Schema's `format` keyword
in its own `input_schema`:

| `format` value | Declared on | Effect |
|---|---|---|
| `murmur-destination` | A string property | The value at that location is checked against every `read_only` entry, wherever in the input it sits |
| `murmur-opaque` | An object or array property | The key-name rules above do not descend into that subtree |

```yaml
input_schema: |
  {"type":"object","properties":{
    "edits":{"type":"array","items":{"type":"object","properties":{
      "path":{"type":"string","format":"murmur-destination"}}}},
    "note":{"type":"object","format":"murmur-opaque"}}}
```

An annotation refines where the runtime looks, never whether it refuses: no `format` value permits
a path, and every refusal is still decided by the operator's own `read_only` entries.

| Case | Behaviour |
|---|---|
| A tool that annotates nothing | Judged by key name, exactly as the table above describes |
| `murmur-opaque` on a string property | Ignored; the key-name rules keep running on the object that carries it |
| `murmur-destination` inside a subtree marked `murmur-opaque` | Still checked |
| `murmur-opaque` on the schema's top level | The key-name rules do not run on that tool's input at all; only its declared destinations are checked |
| An annotation behind a `$ref` | Not resolved, so that tool keeps the key-name rules |

A refusal a declared destination triggered names the location in the model's error and in the
trace record: `edits[].path` for the schema above.

A capsule that declares `read_only` and installs a tool whose schema names a path-shaped or
destination-shaped property and annotates nothing fires
[`W-SEC-018`](diagnostics.md#w-sec-018) at staging, naming the tool and the property.

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
component instantiation, and leaves no session workdir behind. `mur run --explain-scope` refuses
the same blocks with the same code. An oversized block is refused rather than truncated, and the
message names both the size it serialized to and the limit.

`config:` written with no value under it is an empty block, not an absent key, and is refused on
the same terms. Omit the key to deliver no variable.

### Shell allow

- Each entry is a bare binary name (`bash`, `jq`) — paths are not supported.
- A `capabilities.shell` block present with an empty `allow` list is rejected at parse time.
- A synthetic tool manifest is written to `workdir/tools/<binary>/murmur.yaml` at session start for
  each listed binary; the agent discovers these alongside artifact-backed tools.

---

## Retention { #retention }

Two stores grow as a capsule runs: the [session directories](workdir.md) under the workdir, and
the [conversation records](workdir.md#the-conversation-record) under `~/.murmur/conversations/`.
`trace.retain` bounds the first, `context.retain` the second. Both blocks are enforced at launch,
by the runtime, and every deletion is written to the running session's trace as a
[`retention` event](observability-schemas.md#retention).

**There are no defaults. A capsule with no `retain:` block deletes nothing, ever.**

| Rule | Effect |
|---|---|
| The block is omitted | Nothing is deleted. Both stores grow without bound, as they always have. |
| The block is present but empty (`retain: {}`, or `retain:` with nothing under it) | Refused at parse time, naming the block. Omitting the block is how a capsule declares no policy. |
| Both keys are present | ANDed. A session or record survives only if it is inside both limits. |
| A key is `0` | Refused at parse time, naming the key. |

Durations are written as an integer optionally suffixed `s`, `m`, `h` or `d`; a bare integer is
seconds.

### What each key measures { #retention-measures }

| Key | Age is measured from | Unit removed |
|---|---|---|
| `trace.retain.max_sessions` | — | One session directory, whole |
| `trace.retain.max_age` | The uuid-v7 timestamp inside the `ses_` id | One session directory, whole |
| `context.retain.max_messages` | — | The oldest messages of one record |
| `context.retain.max_age` | The last write to `conversation.jsonl` | One context directory, whole |

A session directory is an independent unit — nothing references it and nothing spans two of them
— so it is removed whole, taking its `trace.jsonl` and its `blobs/` with it. A record is one unit
that grows, so it is trimmed from the front: the newest `max_messages` stay, each keeping the `id`
it has always carried, and the [header line](workdir.md#record-header) records what went.

### What retention never touches { #retention-never }

| Never removed | Why |
|---|---|
| The running session's own directory, or any `ses_` id at or after it | A capsule launched while this one is running is inside the same workdir, and its session is not this session's to delete. |
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
```

### `lifecycle.task_acceptance` { #lifecycle-task-acceptance }

| Value | Behaviour |
|---|---|
| `none` | Capsule runs from `task.md` if present, then exits. All incoming messages return JSON-RPC error `-32601`. |
| `single` (default) | Capsule accepts one A2A task, runs it, then exits. A second message while a task is active returns `state: "rejected"`. |
| `queue` | Capsule accepts up to `queue_depth` pending tasks simultaneously. Tasks are processed serially; new tasks are accepted as soon as the pending queue drops below `queue_depth`. |

### `lifecycle.after_task` { #lifecycle-after-task }

| Value | Behaviour |
|---|---|
| `exit` (default) | The capsule exits immediately after the task finishes, or after the idle timeout fires. |
| `sleep` | The capsule loops back to wait for the next task. Only useful with `task_acceptance: queue`; with `single` it behaves like `exit`. |

### `lifecycle.input_timeout_secs` { #lifecycle-input-timeout-secs }

Controls how long the capsule waits for a `message/send` reply after a WASM tool component calls
`murmur:task/task#request-input`.

| Value | Behaviour |
|---|---|
| absent (default) | Wait indefinitely — the task stays in `input-required` state until a reply arrives or the process is killed. |
| `N` (positive integer) | If no `message/send` arrives within `N` seconds, the task transitions to `failed` with status message `"input-timeout"`. SSE clients receive a final `TaskStatusUpdateEvent` with `"final":true`. |

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

A backgrounded command hands the turn a handle of the form `wrk_<id>` and the fact that it is
still running. Its full stdout and stderr are written to `logs/<work_id>.log` under the
[capsule workdir](workdir.md), and stay there — the turn is told the path, never the contents.

Nothing accepts a work id, so the handle cannot be used to ask after the command. The result
arrives on its own, as a task with
[origin](../concepts/access-control.md#task-origin-and-trust-class) `completion` in the `bg`
lane, carrying the exit code and the output path. A non-zero exit, a signal kill and a
[resource limit](resource-limits.md#which-limit) all arrive the same way, told apart by the
`status` field on the command's
[`shell_completed`](observability-schemas.md#session-trace-tracejsonl) record.

That completion reaches only a capsule that outlives the task which started the command. Under
the default `after_task: exit` the session ends when the task does, and a command still running
is recorded as `shell_abandoned` and its result is lost. A capsule that means to receive
completions declares `task_acceptance: queue` and `after_task: sleep`:

```yaml
lifecycle:
  task_acceptance: queue
  after_task: sleep
  shell_grace_secs: 2
```

#### What a backgrounded command costs when the host dies

A backgrounded command outlives the runtime that started it. If the runtime is killed — `SIGKILL`,
an out-of-memory kill, a host that goes away — the command keeps running with nothing reading its
output, and nothing of its result survives: no exit code, no `logs/<work_id>.log`, no record of
whether it finished. The work is a full run's compute spent for nothing, so `lifecycle.shell_grace_secs` is
also a decision about how much compute one lost host can waste.

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

The single bound on a delegation. The capsule's own runtime holds the clock; no daemon has to be
reachable for the deadline to fire.

It covers the two waits a delegation has, one per caller:

| Caller | What the deadline bounds |
|---|---|
| [`delegate-task`](runtime-provided-tools.md) | How long the started sub-capsule is watched. On expiry the sub-capsule is ended and a `terminated` outcome is posted to the delegating capsule |
| A plan's `capsule` step | How long the step waits for the sub-capsule's answer. On expiry the step fails and the sub-capsule is stopped |

| Value | Behaviour |
|---|---|
| `600` (default) | A sub-capsule has 10 minutes from the moment it reports itself ready |
| `N` (positive integer) | The same, with an `N`-second window |
| `0` | For a plan step, the first poll after the task is delivered gives up |

Absent is a ceiling, not an absence: a capsule that never declares this one still delegates under
600 seconds, because an unbounded delegation leaves a wedged sub-capsule running for as long as the
capsule that started it. The `MURMUR_DELEGATION_TIMEOUT_SECS` environment variable sets the same
bound for a whole process when it names a positive integer; the declared value applies otherwise,
and `600` when neither is given. Getting the sub-capsule *started* is bounded separately — see
[Bounds](roost-api.md#bounds) — so a slow host does not spend this window on staging.

**Reaching the deadline ends the sub-capsule.** Under `delegate-task` the delegating capsule is
told, in a task of its own: a `completion`-origin task in the `bg` lane carrying the delegation's
`dlg_` id, `status: terminated` and a `detail` naming the bound in seconds. That task is the same
shape every delegation outcome takes — see [The completion path](roost-api.md#the-completion-path).

An outcome reaches only a capsule that outlives the task which made the delegation. Under the
default `after_task: exit` the session ends when the task does, and nothing arrives; a capsule that
delegates and leaves its `lifecycle` block that way is warned at launch with
[`W-SEC-020`](diagnostics.md#w-sec-020). A capsule that means to receive outcomes declares:

```yaml
lifecycle:
  task_acceptance: queue
  queue_depth: 4
  after_task: sleep
  delegation_deadline_secs: 120
```

An outcome that arrives when the delegating capsule's task loop has already ended is recorded
rather than dropped: the sub-capsule's own `completion.json` is written with `delivered: false` and
the reason, and a line goes to stderr.

### `lifecycle.conversation` { #lifecycle-conversation }

Controls whether a task starts from the conversation its `contextId` already has. It governs what
a task *loads*, never what is recorded: both modes append every message to the conversation
record, which [`context.record`](#context-record) is what turns off.

| Value | Behaviour |
|---|---|
| `stateless` (default) | Every task starts with an empty message history. The `contextId` on the incoming message is recorded but has no effect on context. |
| `threaded` | A task that arrives with a `contextId` starts from the whole [conversation record](workdir.md#the-conversation-record) for that context, including messages an earlier session wrote. Each completed task also writes a per-task result to `workdir/out/result_<taskId>.txt` alongside the shared `workdir/out/result.txt`. |

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
| Every other combination | Waits 30 seconds, then runs the agent loop once with an empty task and exits. |

The 30-second window is set by the `MURMUR_A2A_TIMEOUT_SECS` environment variable, which tests use
to keep wait times short.

### CLI overrides

`mur run` accepts two flags that override the manifest's lifecycle without editing the file:

```bash
mur run --manifest murmur.yaml --lifecycle-task-acceptance queue --lifecycle-after-task sleep
```

Values follow the same `snake_case` names as the manifest fields.
