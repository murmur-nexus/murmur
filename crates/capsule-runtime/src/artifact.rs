use murmur_artifact::payload_shape::{
    native_binary_entry, select_root_wasm_in_archive, SKILL_MD_ENTRY,
};
use murmur_artifact::zip_guard;
use murmur_artifact::PACKED_MANIFEST_ENTRY;
use zip::ZipArchive;

use crate::errors::RuntimeError;

pub fn extract_root_wasm(
    artifact_name: &str,
    artifact_version: &str,
    artifact_bytes: &[u8],
) -> Result<Vec<u8>, RuntimeError> {
    extract_root_wasm_capped(
        artifact_name,
        artifact_version,
        artifact_bytes,
        zip_guard::max_artifact_decompressed_bytes(),
    )
}

fn extract_root_wasm_capped(
    artifact_name: &str,
    artifact_version: &str,
    artifact_bytes: &[u8],
    max_bytes: u64,
) -> Result<Vec<u8>, RuntimeError> {
    let cursor = std::io::Cursor::new(artifact_bytes);
    let mut archive = ZipArchive::new(cursor).map_err(|err| RuntimeError::ArtifactArchive {
        name: artifact_name.to_string(),
        version: artifact_version.to_string(),
        message: err.to_string(),
    })?;

    // Which root entry counts as the wasm payload is the shared payload-shape contract; the
    // selector (and its error text) lives in murmur_artifact::payload_shape.
    let selected_name =
        select_root_wasm_in_archive(&mut archive).map_err(|err| RuntimeError::ArtifactArchive {
            name: artifact_name.to_string(),
            version: artifact_version.to_string(),
            message: err.to_string(),
        })?;

    zip_guard::read_zip_entry_capped(&mut archive, &selected_name, max_bytes).map_err(|err| {
        RuntimeError::ArtifactArchive {
            name: artifact_name.to_string(),
            version: artifact_version.to_string(),
            message: err.to_string(),
        }
    })
}

/// Extract the native binary from `bin/<artifact_name>` inside a `.mur.zip`.
///
/// Canonical native artifact layout:
///   murmur.yaml            — artifact manifest at zip root
///   bin/<artifact_name>    — compiled binary with executable permissions
pub fn extract_native_binary(
    artifact_name: &str,
    artifact_version: &str,
    artifact_bytes: &[u8],
) -> Result<Vec<u8>, RuntimeError> {
    extract_native_binary_capped(
        artifact_name,
        artifact_version,
        artifact_bytes,
        zip_guard::max_artifact_decompressed_bytes(),
    )
}

fn extract_native_binary_capped(
    artifact_name: &str,
    artifact_version: &str,
    artifact_bytes: &[u8],
    max_bytes: u64,
) -> Result<Vec<u8>, RuntimeError> {
    let cursor = std::io::Cursor::new(artifact_bytes);
    let mut archive = ZipArchive::new(cursor).map_err(|err| RuntimeError::ArtifactArchive {
        name: artifact_name.to_string(),
        version: artifact_version.to_string(),
        message: err.to_string(),
    })?;

    let bin_path = native_binary_entry(artifact_name);
    zip_guard::read_zip_entry_capped(&mut archive, &bin_path, max_bytes).map_err(|err| {
        RuntimeError::ArtifactArchive {
            name: artifact_name.to_string(),
            version: artifact_version.to_string(),
            message: format!(
                "failed to read native binary at '{bin_path}' from archive (ensure the binary \
                 is at {bin_path} inside the .mur.zip): {err}"
            ),
        }
    })
}

/// Extract `skill.md` from a skill artifact's `.mur.zip`.
///
/// Canonical skill artifact layout:
///   murmur.yaml      — artifact manifest at zip root
///   skill.md         — guidance content at zip root (primary payload)
pub fn extract_skill_md(
    artifact_name: &str,
    artifact_version: &str,
    artifact_bytes: &[u8],
) -> Result<Vec<u8>, RuntimeError> {
    extract_skill_md_capped(
        artifact_name,
        artifact_version,
        artifact_bytes,
        zip_guard::max_artifact_decompressed_bytes(),
    )
}

