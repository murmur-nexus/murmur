//! Compiled forms of staged WASM components, so a warm launch deserializes instead of compiling.
//!
//! A compiled form is native code that `Component::deserialize` maps executable without
//! validating it, so it is only ever read from where mur wrote it: an owner-only file of this uid,
//! in the owner-only directory `~/.murmur/compiled` reached without a symlink, and never from a
//! directory the capsule being staged can write. A form is keyed on the sha256 of the bytes that
//! determine its compile input and on the engine's precompile compatibility hash. Its own sha256
//! sits beside it and is checked before wasmtime sees the bytes. A missing, stale, unreadable or
//! mismatched form is compiled again through `Component::new` and rewritten, so the cache never
//! fails a launch and never changes a compile error.
//!
//! The directory sits beside `~/.murmur/artifacts`, not in it, so nothing that walks the artifact
//! store sees it. Two `mur` builds with different engines keep separate forms, since the engine
//! key is part of every file name; [`prune_stale`] bounds what accumulates.
//!
//! A process that can write as this uid outside the capsule writable root, such as an `advisory`
//! capsule's shell, can plant a form that is loaded on a later launch. That gives it code
//! execution as the same uid, which it already has through `~/.murmur/config.yaml`, shell startup
//! files or the `mur` binary itself.

use std::fs::Metadata;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use wasmtime::component::Component;
use wasmtime::Engine;

/// The directory under the murmur home that holds compiled forms.
const COMPILED_DIR: &str = "compiled";

/// How long an entry of [`COMPILED_DIR`] survives without being read or written.
const FORM_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);

pub(crate) struct CompiledForms {
    /// The directory the staged capsule's writable root is created in. A murmur home or
    /// compiled-forms directory at or beneath it is neither read nor written.
    capsule_writable: PathBuf,
    engine_key: u64,
}

impl CompiledForms {
    pub(crate) fn new(engine: &Engine, capsule_writable: &Path) -> Self {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        engine.precompile_compatibility_hash().hash(&mut hasher);
        Self {
            capsule_writable: capsule_writable.to_path_buf(),
            engine_key: hasher.finish(),
        }
    }

    /// The component for `wasm`. Errors exactly as `Component::new(engine, wasm)` does.
    ///
    /// `key_sha256` is the lowercase hex sha256 of bytes that uniquely determine `wasm`: an
    /// artifact payload staging has verified, or `wasm` itself. A key that is not 64 lowercase hex
    /// characters compiles without touching the cache.
    pub(crate) fn compile(
        &self,
        engine: &Engine,
        key_sha256: &str,
        wasm: &[u8],
    ) -> wasmtime::Result<Component> {
        let is_key = key_sha256.len() == 64
            && key_sha256
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'));
        if !is_key {
            return Component::new(engine, wasm);
        }
        let file_name = format!("{key_sha256}-{:016x}.cwasm", self.engine_key);
        if let Some(dir) = self.dir(false) {
            if let Some(component) = load(engine, &dir.join(&file_name)) {
                return Ok(component);
            }
        }
        let component = Component::new(engine, wasm)?;
        if let (Some(dir), Ok(bytes)) = (self.dir(true), component.serialize()) {
            let path = dir.join(&file_name);
            let _ = crate::murmur_home::write_private_file(&path, &bytes).and_then(|()| {
                crate::murmur_home::write_private_file(
                    &sidecar(&path),
                    murmur_artifact::sha256_hex(&bytes).as_bytes(),
                )
            });
            prune_stale(&dir, SystemTime::now());
        }
        Ok(component)
    }

    /// `~/.murmur/compiled`, or `None` when it cannot be used. Only `create` touches the disk: it
    /// holds the home and the directory owner-only, as every other writer under the home does,
    /// and creates or narrows nothing through a symlink. The home is checked before anything is
    /// created beneath it and the directory again after.
    ///
    /// A directory `create` finds wider than owner-only is narrowed and emptied of everything but
    /// subdirectories: while it was wide, another uid could have planted or hard-linked an entry
    /// that passes every per-file check in [`load`].
    fn dir(&self, create: bool) -> Option<PathBuf> {
        let home = if create {
            crate::murmur_home::ensure_murmur_home().ok()?
        } else {
            crate::state_store::murmur_home_dir().ok()?
        };
        if reachable_from(&home, &self.capsule_writable) {
            return None;
        }
        let dir = home.join(COMPILED_DIR);
        let mut widened = false;
        if create {
            // `hold_private_dir` sets the mode through a symlink, so it only ever sees a real
            // directory or a missing path.
            match std::fs::symlink_metadata(&dir) {
                Ok(metadata) if metadata.is_dir() => widened = !held_private(&dir),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                _ => return None,
            }
            crate::murmur_home::hold_private_dir(&dir).ok()?;
        }
        if !held_private(&dir) || reachable_from(&dir, &self.capsule_writable) {
            return None;
        }
        if widened {
            remove_files(&dir, |_| true);
        }
        Some(dir)
    }
}

