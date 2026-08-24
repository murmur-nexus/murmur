# WIT package versioning policy

Every `murmur:*` WIT package under `crates/capsule-runtime/wit/**` carries an
explicit `@x.y.z` version on its `package` declaration. The version is embedded
into the component-type section of any `.wasm` compiled against it, so the
interface contract an artifact was built against is readable from the binary
itself rather than inferred by probing which functions happen to be exported.

Current versions:

| Package                  | Version  |
| ------------------------ | -------- |
| `murmur:hook`            | `0.5.0`  |
| `murmur:tool`            | `0.1.0`  |
| `murmur:capsule`         | `0.1.0`  |
| `murmur:tool-registry`   | `0.1.0`  |
| `murmur:artifact-manager`| `0.1.0`  |
| `murmur:shell`           | `0.1.0`  |
| `murmur:message`         | `0.1.0`  |
| `murmur:task`            | `0.1.0`  |
| `murmur:task-io`         | `0.1.0`  |
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
type fails with "type mismatch with results". Hooks built against `@0.3.0` or
earlier stopped resolving at this bump and had to be rebuilt.

`murmur:hook` then went to `0.5.0` when `shell-event` gained `binary: string` —
the canonicalized path of the program a shell tool actually invoked. The record
previously carried only `command` (the argument list, never the binary name), so
no hook bound to `on-shell` could tell whether an event was `pytest`, `cargo` or
`ls`. Adding a field to an existing record is always a major bump, per the rule
below; the `0.3.0 → 0.4.0` entry above records what happens if you try to ship
one additively instead. Hooks built against `@0.4.0` or earlier stopped
resolving at this bump and had to be rebuilt.

`murmur:task-io` was created at `0.1.0` for the `read` interface — `input-len`
/ `read-input` / `output-len` / `read-output`, the host-provided window onto a
task's input and result text. The interface is a hook-side import, so the rule
below classifies it as a minor bump, which would have taken `murmur:runtime` to
`0.3.0`. That was not shippable: the package version is part of *every* instance
name in the package, so `murmur:runtime/inference@0.2.0` would have become
`@0.3.0`, and the host accepts exactly one version with no fallback — every
already-published hook importing `inference` would have stopped instantiating
until rebuilt. Leaving `murmur:runtime` at `0.2.0` while adding an interface to
it would have made this table untrue instead.

**A wholly new interface added to a package that already has published
consumers goes in a new package at `0.1.0`.** No existing instance name changes,
no artifact is rebuilt, and this table simply gains a row. Bump an existing
package only when the change touches something that package already ships.

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

### Major (the breaking tier)

Every `murmur:*` package is pre-1.0, where semver makes the **minor** field the
breaking axis: a breaking change to `0.4.0` ships as `0.5.0`, not `1.0.0`. This
tier is called *major* throughout this document in the sense of "breaking", and
that is what every bump recorded above has done.

Any of:

- a signature change to an existing function (parameters or return type),
- any shape change to an existing `record` or `variant` (add/remove/reorder a
  field or case, change a member type),
- removing or renaming an existing function or interface.

A host built for the old major version cannot safely call a component built for
the new one, and vice versa.

## No compatibility fallbacks

The host accepts **exactly one version of each interface** — the version
declared in this tree — and keeps no fallback for any earlier one. There is no
compat-shim layer, no version-keyed dispatch, and no transition window.

A bump therefore requires every affected artifact to be rebuilt and
republished. An artifact built against a retired version does not silently
degrade: it fails at instantiation with an error naming the interface the host
expects and pointing the author at a rebuild. That is deliberate — a loud
failure with a known fix is preferable to a compatibility layer that accretes
one shim per bump and is never removed.

Practically, this means the cost of a bump is paid at bump time, by whoever
makes it: bump the version here, rebuild the artifacts in `default-artifacts`
(see the sync note in [README.md](README.md)), and republish. Do not reach for
a fallback to defer that work.

If you are reviewing a change that adds a version fallback of any kind — a
compat struct mirroring an old shape, a `match` on a legacy version, a
try-new-then-old resolution chain — that is the thing this policy exists to
reject.

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

- `src/hooks.rs` — `resolve_lifecycle_iface` resolves the single
  `LIFECYCLE_IFACE` name and nothing else: no earlier version, and not the bare
  unversioned name.
- `src/runtime.rs` — `resolve_versioned_iface` resolves
  `murmur:capsule/run@0.1.0` and `murmur:tool/run@0.1.0`.

The host-provided *import* interfaces (`murmur:tool-registry/invoke@0.1.0`,
`murmur:text/chunks@0.1.0`, `murmur:task/task@0.1.0`,
`murmur:runtime/inference@0.2.0`, `murmur:task-io/read@0.1.0`) are likewise
registered under the versioned name only.

An artifact that still exports (or imports) only the unversioned name now
**fails to instantiate** with a hard error naming the versioned interface the
host expected and pointing at rebuilding — `mur install` for a default artifact,
or a source rebuild otherwise. Rebuild and republish any such artifact.
