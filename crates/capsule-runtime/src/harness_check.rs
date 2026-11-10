//! Whether a `transport: process` capsule's harness is installed, at a tested version, and logged
//! in for this user, asked without launching a session.
//!
//! `mur doctor` asks it. Every answer comes from what `mur run` uses: the same driver checks at
//! staging, the same binary resolution, the same probe environment and bound as the version probe
//! a run makes. Only the driver knows how its harness reports a login, so the login question is
//! the driver's `login-check` and `read-login`, never a guess made here.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::agent::process::{
    harness_base_env, probe_harness_version, run_harness_probe, untested_version_message,
    version_is_tested, VersionProbe,
};
use crate::errors::RuntimeError;
use crate::process_driver::{
    check_driver_interface, check_required_env, LoginCheck, LoginOutput, LoginState, ProcessDriver,
    StandaloneDriver,
};
use crate::shell::HostAccountEnv;
use crate::types::CapabilityPolicy;

/// The longest reason, in characters, a [`HarnessLogin`] carries.
pub const LOGIN_REASON_MAX_CHARS: usize = 200;

/// The `PATH` a harness `#!` interpreter is resolved on, for the doctor and explain-scope checks:
/// the runtime's own, as a real run resolves it.
fn harness_path_env() -> String {
    std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_string())
}

/// Owns the paths a [`crate::host_paths::HostPathContext`] borrows for a harness-needs check off a
/// session: the `HOME` `mur` runs with, murmur's own home, and the work folder.
struct HarnessContextHome {
    home: Option<PathBuf>,
    murmur_home: Option<PathBuf>,
    workdir: PathBuf,
}

impl HarnessContextHome {
    fn read(workdir: &Path) -> Self {
        Self {
            home: std::env::var_os("HOME")
                .filter(|home| !home.is_empty())
                .map(PathBuf::from),
            murmur_home: crate::murmur_home::home_path().ok(),
            workdir: workdir.to_path_buf(),
        }
    }

    fn context(&self) -> crate::host_paths::HostPathContext<'_> {
        crate::host_paths::HostPathContext {
            home: self.home.as_deref(),
            murmur_home: self.murmur_home.as_deref(),
            workdir: &self.workdir,
        }
    }
}

/// The `harness` report `mur run --explain-scope` prints and `session_start.effective_grants`
/// carries: the harness's needs resolved against the manifest and this host, read-only, with every
/// need's status. A need the manifest does not allow is `refused` in the report rather than
/// raising — explain-scope exits 0. A driver that cannot be read (missing, unloadable, wrong WIT
/// level) is an [`crate::harness_needs::HarnessReport`] with `unresolved` naming why and every
/// list empty.
///
/// `driver_bytes` is the driver artifact's `.mur.zip` bytes; `command` is `inference.command`;
/// `workdir` is the launch's work folder root. Blocks on its own current-thread runtime.
pub fn harness_needs_report(
    driver_bytes: &[u8],
    name: &str,
    version: &str,
    command: Option<&str>,
    policy: &CapabilityPolicy,
    workdir: &Path,
) -> crate::harness_needs::HarnessReport {
    let driver_ref = format!("{name}@{version}");
    let unresolved = |reason: String| crate::harness_needs::unresolved_report(&driver_ref, reason);
    let wasm = match crate::artifact::extract_root_wasm(name, version, driver_bytes) {
        Ok(wasm) => wasm,
        Err(error) => return unresolved(error.to_string()),
    };
    let contracts = murmur_artifact::extract_wit_contracts(&wasm).ok().flatten();
    if let Err(error) = check_driver_interface(
        "process",
        name,
        version,
        contracts.as_ref(),
        crate::served_wit_level(),
    ) {
        return unresolved(error.to_string());
    }
    let standalone = match StandaloneDriver::compile_named(&wasm, name, version) {
        Ok(standalone) => standalone,
        Err(error) => return unresolved(error.to_string()),
    };
    standalone.rt.block_on(async {
        let description = match async {
            let mut driver = standalone.instantiate().await?;
            driver.describe().await
        }
        .await
        {
            Ok(description) => description,
            Err(error) => return unresolved(error.to_string()),
        };
        let binary_info = crate::runtime::resolve_harness_binary(command, &description.binary)
            .ok()
            .map(|(path, _)| crate::harness_needs::HarnessBinaryInfo {
                interpreter: crate::harness_needs::resolve_interpreter(&path, &harness_path_env()),
                path,
            });
        let context_home = HarnessContextHome::read(workdir);
        crate::harness_needs::resolve_harness_grants(
            &driver_ref,
            &description.harness_needs,
            &policy.harness_paths,
            &policy.network_allow,
            &description.required_env,
            &policy.env_allow,
            binary_info.as_ref(),
            &context_home.context(),
        )
        .report
    })
}

