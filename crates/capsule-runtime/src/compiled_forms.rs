//! Compiled forms of staged WASM components, so a warm launch deserializes instead of compiling.
//!
//! A compiled form is native code that `Component::deserialize` maps executable without
//! validating it, so it is only ever read from where mur wrote it: an owner-only file of this uid,
//! in the owner-only directory `~/.murmur/compiled` reached without a symlink, and never from a
//! directory the capsule being staged can write. A form is keyed on the sha256 of the bytes that
//! determine its compile input, the engine's precompile compatibility hash and the artifact
//! decompression ceiling ([`max_artifact_decompressed_bytes`]). Its own sha256 sits beside it, and
//! its bytes are hashed against that sha256 when the form is written and again whenever the file's
//! [`stamp`] has moved since they last matched; [`read_verified`] states the rule. A missing,
//! stale, unreadable or mismatched form is compiled again through `Component::new` and rewritten,
//! so the cache never fails a launch and never changes a compile error.
//!
//! The sidecar and the stamp it records sit in the same directory as the form, so they detect
//! damage, not a rewrite by code running as this uid, which can rewrite all three together. A
//! `sealed` or `scoped` capsule cannot write under the murmur home, and an `advisory` capsule
//! already runs with the operator's trust. Trusting an unchanged stamp gives up one thing:
//! corruption below the filesystem that leaves the inode's metadata alone, such as bit rot on a
//! filesystem without data checksums like ext4, goes unnoticed until the stamp next moves. On Linux
//! before 6.13, ctime has jiffy resolution, so a same-size rewrite by another writer within one
//! tick of the stamp being taken could keep it; mur's own writes always rename a new inode into
//! place.
//!
//! An artifact's key must be a sha256 that `verify_sha256` has just recomputed over the payload
//! bytes in hand, so a form named `H` is always the `extract_root_wasm` component of the payload
//! whose bytes hash to `H`, for this engine. `runtime::stage_session` and `manage::Host::pull`
//! both key this way and share the one [`CompiledForms`] handle staging built for the session.
//! What accepts a payload differs, the `murmur.lock` pin on staging and the registry's reported
//! hash on a pull, but never what the key names. A form either caller writes is therefore correct
//! for any later caller that has accepted payload `H`, and loading it grants that payload nothing
//! its caller had not.
//!
//! The directory sits beside `~/.murmur/artifacts`, not in it, so nothing that walks the artifact
//! store sees it. Two `mur` builds with different engines, or two decompression ceilings, keep
//! separate forms, since the engine key is part of every file name.
//!
//! Only a store prunes. The store branch of [`CompiledForms::compile`] is the one caller of
//! [`prune_stale`], so after a store the directory holds only forms read or written in the
//! [`FORM_RETENTION`] before it, and a launch whose every form loads writes, renames and deletes
//! nothing here. A change of engine key, from a new wasmtime or a changed decompression ceiling,
//! finds none of its forms, so its first launch stores and always prunes; the previous key's forms
//! are removed by the first store after they have gone unused for [`FORM_RETENTION`]. Pruning goes
//! by age alone, never by key, so two builds or ceilings used alternately never delete each
//! other's forms and each keeps loading its own.
//!
//! A form keyed on an artifact payload's sha256 must be compiled only from the root wasm
//! [`extract_root_wasm`] returned for that payload in the same process, so under the same ceiling.
//! Whether that extraction succeeds is fixed by the payload bytes and the ceiling, so a form found
//! under a payload key stands for the payload's successful extraction under that ceiling. That is
//! what lets a caller of [`CompiledForms::load`] skip the inflate.
//!
//! [`max_artifact_decompressed_bytes`]: murmur_artifact::zip_guard::max_artifact_decompressed_bytes
//! [`extract_root_wasm`]: crate::artifact::extract_root_wasm
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

