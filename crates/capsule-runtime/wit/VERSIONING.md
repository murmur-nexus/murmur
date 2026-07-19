# WIT package versioning policy

Every `murmur:*` WIT package under `crates/capsule-runtime/wit/**` carries an
explicit `@x.y.z` version on its `package` declaration. The version is embedded
into the component-type section of any `.wasm` compiled against it, so the
interface contract an artifact was built against is readable from the binary
itself rather than inferred by probing which functions happen to be exported.

Current versions:

| Package                  | Version  |
| ------------------------ | -------- |
| `murmur:hook`            | `0.2.0`  |
| `murmur:tool`            | `0.1.0`  |
| `murmur:capsule`         | `0.1.0`  |
| `murmur:tool-registry`   | `0.1.0`  |
| `murmur:artifact-manager`| `0.1.0`  |
| `murmur:shell`           | `0.1.0`  |
| `murmur:message`         | `0.1.0`  |
| `murmur:task`            | `0.1.0`  |
| `murmur:text`            | `0.1.0`  |
| `murmur:host`            | `0.1.0`  |
| `murmur:runtime`         | `0.1.0`  |
| `murmur:runtime-guest`   | `0.1.0`  |

`murmur:hook` starts at `0.2.0` because its 9-function `lifecycle` interface
already reflects one prior additive evolution — the original 7-function baseline
plus `on-task-start`/`on-task-end`. This slice retroactively names that step
rather than starting the package fresh at `0.1.0`.

## When to bump

### Patch (`x.y.Z`)

Documentation or comment-only edits with **no** shape or dispatch change: a
reworded doc comment, a clarified reserved-key note, whitespace. Nothing about
the binary ABI or the set/signature of exported functions changes.

### Minor (`x.Y.0`)

Adding a **wholly new** function, or a wholly new interface, that the host treats
as *optional to export* — generalizing the `OPTIONAL_HOOK_FNS` pattern in
`src/hooks.rs`, where a component compiled before the function existed simply
omits it and is never dispatched for it. The `on-task-start`/`on-task-end`
additions that took `murmur:hook` from its 7-function baseline to `0.2.0` are the
canonical example.

A minor bump is **never** the right level for:

- adding a field to an existing `record`,
- inserting a case into an existing `variant`,
- widening or narrowing an existing type.

The Component Model's binary ABI has no structural tolerance for compound-type
shape changes: every field/case is positional in the canonical ABI, so any such
edit is a breaking change even though it "looks additive" in the source. These
are always **major** bumps.

### Major (`X.0.0`)

Any of:

- a signature change to an existing function (parameters or return type),
- any shape change to an existing `record` or `variant` (add/remove/reorder a
  field or case, change a member type),
- removing or renaming an existing function or interface.

A host built for the old major version cannot safely call a component built for
the new one, and vice versa.

## Unversioned artifacts: fallback removed

Artifacts published before WIT package versioning exported the **unversioned**
instance names (`murmur:capsule/run`, `murmur:tool/run`,
`murmur:hook/lifecycle`). A transitional dual-accept runtime
resolved those interfaces by trying the versioned instance name first and
falling back to the bare unversioned name, so already-published artifacts kept
running unmodified during the transition window.

That fallback has been **removed**. The host now resolves the three
dynamically-instantiated guest interfaces by the **versioned** instance name
only:

- `src/hooks.rs` — `resolve_lifecycle_iface` resolves
  `murmur:hook/lifecycle@0.2.0`.
- `src/runtime.rs` — `resolve_versioned_iface` resolves
  `murmur:capsule/run@0.1.0` and `murmur:tool/run@0.1.0`.

The host-provided *import* interfaces (`murmur:tool-registry/invoke@0.1.0`,
`murmur:text/chunks@0.1.0`, `murmur:task/task@0.1.0`) are likewise registered
under the versioned name only.

An artifact that still exports (or imports) only the unversioned name now
**fails to instantiate** with a hard error naming the versioned interface the
host expected and pointing at rebuilding — `mur install` for a default artifact,
or a source rebuild otherwise. Rebuild and republish any such artifact.
