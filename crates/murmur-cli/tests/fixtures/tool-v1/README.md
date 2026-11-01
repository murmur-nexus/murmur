# `tool-v1` — a frozen `murmur:tool/run@0.1.0` tool

`tool/jsonl-line-count-v1.wasm` is byte for byte the `graduation/tool/jsonl-line-count.wasm` that
murmur `90424144` committed, built against `murmur:tool@0.1.0` before WIT level 3 moved the package
to `0.2.0`. It is listed frozen in `crates/capsule-runtime/wit/components.list`, so
`scripts/rebuild-components.sh` never rebuilds it.

A capsule declaring it is refused at staging with `E-WIT-LEVEL-001`: it exports
`murmur:tool/run@0.1.0`, and level 3 has `murmur:tool@0.2.0`. The `images` test binary's
`old_level_tool_is_refused` checks that refusal.
