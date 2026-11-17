# tool-repl-js fixture

`tool-repl-js` is default-artifacts' code mode REPL: a `wasm32-wasip2` component embedding QuickJS
that exports `murmur:tool/run@0.2.0` and imports `murmur:bus/bus@0.1.0` and
`murmur:process/process@0.2.0`. `crates/murmur-cli/tests/code_mode_e2e.rs` packs these two files
into `local/tool-repl-js` and runs it as the capsule's `code_mode.repl`. No test reads a
default-artifacts checkout.

| File | What it is |
|---|---|
| `murmur.yaml` | `tools/tool-repl-js/murmur.yaml` from default-artifacts, copied verbatim |
| `tool_repl_js.wasm` | The component, copied byte for byte from the build below. Listed frozen in `crates/capsule-runtime/wit/components.list`, so `scripts/rebuild-components.sh` skips it |

## Current build

| | |
|---|---|
| Source | default-artifacts `3eb6db473c543ef36076fffcaba607dcc5cb82ab` ("There is no REPL artifact, so a capsule cannot give its model code mode (#123)"), `tool-repl-js` 0.1.0 |
| Built | in a detached worktree of that commit at `/space/_murmur/65a3-work/da`, with no local change |
| Payload size | 963828 bytes |
| Payload sha256 | `8d869c27a9028a9d4aa5579523eb56057584e3c5618eed2a55bdc8abb3e4c38a` |
| `murmur.yaml` sha256 | `dc53bc0f458060f7db025a01349dd1a230dafdd728d7c98a08a88472467c418d` |

The build command, run from the worktree's root. It is the one `scripts/build-wasm.sh` runs for
this artifact; that script builds every artifact one cargo invocation each, so a component built
alone has the same bytes:

```bash
bash scripts/wasi-sdk.sh
cargo build -p tool-repl-js --target wasm32-wasip2 --release --locked
```

The output is `target/wasm32-wasip2/release/tool_repl_js.wasm`. The toolchain comes from that
commit's `rust-toolchain.toml` (1.95.0) and `wasi-sdk.toml` (wasi-sdk 34.0).

The payload holds the absolute paths of QuickJS's C sources, which `rquickjs-sys` compiles from
its build directory, here `/space/_murmur/65a3-work/da/target/wasm32-wasip2/release/build/rquickjs-sys-2fc5a48e7348082f/out/`.
A build in a worktree at `/space/_murmur/65a3-work/da` gives these bytes exactly, from a cold
`target/` as well as a warm one; a build anywhere else, or into another `CARGO_TARGET_DIR`, differs
throughout, because the longer or shorter paths move the component's data.

## Re-vendor

After a WIT bump of an interface the component imports (`rebuild-components.sh --check` names
`tool-repl-js` with a version this tree no longer declares, or `code_mode_e2e` fails with
`E-WIT-LEVEL-001`), or for a new `tool-repl-js` release:

1. Take the default-artifacts commit that rebuilt `tool-repl-js` against the new interfaces.
2. `git -C <default-artifacts> worktree add --detach <dir> <commit>`, then run the build command
   above in `<dir>`. The bytes follow `<dir>`'s path, so write it in the table.
3. Copy `<dir>/target/wasm32-wasip2/release/tool_repl_js.wasm` and
   `<dir>/tools/tool-repl-js/murmur.yaml` over the two files here.
4. Update the table above: the commit, the version, the payload's size and both sha256 sums.
5. `code_mode_e2e.rs` reads the version it installs from `murmur.yaml` (`repl_version`). Read the
   new `tools/tool-repl-js/README.md` for changed globals or result shapes, and change the
   programs in `code_mode_e2e.rs` and in `docs/content/how-to/use-code-mode.md` together: the test
   holds every JavaScript block on that page to one of its programs.
6. Run `cargo nextest run -p murmur-cli --features beta-all --test code_mode_e2e` and copy the
   `mur trace show` lines it reports for the batch call into the how-to's trace block if they
   changed.
7. `git worktree remove <dir>`.
