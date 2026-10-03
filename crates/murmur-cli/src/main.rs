mod allocator;
mod beta;
mod commands;
mod config;
mod env_requirements;
mod error;
mod formation_trace;
mod live_address;
mod registry_client;
mod residue;
mod session_address;
mod source;

use std::path::PathBuf;

use capsule_runtime::ResumeMode;
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};

#[cfg(feature = "beta-mur-deploy")]
use commands::deploy::{run_deploy, DeployCommand};
#[cfg(feature = "beta-mur-deploy")]
use commands::deploy_ls::run_deploy_ls;
#[cfg(feature = "beta-mur-deploy")]
use commands::destroy::run_destroy;
#[cfg(feature = "beta-mur-new")]
use commands::new::run_new;
#[cfg(feature = "beta-mur-topology")]
use commands::topology::{run_topology, TopologyArgs};
use commands::{
    beta::{run_beta, BetaCommand},
    build::run_build,
    cancel::run_cancel,
    config_cmd::{run_config, ConfigCommand},
    control::{run_control, ControlCommand},
    conversation::{
        run_conversation_ls, run_conversation_rm, run_conversation_truncate, ConversationCommand,
    },
    doctor::run_doctor,
    eval::{run_eval_diff, run_eval_run, run_eval_show, EvalCommand},
    install::run_install,
    list::run_list,
    new_roster::run_new_roster,
    precompile::run_precompile,
    ps::run_ps,
    publish::run_publish,
    run::run_run,
    run_roster::{run_roster, RosterLaunch},
    search::run_search,
    stop::run_stop,
    trace::{run_trace_diff, run_trace_report, run_trace_show, run_trace_steps, TraceCommand},
    watch::run_watch,
};

/// `--resume-mode`'s value vocabulary. Separate from [`ResumeMode`] so the runtime enum carries
/// no clap derive and the CLI owns the spelling of its own values.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lower")]
enum ResumeModeArg {
    #[default]
    Full,
    Compact,
}

impl From<ResumeModeArg> for ResumeMode {
    fn from(arg: ResumeModeArg) -> Self {
        match arg {
            ResumeModeArg::Full => ResumeMode::Full,
            ResumeModeArg::Compact => ResumeMode::Compact,
        }
    }
}

#[derive(Debug, Parser)]
#[command(author, version, about)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

