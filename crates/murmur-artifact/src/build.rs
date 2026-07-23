use std::{
    collections::HashSet,
    fs::{self, File},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

use thiserror::Error;
use zip::{
    write::{FileOptions, SimpleFileOptions},
    CompressionMethod, ZipWriter,
};

use crate::manifest::{load_manifest, ManifestError};
use crate::manifest_path::{resolve_manifest_path, MANIFEST_FILENAME};

/// The name the project manifest is stored under *inside* a `.mur.zip`.
///
/// Addressing a zip entry and resolving a project-root path are separate concerns,
/// so they keep separate names. They now resolve to the same string: an artifact's
/// manifest is spelled `murmur.yaml` whether you are looking inside the archive or
/// at the project directory it was built from. Defining this as an alias of
/// [`MANIFEST_FILENAME`] rather than a second literal keeps that guarantee
/// mechanical — the two cannot drift apart without an explicit edit here.
///
/// `capsule-runtime`'s artifact unpacking and `murmur-artifact::artifact`'s readers
/// both address the packed entry through this constant.
pub const PACKED_MANIFEST_ENTRY: &str = MANIFEST_FILENAME;

#[derive(Debug, Error)]
pub enum BuildError {
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    #[error("output path must end with .mur.zip: {0}")]
    InvalidOutputExtension(String),
    #[error("missing required file '{file}' in {path}")]
    MissingRequiredFile { file: String, path: String },
    #[error("failed to create output file at {path}: {source}")]
    CreateOutput {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to read source at {path}: {source}")]
    ReadSource {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to determine working directory: {0}")]
    WorkingDir(std::io::Error),
    #[error("failed to package file {path}: {source}")]
    PackageFile {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("zip error while writing {path}: {source}")]
    Zip {
        path: String,
        #[source]
        source: zip::result::ZipError,
    },
}

#[must_use = "artifact path is needed for caller output and follow-up actions"]
pub fn build_artifact(source_dir: &Path, output_path: &Path) -> Result<PathBuf, BuildError> {
    let manifest_path = resolve_manifest_path(source_dir);
    let manifest = load_manifest(&manifest_path)?;

    // Which companion files an artifact needs is the artifact's own declaration
    // (`requires_files:`), not something this function infers from the role. `runtime: skill`
    // still requires `skill.md` because that is what the field defaults to when absent.
    for required in &manifest.requires_files {
        if !source_dir.join(required).exists() {
            return Err(BuildError::MissingRequiredFile {
                file: required.clone(),
                path: source_dir.display().to_string(),
            });
        }
    }

    if !output_path.to_string_lossy().ends_with(".mur.zip") {
        return Err(BuildError::InvalidOutputExtension(
            output_path.display().to_string(),
        ));
    }

    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent).map_err(|source| BuildError::CreateOutput {
            path: output_path.display().to_string(),
            source,
        })?;
    }

    let output_abs = absolute_normalized(output_path)?;
    let source_abs = source_dir
        .canonicalize()
        .map_err(|source| BuildError::ReadSource {
            path: source_dir.display().to_string(),
            source,
        })?;

    // An artifact ships what it declares, not what happens to sit next to it. The packed set is
    // the manifest plus the `requires_files:` entries validated above — so `src/`, `Cargo.toml`,
    // `README.md`, editor droppings and stray build output stay in the source tree rather than
    // riding along in every published `.mur.zip`. Order is manifest-first, then declaration
    // order: deterministic, and the same shape hand-rolled release packaging already produces.
    let declared = std::iter::once(MANIFEST_FILENAME)
        .chain(manifest.requires_files.iter().map(String::as_str));
    let mut files: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut packed: HashSet<PathBuf> = HashSet::new();
    for rel in declared {
        // Dedup on the resolved path, so a manifest that redundantly lists `murmur.yaml` (or
        // spells a file two ways) still yields exactly one zip entry per file on disk.
        let abs = absolute_normalized(&source_abs.join(rel))?;
        if packed.insert(abs.clone()) {
            files.push((abs, PathBuf::from(rel)));
        }
    }
    files.retain(|(abs, _)| *abs != output_abs);

    let output_file = File::create(output_path).map_err(|source| BuildError::CreateOutput {
        path: output_path.display().to_string(),
        source,
    })?;
    let mut zip = ZipWriter::new(output_file);
    let options: SimpleFileOptions =
        FileOptions::default().compression_method(CompressionMethod::Deflated);

    for (abs_path, rel_path) in files {
        // Exactly one candidate can have a relative path of MANIFEST_FILENAME — it is the
        // entry seeded above, and the dedup drops any `requires_files:` restatement of it —
        // so the PACKED_MANIFEST_ENTRY slot is claimed once, by the manifest
        // `resolve_manifest_path` already loaded. A same-named file in a subdirectory packs
        // under its own relative path and never displaces it.
        let rel_str = if rel_path == Path::new(MANIFEST_FILENAME) {
            PACKED_MANIFEST_ENTRY.to_string()
        } else {
            rel_path.to_string_lossy().replace('\\', "/")
        };
        zip.start_file(rel_str, options)
            .map_err(|source| BuildError::Zip {
                path: abs_path.display().to_string(),
                source,
            })?;

        let mut content = Vec::new();
        File::open(&abs_path)
            .and_then(|mut f| f.read_to_end(&mut content))
            .map_err(|source| BuildError::PackageFile {
                path: abs_path.display().to_string(),
                source,
            })?;

        zip.write_all(&content)
            .map_err(|source| BuildError::PackageFile {
                path: abs_path.display().to_string(),
                source,
            })?;
    }

    zip.finish().map_err(|source| BuildError::Zip {
        path: output_path.display().to_string(),
        source,
    })?;

    Ok(output_path.to_path_buf())
}

fn absolute_normalized(path: &Path) -> Result<PathBuf, BuildError> {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(BuildError::WorkingDir)?
            .join(path)
    };

    Ok(normalize_path(&abs))
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use zip::ZipArchive;

    #[test]
    fn skill_artifact_build_succeeds_with_skill_md() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join(MANIFEST_FILENAME),
            "name: test-skill\nversion: 0.1.0\nruntime: skill\n",
        )
        .unwrap();
        fs::write(dir.path().join("skill.md"), "# Skill\nGuidance.\n").unwrap();

        let out = dir.path().join("artifact.mur.zip");
        build_artifact(dir.path(), &out).unwrap();

        let file = std::fs::File::open(&out).unwrap();
        let mut archive = ZipArchive::new(file).unwrap();
        let names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect();

        assert!(names.contains(&"murmur.yaml".to_string()));
        assert!(names.contains(&"skill.md".to_string()));
    }

    #[test]
    fn skill_artifact_build_fails_without_skill_md() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join(MANIFEST_FILENAME),
            "name: test-skill\nversion: 0.1.0\nruntime: skill\n",
        )
        .unwrap();

        let out = dir.path().join("artifact.mur.zip");
        let err = build_artifact(dir.path(), &out).unwrap_err();

        let msg = err.to_string();
        assert!(
            msg.contains("skill.md"),
            "error should name the missing file; got: {msg}"
        );
        assert!(!out.exists(), "no zip should be created on failure");
    }

    #[test]
    fn requires_files_is_enforced_for_non_skill_role() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join(MANIFEST_FILENAME),
            "name: test-tool\nversion: 0.1.0\nruntime: tool\nrequires_files:\n  - config.json\n",
        )
        .unwrap();

        let out = dir.path().join("artifact.mur.zip");
        let err = build_artifact(dir.path(), &out).unwrap_err();
        assert!(
            matches!(&err, BuildError::MissingRequiredFile { file, .. } if file == "config.json"),
            "expected MissingRequiredFile for config.json; got: {err}"
        );
        assert!(!out.exists(), "no zip should be created on failure");

        fs::write(dir.path().join("config.json"), "{}\n").unwrap();
        build_artifact(dir.path(), &out).expect("build succeeds once the declared file exists");
    }

    #[test]
    fn empty_requires_files_lets_a_skill_skip_the_skill_md_check() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join(MANIFEST_FILENAME),
            "name: test-skill\nversion: 0.1.0\nruntime: skill\nrequires_files: []\n",
        )
        .unwrap();

        let out = dir.path().join("artifact.mur.zip");
        build_artifact(dir.path(), &out).expect("explicit empty requires_files overrides default");
    }

    /// Names every entry in a built archive, in the order the packer wrote them.
    fn entry_names(archive_path: &Path) -> Vec<String> {
        let file = File::open(archive_path).unwrap();
        let mut archive = ZipArchive::new(file).unwrap();
        (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect()
    }

    /// The shape the project's own release packaging builds by hand (`zip -j out.mur.zip
    /// murmur.yaml <name>.wasm`): a declared payload ships, and the source tree it was compiled
    /// from does not.
    #[test]
    fn packs_only_the_manifest_and_declared_files() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join(MANIFEST_FILENAME),
            "name: murmur-hook-compact\nversion: 0.3.0\nruntime: hook\nexecution: wasm\nrequires_files:\n  - murmur_hook_compact.wasm\n",
        )
        .unwrap();
        fs::write(dir.path().join("murmur_hook_compact.wasm"), b"\0asm").unwrap();

        // Everything below is undeclared, so none of it may ship.
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/lib.rs"), "fn main() {}").unwrap();
        fs::write(dir.path().join("Cargo.toml"), "[package]\n").unwrap();
        fs::write(dir.path().join("README.md"), "# hook\n").unwrap();
        fs::write(dir.path().join(".DS_Store"), "junk").unwrap();

        let out = dir.path().join("artifact.mur.zip");
        build_artifact(dir.path(), &out).unwrap();

        assert_eq!(
            entry_names(&out),
            vec![
                PACKED_MANIFEST_ENTRY.to_string(),
                "murmur_hook_compact.wasm".to_string()
            ],
            "a curated artifact carries its manifest and its declared payload, nothing else"
        );
    }

    /// Curation is per declared entry, not per directory: declaring one file in `assets/` does
    /// not sweep in its neighbours.
    #[test]
    fn declaring_a_nested_asset_excludes_its_undeclared_siblings() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join(MANIFEST_FILENAME),
            "name: nested\nversion: 0.1.0\nruntime: tool\nrequires_files:\n  - assets/logo.png\n",
        )
        .unwrap();
        fs::create_dir_all(dir.path().join("assets")).unwrap();
        fs::write(dir.path().join("assets/logo.png"), "png").unwrap();
        fs::write(dir.path().join("assets/other.txt"), "not declared").unwrap();

        let out = dir.path().join("artifact.mur.zip");
        build_artifact(dir.path(), &out).unwrap();

        assert_eq!(
            entry_names(&out),
            vec![
                PACKED_MANIFEST_ENTRY.to_string(),
                "assets/logo.png".to_string()
            ],
            "only the declared member of assets/ ships"
        );
    }

    /// The accepted consequence of allowlist packing: an artifact that declares no companion
    /// files gets a manifest-only zip. Nothing infers a payload from the role or the name.
    #[test]
    fn empty_requires_files_packs_a_manifest_only_zip() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join(MANIFEST_FILENAME),
            "name: undeclared\nversion: 0.1.0\nruntime: tool\n",
        )
        .unwrap();
        fs::write(dir.path().join("undeclared.wasm"), b"\0asm").unwrap();
        fs::write(dir.path().join("notes.txt"), "stray").unwrap();

        let out = dir.path().join("artifact.mur.zip");
        build_artifact(dir.path(), &out).expect("nothing declared is not an error");

        assert_eq!(
            entry_names(&out),
            vec![PACKED_MANIFEST_ENTRY.to_string()],
            "nothing declared means nothing but the manifest is packed"
        );
    }

    /// A manifest that redundantly lists itself is a degenerate declaration, not a request for
    /// a duplicate entry.
    #[test]
    fn requires_files_naming_the_manifest_does_not_double_pack_it() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join(MANIFEST_FILENAME),
            "name: redundant\nversion: 0.1.0\nruntime: tool\nrequires_files:\n  - murmur.yaml\n  - payload.bin\n",
        )
        .unwrap();
        fs::write(dir.path().join("payload.bin"), "bytes").unwrap();

        let out = dir.path().join("artifact.mur.zip");
        build_artifact(dir.path(), &out).unwrap();

        assert_eq!(
            entry_names(&out),
            vec![PACKED_MANIFEST_ENTRY.to_string(), "payload.bin".to_string()],
            "the manifest is packed once regardless of how often it is declared"
        );
    }

    /// The declared-file check runs before anything is written, so a build that cannot ship its
    /// payload leaves no half-formed artifact behind.
    #[test]
    fn missing_declared_payload_fails_before_any_zip_is_written() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join(MANIFEST_FILENAME),
            "name: murmur-hook-compact\nversion: 0.3.0\nruntime: hook\nexecution: wasm\nrequires_files:\n  - murmur_hook_compact.wasm\n",
        )
        .unwrap();

        let out = dir.path().join("artifact.mur.zip");
        let err = build_artifact(dir.path(), &out).unwrap_err();

        assert!(
            matches!(&err, BuildError::MissingRequiredFile { file, .. } if file == "murmur_hook_compact.wasm"),
            "expected MissingRequiredFile for the declared wasm; got: {err}"
        );
        assert!(!out.exists(), "no zip should be created on failure");
    }

    #[test]
    fn builds_zip_with_manifest_at_root() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join(MANIFEST_FILENAME),
            "name: hello\nversion: 0.0.1\nruntime: wasm\nrequires_files:\n  - assets/file.txt\n",
        )
        .unwrap();
        fs::create_dir_all(dir.path().join("assets")).unwrap();
        fs::write(dir.path().join("assets/file.txt"), "ok").unwrap();

        let out = dir.path().join("artifact.mur.zip");
        build_artifact(dir.path(), &out).unwrap();

        let file = File::open(&out).unwrap();
        let mut archive = ZipArchive::new(file).unwrap();

        let mut names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect();
        names.sort();

        assert!(
            names.contains(&"murmur.yaml".to_string()),
            "murmur.yaml must be at archive root, got: {:?}",
            names
        );
        assert!(
            names.contains(&"assets/file.txt".to_string()),
            "nested file must be included, got: {:?}",
            names
        );

        let mut content = String::new();
        archive
            .by_name("murmur.yaml")
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();
        assert!(content.contains("name: hello"));

        assert!(
            !names.iter().any(|n| n.ends_with(".mur.zip")),
            "output artifact must not appear inside zip"
        );
    }

    /// The packed-entry slot can only ever be claimed by the source manifest: a
    /// directory cannot hold two files named `MANIFEST_FILENAME`, and a same-named
    /// file in a subdirectory packs under its own relative path, not the root slot.
    #[test]
    fn source_manifest_is_the_only_root_manifest_entry() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join(MANIFEST_FILENAME),
            "name: root\nversion: 0.0.2\nruntime: wasm\nrequires_files:\n  - tools/murmur.yaml\n",
        )
        .unwrap();
        fs::create_dir_all(dir.path().join("tools")).unwrap();
        fs::write(
            dir.path().join("tools").join(MANIFEST_FILENAME),
            "name: nested\nversion: 0.0.1\nruntime: wasm\n",
        )
        .unwrap();

        let out = dir.path().join("artifact.mur.zip");
        build_artifact(dir.path(), &out).unwrap();

        let file = File::open(&out).unwrap();
        let mut archive = ZipArchive::new(file).unwrap();
        let names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect();

        assert_eq!(
            names.iter().filter(|n| *n == PACKED_MANIFEST_ENTRY).count(),
            1,
            "exactly one root manifest entry expected, got: {names:?}"
        );
        assert!(
            names.contains(&"tools/murmur.yaml".to_string()),
            "a same-named nested file must keep its own path, got: {names:?}"
        );

        let mut content = String::new();
        archive
            .by_name(PACKED_MANIFEST_ENTRY)
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();
        assert!(
            content.contains("name: root"),
            "packed entry must come from the root {MANIFEST_FILENAME}, got: {content}"
        );
    }

    #[test]
    fn built_artifact_round_trips_through_manifest_readers() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join(MANIFEST_FILENAME),
            "name: roundtrip\nversion: 0.4.2\nruntime: wasm\n",
        )
        .unwrap();

        let out = dir.path().join("artifact.mur.zip");
        build_artifact(dir.path(), &out).unwrap();
        let bytes = fs::read(&out).unwrap();

        let yaml = crate::artifact::load_manifest_yaml_from_artifact_bytes(&bytes).unwrap();
        assert!(yaml.contains("name: roundtrip"));

        let manifest = crate::artifact::load_manifest_from_artifact_bytes(&bytes).unwrap();
        assert_eq!(manifest.name, "roundtrip");
        assert_eq!(manifest.version, "0.4.2");
    }
}
