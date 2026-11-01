# Driver fixture source

These fixture WASM files are built from the `default-artifacts` repository and copied here manually.

Both copies are listed frozen in `crates/capsule-runtime/wit/components.list`, so
`scripts/rebuild-components.sh` skips them. Its `--check` reports a `murmur:*` interface either one
carries that `crates/capsule-runtime/wit/guest` no longer declares; that is the sign a WIT bump
needs the build and copy below.

## Build in `default-artifacts`

```bash
cd ~/default-artifacts
cargo build --workspace --target wasm32-wasip2 --release
```

## Current build

Both wasm files are built against WIT level 3 — `murmur:tool/run@0.2.0` and
`murmur:stream/events@0.1.0` — from default-artifacts
`66504109fd5f19635eb57355bf4111634838f858` (`origin/main` at "chore: bump to v0.22.0", the same
source the level-2 build used), in a detached worktree, with three local changes and nothing else:

1. Its vendored `wit/guest` replaced with this repository's `crates/capsule-runtime/wit/guest`
   (which drops `wit/guest/deps/murmur-text/` and adds `wit/guest/deps/murmur-stream/`).
2. Every `murmur::text::chunks::` path in the two drivers changed to `murmur::stream::events::`,
   because default-artifacts at that commit still imports the retired `murmur:text`.
3. `attachments: Vec::new()` added to the one `ToolResult` each driver builds, the field
   `tool-result` gained at level 3.

