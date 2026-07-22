# WIT package versioning policy

Every `murmur:*` WIT package under `crates/capsule-runtime/wit/**` carries an
explicit `@x.y.z` version on its `package` declaration. The version is embedded
into the component-type section of any `.wasm` compiled against it, so the
interface contract an artifact was built against is readable from the binary
itself rather than inferred by probing which functions happen to be exported.

Current versions:

| Package                  | Version  |
| ------------------------ | -------- |
| `murmur:hook`            | `0.3.0`  |
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

## Two accepted `murmur:hook/lifecycle` versions

`murmur:hook`'s `0.2.0 → 0.3.0` bump changed exactly one record,
`compaction-event`, which only a compaction hook reads. Forcing every hook
artifact in the fleet to be rebuilt for that would be pure churn, so
`src/hooks.rs` resolves the lifecycle instance export by trying
`murmur:hook/lifecycle@0.3.0` first and falling back to
`murmur:hook/lifecycle@0.2.0`, recording which name matched on each
`HookInstance`.

That recorded version drives exactly one dispatch decision: `on-compaction` is
sent the 5-field `CompactionEvent` when the hook resolved at `@0.3.0` and a
hand-derived 3-field `CompactionEventV02` when it resolved at `@0.2.0`.
`TypedFunc::typed` checks a component function *structurally*, so the wrong-arity
record does not truncate — it fails the type check outright, which is what makes
this split load-bearing rather than cosmetic. Every other lifecycle record is
byte-identical between the two versions, so the single bindgen-generated type
dispatches correctly to either and no other handler is special-cased.

This is a **transitional exception scoped to two specific versions of one
package**. It is deliberately *not* a reinstatement of the general unversioned
fallback described in the next section, which stays permanently removed: a hook
exporting the bare `murmur:hook/lifecycle` name still fails to instantiate.

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
  `murmur:hook/lifecycle@0.3.0`, falling back to `@0.2.0` (see the previous
  section) and to nothing else.
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
