# escape-conformance-probe-guest

The probe guest the escape-conformance gate's `wasm-guest` route calls. It exports
`murmur:tool/run` from the `tool` world, imports nothing but WASI, and does one thing: it returns
its input verbatim under the result metadata key `murmur-escape-probe`, with the summary
`escape-probe request not answered`.

A guest worker in a `mur` built with the test-only `escape-probe` Cargo feature answers that key
(`crates/capsule-runtime/src/guest_worker/escape_probe.rs`): it runs the probe body of the case the
input names in the worker process, under the worker's enforcement, writes the `VERDICT=`/`DETAIL=`
file into the work folder, and replaces the summary with the verdict. Any other `mur` leaves the
result as the guest returned it, which the gate's preflight reports as a `mur` built without the
feature. The feature compiles only with debug assertions, so no release carries it.

The gate embeds `escape_conformance_probe_guest.wasm` and `murmur.yaml` from this directory, packs
them with `mur build` at startup, and installs the artifact into the project of each case that
runs on the `wasm-guest` route. It is never published.

## Input

| Key | Meaning |
| --- | --- |
| `case` | The case id whose probe body the worker runs |

## Rebuild

From the repository root, with the `wasm32-wasip2` target installed:

```bash
scripts/rebuild-components.sh escape-conformance-probe-guest
```

Then rebuild `escape-conformance`, which embeds the component.