fn extract_skill_md_capped(
    artifact_name: &str,
    artifact_version: &str,
    artifact_bytes: &[u8],
    max_bytes: u64,
) -> Result<Vec<u8>, RuntimeError> {
    let cursor = std::io::Cursor::new(artifact_bytes);
    let mut archive = ZipArchive::new(cursor).map_err(|err| RuntimeError::ArtifactArchive {
        name: artifact_name.to_string(),
        version: artifact_version.to_string(),
        message: err.to_string(),
    })?;

    zip_guard::read_zip_entry_capped(&mut archive, SKILL_MD_ENTRY, max_bytes).map_err(|err| {
        RuntimeError::ArtifactArchive {
            name: artifact_name.to_string(),
            version: artifact_version.to_string(),
            message: format!(
                "{SKILL_MD_ENTRY} not found or unreadable at archive root (ensure \
                 {SKILL_MD_ENTRY} is at the root of the .mur.zip): {err}"
            ),
        }
    })
}

pub fn extract_manifest_yaml(
    artifact_name: &str,
    artifact_version: &str,
    artifact_bytes: &[u8],
) -> Result<String, RuntimeError> {
    extract_manifest_yaml_capped(
        artifact_name,
        artifact_version,
        artifact_bytes,
        zip_guard::max_artifact_decompressed_bytes(),
    )
}

fn extract_manifest_yaml_capped(
    artifact_name: &str,
    artifact_version: &str,
    artifact_bytes: &[u8],
    max_bytes: u64,
) -> Result<String, RuntimeError> {
    let cursor = std::io::Cursor::new(artifact_bytes);
    let mut archive = ZipArchive::new(cursor).map_err(|err| RuntimeError::ArtifactArchive {
        name: artifact_name.to_string(),
        version: artifact_version.to_string(),
        message: err.to_string(),
    })?;

    zip_guard::read_zip_entry_to_string_capped(&mut archive, PACKED_MANIFEST_ENTRY, max_bytes)
        .map_err(|err| RuntimeError::ArtifactArchive {
            name: artifact_name.to_string(),
            version: artifact_version.to_string(),
            message: format!(
                "missing or unreadable {PACKED_MANIFEST_ENTRY} in artifact archive: {err}"
            ),
        })
}

/// The `capabilities.env.allow` a packed manifest declares, read on its own.
///
/// Narrow on purpose. `murmur_artifact::RuntimeManifest::from_yaml_str` validates the whole
/// manifest, and that resolves every `${VAR}` in `inference.api_key` against the reading
/// process's environment — so a parent asking what its child declares would have to already hold
/// the child's variables before it could read which ones they are. Reading the one key answers
/// that without the circle.
///
/// A manifest with no `capabilities:` block, or none under `env:`, declares nothing and yields an
/// empty list. Malformed YAML, or an `env.allow` that is not a list of strings, is an error
/// naming the artifact and version.
pub fn extract_declared_env_allow(
    artifact_name: &str,
    artifact_version: &str,
    manifest_yaml: &str,
) -> Result<Vec<String>, RuntimeError> {
    let declared: DeclaredAllows =
        serde_yaml::from_str(manifest_yaml).map_err(|err| RuntimeError::ArtifactArchive {
            name: artifact_name.to_string(),
            version: artifact_version.to_string(),
            message: format!(
                "cannot read capabilities.env.allow from {PACKED_MANIFEST_ENTRY}: {err}"
            ),
        })?;
    Ok(declared
        .capabilities
        .and_then(|capabilities| capabilities.env)
        .map(|env| env.allow)
        .unwrap_or_default())
}

/// The `capabilities.spawn.allow` a packed manifest declares, read on its own.
///
/// The sibling of [`extract_declared_env_allow`], and narrow for the same reason: walking a
/// formation's delegation graph means reading which capsules a child may in turn spawn, and a
/// whole-manifest parse would demand that child's own `${VAR}` references be resolvable first.
///
/// A manifest with no `capabilities:` block, or none under `spawn:`, declares nothing and yields
/// an empty list. Malformed YAML, or a `spawn.allow` that is not a list of strings, is an error
/// naming the artifact and version.
pub fn extract_declared_spawn_allow(
    artifact_name: &str,
    artifact_version: &str,
    manifest_yaml: &str,
) -> Result<Vec<String>, RuntimeError> {
    let declared: DeclaredAllows =
        serde_yaml::from_str(manifest_yaml).map_err(|err| RuntimeError::ArtifactArchive {
            name: artifact_name.to_string(),
            version: artifact_version.to_string(),
            message: format!(
                "cannot read capabilities.spawn.allow from {PACKED_MANIFEST_ENTRY}: {err}"
            ),
        })?;
    Ok(declared
        .capabilities
        .and_then(|capabilities| capabilities.spawn)
        .map(|spawn| spawn.allow)
        .unwrap_or_default())
}

