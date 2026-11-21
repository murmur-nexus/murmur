//! `reference/capsule-limits.md` held against the doctor: its `#doctor-causes` table names every
//! `cause` `mur doctor --programs` can print and nothing else, and every limit section carries the
//! rows a reader looks a limit up by.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::PathBuf,
};

use capsule_runtime::program_probe::ProbeCause;

const PAGE: &str = "reference/capsule-limits.md";

/// The rows every limit section's table holds, in this order.
const LIMIT_ROWS: [&str; 5] = ["What fails", "Why", "Lifted by", "Shown by", "Doctor"];

/// Anchors other pages, cards and the doctor's grant text link to.
const REQUIRED_ANCHORS: [&str; 7] = [
    "doctor-causes",
    "no-network",
    "home-folder",
    "curl",
    "namespaces",
    "compiler-helpers",
    "cargo",
];

fn docs_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs")
}

fn page() -> String {
    let path = docs_dir().join("content").join(PAGE);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

/// Every `ProbeCause`, produced by an exhaustive `match`: a variant added to the runtime fails to
/// compile here until it is given a place in the chain, and then fails
/// `the_cause_table_names_exactly_the_causes_the_doctor_prints` until the page has its row.
fn every_cause() -> Vec<ProbeCause> {
    use ProbeCause::*;
    let mut all = Vec::new();
    let mut next = Some(NotInstalled);
    while let Some(cause) = next {
        all.push(cause);
        next = match cause {
            NotInstalled => Some(NotInShellAllow),
            NotInShellAllow => Some(SnapWrapper),
            SnapWrapper => Some(NamespacesRefused),
            NamespacesRefused => Some(NoNewPrivileges),
            NoNewPrivileges => Some(HostPathNeeded),
            HostPathNeeded => Some(InterpreterRuntimeNeeded),
            InterpreterRuntimeNeeded => Some(InterpreterNotAllowed),
            InterpreterNotAllowed => Some(HelperNotAllowed),
            HelperNotAllowed => Some(NoSandbox),
            NoSandbox => Some(TimedOut),
            TimedOut => Some(Failed),
            Failed => None,
        };
    }
    all
}

/// The doctor's vocabulary: each cause as `mur doctor --json` spells it.
fn doctor_causes() -> BTreeSet<String> {
    every_cause()
        .into_iter()
        .map(|cause| match serde_json::to_value(cause) {
            Ok(serde_json::Value::String(name)) => {
                assert_eq!(
                    name,
                    cause.as_str(),
                    "{cause:?} serializes unlike its as_str"
                );
                name
            }
            other => panic!("{cause:?} serialized to {other:?}, not a string"),
        })
        .collect()
}

/// The page's lines with fenced code blocks blanked, so a `#` or `|` inside an example is never
/// read as a heading or a table row.
fn prose_lines(text: &str) -> Vec<&str> {
    let mut fenced = false;
    text.lines()
        .map(|line| {
            if line.trim_start().starts_with("```") {
                fenced = !fenced;
                ""
            } else if fenced {
                ""
            } else {
                line
            }
        })
        .collect()
}

fn cells(row: &str) -> Vec<String> {
    let inner = row.trim().trim_start_matches('|').trim_end_matches('|');
    inner
        .split('|')
        .map(|cell| cell.trim().to_string())
        .collect()
}

/// `|---|---|`. An empty header row, `| | |`, is not one.
fn is_separator(row: &str) -> bool {
    let row = row.trim();
    row.contains('-') && row.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
}

/// The body rows of the first table at or after `start`, stopping at the next heading.
fn first_table(lines: &[&str], start: usize) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut in_table = false;
    for line in &lines[start..] {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            break;
        }
        if trimmed.starts_with('|') {
            in_table = true;
            rows.push(*line);
        } else if in_table {
            break;
        }
    }
    // Drop the header row and its separator.
    rows.into_iter()
        .skip_while(|row| !is_separator(row))
        .skip(1)
        .map(cells)
        .collect()
}

/// The explicit `{ #anchor }` of a heading line.
fn heading_anchor(line: &str) -> Option<&str> {
    let open = line.rfind("{ #")?;
    let rest = &line[open + 3..];
    Some(rest[..rest.find('}')?].trim())
}

fn anchors(lines: &[&str]) -> BTreeSet<String> {
    lines
        .iter()
        .filter(|line| line.starts_with('#'))
        .filter_map(|line| heading_anchor(line))
        .map(str::to_string)
        .collect()
}

