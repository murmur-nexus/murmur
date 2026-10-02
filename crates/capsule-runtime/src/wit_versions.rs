//! The one version of each `murmur:*` WIT package this host serves.

/// Every `murmur:*` WIT package declared under `crates/capsule-runtime/wit/`, paired with the one
/// version of it this host links and resolves, sorted by package name.
///
/// These are the only versions the host accepts. Each interface is resolved or provided under
/// exactly one instance name, with no fallback to any other version (see `wit/VERSIONING.md`), so
/// a component naming any other version of one of these packages fails to instantiate.
/// `mur doctor` compares every installed artifact's recorded interfaces against this table to name
/// the ones a launch would refuse.
///
/// The tests below hold this table equal to the `package` declarations in the WIT tree and to
/// every interface name the host resolves, so it moves with a version bump or fails the build.
pub const SERVED_WIT_PACKAGES: &[(&str, &str)] = &[
    ("murmur:artifact-manager", "0.1.0"),
    ("murmur:capsule", "0.1.0"),
    ("murmur:conversation", "0.2.0"),
    ("murmur:driver", "0.2.0"),
    ("murmur:hook", "0.9.0"),
    ("murmur:host", "0.1.0"),
    ("murmur:message", "0.1.0"),
    ("murmur:runtime", "0.4.0"),
    ("murmur:runtime-guest", "0.1.0"),
    ("murmur:shell", "0.1.0"),
    ("murmur:task", "0.1.0"),
    ("murmur:task-io", "0.1.0"),
    ("murmur:text", "0.1.0"),
    ("murmur:tool", "0.1.0"),
    ("murmur:tool-registry", "0.1.0"),
];

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::Path;

    use super::SERVED_WIT_PACKAGES;

    fn collect_package_declarations(dir: &Path, into: &mut BTreeSet<(String, String)>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                collect_package_declarations(&path, into);
                continue;
            }
            if path.extension().and_then(|ext| ext.to_str()) != Some("wit") {
                continue;
            }
            for line in fs::read_to_string(&path).unwrap().lines() {
                let Some(declaration) = line
                    .trim()
                    .strip_prefix("package murmur:")
                    .and_then(|rest| rest.strip_suffix(';'))
                else {
                    continue;
                };
                let (name, version) = declaration
                    .split_once('@')
                    .unwrap_or_else(|| panic!("unversioned package in {}", path.display()));
                into.insert((format!("murmur:{name}"), version.to_string()));
            }
        }
    }

    fn served_set() -> BTreeSet<(String, String)> {
        SERVED_WIT_PACKAGES
            .iter()
            .map(|(package, version)| (package.to_string(), version.to_string()))
            .collect()
    }

    /// The table names each package once, and exactly the package versions the WIT tree declares.
    /// The tree is what `wit-bindgen` generates the host's bindings from, so a version bump in it
    /// that the table does not follow fails here.
    #[test]
    fn wit_versions_table_matches_the_wit_tree() {
        let mut declared = BTreeSet::new();
        collect_package_declarations(
            Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/wit")),
            &mut declared,
        );
        assert!(!declared.is_empty(), "no murmur package declarations found");

        let mut packages: Vec<&str> = SERVED_WIT_PACKAGES.iter().map(|(p, _)| *p).collect();
        packages.sort_unstable();
        packages.dedup();
        assert_eq!(
            packages.len(),
            SERVED_WIT_PACKAGES.len(),
            "a package appears twice in SERVED_WIT_PACKAGES"
        );

        assert_eq!(served_set(), declared);
    }

    /// Every instance name the host resolves or provides is at its package's served version. A
    /// resolver constant moved without the table, or the table without the constant, fails here.
    #[test]
    fn wit_versions_table_covers_every_resolved_interface() {
        let resolved = [
            crate::hooks::LIFECYCLE_IFACE,
            crate::process_driver::PROCESS_DRIVER_IFACE,
            crate::runtime::WIT_CAPSULE_IFACE_VERSIONED,
            crate::runtime::WIT_TOOL_IFACE_VERSIONED,
            crate::runtime::WIT_TOOL_REGISTRY_IFACE,
            crate::runtime::WIT_TEXT_CHUNKS_IFACE,
            crate::runtime::WIT_TASK_IFACE,
            crate::conversation_import::CONVERSATION_IFACE_VERSIONED,
            crate::inference_import::INFERENCE_IFACE_VERSIONED,
            crate::task_io_import::TASK_IO_IFACE_VERSIONED,
            crate::tokens_import::TOKENS_IFACE_VERSIONED,
        ];
        let served = served_set();
        for interface in resolved {
            let (package, rest) = interface
                .split_once('/')
                .unwrap_or_else(|| panic!("{interface} names no interface"));
            let (_, version) = rest
                .rsplit_once('@')
                .unwrap_or_else(|| panic!("{interface} carries no version"));
            assert!(
                served.contains(&(package.to_string(), version.to_string())),
                "{interface} is not at a version SERVED_WIT_PACKAGES lists for {package}"
            );
        }
    }

    /// Doctor compares versions as exact strings, while wasmtime's linker treats two names that
    /// differ only in a non-zero patch field as semver-compatible and links them. The two answers
    /// agree only while murmur never ships a package at a non-zero patch.
    #[test]
    fn wit_versions_every_patch_field_is_zero() {
        for (package, version) in SERVED_WIT_PACKAGES {
            let patch = version
                .split('.')
                .nth(2)
                .unwrap_or_else(|| panic!("{package}@{version} has no patch field"));
            assert_eq!(patch, "0", "{package}@{version} has a non-zero patch field");
        }
    }
}
