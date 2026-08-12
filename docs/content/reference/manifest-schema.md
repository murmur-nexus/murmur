# Manifest Schema

This page reflects the manifest fields parsed by the **current implementation**.

Murmur currently uses two parsers:

1. **Build/publish manifest parser** (`murmur_artifact::manifest`) — minimal identity fields
2. **Run-time manifest parser** (`murmur_artifact::runtime_manifest`) — capsule artifact + capability fields

---

## 1) Minimal artifact manifest (`mur build`, `mur publish`)

Required fields:

```yaml
name: my-artifact
version: "0.1.0"
```

Rules:

- both fields are required
- both must be strings
- `name` is lowercase letters, digits and inner hyphens (`[a-z0-9-]`, no leading or trailing
  `-`), non-empty and at most 100 characters — anything else fails the build with
  [`E-BLD-001`](build-lints.md#e-bld-001)
- reserved versions (`latest`, `stable`, `edge`) cannot be published

Optional fields:

| Field | Type | Notes |
|---|---|---|
| `execution` | `wasm \| native \| static` | Declares this artifact's registry packaging type directly. When set, it is authoritative for `mur publish` and overrides the role-based derivation (`runtime: skill` → `static`, `runtime: tool` + `implementation: native` → `native`, everything else → `wasm`). Case-insensitive. An unrecognized value is a parse-time error, never a silent fallback. |
| `requires_files` | list<string> | Companion files `mur build` requires to exist alongside `murmur.yaml` — **and the complete list of what it packages** besides the manifest itself. Paths are relative to the source directory and may be nested (`assets/logo.png`). Defaults to `["skill.md"]` for `runtime: skill` and to an empty list for every other role. An explicit value — including `[]` — always overrides that default, so a skill can opt out of the `skill.md` requirement by declaring `requires_files: []`. Missing files fail the build with `BuildError::MissingRequiredFile`, naming the first missing entry. An artifact with a compiled payload must declare it here or the built `.mur.zip` will contain nothing but `murmur.yaml` — for a wasm artifact that is a build failure ([`E-BLD-003`](build-lints.md#e-bld-003)). Entries must be plain relative paths to real files: absolute paths, `..` components and symlinks are rejected ([`E-BLD-002`](build-lints.md#e-bld-002)). |

```yaml
name: my-tool
version: "0.1.0"
runtime: tool
execution: static      # optional: force registry_runtime() to Static instead of the derived Wasm
requires_files:        # optional: the files build_artifact requires — and packages — under source_dir
  - config.json
```

---

## 2) Runtime capsule manifest (`mur run`)

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
  filesystem:
    scope: ./workdir
  shell:
    allow:
      - bash           # binaries the agent may invoke as shell tools
    strip_env:         # optional: env var patterns to strip from subprocess environment
      - AWS_*
    baseline_env:      # optional: env var patterns to keep after stripping
      - PATH
    interpreter_runtime:            # optional: host dirs a path-based interpreter needs (see W-SEC-009)
      - binary: python3             # MUST already appear in allow above
        dirs:
          - path: /usr/lib/python3.11
            list_dir: true          # Execute+ReadFile+ReadDir — the dir's entries are enumerable
          - path: /usr/lib/python3.11/lib-dynload
            list_dir: false         # Execute+ReadFile — files openable by exact name, dir not listable
    staged_runtime:                 # optional: pinned host runtime trees to bind-mount into a sealed root
      - binary: python3             # MUST already appear in allow above; MUST NOT also have an interpreter_runtime grant
        source_path: /opt/testbed/conda/envs/django__django   # absolute host path to an already-pinned tree
        pin: conda-4.10.3/python-3.9.19/testbed-2024-05-01    # required; never inferred
  env:
    allow:             # optional: host env vars a WASM guest (capsule/tool/driver) may observe
      - MY_APP_REGION

network:
  internal_port: 14159  # optional capsule-declared internal port; local mode still binds an OS-assigned external port

context:
  max_tokens: 200000   # enables context compaction; omit to disable

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

inference:
  transport: http
  endpoint: https://api.anthropic.com
  model: claude-opus-4-5
  api_key: ${ANTHROPIC_API_KEY}  # optional; supports literal or ${ENV_VAR}
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
    # dump_summaries: true         # optional; default false — see below
    # The compaction hook is selected by binding, not named here: declare an
    # artifact with `runtime: hook` and `binding: on-compaction` (see artifacts:).
  system_prompt: |              # optional; injected as `system` on every API call
    Always begin responses with CONFIRMED:
  # system_prompt_file: conventions.md  # alternative: load from file
  # max_turns: 10               # optional; default 10

# Alternative: spawn the claude CLI as a subprocess (no ANTHROPIC_API_KEY required)
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
```

### Field reference

#### Identity { #field-identity }

| Field | Type | Required | Notes |
|---|---|---:|---|
| `name` | string | yes | capsule identity |
| `version` | string | yes | capsule version |
| `mur_version` | string | no | pins the `mur` runtime version required by this capsule; see [`mur_version`](#field-mur-version) |

#### `mur_version` { #field-mur-version }

Pins the exact version of the `mur` binary that must run this capsule.

```yaml
mur_version: "0.4.9"
```

**`mur deploy` behaviour:** when set, `mur deploy` downloads `mur-{version}-linux-x86_64` from GitHub releases and installs it on the target VM, regardless of which `mur` version is running locally. The binary is cached at `~/.murmur/bin/mur-{version}-{platform}` and reused on subsequent deploys. If omitted, the version of the locally running `mur` binary is used.

**`mur run` behaviour:** if the field is set and the running `mur` version does not match, a warning is printed to stderr. The run is not aborted — local development commonly runs ahead of a pinned version.

#### `artifacts` { #field-artifacts }

| Field | Type | Required | Notes |
|---|---|---:|---|
| `artifacts` | list | no | defaults to empty |
| `artifacts[].name` | string | yes (per entry) | artifact name |
| `artifacts[].version` | string | yes (per entry), **unless `source` is set** | artifact version. Optional when `source` is present — see `artifacts[].source` below. |
| `artifacts[].runtime` | `tool \| driver \| hook \| skill` | no | defaults to `tool`; `tool` artifacts are model-visible; `driver`, `hook`, and `skill` artifacts are hidden from the model. Using `wasm` or `native` here is a parse error — implementation details belong in the artifact's own `murmur.yaml` (`implementation: wasm \| native`). |
| `artifacts[].source` | string | no | Local path the runtime resolves this artifact from instead of the registry — no `.mur.zip`, no `mur publish`. Requires `local_source: true` (see below). See [Local-source artifacts](#local-source-skills) below. |
| `artifacts[].local_source` | bool | no | Opts this artifact into `source:` resolution. Defaults to `true` for `runtime: skill` and `false` for every other role — an explicit value always overrides that default, in both directions. See [Local-source artifacts](#local-source-skills). |
| `artifacts[].prompt_payload` | bool | no | Opts this artifact into being named by `inference.system_prompt_artifact`. Defaults to `true` for `runtime: skill` and `false` for every other role — an explicit value always overrides that default. See [`inference.system_prompt_artifact`](#inference-system-prompt-artifact). |
| `artifacts[].capabilities` | map | no | Per-artifact capability grant, recognized on `runtime: hook`, `runtime: tool`, and `runtime: driver`. The baseline differs by role: on a hook, absent = no network and no filesystem at all (see [Hook capabilities](#hook-capabilities)); on a tool or driver, absent = the unchanged capsule-wide ceiling, and a declared block *narrows* below it (see [Tool and driver capabilities](#tool-capabilities)). Declaring it on `runtime: skill` is a manifest validation error (`E-MAN-003`). |
| `artifacts[].on_overflow` | `drop \| block` | no | Recognized only on `runtime: hook`; declaring it on any other role is a manifest validation error. Governs what happens when an `execution_mode: async` hook's job queue is full — see [Async hook overflow policy](#hook-overflow). Defaults to `drop`. Legal (but inert) on a hook that turns out to be `execution_mode: blocking`, since a blocking hook has no queue. |

##### Hook capabilities { #hook-capabilities }

A hook runs **default-deny**: a `runtime: hook` entry with no `capabilities:` block gets zero
network capability (no raw WASI sockets, and an empty outbound allow-list so every HTTP request
is denied) and zero preopened directories — it cannot read or write any file, not even in its own
working directory. Network and filesystem are granted back one hook at a time, from that hook's
entry in **your own** `murmur.yaml`:

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
```

Rules:

- **Only the capsule operator can grant.** The grant is read from your capsule manifest's artifact
  entry, never from the hook artifact's own bundled `murmur.yaml`. A `capabilities:` key inside a
  published hook artifact is inert — nothing carried in a `.mur.zip` can widen what the host lets
  that hook do.
- **The grant is per-hook, not shared.** The capsule-wide top-level [`capabilities`](#field-capabilities)
  block does not reach hooks, and a hook's grant does not widen the capsule.
- **`network.allow` uses the same entries and the same enforcement as the capsule-wide block** —
  same `host`, `host:port`, and `scheme://host[:port]` forms, checked by the same allow-list gate a
  capsule's or tool's outbound HTTP goes through. Anything not listed is denied.
- **`filesystem.scope` is a real preopen, not advisory.** Exactly one directory —
  `<workdir>/<scope>` — is mounted as the hook's current directory, and it is created if missing.
  Paths outside that subtree (a sibling artifact's directory, the workdir root, `..`) are
  unreachable because no descriptor for them exists. An absolute scope, or one that escapes the
  workdir via `..`, fails at launch (`E-CAP-002`) before any hook component is instantiated.
- **Only `network` and `filesystem` govern a hook.** `shell`, `spawn`, `env`, and `limits` parse
  here but are capsule-wide concerns the runtime does not apply per-hook; declaring one prints a
  [`W-SEC-006`](security-warnings.md#w-sec-006) warning saying so.

##### Tool and driver capabilities { #tool-capabilities }

A `runtime: tool` or `runtime: driver` entry may carry the same `capabilities:` key, but the
baseline is the opposite of a hook's: **inherit-and-clamp**, not default-deny. An entry with no
`capabilities:` block runs on the full capsule-wide [`capabilities`](#field-capabilities) ceiling,
exactly as every tool did before this key existed. Declaring a block *narrows* that one artifact:

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
```

Rules:

- **Narrowing only ever subtracts.** The effective grant is `declaration ∩ ceiling`. An entry
  naming a host the ceiling does not itself allow is dropped, never granted, and prints a
  [`W-SEC-007`](security-warnings.md#w-sec-007) warning naming the artifact and the dropped entry.
  Staging continues — the resulting posture is tighter than asked for, not looser.
- **A bare host does not fit under a scheme-bound ceiling entry.** `api.example.com` spans both
  schemes and every port, so a ceiling of `https://api.example.com` does not cover it and it is
  dropped. Write the narrowing at least as specific as the ceiling entry it sits under.
- **`network.allow: []` is a real narrowing to zero**, distinct from omitting the key: an explicit
  empty list denies that artifact all outbound HTTP while its siblings keep the ceiling.
- **`filesystem.scope` is a real preopen.** `<accessible workdir>/<scope>` is mounted as that
  artifact's current directory instead of the whole workdir, and is created if missing. `scope: "."`
  is the explicit "whole workdir" grant. An absolute scope, or one escaping via `..`, fails at
  staging (`E-CAP-002`) before the tool runs.
- **Only the capsule operator can grant**, exactly as for hooks: the block is read from your
  manifest's artifact entry, never from the artifact's own bundled `murmur.yaml`.
- **Drivers narrow identically.** The artifact named by `inference.driver.artifact` dispatches
  through the same path as any WASM tool, so a `capabilities:` block on its entry applies to every
  driver call — including one made by a hook's `run-inference`.
- **Only `network` and `filesystem` narrow.** `shell`, `spawn`, `env`, and `limits` parse but are
  inert here and print a [`W-SEC-008`](security-warnings.md#w-sec-008) warning, as does a grant on a
  tool with a **native** (non-WASM) implementation, which never runs through the WASI tool path at
  all.

##### Local-source artifacts { #local-source-skills }

Any artifact can be sourced from a local path instead of the registry by setting `source:` and declaring `local_source: true`. This is the authoring loop for skills (and any other role that opts in): edit the file, relaunch the capsule, and the change is live — no build/publish round-trip.

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

- **`local_source` gates `source:`, not the role directly.** When `local_source` is absent, its value is derived from the role: `true` for `runtime: skill`, `false` for everything else — this reproduces the skill-only behavior every existing manifest already relies on. Declaring `local_source: true` on a `tool`, `driver`, or `hook` artifact opts it in; declaring `local_source: false` on a skill opts it back out. Setting `source:` while `local_source` is `false` (explicit or defaulted) is a manifest validation error (`E-MAN-003`).
- **`version` is optional when `source` is set.** A local file has no registry version, so the runtime substitutes the literal `local` wherever a version is shown (the installed-path label and the `MURMUR.md` listing). If you set `version` anyway, it is ignored and a warning is printed to stderr.
- **Relative paths resolve against the directory containing `murmur.yaml`**, not the current working directory. Absolute paths are used as-is.
- **For a skill, `source` may point at a `skill.md` file or at a directory.** For a directory, the runtime finds `skill.md` case-insensitively (`SKILL.md`, `Skill.md`, … all match) and installs it as lowercase `skill.md`. A file path is read directly regardless of its name.
- Everything downstream is identical to a registry skill: the file is installed to `workdir/tools/<name>/skill.md`, listed under `## Installed Skills` in `MURMUR.md`, and never injected into the system prompt.
- A non-skill artifact declaring `local_source: true` passes manifest validation, but only a skill-shaped local payload (a `skill.md` file or a directory containing one) is currently resolvable at launch — this is a known launch-surface boundary, not a bug.

Failure modes (all exit non-zero before the workdir is created):

| Condition | Error code | Message |
|---|---|---|
| `source` declared without `local_source: true` | `E-MAN-003` | `artifact '<name>' declares 'source:' but does not declare 'local_source: true' (runtime: <role>)` |
| `source` path does not exist | `E-IO-001` | `skill source path not found: <path>` |
| `source` directory has no `skill.md` | `E-IO-001` | `skill source directory '<path>' contains no skill.md` |

Local-source artifacts are never written to `murmur.lock` and are skipped by `mur install` — they are resolved fresh from disk on every `mur run`.

##### `inference.system_prompt_artifact` { #inference-system-prompt-artifact }

Names an artifact declared in `artifacts:` whose content is read once at launch and bound as the system prompt, instead of being called on demand. The named artifact must declare `prompt_payload: true` (or default to it, as `runtime: skill` does) — naming an artifact where `prompt_payload` is `false` is a manifest validation error. See [Shape agent behavior with a system prompt](../how-to/capsule-system-prompt.md#step-4-load-the-prompt-from-a-skill-artifact).

#### `capabilities` { #field-capabilities }

| Field | Type | Required | Notes |
|---|---|---:|---|
| `capabilities.network.allow` | list<string> | no | host/URL allow entries. Governs **IP destinations only — TCP and UDP alike**, decided by destination address and port at `connect(2)`/`sendto(2)`. It has no effect on unix-domain sockets, which `capabilities.network.unix_sockets` governs separately, and none on `AF_NETLINK`/`AF_PACKET`, which are always refused. An empty or absent `allow` therefore means no TCP or UDP destination is reachable, not that the capsule has no way to communicate |
| `capabilities.network.unix_sockets` | bool | no | defaults to **`false`**. When false, the capsule's shell subprocess tree cannot create an `AF_UNIX` socket at all: `socket(AF_UNIX, ...)` fails with `EACCES`, enforced by seccomp on both Linux tiers. Set `true` only if a shell tool genuinely needs a local daemon socket — it is a coarse, capsule-wide grant, not a per-socket-path allowlist, so it re-exposes **every** unix socket the process can reach, `/var/run/docker.sock` (host root) included. `AF_NETLINK` and `AF_PACKET` are always refused and have no equivalent key. No effect on non-Linux hosts, which have no kernel enforcement at all — see [`W-SEC-001`](security-warnings.md#w-sec-001) |
| `capabilities.filesystem.scope` | string | no | relative scope under workdir |
| `capabilities.filesystem.workdir_exec` | bool | no | defaults to **`false`**. When false, the session workdir's Landlock rule withholds the `Execute` right, so nothing the capsule writes there can be executed — under any name, including one that appears in `capabilities.shell.allow`. That withholding is what makes `shell.allow` a complete, kernel-enforced statement rather than a name-matching convention. Set `true` only for compile-and-run workflows (the capsule builds a binary in its workdir and then runs it); doing so makes `shell.allow` unenforceable for anything inside the workdir, caps the capsule's achieved containment class at `advisory` on **every** host, and fires [`W-SEC-011`](security-warnings.md#w-sec-011) at staging. See [Executable workdirs](#field-workdir-exec) below. No effect on non-Linux hosts or on Linux without a usable Landlock ABI, neither of which mediates exec at all — see [`W-SEC-001`](security-warnings.md#w-sec-001) and [`W-SEC-002`](security-warnings.md#w-sec-002) |
| `capabilities.shell.allow` | list<string> | no | shell binaries the agent may invoke (e.g. `bash`); each listed binary gets a synthetic tool manifest staged at launch |
| `capabilities.shell.strip_env` | list<string> | no | glob patterns for env vars to strip from subprocess environment (e.g. `AWS_*`) |
| `capabilities.shell.baseline_env` | list<string> | no | glob patterns for env vars to keep after stripping (e.g. `PATH`) |
| `capabilities.shell.interpreter_runtime` | list<grant> | no | narrows an allowlisted binary's Landlock scope to specific host directories its import machinery needs outside the workdir (a path-based interpreter's stdlib). Fires [`W-SEC-009`](security-warnings.md#w-sec-009) at staging. See below for the per-entry shape. |
| `capabilities.shell.interpreter_runtime[].binary` | string | yes | a binary that MUST already appear in `capabilities.shell.allow` — this narrows filesystem access alongside an existing exec grant, it never grants exec |
| `capabilities.shell.interpreter_runtime[].dirs` | list<dir> | yes | the host directories to grant; must name at least one |
| `capabilities.shell.interpreter_runtime[].dirs[].path` | string | yes | an absolute host path (must start with `/`) outside the workdir |
| `capabilities.shell.interpreter_runtime[].dirs[].list_dir` | bool | yes | `true` → `Execute+ReadFile+ReadDir` (directory entries enumerable); `false` → `Execute+ReadFile` (files openable by exact name, directory not listable). Never inferred — must be written explicitly. |
| `capabilities.shell.staged_runtime` | list<grant> | no | names a pinned host runtime tree to bind-mount **read-only into a `sealed` capsule's composed root**, so the interpreter is present inside the root rather than reachable outside it. Requires an effective `sealed` containment floor, and is mutually exclusive per binary with `interpreter_runtime`. See [Staged runtime](#field-staged-runtime) below. |
| `capabilities.shell.staged_runtime[].binary` | string | yes | a binary that MUST already appear in `capabilities.shell.allow`, and MUST NOT also have a `capabilities.shell.interpreter_runtime` grant — this says where an existing exec grant's runtime comes from, it never grants exec |
| `capabilities.shell.staged_runtime[].source_path` | string | yes | absolute host path (must start with `/`) of an already-pinned runtime tree — a vendored toolchain directory, a baked-in conda env. Not resolved, discovered or version-sniffed by the runtime, and not required to exist on the machine that merely *parses* the manifest |
| `capabilities.shell.staged_runtime[].pin` | string | yes | non-empty, opaque identifier of which build the tree is. Never inferred: it exists so a human can compare the declared pin across two hosts and confirm the same runtime shipped to both |
| `capabilities.env.allow` | list<string> | no | host env var names a WASM guest (capsule/tool/driver component) may observe. Omitted or `[]` — a legitimate no-op, not an error — grants nothing beyond the runtime's own `MURMUR_*` injections. |
| `capabilities.limits.memory_bytes` | integer | no | cap on a component's linear-memory growth, in bytes. Default: 536870912 (512 MiB). Must be > 0. |
| `capabilities.limits.table_elements` | integer | no | cap on a component's table growth, in elements. Default: 100000. Must be > 0. |
| `capabilities.limits.instances` | integer | no | cap on the number of instances a single store may create. Default: 1000. Must be > 0. |
| `capabilities.limits.deadline_seconds` | integer | no | wall-clock budget for a single component call (capsule `run`, tool/driver `run`, or one hook lifecycle call), in seconds. Default: 600 (10 minutes) for a capsule, tool, or driver call; 30 seconds for a hook lifecycle call. An explicit value here overrides both defaults, hooks included. Must be > 0. |
| `capabilities.resources.max_processes` | integer | no | `RLIMIT_NPROC` headroom for each native subprocess — how much past the runtime's own uid baseline its tree may add, in the unit the kernel enforces against (threads on Linux, processes on macOS). Default: 128. Must be > 0. See the per-uid note below. |
| `capabilities.resources.max_open_files` | integer | no | `RLIMIT_NOFILE` hard ceiling on each native subprocess. Default: 1024. Must be > 0. |
| `capabilities.resources.max_file_size_bytes` | integer | no | `RLIMIT_FSIZE` hard ceiling — largest single file a subprocess may write, in bytes. Default: 4294967296 (4 GiB). Must be > 0. |
| `capabilities.resources.cpu_seconds` | integer | no | `RLIMIT_CPU` hard ceiling on each native subprocess, in CPU-seconds. Default: 3600 (1 hour). Must be > 0. |
| `capabilities.resources.memory_bytes` | integer | no | `RLIMIT_AS` (Linux) hard ceiling on each native subprocess's address space, in bytes. Default: 2147483648 (2 GiB). Must be > 0. macOS maps this to `RLIMIT_DATA`, which its kernel does not enforce — see the platform note below. |
| `capabilities.resources.cgroup_memory_bytes` | integer | no | cgroup v2 `memory.max` — aggregate memory across the whole subprocess tree, in bytes. Default: 4294967296 (4 GiB). Must be > 0. Linux only. |
| `capabilities.resources.cgroup_pids_max` | integer | no | cgroup v2 `pids.max` — aggregate task count across the whole subprocess tree. Default: 256. Must be > 0. Linux only. |
| `capabilities.resources.cgroup_cpu_percent` | integer | no | cgroup v2 `cpu.max` quota as a percentage of one core (200 = two cores' worth). Default: 200. Must be > 0. Linux only. |
| `capabilities.resources.cgroup_io_bytes_per_sec` | integer | no | cgroup v2 `io.max` read+write throughput on the workdir's backing device, in bytes/sec. Default: 104857600 (100 MiB/s). Must be > 0. Linux only, best-effort — a workdir whose backing device cannot be resolved (overlayfs, tmpfs, device-mapper) logs a note and keeps the other three controllers. |
| `capabilities.resources.workdir_max_bytes` | integer | no | ceiling on total session-workdir size, in bytes, enforced by a periodic check. Default: 10737418240 (10 GiB). Must be > 0. Every platform. Under the `sealed` containment class this also bounds `/tmp` — see the note below. |
| `capabilities.containment` | string | no | minimum containment class this capsule requires: `advisory`, `scoped`, or `sealed` (ascending strength). Omitted means the capsule states no requirement — see [Containment class](#field-containment) below. Only meaningful capsule-wide; declaring it on a per-artifact (tool/driver/hook) entry has no effect and warns at staging |
| `network.internal_port` | integer | no | Capsule-declared internal service port. The local runtime currently binds an OS-assigned external localhost port for `MURMUR_CAPSULE_URL`; when omitted, the intended internal default is `14159`. |

Under the [`sealed` containment class](#field-containment), `/tmp` inside the composed root is
writable, and it is backed by a directory *inside* the session workdir. Writes to `/tmp` therefore
count against `capabilities.resources.workdir_max_bytes` like any other workdir write, and are
discarded when the session ends — a `sealed` capsule cannot use `/tmp` to escape its workdir ceiling
or to leave anything behind on the host.

A `capabilities.network.allow` host that fails DNS resolution at launch is skipped, not treated as a fatal error — the run proceeds with that host simply contributing no addresses to the launch-time IP allowlist that `capabilities.shell.allow` subprocesses fall back to when they reach a destination by address rather than by name (a native subprocess's `network.allow` is enforced day to day by name, through the capsule's own DNS responder, not by this launch-time set alone). This only ever *shrinks* what a shell binary can reach; it does not widen what the capsule's own outbound HTTP calls may reach, since that check is a host-pattern match against `network.allow` and never depends on DNS. Malformed host *syntax* (as opposed to a resolution failure) is still rejected outright.

A WASM guest never inherits the host process's environment: `capabilities.env.allow` is the only way to expose a host variable, and even a name declared there is dropped if it is credential-shaped (see [Lock down a capsule's capabilities](../how-to/lock-down-capsule.md#step-2-manage-the-subprocess-environment) for the exact pattern list — the same backstop applies here) or matches `capabilities.shell.strip_env`. A declared-but-unset host variable is silently omitted, not an error.

### Executable workdirs { #field-workdir-exec }

`capabilities.filesystem.workdir_exec` decides one Landlock bit: whether the session workdir's own
rule carries the `Execute` right.

**The default (`false`, and what every manifest that omits the key gets).** The workdir is
readable and writable but not executable. Each binary named in `capabilities.shell.allow` gets its
own narrow read+execute grant at its real host path — the binary, its ELF interpreter, and its
`DT_NEEDED` shared-library closure — so allowlisted programs run normally from `/usr/bin` and
friends. Nothing the capsule *produces* runs. The decision is made by the kernel, on the path it
resolved itself, so there is no name to spoof and no window to race:

```console
$ # inside a capsule with `shell.allow: [bash]` and workdir_exec absent
$ cp /usr/bin/nc ./bash && ./bash -l
bash: ./bash: Permission denied
```

This closes a bypass a prior release shipped, where exec was decided by a userspace supervisor
comparing a pathname it read out of the calling process. That comparison could be defeated by
renaming a binary to an allowlisted basename, and — per `seccomp_unotify(2)` — raced even when it
could not. Withholding one Landlock right replaces the whole mechanism.

**The cost, stated plainly.** A binary the capsule legitimately compiled cannot run either:

```console
$ gcc -o ./hello hello.c && ./hello
bash: ./hello: Permission denied
```

That is the accepted price of the default, not a bug to work around.

**`workdir_exec: true`.** The workdir keeps `Execute`, and compile-and-run works. In exchange,
`capabilities.shell.allow` stops being an enforceable property of the capsule: anything written
into the workdir runs regardless of what the allowlist says. The runtime does not pretend
otherwise —

* the capsule's achieved containment class is `advisory`, on every host, including a
  Landlock-capable one;
* `mur run --explain-scope` prints `workdir exec: true` and the `advisory` it forced;
* `trace.jsonl`'s `session_start` carries `workdir_exec: true`;
* [`W-SEC-011`](security-warnings.md#w-sec-011) fires once at staging;
* pairing it with `capabilities.containment: scoped` (or `sealed`) refuses the launch.

```yaml
capabilities:
  filesystem:
    workdir_exec: true           # compile-and-run; shell.allow is advisory inside the workdir
  shell:
    allow:
      - bash
      - gcc
```

The refusal, when a manifest asks for both, names the manifest rather than the host — because no
host can satisfy it:

```text
error[E-CAP-003]: declared containment class 'scoped' is not achievable on this host (achieved: 'advisory'): capabilities.filesystem.workdir_exec: true keeps the Landlock Execute right on the session workdir, so a binary the capsule compiles, downloads or renames inside it runs regardless of capabilities.shell.allow — the allowlist stops being an enforceable property of this capsule. No host can back a class above advisory for it. Either remove workdir_exec (the allowlist is then enforced by the kernel on the resolved path) or lower the declared containment floor to advisory
  hint: lower the declared floor to 'advisory' (capabilities.containment in murmur.yaml, containment in .murmur/config.yaml, or --containment), or run on a host that provides 'scoped'
```

**Where it has no effect.** The bit is a Landlock right, so a host with no usable Landlock ABI
(Linux < 5.13) and every non-Linux host ignore it entirely — on those hosts nothing mediates exec
either way, which is exactly why neither can reach `scoped`. `W-SEC-001` and `W-SEC-002` already
say so.

### Staged runtime { #field-staged-runtime }

`capabilities.shell.staged_runtime` and `capabilities.shell.interpreter_runtime` solve the same
problem — a path-based interpreter cannot run from its binary alone, because its stdlib lives
outside the workdir at a path the `DT_NEEDED` closure cannot discover — and they solve it in
opposite directions.

| | `interpreter_runtime` | `staged_runtime` |
|---|---|---|
| Direction | widens the capsule's Landlock scope **outwards** to host paths | bind-mounts the runtime tree **inwards** into the capsule's own root |
| Floor required | any (works at `scoped`) | `sealed` only |
| Host paths reachable from inside | yes, the granted directories | no |
| Coupled to one host's layout | yes — fires [`W-SEC-009`](security-warnings.md#w-sec-009) | no, and `pin` makes the coupling checkable |
| Granularity | specific directories, each with an explicit `list_dir` | the whole named tree, read-only |

Because the second makes the first unnecessary for any binary that uses it, **declaring both for
the same binary is rejected at parse time.** It is a contradiction, not a layering: a composed root
does not contain the host directories an `interpreter_runtime` grant names, so the grant would
describe paths that do not exist inside the capsule.

```yaml
capabilities:
  containment: sealed              # required — see below
  shell:
    allow:
      - python3
    staged_runtime:
      - binary: python3
        source_path: /opt/testbed/conda/envs/django__django
        pin: conda-4.10.3/python-3.9.19/testbed-2024-05-01
```

**`sealed` is required, and it is checked against the declared floor.** A `staged_runtime` grant is
staged into a composed root, and a composed root is built only for a capsule that asked for
`sealed` (see [A weaker declaration is never silently upgraded](#field-containment)). So a capsule
declaring `staged_runtime` below an effective `sealed` floor is refused before any registry pull,
artifact compile or workdir creation:

```text
error[E-CAP-004]: capabilities.shell.staged_runtime is declared for python3 but the effective containment floor is 'scoped' — staging a runtime tree requires the 'sealed' floor, because there is no composed root to bind-mount it into below that
  hint: set `capabilities.containment: sealed` in murmur.yaml (or pass `--containment sealed`) so the capsule gets a composed root to stage the runtime into, or remove the capabilities.shell.staged_runtime grant.
```

This is deliberately **not** the same check as `E-CAP-003`, and the two remedies are opposites.
`E-CAP-003` means the host is too weak for what the capsule declared — lower the floor or move
hosts. `E-CAP-004` means the capsule declared too little for what it asked for — raise the floor or
drop the grant. `E-CAP-004` therefore fires identically on a host that could deliver `sealed`,
because the operator never asked for it. `mur doctor` surfaces the same condition as a warning
ahead of a run.

**`pin` is for humans, not for the runtime.** Nothing parses it, matches it, or verifies it against
the tree at `source_path`. It exists so that "the same interpreter build ran on both hosts" is a
claim someone can check by comparing two manifests, rather than one assumed from two directories
having the same name. `mur run --explain-scope` prints every declared grant with its pin, on every
host and whatever the floor:

```text
  staged runtime:
    - python3: /opt/testbed/conda/envs/django__django (pin: conda-4.10.3/python-3.9.19/testbed-2024-05-01)
```

`--explain-scope` is a diagnostic, so it reports the grant even where `mur run` would refuse —
which is exactly the case an operator is inspecting.

#### Under `sealed`, a missing grant is caught at launch { #sealed-reachability-checks }

Staging a `shell.allow` binary's own ELF dependency closure into the composed root is necessary for
that binary to run, and for two kinds of program it is not sufficient. Either would otherwise launch
cleanly and fail deep into a run, so under a declared `sealed` floor both are decided at staging,
before any registry pull, artifact compile or workdir creation.

**An interpreted entrypoint refuses with `E-CAP-006`.** A console script such as `pip` at
`~/.local/bin/pip` is a `#!` script, not an ELF image, so its dependency closure is *empty* — the
staging that makes `/usr/bin/bash` work stages nothing at all of what the script imports, and the
package it needs (`~/.local/lib/python3.12/site-packages/pip`) is a different directory nothing
derives. Under `sealed`, `mur run` refuses unless one of the following holds: the script
already resolves under a fixed sealed runtime path (`/usr`, `/bin`, …), or it lives inside a
directory a `staged_runtime`/`interpreter_runtime` grant already names, or some such grant names
the script or its shebang interpreter:

```text
error[E-CAP-006]: capabilities.shell.allow grants 'pip' (/home/dev/.local/bin/pip, a script run by 'python3') under the 'sealed' containment floor, but nothing declared makes the interpreted entrypoint's own package tree reachable inside the composed root — ...
  hint: declare `capabilities.shell.interpreter_runtime` (or `staged_runtime`) for the interpreter named above, listing the directories its import machinery actually reads — measure them on this host with `strace -f -e trace=openat,getdents64 <the command>` rather than guessing ...
```

The name match is deliberately loose, and the guarantee is correspondingly narrow: declaring
`interpreter_runtime` for `python3` satisfies every `python3` script, whatever directories the
grant names. murmur does not try to derive an interpreted program's import closure — `sys.path`,
`.pth` files and whatever the script does at runtime make that undecidable in general — so this
check verifies you declared *something*, never that the directory you declared is the right one.
Measuring it is still yours to do, which is why the hint names the `strace` invocation.

Distinguish this from `E-CAP-004` above: that one fires when a grant exists at too low a floor
(*raise the floor*), this one when the floor is already `sealed` and no grant exists (*add a
grant*).

**A compiler driver warns with `W-SEC-012`.** `cc`/`gcc`/`g++`/`c++` fork and exec `cc1`,
`cc1plus`, `as`, `ld` and `collect2` — separate binaries, outside the driver's own dependency
closure, living under `/usr`, which a composed root binds read-only and grants `ReadFile +
ReadDir` but deliberately **not** `Execute`. The driver therefore starts and the first real compile
fails partway through. This one warns rather than refuses, because the probe behind it is a
heuristic over a fixed driver/helper table; see
[`W-SEC-012`](security-warnings.md#w-sec-012) for the full comparison and the fix.

Both checks read only the *declared* floor, never a host probe, and are completely inert at
`scoped` and `advisory` — below `sealed` there is no composed root, and the host filesystem is
simply the host filesystem. `mur doctor` surfaces both ahead of a run, as warnings, without
launching anything.

### Containment class { #field-containment }

A containment class is a floor requirement — "don't launch me unless the host can actually enforce
at least this much" — as opposed to a capability grant like `network.allow` or `shell.allow`, which
describe *what* is allowed once a session is running. Three classes exist, weakest to strongest:

| Class | Meaning |
|---|---|
| `advisory` | No kernel-level enforcement required. Every host satisfies this, including macOS and older Linux. |
| `scoped` | Landlock filesystem mediation + seccomp syscall filtering over the host filesystem. Requires Linux 5.13+ with a usable Landlock ABI. |
| `sealed` | Mount-namespace isolation onto a composed root, with Landlock and seccomp still applied inside it. Everything outside that root is *absent*, not merely denied. Requires an uncontainerised Linux host with a usable Landlock ABI and unprivileged user namespaces; on an AppArmor host, the profile shipped with `mur` must be loaded. |

!!! note "`sealed`'s one documented exception: `/proc`"

    A private `procfs` needs a privilege an unprivileged user namespace does not have, so on most
    hosts a `sealed` root carries the host's `/proc` instead. **Host process metadata is visible
    under `/proc` inside a `sealed` capsule**, as it is under `scoped`; opens through it are still
    refused. Every other axis of the root — `/etc`, `/dev`, block devices, sockets, other users'
    homes — is absent, not merely denied. Where the runtime does hold that privilege, a private
    masked `/proc` is used and this exception does not apply.

**Declaring a floor.** Three independent sources can each declare a minimum class, and they combine
by taking the **strongest** requested — never the weakest:

1. `capabilities.containment` in `murmur.yaml` (this field)
2. `containment` in `.murmur/config.yaml`, global or project scope (see [Configuration files](cli.md#configuration-files); note this key uses strongest-wins merging, not the usual project-wins rule)
3. `mur run --containment <advisory|scoped|sealed>` on the command line

Any source left undeclared contributes nothing; if all three are undeclared, the effective floor is
`advisory`. A CLI flag or workspace default can only *raise* the floor a manifest already set —
never lower it.

**Achieved class.** `mur run` derives the class the host can *actually* provide by probing the
kernel directly (never by trusting the manifest). The probe is a conjunction, and every element has
to hold for the next class up: a Landlock-capable Linux 5.13+ host achieves `scoped`; a host that
also creates an unprivileged user+mount namespace and mounts inside it — verified by really doing
it in a forked child, not by reading a version string — achieves `sealed`; every other host (older
Linux without Landlock, or macOS) achieves `advisory`. Granting a `scoped` capsule access to host
paths outside the workdir via `capabilities.shell.interpreter_runtime` never changes the achieved
class.

**One manifest property does lower it, and only lower it.**
`capabilities.filesystem.workdir_exec: true` caps the achieved class at `advisory` on every host,
including a `sealed`-capable one. This is not the host probe changing its mind — the probe still
reports what the machine can do, and `mechanism:` in `--explain-scope` still names the full tier.
It is the capsule giving up the claim `scoped` makes: with an executable workdir, `shell.allow` is
no longer something the kernel can hold the capsule to. Nothing in a manifest can ever *raise* an
achieved class. See [Executable workdirs](#field-workdir-exec).

**A weaker declaration is never silently upgraded.** On a `sealed`-capable host, a capsule
declaring `scoped` still runs with `scoped`'s mechanism — Landlock and seccomp over the host
filesystem, no composed root. Installing one anyway would delete the host paths its
`interpreter_runtime` grants legitimately name, weakening the capsule rather than strengthening it.
The achieved class reported in the trace still says what the *host* can back; the mechanism
installed follows what the capsule *asked for*.

**Refusal.** If the achieved class is weaker than the effective declared floor, `mur run` refuses to
launch before any registry pull, artifact compile, or workdir creation, with `error[E-CAP-003]`
naming both classes and the reason (missing mechanism, e.g. "Landlock filesystem mediation is
unavailable on this host"):

```text
error[E-CAP-003]: declared containment class 'scoped' is not achievable on this host (achieved: 'advisory'): scoped requires Landlock filesystem mediation (Linux 5.13+ with a usable Landlock ABI); this host provides no kernel filesystem mediation, so paths outside the workdir are constrained by convention only
  hint: lower the declared floor to 'advisory' (capabilities.containment in murmur.yaml, containment in .murmur/config.yaml, or --containment), or run on a host that provides 'scoped'
```

For `sealed` the reason names the *specific* missing piece and the command that fixes it, because
"no mount namespace here" has several completely different causes. On an AppArmor host without the
shipped profile:

```text
error[E-CAP-003]: declared containment class 'sealed' is not achievable on this host (achieved: 'scoped'): sealed requires an unprivileged user+mount namespace, and AppArmor's unprivileged-userns restriction is active on this host while the 'mur-sealed' profile is not confining this binary. Install and load the profile shipped with mur: `sudo install -m 644 packaging/apparmor/mur-sealed /etc/apparmor.d/mur-sealed && sudo apparmor_parser -r /etc/apparmor.d/mur-sealed` (or re-run the mur installer as root), then re-run. To turn the restriction off host-wide instead: `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0`.
  hint: lower the declared floor to 'scoped' (capabilities.containment in murmur.yaml, containment in .murmur/config.yaml, or --containment), or run on a host that provides 'sealed'
```

Inside a container without `CAP_SYS_ADMIN` — the case a CI or containerised test harness hits — the
same declaration refuses with a different remediation, and never falls back to `scoped`:

```text
error[E-CAP-003]: declared containment class 'sealed' is not achievable on this host (achieved: 'scoped'): sealed requires unshare(CLONE_NEWUSER | CLONE_NEWNS), which this host refused. This is the usual answer inside a container: CAP_SYS_ADMIN is absent, or the container's own seccomp filter blocks unshare(2). Either add `--cap-add SYS_ADMIN` to the container invocation, or establish the mount namespace outside the container and run mur inside it. The runtime will not fall back to a weaker class.
  hint: lower the declared floor to 'scoped' (capabilities.containment in murmur.yaml, containment in .murmur/config.yaml, or --containment), or run on a host that provides 'sealed'
```

A `sealed` host that clears the probe at launch and *then* fails to build the composed root for a
particular subprocess is a different event and gets its own code, `E-RUN-014`. It means something
moved underneath the runtime mid-session (a profile reloaded, a container policy changed), not that
the floor was mis-declared.

A manifest that never declares `capabilities.containment` is never gated by this check — the
effective floor resolves to `advisory`, which every host satisfies.

#### `mur run --explain-scope` { #explain-scope }

Prints the declared floor (and which source supplied it), the achieved class, whether the floor is
met, and the resolved capability grant set (network allow, `unix_sockets` policy, filesystem scope,
shell allow, and Landlock grant paths) — then exits `0` without contacting the registry, compiling
any component, creating a workdir, or launching a session, even when the floor would not be met on
this host:

```text
Containment
  declared:  sealed
  achieved:  advisory
  floor met: no
  reason:    sealed requires a Linux mount namespace + pivot_root; this platform has no such primitive and never will (macOS and every non-Linux target stay at advisory permanently)
  mechanism: none

Effective grants
  filesystem scope: <none>
  workdir exec:     false
  network allow:
    - https://api.anthropic.com
  unix sockets:     false
  shell allow:
    - python3
  spawn allow: <none>
  env allow: <none>
  interpreter runtime:
    - python3: /usr/lib/python3.11 (list_dir)

This is a report only — `mur run` without --explain-scope would refuse to launch here.
```

Pass `--json` alongside `--explain-scope` for a single machine-readable JSON line instead. The
report shows *declared* network destinations, not resolved IPs — it deliberately skips DNS
resolution and the shell allowlist's DT_NEEDED closure to stay fast and read-only.

`mechanism:` is a stable wire name for the probed host tier, not a `Debug` rendering:

| `mechanism` | achieved class | what the host provides |
|---|---|---|
| `mountns+pivot_root+landlock+seccomp` | `sealed` | private mount namespace pivoted onto a composed root, with Landlock and seccomp inside it |
| `landlock+seccomp` | `scoped` | Landlock filesystem mediation + the seccomp syscall allowlist over the host filesystem |
| `seccomp-only` | `advisory` | Linux without a usable Landlock ABI: seccomp only, filesystem scope by convention |
| `none` | `advisory` | no kernel sandboxing primitive (macOS and every non-Linux target) |

**`trace.jsonl`** records both classes on every session, in the `session_start` event's
`containment_declared` and `containment_achieved` fields (see [session trace
schema](cli.md#session-trace-tracejsonl)) — regardless of whether `capabilities.containment` was
ever declared. The same event carries `workdir_exec`, always, so a trace showing
`containment_achieved: advisory` can be told apart from one written on a host with no Landlock:
`workdir_exec: true` means the capsule chose it.

The same `session_start` event also carries `effective_grants`: the complete report above —
network allow, shell allow, spawn allow, env allow, interpreter runtime grants, and all — recorded
verbatim, field for field identical to what `--explain-scope --json` prints for the same manifest
on the same host. `containment_declared`/`containment_achieved`/`workdir_exec` stay as their own
top-level fields for convenience; `effective_grants` is the one place a finished trace answers
"network access to *where*, shell access to *what*" without the manifest in hand.

#### `inference` { #field-inference }

| Field | Type | Required | Notes |
|---|---|---:|---|
| `inference.transport` | string | yes (if `inference` present) | `http` (WASM driver) or `process` (claude CLI subprocess) |
| `inference.model` | string | yes (if `inference` present) | model identifier passed to the driver or CLI |
| `inference.max_turns` | integer | no | Maximum LLM inference calls per session. Default: `10`. Must be > 0. |
| `inference.max_task_reopens` | integer | no | Maximum times an `on-task-end` hook (`commit_policy: reopen-task`) may reopen a single task. Default: `1`. Unlike `max_turns`, `0` is a valid explicit value — it disables reopening entirely. Reopening never grants turns past `inference.max_turns`; see [Task reopening](../concepts/capsule-runtime.md#task-reopening-commit_policy-reopen-task). |
| `inference.max_tokens` | integer | `http` only | Maximum output tokens the model may generate **per turn**, sent as `max_tokens` in the driver wire payload. Default: `8192`. Must be > 0; not clamped at the top end. Invalid for `transport: process`. Distinct from [`context.max_tokens`](#field-context), which is the session-wide budget for compaction. |
| `inference.endpoint` | string | `http` only | base URL for inference API requests; invalid for `transport: process`. Must be `https://` (any host) or `http://` with a loopback host (`localhost` or an `IpAddr::is_loopback()` address, e.g. `127.0.0.1`, `::1`) — see [Endpoint scheme and host validation](#endpoint-validation) |
| `inference.api_key` | string | no | literal value or `${ENV_VAR}` reference; `http` transport only; invalid for `transport: process` |
| `inference.driver.artifact` | string | `http` only | inference driver artifact name; must be declared in `artifacts:` with `runtime: driver`; invalid for `transport: process` |
| `inference.driver.config` | object | no | driver-specific JSON object passed via WASI env (`MURMUR_INFERENCE_DRIVER_CONFIG`); `http` transport only |
| `inference.provider.artifact` | string | legacy | accepted for backward compatibility; `inference.driver.artifact` wins when both are set |
| `inference.command` | string | `process` only | CLI binary name to spawn; must be on `PATH`. Invalid for `transport: http`. Defaults to `claude` when omitted. |
| `inference.compaction.threshold` | float (0.0–1.0] | no | Fraction of `context.max_tokens` at which compaction fires. Default: `0.98`. |
| `inference.compaction.model` | string | no | Model override for compaction calls. Defaults to the primary inference model. |
| `inference.compaction.system_prompt` | string | no | System prompt override for compaction calls, passed verbatim to the compaction hook. No trimming, length limit, or format check. Defaults to `none`; the compaction hook picks its own default prompt. Mutually exclusive with `inference.compaction.system_prompt_file`. |
| `inference.compaction.system_prompt_file` | string | no | Path to a file whose content is passed verbatim as the compaction system prompt. Relative to the manifest directory; read when the session launches. Mutually exclusive with `inference.compaction.system_prompt`. |
| `inference.compaction.dump_summaries` | boolean | no | When `true`, every committed compaction appends one JSON line to `out/compaction-summaries.jsonl` in the session workdir, recording the verbatim summary text plus token counts. Default: `false` — no file is written. |
| `inference.system_prompt` | string | no | Text injected verbatim as the `system` parameter on every inference call. Mutually exclusive with `inference.system_prompt_file`. |
| `inference.system_prompt_file` | string | no | Path to a file whose content is injected as the system prompt. Relative to the manifest directory. Mutually exclusive with `inference.system_prompt`. |

#### `context` { #field-context }

| Field | Type | Required | Notes |
|---|---|---:|---|
| `context.max_tokens` | integer | no | Token budget for the session. Required to enable compaction; omit to disable entirely. Must be > 0. Not to be confused with [`inference.max_tokens`](#field-inference), the per-turn output cap. |

#### `observability` { #field-observability }

| Field | Type | Required | Notes |
|---|---|---:|---|
| `observability.otel_endpoint` | string | no | OTLP/HTTP endpoint for OTel span export (e.g. `http://localhost:4318`). When set, the runtime emits a full span tree at session end and injects `MURMUR_OTEL_ENDPOINT` into every hook component's WASI environment. When absent, no outbound telemetry is sent and `trace.jsonl` is the only output. Empty strings are treated as unset. |
| `observability.eval.dataset_id` | string | no | Labels the `dataset_run` record in `eval.jsonl` and is forwarded to hooks as `MURMUR_DATASET_ID`. Useful when diffing runs from multiple datasets. |
| `observability.eval.scorers` | list | no | List of scorer configurations (see below). When present with at least one valid scorer, `murmur-hook-eval` writes `eval.jsonl`. When absent or empty, the hook is a no-op and logs a warning. |
| `observability.eval.scorers[].type` | string | yes (per entry) | Scorer type. One of: `exit_ok`, `max_turns`, `max_tokens`, `tool_sequence`, `llm_judge` (stubbed — no score emitted, warning logged). |
| `observability.eval.scorers[].name` | string | yes (per entry) | Scorer name; appears as the key in `eval.jsonl` score records. |
| `observability.eval.scorers[].max` | integer | yes for `max_turns`, `max_tokens` | Upper bound for the scorer. |
| `observability.eval.scorers[].expected` | list<string> | yes for `tool_sequence` | Ordered list of tool names that must appear as a subsequence of observed calls. |

#### `lifecycle` { #field-lifecycle }

| Field | Type | Required | Notes |
|---|---|---:|---|
| `lifecycle.task_acceptance` | `none \| single \| queue` | no | How the capsule accepts incoming A2A tasks. `none`: runs from `task.md` only, rejects all incoming messages. `single`: accepts one task then exits (default). `queue`: accepts a queue of tasks, processing them serially. |
| `lifecycle.after_task` | `exit \| sleep` | no | What the capsule does after completing a task. `exit`: terminate immediately (default). `sleep`: return to the idle loop and wait for the next task (only meaningful with `queue`). |
| `lifecycle.queue_depth` | integer | no | Maximum number of pending (not-yet-started) tasks the capsule will hold when `task_acceptance: queue`. Default: `1`. Tasks beyond this limit receive `state: "rejected"`. |
| `lifecycle.input_timeout_secs` | integer | no | Maximum seconds to wait for a `message/send` reply after a tool component calls `request-input`. Absent means wait indefinitely. When the timeout fires the task transitions to `failed` with message `"input-timeout"`. |
| `lifecycle.conversation` | `stateless \| threaded` | no | Conversation threading mode. `stateless` (default): every task starts with an empty context. `threaded`: tasks sharing a `contextId` load and persist the full conversation history for that thread. |

---

## Inference configuration { #inference-config }

The `inference` block is optional for plain tool capsules. Two transports are supported:

### `transport: http` — WASM driver { #transport-http }

The default transport. Murmur loads a WASM driver artifact and routes all inference calls through it. Requires `inference.driver.artifact` to be declared in `artifacts:` with `runtime: driver`. If the driver artifact is missing at launch, `mur run` exits with `error[E-RUN-005]`.

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

`inference.max_tokens` is the per-turn **output** cap — the `max_tokens` field of the payload the runtime hands the driver, which every driver forwards verbatim to its provider API. Omit it and the runtime sends `8192`, the value that was previously hard-coded.

Two things it is *not*:

- It is **not** `context.max_tokens`. That one is the session-wide token budget that decides when compaction fires, counted across the whole conversation. This one bounds a single response. They are parsed and validated independently and never share a default.
- It is **not** a driver-specific setting. It is provider-agnostic and reaches every driver through the same wire field, so no `driver.config` entry is needed for it.

Validation is deliberately one-sided: `0` is rejected at parse time as an authoring mistake, but a value larger than any given model's documented maximum is **not** rejected or clamped — an over-large cap surfaces as the provider's own error at request time rather than as a manifest load failure against a ceiling Murmur would have to guess.

One interaction worth knowing: the anthropic driver caps extended thinking's `budget_tokens` to `max_tokens - 1`, so lowering this value also squeezes a configured thinking budget.

Hook-initiated completions (e.g. the compaction hook's own summarization call) always use the built-in `8192` default, not this value — it caps the agent's turns, not the runtime's internal calls.

#### Endpoint scheme and host validation { #endpoint-validation }

`inference.endpoint` is validated at manifest parse time, before any capsule launches or any network call is made. This closes the "redirect the trust root" attack where a manifest points inference traffic at an attacker-controlled host over plain HTTP.

Accepted:
- Any `https://` URL, regardless of host (e.g. `https://api.anthropic.com`).
- An `http://` URL whose host is the literal string `localhost`, or an IP literal for which Rust's `IpAddr::is_loopback()` returns `true` (e.g. `127.0.0.1`, `::1`) — this covers local model servers such as Ollama (`http://localhost:11434`).

Rejected, with a manifest error naming the endpoint and the reason:
- An `http://` URL whose host is not loopback (e.g. `http://api.attacker.example.com`).
- A schemeless or otherwise malformed value (e.g. `api.anthropic.com`, `"not a url"`).
- Any scheme other than `http`/`https` (e.g. `ftp://example.com`).

This check only applies to `transport: http`; it does not run for `transport: process`, which rejects `endpoint` outright.

### `transport: process` — Anthropic CLI subprocess { #transport-process }

Murmur spawns the `claude` CLI as a persistent subprocess and communicates via JSON-lines over stdin/stdout. No `ANTHROPIC_API_KEY` is required — authentication uses whatever the `claude` CLI is already configured with (typically Claude Max or a Pro subscription).

```yaml
inference:
  transport: process
  command: claude        # must be on PATH; defaults to "claude" if omitted
  model: claude-haiku-4-5-20251001
  max_turns: 10
```

**Pre-flight check:** `mur run` verifies that `command` is on `PATH` before creating the workdir. If not found, it exits with `error[E-RUN-006]` and a hint to install the claude CLI.

**Field rules for `transport: process`:**

- `command`, `model` — required
- `endpoint`, `driver.artifact`, `api_key`, `max_tokens` — all invalid; setting any of these is a manifest error
- No WASM driver artifact is needed or staged

**What the subprocess sees:**

The process is spawned with these flags:

```
claude --print --output-format stream-json --verbose \
       --input-format stream-json --model <model> --tools ""
```

Murmur writes the task as a single JSON-line to stdin, then closes stdin. It reads stdout line-by-line, counting assistant turns and enforcing `max_turns`. A 10-minute wall-clock timeout applies. The final result (the `"result"` field from the claude result event) is written to `out/result.txt`.

**Observability:** session/inference/tool hooks, `trace.jsonl`, and OTel spans are all emitted normally. Token counts are reported as 0 (not available from the subprocess protocol).

**Limitations:** capsule WASM tools are not exposed to the subprocess. The claude CLI uses its own built-in tools (e.g. `Bash`, `WebSearch`) — these are visible in the event stream for hook observability but are not dispatched through murmur's tool registry. Use `transport: http` when you need murmur-managed tool dispatch.

### `inference.api_key` resolution { #inference-api-key }

`api_key` accepts two forms:

- **Literal string:** `api_key: sk-ant-xxxx`
- **Environment variable reference:** `api_key: ${ANTHROPIC_API_KEY}` — resolved at parse time from the host environment. If the variable is not set, `mur run` exits with an error before launch.

Only `${UPPER_SNAKE_CASE}` references are expanded. Anything not matching that pattern is treated as a literal value.

### `inference.system_prompt` / `system_prompt_file` { #inference-system-prompt }

The `inference.system_prompt` and `inference.system_prompt_file` fields let you inject a
static prompt as the top-level `system` parameter on every API call — including the first
turn and every subsequent turn in a multi-turn session.

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

- **Mutually exclusive** — setting both fields is a manifest error (`error[E-MAN-003]`).
- **File path** is relative to the directory containing `murmur.yaml`, not to the session workdir.
- **File is read once at launch time** (before any inference call). If the file is missing or unreadable, `mur run` exits with `error[E-RUN-009]` before making any API call.
- **File content is used verbatim** — whitespace is preserved; no trimming is applied to the file's contents.
- When neither field is set, no `system` parameter is emitted — behaviour is unchanged from a manifest without the field.

---

## Hook artifacts { #hook-artifacts }

Declare hook artifacts with `runtime: hook` in `artifacts:`. Hook artifacts are WASM components that implement `murmur:hook/lifecycle`; the runtime calls them synchronously at fixed lifecycle points and discards successful return values.

```yaml
artifacts:
  - name: murmur-hook-debug
    version: "{{ v.murmur_hook_debug }}"
    runtime: hook

observability:
  otel_endpoint: "http://localhost:4318"
```

Hook behavior:

| Behavior | Details |
|---|---|
| Model visibility | Hook artifacts are not included in the tool inventory or `MURMUR.md` installed-tool list. |
| Invocation order | Multiple hooks are invoked in manifest declaration order for each event. |
| Failure handling | A hook handler that returns `Err(string)` does not abort the agent loop. The error is appended to `workdir/logs/hook-<name>.log`. For an `execution_mode: async` hook it is also recorded as a `hook_dispatch_error` event in `trace.jsonl`, since the log is otherwise the only place it appears — a blocking hook's error is additionally surfaced to the agent loop, which is fatal only for compaction. |
| Workdir access | Hook components receive the session workdir as their WASI `.` preopen. |
| Reference hook | `murmur-hook-debug` writes one JSON object per event to `workdir/hook-debug.jsonl`. |
| Call deadline | Each hook lifecycle call gets its own wall-clock budget: `capabilities.limits.deadline_seconds` when the manifest sets it, otherwise 30 seconds — well below the capsule-wide 600-second default, so one wedged hook cannot stall a session for most of ten minutes per event. |

The runtime generates one UUID v7 session id at startup, used uniformly for the workdir folder name, `trace.jsonl`, `MURMUR_SESSION_ID`, and hook context.

### Hook contract fields { #hook-contract-fields }

These three fields live in the **hook artifact's own** `murmur.yaml`, not in the capsule
manifest that installs it — they are the hook author's declaration of what the hook does, so a
capsule operator cannot change them by editing their own manifest.

| Field | Values | Default | Meaning |
|---|---|---|---|
| `binding` | `on-stage`, `on-session-start`, `on-task-start`, `on-inference`, `on-tool-call`, `on-shell`, `on-compaction`, `on-task-end`, `on-session-end` | absent — the hook receives every session event | Which lifecycle event(s) the hook is dispatched for. |
| `execution_mode` | `blocking`, `async` | `blocking` | Whether the runtime waits for the hook's result. An `async` hook's output is never committed, so it requires `commit_policy: none`. |
| `commit_policy` | `none`, `write-manifests`, `replace-context`, `reopen-task` | `none` | What the runtime does with the hook's successful output. |

**`binding` is the single source of truth for what a hook can commit**, and `commit_policy` is
checked against it when the capsule is staged. Each binding honors exactly one output, so it
admits exactly one `commit_policy` (plus `none`, which is always valid and means the hook only
observes):

| `binding` | Valid `commit_policy` |
|---|---|
| `on-stage` | `write-manifests` or `none` |
| `on-compaction` | `replace-context` or `none` |
| `on-task-end` | `reopen-task` or `none` |
| `on-inference` | `none` only — `on-inference` commits an `artifact` output, which has no `commit_policy` spelling |
| `on-session-start`, `on-task-start`, `on-tool-call`, `on-shell`, `on-session-end` | `none` only — these events commit nothing |
| absent (all events) | any value — the hook receives every event, including all four that commit something |

Declaring a `commit_policy` the `binding` cannot honor is an error at capsule-staging time,
before the hook component is compiled or run — for example `binding: on-task-end` with
`commit_policy: replace-context` fails with:

```
hook my-hook@1.0.0 invalid config: commit_policy 'replace-context' is not valid for binding
'on-task-end'; binding 'on-task-end' honors commit_policy 'reopen-task'
```

See [What each handler can commit](wit-interfaces.md#what-each-handler-can-commit) for the
runtime side of the same table.

### Async hook execution { #hook-overflow }

An `execution_mode: async` hook (declared in the hook artifact's own `murmur.yaml` — see
[Hook model](../concepts/capsule-runtime.md#hook-model)) is instantiated once for the session
and reused for every event: state the hook keeps in memory (a running counter, a buffered
span, an open client) survives across calls. Each async hook has its own bounded, ordered job
queue and a dedicated worker that drains it one call at a time — dispatching an event to an
async hook returns immediately, and calls to that hook are never reordered or run concurrently
with each other. Every async hook's queue is drained, and its in-flight call awaited, before
the session ends, so a queued `on-session-end` call (a final metrics export, for example) is
not lost when the session tears down. A hook that is still working when the drain's bounded
budget runs out is abandoned and reported the same way any other hook fault is.

`on_overflow:` on the capsule's own `artifacts:` entry for a `runtime: hook` artifact controls
what happens when that hook's queue is full — which only happens when the hook is falling
behind the rate of lifecycle events:

| Value | Behavior |
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

An async hook's output — even an arm that would be honored for a blocking hook on the same
binding — is always discarded: nothing waits for its answer, so there is nowhere to apply it.
`execution_mode: async` is only valid with `commit_policy: none` for this reason.

### OTel span emission

When `observability.otel_endpoint` is set, the runtime runs two independent emission paths at `session_end`:

1. **Native `OtelEmitter`** (host process) — collects lifecycle spans in memory and POSTs them as a single OTLP/HTTP JSON batch to `<otel_endpoint>/v1/traces`. This path is always present; no artifact is required.

2. **Hook-side emission via `MURMUR_OTEL_ENDPOINT`** — the runtime injects the endpoint as a WASI environment variable into every hook component. `murmur-hook-grafana` (and any hook that reads `MURMUR_OTEL_ENDPOINT`) uses this to export its own enriched span tree.

The two paths are completely independent: an error in one cannot suppress or corrupt the other. Hook OTLP failures are logged to `workdir/logs/hook-<name>.log` and are non-fatal.

**`murmur-hook-grafana`** is the first-party hook artifact targeting Grafana Tempo:

```yaml
artifacts:
  - name: murmur-hook-grafana
    version: "{{ v.murmur_hook_grafana }}"
    runtime: hook

observability:
  otel_endpoint: "http://localhost:4318"
```

When `MURMUR_OTEL_ENDPOINT` is absent or empty, `murmur-hook-grafana` logs a warning to `logs/hook-murmur-hook-grafana.log` and becomes a no-op for that session — it does not crash.

**`murmur-hook-eval`** is the first-party hook that scores sessions and writes `eval.jsonl`:

```yaml
artifacts:
  - name: murmur-hook-eval
    version: "{{ v.murmur_hook_eval }}"
    runtime: hook

observability:
  eval:
    dataset_id: my-dataset
    scorers:
      - type: exit_ok
        name: success_check
      - type: max_turns
        name: turn_limit
        max: 5
```

| Scorer type | Passes when | Extra fields |
|---|---|---|
| `exit_ok` | `exit_status == "ok"` | — |
| `max_turns` | `total_turns <= max` | `max: <integer>` |
| `max_tokens` | `total_input_tokens + total_output_tokens <= max` | `max: <integer>` |
| `tool_sequence` | `expected` is a subsequence of observed tool calls | `expected: [tool1, tool2, ...]` |
| `llm_judge` | *not implemented* — logs warning, emits no score | — |

When `MURMUR_EVAL_CONFIG` is absent (i.e. no `observability.eval` in the manifest), the hook logs a warning to `logs/hook-murmur-hook-eval.log` and becomes a no-op. The session proceeds normally.

See [Structured evaluation (`eval.jsonl`)](../reference/cli.md#structured-evaluation-evaljsonl) for the output file format and [mur eval](../reference/cli.md#mur-eval) for how to read and compare eval files.

**Span schema** — how `trace.jsonl` events map to OTel span names and attributes:

| Span name | Source event | Key attributes |
|---|---|---|
| `capsule.session` | `session_start` / `session_end` | `service.name` (capsule name), `service.version`, `model`, `exit_status`, `murmur.session_id` |
| `capsule.inference` | `inference` | `turn`, `input_tokens`, `output_tokens`, `decision`, `tool_name` |
| `capsule.tool_call` | `tool_call` | `tool_name`, `input_bytes`, `output_bytes`, `duration_ms`, `status` |
| `capsule.shell` | `shell` | `command` (first 200 chars), `exit_code`, `duration_ms` |
| `capsule.compaction` | `compaction` | `tokens_before`, `tokens_after` |

**Non-obvious behaviour:**

- Export is **batched at session end** — the root span's duration is known before any POST fires. There are no partial traces.
- OTel emission is **non-blocking from the agent loop**. The single synchronous TCP call happens after `run_agent_loop` returns, so a slow or unreachable endpoint never stalls inference.
- `trace.jsonl` is **always written** regardless of `otel_endpoint` — it is not conditional on a reachable endpoint.
- `service.version` is the manifest `version` for sessions launched with the current runtime.
- The `MURMUR_FORMATION_ID` host environment variable, when set, is forwarded into every hook's WASI env and added as `murmur.formation_id` to the root span by `murmur-hook-grafana`.

### Hook WASI environment variables

The runtime injects the following env vars into every hook component's WASI context:

| Env var | Injected when | Value |
|---|---|---|
| `MURMUR_OTEL_ENDPOINT` | `observability.otel_endpoint` is set | The configured OTLP endpoint URL |
| `MURMUR_FORMATION_ID` | `MURMUR_FORMATION_ID` is set on the host | Forwarded from host env |
| `MURMUR_EVAL_CONFIG` | `observability.eval.scorers` is non-empty | JSON-serialized `EvalConfig`; consumed by `murmur-hook-eval` |
| `MURMUR_CASE_ID` | `mur eval run` is driving a dataset case | The `case_id` field from the dataset line |
| `MURMUR_DATASET_ID` | `observability.eval.dataset_id` is set, or `mur eval run` is active | The configured or overridden dataset ID |

Custom hooks can read any of these variables from their WASI environment at `on_session_start`.

### Inference config and the driver

When `inference` is configured with `transport: http`, the runtime passes the following values to the WASM driver component via WASI environment variables:

| Env var | Source field | `transport: process` |
|---|---|---|
| `MURMUR_INFERENCE_TRANSPORT` | `inference.transport` | `"process"` |
| `MURMUR_INFERENCE_ENDPOINT` | `inference.endpoint` | `""` (empty) |
| `MURMUR_INFERENCE_MODEL` | `inference.model` | set |
| `MURMUR_INFERENCE_DRIVER` | `inference.driver.artifact` | `""` (empty) |
| `MURMUR_INFERENCE_DRIVER_CONFIG` | `inference.driver.config` as JSON | not set |
| `MURMUR_INFERENCE_API_KEY` | `inference.api_key` (only when set) | not set |

For `transport: process`, no WASM driver component is loaded. The env vars above are still injected into any hook components, but `MURMUR_INFERENCE_DRIVER` and `MURMUR_INFERENCE_ENDPOINT` will be empty strings.

---

## Capability validation { #capability-validation }

### Network allow entries

Accepted forms:

- full URL: `https://api.example.com`
- host only: `api.example.com`
- host + port: `localhost:11434`

Rejected examples:

- URL with path/query/fragment (for example `https://api.example.com/v1`)
- invalid URL/scheme

### Filesystem scope

- must be relative (not absolute)
- cannot escape with leading `..`

### Shell allow

- `capabilities.shell` present with an empty `allow` list is rejected at parse time
- Each entry is a bare binary name (e.g. `bash`, `jq`) — paths are not supported
- A synthetic tool manifest is written to `workdir/tools/<binary>/murmur.yaml` at session start for each listed binary; the agent discovers these alongside artifact-backed tools

### Execution limits

`capabilities.limits` bounds how long a component (capsule, tool/driver, or hook) may run and
how much memory it may consume — see [Execution limits](../concepts/capsule-runtime.md#execution-limits)
for the runtime-level behavior. Every field is optional and independently defaulted; omitting
the whole block is equivalent to omitting every field.

```yaml
capabilities:
  limits:
    memory_bytes: 16777216   # 16 MiB
    table_elements: 10000
    instances: 100
    deadline_seconds: 30
```

- Setting any field to `0` is rejected at manifest-parse time with `E-MAN-003`, before any WASM
  executes:

  ```
  error[E-MAN-003]: murmur.yaml: invalid capability config for 'capabilities.limits.memory_bytes': must be greater than zero
  ```

- A component that exceeds `deadline_seconds` fails with an `E-RUN-001` naming the deadline that
  fired; one that grows past `memory_bytes` or `table_elements` fails with an `E-RUN-001` naming
  the limit and the size it tried to reach. Both are reported distinctly from a plain crash —
  see [CLI error codes](cli.md#error-codes).

### Host resource limits

`capabilities.resources` bounds the **operating-system processes** the runtime spawns for
`capabilities.shell.allow` binaries and for native-implementation tool artifacts. It is a
different subject from `capabilities.limits` above, which bounds a WASM *component* inside the
runtime: a capsule that cannot escape its containment can still wedge its host by forking,
allocating, opening files or writing without bound. That is denial of service, not a containment
escape — nothing outside the capsule's granted scope is read, written, or reached — but the host
is wedged either way.

Every field is optional and independently defaulted; omitting the whole block is equivalent to
omitting every field. **A silent manifest means defaults, never "unlimited"** — the same rule
`capabilities.limits` already follows.

```yaml
capabilities:
  shell:
    allow: [bash]
  resources:
    max_processes: 32
    max_open_files: 64
    cpu_seconds: 60
    cgroup_pids_max: 64
    workdir_max_bytes: 1073741824   # 1 GiB
```

Three mechanisms enforce these, in descending order of portability:

- **`setrlimit(2)` ceilings**, applied to every spawned subprocess before `execve` on every
  platform. They are set as **hard** limits (`rlim_cur == rlim_max`), not soft ones: an
  unprivileged process may raise its own soft limit up to the hard one at any time, so a
  soft-only cap is advisory against a hostile capsule. From inside a capsule, `ulimit -Hn`
  reports the configured ceiling and any attempt to raise past it is refused by the kernel. A
  value above the runtime's own inherited hard limit is clamped down to it rather than rejected.
  `RLIMIT_CORE` is additionally pinned to `0` with no manifest surface at all.

    `max_processes` is the one field applied as **headroom rather than an absolute number**, and
    the reason is `RLIMIT_NPROC`'s own semantics: it counts everything already owned by the *uid*,
    not the entries in the capsule's tree — and the unit it counts differs by platform. On Linux
    it is **threads**: `setrlimit(2)` describes the limit as "the maximum number of processes (or,
    more precisely on Linux, threads) that can be created for the real user ID". On macOS, whose
    BSD-derived limit is genuinely per-process, it is **processes**. A workstation account is
    routinely several hundred processes and several *thousand* threads deep before a capsule
    starts, so a literal hard ceiling of 128 would not bound the capsule at 128 — it would make
    the subprocess's very first `fork()` fail with `EAGAIN` before the capsule did anything. The
    runtime therefore measures the uid's live count once at launch, in that platform's own unit
    (threads on Linux, processes on macOS), and sets `RLIMIT_NPROC = baseline + max_processes`.
    The Linux cgroup `pids.max` needs no such adjustment, because it counts only the tasks in the
    capsule's own scope — which is exactly why it, and not this field, is the bound that actually
    stops a fork bomb.
- **A cgroup v2 scope** around the whole subprocess tree (`cgroup_*` fields), **Linux only**.
  This is what rlimits structurally cannot do: `RLIMIT_NPROC` is a per-**uid** ceiling, so a fork
  bomb of distinct, short-lived processes evades it; `pids.max` on a cgroup does not.
- **A periodic workdir-size check** (`workdir_max_bytes`), on every platform. A breach is caught
  within one poll interval — not at the instant it happens — and terminates the session with
  `E-RUN-013`. The interval is an internal constant, not a manifest key.

Setting any field to `0` is rejected at manifest-parse time with `E-MAN-003`, before any WASM
executes:

```
error[E-MAN-003]: murmur.yaml: invalid capability config for 'capabilities.resources.max_processes': must be greater than zero
```

**Platform behavior.** On Linux, a capsule that can spawn *any* native subprocess
(`capabilities.shell.allow`, `capabilities.spawn.allow`, or a native-implementation artifact)
refuses to launch with `E-RUN-012` when the host cannot delegate a cgroup — running that tree
with no aggregate ceiling is worse than not running it. This requires systemd user delegation
(`Delegate=yes` for `memory pids cpu io` on the unit `mur` runs under); see
[Verification](verification.md) for how these bounds are checked by hand. A capsule that
declares no subprocess capability at all is never blocked — there is no process tree to bound.
On macOS (and any non-Linux host) cgroups cannot exist, so the launch always proceeds with
rlimits only and `W-SEC-010` documents the residual gap: no aggregate bound, and — because macOS
has no `RLIMIT_AS` and does not enforce `RLIMIT_DATA` — no per-process memory bound either.

When a subprocess is killed for exceeding a limit and the kernel's own evidence names exactly
one, the `shell` event in `trace.jsonl` carries a `resource_limit` field naming it
(`cpu_seconds`, `max_file_size_bytes`, `cgroup_memory_bytes`, `cgroup_pids_max`). Cases the
kernel does not identify unambiguously — `RLIMIT_AS`/`RLIMIT_DATA` (surfaces as `ENOMEM` inside
the child's allocator), `RLIMIT_NPROC` and `RLIMIT_NOFILE` (fail a `fork()`/`open()` with
`EAGAIN`/`EMFILE` inside the child, killing nothing) — are deliberately left unattributed rather
than guessed at.

## Threat model: prompt injection and network-bypass posture { #threat-model }

Two findings from `murmur-security-assessment.md` shape how you should author manifests, not just
what the runtime enforces. C-4 has no structural fix and remains open on every platform; C-7 is
closed on Linux with kernel enforcement available, and permanently open elsewhere.

- **C-4 — prompt injection.** No manifest setting or runtime mechanism fully prevents a model
  from being manipulated by instructions smuggled inside data it reads (a fetched web page, a
  tool result, output from a binary declared under `capabilities.shell.allow`). There is no
  complete structural fix for this — the runtime's mitigation is limited to always prepending an
  untrusted-content notice to the system prompt (both `transport: http` and `transport: process`)
  instructing the model to treat tool results, shell output, and fetched content as data, never as
  instructions. Manifest authors are still responsible for not combining broad tool authority with
  exposure to untrusted content (see the phase-separation pattern below).
- **C-7 — network allowlist bypass via subprocess.** `capabilities.network.allow` constrains
  requests the *runtime itself* makes (WASI HTTP calls from tool/driver components); by itself it
  does **not** constrain a subprocess spawned via `capabilities.shell.allow`. Closing that gap
  needs kernel-level subprocess enforcement, which only exists on Linux. The runtime warns at
  capsule launch whenever this gap is live on the current host — see
  [Security Warnings](security-warnings.md) for the enforcement-tier breakdown and each warning
  code (`W-SEC-001` through `W-SEC-003`).

**Maximum-injection-risk combination:** `capabilities.shell.allow` containing `bash` *combined
with* any external-fetch capability — either a declared `capabilities.network.allow`, or a
tool/driver artifact that performs its own outbound fetches independent of `network.allow`. This
pairing gives a capsule both the ability to ingest attacker-influenced content from the network
and the shell authority to act on it unchecked, which is exactly the shape C-4 and C-7 describe
together: injected instructions with a bypassable enforcement boundary underneath them.

### Recommended pattern: data/action phase separation

For any capsule that ingests untrusted external content (fetched web pages, third-party file
contents, output from a tool the operator didn't author), split the capsule's work into two
phases instead of giving one phase both fetch and shell/write authority at once:

1. **Data-gathering phase** — only fetch/read tools are active (network calls, file reads). No
   `bash`/shell/write capability is available during this phase.
2. **Action phase** — the fetched content is summarized or extracted into a bounded structure
   (a short JSON object, a fixed set of fields) *before* it is handed to a phase that has
   bash/shell/write authority. The action phase never receives the raw untrusted bytes directly —
   only the bounded, already-extracted structure.

The point of the split is that raw untrusted content and tool-calling/shell authority should never
be present in the same context window at the same time. Summarizing/extracting first means an
injected instruction inside the fetched content has nothing to act through by the time a
shell-capable phase is reading it.

---

## Lockfile (`murmur.lock`)

```yaml
lock_version: 1
artifacts:
  - name: some-tool
    resolved_version: "1.2.3"
    sha256:
      wasm: "<sha256>"
```

Used for pinned version + integrity checks. Three code paths read and write this file, all
sharing the same "verify against the pin, then upsert" rule — a registry-resolved artifact
whose hash disagrees with an existing entry is always rejected (non-zero exit / error, nothing
written to disk or to the lock), never silently overwritten:

| Path | When it writes |
|---|---|
| `mur run` | Only creates `murmur.lock` if it doesn't exist yet; once present, `mur run` only verifies against it — it never refreshes an existing entry. |
| `mur install <name>@<version>` (registry-resolved, project-scoped) | Upserts this artifact's entry on every successful install, preserving all other entries. Skipped entirely for `-g`/global installs (no project directory) and for local-file/`--all-platforms` installs (no independent registry hash to pin). |
| `manage.pull()` (in-capsule runtime call) | Same verify-then-upsert behavior as `mur install`, invoked by a running capsule's guest code instead of the CLI. |

---

## Lifecycle { #lifecycle }

The `lifecycle:` block controls how long a capsule runs and how many tasks it accepts. Omitting it is equivalent to:

```yaml
lifecycle:
  task_acceptance: single
  after_task: exit
  queue_depth: 1
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
| `exit` (default) | The capsule exits immediately after the task finishes (or the idle timeout fires). |
| `sleep` | The capsule loops back to wait for the next task. Only useful with `task_acceptance: queue`; with `single` it behaves like `exit`. |

### `lifecycle.input_timeout_secs` { #lifecycle-input-timeout-secs }

Controls how long the capsule waits for a `message/send` reply after a WASM tool component
calls `murmur:task/task#request-input`.

| Value | Behaviour |
|---|---|
| absent (default) | Wait indefinitely — the task stays in `input-required` state until a reply arrives or the process is killed. |
| `N` (positive integer) | If no `message/send` arrives within `N` seconds, the task transitions to `failed` with status message `"input-timeout"`. SSE clients receive a final `TaskStatusUpdateEvent` with `"final":true`. |

Example: require a reply within 5 minutes:

```yaml
lifecycle:
  task_acceptance: single
  input_timeout_secs: 300
```

See [request-input WIT import](../reference/wit-interfaces.md#murmurtasktask) and
[Pause the agent loop for human input](../how-to/hitl-request-input.md).

### `lifecycle.conversation` { #lifecycle-conversation }

Controls whether tasks that share a `contextId` accumulate conversation history within a session.

| Value | Behaviour |
|---|---|
| `stateless` (default) | Every task starts with an empty message history. The `contextId` field in the incoming message is recorded but has no effect on context. |
| `threaded` | When a task arrives with a `contextId` that has prior completed tasks in this session, the agent loads the full conversation history for that thread before running. History is persisted to `workdir/contexts/<contextId>/history.json` after every successful `end_turn` or `max_tokens` stop. A failed task does not overwrite the history, preserving the last known good state. Each completed task also writes a per-task result to `workdir/out/result_<taskId>.txt` in addition to the shared `workdir/out/result.txt`. |

Threaded mode requires a long-running capsule (`task_acceptance: queue`, `after_task: sleep`). It is not useful with `task_acceptance: single` because the capsule exits after the first task and cannot receive a follow-up.

Example:

```yaml
lifecycle:
  task_acceptance: queue
  after_task: sleep
  queue_depth: 2
  conversation: threaded
```

### Idle timeout

When waiting for the next A2A message (`task_acceptance: single` or `queue`), the capsule waits up to **30 seconds**. If no task arrives within the window:

- `queue` mode: clean exit.
- `single` mode: backward-compatible fallback — runs the agent loop with an empty task.

The timeout is configurable via the `MURMUR_A2A_TIMEOUT_SECS` environment variable (used in tests to keep wait times short).

### CLI overrides

The `mur run` command accepts two flags to override the manifest's lifecycle without editing the file:

```bash
mur run --manifest murmur.yaml \
  --lifecycle-task-acceptance queue \
  --lifecycle-after-task sleep
```

Valid values follow the same `snake_case` names as the manifest fields.
