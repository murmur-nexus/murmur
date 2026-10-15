# Sources and stores

`mur` gets artifacts from two kinds of place:

| Kind | What it is | Where it is configured |
|---|---|---|
| Store | A directory of installed `.mur.zip` artifacts on this machine: the project store `<project root>/.murmur/artifacts/` and the global store `~/.murmur/artifacts/`. See [Local artifact cache](../reference/installing-artifacts.md#local-artifact-cache) | Fixed paths |
| Source | A GitHub repository whose releases carry `.mur.zip` assets. `murmur-nexus/default-artifacts` is built in | `registry.sources` in `~/.murmur/config.yaml` — see [Multiple sources and fallthrough](../reference/installing-artifacts.md#multiple-sources-and-fallthrough) |

`mur install` copies an artifact from a source, a store or a file into a store. `mur publish` writes
an artifact you built into the global store. `mur run` reads the stores only, so a capsule launches
with exactly what was installed and never reaches the network for an artifact.

A source can publish an [artifact index](../reference/artifact-index.md), `artifacts-index.json`,
beside its release assets. It lists every version the source serves, with the release holding each
one, the SHA-256 of each file, and the `murmur:` interfaces each version needs. That is how
[`mur search`](../reference/cli.md#mur-search) shows, for each artifact, the newest version that runs
on this `mur`. [`mur index`](../reference/cli.md#mur-index) writes the index from the release's files;
see [Publish your own artifact source](../how-to/publish-artifact-source.md).

`mur install <scope>/<name>@<version>` asks each source for the bare `<name>` and stores what it
finds under the full name you gave. The releases `murmur-nexus/default-artifacts` publishes name
their assets with a `murmur-` prefix, so `murmur/murmur-tool-git@0.2.0` resolves from it and
`murmur/tool-git@0.2.0` does not.

## Artifact names { #artifact-names }

An installed artifact has a full name, `<scope>/<name>`: `murmur/tool-git`, `ana/tool-x`,
`local/my-tool`.

| Part | What it is |
|---|---|
| `<scope>` | Whose artifact it is |
| `<name>` | The bare name: the `name:` the artifact's own `murmur.yaml` declares, and the name a capsule's model calls it by |

A full name has exactly one `/`. Each part is lowercase letters, digits and `-`, does not start or
end with `-`, and is at most 100 characters.

The scope is chosen by where the artifact is installed from:

| Installed from | Scope |
|---|---|
| `murmur-nexus/default-artifacts` | `murmur` |
| A `.mur.zip` file (`mur install <path>`), or written by `mur publish` | `local`. No source is ever asked for a `local/` artifact |
| Any other GitHub repository | The repository owner, lowercased: `github:Ana/tools@v1` installs as `ana/<name>` |

`murmur` and `local` are reserved: a `github:` owner named either is refused with
[`E-REG-007`](../reference/diagnostics.md#e-reg-007).

Everything that resolves an artifact takes the full name: `mur install`, `mur run --capsule`,
`artifacts:` entries in `murmur.yaml`, `members[].capsule` in `roster.yaml`, `murmur.lock`, and
`manage.*`. A bare name there is refused, with a hint naming its scoped forms. Three places take
the bare name:

| Place | Example |
|---|---|
| An artifact's own `name:`, the one `mur build` packs | `name: tool-git` |
| A `source:` entry in `artifacts:` | `- name: style` with `source: ./skills/style` |
| Inside a session: the tool name the model calls, `workdir/tools/<name>/`, skill names in `MURMUR.md` | `tool-git` |

Because a session names each artifact by its bare name, two artifacts a capsule declares cannot
share one: `murmur/tool-editor` and `ana/tool-editor` together are refused with
[`E-CAP-020`](../reference/diagnostics.md#e-cap-020).

### Store layout { #store-layout }

Under a store root, an artifact is filed at:

```text
<root>/<scope>/<name>/<version>/<name>-<version>[-<platform>].mur.zip
```

with its `.sha256` and `.meta.json` beside it under the same stem. A store directory in the flat
`<root>/<name>/<version>/` layout is not read by any command, and [`mur doctor`](../reference/cli.md#mur-doctor)
names each one as safe to delete. An artifact is filed under the bare name its own `murmur.yaml`
declares: installing one under a full name whose bare part differs is refused with
[`E-REG-002`](../reference/diagnostics.md#e-reg-002). The files of the layout are in
[Local artifact cache](../reference/installing-artifacts.md#local-artifact-cache).

## Resolution order

`mur install <scope>/<name>@<version>`, and `mur install` with no argument, check the global store before
any source, so an artifact you published or installed with `-g` is copied rather than downloaded
again. `mur run`, `mur doctor` and `mur eval` read the project store, then the global store, and
never a source. Sources are tried with the one `registry.default` names first, then the rest in
the order listed. The order for every form of `mur install` is in
[Resolution order](../reference/config.md#resolution-order).

A `murmur.yaml` does not choose where its artifacts come from: a top-level `registry:` key is
refused with [`E-MAN-003`](../reference/diagnostics.md#e-man-003).

## Versioning

Murmur treats versions as immutable identities. For reproducibility, lock data records concrete
resolved versions and artifact hashes — see
[Lockfile (`murmur.lock`)](../reference/workdir.md#lockfile-murmurlock) for which
commands read and write it.

## Lock integrity { #lock-integrity }

`murmur.lock` pins what a project actually resolved: for every artifact resolved from a store or a source, the
concrete version and the sha256 of its bytes. Artifacts declared with `source:` are never written to it —
they are read fresh from disk on every run.

When a lockfile is present, three things are checked before a session is staged:

1. The lock has an entry for the artifact, carrying a hash for this host's platform.
2. That entry's `resolved_version` matches the version `murmur.yaml` declares.
3. Its recorded sha256 matches the sha256 of the bytes actually installed.

A native tool is a different binary per platform, so an entry for one pins a hash per platform tag
and a host verifies against its own — never against another platform's, which would report a
correct artifact as tampered with. Everything else pins one hash every host verifies against. See
[the lockfile reference](../reference/workdir.md#lockfile-murmurlock) for the file's shape.

Any disagreement is a refusal, not a warning — the artifact is never used. `mur run` applies the
check before staging, and `mur doctor` applies the same one without launching anything, so a
project `mur run` would reject never reports as healthy. A lockfile that exists but fails to parse
is a hard failure in both, reported before any artifact is checked.

With no lockfile present, resolution falls back to presence-only: the artifact is used if it is
installed, with nothing to compare it against.

## Artifact integrity

Every `.mur.zip` a capsule or the CLI reads — whether fetched from a source or already in a
store — goes through a shared hardening layer before any of its bytes are trusted:

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
