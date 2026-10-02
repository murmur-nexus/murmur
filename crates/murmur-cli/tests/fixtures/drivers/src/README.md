# Driver fixture source

These fixture WASM files are built from the `default-artifacts` repository and copied here manually.

## Build in `default-artifacts`

```bash
cd ~/default-artifacts
cargo build --workspace --target wasm32-wasip2 --release
```

## Current build

Both wasm files are built against `murmur:stream/events@0.1.0`, from default-artifacts
`66504109fd5f19635eb57355bf4111634838f858` (`origin/main`, "chore: bump to v0.22.0") with two
local changes and nothing else, because default-artifacts itself still imports the retired
`murmur:text` there:

1. Its vendored `wit/guest` replaced with this repository's `crates/capsule-runtime/wit/guest`.
2. Every `murmur::text::chunks::` path in the two drivers changed to `murmur::stream::events::`:

```diff
diff --git a/drivers/murmur-driver-anthropic/src/lib.rs b/drivers/murmur-driver-anthropic/src/lib.rs
index aab2b61..0fb242f 100644
--- a/drivers/murmur-driver-anthropic/src/lib.rs
+++ b/drivers/murmur-driver-anthropic/src/lib.rs
@@ -1255,8 +1255,8 @@ mod wasm_driver {
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
@@ -1279,8 +1279,8 @@ mod wasm_driver {
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
index a770fcd..59fd8de 100644
--- a/drivers/murmur-driver-openai/src/lib.rs
+++ b/drivers/murmur-driver-openai/src/lib.rs
@@ -1928,8 +1928,8 @@ mod wasm_driver {
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
@@ -1961,8 +1961,8 @@ mod wasm_driver {
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
@@ -1987,8 +1987,8 @@ mod wasm_driver {
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
@@ -2019,8 +2019,8 @@ mod wasm_driver {
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
@@ -2055,8 +2055,8 @@ mod wasm_driver {
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

`cargo test -p murmur-driver-anthropic` (77 passed) and `cargo test -p murmur-driver-openai`
(95 passed) passed on that tree before the build:

```bash
cargo build --target wasm32-wasip2 --release -p murmur-driver-anthropic -p murmur-driver-openai
```

| File | sha256 before | sha256 now |
|---|---|---|
| `anthropic/driver/murmur-driver-anthropic.wasm` | `f4024dcd1a813a088fc1c3510a2c6834d6f8c0e7299c2b283edaed356b8b4c5b` | `969af54a314e19d7afde2758a1529e7ccc5768696f084ba29cb1c3814fbca2a7` |
| `openai/driver/murmur-driver-openai.wasm` | `6e0895e4dcdb52ebb38e16a54d28b2e4292d7a960fa2c66f7ee7d51a745094ef` | `23961e18ed470fda97b71fc0b3682bdbeb4d0b2521ee2a563dbbfc6eb3280167` |

The anthropic driver was previously built from default-artifacts
`e0f2f62696fb9124a6002e0f6f69807e88f2b224`.

## Copy outputs into this fixture

```bash
cp ~/default-artifacts/target/wasm32-wasip2/release/murmur_driver_anthropic.wasm \
  ../anthropic/driver/murmur-driver-anthropic.wasm

cp ~/default-artifacts/target/wasm32-wasip2/release/murmur_driver_openai.wasm \
  ../openai/driver/murmur-driver-openai.wasm
```
