use std::{
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

    let mut files = collect_files(&source_abs)?;
    files.sort_by(|a, b| a.1.cmp(&b.1));
    files.retain(|(abs, _)| *abs != output_abs);

    let output_file = File::create(output_path).map_err(|source| BuildError::CreateOutput {
        path: output_path.display().to_string(),
        source,
    })?;
    let mut zip = ZipWriter::new(output_file);
    let options: SimpleFileOptions =
        FileOptions::default().compression_method(CompressionMethod::Deflated);

    for (abs_path, rel_path) in files {
        // At most one collected file can have a relative path of MANIFEST_FILENAME —
        // a directory cannot hold two entries under one name — and it is definitionally
        // the file `resolve_manifest_path` already loaded above. So exactly one file
        // claims the PACKED_MANIFEST_ENTRY slot, and it is the real manifest.
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

fn collect_files(source_abs: &Path) -> Result<Vec<(PathBuf, PathBuf)>, BuildError> {
    let mut out = Vec::new();
    collect_files_from(source_abs, source_abs, &mut out)?;
    Ok(out)
}

fn collect_files_from(
    base: &Path,
    current: &Path,
    out: &mut Vec<(PathBuf, PathBuf)>,
) -> Result<(), BuildError> {
    for entry in fs::read_dir(current).map_err(|source| BuildError::ReadSource {
        path: current.display().to_string(),
        source,
    })? {
        let entry = entry.map_err(|source| BuildError::ReadSource {
            path: current.display().to_string(),
            source,
        })?;
        let path = entry.path();

        if path.is_dir() {
            collect_files_from(base, &path, out)?;
        } else if path.is_file() {
            let rel = path.strip_prefix(base).unwrap_or(&path).to_path_buf();
            out.push((absolute_normalized(&path)?, rel));
        }
    }

    Ok(())
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

    #[test]
    fn builds_zip_with_manifest_at_root() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join(MANIFEST_FILENAME),
            "name: hello\nversion: 0.0.1\nruntime: wasm\n",
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
            "name: root\nversion: 0.0.2\nruntime: wasm\n",
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