/// The file beside a form that records the form's sha256.
fn sidecar(path: &Path) -> PathBuf {
    path.with_extension("cwasm.sha256")
}

/// Whether `dir` is `root` or lies beneath it once both are resolved. A `dir` that cannot be
/// resolved, or a `root` that cannot be resolved for any reason but not existing, counts as
/// reachable.
fn reachable_from(dir: &Path, root: &Path) -> bool {
    match (dir.canonicalize(), root.canonicalize()) {
        (Ok(dir), Ok(root)) => dir.starts_with(root),
        (Ok(_), Err(err)) => err.kind() != std::io::ErrorKind::NotFound,
        (Err(_), _) => true,
    }
}

/// Whether `dir` itself, not a symlink to it, is a directory of this uid in the state
/// `hold_private_dir` leaves it in.
fn held_private(dir: &Path) -> bool {
    std::fs::symlink_metadata(dir).is_ok_and(|metadata| {
        metadata.is_dir()
            && owned_within(
                &metadata,
                current_euid(),
                crate::murmur_home::MURMUR_HOME_DIR_MODE,
            )
    })
}

/// Whether `metadata` belongs to `euid` and grants no permission bit beyond `allowed`.
fn owned_within(metadata: &Metadata, euid: u32, allowed: u32) -> bool {
    use std::os::unix::fs::MetadataExt;

    metadata.uid() == euid && !crate::murmur_home::is_wider_than(metadata.mode(), allowed)
}

#[allow(unsafe_code)]
fn current_euid() -> u32 {
    // SAFETY: `geteuid` takes no arguments, touches no memory of the caller and cannot fail.
    unsafe { libc::geteuid() }
}

/// `path` opened for reading, when it is a regular file of this uid at [`PRIVATE_FILE_MODE`] or
/// narrower. Does not follow a symlink on the final component, does not block on a FIFO, and
/// checks the owner, type and mode on the open handle.
///
/// [`PRIVATE_FILE_MODE`]: crate::murmur_home::PRIVATE_FILE_MODE
fn open_private(path: &Path) -> Option<(std::fs::File, Metadata)> {
    use std::os::unix::fs::OpenOptionsExt;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    let metadata = file.metadata().ok()?;
    (metadata.is_file()
        && owned_within(
            &metadata,
            current_euid(),
            crate::murmur_home::PRIVATE_FILE_MODE,
        ))
    .then_some((file, metadata))
}

/// The form at `path`, when it and its sidecar are owner-only regular files of this uid and the
/// form's bytes match the sidecar.
///
/// Each file's own owner and mode are checked on its open handle, not only the directory's: a
/// directory that was ever wider than owner-only can hold an entry another uid created, and
/// narrowing the directory afterwards does not change that entry's owner.
fn load(engine: &Engine, path: &Path) -> Option<Component> {
    use std::io::Read;

    let (mut file, metadata) = open_private(path)?;
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).ok()?);
    file.read_to_end(&mut bytes).ok()?;
    let mut recorded = String::new();
    open_private(&sidecar(path))?
        .0
        .read_to_string(&mut recorded)
        .ok()?;
    if recorded.trim() != murmur_artifact::sha256_hex(&bytes) {
        return None;
    }
    // SAFETY: `bytes` is an image `Component::serialize` produced for an engine whose
    // precompile compatibility hash keys the file name, and `deserialize` itself refuses an
    // image built by another wasmtime version or an incompatible engine configuration. It was read
    // from an owner-only regular file of this uid, in the owner-only `compiled` directory of this
    // uid that `CompiledForms::dir` only returns when the staged capsule cannot reach it. Its
    // sha256 matched the one recorded beside it when it was written, and `deserialize` copies
    // `bytes`, so a later change to the file cannot reach the loaded code.
    #[allow(unsafe_code)]
    let component = unsafe { Component::deserialize(engine, &bytes) };
    component.ok()
}

