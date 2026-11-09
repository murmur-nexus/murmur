use std::{
    fs,
    io::Write as _,
    path::{Path, PathBuf},
};

use murmur_artifact::{
    build_artifact, build_warning_link, check_skill_md, collect_skill_folder, is_skill_md_spelling,
    lint_build_warnings, load_manifest, newest_fitting_level, refuse_skill_manifest,
    resolve_manifest_path, scan_yaml_secrets, security_warning_link, BuildInfo, SkillFolderError,
    SkillVersionRule, WitContracts, BUILD_INFO_FILE, PACKED_MANIFEST_ENTRY, SKILL_MD_ENTRY,
    W_SEC_004,
};
use zip::{
    write::{FileOptions, SimpleFileOptions},
    CompressionMethod, ZipWriter,
};

use crate::error::{CliError, E_BLD_002, E_BLD_009, E_IO_001, E_IO_003, E_MAN_002};

pub(crate) fn run_build(source: &Path, output_arg: Option<&Path>) -> Result<(), CliError> {
    if !holds_skill_md_spelling(source) {
        return run_build_standard(source, output_arg);
    }
    let cwd = std::env::current_dir().map_err(|e| {
        CliError::new(
            E_IO_003,
            format!("failed to determine working directory: {e}"),
        )
    })?;
    let out = build_skill_artifact(source, output_arg.unwrap_or(&cwd))?;
    capsule_runtime::report_println!(
        "Built artifact: {}{}",
        out.display(),
        level_suffix(skill_built_at_level())
    );
    Ok(())
}

/// A folder whose root holds a file spelling `skill.md` in any casing. A spelling other than
/// `SKILL.md` takes the skill path too, so that it is refused with the rename it needs rather
/// than with "murmur.yaml not found".
fn holds_skill_md_spelling(source: &Path) -> bool {
    source.is_dir()
        && fs::read_dir(source).is_ok_and(|entries| {
            entries
                .flatten()
                .any(|entry| is_skill_md_spelling(&entry.file_name().to_string_lossy()))
        })
}

