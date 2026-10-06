//! `mur new --roster <NAME>`: write `./<NAME>/`, a two-member formation that roster admission
//! accepts and `mur run --roster <NAME>` launches as written.
//!
//! The files are templates rendered from the name and the global config's `inference:` block and
//! nothing else: no model is called, no key is read, and the config is never written. Each file is
//! parsed by the parser that later reads it before anything touches the disk.
//!
//! The layout is the convention for every roster: one directory per formation, `roster.yaml` at its
//! root, and one subdirectory per member, named after the member, holding that member's
//! `murmur.yaml`. The members install into the global store, because a project store is reached
//! only through a top-level `murmur.yaml`, and the formation directory has none.

use std::fs;
use std::path::{Path, PathBuf};

use murmur_artifact::{
    artifact_name_format_error, Roster, RuntimeManifest, MANIFEST_FILENAME, ROSTER_FILENAME,
};

use crate::config::{load_mur_config, InferenceConfig};
use crate::error::{
    CliError, E_IO_003, E_MAN_002, E_MAN_003, E_NEW_002, E_NEW_003, E_NEW_004, E_ROS_001,
};

/// An inference provider the scaffold can name a driver for. `mur new <task>` takes its default
/// endpoint, model and key variable from here too.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Provider {
    /// The `inference.provider` value in `~/.murmur/config.yaml` that selects it.
    pub(crate) name: &'static str,
    pub(crate) driver: &'static str,
    /// The driver version a scaffolded member pins: the `extra.v` entry `docs/mkdocs.yml` holds
    /// for this driver, which a unit test keeps equal.
    pub(crate) driver_version: &'static str,
    /// `gateway.endpoint` when `inference.endpoint` is empty. The driver appends its own path to
    /// the endpoint: `/v1/messages` for the Anthropic driver, `chat/completions` or `responses`
    /// for the OpenAI driver. The default therefore holds everything before that path.
    pub(crate) default_endpoint: &'static str,
    /// `inference.model` when the config's `inference.model` is empty.
    pub(crate) default_model: &'static str,
    /// The variable a member's `gateway.api_key` references.
    pub(crate) key_var: &'static str,
}

pub(crate) const ANTHROPIC: Provider = Provider {
    name: "anthropic",
    driver: "murmur-driver-anthropic",
    driver_version: "0.10.0",
    default_endpoint: "https://api.anthropic.com",
    default_model: "claude-haiku-4-5-20251001",
    key_var: "ANTHROPIC_API_KEY",
};

pub(crate) const OPENAI: Provider = Provider {
    name: "openai",
    driver: "murmur-driver-openai",
    driver_version: "0.9.0",
    default_endpoint: "https://api.openai.com/v1",
    default_model: "gpt-5.6-luna",
    key_var: "OPENAI_API_KEY",
};

/// Every provider the scaffold accepts. The first is the one taken when the config has no
/// `inference:` block or leaves `inference.provider` empty.
pub(crate) const PROVIDERS: [&Provider; 2] = [&ANTHROPIC, &OPENAI];

/// The version every scaffolded capsule is written at.
pub(crate) const SCAFFOLD_VERSION: &str = "0.1.0";

/// The provider a scaffold names, and the endpoint and model its members are written with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderChoice {
    pub(crate) provider: &'static Provider,
    pub(crate) endpoint: String,
    pub(crate) model: String,
}

/// One member of the scaffolded formation.
struct MemberTemplate {
    name: &'static str,
    entry: bool,
    /// Declares `exports.peer_tasks.accept: true`. True for exactly the members a
    /// [`REACHABILITY`] rule names in its `to`, which admission requires.
    serves_peers: bool,
    task_acceptance: &'static str,
    task_acceptance_note: &'static str,
    after_task: &'static str,
    after_task_note: &'static str,
    /// The sentence after the capsule name in the file's header comment.
    role: &'static str,
    lifecycle_note: &'static str,
}

const LEAD: MemberTemplate = MemberTemplate {
    name: "lead",
    entry: true,
    serves_peers: false,
    task_acceptance: "single",
    task_acceptance_note: "one task: the formation's",
    after_task: "exit",
    after_task_note: "exit when it ends, which stops the formation",
    role: "the entry member, which receives the formation's task",
    lifecycle_note: "the entry member takes the formation's one task and exits, which ends the \
                     formation",
};

const WORKER: MemberTemplate = MemberTemplate {
    name: "worker",
    entry: false,
    serves_peers: true,
    task_acceptance: "queue",
    task_acceptance_note: "take tasks one after another",
    after_task: "sleep",
    after_task_note: "wait at the door for the next one",
    role: "the member lead may call, which serves tasks at its door",
    lifecycle_note: "worker waits at its door for as long as the formation runs",
};

/// The formation's members, in the order `roster.yaml` lists them.
const MEMBERS: [&MemberTemplate; 2] = [&LEAD, &WORKER];

/// The formation's reachability rules: each `from` may call every member in its `to`.
const REACHABILITY: &[(&str, &[&str])] = &[("lead", &["worker"])];

const DOCS: &str = murmur_artifact::DOCS_REFERENCE_URL;

/// The column trailing comments start at, when the line before them is short enough.
const COMMENT_COLUMN: usize = 36;

/// The width comment blocks are wrapped to, indentation included.
const WRAP_WIDTH: usize = 96;

