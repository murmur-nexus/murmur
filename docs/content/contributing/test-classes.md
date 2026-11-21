# Test classes

Every Murmur test has a **class**, which says where it can give a true result, and a **kind**,
which says what it exercises. A verification board, a capsule stage or a person can run one class
with plain nextest, and read the whole classification from one checked-in list.

## Classes

| Class | A test belongs here when | How the class is checked |
|---|---|---|
| `capsule` | It gives a true result as a command inside a contained Murmur capsule: it creates no namespace, writes no cgroup, binds no `AF_UNIX` socket file, reaches no systemd user manager, needs no network beyond loopback and no real `$HOME` | It passes, with no host-skip line, under the [capsule-shaped construction](#the-capsule-shaped-construction) |
| `isolation` | It needs kernel isolation an unprivileged user gets on a host prepared like CI's `contained` job: user, mount, network or pid namespaces, a Landlock or seccomp assertion, a delegated cgroup scope, or a real contained `mur run` launch | It runs on the prepared host, in a delegated scope |
| `host` | It needs what a prepared host does not hand an unprivileged test process: root or `sudo`, a sysctl or AppArmor change, a program installed outside the repo and the Rust toolchain (a browser, `node`), internet egress or a provider key, the operator's real home, or a property of one machine | It runs on the host |

A test that could sit in two classes goes in the higher one (`capsule` → `isolation` → `host`).
An isolation test on a host that cannot isolate prints `[SKIP-HOST]` and passes, so one placed in
`capsule` by mistake is a false green; one placed too high only costs a capsule stage a test it
could have run.

| Class | Tests | `unit` | `integration` | `e2e` |
|---|---:|---:|---:|---:|
| `capsule` | 3,426 | 3,012 | 73 | 341 |
| `isolation` | 3,520 | 1,611 | 80 | 1,829 |
| `host` | 18 | 0 | 0 | 18 |

These are the counts of `.config/test-classes.tsv`; `python3 -I scripts/test-classes.py list --json`
prints them for the list as it stands.

## Kinds

| Kind | Tests |
|---|---|
| `unit` | lib, bin and proc-macro test targets |
| `e2e` | integration-test binaries that run the shipped `mur` or `mur-roost`: the `[kinds] e2e` filterset in `.config/test-classes.toml` |
| `integration` | every other integration-test binary |

`murmur-cli` has no lib target, so its integration tests reach its code only through `mur`; all
but four of its test binaries are `e2e`.

## Where the classification lives

| File | Holds | Written by |
|---|---|---|
| `.config/nextest.toml` | the profiles `class-capsule`, `class-isolation` and `class-host`; each profile's `default-filter` is its class's rule | hand |
| `.config/test-classes.toml` | what each class needs and how it is checked, the `e2e` filterset, and the nextest configurations the list covers | hand |
| `.config/test-classes.tsv` | one row per test: `binary_id`, `test`, `class`, `kind`, `config`, `ignored` | `scripts/test-classes.py generate` only |
| `.config/e2e-baseline.json` | the [end to end coverage baseline](#end-to-end-coverage-baseline) and the [mutation sample](#mutation-sample) | `scripts/e2e-coverage.py` |

The list covers two configurations:

| `config` | nextest arguments | Why |
|---|---|---|
| `beta-all` | `--workspace --features beta-all` | CI's suite run |
| `default` | `-p murmur-cli --test new_roster` | CI's featureless step: `the_default_build_has_only_the_roster_form` is compiled only without `beta-mur-new` |

Ignored tests are listed too, with `ignored` set to `true`.

## Run one class

```bash
cargo nextest run --profile class-capsule --workspace --features beta-all
cargo nextest run --profile class-capsule -p mur-roost       # -p and -E narrow the class
cargo nextest run --profile class-host --workspace --features beta-all -E 'binary_id(murmur-cli::unconfined)'
```

Each profile inherits `ci`: two retries, the five-minute slow-timeout, and a JUnit report that keeps
passing tests' output. The report is written to `target/nextest/class-<class>/junit.xml`, beside
`ci`'s. A filterset on the command line is intersected with the profile's `default-filter`;
`--ignore-default-filter` drops it.

Run `isolation` and `host` in a delegated cgroup scope, as CI does:

```bash
systemd-run --user --scope --quiet --collect -p Delegate=yes -p TasksMax=infinity -- \
  cargo nextest run --profile class-isolation --workspace --features beta-all
```

`python3 -I scripts/test-classes.py filter <class>` prints a class's filterset, for `-E` with
another profile.

### Check a run

```bash
python3 -I scripts/test-classes.py audit target/nextest/class-capsule/junit.xml --class capsule
```

`audit` prints one line per problem and then the totals `run`, `failed`, `host-skips`,
`other-skips` and `outside-class`. It fails on:

- a failed test
- a test outside the named class
- a host-skip line: `[SKIP-HOST]`, `SKIP — PROVES NOTHING`, or a line starting `skipping`

A `[SKIP]` line marks a missing local program or fixture, such as `no python3 on PATH`; `audit`
counts it and does not fail on it.

### The capsule-shaped construction

The `capsule` class is checked by running it in a transient systemd user service shaped like a
capsule's command:

```bash
systemd-run --user --wait --pipe --collect --same-dir --quiet \
  -p RestrictNamespaces=yes -p NoNewPrivileges=yes \
  -p PrivateNetwork=yes -p PrivateTmp=yes -p RestrictAddressFamilies=~AF_UNIX \
  --setenv=PATH=$HOME/.cargo/bin:/usr/local/bin:/usr/bin:/bin \
  --setenv=CARGO_HOME=$HOME/.cargo --setenv=RUSTUP_HOME=$HOME/.rustup \
  --setenv=CARGO_TARGET_DIR=$PWD/target \
  --setenv=HOME=$SCRATCH/home --setenv=XDG_RUNTIME_DIR=$SCRATCH/xdg \
  -- /usr/bin/env -u DBUS_SESSION_BUS_ADDRESS \
  cargo nextest run --profile class-capsule --workspace --features beta-all --no-fail-fast
```

`$SCRATCH/home` and `$SCRATCH/xdg` are empty directories.

| Property | What it withholds |
|---|---|
| `RestrictNamespaces=yes`, `NoNewPrivileges=yes` | creating any namespace, as a capsule's seccomp filter does |
| `PrivateNetwork=yes` | every network but loopback |
| `PrivateTmp=yes` | the host's `/tmp` |
| `RestrictAddressFamilies=~AF_UNIX` | `AF_UNIX` sockets |
| empty `HOME` and `XDG_RUNTIME_DIR`, no `DBUS_SESSION_BUS_ADDRESS` | the operator's home and the systemd user manager |
| no `Delegate=yes` | a cgroup the test may write |

## Classify a new test binary

`check` runs in CI's `cargo-test` job and fails on a test that no class selects, or that two
classes select:

```text
unclassified: mur-roost::zz_new probe (config beta-all) is in no class; add a term selecting
binary_id(mur-roost::zz_new) to one class profile's default-filter in .config/nextest.toml, …
```

1. Run `python3 -I scripts/test-classes.py scan` and read the binary's line: the markers found in
   its source and in the `tests/common` helpers it calls, and the class they suggest.
2. Add `| binary_id(<crate>::<binary>)` to the `default-filter` of the class it needs. Split a
   large binary that mixes classes with `test(=<name>)` or `test(/<regex>/)` terms, one term in
   each class; otherwise put the whole binary in the higher class.
3. A new `murmur-cli` binary that does not run `mur`, or a `mur-roost` binary that runs
   `mur-roost`, also needs an edit to `[kinds] e2e` in `.config/test-classes.toml`.
4. For `capsule`, run the binary under the construction and `audit` its report.
5. Regenerate the list and commit it with the rule.

## Regenerate the list

```bash
python3 -I scripts/test-classes.py generate        # rewrites .config/test-classes.tsv
python3 -I scripts/test-classes.py check --strict  # exits 0 when the list matches the rules
```

`generate` refuses to write while any test is unclassified or in two classes. `check` reports rows
added, removed or reclassified since the list was written; CI does not fail on that drift, and
`--strict` does. `list [--class C] [--kind K] [--json]` reads the checked-in list, and `--live`
recomputes it from nextest.

| Exit code | `test-classes.py` and `e2e-coverage.py` |
|---|---|
| 0 | ok |
| 1 | a finding: an unclassified or doubly classified test, drift under `--strict`, an audit problem |
| 2 | unusable input: a missing or malformed TOML, TSV or JUnit file, nextest failing, an unknown class |

## End to end coverage baseline

End to end line coverage is the line coverage of workspace source compiled into `mur` and
`mur-roost`, measured while the `e2e` tests run. Test binaries' own code is not counted.

| Number | Tests run | Lines covered |
|---|---|---|
| `all_e2e` | every `e2e` test | 79,724 of 101,845 (78.28%) |
| `capsule_e2e` | the `capsule`-class `e2e` tests: the floor a capsule stage can be held to | 16,957 of 101,845 (16.65%) |

`.config/e2e-baseline.json` records both numbers with the commit, the command, the instrumented
run's pass and fail counts, each test that failed under instrumentation, and a per-crate breakdown.
On an 8-core host the instrumented build takes about 35 minutes and the test runs about 65.
To reproduce it, give the script its own target directory; an instrumented build cannot share one
with a normal build:

```bash
python3 -I scripts/e2e-coverage.py --target-dir ~/.cache/cov-target \
  --profiles ~/.cache/cov-profiles --out .config/e2e-baseline.json --keep-going \
  --recheck-target-dir target
```

The script:

1. builds the workspace with `-C instrument-coverage` for the host target only, so nothing built
   for `wasm32` is instrumented
2. runs the `capsule`-class `e2e` tests, then the `isolation` and `host` ones, each in a delegated
   scope
3. replaces the profile runtime's start-up `setenv` call in the instrumented `mur` and `mur-roost`
   with a no-op (see [Known limits](#known-limits))
4. merges the profiles with `llvm-profdata` from the `llvm-tools` rustup component
5. exports the summary with `llvm-cov export` for the `mur` and `mur-roost` objects

| Flag | Effect |
|---|---|
| `--target-dir <dir>` | the instrumented build's target directory |
| `--profiles <dir>` | where the profiles, JUnit reports and exports go |
| `--out <json>` | the `murmur.e2e-baseline/1` file to write |
| `--only <filterset>` | intersect every part with one more filterset |
| `--keep-going` | record a test that fails under instrumentation instead of exiting 1 |
| `--recheck-target-dir <dir>` | re-run each such test once on a normal build and record whether it passes |
| `--build-only`, `--report-only` | stop after the build; merge and export a previous run's profiles |

### Mutation sample

The baseline's `mutation_sample` section records a `cargo-mutants` run on one `murmur-cli` source
file, scored by that crate's `e2e` tests alone: the caught, missed, timeout and unviable counts, the
missed mutants, the command and why the file was chosen. It reports how much of the file the end to
end tests pin down; no CI job reads it.

## Known limits

| Limit | Effect |
|---|---|
| The instrumented `mur` is patched | the profile runtime sets `__LLVM_PROFILE_RT_INIT_ONCE` in every instrumented process, and `mur`'s guest workers refuse to start with any variable set, so every capsule launch would fail. `e2e-coverage.py` replaces that one `setenv` call with a no-op in the instrumented `mur` and `mur-roost`, and records it in `profile_runtime_patch` |
| Profiles from children that clear their environment or run confined | `mur`'s guest workers and capsule commands can lose `LLVM_PROFILE_FILE`, or may not write where it points; their lines count as uncovered. The script deletes the stray `default*.profraw` files such children leave and records how many in `stray_profiles` |
| Instrumentation-only failures | an instrumented build is slower, and a test with a tight deadline can fail under it; `failed_under_instrumentation` lists each with `passes_uninstrumented` |
| `race-windows` tests | the 4 tests compiled only with the `race-windows` feature, which `scripts/test.sh` turns on and CI does not, are outside the list. Their binaries' rules classify them (`isolation`) when they are built |
| Classes are checked by running them | a `capsule` test that passes in the construction without proving anything there would be missed; the construction catches failures and host-skip lines |

The `race-windows` tests:

| Binary | Test |
|---|---|
| `murmur-cli::keep_working` | `a_task_that_finishes_inside_the_deadlines_termination_still_ends_deadline_reached` |
| `murmur-cli::formation_lifeline` | `a_task_cancelled_inside_the_formations_termination_still_ends_formation_ended` |
| `murmur-cli::workdir_breach_window` | `a_breach_the_poll_never_saw_still_ends_the_session_with_the_ceiling_error` |
| `murmur-cli::workdir_breach_window` | `a_session_under_its_ceiling_still_ends_ok_with_the_poll_held` |
