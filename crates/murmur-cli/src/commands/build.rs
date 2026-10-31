use std::{
    fs,
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
};

use murmur_artifact::{
    build_artifact, build_warning_link, collect_skill_folder, lint_build_warnings, load_manifest,
    newest_fitting_level, parse_skill_front_block, resolve_manifest_path, scan_yaml_secrets,
    security_warning_link, BuildInfo, SkillFolderError, SkillFrontBlock, WitContracts,
    BUILD_INFO_FILE, SKILL_MD_ENTRY, W_SEC_004,
};
use zip::{
    write::{FileOptions, SimpleFileOptions},
    CompressionMethod, ZipArchive, ZipWriter,
};

use crate::error::{CliError, E_BLD_002, E_IO_001, E_IO_003, E_MAN_002, E_MAN_003};

pub(crate) fn run_build(
    source: &Path,
    output_arg: Option<&Path>,
    skill: Option<Option<String>>,
    version: Option<&str>,
    summary: Option<&str>,
) -> Result<(), CliError> {
    let skill_arg = match skill {
        Some(skill_arg) => skill_arg,
        None if is_manifest_less_skill_folder(source) => None,
        None => return run_build_standard(source, output_arg),
    };
    let cwd = std::env::current_dir().map_err(|e| {
        CliError::new(
            E_IO_003,
            format!("failed to determine working directory: {e}"),
        )
    })?;
    let (name, input_path) = resolve_skill_args(skill_arg.as_deref(), source);
    let out = build_skill_artifact(
        &input_path,
        name.as_deref(),
        version.unwrap_or("0.1.0"),
        output_arg.unwrap_or(&cwd),
        summary,
    )?;
    capsule_runtime::report_println!(
        "Built artifact: {}{}",
        out.display(),
        level_suffix(skill_built_at_level())
    );
    Ok(())
}

/// A folder with no `murmur.yaml` and a root `skill.md` in any casing: the layout a Claude Code,
/// Codex or pi skill ships in, which `mur build` packs as `mur build --skill` would.
fn is_manifest_less_skill_folder(source: &Path) -> bool {
    if resolve_manifest_path(source).exists() || !source.is_dir() {
        return false;
    }
    fs::read_dir(source).is_ok_and(|entries| {
        entries
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().to_lowercase() == SKILL_MD_ENTRY)
    })
}

fn run_build_standard(source: &Path, output_arg: Option<&Path>) -> Result<(), CliError> {
    let manifest_path = resolve_manifest_path(source);
    let manifest = load_manifest(&manifest_path).map_err(CliError::from)?;

    for warning in scan_yaml_secrets(&manifest_path).map_err(CliError::from)? {
        let filename = Path::new(&warning.file)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| warning.file.clone());
        capsule_runtime::report_eprintln!(
            "warning[{}]: {}: field '{}' appears to contain a literal secret value\n  → use a ${{VAR_NAME}} reference instead and inject the value via environment\n  → this file may not be safe to commit to version control\n  → {}",
            W_SEC_004, filename, warning.field_path, security_warning_link(W_SEC_004)
        );
    }

    let default_name = format!("{}-{}.mur.zip", manifest.name, manifest.version);
    let output_path = resolve_output_path(source, output_arg, &default_name);

    // Authoring lints run over the same entry set the build is about to pack, so they print
    // before the artifact line — and before a payload-shape failure, when there is one.
    for warning in lint_build_warnings(source, &output_path).map_err(CliError::from)? {
        capsule_runtime::report_eprintln!(
            "warning[{}]: {}\n  → {}\n  → {}",
            warning.code,
            warning.message,
            warning.hint,
            build_warning_link(warning.code)
        );
    }

    let built = build_artifact(source, &output_path, capsule_runtime::wit_levels())
        .map_err(CliError::from)?;

    capsule_runtime::report_println!(
        "Built artifact: {}{}",
        built.path.display(),
        level_suffix(built.built_at_level)
    );
    Ok(())
}

/// The ` (WIT level N)` the `Built artifact:` line ends with, or what it says when the payload
/// fits no level and so packs no `murmur-build.json`.
fn level_suffix(built_at_level: Option<u64>) -> String {
    match built_at_level {
        Some(level) => format!(" (WIT level {level})"),
        None => " (fits no WIT level this mur knows)".to_string(),
    }
}

