//! The content-addressed bodies behind a trace's hashes.
//!
//! Under `trace.capture: content` every hash an `inference` event names has a file beside the
//! trace holding the exact bytes that hash covers, so `cat <session>/blobs/<sha>` prints the
//! literal thing the driver received. Every other capture mode writes nothing here and the
//! directory never appears.

use crate::workdir_fs::{EntryKind, WorkdirRoot, DEFAULT_FILE_MODE};

/// Directory holding one session's blobs, beside its `trace.jsonl`.
pub(crate) const BLOB_DIR_NAME: &str = "blobs";

/// Mode applied to the blob directory: owner-only, because a blob is the wire payload verbatim —
/// system prompt, tool inventory and every message, unredacted.
const BLOB_DIR_MODE: u32 = 0o700;

/// Write-once, content-addressed store at `<records dir>/blobs/<sha256>`.
///
/// One file per distinct body, named by its own lowercase-hex SHA-256 with no prefix and no
/// extension — the same bare-sha convention the rest of the trace uses for content, as against
/// the `evt_`/`msg_`/`ses_` prefixes that name entities. The directory is created on the first
/// write and not before, so a session that stores no bodies leaves no empty directory behind.
///
/// Session-scoped and never pruned: a blob is readable exactly as long as its session directory
/// is.
pub(crate) struct BlobStore {
    /// The session's records directory, `blobs/` is created beneath.
    records_root: WorkdirRoot,
}

impl BlobStore {
    /// A store rooted at `<records dir>/blobs`. Creates nothing; see [`Self::put`].
    pub(crate) fn new(records_root: &WorkdirRoot) -> Self {
        Self {
            records_root: records_root.clone(),
        }
    }

    /// The directory this store writes into, whether or not it exists yet.
    #[cfg(test)]
    pub(crate) fn dir(&self) -> std::path::PathBuf {
        self.records_root.host_path(BLOB_DIR_NAME)
    }

    /// Store `bytes` as `<blobs>/<sha256>`, where `sha256` is the caller's already-computed
    /// lowercase-hex digest of exactly those bytes.
    ///
    /// A blob already stored is left alone rather than rewritten: its name is its content, so a
    /// body repeated on every turn is written once.
    ///
    /// A link at `blobs/` or at the blob's name is refused, never taken as "already stored".
    pub(crate) async fn put(&self, sha256: &str, bytes: &[u8]) -> std::io::Result<()> {
        if self
            .records_root
            .entry_kind(format!("{BLOB_DIR_NAME}/{sha256}"))?
            == Some(EntryKind::File)
        {
            return Ok(());
        }
        self.records_root
            .create_dir_all(BLOB_DIR_NAME, BLOB_DIR_MODE)?;
        self.records_root.write_file(
            format!("{BLOB_DIR_NAME}/{sha256}"),
            bytes,
            DEFAULT_FILE_MODE,
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn put_names_the_file_by_its_digest_and_creates_the_dir_private() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(&WorkdirRoot::open(dir.path()).unwrap());
        assert!(!store.dir().exists(), "nothing is created before a write");

        let body = b"the literal bytes";
        let sha = murmur_artifact::sha256_hex(body);
        store.put(&sha, body).await.unwrap();

        let path = store.dir().join(&sha);
        assert_eq!(std::fs::read(&path).unwrap(), body);
        assert_eq!(
            murmur_artifact::sha256_hex(&std::fs::read(&path).unwrap()),
            sha
        );
        let mode = std::fs::metadata(store.dir()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[tokio::test]
    async fn put_never_rewrites_an_existing_blob() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(&WorkdirRoot::open(dir.path()).unwrap());
        let body = b"once";
        let sha = murmur_artifact::sha256_hex(body);

        store.put(&sha, body).await.unwrap();
        let first = std::fs::metadata(store.dir().join(&sha)).unwrap();

        // A second put under the same name must not touch the file, even when handed different
        // bytes — the name is the content, so the first writer's bytes are the true ones.
        store.put(&sha, b"twice, differently").await.unwrap();
        assert_eq!(std::fs::read(store.dir().join(&sha)).unwrap(), body);
        assert_eq!(
            std::fs::metadata(store.dir().join(&sha)).unwrap().len(),
            first.len()
        );
    }
}