/// How long a staged write's temp file survives. `write_private_file` writes, fsyncs and renames
/// its `.<name>.<pid>.<uuid>.tmp` file within the one call, so a temp file an hour old was left by
/// a write that crashed or failed before the rename.
const TEMP_RETENTION: Duration = Duration::from_secs(60 * 60);

#[derive(Clone)]
pub(crate) struct CompiledForms {
    /// The directory the staged capsule's writable root is created in. A murmur home or
    /// compiled-forms directory at or beneath it is neither read nor written.
    capsule_writable: PathBuf,
    engine_key: u64,
}

impl CompiledForms {
    pub(crate) fn new(engine: &Engine, capsule_writable: &Path) -> Self {
        Self::with_ceiling(
            engine,
            capsule_writable,
            murmur_artifact::zip_guard::max_artifact_decompressed_bytes(),
        )
    }

    /// Forms for an engine staging under the artifact decompression ceiling `ceiling`. Each
    /// ceiling keys its own forms, so a lowered ceiling never finds a form whose payload only
    /// extracts under a higher one.
    fn with_ceiling(engine: &Engine, capsule_writable: &Path, ceiling: u64) -> Self {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        engine.precompile_compatibility_hash().hash(&mut hasher);
        ceiling.hash(&mut hasher);
        Self {
            capsule_writable: capsule_writable.to_path_buf(),
            engine_key: hasher.finish(),
        }
    }

    /// The stored form for `key_sha256`, or `None` when the key is not 64 lowercase hex
    /// characters or no usable form is stored under it. Creates nothing on disk.
    pub(crate) fn load(&self, engine: &Engine, key_sha256: &str) -> Option<Component> {
        if !is_key(key_sha256) {
            return None;
        }
        let dir = self.dir(false)?;
        load(engine, &dir.join(self.file_name(key_sha256)))
    }

    /// The component for `wasm`: the form stored under `key_sha256` when [`Self::load`] finds
    /// one, and otherwise `wasm` compiled and stored. Errors exactly as
    /// `Component::new(engine, wasm)` does.
    ///
    /// `key_sha256` is the lowercase hex sha256 of bytes that uniquely determine `wasm`: an
    /// artifact payload the caller has verified, or `wasm` itself. A key that is not 64 lowercase hex
    /// characters compiles without touching the cache.
    pub(crate) fn compile(
        &self,
        engine: &Engine,
        key_sha256: &str,
        wasm: &[u8],
    ) -> wasmtime::Result<Component> {
        if !is_key(key_sha256) {
            return Component::new(engine, wasm);
        }
        if let Some(component) = self.load(engine, key_sha256) {
            return Ok(component);
        }
        let component = Component::new(engine, wasm)?;
        if let (Some(dir), Ok(bytes)) = (self.dir(true), component.serialize()) {
            store(&dir, &self.file_name(key_sha256), &bytes);
            prune_stale(&dir, SystemTime::now());
        }
        Ok(component)
    }

    /// The name of the form stored under `key_sha256`: `<key>-<engine key>.cwasm`, the engine key
    /// as 16 lowercase hex digits.
    fn file_name(&self, key_sha256: &str) -> String {
        format!("{key_sha256}-{:016x}.cwasm", self.engine_key)
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
            for (path, _) in files(&dir) {
                let _ = std::fs::remove_file(path);
            }
        }
        Some(dir)
    }
}

/// Whether `key_sha256` is 64 lowercase hex characters, the only shape a form is stored under.
fn is_key(key_sha256: &str) -> bool {
    key_sha256.len() == 64
        && key_sha256
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
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
    let (file, metadata) = crate::resource_plane::open_no_follow(path).ok()?;
    (metadata.is_file()
        && owned_within(
            &metadata,
            current_euid(),
            crate::murmur_home::PRIVATE_FILE_MODE,
        ))
    .then_some((file, metadata))
}

