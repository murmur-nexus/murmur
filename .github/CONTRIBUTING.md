# Contributing to Murmur

Thanks for thinking about contributing! Murmur is licensed under [Apache 2.0](../LICENSE), and we'd love your help making it better.

## Sign off your commits

Just add a sign-off to your commits:

```
git commit -s -m "your commit message"
```

That's it. It's a quick certification under the [DCO 1.1](https://developercertificate.org/) that you wrote the contribution (and have the right to submit it) and you're happy for it to go out under Apache 2.0. A bot checks for the sign-off on every PR and we'll let you know if a commit's missing one.

## Contributing on behalf of an employer

If the contribution was created as part of your job, make sure your employer has authorized it before you submit. Your sign-off is your certification of that. For substantial employer-owned contributions, reach out to a maintainer directly or on [Discord](https://discord.gg/Y45yJv5rrC) first.

## Testing your change

Every PR that changes behavior must include tests. Refactors, docs, and CI-only changes are exempt — the same logic as a `NONE` release note.

**Where tests go:**

- Unit tests live inline (`#[cfg(test)]`) next to the logic they cover.
- Anything observable through the CLI belongs in an integration suite under `crates/murmur-cli/tests/` — extend the existing suite for the area you touched, and reuse the `tests/common/` helpers and existing fixtures rather than adding new ones.

**How much:** we don't chase coverage numbers. The bar is: every behavior your release note claims should have a test proving it, and every new failure mode your change introduces should have a test hitting it. A one-line fix can be one test; a PR whose release note lists three behaviors shouldn't arrive with one. Changes touching sandboxing, capabilities, or network policy should test failure paths, not just happy paths — expect reviewers to ask for them there.

**Running tests:**

```bash
cargo test --workspace --lib --bins     # unit tests across all crates
cargo test -p murmur-cli --test build   # one integration suite (see crates/murmur-cli/tests/)
cargo test --workspace --no-fail-fast   # the full suite
```

Run the full suite with `--no-fail-fast`. Plain `cargo test` stops at the first test binary that fails and never runs the binaries after it, and its output reports the failure without saying what it skipped — `murmur-cli` alone has over a hundred integration-test binaries.

- A run that ends with `error: N targets failed:` (`error: 1 target failed:` for one) ran every binary, and the list under it is the whole set of failures.
- A run that ends with `error: test failed, to rerun pass …` and no `targets failed` list stopped at that binary and has not run the suite.

To count how many binaries a stopped run never reached, compare the `Executable` lines of a `--no-run` build with the `Running` lines of the stopped run, using the same package selection. Cargo prints its `Running` lines to standard error, so save a run with `2>&1`, for example `cargo test --workspace 2>&1 | tee stopped-run.log`:

```bash
cargo test --workspace --no-run 2>&1 | grep -c '^ *Executable'   # binaries a full run executes
grep -c '^ *Running' stopped-run.log                               # binaries the stopped run started
```

A few integration tests are marked `#[ignore]` because they depend on a sibling `default-artifacts` checkout; they only run with `cargo test -- --ignored` and are safe to skip for most changes.

## Review

Maintainers review and merge PRs at their discretion. Changes touching the sandbox, capability, or network-execution code get extra scrutiny before merge. Please bear with us while we review.

## Community

Don't be a stranger. Join us in our community server at [Discord](https://discord.gg/Y45yJv5rrC).
