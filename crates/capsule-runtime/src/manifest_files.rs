//! Where the files a capsule manifest names by path are read from: `inference.system_prompt_file`,
//! `inference.compaction.system_prompt_file` and a local-source `artifacts[].source`.
//!
//! A relative path resolves against the manifest. For a capsule launched from a project
//! (`mur run --manifest`) that is the folder holding `murmur.yaml`; for an installed capsule
//! (`mur run --capsule`) it is the installed package, whose entries are read from the bytes the
//! registry holds. The workdir, the current directory and the store root are never consulted for
//! an installed capsule. An absolute path is read from the host in both cases.
//!
//! A package path is displayed as `<scope>/<name>@<version>!/<entry>`, the form error messages
//! and [`ManifestFileReport::resolved`] carry.

use std::fs;
use std::io::{self, Cursor, ErrorKind};
use std::path::{Component, Path, PathBuf};

use bytes::Bytes;
use murmur_artifact::payload_shape::SKILL_MD_ENTRY;
use murmur_artifact::{zip_guard, SkillFolderError, BUILD_INFO_FILE, MANIFEST_FILENAME};
use serde::Serialize;
use zip::ZipArchive;

use crate::artifact::SkillFiles;
use crate::errors::RuntimeError;

/// [`ManifestFileReport::base`] for a relative path under `mur run --manifest`.
pub const BASE_MANIFEST_FOLDER: &str = "manifest-folder";
/// [`ManifestFileReport::base`] for a relative path under `mur run --capsule`.
pub const BASE_INSTALLED_PACKAGE: &str = "installed-package";
/// [`ManifestFileReport::base`] for an absolute path, under either launch.
pub const BASE_HOST_PATH: &str = "host-path";

/// The reason a relative path missing from an installed package fails with.
const NOT_IN_PACKAGE: &str = "not in the installed package; list it under requires_files: in the \
                              capsule's murmur.yaml and rebuild the package";
/// The reason a relative path with a `..` component fails with under `mur run --capsule`.
const OUTSIDE_PACKAGE: &str = "names a path outside the installed package";

/// A capsule launched by name: the artifact it resolved to and that artifact's package bytes.
#[derive(Clone)]
pub struct InstalledCapsule {
    /// `<scope>/<name>`, as the capsule was resolved by.
    pub artifact: String,
    pub version: String,
    /// The `.mur.zip` the registry holds for `artifact@version`.
    pub package: Bytes,
}

impl InstalledCapsule {
    /// `<artifact>@<version>`, the prefix of every package path this capsule displays.
    #[must_use]
    pub fn label(&self) -> String {
        format!("{}@{}", self.artifact, self.version)
    }
}

impl std::fmt::Debug for InstalledCapsule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstalledCapsule")
            .field("artifact", &self.artifact)
            .field("version", &self.version)
            .field("package_bytes", &self.package.len())
            .finish()
    }
}

/// What a relative path in a capsule's manifest resolves against.
#[derive(Debug, Clone)]
pub enum ManifestFiles {
    /// The folder holding the manifest, for a capsule launched from a project.
    Directory(PathBuf),
    /// The installed package, for a capsule launched by name.
    Package(InstalledCapsule),
}

/// Where one declared path is read from.
enum Location {
    /// A path on the host.
    Host(PathBuf),
    /// The package entry whose normalised name is this.
    Entry(String),
    /// A package path with a `..` component, which is refused.
    Outside,
}

/// One declared path and where it resolves, as `--explain-scope` and
/// `session_start.effective_grants.manifest_files` record it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ManifestFileReport {
    /// The manifest key: `inference.system_prompt_file`, `inference.compaction.system_prompt_file`
    /// or `artifacts[<name>].source`.
    pub key: String,
    /// The path as the manifest spells it.
    pub declared: String,
    /// Where it is read from: a host path, or `<scope>/<name>@<version>!/<entry>` for an entry of
    /// an installed package.
    pub resolved: String,
    /// [`BASE_MANIFEST_FOLDER`], [`BASE_INSTALLED_PACKAGE`] or [`BASE_HOST_PATH`].
    pub base: &'static str,
}