/// Writes the form `bytes` as `file_name` in `dir` and a sidecar holding their sha256, then reads
/// the form back through [`read_verified`], which hashes it once more and records its [`stamp`].
/// `dir` is one [`CompiledForms::dir`] returned. Every error is ignored: a form or sidecar that
/// was not written is compiled again on the next launch, and a stamp that was not recorded means
/// only that the next launch hashes the form.
fn store(dir: &Path, file_name: &str, bytes: &[u8]) {
    let path = dir.join(file_name);
    let written = crate::murmur_home::write_private_file(&path, bytes).and_then(|()| {
        crate::murmur_home::write_private_file(
            &sidecar(&path),
            murmur_artifact::sha256_hex(bytes).as_bytes(),
        )
    });
    if written.is_ok() {
        let _ = read_verified(&path);
    }
}

/// The bytes of the form at `path`, when it and its sidecar are owner-only regular files of this
/// uid and the bytes are the ones whose sha256 was recorded when the form was written.
///
/// Sidecar line 1 is that write-time sha256. Line 2, when present, is the form's [`stamp`] at the
/// last read whose bytes hashed to it. A form whose stamp is the same before and after this read,
/// and equal to line 2, is taken without hashing. Any other form is hashed in full: a mismatch is
/// `None`, and a match whose stamp held still during the read has the sidecar rewritten as
/// `<sha256>\n<stamp>\n`. Lines after the second are ignored. This is the only write here, and it
/// goes into the directory `path` is in, through `write_private_file`; a failed write is ignored
/// and leaves the next read to hash again.
///
/// Each file's own owner and mode are checked on its open handle, not only the directory's: a
/// directory that was ever wider than owner-only can hold an entry another uid created, and
/// narrowing the directory afterwards does not change that entry's owner.
fn read_verified(path: &Path) -> Option<Vec<u8>> {
    use std::io::Read;

    let (mut file, before) = open_private(path)?;
    let mut bytes = Vec::with_capacity(usize::try_from(before.len()).ok()?);
    file.read_to_end(&mut bytes).ok()?;
    let mut recorded = String::new();
    open_private(&sidecar(path))?
        .0
        .read_to_string(&mut recorded)
        .ok()?;
    let mut lines = recorded.lines();
    let sha256 = lines.next()?.trim();
    // A write that lands while the bytes are read moves the stamp between the two fstat calls.
    let unchanged = Some(stamp(&before))
        .filter(|before| file.metadata().is_ok_and(|after| stamp(&after) == *before));
    if unchanged.is_some() && lines.next() == unchanged.as_deref() {
        return Some(bytes);
    }
    if murmur_artifact::sha256_hex(&bytes) != sha256 {
        return None;
    }
    if let Some(unchanged) = unchanged {
        // Pairs a sha256 only with the stamp of an inode whose bytes, read while that stamp held
        // still, hashed to it. A concurrent `store` that renames a new form into place moves the
        // form's stamp away from this one, so this write landing over its sidecar costs at most
        // one more hash or one recompile, never unhashed bytes.
        let _ = crate::murmur_home::write_private_file(
            &sidecar(path),
            format!("{sha256}\n{unchanged}\n").as_bytes(),
        );
    }
    Some(bytes)
}

