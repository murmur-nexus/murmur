# Release Guide

What a murmur release consists of, in order. This is the map, not the
keystrokes — each step names the script or workflow that does the work.

## Shape of a release

```
gates green on main  →  bump + commit  →  publish to crates.io  →  tag
                                                                    ↓
                                          CI builds, checksums, GitHub Release
                                                                    ↓
                                              changelog  →  verify  →  announce
```

The tag is the point of no return: pushing `vX.Y.Z` triggers `release.yml`,
which builds the binaries and creates the GitHub Release. Everything that could
fail should fail before it.

## 1. Gates

All of these pass on `main` before the version moves. CI enforces the first
four on every PR; the rest are release-time only.

| Gate | Command |
| --- | --- |
| Test suite | `./scripts/test.sh` — not bare `cargo test`; the beta features compile out otherwise |
| Check | `cargo check --workspace --all-targets` |
| Lint | `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings` |
| Supply chain | `cargo deny check advisories bans licenses` |
| WIT self-consistency | `./scripts/check-wit-versions.sh` |
| WIT contracts moved since the last release | `./scripts/wit-versions-changed-since.sh vX.Y.Z` — pass the *previous* release tag; exits 1 naming every `murmur:*` package whose version moved |
| Docs | `cd docs && mkdocs build --strict` |
| Host-only isolation tests | `docs/content/reference/resource-limits-manual-verification.md` — CI reports these as `[SKIP-HOST]`; run them by hand on a real Linux host for any release touching containment |
| Smoke | the maintainer-local smoke suite: real capsules against the built `mur` |

**If `wit-versions-changed-since.sh` fails, the release has work in another
repo.** Every artifact in `default-artifacts` built against a package whose
version moved must be rebuilt and republished *before* the tag is pushed. The
host keeps **no fallback**: a stale artifact fails at instantiation rather than
degrading. See `crates/capsule-runtime/wit/VERSIONING.md`. The script reports
and fails only — it never touches `default-artifacts`, which is a deliberate
act in that repo.

**Release-note labels.** Every merged PR in the range needs one; the changelog
generator reads them, and unlabelled PRs cannot be fixed after the fact without
re-running the workflow. See [RELEASE_NOTES.md](RELEASE_NOTES.md).

## 2. Bump

Three things move together in one commit:

1. `[workspace.package] version` in the root `Cargo.toml`.
2. **The pinned versions in `[workspace.dependencies]`** — `murmur-artifact` and
   `capsule-runtime` carry explicit versions that must point at what will exist
   once published. Nothing enforces this, and CI's `verify-version` will not
   catch it: that job only compares the tag against `murmur-cli`.
3. `Cargo.lock`, refreshed with `cargo check`.

## 3. Publish to crates.io

Manual, and order matters — each crate must be on the index before its
dependents resolve. Bottom-up: `murmur-artifact` → `capsule-runtime` →
`mur-roost` → `murmur-cli`, allowing ~30–60s after `capsule-runtime` for the
index to catch up. `cargo publish --dry-run` before each real publish.

Publishes are irreversible — `cargo yank` hides a version, it does not remove
it. Verify the resulting `cargo install` before tagging.

## 4. Tag

```bash
git tag vX.Y.Z
git push origin main --tags
```

`release.yml` then: verifies the tag matches `murmur-cli`'s version, builds for
`darwin-aarch64` / `darwin-x86_64` / `linux-x86_64`, computes SHA256 and SHA512,
generates the changelog via `.github/scripts/release-notes.py aggregate`, and
creates the GitHub Release with the binaries and `checksums.txt`.

## 5. Changelog

CI generates it as a build artifact; committing it is manual.

- Review the `changelog-vX.Y.Z` artifact and add anything the generator cannot
  know — upgrade notes, caches users must clear, artifacts they must reinstall.
- Commit it to `CHANGELOG/vX.Y.Z.md` **and** add its index line to
  `CHANGELOG.md` in the same commit.
- Update the changelog on murmur.nexus.

## 6. Verify as a user

Both published install paths, on a machine that is not the build host:

- **crates.io** — `cargo install murmur-cli --version X.Y.Z`
- **curl** — the published `install.murmur.rs` one-liner, which exercises tag
  resolution, platform detection, asset download and checksum verification

Check `sha256sum -c checksums.txt` against the downloaded assets, and test an
**upgrade in place** from the previous version, not just a fresh install.

See [RELEASE_WORKFLOW.md](RELEASE_WORKFLOW.md) for the technical details of the
release-notes system.
