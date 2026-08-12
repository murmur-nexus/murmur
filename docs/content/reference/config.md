# Configuration

## Configuration files

Murmur CLI configuration lives in up to two YAML files, resolved into one *effective* config:

| File | Scope | Discovery |
|---|---|---|
| `~/.murmur/config.yaml` | Global (per-user) | Fixed path |
| `<cwd>/.murmur/config.yaml` | Project (per-workspace) | `<cwd>` only — unlike project-root manifest discovery elsewhere in the CLI, this does **not** walk up to parent directories |

Both files are optional; a missing file is treated as empty. Write them with
[`mur config set`](cli.md#mur-config) (project by default, `-g` for global) or edit the YAML by hand.

### Merge rules

Where both files set a value, the effective config is built per field:

| Field | Rule |
|---|---|
| `registry.default` | Project wins if non-empty, else global, else the built-in default (`official`) |
| `registry.index_url` | Same non-empty-wins rule |
| `inference.provider` / `inference.model` / `inference.endpoint` | Same non-empty-wins rule |
| `inference.api_key` | **Always** the global value — see below |
| `registry.sources` | Union by `name`: a project entry sharing a global entry's name replaces it in place (position preserved); a project entry with a new name is appended; global-only entries are never dropped |
| `beta.enabled` | Union by value: global flags first, then any project-only flags appended, in the project file's order |
| `containment` | **Strongest wins**, not project-wins — see [Containment class](containment.md#field-containment) |

If the project file does not exist, the effective config is exactly the global config (with
built-in defaults applied where global is also empty).

### `inference:` section

Configures the inference provider used by `mur new`. When present and complete in the global
file, it takes priority over the `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` env vars and the
interactive wizard. (`mur new` reads `~/.murmur/config.yaml` directly, not the merged effective
config — see the note under [`mur new`](cli.md#mur-new).)

```yaml
inference:
  provider: anthropic              # "anthropic" or "openai"
  model: claude-haiku-4-5-20251001
  api_key: sk-ant-...
  endpoint: ""                     # optional; leave empty for the provider default
```

| Key | Required | Description |
|---|---|---|
| `provider` | yes | `"anthropic"` or `"openai"` |
| `model` | yes | Model name passed to the inference driver |
| `api_key` | yes | API key for the provider |
| `endpoint` | no | Override the base URL (e.g. for a proxy); empty = provider default |

**Provider defaults:**

| Provider | Default endpoint | Default model (wizard) |
|---|---|---|
| `anthropic` | `https://api.anthropic.com` | `claude-haiku-4-5-20251001` |
| `openai` | `https://api.openai.com` | `gpt-4o-mini` |

**Env var override behavior:** `inference:` in the global file is checked first; if absent or
incomplete, the env vars `ANTHROPIC_API_KEY` and `OPENAI_API_KEY` are checked (in that order); if
neither is set, the interactive wizard fires. To disable the env var fallback, set `inference:`
in the global config file.

#### `inference.api_key` is always global

`inference.api_key` is the one field that does not follow "project wins": the effective config
reads it from `~/.murmur/config.yaml` only, regardless of what the project file contains — even
if the project value is a `${VAR}` reference. If the global file has no `inference:` block at
all, the effective `api_key` is `""`.

A **literal** (non-`${VAR}`) `inference.api_key` in the project file triggers a warning — both
when the effective config is loaded and when `mur config set` writes it (see
[`mur config set`](cli.md#mur-config-set-key-value) above):

```text
warning: <cwd>/.murmur/config.yaml sets inference.api_key to a literal value, but inference.api_key is always read from the global config (~/.murmur/config.yaml); this project-level value will be ignored
```

A `${VAR}`-shaped value (e.g. `${MY_ORG_KEY}`) prints no warning on either path — it's simply
never consulted, same as any other project-level `inference.api_key`.

### `registry:` section

See [Artifact index and custom registry URL](#artifact-index-and-custom-registry-url) below for
`registry.index_url`, and [Multiple sources and fallthrough](installing-artifacts.md#multiple-sources-and-fallthrough) for
`registry.sources`.

### `beta:` section

Stores the list of beta features opted into via `mur beta enable`/`mur beta disable`. Those
commands always read and write the global file only (no `-g`/project flag). `beta.enabled` is
not a settable `mur config set` key (see [`mur config set`](cli.md#mur-config-set-key-value)) — a
project-level `beta.enabled` entry can only be added by hand-editing
`<cwd>/.murmur/config.yaml`.

```yaml
beta:
  enabled:
    - blueprint
    - dag-topology
```

| Key | Type | Description |
|---|---|---|
| `enabled` | array of strings | Feature names opted into |

An absent `beta:` section is equivalent to `enabled: []`. Features listed here but not compiled
into the current binary are silently ignored — they take effect once a binary that includes them
is installed.

### Where the effective (merged) config is used

The merged global+project config is read by:

- Beta feature gating (which subcommands `mur --help` shows, and `mur beta list`'s enabled column)
- `mur install`'s registry source-chain resolution
- `mur search`'s `registry.index_url` resolution

`mur new` and `mur deploy` — both beta-gated — still read `~/.murmur/config.yaml` only; this is a
deliberate scope limit, not an oversight.

---

## Artifact index and custom registry URL

`mur search` fetches a static JSON catalog called `artifacts-index.json` hosted at a canonical public URL. The default URL points to the Murmur default-artifacts repository. To use a different URL (for example a private org catalog), add this to `~/.murmur/config.yaml` — or `<cwd>/.murmur/config.yaml` to scope it to one project — or set it with `mur config set registry.index_url <url>`:

```yaml
registry:
  index_url: https://my-org.example.com/artifacts-index.json
```

The `registry.index_url` key overrides the default for all `mur search` invocations that do not pass an explicit `--registry` flag. See [Configuration files](#configuration-files) for how the project and global values merge.

**`artifacts-index.json` schema:**

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
| `schema_version` | string | yes | Always `"1"` — future incompatible changes bump this |
| `updated_at` | string | yes | ISO 8601 UTC timestamp of last regeneration |
| `artifacts` | array | yes | One entry per published artifact |
| `name` | string | yes | Artifact name (matches `name:` in murmur.yaml) |
| `version` | string | yes | SemVer string |
| `runtime` | string | yes | `driver`, `hook`, `tool`, or `skill` |
| `description` | string | yes | Short description from murmur.yaml |
| `tags` | array[string] | no | Keyword tags; empty array if absent |
| `platforms` | array[string] | no | e.g. `darwin-aarch64`; empty for skill artifacts |

**Regenerating the index:** the index is regenerated by `scripts/apply-versions.sh` in the default-artifacts repository whenever artifact versions are bumped. Run that script from the default-artifacts repo root to update `artifacts-index.json` in place.

---

## Registry selection rules

Resolution order in CLI:

1. `--registry <url>` flag
2. `murmur.yaml` `registry.remote_url`
3. `murmur.yaml` `registry.default` (`local` or `remote`)
4. fallback local mode

Remote mode requires:

- `NEXUS_API_KEY` environment variable
