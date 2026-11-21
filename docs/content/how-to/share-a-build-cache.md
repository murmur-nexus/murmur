# How to share a build cache between capsules

A capsule that builds a Rust project in a fresh clone compiles every dependency from scratch, and
on a large workspace that takes most of the run. You can build the project once on the host and
import its target folder with [`mur build-cache add`](../reference/cli.md#mur-build-cache). From
then on, every capsule whose work folder holds the same `Cargo.lock` starts with a private copy of
that target folder as `CARGO_TARGET_DIR`, and Cargo reports the dependencies `Fresh` instead of
compiling them. Each capsule writes only its own copy, so no clone can change what another reads,
and nothing is fetched, so it works with no network.

The relevant manifest options are:

| Option | Controls |
|---|---|
| [capabilities.containment](../reference/containment.md#field-containment) | Whether a command can reach the store by its path |
| [capabilities.shell.allow](../reference/manifest.md#field-capabilities) | Which programs the capsule's commands may run, by name |
| [capabilities.filesystem.workdir_exec](../reference/containment.md#field-workdir-exec) | Whether build scripts the capsule compiles may run |
| [capabilities.resources.workdir_max_bytes](../reference/resource-limits.md#host-resource-limits) | The ceiling the seed counts against with the work folder |

---

## Step 1 — put the dependency sources at one fixed path

Cargo treats a compiled dependency as up to date only when its source sits at the same absolute
path as in the build that compiled it. Vendor the dependencies once into a folder outside every
clone:

```bash
cd ~/src/app
cargo vendor --locked /srv/vendor/app > /dev/null
```

In each clone a capsule will work in, create a `.cargo/config.toml` file pointing Cargo at that
folder:

```toml
[source.crates-io]
replace-with = "vendored-sources"

[source.vendored-sources]
directory = "/srv/vendor/app"
```

A shared `CARGO_HOME` registry at a fixed path works the same way. A vendored folder inside each
clone does not: its path differs per clone, so every dependency compiles again.

## Step 2 — build the project on the host

Build in one clone, with an empty `CARGO_HOME` and the toolchain the capsules will use, into a
target folder of its own:

```bash
cd ~/src/app
CARGO_HOME=$(mktemp -d) cargo build --offline --locked --target-dir ~/build/app-target
```

Use the same Cargo command and profile the capsules will run: a `cargo check` target serves
`cargo check`, a `cargo build` target serves `cargo build`.

## Step 3 — import the target folder

```bash
mur build-cache add --lockfile ~/src/app/Cargo.lock ~/build/app-target
mur build-cache list
```

```text
added 3f1c…9a2e (2140 files, 812736410 bytes) from /home/alice/build/app-target
3f1c…9a2e  2140 files  812736410 bytes  rustc 1.0.0 (abcdef012 2026-01-01)  2026-11-20T21:00:00Z  /home/alice/build/app-target
```

The entry is keyed by the SHA-256 of that `Cargo.lock` and kept under `~/.murmur/build-cache/`.
Run `add` again after the lockfile or the toolchain changes; it replaces the entry for the same
lockfile whole. To remove an entry, delete its folder.

## Step 4 — lend the toolchain and the vendored folder

A contained capsule reads only what it is granted. In `~/.murmur/config.yaml`, lend the toolchain
and the vendored folder `read` through [`host_paths`](../reference/config.md#host-paths):

```yaml
host_paths:
  - path: /home/alice/.rustup/toolchains/stable-x86_64-unknown-linux-gnu
    access: read
  - path: /srv/vendor/app
    access: read
```

Then create the capsule's `murmur.yaml` with a containment class and the programs Cargo runs:

```yaml
capabilities:
  containment: scoped
  shell:
    allow: [bash, cargo, rustc]
```

This is enough for a project whose crates have no build scripts. A project with build scripts
or procedural macros needs more:

| Need | Grant |
|---|---|
| Linking build scripts and procedural macros | `cc`, `ld`, `as` and `ar` on `shell.allow`, and an `interpreter_runtime` entry for `cc` naming the folders it reads, as [Derived interpreter runtimes](../reference/containment.md#derived-interpreter-runtime) describes |
| Compiling large crates | A larger [`capabilities.resources.memory_bytes`](../reference/resource-limits.md#address-space-and-memory): `rustc` on a large crate needs more address space than the default |
| Running build scripts | [`capabilities.filesystem.workdir_exec: true`](../reference/containment.md#field-workdir-exec), which runs the capsule at `advisory`: a build script is a program the capsule compiled, and `scoped` and `sealed` run none. Every workspace crate with a `build.rs` compiles and runs it again in each clone, so this applies even when every dependency is seeded |
Put the toolchain's own `bin` folder first on `PATH` when you run `mur`, rather than
`~/.cargo/bin`.

## Step 5 — run a capsule on another clone

```bash
mur run --task task.md --workdir ~/clones/app-2
```

```text
build cache: seeded 2140 files, 812736410 bytes from /home/alice/.murmur/build-cache/3f1c…9a2e in 412 ms (2140 reflinked, 0 copied); CARGO_TARGET_DIR=/home/alice/.murmur/records/home/alice/clones/app-2/ses_019ed2af53da75c2aefee84ee10c34af/home/.cache/cargo-target
murmur: url localhost:52222
session: ses_019ed2af53da75c2aefee84ee10c34af
status:  ok
```

The copy is in the session's own home folder, so it never touches the work folder, and it is
removed when the session ends. A command that runs `cargo build -v --offline` prints
`Fresh <crate>` for every dependency in the entry; the clone's own crates compile, because their
sources are newer than the seed. `session_start.build_cache` in the trace records the same
[fields](../reference/observability-schemas.md#session-start-build-cache).

When the entry matches but cannot be used, the run prints
[`W-RUN-014`](../reference/diagnostics.md#w-run-014) with the reason and builds without it.

## Limits

| Limit | What it means |
|---|---|
| Exact lockfile match | A `Cargo.lock` that differs by one byte finds no entry and builds cold. No nearest match is tried |
| Same inputs as the host build | A unit is `Fresh` only with the same `rustc`, the same effective Cargo configuration and its sources at the same absolute path. A `~/.cargo/config.toml` the host build read but the capsule cannot changes the profile and rebuilds everything: lend it, or build without it |
| The store is yours to fill | Only `mur build-cache add` writes the store; nothing a capsule produces is copied into it. Whatever folder you import becomes what every later capsule on that lockfile starts from. `add` refuses a folder inside `~/.murmur` |
| Containment | At `scoped` and `sealed` no command can open the store. At `advisory` and `unconfined` nothing in the kernel stops a command that names it by path; the seed is still provided. A project with build scripts runs at `advisory`, so the store is protected only by its owner-only modes against other accounts, not against the capsule |
| Disk | The seed counts against `capabilities.resources.workdir_max_bytes` at its full size, reflinked or not. An entry that would pass the ceiling is not seeded. On btrfs and XFS the copy shares extents with the store and costs almost nothing; on other filesystems it is a full copy |
| Cargo only | The lookup reads `Cargo.lock` at the work folder's root and sets `CARGO_TARGET_DIR`; no other toolchain is cached |

## Summary

| Feature / setting | How it works |
|---|---|
| `mur build-cache add --lockfile <lock> <target>` | Imports a host-built target folder, keyed by the lockfile's SHA-256 |
| `mur build-cache list` | Shows every entry: key, files, bytes, `rustc`, when added, source |
| Matching | `Cargo.lock` at the `--workdir` root, byte for byte |
| The seed | A private copy in `<records_dir>/home/.cache/cargo-target`, named by `CARGO_TARGET_DIR` in every command, removed when the session ends |
| Reporting | One `build cache:` line on stderr, `session_start.build_cache` in the trace, `W-RUN-014` when an entry was not used |
