use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

use thiserror::Error;
use zip::{
    write::{FileOptions, SimpleFileOptions},
    CompressionMethod, ZipWriter,
};

use crate::manifest::{load_manifest, Manifest, ManifestError};
use crate::manifest_path::{resolve_manifest_path, MANIFEST_FILENAME};
use crate::payload_shape::{select_root_wasm_from_entries, PayloadShapeError};
use crate::registry::RuntimeType;
use crate::zip_guard::{sanitize_entry_path, ZipGuardError};

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

/// Longest `name:` an artifact manifest may declare.
///
/// A name is an identifier that ends up in a filename (`<name>-<version>.mur.zip`), a registry
/// key and a store directory, so it gets a bound well below any filesystem limit rather than
/// whatever the YAML happened to contain.
pub const MAX_ARTIFACT_NAME_LEN: usize = 100;

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
    /// `name:` is not a usable artifact identifier. Format only — nothing here reserves a
    /// prefix or a namespace, which is a registry concern rather than a packaging one.
    #[error("invalid artifact name '{name}': {reason}")]
    InvalidArtifactName { name: String, reason: String },
    /// A `requires_files:` entry would not survive the runtime's own entry-name rule
    /// ([`sanitize_entry_path`]): it is absolute, or it climbs out of the source tree.
    #[error("requires_files entry '{entry}' has an unsafe path: {reason}")]
    UnsafeRequiredPath { entry: String, reason: String },
    /// A `requires_files:` entry is a symlink. Packing it would silently ship whatever the
    /// link resolves to — possibly from outside the source tree — under the declared name.
    #[error("requires_files entry '{entry}' is a symlink ({path}); declare the file it points to instead")]
    SymlinkedRequiredFile { entry: String, path: String },
    /// Two distinct source files claim the same entry name inside the archive, so one would
    /// silently overwrite the other on unpack.
    #[error("requires_files entries '{first}' and '{second}' both pack as the archive entry '{entry}'")]
    DuplicateArchiveEntry {
        first: String,
        second: String,
        entry: String,
    },
    /// The curated entry set is not a launchable wasm payload. Reported verbatim: the text an
    /// author sees at build time is the text the runtime would have printed at launch.
    #[error(transparent)]
    PayloadShape(#[from] PayloadShapeError),
}

/// One file the packer will write into the archive.
pub(crate) struct PackedEntry {
    /// The `requires_files:` entry (or [`MANIFEST_FILENAME`]) this came from, as declared.
    pub(crate) declared: String,
    /// Absolute, normalized path of the file to read.
    pub(crate) source_path: PathBuf,
    /// The name this file is written under inside the `.mur.zip`.
    pub(crate) archive_name: String,
}

/// A `requires_files:` declaration the curation dropped because the slot it claims is already
/// filled — today only ever the root manifest the packer seeds for itself.
pub(crate) struct ShadowedDeclaration {
    pub(crate) declared: String,
    pub(crate) archive_name: String,
}

/// Everything `mur build` decides about *what* it will pack, before it writes a byte.
pub(crate) struct PackedPlan {
    pub(crate) entries: Vec<PackedEntry>,
    pub(crate) shadowed: Vec<ShadowedDeclaration>,
}

