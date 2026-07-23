//! Non-fatal authoring lints for `mur build`.
//!
//! These are the mistakes `mur build` can see in the file set it is about to pack but which
//! still produce a working artifact: a redundant declaration, a payload the runtime will
//! silently shadow, a build input that has no business shipping. Each one carries a `W-BLD-NNN`
//! code and a link back to its section on the build-lints reference page, mirroring the
//! `W-SEC-NNN` convention in [`crate::security_warnings`].
//!
//! Anything that makes the artifact *unlaunchable* is a [`BuildError`] instead — see
//! [`crate::build`].

use std::path::Path;

use crate::build::{plan_packed_entries, BuildError, PackedPlan, PACKED_MANIFEST_ENTRY};
use crate::manifest::{load_manifest, Manifest};
use crate::manifest_path::resolve_manifest_path;
use crate::payload_shape::{is_root_wasm_candidate, CAPSULE_WASM_ENTRY};
use crate::registry::RuntimeType;

/// A `requires_files:` entry claims an archive slot the packer already fills for itself, so the
/// declaration ships nothing extra.
pub const W_BLD_001: &str = "W-BLD-001";

/// The artifact carries a root `capsule.wasm` *and* another root `*.wasm`. The runtime always
/// selects `capsule.wasm`, so the other payload is shipped but never executed.
pub const W_BLD_002: &str = "W-BLD-002";

/// A wasm or native artifact packages an obvious build input (`Cargo.toml`, `Cargo.lock`, a
/// `*.rs` source file, or something under `target/`) — almost always a stray `requires_files:`
/// entry rather than a payload.
pub const W_BLD_003: &str = "W-BLD-003";

/// Root archive entries a `.mur.zip` reserves for a fixed meaning: `murmur.yaml` is seeded by
/// the packer itself, and `capsule.wasm` is the payload the runtime prefers over every other
/// root `*.wasm`. A declaration that lands on either slot is shadowed rather than honoured,
/// which is what [`W_BLD_001`] and [`W_BLD_002`] report.
pub const RESERVED_ROOT_ENTRIES: [&str; 2] = [PACKED_MANIFEST_ENTRY, CAPSULE_WASM_ENTRY];

const BUILD_LINTS_DOC_URL: &str =
    "https://docs.murmur.nexus/murmur-nexus/murmur/reference/build-lints/";

/// Builds the doc link for a `W-BLD-*` code, e.g. `.../build-lints/#w-bld-001`.
pub fn build_warning_link(code: &str) -> String {
    format!("{BUILD_LINTS_DOC_URL}#{}", code.to_lowercase())
}

/// One thing worth telling the author about an otherwise successful build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildWarning {
    /// The `W-BLD-NNN` code, for the printed prefix and the doc anchor.
    pub code: &'static str,
    /// What was found, naming the offending file(s).
    pub message: String,
    /// What to do about it.
    pub hint: String,
}

/// Lint the entry set `build_artifact(source_dir, output_path)` would pack.
///
/// Runs the same planning pass the packer runs — same curation, same archive names — so a
/// warning can never describe a file set the build would not produce. Planning errors are
/// propagated verbatim: a caller that lints before building sees the identical failure it
/// would have seen from [`crate::build_artifact`], and no output file is created either way.
pub fn lint_build_warnings(
    source_dir: &Path,
    output_path: &Path,
) -> Result<Vec<BuildWarning>, BuildError> {
    let manifest = load_manifest(&resolve_manifest_path(source_dir))?;
    let plan = plan_packed_entries(source_dir, &manifest, output_path)?;

    Ok(lint_plan(&manifest, &plan))
}

fn lint_plan(manifest: &Manifest, plan: &PackedPlan) -> Vec<BuildWarning> {
    let mut warnings = Vec::new();
    warnings.extend(reserved_name_shadowing(plan));
    warnings.extend(capsule_wasm_ambiguity(plan));
    warnings.extend(packaged_build_inputs(manifest, plan));
    warnings
}

