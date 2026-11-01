# WIT Levels

A WIT level is one number naming an exact set of `murmur:*` WIT package versions. Each `mur`
serves one level, and an artifact loads on it only when it fits that level. The levels are listed
in [`crates/capsule-runtime/wit/levels.txt`](https://github.com/murmur-nexus/murmur/blob/main/crates/capsule-runtime/wit/levels.txt);
the tables on this page are generated from that file and the repository's release tags.

| Term | Meaning |
|---|---|
| Level | A positive integer and the version of every `murmur:*` package it lists. Levels are numbered from 1 with no gap, and a committed level never changes |
| Served level | The newest level in `levels.txt` of the `mur` you run. `mur version` prints it with its packages |
| Fit | An artifact fits a level when every `murmur:*` interface it names, exported or imported, is at exactly the version that level lists for its package |
| Built-at level | The newest level, in the building `mur`'s table, that an artifact fits |

## Compatibility table

<!-- wit-levels:tables -->

## Fit

An artifact's `murmur:*` interface names are read from its component, for example
`murmur:driver/process@0.4.0`. Its package is the text before the `/` (`murmur:driver`) and its
version the text after the `@`.

| The artifact names | Fits the level when |
|---|---|
| No `murmur:*` interface (a skill, a native tool, a component importing only `wasi:*`) | Always: it fits every level |
| A `murmur:*` interface | The level lists its package at exactly that version |
| A `murmur:*` interface with no `@version`, or of a package the level does not list | Never |

Versions compare as exact strings: `0.3.0` does not fit a level listing `0.4.0`, in either
direction.

Where fit is decided:

| Command | What it does with fit |
|---|---|
| `mur run`, `mur eval`, `mur new` | Refuse a driver that does not fit the served level with [`E-WIT-LEVEL-001`](diagnostics.md#e-wit-level-001), naming the newest version in its source's index that fits |
| `mur install <scope>/<name>` | Installs the newest version that fits the served level and has a payload for this platform. `<scope>/<name>@<version>`, `--all-platforms` and a no-argument install refuse a version that does not fit with [`E-WIT-LEVEL-001`](diagnostics.md#e-wit-level-001) before anything is fetched — see [Version selection](installing-artifacts.md#version-selection) |
| `mur search` | Lists the newest version of each artifact that fits the served level |
| `mur index` | Writes each version's built-at level |
| `mur doctor` | Lists every installed artifact with its built-at level and whether it fits ([WIT level block](cli.md#doctor-interface-versions)) |

## Built-at level

`mur build` writes the built-at level into every `.mur.zip` it packs, as the archive entry
`murmur-build.json`:

```json
{"built_at_level":1}
```

| Where | Field | Written by |
|---|---|---|
| `.mur.zip` | `murmur-build.json` → `built_at_level` | `mur build`. A payload that fits no level is packed without the entry, and the `Built artifact:` line says so |
| Installed artifact | `.meta.json` → `meta.built_at_level` | Read from the archive on every install or publish. Absent when the archive packs no `murmur-build.json` |
| Artifact index | `versions[].built_at_level` | `mur index`, from the files' `murmur-build.json`, or derived ([Artifact Index](artifact-index.md)) |

`murmur-build.json` is reserved: a `requires_files:` entry, or a file in a `mur build --skill`
input, that packs under that name is refused with [`E-BLD-002`](diagnostics.md#e-bld-002). An
archive whose `murmur-build.json` does not hold a non-negative integer `built_at_level` is refused
on install with `E-IO-003`.

### Derived labels

An artifact built before `murmur-build.json` existed carries no label. Wherever a label is needed,
it is derived by the same rule `mur build` uses: the newest level the artifact's recorded
interfaces fit, or none.

| Shown by | Recorded label | Derived label |
|---|---|---|
| `mur doctor` | `built at 1` | `built at 1 (derived)`, or `built at none (derived)` when it fits no level |
| `mur index` | `"built_at_level": 1` | `"built_at_level": 1`, or no `built_at_level` key when it fits no level |

A recorded label above the served level reads `built at <N> (newer than this mur knows)` in
`mur doctor`: the artifact was built by a newer `mur`.

## See also

- [`mur version`](cli.md#mur-version): the served level and its package table.
- [`mur doctor`](cli.md#doctor-interface-versions): installed artifacts against the served level.
- [WIT Interfaces](wit-interfaces.md#package-versioning): what each package holds and when its
  version moves.
