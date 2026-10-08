//! The comparison of committed components against the WIT they build against, and its report.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use murmur_artifact::{
    extract_wit_contracts, wit_package_declarations, ContractDirection, UnservedInterface,
};

use crate::list::{find_component, Component, ListError};
use crate::{COMMAND, LIST_PATH};

/// What the check found for one listed component.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Every `murmur:*` name the component imports or exports is at the version its `wit`
    /// subtree declares.
    Current,
    /// The `murmur:*` names whose version differs from the subtree's, or whose package the
    /// subtree does not declare.
    Stale(Vec<UnservedInterface>),
    /// The component could not be compared: its output is missing, does not parse as wasm, is a
    /// core module rather than a component, or its `wit` subtree could not be read.
    Failed(String),
    /// A frozen line. `differs` is informational and never fails the check: the names that differ
    /// from the line's `wit` subtree, empty when it names none or the output cannot be read.
    Frozen {
        reason: String,
        differs: Vec<UnservedInterface>,
    },
}

/// One listed component and what the check found for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub component: Component,
    pub outcome: Outcome,
}

/// The result of [`check`]. [`CheckReport::render`] is the text both the binary's `--check` and
/// the crate's test print.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckReport {
    /// One entry per checked component, in list order.
    pub entries: Vec<Entry>,
    /// Committed `.wasm` files no line names, relative to the repository root. Always empty when
    /// the check was asked about one name.
    pub unlisted: Vec<PathBuf>,
}

/// Compares every listed component, or only the one named `only`, against the `package`
/// declarations of its `wit` subtree. With no name, also scans `crates/` for committed `.wasm`
/// files the list does not name.
///
/// Reads only: nothing is written and no toolchain is consulted. Errors when `only` names no
/// listed component, or when `crates/` cannot be scanned.
pub fn check(
    repo_root: &Path,
    list: &[Component],
    only: Option<&str>,
) -> Result<CheckReport, ListError> {
    let selected: Vec<&Component> = match only {
        Some(name) => vec![find_component(list, name)?],
        None => list.iter().collect(),
    };
    let entries = selected
        .into_iter()
        .map(|component| Entry {
            component: component.clone(),
            outcome: judge(repo_root, component),
        })
        .collect();
    let unlisted = match only {
        Some(_) => Vec::new(),
        None => unlisted_components(repo_root, list).map_err(|err| {
            ListError(format!(
                "could not scan {} for committed components: {err}",
                repo_root.join("crates").display()
            ))
        })?,
    };
    Ok(CheckReport { entries, unlisted })
}

/// Judges one component against its `wit` subtree. A frozen component is never judged
/// [`Outcome::Stale`] or [`Outcome::Failed`].
pub fn judge(repo_root: &Path, component: &Component) -> Outcome {
    let compared = compare(repo_root, component);
    match (&component.frozen, compared) {
        (Some(reason), compared) => Outcome::Frozen {
            reason: reason.clone(),
            differs: compared.ok().flatten().unwrap_or_default(),
        },
        (None, Ok(Some(differs))) if differs.is_empty() => Outcome::Current,
        (None, Ok(Some(differs))) => Outcome::Stale(differs),
        (None, Ok(None)) => Outcome::Failed("the line names no wit directory".to_string()),
        (None, Err(message)) => Outcome::Failed(message),
    }
}

/// The names that differ from the component's `wit` subtree; `Ok(None)` when the line names no
/// subtree.
fn compare(
    repo_root: &Path,
    component: &Component,
) -> Result<Option<Vec<UnservedInterface>>, String> {
    let Some(wit) = &component.wit else {
        return Ok(None);
    };
    let path = repo_root.join(&component.output);
    let bytes = fs::read(&path).map_err(|err| format!("could not read the output: {err}"))?;
    let contracts = extract_wit_contracts(&bytes)
        .map_err(|err| format!("the output is not readable wasm: {err}"))?
        .ok_or_else(|| "the output is a core module, not a component".to_string())?;
    let declared = wit_package_declarations(&repo_root.join(wit)).map_err(|err| {
        format!(
            "could not read the package declarations under {}: {err}",
            wit.display()
        )
    })?;
    let table: Vec<(&str, &str)> = declared
        .iter()
        .map(|(package, version)| (package.as_str(), version.as_str()))
        .collect();
    Ok(Some(contracts.unserved_against(&table)))
}

/// Every `.wasm` file under `<repo_root>/crates` that no line's `output` names, relative to the
/// repository root and sorted. Directories called `target` and hidden directories are not
/// entered: they hold build output and local stores, never a committed component.
pub fn unlisted_components(repo_root: &Path, list: &[Component]) -> std::io::Result<Vec<PathBuf>> {
    let listed: BTreeSet<&Path> = list.iter().map(|c| c.output.as_path()).collect();
    let mut found = Vec::new();
    collect_wasm(repo_root, Path::new("crates"), &mut found)?;
    found.retain(|path| !listed.contains(path.as_path()));
    found.sort();
    Ok(found)
}