#[must_use = "artifact path is needed for caller output and follow-up actions"]
pub fn build_artifact(source_dir: &Path, output_path: &Path) -> Result<PathBuf, BuildError> {
    let manifest_path = resolve_manifest_path(source_dir);
    let manifest = load_manifest(&manifest_path)?;

    // Everything that decides *what* ships — name validation, per-entry path safety, curation
    // and archive-name collisions — happens here, and is the same computation the build lints
    // read. Nothing below re-derives the entry list.
    let plan = plan_packed_entries(source_dir, &manifest, output_path)?;

    if !output_path.to_string_lossy().ends_with(".mur.zip") {
        return Err(BuildError::InvalidOutputExtension(
            output_path.display().to_string(),
        ));
    }

    // The shape check the runtime applies at launch, applied to the entry set about to be
    // written. An artifact whose payload cannot be selected is a build failure now rather than
    // a `mur run` failure later; the message is the runtime's, verbatim.
    if manifest.registry_runtime() == RuntimeType::Wasm {
        select_root_wasm_from_entries(plan.entries.iter().map(|entry| entry.archive_name.as_str()))?;
    }

    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent).map_err(|source| BuildError::CreateOutput {
            path: output_path.display().to_string(),
            source,
        })?;
    }

    let output_file = File::create(output_path).map_err(|source| BuildError::CreateOutput {
        path: output_path.display().to_string(),
        source,
    })?;
    let mut zip = ZipWriter::new(output_file);
    let options: SimpleFileOptions =
        FileOptions::default().compression_method(CompressionMethod::Deflated);

    for entry in plan.entries {
        zip.start_file(&entry.archive_name, options)
            .map_err(|source| BuildError::Zip {
                path: entry.source_path.display().to_string(),
                source,
            })?;

        let mut content = Vec::new();
        File::open(&entry.source_path)
            .and_then(|mut f| f.read_to_end(&mut content))
            .map_err(|source| BuildError::PackageFile {
                path: entry.source_path.display().to_string(),
                source,
            })?;

        zip.write_all(&content)
            .map_err(|source| BuildError::PackageFile {
                path: entry.source_path.display().to_string(),
                source,
            })?;
    }

    zip.finish().map_err(|source| BuildError::Zip {
        path: output_path.display().to_string(),
        source,
    })?;

    Ok(output_path.to_path_buf())
}