/// The identity and change state of the file `metadata` describes: device, inode, size, owner,
/// mode, mtime and ctime. The kernel sets ctime to the current time on every write, truncate,
/// chmod, chown and rename of the inode, and no call made without `CAP_SYS_TIME` sets it back, so
/// the stamp of a file changed or replaced since it was taken differs from it, even when the
/// mtime has been restored.
fn stamp(metadata: &Metadata) -> String {
    use std::os::unix::fs::MetadataExt;

    format!(
        "{} {} {} {} {:o} {}.{:09} {}.{:09}",
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.uid(),
        metadata.mode(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ctime(),
        metadata.ctime_nsec(),
    )
}

/// The form at `path`, read through [`read_verified`].
fn load(engine: &Engine, path: &Path) -> Option<Component> {
    let bytes = read_verified(path)?;
    // SAFETY: `bytes` is an image `Component::serialize` produced for an engine whose
    // precompile compatibility hash keys the file name, and `deserialize` itself refuses an
    // image built by another wasmtime version or an incompatible engine configuration. It was read
    // from an owner-only regular file of this uid, in the owner-only `compiled` directory of this
    // uid that `CompiledForms::dir` only returns when the staged capsule cannot reach it. The
    // bytes hashed to the sha256 recorded beside the file when it was written, either in this read
    // or in an earlier one since which the file's stamp (device, inode, size, owner, mode, mtime
    // and ctime) has not moved. `deserialize` copies `bytes`, so a later change to the file cannot
    // reach the loaded code.
    #[allow(unsafe_code)]
    let component = unsafe { Component::deserialize(engine, &bytes) };
    component.ok()
}

/// Removes each direct entry of `dir` that has gone unused for its retention period, going by the
/// later of its atime and mtime:
///
/// - a directory is never removed;
/// - a form, named `*.cwasm`, is removed when neither it nor its sidecar has been used in
///   [`FORM_RETENTION`], and its sidecar is removed after it. A sidecar whose form exists goes only
///   with that form, so a form in use never loses its sidecar to the sidecar's own age;
/// - a staged-write temp file, named `.*.tmp`, is removed after [`TEMP_RETENTION`];
/// - anything else, a sidecar whose form is gone included, is removed after [`FORM_RETENTION`].
///
/// The only caller is the store branch of [`CompiledForms::compile`], on the directory it has just
/// written a form to. A form in use is read on every launch, which keeps its atime fresh under
/// `relatime`; under `noatime` only the mtime counts, so a form in use is compiled again at most
/// once per retention period. Never recurses, never follows a symlink, and ignores every error.
fn prune_stale(dir: &Path, now: SystemTime) {
    use std::os::unix::ffi::OsStrExt;

    let unused = |used: Option<SystemTime>, retention: Duration| {
        used.is_some_and(|used| {
            now.duration_since(used)
                .is_ok_and(|unused| unused > retention)
        })
    };
    for (path, metadata) in files(dir) {
        let Some(name) = path.file_name().map(OsStrExt::as_bytes) else {
            continue;
        };
        if name.ends_with(b".cwasm") {
            let sidecar = sidecar(&path);
            let sidecar_metadata = std::fs::symlink_metadata(&sidecar)
                .ok()
                .filter(|metadata| !metadata.is_dir());
            let used = last_used(&metadata).max(sidecar_metadata.as_ref().and_then(last_used));
            if unused(used, FORM_RETENTION)
                && std::fs::remove_file(&path).is_ok()
                && sidecar_metadata.is_some()
            {
                let _ = std::fs::remove_file(&sidecar);
            }
            continue;
        }
        if is_sidecar_of_a_form(&path) {
            continue;
        }
        let retention = if name.starts_with(b".") && name.ends_with(b".tmp") {
            TEMP_RETENTION
        } else {
            FORM_RETENTION
        };
        if unused(last_used(&metadata), retention) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Whether `path` is the [`sidecar`] of a form that exists and is not a directory.
fn is_sidecar_of_a_form(path: &Path) -> bool {
    let form = path.with_extension("");
    sidecar(&form) == path
        && std::fs::symlink_metadata(&form).is_ok_and(|metadata| !metadata.is_dir())
}

/// The later of `metadata`'s atime and mtime, or `None` when neither can be read.
fn last_used(metadata: &Metadata) -> Option<SystemTime> {
    match (metadata.accessed(), metadata.modified()) {
        (Ok(accessed), Ok(modified)) => Some(accessed.max(modified)),
        (Ok(used), Err(_)) | (Err(_), Ok(used)) => Some(used),
        (Err(_), Err(_)) => None,
    }
}

/// Every direct entry of `dir` that is not a directory, with its `lstat` metadata. Never recurses,
/// never follows a symlink, and skips an entry it cannot read.
fn files(dir: &Path) -> impl Iterator<Item = (PathBuf, Metadata)> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path).ok()?;
            (!metadata.is_dir()).then_some((path, metadata))
        })
}

