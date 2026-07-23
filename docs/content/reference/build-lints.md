# Build Lints

`mur build` checks the file set it is about to pack before it writes a byte of zip. What it
finds is reported two ways:

- **`E-BLD-NNN` errors** stop the build. No `.mur.zip` is written — not a partial one, not an
  empty one.
- **`W-BLD-NNN` warnings** go to stderr and the build completes. They flag a packaging mistake
  that still produces a working artifact.

Both classes are about *packaging*: what ends up inside the archive, and whether the runtime
will be able to launch it. They are distinct from the [`W-SEC-NNN`](security-warnings.md)
warnings, which are about capability and enforcement posture.

| Code | Kind | Summary |
|---|---|---|
| [`E-BLD-001`](#e-bld-001) | error | `name:` is not a valid artifact identifier |
| [`E-BLD-002`](#e-bld-002) | error | A `requires_files:` entry is unsafe or collides inside the archive |
| [`E-BLD-003`](#e-bld-003) | error | The packed entry set is not a launchable wasm payload |
| [`W-BLD-001`](#w-bld-001) | warning | A declaration names an archive entry the packer already fills |
| [`W-BLD-002`](#w-bld-002) | warning | `capsule.wasm` shadows another root `*.wasm` |
| [`W-BLD-003`](#w-bld-003) | warning | A compiled artifact packages build inputs |

---

## E-BLD-001

**`invalid artifact name '<name>': <reason>`**

An artifact `name:` becomes a filename (`<name>-<version>.mur.zip`), a registry key and a
directory in the local store, so it is held to the lowest common denominator of all three:

- non-empty, at most 100 characters
- ASCII lowercase letters, digits and `-` only
- no leading or trailing `-`

Uppercase letters, spaces, `/`, `\` and `_` are rejected by the character rule. This is a
*format* check only — it reserves no prefix and no namespace. `murmur-hook-compact` and
`murmur-tool-git` are ordinary valid names; who may publish a given name is a registry
question, not a packaging one.

```yaml
name: my-tool        # ok
name: My Tool        # E-BLD-001
name: my_tool        # E-BLD-001
name: -my-tool       # E-BLD-001
```

## E-BLD-002

**`requires_files entry '<entry>' has an unsafe path: <reason>`**
**`requires_files entry '<entry>' is a symlink (<path>); declare the file it points to instead`**
**`requires_files entries '<a>' and '<b>' both pack as the archive entry '<name>'`**

Every `requires_files:` entry must be a plain relative path to a real file inside the source
directory, and the entries must not collide once they are inside the archive:

- **No absolute paths.** `/etc/hosts` joined onto the source directory *is* `/etc/hosts`.
- **No `..` components.** This is the same rule any `.mur.zip` reader applies to an entry name
  it unpacks (see `sanitize_entry_path`), applied at authoring time.
- **No symlinks.** The link is not followed: packing it would ship whatever it resolves to —
  plausibly from outside the source tree — under the declared name.
- **No two declarations claiming one archive entry.** The archive name is a rewrite of the
  declared path (`\` → `/`), so two distinct files can otherwise land in the same slot, where
  one silently overwrites the other on unpack.

A `requires_files:` entry that redundantly names `murmur.yaml` is *not* a collision — it is
dropped before packing, and reported as [`W-BLD-001`](#w-bld-001) instead.

## E-BLD-003

**`missing root .wasm file (expected capsule.wasm or one root *.wasm)`**
**`multiple root .wasm files found: <names>`**

The artifact's `runtime:`/`execution:` resolve to a **wasm** artifact, but the entry set about
to be packed does not contain exactly one payload the runtime could select. `mur build` never
compiles anything, so a `.wasm` must already exist on disk *and* be declared in
`requires_files:`.

The rule is the runtime's own payload-selection rule, and the message is the runtime's message
verbatim — a build that passes this check cannot fail payload selection at `mur run` time:

- exactly one root `*.wasm` entry → selected
- a root `capsule.wasm` → always selected, however many other root `*.wasm` entries exist
  (which is why that case is a warning, not an error — see [`W-BLD-002`](#w-bld-002))
- zero root `*.wasm` entries → `missing root .wasm file`
- two or more, none named `capsule.wasm` → `multiple root .wasm files found`

A `*.wasm` in a subdirectory is not a root entry and does not count. This check applies only to
wasm artifacts — native (`implementation: native`) and static (`runtime: skill`,
`execution: static`) artifacts are packed without it.

## W-BLD-001

**`requires_files entry '<entry>' names the reserved archive entry '<name>', which mur build already packs`**

Two root entries have a fixed meaning inside a `.mur.zip`: `murmur.yaml` is seeded by the
packer itself, and `capsule.wasm` is the payload the runtime prefers over every other root
`*.wasm`. Declaring `murmur.yaml` in `requires_files:` packs nothing extra — the entry is
deduplicated away and the artifact is byte-identical without it.

Remove the declaration.

## W-BLD-002

**`root 'capsule.wasm' is always selected as the payload, so <names> ships but never runs`**

The artifact carries a root `capsule.wasm` *and* at least one other root `*.wasm`. The build
succeeds — `capsule.wasm` makes payload selection unambiguous — but the other file is shipped
and never executed.

Keep exactly one root `*.wasm`: drop `capsule.wasm`, or rename the payload you meant to run.

## W-BLD-003

**`compiled artifact packages build inputs: <names>`**

A wasm or native artifact declares an obvious build input in `requires_files:` — `Cargo.toml`,
`Cargo.lock`, a `*.rs` file, or anything under a `target/` path. A compiled artifact ships its
payload, not the sources it was built from, so this is almost always a stray declaration.

Static artifacts (`runtime: skill`) are exempt: their files *are* their content.