/// The only keys [`extract_declared_env_allow`] and [`extract_declared_spawn_allow`] read. Every
/// other manifest key deserializes into nothing here, so a child declaring anything at all is
/// still readable.
#[derive(serde::Deserialize)]
struct DeclaredAllows {
    #[serde(default)]
    capabilities: Option<DeclaredCapabilities>,
}

#[derive(serde::Deserialize)]
struct DeclaredCapabilities {
    #[serde(default)]
    env: Option<DeclaredAllowList>,
    #[serde(default)]
    spawn: Option<DeclaredAllowList>,
}

#[derive(serde::Deserialize)]
struct DeclaredAllowList {
    #[serde(default)]
    allow: Vec<String>,
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use zip::write::SimpleFileOptions;

    use super::*;

    fn archive_with_files(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut cursor = std::io::Cursor::new(Vec::<u8>::new());
        {
            let mut zip = zip::ZipWriter::new(&mut cursor);
            let options = SimpleFileOptions::default();
            for (name, bytes) in files {
                zip.start_file(*name, options).unwrap();
                zip.write_all(bytes).unwrap();
            }
            zip.finish().unwrap();
        }
        cursor.into_inner()
    }

    #[test]
    fn prefers_capsule_wasm_when_present() {
        let archive = archive_with_files(&[
            ("tool.wasm", b"tool"),
            ("capsule.wasm", b"capsule"),
            (PACKED_MANIFEST_ENTRY, b"name: demo"),
        ]);

        let wasm = extract_root_wasm("demo", "0.0.1", &archive).unwrap();
        assert_eq!(wasm, b"capsule");
    }

    #[test]
    fn extracts_manifest_yaml_from_archive() {
        let archive = archive_with_files(&[
            (PACKED_MANIFEST_ENTRY, b"name: demo\nversion: 0.1.0\n"),
            ("tool.wasm", b"tool"),
        ]);

        let manifest = extract_manifest_yaml("demo", "0.1.0", &archive).unwrap();
        assert!(manifest.contains("name: demo"));
        assert!(manifest.contains("version: 0.1.0"));
    }

    #[test]
    fn reads_the_declared_env_allow_out_of_a_manifest() {
        let manifest = "name: worker\nversion: 0.1.0\ncapabilities:\n  network:\n    \
                        allow: [127.0.0.1]\n  env:\n    allow: [OPENAI_API_KEY, HTTPS_PROXY]\n";

        assert_eq!(
            extract_declared_env_allow("worker", "0.1.0", manifest).unwrap(),
            vec!["OPENAI_API_KEY".to_string(), "HTTPS_PROXY".to_string()]
        );
    }

    /// The read answers what the child declares without validating the rest of the manifest: a
    /// `${VAR}` the reading process does not hold would fail a full parse, and the whole point is
    /// to learn which variables the child needs before holding any of them.
    #[test]
    fn an_unresolvable_inference_key_does_not_stop_the_read() {
        let manifest = "name: worker\nversion: 0.1.0\ninference:\n  transport: http\n  \
                        api_key: ${MURMUR_A_VARIABLE_NOBODY_EXPORTED}\ncapabilities:\n  env:\n    \
                        allow: [MURMUR_A_VARIABLE_NOBODY_EXPORTED]\n";

        assert_eq!(
            extract_declared_env_allow("worker", "0.1.0", manifest).unwrap(),
            vec!["MURMUR_A_VARIABLE_NOBODY_EXPORTED".to_string()]
        );
    }

    #[test]
    fn a_manifest_declaring_no_env_allow_declares_nothing() {
        for manifest in [
            "name: worker\nversion: 0.1.0\n",
            "name: worker\nversion: 0.1.0\ncapabilities:\n  shell:\n    allow: [bash]\n",
            "name: worker\nversion: 0.1.0\ncapabilities:\n  env:\n    allow: []\n",
        ] {
            assert_eq!(
                extract_declared_env_allow("worker", "0.1.0", manifest).unwrap(),
                Vec::<String>::new(),
                "{manifest}"
            );
        }
    }

