# Release Guide

Complete walkthrough for publishing a new release.

## Quick Release Checklist

```
[ ] All PRs merged with valid release notes (validated by CI)
[ ] Bump version in Cargo.toml
[ ] Commit version bump
[ ] Create git tag
[ ] Push tag (triggers CI build + changelog generation)
[ ] Wait for CI (~2-5 min)
[ ] Download changelog artifact, review, and commit to repo
[ ] Update main CHANGELOG.md index
[ ] Update Homebrew formula
```

---

## Step-by-Step: Publishing a New Release

### 1. Bump Version

Edit the workspace version in `Cargo.toml`:

```toml
[workspace.package]
version = "X.Y.Z"  # ← Change this
```

### 2. Update Cargo.lock

```bash
cargo check
```

### 3. Commit Version Bump

```bash
git add Cargo.toml Cargo.lock
git commit -m "chore: bump version to X.Y.Z"
```

### 4. Tag and Push (Triggers CI Build + Changelog Generation)

```bash
git tag vX.Y.Z
git push origin main --tags
```

**⏳ Wait for CI** — Go to Actions and wait for the `release.yml` workflow to complete (usually 2-5 minutes).

CI automatically:
- ✅ Builds `mur` for all 3 platforms
- ✅ Computes SHA256 and SHA512 checksums
- ✅ **Generates detailed changelog** with binary downloads table
- ✅ Uploads changelog as artifact for download
- ✅ Creates GitHub Release with:
  - Platform-specific binaries (3 files)
  - SHA256 checksums in release notes

### 5. Download and Review Changelog

1. Go to **Actions** → **Release** workflow → Latest run
2. Scroll to **Artifacts** section at the bottom
3. Download **`changelog-vX.Y.Z`**
4. Review the generated changelog (it includes binaries table and categorized notes)
5. Commit to repo:
   ```bash
   # Extract and move the file
   mv ~/Downloads/vX.Y.Z.md CHANGELOG/vX.Y.Z.md
   git add CHANGELOG/vX.Y.Z.md
   git commit -m "docs: add changelog for vX.Y.Z"
   git push origin main
   ```

### 6. Update Main CHANGELOG.md

Now update the main index:

1. Edit `CHANGELOG.md`
2. Add the new version to the top of the releases list:
   ```markdown
   - [vX.Y.Z](./CHANGELOG/vX.Y.Z.md) - 2026-07-08
   ```
3. Commit and push:
   ```bash
   git add CHANGELOG.md
   git commit -m "docs: update changelog index for vX.Y.Z"
   git push origin main
   ```

### 7. Update Homebrew Formula

Copy the SHA256 checksums from the GitHub Release notes and update the Homebrew tap:

```bash
# Clone or go to the tap repo
cd ~/.homebrew-murmur  # or your local clone

# Edit the formula
vim Formula/mur.rb

# Update:
# - version "X.Y.Z"
# - sha256 values (one per platform)
```

Example (update the 3 sha256 values):
```ruby
class Mur < Formula
  desc "Murmur CLI"
  homepage "https://github.com/murmur-nexus/murmur"
  version "X.Y.Z"

  on_macos do
    on_arm do
      url "https://github.com/murmur-nexus/murmur/releases/download/vX.Y.Z/mur-X.Y.Z-darwin-aarch64"
      sha256 "abc123..."  # ← from release notes
    end
    on_intel do
      url "https://github.com/murmur-nexus/murmur/releases/download/vX.Y.Z/mur-X.Y.Z-darwin-x86_64"
      sha256 "def456..."  # ← from release notes
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/murmur-nexus/murmur/releases/download/vX.Y.Z/mur-X.Y.Z-linux-x86_64"
      sha256 "ghi789..."  # ← from release notes
    end
  end
end
```

Commit and push:
```bash
git add Formula/mur.rb
git commit -m "chore: bump mur to X.Y.Z"
git push
```

Users running `brew update && brew upgrade mur` get the new version automatically.

---

## Release Process Flow

```
Step 1-3: Bump version & commit
  ↓
Step 4: Tag & push
  ↓
CI builds for 3 platforms & generates changelog (~2-5 min)
  ↓
GitHub Release created with binaries + changelog uploaded as artifact
  ↓
Step 5: Download & review changelog, commit to repo
  ↓
Step 6: Update main CHANGELOG.md index
  ↓
Step 7: Update Homebrew formula
  ↓
✅ Released! Users can install/upgrade
```

---

## Verifying a Release

After publishing:

```bash
# Check the tag
git tag -l vX.Y.Z

# Check the release on GitHub
curl https://api.github.com/repos/murmur-nexus/murmur/releases/tags/vX.Y.Z | jq '.assets[].name'

# Test installation (if you have Homebrew tap added)
brew install murmur-nexus/murmur/mur@X.Y.Z
mur --version  # Should print X.Y.Z
```

---

## Troubleshooting

### CI build failed
- Check the release.yml workflow logs in Actions
- Ensure no uncommitted changes locally
- Re-push the tag if needed: `git push origin main --tags --force`

### Changelog didn't generate
- Verify the previous version tag exists: `git tag -l`
- Check that PRs have valid release-note labels
- See [RELEASE_NOTES.md](RELEASE_NOTES.md) for validation details

### Homebrew formula won't update
- Ensure checksums exactly match release assets
- Run `brew audit Formula/mur.rb` to validate formula syntax
- Test locally: `brew install --build-from-source Formula/mur.rb`

---

## What's Automated

✅ **Changelog generation** — Automatically created with:
- Binary downloads table (filename, SHA512 hash, size)
- Categorized release notes from PRs
- Published date and version
- Uploaded as artifact for manual review

✅ **GitHub Release creation** — Includes:
- All 3 platform binaries
- SHA256 checksums in release notes

✅ **You need to do manually**:
- Download and review the changelog artifact
- Commit changelog to `CHANGELOG/vX.Y.Z.md`
- Update main `CHANGELOG.md` index
- Update Homebrew formula with new checksums

## Automating Further (Optional Future Work)

Possible future improvements:
- Auto-create PR with Homebrew formula bump and changelog update
- Auto-commit CHANGELOG.md updates (skip manual step)
- Slack notification when release is live
- Auto-generate release announcement

See [RELEASE_WORKFLOW.md](RELEASE_WORKFLOW.md) for technical details on the release notes system.
