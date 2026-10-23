# A published default-artifacts index

`artifacts-index.json` is the schema 2 index the default artifacts publish, copied verbatim
from a release asset. It is test data:

| Reader | Uses it as |
|---|---|
| `default_versions` and `commands::new` unit tests in `murmur-cli` | An index the version resolver picks each default artifact's newest fitting version from |
| `tests/new_roster.rs` | The `--index` every `mur new --roster` case passes, and the versions it expects the scaffold to pin |
| `tests/native_tool_confinement.rs` | A real published index |

No version `mur` writes, and no version the docs show, is read from it: `mur new` reads the
latest release's index at run time, and the docs read it at build time.

## Provenance: Case A, the asset published on v0.25.0

| | |
|---|---|
| Case | A: a verbatim copy of the release's `artifacts-index.json` asset |
| Release | `v0.25.0`, <https://github.com/murmur-nexus/default-artifacts/releases/tag/v0.25.0> |
| Asset | <https://github.com/murmur-nexus/default-artifacts/releases/download/v0.25.0/artifacts-index.json> |
| Copied | 2026-10-16 |
| sha256 of `artifacts-index.json` | `414d30013b3e3c9f3f02370699b53cc2adce211fc29b78a69b49c41934fc6f52` |

It was fetched into an empty directory and copied over this file unchanged:

```bash
gh release download v0.25.0 -R murmur-nexus/default-artifacts -p artifacts-index.json -D <empty dir>
sha256sum <empty dir>/artifacts-index.json
```

## Replacing the copy

1. Download a release's `artifacts-index.json` into an empty directory with the command above,
   naming its tag, and copy it here unchanged.
2. Update the table above: the tag, both URLs, the date and the sha256.
3. Update the versions the tests above expect where the new copy lists other ones.
