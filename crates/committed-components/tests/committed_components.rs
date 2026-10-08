//! The committed components against the list and the WIT tree, in `cargo test --workspace`.

use std::path::{Path, PathBuf};

use committed_components::{check, find_component, read_list, Component};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the repository root resolves")
}

fn list() -> Vec<Component> {
    read_list(&repo_root()).unwrap_or_else(|err| panic!("{err}"))
}

/// The check `scripts/rebuild-components.sh --check` runs, with the same report on failure.
#[test]
fn every_committed_component_matches_the_wit_it_builds_against() {
    let root = repo_root();
    let report = check(&root, &list(), None).unwrap_or_else(|err| panic!("{err}"));
    if report.failed() {
        panic!(
            "committed components do not match the WIT they build against:\n\n{}",
            report.render()
        );
    }
}

#[test]
fn the_list_parses_and_names_at_least_one_component() {
    assert!(!list().is_empty());
}

/// The escape-conformance driver's own test and its gate's refusal both tell the reader to run
/// `scripts/rebuild-components.sh escape-conformance-driver`.
#[test]
fn escape_conformance_driver_is_listed_and_rebuildable() {
    let list = list();
    let driver = find_component(&list, "escape-conformance-driver").unwrap();
    assert!(!driver.is_frozen(), "{driver:?}");
}

#[test]
fn every_frozen_line_carries_a_reason() {
    for component in list() {
        if let Some(reason) = &component.frozen {
            assert!(!reason.trim().is_empty(), "{component:?}");
        }
    }
}

/// The staleness test only reads non-frozen outputs; a frozen line naming a deleted file would
/// otherwise go unnoticed.
#[test]
fn every_listed_output_exists() {
    let root = repo_root();
    for component in list() {
        assert!(
            root.join(&component.output).is_file(),
            "{} (line {}) names {}, which does not exist",
            component.name,
            component.line,
            component.output.display()
        );
    }
}
