//! The versioned WIT interface names a component binary declares.
//!
//! The host resolves exactly one version of each interface and keeps no fallback (see
//! `crates/capsule-runtime/wit/VERSIONING.md`), so both directions matter: an artifact stops
//! instantiating when a package it *exports* is renamed by a version bump, and equally when a
//! package it *imports* is. The two lists here are the floor an installed artifact was built
//! against, read out of the component bytes rather than declared anywhere by hand.
//!
//! Extraction is best-effort by design. A native tool, a skill, a core module, or a payload
//! that does not parse simply has nothing to record; that is not an error, and
//! [`wit_contracts_from_artifact_bytes`] is total for exactly that reason.

use std::io::{Cursor, Read, Seek};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use wasmparser::{Encoding, Parser, Payload};
use zip::ZipArchive;

use crate::{payload_shape::select_root_wasm_in_archive, zip_guard};

/// The versioned WIT interface names a component declares, in both directions.
///
/// Each list is sorted ascending and deduplicated, and holds fully-qualified instance names
/// byte for byte as the component binary carries them (e.g. `murmur:tool/run@0.1.0`). Bare
/// function and type externs — names without a `/` — are not interfaces and are not recorded.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WitContracts {
    /// Interfaces the host resolves when it instantiates the artifact.
    #[serde(default)]
    pub exports: Vec<String>,
    /// Interfaces the host must provide for the artifact to link.
    #[serde(default)]
    pub imports: Vec<String>,
}

impl WitContracts {
    /// Are both directions empty?
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.exports.is_empty() && self.imports.is_empty()
    }

    /// Every recorded interface name across both directions, exports first.
    pub fn all(&self) -> impl Iterator<Item = &str> {
        self.exports
            .iter()
            .chain(self.imports.iter())
            .map(String::as_str)
    }

    /// The recorded names starting with `prefix`, sorted ascending and deduplicated across
    /// the two directions.
    #[must_use]
    pub fn matching_prefix(&self, prefix: &str) -> Vec<String> {
        let mut matches: Vec<String> = self
            .all()
            .filter(|name| name.starts_with(prefix))
            .map(str::to_string)
            .collect();
        sort_dedup(&mut matches);
        matches
    }
}

/// Why component bytes could not be read for their WIT contracts.
#[derive(Debug, Error)]
pub enum WitContractError {
    /// The bytes are not a well-formed wasm binary.
    #[error("could not parse wasm payload: {0}")]
    Parse(#[from] wasmparser::BinaryReaderError),
}

/// Extract the WIT contracts declared by a wasm binary.
///
/// Returns `Ok(None)` when the bytes are a core module rather than a component: a core module
/// carries no component-model interface names, so there is nothing to record.
///
/// Only the outermost component's own import and export sections are read. `parse_all` descends
/// into nested modules and components, whose interfaces belong to the composition, not to what
/// the host resolves on this artifact.
pub fn extract_wit_contracts(
    component_bytes: &[u8],
) -> Result<Option<WitContracts>, WitContractError> {
    let mut contracts = WitContracts::default();
    let mut is_component = false;
    // 0 while inside the outermost unit; deeper once a nested module or component opens.
    let mut depth: usize = 0;

    for payload in Parser::new(0).parse_all(component_bytes) {
        match payload? {
            Payload::Version { encoding, .. } => {
                if depth == 0 {
                    is_component = encoding == Encoding::Component;
                    if !is_component {
                        return Ok(None);
                    }
                }
            }
            Payload::ModuleSection { .. } | Payload::ComponentSection { .. } => depth += 1,
            Payload::End(_) => depth = depth.saturating_sub(1),
            Payload::ComponentExportSection(reader) if depth == 0 => {
                for export in reader {
                    push_interface(&mut contracts.exports, export?.name.name);
                }
            }
            Payload::ComponentImportSection(reader) if depth == 0 => {
                for import in reader {
                    push_interface(&mut contracts.imports, import?.name.name);
                }
            }
            _ => {}
        }
    }

    if !is_component {
        return Ok(None);
    }

    sort_dedup(&mut contracts.exports);
    sort_dedup(&mut contracts.imports);
    Ok(Some(contracts))
}

/// Extract the WIT contracts of the component packed inside a `.mur.zip`.
///
/// Total: `None` for anything that cannot be read as a component — bytes that are not a zip, an
/// archive with no root wasm entry or an ambiguous pair of them, a payload over the artifact
/// decompression cap, a core module, or a binary that does not parse.
#[must_use]
pub fn wit_contracts_from_artifact_bytes(artifact_bytes: &[u8]) -> Option<WitContracts> {
    let wasm = read_root_wasm(artifact_bytes)?;
    extract_wit_contracts(&wasm).ok().flatten()
}

fn read_root_wasm(artifact_bytes: &[u8]) -> Option<Vec<u8>> {
    let mut archive = ZipArchive::new(Cursor::new(artifact_bytes)).ok()?;
    read_root_wasm_from_archive(&mut archive)
}

fn read_root_wasm_from_archive<R: Read + Seek>(archive: &mut ZipArchive<R>) -> Option<Vec<u8>> {
    let selected = select_root_wasm_in_archive(archive).ok()?;
    zip_guard::read_zip_entry_capped(
        archive,
        &selected,
        zip_guard::max_artifact_decompressed_bytes(),
    )
    .ok()
}

/// Record `name` when it names an interface. Component extern names that carry no `/` are
/// bare functions or types, not interfaces, and pin no interface version.
fn push_interface(into: &mut Vec<String>, name: &str) {
    if name.contains('/') {
        into.push(name.to_string());
    }
}

fn sort_dedup(names: &mut Vec<String>) {
    names.sort();
    names.dedup();
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use zip::{write::SimpleFileOptions, ZipWriter};

    use super::*;

    fn zip_with(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut zip = ZipWriter::new(Cursor::new(&mut buf));
            for (name, bytes) in files {
                zip.start_file(*name, SimpleFileOptions::default()).unwrap();
                zip.write_all(bytes).unwrap();
            }
            zip.finish().unwrap();
        }
        buf
    }

