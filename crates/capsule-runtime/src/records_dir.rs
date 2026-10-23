//! The session's records directory: every record the runtime writes about a session, outside the
//! workdir the capsule's commands can write, and kept after the session ends.
//!
//! ```text
//! ~/.murmur/records/                              0700
//! └── <workdir root without its leading />/       0700, each component
//!     └── <session-id>/                           0700, created at staging, kept after the session
//!         ├── trace.jsonl
//!         ├── blobs/<sha256>
//!         ├── logs/bootstrap.log | otel.log | hook-<name>.log
//!         ├── logs/shell-<ms>.log | <work-id>.log
//!         │                                       a shell command's full output
//!         ├── completion.json                     a delegated child's outcome
//!         ├── hooks/                              `.` of every hook declared
//!         │                                       `capabilities.filesystem.records: true`
//!         ├── out/result.txt | result_<task-id>.txt | compaction-summaries.jsonl
//!         │                                       an agent's answers and compaction summaries
//!         ├── tmp/                                `$TMPDIR` of every command
//!         ├── home/                               `$HOME` of every command, created at staging
//!         └── children/<capsule>-<16 hex>/        0700, a delegated child's `--workdir`
//! ```
//!
//! The group a session's directory sits in is its **workdir root**: the directory passed to
//! `mur run --workdir`, otherwise the directory holding the accessible workdir
//! (`<manifest-dir>/workdir`). Neither is beneath the accessible workdir, so no capsule command
//! can rename or replace it, and every reader given the same directory computes the same group.
//!
//! The Landlock rule and sealed bind that keep it read-only to commands are the runtime
//! directory's ([`crate::runtime_dir`]). `tmp/` and `home/` are the two exceptions, granted the
//! work folder's rights by their own rule and bind; [`crate::containment::records_dir_report`] says per tier
//! what holds. Unlike the runtime directory it is not removed when the session ends;
//! `trace.retain` ([`crate::retention::prune_sessions`]) prunes it.

use std::path::{Component, Path, PathBuf};

use crate::murmur_home::HomeUnusable;
use crate::protected_paths::canonicalize_existing_prefix;
use crate::workdir_fs::WorkdirRoot;

/// `~/.murmur/records`, relative to the murmur home.
pub const RECORDS_DIR_NAME: &str = "records";

/// The variable naming the records directory in every shell command's, plan shell step's,
/// native tool's and `transport: process` harness's environment. Runtime-owned like
/// [`crate::runtime_dir::RUNTIME_DIR_ENV`]: no manifest, override or host value can set it.
pub const RECORDS_DIR_ENV: &str = "MURMUR_RECORDS_DIR";

/// The directory beneath `<session-id>/` a hook declared `capabilities.filesystem.records: true`
/// is preopened at as `"."`, read-write.
pub const HOOKS_SUBDIR: &str = "hooks";

/// The directory beneath `<session-id>/` that is `$HOME` for every shell command, plan shell step
/// and native tool, created at staging and granted the work folder's rights.
pub const HOME_SUBDIR: &str = "home";

/// The directory beneath `<session-id>/` an agent's `result.txt`, `result_<task-id>.txt` and
/// `compaction-summaries.jsonl` are written to, by the runtime alone.
pub const RESULTS_SUBDIR: &str = "out";

/// [`RESULTS_SUBDIR`]'s `result.txt`, relative to `<session-id>/`.
pub const RESULT_FILE: &str = "out/result.txt";

/// The mode every directory beneath `~/.murmur/records` is created at.
const PRIVATE_DIR_MODE: u32 = crate::murmur_home::MURMUR_HOME_DIR_MODE;

/// `~/.murmur/records`, decided without creating anything.
pub fn records_dir_root() -> Result<PathBuf, HomeUnusable> {
    Ok(crate::murmur_home::usable_murmur_home()?.join(RECORDS_DIR_NAME))
}

/// The directory every session of `workdir_root` keeps its records in:
/// `~/.murmur/records/<workdir_root, absolute and canonical, without its leading />`. Creates
/// nothing.
///
/// `workdir_root` is made absolute against the current directory and canonicalized through its
/// nearest existing ancestor, the rest appended, so the runtime at staging and a reader given the
/// same directory compute the same group whether or not it exists yet.
pub fn records_group(workdir_root: &Path) -> Result<PathBuf, HomeUnusable> {
    Ok(records_group_under(
        &crate::murmur_home::usable_murmur_home()?,
        workdir_root,
    ))
}

/// [`records_group`] beneath `murmur_home` instead of the process's `~/.murmur`, for a caller
/// that knows the home a session ran under without it being this process's `HOME`.
pub fn records_group_under(murmur_home: &Path, workdir_root: &Path) -> PathBuf {
    murmur_home
        .join(RECORDS_DIR_NAME)
        .join(mirrored(workdir_root))
}

