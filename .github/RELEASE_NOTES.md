# Release Notes Guidelines

This document explains how release notes are collected, aggregated, and published for each release.

## For Contributors

### Adding a Release Note to Your PR

Every PR includes a release note in the PR body, even if it's just to indicate "no user-facing change".

**In your PR description, include:**

```release-note
<your release note here>
```

For a card-tracked PR, this block is not written by hand: the Design phase writes the note to
`.nexus/workspace/slices/<stem>-release-note.txt`, Review corrects it against what actually shipped,
and barkfactory copies it onto the card and into the PR body. A card cannot leave Design without
that file, so `NONE` is a recorded decision rather than a default. Editing the note afterwards —
in the card sidebar or the PR body — still works and still wins.

### When to write a release note

Write a release note if your PR:
- Adds a new feature or capability
- Fixes a user-facing bug
- Changes behavior that users interact with
- Includes a breaking change
- Adds or removes a CLI flag, API endpoint, or configuration option
- Improves performance in a user-visible way

**Write "NONE" only if:**
- Your change is internal refactoring with no change in behavior
- Your change is only for tests or CI/CD
- Your change is documentation-only
- Your change is a minor code cleanup with no user impact
- Your change fixes a bug no released version ever contained — see
  [When a fix is not a fix](#when-a-fix-is-not-a-fix)

Nothing outside that list is a `NONE`. **When you are torn, write the note**: a redundant line is cut
when the changelog is read before publishing, a missing one is never noticed — which is how a release
ships with unmentioned breaking changes.

Example:
```release-note
NONE
```

### Release Note Format

Keep release notes:
- **One sentence** — the whole note, with no heading, bullet, or second sentence
- **Clear and concise** — write for end users, not developers
- **Action-focused** — what can users do now that they couldn't before?
- **User-perspective** — describe the benefit, not the implementation; never name a Rust
  identifier, struct, file path, or crate. Name the manifest field, the CLI flag, or the command
- **Prefixed for breaking changes** — start with "Breaking:" if it's a breaking change

### Breaking changes before 1.0

Murmur is pre-1.0. Breaking changes are ordinary here, expected by anyone tracking the project, and
land in most releases, often several at a time. The `Breaking:` prefix is routine labelling, not an
alarm, and **under-using it is by far the more expensive mistake**: it is the only signal a reader
gets that an upgrade costs them work, and it is what floats the note to the top of its section.

Prefix the note with `Breaking: ` when any of these is true:

- a manifest field is renamed, removed, or becomes required
- something previously permitted is now denied by default
- a CLI flag changes name, shape, or output
- an artifact has to be rebuilt to keep loading
- a default changes such that an unmodified project behaves differently

Say what breaks *and* what to do about it, in the same sentence:

```release-note
Breaking: hooks reach the network only where the capsule manifest grants it — hooks that relied on ambient access need their grants declared.
```

### Putting several PRs on one changelog line

One user-facing outcome is often built by more than one PR — a capability and
the manifest field that narrows it, a bug fixed in two places. Give each PR the
same key on the fence line:

````
```release-note key=hook-capabilities
Breaking: hooks reach the network only where the capsule manifest grants it.
```
````

They collapse into a single changelog entry carrying every link:

```markdown
- Breaking: hooks reach the network and filesystem only where the capsule manifest grants it, narrowable per artifact. ([#17](…), [#18](…))
```

The **last-merged** note wins the wording and the category, so when you reuse a
key another PR already used, describe the whole outcome as it now stands — not
the slice you added. A key is a lowercase slug (`[a-z0-9-]`); anything else
fails the check. No key means one line per PR, which is the common case.

Reach for a key less often than you would expect. A PR repairing something no
release ever shipped writes `NONE` (see below), so a nine-PR epic is usually one
note and eight opt-outs already — no grouping required. The key is for the
narrower case where two or more PRs each add a genuinely user-facing piece of
the *same* outcome.

### When a fix is not a fix

The least obvious entry on the `NONE` list. Write `NONE` for a bug in something that has not shipped
yet. A card repairing a defect an earlier
card in the same epic introduced is fixing work no released version ever contained: from a user's
seat nothing was broken and nothing was fixed. The changelog entry belongs to the card that
introduces the feature, not to the ones that got it right afterwards. Applying this is what keeps a
release's changelog at the size a reader will actually read.

### Examples

✅ Good:
```release-note
Added `--output-format` flag to CLI for JSON export support
```

```release-note
Fixed crash when processing empty input files
```

```release-note
Breaking: Configuration format changed from TOML to YAML. See migration guide in docs.
```

❌ Bad:
```release-note
Refactored internal parser logic
```

```release-note
Updated dependencies
```

## For Release Team

### Aggregating Release Notes

1. **Scan merged PRs** between two version tags
2. **Filter by release-note label** (applied automatically by CI checks)
3. **Extract release-note blocks** from PR descriptions
4. **Remove "NONE" entries** — they don't appear in changelog
5. **Group notes** by category if desired (Features, Fixes, Breaking Changes, etc.)
6. **Generate changelog** as Markdown or JSON
7. **Review and edit** for consistency and clarity
8. **Commit the edited file** as `CHANGELOG/vX.Y.Z.md` and add an index entry in `CHANGELOG.md`

The GitHub Release itself is created by `release.yml` when the tag is pushed — steps 1–7 describe
what that workflow does, not work to be repeated by hand. Do them manually only when regenerating a
changelog for a release that already exists.

### Release Note Labels

The CI system automatically applies labels to PRs:
- `release-note` — PR includes a release note (not "NONE")
- `release-note/none` — PR explicitly has no user-facing changes
- `release-note/invalid` — PR is missing or has malformed release note block

### Categories

A note is filed by the PR's own `type/*` label, which barkfactory carries over from the card — not
by guessing from the note's wording. For a grouped note, the last-merged PR's label decides:

| PR label | Section |
|---|---|
| `type/feature` | Features |
| `type/bug` | Bug Fixes |
| `type/refactor`, `type/cleanup`, `type/docs` | Other |

A `Breaking: ` prefix overrides the label and files the note at the top of **Other**. A PR carrying
no `type/*` label falls back to a keyword guess, which is why the label matters.

### Tools for Aggregation

`.github/scripts/release-notes.py` does both jobs — `validate` on a PR, `aggregate` on a release
tag. See [`scripts/README.md`](scripts/README.md) for its options.

### Changelog Format Example

```markdown
# Changelog

## Changes since vX.Y.Z

### Features
- `mur run --system-prompt` overrides a capsule's system prompt for a single run.
- `inference.max_tokens` sets the per-turn output token cap.

### Bug Fixes
- Fixed shell commands being blocked from running on Linux.
- `mur install` reports which artifacts installed and which failed instead of stopping at the first failure.

### Other
- Breaking: `mur deploy` reports `deployment_id`, not `job_id`.
- Capsule subprocesses run under a default-deny syscall allowlist rather than an allow-by-default filter.
```

Three sections, no more. Breaking changes lead Other rather than getting a section of their own —
before 1.0 they are common enough that a dedicated section stops carrying information.

## CI/CD Integration

The GitHub Actions workflows handle this as follows:

Both run `.github/scripts/release-notes.py`.

1. **On PR open/update** (`release-notes.yml`) — `release-notes.py validate`:
   - Check the release-note block and its `key=`, if any
   - Apply the matching label (`release-note`, `release-note/none`, `release-note/invalid`)
   - Comment and fail the check when the block is missing or malformed

2. **On release tag** (`release.yml`) — `release-notes.py aggregate`:
   - Resolve the PRs merged since the previous tag, and read their notes
   - Collapse notes sharing a `key=` into one entry
   - Generate `CHANGELOG/vX.Y.Z.md` and upload it as a workflow artifact for review
   - Create the GitHub Release
