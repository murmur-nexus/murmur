//! Where the commit `mur version` reports comes from. Shared by `build.rs`, which fixes the
//! commit at compile time, and by the `mur` binary's unit tests.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The stamp `git archive` expands, relative to the crate directory. The root `.gitattributes`
/// marks it `export-subst`; in a checkout it holds the unexpanded `$Format:%H$`.
const ARCHIVE_STAMP: &str = "build/archive-commit.txt";

/// The commit a build reports, and the existing files whose change can alter it, for
/// `cargo:rerun-if-changed`.
pub struct BuildCommit {
    /// A full 40-character lowercase hex commit, or `None` when no source gave one.
    pub commit: Option<String>,
    pub watched: Vec<PathBuf>,
}

/// The commit of the source in `manifest_dir`, from the first source that gives one:
///
/// 1. `.cargo_vcs_info.json`, for a crates.io source package. When it exists it is the only
///    source, even when it holds no commit.
/// 2. `git rev-parse HEAD`, when the git checkout around `manifest_dir` has `workspace_root` as
///    its top level. An archive unpacked inside another repository sits in that repository's
///    worktree, and its `HEAD` is not this source's commit.
/// 3. The `git archive` stamp at [`ARCHIVE_STAMP`], when it holds an expanded commit.
///
/// Never fails: a missing git, file or commit only leaves `commit` as `None`.
pub fn resolve_build_commit(manifest_dir: &Path, workspace_root: &Path) -> BuildCommit {
    let vcs_info = manifest_dir.join(".cargo_vcs_info.json");
    if vcs_info.exists() {
        let commit = std::fs::read_to_string(&vcs_info)
            .ok()
            .and_then(|json| commit_from_vcs_info(&json));
        return BuildCommit {
            commit,
            watched: vec![vcs_info],
        };
    }
    if is_workspace_checkout(manifest_dir, workspace_root) {
        if let Some(commit) =
            git(manifest_dir, &["rev-parse", "HEAD"]).and_then(|out| commit_from_rev_parse(&out))
        {
            return BuildCommit {
                commit: Some(commit),
                watched: head_files(manifest_dir),
            };
        }
    }
    let stamp = manifest_dir.join(ARCHIVE_STAMP);
    if stamp.exists() {
        let commit = std::fs::read_to_string(&stamp)
            .ok()
            .and_then(|text| commit_from_archive_stamp(&text));
        return BuildCommit {
            commit,
            watched: vec![stamp],
        };
    }
    BuildCommit {
        commit: None,
        watched: Vec::new(),
    }
}

/// Whether `git rev-parse --show-toplevel` in `manifest_dir` names `workspace_root`. Both sides
/// are canonicalised: the two can differ by a symlink, such as macOS's `/tmp`.
fn is_workspace_checkout(manifest_dir: &Path, workspace_root: &Path) -> bool {
    let Some(toplevel) = git(manifest_dir, &["rev-parse", "--show-toplevel"]) else {
        return false;
    };
    match (
        std::fs::canonicalize(toplevel.trim()),
        std::fs::canonicalize(workspace_root),
    ) {
        (Ok(toplevel), Ok(root)) => toplevel == root,
        _ => false,
    }
}

/// The files a commit, checkout or reset rewrites: `HEAD`, its reflog, the branch it names and
/// `packed-refs`. `--git-path` resolves them in a linked worktree, where `.git` is a file.
/// Only existing ones are returned: cargo counts a missing `rerun-if-changed` path as changed
/// on every build, which would recompile `murmur-cli` each time.
///
/// The reflog is what catches a commit on a branch that lives only in `packed-refs`, as
/// `git gc` leaves every branch: the commit creates the loose ref file, which did not exist to
/// be watched, and rewrites neither `HEAD` nor `packed-refs`.
fn head_files(manifest_dir: &Path) -> Vec<PathBuf> {
    let mut names = vec!["HEAD".to_string(), "logs/HEAD".to_string()];
    if let Some(branch) = git(manifest_dir, &["symbolic-ref", "-q", "HEAD"]) {
        names.push(branch.trim().to_string());
    }
    names.push("packed-refs".to_string());
    names
        .iter()
        .filter_map(|name| git(manifest_dir, &["rev-parse", "--git-path", name]))
        .map(|path| manifest_dir.join(path.trim()))
        .filter(|path| path.exists())
        .collect()
}

/// `git <args>`'s stdout, run in `dir`; `None` when git is missing or the command fails.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// The commit a crates.io source package was cut from: `git.sha1` in its `.cargo_vcs_info.json`.
pub fn commit_from_vcs_info(json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let sha1 = value.get("git")?.get("sha1")?.as_str()?;
    full_commit(sha1)
}