/// The cause table: first cell (the backticks stripped) → second cell.
fn cause_table(lines: &[&str]) -> BTreeMap<String, String> {
    let heading = lines
        .iter()
        .position(|line| line.starts_with("## ") && heading_anchor(line) == Some("doctor-causes"))
        .expect("the page has a `## … { #doctor-causes }` heading");
    let mut table = BTreeMap::new();
    for row in first_table(lines, heading + 1) {
        let cause = row[0].trim_matches('`').to_string();
        let limit = row.get(1).cloned().unwrap_or_default();
        assert!(
            table.insert(cause.clone(), limit).is_none(),
            "the cause table lists `{cause}` twice"
        );
    }
    table
}

#[test]
fn the_cause_table_names_exactly_the_causes_the_doctor_prints() {
    let text = page();
    let lines = prose_lines(&text);
    let on_page: BTreeSet<String> = cause_table(&lines).into_keys().collect();
    let doctor = doctor_causes();

    let missing: Vec<&String> = doctor.difference(&on_page).collect();
    let unknown: Vec<&String> = on_page.difference(&doctor).collect();
    assert!(
        missing.is_empty() && unknown.is_empty(),
        "{PAGE}#doctor-causes and `mur doctor --programs` disagree.\n\
         causes the doctor prints with no row on the page: {missing:?}\n\
         rows naming a cause the doctor never prints: {unknown:?}"
    );
}

#[test]
fn every_cause_row_links_to_a_limit_section_on_the_page() {
    let text = page();
    let lines = prose_lines(&text);
    let page_anchors = anchors(&lines);
    for (cause, limit) in cause_table(&lines) {
        let targets: Vec<&str> = limit
            .split("](#")
            .skip(1)
            .filter_map(|rest| rest.split(')').next())
            .collect();
        assert!(
            !targets.is_empty(),
            "the row for `{cause}` links to no section of the page: {limit:?}"
        );
        for target in targets {
            assert!(
                page_anchors.contains(target),
                "the row for `{cause}` links to #{target}, which no heading on the page carries"
            );
        }
    }
}

#[test]
fn every_limit_section_has_the_five_rows_each_filled() {
    let text = page();
    let lines = prose_lines(&text);
    let sections: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.starts_with("### "))
        .map(|(i, _)| i)
        .collect();
    assert!(!sections.is_empty(), "{PAGE} has no `###` limit sections");

    let mut problems = Vec::new();
    for start in sections {
        let heading = lines[start];
        if heading_anchor(heading).is_none() {
            problems.push(format!("{heading:?} has no explicit {{ #anchor }}"));
        }
        let rows = first_table(&lines, start + 1);
        let names: Vec<&str> = rows.iter().map(|row| row[0].as_str()).collect();
        if names != LIMIT_ROWS {
            problems.push(format!(
                "{heading:?}: rows are {names:?}, expected {LIMIT_ROWS:?}"
            ));
            continue;
        }
        for row in &rows {
            if row.get(1).is_none_or(|value| value.is_empty()) {
                problems.push(format!("{heading:?}: the {:?} row is empty", row[0]));
            }
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn the_anchors_other_pages_link_to_exist() {
    let text = page();
    let page_anchors = anchors(&prose_lines(&text));
    let missing: Vec<&str> = REQUIRED_ANCHORS
        .into_iter()
        .filter(|anchor| !page_anchors.contains(*anchor))
        .collect();
    assert!(missing.is_empty(), "{PAGE} lacks the anchors {missing:?}");
}

#[test]
fn the_page_is_in_the_reference_nav_right_after_containment() {
    let path = docs_dir().join("mkdocs.yml");
    let config =
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    let entries: Vec<(usize, &str)> = config
        .lines()
        .enumerate()
        .filter(|(_, line)| line.trim_start().starts_with("- ") && line.contains(PAGE))
        .collect();
    assert_eq!(
        entries.len(),
        1,
        "mkdocs.yml should list {PAGE} in its nav once: {entries:?}"
    );
    let (at, _) = entries[0];
    let previous = config.lines().nth(at - 1).unwrap_or_default();
    assert!(
        previous.contains("reference/containment.md"),
        "{PAGE} should follow the Containment entry in the nav, but follows {previous:?}"
    );
}
