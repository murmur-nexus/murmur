# Configuration

## Configuration files

`mur` reads up to two YAML files and resolves them into one *effective* config:

| File | Scope | Discovery |
|---|---|---|
| `~/.murmur/config.yaml` | Global (per-user) | Fixed path |
| `<cwd>/.murmur/config.yaml` | Project (per-workspace) | `<cwd>` only; parent directories are not searched |

Both files are optional; a missing file is treated as empty. Write them with
[`mur config set`](cli.md#mur-config) (project by default, `-g` for global) or edit the YAML by
hand.

### Merge rules

Where both files set a value, the effective config is built per field:

| Field | Rule |
|---|---|
| `registry.index_url` | Project wins if non-empty, else global |
| `inference.provider`, `inference.model`, `inference.endpoint` | Project wins if non-empty, else global |
| `inference.api_key` | Always the global value — see [`inference.api_key` is always global](#inferenceapi_key-is-always-global) |
| `credentials` | Always the global map. A non-empty `credentials:` in the project file is ignored, with a warning naming that file — see [`credentials:` section](#credentials) |
| `registry.sources` | Union by scope: a project entry for a scope the global file also serves replaces the global entry; every other entry of both files is kept |
| `beta.enabled` | Union by value: global flags first, then any project-only flags appended, in the project file's order |
| `containment` | **Strongest wins**: a project file may raise the class the global file asked for, never lower it. See [Containment class](containment.md#field-containment) |
| `spend.machine_tokens_per_day` | **Lowest wins**: a project file may lower the ceiling, never raise it. See [`spend:` section](#spend) |

The base of the merge is the global file, or an empty config when `~/.murmur/config.yaml` is
absent. The source of the `murmur` scope, `murmur-nexus/default-artifacts`, is built in and is
not a `registry.sources` entry, so it is present with or without either file.

`mur install` and `mur deploy` check each file's `registry:` section on its own before merging, and
refuse one they cannot use with [`E-CFG-003`](diagnostics.md#e-cfg-003), naming that file.

### `inference:` section

Which inference provider `mur` uses, and the credentials and endpoint to reach it with. Both
forms of [`mur new`](cli.md#mur-new) read this block from `~/.murmur/config.yaml` directly rather
than from the effective config, and neither writes it:

| Command | Reads | An empty or absent field |
|---|---|---|
| [`mur new --roster`](cli.md#mur-new-roster-provider) | `provider`, `model` and `endpoint`. Never `api_key`: the generated members reference the key by name | Takes the provider's default. With no block the provider is `anthropic` |
| [`mur new "<task description>"`](cli.md#mur-new-task) (beta) | All four keys | `provider` must be `anthropic` or `openai`, and `model` and `api_key` non-empty, or the whole block is skipped: `mur new` falls back to the `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` environment variables, then the interactive wizard |

```yaml
inference:
  provider: anthropic              # "anthropic" or "openai"
  model: claude-haiku-4-5-20251001
  api_key: sk-ant-...
  endpoint: ""                     # optional; leave empty for the provider default
```

| Key | Description |
|---|---|
| `provider` | `anthropic` or `openai` |
| `model` | Model name to request from the provider |
| `api_key` | API key for the provider |
| `endpoint` | Base URL of the provider's API, for a proxy or a compatible service. Empty selects the provider default |

An empty `endpoint` resolves per provider:

| Provider | Default endpoint |
|---|---|
| `anthropic` | `https://api.anthropic.com` |
| `openai` | `https://api.openai.com/v1` |

#### `inference.api_key` is always global

`inference.api_key` is the one field that does not follow "project wins": the effective config
reads it from `~/.murmur/config.yaml` only, whatever the project file contains, and whether that
value is a literal or a `${VAR}` reference. If the global file has no `inference:` block at all,
the effective `api_key` is `""`.

A **literal** `inference.api_key` in the project file triggers a warning — both when the
effective config is loaded and when [`mur config set`](cli.md#mur-config-set-key-value) writes it:

```text
warning: <cwd>/.murmur/config.yaml sets inference.api_key to a literal value, but inference.api_key is always read from the global config (~/.murmur/config.yaml); this project-level value will be ignored
```

A `${VAR}` reference prints no warning. The variable name must be uppercase letters, digits and
underscores, starting with a letter or underscore — `${MY_ORG_KEY}` is a reference, `${my_key}`
is a literal and warns.

`mur run` does not read this `inference.api_key`. It reads provider keys from
[`credentials:`](#credentials), and `mur config set inference.api_key` prints a one-line note saying
so.

### `credentials:` section { #credentials }

Provider and third-party API keys, by credential name. A manifest artifact entry's
[`gateway.api_key: ${NAME}`](manifest.md#gateway-api-key) names a credential, and `mur run` reads
its value from `credentials.NAME` here.

```yaml
credentials:
  ANTHROPIC_API_KEY: sk-ant-...
  OPENAI_API_KEY: sk-...
```

| Key | Type | Description |
|---|---|---|
| `<NAME>` | string | The key for credential `NAME`. `NAME` is uppercase letters, digits and underscores, starting with a letter or underscore. An empty value counts as absent |

Write an entry with `mur config set -g`. Only the global file holds credentials, so the command
refuses without `-g`, and it prints the key's name, never its value:

```bash
mur config set -g credentials.ANTHROPIC_API_KEY sk-ant-...
# Set credentials.ANTHROPIC_API_KEY in ~/.murmur/config.yaml
```

#### Where `${NAME}` is read from { #credentials-precedence }

| Situation | Key used | Re-read while the capsule runs |
|---|---|---|
| `credentials.NAME` is a non-empty entry in `~/.murmur/config.yaml` | That entry, even when the environment variable `NAME` is also set | Yes |
| No entry, and the environment variable `NAME` is set | The variable, with [`W-SEC-027`](diagnostics.md#w-sec-027) | No |
| Neither | None: `mur run` refuses before any session directory exists, with `E-MAN-003` naming the artifact and both places | — |
| `gateway.api_key` is a literal, not `${NAME}` | The literal, with [`W-SEC-027`](diagnostics.md#w-sec-027) | No |
| No `gateway.api_key` | None | — |

#### Rotating a key { #credentials-rotation }

A replaced entry takes effect on the next request any running capsule sends with that key — an
inference request, or a tool's or hook's call through its gateway. Nothing restarts. Before each
request that carries the key, the runtime reads the entry from the config
file, however the file was last written.

| Change | When a running capsule uses it |
|---|---|
| `mur config set -g credentials.NAME <key>`, or any hand edit, in place or by rename | The next request that reads the file after the write completes. This is the worst case for every way of writing the file |
| A request that reads the file while an editor is part-way through saving it | That request uses the last key read, and the trace records `unreadable`. The next request uses the saved key |
| Removing the entry, or the file becoming missing or unparseable | Never. The capsule keeps the last key it read, and the trace records `change: "unreadable"` once for that state of the file |

Removing an entry does not revoke a running capsule's key; replacing it does.

When an upstream answers `401`, the runtime reads the file again before deciding whether to
resend. A changed value is sent in one resend of the same request, and that response goes to the
artifact whatever its status. An unchanged value is not resent. A `401` that stands on the
driver's upstream ends the task with [`E-RUN-027`](diagnostics.md#e-run-027); on any other
artifact's upstream it goes back to that artifact as the response. Every rotation and rejection is
recorded in the session trace, never with the key:

| Credential | Event |
|---|---|
| The configured driver's | [`inference_credential`](observability-schemas.md#inference-credential) |
| Any other artifact's | [`gateway_credential`](observability-schemas.md#gateway-credential) |

A capsule started by another capsule's `delegate-task` runs with the same `HOME`, so it reads the
same file and picks up a rotated key too.

#### File permissions { #credentials-permissions }

Every write to the global file leaves `~/.murmur/config.yaml` mode `0600` inside a `~/.murmur` held
at `0700`, whatever the umask and whatever modes the two had before. A write that cannot set either
mode fails with `E-IO-003`. The project-level `<cwd>/.murmur/config.yaml` is written under the
umask; it cannot hold credentials.

| Writes the global file | Command |
|---|---|
| Keys and credentials | `mur config set -g` |
| `beta.enabled` | `mur beta enable`, `mur beta disable` |
| `inference:` | The `mur new` first-run setup |

A file loosened after it was written stays loose until the next write, and is reported:

| Reports it | When | Output |
|---|---|---|
| `mur run` | A capsule reads a `gateway.api_key` from `credentials.<NAME>` in a file that grants any group or other permission | [`W-SEC-028`](diagnostics.md#w-sec-028) on stderr, once per launch |
| `mur doctor` | Always | The mode of every entry in `~/.murmur`, and `W-SEC-028` for each one wider than the table below — see [`mur doctor`](cli.md#doctor-murmur-home) |

Neither changes a mode or refuses. Tighten the file by hand:

```bash
chmod 600 ~/.murmur/config.yaml
```

#### `~/.murmur` modes { #murmur-home-permissions }

| Path | Mode | Holds | Written by |
|---|---|---|---|
| `~/.murmur/` | `0700` | Everything below | Held by every writer below before it creates anything |
| `config.yaml` | `0600` | Provider credentials, registry tokens | `mur config set -g`, `mur beta`, `mur new` |
| `state/`, `state/<store>/` | `0700` | Capsule state stores. Files inside are written by the capsule with no mode set | `mur run` at staging |
| `spend/`, `spend/<YYYY-MM-DD>.jsonl` | `0700`, `0600` | The machine spend ledger | `mur run` with [`spend.machine_tokens_per_day`](#spend) in effect |
| `conversations/` and each directory below, `conversation.jsonl` | `0700`, `0600` | Conversation records | `mur run`, on the first recorded message; `mur conversation` rewrites |
| `running/`, `running/<session_id>.json` | `0700`, `0600` | Running-capsule records | `mur run` when a session opens its door |
| `deployments.json` | `0600` | Deployment records | `mur deploy`, `mur destroy` |
| `deploy_staging/`, `deploy_staging/<deployment_id>/` | `0700` | A copy of the manifest, workdir and `mur` binary while a deploy uploads | `mur deploy` |
| `deploy_keys/` | Expected `0700`, files `0600` | SSH private keys for a deployment | Nothing writes here; `mur destroy` removes a deployment's directory |
| `artifacts/` | Umask | Installed artifacts | `mur install`, `mur publish` |
| `compiled/`, `compiled/<sha256>-<engine>.cwasm` | `0700`, `0600` | Compiled WASM artifacts, so a later launch or `manage.pull()` of the same artifact version skips compiling. An entry not read in 30 days is deleted by the next `mur install`, `mur precompile` or `mur run` that compiles a component; deleting the directory is safe | `mur install`, `mur precompile`, `mur run` at staging, and when a capsule pulls a WASM tool (`manage.pull()`, `pull-artifact`) |
| `host_facts/`, `host_facts/<sha256>.json` | `0700`, `0600` | One entry per [`capabilities.shell.allow` binary](containment.md#host-facts), named by the sha256 of its resolved path: whether it is a recognised interpreter, what it reported and the libraries its extension modules need, so a later launch skips running the interpreter. Entries are never pruned; deleting the directory is safe | `mur run` at staging and `mur run --explain-scope`, for a binary whose entry is missing or no longer matches |
| `bin/mur-*` | `0755` | Cached `mur` binaries for deploy targets | `mur deploy` |
| `formations/`, `formations/<frm_id>/`, `formations/<frm_id>/<member>/` | `0700` | Each formation peer's [working directory](roster.md#member-directories). What the member's sessions write inside its directory keeps the umask's modes; the owner-only member directory is what keeps it from other accounts. A formation's directory is kept until the formation has ended and the roster entry member's `trace.retain` [expires it](roster.md#formation-retention); with no `trace.retain`, it is kept forever | `mur run --roster` |
| `sessions/`, `sessions/<session_id>/` and the `runtime/`, `sealed-etc/` and `harness/` in it | `0700` | Each running session's [runtime directory](workdir.md#runtime-directory): its `MURMUR.md`, task, manifest copy, installed tools and plans. Files inside keep the umask's modes. Removed when the session ends; one a session left behind is removed by the next launch once its lock is free and it is more than 60 seconds old. A `HOME` that cannot hold `~/.murmur` refuses the launch with [`E-IO-004`](diagnostics.md#e-io-004) | `mur run` at staging |
| `records/`, every directory of each mirrored workdir root and each `<session_id>/` in it | `0700` | Every session's [records directory](workdir.md#records-directory): its `trace.jsonl`, `blobs/`, `logs/`, `completion.json` and `hooks/`. Files inside keep the umask's modes. Kept after the session ends; removed by [`trace.retain`](manifest.md#trace-retain) | `mur run` at staging |
| `formations/<frm_id>/formation.json` | `0600` | The formation's [ownership marker](roster.md#member-directories): its project directory and its launcher. A formation directory without one is never removed | `mur run --roster`, once, before the first peer starts |

`mur run` uses a file in `compiled/` only when all of these hold, and otherwise compiles the
artifact again and replaces the file:

- `compiled/` is a real directory, not a symlink, and it and the file are yours and owner-only.
- The file matches the checksum stored beside it. `mur` checks this when it writes the file, and
  again whenever the file has been changed, replaced or had its permissions changed since.
- `~/.murmur` is outside the capsule's workdir. When it is inside, `mur run` neither reads nor
  writes `compiled/`, and every launch and pull compiles.

Each value of the [`MURMUR_MAX_ARTIFACT_DECOMPRESSED_BYTES`](../concepts/registry.md#artifact-integrity)
ceiling keeps its own compiled artifacts, so the first launch after you change it compiles every
artifact again, unless `mur install` or `mur precompile` has run under the new value first.
[`mur deploy run`](cli.md#deploy-precompile) compiles on the target under the target's value, the
one the capsule runs under.

`mur run` uses an entry in `host_facts/` only when all of these hold, and otherwise works the
binary's facts out again, runs the interpreter and replaces the entry:

- `host_facts/` is a real directory, not a symlink, and it and the entry are yours and owner-only.
  A directory found wider than `0700` is narrowed and emptied first.
- The entry was written by the same `mur` version, with the same interpreter table.
- The binary, its directory, the directory above it, and every reported path, extension module and
  library the entry records are unchanged since the entry was written: same file, size, owner,
  mode and change time.
- `~/.murmur` is outside every tree the capsule's commands can write. When it is inside,
  `mur run` neither reads nor writes `host_facts/`.

Which capsules can write to `compiled/` and `host_facts/`, and so choose what a later launch or
pull loads or grants, is set out under [Containment class](containment.md#field-containment).

Every mode marked `0700` or `0600` is set again on each write, not only when the path is created.
A `mur run` that writes under an existing `~/.murmur` removes its group and other permissions and
leaves its owner permissions as they are, so a home closed with `chmod 500` stays closed.

### `registry:` section

| Key | Description |
|---|---|
| `index_url` | Artifact index `mur search` fetches. See [Artifact index and custom registry URL](#artifact-index-and-custom-registry-url) |
| `sources` | The source of each third-party scope, one entry per scope. Default: none. See [Sources and scopes](installing-artifacts.md#sources-and-scopes) |

```yaml
registry:
  sources:
    - repo: ana/artifacts
      token: ${GITHUB_TOKEN}
```

Each `sources` entry has these keys:

| Key | Required | Notes |
|---|---:|---|
| `repo` | yes | The GitHub repository, `<owner>/<repo>`. It serves the scope `<owner>`, lowercased. An owner of `local` or `murmur` is refused, except `murmur-nexus/default-artifacts` |
| `token` | no | A GitHub token sent with the source's requests: `${VAR}`, `VAR`, or the literal token — see [Authentication](installing-artifacts.md#authentication). Default: `GITHUB_TOKEN` |

An entry for `murmur-nexus/default-artifacts` only sets the token of the built-in `murmur` source.
Two entries for one scope in one file, a `name`, `type` or `url` key on an entry, and a `default`
key under `registry:` are refused with [`E-CFG-003`](diagnostics.md#e-cfg-003).

### `beta:` section

The beta features opted into with `mur beta enable` / `mur beta disable`. Those commands read and
write the global file only, and `beta.enabled` is not a [`mur config set`](cli.md#mur-config) key
— a project-level entry can only be added by editing `<cwd>/.murmur/config.yaml` by hand.

```yaml
beta:
  enabled:
    - mur-new
    - mur-deploy
```

| Key | Type | Description |
|---|---|---|
| `enabled` | array of strings | Feature names opted into. An absent `beta:` section is equivalent to `enabled: []` |

A name this build does not compile in has no effect until a build that includes it is installed.
[`mur beta list`](cli.md#mur-beta) prints the features this build has, and `mur beta enable`
warns when the name is not one of them.

### `spend:` section { #spend }

A machine-wide ceiling on inference spend, counted in tokens per UTC day.

```yaml
spend:
  machine_tokens_per_day: 5000000
```

| Key | Type | Required | Description |
|---|---|---|---|
| `machine_tokens_per_day` | integer | no | Most tokens every run on this `~/.murmur` may spend per UTC day. No default: absent sets no machine ceiling. `0` is refused with `E-IO-003` when the config is loaded |

It charges every agent turn and every hook's `run-inference` call what
[`inference.max_session_tokens`](manifest.md#inference-max-session-tokens)
charges it: under `transport: http`, the provider's request and completion when the call reported
both, and the runtime's own counts otherwise. Under `transport: http`, before each call a run adds the day's
total, its own calls still in flight and the call's input plus its most output; a call that would
cross the ceiling is refused before it is sent. Under [`transport: process`](manifest.md#transport-process)
there is nothing to admit: each turn is charged the counts the harness reported once it closes, and
a turn that reaches the ceiling stops the run. Either way the stop carries `limit: "machine"` on its
[`spend_ceiling_reached`](observability-schemas.md#spend-ceiling-reached) line. A machine refusal
does not latch: the next call is checked again, and the total starts from zero at 00:00 UTC.

**The ceiling is approximate.** Each run appends a call to the shared ledger when the call settles,
and checks the ledger before its next call. A call that is admitted and not yet settled is
invisible to every other run. The machine total can therefore exceed
`spend.machine_tokens_per_day` by at most the tokens of the calls in flight on the machine
(admitted and not yet settled) when the last call was admitted, however much output each asked for.
A `transport: process` run adds one more turn's worth: its spend is known only once the turn is
over, so the turn that crosses the ceiling has already been paid for when the run stops.

Calls that can be in flight at the same time:

- one agent turn per running session
- one agent turn per [delegated child](roost-api.md#the-delegation-tool), since each child is a
  session of its own and keeps running after `delegate-task` returns, until it finishes or the
  task that delegated to it [ends it](roost-api.md#how-the-outcome-arrives)
- one `run-inference` call per [`execution_mode: async`](manifest.md#hook-contract-fields) hook,
  which runs alongside the agent turn

The bound holds only while the ledger can be read and appended to. A run that cannot append to it
prints a warning, and those calls are not counted.

| Covered | Not covered |
|---|---|
| Runs whose `HOME` shares this `~/.murmur` — in practice, one user account | Other user accounts on the same host |
| Sessions started by `mur run`, including delegated children, with the ceiling in effect | Sessions started by `mur eval run` or `mur new`, and any run launched without the ceiling in effect: they write no ledger lines and are not counted |
| `transport: http` capsules | `transport: process` capsules, whose CLI reaches its provider with its own credentials — see [`W-SEC-026`](diagnostics.md#w-sec-026) |
| A `~/.murmur` on a local filesystem | A `~/.murmur` on NFS, where the atomicity of appends the ledger relies on does not hold |

Each run enforces its own effective config's value against the shared total, so two runs launched
from projects with different ceilings each stop at their own.

**The ledger** is one file per UTC day:

| Property | Value |
|---|---|
| Path | `~/.murmur/spend/<YYYY-MM-DD>.jsonl` |
| Modes | Directory `0700`, file `0600` |
| Line | `{"ts":<unix ms>,"session_id":"ses_…","input_tokens":<n>,"output_tokens":<n>}` |
| When written | Once per settled call. A call cancelled, refused at dispatch or failed is written with its `input_tokens` and `output_tokens: 0`, unless the failed call's provider reported both counts |

What each line's two numbers are, by the call's [`inference`](observability-schemas.md#session-trace-tracejsonl) line:

| The `inference` line | `input_tokens` on the ledger | `output_tokens` on the ledger |
|---|---|---|
| `transport: http`, carries both `input_tokens_actual` and `output_tokens_actual` | `input_tokens_actual + cached_tokens + cache_write_tokens`, an absent cache member counted as `0` | `output_tokens_actual` |
| Any other line | Its `input_tokens` | Its `output_tokens`, or `0` for a call cancelled, refused at dispatch or failed |
| Retention | A run that opens the ledger removes `<YYYY-MM-DD>.jsonl` files dated more than 30 days before today. Other names are left alone |

A run that cannot create or open the ledger refuses to start with
[`E-RUN-026`](diagnostics.md#e-run-026).

### Where the effective config is used

| Consumer | Reads |
|---|---|
| Beta gating — which beta subcommands `mur --help` lists, and `mur beta list`'s enabled column | `beta.enabled` |
| `mur install` — the source of each scope | `registry.sources`, each file checked on its own |
| `mur search` | `registry.index_url` |
| `mur run`, `mur doctor` — the containment floor | `containment` |
| `mur run` — the key for each `gateway.api_key: ${NAME}`, re-read while the capsule runs; `mur doctor` — whether to warn with `W-SEC-027` | `credentials`, from the global file only |
| `mur run` — the machine spend ceiling; `mur run` and `mur doctor` — [`W-SEC-026`](diagnostics.md#w-sec-026) | `spend.machine_tokens_per_day` |

`mur new` and `mur deploy` read `~/.murmur/config.yaml` only; a project-level file does not
affect them. `mur deploy` builds each scope's source from that file's `registry.sources`.

---

## Artifact index and custom registry URL

`mur search` fetches a static JSON catalog, the [artifact index](artifact-index.md)
`artifacts-index.json`. The default is the index on the latest default-artifacts release:

```text
https://github.com/murmur-nexus/default-artifacts/releases/latest/download/artifacts-index.json
```

To point `mur search` at a different catalog — a private org index, say — set
`registry.index_url` in `~/.murmur/config.yaml`, or in `<cwd>/.murmur/config.yaml` to scope it to
one project:

```yaml
registry:
  index_url: https://my-org.example.com/artifacts-index.json
```

`registry.index_url` applies to every `mur search` invocation that does not pass `--index`.

The index must be a schema 2 [artifact index](artifact-index.md); `mur search` refuses any other
with [`E-IDX-002`](diagnostics.md#e-idx-002).

---

## Resolution order { #resolution-order }

`mur` reads artifacts from the stores on disk and the [source of each scope](installing-artifacts.md#sources-and-scopes). A
`murmur.yaml` does not choose between them: a top-level `registry:` key in one is refused with
[`E-MAN-003`](diagnostics.md#e-man-003).

| Command | Resolves from, in order |
|---|---|
| `mur install <scope>/<name>@<version>` | 1. The global store `~/.murmur/artifacts/`<br>2. The source of `<scope>`, through its [index](installing-artifacts.md#index-resolution). A `local/` artifact is never asked of a source |
| `mur install` (no argument) | 1. The project store<br>2. The global store<br>3. The source of the scope — for each artifact `murmur.yaml` declares |
| `mur install <scope>/<name>` | The source of `<scope>`: the newest version that [fits this `mur`](installing-artifacts.md#version-selection) |
| `mur install github:<owner>/<repo>@<tag>` | That repository's release |
| `mur install <path>` | The file, installed as `local/<name>` |
| `mur publish` | Writes into the global store, as `local/<name>` |
| `mur run`, `mur doctor`, `mur eval` | 1. The project store `<project root>/.murmur/artifacts/`<br>2. The global store. Never a source |

A scope has one source, so no other source is asked when it does not list an artifact: a
`<scope>/<name>@<version>` that neither the global store nor the scope's source has fails with
[`E-REG-001`](diagnostics.md#e-reg-001), and a scope with no source fails with
[`E-REG-008`](diagnostics.md#e-reg-008).