// Parsed once per process and matched once: `Run`'s flag set making the enum large costs nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Subcommand)]
enum Commands {
    /// List installed artifacts
    List {
        /// Show the global store (~/.murmur/artifacts/) instead of the project store
        #[arg(short = 'g', long)]
        global: bool,
        /// Show artifacts from both the project store and the global store, with a SCOPE column
        #[arg(long, conflicts_with = "global")]
        all: bool,
        /// Show only artifacts declaring a WIT interface whose name starts with this prefix,
        /// with a CONTRACTS column naming the matches (e.g. `murmur:hook`)
        #[arg(long, value_name = "PREFIX")]
        contract: Option<String>,
    },
    #[cfg(not(feature = "beta-mur-new"))]
    /// Scaffold a formation: ./<NAME>/ holding a roster.yaml and one capsule per member
    New {
        /// Write ./<NAME>/: roster.yaml, lead/murmur.yaml and worker/murmur.yaml. NAME is an
        /// artifact name, and prefixes each member's capsule name
        #[arg(long, value_name = "NAME")]
        roster: String,
    },
    #[cfg(feature = "beta-mur-new")]
    /// Scaffold a formation with --roster, or generate a murmur.yaml from a plain-language task
    /// description
    New {
        /// Write ./<NAME>/: roster.yaml, lead/murmur.yaml and worker/murmur.yaml. NAME is an
        /// artifact name, and prefixes each member's capsule name
        #[arg(
            long,
            value_name = "NAME",
            conflicts_with_all = ["task", "registry"],
            required_unless_present = "task"
        )]
        roster: Option<String>,

        /// Plain-language task description for the capsule to generate (beta: mur-new)
        task: Option<String>,

        /// Registry to search for artifacts: "local" scans ~/.murmur/artifacts/; a URL fetches
        /// that index. Defaults to the configured public index URL.
        #[arg(long, value_name = "URL|local")]
        registry: Option<String>,
    },
    /// Search the public artifact index for artifacts matching a keyword
    Search {
        /// Search query — matched case-insensitively against name, description, and tags
        query: String,

        /// Registry to search: "local" scans ~/.murmur/artifacts/; a URL fetches that index.
        /// Defaults to the configured public index URL (registry.index_url in ~/.murmur/config.yaml).
        #[arg(long, value_name = "URL|local")]
        registry: Option<String>,

        /// Maximum number of results to show
        #[arg(long, default_value = "10")]
        limit: usize,
    },
    /// Check that every artifact declared in murmur.yaml is present locally
    Doctor {
        /// The address `mur run` would bind the door on, for the public-door check
        /// (default: 127.0.0.1, as for `mur run`).
        #[arg(long, default_value = "127.0.0.1", value_name = "ADDR")]
        bind: String,
    },
    /// Build a .mur.zip artifact from a source directory
    Build {
        /// Source directory containing murmur.yaml (or input path/zip for --skill)
        #[arg(default_value = ".")]
        source: PathBuf,

        /// Output file path (or directory)
        #[arg(short, long)]
        output: Option<PathBuf>,

        /// Package an external skill (SKILL.md) into a .mur.zip artifact.
        /// Optional value sets the artifact name; omit to infer from the directory or filename.
        #[arg(long = "skill", value_name = "NAME", num_args = 0..=1)]
        skill: Option<Option<String>>,

        /// Artifact version for the generated manifest (used with --skill only; default: 0.1.0)
        #[arg(long, value_name = "VERSION")]
        version: Option<String>,

        /// One-line description written into the manifest (used with --skill only).
        /// Appears as the skill's summary in the tool inventory and mur list output.
        #[arg(long, value_name = "TEXT")]
        summary: Option<String>,
    },
    /// Publish an existing .mur.zip artifact to the configured registry
    Publish {
        /// Artifact path. If omitted, defaults to <name>-<version>.mur.zip in current directory.
        artifact_path: Option<PathBuf>,

        /// Remote registry base URL override (forces remote mode)
        #[arg(long)]
        registry: Option<String>,

        /// Platform tag for native artifacts (e.g. darwin-aarch64). Auto-detected when omitted for native artifacts.
        #[arg(long)]
        platform: Option<String>,
    },
    /// Install an artifact into the project store (.murmur/artifacts/) by default, or globally with -g.
    /// With no arguments, installs all manifest deps from murmur.yaml into the project store.
    Install {
        /// Artifact reference: name@version (registry), bare name (source chain),
        /// github:<owner>/<repo>@<tag>, or a local file path (./artifact.mur.zip).
        /// Omit to install all manifest deps from murmur.yaml.
        artifact: Option<String>,

        /// Remote registry base URL override (forces remote mode)
        #[arg(long)]
        registry: Option<String>,

        /// Install into the global store (~/.murmur/artifacts/) instead of the project store
        #[arg(short = 'g', long)]
        global: bool,

        /// Download all platform variants and install into the global store (CI / roost seeding).
        /// Requires a name@version artifact reference and a configured source chain.
        #[arg(long)]
        all_platforms: bool,

        /// Skip compiling installed WASM artifacts for this machine; they compile on first launch instead.
        #[arg(long)]
        no_precompile: bool,
    },
    /// Compile WASM artifacts (.mur.zip files) for this machine, so their first launch loads them instead of compiling
    Precompile {
        /// The .mur.zip files to compile
        #[arg(required = true, value_name = "ZIP")]
        files: Vec<PathBuf>,

        /// The --workdir of the `mur run` that will launch these artifacts. Nothing is stored when
        /// ~/.murmur is inside it, exactly as that launch would store and load nothing
        #[arg(long)]
        workdir: Option<PathBuf>,

        /// Print the report as one JSON object on stdout
        #[arg(long)]
        json: bool,
    },
    /// Run a capsule component with local lockfile-aware tool resolution
    Run {
        /// Path to murmur.yaml
        #[arg(long, default_value = "./murmur.yaml")]
        manifest: PathBuf,

        /// Run an installed registry artifact by name instead of a project directory.
        /// Requires --capsule-version. The capsule is resolved from the project store, then
        /// the global store, and staged from the artifact bytes in memory: --workdir is the
        /// only directory involved, and no murmur.yaml is read from disk.
        #[arg(
            long,
            value_name = "NAME",
            requires = "capsule_version",
            conflicts_with = "manifest"
        )]
        capsule: Option<String>,

        /// Version of the --capsule artifact to run. Required with --capsule.
        #[arg(long, value_name = "VERSION", requires = "capsule")]
        capsule_version: Option<String>,

        /// The sha256 the --capsule artifact's bytes must hash to. Set by `mur run --roster` on
        /// every member it starts, so a member runs the bytes roster admission validated.
        #[arg(long, value_name = "SHA256", requires = "capsule", hide = true)]
        capsule_sha256: Option<String>,

        /// Never run a task.md found in the workdir at launch; take work only at the door. Set by
        /// `mur run --roster` on every peer, which shares the roster's project directory with the
        /// entry member and its task.
        #[arg(long, hide = true, conflicts_with = "task")]
        ignore_task_file: bool,

        /// Launch the formation a roster declares, one task, then stop every member.
        /// Takes the roster's project directory or its roster.yaml; given with no value it means
        /// ./roster.yaml. Every member runs as its own `mur run` process: the peers first, each
        /// ready once its door answers, then the entry member with --task. --task, --json,
        /// --verbose, --no-env-file and --containment are passed through.
        #[arg(
            long,
            num_args = 0..=1,
            default_missing_value = "roster.yaml",
            value_name = "PATH",
            conflicts_with_all = [
                "manifest",
                "capsule",
                "capsule_version",
                "capsule_sha256",
                "ignore_task_file",
                "spawn_grant_stdin",
                "system_prompt",
                "context",
                "resume",
                "resume_mode",
                "forget_session",
                "lifecycle_task_acceptance",
                "lifecycle_after_task",
                "workdir",
                "bind",
                "explain_scope",
            ]
        )]
        roster: Option<PathBuf>,

        /// Read one line from standard input as this launch's spawn approval.
        /// Set by a parent capsule's runtime when it launches a delegated child; the approval is
        /// presented once when the session registers with mur-roost. Standard input is used
        /// rather than an argument or an environment variable, both of which a process running
        /// as the same user can read out of /proc.
        #[arg(long)]
        spawn_grant_stdin: bool,

        /// File path or inline text to write as task.md in the capsule workdir.
        /// If the value is the path to an existing file its contents are copied;
        /// otherwise the value itself is written as UTF-8 text.
        #[arg(long)]
        task: Option<String>,

        /// Replace the manifest's system prompt for this invocation only.
        /// Overrides inference.system_prompt, system_prompt_file and system_prompt_artifact
        /// alike; the value is trimmed, and an empty or whitespace-only value clears the
        /// prompt instead of setting one. murmur.yaml is not modified.
        /// Requires an agent capsule (a manifest with an inference: block).
        #[arg(long, value_name = "TEXT")]
        system_prompt: Option<String>,

        /// Context id for this run's task, making its conversation record reachable by name.
        /// Two runs given the same id share one record, and a hook granted
        /// capabilities.conversation.read reads what the earlier run left. Must be a single path
        /// segment. Defaults to a fresh id per task.
        #[arg(long, value_name = "ID")]
        context: Option<String>,

        /// Continue the conversation a previous session ran. Takes the same session address
        /// `mur trace diff` does: a full ses_ id, a 4+-character suffix, an @N ordinal
        /// (@1 = most recent), or a path to a session directory or its trace.jsonl.
        /// Given with no value, it means @1 — the session that just finished.
        /// Resolves that session's context id and runs under it, loading its conversation
        /// record even when the capsule declares lifecycle.conversation: stateless.
        /// Cannot be combined with --context, which names the same thing directly.
        #[arg(long, num_args = 0..=1, default_missing_value = "@1", value_name = "SESSION")]
        resume: Option<String>,

        /// How --resume puts the loaded conversation in front of the model
        /// (full|compact, default: full).
        /// full loads the record verbatim; compact runs the capsule's on-compaction hook over
        /// it first and continues from the summary, which is the answer when the conversation
        /// would not fit the context window at all.
        /// full is often the cheaper of the two: a verbatim reload can hit the provider's
        /// prompt cache, while compaction changes the prefix from the first altered token,
        /// guarantees a cache miss, and costs an extra inference call to produce the summary.
        #[arg(long, value_name = "MODE", requires = "resume")]
        resume_mode: Option<ResumeModeArg>,

        /// Drop the harness session --context names, then run this launch's first task as a new
        /// conversation under the same context id.
        /// The answer to E-RUN-036, where the harness no longer holds the conversation a context
        /// names and every later task in it fails the same way.
        /// The run's trace records what was dropped and that a person asked for it.
        /// Requires --context, and applies only to a capsule on inference.transport: process,
        /// which is the only transport whose harness owns the conversation.
        #[arg(long, requires = "context", conflicts_with = "resume")]
        forget_session: bool,

        /// Override manifest lifecycle.task_acceptance (none|single|queue)
        #[arg(long, value_name = "MODE")]
        lifecycle_task_acceptance: Option<String>,

        /// Override manifest lifecycle.after_task (exit|sleep)
        #[arg(long, value_name = "BEHAVIOR")]
        lifecycle_after_task: Option<String>,

        /// Mount a directory as the capsule's accessible workspace.
        /// The agent can read and write all files within this directory.
        /// Session artifacts (.murmur/) are created inside it.
        /// Defaults to a temporary directory if not specified.
        #[arg(long)]
        workdir: Option<std::path::PathBuf>,

        /// Emit launch info as a single JSON line instead of human-readable output.
        /// Output shape: {"url":"localhost:PORT","pid":N,"session_id":"uuid","name":"...","version":"...","workdir":"/path"}
        /// When both --json and --verbose are set, --json takes precedence and no human output is produced.
        #[arg(long)]
        json: bool,

        /// Print extended startup info: workdir, manifest identity, driver, and installed skills.
        /// Session ID is always shown at startup regardless of this flag.
        /// When both --json and --verbose are set, --json takes precedence and no human output is produced.
        #[arg(long, short = 'v')]
        verbose: bool,

        /// Address for the HTTP server to bind on (default: 127.0.0.1).
        /// Use 0.0.0.0 to make the capsule reachable from outside the machine.
        #[arg(long, default_value = "127.0.0.1", value_name = "ADDR")]
        bind: String,

        /// Skip auto-loading the workspace-root .env file for this invocation.
        /// Recommended default for CI/CD pipelines: inject secrets as scoped
        /// environment variables from a vault or secrets manager instead.
        #[arg(long)]
        no_env_file: bool,

        /// Require at least this containment class (advisory|scoped|sealed).
        /// Combined with capabilities.containment in murmur.yaml and containment in
        /// .murmur/config.yaml by taking the strongest — this flag can raise the floor
        /// but never lower one another source already set. `mur run` refuses to launch
        /// when the host's kernel cannot provide the resulting class.
        #[arg(long, value_name = "CLASS")]
        containment: Option<String>,

        /// Print the effective grant set and the declared/achieved containment classes,
        /// then exit 0 without staging or launching anything. Read-only: it reports even
        /// when the declared floor is not met, and never creates a workdir.
        #[arg(long)]
        explain_scope: bool,
    },
    /// Inspect and prune the durable conversation records under ~/.murmur/conversations/
    Conversation {
        #[command(subcommand)]
        command: ConversationCommand,
    },
    /// Analyze trace.jsonl files from past sessions
    Trace {
        #[command(subcommand)]
        command: TraceCommand,
    },
    /// Run structured evaluations against capsule sessions
    Eval {
        #[command(subcommand)]
        command: EvalCommand,
    },
    #[cfg(feature = "beta-mur-topology")]
    /// Render running capsule sessions as a topology graph from OTel trace data
    Topology(TopologyArgs),
    /// Watch a running capsule's output stream
    Watch {
        /// Running session to watch: @1, a ses_ id, or a 4-character suffix of one
        #[arg(value_name = "SESSION")]
        session: Option<String>,
        /// Capsule address to reach directly, e.g. localhost:12345
        #[arg(long, value_name = "HOST:PORT", conflicts_with = "session")]
        url: Option<String>,
    },
    /// Stop one running task on a capsule, leaving the session running
    Cancel {
        /// Running session holding the task: @1, a ses_ id, or a 4-character suffix of one.
        /// With --url, give the task id alone.
        #[arg(value_name = "SESSION")]
        session: Option<String>,
        /// Task to stop (e.g. tsk_0199...)
        #[arg(value_name = "TASK_ID")]
        task_id: Option<String>,
        /// Capsule address to reach directly, e.g. localhost:12345
        #[arg(long, value_name = "HOST:PORT")]
        url: Option<String>,
    },
    /// Change what a running capsule's control: block lets a controller change
    Control {
        #[command(subcommand)]
        command: ControlCommand,
    },
    /// List the capsules running on this machine
    Ps,
    /// Stop one running capsule, ending its session and everything it still holds
    Stop {
        /// Running session to stop: @1, a ses_ id, or a 4-character suffix of one
        #[arg(value_name = "SESSION")]
        session: String,
        /// Seconds to wait after SIGTERM before SIGKILL; 0 escalates immediately
        #[arg(long, default_value = "10", value_name = "SECONDS")]
        timeout: u64,
    },
    #[cfg(feature = "beta-mur-deploy")]
    /// Deploy capsules to VMs and list what is deployed
    Deploy {
        #[command(subcommand)]
        command: DeployCommand,
    },
    #[cfg(feature = "beta-mur-deploy")]
    /// Terminate a deployed capsule VM and remove it from the deployment list
    Destroy {
        /// Deployment ID returned by `mur deploy run`; a unique prefix is enough
        deployment_id: String,
    },
    /// Manage opt-in beta features
    Beta {
        #[command(subcommand)]
        command: BetaCommand,
    },
    /// Manage mur configuration (global ~/.murmur/config.yaml and project-level
    /// <cwd>/.murmur/config.yaml)
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
}

