# `process-probe-v1` — a frozen `murmur:process/process@0.1.0` tool

`tool/process-probe-v1.wasm` is byte for byte the `process-probe/tool/process-probe.wasm` that
murmur `398a9813` committed, built against `murmur:process@0.1.0` before WIT level 7 moved the
package to `0.2.0`. It is listed frozen in `crates/capsule-runtime/wit/components.list`, so
`scripts/rebuild-components.sh` never rebuilds it.

A capsule declaring it is refused at staging with `E-WIT-LEVEL-001`: it imports
`murmur:process/process@0.1.0`, and level 7 has `murmur:process@0.2.0`. The `session_processes`
test binary's `a_process_tool_built_for_level_6_is_refused` checks that refusal.
