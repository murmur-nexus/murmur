# The published default-artifacts index

`artifacts-index.json` stands for the schema 2 index the default artifacts publish. The
`published_pins` unit tests in `murmur-cli` check every artifact version `mur` or its docs tell
a user to install against it: the `mur new --roster` drivers, the `mur new <task>` generator,
every `extra.v.murmur_*` pin in `docs/mkdocs.yml`, and the `README.md` quickstart.

## Provenance: Case B, a stand-in derived from v0.24.0

| | |
|---|---|
| Case | B: the latest release carries no `artifacts-index.json` asset |
| Release | `v0.24.0`, <https://github.com/murmur-nexus/default-artifacts/releases/tag/v0.24.0> |
| Derived | 2026-10-15 |
| sha256 of `artifacts-index.json` | `acee7b972db1f6c063396e94e20af53abed191f133f81079332ed60ae72da864` |

The file was derived mechanically from the release's 31 `.mur.zip` assets (26 artifacts):

```bash
gh release download v0.24.0 -R murmur-nexus/default-artifacts -p '*.mur.zip' -D <scratch>/assets
mur index <scratch>/assets --repo murmur-nexus/default-artifacts --release v0.24.0 \
  --output <scratch>/i.json
```

then every artifact `name` and every asset `file` lost its leading `murmur-`, the rename the
default artifacts announce (they will be served as `murmur/<name>`). Nothing else changed: the
versions, `release`, `built_at_level`, `interfaces`, `tags` and the asset `sha256`s are those
`mur index` wrote. The `sha256`s are therefore those of the flat-named v0.24.0 payloads, whose
packed `name:` still carries the `murmur-` prefix; no published file has the renamed name.

Every name and version pair here exists as published bytes in v0.24.0.

## Replacing it

Once the default artifacts publish a release carrying `artifacts-index.json`, copy that asset
here verbatim, update this README to Case A (tag, URL, sha256, date), and repin
`ANTHROPIC`/`OPENAI` in `src/commands/new_roster.rs`, `GENERATOR_ARTIFACTS` in
`src/commands/new.rs`, `extra.v` in `docs/mkdocs.yml` and the `README.md` quickstart to versions
it lists. `cargo nextest run -p murmur-cli --features beta-all --bin mur published_pins` names
every pin it does not list.
