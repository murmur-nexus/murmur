# Registry

Murmur Nexus is the artifact registry layer.

Its responsibility is simple: store and serve versioned `.mur.zip` artifacts.

## Versioning and channels

Murmur treats versions as immutable identities. Channel pointers (for example `latest`/`stable`) are mutable aliases that resolve to concrete versions at install time.

For reproducibility, lock data records concrete resolved versions and artifact hashes — see
[Lockfile (`murmur.lock`)](../reference/manifest-schema.md#lockfile-murmurlock) for which
commands read and write it.

## Local vs remote resolution

Murmur supports:

- **Local registry mode** — resolves artifacts from the [local cache](../reference/installing-artifacts.md#local-artifact-cache) at `~/.murmur/artifacts/`. The default source for cache population is the `murmur-nexus/default-artifacts` GitHub repository.
- **Remote registry mode** — resolves artifacts from a Nexus instance over HTTP. Requires `NEXUS_API_KEY` to be set.

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
