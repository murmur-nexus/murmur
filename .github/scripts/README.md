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
4. Files each entry under **Features**, **Bug Fixes** or **Other**, by the PR's
   `type/*` label. A `Breaking:` prefix overrides the label, files under Other,
   and floats to the top of the section.
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
