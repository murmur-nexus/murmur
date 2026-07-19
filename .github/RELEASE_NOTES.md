# Release Notes Guidelines

This document explains how release notes are collected, aggregated, and published for each release.

## For Contributors

### Adding a Release Note to Your PR

Every PR should include a release note in the PR body, even if it's just to indicate "no user-facing change".

**In your PR description, include:**

```release-note
<your release note here>
```

### When to write a release note

Write a release note if your PR:
- Adds a new feature or capability
- Fixes a user-facing bug
- Changes behavior that users interact with
- Includes a breaking change
- Adds or removes a CLI flag, API endpoint, or configuration option
- Improves performance in a user-visible way

**Write "NONE" if:**
- Your change is internal refactoring
- Your change is only for tests or CI/CD
- Your change is documentation-only
- Your change is a minor code cleanup with no user impact

Example:
```release-note
NONE
```

### Release Note Format

Keep release notes:
- **Clear and concise** — write for end users, not developers
- **Action-focused** — what can users do now that they couldn't before?
- **User-perspective** — describe the benefit, not the implementation
- **Prefixed for breaking changes** — start with "Breaking:" if it's a breaking change

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
9. **Publish GitHub release** with the generated notes

### Release Note Labels

The CI system automatically applies labels to PRs:
- `release-note` — PR includes a release note (not "NONE")
- `release-note/none` — PR explicitly has no user-facing changes
- `release-note/invalid` — PR is missing or has malformed release note block

### Tools for Aggregation

The release tooling should:
1. Query GitHub API for merged PRs in the release range
2. Parse the `release-note` code block from PR descriptions
3. Skip entries with value "NONE"
4. Build changelog structure
5. Output as Markdown for `CHANGELOG/vX.Y.Z.md` and/or JSON for structured data

### Changelog Format Example

```markdown
## [v1.5.0] - 2026-03-15

### Features
- Added `--output-format` flag to CLI for JSON export support
- New HTTP middleware for request logging

### Bug Fixes
- Fixed crash when processing empty input files
- Corrected memory leak in connection pool

### Breaking Changes
- Configuration format changed from TOML to YAML. See [migration guide](docs/migration-v1.5.md)

### Internal
- Refactored parser logic for better maintainability
```

## CI/CD Integration

The GitHub Actions workflows handle this as follows:

1. **On PR open/update** (`release-notes.yml`):
   - Check for release-note block
   - Validate format
   - Apply appropriate label (`release-note`, `release-note/none`, `release-note/invalid`)

2. **On release tag** (`release.yml`):
   - Aggregate release notes for the version range
   - Generate `CHANGELOG/vX.Y.Z.md` and upload it as a workflow artifact for review
   - Create the GitHub Release
