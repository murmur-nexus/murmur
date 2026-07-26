# WIT package versioning policy

Every `murmur:*` WIT package under `crates/capsule-runtime/wit/**` carries an
explicit `@x.y.z` version on its `package` declaration. The version is embedded
into the component-type section of any `.wasm` compiled against it, so the
interface contract an artifact was built against is readable from the binary
itself rather than inferred by probing which functions happen to be exported.

Current versions:

| Package                  | Version  |
| ------------------------ | -------- |
| `murmur:hook`            | `0.4.0`  |
| `murmur:tool`            | `0.1.0`  |
| `murmur:capsule`         | `0.1.0`  |
| `murmur:tool-registry`   | `0.1.0`  |
| `murmur:artifact-manager`| `0.1.0`  |
| `murmur:shell`           | `0.1.0`  |
| `murmur:message`         | `0.1.0`  |
| `murmur:task`            | `0.1.0`  |
| `murmur:text`            | `0.1.0`  |
| `murmur:host`            | `0.1.0`  |
| `murmur:runtime`         | `0.2.0`  |
| `murmur:runtime-guest`   | `0.1.0`  |

`murmur:hook` started at `0.2.0` because its 9-function `lifecycle` interface
already reflected one prior additive evolution — the original 7-function baseline
plus `on-task-start`/`on-task-end`. Versioning retroactively named that step
rather than starting the package fresh at `0.1.0`.

`murmur:hook` then went to `0.3.0` when `compaction-event` gained
`model: option<string>` and `system-prompt: option<string>` — a field addition to
an existing record, which is always a major bump (see below). In the same step
`murmur:runtime` went to `0.2.0`: it gained a wholly new interface, `inference`,
whose single `run-inference` function the host provides as an *import* to any
hook that declares it (a hook that does not import it is unaffected), which is a
minor bump.

`murmur:hook` then went to `0.4.0` when `hook-output` gained a fifth case,
`reopen-task(string)` (the `on-task-end` control-return). `hook-output` is the
return type of every one of the nine `lifecycle` functions, so widening the
variant changes the wire shape of every export — a major bump. It was **not**
shippable as a purely additive no-version change: `TypedFunc::typed` is
structural and does not admit variant subtyping across the call boundary, so
lifting a pre-`reopen-task` four-case guest return against the new five-case host
type fails with "type mismatch with results" (verified empirically — see the
`v0_2_*`/`v0_3_*`/`compaction_hook_*` tests in `src/hooks.rs`, which fail under a
bare additive change and pass once the host lifts pre-`@0.4.0` hooks through the
`lifecycle_v0_3` twin). The host keeps loading `@0.3.0`- and `@0.2.0`-compiled
hooks by lifting their returns through that twin (`src/compat/lifecycle_v0_3.rs`)
rather than forcing a fleet-wide rebuild.

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

## Compatibility shims

Bumping a package's major version can require the host to keep accepting an
older interface shape for a transition period, rather than forcing every
affected artifact to be rebuilt in lockstep with the WIT change. Any such
fallback is a **compat shim**, and its code, removal condition, and inventory
row live under the compat-shim policy — see `COMPAT_SHIMS.md` at the repo root
for the full, current list (e.g. the `murmur:hook/lifecycle@0.2.0` shim that
`src/compat/lifecycle_v0_2.rs` provides after the `0.2.0 → 0.3.0` bump, and the
`lifecycle_v0_3` shim that lifts pre-`@0.4.0` four-case `hook-output` returns
after the `0.3.0 → 0.4.0` bump).

This doc stays the place to record *why a version number changed*; the shim
table is the place to record *what backward-compat code exists because of it,
and when it can be deleted*. Don't re-describe an active shim's mechanics here
— point at its row instead.

A compat shim is deliberately narrower than the general unversioned-name
fallback described in the next section, which stays permanently removed: a
hook exporting only the bare `murmur:hook/lifecycle` name still fails to
instantiate, shim or no shim.

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
  `murmur:hook/lifecycle@0.3.0`, falling back to `@0.2.0` via the
  `lifecycle-v0_2` compat shim (`COMPAT_SHIMS.md`) and to nothing else.
- `src/runtime.rs` — `resolve_versioned_iface` resolves
  `murmur:capsule/run@0.1.0` and `murmur:tool/run@0.1.0`.

The host-provided *import* interfaces (`murmur:tool-registry/invoke@0.1.0`,
`murmur:text/chunks@0.1.0`, `murmur:task/task@0.1.0`,
`murmur:runtime/inference@0.2.0`) are likewise registered under the versioned
name only.

An artifact that still exports (or imports) only the unversioned name now
**fails to instantiate** with a hard error naming the versioned interface the
host expected and pointing at rebuilding — `mur install` for a default artifact,
or a source rebuild otherwise. Rebuild and republish any such artifact.
