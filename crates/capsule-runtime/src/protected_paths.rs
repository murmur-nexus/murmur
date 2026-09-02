//! `capabilities.filesystem.read_only`: workdir subtrees a capsule may read but must not write,
//! lowered once and answered in one place.
//!
//! The threat this addresses is a manipulated model steering an honest tool — the agent cannot
//! make the code pass the test, so it edits the test. That failure is invisible in a green run.
//! The check runs on the *resolved* call, at dispatch, before anything is spawned or instantiated
//! and before a policy hook is asked: the manifest is the authority, and a hook can only narrow
//! further.
//!
//! # What the write-intent analyser can and cannot see
//!
//! It flags only what it can positively identify as a write. Everything it cannot identify is not
//! flagged, and that set is written down here rather than implied:
//!
//! | Not flagged | Why |
//! |---|---|
//! | An interpreter's own file I/O (`python3 -c "open(p,'w')…"`) | The path is inside an opaque argument; nothing names a redirection or a recognized verb. Staging fires `W-SEC-017` for this. |
//! | Command substitution (`$(…)`, backticks), `eval`, `exec 3>f` | The write target is produced by a shell evaluation the analyser does not perform. |
//! | A binary outside [`SHELL_WRITE_VERBS`] | The table names what is recognized; an unrecognized binary's argument positions have no meaning here. |
//! | A tool input carrying no [`TOOL_DESTINATION_KEYS`] key and no [`TOOL_PATH_KEYS`]/[`TOOL_CONTENT_KEYS`] pairing | A path alone is a read. |
//! | A malicious artifact writing wherever its preopen allows | Dispatch sees innocuous input. Only a narrow `capabilities.filesystem.scope` on that artifact's entry stops it. |
//! | A path inside a subtree a tool's own schema declared [`crate::tool_annotations::FORMAT_OPAQUE`] | The key-name heuristic does not descend there, so a destination the tool did not also declare is not seen. The tool author's declaration is taken at its word; the tool author is not this layer's adversary. |
//! | A destination whose declaration the lowering could not read — behind a `$ref`, past [`crate::tool_annotations`]'s depth bound, in an unparsable schema, or in an artifact `manage.pull()` fetched after staging | The tool keeps the key-name heuristic, which is the conservative direction: it can only miss a declared destination, never permit one it looked at. |
//!
//! This layer is the only one that covers the tool path, which is where the threat lives, and the
//! only portable one — Landlock does not exist on macOS or on older Linux kernels. A
//! kernel-enforced layer would consume [`ProtectedPaths::absolute_roots`]; nothing here calls it
//! from `sandbox.rs` or from any Landlock path.

use std::path::{Component, Path, PathBuf};

use crate::errors::RuntimeError;
use crate::hooks::ResolvedCall;
use crate::tool_annotations::{LocationStep, ToolAnnotationMap, ToolAnnotations};

/// One lowered `capabilities.filesystem.read_only` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProtectedPathRule {
    /// The entry exactly as the manifest wrote it, so a refusal quotes the operator's own words
    /// rather than a normalized form they would have to translate back.
    declared: String,
    /// The same entry lexically normalized against the workdir root: no `.` components, and no
    /// `..` (an entry carrying one that escapes is refused at [`ProtectedPaths::from_declared`]).
    relative: PathBuf,
}

impl ProtectedPathRule {
    /// The entry as declared, which is what a refusal names.
    pub(crate) fn declared(&self) -> &str {
        &self.declared
    }
}

/// The declared read-only surface of one capsule, lowered once at staging.
///
/// Empty for the overwhelmingly common capsule that declares nothing, and
/// [`Self::is_empty`] is what keeps the dispatch path from resolving a call at all in that case.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ProtectedPaths {
    rules: Vec<ProtectedPathRule>,
}

/// What a refused call was refused for: the declared rule, the resolved path under it, and the
/// evidence that identified the call as a write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProtectedPathRefusal {
    /// The `read_only` entry as declared.
    pub(crate) rule: String,
    /// The workdir-relative path the call resolved to — never the string the model typed, so two
    /// spellings of one file produce one comparable record.
    pub(crate) path: String,
    pub(crate) signal: WriteSignal,
}

/// The evidence that identified a call as a write. Each variant names the exact thing seen, so a
/// trace reader can tell a redirection from a verb argument from a JSON key without re-deriving it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WriteSignal {
    /// A redirection operator in a `-c` script body, followed by a path.
    ShellRedirection { operator: String },
    /// An argument in a write-target position of a binary in [`SHELL_WRITE_VERBS`].
    ShellArgument { binary: String },
    /// A string value under a [`TOOL_DESTINATION_KEYS`] key, which is a write target on its own.
    ToolDestinationKey { key: String },
    /// A [`TOOL_PATH_KEYS`] key paired, in the same JSON object, with a [`TOOL_CONTENT_KEYS`] key.
    ToolPathWithContent {
        path_key: String,
        content_key: String,
    },
    /// A location the tool's own `input_schema` declared to be a filesystem destination with
    /// [`crate::tool_annotations::FORMAT_DESTINATION`], named as
    /// [`crate::tool_annotations::InputLocation::render`] writes it.
    ToolDeclaredDestination { location: String },
}

impl WriteSignal {
    /// The one-line spelling written to the trace and shown to the model.
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::ShellRedirection { operator } => {
                format!("shell redirection '{operator}' into the path")
            }
            Self::ShellArgument { binary } => {
                format!("write-target argument of '{binary}'")
            }
            Self::ToolDestinationKey { key } => {
                format!("destination key '{key}' in the tool input")
            }
            Self::ToolPathWithContent {
                path_key,
                content_key,
            } => format!("tool input pairs '{path_key}' with '{content_key}'"),
            Self::ToolDeclaredDestination { location } => {
                format!("destination '{location}' declared by the tool's input schema")
            }
        }
    }
}

/// Which of a recognized binary's non-flag arguments are write targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriteTargets {
    /// Every non-flag argument, e.g. `rm a b c`.
    All,
    /// The last non-flag argument only, e.g. `cp src dst`.
    Last,
    /// The file arguments, and only when `-i`/`--in-place` is present.
    InPlaceOnly,
    /// The value of an `of=` operand, e.g. `dd if=a of=b`.
    DdOperand,
}