/// The rendered files of one formation, each paired with its path under the formation directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Scaffold {
    pub(crate) files: Vec<(PathBuf, String)>,
}

#[cfg(test)]
impl Scaffold {
    /// The text of the file at `relative`, a path under the formation directory.
    fn file(&self, relative: &Path) -> Option<&str> {
        self.files
            .iter()
            .find(|(path, _)| path == relative)
            .map(|(_, text)| text.as_str())
    }
}

/// The capsule name the member `member` of the formation `name` runs.
pub(crate) fn capsule_name(name: &str, member: &str) -> String {
    format!("{name}-{member}")
}

/// Write `./<name>/` in the current directory. Nothing is written unless every file renders and
/// parses, and nothing is left behind on any error.
pub(crate) fn run_new_roster(name: &str) -> Result<(), CliError> {
    check_formation_name(name)?;
    let choice = choose_provider(load_mur_config()?.inference.as_ref())?;
    let cwd = std::env::current_dir().map_err(|source| {
        CliError::new(
            E_IO_003,
            format!("failed to determine current working directory: {source}"),
        )
    })?;
    let target = cwd.join(name);
    refuse_existing(name, &target)?;
    let scaffold = render_scaffold(name, &choice);
    self_check(&scaffold, name, &choice)?;
    write_formation(&cwd, name, &scaffold)?;
    print!("{}", render_summary(name, &choice));
    Ok(())
}

/// `E-NEW-002` unless `name`, and every capsule name derived from it, is a valid artifact name.
pub(crate) fn check_formation_name(name: &str) -> Result<(), CliError> {
    if let Some(reason) = artifact_name_format_error(name) {
        return Err(CliError::with_hint(
            E_NEW_002,
            format!("formation name '{name}' is not a valid artifact name: it {reason}"),
            "the name is the directory written and the prefix of every member's capsule name; \
             use lowercase letters, digits and '-', for example `mur new --roster crew`",
        ));
    }
    for member in MEMBERS {
        let capsule = capsule_name(name, member.name);
        if let Some(reason) = artifact_name_format_error(&capsule) {
            return Err(CliError::with_hint(
                E_NEW_002,
                format!(
                    "formation name '{name}' gives the member '{member}' the capsule name \
                     '{capsule}', which is not a valid artifact name: it {reason}",
                    member = member.name,
                ),
                "each member's capsule is named <name>-<member>; shorten the formation name",
            ));
        }
    }
    Ok(())
}

/// The provider `inference:` selects, with its endpoint and model. An absent block, or an empty
/// field, takes the default; `inference.api_key` is never read. `E-NEW-004` for a provider with
/// no entry in [`PROVIDERS`].
pub(crate) fn choose_provider(
    inference: Option<&InferenceConfig>,
) -> Result<ProviderChoice, CliError> {
    let requested = configured(inference.map(|inference| inference.provider.as_str()));
    let provider = match requested {
        None => PROVIDERS[0],
        Some(requested) => *PROVIDERS
            .iter()
            .find(|provider| provider.name == requested)
            .ok_or_else(|| unknown_provider(requested))?,
    };
    let endpoint = configured(inference.map(|inference| inference.endpoint.as_str()))
        .unwrap_or(provider.default_endpoint);
    let model = configured(inference.map(|inference| inference.model.as_str()))
        .unwrap_or(provider.default_model);
    Ok(ProviderChoice {
        provider,
        endpoint: endpoint.to_string(),
        model: model.to_string(),
    })
}

