# Isolation tests on a board

Some of Murmur's tests prove isolation: a Landlock test checks that a scoped command cannot move a
file out of its workdir, and a guest-worker test checks that the real `mur` runs its WASM workers
under the session's seccomp filter. Each needs a host that can create user and network namespaces,
install a Landlock ruleset and delegate a cgroup scope. On a host that cannot, the test prints a
host-skip line and returns, and nextest counts it as a pass:

| Host-skip line | Printed by |
|---|---|
| `[SKIP-HOST] <test>: <reason>` | the `capsule_runtime` host checks and murmur-cli's test helpers |
| `skipping <test>: <reason>` | six `capsule-runtime` sandbox tests, the `kernel_tier_*` move, link and truncate tests among them |

This page covers how a Transit board proves those tests through a card, with per-test results in
which a skip is never a pass.

## Where each kind of test runs

A board's capsule stage runs the card's tests inside a contained capsule. Every isolation test
there takes its host-skip path: a capsule command's seccomp filter refuses `unshare`, `setns`,
`mount`, `pivot_root`, `chroot` and `seccomp`, and no manifest field loosens that list
(`SECCOMP_MUST_STAY_DENIED` in `crates/capsule-runtime/src/sandbox.rs`). Nested containment is
therefore not a route: a capsule cannot give a test the namespaces the capsule itself is denied.