/// `workdir_root` as a relative path beneath `~/.murmur/records`: absolute, canonical through its
/// nearest existing ancestor, and with no `.` or `..` left in it, so the mirror never names a
/// directory outside the records root.
fn mirrored(workdir_root: &Path) -> PathBuf {
    let absolute = std::path::absolute(workdir_root).unwrap_or_else(|_| workdir_root.to_path_buf());
    let canonical = canonicalize_existing_prefix(&absolute);
    let mut mirror = PathBuf::new();
    for component in canonical.components() {
        match component {
            Component::Normal(name) => mirror.push(name),
            Component::ParentDir => {
                mirror.pop();
            }
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
    mirror
}

/// One session's records directory, created at staging and kept after the session.
#[derive(Debug, Clone)]
pub struct RecordsDir {
    /// `<group>/<session-id>`, absolute.
    path: PathBuf,
    /// [`Self::path`], held, sharing the accessible root's refusal latch.
    root: WorkdirRoot,
}

impl RecordsDir {
    /// Creates `<group>/<session_id>/` and every directory above it up to `~/.murmur/records` at
    /// `0700`, with no link followed beneath `~/.murmur/records`, and holds it. Every refusal
    /// beneath it lands on `latch_of`'s latch and names its path absolutely.
    ///
    /// `group` is [`records_group`]'s answer for the session's workdir root, the one the launch
    /// was judged against.
    pub fn create(group: &Path, session_id: &str, latch_of: &WorkdirRoot) -> Result<Self, String> {
        let located = |path: &Path, reason: String| format!("{}: {reason}", path.display());
        let home = crate::murmur_home::ensure_murmur_home()?;
        let records = home.join(RECORDS_DIR_NAME);
        let rel = group
            .strip_prefix(&records)
            .map_err(|_| located(group, format!("is not beneath {}", records.display())))?
            .join(session_id);
        crate::murmur_home::hold_private_dir(&records)
            .map_err(|reason| located(&records, reason))?;
        let records_tree = WorkdirRoot::open(&records).map_err(|error| error.to_string())?;
        records_tree
            .create_dir_all(&rel, PRIVATE_DIR_MODE)
            .map_err(|error| error.to_string())?;
        let rel_text = rel.to_string_lossy();
        let walked = records_tree
            .open_beneath(&rel_text)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| located(&records.join(&rel), "vanished after its creation".into()))?;
        let path = records.join(&rel);
        let root = WorkdirRoot::open_sharing_latch_absolute(&path, latch_of)
            .map_err(|error| error.to_string())?;
        // The held root was opened by path; it must be the directory the no-follow walk created.
        if root.identity().map_err(|error| error.to_string())?
            != walked.identity().map_err(|error| error.to_string())?
        {
            return Err(located(
                &path,
                "changed between its creation and its open".to_string(),
            ));
        }
        Ok(Self { path, root })
    }

    /// The records directory's absolute path, the value of [`RECORDS_DIR_ENV`].
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The held directory every runtime record of the session is written through.
    #[must_use]
    pub fn root(&self) -> &WorkdirRoot {
        &self.root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_dir_mirror_drops_the_root_and_every_dot_segment() {
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().canonicalize().unwrap();
        let mirror = mirrored(&real.join("proj/./workdir/../workdir"));
        assert_eq!(
            mirror,
            real.strip_prefix("/").unwrap().join("proj").join("workdir")
        );
        assert!(mirror.is_relative());
        assert_eq!(mirrored(Path::new("/../../etc")), PathBuf::from("etc"));
    }

    #[test]
    fn records_dir_mirror_resolves_a_link_above_the_workdir_root() {
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(
            mirrored(&link.join("workdir")),
            mirrored(&real.join("workdir"))
        );
    }

    /// The group is the workdir root mirrored under `~/.murmur/records`; creating a session's
    /// directory makes every level `0700`, and a link planted in the mirrored path is refused.
    #[test]
    fn records_dir_is_created_owner_only_beneath_the_murmur_home() {
        let home = tempfile::tempdir().unwrap();
        crate::murmur_home::run_with_home(
            "records_dir::tests::records_dir_create_inner",
            home.path(),
        );
    }

    #[test]
    #[ignore = "run by records_dir_is_created_owner_only_beneath_the_murmur_home"]
    fn records_dir_create_inner() {
        if !crate::murmur_home::in_scratch_home() {
            return;
        }
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        let project = home.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let group = records_group(&project.join("workdir")).unwrap();
        assert_eq!(
            group,
            home.join(".murmur/records")
                .join(mirrored(&project.join("workdir")))
        );
        let accessible = WorkdirRoot::open(&project).unwrap();
        let created = RecordsDir::create(&group, "ses_0001", &accessible).unwrap();
        assert_eq!(created.path(), group.join("ses_0001"));
        let mut level = created.path().to_path_buf();
        while level != home.join(".murmur") {
            assert_eq!(
                crate::murmur_home::mode_of(&level),
                0o700,
                "{}",
                level.display()
            );
            level.pop();
        }
        created
            .root()
            .write_file("trace.jsonl", b"{}\n", crate::workdir_fs::DEFAULT_FILE_MODE)
            .unwrap();

        let elsewhere = home.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let planted_group = records_group(&home.join("planted/workdir")).unwrap();
        std::fs::create_dir_all(planted_group.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &planted_group).unwrap();
        assert!(RecordsDir::create(&planted_group, "ses_0002", &accessible).is_err());
        assert!(!elsewhere.join("ses_0002").exists());
    }
}