/// A config field's value, or `None` when it is absent, empty or only whitespace.
fn configured(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn unknown_provider(requested: &str) -> CliError {
    let known = PROVIDERS
        .iter()
        .map(|provider| format!("'{}'", provider.name))
        .collect::<Vec<_>>()
        .join(" or ");
    CliError::with_hint(
        E_NEW_004,
        format!(
            "inference.provider in ~/.murmur/config.yaml is '{requested}', and mur new --roster \
             has a driver only for {known}"
        ),
        format!(
            "set inference.provider to {known} with `mur config set -g inference.provider \
             {first}`, or remove it to use {first}",
            first = PROVIDERS[0].name,
        ),
    )
}

/// `E-NEW-003` when anything at all is at `target`, a dangling symlink included.
fn refuse_existing(name: &str, target: &Path) -> Result<(), CliError> {
    if target.symlink_metadata().is_err() {
        return Ok(());
    }
    Err(CliError::with_hint(
        E_NEW_003,
        format!(
            "./{name} already exists; mur new --roster writes a new directory and never writes \
             into or over an existing path"
        ),
        format!("choose another formation name, or move ./{name} out of the way"),
    ))
}

/// Parse every file as the command that later reads it will. A refusal is a defect in the
/// templates, unless the configured `inference.endpoint` is what the manifest parser refuses.
fn self_check(scaffold: &Scaffold, name: &str, choice: &ProviderChoice) -> Result<(), CliError> {
    for (path, text) in &scaffold.files {
        if path.file_name().and_then(|file| file.to_str()) == Some(MANIFEST_FILENAME) {
            if let Err(error) = RuntimeManifest::from_yaml_str(text) {
                return Err(manifest_refused(name, choice, path, &error.to_string()));
            }
        } else if let Err(error) = Roster::from_yaml_str(text) {
            return Err(CliError::new(
                E_ROS_001,
                format!(
                    "internal: scaffolded {} is invalid: {error}",
                    Path::new(name).join(path).display()
                ),
            ));
        }
    }
    Ok(())
}

fn manifest_refused(name: &str, choice: &ProviderChoice, path: &Path, error: &str) -> CliError {
    let with_default_endpoint = ProviderChoice {
        endpoint: choice.provider.default_endpoint.to_string(),
        ..choice.clone()
    };
    let endpoint_at_fault = choice.endpoint != choice.provider.default_endpoint
        && render_scaffold(name, &with_default_endpoint)
            .files
            .iter()
            .filter(|(path, _)| path.ends_with(MANIFEST_FILENAME))
            .all(|(_, text)| RuntimeManifest::from_yaml_str(text).is_ok());
    if endpoint_at_fault {
        return CliError::with_hint(
            E_MAN_003,
            format!(
                "inference.endpoint in ~/.murmur/config.yaml cannot be a member's \
                 gateway.endpoint: {error}"
            ),
            format!(
                "set inference.endpoint to an https:// URL, or an http:// URL on localhost or a \
                 loopback address; or remove it to use {}",
                choice.provider.default_endpoint
            ),
        );
    }
    CliError::new(
        E_MAN_002,
        format!(
            "internal: scaffolded {} is invalid: {error}",
            Path::new(name).join(path).display()
        ),
    )
}

/// Write the files into a fresh `./.<name>.tmp-<uuid>` and rename it to `./<name>`. The staging
/// directory is removed on every error.
fn write_formation(cwd: &Path, name: &str, scaffold: &Scaffold) -> Result<(), CliError> {
    let target = cwd.join(name);
    let staging = Staging::create(cwd.join(format!(".{name}.tmp-{}", uuid::Uuid::new_v4())))?;
    for (relative, text) in &scaffold.files {
        let path = staging.path.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| {
                CliError::new(
                    E_IO_003,
                    format!("failed to create {}: {source}", parent.display()),
                )
            })?;
        }
        fs::write(&path, text).map_err(|source| {
            CliError::new(
                E_IO_003,
                format!("failed to write {}: {source}", path.display()),
            )
        })?;
    }
    // A rename onto an existing non-empty directory fails, so a path created after
    // `refuse_existing` looked is still refused rather than replaced.
    if let Err(source) = fs::rename(&staging.path, &target) {
        refuse_existing(name, &target)?;
        return Err(CliError::new(
            E_IO_003,
            format!(
                "failed to rename {} to {}: {source}",
                staging.path.display(),
                target.display()
            ),
        ));
    }
    staging.renamed();
    Ok(())
}

/// The directory a formation is written into before it is renamed into place. Removed on drop
/// unless it was renamed.
struct Staging {
    path: PathBuf,
    renamed: bool,
}

impl Staging {
    /// Fails if anything is at `path`, so dropping the guard removes only what it created.
    fn create(path: PathBuf) -> Result<Self, CliError> {
        fs::create_dir(&path).map_err(|source| {
            CliError::new(
                E_IO_003,
                format!("failed to create {}: {source}", path.display()),
            )
        })?;
        Ok(Self {
            path,
            renamed: false,
        })
    }

