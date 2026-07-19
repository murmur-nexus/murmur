# Release Workflow Guide

Complete guide for the automated release notes and changelog generation flow.

## Architecture

```
Contributor opens PR
  ↓
PR includes release-note block
  ↓
GitHub Actions validates release note
  ↓
Applies label: release-note, release-note/none, or release-note/invalid
  ↓
PR is merged (if not invalid)
  ↓
Maintainer pushes a version tag (triggers the release workflow)
  ↓
Script queries GitHub API for PRs in version range
  ↓
Extracts and aggregates release notes
  ↓
Groups by category (Features, Fixes, Breaking Changes, etc.)
  ↓
Generates CHANGELOG/vX.Y.Z.md, uploaded as a workflow artifact
  ↓
GitHub Release is created with the binaries
  ↓
Maintainer reviews and edits the generated changelog
  ↓
Edited file is committed to CHANGELOG/ and indexed in CHANGELOG.md
```

## For Contributors: Writing Release Notes

### What to Include

Every PR needs a release note block in the description:

```release-note
<your release note or NONE>
```

### When to Write a Note

**Write a note** if the PR:
- Adds a feature
- Fixes a user-facing bug
- Changes user-visible behavior
- Includes breaking changes
- Changes CLI flags or API
- Improves performance users see

**Write "NONE"** if the PR:
- Is internal refactoring
- Only modifies tests
- Only updates docs (unless docs change is significant)
- Is a code cleanup

### Format Guidelines

- **Clear and concise** — write for end users
- **Action-focused** — describe what users can do
- **User perspective** — benefits over implementation
- **Prefix breaking changes** — start with "Breaking:"

### Examples

```release-note
Added --json output flag to CLI commands
```

```release-note
Fixed memory leak in connection pooling
```

```release-note
Breaking: Changed config format from TOML to YAML. See migration guide in docs.
```

```release-note
NONE
```

### Validation

The `Validate Release Notes` workflow automatically:
1. ✓ Checks for `release-note` block
2. ✓ Validates content isn't empty/placeholder
3. ✓ Applies appropriate label
4. ✓ Comments with help if invalid

**If your PR fails validation:**
1. Check the workflow comment on your PR (it explains what's wrong)
2. Edit the PR description to fix the `release-note` block
3. **Done!** The workflow re-runs automatically when you save the description edit

No need to push commits — just edit the PR description directly in GitHub's UI.

## For Release Team: Generating Changelog

Pushing a release tag runs changelog generation automatically as part of
`.github/workflows/release.yml`, which uploads the generated `CHANGELOG/vX.Y.Z.md`
as a workflow artifact. The manual **Generate Changelog** workflow below is for
regenerating a changelog outside a tag push.

### Step 1: Trigger the Workflow

1. Go to **Actions** tab
2. Select **Generate Changelog** workflow
3. Click **Run workflow**
4. Fill in:
   - **from_version**: Starting tag (e.g., `v1.0.0`)
   - **to_version**: Ending tag or `HEAD` (default: `HEAD`)
   - **release_version**: Version for header (e.g., `v1.1.0`)
   - **release_date**: Release date (optional, defaults to today)

### Step 2: Review Generated Changelog

1. Workflow generates changelog automatically
2. Download artifact "changelog" from workflow run
3. Review for:
   - Completeness (all user-facing changes included)
   - Accuracy (release notes are correct)
   - Clarity (wording is user-friendly)
   - Categorization (items in right sections)

### Step 3: Edit and Publish

```bash
# 1. Download the generated changelog artifact
# 2. Review and edit the wording (see Step 2 checklist)
# 3. Commit the edited file as CHANGELOG/vX.Y.Z.md
# 4. Add an entry linking to it in the CHANGELOG.md index
```

The GitHub Release itself is created automatically by the release workflow when
the tag is pushed. Only when regenerating a changelog for an existing release do
you need to update the release notes by hand (**Releases** page → edit release →
paste the markdown).

## Running Aggregation Locally

### Prerequisites

```bash
pip install requests
```

### Usage

**Basic usage:**
```bash
python .github/scripts/aggregate-release-notes.py v1.0.0 v1.1.0
```

**With options:**
```bash
python .github/scripts/aggregate-release-notes.py \
  --from v1.0.0 \
  --to v1.1.0 \
  --version v1.1.0 \
  --date 2026-03-15 \
  --output markdown \
  --file CHANGELOG.new.md
```

**Output to file:**
```bash
python .github/scripts/aggregate-release-notes.py v1.0.0 v1.1.0 --file changelog.md
```

**JSON output:**
```bash
python .github/scripts/aggregate-release-notes.py v1.0.0 v1.1.0 --output json
```

### Options

- `from_version`: Starting ref (tag/commit)
- `to_version`: Ending ref (tag/commit)
- `--from`: Alternative to positional from_version
- `--to`: Alternative to positional to_version
- `--repo`: GitHub repo (default: `murmur-nexus/murmur`)
- `--version`: Version string for changelog header
- `--date`: Release date (YYYY-MM-DD format)
- `--output`: `markdown`, `json`, or `both`
- `--file`: Write to file instead of stdout

## Label Reference

| Label | Meaning |
|-------|---------|
| `release-note` | PR has a valid release note for the changelog |
| `release-note/none` | PR explicitly has no user-facing changes |
| `release-note/invalid` | PR has missing or invalid release note (blocks merge) |

## Troubleshooting

### Workflow Fails: "No commits found"

**Cause:** The git refs don't exist or branch isn't up to date

**Fix:**
```bash
git fetch origin
git tag -l  # List available tags
```

### Workflow Fails: "Release note block not found"

**Cause:** PR body doesn't have properly formatted release note

**Format check:**
```
```release-note
Your note here
```
```

The triple backticks and `release-note` keyword are required.

### Changelog is incomplete

**Possible causes:**
- PR doesn't have release-note block (check labels)
- Release note extraction failed (validation comment has details)
- Commit not in version range (verify git history)

**Check PR labels:**
```bash
# List all PRs in a range with their labels
# This helps identify missing release-note labels
```

### Manual extraction fallback

If the script fails to extract notes:

```bash
# Query GitHub API directly
curl -H "Authorization: token $GITHUB_TOKEN" \
  "https://api.github.com/repos/murmur-nexus/murmur/pulls?state=closed&per_page=100" \
  | jq '.[] | {number, title, body}' | grep -A 20 "release-note"
```

## CI Integration

The release-notes workflow runs on every PR:

1. **Trigger:** PR opened/updated
2. **Validation:** Checks for valid release note
3. **Labeling:** Applies appropriate label
4. **Feedback:** Comments if invalid
5. **Merge gate:** Invalid PRs blocked (optional, can remove)

## Future Enhancements

Possible additions:
- Auto-create release PR after changelog generation
- Automatically publish GitHub Release
- Slack notifications on release
- Validate that release notes follow style guide
- Generate JSON for integration with docs/websites
- Add changelog to release tag annotations (git)
- Support for pre-release notes and changelogs
