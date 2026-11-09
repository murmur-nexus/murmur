# GitHub Scripts

## `release-notes.py`

One script, two subcommands, sharing one definition of what a release-note
block is. They were two scripts in two languages once, and they drifted — the
validator accepted blocks the aggregator then ignored.

```bash
pip install requests
```

`GITHUB_TOKEN` is required for `validate` (it labels and comments) and optional
for `aggregate` (raises the API rate limit).

---

### `validate` — check one PR

```bash
python .github/scripts/release-notes.py validate --pr 42
python .github/scripts/release-notes.py validate --pr 42 --no-labels   # report only
```

**Used by:** `release-notes.yml`, on every PR open / edit / push.

Reads the PR body, then:

| Block | Label | Check |
|---|---|---|
| One sentence | `release-note` | passes |
| `NONE` | `release-note/none` | passes |
| Missing, empty, under 10 chars, placeholder text, malformed `key=` | `release-note/invalid` | fails, and comments on the PR |

The exit code is the check result, so an invalid note blocks the PR.

---

### `aggregate` — render a release's changelog

```bash
python .github/scripts/release-notes.py aggregate --from <previous-tag> --to <release-tag> \
  --version <release-version> --previous-version <previous-version> \
  --file CHANGELOG/<release-tag>.md
```

**Used by:** `release.yml` on a release tag, and `generate-changelog.yml` on
demand.

1. Resolves the PR numbers merged in `from..to` from the local `git log`.
   Needs full history — `fetch-depth: 0`. An unresolvable range is fatal
   rather than silently falling back to every PR in the repo.
2. Fetches those PRs and extracts each release-note block, dropping `NONE`.
3. Collapses notes sharing a `key=` into one entry (see below).
4. Files each entry under **Security**, **Features**, **Bug Fixes** or
   **Other**, by the PR's `type/*` label; `type/security` outranks any other.
   A `Breaking:` prefix overrides the label, files under Other, and floats to
   the top of the section. Security leads the changelog when present.
5. Writes `CHANGELOG/vX.Y.Z.md` with the downloads table.

Output is a **draft**. Read it, edit it, commit it — that step is not
automated, and is where a changelog stops being a list of merges.

---

### Grouping several PRs onto one line

One user-facing outcome is often built by more than one PR. Give each the same
key:

````
```release-note key=hook-capabilities
Breaking: hooks reach the network only where the capsule manifest grants it.
```
````

They render as a single entry carrying every link:

```markdown
- Breaking: hooks reach the network and filesystem only where the capsule manifest grants it, narrowable per artifact. ([#17](…), [#18](…))
```

The **last-merged** note wins the wording and the category — its Review saw the
most of the finished thing, so write the sentence for the whole outcome as it
then stands, not for your slice. A key is a lowercase slug. No key means one
entry per PR, which is the common case.

Most multi-PR outcomes don't need a key: a PR repairing something no release
ever shipped writes `NONE`, so a nine-PR epic is usually one note and eight
opt-outs already.

---

## Where the note comes from

For a card-tracked PR nobody types the block. Design writes the sentence,
Review corrects it against the shipped diff, and barkfactory renders it into
the PR description. The board is gitignored, so the PR body is the only channel
from the board to CI.

See [`../RELEASE_NOTES.md`](../RELEASE_NOTES.md) for how to word one, when to
write `NONE`, and when to prefix `Breaking: `.

---

## `check-workflow-invocations.py`

```bash
python .github/scripts/check-workflow-invocations.py
```

**Used by:** `ci.yml`, on every PR and push to `main`.

Reads the `release-notes.py` command lines out of `.github/workflows/*.yml` and
parses each one, stopping at `parse_args` — nothing runs, so it needs no token
and no network.

It imports `release-notes.py` to build the parser, and uses the standard
library only. That is why a missing `requests` no longer kills that module at
import time: the failure now happens in `ReleaseNotes.__init__`, the first
point that actually needs the network. Keep any new third-party import in this
job's path deferred the same way, or install it here.