/// Decide the exact entry set `build_artifact` will write, rejecting anything unsafe.
///
/// The single source of truth for "what's packed": [`build_artifact`] writes this list and
/// [`crate::build_lints::lint_build_warnings`] lints it, so the two cannot drift. It touches
/// the filesystem only to check that declared files exist and are not symlinks — no bytes are
/// read and no output is created, so every rejection here leaves the disk untouched.
pub(crate) fn plan_packed_entries(
    source_dir: &Path,
    manifest: &Manifest,
    output_path: &Path,
) -> Result<PackedPlan, BuildError> {
    validate_artifact_name(&manifest.name)?;

    // Which companion files an artifact needs is the artifact's own declaration
    // (`requires_files:`), not something this function infers from the role. `runtime: skill`
    // still requires `skill.md` because that is what the field defaults to when absent.
    for required in &manifest.requires_files {
        check_declared_entry(source_dir, required)?;
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
    let mut entries: Vec<PackedEntry> = Vec::new();
    let mut shadowed: Vec<ShadowedDeclaration> = Vec::new();
    let mut packed: HashSet<PathBuf> = HashSet::new();
    for rel in declared {
        // Dedup on the resolved path, so a manifest that redundantly lists `murmur.yaml` (or
        // spells a file two ways) still yields exactly one zip entry per file on disk. The
        // dropped declarations are kept so a lint can point at the redundant line.
        let abs = absolute_normalized(&source_abs.join(rel))?;
        let archive_name = archive_entry_name(rel);
        if packed.insert(abs.clone()) {
            entries.push(PackedEntry {
                declared: rel.to_string(),
                source_path: abs,
                archive_name,
            });
        } else {
            shadowed.push(ShadowedDeclaration {
                declared: rel.to_string(),
                archive_name,
            });
        }
    }
    entries.retain(|entry| entry.source_path != output_abs);

    // Deduping on the source path is not enough: the archive name is a rewrite of the declared
    // path (`\` → `/`), so two distinct files can still land in the same slot, where one would
    // silently overwrite the other on unpack.
    let mut claimed: HashMap<&str, &str> = HashMap::new();
    for entry in &entries {
        if let Some(first) = claimed.insert(&entry.archive_name, &entry.declared) {
            return Err(BuildError::DuplicateArchiveEntry {
                first: first.to_string(),
                second: entry.declared.clone(),
                entry: entry.archive_name.clone(),
            });
        }
    }

    Ok(PackedPlan { entries, shadowed })
}

/// The name a declared file is written under inside the archive.
///
/// Exactly one candidate can be spelled [`MANIFEST_FILENAME`] — it is the entry the packer
/// seeds, and the dedup drops any `requires_files:` restatement of it — so the
/// [`PACKED_MANIFEST_ENTRY`] slot is claimed once, by the manifest `resolve_manifest_path`
/// loaded. A same-named file in a subdirectory packs under its own relative path and never
/// displaces it.
fn archive_entry_name(rel: &str) -> String {
    if Path::new(rel) == Path::new(MANIFEST_FILENAME) {
        PACKED_MANIFEST_ENTRY.to_string()
    } else {
        rel.replace('\\', "/")
    }
}

/// Validate one `requires_files:` entry before anything is resolved or opened on its behalf.
fn check_declared_entry(source_dir: &Path, declared: &str) -> Result<(), BuildError> {
    // `sanitize_entry_path` *strips* a leading `/` — the right call when hardening an entry
    // read out of someone else's archive, the wrong one for a declaration being written into
    // ours: joining an absolute path onto the source directory discards the source directory.
    // Rejected here for the same reason `payload_shape` refuses a leading `/` as a root entry.
    if declared.starts_with('/') || Path::new(declared).is_absolute() {
        return Err(BuildError::UnsafeRequiredPath {
            entry: declared.to_string(),
            reason: "is an absolute path".to_string(),
        });
    }

    // The runtime's own entry-name rule, applied at authoring time: an entry that a
    // `.mur.zip` reader would refuse to unpack must never be written into one.
    sanitize_entry_path(declared).map_err(|error| BuildError::UnsafeRequiredPath {
        entry: declared.to_string(),
        reason: match error {
            ZipGuardError::UnsafeEntryPath { reason, .. } => reason,
            other => other.to_string(),
        },
    })?;

    let path = source_dir.join(declared);
    if !path.exists() {
        return Err(BuildError::MissingRequiredFile {
            file: declared.to_string(),
            path: source_dir.display().to_string(),
        });
    }

    // Read the link itself, not its target: following it would ship bytes from wherever it
    // points — plausibly outside the source tree — under the declared name.
    let metadata = fs::symlink_metadata(&path).map_err(|source| BuildError::ReadSource {
        path: path.display().to_string(),
        source,
    })?;
    if metadata.file_type().is_symlink() {
        return Err(BuildError::SymlinkedRequiredFile {
            entry: declared.to_string(),
            path: path.display().to_string(),
        });
    }

    Ok(())
}

/// Validate an artifact `name:` as a format, and nothing more.
///
/// A name becomes a filename, a registry key and a store directory, so it is held to the
/// lowest common denominator of all three: ASCII lowercase, digits and inner hyphens. This is
/// deliberately *not* a namespace or prefix policy — who may publish which name is a registry
/// question, not something `mur build` gets an opinion about.
fn validate_artifact_name(name: &str) -> Result<(), BuildError> {
    let invalid = |reason: &str| BuildError::InvalidArtifactName {
        name: name.to_string(),
        reason: reason.to_string(),
    };

    if name.is_empty() {
        return Err(invalid("must not be empty"));
    }
    if name.chars().count() > MAX_ARTIFACT_NAME_LEN {
        return Err(invalid(&format!(
            "must be at most {MAX_ARTIFACT_NAME_LEN} characters"
        )));
    }
    if let Some(bad) = name
        .chars()
        .find(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-'))
    {
        return Err(invalid(&format!(
            "may contain only lowercase letters, digits and '-' (found {bad:?})"
        )));
    }
    if name.starts_with('-') || name.ends_with('-') {
        return Err(invalid("must not start or end with '-'"));
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
            "name: test-tool\nversion: 0.1.0\nruntime: tool\nrequires_files:\n  - config.json\n  - tool.wasm\n",
        )
        .unwrap();
        fs::write(dir.path().join("tool.wasm"), b"\0asm").unwrap();

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
            "name: nested\nversion: 0.1.0\nruntime: tool\nrequires_files:\n  - assets/logo.png\n  - tool.wasm\n",
        )
        .unwrap();
        fs::write(dir.path().join("tool.wasm"), b"\0asm").unwrap();
        fs::create_dir_all(dir.path().join("assets")).unwrap();
        fs::write(dir.path().join("assets/logo.png"), "png").unwrap();
        fs::write(dir.path().join("assets/other.txt"), "not declared").unwrap();

        let out = dir.path().join("artifact.mur.zip");
        build_artifact(dir.path(), &out).unwrap();

        assert_eq!(
            entry_names(&out),
            vec![
                PACKED_MANIFEST_ENTRY.to_string(),
                "assets/logo.png".to_string(),
                "tool.wasm".to_string()
            ],
            "only the declared member of assets/ ships"
        );
    }

    /// The accepted consequence of allowlist packing: an artifact that declares no companion
    /// files gets a manifest-only zip. Nothing infers a payload from the role or the name — an
    /// undeclared `.wasm` sitting in the directory is still not packed. A *wasm* artifact can no
    /// longer end up here (a manifest-only wasm payload fails the shape check, see
    /// `wasm_artifact_without_a_root_wasm_fails_before_any_zip_is_written`), so the role is
    /// declared `static`.
    #[test]
    fn empty_requires_files_packs_a_manifest_only_zip() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join(MANIFEST_FILENAME),
            "name: undeclared\nversion: 0.1.0\nruntime: tool\nexecution: static\n",
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
            "name: redundant\nversion: 0.1.0\nruntime: tool\nexecution: static\nrequires_files:\n  - murmur.yaml\n  - payload.bin\n",
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
            "name: hello\nversion: 0.0.1\nruntime: wasm\nrequires_files:\n  - assets/file.txt\n  - tool.wasm\n",
        )
        .unwrap();
        fs::write(dir.path().join("tool.wasm"), b"\0asm").unwrap();
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
            "name: root\nversion: 0.0.2\nruntime: wasm\nrequires_files:\n  - tools/murmur.yaml\n  - tool.wasm\n",
        )
        .unwrap();
        fs::write(dir.path().join("tool.wasm"), b"\0asm").unwrap();
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
            "name: roundtrip\nversion: 0.4.2\nruntime: wasm\nrequires_files:\n  - tool.wasm\n",
        )
        .unwrap();
        fs::write(dir.path().join("tool.wasm"), b"\0asm").unwrap();

        let out = dir.path().join("artifact.mur.zip");
        build_artifact(dir.path(), &out).unwrap();
        let bytes = fs::read(&out).unwrap();

        let yaml = crate::artifact::load_manifest_yaml_from_artifact_bytes(&bytes).unwrap();
        assert!(yaml.contains("name: roundtrip"));

        let manifest = crate::artifact::load_manifest_from_artifact_bytes(&bytes).unwrap();
        assert_eq!(manifest.name, "roundtrip");
        assert_eq!(manifest.version, "0.4.2");
    }

    // ── name format ─────────────────────────────────────────────────────────────────────

    /// Writes a manifest declaring `name` and builds it, returning the outcome.
    fn build_named(name: &str) -> Result<PathBuf, BuildError> {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join(MANIFEST_FILENAME),
            format!("name: {name}\nversion: 0.1.0\nruntime: skill\nrequires_files: []\n"),
        )
        .unwrap();

        let out = dir.path().join("artifact.mur.zip");
        let result = build_artifact(dir.path(), &out);
        if result.is_err() {
            assert!(!out.exists(), "no zip should be created on failure");
        }
        result
    }

    #[test]
    fn malformed_artifact_names_are_rejected() {
        for (name, expected_reason) in [
            ("My-Tool", "lowercase"),
            ("my tool", "lowercase"),
            ("tools/my-tool", "lowercase"),
            ("tools\\\\my-tool", "lowercase"),
            ("my_tool", "lowercase"),
            ("-my-tool", "start or end"),
            ("my-tool-", "start or end"),
            ("''", "empty"),
        ] {
            let err = build_named(name)
                .expect_err("expected an invalid-name failure")
                .to_string();
            assert!(
                err.contains("invalid artifact name") && err.contains(expected_reason),
                "name {name:?} should be rejected as {expected_reason}; got: {err}"
            );
        }
    }

    #[test]
    fn an_over_long_artifact_name_is_rejected() {
        let name = "a".repeat(MAX_ARTIFACT_NAME_LEN + 1);
        let err = build_named(&name).unwrap_err().to_string();
        assert!(
            err.contains("at most 100 characters"),
            "expected a length rejection; got: {err}"
        );

        build_named(&"a".repeat(MAX_ARTIFACT_NAME_LEN)).expect("the bound itself is allowed");
    }

    /// The project's own artifacts are named `murmur-*`; validation is about format, never
    /// about reserving a brand prefix.
    #[test]
    fn conventional_artifact_names_including_murmur_prefixed_ones_are_accepted() {
        for name in [
            "murmur-hook-compact",
            "murmur-tool-git",
            "murmur-driver-anthropic",
            "hello-slice",
            "jsonl-line-count",
            "tool2",
        ] {
            build_named(name).unwrap_or_else(|err| panic!("{name} should build; got: {err}"));
        }
    }

    // ── structural path safety ──────────────────────────────────────────────────────────

    #[test]
    fn a_traversing_requires_files_entry_is_rejected() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("secret.txt"), "outside").unwrap();
        let source = dir.path().join("artifact");
        fs::create_dir_all(&source).unwrap();
        fs::write(
            source.join(MANIFEST_FILENAME),
            "name: traversal\nversion: 0.1.0\nruntime: skill\nrequires_files:\n  - ../secret.txt\n",
        )
        .unwrap();

        let out = source.join("artifact.mur.zip");
        let err = build_artifact(&source, &out).unwrap_err();

        assert!(
            matches!(&err, BuildError::UnsafeRequiredPath { entry, reason } if entry == "../secret.txt" && reason.contains("'..'")),
            "expected UnsafeRequiredPath; got: {err}"
        );
        assert!(!out.exists(), "no zip should be created on failure");
    }

    #[test]
    fn an_absolute_requires_files_entry_is_rejected() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join(MANIFEST_FILENAME),
            "name: absolute\nversion: 0.1.0\nruntime: skill\nrequires_files:\n  - /etc/hosts\n",
        )
        .unwrap();

        let out = dir.path().join("artifact.mur.zip");
        let err = build_artifact(dir.path(), &out).unwrap_err();

        assert!(
            matches!(&err, BuildError::UnsafeRequiredPath { entry, reason } if entry == "/etc/hosts" && reason.contains("absolute")),
            "expected UnsafeRequiredPath; got: {err}"
        );
        assert!(!out.exists(), "no zip should be created on failure");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_requires_files_entry_is_rejected_without_following_it() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("real.txt"), "real bytes").unwrap();
        std::os::unix::fs::symlink(dir.path().join("real.txt"), dir.path().join("link.txt"))
            .unwrap();
        fs::write(
            dir.path().join(MANIFEST_FILENAME),
            "name: symlinked\nversion: 0.1.0\nruntime: skill\nrequires_files:\n  - link.txt\n",
        )
        .unwrap();

        let out = dir.path().join("artifact.mur.zip");
        let err = build_artifact(dir.path(), &out).unwrap_err();

        assert!(
            matches!(&err, BuildError::SymlinkedRequiredFile { entry, .. } if entry == "link.txt"),
            "expected SymlinkedRequiredFile; got: {err}"
        );
        assert!(!out.exists(), "no zip should be created on failure");
    }

    /// The archive name is a rewrite of the declared path, so two distinct files can collide in
    /// the archive even though the source-path dedup sees them as different files. On unix a
    /// literal backslash is an ordinary filename character, which is exactly how the rewrite
    /// becomes lossy.
    #[cfg(unix)]
    #[test]
    fn two_declared_files_may_not_claim_the_same_archive_entry() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/payload.bin"), "nested").unwrap();
        fs::write(dir.path().join("sub\\payload.bin"), "flat").unwrap();
        fs::write(
            dir.path().join(MANIFEST_FILENAME),
            "name: collision\nversion: 0.1.0\nruntime: skill\nrequires_files:\n  - sub/payload.bin\n  - sub\\payload.bin\n",
        )
        .unwrap();

        let out = dir.path().join("artifact.mur.zip");
        let err = build_artifact(dir.path(), &out).unwrap_err();

        assert!(
            matches!(&err, BuildError::DuplicateArchiveEntry { entry, .. } if entry == "sub/payload.bin"),
            "expected DuplicateArchiveEntry; got: {err}"
        );
        assert!(!out.exists(), "no zip should be created on failure");
    }

    // ── payload shape ───────────────────────────────────────────────────────────────────

    #[test]
    fn wasm_artifact_without_a_root_wasm_fails_before_any_zip_is_written() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join(MANIFEST_FILENAME),
            "name: no-payload\nversion: 0.1.0\nruntime: wasm\nrequires_files:\n  - README.md\n",
        )
        .unwrap();
        fs::write(dir.path().join("README.md"), "# docs\n").unwrap();

        let out = dir.path().join("artifact.mur.zip");
        let err = build_artifact(dir.path(), &out).unwrap_err();

        assert_eq!(
            err.to_string(),
            "missing root .wasm file (expected capsule.wasm or one root *.wasm)",
            "the build-time message is the runtime's message, verbatim"
        );
        assert!(!out.exists(), "no zip should be created on failure");
    }

    #[test]
    fn wasm_artifact_with_two_root_wasm_files_fails_with_sorted_names() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join(MANIFEST_FILENAME),
            "name: ambiguous\nversion: 0.1.0\nruntime: hook\nexecution: wasm\nrequires_files:\n  - zeta.wasm\n  - alpha.wasm\n",
        )
        .unwrap();
        fs::write(dir.path().join("zeta.wasm"), b"\0asm").unwrap();
        fs::write(dir.path().join("alpha.wasm"), b"\0asm").unwrap();

        let out = dir.path().join("artifact.mur.zip");
        let err = build_artifact(dir.path(), &out).unwrap_err();

        assert_eq!(
            err.to_string(),
            "multiple root .wasm files found: alpha.wasm, zeta.wasm"
        );
        assert!(!out.exists(), "no zip should be created on failure");
    }

    /// `capsule.wasm` resolves the ambiguity the runtime's own selection rule resolves, so this
    /// builds — the author gets a warning about it instead (see `build_lints`).
    #[test]
    fn capsule_wasm_beside_another_root_wasm_still_builds() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join(MANIFEST_FILENAME),
            "name: preferred\nversion: 0.1.0\nruntime: wasm\nrequires_files:\n  - capsule.wasm\n  - tool.wasm\n",
        )
        .unwrap();
        fs::write(dir.path().join("capsule.wasm"), b"\0asm").unwrap();
        fs::write(dir.path().join("tool.wasm"), b"\0asm").unwrap();

        let out = dir.path().join("artifact.mur.zip");
        build_artifact(dir.path(), &out).expect("capsule.wasm makes the selection unambiguous");
        assert_eq!(
            entry_names(&out),
            vec![
                PACKED_MANIFEST_ENTRY.to_string(),
                "capsule.wasm".to_string(),
                "tool.wasm".to_string()
            ]
        );
    }

    /// The payload-shape rule is the wasm rule. A native or static artifact is packed by the
    /// same curation without acquiring a `.wasm` requirement it never had.
    #[test]
    fn native_and_static_artifacts_are_not_held_to_the_wasm_payload_shape() {
        for manifest in [
            "name: native-tool\nversion: 0.1.0\nruntime: tool\nimplementation: native\nrequires_files: []\n",
            "name: static-thing\nversion: 0.1.0\nruntime: tool\nexecution: static\nrequires_files: []\n",
        ] {
            let dir = tempdir().unwrap();
            fs::write(dir.path().join(MANIFEST_FILENAME), manifest).unwrap();

            let out = dir.path().join("artifact.mur.zip");
            build_artifact(dir.path(), &out)
                .unwrap_or_else(|err| panic!("{manifest} should build; got: {err}"));
            assert_eq!(entry_names(&out), vec![PACKED_MANIFEST_ENTRY.to_string()]);
        }
    }
}