/// Removes every direct entry of `dir` that is not a directory and has been neither read nor
/// written in [`FORM_RETENTION`] before `now`, going by the later of its atime and mtime.
///
/// A form in use is read on every launch, which keeps its atime fresh under `relatime`; under
/// `noatime` only the mtime counts, so a form in use is compiled again at most once per retention
/// period. Never recurses, never follows a symlink, and ignores every error.
fn prune_stale(dir: &Path, now: SystemTime) {
    remove_files(dir, |metadata| {
        let last_used = match (metadata.accessed(), metadata.modified()) {
            (Ok(accessed), Ok(modified)) => accessed.max(modified),
            (Ok(used), Err(_)) | (Err(_), Ok(used)) => used,
            (Err(_), Err(_)) => return false,
        };
        now.duration_since(last_used)
            .is_ok_and(|unused| unused > FORM_RETENTION)
    });
}

/// Removes every direct entry of `dir` that is not a directory and whose `lstat` metadata
/// satisfies `remove`. Never recurses, never follows a symlink, and ignores every error.
fn remove_files(dir: &Path, remove: impl Fn(&Metadata) -> bool) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.is_dir() && remove(&metadata) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{FileTimes, Permissions};
    use std::os::unix::fs::PermissionsExt;

    /// The empty component in the binary format: the component preamble alone.
    const EMPTY_COMPONENT: &[u8] = b"\0asm\x0d\x00\x01\x00";

    fn engine() -> Engine {
        let mut config = wasmtime::Config::new();
        config.wasm_component_model(true);
        Engine::new(&config).unwrap()
    }

    /// Writes `bytes` as a compiled form, at the mode and with the sidecar `compile` writes.
    fn write_form(dir: &Path, bytes: &[u8]) -> PathBuf {
        let path = dir.join("form.cwasm");
        std::fs::write(&path, bytes).unwrap();
        std::fs::set_permissions(&path, Permissions::from_mode(0o600)).unwrap();
        std::fs::write(sidecar(&path), murmur_artifact::sha256_hex(bytes)).unwrap();
        std::fs::set_permissions(sidecar(&path), Permissions::from_mode(0o600)).unwrap();
        path
    }

    fn serialized_empty_component(engine: &Engine) -> Vec<u8> {
        Component::new(engine, EMPTY_COMPONENT)
            .unwrap()
            .serialize()
            .unwrap()
    }

    #[test]
    fn load_returns_a_form_whose_sidecar_matches_its_bytes() {
        let engine = engine();
        let temp = tempfile::tempdir().unwrap();
        let path = write_form(temp.path(), &serialized_empty_component(&engine));
        assert!(load(&engine, &path).is_some());
    }

    #[test]
    fn load_refuses_a_form_whose_bytes_differ_from_its_sidecar() {
        let engine = engine();
        let temp = tempfile::tempdir().unwrap();
        let mut bytes = serialized_empty_component(&engine);
        let path = write_form(temp.path(), &bytes);
        let middle = bytes.len() / 2;
        bytes[middle] ^= 0xff;
        std::fs::write(&path, &bytes).unwrap();
        assert!(load(&engine, &path).is_none());
    }

    #[test]
    fn load_refuses_a_form_with_no_sidecar() {
        let engine = engine();
        let temp = tempfile::tempdir().unwrap();
        let path = write_form(temp.path(), &serialized_empty_component(&engine));
        std::fs::remove_file(sidecar(&path)).unwrap();
        assert!(load(&engine, &path).is_none());
    }

    #[test]
    fn load_refuses_a_form_readable_beyond_its_owner() {
        let engine = engine();
        let temp = tempfile::tempdir().unwrap();
        let path = write_form(temp.path(), &serialized_empty_component(&engine));
        std::fs::set_permissions(&path, Permissions::from_mode(0o640)).unwrap();
        assert!(load(&engine, &path).is_none());
    }

    #[test]
    fn load_refuses_a_form_whose_sidecar_is_readable_beyond_its_owner() {
        let engine = engine();
        let temp = tempfile::tempdir().unwrap();
        let path = write_form(temp.path(), &serialized_empty_component(&engine));
        std::fs::set_permissions(sidecar(&path), Permissions::from_mode(0o644)).unwrap();
        assert!(load(&engine, &path).is_none());
    }

    #[test]
    fn load_refuses_a_form_reached_through_a_symlink() {
        let engine = engine();
        let temp = tempfile::tempdir().unwrap();
        let path = write_form(temp.path(), &serialized_empty_component(&engine));
        let link = temp.path().join("link.cwasm");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        std::fs::copy(sidecar(&path), sidecar(&link)).unwrap();
        assert!(load(&engine, &link).is_none());
    }

    #[test]
    fn owned_within_refuses_an_owner_other_than_the_expected_uid() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("form.cwasm");
        std::fs::write(&path, b"form").unwrap();
        std::fs::set_permissions(&path, Permissions::from_mode(0o600)).unwrap();
        let metadata = std::fs::metadata(&path).unwrap();

        assert!(owned_within(&metadata, current_euid(), 0o600));
        assert!(!owned_within(
            &metadata,
            current_euid().wrapping_add(1),
            0o600
        ));
    }

    #[test]
    fn held_private_accepts_only_an_owner_only_directory_reached_without_a_symlink() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("forms");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, Permissions::from_mode(0o700)).unwrap();
        assert!(held_private(&dir));

        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        assert!(!held_private(&link));

        std::fs::set_permissions(&dir, Permissions::from_mode(0o750)).unwrap();
        assert!(!held_private(&dir));
    }

    #[test]
    fn a_directory_beneath_the_capsule_workdir_is_reachable_from_it() {
        let temp = tempfile::tempdir().unwrap();
        let workdir = temp.path().join("work");
        let inside = workdir.join(".murmur");
        let outside = temp.path().join("home");
        std::fs::create_dir_all(&inside).unwrap();
        std::fs::create_dir(&outside).unwrap();

        assert!(reachable_from(&workdir, &workdir));
        assert!(reachable_from(&inside, &workdir));
        assert!(!reachable_from(&outside, &workdir));
        // A workdir staging has not created yet cannot contain an existing directory.
        assert!(!reachable_from(&outside, &temp.path().join("not-created")));
        // A directory that cannot be resolved is never used.
        assert!(reachable_from(&temp.path().join("missing"), &workdir));
    }

    #[test]
    fn prune_removes_only_entries_unused_for_the_retention_period() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let now = SystemTime::now();
        let day = Duration::from_secs(24 * 60 * 60);
        let stale = now - 31 * day;
        let set_times = |path: &Path, accessed: SystemTime, modified: SystemTime| {
            std::fs::File::open(path)
                .unwrap()
                .set_times(
                    FileTimes::new()
                        .set_accessed(accessed)
                        .set_modified(modified),
                )
                .unwrap();
        };

        let unused = dir.join("a.cwasm");
        let read_recently = dir.join("b.cwasm");
        let fresh = dir.join("c.cwasm");
        let subdir = dir.join("d");
        std::fs::write(&unused, b"a").unwrap();
        std::fs::write(&read_recently, b"b").unwrap();
        std::fs::write(&fresh, b"c").unwrap();
        std::fs::create_dir(&subdir).unwrap();
        set_times(&unused, stale, stale);
        set_times(&read_recently, now - day, stale);
        set_times(&subdir, stale, stale);

        prune_stale(dir, now);

        assert!(!unused.exists());
        assert!(read_recently.exists());
        assert!(fresh.exists());
        assert!(subdir.is_dir());
    }

    #[test]
    fn compile_errors_exactly_as_component_new_and_writes_nothing() {
        let home = tempfile::tempdir().unwrap();
        crate::murmur_home::run_with_home(
            "compiled_forms::tests::inner_compile_errors_exactly_as_component_new",
            home.path(),
        );
        let compiled = home.path().join(".murmur").join(COMPILED_DIR);
        let forms = std::fs::read_dir(&compiled)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "cwasm"))
                    .count()
            })
            .unwrap_or(0);
        assert_eq!(forms, 0, "{} holds a form", compiled.display());
    }

    #[test]
    #[ignore = "run by compile_errors_exactly_as_component_new_and_writes_nothing"]
    fn inner_compile_errors_exactly_as_component_new() {
        if !crate::murmur_home::in_scratch_home() {
            return;
        }
        let engine = engine();
        let workdir = tempfile::tempdir().unwrap();
        let forms = CompiledForms::new(&engine, workdir.path());
        let wasm: &[u8] = b"\0asm\x01\0\0\0";
        let key = murmur_artifact::sha256_hex(wasm);

        let cached = forms
            .compile(&engine, &key, wasm)
            .err()
            .expect("compile fails");
        let direct = Component::new(&engine, wasm)
            .err()
            .expect("Component::new fails");
        assert_eq!(cached.to_string(), direct.to_string());
    }
}
