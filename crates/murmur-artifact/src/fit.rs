//! Whether an artifact version fits this host: its `murmur:*` interfaces against the served WIT
//! level, and a payload for this platform.
//!
//! [`version_fit`] is the one place that is decided. Install, source resolution, launch, a
//! capsule's pull, `mur search` and `mur doctor` each gather a [`FitSubject`] from what they hold
//! (an index entry, a store sidecar, a component's contracts) and call it.

use crate::{
    current_platform, wit_contracts_from_artifact_bytes, ArtifactMeta, IndexedVersion,
    UnservedInterface, WitContracts, WitLevel,
};

/// Whether one version can run on a host serving a given WIT level and platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionFit {
    Fits,
    /// Its interfaces whose version is not the one the level lists for their package, in
    /// recorded order. Never empty.
    Unserved(Vec<UnservedInterface>),
    /// Every payload is tagged for a platform other than this one.
    NoPayloadFor(String),
}

/// What fit is decided from. Gathers inputs; decides nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FitSubject {
    /// The `murmur:*` names the version's component exports and imports; empty for a payload
    /// with no component (a skill, a native tool).
    pub contracts: WitContracts,
    /// One entry per payload: its platform tag, `None` when untagged. Never empty.
    pub payload_platforms: Vec<Option<String>>,
}

impl FitSubject {
    /// A component's contracts in one untagged payload.
    #[must_use]
    pub fn component(contracts: WitContracts) -> Self {
        Self {
            contracts,
            payload_platforms: vec![None],
        }
    }

    /// An index version: its `interfaces`, which merge both directions, as imports, and one
    /// payload per asset with the asset's platform tag. Fit treats exports and imports alike,
    /// so which list the interfaces go in does not change it.
    #[must_use]
    pub fn indexed(version: &IndexedVersion) -> Self {
        Self {
            contracts: WitContracts {
                exports: Vec::new(),
                imports: version.interfaces.clone(),
            },
            payload_platforms: version
                .assets
                .iter()
                .map(|asset| asset.platform.clone())
                .collect(),
        }
    }

    /// A stored version, from its sidecar. Contracts are the sidecar's `wit_contracts`, else
    /// those read from `bytes` when given, else none: a payload with no readable component names
    /// no `murmur:*` interface. Payloads are one per `"{os}-{arch}"` in `meta.platforms`, or one
    /// untagged payload when it records none.
    #[must_use]
    pub fn stored(meta: &ArtifactMeta, bytes: Option<&[u8]>) -> Self {
        let contracts = meta
            .wit_contracts
            .clone()
            .or_else(|| bytes.and_then(wit_contracts_from_artifact_bytes))
            .unwrap_or_default();
        let payload_platforms = if meta.platforms.is_empty() {
            vec![None]
        } else {
            meta.platforms
                .iter()
                .map(|(os, arch)| Some(format!("{os}-{arch}")))
                .collect()
        };
        Self {
            contracts,
            payload_platforms,
        }
    }
}

/// The one fit decision. Interfaces first: every `murmur:*` name, exports and imports alike,
/// must be at exactly the version `level` lists for its package, else [`VersionFit::Unserved`].
/// A name with no `@version`, or of a package the level does not list, does not fit; names
/// outside `murmur:` are not read. Then one payload must be untagged or tagged `platform`, else
/// [`VersionFit::NoPayloadFor`]. A version failing both reports `Unserved`.
#[must_use]
pub fn version_fit(subject: &FitSubject, level: &WitLevel, platform: &str) -> VersionFit {
    let unserved = subject.contracts.unserved_against(&level.served());
    if !unserved.is_empty() {
        return VersionFit::Unserved(unserved);
    }
    let runs_here = subject
        .payload_platforms
        .iter()
        .any(|tag| tag.as_deref().is_none_or(|tag| tag == platform));
    if runs_here {
        VersionFit::Fits
    } else {
        VersionFit::NoPayloadFor(platform.to_string())
    }
}