/// What [`check_process_harness`] found about a process capsule's harness.
#[derive(Debug)]
pub struct HarnessCheck {
    /// The harness the driver drives, as its `describe()` names it.
    pub harness: String,
    /// Where the harness binary resolved to, or the refusal staging would make.
    pub binary: HarnessBinary,
    /// What the version probe read; `None` when the binary did not resolve.
    pub version: Option<HarnessVersion>,
    /// What the driver made of the login check; `None` when the binary did not resolve.
    pub login: Option<HarnessLogin>,
}

/// Where the harness binary resolved to.
#[derive(Debug)]
pub enum HarnessBinary {
    /// The absolute path `mur run` would spawn, and the field that named it: `inference.command`
    /// or `the process driver's describe()`.
    Resolved { path: PathBuf, source: String },
    /// [`RuntimeError::HarnessBinaryNotFound`], as staging builds it (`E-RUN-006`).
    NotFound(RuntimeError),
}

/// What the version probe read, judged against the driver's `tested-versions`.
#[derive(Debug, PartialEq, Eq)]
pub enum HarnessVersion {
    /// The line the harness printed names a tested version.
    Tested { line: String },
    /// The version is untested or could not be read: the `W-RUN-002` body `mur run` prints.
    Untested { message: String },
}

/// Whether the harness is logged in, as the driver reads its login check.
#[derive(Debug, PartialEq, Eq)]
pub enum HarnessLogin {
    LoggedIn,
    /// The driver's `logged-out` reason, cut to its first line and [`LOGIN_REASON_MAX_CHARS`].
    LoggedOut {
        reason: String,
    },
    /// Nothing could tell: the driver's `unavailable` or `unknown` reason, the probe's reason
    /// when the login command could not be run or did not answer, or the driver's failure in
    /// `read-login`. Cut like [`HarnessLogin::LoggedOut`]'s.
    NotChecked {
        reason: String,
    },
}

/// Checks the process driver in `driver_bytes` (an artifact's `.mur.zip` bytes) and the harness it
/// drives on this host.
///
/// Errs only where staging would refuse before resolving the binary: the artifact holds no root
/// wasm, the driver does not export the process interface at the served WIT level, it cannot be
/// compiled, loaded or described (`E-RUN-032`), or a variable it requires is not in
/// `capabilities.env.allow`. A binary that does not resolve is a [`HarnessBinary::NotFound`] in
/// the answer, with no version or login probe run. Otherwise the binary is run twice, the way the
/// version probe of a run runs it: with the driver's `version-args`, then, when the driver names a
/// `login-check` command, with those arguments, each with the host values of the
/// `capabilities.env.allow` names and the host's `USER` and `LOGNAME`, empty stdin, `cwd`, and the
/// same 10-second bound. What the login command printed goes to the driver and nowhere else.
///
/// `name` and `version` name the driver in errors; `command` is the manifest's
/// `inference.command`. It blocks on its own current-thread runtime, so it must not be called from
/// inside one.
pub fn check_process_harness(
    driver_bytes: &[u8],
    name: &str,
    version: &str,
    command: Option<&str>,
    policy: &CapabilityPolicy,
    cwd: &Path,
) -> Result<HarnessCheck, RuntimeError> {
    let wasm = crate::artifact::extract_root_wasm(name, version, driver_bytes)?;
    check_process_harness_wasm(
        &wasm,
        name,
        version,
        command,
        &harness_base_env(policy, &HostAccountEnv::from_host()),
        policy,
        cwd,
    )
}