/// A skill packs no component, so it names no `murmur:*` interface and is labelled with the
/// newest level, the one this mur serves.
fn skill_built_at_level() -> Option<u64> {
    newest_fitting_level(&WitContracts::default(), capsule_runtime::wit_levels())
}

// Determines whether the --skill value is a path or a name.
//
// Values containing '/', '\', or starting with '.' are treated as paths.
// Anything else is treated as an explicit artifact name.
fn looks_like_path(s: &str) -> bool {
    s.contains('/') || s.contains('\\') || s.starts_with('.')
}

// Returns (explicit_name, input_path) from --skill's optional value and the positional source.
fn resolve_skill_args(skill_arg: Option<&str>, source: &Path) -> (Option<String>, PathBuf) {
    match skill_arg {
        // --skill with no value: source is the input path, name inferred
        None => (None, source.to_path_buf()),
        Some(val) => {
            // `ana/x` reads like a relative path, but when no such path exists it is a scoped
            // name, which `build_skill_artifact` refuses as one.
            let scoped_name =
                !Path::new(val).exists() && murmur_artifact::ArtifactName::parse(val).is_ok();
            if looks_like_path(val) && !scoped_name {
                // Looks like a path: treat as input, infer name
                (None, PathBuf::from(val))
            } else {
                // Treat as explicit artifact name; source is the input path
                (Some(val.to_string()), source.to_path_buf())
            }
        }
    }
}

// Infers an artifact name from a folder or zip path.
//
// Transform rules (in order):
//   1. Take only the filename component (strip directory parts)
//   2. Strip .zip extension (case-insensitive)
//   3. Lowercase
//   4. Replace any character that is not alphanumeric or '-' with '_'
//   5. Collapse consecutive underscores into one
//   6. Strip leading/trailing underscores and hyphens
//
// Hyphens are preserved (not replaced with underscores) because murmur artifact
// names conventionally use hyphens (e.g. murmur-tool-git, murmur-skill-create-manifest).
fn infer_name_from_path(path: &Path) -> String {
    // Strip trailing separators so file_name() works on "foo/" paths
    let s = path.to_string_lossy();
    let s = s.trim_end_matches(['/', '\\']);
    let stem_str = Path::new(s)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| s.to_string());

    // Strip .zip extension (case-insensitive)
    let stem = if stem_str.to_lowercase().ends_with(".zip") {
        stem_str[..stem_str.len() - 4].to_string()
    } else {
        stem_str
    };

    // Lowercase
    let stem = stem.to_lowercase();

    // Replace non-ASCII-alphanumeric-non-hyphen chars with underscores
    let stem: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();

    // Collapse consecutive underscores
    let mut result = String::new();
    let mut prev_underscore = false;
    for c in stem.chars() {
        if c == '_' {
            if !prev_underscore {
                result.push(c);
            }
            prev_underscore = true;
        } else {
            result.push(c);
            prev_underscore = false;
        }
    }

    // Strip leading/trailing underscores and hyphens
    let name = result.trim_matches(|c| c == '_' || c == '-').to_string();
    // Degenerate input (e.g. "---/") can produce an empty name — fall back to "skill"
    if name.is_empty() {
        "skill".to_string()
    } else {
        name
    }
}

/// What a skill build packs, read from its input.
struct SkillInput {
    skill_md: Vec<u8>,
    manifest: Option<Vec<u8>>,
    extra: Vec<ExtraFile>,
    /// The `SKILL.md` front block, read only for a folder with no `murmur.yaml`.
    front: SkillFrontBlock,
}

/// A packed file besides `murmur.yaml` and `skill.md`.
struct ExtraFile {
    /// The archive entry name.
    name: String,
    bytes: Vec<u8>,
    /// Where a folder build read it from; `None` for zip input.
    source: Option<PathBuf>,
}