    #[test]
    fn an_unreadable_env_allow_is_an_error_naming_the_artifact() {
        for manifest in [
            "name: worker\nversion: 0.1.0\ncapabilities:\n  env:\n    allow: OPENAI_API_KEY\n",
            "name: worker\nversion: 0.1.0\ncapabilities:\n  env:\n    allow: [[OPENAI_API_KEY]]\n",
            "name: worker\nversion: 0.1.0\ncapabilities:\n\tenv: broken\n",
        ] {
            let err = extract_declared_env_allow("worker", "0.1.0", manifest).unwrap_err();
            match err {
                RuntimeError::ArtifactArchive {
                    name,
                    version,
                    message,
                } => {
                    assert_eq!(name, "worker");
                    assert_eq!(version, "0.1.0");
                    assert!(message.contains("capabilities.env.allow"), "{message}");
                }
                other => panic!("expected ArtifactArchive error, got {other:?}"),
            }
        }
    }

    #[test]
    fn rejects_path_traversal_entry_as_root_wasm_candidate() {
        let archive = archive_with_files(&[
            ("../../evil.wasm", b"evil"),
            (PACKED_MANIFEST_ENTRY, b"name: demo"),
        ]);

        let err = extract_root_wasm("demo", "0.0.1", &archive).unwrap_err();
        match err {
            RuntimeError::ArtifactArchive { message, .. } => {
                assert!(message.contains("missing root .wasm file"));
            }
            other => panic!("expected ArtifactArchive error, got {other:?}"),
        }
    }

    #[test]
    fn rejects_leading_slash_entry_as_root_wasm_candidate() {
        let archive = archive_with_files(&[
            ("/capsule.wasm", b"evil"),
            (PACKED_MANIFEST_ENTRY, b"name: demo"),
        ]);

        let err = extract_root_wasm("demo", "0.0.1", &archive).unwrap_err();
        match err {
            RuntimeError::ArtifactArchive { message, .. } => {
                assert!(message.contains("missing root .wasm file"));
            }
            other => panic!("expected ArtifactArchive error, got {other:?}"),
        }
    }

    #[test]
    fn extract_manifest_yaml_rejects_entry_past_ceiling() {
        let big = vec![b'a'; 1024];
        let archive = archive_with_files(&[(PACKED_MANIFEST_ENTRY, &big)]);

        let err = extract_manifest_yaml_capped("demo", "0.0.1", &archive, 16).unwrap_err();
        match err {
            RuntimeError::ArtifactArchive { message, .. } => {
                assert!(message.contains("decompression ceiling"));
            }
            other => panic!("expected ArtifactArchive error, got {other:?}"),
        }
    }

    #[test]
    fn extract_root_wasm_rejects_entry_past_ceiling() {
        let big = vec![b'a'; 1024];
        let archive = archive_with_files(&[("capsule.wasm", &big)]);

        let err = extract_root_wasm_capped("demo", "0.0.1", &archive, 16).unwrap_err();
        match err {
            RuntimeError::ArtifactArchive { message, .. } => {
                assert!(message.contains("decompression ceiling"));
            }
            other => panic!("expected ArtifactArchive error, got {other:?}"),
        }
    }

    #[test]
    fn extract_native_binary_rejects_entry_past_ceiling() {
        let big = vec![b'a'; 1024];
        let archive = archive_with_files(&[("bin/demo", &big)]);

        let err = extract_native_binary_capped("demo", "0.0.1", &archive, 16).unwrap_err();
        match err {
            RuntimeError::ArtifactArchive { message, .. } => {
                assert!(message.contains("decompression ceiling"));
            }
            other => panic!("expected ArtifactArchive error, got {other:?}"),
        }
    }

    #[test]
    fn extract_skill_md_rejects_entry_past_ceiling() {
        let big = vec![b'a'; 1024];
        let archive = archive_with_files(&[("skill.md", &big)]);

        let err = extract_skill_md_capped("demo", "0.0.1", &archive, 16).unwrap_err();
        match err {
            RuntimeError::ArtifactArchive { message, .. } => {
                assert!(message.contains("decompression ceiling"));
            }
            other => panic!("expected ArtifactArchive error, got {other:?}"),
        }
    }

    #[test]
    fn default_ceiling_does_not_reject_small_fixtures() {
        let archive = archive_with_files(&[
            (PACKED_MANIFEST_ENTRY, b"name: demo\nversion: 0.1.0\n"),
            ("capsule.wasm", b"small wasm bytes"),
        ]);

        extract_manifest_yaml("demo", "0.1.0", &archive).unwrap();
        extract_root_wasm("demo", "0.1.0", &archive).unwrap();
    }
}
