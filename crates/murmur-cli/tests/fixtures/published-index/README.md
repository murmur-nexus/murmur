# The published default-artifacts index

`artifacts-index.json` is the schema 2 index the default artifacts publish, copied verbatim
from a release asset. The `published_pins` unit tests in `murmur-cli` check every artifact
version `mur` or its docs tell a user to install against it: the `mur new --roster` drivers,
the `mur new <task>` generator, every `extra.v.murmur_*` pin in `docs/mkdocs.yml`, and the
`README.md` quickstart.

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

## Replacing it

When the default artifacts publish a newer release:

1. Download its `artifacts-index.json` with the command above, naming the new tag, and copy it
   here verbatim.
2. Update the table above: the tag, both URLs, the date and the sha256.
3. Run `cargo nextest run -p murmur-cli --features beta-all --bin mur published_pins`. It names
   every pin the new index does not list.
4. Repin each one it names: `ANTHROPIC`/`OPENAI` in `src/commands/new_roster.rs`,
   `GENERATOR_ARTIFACTS` in `src/commands/new.rs`, `extra.v` in `docs/mkdocs.yml` (and the
   release tag in the comment above those pins), and the `README.md` quickstart.