fn read_skill_from_dir(dir: &Path) -> Result<SkillInput, CliError> {
    let folder = collect_skill_folder(dir).map_err(|e| skill_folder_error(dir, e))?;
    let skill_md = read_source_file(&folder.skill_md)?;
    let manifest = folder
        .manifest
        .as_deref()
        .map(read_source_file)
        .transpose()?;
    // The front block names and describes a skill only when no murmur.yaml does.
    let front = manifest
        .is_none()
        .then(|| parse_skill_front_block(&String::from_utf8_lossy(&skill_md)))
        .flatten()
        .unwrap_or_default();
    let mut extra = Vec::with_capacity(folder.supporting_files.len());
    for (name, path) in folder.supporting_files {
        let bytes = read_source_file(&path)?;
        extra.push(ExtraFile {
            name,
            bytes,
            source: Some(path),
        });
    }
    Ok(SkillInput {
        skill_md,
        manifest,
        extra,
        front,
    })
}

fn read_source_file(path: &Path) -> Result<Vec<u8>, CliError> {
    fs::read(path)
        .map_err(|e| CliError::new(E_IO_003, format!("cannot read {}: {e}", path.display())))
}

fn skill_folder_error(dir: &Path, error: SkillFolderError) -> CliError {
    match error {
        SkillFolderError::MissingSkillMd { .. } => CliError::new(E_IO_001, error.to_string()),
        SkillFolderError::Io {
            ref path,
            ref source,
        } if path == dir => CliError::new(
            E_IO_001,
            format!("cannot read directory {}: {source}", dir.display()),
        ),
        SkillFolderError::Io { .. } => CliError::new(E_IO_003, error.to_string()),
        SkillFolderError::DuplicateSkillMd { .. } => CliError::with_hint(
            E_BLD_002,
            error.to_string(),
            "keep one of them — the skill packs as skill.md either way",
        ),
        SkillFolderError::Symlink { .. } => CliError::with_hint(
            E_BLD_002,
            error.to_string(),
            "replace the link with the file it points to, or move it out of the skill folder",
        ),
        SkillFolderError::ReservedBuildInfo { .. } => CliError::with_hint(
            E_BLD_002,
            error.to_string(),
            "remove it from the skill — mur build writes murmur-build.json itself",
        ),
    }
}

fn read_skill_from_zip(zip_path: &Path) -> Result<SkillInput, CliError> {
    let file = fs::File::open(zip_path)
        .map_err(|e| CliError::new(E_IO_001, format!("cannot open {}: {e}", zip_path.display())))?;
    let mut archive = ZipArchive::new(file).map_err(|e| {
        CliError::new(
            E_IO_003,
            format!("cannot read zip {}: {e}", zip_path.display()),
        )
    })?;

    let mut skill_md: Option<(String, Vec<u8>)> = None;
    let mut manifest: Option<Vec<u8>> = None;
    let mut extra: Vec<ExtraFile> = Vec::new();

    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| CliError::new(E_IO_003, format!("zip read error: {e}")))?;

        // Root-level entries only: skip anything with a directory separator
        let entry_name = entry
            .name()
            .map_err(|e| CliError::new(E_IO_003, format!("zip read error: {e}")))?
            .into_owned();
        if entry_name.contains('/') || entry_name.contains('\\') || entry.is_dir() {
            continue;
        }
        if entry_name.is_empty() {
            continue;
        }

        let lower = entry_name.to_lowercase();
        let mut bytes = Vec::new();
        entry
            .read_to_end(&mut bytes)
            .map_err(|e| CliError::new(E_IO_003, format!("zip read error: {e}")))?;

        if lower == "skill.md" {
            skill_md = Some((entry_name, bytes));
        } else if lower == "murmur.yaml" {
            manifest = Some(bytes);
        } else {
            extra.push(ExtraFile {
                name: entry_name,
                bytes,
                source: None,
            });
        }
    }

    let (_, skill_md) = skill_md.ok_or_else(|| {
        CliError::new(
            E_IO_001,
            format!(
                "SKILL.md not found in zip {} (case-insensitive search found no match)",
                zip_path.display()
            ),
        )
    })?;

    Ok(SkillInput {
        skill_md,
        manifest,
        extra,
        front: SkillFrontBlock::default(),
    })
}

