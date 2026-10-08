//! The committed components against the list and the WIT tree, in `cargo test --workspace`.

use std::fs;
use std::path::{Path, PathBuf};

use committed_components::{
    check, find_component, parse_list, read_list, rebuild_selection, Component, Outcome, LIST_PATH,
};

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

/// The escape-conformance driver's own test and its gate's refusal both tell the reader to run
/// `scripts/rebuild-components.sh escape-conformance-driver`.
#[test]
fn escape_conformance_driver_is_listed_and_rebuildable() {
    let list = list();
    let driver = find_component(&list, "escape-conformance-driver").unwrap();
    assert!(!driver.is_frozen(), "{driver:?}");
}

/// A rebuild refuses a frozen or unknown name before it builds anything; with no name it selects
/// every non-frozen line and nothing else.
#[test]
fn a_rebuild_refuses_a_frozen_or_unknown_name_and_never_selects_a_frozen_line() {
    let list = list();

    let err = rebuild_selection(&list, Some("process-driver-v1")).unwrap_err();
    assert!(
        err.0.contains("`process-driver-v1` is frozen")
            && err.0.contains("never rebuilds it")
            && err.0.contains("murmur:driver/process@0.1.0"),
        "{err}"
    );

    let err = rebuild_selection(&list, Some("no-such-component")).unwrap_err();
    assert!(
        err.0.contains("`no-such-component`")
            && err.0.contains(&format!("first column of {LIST_PATH}")),
        "{err}"
    );

    let all = rebuild_selection(&list, None).unwrap();
    assert!(!all.is_empty());
    assert!(all.iter().all(|c| !c.is_frozen()), "{all:?}");
    assert_eq!(all.len(), list.iter().filter(|c| !c.is_frozen()).count());
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

fn write(path: &Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

/// A scratch repository holding the committed `echo-tool` component (which exports
/// `murmur:tool/run@0.1.0`) under three lines: one checked against a tree that declares
/// `murmur:tool@0.1.0`, one against a tree bumped to `0.2.0`, and a frozen one against the bumped
/// tree. A stray `.wasm` sits beside them, and two more sit where the scan must not look.
#[test]
fn a_bump_names_the_stale_component_and_its_rebuild_command_and_an_unlisted_wasm() {
    let echo_tool = fs::read(
        repo_root().join("crates/murmur-cli/tests/fixtures/run/components/echo-tool.wasm"),
    )
    .unwrap();
    let repo = tempfile::tempdir().unwrap();
    let root = repo.path();
    write(
        &root.join("crates/old/tool.wit"),
        b"package murmur:tool@0.1.0;\n",
    );
    write(
        &root.join("crates/new/deps/tool.wit"),
        b"package murmur:tool@0.2.0;\n",
    );
    for name in ["current", "stale", "frozen"] {
        write(&root.join(format!("crates/out/{name}.wasm")), &echo_tool);
    }
    write(&root.join("crates/out/stray.wasm"), &echo_tool);
    write(
        &root.join("crates/src/target/release/built.wasm"),
        &echo_tool,
    );
    write(&root.join("crates/.store/cached.wasm"), &echo_tool);

    let list = parse_list(
        "current crates/old crates/src - crates/out/current.wasm\n\
         stale   crates/new crates/src - crates/out/stale.wasm\n\
         frozen  crates/new -          - crates/out/frozen.wasm  frozen: kept as built\n",
    )
    .unwrap();
    let report = check(root, &list, None).unwrap();

    let outcomes: Vec<(&str, &Outcome)> = report
        .entries
        .iter()
        .map(|entry| (entry.component.name.as_str(), &entry.outcome))
        .collect();
    assert_eq!(outcomes[0], ("current", &Outcome::Current));
    let Outcome::Stale(differs) = outcomes[1].1 else {
        panic!("{outcomes:?}");
    };
    assert_eq!(differs.len(), 1, "{differs:?}");
    assert_eq!(differs[0].interface, "murmur:tool/run@0.1.0");
    let Outcome::Frozen { differs, .. } = outcomes[2].1 else {
        panic!("{outcomes:?}");
    };
    assert_eq!(differs.len(), 1, "{differs:?}");
    assert_eq!(
        report.unlisted,
        vec![PathBuf::from("crates/out/stray.wasm")]
    );
    assert!(report.failed());

    let text = report.render();
    for line in [
        "stale stale (crates/out/stale.wasm)",
        "    export murmur:tool/run@0.1.0: built against 0.1.0, crates/new declares 0.2.0",
        "    rebuild: scripts/rebuild-components.sh stale",
        "skipped frozen: frozen — kept as built (differs from crates/new: murmur:tool/run@0.1.0 \
         built against 0.1.0, tree declares 0.2.0)",
        "unlisted crates/out/stray.wasm: no line in crates/capsule-runtime/wit/components.list \
         names it; add one",
        "1 current, 1 stale, 0 failed, 1 unlisted, 1 frozen skipped",
    ] {
        assert!(
            text.lines().any(|l| l == line),
            "missing {line:?} in:\n{text}"
        );
    }
    assert!(
        !text.contains("rebuild: scripts/rebuild-components.sh current"),
        "{text}"
    );
    assert!(
        !text.contains("rebuild: scripts/rebuild-components.sh frozen"),
        "{text}"
    );

    // Only the frozen line differing is not a failure.
    let frozen_only = check(root, &list[2..], Some("frozen")).unwrap();
    assert!(!frozen_only.failed(), "{}", frozen_only.render());
}