/// [`check_process_harness`] on the driver's component bytes, with the probe environment given.
fn check_process_harness_wasm(
    wasm: &[u8],
    name: &str,
    version: &str,
    command: Option<&str>,
    probe_env: &BTreeMap<String, String>,
    policy: &CapabilityPolicy,
    cwd: &Path,
) -> Result<HarnessCheck, RuntimeError> {
    let contracts = murmur_artifact::extract_wit_contracts(wasm).ok().flatten();
    check_driver_interface(
        "process",
        name,
        version,
        contracts.as_ref(),
        crate::served_wit_level(),
    )?;
    let standalone = StandaloneDriver::compile_named(wasm, name, version)?;
    standalone.rt.block_on(async {
        let mut driver = standalone.instantiate().await?;
        let description = driver.describe().await?;
        check_required_env(name, version, &description, &policy.env_allow)?;
        let harness = description.harness.clone();
        let resolved = crate::runtime::resolve_harness_binary(command, &description.binary);
        // The harness's declared needs against the manifest, right after `check_required_env`, as
        // staging checks them — so `mur doctor` reports a disallowed need with `E-CAP-021` before
        // any probe runs. Resolved whether or not the binary did, since a need refusal does not
        // depend on the binary.
        let binary_info =
            resolved
                .as_ref()
                .ok()
                .map(|(path, _)| crate::harness_needs::HarnessBinaryInfo {
                    path: path.clone(),
                    interpreter: crate::harness_needs::resolve_interpreter(
                        path,
                        &harness_path_env(),
                    ),
                });
        let context_home = HarnessContextHome::read(cwd);
        let grants = crate::harness_needs::resolve_harness_grants(
            &format!("{name}@{version}"),
            &description.harness_needs,
            &policy.harness_paths,
            &policy.network_allow,
            &description.required_env,
            &policy.env_allow,
            binary_info.as_ref(),
            &context_home.context(),
        );
        if let Some(refusal) = grants.refusal {
            return Err(refusal);
        }
        let (path, source) = match resolved {
            Ok(resolved) => resolved,
            Err(error) => {
                return Ok(HarnessCheck {
                    harness,
                    binary: HarnessBinary::NotFound(error),
                    version: None,
                    login: None,
                })
            }
        };

        let probe =
            probe_harness_version(&path, &description.version_args, probe_env, cwd, None).await;
        let version = match &probe {
            VersionProbe::Read(line) if version_is_tested(line, &description.tested_versions) => {
                HarnessVersion::Tested { line: line.clone() }
            }
            _ => HarnessVersion::Untested {
                message: untested_version_message(&probe, &description.tested_versions),
            },
        };
        let login = check_login(&mut driver, &description.login_check, &path, probe_env, cwd).await;
        Ok(HarnessCheck {
            harness,
            binary: HarnessBinary::Resolved { path, source },
            version: Some(version),
            login: Some(login),
        })
    })
}

/// Runs the driver's login check against the resolved `binary` and has the driver read it.
///
/// `describe()` refused a `command` with no arguments at load, so the bare binary, which may start
/// a session, is never run here.
async fn check_login(
    driver: &mut ProcessDriver,
    login_check: &LoginCheck,
    binary: &Path,
    probe_env: &BTreeMap<String, String>,
    cwd: &Path,
) -> HarnessLogin {
    let args = match login_check {
        LoginCheck::Unavailable(reason) => {
            return HarnessLogin::NotChecked {
                reason: login_reason(reason),
            }
        }
        LoginCheck::Command(args) => args,
    };
    let output = match run_harness_probe(binary, args, probe_env, cwd, None).await {
        Ok(output) => output,
        Err(reason) => {
            return HarnessLogin::NotChecked {
                reason: login_reason(&reason),
            }
        }
    };
    let output = LoginOutput {
        exit_code: output.exit_code,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    };
    match driver.read_login(output).await {
        Ok(LoginState::LoggedIn) => HarnessLogin::LoggedIn,
        Ok(LoginState::LoggedOut(reason)) => HarnessLogin::LoggedOut {
            reason: login_reason(&reason),
        },
        Ok(LoginState::Unknown(reason)) => HarnessLogin::NotChecked {
            reason: login_reason(&reason),
        },
        Err(error) => HarnessLogin::NotChecked {
            reason: login_reason(&error.to_string()),
        },
    }
}