    fn component(wat: &str) -> Vec<u8> {
        wat::parse_str(wat).unwrap()
    }

    const EXPORTING_COMPONENT: &str = r#"
        (component
          (core module $m (func (export "run")))
          (core instance $i (instantiate $m))
          (func $run (result u32) (canon lift (core func $i "run")))
          (component $inner
            (core module $n (func (export "nested")))
          )
          (instance $iface (export "handle" (func $run)))
          (export "murmur:hook/lifecycle@0.5.0" (instance $iface))
        )
    "#;

    #[test]
    fn records_exported_interface_names() {
        let contracts = extract_wit_contracts(&component(EXPORTING_COMPONENT))
            .unwrap()
            .unwrap();
        assert_eq!(contracts.exports, vec!["murmur:hook/lifecycle@0.5.0"]);
        assert!(contracts.imports.is_empty());
    }

    #[test]
    fn records_imported_interface_names_sorted_and_deduplicated() {
        let wat = r#"
            (component
              (import "murmur:tool-registry/invoke@0.1.0" (instance))
              (import "wasi:cli/environment@0.2.0" (instance))
              (import "murmur:runtime/inference@0.3.0" (instance))
            )
        "#;
        let contracts = extract_wit_contracts(&component(wat)).unwrap().unwrap();
        assert_eq!(
            contracts.imports,
            vec![
                "murmur:runtime/inference@0.3.0",
                "murmur:tool-registry/invoke@0.1.0",
                "wasi:cli/environment@0.2.0",
            ]
        );
    }

    #[test]
    fn ignores_externs_that_do_not_name_an_interface() {
        let wat = r#"
            (component
              (import "plain-func" (func))
              (core module $m (func (export "f")))
              (core instance $i (instantiate $m))
              (func $f (canon lift (core func $i "f")))
              (export "bare-export" (func $f))
            )
        "#;
        let contracts = extract_wit_contracts(&component(wat)).unwrap().unwrap();
        assert!(contracts.is_empty());
    }

    #[test]
    fn a_core_module_yields_none() {
        let module = component(r#"(module (func (export "f")))"#);
        assert!(extract_wit_contracts(&module).unwrap().is_none());
    }

    #[test]
    fn unparseable_bytes_are_an_error() {
        assert!(extract_wit_contracts(b"\0asm").is_err());
    }

    #[test]
    fn reads_the_component_out_of_an_artifact_zip() {
        let archive = zip_with(&[
            ("murmur.yaml", b"name: demo\n"),
            ("tool.wasm", &component(EXPORTING_COMPONENT)),
        ]);
        let contracts = wit_contracts_from_artifact_bytes(&archive).unwrap();
        assert_eq!(contracts.exports, vec!["murmur:hook/lifecycle@0.5.0"]);
    }

    #[test]
    fn capsule_wasm_wins_over_other_root_wasm_entries() {
        let archive = zip_with(&[
            ("capsule.wasm", &component(EXPORTING_COMPONENT)),
            ("other.wasm", b"\0asm"),
        ]);
        let contracts = wit_contracts_from_artifact_bytes(&archive).unwrap();
        assert_eq!(contracts.exports, vec!["murmur:hook/lifecycle@0.5.0"]);
    }

    #[test]
    fn unreadable_payloads_yield_none() {
        assert!(wit_contracts_from_artifact_bytes(b"not-a-zip-at-all").is_none());
        assert!(wit_contracts_from_artifact_bytes(&zip_with(&[("tool.wasm", b"\0asm")])).is_none());
        assert!(
            wit_contracts_from_artifact_bytes(&zip_with(&[("skill.md", b"# guidance")])).is_none()
        );
        assert!(
            wit_contracts_from_artifact_bytes(&zip_with(&[("bin/demo", b"\x7fELF")])).is_none()
        );
        assert!(wit_contracts_from_artifact_bytes(&zip_with(&[
            ("a.wasm", b"\0asm"),
            ("b.wasm", b"\0asm"),
        ]))
        .is_none());
        let module = component(r#"(module (func (export "f")))"#);
        assert!(wit_contracts_from_artifact_bytes(&zip_with(&[("tool.wasm", &module)])).is_none());
    }

    #[test]
    fn matching_prefix_spans_both_directions() {
        let contracts = WitContracts {
            exports: vec!["murmur:tool/run@0.1.0".to_string()],
            imports: vec![
                "murmur:tool-registry/invoke@0.1.0".to_string(),
                "wasi:cli/environment@0.2.0".to_string(),
            ],
        };
        assert_eq!(
            contracts.matching_prefix("murmur:tool"),
            vec!["murmur:tool-registry/invoke@0.1.0", "murmur:tool/run@0.1.0",]
        );
        assert!(contracts.matching_prefix("murmur:hook").is_empty());
    }
}