/// The built-at label: the newest of `levels` that [`version_fit`] accepts for
/// [`FitSubject::component`]`(contracts)`, `None` when it accepts none. A label, not a load
/// decision.
#[must_use]
pub fn newest_fitting_level(contracts: &WitContracts, levels: &[WitLevel]) -> Option<u64> {
    let subject = FitSubject::component(contracts.clone());
    levels
        .iter()
        .filter(|level| version_fit(&subject, level, current_platform()) == VersionFit::Fits)
        .map(|level| level.level)
        .max()
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};

    use zip::{write::SimpleFileOptions, ZipWriter};

    use super::*;
    use crate::{
        level_has_phrase, parse_wit_levels, ArtifactName, ContractDirection, IndexedAsset,
        RuntimeType,
    };

    const LEVEL_1: &str = "\
1 murmur:artifact-manager@0.1.0
1 murmur:capsule@0.1.0
1 murmur:conversation@0.2.0
1 murmur:driver@0.3.0
1 murmur:hook@0.9.0
1 murmur:host@0.1.0
1 murmur:message@0.1.0
1 murmur:runtime@0.4.0
1 murmur:runtime-guest@0.1.0
1 murmur:shell@0.1.0
1 murmur:stream@0.1.0
1 murmur:task@0.1.0
1 murmur:task-io@0.1.0
1 murmur:tool@0.1.0
1 murmur:tool-registry@0.1.0
";

    /// Level 1, plus a level 2 identical but for `murmur:driver@0.4.0`.
    fn two_levels() -> Vec<WitLevel> {
        let level_2 = LEVEL_1
            .lines()
            .map(|line| {
                line.replacen("1 ", "2 ", 1)
                    .replace("murmur:driver@0.3.0", "murmur:driver@0.4.0")
            })
            .collect::<Vec<_>>()
            .join("\n");
        parse_wit_levels(&format!("{LEVEL_1}{level_2}\n")).unwrap()
    }

    fn contracts(exports: &[&str], imports: &[&str]) -> WitContracts {
        WitContracts {
            exports: exports.iter().map(|s| s.to_string()).collect(),
            imports: imports.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn tagged(contracts: WitContracts, tags: &[Option<&str>]) -> FitSubject {
        FitSubject {
            contracts,
            payload_platforms: tags.iter().map(|tag| tag.map(str::to_string)).collect(),
        }
    }

    /// The numbers of every level in `levels` that `contracts` fits as one untagged component.
    fn levels_accepting(contracts: &WitContracts, levels: &[WitLevel]) -> Vec<u64> {
        let subject = FitSubject::component(contracts.clone());
        levels
            .iter()
            .filter(|level| version_fit(&subject, level, "linux-x86_64") == VersionFit::Fits)
            .map(|level| level.level)
            .collect()
    }

    fn meta(platforms: &[(&str, &str)], wit_contracts: Option<WitContracts>) -> ArtifactMeta {
        ArtifactMeta {
            name: ArtifactName::parse("ana/tool-x").unwrap(),
            version: "0.1.0".to_string(),
            runtime: RuntimeType::Wasm,
            artifact_runtime: "tool".to_string(),
            platforms: platforms
                .iter()
                .map(|(os, arch)| (os.to_string(), arch.to_string()))
                .collect(),
            description: None,
            tags: Vec::new(),
            wit_contracts,
            owner_id: None,
            built_at_level: None,
        }
    }

    /// A `.mur.zip` whose `capsule.wasm` component imports each of `names` as an instance.
    fn artifact_importing(names: &[&str]) -> Vec<u8> {
        let imports: String = names
            .iter()
            .map(|name| format!("(import \"{name}\" (instance))"))
            .collect();
        let mut buf = Vec::new();
        {
            let mut zip = ZipWriter::new(Cursor::new(&mut buf));
            zip.start_file("capsule.wasm", SimpleFileOptions::default())
                .unwrap();
            zip.write_all(&wat::parse_str(format!("(component {imports})")).unwrap())
                .unwrap();
            zip.finish().unwrap();
        }
        buf
    }

    #[test]
    fn fit_an_interface_mismatch_is_unserved() {
        let levels = two_levels();
        let subject = FitSubject::component(contracts(&["murmur:driver/process@0.3.0"], &[]));
        let VersionFit::Unserved(unserved) = version_fit(&subject, &levels[1], "linux-x86_64")
        else {
            panic!("a 0.3.0 driver fits a level serving murmur:driver@0.4.0");
        };
        assert_eq!(unserved[0].package, "murmur:driver");
    }

    #[test]
    fn fit_a_mismatch_with_only_a_foreign_payload_is_unserved() {
        let levels = two_levels();
        let subject = tagged(
            contracts(&[], &["murmur:tool/run@9.9.9"]),
            &[Some("darwin-aarch64")],
        );
        assert!(matches!(
            version_fit(&subject, &levels[0], "linux-x86_64"),
            VersionFit::Unserved(_)
        ));
    }

    #[test]
    fn fit_matching_interfaces_with_only_a_foreign_tag_have_no_payload() {
        let levels = two_levels();
        let subject = tagged(
            contracts(&[], &["murmur:tool/run@0.1.0"]),
            &[Some("darwin-aarch64")],
        );
        assert_eq!(
            version_fit(&subject, &levels[0], "linux-x86_64"),
            VersionFit::NoPayloadFor("linux-x86_64".to_string())
        );
        assert_eq!(
            version_fit(&subject, &levels[0], "darwin-aarch64"),
            VersionFit::Fits
        );
    }

    #[test]
    fn fit_an_untagged_payload_fits_any_platform() {
        let levels = two_levels();
        let subject = tagged(WitContracts::default(), &[Some("darwin-aarch64"), None]);
        for platform in ["linux-x86_64", "linux-aarch64", "darwin-aarch64"] {
            assert_eq!(
                version_fit(&subject, &levels[0], platform),
                VersionFit::Fits
            );
        }
    }

    #[test]
    fn fit_empty_contracts_fit_every_level() {
        let levels = two_levels();
        let skill = WitContracts::default();
        assert_eq!(levels_accepting(&skill, &levels), vec![1, 2]);
        let wasi_only = contracts(&[], &["wasi:cli/stdout@0.2.0"]);
        assert_eq!(newest_fitting_level(&wasi_only, &levels), Some(2));
    }

    #[test]
    fn fit_indexed_reads_interfaces_and_one_payload_per_asset() {
        let version = IndexedVersion {
            version: "1.0.0".to_string(),
            release: "v1".to_string(),
            built_at_level: None,
            interfaces: vec!["murmur:tool/run@0.1.0".to_string()],
            assets: ["darwin-aarch64", "linux-x86_64"]
                .iter()
                .map(|platform| IndexedAsset {
                    file: format!("tool-x-1.0.0-{platform}.mur.zip"),
                    platform: Some((*platform).to_string()),
                    sha256: "a".repeat(64),
                })
                .collect(),
        };
        let subject = FitSubject::indexed(&version);
        assert_eq!(subject.contracts.imports, version.interfaces);
        assert_eq!(
            subject.payload_platforms,
            vec![
                Some("darwin-aarch64".to_string()),
                Some("linux-x86_64".to_string())
            ]
        );
    }

    #[test]
    fn fit_stored_prefers_sidecar_contracts_then_bytes() {
        let bytes = artifact_importing(&["murmur:tool/run@0.1.0"]);
        let sidecar = contracts(&["murmur:hook/lifecycle@0.9.0"], &[]);

        let from_sidecar = FitSubject::stored(&meta(&[], Some(sidecar.clone())), Some(&bytes));
        assert_eq!(from_sidecar.contracts, sidecar);

        let from_bytes = FitSubject::stored(&meta(&[], None), Some(&bytes));
        assert_eq!(
            from_bytes.contracts,
            contracts(&[], &["murmur:tool/run@0.1.0"])
        );

        let neither = FitSubject::stored(&meta(&[], None), None);
        assert_eq!(neither.contracts, WitContracts::default());
        let opaque = FitSubject::stored(&meta(&[], None), Some(b"not a zip"));
        assert_eq!(opaque.contracts, WitContracts::default());
    }

    #[test]
    fn fit_stored_payloads_follow_the_sidecar_platforms() {
        assert_eq!(
            FitSubject::stored(&meta(&[], None), None).payload_platforms,
            vec![None]
        );
        assert_eq!(
            FitSubject::stored(&meta(&[("linux", "x86_64")], None), None).payload_platforms,
            vec![Some("linux-x86_64".to_string())]
        );
    }

    #[test]
    fn fit_a_tool_naming_no_driver_interface_fits_both_levels() {
        let levels = two_levels();
        let tool = contracts(&["murmur:tool/run@0.1.0"], &[]);
        assert_eq!(levels_accepting(&tool, &levels), vec![1, 2]);
        assert_eq!(newest_fitting_level(&tool, &levels), Some(2));
    }

    #[test]
    fn fit_a_process_driver_fits_only_the_level_it_was_built_against() {
        let levels = two_levels();
        let driver = contracts(&["murmur:driver/process@0.3.0"], &[]);
        let subject = FitSubject::component(driver.clone());
        assert_eq!(
            version_fit(&subject, &levels[0], "linux-x86_64"),
            VersionFit::Fits
        );
        let VersionFit::Unserved(mismatched) = version_fit(&subject, &levels[1], "linux-x86_64")
        else {
            panic!("a 0.3.0 driver fits a level serving murmur:driver@0.4.0");
        };
        assert_eq!(mismatched.len(), 1);
        assert_eq!(mismatched[0].direction, ContractDirection::Export);
        assert_eq!(mismatched[0].package, "murmur:driver");
        assert_eq!(mismatched[0].served_version.as_deref(), Some("0.4.0"));
        assert_eq!(
            level_has_phrase(2, &mismatched[0]),
            "level 2 has murmur:driver@0.4.0"
        );
        assert_eq!(levels_accepting(&driver, &levels), vec![1]);
        assert_eq!(newest_fitting_level(&driver, &levels), Some(1));
    }

    #[test]
    fn fit_a_hook_at_an_older_version_fits_no_level() {
        let levels = two_levels();
        let hook = contracts(&["murmur:hook/lifecycle@0.8.0"], &[]);
        assert_eq!(levels_accepting(&hook, &levels), Vec::<u64>::new());
        assert_eq!(newest_fitting_level(&hook, &levels), None);
        let text = FitSubject::component(contracts(&[], &["murmur:text/stream@0.1.0"]));
        let VersionFit::Unserved(mismatched) = version_fit(&text, &levels[0], "linux-x86_64")
        else {
            panic!("murmur:text fits level 1");
        };
        assert_eq!(
            level_has_phrase(1, &mismatched[0]),
            "level 1 has no version of murmur:text"
        );
    }
}
