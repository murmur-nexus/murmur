# Installing Artifacts

`mur install` fetches an artifact from a configured source and writes it into a local store. How you reference the artifact determines what gets fetched and where authentication comes from.

## Reference forms

| Form | Example | Resolved against |
|---|---|---|
| Name and version | `murmur-tool-git@1.0.0` | The registry first, then the configured sources |
| Bare name | `murmur-tool-git` | The configured sources, latest release |
| GitHub release | `github:<owner>/<repo>@<tag>` | That release, directly |
| Local file | `./murmur-tool-git-1.0.0.mur.zip` | A path on disk. A reference counts as a path when it starts with `./`, `../` or `/`, or contains `/` and ends in `.mur.zip` |

```bash
mur install murmur-tool-git@1.0.0
```

## Which assets an install pulls

| Reference | Assets installed |
|---|---|
| `<name>` or `<name>@<version>` | One — the matching asset from the first source that has it |
| `github:<owner>/<repo>@<tag>` | Every `.mur.zip` asset in that release |

Use a name reference to take a single artifact out of a release that contains several; the `github:` form cannot target one asset. A name reference needs the repository configured as a source — see [Multiple sources and fallthrough](#multiple-sources-and-fallthrough).

`<name>@<version>` searches the release tagged `<version>`, then `v<version>`, then the source's latest release, so a repository whose release tags are independent of artifact versions still resolves. Within a release, assets are matched by filename in this order, where `<platform>` is the host platform — `darwin-aarch64`, `darwin-x86_64`, `linux-aarch64` or `linux-x86_64`:

| Reference | Filenames tried, in order |
|---|---|
| `<name>@<version>` | `<name>-<version>-<platform>.mur.zip`, `<name>-<version>.mur.zip` |
| `<name>` | `<name>-*-<platform>.mur.zip`, `<name>.mur.zip`, `<name>-*.mur.zip` |

## Authentication

Token resolution happens in this order:

1. The `token` field of the matching `registry.sources` entry in the [effective config](config.md#configuration-files)
2. The `GITHUB_TOKEN` environment variable

For `github:` URIs, Murmur borrows the token of the configured source whose `repo` is `<owner>/<repo>`, then falls back to `GITHUB_TOKEN`. Public repositories work without a token; private repositories require one.

The `token` field accepts three forms:

| Value | Resolved as |
|---|---|
| `${GITHUB_TOKEN}` | Value of the `GITHUB_TOKEN` env var. No token is sent when the variable is unset |
| `MY_TOKEN` | Value of the `MY_TOKEN` env var, or the literal string if the var is unset |
| `ghp_abc123` | Used as-is |

## Multiple sources and fallthrough

For lookups by name, configured sources are tried in the order they appear in the effective
`registry.sources` list, with the source named by `registry.default` moved to the front. A source
that does not have the requested artifact falls through to the next.

```yaml
registry:
  default: internal
  sources:
    - name: internal
      type: github
      repo: my-org/private-artifacts
      token: "${GITHUB_TOKEN}"
    - name: official
      type: github
      repo: murmur-nexus/default-artifacts
```

`type` is `github`. Full field reference: [`registry:` section](config.md#registry-section).

## Default source

With neither `~/.murmur/config.yaml` nor `<cwd>/.murmur/config.yaml` present, `mur install` uses one built-in source: the public GitHub repository `murmur-nexus/default-artifacts`, which needs no token. Once `~/.murmur/config.yaml` exists, its `registry.sources` list is the whole chain — see [Merge rules](config.md#merge-rules).

## Local artifact cache

`mur install` writes into the project store; `mur install -g` writes into the global store.

| Store | Root |
|---|---|
| Project | `<project root>/.murmur/artifacts/` — the nearest directory at or above the working directory that holds a `murmur.yaml`. Installing without one fails with `E-IO-001` |
| Global | `~/.murmur/artifacts/` |

Under either root, an artifact occupies `<name>/<version>/`:

| File | Holds |
|---|---|
| `<name>-<version>.mur.zip` | The artifact |
| `<name>-<version>.sha256` | Its SHA-256 |
| `<name>-<version>.meta.json` | Its name, version, runtime, description, tags and WIT contracts |

The `wit_contracts` key of `<name>-<version>.meta.json` records the versioned WIT interface names the packed component declares, under `exports` and `imports`. The store derives both lists from the artifact bytes on every write, so they always describe the artifact they sit beside. The key is absent for an artifact carrying no readable component — a native binary, a skill, or a payload that is not a WebAssembly component. `mur list --contract <PREFIX>` reads it; see [`mur list`](cli.md#mur-list).

`mur install --all-platforms <name>@<version>` writes `<name>-<version>-<platform>.mur.zip` and `<name>-<version>-<platform>.sha256` into the global store instead, one pair per platform.

The global root is derived from `$HOME` (or `$USERPROFILE` on Windows). No environment variable overrides it; to relocate the store, set `HOME` before invoking `mur`.

## Registry selection

A `<name>@<version>` reference is resolved against a registry first, and falls through to the configured sources only when the registry does not have it. The registry is the local store unless remote mode is selected, by `--registry <url>`, by `registry.remote_url` in `murmur.yaml`, or by `registry.default: remote` in `murmur.yaml`. Remote mode requires the `NEXUS_API_KEY` environment variable. See [Registry selection rules](config.md#registry-selection-rules).
