//! `mur run --output <file>`: the task's answer, copied from the session's `out/result.txt` to a
//! path the caller chose, and present there only when the task completed.
//!
//! [`OutputFile::prepare`] runs before anything else `mur run` does. It refuses a path that is
//! not a regular file or absent, removes a regular file already there, and holds the parent
//! directory open; [`OutputFile::deliver`] writes the answer through that held descriptor. No link
//! at the path is ever followed, so a capsule that can write the directory cannot redirect the
//! write.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use capsule_runtime::workdir_fs::DEFAULT_FILE_MODE;
use capsule_runtime::{
    resolve_lifecycle, AfterTask, EntryKind, LifecycleConfig, LifecycleOverride, TaskAcceptance,
    WorkdirRoot, RESULT_FILE,
};

use crate::error::{CliError, E_IO_003};

/// The `--output` path, checked and emptied, with its parent directory held open.
pub(crate) struct OutputFile {
    /// The directory the answer is written into, held from the check in `prepare` to the write.
    parent: WorkdirRoot,
    /// The file name within `parent`; never contains `/`.
    name: String,
    /// The absolute path, for messages.
    path: PathBuf,
}

impl OutputFile {
    /// Check `arg`, remove a stale regular file at it, and hold its parent directory open.
    ///
    /// A relative `arg` is resolved against the current directory. Refused with `E-IO-003`, with
    /// nothing changed on disk, when `arg` names no file or a non-UTF-8 one, its parent is not an
    /// existing directory, it is a directory, a link or another non-regular file, or it is the
    /// existing file `task_arg` names.
    pub(crate) fn prepare(arg: &Path, task_arg: Option<&str>) -> Result<Self, CliError> {
        let path = if arg.is_absolute() {
            arg.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|source| {
                    CliError::new(
                        E_IO_003,
                        format!("failed to determine current working directory: {source}"),
                    )
                })?
                .join(arg)
        };
        let shown = path.display().to_string();

        // `Path::file_name` reads `dir/` and `dir/.` as `dir`, which would turn a path a shell
        // treats as a directory into a file of that name.
        let raw = arg.as_os_str().as_encoded_bytes();
        let names_a_directory = raw.is_empty() || raw.ends_with(b"/") || raw.ends_with(b"/.");
        let (Some(parent_path), Some(file_name)) = (path.parent(), path.file_name()) else {
            return Err(names_no_file(&shown));
        };
        if names_a_directory {
            return Err(names_no_file(&shown));
        }
        let Some(name) = file_name.to_str().map(str::to_string) else {
            return Err(CliError::new(
                E_IO_003,
                format!("--output {shown} is not a UTF-8 file name"),
            ));
        };

        let parent = WorkdirRoot::open(parent_path).map_err(|error| {
            let missing = matches!(
                &error,
                capsule_runtime::WorkdirError::Io { source, .. }
                    if matches!(
                        source.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    )
            );
            if missing {
                CliError::with_hint(
                    E_IO_003,
                    format!(
                        "--output {shown}: the directory {} does not exist",
                        parent_path.display()
                    ),
                    "create it first; mur run creates no directory for --output",
                )
            } else {
                CliError::new(
                    E_IO_003,
                    format!(
                        "--output {shown}: the directory {} could not be opened: {error}",
                        parent_path.display()
                    ),
                )
            }
        })?;

        let kind = parent.entry_kind(&name).map_err(|error| {
            CliError::new(
                E_IO_003,
                format!("--output {shown} could not be checked: {error}"),
            )
        })?;
        match kind {
            None => {}
            Some(EntryKind::Directory) => {
                return Err(CliError::new(
                    E_IO_003,
                    format!(
                        "--output {shown} is a directory; give the path of a file to write the \
                         answer to"
                    ),
                ));
            }
            Some(EntryKind::Symlink) => {
                return Err(CliError::new(
                    E_IO_003,
                    format!(
                        "--output {shown} is a symbolic link; give the path of the file itself"
                    ),
                ));
            }
            Some(EntryKind::Other) => {
                return Err(CliError::new(
                    E_IO_003,
                    format!("--output {shown} is not a regular file"),
                ));
            }
            Some(EntryKind::File) => {
                if is_the_task_file(&path, task_arg) {
                    return Err(CliError::new(
                        E_IO_003,
                        format!(
                            "--output {shown} is the file --task names; it would be removed \
                             before the task is read"
                        ),
                    ));
                }
                parent.remove_file(&name).map_err(|error| {
                    CliError::new(
                        E_IO_003,
                        format!(
                            "--output {shown} holds a file from before this run that could not \
                             be removed: {error}"
                        ),
                    )
                })?;
            }
        }