impl ManifestFiles {
    /// Where `declared` is read from, the string that names it, and its
    /// [`ManifestFileReport::base`]. Reads nothing.
    fn locate(&self, declared: &str) -> (Location, String, &'static str) {
        let raw = Path::new(declared);
        if raw.is_absolute() {
            return (
                Location::Host(raw.to_path_buf()),
                raw.display().to_string(),
                BASE_HOST_PATH,
            );
        }
        match self {
            // Joined without normalising: this string is what `E-RUN-009` and the skill-source
            // errors print for a project launch.
            Self::Directory(dir) => {
                let path = dir.join(raw);
                let shown = path.display().to_string();
                (Location::Host(path), shown, BASE_MANIFEST_FOLDER)
            }
            Self::Package(capsule) => match normalise_declared(declared) {
                Some(entry) => {
                    let shown = format!("{}!/{entry}", capsule.label());
                    (Location::Entry(entry), shown, BASE_INSTALLED_PACKAGE)
                }
                None => (
                    Location::Outside,
                    format!("{}!/{declared}", capsule.label()),
                    BASE_INSTALLED_PACKAGE,
                ),
            },
        }
    }

    /// Where `declared` resolves, as [`ManifestFileReport::resolved`] shows it.
    #[must_use]
    pub fn resolved(&self, declared: &str) -> String {
        self.locate(declared).1
    }

    /// The text of the file `declared` names. The error carries the string naming where it was
    /// looked for, and the I/O error: `NotFound` for an entry the package lacks, `InvalidInput`
    /// for a package path with a `..` component.
    pub fn read_to_string(&self, declared: &str) -> Result<String, (String, io::Error)> {
        let (location, shown, _) = self.locate(declared);
        let result = match (&location, self) {
            (Location::Host(path), _) => fs::read_to_string(path),
            (Location::Entry(entry), Self::Package(capsule)) => {
                read_package_entry(&capsule.package, entry).and_then(|bytes| {
                    String::from_utf8(bytes)
                        .map_err(|error| io::Error::new(ErrorKind::InvalidData, error))
                })
            }
            (Location::Outside, _) | (Location::Entry(_), Self::Directory(_)) => {
                Err(io::Error::new(ErrorKind::InvalidInput, OUTSIDE_PACKAGE))
            }
        };
        result.map_err(|source| (shown, source))
    }

    /// A local-source skill's files: each as its `/`-separated path under `tools/<name>/` and its
    /// bytes, `skill.md` first.
    ///
    /// `source` may name a file, installed alone as `skill.md`, or a folder, whose `skill.md`
    /// (any casing) and supporting files install by the rules
    /// [`murmur_artifact::collect_skill_folder`] applies to a folder on disk. A package folder is
    /// every entry under `<source>/`.
    pub fn read_skill_source(&self, source: &str) -> Result<SkillFiles, RuntimeError> {
        let (location, shown, _) = self.locate(source);
        match (location, self) {
            (Location::Host(path), _) => read_host_skill_source(&path),
            (Location::Entry(prefix), Self::Package(capsule)) => {
                read_package_skill_source(&capsule.package, &prefix, &shown)
            }
            (Location::Outside, _) | (Location::Entry(_), Self::Directory(_)) => {
                Err(RuntimeError::SkillSourceRead {
                    path: shown,
                    source: io::Error::new(ErrorKind::InvalidInput, OUTSIDE_PACKAGE),
                })
            }
        }
    }
}

