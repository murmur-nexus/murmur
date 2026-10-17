# records-hook fixture

A test-only `on-session-end` hook for `tests/records_dir.rs`, built against
`crates/capsule-runtime/wit/hook` `world hook`.

At `on-session-end` it writes, in its current directory:

| Path | Contents |
|---|---|
| `eval.jsonl` | One `dataset_run` line: `overall: pass`, `scores: {"records-hook": 0.75}`, and `MURMUR_DATASET_ID` / `MURMUR_CASE_ID` when the runtime set them |
| `logs/hook-records-hook.log` | `records-hook wrote eval.jsonl`, appended |

Declared `capabilities.filesystem.records: true`, its current directory is the session records
directory's `hooks/`; declared `scope: .`, the workdir. Every other event is a no-op, and every
outcome is `none`.

`murmur.yaml` is the bundled manifest the tests pack with the hook: `binding: on-session-end`,
`execution_mode: blocking`.

## Rebuild

From the repository root:

```bash
scripts/rebuild-components.sh records-hook
```