/// The binaries whose argument positions the analyser reads, and which of their arguments are
/// write targets.
///
/// A closed table on purpose: an entry added here is a claim that the runtime knows where that
/// binary writes, and a binary absent from it is simply not analysed rather than guessed at.
pub(crate) const SHELL_WRITE_VERBS: &[(&str, WriteTargets)] = &[
    ("tee", WriteTargets::All),
    ("rm", WriteTargets::All),
    ("rmdir", WriteTargets::All),
    ("unlink", WriteTargets::All),
    ("shred", WriteTargets::All),
    ("truncate", WriteTargets::All),
    ("mkdir", WriteTargets::All),
    ("touch", WriteTargets::All),
    ("chmod", WriteTargets::All),
    ("chown", WriteTargets::All),
    ("patch", WriteTargets::All),
    ("mv", WriteTargets::Last),
    ("cp", WriteTargets::Last),
    ("install", WriteTargets::Last),
    ("ln", WriteTargets::Last),
    ("sed", WriteTargets::InPlaceOnly),
    ("dd", WriteTargets::DdOperand),
];

/// Tool-input keys whose string value is a write target on its own — the destination half of a
/// copy, move or render, which carries no content key because the content is elsewhere.
pub(crate) const TOOL_DESTINATION_KEYS: &[&str] = &[
    "dest",
    "dest_path",
    "destination",
    "destination_path",
    "target_path",
    "output_path",
    "out_path",
    "new_path",
    "to",
];

/// Tool-input keys naming a file. A write target **only** when the same JSON object also carries
/// a [`TOOL_CONTENT_KEYS`] key: a path on its own is a read.
pub(crate) const TOOL_PATH_KEYS: &[&str] = &["path", "file_path", "filepath", "filename", "file"];

/// Tool-input keys carrying bytes to put somewhere. Their presence is what turns a
/// [`TOOL_PATH_KEYS`] sibling into a write target.
pub(crate) const TOOL_CONTENT_KEYS: &[&str] = &[
    "content",
    "contents",
    "text",
    "data",
    "body",
    "new_str",
    "new_string",
    "replacement",
    "patch",
    "diff",
];

/// Interpreters the shell half of the analyser cannot follow, beyond the shells
/// [`crate::shell::is_shell_interpreter`] already names.
///
/// A capsule allowlisting one of these can construct a write in a form the analyser does not
/// recognize, so `read_only` is advisory for that binary until the kernel layer lands. Names the
/// binaries rather than pretending the boundary covers them — see `W-SEC-017`.
pub(crate) const ADVISORY_INTERPRETERS: &[&str] = &[
    "python", "python3", "perl", "ruby", "node", "deno", "bun", "php", "awk", "gawk", "mawk",
    "lua", "tclsh", "Rscript",
];

/// Whether an allowlisted binary can construct a write the shell analyser cannot see.
pub(crate) fn is_advisory_interpreter(binary: &str) -> bool {
    crate::shell::is_shell_interpreter(binary) || ADVISORY_INTERPRETERS.contains(&binary)
}