It exists because the workflows are the only callers of `release-notes.py`, and
its argument wiring has no other cover. `--repo` was once declared on the main
parser, which in argparse means it must precede the subcommand, while all three
workflows passed it after. Every function in the script was correct, so no test
of its parsing or grouping logic went red: `validate` failed on every PR, and
`aggregate` was broken the same way but silent, because it only runs on a
release tag.

It reads the invocations rather than restating them for the same reason — a
copy would drift from what CI actually runs and prove nothing. If the command
lines move out of `.github/workflows/`, the check fails rather than reporting
that zero invocations all passed.

---

## `check-release-targets-in-ci.py`

```bash
python .github/scripts/check-release-targets-in-ci.py [--root <repo root>]
```

**Used by:** `ci.yml`'s `release-targets-in-ci` job, on every PR and push to
`main`.

Reads the `target:` values of `release.yml`'s `build-macos` matrix and of
`ci.yml`'s `macos` matrix, and fails when the release builds an Apple target
that CI does not compile. Standard library only, so it reads the YAML subset
those two matrices are written in rather than YAML in general.

| Exit | Meaning |
|---|---|
| `0` | every release target is in CI's matrix |
| `1` | a release target is missing from CI's matrix, one line per target |
| `2` | a workflow file, the job, its matrix, or any `target:` in it cannot be found |

`build-macos` runs only on a release tag, so an Apple target missing from CI's
matrix would first be compiled by a release. Adding an Apple target means
adding it to both matrices.

---

## The `contained` job's scripts

`ci.yml`'s `contained` job runs the workspace suite on a runner it has first made into a host
that installs the `scoped` and `sealed` containment classes, and fails if it has not. These four
scripts are its steps, in the order the job runs them. Each prints what it found at every step,
so a failed run is still evidence. Each writes to `$GITHUB_STEP_SUMMARY` only when that variable
is set, and prints `::error::` annotations only when `GITHUB_ACTIONS=true`.

---

## `prepare-contained-runner.sh`

```bash
.github/scripts/prepare-contained-runner.sh
```

**Used by:** `ci.yml`'s `contained` job, step "Prepare the host for scoped and sealed".

Run from a checkout, as a user with passwordless sudo. It changes host-wide settings, so it is
meant for a disposable runner. In order:

1. Prints the kernel, the active LSMs and `kernel.apparmor_restrict_unprivileged_userns`, and
   fails if Landlock is not an active LSM.
2. Fails if sudo needs a password.
3. Grants user namespaces to the checkout's `target/{debug,release}/mur` with
   `sudo scripts/install-dev-apparmor.sh`. If that fails or loads no `mur-sealed-dev` profile, it
   sets `kernel.apparmor_restrict_unprivileged_userns=0` instead. It says which it used.
4. Installs `/etc/systemd/system/user@.service.d/10-murmur-delegate.conf` with
   `Delegate=memory pids cpu io`, restarts a user manager that was already running, and enables
   lingering, so a user manager runs for this user and delegates those controllers.
5. Waits up to 30 s for the user manager's bus, then writes `XDG_RUNTIME_DIR`,
   `DBUS_SESSION_BUS_ADDRESS` and `CONTAINED_USERNS_GRANT` (the mechanism step 3 used) to
   `$GITHUB_ENV`. Without `$GITHUB_ENV` it prints the `export` lines instead.
6. Runs `check-cgroup-delegation.sh`.

| Exit | Meaning |
|---|---|
| `0` | the host is prepared |
| `1` | a step failed; the message names the step and, for an unexpected command failure, the command |

---

## `check-cgroup-delegation.sh`

```bash
bash .github/scripts/check-cgroup-delegation.sh
```

**Used by:** `prepare-contained-runner.sh`, as its last step.

