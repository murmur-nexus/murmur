//! Rebuilding components from source and copying them over their committed outputs.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use murmur_artifact::UnservedInterface;

use crate::check::{describe_mismatch, judge, wit_label, Outcome};
use crate::list::{find_component, Component, ListError};
use crate::{COMMAND, TARGET_DIR, WASM_TARGET};

/// Variables that would carry the caller's host compiler flags into a wasm build.
const RUSTFLAGS_VARIABLES: [&str; 3] = [
    "RUSTFLAGS",
    "CARGO_ENCODED_RUSTFLAGS",
    "CARGO_BUILD_RUSTFLAGS",
];

/// Checks that [`WASM_TARGET`] is installed in the compiler's sysroot, which is where rustup puts
/// a target: `<rustc --print sysroot>/lib/rustlib/wasm32-wasip2`. The compiler is `$RUSTC` when
/// set, else `rustc` on `PATH`, the same one the nested `cargo build` picks. Returns its
/// `rustc -V`, trimmed.
///
/// Errors when `rustc` cannot be run, and when the target is missing; the second error names the
/// `rustc -V` in use and the `rustup target add` line that installs it.
pub fn wasm_target() -> Result<String, String> {
    let rustc = env::var_os("RUSTC").unwrap_or_else(|| OsString::from("rustc"));
    let shown = Path::new(&rustc).display().to_string();
    let run = |args: &[&str]| -> Result<String, String> {
        let output = Command::new(&rustc)
            .args(args)
            .output()
            .map_err(|err| format!("could not run `{shown} {}`: {err}", args.join(" ")))?;
        if !output.status.success() {
            return Err(format!(
                "`{shown} {}` failed ({}): {}",
                args.join(" "),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    };
    let version = run(&["-V"])?;
    let sysroot = PathBuf::from(run(&["--print", "sysroot"])?);
    if !sysroot.join("lib/rustlib").join(WASM_TARGET).is_dir() {
        return Err(format!(
            "the {WASM_TARGET} target is not installed for {version} (sysroot {}). \
             Install it, then run this again:\n\n    rustup target add {WASM_TARGET}\n\n\
             Nothing was built.",
            sysroot.display()
        ));
    }
    Ok(version)
}

/// What a rebuild did with one component.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebuildOutcome {
    /// Built and copied over the output. `changed` is whether the bytes differ from the ones it
    /// replaced.
    Rebuilt { changed: bool },
    /// Built and copied, but its `murmur:*` names still differ from its `wit` subtree: the source
    /// pins an interface the subtree no longer declares.
    StillStale(Vec<UnservedInterface>),
    /// Not built, or built but not copied. The output was not written.
    Failed(String),
}

/// The result of [`rebuild`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebuildReport {
    /// Every component the run tried to build, in list order.
    pub built: Vec<(Component, RebuildOutcome)>,
    /// The frozen components a full run skipped; empty when one name was asked for.
    pub frozen: Vec<Component>,
}

impl RebuildReport {
    /// Whether any build failed or any rebuilt component is still stale.
    pub fn failed(&self) -> bool {
        self.built
            .iter()
            .any(|(_, outcome)| !matches!(outcome, RebuildOutcome::Rebuilt { .. }))
    }

    /// `rebuilt N (C changed); skipped F frozen; failed: <names or none>`.
    pub fn summary(&self) -> String {
        let rebuilt = self
            .built
            .iter()
            .filter(|(_, outcome)| !matches!(outcome, RebuildOutcome::Failed(_)))
            .count();
        let changed = self
            .built
            .iter()
            .filter(|(_, outcome)| matches!(outcome, RebuildOutcome::Rebuilt { changed: true }))
            .count();
        let failed: Vec<&str> = self
            .built
            .iter()
            .filter(|(_, outcome)| !matches!(outcome, RebuildOutcome::Rebuilt { .. }))
            .map(|(component, _)| component.name.as_str())
            .collect();
        format!(
            "rebuilt {rebuilt} ({changed} changed); skipped {} frozen; failed: {}",
            self.frozen.len(),
            if failed.is_empty() {
                "none".to_string()
            } else {
                failed.join(", ")
            }
        )
    }
}

/// The components [`rebuild`] builds: every non-frozen one, or only the one named `only`.
/// Errors when `only` names no listed component or a frozen one.
pub fn rebuild_selection<'a>(
    list: &'a [Component],
    only: Option<&str>,
) -> Result<Vec<&'a Component>, ListError> {
    match only {
        Some(name) => {
            let component = find_component(list, name)?;
            if let Some(reason) = &component.frozen {
                return Err(ListError(format!(
                    "`{name}` is frozen and {COMMAND} never rebuilds it: {reason}"
                )));
            }
            Ok(vec![component])
        }
        None => Ok(list.iter().filter(|c| !c.is_frozen()).collect()),
    }
}

