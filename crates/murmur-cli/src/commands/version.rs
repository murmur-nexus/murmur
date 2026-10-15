//! `mur version`: this mur's version, the WIT level it serves, and that level's packages.

use capsule_runtime::served_wit_level;
use serde::Serialize;

use crate::wit_level::package_table;

/// The `--json` object, keys in this order.
#[derive(Serialize)]
struct VersionReport<'a> {
    mur_version: &'a str,
    wit_level: u64,
    packages: Vec<PackageVersion<'a>>,
}

#[derive(Serialize)]
struct PackageVersion<'a> {
    package: &'a str,
    version: &'a str,
}

pub(crate) fn run_version(json: bool) {
    let level = served_wit_level();
    let mur_version = env!("CARGO_PKG_VERSION");
    if json {
        let report = VersionReport {
            mur_version,
            wit_level: level.level,
            packages: level
                .packages
                .iter()
                .map(|(package, version)| PackageVersion { package, version })
                .collect(),
        };
        let text = serde_json::to_string_pretty(&report)
            .unwrap_or_else(|error| unreachable!("the version report always serializes: {error}"));
        capsule_runtime::report_println!("{text}");
        return;
    }
    capsule_runtime::report_println!("mur {mur_version}");
    capsule_runtime::report_println!("WIT level {}", level.level);
    capsule_runtime::report_println!();
    for line in package_table(level) {
        capsule_runtime::report_println!("{line}");
    }
}
