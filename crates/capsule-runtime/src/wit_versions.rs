//! The one version of each `murmur:*` WIT package this host serves, and the WIT levels that
//! number those sets.

use std::sync::OnceLock;

use murmur_artifact::{parse_wit_levels, WitLevel};

/// Every `murmur:*` WIT package declared under `crates/capsule-runtime/wit/`, paired with the one
/// version of it this host links and resolves, sorted by package name.
///
/// These are the only versions the host accepts. Each interface is resolved or provided under
/// exactly one instance name, with no fallback to any other version (see `wit/VERSIONING.md`), so
/// a component naming any other version of one of these packages fails to instantiate.
/// It is the package set of [`served_wit_level`]; fit is decided against that level, never against
/// this table directly.
///
/// The tests below hold this table equal to the `package` declarations in the WIT tree, to every
/// interface name the host resolves, and to the newest level in [`WIT_LEVELS_FILE`], so it moves
/// with a version bump or fails the build.
pub const SERVED_WIT_PACKAGES: &[(&str, &str)] = &[
    ("murmur:artifact-manager", "0.1.0"),
    ("murmur:capsule", "0.1.0"),
    ("murmur:conversation", "0.2.0"),
    ("murmur:driver", "0.4.0"),
    ("murmur:hook", "0.9.0"),
    ("murmur:host", "0.1.0"),
    ("murmur:message", "0.1.0"),
    ("murmur:runtime", "0.4.0"),
    ("murmur:runtime-guest", "0.1.0"),
    ("murmur:shell", "0.1.0"),
    ("murmur:stream", "0.1.0"),
    ("murmur:task", "0.1.0"),
    ("murmur:task-io", "0.1.0"),
    ("murmur:tool", "0.1.0"),
    ("murmur:tool-registry", "0.1.0"),
];

/// The repository path of the committed WIT level table, for messages that name it.
pub const WIT_LEVELS_FILE: &str = "crates/capsule-runtime/wit/levels.txt";

/// Every committed WIT level, oldest first, parsed once from [`WIT_LEVELS_FILE`].
///
/// The file is compiled in, and `wit_levels_file_parses` parses the same bytes, so a malformed
/// table fails the test suite before it can reach this `expect`.
pub fn wit_levels() -> &'static [WitLevel] {
    static LEVELS: OnceLock<Vec<WitLevel>> = OnceLock::new();
    LEVELS.get_or_init(|| {
        parse_wit_levels(include_str!("../wit/levels.txt"))
            .unwrap_or_else(|error| panic!("{WIT_LEVELS_FILE} is not a valid level table: {error}"))
    })
}

/// The WIT level this host serves: the newest level in [`WIT_LEVELS_FILE`].
pub fn served_wit_level() -> &'static WitLevel {
    wit_levels()
        .last()
        .expect("parse_wit_levels refuses a table with no level")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::Path;

    use super::{served_wit_level, wit_levels, SERVED_WIT_PACKAGES, WIT_LEVELS_FILE};

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
        let declared = murmur_artifact::wit_package_declarations(Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/wit"
        )))
        .unwrap();
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
            crate::runtime::WIT_STREAM_EVENTS_IFACE,
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

    #[test]
    fn wit_levels_file_parses() {
        murmur_artifact::parse_wit_levels(include_str!("../wit/levels.txt"))
            .unwrap_or_else(|error| panic!("{WIT_LEVELS_FILE}: {error}"));
        assert!(!wit_levels().is_empty());
    }

    #[test]
    fn wit_levels_served_level_is_level_2() {
        assert_eq!(served_wit_level().level, 2);
    }

    /// Level 2 moved `murmur:driver` alone, so an artifact built at level 1 whose interfaces name
    /// no `murmur:driver` package fits level 2 unchanged, and a level-1 process driver does not.
    #[test]
    fn wit_levels_level_2_moves_only_the_driver_package() {
        use murmur_artifact::{
            current_platform, version_fit, FitSubject, VersionFit, WitContracts,
        };
        let levels = wit_levels();
        let (one, two) = (&levels[0], &levels[1]);
        let moved: Vec<_> = one
            .packages
            .iter()
            .zip(&two.packages)
            .filter(|(before, after)| before != after)
            .collect();
        let driver = |version: &str| ("murmur:driver".to_string(), version.to_string());
        assert_eq!(moved, [(&driver("0.3.0"), &driver("0.4.0"))]);

        let contracts = |exports: &[&str], imports: &[&str]| WitContracts {
            exports: exports.iter().map(|e| e.to_string()).collect(),
            imports: imports.iter().map(|i| i.to_string()).collect(),
        };
        let fits = |contracts: WitContracts, level| {
            version_fit(&FitSubject::component(contracts), level, current_platform())
                == VersionFit::Fits
        };
        let hook = || {
            contracts(
                &["murmur:hook/lifecycle@0.9.0"],
                &[
                    "murmur:runtime/inference@0.4.0",
                    "murmur:task-io/read@0.1.0",
                ],
            )
        };
        let tool = || contracts(&["murmur:tool/run@0.1.0"], &["murmur:stream/events@0.1.0"]);
        for (what, built) in [("hook", hook()), ("tool", tool())] {
            assert!(fits(built.clone(), one), "{what} at level 1");
            assert!(fits(built, two), "{what} at level 2");
        }
        let old_driver = || contracts(&["murmur:driver/process@0.3.0"], &[]);
        assert!(fits(old_driver(), one));
        assert!(!fits(old_driver(), two));
    }

    /// The served table is the newest level's set. A version moved in one but not the other
    /// fails here with the instruction for the bump: append the next level.
    #[test]
    fn wit_levels_newest_level_is_the_served_table() {
        let newest = served_wit_level();
        let level: BTreeSet<(String, String)> = newest.packages.iter().cloned().collect();
        let served = served_set();
        if level == served {
            return;
        }
        let packages: BTreeSet<&str> = level
            .iter()
            .chain(served.iter())
            .map(|(package, _)| package.as_str())
            .collect();
        let differences: Vec<String> = packages
            .into_iter()
            .filter_map(|package| {
                let at_level = newest.version_of(package);
                let in_table = SERVED_WIT_PACKAGES
                    .iter()
                    .find(|(p, _)| *p == package)
                    .map(|(_, v)| *v);
                (at_level != in_table).then(|| {
                    format!(
                        "  {package}: {} in level {} → {} in SERVED_WIT_PACKAGES",
                        at_level.unwrap_or("absent"),
                        newest.level,
                        in_table.unwrap_or("absent")
                    )
                })
            })
            .collect();
        let next = newest.level + 1;
        panic!(
            "SERVED_WIT_PACKAGES no longer equals WIT level {level} in {WIT_LEVELS_FILE}:\n{}\n\
             append level {next} to {WIT_LEVELS_FILE}, listing every package at its new version \
             (one `{next} <package>@<version>` line per package); never edit, renumber or reuse \
             level {level}",
            differences.join("\n"),
            level = newest.level,
        );
    }
}