    fn renamed(mut self) {
        self.renamed = true;
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if !self.renamed {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

/// Every file of the formation `name`, with its path under `./<name>/`. Pure: the same name and
/// choice always render the same bytes.
pub(crate) fn render_scaffold(name: &str, choice: &ProviderChoice) -> Scaffold {
    let mut files = vec![(PathBuf::from(ROSTER_FILENAME), render_roster(name))];
    for member in MEMBERS {
        files.push((
            Path::new(member.name).join(MANIFEST_FILENAME),
            render_member(name, member, choice),
        ));
    }
    Scaffold { files }
}

/// What `mur new --roster` prints on success: the files written, then the commands that install
/// and launch the formation. The key step comes first because the key is the one value only the
/// operator can supply, and `mur run --roster` fails without it.
pub(crate) fn render_summary(name: &str, choice: &ProviderChoice) -> String {
    let rules = REACHABILITY
        .iter()
        .map(|(from, to)| format!("{from} may call {}", to.join(", ")))
        .collect::<Vec<_>>()
        .join("; ");
    let entry = MEMBERS
        .iter()
        .find(|member| member.entry)
        .map_or("", |member| member.name);
    let mut rows = vec![(
        format!("{name}/{ROSTER_FILENAME}"),
        format!("{entry} is the entry member; {rules}"),
    )];
    for member in MEMBERS {
        rows.push((
            format!("{name}/{}/{MANIFEST_FILENAME}", member.name),
            format!(
                "capsule {}@{SCAFFOLD_VERSION}{}",
                capsule_name(name, member.name),
                if member.serves_peers {
                    ", serves peers"
                } else {
                    ""
                }
            ),
        ));
    }
    let width = rows.iter().map(|(path, _)| path.len()).max().unwrap_or(0) + 3;
    let mut out = format!("Scaffolded formation '{name}' in ./{name}\n");
    for (path, note) in rows {
        out.push_str(&format!("  {path:<width$}{note}\n"));
    }
    out.push_str("\nNext:\n");
    out.push_str(&format!(
        "  mur config set -g credentials.{} <your key>\n",
        choice.provider.key_var
    ));
    out.push_str(&format!(
        "  mur install -g {}@{}\n",
        choice.provider.driver, choice.provider.driver_version
    ));
    for member in MEMBERS {
        let dir = format!("{name}/{}", member.name);
        out.push_str(&format!(
            "  mur build {dir} && mur install -g {dir}/{}-{SCAFFOLD_VERSION}.mur.zip\n",
            capsule_name(name, member.name)
        ));
    }
    out.push_str(&format!(
        "  mur run --roster {name} --task \"<your task>\"\n"
    ));
    out
}

/// A YAML document built line by line, every key with its comment.
struct Yaml(String);

impl Yaml {
    fn new() -> Self {
        Self(String::new())
    }

    /// `text` as `#` lines at `indent` spaces, wrapped to [`WRAP_WIDTH`]. A blank line in `text`
    /// starts a new paragraph, written after a bare `#` line.
    fn comment(&mut self, indent: usize, text: &str) -> &mut Self {
        let width = WRAP_WIDTH.saturating_sub(indent + 2);
        for (index, paragraph) in text.split("\n\n").enumerate() {
            if index > 0 {
                self.0.push_str(&format!("{:indent$}#\n", ""));
            }
            let mut line = String::new();
            for word in paragraph.split_whitespace() {
                if !line.is_empty() && line.len() + 1 + word.len() > width {
                    self.0.push_str(&format!("{:indent$}# {line}\n", ""));
                    line.clear();
                }
                if !line.is_empty() {
                    line.push(' ');
                }
                line.push_str(word);
            }
            if !line.is_empty() {
                self.0.push_str(&format!("{:indent$}# {line}\n", ""));
            }
        }
        self
    }

    /// A comment naming the reference section at `anchor`, a path under the docs site's
    /// `reference/`.
    fn see(&mut self, indent: usize, anchor: &str) -> &mut Self {
        self.0
            .push_str(&format!("{:indent$}# {DOCS}/{anchor}\n", ""));
        self
    }

    /// `text` at `indent` spaces, with `note` as its trailing comment.
    fn line(&mut self, indent: usize, text: &str, note: &str) -> &mut Self {
        let written = format!("{:indent$}{text}", "");
        let pad = COMMENT_COLUMN.saturating_sub(written.len()).max(2);
        self.0.push_str(&format!("{written}{:pad$}# {note}\n", ""));
        self
    }

    /// `text` at `indent` spaces with no comment of its own: the line directly above comments it.
    fn bare(&mut self, indent: usize, text: &str) -> &mut Self {
        self.0.push_str(&format!("{:indent$}{text}\n", ""));
        self
    }

    fn blank(&mut self) -> &mut Self {
        self.0.push('\n');
        self
    }
}

/// `value` as a double-quoted YAML scalar. A JSON string is one, with every character that could
/// end it or start a comment escaped.
fn quoted(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

fn render_roster(name: &str) -> String {
    let entry = MEMBERS
        .iter()
        .find(|member| member.entry)
        .map_or("", |member| member.name);
    let mut yaml = Yaml::new();
    yaml.comment(
        0,
        &format!(
            "The formation '{name}': the capsules `mur run --roster {name}` starts together, the
             one that receives the task, and which of them may call which."
        ),
    )
    .blank()
    .comment(0, "The roster schema version. 1 is the only version.")
    .see(0, "roster/#fields")
    .bare(0, "roster_version: 1")
    .blank()
    .comment(
        0,
        "The members: each one an installed capsule at an exact version. Admission looks each
         capsule up in the project store, then in the global store, where `mur install -g`
         puts it.",
    )
    .see(0, "roster/#members")
    .bare(0, "members:");
    for member in MEMBERS {
        let capsule = capsule_name(name, member.name);
        yaml.line(
            2,
            &format!("- name: {}", member.name),
            "its name in this roster, used by reachability",
        )
        .line(
            4,
            &format!("capsule: {capsule}"),
            &format!("built from {}/{MANIFEST_FILENAME}", member.name),
        )
        .line(
            4,
            &format!("version: {SCAFFOLD_VERSION}"),
            "the exact version installed",
        );
        if member.entry {
            yaml.comment(
                4,
                "The entry member receives the formation's task, and its outcome is the
                 formation's outcome. Exactly one member has entry: true.",
            )
            .see(4, "roster/#entry-member")
            .bare(4, "entry: true");
        }
    }
    yaml.blank()
        .comment(
            0,
            &format!(
                "Which members may call which: each rule lets `from` call every member in `to`.
                 A member named in `to` must serve peers: exports.peer_tasks.accept: true in its
                 {MANIFEST_FILENAME}. With any rule here, every member's door must require a
                 token: network.authentication in its {MANIFEST_FILENAME}.

                 `all` is shorthand for every ordered pair of members that both serve peers. Here
                 `all` would grant nothing: {entry} does not serve peers, so no two members both
                 do.",
            ),
        )
        .see(0, "roster/#reachability")
        .see(0, "roster/#authentication")
        .bare(0, "reachability:");
    for (from, to) in REACHABILITY {
        yaml.line(2, &format!("- from: {from}"), "the calling member")
            .line(
                4,
                &format!("to: [{}]", to.join(", ")),
                &format!("the members {from} may call"),
            );
    }
    yaml.0
}

fn render_member(name: &str, member: &MemberTemplate, choice: &ProviderChoice) -> String {
    let capsule = capsule_name(name, member.name);
    let provider = choice.provider;
    let dir = format!("{name}/{}", member.name);
    let mut yaml = Yaml::new();
    yaml.comment(
        0,
        &format!(
            "{capsule}: the member '{member_name}' of the formation '{name}' in ../roster.yaml,
             {role}.

             Build and install: mur build {dir} && mur install -g
             {dir}/{capsule}-{SCAFFOLD_VERSION}.mur.zip",
            member_name = member.name,
            role = member.role,
        ),
    )
    .blank()
    .comment(
        0,
        &format!(
            "The capsule's artifact name. ../roster.yaml runs it as the member '{}'.",
            member.name
        ),
    )
    .see(0, "manifest/#field-identity")
    .bare(0, &format!("name: {capsule}"))
    .blank()
    .comment(
        0,
        "The capsule's version. ../roster.yaml names this exact version; change both together.",
    )
    .see(0, "manifest/#field-identity")
    .bare(0, &format!("version: {SCAFFOLD_VERSION}"))
    .blank()
    .comment(
        0,
        "How `mur build` packages the capsule. `mur run` takes no meaning from it.",
    )
    .see(0, "manifest/#artifact-manifest")
    .bare(0, "runtime: capsule")
    .blank()
    .comment(
        0,
        "No payload beside this file: the built archive holds only this murmur.yaml.",
    )
    .see(0, "manifest/#artifact-manifest")
    .bare(0, "execution: static")
    .blank()
    .comment(
        0,
        &format!(
            "The artifacts the capsule runs with: its inference driver.

             Install the driver: mur install -g {}@{}",
            provider.driver, provider.driver_version
        ),
    )
    .see(0, "manifest/#field-artifacts")
    .bare(0, "artifacts:")
    .line(
        2,
        &format!("- name: {}", provider.driver),
        "the driver that calls the provider's API",
    )
    .line(
        4,
        &format!("version: {}", provider.driver_version),
        "the exact driver version",
    )
    .line(
        4,
        "runtime: driver",
        "an inference driver, hidden from the model",
    )
    .comment(
        4,
        "The provider's API, and the key the runtime presents there. The driver never holds
         the key.",
    )
    .see(4, "manifest/#artifact-gateway")
    .bare(4, "gateway:")
    .line(
        6,
        &format!("endpoint: {}", quoted(&choice.endpoint)),
        "where the driver's requests go",
    )
    .comment(
        6,
        &format!(
            "A reference, resolved at launch: credentials.{key} in ~/.murmur/config.yaml, then
             the environment variable {key}.",
            key = provider.key_var
        ),
    )
    .see(6, "manifest/#gateway-api-key")
    .bare(6, &format!("api_key: ${{{}}}", provider.key_var))
    .blank()
    .comment(0, "The model, and the driver artifact that serves it.")
    .see(0, "manifest/#field-inference")
    .bare(0, "inference:")
    .line(
        2,
        "transport: http",
        "every model call goes through the driver",
    )
    .line(
        2,
        &format!("model: {}", quoted(&choice.model)),
        "the model identifier passed to the driver",
    )
    .line(2, "driver:", "the driver serving the model")
    .line(
        4,
        &format!("artifact: {}", provider.driver),
        "an artifacts: entry with runtime: driver",
    )
    .comment(2, "Sent as the system prompt on every model call.")
    .bare(
        2,
        &format!("system_prompt: {}", quoted(&system_prompt(name, member))),
    )
    .blank()
    .comment(
        0,
        &format!("How the capsule takes tasks: {}.", member.lifecycle_note),
    )
    .see(0, "manifest/#field-lifecycle")
    .bare(0, "lifecycle:")
    .line(
        2,
        &format!("task_acceptance: {}", member.task_acceptance),
        member.task_acceptance_note,
    )
    .line(
        2,
        &format!("after_task: {}", member.after_task),
        member.after_task_note,
    )
    .blank();
    let callees = callees_of(member.name);
    if !callees.is_empty() {
        yaml.comment(
            0,
            &format!(
                "Where the capsule may connect. ../roster.yaml lets {} call {}, but a roster rule
                 grants a name and a credential, never egress: each call-member call is checked
                 against this list at the callee's real door. A formation serves that door on
                 loopback http at a port chosen at launch, so the rule names the host and pins no
                 port.",
                member.name,
                callees.join(", "),
            ),
        )
        .see(0, "manifest/#field-capabilities")
        .bare(0, "capabilities:")
        .line(2, "network:", "IP destinations the capsule may reach")
        .line(4, "allow:", "anything not listed is denied")
        .line(
            6,
            "- localhost",
            "every member's door, at any port and scheme",
        )
        .blank();
    }
    let callers: Vec<&str> = REACHABILITY
        .iter()
        .filter(|(_, to)| to.contains(&member.name))
        .map(|(from, _)| *from)
        .collect();
    if member.serves_peers {
        yaml.comment(
            0,
            &format!(
                "Whether the door serves tasks other members send. ../roster.yaml lets {} call
                 {}, and admission requires every member a rule calls to consent here.",
                callers.join(", "),
                member.name
            ),
        )
        .see(0, "manifest/#field-exports-peer-tasks")
        .bare(0, "exports:")
        .line(2, "peer_tasks:", "tasks another member sends to this door")
        .line(
            4,
            "accept: true",
            "serve them; absent or false refuses them",
        )
        .blank();
    } else {
        yaml.comment(
            0,
            &format!(
                "{} declares no exports.peer_tasks, so it serves no peer, and no reachability
                 rule may name it in `to`.",
                member.name
            ),
        )
        .see(0, "manifest/#field-exports-peer-tasks")
        .blank();
    }
    yaml.comment(
        0,
        "The capsule's A2A door. A roster with any reachability rule requires every member's
         door to require a token. A capsule that declares this cannot also declare
         capabilities.spawn.allow.",
    )
    .see(0, "manifest/#field-network-authentication")
    .bare(0, "network:")
    .line(
        2,
        "authentication:",
        "refuse every caller without a token this session minted",
    )
    .line(
        4,
        "scheme: bearer",
        "Authorization: Bearer <token>, the only scheme",
    );
    yaml.0
}

/// The members `member` may call, in rule order.
fn callees_of(member: &str) -> Vec<&'static str> {
    REACHABILITY
        .iter()
        .filter(|(from, _)| *from == member)
        .flat_map(|(_, to)| to.iter().copied())
        .collect()
}

fn system_prompt(name: &str, member: &MemberTemplate) -> String {
    let callees = callees_of(member.name);
    if member.entry && !callees.is_empty() {
        let callees = callees.join(" or ");
        format!(
            "You are '{}', the entry member of the formation '{name}'. You do not do the task \
             yourself: {callees} does it. Your first reply is a call-member tool call that hands \
             {callees} the task you are given, stated in full. Then end your turn while \
             {callees} works. When {callees}'s answer arrives in this conversation, answer with \
             it.",
            member.name,
        )
    } else if member.entry {
        format!(
            "You are '{}', the entry member of the formation '{name}'. Complete the task you are \
             given and answer with the result.",
            member.name
        )
    } else {
        format!(
            "You are '{}', a member of the formation '{name}'. Complete each task another member \
             sends you and answer with the result.",
            member.name
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use murmur_artifact::{
        AfterTask, ApiKeyReference, RosterReachability, TaskAcceptance, REACHABILITY_ALL,
    };

    fn default_choice() -> ProviderChoice {
        choose_provider(None).unwrap()
    }

    fn files(choice: &ProviderChoice) -> (Roster, RuntimeManifest, RuntimeManifest, Scaffold) {
        let scaffold = render_scaffold("crew", choice);
        let roster = Roster::from_yaml_str(scaffold.file(Path::new("roster.yaml")).unwrap())
            .expect("the roster parses");
        let manifest = |member: &str| {
            let text = scaffold
                .file(&Path::new(member).join(MANIFEST_FILENAME))
                .unwrap();
            let manifest = RuntimeManifest::from_yaml_str(text)
                .unwrap_or_else(|error| panic!("{member}'s manifest parses: {error}\n{text}"));
            assert!(
                manifest.unknown_keys.is_empty(),
                "{member}: {:?}",
                manifest.unknown_keys
            );
            manifest
        };
        let lead = manifest("lead");
        let worker = manifest("worker");
        (roster, lead, worker, scaffold)
    }

    #[test]
    fn the_roster_has_lead_as_entry_and_one_explicit_rule() {
        let (roster, _, _, scaffold) = files(&default_choice());
        let members: Vec<(&str, &str, &str, bool)> = roster
            .members
            .iter()
            .map(|member| {
                (
                    member.name.as_str(),
                    member.capsule.as_str(),
                    member.version.as_str(),
                    member.entry,
                )
            })
            .collect();
        assert_eq!(
            members,
            [
                ("lead", "crew-lead", "0.1.0", true),
                ("worker", "crew-worker", "0.1.0", false)
            ]
        );
        let RosterReachability::Rules(rules) = &roster.reachability else {
            panic!("reachability is not a rule list: {:?}", roster.reachability);
        };
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].from, "lead");
        assert_eq!(rules[0].to, ["worker"]);
        let text = scaffold.file(Path::new("roster.yaml")).unwrap();
        assert!(
            text.lines()
                .filter(|line| !line.trim_start().starts_with('#'))
                .all(|line| !line.contains(&format!("reachability: {REACHABILITY_ALL}"))),
            "{text}"
        );
        let prose = text
            .lines()
            .filter_map(|line| line.trim_start().strip_prefix("# "))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            prose.contains(
                "`all` is shorthand for every ordered pair of members that both serve peers. \
                 Here `all` would grant nothing: lead does not serve peers"
            ),
            "{text}"
        );
    }

    #[test]
    fn only_the_callee_serves_peers_and_both_doors_are_authenticated() {
        let (_, lead, worker, _) = files(&default_choice());
        assert!(!lead.accepts_peer_tasks());
        assert!(worker.accepts_peer_tasks());
        for manifest in [&lead, &worker] {
            assert!(
                manifest
                    .network
                    .as_ref()
                    .is_some_and(|network| network.authentication.is_some()),
                "{} declares network.authentication",
                manifest.name
            );
            assert!(
                manifest
                    .capabilities
                    .as_ref()
                    .and_then(|capabilities| capabilities.spawn.as_ref())
                    .is_none(),
                "{} declares no capabilities.spawn",
                manifest.name
            );
        }
        for member in MEMBERS {
            let called = REACHABILITY.iter().any(|(_, to)| to.contains(&member.name));
            assert_eq!(member.serves_peers, called, "{}", member.name);
        }
    }

    /// The caller's egress is what `call-member` is checked against at the callee's door, so the
    /// member that calls declares the door host at every port, and the member that only serves
    /// declares no capabilities at all.
    #[test]
    fn only_the_caller_declares_network_allow_reaching_every_door() {
        let (_, lead, worker, scaffold) = files(&default_choice());
        let allow = lead
            .capabilities
            .as_ref()
            .and_then(|capabilities| capabilities.network.as_ref())
            .map(|network| network.allow.clone())
            .unwrap_or_default();
        assert_eq!(allow, ["localhost"]);
        assert!(worker.capabilities.is_none());
        let text = scaffold.file(Path::new("lead/murmur.yaml")).unwrap();
        let prose = text
            .lines()
            .filter_map(|line| line.trim_start().strip_prefix("# "))
            .collect::<Vec<_>>()
            .join(" ");
        for said in [
            "grants a name and a credential, never egress",
            "each call-member call is checked against this list at the callee's real door",
            "loopback http at a port chosen at launch",
        ] {
            assert!(prose.contains(said), "{said}: {text}");
        }
    }

    /// Lead's prompt names the hand-off: lead does not do the task, its first reply is a
    /// call-member call to worker, then it ends the turn and answers with what comes back.
    #[test]
    fn the_lead_prompt_hands_the_task_to_worker() {
        let (_, lead, worker, _) = files(&default_choice());
        let prompt = lead.inference.unwrap().system_prompt.unwrap();
        assert_eq!(
            prompt,
            "You are 'lead', the entry member of the formation 'crew'. You do not do the task \
             yourself: worker does it. Your first reply is a call-member tool call that hands \
             worker the task you are given, stated in full. Then end your turn while worker \
             works. When worker's answer arrives in this conversation, answer with it."
        );
        let worker_prompt = worker.inference.unwrap().system_prompt.unwrap();
        assert!(!worker_prompt.contains("call-member"), "{worker_prompt}");
    }

    #[test]
    fn lifecycles_are_stated() {
        let (_, lead, worker, scaffold) = files(&default_choice());
        let lead_lifecycle = lead.lifecycle.unwrap();
        assert_eq!(lead_lifecycle.task_acceptance, TaskAcceptance::Single);
        assert_eq!(lead_lifecycle.after_task, AfterTask::Exit);
        let worker_lifecycle = worker.lifecycle.unwrap();
        assert_eq!(worker_lifecycle.task_acceptance, TaskAcceptance::Queue);
        assert_eq!(worker_lifecycle.after_task, AfterTask::Sleep);
        let lead_text = scaffold.file(Path::new("lead/murmur.yaml")).unwrap();
        assert!(lead_text.contains("task_acceptance: single"));
        assert!(lead_text.contains("after_task: exit"));
    }

    #[test]
    fn the_driver_key_is_the_providers_reference() {
        for provider in PROVIDERS {
            let config = InferenceConfig {
                provider: provider.name.to_string(),
                ..InferenceConfig::default()
            };
            let (_, lead, worker, _) = files(&choose_provider(Some(&config)).unwrap());
            for manifest in [lead, worker] {
                let driver = manifest
                    .artifacts
                    .iter()
                    .find(|artifact| artifact.name == provider.driver)
                    .expect("the driver is declared");
                assert_eq!(driver.version, provider.driver_version);
                let gateway = driver.gateway.as_ref().unwrap();
                assert_eq!(gateway.endpoint, provider.default_endpoint);
                assert_eq!(
                    gateway.api_key,
                    Some(ApiKeyReference::Environment(provider.key_var.to_string()))
                );
                let inference = manifest.inference.unwrap();
                assert_eq!(inference.model, provider.default_model);
                assert_eq!(inference.driver.unwrap().artifact, provider.driver);
            }
        }
    }

    /// The key a non-comment line writes, when it writes one.
    fn writes_key(line: &str) -> bool {
        let body = line.trim_start().trim_start_matches("- ");
        !line.trim_start().starts_with('#')
            && body.split_once(':').is_some_and(|(key, _)| {
                !key.is_empty() && key.chars().all(|c| c == '_' || c.is_ascii_lowercase())
            })
    }

    #[test]
    fn every_key_is_commented_and_every_top_level_key_links_its_reference() {
        let config = InferenceConfig {
            endpoint: "http://127.0.0.1:9999".to_string(),
            model: "a model: with # in it".to_string(),
            ..InferenceConfig::default()
        };
        for choice in [default_choice(), choose_provider(Some(&config)).unwrap()] {
            let scaffold = render_scaffold("crew", &choice);
            for (path, text) in &scaffold.files {
                let lines: Vec<&str> = text.lines().collect();
                for (index, line) in lines.iter().enumerate() {
                    if !writes_key(line) {
                        continue;
                    }
                    let above = index.checked_sub(1).map(|above| lines[above].trim_start());
                    assert!(
                        line.contains(" # ") || above.is_some_and(|above| above.starts_with('#')),
                        "{}: line {} has no comment: {line}",
                        path.display(),
                        index + 1
                    );
                    if !line.starts_with(' ') {
                        let block = lines[..index]
                            .iter()
                            .rev()
                            .take_while(|above| above.starts_with('#'));
                        assert!(
                            block
                                .clone()
                                .any(|above| above.contains(&format!("{DOCS}/"))),
                            "{}: top-level key {line} links no reference",
                            path.display()
                        );
                    }
                }
            }
        }
    }

    /// Every docs link in the generated files names a page and an anchor that exist in the docs
    /// source, so the comments never point at a section the site does not have.
    #[test]
    fn every_reference_link_resolves() {
        let docs = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/content/reference");
        let scaffold = render_scaffold("crew", &default_choice());
        let mut checked = 0;
        for (_, text) in &scaffold.files {
            for link in text
                .split_whitespace()
                .filter(|word| word.starts_with(DOCS))
            {
                let (page, anchor) = link[DOCS.len() + 1..].split_once("/#").unwrap();
                let source = fs::read_to_string(docs.join(format!("{page}.md")))
                    .unwrap_or_else(|error| panic!("{link}: {error}"));
                assert!(
                    source.contains(&format!("{{ #{anchor} }}")),
                    "{link}: no {{ #{anchor} }} in {page}.md"
                );
                checked += 1;
            }
        }
        assert!(checked > 10, "only {checked} links checked");
    }

    #[test]
    fn a_configured_endpoint_and_model_are_written_quoted() {
        let config = InferenceConfig {
            provider: "anthropic".to_string(),
            model: "test: model # x".to_string(),
            endpoint: "http://127.0.0.1:4242".to_string(),
            api_key: "not-read".to_string(),
        };
        let choice = choose_provider(Some(&config)).unwrap();
        let (_, lead, _, scaffold) = files(&choice);
        assert_eq!(lead.inference.unwrap().model, "test: model # x");
        assert_eq!(
            lead.artifacts[0].gateway.as_ref().unwrap().endpoint,
            "http://127.0.0.1:4242"
        );
        for (_, text) in &scaffold.files {
            assert!(!text.contains("not-read"));
        }
    }

    #[test]
    fn provider_defaults_and_refusals() {
        assert_eq!(default_choice().provider, &ANTHROPIC);
        let blank = InferenceConfig {
            provider: " ".to_string(),
            ..InferenceConfig::default()
        };
        assert_eq!(choose_provider(Some(&blank)).unwrap(), default_choice());
        let mistral = InferenceConfig {
            provider: "mistral".to_string(),
            ..InferenceConfig::default()
        };
        let error = choose_provider(Some(&mistral)).unwrap_err();
        assert_eq!(error.code, E_NEW_004);
        assert!(error.message.contains("'mistral'"), "{}", error.message);
        assert!(
            error.message.contains("'anthropic' or 'openai'"),
            "{}",
            error.message
        );
    }

    #[test]
    fn names_that_are_not_artifact_names_are_refused() {
        let too_long = "a".repeat(95);
        for name in [
            "Crew",
            "my_crew",
            "-crew",
            "crew/sub",
            "..",
            "",
            too_long.as_str(),
        ] {
            let error = check_formation_name(name).unwrap_err();
            assert_eq!(error.code, E_NEW_002, "{name}");
            assert!(
                error.message.contains(&format!("'{name}'")),
                "{}",
                error.message
            );
        }
        check_formation_name("crew").unwrap();
        check_formation_name(&"a".repeat(93)).unwrap();
    }

    #[test]
    fn the_summary_names_the_files_and_the_steps() {
        assert_eq!(
            render_summary("crew", &default_choice()),
            "Scaffolded formation 'crew' in ./crew
  crew/roster.yaml          lead is the entry member; lead may call worker
  crew/lead/murmur.yaml     capsule crew-lead@0.1.0
  crew/worker/murmur.yaml   capsule crew-worker@0.1.0, serves peers

Next:
  mur config set -g credentials.ANTHROPIC_API_KEY <your key>
  mur install -g murmur-driver-anthropic@0.10.0
  mur build crew/lead && mur install -g crew/lead/crew-lead-0.1.0.mur.zip
  mur build crew/worker && mur install -g crew/worker/crew-worker-0.1.0.mur.zip
  mur run --roster crew --task \"<your task>\"
"
        );
    }

    #[test]
    fn rendering_is_deterministic() {
        let choice = default_choice();
        assert_eq!(
            render_scaffold("crew", &choice),
            render_scaffold("crew", &choice)
        );
    }

    /// The `extra.v.<key>` value in `docs/mkdocs.yml`.
    fn docs_version(mkdocs: &str, key: &str) -> String {
        let prefix = format!("{key}:");
        let line = mkdocs
            .lines()
            .map(str::trim)
            .find(|line| line.starts_with(&prefix))
            .unwrap_or_else(|| panic!("no {key} in docs/mkdocs.yml"));
        line[prefix.len()..].trim().trim_matches('"').to_string()
    }

    #[test]
    fn driver_pins_match_the_docs() {
        let mkdocs =
            fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/mkdocs.yml"))
                .unwrap();
        assert_eq!(
            ANTHROPIC.driver_version,
            docs_version(&mkdocs, "murmur_driver_anthropic")
        );
        assert_eq!(
            OPENAI.driver_version,
            docs_version(&mkdocs, "murmur_driver_openai")
        );
    }
}