/// The commit `git rev-parse HEAD` printed.
pub fn commit_from_rev_parse(stdout: &str) -> Option<String> {
    full_commit(stdout.trim())
}

/// The commit `git archive` wrote over the stamp's `$Format:%H$`. The unexpanded placeholder
/// gives `None`.
pub fn commit_from_archive_stamp(text: &str) -> Option<String> {
    full_commit(text.trim())
}

/// `candidate` when it is a full commit hash: 40 lowercase hex characters.
fn full_commit(candidate: &str) -> Option<String> {
    let is_full = candidate.len() == 40
        && candidate
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    is_full.then(|| candidate.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "321640ff1c0d2e3f4a5b6c7d8e9f0a1b2c3d4e5f";

    #[test]
    fn vcs_info_gives_its_sha1() {
        let json = format!(
            "{{\n  \"git\": {{\n    \"sha1\": \"{SHA}\"\n  }},\n  \"path_in_vcs\": \"crates/murmur-cli\"\n}}\n"
        );
        assert_eq!(commit_from_vcs_info(&json).as_deref(), Some(SHA));
    }

    #[test]
    fn vcs_info_without_git_sha1_gives_none() {
        assert_eq!(commit_from_vcs_info(r#"{"path_in_vcs": ""}"#), None);
        assert_eq!(commit_from_vcs_info(r#"{"git": {"dirty": true}}"#), None);
    }

    #[test]
    fn malformed_vcs_info_gives_none() {
        assert_eq!(
            commit_from_vcs_info(&format!(r#"{{"git": {{"sha1": "{SHA}""#)),
            None
        );
        assert_eq!(commit_from_vcs_info(""), None);
    }

    #[test]
    fn a_sha1_that_is_not_forty_lowercase_hex_gives_none() {
        for sha1 in [
            &SHA[..39],
            &format!("{SHA}0"),
            &SHA.to_uppercase(),
            "g21640ff1c0d2e3f4a5b6c7d8e9f0a1b2c3d4e5f",
            "",
        ] {
            let json = format!(r#"{{"git": {{"sha1": "{sha1}"}}}}"#);
            assert_eq!(commit_from_vcs_info(&json), None, "{sha1}");
        }
        assert_eq!(commit_from_vcs_info(r#"{"git": {"sha1": 7}}"#), None);
    }

    #[test]
    fn rev_parse_output_is_trimmed() {
        assert_eq!(
            commit_from_rev_parse(&format!("{SHA}\n")).as_deref(),
            Some(SHA)
        );
    }

    #[test]
    fn rev_parse_output_that_is_not_a_full_commit_gives_none() {
        assert_eq!(commit_from_rev_parse(""), None);
        assert_eq!(commit_from_rev_parse("HEAD\n"), None);
        assert_eq!(commit_from_rev_parse(&SHA[..12]), None);
    }

    #[test]
    fn an_expanded_archive_stamp_gives_its_commit() {
        assert_eq!(commit_from_archive_stamp(SHA).as_deref(), Some(SHA));
        assert_eq!(
            commit_from_archive_stamp(&format!("{SHA}\n")).as_deref(),
            Some(SHA)
        );
    }

    #[test]
    fn an_archive_stamp_that_is_not_a_full_commit_gives_none() {
        for text in [
            "$Format:%H$",
            "$Format:%H$\n",
            "",
            &SHA[..39],
            &format!("{SHA}0"),
            &SHA.to_uppercase(),
        ] {
            assert_eq!(commit_from_archive_stamp(text), None, "{text:?}");
        }
    }

    /// A stamp committed expanded would name a stale commit in every archive's build.
    #[test]
    fn the_checked_in_archive_stamp_is_unexpanded() {
        let stamp = include_str!("archive-commit.txt");
        assert_eq!(stamp, "$Format:%H$\n");
        assert_eq!(commit_from_archive_stamp(stamp), None);
    }

    #[test]
    fn gitattributes_marks_the_archive_stamp_export_subst() {
        let marked = include_str!("../../../.gitattributes").lines().any(|line| {
            line.split_whitespace().collect::<Vec<_>>()
                == ["crates/murmur-cli/build/archive-commit.txt", "export-subst"]
        });
        assert!(marked);
    }

    fn isolated_git(dir: &Path) -> Command {
        let mut command = Command::new("git");
        command
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args([
                "-c",
                "user.name=Build Commit Test",
                "-c",
                "user.email=build-commit@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "tag.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ]);
        command
    }

    fn run_git(dir: &Path, args: &[&str]) -> String {
        let output = isolated_git(dir).args(args).output().unwrap();
        assert!(output.status.success(), "git {args:?}: {output:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    /// The files of this workspace that decide the archive stamp, at their real relative paths.
    fn write_source(root: &Path) {
        std::fs::write(
            root.join(".gitattributes"),
            include_str!("../../../.gitattributes"),
        )
        .unwrap();
        let build = root.join("crates/murmur-cli/build");
        std::fs::create_dir_all(&build).unwrap();
        std::fs::write(
            build.join("archive-commit.txt"),
            include_str!("archive-commit.txt"),
        )
        .unwrap();
    }

    /// A repository holding [`write_source`]'s files in one commit, and that commit.
    fn source_checkout() -> (tempfile::TempDir, String) {
        let repo = tempfile::tempdir().unwrap();
        write_source(repo.path());
        run_git(repo.path(), &["init", "-q"]);
        run_git(repo.path(), &["add", "."]);
        run_git(repo.path(), &["commit", "-q", "-m", "source"]);
        let head = run_git(repo.path(), &["rev-parse", "HEAD"]);
        (repo, head)
    }

    /// `git archive` of `repo`'s `HEAD`, unpacked into `dest`.
    fn unpack_archive(repo: &Path, dest: &Path) {
        let tarball = tempfile::tempdir().unwrap();
        let tar_file = tarball.path().join("source.tar");
        run_git(
            repo,
            &[
                "archive",
                "--format=tar",
                "-o",
                tar_file.to_str().unwrap(),
                "HEAD",
            ],
        );
        std::fs::create_dir_all(dest).unwrap();
        let status = Command::new("tar")
            .arg("-xf")
            .arg(&tar_file)
            .arg("-C")
            .arg(dest)
            .status()
            .unwrap();
        assert!(status.success(), "tar -xf: {status}");
    }

    fn resolve(root: &Path) -> BuildCommit {
        resolve_build_commit(&root.join("crates/murmur-cli"), root)
    }

    #[test]
    fn a_checkout_of_the_workspace_gives_its_head() {
        let (repo, head) = source_checkout();
        let resolved = resolve(repo.path());
        assert_eq!(resolved.commit, Some(head));
        assert!(!resolved.watched.is_empty());
        assert!(resolved.watched.iter().all(|path| path.exists()));
    }

    #[test]
    fn an_unpacked_archive_gives_the_archived_commit() {
        let (repo, head) = source_checkout();
        let unpacked = tempfile::tempdir().unwrap();
        unpack_archive(repo.path(), unpacked.path());
        let resolved = resolve(unpacked.path());
        assert_eq!(resolved.commit, Some(head));
        assert_eq!(
            resolved.watched,
            [unpacked
                .path()
                .join("crates/murmur-cli/build/archive-commit.txt")]
        );
    }

    #[test]
    fn an_archive_unpacked_inside_another_repository_gives_the_archived_commit() {
        let (repo, head) = source_checkout();
        let outer = tempfile::tempdir().unwrap();
        std::fs::write(outer.path().join("dotfiles"), "outer\n").unwrap();
        run_git(outer.path(), &["init", "-q"]);
        run_git(outer.path(), &["add", "."]);
        run_git(outer.path(), &["commit", "-q", "-m", "outer"]);
        let outer_head = run_git(outer.path(), &["rev-parse", "HEAD"]);
        assert_ne!(outer_head, head);

        let unpacked = outer.path().join("src/murmur");
        unpack_archive(repo.path(), &unpacked);
        assert_eq!(resolve(&unpacked).commit, Some(head));
    }

    #[test]
    fn a_copy_without_git_or_an_expanded_stamp_gives_none() {
        let copy = tempfile::tempdir().unwrap();
        write_source(copy.path());
        let resolved = resolve(copy.path());
        assert_eq!(resolved.commit, None);
        assert_eq!(
            resolved.watched,
            [copy
                .path()
                .join("crates/murmur-cli/build/archive-commit.txt")]
        );
    }

    #[test]
    fn vcs_info_wins_over_a_checkout() {
        let (repo, head) = source_checkout();
        assert_ne!(head, SHA);
        let vcs_info = repo.path().join("crates/murmur-cli/.cargo_vcs_info.json");
        std::fs::write(&vcs_info, format!(r#"{{"git": {{"sha1": "{SHA}"}}}}"#)).unwrap();
        let resolved = resolve(repo.path());
        assert_eq!(resolved.commit.as_deref(), Some(SHA));
        assert_eq!(resolved.watched, [vcs_info]);
    }
}
