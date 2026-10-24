//! `mur version`: this mur's version, the commit it was built from, the WIT level it serves,
//! that level's packages, and the machine-readable commands it serves.

use std::collections::BTreeMap;

use capsule_runtime::served_wit_level;
use murmur_artifact::WitLevel;
use serde::Serialize;

use crate::wit_level::package_table;

/// Every command whose output a program parses, keyed as it is typed after `mur` (the
/// subcommand path plus the flag that selects machine output), with the schema number of that
/// output. Keys are in ascending byte order.
///
/// A number moves only when the output breaks for an existing consumer: a key removed or
/// renamed, a key's type or meaning changed, or a different number of lines or objects. A new
/// key does not move it. Removing a command removes its entry.
pub(crate) const MACHINE_COMMANDS: &[(&str, u32)] = &[
    ("control show --json", 1),
    ("conversation ls --json", 1),
    ("conversation show --json", 1),
    ("eval diff --run --json", 1),
    ("eval show --json", 1),
    ("precompile --json", 1),
    ("ps --json", 1),
    ("run --explain-scope --json", 1),
    ("run --json", 1),
    ("run --roster --json", 1),
    ("token", 1),
    ("trace list --json", 1),
    ("trace rm --json", 1),
    ("version --json", 1),
];

/// The commit this binary was built from, fixed by `build.rs`; `None` when the build had
/// neither `.cargo_vcs_info.json` nor a git checkout.
fn build_commit() -> Option<&'static str> {
    Some(env!("MUR_BUILD_COMMIT")).filter(|commit| !commit.is_empty())
}

/// The `--json` object, keys in this order.
#[derive(Serialize)]
struct VersionReport<'a> {
    mur_version: &'a str,
    commit: Option<&'a str>,
    wit_level: u64,
    packages: Vec<PackageVersion<'a>>,
    commands: BTreeMap<&'static str, u32>,
}

#[derive(Serialize)]
struct PackageVersion<'a> {
    package: &'a str,
    version: &'a str,
}

fn version_report<'a>(
    mur_version: &'a str,
    commit: Option<&'a str>,
    level: &'a WitLevel,
) -> VersionReport<'a> {
    VersionReport {
        mur_version,
        commit,
        wit_level: level.level,
        packages: level
            .packages
            .iter()
            .map(|(package, version)| PackageVersion { package, version })
            .collect(),
        commands: MACHINE_COMMANDS.iter().copied().collect(),
    }
}

/// What `mur version` prints, one entry per line: the pretty-printed JSON object with `json`,
/// otherwise the text form.
fn version_lines(report: &VersionReport<'_>, level: &WitLevel, json: bool) -> Vec<String> {
    if json {
        let text = serde_json::to_string_pretty(report)
            .unwrap_or_else(|error| unreachable!("the version report always serializes: {error}"));
        return vec![text];
    }
    let mut lines = vec![
        format!("mur {}", report.mur_version),
        format!("commit {}", report.commit.unwrap_or("unknown")),
        format!("WIT level {}", report.wit_level),
        String::new(),
    ];
    lines.extend(package_table(level));
    lines
}

pub(crate) fn run_version(json: bool) {
    let level = served_wit_level();
    let report = version_report(env!("CARGO_PKG_VERSION"), build_commit(), level);
    for line in version_lines(&report, level, json) {
        capsule_runtime::report_println!("{line}");
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use clap::CommandFactory;
    use serde_json::Value;

    use super::*;

    #[test]
    fn an_unknown_commit_is_json_null_and_text_commit_unknown() {
        let level = served_wit_level();
        let report = version_report("1.1.0-dev", None, level);

        let json = version_lines(&report, level, true);
        assert_eq!(json.len(), 1, "{json:?}");
        let value: Value = serde_json::from_str(&json[0]).unwrap();
        // `Value` sorts its keys, so their order is read off the text.
        let keys: Vec<&str> = json[0]
            .lines()
            .filter_map(|line| line.strip_prefix("  \""))
            .filter_map(|line| line.split_once('"').map(|(key, _)| key))
            .collect();
        assert_eq!(
            keys,
            ["mur_version", "commit", "wit_level", "packages", "commands"]
        );
        assert_eq!(value["commit"], Value::Null);
        assert_eq!(value["mur_version"], "1.1.0-dev");

        let text = version_lines(&report, level, false);
        assert_eq!(text[0], "mur 1.1.0-dev");
        assert_eq!(text[1], "commit unknown");
        assert_eq!(text[2], format!("WIT level {}", level.level));
    }

    #[test]
    fn a_known_commit_is_printed_as_it_is() {
        let level = served_wit_level();
        let commit = "321640ff1c0d2e3f4a5b6c7d8e9f0a1b2c3d4e5f";
        let report = version_report("1.1.0-dev", Some(commit), level);
        let value: Value = serde_json::from_str(&version_lines(&report, level, true)[0]).unwrap();
        assert_eq!(value["commit"], commit);
        assert_eq!(
            version_lines(&report, level, false)[1],
            format!("commit {commit}")
        );
    }

    /// Every subcommand path of `command`, each with the long flags it takes.
    fn subcommand_paths(
        command: &clap::Command,
        prefix: &[String],
        paths: &mut Vec<(Vec<String>, BTreeSet<String>)>,
    ) {
        for sub in command.get_subcommands() {
            let mut path = prefix.to_vec();
            path.push(sub.get_name().to_string());
            let longs = sub
                .get_arguments()
                .filter_map(|arg| arg.get_long().map(str::to_string))
                .collect();
            paths.push((path.clone(), longs));
            subcommand_paths(sub, &path, paths);
        }
    }

    #[test]
    fn machine_commands_name_every_json_subcommand_and_only_real_ones() {
        let mut paths = Vec::new();
        subcommand_paths(&crate::Cli::command(), &[], &mut paths);
        let keys: BTreeSet<&str> = MACHINE_COMMANDS.iter().map(|(key, _)| *key).collect();

        for (path, longs) in &paths {
            if longs.contains("json") {
                // A `--json` that another flag must select is covered by the key naming both,
                // as `eval diff --run --json`.
                let words = path.join(" ");
                let key = format!("{words} --json");
                let selected = format!("{words} --");
                assert!(
                    keys.contains(key.as_str())
                        || keys
                            .iter()
                            .any(|k| k.starts_with(&selected) && k.ends_with(" --json")),
                    "`mur {key}` has no entry in MACHINE_COMMANDS"
                );
            }
        }

        for (key, number) in MACHINE_COMMANDS {
            assert!(*number >= 1, "`{key}` has schema number {number}");
            let (words, flags): (Vec<&str>, Vec<&str>) =
                key.split(' ').partition(|word| !word.starts_with("--"));
            let Some((_, longs)) = paths.iter().find(|(path, _)| *path == words) else {
                panic!(
                    "`{key}` in MACHINE_COMMANDS names no subcommand `mur {}`",
                    words.join(" ")
                );
            };
            for flag in flags {
                assert!(
                    longs.contains(&flag["--".len()..]),
                    "`{key}` in MACHINE_COMMANDS names {flag}, which `mur {}` does not take",
                    words.join(" ")
                );
            }
        }

        assert!(keys.contains("token"), "MACHINE_COMMANDS has no `token`");
        for pair in MACHINE_COMMANDS.windows(2) {
            assert!(
                pair[0].0 < pair[1].0,
                "MACHINE_COMMANDS is not unique and ascending at `{}`, `{}`",
                pair[0].0,
                pair[1].0
            );
        }
    }
}