impl ProtectedPaths {
    /// Validate and lower the declared entries.
    ///
    /// Refuses an absolute entry and one that escapes the workdir via `..`, on exactly the terms
    /// `network_policy::validate_filesystem_scope` refuses the same shapes for
    /// `capabilities.filesystem.scope`. Called at staging so a malformed entry fails the launch
    /// rather than a call at a time.
    pub(crate) fn from_declared(declared: &[String]) -> Result<Self, RuntimeError> {
        let mut rules = Vec::with_capacity(declared.len());
        for entry in declared {
            rules.push(ProtectedPathRule {
                declared: entry.clone(),
                relative: lower_declared_entry(entry)?,
            });
        }
        Ok(Self { rules })
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// The rule covering `candidate`, resolved against `workdir`, with the resolved
    /// workdir-relative path.
    ///
    /// `None` when nothing covers it, and `None` when it lexically resolves outside the workdir —
    /// a path outside the workdir is not this mechanism's business, and what happens to it is the
    /// preopen's and the kernel's decision.
    ///
    /// Canonicalizes the deepest *existing* ancestor before matching, so a pre-existing symlink
    /// into a protected subtree resolves to the rule that covers its target. Matching is
    /// component-wise: a rule names a subtree of the workdir root, not a path fragment, so `tests`
    /// covers `tests/a` and does not cover `tests2`.
    pub(crate) fn covering_rule(
        &self,
        workdir: &Path,
        candidate: &str,
    ) -> Option<(&ProtectedPathRule, String)> {
        let relative = resolve_within_workdir(workdir, candidate)?;
        let rule = self
            .rules
            .iter()
            .find(|rule| starts_with_components(&relative, &rule.relative))?;
        Some((rule, display_relative(&relative)))
    }

    /// The absolute subtree roots, shortest covering root per declaration, in declaration order
    /// and without duplicates.
    ///
    /// The enumeration input for a kernel-enforced layer, which has to turn "deny these subtrees"
    /// into "allow everything else" because Landlock has only allow rules.
    ///
    /// Nothing outside this module's tests calls it: the dispatch check works from
    /// [`Self::covering_rule`], and wiring these roots into `sandbox.rs` without also withdrawing
    /// the blanket workdir write grant would enumerate a deny set as an allow set and widen what
    /// the capsule may write.
    #[allow(dead_code)]
    pub(crate) fn absolute_roots(&self, workdir: &Path) -> Vec<PathBuf> {
        let mut roots: Vec<PathBuf> = Vec::new();
        for rule in &self.rules {
            // A nested declaration collapses into the shortest root that covers it: granting the
            // parent already governs the child, and enumerating both would make the kernel layer
            // build two rules for one subtree.
            let covered_by_another = self.rules.iter().any(|other| {
                other.relative != rule.relative
                    && starts_with_components(&rule.relative, &other.relative)
            });
            if covered_by_another {
                continue;
            }
            let absolute = workdir.join(&rule.relative);
            if !roots.contains(&absolute) {
                roots.push(absolute);
            }
        }
        roots
    }

    /// The single write-intent pass over a resolved call. `None` means the analyser found nothing
    /// it can positively identify as a write into a declared subtree — which is not a claim that
    /// the call writes nothing.
    ///
    /// `annotations` is consulted for a tool call only, and only to decide *where* the analyser
    /// looks: a shell call never reaches it, and no entry in it can make a path the analyser looks
    /// at be permitted.
    pub(crate) fn check_call(
        &self,
        workdir: &Path,
        call: &ResolvedCall,
        annotations: &ToolAnnotationMap,
    ) -> Option<ProtectedPathRefusal> {
        if self.is_empty() {
            return None;
        }
        match call {
            ResolvedCall::Shell {
                binary,
                argv,
                script,
                ..
            } => self.check_shell(workdir, binary, argv, script.as_deref()),
            ResolvedCall::Tool {
                tool_name, input, ..
            } => self.check_tool(workdir, input, annotations.for_tool(tool_name)),
        }
    }

    /// The shell arm: the `-c` script body when there is one, the resolved argv otherwise.
    ///
    /// Two different readings on purpose. A script body goes to a shell, so its redirection
    /// operators and command separators are live and are read as such. An argv does not: no shell
    /// is involved, so a `>` among the arguments is a literal argument and nothing is redirected —
    /// only the binary's own write-target positions can say anything.
    fn check_shell(
        &self,
        workdir: &Path,
        binary: &str,
        argv: &[String],
        script: Option<&str>,
    ) -> Option<ProtectedPathRefusal> {
        let segments = match script {
            Some(body) => scan_script(body),
            None => vec![Segment {
                binary: Some(binary_name(binary).to_string()),
                args: argv.to_vec(),
                redirections: Vec::new(),
            }],
        };
        for segment in &segments {
            if let Some(refusal) = self.check_segment(workdir, segment) {
                return Some(refusal);
            }
        }
        None
    }

    fn check_segment(&self, workdir: &Path, segment: &Segment) -> Option<ProtectedPathRefusal> {
        for (operator, target) in &segment.redirections {
            if let Some((rule, path)) = self.covering_rule(workdir, target) {
                return Some(ProtectedPathRefusal {
                    rule: rule.declared().to_string(),
                    path,
                    signal: WriteSignal::ShellRedirection {
                        operator: operator.clone(),
                    },
                });
            }
        }
        let binary = segment.binary.as_deref()?;
        let targets = SHELL_WRITE_VERBS
            .iter()
            .find(|(name, _)| *name == binary)
            .map(|(_, targets)| *targets)?;
        for candidate in write_target_arguments(targets, &segment.args) {
            if let Some((rule, path)) = self.covering_rule(workdir, &candidate) {
                return Some(ProtectedPathRefusal {
                    rule: rule.declared().to_string(),
                    path,
                    signal: WriteSignal::ShellArgument {
                        binary: binary.to_string(),
                    },
                });
            }
        }
        None
    }

    /// The tool arm: the locations the tool declared as destinations first, then the key-name
    /// heuristic over the input JSON, evaluating the pairing rule per object so a nested edit list
    /// is read the same way a flat input is.
    ///
    /// A declared destination is asked first for the same reason a destination *key* is: it is a
    /// write target on its own, and it is the more precise thing to name in the refusal.
    fn check_tool(
        &self,
        workdir: &Path,
        input: &str,
        annotations: &ToolAnnotations,
    ) -> Option<ProtectedPathRefusal> {
        let value: serde_json::Value = serde_json::from_str(input).ok()?;
        if let Some(refusal) = self.check_declared_destinations(workdir, &value, annotations) {
            return Some(refusal);
        }
        self.walk_json(workdir, &value, &mut Vec::new(), annotations)
    }

    /// Every location the schema declared a destination, checked wherever in the input it sits.
    ///
    /// Read from the schema rather than through the heuristic walk, so a destination inside a
    /// subtree the same schema declared opaque is still checked.
    fn check_declared_destinations(
        &self,
        workdir: &Path,
        value: &serde_json::Value,
        annotations: &ToolAnnotations,
    ) -> Option<ProtectedPathRefusal> {
        for location in annotations.destinations() {
            for candidate in location.resolve(value) {
                if let Some((rule, path)) = self.covering_rule(workdir, candidate) {
                    return Some(ProtectedPathRefusal {
                        rule: rule.declared().to_string(),
                        path,
                        signal: WriteSignal::ToolDeclaredDestination {
                            location: location.render(),
                        },
                    });
                }
            }
        }
        None
    }

    /// `at` is the location of `value` in the input, which is what an opaque declaration is
    /// matched against. It is a scratch buffer: every push is popped before returning.
    fn walk_json(
        &self,
        workdir: &Path,
        value: &serde_json::Value,
        at: &mut Vec<LocationStep>,
        annotations: &ToolAnnotations,
    ) -> Option<ProtectedPathRefusal> {
        // Only a container can be opaque. On a string the declaration is ignored and the heuristic
        // keeps running, so an annotation can never remove a check on a value.
        if (value.is_object() || value.is_array()) && annotations.is_opaque(at) {
            return None;
        }
        match value {
            serde_json::Value::Object(map) => {
                // A destination key is a write target on its own, so it is asked first: an input
                // carrying both a destination and a path+content pair names the destination.
                for (key, entry) in map {
                    let Some(text) = entry.as_str() else { continue };
                    if !matches_key(TOOL_DESTINATION_KEYS, key) {
                        continue;
                    }
                    if let Some((rule, path)) = self.covering_rule(workdir, text) {
                        return Some(ProtectedPathRefusal {
                            rule: rule.declared().to_string(),
                            path,
                            signal: WriteSignal::ToolDestinationKey { key: key.clone() },
                        });
                    }
                }
                let content_key = map
                    .iter()
                    .find(|(key, _)| matches_key(TOOL_CONTENT_KEYS, key))
                    .map(|(key, _)| key.clone());
                if let Some(content_key) = content_key {
                    for (key, entry) in map {
                        let Some(text) = entry.as_str() else { continue };
                        if !matches_key(TOOL_PATH_KEYS, key) {
                            continue;
                        }
                        if let Some((rule, path)) = self.covering_rule(workdir, text) {
                            return Some(ProtectedPathRefusal {
                                rule: rule.declared().to_string(),
                                path,
                                signal: WriteSignal::ToolPathWithContent {
                                    path_key: key.clone(),
                                    content_key: content_key.clone(),
                                },
                            });
                        }
                    }
                }
                map.iter().find_map(|(key, entry)| {
                    at.push(LocationStep::Key(key.clone()));
                    let refusal = self.walk_json(workdir, entry, at, annotations);
                    at.pop();
                    refusal
                })
            }
            serde_json::Value::Array(items) => {
                at.push(LocationStep::Element);
                let refusal = items
                    .iter()
                    .find_map(|entry| self.walk_json(workdir, entry, at, annotations));
                at.pop();
                refusal
            }
            _ => None,
        }
    }
}

/// Key matching is case-insensitive with `-` and `_` folded, so `newPath`, `new-path` and
/// `new_path` are one key.
pub(crate) fn matches_key(table: &[&str], key: &str) -> bool {
    let folded: String = key
        .chars()
        .filter(|c| *c != '-' && *c != '_')
        .flat_map(char::to_lowercase)
        .collect();
    table.iter().any(|candidate| {
        let candidate_folded: String = candidate
            .chars()
            .filter(|c| *c != '-' && *c != '_')
            .collect();
        candidate_folded == folded
    })
}

/// Lower one declared entry to its workdir-relative form, refusing the shapes a subtree of the
/// workdir cannot have.
///
/// The shape rule itself is [`crate::network_policy::lower_workdir_subpath`], which
/// `capabilities.filesystem.scope` is held to as well — one definition, so the two declarations
/// cannot drift into accepting different paths. Only the diagnostic differs.
fn lower_declared_entry(entry: &str) -> Result<PathBuf, RuntimeError> {
    crate::network_policy::lower_workdir_subpath(entry).map_err(|rejection| {
        RuntimeError::InvalidReadOnlyPath {
            path: entry.to_string(),
            message: rejection.message("read-only path"),
        }
    })
}

/// Resolve one candidate — the string a model typed — to its workdir-relative form, or `None`
/// when it does not name something inside the workdir.
///
/// A relative candidate is walked with the same depth rule the declared entries are: a `..` that
/// pops past the workdir root leaves, and stays left, even if later components would re-enter.
/// A capsule that reached outside the workdir has left this mechanism's subject, and re-entering
/// by name does not put it back.
fn resolve_within_workdir(workdir: &Path, candidate: &str) -> Option<String> {
    if candidate.is_empty() {
        return None;
    }
    let candidate_path = Path::new(candidate);
    let lexical = if candidate_path.is_absolute() {
        let normalized = normalize_absolute(candidate_path);
        let root = normalize_absolute(workdir);
        normalized.strip_prefix(&root).ok()?.to_path_buf()
    } else {
        lower_declared_entry(candidate).ok()?
    };

    // The deepest existing ancestor is canonicalized before matching, so a pre-existing symlink
    // into a protected subtree resolves to the subtree it points at rather than to its own name.
    let canonical_workdir = workdir
        .canonicalize()
        .unwrap_or_else(|_| workdir.to_path_buf());
    let resolved = canonicalize_existing_prefix(&canonical_workdir.join(&lexical));
    let relative = resolved
        .strip_prefix(&canonical_workdir)
        .ok()?
        .to_path_buf();
    Some(relative.to_string_lossy().to_string())
}

/// Lexically normalize an absolute path: drop `.`, apply `..` against what is already accumulated.
/// No filesystem access, so it is safe to run on a path that does not exist.
fn normalize_absolute(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Canonicalize the longest existing prefix of `path` and re-append the components below it.
fn canonicalize_existing_prefix(path: &Path) -> PathBuf {
    if let Ok(real) = path.canonicalize() {
        return real;
    }
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cursor = path.to_path_buf();
    while let Some(name) = cursor.file_name().map(|n| n.to_os_string()) {
        tail.push(name);
        if !cursor.pop() {
            break;
        }
        if let Ok(real) = cursor.canonicalize() {
            let mut out = real;
            for part in tail.iter().rev() {
                out.push(part);
            }
            return out;
        }
    }
    path.to_path_buf()
}

/// Component-wise prefix test. `tests` is a prefix of `tests/a`, and of `tests` itself; it is not
/// a prefix of `tests2`, `testsuite/a` or `build/tests`.
fn starts_with_components(candidate: impl AsRef<Path>, rule: &Path) -> bool {
    candidate.as_ref().starts_with(rule)
}

fn display_relative(relative: &str) -> String {
    relative.replace(std::path::MAIN_SEPARATOR, "/")
}

/// The last path segment of a resolved binary path, which is what [`SHELL_WRITE_VERBS`] is keyed
/// on: `resolve_invoked_binary_path` hands back `/usr/bin/rm` where the table says `rm`.
fn binary_name(binary: &str) -> &str {
    Path::new(binary)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(binary)
}

/// Which of a command's arguments the table says are write targets.
fn write_target_arguments(targets: WriteTargets, args: &[String]) -> Vec<String> {
    match targets {
        WriteTargets::All => args.iter().filter(|a| !is_flag(a)).cloned().collect(),
        WriteTargets::Last => args
            .iter()
            .rfind(|a| !is_flag(a))
            .cloned()
            .into_iter()
            .collect(),
        WriteTargets::InPlaceOnly => {
            let in_place = args.iter().any(|a| {
                a == "-i" || a == "--in-place" || (a.starts_with("-i") && !a.starts_with("--"))
            });
            if !in_place {
                return Vec::new();
            }
            // The first non-flag argument of an in-place `sed` is the script, not a file.
            args.iter()
                .filter(|a| !is_flag(a))
                .skip(1)
                .cloned()
                .collect()
        }
        WriteTargets::DdOperand => args
            .iter()
            .filter_map(|a| a.strip_prefix("of=").map(str::to_string))
            .collect(),
    }
}

fn is_flag(arg: &str) -> bool {
    arg.len() > 1 && arg.starts_with('-')
}

/// One command in a script body: its binary, its arguments, and the write redirections attached
/// to it.
#[derive(Debug, Default, PartialEq, Eq)]
struct Segment {
    binary: Option<String>,
    args: Vec<String>,
    /// `(operator, target)` for each recognized write redirection, in source order.
    redirections: Vec<(String, String)>,
}

/// Split a `-c` script body into commands, separating write redirections from words.
///
/// Deliberately shallow: quoting and escaping are honoured so a path inside quotes is still read,
/// and `;`, `|`, `&&`, `||` and `&` end a command — but nothing here evaluates a substitution, an
/// expansion or an alias. A write the shell would produce from any of those is not seen, which is
/// the limit this module's header states.
fn scan_script(body: &str) -> Vec<Segment> {
    let chars: Vec<char> = body.chars().collect();
    let mut segments = Vec::new();
    let mut current = Segment::default();
    let mut i = 0usize;

    let flush = |current: &mut Segment, segments: &mut Vec<Segment>| {
        if current.binary.is_some() || !current.redirections.is_empty() {
            segments.push(std::mem::take(current));
        } else {
            *current = Segment::default();
        }
    };

    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        match c {
            ';' | '\n' => {
                flush(&mut current, &mut segments);
                i += 1;
            }
            '|' => {
                flush(&mut current, &mut segments);
                i += if chars.get(i + 1) == Some(&'|') { 2 } else { 1 };
            }
            '&' if chars.get(i + 1) == Some(&'>') => {
                let (operator, target, next) = read_redirection(&chars, i);
                if let Some(target) = target {
                    current.redirections.push((operator, target));
                }
                i = next;
            }
            '&' => {
                flush(&mut current, &mut segments);
                i += if chars.get(i + 1) == Some(&'&') { 2 } else { 1 };
            }
            '<' => {
                // A read redirection: its target is consumed so it is never mistaken for an
                // argument, and it is never a write.
                i += 1;
                while chars.get(i) == Some(&'<') {
                    i += 1;
                }
                let (_, next) = read_word(&chars, skip_spaces(&chars, i));
                i = next;
            }
            '>' => {
                let (operator, target, next) = read_redirection(&chars, i);
                if let Some(target) = target {
                    current.redirections.push((operator, target));
                }
                i = next;
            }
            _ => {
                // A run of digits immediately before `>` is that redirection's fd, not a word.
                let digits_end = chars[i..]
                    .iter()
                    .position(|c| !c.is_ascii_digit())
                    .map(|offset| i + offset)
                    .unwrap_or(chars.len());
                if digits_end > i && chars.get(digits_end) == Some(&'>') {
                    let (operator, target, next) = read_redirection(&chars, i);
                    if let Some(target) = target {
                        current.redirections.push((operator, target));
                    }
                    i = next;
                    continue;
                }
                let (word, next) = read_word(&chars, i);
                i = next;
                let Some(word) = word else { continue };
                match current.binary {
                    // `VAR=value cmd` and `env cmd` are not unwrapped: the first word is read as
                    // the binary whatever it is, and an unrecognized one is simply not analysed.
                    None => current.binary = Some(binary_name(&word).to_string()),
                    Some(_) => current.args.push(word),
                }
            }
        }
    }
    flush(&mut current, &mut segments);
    segments
}

fn skip_spaces(chars: &[char], mut i: usize) -> usize {
    while matches!(chars.get(i), Some(c) if c.is_whitespace() && *c != '\n') {
        i += 1;
    }
    i
}

/// Read one redirection operator starting at `i` and the path it redirects into.
///
/// Recognizes `>`, `>>`, `>|`, `&>`, `&>>` and the numbered `N>` / `N>>`. Returns `None` for the
/// target when the operator is followed by an fd duplication (`2>&1`), which writes to no path.
fn read_redirection(chars: &[char], mut i: usize) -> (String, Option<String>, usize) {
    let mut operator = String::new();
    while matches!(chars.get(i), Some(c) if c.is_ascii_digit()) {
        operator.push(chars[i]);
        i += 1;
    }
    if chars.get(i) == Some(&'&') {
        operator.push('&');
        i += 1;
    }
    // The caller only enters here on a `>` or on something that provably precedes one.
    operator.push('>');
    i += 1;
    match chars.get(i) {
        Some('>') => {
            operator.push('>');
            i += 1;
        }
        Some('|') => {
            operator.push('|');
            i += 1;
        }
        _ => {}
    }
    let mut cursor = skip_spaces(chars, i);
    if chars.get(cursor) == Some(&'&') {
        // `2>&1` and `>&-`: a descriptor, not a path.
        cursor += 1;
        while matches!(chars.get(cursor), Some(c) if c.is_ascii_digit() || *c == '-') {
            cursor += 1;
        }
        return (operator, None, cursor);
    }
    let (word, next) = read_word(chars, cursor);
    (operator, word, next)
}

/// Read one shell word, honouring single quotes, double quotes and backslash escapes, and
/// stopping at whitespace or an unquoted metacharacter.
fn read_word(chars: &[char], mut i: usize) -> (Option<String>, usize) {
    let mut word = String::new();
    let mut saw_any = false;
    let mut quote: Option<char> = None;
    while i < chars.len() {
        let c = chars[i];
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else if c == '\\' && q == '"' && i + 1 < chars.len() {
                    i += 1;
                    word.push(chars[i]);
                } else {
                    word.push(c);
                }
                i += 1;
            }
            None => {
                if c == '\'' || c == '"' {
                    quote = Some(c);
                    saw_any = true;
                    i += 1;
                } else if c == '\\' && i + 1 < chars.len() {
                    word.push(chars[i + 1]);
                    saw_any = true;
                    i += 2;
                } else if c.is_whitespace() || matches!(c, ';' | '|' | '&' | '<' | '>') {
                    break;
                } else {
                    word.push(c);
                    saw_any = true;
                    i += 1;
                }
            }
        }
    }
    if saw_any {
        (Some(word), i)
    } else {
        // No word here means the cursor sits on a metacharacter the caller does not otherwise
        // consume. Advance past it anyway: returning the cursor unmoved would spin `scan_script`.
        (None, i + usize::from(i < chars.len()))
    }
}