// Packages an external skill into a .mur.zip artifact.
//
// Input may be a directory or a .zip file containing SKILL.md (case-insensitive).
// A directory packs every file collect_skill_folder finds; a zip packs its root entries.
// When no murmur.yaml is present one is generated with runtime: skill, named and described from
// a folder's SKILL.md front block unless explicit_name / summary say otherwise.
// When murmur.yaml is present it must declare runtime: skill or an error is returned.
// output is a directory (the zip is written there as <name>-<version>.mur.zip) or the output file.
// skill.md is always stored as lowercase "skill.md" in the output zip.
// When summary is Some, it is written to (or overwrites) the description field in the manifest.
pub(crate) fn build_skill_artifact(
    input: &Path,
    explicit_name: Option<&str>,
    version: &str,
    output: &Path,
    summary: Option<&str>,
) -> Result<PathBuf, CliError> {
    let is_zip = input.to_string_lossy().to_lowercase().ends_with(".zip");

    let SkillInput {
        skill_md: skill_md_bytes,
        manifest: manifest_bytes_opt,
        mut extra,
        front,
    } = if is_zip {
        read_skill_from_zip(input)?
    } else {
        read_skill_from_dir(input)?
    };

    let artifact_name = match (explicit_name, front.name) {
        (Some(n), _) => n.to_string(),
        (None, Some(front_name)) => {
            murmur_artifact::validate_artifact_name(&front_name).map_err(|e| {
                let mut error = CliError::from(e);
                error.hint = Some(format!(
                    "the SKILL.md front block names the skill '{front_name}'; pass --skill <name> \
                     to build it under a valid artifact name"
                ));
                error
            })?;
            front_name
        }
        (None, None) => infer_name_from_path(input),
    };
    murmur_artifact::validate_artifact_name(&artifact_name).map_err(CliError::from)?;

    let manifest_bytes = match manifest_bytes_opt {
        Some(bytes) => {
            // Always validate runtime: skill.
            let runtime = {
                let yaml: serde_norway::Value = serde_norway::from_slice(&bytes).map_err(|e| {
                    CliError::new(E_MAN_002, format!("murmur.yaml: YAML parse error: {e}"))
                })?;
                yaml.get("runtime")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string()
            };
            if runtime != "skill" {
                return Err(CliError::new(
                    E_MAN_003,
                    format!(
                        "murmur.yaml declares runtime: \"{runtime}\" but --skill requires runtime: skill"
                    ),
                ));
            }
            // Only re-serialize when summary is being added; otherwise keep the file verbatim
            // so original YAML formatting (quoting, ordering) is preserved.
            if let Some(desc) = summary {
                let mut yaml: serde_norway::Value =
                    serde_norway::from_slice(&bytes).map_err(|e| {
                        CliError::new(E_MAN_002, format!("murmur.yaml: YAML parse error: {e}"))
                    })?;
                if let Some(mapping) = yaml.as_mapping_mut() {
                    mapping.insert(
                        serde_norway::Value::String("description".to_string()),
                        serde_norway::Value::String(desc.to_string()),
                    );
                }
                serde_norway::to_string(&yaml)
                    .map_err(|e| {
                        CliError::new(E_MAN_002, format!("murmur.yaml serialization error: {e}"))
                    })?
                    .into_bytes()
            } else {
                bytes
            }
        }
        None => {
            let desc_line = match summary.map(str::to_string).or(front.description) {
                Some(desc) => description_line(&desc)?,
                None => String::new(),
            };
            format!("name: {artifact_name}\nversion: '{version}'\nruntime: skill\n{desc_line}")
                .into_bytes()
        }
    };

    if let Some(file) = extra.iter().find(|file| file.name == BUILD_INFO_FILE) {
        return Err(CliError::with_hint(
            E_BLD_002,
            format!(
                "{} in {} packs as the archive entry '{BUILD_INFO_FILE}', which is reserved \
                 for mur build",
                file.name,
                input.display()
            ),
            "remove it from the skill — mur build writes murmur-build.json itself",
        ));
    }

    let output_name = format!("{artifact_name}-{version}.mur.zip");
    let output_path = if output.is_dir() {
        output.join(&output_name)
    } else {
        output.to_path_buf()
    };
    // An output inside the folder under a name the walk does not skip is never packed into
    // itself.
    if let Ok(output_canonical) = fs::canonicalize(&output_path) {
        extra.retain(|file| {
            file.source
                .as_deref()
                .and_then(|source| fs::canonicalize(source).ok())
                .is_none_or(|source| source != output_canonical)
        });
    }
    let output_file_name = output_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or(output_name);
    // Write to a temp file first, then rename — prevents a partial zip on failure.
    let tmp_path = output_path.with_file_name(format!(".{output_file_name}.tmp"));

    let write_result = write_skill_zip(
        &tmp_path,
        &manifest_bytes,
        &skill_md_bytes,
        &extra,
        skill_built_at_level(),
    );

    if let Err(e) = write_result {
        let _ = fs::remove_file(&tmp_path);
        return Err(e);
    }

    fs::rename(&tmp_path, &output_path).map_err(|e| {
        let _ = fs::remove_file(&tmp_path);
        CliError::new(
            E_IO_003,
            format!("failed to finalize {}: {e}", output_path.display()),
        )
    })?;

    Ok(output_path)
}

