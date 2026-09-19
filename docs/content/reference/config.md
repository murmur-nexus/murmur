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
| `registry.default` | Project wins if non-empty, else global |
| `registry.index_url` | Project wins if non-empty, else global |
| `inference.provider`, `inference.model`, `inference.endpoint` | Project wins if non-empty, else global |
| `inference.api_key` | Always the global value — see [`inference.api_key` is always global](#inferenceapi_key-is-always-global) |
| `credentials` | Always the global map. A non-empty `credentials:` in the project file is ignored, with a warning naming that file — see [`credentials:` section](#credentials) |
| `registry.sources` | Union by `name`: a project entry sharing a global entry's name replaces it in place (position preserved); a project entry with a new name is appended; global-only entries are never dropped |
| `beta.enabled` | Union by value: global flags first, then any project-only flags appended, in the project file's order |
| `containment` | **Strongest wins**: a project file may raise the class the global file asked for, never lower it. See [Containment class](containment.md#field-containment) |
| `spend.machine_tokens_per_day` | **Lowest wins**: a project file may lower the ceiling, never raise it. See [`spend:` section](#spend) |

The base of the merge is the global file, or the built-in default when
`~/.murmur/config.yaml` is absent. That default is `registry.default: official` and a single
GitHub source, `murmur-nexus/default-artifacts`. Once the global file exists it is the base in
full: a key it omits is empty, not defaulted, so a global file that declares no
`registry.sources` leaves `mur install` with an empty source chain.

### `inference:` section

Which inference provider `mur` uses, and the credentials and endpoint to reach it with.
[`mur new`](cli.md#mur-new) reads this block from `~/.murmur/config.yaml` directly rather than
from the effective config, and uses it only when it is complete: `provider` is `anthropic` or
`openai`, and both `model` and `api_key` are non-empty. An incomplete block is skipped, and
`mur new` falls back to the `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` environment variables and
then the interactive wizard.

```yaml
inference:
  provider: anthropic              # "anthropic" or "openai"
  model: claude-haiku-4-5-20251001
  api_key: sk-ant-...
  endpoint: ""                     # optional; leave empty for the provider default
```

| Key | Required | Description |
|---|---|---|
| `provider` | yes | `anthropic` or `openai` |
| `model` | yes | Model name to request from the provider |
| `api_key` | yes | API key for the provider |
| `endpoint` | no | Base URL of the provider's API, for a proxy or a compatible service. Empty selects the provider default |

An empty `endpoint` resolves per provider:

| Provider | Default endpoint |
|---|---|
| `anthropic` | `https://api.anthropic.com` |
| `openai` | `https://api.openai.com` |

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
| `bin/mur-*` | `0755` | Cached `mur` binaries for deploy targets | `mur deploy` |

Every mode marked `0700` or `0600` is set again on each write, not only when the path is created.
A `mur run` that writes under an existing `~/.murmur` removes its group and other permissions and
leaves its owner permissions as they are, so a home closed with `chmod 500` stays closed.

### `registry:` section

| Key | Description |
|---|---|
| `default` | Name of the `sources` entry tried first when resolving an artifact by name. Built-in default: `official` |
| `index_url` | Artifact index `mur search` fetches. See [Artifact index and custom registry URL](#artifact-index-and-custom-registry-url) |
| `sources` | Sources `mur install` walks in order. See [Multiple sources and fallthrough](installing-artifacts.md#multiple-sources-and-fallthrough) |

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

It counts what [`inference.max_session_tokens`](manifest.md#inference-max-session-tokens) counts:
the runtime's own `input_tokens + output_tokens` for every driver call, agent turns and hooks'
`run-inference` calls alike. Before each call, a run adds the day's total, its own calls still in
flight and the call's input plus its most output; a call that would cross the ceiling is refused
before it is sent, with `limit: "machine"` on its
[`spend_ceiling_reached`](observability-schemas.md#spend-ceiling-reached) line. A machine refusal
does not latch: the next call is checked again, and the total starts from zero at 00:00 UTC.

**The ceiling is approximate.** Each run appends a call to the shared ledger when the call settles,
and checks the ledger before its next call. A call that is admitted and not yet settled is
invisible to every other run. The machine total can therefore exceed
`spend.machine_tokens_per_day` by at most the tokens of the calls in flight on the machine
(admitted and not yet settled) when the last call was admitted. Those tokens are the runtime's own
`input_tokens + output_tokens` for each call, however much output the call asked for.

Calls that can be in flight at the same time:

- one agent turn per running session
- one agent turn per [delegated child](roost-api.md#the-delegation-tool), since each child is a
  session of its own and keeps running after `delegate-task` returns
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
| When written | Once per settled call. A call cancelled, refused at dispatch or failed is written with its `input_tokens` and `output_tokens: 0` |
| Retention | A run that opens the ledger removes `<YYYY-MM-DD>.jsonl` files dated more than 30 days before today. Other names are left alone |

A run that cannot create or open the ledger refuses to start with
[`E-RUN-026`](diagnostics.md#e-run-026).

### Where the effective config is used

| Consumer | Reads |
|---|---|
| Beta gating — which beta subcommands `mur --help` lists, and `mur beta list`'s enabled column | `beta.enabled` |
| `mur install` source-chain resolution | `registry.default`, `registry.sources` |
| `mur search` | `registry.index_url` |
| `mur run`, `mur doctor` — the containment floor | `containment` |
| `mur run` — the key for each `gateway.api_key: ${NAME}`, re-read while the capsule runs; `mur doctor` — whether to warn with `W-SEC-027` | `credentials`, from the global file only |
| `mur run` — the machine spend ceiling; `mur run` and `mur doctor` — [`W-SEC-026`](diagnostics.md#w-sec-026) | `spend.machine_tokens_per_day` |

`mur new` and `mur deploy` read `~/.murmur/config.yaml` only; a project-level file does not
affect them.

---

## Artifact index and custom registry URL

`mur search` fetches a static JSON catalog, `artifacts-index.json`. The default is the copy in
the Murmur default-artifacts repository:

```text
https://raw.githubusercontent.com/murmur-nexus/default-artifacts/refs/heads/main/artifacts-index.json
```

To point `mur search` at a different catalog — a private org index, say — set
`registry.index_url` in `~/.murmur/config.yaml`, or in `<cwd>/.murmur/config.yaml` to scope it to
one project:

```yaml
registry:
  index_url: https://my-org.example.com/artifacts-index.json
```

`registry.index_url` applies to every `mur search` invocation that does not pass `--registry`.

**`artifacts-index.json` shape:**

```json
{
  "schema_version": "1",
  "updated_at": "2026-06-07T00:00:00Z",
  "artifacts": [
    {
      "name": "murmur-tool-git",
      "version": "1.0.0",
      "runtime": "tool",
      "description": "Structured git interface for Murmur capsules.",
      "tags": ["tool", "git"],
      "platforms": ["darwin-aarch64", "linux-aarch64", "linux-x86_64"]
    }
  ]
}
```

| Field | Type | Required | Notes |
|---|---|---|---|
| `schema_version` | string | yes | Must be `"1"`. Any other value fails the search with `E-IO-003` |
| `updated_at` | string | yes | ISO 8601 UTC timestamp of the last regeneration |
| `artifacts` | array | yes | One entry per published artifact |
| `name` | string | yes | Artifact name, matching `name:` in its `murmur.yaml` |
| `version` | string | yes | SemVer string |
| `runtime` | string | yes | `driver`, `hook`, `tool`, or `skill` |
| `description` | string | no | Short description from `murmur.yaml`. `mur search` matches the query against it and prints an em dash when it is absent |
| `tags` | array[string] | no | Keyword tags matched against the query. Defaults to empty |
| `platforms` | array[string] | no | e.g. `darwin-aarch64`. Defaults to empty; skill artifacts have none |

---

## Registry selection rules

`mur install` and `mur publish` resolve artifacts against either the local registry under
`~/.murmur/artifacts/` or a remote Nexus registry, chosen in this order:

1. `--registry <value>` — `local` (case-insensitive) selects local mode; any other value is the
   remote URL.
2. `registry.remote_url` in `murmur.yaml` — remote mode at that URL.
3. `registry.default` in `murmur.yaml` — `local` or `remote`; `remote` uses
   `http://localhost:7800`. Any other value fails with `E-IO-003`.
4. Local mode.

These two keys live in the workspace manifest `murmur.yaml`. Its `registry.default` takes `local`
or `remote`, unlike `registry.default` in `.murmur/config.yaml`, which names a `sources` entry.

Remote mode requires the `NEXUS_API_KEY` environment variable; without it the command fails with
`E-IO-003` and the message `NEXUS_API_KEY is required for remote registry mode. Set it or use
local mode.`
