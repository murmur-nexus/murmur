//! Whether a launch runs contained or unconfined, and at what floor: the one resolution `mur run`,
//! `mur run --explain-scope`, `mur run --roster` and `mur doctor` share.
//!
//! Only the operator chooses the unconfined stance, with `--unconfined` or `containment:
//! unconfined` in `~/.murmur/config.yaml`. A declared floor is never lowered, so either choice
//! meeting a floor from any source is refused, naming both. A launch the runtime started on the
//! operator's behalf (`--contained-launch`, `--spawn-grant-stdin`) is contained whatever the
//! global file says, and refuses `--unconfined`.
//!
//! The unconfined stance is refused when `mur`'s effective uid is 0, whichever source chose it: an
//! unconfined capsule's commands run as the launching user, so as root they would run as root on
//! the host with no boundary at all. Nothing overrides that refusal. A contained stance runs at
//! any uid and never consults it.

use murmur_artifact::{effective_containment_floor, ContainmentClass};

use crate::error::{CliError, E_IO_003, E_RUN_059, E_RUN_060, E_RUN_063};

/// Where the global file is named in a refusal.
const GLOBAL_CONFIG: &str = "~/.murmur/config.yaml";
/// Where the project file is named in a refusal.
const PROJECT_CONFIG: &str = ".murmur/config.yaml";
/// Where the manifest's floor is named in a refusal.
const MANIFEST: &str = "the manifest's capabilities.containment";

/// `mur run`'s two stance flags, as clap parsed them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct StanceFlags {
    /// `--unconfined`.
    pub(crate) unconfined: bool,
    /// The hidden `--contained-launch`.
    pub(crate) contained_launch: bool,
}

/// Everything the stance is resolved from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct StanceInputs {
    /// `--unconfined`.
    pub(crate) unconfined_flag: bool,
    /// `--containment`, already parsed: never `unconfined`.
    pub(crate) containment_flag: Option<ContainmentClass>,
    /// The hidden `--contained-launch`.
    pub(crate) contained_launch: bool,
    /// `--spawn-grant-stdin`.
    pub(crate) spawn_grant_stdin: bool,
    /// `containment` in the global file, `unconfined` included.
    pub(crate) global: Option<ContainmentClass>,
    /// `containment` in the project file, never `unconfined` (refused at load).
    pub(crate) project: Option<ContainmentClass>,
    /// The manifest's `capabilities.containment`, never `unconfined` (refused at parse).
    pub(crate) manifest: Option<ContainmentClass>,
    /// The effective uid the unconfined stance is checked against. `None` reads this process's
    /// own with `geteuid`; only tests pass `Some`.
    pub(crate) effective_uid: Option<u32>,
}

/// Who chose the unconfined stance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnconfinedSource {
    /// `mur run --unconfined`.
    Flag,
    /// `containment: unconfined` in `~/.murmur/config.yaml`.
    GlobalConfig,
}

impl UnconfinedSource {
    /// How a refusal or a report names the choice.
    pub(crate) fn describe(self) -> &'static str {
        match self {
            Self::Flag => "--unconfined",
            Self::GlobalConfig => "containment: unconfined in ~/.murmur/config.yaml",
        }
    }

    /// How a refusal tells the operator to run this launch contained instead.
    fn run_contained(self) -> &'static str {
        match self {
            Self::Flag => "run without --unconfined to run contained",
            Self::GlobalConfig => "pass --containment <class> to run this launch contained",
        }
    }
}

/// What a launch runs under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stance {
    /// No containment, chosen by the operator.
    Unconfined(UnconfinedSource),
    /// Contained, at this declared floor.
    Contained(ContainmentClass),
}

impl Stance {
    /// The class `StageRequest::declared_containment_floor` carries: `unconfined` for the
    /// unconfined stance, the floor otherwise.
    pub(crate) fn declared_floor(self) -> ContainmentClass {
        match self {
            Self::Unconfined(_) => ContainmentClass::Unconfined,
            Self::Contained(floor) => floor,
        }
    }

    pub(crate) fn is_unconfined(self) -> bool {
        matches!(self, Self::Unconfined(_))
    }
}