/// Rebuilds every non-frozen component, or only the one named `only`, in list order, and copies
/// each built `.wasm` over its committed output. Writes one line per component to `out` as it
/// goes; cargo's own diagnostics go to this process's stderr. A failed build is recorded and the
/// run continues. Every component that was copied is then judged against its `wit` subtree.
///
/// Assumes [`wasm_target`] has passed. Every build shares [`TARGET_DIR`] under `repo_root`, and
/// none inherits the caller's `RUSTFLAGS`. Frozen outputs are never written. Errors, before
/// building anything, as [`rebuild_selection`] does.
pub fn rebuild(
    repo_root: &Path,
    list: &[Component],
    only: Option<&str>,
    out: &mut dyn Write,
) -> Result<RebuildReport, ListError> {
    let selected = rebuild_selection(list, only)?;

    let target_dir = repo_root.join(TARGET_DIR);
    let mut built = Vec::new();
    for component in selected {
        let outcome = match build_and_copy(repo_root, &target_dir, component) {
            Ok(changed) => {
                let state = if changed { "changed" } else { "unchanged" };
                let _ = writeln!(
                    out,
                    "rebuilt {} -> {} ({state})",
                    component.name,
                    component.output.display()
                );
                RebuildOutcome::Rebuilt { changed }
            }
            Err(message) => {
                let _ = writeln!(out, "failed {}: {message}", component.name);
                RebuildOutcome::Failed(message)
            }
        };
        built.push((component.clone(), outcome));
    }

    for (component, outcome) in &mut built {
        if !matches!(outcome, RebuildOutcome::Rebuilt { .. }) {
            continue;
        }
        match judge(repo_root, component) {
            Outcome::Current => {}
            Outcome::Stale(differs) => {
                let wit = wit_label(component);
                let _ = writeln!(
                    out,
                    "failed {}: rebuilt from source but still stale against {wit}",
                    component.name
                );
                for interface in &differs {
                    let _ = writeln!(out, "    {}", describe_mismatch(interface, &wit));
                }
                *outcome = RebuildOutcome::StillStale(differs);
            }
            Outcome::Failed(message) => {
                let _ = writeln!(out, "failed {}: {message}", component.name);
                *outcome = RebuildOutcome::Failed(message);
            }
            Outcome::Frozen { .. } => unreachable!("frozen components are never built"),
        }
    }

    let frozen = match only {
        Some(_) => Vec::new(),
        None => list.iter().filter(|c| c.is_frozen()).cloned().collect(),
    };
    Ok(RebuildReport { built, frozen })
}

/// Builds one component and copies it over its output. Returns whether the bytes changed.
fn build_and_copy(
    repo_root: &Path,
    target_dir: &Path,
    component: &Component,
) -> Result<bool, String> {
    let source = component
        .source
        .as_ref()
        .ok_or_else(|| "the line names no source".to_string())?;
    let cargo = env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let mut command = Command::new(cargo);
    command
        .current_dir(repo_root)
        .args(["build", "--release", "--locked", "--target", WASM_TARGET])
        .arg("--manifest-path")
        .arg(repo_root.join(source).join("Cargo.toml"))
        .arg("--message-format=json-render-diagnostics")
        .env("CARGO_TARGET_DIR", target_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    if !component.features.is_empty() {
        command.arg("--features").arg(component.features.join(","));
    }
    for variable in RUSTFLAGS_VARIABLES {
        command.env_remove(variable);
    }

    let mut child = command
        .spawn()
        .map_err(|err| format!("could not run cargo: {err}"))?;
    let stdout = child.stdout.take().expect("stdout is piped");
    let mut wasm = None;
    let mut read_error = None;
    for line in BufReader::new(stdout).lines() {
        match line {
            Ok(line) => {
                if let Some(path) = built_wasm(&line) {
                    wasm = Some(path);
                }
            }
            Err(err) => {
                read_error = Some(err);
                break;
            }
        }
    }
    // Reap cargo on every path: an early return would leave it running with nobody reading its
    // output.
    if read_error.is_some() {
        let _ = child.kill();
    }
    let status = child
        .wait()
        .map_err(|err| format!("could not wait for cargo: {err}"))?;
    if let Some(err) = read_error {
        return Err(format!("could not read cargo's output: {err}"));
    }
    if !status.success() {
        return Err(format!("cargo build failed ({status})"));
    }
    let wasm = wasm.ok_or_else(|| "cargo build reported no .wasm artifact".to_string())?;

    let bytes =
        fs::read(&wasm).map_err(|err| format!("could not read {}: {err}", wasm.display()))?;
    let output = repo_root.join(&component.output);
    let changed = match fs::read(&output) {
        Ok(previous) => previous != bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => true,
        Err(err) => return Err(format!("could not read {}: {err}", output.display())),
    };
    if changed {
        fs::write(&output, &bytes)
            .map_err(|err| format!("could not write {}: {err}", output.display()))?;
    }
    Ok(changed)
}

/// The `.wasm` path in one line of cargo's JSON message stream, when the line is a
/// `compiler-artifact` message that lists one.
fn built_wasm(line: &str) -> Option<PathBuf> {
    let message: serde_json::Value = serde_json::from_str(line).ok()?;
    if message.get("reason")?.as_str()? != "compiler-artifact" {
        return None;
    }
    message
        .get("filenames")?
        .as_array()?
        .iter()
        .filter_map(serde_json::Value::as_str)
        .find(|name| name.ends_with(".wasm"))
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wasm_path_comes_from_the_compiler_artifact_message() {
        let artifact = r#"{"reason":"compiler-artifact","package_id":"x","filenames":["/t/wasm32-wasip2/release/deps/libx.rlib","/t/wasm32-wasip2/release/x_fixture.wasm"]}"#;
        assert_eq!(
            built_wasm(artifact),
            Some(PathBuf::from("/t/wasm32-wasip2/release/x_fixture.wasm"))
        );
        let dependency =
            r#"{"reason":"compiler-artifact","filenames":["/t/release/deps/libdep.rlib"]}"#;
        assert_eq!(built_wasm(dependency), None);
        let finished = r#"{"reason":"build-finished","success":true}"#;
        assert_eq!(built_wasm(finished), None);
        assert_eq!(built_wasm("   Compiling x v0.1.0"), None);
    }
}