/// W-BLD-001 — a declaration that lands on a reserved root slot the packer already filled.
fn reserved_name_shadowing(plan: &PackedPlan) -> Vec<BuildWarning> {
    plan.shadowed
        .iter()
        .filter(|dropped| RESERVED_ROOT_ENTRIES.contains(&dropped.archive_name.as_str()))
        .map(|dropped| BuildWarning {
            code: W_BLD_001,
            message: format!(
                "requires_files entry '{}' names the reserved archive entry '{}', which mur build already packs",
                dropped.declared, dropped.archive_name
            ),
            hint: format!(
                "remove '{}' from requires_files: — the artifact is packed identically without it",
                dropped.declared
            ),
        })
        .collect()
}

/// W-BLD-002 — `capsule.wasm` beside another root `*.wasm`. Not an error: the payload-shape
/// rule resolves this without ambiguity, it just resolves it in a way the author may not expect.
fn capsule_wasm_ambiguity(plan: &PackedPlan) -> Vec<BuildWarning> {
    let root_wasm: Vec<&str> = plan
        .entries
        .iter()
        .map(|entry| entry.archive_name.as_str())
        .filter(|name| is_root_wasm_candidate(name))
        .collect();

    if !root_wasm.contains(&CAPSULE_WASM_ENTRY) {
        return Vec::new();
    }

    let shadowed: Vec<&str> = root_wasm
        .into_iter()
        .filter(|name| *name != CAPSULE_WASM_ENTRY)
        .collect();
    if shadowed.is_empty() {
        return Vec::new();
    }

    vec![BuildWarning {
        code: W_BLD_002,
        message: format!(
            "root '{CAPSULE_WASM_ENTRY}' is always selected as the payload, so {} ships but never runs",
            shadowed.join(", ")
        ),
        hint: format!(
            "keep exactly one root *.wasm: drop {CAPSULE_WASM_ENTRY}, or rename the payload you meant to run to {CAPSULE_WASM_ENTRY}"
        ),
    }]
}

/// W-BLD-003 — build inputs riding along in a compiled artifact.
fn packaged_build_inputs(manifest: &Manifest, plan: &PackedPlan) -> Vec<BuildWarning> {
    if !matches!(
        manifest.registry_runtime(),
        RuntimeType::Wasm | RuntimeType::Native
    ) {
        return Vec::new();
    }

    let offenders: Vec<&str> = plan
        .entries
        .iter()
        .map(|entry| entry.archive_name.as_str())
        .filter(|name| is_build_input(name))
        .collect();
    if offenders.is_empty() {
        return Vec::new();
    }

    vec![BuildWarning {
        code: W_BLD_003,
        message: format!(
            "compiled artifact packages build inputs: {}",
            offenders.join(", ")
        ),
        hint: "remove them from requires_files: — a compiled artifact ships its payload, not the sources it was built from".to_string(),
    }]
}

