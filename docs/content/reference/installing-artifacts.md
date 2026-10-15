# Installing Artifacts

`mur install` fetches an artifact from the source of its scope and writes it into a local store. How you reference the artifact determines what gets fetched and where authentication comes from.

## Reference forms

| Form | Example | Resolved against | Stored as |
|---|---|---|---|
| Name and version | `murmur/tool-git@{{ v.murmur_tool_git }}` | The global store first, then the source of the scope. A `local/` name is never asked of a source | The name given |
| Name | `murmur/tool-git` | The source of the scope: the newest version that [fits this `mur`](#version-selection) | The name given |
| GitHub release | `github:<owner>/<repo>@<tag>` | That release, directly | `<scope>/<name>` for each asset, `<scope>` being the repository's scope and `<name>` the name the asset's `murmur.yaml` declares |
| Local file | `./tool-git-{{ v.murmur_tool_git }}.mur.zip` | A path on disk. A reference counts as a path when it starts with `./`, `../` or `/`, or contains `/` and ends in `.mur.zip` | `local/<name>`, `<name>` being the name the file's `murmur.yaml` declares |

Every name on the command line is a full name, `<scope>/<name>` — see
[Artifact names](../concepts/registry.md#artifact-names). A bare name such as `tool-git` is refused
with [`E-REG-007`](diagnostics.md#e-reg-007), and so is a `github:` reference whose owner is
`local` or `murmur`. A version of `latest`, `stable` or `edge` is refused with
[`E-REG-004`](diagnostics.md#index): leave the version out to get the newest that fits.

```bash
mur install murmur/tool-git@{{ v.murmur_tool_git }}
```

## Sources and scopes { #sources-and-scopes }

Each scope has exactly one source, a GitHub repository its owner publishes from:

| Scope | Source |
|---|---|
| `murmur` | `murmur-nexus/default-artifacts`, built in. It is present with or without a config file, and no other repository serves it |
| `local` | None. A `local/` artifact is installed from its `.mur.zip` file |
| Any other, such as `ana` | The `registry.sources` entry whose `repo` is owned by `ana`, in the [effective config](config.md#configuration-files) |

```yaml
registry:
  sources:
    - repo: ana/artifacts
      token: ${GITHUB_TOKEN}
```

The scope of an entry is its owner, lowercased: `Ana/artifacts` serves `ana`. One file may hold one entry per scope, and a project file's entry replaces the global file's entry for the same scope. An entry for `murmur-nexus/default-artifacts` lends the built-in source its `token:` and nothing else. Field reference: [`registry:` section](config.md#registry-section).

A scope with no source is refused with [`E-REG-008`](diagnostics.md#e-reg-008) before any request. `registry.sources: []` gives no third-party scope a source; `murmur/` artifacts still install.

A `github:<owner>/<repo>@<tag>` reference needs no entry: it names its repository, and its assets install in the owner's scope.

## Index resolution { #index-resolution }

A source answers through the `artifacts-index.json` asset of its latest release, a schema 2 [artifact index](artifact-index.md). `<scope>/<name>@<version>` resolves in these steps:

1. Read the owner's numeric GitHub id and check it — see [Owner identity](#owner-identity).
2. Read the index from the latest release. A missing, schema 1 or invalid index is refused with [`E-IDX-002`](diagnostics.md#e-idx-002).
3. Check that the index's `scope` is the source's scope. An index naming another scope is refused with [`E-REG-009`](diagnostics.md#e-reg-009).
4. Find the artifact whose `name` is `<name>` exactly, and its version `<version>` exactly. `tool-git` never matches `tool-git-validate`, and `1.0.0` never matches `v1.0.0`.
5. Choose the asset whose `platform` is the host's, else the untagged one. Another platform's asset is never chosen.
6. Download that asset's `file` from the version's `release`.
7. Check its SHA-256 against the index's `sha256`, and its packed `name:` and `version:` against the reference. A difference is refused with [`E-REG-002`](diagnostics.md#e-reg-002) and nothing is stored.

A name, version or platform payload the index does not list is refused with [`E-REG-001`](diagnostics.md#e-reg-001), naming what the index does list.

| Reference | Assets installed |
|---|---|
| `<scope>/<name>` or `<scope>/<name>@<version>` | One: the asset the index lists for this platform |
| `github:<owner>/<repo>@<tag>` | Every `.mur.zip` asset in that release. No index is read |

A platform tag in an asset is the publisher's declaration of which platform those bytes are for. `linux-aarch64` is a supported target for which Murmur's own releases publish no asset: an artifact needed there is built on that host with [`mur build`](cli.md#mur-build) and installed from the resulting path.

### Version selection { #version-selection }

| Reference | Version installed |
|---|---|
| `<scope>/<name>@<version>` | `<version>`, as asked, whether or not it fits this `mur` |
| `<scope>/<name>` | The newest version in the index that fits this `mur`: every `murmur:` interface it names is one this `mur` serves at that exact version, and it has a payload for this platform. The same rule [`mur search`](cli.md#mur-search) lists by |

When no version fits, the install is refused with [`E-REG-001`](diagnostics.md#e-reg-001), naming the newest version and the first interface this `mur` does not serve.

`mur install` with no argument, `mur install --all-platforms` and `mur deploy` resolve each artifact the same way.

## Owner identity { #owner-identity }

A scope is tied to the GitHub account that owns it, by that account's numeric id. The id is read from `GET /repos/<owner>/<repo>`, which answers without a token for a public repository, and recorded on every install from a source:

| Where | Key |
|---|---|
| The project's `murmur.lock` entry | `owner_id` |
| The payload's `.meta.json` in the store | `owner_id` |

Before anything is downloaded or written, the id the source answers with is compared:

| Scope | Compared with |
|---|---|
| `murmur` | `190196424`, the id of `murmur-nexus`, compiled into `mur`. Checked even with no `murmur.lock` |
| Any other | Every `owner_id` the project's `murmur.lock` records for an entry of that scope |

A project install served from the global store compares the `owner_id` of the stored payload the same way, without a request.

A different id is refused with [`E-REG-010`](diagnostics.md#e-reg-010). It means the account that owned the scope was renamed and its name registered again by someone else, or the repository changed hands. To trust the new owner, delete the scope's entries from `murmur.lock` and install again.

`mur install -g` reads no `murmur.lock`, so it checks only the `murmur` scope's id. `mur deploy` does the same.

## Authentication

Token resolution happens in this order:

1. The `token` field of the `registry.sources` entry for the scope in the [effective config](config.md#configuration-files)
2. The `GITHUB_TOKEN` environment variable

For `github:` URIs, Murmur borrows the token of the configured source whose `repo` is `<owner>/<repo>`, then falls back to `GITHUB_TOKEN`. Public repositories work without a token; private repositories require one.

The `token` field accepts three forms:

| Value | Resolved as |
|---|---|
| `${GITHUB_TOKEN}` | Value of the `GITHUB_TOKEN` env var. No token is sent when the variable is unset |
| `MY_TOKEN` | Value of the `MY_TOKEN` env var, or the literal string if the var is unset |
| `ghp_abc123` | Used as-is |

### Rate limits

Lookups sent without a token share GitHub's limit of 60 API requests an hour per IP address. One `mur install` makes these requests to a source:

| Request | How often |
|---|---|
| `GET /repos/<owner>/<repo>` | Once per source |
| `GET /repos/<owner>/<repo>/releases/latest` | Once per source |
| The `artifacts-index.json` download | Once per source |
| `GET /repos/<owner>/<repo>/releases/tags/<release>` | Once per release other than the latest that holds an artifact asked for |
| The payload download | Once per artifact |

So the first artifact of a scope can spend five requests and each further one two. A lookup refused by the limit fails with [`E-REG-006`](diagnostics.md#e-reg-006).

A token raises the limit. Export one before installing:

```bash
export GITHUB_TOKEN=$(gh auth token)
```

Any GitHub token works; `gh auth token` is one way to get one. To send a token held in a different variable, name that variable on the source:

```yaml
registry:
  sources:
    - repo: murmur-nexus/default-artifacts
      token: ${GH_TOKEN}
```

`GH_TOKEN` is not read unless a source names it this way.

Prefer a `${NAME}` reference to a literal token in `config.yaml`. The token `gh auth token` prints is rotated by the GitHub CLI, so a copy written to disk stops working without warning and later lookups fail with `HTTP 401`.

## Local artifact cache

`mur install` writes into the project store; `mur install -g` writes into the global store.

| Store | Root |
|---|---|
| Project | `<project root>/.murmur/artifacts/` — the nearest directory at or above the working directory that holds a `murmur.yaml`. Installing without one fails with `E-IO-001` |
| Global | `~/.murmur/artifacts/` |

Under either root, an artifact occupies `<scope>/<name>/<version>/`, and the files in it are named by the bare `<name>`: `murmur/tool-git@{{ v.murmur_tool_git }}` is filed at `murmur/tool-git/{{ v.murmur_tool_git }}/tool-git-{{ v.murmur_tool_git }}.mur.zip`. A native artifact is a different binary per platform, so its three files carry the platform tag its payload was built for; every other payload is one set of bytes each host resolves, and its files carry no tag:

| File | Holds |
|---|---|
| `<name>-<version>.mur.zip` | The artifact — a WASM component or a static skill |
| `<name>-<version>.sha256` | Its SHA-256 |
| `<name>-<version>.meta.json` | Its full name, version, runtime, platforms, description, tags, WIT contracts and, for a payload a source served, the [owner id](#owner-identity) |
| `<name>-<version>-<platform>.mur.zip` | A native artifact's payload for one platform |
| `<name>-<version>-<platform>.sha256` | That payload's SHA-256 |
| `<name>-<version>-<platform>.meta.json` | That payload's metadata, with `platforms` naming the one platform |

An artifact is filed under the bare name its own `murmur.yaml` declares: a payload whose packed `name:` differs from the bare part of the name it is installed as is refused with [`E-REG-002`](diagnostics.md#e-reg-002).

A store directory in the flat `<name>/<version>/` layout, with no scope directory, is not read by any command. [`mur doctor`](cli.md#mur-doctor) names each one in its `Artifact stores` block as safe to delete.

One version directory holds one payload, hash and metadata file per platform installed, so installing a native artifact for a second platform sits beside the first rather than replacing it. A resolve prefers the tagged payload for the host's platform and falls back to the untagged one, which is how a WASM artifact resolves everywhere; a *native* artifact resolved through that fallback is reported as [`W-REG-001`](diagnostics.md#w-reg-001).

The `runtime` and `artifact_runtime` keys of the metadata file record what the packed `murmur.yaml` declares: the packaging type (`native`, `wasm` or `static`) and the role (`tool`, `skill`, `driver` or `hook`). Every command that resolves an artifact takes both from the artifact bytes, so a metadata file recording something else does not change what is resolved, run or deployed. `mur list` reads the metadata file rather than the artifact, so an artifact whose file records the wrong role is listed with a blank `RUNTIME` column until it is reinstalled.

The `wit_contracts` key of the metadata file records the versioned WIT interface names the packed component declares, under `exports` and `imports`. The store derives both lists from the artifact bytes on every write, so they always describe the artifact they sit beside. The key is absent for an artifact carrying no readable component — a native binary, a skill, or a payload that is not a WebAssembly component. `mur list --contract <PREFIX>` reads it; see [`mur list`](cli.md#mur-list).

`mur install --all-platforms <scope>/<name>@<version>` installs into the global store, once per platform it downloads for, writing the same three files per platform as any other install.

The global root is derived from `$HOME` (or `$USERPROFILE` on Windows). No environment variable overrides it; to relocate the store, set `HOME` before invoking `mur`.

## Resolution order { #resolution-order }

A `<scope>/<name>@<version>` reference is resolved from the global store `~/.murmur/artifacts/` first, and goes to the source of its scope only when the global store does not have it. `mur install` with no argument does the same for each artifact `murmur.yaml` declares. The order for every command is in [Resolution order](config.md#resolution-order).