#[cfg(test)]
impl ProtectedPaths {
    /// [`Self::check_call`] for a tool that declared no annotations, which is what every case
    /// below that names no schema asserts on.
    fn check_unannotated(
        &self,
        workdir: &Path,
        call: &ResolvedCall,
    ) -> Option<ProtectedPathRefusal> {
        self.check_call(workdir, call, &ToolAnnotationMap::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn paths(entries: &[&str]) -> ProtectedPaths {
        let declared: Vec<String> = entries.iter().map(|e| e.to_string()).collect();
        ProtectedPaths::from_declared(&declared).expect("fixture entries must lower")
    }

    fn shell(script: &str) -> ResolvedCall {
        ResolvedCall::Shell {
            binary: "/bin/bash".to_string(),
            command: script.to_string(),
            argv: vec!["-c".to_string(), script.to_string()],
            script: Some(script.to_string()),
            recipe: None,
        }
    }

    fn tool(input: serde_json::Value) -> ResolvedCall {
        let input = input.to_string();
        ResolvedCall::Tool {
            tool_name: "writer".to_string(),
            input_bytes: input.len() as u64,
            input,
        }
    }

    // ── Declared entry validation ────────────────────────────────────────────

    /// The three shapes a workdir subtree cannot have are refused at lowering, so a session never
    /// runs against a rule the runtime could not build.
    #[test]
    fn a_malformed_entry_is_refused_at_lowering() {
        for entry in ["/etc", "../outside", "tests/../../outside"] {
            let err = ProtectedPaths::from_declared(&[entry.to_string()])
                .expect_err("must refuse: {entry}");
            assert!(
                matches!(&err, RuntimeError::InvalidReadOnlyPath { path, .. } if path == entry),
                "{entry}: {err}"
            );
        }
    }

    #[test]
    fn a_usable_entry_lowers_and_a_declaration_free_capsule_is_empty() {
        assert!(ProtectedPaths::from_declared(&[]).unwrap().is_empty());
        assert!(!paths(&["tests"]).is_empty());
        assert!(paths(&["./tests/../tests"])
            .covering_rule(Path::new("/nowhere"), "tests/a")
            .is_some());
    }

    // ── covering_rule ────────────────────────────────────────────────────────

    /// A rule names a subtree of the workdir root, not a path fragment.
    #[test]
    fn covering_rule_compares_components_not_string_prefixes() {
        let protected = paths(&["tests"]);
        let workdir = Path::new("/nowhere/work");
        for covered in ["tests", "tests/a", "tests/a/b", "./tests/a"] {
            assert!(
                protected.covering_rule(workdir, covered).is_some(),
                "must be covered: {covered}"
            );
        }
        for uncovered in ["tests2", "testsuite/a", "atests", "build/tests"] {
            assert!(
                protected.covering_rule(workdir, uncovered).is_none(),
                "must not be covered: {uncovered}"
            );
        }
    }

    /// A path outside the workdir is not this mechanism's business, and one that leaves cannot
    /// re-enter by naming the workdir again.
    #[test]
    fn covering_rule_declines_everything_outside_the_workdir() {
        let tmp = tempfile::tempdir().unwrap();
        let workdir = tmp.path().join("work");
        fs::create_dir_all(workdir.join("tests")).unwrap();
        let protected = paths(&["tests"]);

        assert!(protected
            .covering_rule(&workdir, "../elsewhere/x.py")
            .is_none());
        assert!(protected
            .covering_rule(&workdir, "../work/tests/x")
            .is_none());
        assert!(protected.covering_rule(&workdir, "/etc/passwd").is_none());
    }

    /// An absolute path and a symlink both resolve to the rule that covers the real file, and both
    /// report the workdir-relative form rather than the spelling that was typed.
    #[test]
    fn covering_rule_resolves_an_absolute_path_and_a_symlink_to_one_rule() {
        let tmp = tempfile::tempdir().unwrap();
        let workdir = tmp.path();
        fs::create_dir_all(workdir.join("tests")).unwrap();
        fs::write(workdir.join("tests/test_foo.py"), "original").unwrap();
        let protected = paths(&["tests"]);

        let absolute = workdir.join("tests/test_foo.py");
        let (rule, path) = protected
            .covering_rule(workdir, absolute.to_str().unwrap())
            .expect("absolute path resolves");
        assert_eq!(rule.declared(), "tests");
        assert_eq!(path, "tests/test_foo.py");

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(workdir.join("tests"), workdir.join("link")).unwrap();
            let (rule, path) = protected
                .covering_rule(workdir, "link/test_foo.py")
                .expect("a symlink into a protected subtree does not evade the rule");
            assert_eq!(rule.declared(), "tests");
            assert_eq!(path, "tests/test_foo.py");

            // The two forms combined: an absolute path whose own components are a symlink into
            // the subtree. Resolution happens before matching for an absolute candidate too, or
            // spelling the same file through a link would evade the rule that covers it.
            let via_link = workdir.join("link/test_foo.py");
            let (rule, path) = protected
                .covering_rule(workdir, via_link.to_str().unwrap())
                .expect("an absolute path through a symlink resolves to the rule");
            assert_eq!(rule.declared(), "tests");
            assert_eq!(path, "tests/test_foo.py");
        }
    }

    // ── absolute_roots ───────────────────────────────────────────────────────

    /// Declaration order, no duplicates, and a nested declaration collapsed to the shortest
    /// covering root.
    #[test]
    fn absolute_roots_are_declaration_ordered_deduplicated_and_collapsed() {
        let workdir = Path::new("/work");
        assert_eq!(
            paths(&["tests", "bench/fixtures"]).absolute_roots(workdir),
            vec![
                PathBuf::from("/work/tests"),
                PathBuf::from("/work/bench/fixtures")
            ]
        );
        assert_eq!(
            paths(&["tests", "tests/unit"]).absolute_roots(workdir),
            vec![PathBuf::from("/work/tests")]
        );
        assert_eq!(
            paths(&["tests", "./tests"]).absolute_roots(workdir),
            vec![PathBuf::from("/work/tests")]
        );
    }

    // ── The shell write-verb table ───────────────────────────────────────────

    /// Every write form the table claims to recognize is flagged, with the path it names.
    #[test]
    fn every_recognized_shell_write_is_flagged() {
        let protected = paths(&["tests"]);
        let workdir = Path::new("/nowhere/work");
        let flagged = [
            ("echo x > tests/a", "tests/a"),
            ("echo x >> tests/a", "tests/a"),
            ("echo x 2> tests/a", "tests/a"),
            ("echo x &> tests/a", "tests/a"),
            ("tee tests/a", "tests/a"),
            ("rm tests/a", "tests/a"),
            ("rm -rf tests", "tests"),
            ("mv build/x tests/a", "tests/a"),
            ("cp build/x tests/a", "tests/a"),
            ("install build/x tests/a", "tests/a"),
            ("ln -s build/x tests/a", "tests/a"),
            ("touch tests/a", "tests/a"),
            ("truncate -s 0 tests/a", "tests/a"),
            ("mkdir tests/a", "tests/a"),
            ("chmod 777 tests/a", "tests/a"),
            ("chown me tests/a", "tests/a"),
            ("sed -i s/a/b/ tests/a", "tests/a"),
            ("dd of=tests/a", "tests/a"),
            ("patch tests/a", "tests/a"),
        ];
        for (script, expected) in flagged {
            let refusal = protected
                .check_unannotated(workdir, &shell(script))
                .unwrap_or_else(|| panic!("must be flagged: {script}"));
            assert_eq!(refusal.path, expected, "{script}");
            assert_eq!(refusal.rule, "tests", "{script}");
        }
    }

    /// A read is not a write, and a protected path in a source position is not a write either.
    #[test]
    fn a_read_of_a_protected_path_is_not_flagged() {
        let protected = paths(&["tests"]);
        let workdir = Path::new("/nowhere/work");
        for script in [
            "cat tests/a",
            "grep x tests/a",
            "ls tests",
            "mv tests/a build/x",
            "cp tests/a build/x",
            "echo tests/a",
            "sed s/a/b/ tests/a",
            "dd if=tests/a of=build/x",
        ] {
            assert!(
                protected
                    .check_unannotated(workdir, &shell(script))
                    .is_none(),
                "must not be flagged: {script}"
            );
        }
    }

    /// The redirection is named as the signal, and the verb argument as its own.
    #[test]
    fn the_signal_names_what_identified_the_write() {
        let protected = paths(&["tests"]);
        let workdir = Path::new("/nowhere/work");
        assert_eq!(
            protected
                .check_unannotated(workdir, &shell("echo broken > tests/test_foo.py"))
                .unwrap()
                .signal,
            WriteSignal::ShellRedirection {
                operator: ">".to_string()
            }
        );
        assert_eq!(
            protected
                .check_unannotated(workdir, &shell("tee tests/a"))
                .unwrap()
                .signal,
            WriteSignal::ShellArgument {
                binary: "tee".to_string()
            }
        );
    }

    /// A command after a separator is analysed on its own, and an fd duplication redirects into
    /// no path at all.
    #[test]
    fn separators_split_commands_and_an_fd_duplication_is_not_a_path() {
        let protected = paths(&["tests"]);
        let workdir = Path::new("/nowhere/work");
        assert!(protected
            .check_unannotated(workdir, &shell("cat build/x && rm tests/a"))
            .is_some());
        assert!(protected
            .check_unannotated(workdir, &shell("cat tests/a 2>&1"))
            .is_none());
    }

    /// No shell is involved in the argv form, so a `>` among the arguments is a literal argument.
    #[test]
    fn the_argv_form_reads_write_verbs_and_not_redirections() {
        let protected = paths(&["tests"]);
        let workdir = Path::new("/nowhere/work");
        let argv_call = |binary: &str, argv: &[&str]| ResolvedCall::Shell {
            binary: binary.to_string(),
            command: argv.join(" "),
            argv: argv.iter().map(|a| a.to_string()).collect(),
            script: None,
            recipe: None,
        };
        assert!(protected
            .check_unannotated(workdir, &argv_call("/usr/bin/rm", &["-rf", "tests"]))
            .is_some());
        assert!(protected
            .check_unannotated(
                workdir,
                &argv_call("/usr/bin/curl", &["-o", ">", "tests/a"])
            )
            .is_none());
    }

    // ── The tool key tables ──────────────────────────────────────────────────

    /// A path paired with content is a write; a path alone is a read.
    #[test]
    fn a_tool_path_is_a_write_only_when_the_same_object_carries_content() {
        let protected = paths(&["tests"]);
        let workdir = Path::new("/nowhere/work");

        let refusal = protected
            .check_unannotated(
                workdir,
                &tool(serde_json::json!({"path": "tests/test_foo.py", "content": "pass"})),
            )
            .expect("path plus content is a write");
        assert_eq!(refusal.path, "tests/test_foo.py");
        assert_eq!(refusal.rule, "tests");
        assert_eq!(
            refusal.signal,
            WriteSignal::ToolPathWithContent {
                path_key: "path".to_string(),
                content_key: "content".to_string()
            }
        );

        assert!(protected
            .check_unannotated(
                workdir,
                &tool(serde_json::json!({"path": "tests/test_foo.py"}))
            )
            .is_none());
    }

    /// A destination key is a write on its own, and reading out of a protected subtree into a
    /// writable one is allowed.
    #[test]
    fn a_destination_key_is_a_write_and_a_source_key_is_not() {
        let protected = paths(&["tests"]);
        let workdir = Path::new("/nowhere/work");

        let refusal = protected
            .check_unannotated(
                workdir,
                &tool(serde_json::json!({
                    "source": "build/out.py",
                    "destination": "tests/test_foo.py"
                })),
            )
            .expect("a destination into a protected subtree is a write");
        assert_eq!(refusal.path, "tests/test_foo.py");
        assert_eq!(
            refusal.signal,
            WriteSignal::ToolDestinationKey {
                key: "destination".to_string()
            }
        );

        assert!(
            protected
                .check_unannotated(
                    workdir,
                    &tool(serde_json::json!({
                        "source": "tests/test_foo.py",
                        "destination": "build/out.py"
                    })),
                )
                .is_none(),
            "reading out of a protected subtree into a writable one is allowed"
        );
    }

    /// The pairing rule is evaluated per object, so a nested edit list is read the same way a
    /// flat input is — and key matching folds case, `-` and `_`.
    #[test]
    fn the_pairing_rule_is_per_object_and_key_matching_folds_case_and_separators() {
        let protected = paths(&["tests"]);
        let workdir = Path::new("/nowhere/work");

        assert!(protected
            .check_unannotated(
                workdir,
                &tool(serde_json::json!({
                    "edits": [
                        {"file_path": "build/a.py", "new_string": "x"},
                        {"file_path": "tests/test_foo.py", "new_string": "y"}
                    ]
                })),
            )
            .is_some());
        // The content key lives in a different object from the path, so nothing pairs.
        assert!(protected
            .check_unannotated(
                workdir,
                &tool(serde_json::json!({
                    "content": "x",
                    "nested": {"path": "tests/test_foo.py"}
                })),
            )
            .is_none());
        assert!(protected
            .check_unannotated(
                workdir,
                &tool(serde_json::json!({"New-Path": "tests/test_foo.py"})),
            )
            .is_some());
    }

    /// Input that is not JSON, and input with no matching key, yield nothing.
    #[test]
    fn unreadable_or_unmatched_tool_input_yields_no_refusal() {
        let protected = paths(&["tests"]);
        let workdir = Path::new("/nowhere/work");
        assert!(protected
            .check_unannotated(
                workdir,
                &ResolvedCall::Tool {
                    tool_name: "writer".to_string(),
                    input: "not json at all".to_string(),
                    input_bytes: 15,
                }
            )
            .is_none());
        assert!(protected
            .check_unannotated(workdir, &tool(serde_json::json!({"amount": 10})))
            .is_none());
    }

    /// A capsule that declared nothing is never analysed, whatever the call looks like.
    #[test]
    fn a_capsule_with_no_declaration_flags_nothing() {
        let protected = ProtectedPaths::default();
        let workdir = Path::new("/nowhere/work");
        assert!(protected
            .check_unannotated(workdir, &shell("echo x > tests/a"))
            .is_none());
        assert!(protected
            .check_unannotated(
                workdir,
                &tool(serde_json::json!({"path": "tests/a", "content": "x"}))
            )
            .is_none());
    }

    // ── What a tool's own schema declares ────────────────────────────────────

    fn named_tool(name: &str, input: serde_json::Value) -> ResolvedCall {
        let input = input.to_string();
        ResolvedCall::Tool {
            tool_name: name.to_string(),
            input_bytes: input.len() as u64,
            input,
        }
    }

    /// A tool that declares an object opaque has that object's interior left alone: the
    /// `{file, text}` pair inside a stored note is data, not filesystem intent.
    #[test]
    fn an_opaque_container_is_not_walked_by_the_heuristic() {
        let protected = paths(&["tests"]);
        let workdir = Path::new("/nowhere/work");
        let annotations = ToolAnnotationMap::from_schemas(&[(
            "noter",
            r#"{"type":"object","properties":{"note":{"type":"object","format":"murmur-opaque"}}}"#,
        )]);

        let call = named_tool(
            "noter",
            serde_json::json!({"note": {"file": "tests/test_foo.py", "text": "protected"}}),
        );
        assert!(protected.check_call(workdir, &call, &annotations).is_none());
        assert!(
            protected.check_unannotated(workdir, &call).is_some(),
            "the same input without the declaration is refused, which is what the declaration is for"
        );
    }

    /// `murmur-opaque` names a container. On a string property it is ignored, and the pairing
    /// rule runs on the object that carries it.
    #[test]
    fn an_opaque_string_property_is_ignored() {
        let protected = paths(&["tests"]);
        let workdir = Path::new("/nowhere/work");
        let annotations = ToolAnnotationMap::from_schemas(&[(
            "mislabeled",
            r#"{"type":"object","properties":{"path":{"type":"string","format":"murmur-opaque"}}}"#,
        )]);

        let refusal = protected
            .check_call(
                workdir,
                &named_tool(
                    "mislabeled",
                    serde_json::json!({"path": "tests/test_foo.py", "content": "x"}),
                ),
                &annotations,
            )
            .expect("a string declaration removes no check");
        assert_eq!(
            refusal.signal,
            WriteSignal::ToolPathWithContent {
                path_key: "path".to_string(),
                content_key: "content".to_string()
            }
        );
    }

    /// A declared destination is checked wherever it sits, including under an array step, and the
    /// refusal names the location that triggered it.
    #[test]
    fn a_declared_destination_is_checked_and_names_its_location() {
        let protected = paths(&["tests"]);
        let workdir = Path::new("/nowhere/work");
        let annotations = ToolAnnotationMap::from_schemas(&[(
            "batcher",
            r#"{"type":"object","properties":{"edits":{"type":"array","items":{"type":"object","properties":{"path":{"type":"string","format":"murmur-destination"}}}}}}"#,
        )]);

        let refusal = protected
            .check_call(
                workdir,
                &named_tool(
                    "batcher",
                    serde_json::json!({"edits": [{"path": "tests/test_foo.py"}]}),
                ),
                &annotations,
            )
            .expect("a declared destination under an array element is checked");
        assert_eq!(refusal.path, "tests/test_foo.py");
        assert_eq!(refusal.rule, "tests");
        assert_eq!(
            refusal.signal,
            WriteSignal::ToolDeclaredDestination {
                location: "edits[].path".to_string()
            }
        );
    }

    /// A declaration can only add a check: a destination under a name no table carries is refused
    /// with the declaration and dispatched without it.
    #[test]
    fn a_declared_destination_adds_coverage_the_heuristic_does_not_have() {
        let protected = paths(&["tests"]);
        let workdir = Path::new("/nowhere/work");
        let annotations = ToolAnnotationMap::from_schemas(&[(
            "renderer",
            r#"{"type":"object","properties":{"sink":{"type":"string","format":"murmur-destination"}}}"#,
        )]);

        let call = named_tool("renderer", serde_json::json!({"sink": "tests/test_foo.py"}));
        let refusal = protected
            .check_call(workdir, &call, &annotations)
            .expect("the declared sink is a destination");
        assert_eq!(
            refusal.signal,
            WriteSignal::ToolDeclaredDestination {
                location: "sink".to_string()
            }
        );
        assert!(
            protected.check_unannotated(workdir, &call).is_none(),
            "a path with no content beside it is a read"
        );
    }

    /// An opaque sibling shelters nothing: a destination declared in the same schema is still
    /// checked, and a destination *inside* an opaque subtree is too.
    #[test]
    fn no_declaration_suppresses_a_check_on_a_declared_destination() {
        let protected = paths(&["tests"]);
        let workdir = Path::new("/nowhere/work");
        let annotations = ToolAnnotationMap::from_schemas(&[
            (
                "sneaky",
                r#"{"type":"object","properties":{"path":{"type":"string","format":"murmur-destination"},"body":{"type":"object","format":"murmur-opaque"}}}"#,
            ),
            (
                "buried",
                r#"{"type":"object","properties":{"body":{"type":"object","format":"murmur-opaque","properties":{"sink":{"type":"string","format":"murmur-destination"}}}}}"#,
            ),
        ]);

        let refusal = protected
            .check_call(
                workdir,
                &named_tool(
                    "sneaky",
                    serde_json::json!({
                        "path": "tests/test_foo.py",
                        "content": "x",
                        "body": {"file": "tests/test_foo.py", "text": "note"}
                    }),
                ),
                &annotations,
            )
            .expect("an opaque sibling does not shelter a declared destination");
        assert_eq!(
            refusal.signal,
            WriteSignal::ToolDeclaredDestination {
                location: "path".to_string()
            }
        );

        let refusal = protected
            .check_call(
                workdir,
                &named_tool(
                    "buried",
                    serde_json::json!({"body": {"sink": "tests/test_foo.py"}}),
                ),
                &annotations,
            )
            .expect("a destination inside an opaque subtree is still checked");
        assert_eq!(
            refusal.signal,
            WriteSignal::ToolDeclaredDestination {
                location: "body.sink".to_string()
            }
        );
    }

    /// `murmur-opaque` on the schema's top level names the input root, which is a container like
    /// any other: the key-name heuristic stops there, and a declared destination is still checked.
    #[test]
    fn a_top_level_opaque_declaration_stops_only_the_heuristic() {
        let protected = paths(&["tests"]);
        let workdir = Path::new("/nowhere/work");
        let annotations = ToolAnnotationMap::from_schemas(&[(
            "wholesale",
            r#"{"type":"object","format":"murmur-opaque","properties":{"sink":{"type":"string","format":"murmur-destination"}}}"#,
        )]);

        assert!(
            protected
                .check_call(
                    workdir,
                    &named_tool(
                        "wholesale",
                        serde_json::json!({"path": "tests/test_foo.py", "content": "x"}),
                    ),
                    &annotations,
                )
                .is_none(),
            "the heuristic does not descend into a container the tool declared opaque"
        );

        let refusal = protected
            .check_call(
                workdir,
                &named_tool(
                    "wholesale",
                    serde_json::json!({"sink": "tests/test_foo.py"}),
                ),
                &annotations,
            )
            .expect("a declared destination is checked wherever the opaque boundary sits");
        assert_eq!(
            refusal.signal,
            WriteSignal::ToolDeclaredDestination {
                location: "sink".to_string()
            }
        );
    }

    /// Annotations are keyed by tool name: another tool's declaration is not this tool's.
    #[test]
    fn annotations_belong_to_the_tool_that_declared_them() {
        let protected = paths(&["tests"]);
        let workdir = Path::new("/nowhere/work");
        let annotations = ToolAnnotationMap::from_schemas(&[(
            "noter",
            r#"{"type":"object","properties":{"note":{"type":"object","format":"murmur-opaque"}}}"#,
        )]);

        assert!(
            protected
                .check_call(
                    workdir,
                    &named_tool(
                        "other",
                        serde_json::json!({"note": {"file": "tests/test_foo.py", "text": "x"}}),
                    ),
                    &annotations,
                )
                .is_some(),
            "a tool absent from the map keeps the heuristic"
        );
    }

    /// A shell call consults no annotation, whatever any tool declared.
    #[test]
    fn the_shell_arm_consults_no_annotation() {
        let protected = paths(&["tests"]);
        let workdir = Path::new("/nowhere/work");
        let annotations = ToolAnnotationMap::from_schemas(&[(
            "bash",
            r#"{"type":"object","format":"murmur-opaque"}"#,
        )]);
        assert!(protected
            .check_call(
                workdir,
                &shell("echo broken > tests/test_foo.py"),
                &annotations
            )
            .is_some());
    }

    /// The interpreter set the shell half cannot follow includes the shells and the general
    /// purpose interpreters, and nothing else.
    #[test]
    fn advisory_interpreters_cover_shells_and_general_purpose_interpreters() {
        for binary in ["bash", "sh", "python3", "node", "perl"] {
            assert!(is_advisory_interpreter(binary), "{binary}");
        }
        for binary in ["rm", "tee", "cargo", "jq"] {
            assert!(!is_advisory_interpreter(binary), "{binary}");
        }
    }
}