/// Is this archive entry an input to a build rather than a product of one?
fn is_build_input(archive_name: &str) -> bool {
    if archive_name.split('/').any(|segment| segment == "target") {
        return true;
    }

    let file_name = archive_name.rsplit('/').next().unwrap_or(archive_name);
    file_name == "Cargo.toml" || file_name == "Cargo.lock" || file_name.ends_with(".rs")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;
    use crate::manifest_path::MANIFEST_FILENAME;

    /// Writes a manifest plus the given files, then lints the build that would follow.
    fn lint(manifest_yaml: &str, files: &[(&str, &[u8])]) -> Vec<BuildWarning> {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join(MANIFEST_FILENAME), manifest_yaml).unwrap();
        for (rel, bytes) in files {
            let path = dir.path().join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }

        lint_build_warnings(dir.path(), &dir.path().join("out.mur.zip")).unwrap()
    }

    #[test]
    fn link_lowercases_the_code_into_the_anchor() {
        assert_eq!(
            build_warning_link(W_BLD_001),
            "https://docs.murmur.nexus/murmur-nexus/murmur/reference/build-lints/#w-bld-001"
        );
    }

    #[test]
    fn a_clean_wasm_artifact_warns_about_nothing() {
        let warnings = lint(
            "name: clean-tool\nversion: 0.1.0\nruntime: wasm\nrequires_files:\n  - tool.wasm\n",
            &[("tool.wasm", b"\0asm")],
        );
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    }

    #[test]
    fn declaring_the_manifest_warns_that_the_declaration_is_redundant() {
        let warnings = lint(
            "name: redundant\nversion: 0.1.0\nruntime: wasm\nrequires_files:\n  - murmur.yaml\n  - tool.wasm\n",
            &[("tool.wasm", b"\0asm")],
        );

        assert_eq!(warnings.len(), 1, "got: {warnings:?}");
        assert_eq!(warnings[0].code, W_BLD_001);
        assert!(
            warnings[0].message.contains(MANIFEST_FILENAME),
            "warning should name the redundant declaration; got: {}",
            warnings[0].message
        );
    }

    #[test]
    fn capsule_wasm_beside_another_root_wasm_warns_about_the_shadowed_payload() {
        let warnings = lint(
            "name: two-wasm\nversion: 0.1.0\nruntime: wasm\nrequires_files:\n  - capsule.wasm\n  - tool.wasm\n",
            &[("capsule.wasm", b"\0asm"), ("tool.wasm", b"\0asm")],
        );

        assert_eq!(warnings.len(), 1, "got: {warnings:?}");
        assert_eq!(warnings[0].code, W_BLD_002);
        assert!(
            warnings[0].message.contains("tool.wasm"),
            "warning should name the shadowed payload; got: {}",
            warnings[0].message
        );
    }

    #[test]
    fn a_lone_capsule_wasm_is_not_ambiguous() {
        let warnings = lint(
            "name: single\nversion: 0.1.0\nruntime: wasm\nrequires_files:\n  - capsule.wasm\n  - assets/nested.wasm\n",
            &[("capsule.wasm", b"\0asm"), ("assets/nested.wasm", b"\0asm")],
        );
        assert!(
            warnings.is_empty(),
            "a nested wasm is not a root payload: {warnings:?}"
        );
    }

    #[test]
    fn packaged_build_inputs_are_named_in_one_warning() {
        let warnings = lint(
            "name: sources\nversion: 0.1.0\nruntime: wasm\nrequires_files:\n  - tool.wasm\n  - Cargo.toml\n  - Cargo.lock\n  - src/lib.rs\n  - target/debug/stale.txt\n",
            &[
                ("tool.wasm", b"\0asm"),
                ("Cargo.toml", b"[package]"),
                ("Cargo.lock", b"# lock"),
                ("src/lib.rs", b"fn main() {}"),
                ("target/debug/stale.txt", b"stale"),
            ],
        );

        assert_eq!(warnings.len(), 1, "got: {warnings:?}");
        assert_eq!(warnings[0].code, W_BLD_003);
        for expected in [
            "Cargo.toml",
            "Cargo.lock",
            "src/lib.rs",
            "target/debug/stale.txt",
        ] {
            assert!(
                warnings[0].message.contains(expected),
                "warning should name {expected}; got: {}",
                warnings[0].message
            );
        }
        assert!(
            !warnings[0].message.contains("tool.wasm"),
            "the payload is not a build input; got: {}",
            warnings[0].message
        );
    }

    #[test]
    fn a_static_artifact_may_ship_sources_without_a_warning() {
        let warnings = lint(
            "name: sample-skill\nversion: 0.1.0\nruntime: skill\nrequires_files:\n  - skill.md\n  - examples/demo.rs\n",
            &[("skill.md", b"# skill"), ("examples/demo.rs", b"fn main(){}")],
        );
        assert!(
            warnings.is_empty(),
            "a skill's sources are its content: {warnings:?}"
        );
    }

    #[test]
    fn a_native_artifact_shipping_cargo_toml_warns() {
        let warnings = lint(
            "name: native-tool\nversion: 0.1.0\nruntime: tool\nimplementation: native\nrequires_files:\n  - bin/native-tool\n  - Cargo.toml\n",
            &[("bin/native-tool", b"ELF"), ("Cargo.toml", b"[package]")],
        );

        assert_eq!(warnings.len(), 1, "got: {warnings:?}");
        assert_eq!(warnings[0].code, W_BLD_003);
    }

    #[test]
    fn a_file_merely_named_like_a_build_input_deeper_in_the_tree_still_warns() {
        assert!(is_build_input("Cargo.toml"));
        assert!(is_build_input("crates/inner/Cargo.toml"));
        assert!(is_build_input("src/lib.rs"));
        assert!(is_build_input("target/release/tool"));
        assert!(!is_build_input("tool.wasm"));
        assert!(!is_build_input("assets/targeting.txt"));
        assert!(!is_build_input("notes/cargo.toml"));
    }
}
