# Graduation fixture source crates

| Crate | Committed component | Name to rebuild it by |
| --- | --- | --- |
| `jsonl-line-count` | `../tool/jsonl-line-count.wasm` | `jsonl-line-count` |
| `graduation-capsule` | `../capsule/capsule.wasm` | `graduation-capsule` |

Rebuild both from the repository root:

```bash
scripts/rebuild-components.sh jsonl-line-count
scripts/rebuild-components.sh graduation-capsule
```
