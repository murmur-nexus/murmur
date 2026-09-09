# Contributing

Contributions are welcome. Start with the
[contributor guidelines](https://github.com/murmur-nexus/murmur/blob/main/.github/CONTRIBUTING.md),
which cover commit sign-off (DCO), employer contributions, and how reviews work.

Every PR needs a `release-note` block in its description (or `NONE` for changes
with no user-facing impact) — CI validates this automatically and comments on
the PR if the block is missing or malformed.

## Adding a beta feature

When a new capability is not yet ready for all users, gate it behind a Cargo feature and a
runtime flag. The full lifecycle:

```
private branch / draft PR
       ↓
Cargo feature: beta-<name>    ← compiled in, invisible by default
       ↓
mur beta enable <name>        ← user opts into the public beta
       ↓
graduate: remove is_enabled() check, remove #[cfg] guards
       ↓
(optional) remove Cargo feature if it is now core behaviour
```

### Step 1 — Add a Cargo feature

In `crates/murmur-cli/Cargo.toml`:

```toml
[features]
default = []
beta = []
beta-blueprint = ["beta"]   # ← add one line per new feature
```

### Step 2 — Register in the feature list

In `crates/murmur-cli/src/beta.rs`, add a block to `compiled_beta_features()`:

```rust
#[cfg(feature = "beta-blueprint")]
features.push(BetaFeature {
    name: "blueprint",
    description: "Blueprint file support in taskflow stage slots (preview)",
});
```

### Step 3 — Gate the code

Wrap any new commands, handlers, or registrations in `#[cfg(feature = "beta-blueprint")]`.
For runtime visibility, also check the enabled flag before registering subcommands in `main.rs`:

```rust
#[cfg(feature = "beta-blueprint")]
{
    let beta_cfg = load_mur_config().map(|c| c.beta).unwrap_or_default();
    if beta_cfg.is_enabled("blueprint") {
        // register the subcommand
    }
}
```

### Step 4 — Graduate to stable

When the feature is ready for all users:

1. Remove the `#[cfg(feature = "beta-blueprint")]` guards from `main.rs` and the command file.
2. Remove the `is_enabled("blueprint")` check — always register the subcommand.
3. Remove the entry from `compiled_beta_features()` in `beta.rs`.
4. Optionally remove the `beta-blueprint` Cargo feature (keeping it as a no-op is harmless).

## Running tests

Run the unit tests plus the integration suites for the area you touched before submitting:

```bash
cargo test --workspace --lib --bins     # unit tests across all crates
cargo test -p murmur-cli --test build   # one integration suite (see crates/murmur-cli/tests/)
```

Every PR that changes behavior must include tests — see the testing section of the
[contributor guidelines](https://github.com/murmur-nexus/murmur/blob/main/.github/CONTRIBUTING.md#testing-your-change)
for where tests go and how much coverage is expected. A few integration tests are marked
`#[ignore]` because they depend on a `default-artifacts` checkout with certain artifacts
built; set `MURMUR_DEFAULT_ARTIFACTS_DIR` to point at one, then run with
`cargo test -- --ignored`. Without that variable set, these tests skip themselves; every
other test runs without needing a `default-artifacts` checkout at all.

Read anything the sibling owns — an artifact's version, the shape of its configuration — out of the
checkout at run time rather than writing it as a literal in the test, so the test states what it
means to assert rather than a copy that goes stale on the sibling's next release. A contract the
sibling owns outright belongs in a test in that repository, where the change that breaks it is the
change that reddens it.

CI runs the full workspace suite, including both beta CLI surfaces, on every push and pull
request. Tests that need a host able to isolate a capsule — a delegated cgroup v2 scope, or a
capsule network namespace — skip themselves with a `[SKIP-HOST]`-prefixed line instead of failing,
since a CI runner provides neither; the job's step summary reports how many tests were skipped for
that reason and points at
`docs/content/reference/resource-limits-manual-verification.md`, which covers them by hand.

## Optional allocator features

A default build of `mur` uses the system allocator. Two optional features on `murmur-cli` swap in
a different one, and a third builds the tool that compares them:

| Feature | Effect |
|---|---|
| `jemalloc` | `mur` allocates through jemalloc |
| `mimalloc` | `mur` allocates through mimalloc, unless `jemalloc` is also on, which wins |
| `alloc-bench` | Builds the `mur-alloc-bench` binary, which times component compilation and the runtime's per-session allocation churn |

Every build of `mur` already needs a C compiler, because some of its dependencies ship C and
assembly. `jemalloc` adds one more tool: it configures and builds jemalloc from source, so
`--features jemalloc` and any `--all-features` build also need `make` on `PATH`.

To compare the three allocators on your own machine:

```bash
scripts/alloc-bench.sh                      # 15 rounds, ~10 minutes with a warm target directory
scripts/alloc-bench.sh --cpu 3 --rounds 21  # different core, more rounds
```

The script builds all three configurations into separate target directories, runs them one round
each per pass pinned to a single core with `taskset`, and prints a table of median component
compile time and median per-session allocation time with the per-round spread. Raw per-round
figures are left at `target/alloc-bench/rounds.tsv`. The measurement resolves differences of a few
percent, so run it on an otherwise idle machine: when the system row's own min/max spans more than
about 10%, the run is measuring the scheduler rather than the allocator.

## Formatting and lints

CI also runs a `lint` job on every push and pull request:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Run both locally before submitting a PR. `--all-features` includes the beta CLI surfaces
(`topology_cmd`, `deploy_cmd`) in the clippy pass, so a change gated behind a beta feature is
still checked; it also turns on the allocator features above. An `#[allow(...)]` is acceptable
when the lint's default judgment is wrong at that specific site, but it needs a comment saying why
— a bare `#[allow(...)]` with no justification, or a crate-level `#![allow(...)]`, will not pass
review.