fn run_build_standard(source: &Path, output_arg: Option<&Path>) -> Result<(), CliError> {
    let manifest_path = resolve_manifest_path(source);
    let manifest = load_manifest(&manifest_path).map_err(CliError::from)?;
    refuse_skill_manifest(&manifest, &manifest_path).map_err(CliError::from)?;

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

/// A packed file besides `murmur.yaml` and `SKILL.md`.
struct ExtraFile {
    /// The archive entry name.
    name: String,
    bytes: Vec<u8>,
    /// Where the build read it from.
    source: PathBuf,
}

fn read_source_file(path: &Path) -> Result<Vec<u8>, CliError> {
    fs::read(path)
        .map_err(|e| CliError::new(E_IO_003, format!("cannot read {}: {e}", path.display())))
}

fn skill_folder_error(dir: &Path, error: SkillFolderError) -> CliError {
    let code = match &error {
        SkillFolderError::MissingSkillMd { .. } => E_IO_001,
        SkillFolderError::Io { path, source } if path == dir => {
            return CliError::new(
                E_IO_001,
                format!("cannot read directory {}: {source}", dir.display()),
            )
        }
        SkillFolderError::Io { .. } => E_IO_003,
        error if error.breaks_standard() => E_BLD_009,
        _ => E_BLD_002,
    };
    match error.hint() {
        Some(hint) => CliError::with_hint(code, error.to_string(), hint),
        None => CliError::new(code, error.to_string()),
    }
}

/// Packs the Agent Skills folder `input` into a `.mur.zip`.
///
/// The artifact is named, described and versioned from the `SKILL.md` front block alone, which
/// [`check_skill_md`] holds to the standard with `metadata.version` required. The archive holds
/// a generated `murmur.yaml`, `SKILL.md` byte for byte, every file [`collect_skill_folder`]
/// finds, and `murmur-build.json`. `output` is a directory (the zip is written there as
/// `<name>-<version>.mur.zip`) or the output file. Every refusal happens before anything is
/// written.
pub(crate) fn build_skill_artifact(input: &Path, output: &Path) -> Result<PathBuf, CliError> {
    let folder = collect_skill_folder(input).map_err(|e| skill_folder_error(input, e))?;
    let skill_md_bytes = read_source_file(&folder.skill_md)?;
    let front = check_skill_md(
        &folder.skill_md,
        &skill_md_bytes,
        folder.folder_name.as_deref(),
        SkillVersionRule::Required,
    )
    .map_err(|e| skill_folder_error(input, e))?;
    let mut extra = Vec::with_capacity(folder.supporting_files.len());
    for (name, path) in folder.supporting_files {
        let bytes = read_source_file(&path)?;
        extra.push(ExtraFile {
            name,
            bytes,
            source: path,
        });
    }

    let name = front.name;
    let version = front.version.unwrap_or_default();
    let manifest_bytes = format!(
        "name: {name}\nversion: '{version}'\nruntime: skill\n{}",
        description_line(&front.description)?
    )
    .into_bytes();

    let output_name = format!("{name}-{version}.mur.zip");
    let output_path = if output.is_dir() {
        output.join(&output_name)
    } else {
        output.to_path_buf()
    };
    // An output inside the folder under a name the walk does not skip is never packed into
    // itself.
    if let Ok(output_canonical) = fs::canonicalize(&output_path) {
        extra.retain(|file| {
            fs::canonicalize(&file.source).map_or(true, |source| source != output_canonical)
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

    zip.start_file(PACKED_MANIFEST_ENTRY, options)
        .map_err(|e| {
            CliError::new(
                E_IO_003,
                format!("zip error writing {PACKED_MANIFEST_ENTRY}: {e}"),
            )
        })?;
    zip.write_all(manifest_bytes)
        .map_err(|e| CliError::new(E_IO_003, format!("zip write error: {e}")))?;

    zip.start_file(SKILL_MD_ENTRY, options)
        .map_err(|e| CliError::new(E_IO_003, format!("zip error writing {SKILL_MD_ENTRY}: {e}")))?;
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
    use std::io::Read as _;

    use super::*;
    use tempfile::tempdir;
    use zip::ZipArchive;

    const PDF_HELPER_SKILL_MD: &str = "---\nname: pdf-helper\ndescription: Fill PDF forms; use when a form must be filled.\nmetadata:\n  version: \"1.2.0\"\n---\n# PDF helper\nSee references/forms.md.\n";

    /// `<base>/pdf-helper` holding `skill_md_name` with the S1 text.
    fn make_skill_dir(base: &Path, skill_md_name: &str) -> PathBuf {
        let dir = base.join("pdf-helper");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(skill_md_name), PDF_HELPER_SKILL_MD).unwrap();
        dir
    }

    fn zip_entries(path: &Path) -> Vec<String> {
        let file = fs::File::open(path).unwrap();
        let mut archive = ZipArchive::new(file).unwrap();
        let mut names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().unwrap().into_owned())
            .collect();
        names.sort();
        names
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
    fn a_skill_folder_builds_from_its_front_block_alone() {
        let src = tempdir().unwrap();
        let out = tempdir().unwrap();
        let dir = make_skill_dir(src.path(), "SKILL.md");

        let zip_path = build_skill_artifact(&dir, out.path()).unwrap();
        assert_eq!(zip_path, out.path().join("pdf-helper-1.2.0.mur.zip"));
        assert_eq!(
            zip_entries(&zip_path),
            ["SKILL.md", BUILD_INFO_FILE, "murmur.yaml"]
        );
        assert_eq!(
            zip_entry_content(&zip_path, "SKILL.md"),
            PDF_HELPER_SKILL_MD
        );
        assert_eq!(
            zip_entry_content(&zip_path, "murmur.yaml"),
            "name: pdf-helper\nversion: '1.2.0'\nruntime: skill\ndescription: Fill PDF forms; use when a form must be filled.\n"
        );
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
        let dir = make_skill_dir(src.path(), "SKILL.md");
        fs::write(dir.join(BUILD_INFO_FILE), "{\"built_at_level\":9}").unwrap();

        let error = build_skill_artifact(&dir, out.path()).unwrap_err();
        assert_eq!(error.code, E_BLD_002);
        assert!(
            error.message.contains("reserved for mur build"),
            "{}",
            error.message
        );
        assert!(fs::read_dir(out.path()).unwrap().next().is_none());
    }

    #[test]
    fn build_line_suffix_names_the_level_or_that_there_is_none() {
        assert_eq!(level_suffix(Some(1)), " (WIT level 1)");
        assert_eq!(level_suffix(None), " (fits no WIT level this mur knows)");
    }

    #[test]
    fn a_misspelled_skill_md_is_refused_with_e_bld_009_and_nothing_written() {
        let src = tempdir().unwrap();
        let out = tempdir().unwrap();
        let dir = make_skill_dir(src.path(), "skill.md");

        let error = build_skill_artifact(&dir, out.path()).unwrap_err();
        assert_eq!(error.code, E_BLD_009);
        assert!(
            error
                .message
                .ends_with("skill.md: a skill's file is named SKILL.md, exactly"),
            "{}",
            error.message
        );
        assert_eq!(error.hint.as_deref(), Some("rename skill.md to SKILL.md"));
        assert!(fs::read_dir(out.path()).unwrap().next().is_none());
    }

    #[test]
    fn a_front_block_rule_is_refused_with_e_bld_009_naming_the_file() {
        let src = tempdir().unwrap();
        let out = tempdir().unwrap();
        let dir = make_skill_dir(src.path(), "SKILL.md");
        fs::write(
            dir.join("SKILL.md"),
            PDF_HELPER_SKILL_MD.replace("name: pdf-helper", "name: PDF-Helper"),
        )
        .unwrap();

        let error = build_skill_artifact(&dir, out.path()).unwrap_err();
        assert_eq!(error.code, E_BLD_009);
        assert_eq!(
            error.message,
            format!(
                "{}: front block name 'PDF-Helper' may contain only lowercase letters, digits and '-'",
                dir.join("SKILL.md").display()
            )
        );
        assert!(error.hint.is_some());
        assert!(fs::read_dir(out.path()).unwrap().next().is_none());
    }

    #[test]
    fn only_a_root_spelling_of_skill_md_takes_the_skill_path() {
        let src = tempdir().unwrap();
        assert!(holds_skill_md_spelling(&make_skill_dir(
            src.path(),
            "Skill.md"
        )));
        let nested = src.path().join("nested");
        fs::create_dir_all(nested.join("sub")).unwrap();
        fs::write(nested.join("sub/SKILL.md"), "x").unwrap();
        assert!(!holds_skill_md_spelling(&nested));
        assert!(!holds_skill_md_spelling(&src.path().join("missing")));
    }
}
