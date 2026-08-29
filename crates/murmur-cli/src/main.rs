mod beta;
mod commands;
mod config;
mod error;
mod registry_client;
mod source;

use std::path::PathBuf;

use capsule_runtime::ResumeMode;
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};

#[cfg(feature = "beta-mur-deploy")]
use commands::deploy::run_deploy;
#[cfg(feature = "beta-mur-deploy")]
use commands::destroy::run_destroy;
#[cfg(feature = "beta-mur-new")]
use commands::new::run_new;
#[cfg(feature = "beta-mur-deploy")]
use commands::ps::run_ps;
#[cfg(feature = "beta-mur-topology")]
use commands::topology::{run_topology, TopologyArgs};
use commands::{
    beta::{run_beta, BetaCommand},
    build::run_build,
    config_cmd::{run_config, ConfigCommand},
    conversation::{
        run_conversation_ls, run_conversation_rm, run_conversation_truncate, ConversationCommand,
    },
    doctor::run_doctor,
    eval::{run_eval_diff, run_eval_run, run_eval_show, EvalCommand},
    install::run_install,
    list::run_list,
    publish::run_publish,
    run::run_run,
    search::run_search,
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
    },
    #[cfg(feature = "beta-mur-new")]
    /// Generate a murmur.yaml from a plain-language task description
    New {
        /// Plain-language task description for the capsule to generate
        task: String,

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
    Doctor,
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
    },
    /// Run a capsule component with local lockfile-aware tool resolution
    Run {
        /// Path to murmur.yaml
        #[arg(long, default_value = "./murmur.yaml")]
        manifest: PathBuf,

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
        /// Resolves that session's context id and runs under it, loading its conversation
        /// record even when the capsule declares lifecycle.conversation: stateless.
        /// Cannot be combined with --context, which names the same thing directly.
        #[arg(long, value_name = "SESSION")]
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
        /// Capsule URL (e.g. localhost:12345)
        url: String,
    },
    #[cfg(feature = "beta-mur-deploy")]
    /// Upload a capsule to an existing VM and start it
    Deploy {
        /// IP address or hostname of the target VM (must already exist)
        #[arg(long)]
        host: String,

        /// SSH user on the target VM (default: root)
        #[arg(long, default_value = "root")]
        ssh_user: String,

        /// Path to SSH private key; uses SSH agent / default keys if omitted
        #[arg(long)]
        ssh_key: Option<std::path::PathBuf>,

        /// Path to murmur.yaml
        #[arg(long, default_value = "./murmur.yaml")]
        manifest: std::path::PathBuf,

        /// Local workdir to upload into the capsule's working directory
        #[arg(long)]
        workdir: Option<std::path::PathBuf>,

        /// Path to a pre-built mur binary for the target platform. If omitted, the version
        /// from manifest.mur_version (or the running mur version) is downloaded from GitHub
        /// releases and cached at ~/.murmur/bin/mur-{version}-{platform}.
        #[arg(long)]
        mur_binary: Option<std::path::PathBuf>,

        /// Environment variables for mur run on the remote VM: --env KEY=VALUE (repeatable).
        /// If neither --env nor --env-file is passed, .env in the manifest directory is
        /// loaded automatically when it exists.
        #[arg(long = "env", value_name = "KEY=VALUE")]
        env_vars: Vec<String>,

        /// Path to a .env file with KEY=VALUE entries (one per line, # comments ignored).
        /// Takes precedence over auto-detection of .env in the manifest directory.
        #[arg(long, value_name = "PATH")]
        env_file: Option<std::path::PathBuf>,

        /// Target platform for artifact resolution (default: linux-x86_64).
        /// Artifacts are pulled and staged for this platform before deploying.
        #[arg(long, default_value = "linux-x86_64")]
        deploy_platform: String,
    },
    #[cfg(feature = "beta-mur-deploy")]
    /// Terminate a deployed capsule VM and remove it from the deployment list
    Destroy {
        /// Deployment ID returned by `mur deploy`; a unique prefix is enough
        deployment_id: String,
    },
    #[cfg(feature = "beta-mur-deploy")]
    /// List all deployed capsules
    Ps,
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

    #[cfg(feature = "beta-mur-new")]
    if !beta_config.is_enabled("mur-new") {
        cmd = cmd.mut_subcommand("new", |sc| sc.hide(true));
    }
    #[cfg(feature = "beta-mur-deploy")]
    if !beta_config.is_enabled("mur-deploy") {
        cmd = cmd.mut_subcommand("deploy", |sc| sc.hide(true));
        cmd = cmd.mut_subcommand("destroy", |sc| sc.hide(true));
        cmd = cmd.mut_subcommand("ps", |sc| sc.hide(true));
    }
    #[cfg(feature = "beta-mur-topology")]
    if !beta_config.is_enabled("mur-topology") {
        cmd = cmd.mut_subcommand("topology", |sc| sc.hide(true));
    }

    let matches = cmd.get_matches();
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());

    let result = match cli.command {
        #[cfg(feature = "beta-mur-new")]
        Commands::New { task, registry } => {
            if !beta_config.is_enabled("mur-new") {
                eprintln!(
                    "error: unrecognized subcommand 'new'\n\n\
                     For more information, try '--help'."
                );
                std::process::exit(1);
            }
            run_new(&task, registry.as_deref())
        }
        Commands::List { global, all } => run_list(global, all),
        Commands::Search {
            query,
            registry,
            limit,
        } => run_search(&query, registry.as_deref(), limit),
        Commands::Doctor => run_doctor(),
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
        } => run_install(
            artifact.as_deref(),
            registry.as_deref(),
            global,
            all_platforms,
        ),
        Commands::Run {
            manifest,
            task,
            system_prompt,
            context,
            resume,
            resume_mode,
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
            task.as_deref(),
            system_prompt.as_deref(),
            context.as_deref(),
            resume.as_deref(),
            resume_mode.unwrap_or_default().into(),
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
            EvalCommand::Diff { a, b, workdir } => run_eval_diff(Some(a), Some(b), workdir),
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
        Commands::Watch { url } => run_watch(&url),
        #[cfg(feature = "beta-mur-deploy")]
        Commands::Deploy {
            host,
            ssh_user,
            ssh_key,
            manifest,
            workdir,
            mur_binary,
            env_vars,
            env_file,
            deploy_platform,
        } => {
            if !beta_config.is_enabled("mur-deploy") {
                eprintln!(
                    "error: unrecognized subcommand 'deploy'\n\n\
                     For more information, try '--help'."
                );
                std::process::exit(1);
            }
            run_deploy(
                &host,
                ssh_key.as_deref(),
                &ssh_user,
                &manifest,
                workdir.as_deref(),
                mur_binary.as_deref(),
                &env_vars,
                env_file.as_deref(),
                &deploy_platform,
            )
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
        #[cfg(feature = "beta-mur-deploy")]
        Commands::Ps => {
            if !beta_config.is_enabled("mur-deploy") {
                eprintln!(
                    "error: unrecognized subcommand 'ps'\n\n\
                     For more information, try '--help'."
                );
                std::process::exit(1);
            }
            run_ps()
        }
        Commands::Beta { command } => run_beta(&command),
        Commands::Config { command } => run_config(&command),
    };

    if let Err(err) = result {
        eprintln!("{err}");
        std::process::exit(1);
    }
}
