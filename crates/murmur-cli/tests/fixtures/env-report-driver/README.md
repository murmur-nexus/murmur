# env-report-driver fixture

A test-only inference driver for `tests/inference_gateway.rs`. It reports whether
`MURMUR_INFERENCE_API_KEY` is present (never its value) and the `MURMUR_INFERENCE_ENDPOINT` it was
handed. It then POSTs a fixed JSON body with no auth header to `<endpoint>/v1/messages`, reads the
response body to the end, and ends the task with the body's sha256, byte count, chunk count and
first-to-last-chunk gap.

The committed `tool/env-report-driver.wasm` is built from `src/env-report-driver`.

## Rebuild

From the repository root:

```bash
scripts/rebuild-components.sh env-report-driver
```