        Ok(Self { parent, name, path })
    }

    /// Copy the session's out/result.txt to the path, atomically.
    ///
    /// The answer is read from `records_dir` for an agent capsule and from `workdir`, where the
    /// script writes it, for a script capsule; an agent's work folder is never read, since its
    /// commands can write there. No result file writes nothing and is not an error. A failed read
    /// or write is an `E-IO-003` naming where the answer still is.
    pub(crate) fn deliver(
        &self,
        records_dir: &Path,
        workdir: &Path,
        script: bool,
    ) -> Result<(), CliError> {
        let source_dir = if script { workdir } else { records_dir };
        let source = source_dir.join(RESULT_FILE);
        let failed = |reason: String| {
            CliError::with_hint(
                E_IO_003,
                format!(
                    "the task completed, but its answer could not be written to {}: {reason}",
                    self.path.display()
                ),
                format!("the answer is still in {}", source.display()),
            )
        };

        let answer = WorkdirRoot::open(source_dir)
            .and_then(|root| root.read_file(RESULT_FILE, u64::MAX))
            .map_err(|error| failed(format!("reading {}: {error}", source.display())))?;
        let Some(bytes) = answer else {
            return Ok(());
        };
        self.write(&bytes).map_err(failed)
    }

    /// Writes `bytes` through a same-directory temporary file renamed over the name, in the held
    /// parent only.
    ///
    /// Not `WorkdirRoot::write_file`: that reaches a removed root again by its path, creating it
    /// and following links on the way, so a capsule that can write the grandparent could swap the
    /// directory for a link to another. A removed parent fails the write here.
    fn write(&self, bytes: &[u8]) -> Result<(), String> {
        match self.parent.entry_kind(&self.name) {
            Ok(None | Some(EntryKind::File)) => {}
            Ok(Some(EntryKind::Symlink)) => {
                return Err(format!(
                    "a symbolic link appeared at {} during the run and was not followed",
                    self.path.display()
                ));
            }
            Ok(Some(EntryKind::Directory | EntryKind::Other)) => {
                return Err(format!(
                    "{} is no longer absent or a regular file",
                    self.path.display()
                ));
            }
            Err(error) => return Err(error.to_string()),
        }
        self.parent
            .write_file_no_create(&self.name, bytes, DEFAULT_FILE_MODE)
            .map_err(|error| error.to_string())
    }
}

fn names_no_file(shown: &str) -> CliError {
    CliError::new(E_IO_003, format!("--output {shown} names no file"))
}

/// Whether `task_arg` names an existing file that is the file at `output`: same device and inode,
/// so a hard link or a link to it counts too.
fn is_the_task_file(output: &Path, task_arg: Option<&str>) -> bool {
    let Some(task) = task_arg else {
        return false;
    };
    let (Ok(task), Ok(output)) = (std::fs::metadata(task), std::fs::symlink_metadata(output))
    else {
        return false;
    };
    (task.dev(), task.ino()) == (output.dev(), output.ino())
}

/// Refuse --output on an agent capsule whose effective lifecycle can run a task other than
/// the --task one.
///
/// The effective lifecycle is the runtime's own merge, [`resolve_lifecycle`]. Under
/// `task_acceptance: queue` or `after_task: sleep` the session's `out/result.txt` can hold a later
/// task's answer.
pub(crate) fn check_output_lifecycle(
    lifecycle: Option<&LifecycleConfig>,
    ov: Option<&LifecycleOverride>,
) -> Result<(), CliError> {
    let effective = resolve_lifecycle(lifecycle.cloned(), ov);
    let mut at_fault = Vec::new();
    if effective.task_acceptance == TaskAcceptance::Queue {
        at_fault.push("lifecycle.task_acceptance: queue");
    }
    if effective.after_task == AfterTask::Sleep {
        at_fault.push("lifecycle.after_task: sleep");
    }
    if at_fault.is_empty() {
        return Ok(());
    }
    Err(CliError::with_hint(
        E_IO_003,
        format!(
            "--output writes the answer of the one task --task gives, and this capsule's \
             lifecycle lets the session run other tasks too ({})",
            at_fault.join(", ")
        ),
        "pass --lifecycle-task-acceptance single and --lifecycle-after-task exit, or drop \
         --output and read each task's answer over the door",
    ))
}