/// The empty component in the binary format: the component preamble alone.
#[cfg(test)]
pub(crate) const EMPTY_COMPONENT: &[u8] = b"\0asm\x0d\x00\x01\x00";

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{FileTimes, Permissions};
    use std::os::unix::fs::PermissionsExt;

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

    fn set_times(path: &Path, accessed: SystemTime, modified: SystemTime) {
        std::fs::File::open(path)
            .unwrap()
            .set_times(
                FileTimes::new()
                    .set_accessed(accessed)
                    .set_modified(modified),
            )
            .unwrap();
    }

    fn inode(path: &Path) -> u64 {
        use std::os::unix::fs::MetadataExt;
        std::fs::symlink_metadata(path)
            .unwrap_or_else(|err| panic!("{}: {err}", path.display()))
            .ino()
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

    /// Replaces the sha256 in `path`'s sidecar with one no bytes hash to and keeps the stamp line,
    /// so only an unchanged stamp can still admit the form.
    fn break_recorded_sha256(path: &Path) {
        let recorded = std::fs::read_to_string(sidecar(path)).unwrap();
        let stamp = recorded.lines().nth(1).unwrap_or_default();
        assert!(!stamp.is_empty(), "no stamp recorded: {recorded:?}");
        std::fs::write(sidecar(path), format!("{}\n{stamp}\n", "0".repeat(64))).unwrap();
    }

    /// Line 2 of `path`'s sidecar.
    fn recorded_stamp(path: &Path) -> String {
        let recorded = std::fs::read_to_string(sidecar(path)).unwrap();
        recorded.lines().nth(1).unwrap_or_default().to_owned()
    }

    fn current_stamp(path: &Path) -> String {
        stamp(&std::fs::metadata(path).unwrap())
    }

    #[test]
    fn store_records_the_stamp_of_the_bytes_it_wrote() {
        let engine = engine();
        let temp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(temp.path(), Permissions::from_mode(0o700)).unwrap();
        let bytes = serialized_empty_component(&engine);
        store(temp.path(), "form.cwasm", &bytes);

        let path = temp.path().join("form.cwasm");
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(
            std::fs::read_to_string(sidecar(&path)).unwrap(),
            format!(
                "{}\n{}\n",
                murmur_artifact::sha256_hex(&bytes),
                current_stamp(&path)
            ),
        );
    }

    #[test]
    fn a_file_whose_stamp_is_unchanged_is_taken_without_hashing() {
        let engine = engine();
        let temp = tempfile::tempdir().unwrap();
        let path = write_form(temp.path(), &serialized_empty_component(&engine));
        assert!(read_verified(&path).is_some());
        break_recorded_sha256(&path);
        assert!(load(&engine, &path).is_some());
    }

    #[test]
    fn a_file_rewritten_since_its_stamp_is_hashed_again() {
        let engine = engine();
        let temp = tempfile::tempdir().unwrap();
        let bytes = serialized_empty_component(&engine);
        let path = write_form(temp.path(), &bytes);
        assert!(read_verified(&path).is_some());
        break_recorded_sha256(&path);
        std::fs::write(&path, [bytes.as_slice(), b"\0"].concat()).unwrap();
        assert!(read_verified(&path).is_none());
    }

    #[test]
    fn a_file_rewritten_in_place_with_its_times_restored_is_hashed_again() {
        use std::os::unix::fs::{FileExt, MetadataExt};

        let engine = engine();
        let temp = tempfile::tempdir().unwrap();
        let bytes = serialized_empty_component(&engine);
        let path = write_form(temp.path(), &bytes);
        assert!(read_verified(&path).is_some());
        break_recorded_sha256(&path);
        let earlier = std::fs::metadata(&path).unwrap();

        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        let middle = bytes.len() / 2;
        file.write_all_at(&[!bytes[middle]], middle as u64).unwrap();
        file.set_times(
            FileTimes::new()
                .set_accessed(earlier.accessed().unwrap())
                .set_modified(earlier.modified().unwrap()),
        )
        .unwrap();
        drop(file);

        let later = std::fs::metadata(&path).unwrap();
        assert_eq!(later.ino(), earlier.ino());
        assert_eq!(later.len(), earlier.len());
        assert_eq!(later.modified().unwrap(), earlier.modified().unwrap());
        assert!(read_verified(&path).is_none());
    }

    #[test]
    fn a_file_replaced_since_its_stamp_is_hashed_again() {
        let engine = engine();
        let temp = tempfile::tempdir().unwrap();
        let bytes = serialized_empty_component(&engine);
        let path = write_form(temp.path(), &bytes);
        assert!(read_verified(&path).is_some());
        break_recorded_sha256(&path);
        let replacement = temp.path().join("replacement");
        std::fs::write(&replacement, &bytes).unwrap();
        std::fs::set_permissions(&replacement, Permissions::from_mode(0o600)).unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        assert!(read_verified(&path).is_none());
    }

    #[test]
    fn a_file_chmodded_since_its_stamp_is_hashed_again() {
        let engine = engine();
        let temp = tempfile::tempdir().unwrap();
        let path = write_form(temp.path(), &serialized_empty_component(&engine));
        assert!(read_verified(&path).is_some());
        break_recorded_sha256(&path);
        std::fs::set_permissions(&path, Permissions::from_mode(0o400)).unwrap();
        assert!(read_verified(&path).is_none());
    }

    #[test]
    fn a_touched_file_whose_bytes_still_match_is_restamped() {
        let engine = engine();
        let temp = tempfile::tempdir().unwrap();
        let path = write_form(temp.path(), &serialized_empty_component(&engine));
        assert!(read_verified(&path).is_some());
        let stamped = recorded_stamp(&path);
        std::fs::File::open(&path)
            .unwrap()
            .set_times(FileTimes::new().set_modified(SystemTime::now()))
            .unwrap();
        assert_ne!(current_stamp(&path), stamped);

        assert!(read_verified(&path).is_some());
        assert_eq!(recorded_stamp(&path), current_stamp(&path));
    }

    #[test]
    fn a_digest_only_sidecar_is_hashed_and_stamped() {
        let engine = engine();
        let temp = tempfile::tempdir().unwrap();
        let bytes = serialized_empty_component(&engine);
        let path = write_form(temp.path(), &bytes);
        assert_eq!(recorded_stamp(&path), "");

        assert!(read_verified(&path).is_some());
        assert_eq!(recorded_stamp(&path), current_stamp(&path));
        assert_eq!(
            std::fs::read_to_string(sidecar(&path))
                .unwrap()
                .lines()
                .next(),
            Some(murmur_artifact::sha256_hex(&bytes).as_str()),
        );
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
    fn the_form_key_carries_the_decompression_ceiling() {
        let engine = engine();
        let workdir = Path::new("/nonexistent-workdir");
        let sha = murmur_artifact::sha256_hex(EMPTY_COMPONENT);
        let name = |ceiling| CompiledForms::with_ceiling(&engine, workdir, ceiling).file_name(&sha);

        assert_ne!(name(1_000_000), name(2_000_000));
        assert_eq!(name(1_000_000), name(1_000_000));
        let engine_key = name(1_000_000)
            .strip_prefix(&format!("{sha}-"))
            .and_then(|rest| rest.strip_suffix(".cwasm"))
            .map(str::to_owned)
            .expect("<sha>-<engine key>.cwasm");
        assert_eq!(engine_key.len(), 16);
        assert!(engine_key
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')));
        assert_eq!(
            CompiledForms::new(&engine, workdir).file_name(&sha),
            name(murmur_artifact::zip_guard::max_artifact_decompressed_bytes()),
        );
    }

    #[test]
    fn load_returns_only_a_form_stored_under_the_same_key_and_ceiling() {
        let home = tempfile::tempdir().unwrap();
        crate::murmur_home::run_with_home(
            "compiled_forms::tests::inner_load_returns_only_a_form_stored_under_the_same_key_and_ceiling",
            home.path(),
        );
    }

    #[test]
    #[ignore = "run by load_returns_only_a_form_stored_under_the_same_key_and_ceiling"]
    fn inner_load_returns_only_a_form_stored_under_the_same_key_and_ceiling() {
        if !crate::murmur_home::in_scratch_home() {
            return;
        }
        let engine = engine();
        let workdir = tempfile::tempdir().unwrap();
        let ceiling = 1_000_000;
        let forms = CompiledForms::with_ceiling(&engine, workdir.path(), ceiling);
        let sha = murmur_artifact::sha256_hex(b"a payload");
        let compiled = crate::state_store::murmur_home_dir()
            .unwrap()
            .join(COMPILED_DIR);

        assert!(forms.load(&engine, &sha).is_none());
        assert!(!compiled.exists(), "{} was created", compiled.display());

        forms.compile(&engine, &sha, EMPTY_COMPONENT).unwrap();
        assert!(forms.load(&engine, &sha).is_some());
        assert!(
            CompiledForms::with_ceiling(&engine, workdir.path(), ceiling + 1)
                .load(&engine, &sha)
                .is_none()
        );
        assert!(forms.load(&engine, &sha.to_uppercase()).is_none());
        assert!(forms.load(&engine, "not-a-key").is_none());
        // This wasm fails `Component::new`, so `Ok` is the stored form, found by `compile` itself.
        assert!(forms.compile(&engine, &sha, b"\0asm\x01\0\0\0").is_ok());
    }

    #[test]
    fn prune_treats_a_form_and_its_sidecar_as_one_set() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let now = SystemTime::now();
        let day = Duration::from_secs(24 * 60 * 60);
        let (stale, recent) = (now - 31 * day, now - day);
        let entry = |name: &str, accessed: SystemTime, modified: SystemTime| {
            let path = dir.join(name);
            std::fs::write(&path, name).unwrap();
            set_times(&path, accessed, modified);
            path
        };

        let unused = [
            entry("1.cwasm", stale, stale),
            entry("1.cwasm.sha256", stale, stale),
        ];
        let sidecar_read = [
            entry("2.cwasm", stale, stale),
            entry("2.cwasm.sha256", recent, stale),
        ];
        let form_read = [
            entry("3.cwasm", recent, stale),
            entry("3.cwasm.sha256", stale, stale),
        ];
        let stale_orphan = entry("4.cwasm.sha256", stale, stale);
        let recent_orphan = entry("5.cwasm.sha256", recent, recent);

        prune_stale(dir, now);

        for path in unused.iter().chain([&stale_orphan]) {
            assert!(!path.exists(), "{} survived", path.display());
        }
        for path in sidecar_read
            .iter()
            .chain(&form_read)
            .chain([&recent_orphan])
        {
            assert!(path.exists(), "{} was removed", path.display());
        }
    }

    #[test]
    fn prune_removes_temp_files_left_by_a_crashed_write() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let now = SystemTime::now();
        let entry = |name: &str, age: Duration| {
            let path = dir.join(name);
            std::fs::write(&path, name).unwrap();
            set_times(&path, now - age, now - age);
            path
        };

        let crashed = entry(
            ".a.cwasm.1.0192d3f4a5b67c8d9e0f1a2b3c4d5e6f.tmp",
            Duration::from_secs(2 * 60 * 60),
        );
        let in_flight = entry(
            ".b.cwasm.2.0192d3f4a5b67c8d9e0f1a2b3c4d5e70.tmp",
            Duration::from_secs(60),
        );
        let form = entry("c.cwasm", Duration::from_secs(2 * 60 * 60));

        prune_stale(dir, now);

        assert!(!crashed.exists());
        assert!(in_flight.exists());
        assert!(form.exists());
    }

    #[test]
    fn two_engine_keys_used_alternately_keep_and_load_their_own_forms() {
        let home = tempfile::tempdir().unwrap();
        crate::murmur_home::run_with_home(
            "compiled_forms::tests::inner_two_engine_keys_used_alternately_keep_and_load_their_own_forms",
            home.path(),
        );
    }

    #[test]
    #[ignore = "run by two_engine_keys_used_alternately_keep_and_load_their_own_forms"]
    fn inner_two_engine_keys_used_alternately_keep_and_load_their_own_forms() {
        if !crate::murmur_home::in_scratch_home() {
            return;
        }
        // Y and Z are the empty component plus a custom section named `y` or `z`, so each of the
        // three has its own sha256.
        const X: &[u8] = EMPTY_COMPONENT;
        const Y: &[u8] = b"\0asm\x0d\x00\x01\x00\x00\x02\x01y";
        const Z: &[u8] = b"\0asm\x0d\x00\x01\x00\x00\x02\x01z";

        let a_engine = engine();
        let b_engine = {
            let mut config = wasmtime::Config::new();
            config.wasm_component_model(true);
            config.cranelift_opt_level(wasmtime::OptLevel::None);
            Engine::new(&config).unwrap()
        };
        let workdir = tempfile::tempdir().unwrap();
        let a = (CompiledForms::new(&a_engine, workdir.path()), &a_engine);
        let b = (CompiledForms::new(&b_engine, workdir.path()), &b_engine);
        let compile = |(forms, engine): &(CompiledForms, &Engine), wasm: &[u8]| {
            forms
                .compile(engine, &murmur_artifact::sha256_hex(wasm), wasm)
                .unwrap();
        };
        let dir = PathBuf::from(std::env::var_os("HOME").unwrap())
            .join(".murmur")
            .join(COMPILED_DIR);
        let set = |(forms, _): &(CompiledForms, &Engine), wasm: &[u8]| {
            let form = dir.join(format!(
                "{}-{:016x}.cwasm",
                murmur_artifact::sha256_hex(wasm),
                forms.engine_key
            ));
            [sidecar(&form), form]
        };
        let inodes = |sets: &[[PathBuf; 2]]| -> Vec<u64> {
            sets.iter().flatten().map(|path| inode(path)).collect()
        };
        assert_ne!(set(&a, X), set(&b, X));

        compile(&a, X);
        compile(&b, X);
        for path in set(&a, X).iter().chain(&set(&b, X)) {
            assert!(path.is_file(), "{} missing", path.display());
        }
        let x_inodes = inodes(&[set(&a, X), set(&b, X)]);

        for forms in [&a, &b, &a, &b] {
            compile(forms, X);
        }
        assert_eq!(inodes(&[set(&a, X), set(&b, X)]), x_inodes);

        compile(&a, Y);
        assert_eq!(inodes(&[set(&b, X)]), x_inodes[2..]);
        let a_xy_inodes = inodes(&[set(&a, X), set(&a, Y)]);

        let stale = SystemTime::now() - Duration::from_secs(31 * 24 * 60 * 60);
        for path in set(&b, X) {
            set_times(&path, stale, stale);
        }
        compile(&a, Z);
        for path in set(&b, X) {
            assert!(!path.exists(), "{} survived", path.display());
        }
        for path in set(&a, Z) {
            assert!(path.is_file(), "{} missing", path.display());
        }
        assert_eq!(inodes(&[set(&a, X), set(&a, Y)]), a_xy_inodes);

        compile(&b, X);
        for path in set(&b, X) {
            assert!(path.is_file(), "{} missing", path.display());
        }
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
