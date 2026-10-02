# process-driver-v2 fixture

One process driver built against the **retired** `murmur:driver/process@0.2.0`, so the runtime's
refusal of a driver built for the version before the current one stays testable after this and
every later bump. `@0.2.0` is the version every process driver published before `@0.3.0` was
built against, so this is the refusal an installed driver meets when `mur` upgrades.

It is never run. `tests/process_driver.rs` publishes it, names it under `inference.driver`, and
asserts the launch is refused with `E-RUN-029` naming both the version the host requires and the
one the component exports, and a hint that names `mur install`. Its `launch` returns an error and
its `parse` returns nothing, because nothing reaches them.

`wit/process.wit` is a frozen copy of `crates/capsule-runtime/wit/process-driver/process.wit` as
it stood at `@0.2.0`. It lives here rather than under `crates/capsule-runtime/wit/` on purpose:
`scripts/check-wit-versions.sh` asserts that each package name is declared at exactly one version
across that tree, and a second copy there would be a second version of `murmur:driver`. **Do not
edit it.** A later bump adds another frozen copy beside this one; it does not change this one.

## Rebuild

Only needed if the component has to be rebuilt for a new toolchain — never to follow an interface
change, which is the one thing this fixture must not do:

```bash
cd src/process-driver-v2
cargo build --target wasm32-wasip2 --release
cp target/wasm32-wasip2/release/process_driver_v2_fixture.wasm ../../tool/process-driver-v2.wasm
```