/// The capsule and the task `mur cancel` was given.
///
/// The positional list is `<SESSION> <TASK_ID>`, and `--url` stands in for the session address,
/// leaving `<TASK_ID>` alone. Any other shape is a usage error, reported by clap in the same words
/// as every other usage error.
fn cancel_arguments(
    session: Option<String>,
    task_id: Option<String>,
    url: Option<String>,
) -> Result<(live_address::Target, String), error::CliError> {
    match (url, session, task_id) {
        (Some(url), Some(task_id), None) => Ok((live_address::Target::Url(url), task_id)),
        (Some(_), _, _) => cancel_usage_error(
            "--url names the capsule, so the task id is the only positional: \
             mur cancel --url <HOST:PORT> <TASK_ID>",
        ),
        (None, Some(session), Some(task_id)) => {
            Ok((live_address::target(Some(&session), None)?, task_id))
        }
        (None, _, _) => cancel_usage_error(
            "mur cancel names a running session and the task to stop: mur cancel <SESSION> <TASK_ID>",
        ),
    }
}

/// Renders `message` off the `cancel` subcommand so the usage line under it is `mur cancel`'s own.
/// A freshly built [`Cli::command`] has no `bin_name`, and would print the crate name instead.
fn cancel_usage_error(message: &str) -> ! {
    let mut root = Cli::command().bin_name("mur");
    root.build();
    root.find_subcommand_mut("cancel")
        .expect("cancel is a subcommand")
        .clone()
        .error(clap::error::ErrorKind::MissingRequiredArgument, message)
        .exit()
}