/// The generated manifest's `description:` line. A front-block description can carry `:`,
/// quotes and newlines, so the value goes through the YAML serializer.
fn description_line(description: &str) -> Result<String, CliError> {
    let mut mapping = serde_norway::Mapping::new();
    mapping.insert(
        serde_norway::Value::String("description".to_string()),
        serde_norway::Value::String(description.to_string()),
    );
    serde_norway::to_string(&mapping)
        .map_err(|e| CliError::new(E_MAN_002, format!("murmur.yaml serialization error: {e}")))
}

fn write_skill_zip(
    path: &Path,
    manifest_bytes: &[u8],
    skill_md_bytes: &[u8],
    extra_files: &[ExtraFile],
    built_at_level: Option<u64>,
) -> Result<(), CliError> {
    let output_file = fs::File::create(path).map_err(|e| {
        CliError::new(
            E_IO_003,
            format!("failed to create {}: {e}", path.display()),
        )
    })?;
    let mut zip = ZipWriter::new(output_file);
    let options: SimpleFileOptions =
        FileOptions::default().compression_method(CompressionMethod::Deflated);

    zip.start_file("murmur.yaml", options)
        .map_err(|e| CliError::new(E_IO_003, format!("zip error writing murmur.yaml: {e}")))?;
    zip.write_all(manifest_bytes)
        .map_err(|e| CliError::new(E_IO_003, format!("zip write error: {e}")))?;

    // skill.md normalized to lowercase in output zip
    zip.start_file("skill.md", options)
        .map_err(|e| CliError::new(E_IO_003, format!("zip error writing skill.md: {e}")))?;
    zip.write_all(skill_md_bytes)
        .map_err(|e| CliError::new(E_IO_003, format!("zip write error: {e}")))?;

    for ExtraFile { name, bytes, .. } in extra_files {
        zip.start_file(name.as_str(), options)
            .map_err(|e| CliError::new(E_IO_003, format!("zip error writing {name}: {e}")))?;
        zip.write_all(bytes)
            .map_err(|e| CliError::new(E_IO_003, format!("zip write error for {name}: {e}")))?;
    }

    if let Some(built_at_level) = built_at_level {
        zip.start_file(BUILD_INFO_FILE, options).map_err(|e| {
            CliError::new(
                E_IO_003,
                format!("zip error writing {BUILD_INFO_FILE}: {e}"),
            )
        })?;
        zip.write_all(BuildInfo { built_at_level }.to_json().as_bytes())
            .map_err(|e| CliError::new(E_IO_003, format!("zip write error: {e}")))?;
    }

    zip.finish()
        .map_err(|e| CliError::new(E_IO_003, format!("zip finish error: {e}")))?;

    Ok(())
}

