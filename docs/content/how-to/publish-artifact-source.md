# How to publish your own artifact source

An artifact source is a GitHub repository whose releases carry `.mur.zip` assets and an
`artifacts-index.json` that lists them. Anyone who names your repository as the source of your
scope can install your artifacts with `mur install <owner>/<name>`, and find them with
`mur search`. Each release adds its
versions to the index and keeps every earlier version listed, so a `mur` that cannot run your
newest version can still find an older one that fits it.

??? definition "What is an artifact index?"
    An **artifact index** is the `artifacts-index.json` file a source publishes: every version of
    every artifact it serves, with the release that holds each version, the SHA-256 of each file,
    and the interfaces each version needs. See [Artifact index](../reference/artifact-index.md).

The relevant artifact manifest options are:

| Option | Controls |
|---|---|
| [name](../reference/manifest.md#artifact-manifest) | The bare name the index lists the artifact under |
| [version](../reference/manifest.md#artifact-manifest) | The version the index lists; a semantic version |
| [description](../reference/manifest.md#artifact-manifest) | The description `mur search` shows, cut to its first sentence |

The examples use the repository `ana/my-artifacts` and the release tag `v1.1.0`. Artifacts from it
install under the scope `ana`.

---

## Step 1 — build every artifact into one directory

Create an empty directory for the release, then run `mur build` once per artifact with `--output`
pointing at it:

```bash
mkdir release
mur build ./tool-editor --output release/
mur build ./hook-audit --output release/
```

Each artifact becomes `<name>-<version>.mur.zip`. A native tool needs one file per platform, named
`<name>-<version>-<platform>.mur.zip`. Build it on each platform and give `--output` that file
name:

```bash
mur build ./tool-git --output release/tool-git-0.2.0-linux-x86_64.mur.zip
```

`<platform>` is one of `darwin-aarch64`, `darwin-x86_64`, `linux-aarch64` or `linux-x86_64`.

## Step 2 — write the index

Download the index from your previous release, then run `mur index` with `--merge` pointing at it:

```bash
gh release download v1.0.0 --repo ana/my-artifacts --pattern artifacts-index.json --dir previous
mur index release --repo ana/my-artifacts --release v1.1.0 --merge previous/artifacts-index.json -o release/artifacts-index.json
```

```text
Indexed 2 artifacts (3 versions) for scope ana into release/artifacts-index.json; 1 versions kept from previous/artifacts-index.json, 1 replaced
```

| Count | Meaning |
|---|---|
| `kept` | Versions only the previous index lists. They keep the release that holds them |
| `replaced` | Versions in both whose files differ, such as a rebuild of an unchanged version. They now point at this release |

On the first release there is no previous index: leave out `--merge`.

`mur index` refuses the whole directory, and writes nothing, if one file's name does not match the
`name:` and `version:` packed inside it. See [`E-IDX-001`](../reference/diagnostics.md#e-idx-001).

## Step 3 — upload the release

Create the release and upload every file in the directory as one of its assets:

```bash
gh release create v1.1.0 --repo ana/my-artifacts release/*
```

Upload the `.mur.zip` files and `artifacts-index.json` together. The index records `v1.1.0` as
the release holding every file in the directory, so each of those files must be an asset of it.

## Step 4 — search your source

Point `mur search` at the index of your latest release:

```bash
mur search editor --index https://github.com/ana/my-artifacts/releases/latest/download/artifacts-index.json
```

```text
NAME             VERSION  RUNTIME  DESCRIPTION
ana/tool-editor  0.7.0    tool     Read, write and patch files in the workdir.
```

To search it without `--index`, set `registry.index_url` to that URL — see
[Artifact index and custom registry URL](../reference/config.md#artifact-index-and-custom-registry-url).

## Step 5 — install from your source

Name the repository as the source of the `ana` scope in `~/.murmur/config.yaml`:

```yaml
registry:
  sources:
    - repo: ana/my-artifacts
```

Then install by full name. `mur install` reads `artifacts-index.json` from the latest release, so
upload it with every release:

```bash
mur install ana/tool-editor
```

```text
Installed ana/tool-editor@0.7.0 from github:ana/my-artifacts
```

The index's `scope` must be `ana`, which `mur index --repo ana/my-artifacts` writes. See
[Sources and scopes](../reference/installing-artifacts.md#sources-and-scopes).

## Summary

| Step | Command |
|---|---|
| Build | `mur build <artifact> --output release/`, once per artifact |
| Index | `mur index release --repo <owner>/<repo> --release <tag> --merge <previous index> -o release/artifacts-index.json` |
| Publish | Upload every file in `release/` as an asset of `<tag>` |
| Search | `mur search <query> --index https://github.com/<owner>/<repo>/releases/latest/download/artifacts-index.json` |
| Install | `- repo: <owner>/<repo>` under `registry.sources`, then `mur install <owner>/<name>` |