/// A declared relative path as a package entry name: its normal components joined by `/`. `None`
/// when a component is `..`.
fn normalise_declared(declared: &str) -> Option<String> {
    let mut parts = Vec::new();
    for component in Path::new(declared).components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::CurDir => {}
            Component::ParentDir => return None,
            Component::RootDir | Component::Prefix(_) => {}
        }
    }
    Some(parts.join("/"))
}

/// An archive entry name normalised as [`normalise_declared`] normalises a declared path. `None`
/// for a name `zip_guard` refuses.
fn normalise_entry(raw_name: &str) -> Option<String> {
    let sanitized = zip_guard::sanitize_entry_path(raw_name).ok()?;
    Some(
        sanitized
            .components()
            .filter_map(|component| match component {
                Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("/"),
    )
}

/// One file entry of a package: its raw archive name, its normalised name, and whether it is a
/// symlink.
struct PackageEntry {
    raw: String,
    name: String,
    symlink: bool,
}

fn open_package(package: &[u8]) -> io::Result<ZipArchive<Cursor<&[u8]>>> {
    ZipArchive::new(Cursor::new(package)).map_err(|error| {
        io::Error::new(
            ErrorKind::InvalidData,
            format!("cannot open the installed package: {error}"),
        )
    })
}

/// Every file entry of `archive`, directories left out.
fn package_entries(archive: &mut ZipArchive<Cursor<&[u8]>>) -> io::Result<Vec<PackageEntry>> {
    let mut entries = Vec::new();
    for index in 0..archive.len() {
        let entry = archive
            .by_index_raw(index)
            .map_err(|error| io::Error::new(ErrorKind::InvalidData, error.to_string()))?;
        if entry.is_dir() {
            continue;
        }
        let raw = entry
            .name()
            .map_err(|error| io::Error::new(ErrorKind::InvalidData, error.to_string()))?
            .into_owned();
        let Some(name) = normalise_entry(&raw) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let symlink = entry.is_symlink();
        entries.push(PackageEntry { raw, name, symlink });
    }
    Ok(entries)
}

fn read_capped(
    archive: &mut ZipArchive<Cursor<&[u8]>>,
    raw_name: &str,
    max_bytes: u64,
) -> io::Result<Vec<u8>> {
    zip_guard::read_zip_entry_capped(archive, raw_name, max_bytes)
        .map_err(|error| io::Error::new(ErrorKind::InvalidData, error.to_string()))
}

/// The bytes of the entry of `package` whose normalised name is `entry`, under the artifact
/// decompression ceiling.
fn read_package_entry(package: &[u8], entry: &str) -> io::Result<Vec<u8>> {
    let not_found = || io::Error::new(ErrorKind::NotFound, NOT_IN_PACKAGE);
    if entry.is_empty() {
        return Err(not_found());
    }
    let mut archive = open_package(package)?;
    let raw = package_entries(&mut archive)?
        .into_iter()
        .find(|candidate| candidate.name == entry)
        .ok_or_else(not_found)?
        .raw;
    read_capped(
        &mut archive,
        &raw,
        zip_guard::max_artifact_decompressed_bytes(),
    )
}

/// A skill source in a package: the entry `prefix` names, or the folder of entries under
/// `<prefix>/`. `shown` is the package path errors name.
fn read_package_skill_source(
    package: &[u8],
    prefix: &str,
    shown: &str,
) -> Result<SkillFiles, RuntimeError> {
    let unreadable = |source| RuntimeError::SkillSourceRead {
        path: shown.to_string(),
        source,
    };
    let not_found = || RuntimeError::SkillSourceNotFound {
        path: shown.to_string(),
    };
    if prefix.is_empty() {
        return Err(not_found());
    }
    let mut archive = open_package(package).map_err(unreadable)?;
    let entries = package_entries(&mut archive).map_err(unreadable)?;
    let max_bytes = zip_guard::max_artifact_decompressed_bytes();

    if let Some(file) = entries.iter().find(|entry| entry.name == prefix) {
        let bytes = read_capped(&mut archive, &file.raw, max_bytes).map_err(unreadable)?;
        return Ok(vec![(SKILL_MD_ENTRY.to_string(), bytes)]);
    }

    let folder = PathBuf::from(shown);
    let under = format!("{prefix}/");
    let mut found_any = false;
    let mut skill_md: Option<&PackageEntry> = None;
    let mut supporting: Vec<(String, &PackageEntry)> = Vec::new();
    for entry in &entries {
        let Some(relative) = entry.name.strip_prefix(&under) else {
            continue;
        };
        found_any = true;
        // Skipped at any depth, as `collect_skill_folder` skips them on disk.
        if relative
            .split('/')
            .any(|part| part.starts_with('.') || part.to_lowercase().ends_with(".mur.zip"))
        {
            continue;
        }
        if entry.symlink {
            return Err(refused(SkillFolderError::Symlink {
                path: folder.join(relative),
            }));
        }
        if relative.contains('/') {
            supporting.push((relative.to_string(), entry));
            continue;
        }
        let lower = relative.to_lowercase();
        if lower == SKILL_MD_ENTRY {
            if let Some(first) = skill_md {
                let first_name = first.name.rsplit('/').next().unwrap_or_default();
                return Err(refused(SkillFolderError::DuplicateSkillMd {
                    dir: folder.clone(),
                    first: first_name.to_string(),
                    second: relative.to_string(),
                }));
            }
            skill_md = Some(entry);
        } else if lower == MANIFEST_FILENAME {
            // The skill's own manifest is never installed: staging writes one there.
        } else if relative == BUILD_INFO_FILE {
            return Err(refused(SkillFolderError::ReservedBuildInfo {
                path: folder.join(relative),
            }));
        } else {
            supporting.push((relative.to_string(), entry));
        }
    }
    if !found_any {
        return Err(not_found());
    }
    let Some(skill_md) = skill_md else {
        return Err(RuntimeError::SkillSourceMissingSkillMd {
            path: shown.to_string(),
        });
    };
    supporting.sort_by(|a, b| a.0.cmp(&b.0));

    let mut total = 0u64;
    let mut files = Vec::with_capacity(supporting.len() + 1);
    let skill_md_raw = skill_md.raw.clone();
    for (relative, raw) in std::iter::once((SKILL_MD_ENTRY.to_string(), skill_md_raw)).chain(
        supporting
            .into_iter()
            .map(|(relative, entry)| (relative, entry.raw.clone())),
    ) {
        let bytes =
            read_capped(&mut archive, &raw, max_bytes.saturating_sub(total)).map_err(|source| {
                RuntimeError::SkillSourceRead {
                    path: format!("{shown}/{relative}"),
                    source,
                }
            })?;
        total += bytes.len() as u64;
        files.push((relative, bytes));
    }
    Ok(files)
}

fn refused(error: SkillFolderError) -> RuntimeError {
    RuntimeError::SkillSourceRefused {
        message: error.to_string(),
    }
}

/// A skill source on the host: a file, installed alone as `skill.md`, or a folder walked by
/// [`murmur_artifact::collect_skill_folder`].
fn read_host_skill_source(path: &Path) -> Result<SkillFiles, RuntimeError> {
    if !path.exists() {
        return Err(RuntimeError::SkillSourceNotFound {
            path: path.display().to_string(),
        });
    }

    let read = |file: &Path| {
        fs::read(file).map_err(|source| RuntimeError::SkillSourceRead {
            path: file.display().to_string(),
            source,
        })
    };
    if !path.is_dir() {
        return Ok(vec![(SKILL_MD_ENTRY.to_string(), read(path)?)]);
    }

    let folder = murmur_artifact::collect_skill_folder(path).map_err(|error| match error {
        SkillFolderError::MissingSkillMd { .. } => RuntimeError::SkillSourceMissingSkillMd {
            path: path.display().to_string(),
        },
        SkillFolderError::Io { path, source } => RuntimeError::SkillSourceRead {
            path: path.display().to_string(),
            source,
        },
        refused => RuntimeError::SkillSourceRefused {
            message: refused.to_string(),
        },
    })?;
    let mut files = vec![(SKILL_MD_ENTRY.to_string(), read(&folder.skill_md)?)];
    for (relative, file) in &folder.supporting_files {
        files.push((relative.clone(), read(file)?));
    }
    Ok(files)
}

/// One row per path the manifest names, in the order `inference.system_prompt_file`,
/// `inference.compaction.system_prompt_file`, then `artifacts[<name>].source` for each
/// local-source entry of `sources` (`(name, source)`, in manifest order). Reads nothing.
///
/// `inference` is the configuration the launch resolves prompts from, so a `--system-prompt`
/// override that clears `system_prompt_file` leaves its row out.
#[must_use]
pub fn manifest_file_reports<'a>(
    files: &ManifestFiles,
    inference: Option<&murmur_artifact::InferenceConfig>,
    sources: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Vec<ManifestFileReport> {
    let row = |key: String, declared: &str| {
        let (_, resolved, base) = files.locate(declared);
        ManifestFileReport {
            key,
            declared: declared.to_string(),
            resolved,
            base,
        }
    };
    let mut rows = Vec::new();
    if let Some(inference) = inference {
        if let Some(path) = &inference.system_prompt_file {
            rows.push(row("inference.system_prompt_file".to_string(), path));
        }
        if let Some(path) = inference
            .compaction
            .as_ref()
            .and_then(|compaction| compaction.system_prompt_file.as_ref())
        {
            rows.push(row(
                "inference.compaction.system_prompt_file".to_string(),
                path,
            ));
        }
    }
    for (name, source) in sources {
        rows.push(row(format!("artifacts[{name}].source"), source));
    }
    rows
}

/// The `Manifest files` block of `mur run --explain-scope`, empty when `rows` is. Starts with a
/// blank line and ends with a newline.
#[must_use]
pub fn render_manifest_files(rows: &[ManifestFileReport]) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let mut out = String::from("\nManifest files\n");
    for row in rows {
        out.push_str(&format!(
            "  {}: {} -> {}\n",
            row.key, row.declared, row.resolved
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use zip::write::SimpleFileOptions;
    use zip::ZipWriter;

    use super::*;

    fn package(entries: &[(&str, &[u8])]) -> InstalledCapsule {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, body) in entries {
            writer
                .start_file(*name, SimpleFileOptions::default())
                .unwrap();
            writer.write_all(body).unwrap();
        }
        InstalledCapsule {
            artifact: "local/house".to_string(),
            version: "0.1.0".to_string(),
            package: Bytes::from(writer.finish().unwrap().into_inner()),
        }
    }

    fn house() -> ManifestFiles {
        ManifestFiles::Package(package(&[
            ("murmur.yaml", b"name: house\n"),
            ("compaction.md", b"Summarise tersely."),
            ("./instructions.md", b"Package instructions."),
            ("skills/house/SKILL.md", b"# House skill"),
            ("skills/house/references/a.md", b"reference a"),
            ("skills/house/.hidden", b"hidden"),
            ("skills/house/murmur.yaml", b"name: inner\n"),
            ("skills/twice/SKILL.md", b"one"),
            ("skills/twice/skill.md", b"two"),
            ("skills/bare/notes.md", b"no skill"),
            ("skills/built/skill.md", b"built"),
            ("skills/built/murmur-build.json", b"{}"),
        ]))
    }

    #[test]
    fn every_spelling_of_a_relative_path_reads_the_normalised_entry() {
        let files = house();
        for declared in [
            "instructions.md",
            "./instructions.md",
            "././instructions.md",
        ] {
            assert_eq!(
                files.read_to_string(declared).unwrap(),
                "Package instructions.",
                "{declared}"
            );
            assert_eq!(
                files.resolved(declared),
                "local/house@0.1.0!/instructions.md"
            );
        }
    }

    #[test]
    fn an_entry_missing_from_the_package_is_not_found_and_names_requires_files() {
        let (shown, error) = house().read_to_string("./missing.md").unwrap_err();
        assert_eq!(shown, "local/house@0.1.0!/missing.md");
        assert_eq!(error.kind(), ErrorKind::NotFound);
        assert!(error.to_string().contains("requires_files"), "{error}");
    }

    #[test]
    fn a_path_naming_only_the_package_root_is_not_found() {
        let (_, error) = house().read_to_string("./").unwrap_err();
        assert_eq!(error.kind(), ErrorKind::NotFound);
    }

    #[test]
    fn a_package_path_that_climbs_out_is_invalid_input() {
        let (shown, error) = house().read_to_string("../x").unwrap_err();
        assert_eq!(shown, "local/house@0.1.0!/../x");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert!(error.to_string().contains("outside the installed package"));
        let (_, error) = house().read_to_string("a/../../x").unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
    }

    #[test]
    fn an_absolute_path_reads_the_host_file_under_either_base() {
        let dir = tempfile::tempdir().unwrap();
        let host = dir.path().join("host.md");
        fs::write(&host, "Host text.").unwrap();
        let declared = host.to_str().unwrap();
        assert_eq!(house().read_to_string(declared).unwrap(), "Host text.");
        let other = ManifestFiles::Directory(PathBuf::from("/nonexistent"));
        assert_eq!(other.read_to_string(declared).unwrap(), "Host text.");
        assert_eq!(house().resolved(declared), declared);
    }

    #[test]
    fn a_directory_joins_the_declared_path_without_normalising() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("instructions.md"), "Folder text.").unwrap();
        let files = ManifestFiles::Directory(dir.path().to_path_buf());
        assert_eq!(
            files.read_to_string("./instructions.md").unwrap(),
            "Folder text."
        );
        let (shown, error) = files.read_to_string("./missing.md").unwrap_err();
        assert_eq!(shown, dir.path().join("./missing.md").display().to_string());
        assert_eq!(error.kind(), ErrorKind::NotFound);
    }

    #[test]
    fn a_package_file_source_installs_as_skill_md() {
        let files = house().read_skill_source("./compaction.md").unwrap();
        assert_eq!(
            files,
            vec![("skill.md".to_string(), b"Summarise tersely.".to_vec())]
        );
    }

    #[test]
    fn a_package_folder_source_installs_skill_md_and_supporting_files() {
        let files = house().read_skill_source("skills/house/").unwrap();
        let names: Vec<&str> = files.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["skill.md", "references/a.md"]);
        assert_eq!(files[0].1, b"# House skill");
    }

    #[test]
    fn a_package_folder_with_two_skill_mds_is_refused() {
        match house().read_skill_source("skills/twice") {
            Err(RuntimeError::SkillSourceRefused { message }) => {
                assert!(
                    message.contains("local/house@0.1.0!/skills/twice"),
                    "{message}"
                );
            }
            other => panic!("expected SkillSourceRefused, got {other:?}"),
        }
    }

    #[test]
    fn a_package_folder_with_build_info_is_refused() {
        assert!(matches!(
            house().read_skill_source("skills/built"),
            Err(RuntimeError::SkillSourceRefused { .. })
        ));
    }

    #[test]
    fn a_package_folder_without_skill_md_names_the_folder() {
        match house().read_skill_source("skills/bare") {
            Err(RuntimeError::SkillSourceMissingSkillMd { path }) => {
                assert_eq!(path, "local/house@0.1.0!/skills/bare");
            }
            other => panic!("expected SkillSourceMissingSkillMd, got {other:?}"),
        }
    }

    #[test]
    fn a_missing_package_prefix_is_not_found() {
        match house().read_skill_source("skills/absent") {
            Err(RuntimeError::SkillSourceNotFound { path }) => {
                assert_eq!(path, "local/house@0.1.0!/skills/absent");
            }
            other => panic!("expected SkillSourceNotFound, got {other:?}"),
        }
    }

    #[test]
    fn a_climbing_package_skill_source_is_refused() {
        match house().read_skill_source("../skills/house") {
            Err(RuntimeError::SkillSourceRead { path, source }) => {
                assert_eq!(path, "local/house@0.1.0!/../skills/house");
                assert_eq!(source.kind(), ErrorKind::InvalidInput);
            }
            other => panic!("expected SkillSourceRead, got {other:?}"),
        }
    }

    #[test]
    fn reports_list_declared_paths_in_manifest_order() {
        let inference = murmur_artifact::InferenceConfig {
            transport: "http".to_string(),
            model: "test-model".to_string(),
            driver: None,
            command: None,
            compaction: Some(murmur_artifact::CompactionConfig {
                threshold: None,
                model: None,
                system_prompt: None,
                system_prompt_file: Some("compaction.md".to_string()),
                dump_summaries: None,
                max_tokens: None,
                driver_config: None,
            }),
            system_prompt: None,
            system_prompt_file: Some("./instructions.md".to_string()),
            system_prompt_artifact: None,
            instruction_files: None,
            max_turns: None,
            max_tokens: None,
            max_session_tokens: None,
            tool_refresh: murmur_artifact::ToolRefresh::Compaction,
            alternates: Vec::new(),
            retry: murmur_artifact::InferenceRetry::default(),
        };
        let rows = manifest_file_reports(
            &house(),
            Some(&inference),
            [("house", "skills/house"), ("abs", "/opt/skill.md")],
        );
        assert_eq!(
            rows,
            vec![
                ManifestFileReport {
                    key: "inference.system_prompt_file".to_string(),
                    declared: "./instructions.md".to_string(),
                    resolved: "local/house@0.1.0!/instructions.md".to_string(),
                    base: BASE_INSTALLED_PACKAGE,
                },
                ManifestFileReport {
                    key: "inference.compaction.system_prompt_file".to_string(),
                    declared: "compaction.md".to_string(),
                    resolved: "local/house@0.1.0!/compaction.md".to_string(),
                    base: BASE_INSTALLED_PACKAGE,
                },
                ManifestFileReport {
                    key: "artifacts[house].source".to_string(),
                    declared: "skills/house".to_string(),
                    resolved: "local/house@0.1.0!/skills/house".to_string(),
                    base: BASE_INSTALLED_PACKAGE,
                },
                ManifestFileReport {
                    key: "artifacts[abs].source".to_string(),
                    declared: "/opt/skill.md".to_string(),
                    resolved: "/opt/skill.md".to_string(),
                    base: BASE_HOST_PATH,
                },
            ]
        );
        let folder = ManifestFiles::Directory(PathBuf::from("/src/house"));
        let rows = manifest_file_reports(&folder, Some(&inference), []);
        assert_eq!(rows[0].resolved, "/src/house/./instructions.md");
        assert_eq!(rows[0].base, BASE_MANIFEST_FOLDER);
        assert!(manifest_file_reports(&folder, None, []).is_empty());
    }

    #[test]
    fn the_block_renders_only_when_rows_exist() {
        assert_eq!(render_manifest_files(&[]), "");
        let rows = vec![ManifestFileReport {
            key: "inference.system_prompt_file".to_string(),
            declared: "./instructions.md".to_string(),
            resolved: "local/house@0.1.0!/instructions.md".to_string(),
            base: BASE_INSTALLED_PACKAGE,
        }];
        assert_eq!(
            render_manifest_files(&rows),
            "\nManifest files\n  inference.system_prompt_file: ./instructions.md -> \
             local/house@0.1.0!/instructions.md\n"
        );
    }
}
