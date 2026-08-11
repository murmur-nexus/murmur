# Registry

Murmur Nexus is the artifact registry layer.

Its responsibility is simple: store and serve versioned `.mur.zip` artifacts.

## Artifact model

Artifacts include tools, bootstrap components, and provider drivers. Capsules declare required artifacts by name and version in the manifest.

## Local artifact cache

Downloaded artifacts are stored on disk at:

```
~/.murmur/artifacts/<name>/<version>/<name>-<version>.mur.zip
```

The path is derived from `$HOME` (or `$USERPROFILE` on Windows). There is no environment variable to override the cache location — if you need to relocate it, set `HOME` before invoking `mur`.

## Local vs remote resolution

Murmur supports:

- **Local registry mode** — resolves artifacts from the local cache at `~/.murmur/artifacts/`. The default source for cache population is the `murmur-nexus/default-artifacts` GitHub repository.
- **Remote registry mode** — resolves artifacts from a Nexus instance over HTTP. Requires `NEXUS_API_KEY` to be set.

### Workspace config (`murmur.yaml`)

Place a `murmur.yaml` file in your project directory to configure registry resolution for that workspace:

```yaml
registry:
  default: local          # "local" (default) or "remote"
  remote_url: http://your-nexus:7800   # only used when default is "remote"
```

`remote_url` overrides the CLI default of `http://localhost:7800`.

### User config (`~/.murmur/config.yaml`)

User-level settings live at `~/.murmur/config.yaml` and configure which remote sources `mur install` pulls from:

```yaml
registry:
  default: official
  sources:
    - name: official
      type: github
      repo: murmur-nexus/default-artifacts
      token: "${GITHUB_TOKEN}"     # env reference or literal token
```

`type` is one of `github` or `nexus`. The `token` field accepts a `${VAR}` reference (resolved at run time) or a bare environment variable name.

An optional project-level file at `<cwd>/.murmur/config.yaml` merges with this global file —
`registry.sources` entries union by `name` (a project entry can add or override a source without
disturbing the rest). See [Configuration files](../reference/cli.md#configuration-files) in the
CLI reference for the full merge rules and `mur config set`.

### Per-command override

Pass `--registry <url>` to any command that contacts a registry to force remote mode with that URL for that invocation, regardless of workspace config.

## Installing artifacts

`mur install` downloads an artifact from a configured source into the local store. How you reference the artifact determines what gets fetched and where authentication comes from.

### URI forms

| Form | Example |
|---|---|
| Bare name with version | `murmur-tool-git@0.3.18` |
| GitHub release | `github:<owner>/<repo>@<tag>` |
| Nexus | `nexus:<name>@<version>` |

```bash
mur install murmur-tool-git@0.3.18
```

### Bare name vs explicit GitHub URI

These two forms behave differently when a release contains multiple `.mur.zip` files:

- **Bare name** — matches by artifact name. Searches configured sources in order and picks the asset whose filename matches `<name>.mur.zip` or `<name>-*.mur.zip`. Only the matching artifact is installed.
- **`github:<owner>/<repo>@<tag>`** — goes directly to that release and installs every `.mur.zip` asset in it. There is no way to target a single asset with the explicit URI form.

Use the bare-name form when you want a specific artifact from a release that contains several. This requires the repository to be configured as a source in `~/.murmur/config.yaml` (see above).

### Authentication

Token resolution happens in this order:

1. The `token` field of the matching `registry.sources` entry in the effective config (global
   `~/.murmur/config.yaml` merged with any project-level `<cwd>/.murmur/config.yaml`)
2. The `GITHUB_TOKEN` environment variable — used as a fallback for both configured and explicit URI pulls

For explicit `github:` URIs, Murmur checks whether `<owner>/<repo>` matches any configured source and borrows that source's token if found, then falls back to `GITHUB_TOKEN`. Public repositories work without a token. Private repositories require one.

The `token` field accepts three forms:

| Value | Resolved as |
|---|---|
| `${GITHUB_TOKEN}` | Value of the `GITHUB_TOKEN` env var |
| `MY_TOKEN` | Value of the `MY_TOKEN` env var, or the literal string if the var is unset |
| `ghp_abc123` | Used as-is |

### Multiple sources and fallthrough

For bare-name lookups, configured sources are tried in the order they appear in the effective
`registry.sources` list, with the `default` source moved to the front. If the first source does
not contain the requested artifact, the lookup falls through to the next.

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

### Default source

When `~/.murmur/config.yaml` does not exist, `mur install` falls back to `murmur-nexus/default-artifacts` as its source. No token is required for this public repository.

This default is a Rust literal compiled into the `murmur-cli` binary (`impl Default for MurConfig` in `crates/murmur-cli/src/config.rs`) — it is never fetched from any remote location at install time or any other point. A local `~/.murmur/config.yaml` legitimately overrides it by declaring its own `registry.sources`, but that override is read from a local file path only; nothing in the config-loading path makes an HTTP call that could be hijacked to substitute a different trust root.

---

## Versioning and channels

Murmur treats versions as immutable identities. Channel pointers (for example `latest`/`stable`) are mutable aliases that resolve to concrete versions at install time.

For reproducibility, lock data records concrete resolved versions and artifact hashes — see
[Lockfile (`murmur.lock`)](../reference/manifest-schema.md#lockfile-murmurlock) for which
commands read and write it.

## Artifact integrity

Every `.mur.zip` a capsule or the CLI reads — whether fetched from the registry or already on
disk — goes through a shared hardening layer before any of its bytes are trusted:

- **Path sanitization** — an entry name with a leading `/` or any `..` component is never
  selected as the capsule's root `.wasm` file (or any other extracted file); it's treated as if
  the entry didn't exist at all.
- **Decompression ceiling** — reading an entry's decompressed bytes stops once more than
  500MB have been produced, so a crafted archive can't exhaust memory or disk before its
  content is even validated. Override the ceiling with the `MURMUR_MAX_ARTIFACT_DECOMPRESSED_BYTES`
  environment variable (bytes; falls back to the 500MB default if unset or unparseable).

This is independent of the sha256/lock verification above: hardening protects against a
malformed or malicious archive shape, while hash verification protects against a
tampered-but-well-formed one.