```diff
diff --git a/drivers/murmur-driver-anthropic/src/lib.rs b/drivers/murmur-driver-anthropic/src/lib.rs
index aab2b61..cde71d9 100644
--- a/drivers/murmur-driver-anthropic/src/lib.rs
+++ b/drivers/murmur-driver-anthropic/src/lib.rs
@@ -1143,6 +1143,7 @@ mod wasm_driver {
                 data_path: None,
                 truncated: false,
                 metadata: vec![],
+                attachments: Vec::new(),
             }
         }
     }
@@ -1255,8 +1256,8 @@ mod wasm_driver {
                     done = process_anthropic_sse_bytes(
                         &line_buf,
                         &mut state,
-                        &mut |chunk| murmur::text::chunks::emit_chunk(chunk),
-                        &mut |chunk| murmur::text::chunks::emit_thinking_chunk(chunk),
+                        &mut |chunk| murmur::stream::events::emit_chunk(chunk),
+                        &mut |chunk| murmur::stream::events::emit_thinking_chunk(chunk),
                     );
                     line_buf.clear();
                     if done {
@@ -1279,8 +1280,8 @@ mod wasm_driver {
                             done = process_anthropic_sse_bytes(
                                 &line_buf,
                                 &mut state,
-                                &mut |chunk| murmur::text::chunks::emit_chunk(chunk),
-                                &mut |chunk| murmur::text::chunks::emit_thinking_chunk(chunk),
+                                &mut |chunk| murmur::stream::events::emit_chunk(chunk),
+                                &mut |chunk| murmur::stream::events::emit_thinking_chunk(chunk),
                             );
                             line_buf.clear();
                             if done {
diff --git a/drivers/murmur-driver-openai/src/lib.rs b/drivers/murmur-driver-openai/src/lib.rs
index a770fcd..1c7d366 100644
--- a/drivers/murmur-driver-openai/src/lib.rs
+++ b/drivers/murmur-driver-openai/src/lib.rs
@@ -1780,6 +1780,7 @@ mod wasm_driver {
                 data_path: None,
                 truncated: false,
                 metadata,
+                attachments: Vec::new(),
             }
         }
     }
@@ -1928,8 +1929,8 @@ mod wasm_driver {
                 let line = String::from_utf8_lossy(&line_buf);
                 let line = line.trim_end_matches('\r');
                 {
-                    let mut emit_t = |t: &str| { murmur::text::chunks::emit_chunk(t); text_acc.push_str(t); };
-                    let mut emit_think = |t: &str| { murmur::text::chunks::emit_thinking_chunk(t); thinking_acc.push_str(t); };
+                    let mut emit_t = |t: &str| { murmur::stream::events::emit_chunk(t); text_acc.push_str(t); };
+                    let mut emit_think = |t: &str| { murmur::stream::events::emit_thinking_chunk(t); thinking_acc.push_str(t); };
                     done = process_openai_sse_line(
                         line,
                         &mut tool_states,
@@ -1961,8 +1962,8 @@ mod wasm_driver {
                         let line = String::from_utf8_lossy(&line_buf);
                         let line = line.trim_end_matches('\r');
                         {
-                            let mut emit_t = |t: &str| { murmur::text::chunks::emit_chunk(t); text_acc.push_str(t); };
-                            let mut emit_think = |t: &str| { murmur::text::chunks::emit_thinking_chunk(t); thinking_acc.push_str(t); };
+                            let mut emit_t = |t: &str| { murmur::stream::events::emit_chunk(t); text_acc.push_str(t); };
+                            let mut emit_think = |t: &str| { murmur::stream::events::emit_thinking_chunk(t); thinking_acc.push_str(t); };
                             done = process_openai_sse_line(
                                 line,
                                 &mut tool_states,
@@ -1987,8 +1988,8 @@ mod wasm_driver {
         drop(stream);
         let _ = wasip2::http::types::IncomingBody::finish(incoming_body);
         {
-            let mut emit_t = |t: &str| { murmur::text::chunks::emit_chunk(t); text_acc.push_str(t); };
-            let mut emit_think = |t: &str| { murmur::text::chunks::emit_thinking_chunk(t); thinking_acc.push_str(t); };
+            let mut emit_t = |t: &str| { murmur::stream::events::emit_chunk(t); text_acc.push_str(t); };
+            let mut emit_think = |t: &str| { murmur::stream::events::emit_thinking_chunk(t); thinking_acc.push_str(t); };
             thinking.flush(&mut emit_t, &mut emit_think);
         }
         assemble_openai_streaming_response(&text_acc, &thinking_acc, tool_states, stop_reason, usage)
@@ -2019,8 +2020,8 @@ mod wasm_driver {
                 let line = String::from_utf8_lossy(&line_buf);
                 let line = line.trim_end_matches('\r');
                 {
-                    let mut emit_t = |t: &str| { murmur::text::chunks::emit_chunk(t); text_acc.push_str(t); };
-                    let mut emit_think = |t: &str| { murmur::text::chunks::emit_thinking_chunk(t); thinking_acc.push_str(t); };
+                    let mut emit_t = |t: &str| { murmur::stream::events::emit_chunk(t); text_acc.push_str(t); };
+                    let mut emit_think = |t: &str| { murmur::stream::events::emit_thinking_chunk(t); thinking_acc.push_str(t); };
                     done = process_responses_sse_line(
                         line,
                         &mut tool_states,
@@ -2055,8 +2056,8 @@ mod wasm_driver {
                         let line = String::from_utf8_lossy(&line_buf);
                         let line = line.trim_end_matches('\r');
                         {
-                            let mut emit_t = |t: &str| { murmur::text::chunks::emit_chunk(t); text_acc.push_str(t); };
-                            let mut emit_think = |t: &str| { murmur::text::chunks::emit_thinking_chunk(t); thinking_acc.push_str(t); };
+                            let mut emit_t = |t: &str| { murmur::stream::events::emit_chunk(t); text_acc.push_str(t); };
+                            let mut emit_think = |t: &str| { murmur::stream::events::emit_thinking_chunk(t); thinking_acc.push_str(t); };
                             done = process_responses_sse_line(
                                 line,
                                 &mut tool_states,
```

```bash
cargo build --target wasm32-wasip2 --release -p murmur-driver-anthropic -p murmur-driver-openai
```

| File | sha256 before | sha256 now |
|---|---|---|
| `anthropic/driver/murmur-driver-anthropic.wasm` | `969af54a314e19d7afde2758a1529e7ccc5768696f084ba29cb1c3814fbca2a7` | `8ecab9e24acfc019c0d3a473fcd68a8a9f5ea7d4dc932e11158e67b565814306` |
| `openai/driver/murmur-driver-openai.wasm` | `23961e18ed470fda97b71fc0b3682bdbeb4d0b2521ee2a563dbbfc6eb3280167` | `dbe00a6346fcad101d45528241241282c5d1cc99cc6541b3bb616606e340c64b` |

The level-2 files were built from the same commit with changes 1 and 2 only; the anthropic driver
before them from default-artifacts `e0f2f62696fb9124a6002e0f6f69807e88f2b224`.

## Copy outputs into this fixture

```bash
cp ~/default-artifacts/target/wasm32-wasip2/release/murmur_driver_anthropic.wasm \
  ../anthropic/driver/murmur-driver-anthropic.wasm

cp ~/default-artifacts/target/wasm32-wasip2/release/murmur_driver_openai.wasm \
  ../openai/driver/murmur-driver-openai.wasm
```