fn collect_wasm(repo_root: &Path, relative: &Path, into: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in fs::read_dir(repo_root.join(relative))? {
        let entry = entry?;
        let name = entry.file_name();
        let path = relative.join(&name);
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            let name = name.to_string_lossy();
            if name == "target" || name.starts_with('.') {
                continue;
            }
            collect_wasm(repo_root, &path, into)?;
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("wasm") {
            into.push(path);
        }
    }
    Ok(())
}

impl CheckReport {
    /// Whether any non-frozen component is stale or could not be compared, or any committed
    /// `.wasm` is unlisted. Frozen components never count.
    pub fn failed(&self) -> bool {
        !self.unlisted.is_empty()
            || self
                .entries
                .iter()
                .any(|entry| matches!(entry.outcome, Outcome::Stale(_) | Outcome::Failed(_)))
    }

    /// The names of the stale components, in list order.
    pub fn stale(&self) -> Vec<&str> {
        self.entries
            .iter()
            .filter(|entry| matches!(entry.outcome, Outcome::Stale(_)))
            .map(|entry| entry.component.name.as_str())
            .collect()
    }

    /// One line per current or frozen component, a block per stale one naming each mismatch and
    /// the command that rebuilds it, a line per failed or unlisted one, then a count. Ends with
    /// the full-rebuild line when anything is stale.
    pub fn render(&self) -> String {
        let mut out = String::new();
        let (mut current, mut stale, mut failed, mut frozen) = (0, 0, 0, 0);
        for Entry { component, outcome } in &self.entries {
            let name = &component.name;
            let output = component.output.display();
            match outcome {
                Outcome::Current => {
                    current += 1;
                    let _ = writeln!(out, "current {name}");
                }
                Outcome::Stale(differs) => {
                    stale += 1;
                    let wit = wit_label(component);
                    let _ = writeln!(out, "stale {name} ({output})");
                    for interface in differs {
                        let _ = writeln!(out, "    {}", describe_mismatch(interface, &wit));
                    }
                    let _ = writeln!(out, "    rebuild: {COMMAND} {name}");
                }
                Outcome::Failed(message) => {
                    failed += 1;
                    let _ = writeln!(out, "failed {name} ({output}): {message}");
                }
                Outcome::Frozen { reason, differs } => {
                    frozen += 1;
                    let _ = write!(out, "skipped {name}: frozen — {reason}");
                    if !differs.is_empty() {
                        let details: Vec<String> = differs.iter().map(describe_differs).collect();
                        let _ = write!(
                            out,
                            " (differs from {}: {})",
                            wit_label(component),
                            details.join("; ")
                        );
                    }
                    out.push('\n');
                }
            }
        }
        for path in &self.unlisted {
            let _ = writeln!(
                out,
                "unlisted {}: no line in {LIST_PATH} names it; add one",
                path.display()
            );
        }
        let _ = writeln!(
            out,
            "{current} current, {stale} stale, {failed} failed, {} unlisted, {frozen} frozen skipped",
            self.unlisted.len()
        );
        if stale > 0 {
            let _ = writeln!(
                out,
                "`{COMMAND}` with no arguments rebuilds every non-frozen component; commit the \
                 rebuilt files."
            );
        }
        out
    }
}

pub(crate) fn wit_label(component: &Component) -> String {
    component
        .wit
        .as_ref()
        .map_or_else(|| "-".to_string(), |wit| wit.display().to_string())
}

/// `import murmur:task/task@0.1.0: built against 0.1.0, <wit> declares 0.2.0`.
pub(crate) fn describe_mismatch(interface: &UnservedInterface, wit: &str) -> String {
    let direction = match interface.direction {
        ContractDirection::Export => "export",
        ContractDirection::Import => "import",
    };
    let built = match &interface.artifact_version {
        Some(version) => format!("built against {version}"),
        None => "carries no version".to_string(),
    };
    let declared = match &interface.served_version {
        Some(version) => format!("{wit} declares {version}"),
        None => format!("{wit} declares no {} package", interface.package),
    };
    format!("{direction} {}: {built}, {declared}", interface.interface)
}

/// `murmur:text/chunks@0.1.0 built against 0.1.0, tree declares nothing`.
fn describe_differs(interface: &UnservedInterface) -> String {
    format!(
        "{} built against {}, tree declares {}",
        interface.interface,
        interface
            .artifact_version
            .as_deref()
            .unwrap_or("no version"),
        interface.served_version.as_deref().unwrap_or("nothing")
    )
}