fn resolve_output_path(source: &Path, output_arg: Option<&Path>, default_name: &str) -> PathBuf {
    match output_arg {
        None => source.join(default_name),
        Some(path) if path.exists() && path.is_dir() => path.join(default_name),
        Some(path) => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use zip::ZipArchive;

    fn make_skill_dir(dir: &Path, skill_md_name: &str) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(skill_md_name), "# My Skill\nDo things.\n").unwrap();
        dir.to_path_buf()
    }

    fn zip_entries(path: &Path) -> Vec<String> {
        let file = fs::File::open(path).unwrap();
        let mut archive = ZipArchive::new(file).unwrap();
        (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().unwrap().into_owned())
            .collect()
    }

    fn zip_entry_content(path: &Path, entry: &str) -> String {
        let file = fs::File::open(path).unwrap();
        let mut archive = ZipArchive::new(file).unwrap();
        let mut s = String::new();
        archive
            .by_name(entry)
            .unwrap()
            .read_to_string(&mut s)
            .unwrap();
        s
    }

    #[test]
    fn skill_build_from_folder_no_manifest_succeeds() {
        let src = tempdir().unwrap();
        let out = tempdir().unwrap();
        make_skill_dir(src.path(), "SKILL.md");

        let result =
            build_skill_artifact(src.path(), Some("test-skill"), "0.1.0", out.path(), None);
        assert!(result.is_ok(), "expected Ok, got: {result:?}");

        let zip_path = result.unwrap();
        assert!(zip_path.exists());
        let entries = zip_entries(&zip_path);
        assert!(entries.contains(&"murmur.yaml".to_string()));
        assert!(entries.contains(&"skill.md".to_string()));
        let level = capsule_runtime::served_wit_level().level;
        assert_eq!(skill_built_at_level(), Some(level));
        assert_eq!(
            zip_entry_content(&zip_path, BUILD_INFO_FILE),
            format!("{{\"built_at_level\":{level}}}\n")
        );
    }

    #[test]
    fn skill_build_refuses_an_authored_build_info_file() {
        let src = tempdir().unwrap();
        let out = tempdir().unwrap();
        make_skill_dir(src.path(), "SKILL.md");
        fs::write(src.path().join(BUILD_INFO_FILE), "{\"built_at_level\":9}").unwrap();

        let error = build_skill_artifact(src.path(), Some("test-skill"), "0.1.0", out.path(), None)
            .unwrap_err();
        assert_eq!(error.code, E_BLD_002);
        assert!(
            error.message.contains("reserved for mur build"),
            "{}",
            error.message
        );
        assert!(!out.path().join("test-skill-0.1.0.mur.zip").exists());
    }

    #[test]
    fn build_line_suffix_names_the_level_or_that_there_is_none() {
        assert_eq!(level_suffix(Some(1)), " (WIT level 1)");
        assert_eq!(level_suffix(None), " (fits no WIT level this mur knows)");
    }

    #[test]
    fn skill_build_infers_name_from_folder() {
        let base = tempdir().unwrap();
        let src = base.path().join("my-test-skill");
        let out = tempdir().unwrap();
        make_skill_dir(&src, "SKILL.md");

        let result = build_skill_artifact(&src, None, "0.1.0", out.path(), None);
        assert!(result.is_ok(), "expected Ok, got: {result:?}");

        let zip_path = result.unwrap();
        let manifest = zip_entry_content(&zip_path, "murmur.yaml");
        assert!(manifest.contains("name: my-test-skill"), "got: {manifest}");
        assert!(zip_path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("my-test-skill-"));
    }

    #[test]
    fn skill_build_explicit_name_overrides_inference() {
        let base = tempdir().unwrap();
        let src = base.path().join("some-folder");
        let out = tempdir().unwrap();
        make_skill_dir(&src, "skill.md");

        let result = build_skill_artifact(&src, Some("explicit-name"), "0.1.0", out.path(), None);
        assert!(result.is_ok(), "expected Ok, got: {result:?}");

        let zip_path = result.unwrap();
        let manifest = zip_entry_content(&zip_path, "murmur.yaml");
        assert!(manifest.contains("name: explicit-name"), "got: {manifest}");
        assert!(zip_path.to_str().unwrap().contains("explicit-name-0.1.0"));
    }

    #[test]
    fn skill_build_version_flag_sets_version() {
        let src = tempdir().unwrap();
        let out = tempdir().unwrap();
        make_skill_dir(src.path(), "SKILL.md");

        let result = build_skill_artifact(src.path(), Some("my-skill"), "2.3.4", out.path(), None);
        assert!(result.is_ok());

        let zip_path = result.unwrap();
        let manifest = zip_entry_content(&zip_path, "murmur.yaml");
        assert!(manifest.contains("version: '2.3.4'"), "got: {manifest}");
        assert!(zip_path
            .to_str()
            .unwrap()
            .contains("my-skill-2.3.4.mur.zip"));
    }

    #[test]
    fn skill_build_zip_input_succeeds() {
        // Build a zip containing SKILL.md and pass it as input
        let src = tempdir().unwrap();
        let out = tempdir().unwrap();

        let zip_path = src.path().join("input.zip");
        {
            let file = fs::File::create(&zip_path).unwrap();
            let mut zip = ZipWriter::new(file);
            let opts: SimpleFileOptions =
                FileOptions::default().compression_method(CompressionMethod::Deflated);
            zip.start_file("SKILL.md", opts).unwrap();
            zip.write_all(b"# Skill content\n").unwrap();
            zip.finish().unwrap();
        }

        let result = build_skill_artifact(&zip_path, Some("zip-skill"), "0.1.0", out.path(), None);
        assert!(result.is_ok(), "expected Ok, got: {result:?}");

        let artifact = result.unwrap();
        let entries = zip_entries(&artifact);
        assert!(entries.contains(&"murmur.yaml".to_string()));
        assert!(entries.contains(&"skill.md".to_string()));
    }

    #[test]
    fn skill_build_missing_skill_md_fails() {
        let src = tempdir().unwrap();
        let out = tempdir().unwrap();
        fs::write(src.path().join("README.md"), "just a readme").unwrap();

        let result = build_skill_artifact(src.path(), Some("no-skill"), "0.1.0", out.path(), None);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.to_lowercase().contains("skill.md"),
            "error should mention SKILL.md; got: {msg}"
        );
        // No zip should be produced
        assert!(!out.path().join("no-skill-0.1.0.mur.zip").exists());
    }

    #[test]
    fn skill_build_wrong_runtime_in_manifest_fails() {
        let src = tempdir().unwrap();
        let out = tempdir().unwrap();
        fs::write(src.path().join("SKILL.md"), "# Skill\n").unwrap();
        fs::write(
            src.path().join("murmur.yaml"),
            "name: x\nversion: '0.1.0'\nruntime: tool\n",
        )
        .unwrap();

        let result = build_skill_artifact(src.path(), None, "0.1.0", out.path(), None);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("runtime: skill"),
            "error should mention runtime: skill; got: {msg}"
        );
    }

    #[test]
    fn skill_build_valid_manifest_preserved() {
        let src = tempdir().unwrap();
        let out = tempdir().unwrap();
        fs::write(src.path().join("SKILL.md"), "# Skill\n").unwrap();
        fs::write(
            src.path().join("murmur.yaml"),
            "name: preserved-skill\nversion: '3.0.0'\nruntime: skill\n",
        )
        .unwrap();

        let result = build_skill_artifact(src.path(), None, "0.1.0", out.path(), None);
        assert!(result.is_ok(), "expected Ok, got: {result:?}");

        let zip_path = result.unwrap();
        let manifest = zip_entry_content(&zip_path, "murmur.yaml");
        // Manifest content comes from the pre-existing file, unchanged
        assert!(manifest.contains("name: preserved-skill"));
        assert!(manifest.contains("version: '3.0.0'"));
        assert!(manifest.contains("runtime: skill"));
    }

    #[test]
    fn skill_build_case_insensitive_skill_md_match() {
        // Test with lowercase skill.md
        let src = tempdir().unwrap();
        let out = tempdir().unwrap();
        fs::write(src.path().join("skill.md"), "# lowercase skill\n").unwrap();

        let result = build_skill_artifact(src.path(), Some("ci-skill"), "0.1.0", out.path(), None);
        assert!(
            result.is_ok(),
            "lowercase skill.md should be found; got: {result:?}"
        );

        let zip_path = result.unwrap();
        let entries = zip_entries(&zip_path);
        // Output always uses lowercase "skill.md"
        assert!(entries.contains(&"skill.md".to_string()));
    }

    #[test]
    fn infer_name_strips_trailing_slash() {
        assert_eq!(infer_name_from_path(Path::new("my-skill/")), "my-skill");
    }

    #[test]
    fn infer_name_strips_zip_extension() {
        assert_eq!(
            infer_name_from_path(Path::new("external-skill.zip")),
            "external-skill"
        );
    }

    #[test]
    fn infer_name_lowercases_and_replaces_spaces() {
        assert_eq!(
            infer_name_from_path(Path::new("My Coding Skill")),
            "my_coding_skill"
        );
    }
}
