//! The process driver every case runs through, carried inside the gate binary.
//!
//! `driver/` holds its source, its `murmur.yaml` and the built component. The gate packs the
//! embedded copies with `mur build` once per run and installs the artifact into each case's
//! project store with `mur install`, so a run grades against exactly the driver committed at the
//! commit under test and makes no registry request.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The driver's artifact name; every generated manifest names it in `inference.driver.artifact`.
pub const DRIVER_NAME: &str = "escape-conformance-driver";
/// The driver's artifact version, as `driver/murmur.yaml` declares it.
pub const DRIVER_VERSION: &str = "0.1.0";
/// The committed component, built from `driver/src/lib.rs`.
pub const DRIVER_WASM: &[u8] = include_bytes!("../driver/escape_conformance_driver.wasm");
/// The driver artifact's own manifest. Its `requires_files` names the component.
pub const DRIVER_MANIFEST: &str = include_str!("../driver/murmur.yaml");

/// The file name `DRIVER_MANIFEST`'s `requires_files` lists.
const DRIVER_WASM_FILE: &str = "escape_conformance_driver.wasm";

/// Checks the embedded component's exports against the `murmur:driver/process` version this
/// build of `capsule-runtime` accepts. The error is the runtime's own refusal text, which names
/// both the version it expects and the versions the component exports.
pub fn check_interface() -> Result<(), String> {
    let contracts = murmur_artifact::extract_wit_contracts(DRIVER_WASM)
        .map_err(|err| format!("could not read the embedded driver's WIT contracts: {err}"))?;
    capsule_runtime::process_driver::check_driver_interface(
        "process",
        DRIVER_NAME,
        DRIVER_VERSION,
        contracts.as_ref(),
    )
    .map_err(|err| err.to_string())
}