/// Parses a `--containment` value: a declarable class, or `E-IO-003` naming the accepted set, or
/// naming `--unconfined` for `unconfined`, which is the operator's stance and not a floor.
pub(crate) fn parse_containment_value(value: &str) -> Result<ContainmentClass, CliError> {
    let class = value
        .parse::<ContainmentClass>()
        .map_err(|error| CliError::new(E_IO_003, format!("--containment {error}")))?;
    if class == ContainmentClass::Unconfined {
        return Err(CliError::new(
            E_IO_003,
            "--containment names a floor and unconfined is not one; run the capsule unconfined \
             with --unconfined",
        ));
    }
    Ok(class)
}

/// `E-RUN-060` when `--unconfined` is given to a launch the runtime started contained. Decided
/// from the flags alone, so `mur run` asks it before reading anything.
pub(crate) fn refuse_unconfined_contained_launch(
    unconfined_flag: bool,
    contained_launch: bool,
    spawn_grant_stdin: bool,
) -> Result<(), CliError> {
    if !unconfined_flag || !(contained_launch || spawn_grant_stdin) {
        return Ok(());
    }
    let launch = if contained_launch {
        "a launch the runtime started on the operator's behalf (--contained-launch)"
    } else {
        "a launch redeeming a spawn approval (--spawn-grant-stdin)"
    };
    Err(CliError::with_hint(
        E_RUN_060,
        format!("--unconfined cannot be given to {launch}, which always runs contained"),
        "a delegated child, a plan capsule step, a contained roster member, a deployed capsule \
         and a spawn-approval redemption run contained; only a capsule the operator launches \
         with --unconfined themselves runs unconfined",
    ))
}

/// Resolves the stance, in order: `E-RUN-060` for `--unconfined` on a contained launch;
/// `--unconfined`; the global file's `unconfined` on a launch with no `--contained-launch`, no
/// `--spawn-grant-stdin` and no `--containment`; otherwise contained, at the strongest floor any
/// source declares, defaulting to `advisory`. Either unconfined choice is refused with `E-RUN-063`
/// when the effective uid is 0, and only then with `E-RUN-059` against a declared floor: any floor
/// from the global file, the project file or the manifest for `--unconfined`, a floor from the
/// project file or the manifest for the global file's choice. `--unconfined` with `--containment`
/// is a usage error clap raises before this runs.
pub(crate) fn resolve_stance(inputs: StanceInputs) -> Result<Stance, CliError> {
    refuse_unconfined_contained_launch(
        inputs.unconfined_flag,
        inputs.contained_launch,
        inputs.spawn_grant_stdin,
    )?;
    let global_floor = inputs
        .global
        .filter(|class| *class != ContainmentClass::Unconfined);
    if inputs.unconfined_flag {
        refuse_unconfined_as_root(
            UnconfinedSource::Flag,
            inputs.effective_uid.unwrap_or_else(process_effective_uid),
        )?;
        let floors = [
            (global_floor, GLOBAL_CONFIG),
            (inputs.project, PROJECT_CONFIG),
            (inputs.manifest, MANIFEST),
        ];
        return unconfined_unless_floored(UnconfinedSource::Flag, &floors);
    }
    let global_unconfined = inputs.global == Some(ContainmentClass::Unconfined)
        && !inputs.contained_launch
        && !inputs.spawn_grant_stdin
        && inputs.containment_flag.is_none();
    if global_unconfined {
        refuse_unconfined_as_root(
            UnconfinedSource::GlobalConfig,
            inputs.effective_uid.unwrap_or_else(process_effective_uid),
        )?;
        let floors = [
            (inputs.project, PROJECT_CONFIG),
            (inputs.manifest, MANIFEST),
        ];
        return unconfined_unless_floored(UnconfinedSource::GlobalConfig, &floors);
    }
    Ok(Stance::Contained(effective_containment_floor(
        crate::config::merge_containment(inputs.global, inputs.project),
        inputs.manifest,
        inputs.containment_flag,
    )))
}

/// This process's effective uid.
#[allow(unsafe_code)]
fn process_effective_uid() -> u32 {
    // SAFETY: geteuid takes no arguments, touches no memory of the caller and cannot fail.
    unsafe { libc::geteuid() }
}

/// `E-RUN-063` when the unconfined stance chosen by `source` would run with effective uid 0.
fn refuse_unconfined_as_root(source: UnconfinedSource, effective_uid: u32) -> Result<(), CliError> {
    if effective_uid != 0 {
        return Ok(());
    }
    Err(CliError::with_hint(
        E_RUN_063,
        format!(
            "{} cannot run as root: mur's effective uid is 0, so the capsule's commands would run \
             as root on this host with no boundary at all",
            source.describe()
        ),
        format!(
            "run mur as a normal user (on a machine whose only user is root, add one), or {}",
            source.run_contained()
        ),
    ))
}

