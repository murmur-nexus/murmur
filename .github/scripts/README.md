# GitHub Scripts

Automation scripts for managing releases, changelog generation, and release notes validation.

## Scripts

### `validate-release-notes.js`

**Purpose:** Validates release notes in PR bodies

**Used by:** `release-notes.yml` GitHub Actions workflow

**What it does:**
- Parses the `release-note` block from PR description
- Validates content exists and isn't just placeholder text
- Outputs validation result and category label
- Called automatically on every PR

**Output:**
- `label`: Label to apply (`release-note`, `release-note/none`, `release-note/invalid`)
- `valid`: Whether validation passed (`true` or `false`)
- `content`: The extracted release note content

**Manual testing:**
```bash
# Add to PR body and push:
# ```release-note
# Added new feature X
# ```

# Then check the "Validate Release Notes" workflow run
```

---

### `aggregate-release-notes.py`

**Purpose:** Aggregates release notes from PRs between two versions

**Used by:** `generate-changelog.yml` workflow or run manually

**What it does:**
1. Gets commits between two git refs (tags/branches)
2. Queries GitHub API for PRs that contain those commits
3. Extracts release notes from PR bodies
4. Categorizes notes (Features, Fixes, Breaking Changes, etc.)
5. Generates formatted changelog (Markdown or JSON)

**Requirements:**
```bash
pip install requests
```

**Usage:**

Basic:
```bash
python aggregate-release-notes.py v1.0.0 v1.1.0
```

With options:
```bash
python aggregate-release-notes.py \
  --from v1.0.0 \
  --to v1.1.0 \
  --version v1.1.0 \
  --date 2026-03-15 \
  --output markdown \
  --file CHANGELOG.md
```

Full options:
```bash
python aggregate-release-notes.py --help
```

**Output formats:**
- `markdown` — Human-readable changelog (default)
- `json` — Structured data for integration
- `both` — Both formats

**Environment:**
- `GITHUB_TOKEN` — Optional, for higher API rate limits and private repos

**Example output:**

```markdown
## [v1.1.0] - 2026-03-15

### Breaking Changes
- Configuration format changed from TOML to YAML (#123)

### Features
- Added --json output flag to CLI (#124)
- Implemented caching layer for better performance (#125)

### Bug Fixes
- Fixed crash in connection pool (#126)
- Resolved race condition in worker (#127)

### Improvements
- Optimized query performance by 50% (#128)
```

---

## GitHub Actions Workflows

### `release-notes.yml`

**Trigger:** Pull request opened, synchronized, reopened, or description edited

**What it does:**
1. Checks PR for valid release-note block
2. Applies appropriate label (`release-note`, `release-note/none`, `release-note/invalid`)
3. Comments on PR if validation fails

**Re-running validation:**
If the check fails, simply edit the PR description to fix the release-note block — the workflow will re-run automatically (no need to push commits).

**Logs:**
- Check the workflow run to see validation results
- PR comments explain any issues found

---

### `generate-changelog.yml`

**Trigger:** Manual via **Run workflow** button

**Inputs:**
- `from_version` — Starting version tag (e.g., `v1.0.0`)
- `to_version` — Ending version tag or `HEAD` (default: `HEAD`)
- `release_version` — Version for changelog header (optional)
- `release_date` — Release date YYYY-MM-DD (optional, defaults to today)

**What it does:**
1. Checks out repo with full git history
2. Runs aggregate-release-notes.py
3. Generates Markdown changelog
4. Creates workflow summary and artifact
5. Uploads changelog as artifact for download

**Output:**
- Workflow summary shows generated changelog
- Artifact "changelog" contains the markdown file
- Ready to add to CHANGELOG.md or create GitHub Release

---

## Local Testing

### Test release notes validation

```bash
# Simulate a PR description with release note
cat > test_pr.md << 'EOF'
## Description
Test PR

```release-note
Added new feature
```
EOF

# Note: Manual testing requires GitHub CLI or direct API calls
```

### Test changelog generation

```bash
# Generate changelog for recent versions
python aggregate-release-notes.py v1.0.0 HEAD --output markdown

# Or for a specific date range (using git history)
python aggregate-release-notes.py v1.0.0 v1.1.0 --file test_changelog.md

# View the output
cat test_changelog.md
```

### Test with GitHub CLI

```bash
# View PRs with release-note labels
gh pr list --state merged --label "release-note" --repo murmur-nexus/murmur

# Check specific PR body for release note
gh pr view 123 --repo murmur-nexus/murmur --json body
```

---

## Debugging

### Enable debug logging

Add to workflow or script:
```bash
set -x  # bash
$DebugPreference = "Continue"  # PowerShell
```

### Check GitHub API rate limits

```bash
curl -H "Authorization: token $GITHUB_TOKEN" \
  https://api.github.com/rate_limit | jq .
```

### Validate git history

```bash
# List commits between refs
git log v1.0.0..v1.1.0 --oneline

# List tags
git tag -l

# Check if specific ref exists
git rev-parse v1.0.0
```

---

## Integration Points

These scripts integrate with:
- **GitHub Actions** — Workflows for automation
- **GitHub API** — PR and commit querying
- **Git** — Version history and refs
- **CHANGELOG.md** — Destination for release notes

See [RELEASE_WORKFLOW.md](../RELEASE_WORKFLOW.md) for the complete flow.