#[cfg(test)]
mod tests {
    use std::ffi::{CString, OsStr};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};

    use super::*;

    fn refusal(result: Result<OutputFile, CliError>) -> CliError {
        match result {
            Ok(_) => panic!("prepare accepted the path"),
            Err(error) => error,
        }
    }

    fn assert_io_003(error: &CliError, needle: &str) {
        assert_eq!(error.code, E_IO_003, "{error}");
        assert!(error.message.contains(needle), "{error}");
    }

    #[test]
    fn prepare_refuses_a_directory_and_leaves_it() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("out");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("inside"), "x").unwrap();
        let error = refusal(OutputFile::prepare(&target, None));
        assert_io_003(&error, "is a directory");
        assert_eq!(std::fs::read(target.join("inside")).unwrap(), b"x");
    }

    #[test]
    fn prepare_refuses_a_missing_parent_and_creates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        let error = refusal(OutputFile::prepare(&missing.join("a.txt"), None));
        assert_io_003(&error, "does not exist");
        assert!(error
            .hint
            .as_deref()
            .unwrap()
            .contains("creates no directory"));
        assert!(!missing.exists());
    }

    #[test]
    fn prepare_refuses_a_parent_that_is_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, "keep").unwrap();
        let error = refusal(OutputFile::prepare(&file.join("a.txt"), None));
        assert_io_003(&error, "does not exist");
        assert_eq!(std::fs::read(&file).unwrap(), b"keep");
    }

    #[test]
    fn prepare_refuses_a_symlink_and_leaves_link_and_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::write(&target, "keep").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let error = refusal(OutputFile::prepare(&link, None));
        assert_io_003(&error, "is a symbolic link");
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read(&target).unwrap(), b"keep");
    }

    #[test]
    fn prepare_refuses_a_fifo_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("fifo");
        let fifo_c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: `fifo_c` is a NUL-terminated path that outlives the call.
        assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
        let error = refusal(OutputFile::prepare(&fifo, None));
        assert_io_003(&error, "is not a regular file");
        assert!(std::fs::symlink_metadata(&fifo)
            .unwrap()
            .file_type()
            .is_fifo());
    }

    #[test]
    fn prepare_refuses_a_non_utf8_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(OsStr::from_bytes(b"answer-\xff.txt"));
        let error = refusal(OutputFile::prepare(&path, None));
        assert_io_003(&error, "is not a UTF-8 file name");
        assert!(!path.exists());
    }

    #[test]
    fn prepare_refuses_a_path_that_names_no_file() {
        let dir = tempfile::tempdir().unwrap();
        for arg in [
            PathBuf::from("/"),
            PathBuf::from(""),
            dir.path().join("sub/"),
            dir.path().join("sub/."),
            dir.path().join(".."),
        ] {
            let error = refusal(OutputFile::prepare(&arg, None));
            assert_io_003(&error, "names no file");
        }
    }

    #[test]
    fn prepare_refuses_the_task_file_and_its_hard_link() {
        let dir = tempfile::tempdir().unwrap();
        let task = dir.path().join("task.md");
        std::fs::write(&task, "do it").unwrap();
        let task_str = task.to_str().unwrap();
        let error = refusal(OutputFile::prepare(&task, Some(task_str)));
        assert_io_003(&error, "is the file --task names");
        let hard = dir.path().join("hard.md");
        std::fs::hard_link(&task, &hard).unwrap();
        let error = refusal(OutputFile::prepare(&hard, Some(task_str)));
        assert_io_003(&error, "is the file --task names");
        assert_eq!(std::fs::read(&task).unwrap(), b"do it");
        assert_eq!(std::fs::read(&hard).unwrap(), b"do it");
    }

    #[test]
    fn prepare_removes_a_regular_file_and_accepts_task_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("answer.txt");
        std::fs::write(&path, "stale answer").unwrap();
        let output = OutputFile::prepare(&path, Some("answer.txt is not this text")).unwrap();
        assert!(!path.exists());
        assert_eq!(output.name, "answer.txt");
        assert_eq!(output.path, path);
    }

    #[test]
    fn prepare_resolves_a_relative_path_against_the_current_directory() {
        let output = OutputFile::prepare(Path::new("run-output-relative-check.txt"), None).unwrap();
        assert_eq!(
            output.path,
            std::env::current_dir()
                .unwrap()
                .join("run-output-relative-check.txt")
        );
    }

    fn lifecycle(task_acceptance: TaskAcceptance, after_task: AfterTask) -> LifecycleConfig {
        LifecycleConfig {
            task_acceptance,
            after_task,
            ..LifecycleConfig::default()
        }
    }

    fn overriding(
        task_acceptance: Option<TaskAcceptance>,
        after_task: Option<AfterTask>,
    ) -> LifecycleOverride {
        LifecycleOverride {
            task_acceptance,
            after_task,
        }
    }

    #[test]
    fn check_output_lifecycle_accepts_one_task_lifecycles() {
        check_output_lifecycle(None, None).unwrap();
        let single = lifecycle(TaskAcceptance::Single, AfterTask::Exit);
        check_output_lifecycle(Some(&single), None).unwrap();
        let none = lifecycle(TaskAcceptance::None, AfterTask::Exit);
        check_output_lifecycle(Some(&none), None).unwrap();
    }

    #[test]
    fn check_output_lifecycle_refuses_queue_and_sleep_from_the_manifest() {
        let queue = lifecycle(TaskAcceptance::Queue, AfterTask::Exit);
        let error = check_output_lifecycle(Some(&queue), None).unwrap_err();
        assert_eq!(error.code, E_IO_003);
        assert!(
            error
                .message
                .ends_with("(lifecycle.task_acceptance: queue)"),
            "{error}"
        );
        assert!(error
            .hint
            .as_deref()
            .unwrap()
            .contains("--lifecycle-task-acceptance single"));
        let sleep = lifecycle(TaskAcceptance::Single, AfterTask::Sleep);
        let error = check_output_lifecycle(Some(&sleep), None).unwrap_err();
        assert!(
            error.message.ends_with("(lifecycle.after_task: sleep)"),
            "{error}"
        );
    }

    #[test]
    fn check_output_lifecycle_reads_overrides_over_the_manifest() {
        let error =
            check_output_lifecycle(None, Some(&overriding(Some(TaskAcceptance::Queue), None)))
                .unwrap_err();
        assert!(error.message.contains("lifecycle.task_acceptance: queue"));
        let error = check_output_lifecycle(None, Some(&overriding(None, Some(AfterTask::Sleep))))
            .unwrap_err();
        assert!(error.message.contains("lifecycle.after_task: sleep"));

        let queue = lifecycle(TaskAcceptance::Queue, AfterTask::Exit);
        check_output_lifecycle(
            Some(&queue),
            Some(&overriding(Some(TaskAcceptance::Single), None)),
        )
        .unwrap();
        let sleep = lifecycle(TaskAcceptance::Single, AfterTask::Sleep);
        check_output_lifecycle(Some(&sleep), Some(&overriding(None, Some(AfterTask::Exit))))
            .unwrap();
        let fine = lifecycle(TaskAcceptance::Single, AfterTask::Exit);
        assert!(check_output_lifecycle(
            Some(&fine),
            Some(&overriding(None, Some(AfterTask::Sleep)))
        )
        .is_err());
    }

    /// A records directory, a work folder and an output directory, each its own.
    struct Dirs {
        _root: tempfile::TempDir,
        records: PathBuf,
        workdir: PathBuf,
        out: PathBuf,
    }

    fn dirs() -> Dirs {
        let root = tempfile::tempdir().unwrap();
        let records = root.path().join("records");
        let workdir = root.path().join("workdir");
        let out = root.path().join("out-dir");
        for dir in [&records, &workdir, &out] {
            std::fs::create_dir_all(dir.join("out")).unwrap();
        }
        Dirs {
            _root: root,
            records,
            workdir,
            out,
        }
    }

    fn no_temp_files(dir: &Path) {
        let names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(names.is_empty(), "temporary files left: {names:?}");
    }

    #[test]
    fn agent_deliver_copies_the_records_file_and_ignores_the_work_folder() {
        let d = dirs();
        std::fs::write(d.records.join(RESULT_FILE), b"first line\nsecond line").unwrap();
        std::fs::write(d.workdir.join(RESULT_FILE), b"planted").unwrap();
        let path = d.out.join("answer.txt");
        let output = OutputFile::prepare(&path, None).unwrap();
        output.deliver(&d.records, &d.workdir, false).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"first line\nsecond line");
        no_temp_files(&d.out);
    }

    #[test]
    fn agent_deliver_does_not_fall_back_to_the_work_folder() {
        let d = dirs();
        std::fs::write(d.workdir.join(RESULT_FILE), b"planted").unwrap();
        let path = d.out.join("answer.txt");
        let output = OutputFile::prepare(&path, None).unwrap();
        output.deliver(&d.records, &d.workdir, false).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn script_deliver_copies_the_work_folder_file() {
        let d = dirs();
        std::fs::write(d.workdir.join(RESULT_FILE), b"GITHUB_TOKEN=absent").unwrap();
        let path = d.out.join("answer.txt");
        let output = OutputFile::prepare(&path, None).unwrap();
        output.deliver(&d.records, &d.workdir, true).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"GITHUB_TOKEN=absent");
    }

    #[test]
    fn script_deliver_refuses_a_linked_result_and_reads_nothing_through_it() {
        let d = dirs();
        let secret = d.out.join("secret");
        std::fs::write(&secret, b"host secret").unwrap();
        std::os::unix::fs::symlink(&secret, d.workdir.join(RESULT_FILE)).unwrap();
        let path = d.out.join("answer.txt");
        let output = OutputFile::prepare(&path, None).unwrap();
        let error = output.deliver(&d.records, &d.workdir, true).unwrap_err();
        assert_io_003(
            &error,
            "the task completed, but its answer could not be written",
        );
        assert!(error
            .hint
            .as_deref()
            .unwrap()
            .contains(&d.workdir.join(RESULT_FILE).display().to_string()));
        assert!(!path.exists());
    }

    #[test]
    fn deliver_without_a_result_file_writes_nothing() {
        let d = dirs();
        let path = d.out.join("answer.txt");
        let output = OutputFile::prepare(&path, None).unwrap();
        output.deliver(&d.records, &d.workdir, false).unwrap();
        output.deliver(&d.records, &d.workdir, true).unwrap();
        assert!(!path.exists());
        no_temp_files(&d.out);
    }

    #[test]
    fn deliver_refuses_a_link_planted_after_prepare() {
        let d = dirs();
        std::fs::write(d.records.join(RESULT_FILE), b"the answer").unwrap();
        let target = d.out.join("target");
        std::fs::write(&target, b"keep").unwrap();
        let path = d.out.join("answer.txt");
        let output = OutputFile::prepare(&path, None).unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        let error = output.deliver(&d.records, &d.workdir, false).unwrap_err();
        assert_io_003(&error, "symbolic link");
        assert!(error
            .hint
            .as_deref()
            .unwrap()
            .contains(&d.records.join(RESULT_FILE).display().to_string()));
        assert_eq!(std::fs::read(&target).unwrap(), b"keep");
        assert!(std::fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn deliver_does_not_recreate_a_removed_parent() {
        let d = dirs();
        std::fs::write(d.records.join(RESULT_FILE), b"the answer").unwrap();
        let elsewhere = d.records.parent().unwrap().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        let path = d.out.join("answer.txt");
        let output = OutputFile::prepare(&path, None).unwrap();
        std::fs::remove_dir_all(&d.out).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &d.out).unwrap();
        let error = output.deliver(&d.records, &d.workdir, false).unwrap_err();
        assert_io_003(&error, "could not be written");
        assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);
    }

    #[test]
    fn deliver_refuses_a_parent_made_read_only() {
        // SAFETY: `geteuid` reads the process's effective uid and has no preconditions.
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("skipped: root writes a read-only directory regardless of its mode");
            return;
        }
        let d = dirs();
        std::fs::write(d.records.join(RESULT_FILE), b"the answer").unwrap();
        let path = d.out.join("answer.txt");
        let output = OutputFile::prepare(&path, None).unwrap();
        std::fs::set_permissions(&d.out, std::fs::Permissions::from_mode(0o555)).unwrap();
        let result = output.deliver(&d.records, &d.workdir, false);
        std::fs::set_permissions(&d.out, std::fs::Permissions::from_mode(0o755)).unwrap();
        let error = result.unwrap_err();
        assert_io_003(&error, "could not be written");
        assert!(error
            .hint
            .as_deref()
            .unwrap()
            .contains(&d.records.join(RESULT_FILE).display().to_string()));
        assert!(!path.exists());
        no_temp_files(&d.out);
    }
}