/// The unconfined stance chosen by `source`, or `E-RUN-059` naming it and every floor present in
/// `floors` with its source.
fn unconfined_unless_floored(
    source: UnconfinedSource,
    floors: &[(Option<ContainmentClass>, &str)],
) -> Result<Stance, CliError> {
    let declared: Vec<String> = floors
        .iter()
        .filter_map(|(class, from)| class.map(|class| format!("{class} in {from}")))
        .collect();
    if declared.is_empty() {
        return Ok(Stance::Unconfined(source));
    }
    Err(CliError::with_hint(
        E_RUN_059,
        format!(
            "{} cannot run this capsule unconfined: a containment floor is declared ({}), and a \
             declared floor is never lowered",
            source.describe(),
            declared.join("; ")
        ),
        format!(
            "remove the declared floor to run unconfined, or {}",
            source.run_contained()
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ContainmentClass::{Advisory, Scoped, Sealed, Unconfined};

    /// An effective uid other than 0, for the tests where the unconfined stance must stand.
    const NORMAL_USER: u32 = 1000;

    fn refused(inputs: StanceInputs) -> CliError {
        resolve_stance(inputs).expect_err("this stance is refused")
    }

    /// `--unconfined` with `--containment` is a clap usage error.
    #[test]
    fn stance_unconfined_with_containment_is_a_usage_error() {
        use clap::Parser;
        let error =
            crate::Cli::try_parse_from(["mur", "run", "--unconfined", "--containment", "scoped"])
                .expect_err("the two flags conflict");
        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
        assert!(error.to_string().contains("--unconfined"), "{error}");
    }

    /// `--containment unconfined` is refused, naming `--unconfined`; an unknown name keeps the
    /// declarable list.
    #[test]
    fn stance_containment_flag_refuses_unconfined() {
        let error = parse_containment_value("unconfined").unwrap_err();
        assert_eq!(error.code, E_IO_003);
        assert!(error.message.contains("--unconfined"), "{error}");
        assert_eq!(
            parse_containment_value("paranoid").unwrap_err().message,
            "--containment must be one of: advisory, scoped, sealed; got 'paranoid'"
        );
        assert_eq!(parse_containment_value("scoped").unwrap(), Scoped);
    }

    /// The hidden flag clap accepts is the one the runtime passes.
    #[test]
    fn stance_contained_launch_flag_is_the_runtimes() {
        use clap::Parser;
        crate::Cli::try_parse_from([
            "mur",
            "run",
            capsule_runtime::child_launch::CONTAINED_LAUNCH_FLAG,
        ])
        .expect("mur run accepts the contained-launch flag");
    }

    /// `mur deploy` has no `--unconfined`: a deployed capsule is always contained.
    #[cfg(feature = "beta-mur-deploy")]
    #[test]
    fn stance_deploy_has_no_unconfined_flag() {
        use clap::Parser;
        let error = crate::Cli::try_parse_from([
            "mur",
            "deploy",
            "run",
            "--host",
            "203.0.113.9",
            "--unconfined",
        ])
        .expect_err("mur deploy run takes no --unconfined");
        assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
        assert!(error.to_string().contains("--unconfined"), "{error}");
    }

    /// `--unconfined` on a launch the runtime started contained is `E-RUN-060`.
    #[test]
    fn stance_unconfined_on_a_contained_launch_is_e_run_060() {
        for (contained_launch, spawn_grant_stdin, named) in [
            (true, false, "--contained-launch"),
            (false, true, "--spawn-grant-stdin"),
        ] {
            let error = refused(StanceInputs {
                unconfined_flag: true,
                contained_launch,
                spawn_grant_stdin,
                ..StanceInputs::default()
            });
            assert_eq!(error.code, E_RUN_060);
            assert!(error.message.contains("--unconfined"), "{error}");
            assert!(error.message.contains(named), "{error}");
        }
    }

    /// `--unconfined` alone is unconfined; against any floor it is `E-RUN-059` naming the
    /// flag and every floor with its source.
    #[test]
    fn stance_unconfined_flag_runs_unconfined_unless_a_floor_is_declared() {
        assert_eq!(
            resolve_stance(StanceInputs {
                unconfined_flag: true,
                effective_uid: Some(NORMAL_USER),
                ..StanceInputs::default()
            })
            .unwrap(),
            Stance::Unconfined(UnconfinedSource::Flag)
        );
        // The global file's own `unconfined` agrees with the flag rather than flooring it.
        assert_eq!(
            resolve_stance(StanceInputs {
                unconfined_flag: true,
                global: Some(Unconfined),
                effective_uid: Some(NORMAL_USER),
                ..StanceInputs::default()
            })
            .unwrap(),
            Stance::Unconfined(UnconfinedSource::Flag)
        );
        let error = refused(StanceInputs {
            unconfined_flag: true,
            global: Some(Advisory),
            project: Some(Scoped),
            manifest: Some(Sealed),
            effective_uid: Some(NORMAL_USER),
            ..StanceInputs::default()
        });
        assert_eq!(error.code, E_RUN_059);
        for named in [
            "--unconfined",
            "advisory in ~/.murmur/config.yaml",
            "scoped in .murmur/config.yaml",
            "sealed in the manifest's capabilities.containment",
        ] {
            assert!(error.message.contains(named), "{named}: {error}");
        }
        for (inputs, named) in [
            (
                StanceInputs {
                    manifest: Some(Advisory),
                    ..StanceInputs::default()
                },
                "advisory in the manifest",
            ),
            (
                StanceInputs {
                    project: Some(Scoped),
                    ..StanceInputs::default()
                },
                "scoped in .murmur/config.yaml",
            ),
            (
                StanceInputs {
                    global: Some(Scoped),
                    ..StanceInputs::default()
                },
                "scoped in ~/.murmur/config.yaml",
            ),
        ] {
            let error = refused(StanceInputs {
                unconfined_flag: true,
                effective_uid: Some(NORMAL_USER),
                ..inputs
            });
            assert_eq!(error.code, E_RUN_059);
            assert!(error.message.contains(named), "{named}: {error}");
        }
    }

    /// The global file's `unconfined` is unconfined on an operator launch; against a floor
    /// from the project file or the manifest it is `E-RUN-059` naming the global file.
    #[test]
    fn stance_global_unconfined_runs_unconfined_unless_a_floor_is_declared() {
        assert_eq!(
            resolve_stance(StanceInputs {
                global: Some(Unconfined),
                effective_uid: Some(NORMAL_USER),
                ..StanceInputs::default()
            })
            .unwrap(),
            Stance::Unconfined(UnconfinedSource::GlobalConfig)
        );
        let error = refused(StanceInputs {
            global: Some(Unconfined),
            manifest: Some(Sealed),
            project: Some(Advisory),
            effective_uid: Some(NORMAL_USER),
            ..StanceInputs::default()
        });
        assert_eq!(error.code, E_RUN_059);
        for named in [
            "containment: unconfined in ~/.murmur/config.yaml",
            "sealed in the manifest's capabilities.containment",
            "advisory in .murmur/config.yaml",
        ] {
            assert!(error.message.contains(named), "{named}: {error}");
        }
        assert!(error.hint.unwrap().contains("--containment"));
    }

    /// Otherwise contained, at the strongest floor, defaulting to `advisory`;
    /// `--containment` overrides the global `unconfined` for one run, and a contained launch makes
    /// it inert.
    #[test]
    fn stance_otherwise_runs_contained_at_the_strongest_floor() {
        assert_eq!(
            resolve_stance(StanceInputs::default()).unwrap(),
            Stance::Contained(Advisory)
        );
        assert_eq!(
            resolve_stance(StanceInputs {
                global: Some(Scoped),
                project: Some(Advisory),
                manifest: Some(Advisory),
                ..StanceInputs::default()
            })
            .unwrap(),
            Stance::Contained(Scoped)
        );
        assert_eq!(
            resolve_stance(StanceInputs {
                global: Some(Unconfined),
                containment_flag: Some(Advisory),
                manifest: Some(Scoped),
                ..StanceInputs::default()
            })
            .unwrap(),
            Stance::Contained(Scoped)
        );
        for (contained_launch, spawn_grant_stdin) in [(true, false), (false, true)] {
            assert_eq!(
                resolve_stance(StanceInputs {
                    global: Some(Unconfined),
                    contained_launch,
                    spawn_grant_stdin,
                    ..StanceInputs::default()
                })
                .unwrap(),
                Stance::Contained(Advisory)
            );
        }
        assert_eq!(Stance::Contained(Sealed).declared_floor(), Sealed);
        assert_eq!(
            Stance::Unconfined(UnconfinedSource::Flag).declared_floor(),
            Unconfined
        );
    }

    /// As root, either unconfined choice is `E-RUN-063`, naming the choice, the rule and both
    /// fixes; it comes before `E-RUN-059`, and after `E-RUN-060`.
    #[test]
    fn stance_unconfined_as_root_is_e_run_063() {
        let error = refused(StanceInputs {
            unconfined_flag: true,
            effective_uid: Some(0),
            ..StanceInputs::default()
        });
        assert_eq!(error.code, E_RUN_063);
        for named in ["--unconfined", "root", "effective uid is 0"] {
            assert!(error.message.contains(named), "{named}: {error}");
        }
        let hint = error.hint.clone().unwrap();
        for named in ["normal user", "without --unconfined"] {
            assert!(hint.contains(named), "{named}: {hint}");
        }
        assert_eq!(
            error.to_string(),
            "error[E-RUN-063]: --unconfined cannot run as root: mur's effective uid is 0, so the \
             capsule's commands would run as root on this host with no boundary at all\n  hint: \
             run mur as a normal user (on a machine whose only user is root, add one), or run \
             without --unconfined to run contained"
        );

        let error = refused(StanceInputs {
            global: Some(Unconfined),
            effective_uid: Some(0),
            ..StanceInputs::default()
        });
        assert_eq!(error.code, E_RUN_063);
        assert!(
            error
                .message
                .starts_with("containment: unconfined in ~/.murmur/config.yaml cannot run as root"),
            "{error}"
        );
        assert!(
            error
                .hint
                .as_deref()
                .unwrap_or_default()
                .contains("--containment <class>"),
            "{error}"
        );

        let error = refused(StanceInputs {
            unconfined_flag: true,
            manifest: Some(Scoped),
            effective_uid: Some(0),
            ..StanceInputs::default()
        });
        assert_eq!(error.code, E_RUN_063, "{error}");
        let error = refused(StanceInputs {
            global: Some(Unconfined),
            project: Some(Sealed),
            effective_uid: Some(0),
            ..StanceInputs::default()
        });
        assert_eq!(error.code, E_RUN_063, "{error}");

        let error = refused(StanceInputs {
            unconfined_flag: true,
            contained_launch: true,
            effective_uid: Some(0),
            ..StanceInputs::default()
        });
        assert_eq!(error.code, E_RUN_060, "{error}");
    }

    /// As root, a contained stance resolves exactly as at any other uid.
    #[test]
    fn stance_contained_as_root_is_unchanged() {
        let root = |inputs: StanceInputs| {
            resolve_stance(StanceInputs {
                effective_uid: Some(0),
                ..inputs
            })
            .unwrap()
        };
        assert_eq!(root(StanceInputs::default()), Stance::Contained(Advisory));
        assert_eq!(
            root(StanceInputs {
                global: Some(Unconfined),
                containment_flag: Some(Scoped),
                ..StanceInputs::default()
            }),
            Stance::Contained(Scoped)
        );
        for (contained_launch, spawn_grant_stdin) in [(true, false), (false, true)] {
            assert_eq!(
                root(StanceInputs {
                    global: Some(Unconfined),
                    contained_launch,
                    spawn_grant_stdin,
                    ..StanceInputs::default()
                }),
                Stance::Contained(Advisory)
            );
        }
    }

    /// As a normal user, both unconfined choices stand; with no uid given, the process's own
    /// effective uid decides.
    #[test]
    fn stance_unconfined_as_a_normal_user_stands() {
        assert_eq!(
            resolve_stance(StanceInputs {
                unconfined_flag: true,
                effective_uid: Some(NORMAL_USER),
                ..StanceInputs::default()
            })
            .unwrap(),
            Stance::Unconfined(UnconfinedSource::Flag)
        );
        assert_eq!(
            resolve_stance(StanceInputs {
                global: Some(Unconfined),
                effective_uid: Some(NORMAL_USER),
                ..StanceInputs::default()
            })
            .unwrap(),
            Stance::Unconfined(UnconfinedSource::GlobalConfig)
        );
        let flag = StanceInputs {
            unconfined_flag: true,
            ..StanceInputs::default()
        };
        let own = StanceInputs {
            effective_uid: Some(process_effective_uid()),
            ..flag
        };
        assert_eq!(
            resolve_stance(flag).map_err(|error| error.code),
            resolve_stance(own).map_err(|error| error.code)
        );
    }
}