| Placement | What it needs | What it can prove |
|---|---|---|
| Contained capsule (a board's capsule stage) | nothing beyond the board | every test that needs no namespace of its own. Each isolation test passes with a host-skip line, which proves nothing about isolation |
| Unconfined capsule (`mur run --unconfined`, or `containment: unconfined`) | the operator gives up the capsule's boundary for the whole session | the isolation tests, since nothing sits between the commands and the host. The model's commands then run with the operator's `HOME`, credentials and `sudo` |
| Host runner (`scripts/isolation-proofs.py run`, in a board's script stage) | the host grants below | the isolation tests, run as the operator outside any sandbox, with evidence a judging stage checks |

The reference board uses the host runner. It runs only the named test sets, not a model's
commands, and the judging stage reads its evidence rather than a capsule's report.

## What the host needs

The host runner refuses to start unless the host reaches the class the selected sets declare. Its
preflight, `.github/scripts/assert-containment-tier.py`, asks a freshly built `mur` for that class.

- **Landlock at ABI 2 or later.** The `landlock` set needs rename and link refusal, which ABI 2
  adds; its truncate test needs ABI 3.
- **User namespaces for `mur` and for the test binaries.** The `mur-sealed` AppArmor profile names
  only the installed `mur`, and the test binaries create namespaces themselves, so a host with
  `kernel.apparmor_restrict_unprivileged_userns=1` needs it set to `0` for the run.
- **A systemd user manager that delegates a cgroup scope.** The runner wraps the build, the
  preflight and each test run in `systemd-run --user --scope -p Delegate=yes`.
- **`cargo-nextest`**, for the `isolation` profile in `.config/nextest.toml`.

## What each looser grant exposes

Each placement that proves isolation tests trades something away. Read this before you choose one.

| Grant | What it exposes | How to limit it |
|---|---|---|
| The unconfined stance | the model's commands get the operator's `HOME`, every credential variable, the host filesystem and network, and `sudo`; see [What running unconfined gives up](../how-to/run-capsule-on-the-host.md#what-running-unconfined-gives-up) | use it only for a session you would run by hand |
| `kernel.apparmor_restrict_unprivileged_userns=0`, host-wide | every process on the host can create user namespaces, which is the kernel attack surface the restriction exists to close | prefer a dedicated host or VM; set it back to `1` after the run |
| The host runner | card-authored test code runs as the operator, outside any sandbox; it can read the operator's files, reach the network, and change any file the operator can, the board's trusted copies and evidence included | run the board as a dedicated user that owns nothing else, where you can |

The host runner limits what a card controls:

- It refuses to run as root.
- It refuses to run inside a sandbox: a process with `no_new_privs` or a seccomp filter would prove
  the outer sandbox, not the code.
- It passes cargo, and so every test, an environment cut down to an allowlist, and prints the names
  it dropped.
- A board runs trusted copies of the runner and its inputs, never the card's.
- It writes the evidence outside the card's clone.

## Why the runner and its inputs come from a trusted tree

The card's clone holds a runner, a classification, a nextest config and a preflight script, and a
card can change all four. A card that edits the runner can make it report a pass; one that edits
the classification can drop a failing test from its set, and one that edits the preflight can
accept a host that cannot isolate. The reference board's `install.sh` therefore copies those four
files from a reviewed tree into the board, and its stages pass the copies with `--classification`,
`--nextest-config` and `--preflight-script`. Re-run `install.sh` when a reviewed change to any of
them lands.

## The classification

`.config/isolation-tests.toml` names the isolation sets. Each `[[set]]` gives:

| Field | Value |
|---|---|
| `name` | unique, `[a-z0-9-]+` |
| `package` | the cargo package |
| `target` | `lib`, `test:<name>` or `bin:<name>` |
| `filter` | `test(=<test path>)` or `test(/<regex>/)` terms joined by the pipe character; any other nextest filter is refused |
| `needs` | from `landlock`, `user-namespace`, `network-namespace`, `mount-namespace`, `pid-namespace`, `cgroup-delegation`, `seccomp`, `wasmtime` |
| `class` | the class the host must reach: `scoped` or `sealed` |
| `why` | one sentence |
| `features` | optional; passed as `--features` |

`python3 -I scripts/isolation-proofs.py list` prints the sets; `--expand` builds the binaries and
adds each set's tests. The file is not an exhaustive list: `handover` reports any other test that
printed a host-skip line as `unclassified`. Add a test to a set only after you have seen it run,
not skip, on a prepared host, or every run of that set is no evidence.

## Wiring a board

A board that already runs the suite in a capsule stage learns which tests to send to the host from
that stage's nextest JUnit report:

```bash
python3 -I scripts/isolation-proofs.py handover target/nextest/ci/junit.xml --out handover.json
```

The report must keep passing tests' output (`store-success-output = true`), or the host-skip lines
are not in it.

A host script stage then runs the sets, or the handed-over tests, from the trusted copies, with the
evidence outside the clone:

```bash
python3 -I "$T/trusted/isolation-proofs.py" run \
  --repo "$workspace" \
  --set landlock --set wasm-guest-workers \
  --evidence "$T/evidence/$card_id.json" \
  --classification "$T/trusted/isolation-tests.toml" \
  --nextest-config "$T/trusted/nextest.toml" \
  --preflight-script "$T/trusted/assert-containment-tier.py"
```

Use `--handover handover.json` in place of the `--set` options to run what a capsule stage handed
over. The stage should succeed whenever evidence was written (exit 0, 1 or 3), so a failing test
reaches the judging stage.

The judging stage recomputes the clone's commit and tree and fails the card on anything but a pass:

```bash
python3 -I "$T/trusted/isolation-proofs.py" judge "$T/evidence/$card_id.json" \
  --repo "$workspace" \
  --classification "$T/trusted/isolation-tests.toml" \
  --require-set landlock --require-set wasm-guest-workers
```

### The reference board

`scripts/isolation-board/` is a Transit preset made only of script stages:

| Stage | Runs | Attaches |
|---|---|---|
| Intake | nothing; a person moves a card to Isolate | |
| Isolate | `run` for both sets, one card at a time | `isolation-evidence` |
| Judge | `judge`, requiring both sets; exits with the judge's status | `isolation-verdict`, on a pass |
| Done | nothing | |

Install it into a project and check it:

```bash
bash scripts/isolation-board/install.sh <project> [<trusted tree>]
nexus transit doctor --project <project>
```

Set `ISOLATION_TARGET_DIR` in the environment Transit runs in to choose the cargo target folder;
the default is `~/.cache/murmur-targets/isolation-board`. Transit attaches a script's deliverables
only when it exits 0, so a failing verdict is kept in `.nexus/transit/evidence/<card>.verdict.txt`
and in the card's log, and `nexus transit why <card>` shows its first failing line.

## Exit codes

| Command | 0 | 1 | 2 | 3 |
|---|---|---|---|---|
| `run` | every test passed | a test failed | refused; no evidence file exists | no test failed, but one was skipped for the host or never ran |
| `judge` | pass | a failure, a test with no evidence, or a missing required set or test | the evidence is unusable: wrong format, another commit, a changed tree, or a JUnit report whose hash no longer matches | |
| `list`, `handover` | done | | refused: a malformed classification or report | |

`run` refuses as root, inside a sandbox, without `cargo-nextest`, for an unknown or empty
selection, outside a git repository, and when the preflight says the host does not reach the
class. It removes any file at `--evidence` before it checks anything, so a refused run never leaves
old evidence behind.

## The evidence

`run` writes a `murmur.isolation-evidence/1` JSON file and copies each nextest JUnit report beside
it as `<evidence stem>.junit-<n>.xml`.

| Field | Holds |
|---|---|
| `commit`, `tree` | `git rev-parse HEAD`, and whether tracked files are unchanged (`clean`) or the sha256 of `git diff HEAD` |
| `classification`, `request` | the classification's path and sha256; the sets asked for, and the handover file if one was |
| `host` | the kernel, `systemd-detect-virt`'s answer, the uid, whether a systemd scope was used, and the preflight's class, exit code and output |
| `runs` | each nextest invocation: its command line, exit code, and the JUnit copy's path and sha256 |
| `tests` | per test: `passed`, `failed` with its first failure line, `skipped_for_host` with its host-skip line, or `not_run` |
| `totals`, `verdict` | the counts, and `pass`, `fail` or `no_evidence` |

A failure in any attempt makes a test `failed`; the `isolation` profile and the runner both set
retries to 0. A failed test makes the verdict `fail`. Otherwise any test skipped for the host or
never run makes it `no_evidence`.
