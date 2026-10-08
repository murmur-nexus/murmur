//! The per-run scratch directory every case is staged under.
//!
//! A case is graded on files it finds at fixed paths under its case directory — the probe's
//! verdict file, the driver's summary, every `.murmur/` session's trace and bootstrap log. A file
//! an earlier run left at one of those paths grades the new run on old evidence, so a work root is
//! emptied before every run that uses it. The marker file is what licenses that: the harness only
//! ever clears a directory it created or adopted itself.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Written into every work root the harness creates or adopts. A non-empty directory without it
/// is never cleared.
pub const WORK_ROOT_MARKER: &str = ".escape-conformance-work-root";

/// Makes `path` an empty work root carrying [`WORK_ROOT_MARKER`] and returns it canonicalized.
///
/// A missing path or an empty directory is created and marked. A directory that carries the marker
/// has everything but the marker removed. A non-empty directory without the marker is left
/// untouched and refused with an error naming the `rm -rf` that clears it, as is a path that is
/// not a directory. The path is canonical on return because each case's `mur run` is spawned with
/// its own capsule workdir as cwd, so a relative scratch path would resolve against the wrong
/// directory.
pub fn prepare_work_root(path: &Path) -> Result<PathBuf, String> {
    let shown = path.display();
    match fs::symlink_metadata(path) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(path).map_err(|err| format!("could not create {shown}: {err}"))?;
        }
        Err(err) => return Err(format!("could not inspect {shown}: {err}")),
        Ok(_) if !path.is_dir() => {
            return Err(format!(
                "work root {shown} exists and is not a directory; pass an empty directory or a \
                 path that does not exist yet"
            ));
        }
        Ok(_) => {}
    }
    let root = path
        .canonicalize()
        .map_err(|err| format!("could not resolve {shown}: {err}"))?;

    let entries: Vec<fs::DirEntry> = fs::read_dir(&root)
        .and_then(|dir| dir.collect::<io::Result<_>>())
        .map_err(|err| format!("could not list {}: {err}", root.display()))?;
    let marked = entries
        .iter()
        .any(|entry| entry.file_name() == WORK_ROOT_MARKER);
    if !entries.is_empty() && !marked {
        return Err(format!(
            "work root {root} is not empty and was not created by escape-conformance (it has no \
             {WORK_ROOT_MARKER} file), so it is left untouched: files already in it could be read \
             as this run's evidence. Clear it with `rm -rf {quoted}`, or pass an empty directory \
             or a path that does not exist yet.",
            root = root.display(),
            quoted = shell_quote(&root),
        ));
    }

    for entry in entries {
        if entry.file_name() == WORK_ROOT_MARKER {
            continue;
        }
        let entry_path = entry.path();
        // `file_type` does not follow symlinks, so a link to a directory outside the work root is
        // removed as a link and its target is never entered.
        let removed = match entry.file_type() {
            Ok(kind) if kind.is_dir() => fs::remove_dir_all(&entry_path),
            Ok(_) => fs::remove_file(&entry_path),
            Err(err) => Err(err),
        };
        removed.map_err(|err| {
            format!(
                "could not clear {} from the work root: {err}",
                entry_path.display()
            )
        })?;
    }
    if !marked {
        fs::write(root.join(WORK_ROOT_MARKER), "")
            .map_err(|err| format!("could not mark {} as a work root: {err}", root.display()))?;
    }
    Ok(root)
}

/// `path` as one POSIX shell word: bare when it holds only characters no shell treats specially,
/// single-quoted otherwise.
fn shell_quote(path: &Path) -> String {
    let text = path.display().to_string();
    let plain = text
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "/._-+,:@%".contains(c));
    if plain && !text.is_empty() {
        text
    } else {
        format!("'{}'", text.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A path under the system temp dir that does not exist yet and is unique to this call.
    pub(crate) fn scratch_path(label: &str) -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        std::env::temp_dir().join(format!(
            "escape-conformance-{label}-{}-{nanos}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn a_missing_work_root_is_created_and_marked() {
        let path = scratch_path("missing");
        let root = prepare_work_root(&path).expect("a missing path is a fresh work root");
        assert!(root.is_absolute());
        assert!(root.join(WORK_ROOT_MARKER).is_file());
        assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_empty_directory_is_adopted_and_marked() {
        let path = scratch_path("empty");
        fs::create_dir_all(&path).unwrap();
        let root = prepare_work_root(&path).expect("an empty directory is adopted");
        assert!(root.join(WORK_ROOT_MARKER).is_file());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_non_empty_work_root_without_the_marker_is_refused_with_the_rm_command() {
        let path = scratch_path("unmarked");
        fs::create_dir_all(path.join("someone-elses")).unwrap();
        fs::write(path.join("notes.txt"), "keep me").unwrap();
        fs::write(path.join("someone-elses").join("data"), "and me").unwrap();

        let err = prepare_work_root(&path).expect_err("an unmarked non-empty directory");
        let canonical = path.canonicalize().unwrap();
        assert!(
            err.contains(&format!("rm -rf {}", canonical.display())),
            "the refusal must name the command that clears it: {err}"
        );
        assert_eq!(
            fs::read_to_string(path.join("notes.txt")).unwrap(),
            "keep me"
        );
        assert_eq!(
            fs::read_to_string(path.join("someone-elses").join("data")).unwrap(),
            "and me"
        );
        assert!(!path.join(WORK_ROOT_MARKER).exists());
        fs::remove_dir_all(&path).unwrap();
    }

    #[test]
    fn a_file_is_not_a_work_root() {
        let path = scratch_path("file");
        fs::write(&path, "not a directory").unwrap();
        assert!(prepare_work_root(&path).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "not a directory");
        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_path_with_shell_metacharacters_is_quoted() {
        assert_eq!(shell_quote(Path::new("/tmp/a-b_c.1")), "/tmp/a-b_c.1");
        assert_eq!(shell_quote(Path::new("/tmp/a b")), "'/tmp/a b'");
        assert_eq!(shell_quote(Path::new("/tmp/it's")), r"'/tmp/it'\''s'");
    }
}
