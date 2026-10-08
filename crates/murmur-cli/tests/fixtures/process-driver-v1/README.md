# process-driver-v1 fixture

One process driver built against the **retired** `murmur:driver/process@0.1.0`, so the runtime's
refusal of a driver built for a version of the interface it no longer accepts stays testable after
this and every later bump.

It is never run. `tests/process_driver.rs` publishes it, names it under `inference.driver`, and
asserts the launch is refused with `E-RUN-029` naming both the version the host requires and the
one the component exports. Its `launch` returns an error and its `parse` returns nothing, because
nothing reaches them.

`wit/process.wit` is a frozen copy of `crates/capsule-runtime/wit/process-driver/process.wit` as
it stood at `@0.1.0`. It lives here rather than under `crates/capsule-runtime/wit/` on purpose:
`scripts/check-wit-versions.sh` asserts that each package name is declared at exactly one version
across that tree, and a second copy there would be a second version of `murmur:driver`. **Do not
edit it.** A later bump adds another frozen copy beside this one; it does not change this one.

## Rebuild

`process-driver-v1` is listed frozen in `crates/capsule-runtime/wit/components.list`, so
`scripts/rebuild-components.sh` never rebuilds it. It must never be rebuilt to follow an interface
change: exporting `@0.1.0` is the one thing it exists to do.
