//! The content-addressed bodies behind a trace's hashes.
//!
//! Under `trace.capture: content` every hash an `inference` event names has a file beside the
//! trace holding the exact bytes that hash covers, so `cat <session>/blobs/<sha>` prints the
//! literal thing the driver received. Every other capture mode writes nothing here and the
//! directory never appears.

use std::path::{Path, PathBuf};

/// Directory holding one session's blobs, beside its `trace.jsonl`.
pub(crate) const BLOB_DIR_NAME: &str = "blobs";

/// Mode applied to the blob directory: owner-only, because a blob is the wire payload verbatim —
/// system prompt, tool inventory and every message, unredacted.
const BLOB_DIR_MODE: u32 = 0o700;

/// Write-once, content-addressed store at `<session workdir>/blobs/<sha256>`.
///
/// One file per distinct body, named by its own lowercase-hex SHA-256 with no prefix and no
/// extension — the same bare-sha convention the rest of the trace uses for content, as against
/// the `evt_`/`msg_`/`ses_` prefixes that name entities. The directory is created on the first
/// write and not before, so a session that stores no bodies leaves no empty directory behind.
///
/// Session-scoped and never pruned: a blob is readable exactly as long as its session directory
/// is.
pub(crate) struct BlobStore {
    dir: PathBuf,
}

impl BlobStore {
    /// A store rooted at `<session_workdir>/blobs`. Creates nothing; see [`Self::put`].
    pub(crate) fn new(session_workdir: &Path) -> Self {
        Self {
            dir: session_workdir.join(BLOB_DIR_NAME),
        }
    }

    /// The directory this store writes into, whether or not it exists yet.
    #[cfg(test)]
    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    /// Store `bytes` as `<blobs>/<sha256>`, where `sha256` is the caller's already-computed
    /// lowercase-hex digest of exactly those bytes.
    ///
    /// A path that already exists is left alone rather than rewritten: the name *is* the content,
    /// so a second write of the same body has nothing new to say, and the ten-turn case where one
    /// unchanged system prompt is hashed on every turn costs one file and one `exists` check per
    /// turn thereafter.
    pub(crate) async fn put(&self, sha256: &str, bytes: &[u8]) -> std::io::Result<()> {
        let path = self.dir.join(sha256);
        if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Ok(());
        }
        crate::state_store::ensure_private_dir(&self.dir, BLOB_DIR_MODE)
            .map_err(std::io::Error::other)?;
        tokio::fs::write(&path, bytes).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn put_names_the_file_by_its_digest_and_creates_the_dir_private() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path());
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
        let store = BlobStore::new(dir.path());
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
