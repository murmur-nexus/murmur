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

CI runs the full workspace suite, including both beta CLI surfaces, on every push and pull
request. Tests that need a delegated cgroup v2 scope skip themselves with a
`[SKIP-CGROUP]`-prefixed line instead of failing, since a CI runner cannot provide one; the job's
step summary reports how many tests were skipped for that reason and points at
`docs/content/reference/resource-limits-manual-verification.md`, which covers them by hand.

## Formatting and lints

CI also runs a `lint` job on every push and pull request:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Run both locally before submitting a PR. `--all-features` includes the beta CLI surfaces
(`topology_cmd`, `deploy_cmd`) in the clippy pass, so a change gated behind a beta feature is
still checked. An `#[allow(...)]` is acceptable when the lint's default judgment is wrong at that
specific site, but it needs a comment saying why — a bare `#[allow(...)]` with no justification,
or a crate-level `#![allow(...)]`, will not pass review.