/// `reason`'s first line, trimmed and cut to [`LOGIN_REASON_MAX_CHARS`] characters: what a driver
/// says is shown on one line, and a driver that echoed its harness's output into a reason shows at
/// most the start of it.
fn login_reason(reason: &str) -> String {
    let first_line = reason.lines().next().unwrap_or_default().trim();
    first_line.chars().take(LOGIN_REASON_MAX_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use tempfile::TempDir;

    use super::*;

    /// The fixture process driver: `describe()` names `fixture-cli`, tested version `1.0.0`,
    /// required `HOME` and `FIXTURE_HARNESS_PROFILE`, and `login-check: command(["auth",
    /// "status"])`; `read-login` reads `loggedIn` out of a JSON object.
    const DRIVER: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../murmur-cli/tests/fixtures/process-driver/tool/process-driver.wasm"
    ));

    /// The same driver built with `no-login-check`: `login-check: unavailable(…)`.
    const NO_LOGIN_CHECK_DRIVER: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../murmur-cli/tests/fixtures/process-driver/tool/process-driver-no-login-check.wasm"
    ));

    fn policy() -> CapabilityPolicy {
        CapabilityPolicy {
            env_allow: vec!["HOME".to_string(), "FIXTURE_HARNESS_PROFILE".to_string()],
            ..CapabilityPolicy::default()
        }
    }

    /// A harness script at `<dir>/harness` running `body` under bash, mode 0755.
    fn harness(dir: &TempDir, body: &str) -> PathBuf {
        let path = dir.path().join("harness");
        std::fs::write(&path, format!("#!/bin/bash\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn check(
        wasm: &[u8],
        binary: &Path,
        env: &BTreeMap<String, String>,
        cwd: &Path,
    ) -> HarnessCheck {
        check_process_harness_wasm(
            wasm,
            "local/fixture-driver",
            "1.0.0",
            binary.to_str(),
            env,
            &policy(),
            cwd,
        )
        .unwrap()
    }

    /// Both probes run with exactly the environment `harness_base_env` builds: the script answers
    /// a tested version and a login only when it sees the account and the declared name, and not
    /// a variable this process holds but the policy does not declare.
    #[test]
    fn harness_check_probes_run_with_the_harness_base_env() {
        std::env::set_var("HOME", "/home/harness-check");
        std::env::set_var("FIXTURE_HARNESS_PROFILE", "declared");
        std::env::set_var("MURMUR_TEST_HARNESS_CHECK_UNDECLARED", "leak");
        let env = harness_base_env(
            &policy(),
            &HostAccountEnv::from_pairs(&[("USER", "alice"), ("LOGNAME", "alice-login")]),
        );
        let dir = TempDir::new().unwrap();
        let binary = harness(
            &dir,
            r#"ok=no
if [ "${USER:-}" = alice ] && [ "${LOGNAME:-}" = alice-login ] \
   && [ "${FIXTURE_HARNESS_PROFILE:-}" = declared ] && [ "${HOME:-}" = /home/harness-check ] \
   && [ -z "${MURMUR_TEST_HARNESS_CHECK_UNDECLARED:-}" ]; then ok=yes; fi
if [ "$1" = auth ] && [ "$2" = status ]; then
  if [ $ok = yes ]; then echo '{"loggedIn": true}'; else echo '{"loggedIn": false}'; fi
  exit 0
fi
if [ $ok = yes ]; then echo "fixture-harness 1.0.0"; else echo "fixture-harness 0.0.1"; fi"#,
        );

        let found = check(DRIVER, &binary, &env, dir.path());
        assert_eq!(found.harness, "fixture-harness");
        match &found.binary {
            HarnessBinary::Resolved { path, source } => {
                assert_eq!(path, &std::fs::canonicalize(&binary).unwrap());
                assert_eq!(source, "inference.command");
            }
            other => panic!("unexpected binary: {other:?}"),
        }
        assert_eq!(
            found.version,
            Some(HarnessVersion::Tested {
                line: "fixture-harness 1.0.0".to_string()
            })
        );
        assert_eq!(found.login, Some(HarnessLogin::LoggedIn));
    }

    /// A logged-out answer carries the driver's reason, and nothing the harness printed.
    #[test]
    fn harness_check_reports_the_drivers_logged_out_reason_only() {
        let dir = TempDir::new().unwrap();
        let binary = harness(
            &dir,
            r#"if [ "$1" = auth ]; then echo '{"loggedIn": false, "token": "secret-77"}'; exit 0; fi
echo "fixture-harness 1.0.0""#,
        );
        let found = check(DRIVER, &binary, &BTreeMap::new(), dir.path());
        let Some(HarnessLogin::LoggedOut { reason }) = &found.login else {
            panic!("unexpected login: {:?}", found.login);
        };
        assert_eq!(
            reason,
            "fixture-harness is not logged in for this user; run fixture-cli auth login"
        );
        assert!(!format!("{found:?}").contains("secret-77"));
    }

    /// A driver whose harness cannot be asked says why, and an answer the driver cannot read is
    /// not a login.
    #[test]
    fn harness_check_cannot_tell_without_a_login_command_or_a_readable_answer() {
        let dir = TempDir::new().unwrap();
        let binary = harness(
            &dir,
            r#"if [ "$1" = auth ]; then echo 'not json'; exit 0; fi
echo "fixture-harness 1.0.0""#,
        );
        let unavailable = check(NO_LOGIN_CHECK_DRIVER, &binary, &BTreeMap::new(), dir.path());
        assert_eq!(
            unavailable.login,
            Some(HarnessLogin::NotChecked {
                reason: "fixture-harness has no way to report its login without a turn".to_string()
            })
        );
        let unreadable = check(DRIVER, &binary, &BTreeMap::new(), dir.path());
        assert_eq!(
            unreadable.login,
            Some(HarnessLogin::NotChecked {
                reason: "fixture-harness printed no loggedIn field".to_string()
            })
        );
    }

    /// A harness that hangs is given up on after the same bound, for the same reason, whichever
    /// probe asked.
    #[test]
    fn harness_check_gives_the_same_timeout_reason_for_login_and_version() {
        let dir = TempDir::new().unwrap();
        let binary = harness(&dir, "exec sleep 30");
        let found = check(DRIVER, &binary, &BTreeMap::new(), dir.path());
        let reason = "it did not answer within 10s";
        assert_eq!(
            found.login,
            Some(HarnessLogin::NotChecked {
                reason: reason.to_string()
            })
        );
        let Some(HarnessVersion::Untested { message }) = &found.version else {
            panic!("unexpected version: {:?}", found.version);
        };
        assert_eq!(
            message,
            &format!(
                "the harness version could not be read: {reason}; this process driver is tested \
                 against 1.0.0"
            )
        );
    }

    /// An untested version is the `W-RUN-002` body a run prints.
    #[test]
    fn harness_check_reports_an_untested_version_in_the_runs_words() {
        let dir = TempDir::new().unwrap();
        let binary = harness(
            &dir,
            r#"if [ "$1" = auth ]; then echo '{"loggedIn": true}'; exit 0; fi
echo "fixture-harness 0.9.0""#,
        );
        let found = check(DRIVER, &binary, &BTreeMap::new(), dir.path());
        assert_eq!(
            found.version,
            Some(HarnessVersion::Untested {
                message: untested_version_message(
                    &VersionProbe::Read("fixture-harness 0.9.0".to_string()),
                    &["1.0.0".to_string()]
                )
            })
        );
    }

    /// A binary that does not resolve is staging's refusal, and neither probe runs.
    #[test]
    fn harness_check_runs_no_probe_for_a_binary_that_does_not_resolve() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("no-such-harness");
        let found = check(DRIVER, &missing, &BTreeMap::new(), dir.path());
        match &found.binary {
            HarnessBinary::NotFound(RuntimeError::HarnessBinaryNotFound {
                binary,
                binary_source,
            }) => {
                assert_eq!(binary, missing.to_str().unwrap());
                assert_eq!(binary_source, "inference.command");
            }
            other => panic!("unexpected binary: {other:?}"),
        }
        assert!(found.version.is_none());
        assert!(found.login.is_none());
    }

    /// A required variable the policy does not declare is staging's `E-CAP-019` refusal.
    #[test]
    fn harness_check_refuses_an_undeclared_required_variable() {
        let dir = TempDir::new().unwrap();
        let err = check_process_harness_wasm(
            DRIVER,
            "local/fixture-driver",
            "1.0.0",
            Some("/bin/true"),
            &BTreeMap::new(),
            &CapabilityPolicy::default(),
            dir.path(),
        )
        .unwrap_err();
        assert!(
            matches!(err, RuntimeError::ProcessDriverRequiredEnvNotAllowed { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn harness_check_login_reason_is_one_line_of_at_most_200_chars() {
        assert_eq!(login_reason("  first line  \nsecond line"), "first line");
        let long = "é".repeat(300);
        let cut = login_reason(&long);
        assert_eq!(cut.chars().count(), LOGIN_REASON_MAX_CHARS);
        assert_eq!(login_reason(""), "");
    }
}