Needs no root. Finds the user manager through `XDG_RUNTIME_DIR` (default `/run/user/<uid>`),
then runs `systemd-run --user --scope -p Delegate=yes` around a shell that prints the scope's
cgroup, its parent's `cgroup.subtree_control` and its own `cgroup.controllers`, and creates and
removes a child cgroup named `tier-probe`. A capsule that spawns subprocesses needs exactly that,
or its launch is refused with `E-RUN-012`.

| Exit | Meaning |
|---|---|
| `0` | a child cgroup was created and removed |
| `1` | a step failed: no user manager bus, `systemd-run` could not create the scope, or the child cgroup could not be created or removed. The message names the step |

---

## `assert-containment-tier.py`

```bash
systemd-run --user --scope --quiet --collect -p Delegate=yes -- \
  python3 -I .github/scripts/assert-containment-tier.py --class scoped|sealed [--mur target/debug/mur]
```

**Used by:** `ci.yml`'s `contained` job, steps "Tier assertion: scoped" and "Tier assertion:
sealed", before any test is built.

Writes a probe capsule to a temporary directory, declaring the class and `shell.allow: [bash]`,
so it needs a cgroup scope and a network namespace like any subprocess-capable capsule. It
passes only when all of these hold:

| Command | Check |
|---|---|
| `mur version --json` | `commands` lists `run --explain-scope --json` at schema 2 and `doctor --json` at schema 1 |
| `mur run --manifest murmur.yaml --explain-scope --json` | `achieved_containment` is the class, `floor_met` is `true`, `landlock_abi` is not null, `cgroup_scope.status` is `delegated` |
| `mur doctor --json`, in the probe's directory | no finding has severity `error` |

`mur doctor` is what catches `E-CAP-005` (no network namespace) and `E-RUN-012` (no cgroup
scope): `--explain-scope` alone can report `scoped` with the floor met on a host that would refuse
the launch. The findings list decides; doctor's `errors` tally is printed beside it, and its exit
code is ignored.

A failing leg prints every unmet check, each quoting the report's own words: `shortfall_reason`,
`userns_grant`, `cgroup_scope.errno` and `reason`, each error finding's code and message, or the
schema mismatch. It keeps the probe directory and prints its path. It never skips. Run it inside
the scope a capsule launch gets, as above: outside one, the cgroup check fails.

With `$GITHUB_STEP_SUMMARY` set it appends one block, passed or failed:

```markdown
### Tier assertion `sealed`: passed

- class asked: sealed
- class installed: sealed (enforcement tier `mountns+pivot_root+landlock+seccomp`)
- Landlock ruleset ABI: 3
- userns grant: profile_confining
- cgroup scope: delegated
- mur doctor: ready (0 errors, 0 warnings)
- kernel: <uname -r>
- mur: 0.11.0-dev (<commit>)
```

A failed block reads `FAILED` in its heading and ends with one `- reason:` line per unmet check.

| Exit | Meaning |
|---|---|
| `0` | the class is installed |
| `1` | at least one check failed, each printed |
| `2` | bad arguments |

---

## `fail-on-host-skips.py`

```bash
python3 -I .github/scripts/fail-on-host-skips.py target/nextest/ci/junit.xml
```

**Used by:** `ci.yml`'s `contained` job, step "Fail on any test skipped for the host", under
`if: always()`.

A test that cannot isolate a capsule prints `[SKIP-HOST] <test>: <reason>` and returns, which
nextest counts as a pass. nextest's `ci` profile keeps passing tests' output in the JUnit report,
so the line is there to find. On a host prepared to install `scoped` and `sealed`, this gate makes
every such test a failure and lists it with its first `[SKIP-HOST]` line. `[SKIP]` lines, for a
local fixture or checkout the runner lacks, are counted in the summary and do not fail it.

| Exit | Meaning |
|---|---|
| `0` | no test printed `[SKIP-HOST]` |
| `1` | at least one did; each is listed with the count |
| `2` | no report at that path, or one that cannot be parsed or holds no test. A missing report is never read as zero skips |