/// Writes the embedded manifest and component into `<work_root>/driver-artifact/` and packs them
/// with `<mur> build` into `<work_root>/escape-conformance-driver-<version>.mur.zip`, which it
/// returns. Errors carry `mur`'s stderr.
pub fn build_artifact(mur: &Path, work_root: &Path) -> Result<PathBuf, String> {
    let source = work_root.join("driver-artifact");
    fs::create_dir_all(&source)
        .map_err(|err| format!("could not create {}: {err}", source.display()))?;
    for (name, bytes) in [
        ("murmur.yaml", DRIVER_MANIFEST.as_bytes()),
        (DRIVER_WASM_FILE, DRIVER_WASM),
    ] {
        let path = source.join(name);
        fs::write(&path, bytes)
            .map_err(|err| format!("could not write {}: {err}", path.display()))?;
    }
    let artifact = work_root.join(format!("{DRIVER_NAME}-{DRIVER_VERSION}.mur.zip"));
    let output = Command::new(mur)
        .arg("build")
        .arg(&source)
        .arg("-o")
        .arg(&artifact)
        .output()
        .map_err(|err| format!("could not run `{} build`: {err}", mur.display()))?;
    if !output.status.success() {
        return Err(format!(
            "`{} build {}` failed: {}",
            mur.display(),
            source.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    // `mur build` can exit 0 without writing the file it was asked for.
    if !artifact.is_file() {
        return Err(format!(
            "`{} build` exited 0 but wrote no {}",
            mur.display(),
            artifact.display()
        ));
    }
    Ok(artifact)
}

/// Installs `artifact` into `project_dir`'s store with `<mur> install`, run from `project_dir`,
/// which must already hold the case's `murmur.yaml`. `mur` then writes the artifact under
/// `<project_dir>/.murmur/artifacts` and records it in `<project_dir>/murmur.lock`. Errors carry
/// `mur`'s stderr.
pub fn install_into(mur: &Path, artifact: &Path, project_dir: &Path) -> Result<(), String> {
    let output = Command::new(mur)
        .arg("install")
        .arg(artifact)
        .current_dir(project_dir)
        .output()
        .map_err(|err| format!("could not run `{} install`: {err}", mur.display()))?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "`{} install {}` failed in {}: {}",
        mur.display(),
        artifact.display(),
        project_dir.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness_protocol::{
        end_line, fail_line, result_line, tool_line, usage_line, ProbeConfig, ENV_BRIDGE_TOKEN,
        ENV_BRIDGE_URL, ENV_PROBE, PROBE_HARNESS_VERSION,
    };
    use capsule_runtime::process_driver::{
        check_required_env, describe_process_driver_wasm, launch_and_parse_process_driver_wasm,
        Bridge, Event, FailureKind, InterruptMethod, LaunchRequest, Session, SessionMode,
        TurnFailure, Usage,
    };

    #[test]
    fn committed_driver_exports_the_process_interface_this_runtime_accepts() {
        if let Err(err) = check_interface() {
            panic!(
                "the committed driver does not export {}; rebuild it with \
                 `scripts/rebuild-components.sh escape-conformance-driver` from the repository \
                 root: {err}",
                capsule_runtime::process_driver::PROCESS_DRIVER_IFACE
            );
        }
    }

    #[test]
    fn embedded_manifest_names_this_driver() {
        assert!(DRIVER_MANIFEST.contains(&format!("name: {DRIVER_NAME}\n")));
        assert!(DRIVER_MANIFEST.contains(&format!("version: {DRIVER_VERSION}\n")));
        assert!(DRIVER_MANIFEST.contains("runtime: driver\n"));
        assert!(DRIVER_MANIFEST.contains(&format!("- {DRIVER_WASM_FILE}")));
    }

    #[test]
    fn committed_driver_describes_the_probe_harness() {
        let description = describe_process_driver_wasm(DRIVER_WASM).expect("the driver loads");
        assert_eq!(description.binary, "probe-driver");
        assert!(
            description
                .tested_versions
                .iter()
                .any(|v| v == PROBE_HARNESS_VERSION),
            "tested-versions {:?} must contain the version probe-driver prints, {}",
            description.tested_versions,
            PROBE_HARNESS_VERSION
        );
        assert!(description.required_env.is_empty());
        assert!(description.reports_usage);
        assert!(matches!(
            description.interrupt,
            InterruptMethod::Unsupported
        ));
        // Every generated manifest leaves `capabilities.env.allow` undeclared.
        check_required_env(DRIVER_NAME, DRIVER_VERSION, &description, &[])
            .expect("the driver needs no variable the manifest does not allow");
    }

    fn request(bridge: bool, config: Option<String>) -> LaunchRequest {
        LaunchRequest {
            model: None,
            system_prompt: "system".to_string(),
            config,
            bridge: bridge.then(|| Bridge {
                server_name: "murmur".to_string(),
                url: "http://127.0.0.1:4242/mcp".to_string(),
                bearer_token: "bearer-token".to_string(),
                tool_names: vec!["bash".to_string(), "python3".to_string()],
            }),
            session: Session {
                id: "session".to_string(),
                mode: SessionMode::New,
            },
            harness_version: Some(PROBE_HARNESS_VERSION.to_string()),
            task: "Escape-conformance probe.".to_string(),
        }
    }

    #[test]
    fn committed_driver_reads_every_line_the_probe_writes() {
        let config = ProbeConfig {
            tool: "python3".to_string(),
            script: "ec-probe.py".to_string(),
            script2: Some("ec-probe2.py".to_string()),
            case: "resource-workdir-filler".to_string(),
            log: "/tmp/a work root/probe-driver.txt".to_string(),
        }
        .to_json()
        .to_string();
        let lines = vec![
            tool_line("c1", "python3", r#"{"command":"ec-probe.py"}"#),
            result_line("c1", false, "$ python3 ec-probe.py\nExit code: 0"),
            tool_line("c2", "python3", r#"{"command":"ec-probe2.py"}"#),
            result_line("c2", true, "workdir ceiling exceeded"),
            usage_line(),
            end_line("case=resource-workdir-filler tool=python3 :: isError=false :: done"),
        ];

        let (plan, events) = launch_and_parse_process_driver_wasm(
            DRIVER_WASM,
            request(true, Some(config.clone())),
            lines,
        )
        .expect("the driver runs");
        let plan = plan.expect("a launch with a bridge and a config is accepted");
        let mut env_set = plan.env_set.clone();
        env_set.sort();
        let mut expected = vec![
            (
                ENV_BRIDGE_URL.to_string(),
                "http://127.0.0.1:4242/mcp".to_string(),
            ),
            (ENV_BRIDGE_TOKEN.to_string(), "bearer-token".to_string()),
            (ENV_PROBE.to_string(), config),
        ];
        expected.sort();
        assert_eq!(env_set, expected);
        assert!(plan.args.is_empty());
        assert!(plan.files.is_empty());
        assert!(plan.stdin.is_none());

        match &events[..] {
            [Event::ToolCall(c1), Event::ToolResult(r1), Event::ToolCall(c2), Event::ToolResult(r2), Event::Usage(Usage {
                input: Some(0),
                output: Some(0),
                ..
            }), Event::TurnEnd(summary)] => {
                assert_eq!((c1.id.as_str(), c1.name.as_str()), ("c1", "python3"));
                assert_eq!(c1.input, r#"{"command":"ec-probe.py"}"#);
                assert_eq!(r1.id, "c1");
                assert!(!r1.is_error);
                assert_eq!(r1.output, "$ python3 ec-probe.py Exit code: 0");
                assert_eq!((c2.id.as_str(), c2.name.as_str()), ("c2", "python3"));
                assert_eq!(r2.id, "c2");
                assert!(r2.is_error);
                assert_eq!(
                    summary,
                    "case=resource-workdir-filler tool=python3 :: isError=false :: done"
                );
            }
            other => panic!("the driver read the probe's lines as {other:?}"),
        }

        let lines = vec![result_line("c1", false, ""), fail_line("x")];
        let (_, events) = launch_and_parse_process_driver_wasm(
            DRIVER_WASM,
            request(true, Some("{}".to_string())),
            lines,
        )
        .expect("the driver runs");
        match &events[..] {
            [Event::ToolResult(result), Event::TurnFailed(TurnFailure {
                kind: FailureKind::HarnessError,
                message,
            })] => {
                assert_eq!(result.output, "");
                assert!(!result.is_error);
                assert_eq!(message, "x");
            }
            other => panic!("an empty result and a fail line read as {other:?}"),
        }

        for request in [request(false, Some("{}".to_string())), request(true, None)] {
            let (plan, _) = launch_and_parse_process_driver_wasm(DRIVER_WASM, request, Vec::new())
                .expect("the driver runs");
            assert!(plan.is_err(), "{plan:?}");
        }
    }
}