fn main() {
    if let Err(e) = capsule_runtime::security::harden_process_dumpable() {
        eprintln!("mur: warning: failed to harden process against /proc environ reads: {e}");
    }

    #[cfg(any(
        feature = "beta-mur-new",
        feature = "beta-mur-deploy",
        feature = "beta-mur-topology"
    ))]
    let beta_config = config::load_effective_mur_config().unwrap_or_default().beta;

    #[cfg(any(
        feature = "beta-mur-new",
        feature = "beta-mur-deploy",
        feature = "beta-mur-topology"
    ))]
    let mut cmd = Cli::command();
    #[cfg(not(any(
        feature = "beta-mur-new",
        feature = "beta-mur-deploy",
        feature = "beta-mur-topology"
    )))]
    let cmd = Cli::command();

    // `mur new --roster` is in every build; only the task form is beta.
    #[cfg(feature = "beta-mur-new")]
    if !beta_config.is_enabled("mur-new") {
        cmd = cmd.mut_subcommand("new", |sc| {
            sc.mut_arg("task", |arg| arg.hide(true))
                .mut_arg("registry", |arg| arg.hide(true))
        });
    }
    #[cfg(feature = "beta-mur-deploy")]
    if !beta_config.is_enabled("mur-deploy") {
        cmd = cmd.mut_subcommand("deploy", |sc| sc.hide(true));
        cmd = cmd.mut_subcommand("destroy", |sc| sc.hide(true));
    }
    #[cfg(feature = "beta-mur-topology")]
    if !beta_config.is_enabled("mur-topology") {
        cmd = cmd.mut_subcommand("topology", |sc| sc.hide(true));
    }

    let matches = cmd.get_matches();
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());

    let result = match cli.command {
        #[cfg(not(feature = "beta-mur-new"))]
        Commands::New { roster } => run_new_roster(&roster),
        #[cfg(feature = "beta-mur-new")]
        Commands::New {
            roster: Some(roster),
            ..
        } => run_new_roster(&roster),
        #[cfg(feature = "beta-mur-new")]
        Commands::New {
            roster: None,
            task,
            registry,
        } => {
            if !beta_config.is_enabled("mur-new") {
                eprintln!(
                    "error: `mur new <task>` is a beta feature, and it is not enabled\n  \
                     hint: run `mur beta enable mur-new` to enable it; \
                     `mur new --roster <NAME>` scaffolds a formation without it"
                );
                std::process::exit(1);
            }
            // clap requires --roster or <task>, so a task is present here.
            run_new(&task.unwrap_or_default(), registry.as_deref())
        }
        Commands::List {
            global,
            all,
            contract,
        } => run_list(global, all, contract.as_deref()),
        Commands::Search {
            query,
            registry,
            limit,
        } => run_search(&query, registry.as_deref(), limit),
        Commands::Doctor { bind } => run_doctor(&bind),
        Commands::Build {
            source,
            output,
            skill,
            version,
            summary,
        } => run_build(
            &source,
            output.as_deref(),
            skill,
            version.as_deref(),
            summary.as_deref(),
        ),
        Commands::Publish {
            artifact_path,
            registry,
            platform,
        } => run_publish(
            artifact_path.as_deref(),
            registry.as_deref(),
            platform.as_deref(),
        ),
        Commands::Install {
            artifact,
            registry,
            global,
            all_platforms,
            no_precompile,
        } => run_install(
            artifact.as_deref(),
            registry.as_deref(),
            global,
            all_platforms,
            no_precompile,
        ),
        Commands::Precompile {
            files,
            workdir,
            json,
        } => run_precompile(&files, workdir.as_deref(), json),
        Commands::Run {
            roster: Some(roster),
            task,
            json,
            verbose,
            no_env_file,
            containment,
            ..
        } => match run_roster(RosterLaunch {
            roster: &roster,
            task: task.as_deref(),
            json,
            verbose,
            no_env_file,
            containment: containment.as_deref(),
        }) {
            // Every member has been stopped and reaped by the time a status comes back, so this
            // exit leaves nothing running.
            Ok(0) => Ok(()),
            Ok(code) => std::process::exit(code),
            Err(error) => Err(error),
        },
        Commands::Run {
            manifest,
            capsule,
            capsule_version,
            capsule_sha256,
            ignore_task_file,
            roster: None,
            spawn_grant_stdin,
            task,
            system_prompt,
            context,
            resume,
            resume_mode,
            forget_session,
            lifecycle_task_acceptance,
            lifecycle_after_task,
            workdir,
            json,
            verbose,
            bind,
            no_env_file,
            containment,
            explain_scope,
        } => run_run(
            &manifest,
            capsule.as_deref(),
            capsule_version.as_deref(),
            capsule_sha256.as_deref(),
            ignore_task_file,
            spawn_grant_stdin,
            task.as_deref(),
            system_prompt.as_deref(),
            context.as_deref(),
            resume.as_deref(),
            resume_mode.unwrap_or_default().into(),
            forget_session,
            lifecycle_task_acceptance.as_deref(),
            lifecycle_after_task.as_deref(),
            workdir,
            json,
            verbose,
            &bind,
            no_env_file,
            containment.as_deref(),
            explain_scope,
        ),
        Commands::Conversation { command } => match command {
            ConversationCommand::Ls {
                record,
                message,
                json,
            } => run_conversation_ls(record, message, json),
            ConversationCommand::Rm { context_id, record } => {
                run_conversation_rm(&context_id, record)
            }
            ConversationCommand::Truncate {
                context_id,
                keep,
                record,
            } => run_conversation_truncate(&context_id, keep, record),
        },
        Commands::Trace { command } => match command {
            TraceCommand::Show {
                session,
                workdir,
                body,
                turn,
            } => run_trace_show(session, workdir, body, turn),
            TraceCommand::Steps {
                session,
                verbose,
                workdir,
            } => run_trace_steps(session, workdir, verbose),
            TraceCommand::Diff {
                before,
                after,
                workdir,
            } => run_trace_diff(before, after, workdir),
            TraceCommand::Report {
                sessions,
                last,
                since,
                workdir,
            } => run_trace_report(sessions, last, since, workdir),
        },
        Commands::Eval { command } => match command {
            EvalCommand::Show {
                session,
                workdir,
                json,
            } => run_eval_show(session, workdir, json),
            EvalCommand::Diff { a, b, workdir } => run_eval_diff(a, b, workdir),
            EvalCommand::Run { capsule, dataset } => {
                run_eval_run(capsule.as_deref(), dataset.as_deref())
            }
        },
        #[cfg(feature = "beta-mur-topology")]
        Commands::Topology(args) => {
            if !beta_config.is_enabled("mur-topology") {
                eprintln!(
                    "error: unrecognized subcommand 'topology'\n\n\
                     For more information, try '--help'."
                );
                std::process::exit(1);
            }
            run_topology(&args)
        }
        Commands::Watch { session, url } => {
            live_address::target(session.as_deref(), url.as_deref())
                .and_then(|target| run_watch(&target))
        }
        Commands::Cancel {
            session,
            task_id,
            url,
        } => cancel_arguments(session, task_id, url)
            .and_then(|(target, task_id)| run_cancel(&target, &task_id)),
        Commands::Control { command } => run_control(command),
        Commands::Ps => run_ps(),
        Commands::Stop { session, timeout } => run_stop(&session, timeout),
        #[cfg(feature = "beta-mur-deploy")]
        Commands::Deploy { command } => {
            if !beta_config.is_enabled("mur-deploy") {
                eprintln!(
                    "error: unrecognized subcommand 'deploy'\n\n\
                     For more information, try '--help'."
                );
                std::process::exit(1);
            }
            match command {
                DeployCommand::Run {
                    host,
                    ssh_user,
                    ssh_key,
                    manifest,
                    workdir,
                    mur_binary,
                    env_vars,
                    env_file,
                    deploy_platform,
                    no_precompile,
                } => run_deploy(
                    &host,
                    ssh_key.as_deref(),
                    &ssh_user,
                    &manifest,
                    workdir.as_deref(),
                    mur_binary.as_deref(),
                    &env_vars,
                    env_file.as_deref(),
                    &deploy_platform,
                    no_precompile,
                ),
                DeployCommand::Ls => run_deploy_ls(),
            }
        }
        #[cfg(feature = "beta-mur-deploy")]
        Commands::Destroy { deployment_id } => {
            if !beta_config.is_enabled("mur-deploy") {
                eprintln!(
                    "error: unrecognized subcommand 'destroy'\n\n\
                     For more information, try '--help'."
                );
                std::process::exit(1);
            }
            run_destroy(&deployment_id)
        }
        Commands::Beta { command } => run_beta(&command),
        Commands::Config { command } => run_config(&command),
    };

    if let Err(err) = result {
        exit_with_error(&err);
    }
}

/// Report a failed command and exit 1.
///
/// Reached after a session has announced itself, when standard error may be closed: a bare
/// `eprintln!` would panic and replace exit status 1 with an abort.
#[deny(clippy::print_stdout, clippy::print_stderr)]
fn exit_with_error(err: &impl std::fmt::Display) -> ! {
    capsule_runtime::runtime_err!("{err}");
    std::process::exit(1);
}
