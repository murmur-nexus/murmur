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

`mur run` does not read `inference.api_key`. It reads provider keys from
[`credentials:`](#credentials), and `mur config set inference.api_key` prints a one-line note saying
so.

### `credentials:` section { #credentials }

Provider keys, by credential name. A manifest's
[`inference.api_key: ${NAME}`](manifest.md#inference-api-key) names a credential, and `mur run`
reads its value from `credentials.NAME` here.

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
| No entry, and the environment variable `NAME` is set | The variable, with [`W-SEC-026`](diagnostics.md#w-sec-026) | No |
| Neither | None: `mur run` refuses before any session directory exists, with `E-MAN-003` naming both places | — |
| `inference.api_key` is a literal, not `${NAME}` | The literal, with [`W-SEC-026`](diagnostics.md#w-sec-026) | No |
| No `inference.api_key` | None | — |

#### Rotating a key { #credentials-rotation }

A replaced entry takes effect on the next inference request that any running capsule sends. Nothing
restarts. Before each request that carries the key, the runtime checks whether the config file has
changed — its inode, size or timestamps — and re-reads it only then.

| Change | When a running capsule uses it |
|---|---|
| `mur config set -g credentials.NAME <key>` | The next request. The command replaces the file by rename, so the inode always changes |
| A hand edit that changes the file's size or timestamps | The next request |
| An in-place hand edit that keeps the size within one filesystem timestamp tick | The next change to the file, or the next `401` |
| Removing the entry, or the file becoming missing or unparseable | Never. The capsule keeps the last key it read, and the trace records `change: "unreadable"` once for that state of the file |

Removing an entry does not revoke a running capsule's key; replacing it does.

When the provider answers `401`, the runtime re-reads the file at once, whatever its timestamps
say. A changed value is sent in one resend of the same request, and that response goes to the
driver whatever its status. An unchanged value is not resent. A `401` that stands ends the task with
[`E-RUN-026`](diagnostics.md#e-run-026). Every rotation and rejection is recorded in the session
trace as an [`inference_credential`](observability-schemas.md#inference-credential) event, which
never carries the key.

A capsule started by another capsule's `delegate-task` runs with the same `HOME`, so it reads the
same file and picks up a rotated key too. Only `transport: http` capsules read credentials.

#### File permissions { #credentials-permissions }

`mur config set` does not set a mode on what it writes. Under the common umask `022`,
`~/.murmur` is created `0755` and `~/.murmur/config.yaml` is written `0644`, which every user on the
host can read. Restrict the file once it holds a key:

```bash
chmod 600 ~/.murmur/config.yaml
```

The mode survives later `mur config set` writes only if your umask keeps new files private.
Run `chmod 600` again after each write, or set `umask 077` in the shell that writes it.

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

### Where the effective config is used

| Consumer | Reads |
|---|---|
| Beta gating — which beta subcommands `mur --help` lists, and `mur beta list`'s enabled column | `beta.enabled` |
| `mur install` source-chain resolution | `registry.default`, `registry.sources` |
| `mur search` | `registry.index_url` |
| `mur run`, `mur doctor` — the containment floor | `containment` |
| `mur run` — the key for `inference.api_key: ${NAME}`, re-read while the capsule runs; `mur doctor` — whether to warn with `W-SEC-026` | `credentials`, from the global file only |

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
