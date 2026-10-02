# streaming-driver-text fixture

One http driver built against the **retired** `murmur:text@0.1.0`: it imports
`murmur:text/chunks@0.1.0`, the interface `murmur:stream/events@0.1.0` replaced. It exists only so
the runtime's refusal of such a driver stays testable. Nothing runs it.

`tool/streaming-driver.wasm` is a byte-for-byte copy of
`crates/murmur-cli/tests/fixtures/streaming-driver/tool/streaming-driver.wasm` as committed at
murmur `e8bf7c2` ("Stream a tool call's start and its size on process transport (#237)"). That
binary was built from `fixtures/streaming-driver/src` and `crates/capsule-runtime/wit/guest` as
they stood at that commit, when `wit/guest/deps/murmur-text/stream.wit` still declared
`package murmur:text@0.1.0`. Its sha256 is
`fc85b70949925dfb66e80910c7cf90a81416961c047e48b946e66fd0ec552959`.

`tests/process_driver.rs` publishes it and asserts the launch is refused with `E-RUN-029`, naming
`murmur:text/chunks@0.1.0`, `murmur:stream/events@0.1.0` and `mur install`.

**Do not rebuild it.** There is no source here: the `murmur:text` WIT it was built from is gone
from the tree, and a rebuild against the current tree would no longer import it.
