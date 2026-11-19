# How to gate a runtime change on eval results

A change to a capsule's harness or runtime — a new skill, a hook, a system prompt, a Murmur
upgrade — should be kept only when the work it produces passes more often or costs less per
pass. This guide sets up a fair comparison between the current capsule and the changed one, shows
how to read it, and turns it into a gate a script or CI job can act on with
`mur eval diff --run --require`. Murmur ships no thresholds: every number in this guide is an
example, and the gate applies only the thresholds you write.

??? definition "What is a strict pass?"
    A **strict pass** is a run whose `check` command exited `0` after the session ended. A run
    whose check failed, timed out, errored or never ran is not one, whatever its scorers said.

The relevant manifest options are:

| Option | Controls |
|---|---|
| [inference.model](../reference/manifest.md#field-inference) | The model both arms must share |
| [capabilities.shell.allow](../reference/manifest.md#field-capabilities) | The binaries a case's check may run |
| [inference.max_session_tokens](../reference/manifest.md#field-inference) | The session spend ceiling, counted in the same tokens as `spend` |

---

## Step 1 — build the variant as a copy of the baseline

Copy the baseline capsule's folder and make exactly one change in the copy:

```bash
cp -r base variant
$EDITOR variant/murmur.yaml
```

Both arms then run from identical folders, with the same artifacts, the same model and the same
grants, so a difference in the results comes from your change. `mur eval run` refuses a variant
whose `inference.model` differs from the baseline's.

## Step 2 — give every case a start and a check

Create an `eval.jsonl` file with one line per case. Each case needs a `start` folder, copied into
every session's work folder before it starts, and a `check` command, run in that folder after the
session ends:

```json
{ "case_id": "fix-parser", "task_path": "tasks/fix-parser.md", "start": "cases/parser", "check": "make test" }
{ "case_id": "fix-config", "task_path": "tasks/fix-config.md", "start": "cases/config", "check": { "command": "bash check.sh", "files": "checks/config" } }
```

A case without a check counts toward no pass and no spend per pass. The check's binary must be in
both manifests' `capabilities.shell.allow`. See [the check](../reference/cli.md#mur-eval-run-check)
for its fields.

## Step 3 — split the cases into a dev set and a held-out set

Write two dataset files:

| File | Use |
|---|---|
| `dev.jsonl` | The cases you run while you iterate on the change |
| `heldout.jsonl` | Cases you run only to gate the finished change, and never tune against |

A change tuned until it passes the cases it is judged on has learned those cases. The held-out
set is what tells you it generalizes.

## Step 4 — run three seeds of each case

Run every case three times on each arm:

```bash
mur eval run base --dataset dev.jsonl --variant variant --repeat 3
```

Each repeat starts from a fresh copy of the case's `start`, and repeats alternate which arm goes
first. One run per case is too few: the same model on the same task passes on one run and fails
on the next.

## Step 5 — read spend per pass beside the check row

`mur eval run` ends with the comparison; `mur eval diff --run --workdir base/workdir` prints it
again:

```text
── total ────────────────────────
                       baseline                 variant                  delta (variant − baseline)
  check                5/12 pass                9/12 pass                +4
  spend per pass       81240.2                  47785.6                  -33454.6
  spend                406201                   430070                   +23869
  turns                38                       31                       -7
```

Read the two rows together:

| Row | What it says |
|---|---|
| `check` | How many runs' work passed. The delta is the pass lead |
| `spend per pass` | The tokens an arm spent on every run of a checked case, divided by its strict passes: the cost of one piece of passing work |
| `spend` | The tokens an arm spent in total |

Here the variant spent more tokens in total but passed four more runs, so each pass cost it fewer
tokens. Spend is tokens as the session's spend meter counts them; multiply by your provider's
price per token for money.

Treat a lead of 3 passes or fewer as noise. Across a dozen runs, three passes either way happen
from run-to-run variation alone, so a change that leads by that little has not shown it helps.

## Step 6 — gate on the held-out set with --require

Once the change looks better on the dev set, run the held-out set once:

```bash
mur eval run base --dataset heldout.jsonl --variant variant --repeat 3
```

Then state the thresholds the change must meet. Quote each requirement, so the shell does not read
`>` or `<` as a redirection:

```bash
mur eval diff --run --workdir base/workdir \
  --require 'pass-lead>=4' \
  --require 'variant.spend-per-pass<=60000'
```

The comparison prints as before, followed by a requirements block:

```text
── requirements ────────────────────────
  met      pass-lead>=4                   pass lead 5
  met      variant.spend-per-pass<=60000  spend per pass 51022.4
```

`mur eval diff` exits `0` when every requirement is met and the run is complete, and `1` with
`E-EVAL-005` when any is not. Pick your own numbers: `60000` here stands for what one passing
change is worth to you in tokens. The [requirement grammar](../reference/cli.md#mur-eval-diff-require)
lists every measure and operator.

## Step 7 — use the exit status in a script or CI job

The exit status is the gate:

```bash
set -e
mur eval run base --dataset heldout.jsonl --variant variant --repeat 3
mur eval diff --run --workdir base/workdir --json \
  --require 'pass-lead>=4,variant.spend-per-pass<=60000' > gate.json
```

With `set -e`, the script stops at the failed gate. `gate.json` holds the comparison either way,
with a `requirements` array stating each requirement's number and `requirements_met`, so the job
can report why it failed. An interrupted run never passes the gate: its record is incomplete, and
`--require` reports that as not met.

---

## Summary

| Practice | Why |
|---|---|
| Variant copied from the baseline folder with one change | Both arms run from identical folders and the same model |
| A `start` and a `check` on every case | Only checked cases count toward passes and spend per pass |
| `--repeat 3` | One run per case is too noisy to compare |
| A lead of 3 passes or fewer is noise | Run-to-run variation produces it without any change |
| `spend per pass` read beside `check` | Weighs what the change costs against the work it gets done |
| Dev set to iterate, held-out set to gate | A change tuned on its gate cases has not shown it generalizes |
| `--require` thresholds you choose | Murmur ships none; the exit status reflects only yours |
