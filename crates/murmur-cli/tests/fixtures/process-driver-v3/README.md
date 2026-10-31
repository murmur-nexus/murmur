# process-driver-v3 fixture

One process driver built against the **retired** `murmur:driver/process@0.3.0`, so the runtime's
refusal of a driver built for the version before the current one stays testable after this and
every later bump. `@0.3.0` is the version every process driver published before `@0.4.0` was
built against, so this is the refusal an installed driver meets when `mur` upgrades.

It is never run. `tests/process_driver.rs` and `tests/wit_level.rs` publish it, name it under
`inference.driver`, and assert the launch is refused with `E-WIT-LEVEL-001` naming the version
the component exports, the level this `mur` serves and its `murmur:driver` version, and a hint
that names `mur install`.

`tool/process-driver-v3.wasm` is byte-for-byte the `process-driver` fixture component as it was
committed at `@0.3.0`, copied rather than rebuilt. `src/process-driver-v3/` is that component's
source at the same commit, with only its package name and its `wit_bindgen::generate!` path
changed to point at the frozen `wit/` beside it.

`wit/process.wit` is a frozen copy of `crates/capsule-runtime/wit/process-driver/process.wit` as
it stood at `@0.3.0`. It lives here rather than under `crates/capsule-runtime/wit/` on purpose:
`scripts/check-wit-versions.sh` asserts that each package name is declared at exactly one version
across that tree, and a second copy there would be a second version of `murmur:driver`. **Do not
edit it.** A later bump adds another frozen copy beside this one; it does not change this one.

## Rebuild

`process-driver-v3` is listed frozen in `crates/capsule-runtime/wit/components.list`, so
`scripts/rebuild-components.sh` never rebuilds it. It must never be rebuilt to follow an interface
change: exporting `@0.3.0` is the one thing it exists to do.
