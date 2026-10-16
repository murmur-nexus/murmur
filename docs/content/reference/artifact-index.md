# Artifact index

An artifact index, `artifacts-index.json`, lists every version an artifact source serves: for each
version, the release that holds it, its payload files with their SHA-256, and the `murmur:`
interfaces its component names. [`mur index`](cli.md#mur-index) writes it from a release's
`.mur.zip` assets, and [`mur search`](cli.md#mur-search) reads it. To publish one, see
[Publish your own artifact source](../how-to/publish-artifact-source.md).

The format is schema 2. Its JSON Schema is published beside this page:
[`artifact-index.schema.json`](artifact-index.schema.json).

## Example { #example }

An index with a WASM tool at two versions, the older one kept from an earlier release, and a native
tool with one payload per platform:

```json
{
  "schema_version": 2,
  "scope": "ana",
  "artifacts": [
    {
      "name": "tool-editor",
      "runtime": "tool",
      "description": "Read, write and patch files in the workdir.",
      "tags": [
        "tool",
        "editor"
      ],
      "versions": [
        {
          "version": "0.7.0",
          "release": "v1.1.0",
          "interfaces": [
            "murmur:tool/run@0.1.0"
          ],
          "assets": [
            {
              "file": "tool-editor-0.7.0.mur.zip",
              "sha256": "fab03fef2d0325fdb97aea496f0e871a8e66cced74c74bd1dd978a04eda4f5a3"
            }
          ]
        },
        {
          "version": "0.6.0",
          "release": "v1.0.0",
          "interfaces": [
            "murmur:tool/run@0.1.0"
          ],
          "assets": [
            {
              "file": "tool-editor-0.6.0.mur.zip",
              "sha256": "f0b47543b547a0d150b3b40b082839b81464fb0107b669e0fbadca1e29de0932"
            }
          ]
        }
      ]
    },
    {
      "name": "tool-git",
      "runtime": "tool",
      "description": "Structured git interface.",
      "tags": [
        "tool",
        "git"
      ],
      "versions": [
        {
          "version": "0.2.0",
          "release": "v1.1.0",
          "interfaces": [],
          "assets": [
            {
              "file": "tool-git-0.2.0-darwin-aarch64.mur.zip",
              "platform": "darwin-aarch64",
              "sha256": "b197bca09698e8a8ec7658f66208193a309d1675b8756a0dfc6acc368595ec7e"
            },
            {
              "file": "tool-git-0.2.0-linux-x86_64.mur.zip",
              "platform": "linux-x86_64",
              "sha256": "e9bbd88e3db8efef0597b0ad135607edca3d811d582586726dd570d2717c5a75"
            }
          ]
        }
      ]
    }
  ]
}
```

## Fields { #fields }

| Field | Type | Required | Notes |
|---|---|---:|---|
| `schema_version` | integer | yes | Always `2`. Any other value is refused with [`E-IDX-002`](diagnostics.md#e-idx-002) |
| `scope` | string | yes | The scope every artifact in the index installs under. Follows the name rule for one part of an [artifact name](../concepts/registry.md#artifact-names); never `local` |
| `artifacts` | array | yes | One entry per artifact, sorted by `name` in byte order. Names are unique |
| `artifacts[].name` | string | yes | The bare name, the `name:` packed in the artifact's `murmur.yaml` |
| `artifacts[].runtime` | string | yes | The packed `runtime:` of the newest version: `tool`, `hook`, `driver`, `skill`, or another role |
| `artifacts[].description` | string | no | The first sentence of the newest version's packed `description:`. Absent when it has none |
| `artifacts[].tags` | array of strings | yes | Search keywords. May be empty. Unique |
| `artifacts[].versions` | array | yes | At least one. Newest first by semver precedence; versions are unique |
| `versions[].version` | string | yes | The packed `version:`. A semantic version; never `latest`, `stable` or `edge` |
| `versions[].release` | string | yes | The tag of the release whose assets hold this version's files. No whitespace |
| `versions[].built_at_level` | integer | no | The [WIT level](wit-levels.md#built-at-level) the version was built at. `mur index` writes the value the version's files pack in `murmur-build.json`, else derives the newest level its `interfaces` fit, and omits the key when they fit none. `--merge` keeps a value an earlier index carries, and derives one for a kept version that has none |
| `versions[].interfaces` | array of strings | yes | Every `murmur:` interface the version's component exports or imports, across all its files. Sorted and unique; `[]` for a version with no component, such as a native tool or a skill |
| `versions[].assets` | array | yes | At least one. One entry per payload file, sorted by `file` |
| `assets[].file` | string | yes | The release asset's file name: `<name>-<version>.mur.zip`, or `<name>-<version>-<platform>.mur.zip` when `platform` is present |
| `assets[].platform` | string | no | `darwin-aarch64`, `darwin-x86_64`, `linux-aarch64` or `linux-x86_64`. Present exactly when `file` carries the platform tag |
| `assets[].sha256` | string | yes | SHA-256 of the file's bytes as 64 lowercase hexadecimal characters, the same digest `murmur.lock` pins |

A reader ignores keys this table does not list.

## How `mur index` derives each field { #derivation }

Every field comes from the `.mur.zip` files, except `scope` and `release`, which say where the
files are published.

| Field | Source |
|---|---|
| `scope` | `--repo <owner>/<repo>`: `murmur` for `murmur-nexus/default-artifacts`, otherwise the owner lowercased, the rule `mur install github:<owner>/<repo>@<tag>` uses |
| `release` | `--release <tag>` for every version in the directory. A version kept by `--merge` keeps its own |
| `name`, `version` | The packed `murmur.yaml` |
| `runtime` | The packed `runtime:` of the newest version |
| `description` | The first non-empty line of the newest version's packed `description:`, cut after the first `.`, `!` or `?` that ends the line or is followed by a space |
| `tags` | The role tags of `runtime`, then each `-`-separated word of the name except `murmur`, without repeats |
| `interfaces` | The `murmur:` names the version's components export and import. A WASM payload whose component cannot be read is refused rather than given `[]`; a native or static payload has none |
| `file`, `platform` | The file name |
| `sha256` | The file's bytes |

The role tags are:

| `runtime` | Tags |
|---|---|
| `driver` | `driver`, `inference` |
| `hook` | `hook` |
| `tool` | `tool` |
| `skill` | `skill` |
| Any other role | The role itself |

`tool-editor` gets `["tool", "editor"]`, and `murmur-driver-anthropic` gets
`["driver", "inference", "anthropic"]`.

The output depends only on the files' names and bytes and the two flags, so indexing the same
files twice gives byte-identical files.

## Which version fits { #fit }

`mur search` lists each artifact at the newest version that fits the `mur` running it. A version
fits when both hold:

1. Every name in `interfaces` is an interface this `mur` serves at exactly that version. The
   versions a `mur` serves are listed under [WIT interfaces](wit-interfaces.md).
2. One of its `assets` has no `platform`, or has this host's platform.

When the newest version does not fit, `mur search` adds a line under its table naming that version
and the reason.
