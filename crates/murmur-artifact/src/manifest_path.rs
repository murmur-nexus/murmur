//! Location of a project's top-level identity manifest.
//!
//! This is the file a developer hand-writes at their project root to declare the
//! project's `name`, `version`, `artifacts`, `capabilities`, and `inference`.
//!
//! It remains a distinct concept from the `murmur.yaml` entry packed inside every
//! `.mur.zip` archive — that zip-entry name is part of the artifact layout, shared
//! by the project's own packaged output and by every dependency artifact it
//! declares. The two now spell the same name; [`crate::PACKED_MANIFEST_ENTRY`] is
//! defined as an alias of the constant below so they cannot drift apart silently.

use std::path::{Path, PathBuf};

/// Filename of a project's top-level identity manifest.
pub const MANIFEST_FILENAME: &str = "murmur.yaml";

/// Resolve the identity-manifest path for a project directory.
#[must_use]
pub fn resolve_manifest_path(project_dir: &Path) -> PathBuf {
    project_dir.join(MANIFEST_FILENAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_manifest_path_joins_manifest_filename() {
        assert_eq!(
            resolve_manifest_path(Path::new("/tmp/project")),
            PathBuf::from("/tmp/project/murmur.yaml")
        );
    }

    #[test]
    fn resolve_manifest_path_preserves_relative_dirs() {
        assert_eq!(
            resolve_manifest_path(Path::new("nested/project")),
            PathBuf::from("nested/project/murmur.yaml")
        );
    }
}
