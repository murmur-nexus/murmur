# WIT tree layout and consumers

This directory holds several copies of the `murmur:*` WIT packages. Each copy
exists for a specific consumer; none of them is discovered by a build system,
so this file records who binds what. Versioning policy lives in
[VERSIONING.md](VERSIONING.md).

| Path | Consumed by |
| --- | --- |
| `host/` | `wasmtime::component::bindgen!({ path: "wit/host", world: "runtime-host" })` in `src/bindings.rs` — the host-side bindings for every interface the runtime provides to guests (`tool-registry`, `artifact-manager`, `shell`, `message`). |
| `hook/` | `wasmtime::component::bindgen!({ path: "wit/hook", world: "hook" })` in `src/bindings.rs` — the host-side view of the hook lifecycle contract. |
| `guest/` | `wit_bindgen::generate!` in guest components: the test fixtures under `crates/murmur-cli/tests/fixtures/*/src/*` (worlds `tool`, `capsule`) **and the out-of-repo `default-artifacts` repository**, which vendors this tree (drivers, tools, and hooks there compile against its copy — see below). |
| top-level `*.wit`, `worlds.wit`, `host.wit` | Nothing compiles these. They are the reference copies quoted by `docs/content/reference/wit-interfaces.md`. Keep them byte-identical to the bindgen copies of the same package (same version ⇒ same content, doc comments included). |

## The out-of-repo consumer

The `default-artifacts` repository (expected checked out side-by-side with this
repo) vendors this tree at its own `wit/` root and compiles its artifacts
against `wit/guest` and `wit/hook`. Syncing is **manual**; drift is detected by
`scripts/check-wit-sync.sh` in that repo (compares `guest/` and `hook/` only,
requires both repos side-by-side, not run in CI). If you change any package
under `guest/` or `hook/`, bump its version per VERSIONING.md and re-run that
script from the default-artifacts repo — artifacts there need a rebuild and
republish to pick up the change.

## Removed trees (do not resurrect)

- `wit/runtime/` — a parallel world tree nothing ever compiled against
  (bindings.rs never pointed at it). Deleted 2026-07-15.
- `murmur:plan` (`plan.wit` in every copy) — the `import murmur:plan/execute`
  was removed from the `runtime-host` world in commit `f1d34d3` (2026-05-03);
  no world has imported it since, so the package files were dead. The native
  plan module (`src/plan.rs`) is unrelated to WIT and still exists.
