//! Which recorded sessions belong to one formation, read from their traces.
//!
//! This is the only reader that looks at more than one session's trace. It leans on one
//! property of the file: `session_start` is the first line of every agent session's
//! `trace.jsonl`, and a member's `session_start` carries its `formation_id`. So telling a member
//! from any other session costs one line, and a whole trace is read only for a session already
//! known to be a member. A session whose first line is anything else — a script capsule's
//! drain-only trace, a torn file — is no member.
//!
//! A formation's members record under more than one root: the entry member under the roster's
//! project, each peer under its own directory in `~/.murmur/formations/<frm_id>/`.
//! [`search_formation`] reads a given root and then those, and merges what they record. Nothing
//! here follows a delegation edge into another session root: a member's delegated child that was
//! not found under a searched root is counted, not chased.

use std::{
    collections::BTreeSet,
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

use capsule_runtime::{retention::session_id_timestamp_ms, FormationId};
use serde::Deserialize;

use crate::error::CliError;
use crate::session_address::ses_entries;

/// One session under a root whose `session_start` names the formation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecordedMember {
    /// The session directory's name, a `ses_` id.
    pub(crate) session_id: String,
    /// `name@version` from `session_start`.
    pub(crate) capsule: String,
    /// The session that delegated this one, from `session_start`.
    pub(crate) spawned_by: Option<String>,
    /// `exit_status` of the first `session_end`; `None` for a session that wrote none — one still
    /// running, or one that was killed.
    pub(crate) exit_status: Option<String>,
    /// `child_session_id` of every `delegation_start`, in file order.
    pub(crate) delegated_children: Vec<String>,
}

/// What one session root records about a formation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RecordedMembers {
    /// Every member found, by session id ascending.
    pub(crate) members: Vec<RecordedMember>,
    /// The members' delegated children that are not themselves among `members`, ascending: their
    /// traces are in another session root, or nowhere.
    pub(crate) children_elsewhere: Vec<String>,
}

/// The few fields of a trace line this reader needs. Every field defaults, so any JSON object
/// parses and only the event type decides what is read from it.
#[derive(Deserialize)]
struct Line {
    event_type: String,
    #[serde(default)]
    formation_id: Option<String>,
    #[serde(default)]
    capsule_name: Option<String>,
    #[serde(default)]
    capsule_version: Option<String>,
    #[serde(default)]
    spawned_by: Option<String>,
    #[serde(default)]
    exit_status: Option<String>,
    #[serde(default)]
    child_session_id: Option<String>,
}

/// The formation `<session_dir>/trace.jsonl` names on its first line, or `None` when it names
/// none: a missing or unreadable file, a first line that is not JSON or not `session_start`, or a
/// `session_start` with no `formation_id`.
pub(crate) fn session_formation_id(session_dir: &Path) -> Option<String> {
    first_line(session_dir)?.formation_id
}

/// The first non-empty line of a session's trace, if it is a `session_start`.
fn first_line(session_dir: &Path) -> Option<Line> {
    let file = fs::File::open(session_dir.join("trace.jsonl")).ok()?;
    let line = BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .find(|line| !line.trim().is_empty())?;
    serde_json::from_str::<Line>(line.trim())
        .ok()
        .filter(|line| line.event_type == "session_start")
}

/// Every session under `root` whose `session_start` names `formation`.
///
/// A root that does not exist holds no member. The `Err` is a root that exists and cannot be
/// listed, `E-IO-003`; each caller decides whether that fails it. A session directory minted
/// before the formation id is not opened. A member's trace is read to its end with every line
/// that does not parse skipped, so a writer killed mid-line costs only that line.
pub(crate) fn recorded_members(
    root: &Path,
    formation: &FormationId,
) -> Result<RecordedMembers, CliError> {
    let minted = formation.minted_at_ms();
    let mut names = ses_entries(root)?;
    names.sort();

    let mut members = Vec::new();
    for name in names {
        if matches!(session_id_timestamp_ms(&name), Some(at) if at < minted) {
            continue;
        }
        let session_dir = root.join(&name);
        if session_formation_id(&session_dir).as_deref() == Some(formation.as_str()) {
            members.push(read_member(&session_dir, name));
        }
    }

    let found: BTreeSet<&str> = members.iter().map(|m| m.session_id.as_str()).collect();
    let children_elsewhere: BTreeSet<String> = members
        .iter()
        .flat_map(|member| member.delegated_children.iter())
        .filter(|child| !found.contains(child.as_str()))
        .cloned()
        .collect();
    Ok(RecordedMembers {
        members,
        children_elsewhere: children_elsewhere.into_iter().collect(),
    })
}

/// One root [`search_formation`] searched, and why it could not be read when it could not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SearchedRoot {
    pub(crate) root: PathBuf,
    /// `None` for a root that was read.
    pub(crate) unreadable: Option<String>,
}

/// What every searched root records about one formation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct FormationSearch {
    /// Every root searched, in the order searched.
    pub(crate) searched: Vec<SearchedRoot>,
    /// Every member found under any of them, once each, by session id ascending.
    pub(crate) found: RecordedMembers,
}

/// Every session under `first`, then under each of `more`, whose `session_start` names
/// `formation`.
///
/// `first` is the root the caller was given, and one that cannot be listed fails the search as
/// [`recorded_members`] does. A root among `more` that cannot be listed is reported in
/// [`FormationSearch::searched`] and fails nothing. A session found under two roots is listed
/// once; `more` repeating `first` is searched once.
pub(crate) fn search_formation(
    first: &Path,
    more: Vec<PathBuf>,
    formation: &FormationId,
) -> Result<FormationSearch, CliError> {
    let mut members = recorded_members(first, formation)?.members;
    let mut searched = vec![SearchedRoot {
        root: first.to_path_buf(),
        unreadable: None,
    }];
    for root in more {
        if searched.iter().any(|done| done.root == root) {
            continue;
        }
        match recorded_members(&root, formation) {
            Ok(found) => {
                for member in found.members {
                    if !members.iter().any(|m| m.session_id == member.session_id) {
                        members.push(member);
                    }
                }
                searched.push(SearchedRoot {
                    root,
                    unreadable: None,
                });
            }
            Err(error) => searched.push(SearchedRoot {
                root,
                unreadable: Some(error.message),
            }),
        }
    }
    members.sort_by(|a, b| a.session_id.cmp(&b.session_id));
    let found: BTreeSet<&str> = members.iter().map(|m| m.session_id.as_str()).collect();
    let children_elsewhere: BTreeSet<String> = members
        .iter()
        .flat_map(|member| member.delegated_children.iter())
        .filter(|child| !found.contains(child.as_str()))
        .cloned()
        .collect();
    Ok(FormationSearch {
        searched,
        found: RecordedMembers {
            members,
            children_elsewhere: children_elsewhere.into_iter().collect(),
        },
    })
}

/// A member's whole trace, read for its identity, its ending and its delegations.
fn read_member(session_dir: &Path, session_id: String) -> RecordedMember {
    let mut member = RecordedMember {
        session_id,
        capsule: String::new(),
        spawned_by: None,
        exit_status: None,
        delegated_children: Vec::new(),
    };
    let Ok(file) = fs::File::open(session_dir.join("trace.jsonl")) else {
        return member;
    };
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let Ok(line) = serde_json::from_str::<Line>(line.trim()) else {
            continue;
        };
        match line.event_type.as_str() {
            "session_start" if member.capsule.is_empty() => {
                member.capsule = format!(
                    "{}@{}",
                    line.capsule_name.unwrap_or_default(),
                    line.capsule_version.unwrap_or_default()
                );
                member.spawned_by = line.spawned_by;
            }
            "session_end" if member.exit_status.is_none() => {
                member.exit_status = Some(line.exit_status.unwrap_or_default());
            }
            "delegation_start" => member.delegated_children.extend(line.child_session_id),
            _ => {}
        }
    }
    member
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A `ses_` id minted `offset_ms` after `formation`, ending in `tail`.
    fn session_after(formation: &FormationId, offset_ms: i64, tail: &str) -> String {
        let at = (formation.minted_at_ms() as i64 + offset_ms) as u64;
        format!("ses_{at:012x}{tail:0>20}")
    }

    fn write_trace(root: &Path, session_id: &str, lines: &[String]) {
        let dir = root.join(session_id);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("trace.jsonl"), lines.join("\n") + "\n").unwrap();
    }

    fn start(session_id: &str, formation: Option<&str>, spawned_by: Option<&str>) -> String {
        let mut line = json!({
            "event_type": "session_start",
            "session_id": session_id,
            "capsule_name": "member",
            "capsule_version": "0.1.0",
            "model": "m",
            "max_turns": 3,
        });
        if let Some(id) = formation {
            line["formation_id"] = id.into();
        }
        if let Some(parent) = spawned_by {
            line["spawned_by"] = parent.into();
        }
        line.to_string()
    }

    fn end(status: &str) -> String {
        json!({"event_type": "session_end", "exit_status": status}).to_string()
    }

    fn delegation(child: &str) -> String {
        json!({
            "event_type": "delegation_start",
            "delegation_id": "dlg_1",
            "capsule": "child",
            "version": "0.1.0",
            "child_session_id": child,
            "child_workdir": "/elsewhere",
        })
        .to_string()
    }

    /// A formation's peers record under their own directories in the murmur home: the search
    /// reads the given root and then each peer's root, lists every member once, and reports a
    /// root it could not read without failing.
    #[test]
    fn a_formation_is_found_across_the_project_root_and_its_peers_roots() {
        let formation = FormationId::mint();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let root = project.path().join(".murmur");
        let entry = session_after(&formation, 30, "1");
        write_trace(
            &root,
            &entry,
            &[start(&entry, Some(formation.as_str()), None), end("ok")],
        );
        let formations = home.path().join("formations");
        let worker_root = formations
            .join(formation.as_str())
            .join("worker")
            .join(".murmur");
        let worker = session_after(&formation, 10, "2");
        write_trace(
            &worker_root,
            &worker,
            &[
                start(&worker, Some(formation.as_str()), None),
                end("canceled"),
            ],
        );
        let roots =
            capsule_runtime::formation_launch::formation_member_roots_in(&formations, &formation);
        assert_eq!(roots, std::slice::from_ref(&worker_root));

        // A directory this user may not list.
        let unreadable = home.path().join("closed");
        fs::create_dir(&unreadable).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).unwrap();
        let mut more = roots.clone();
        more.push(root.clone());
        more.push(unreadable.clone());
        let search = search_formation(&root, more, &formation).unwrap();
        let sessions: Vec<&str> = search
            .found
            .members
            .iter()
            .map(|member| member.session_id.as_str())
            .collect();
        assert_eq!(sessions, [worker.as_str(), entry.as_str()]);
        let searched: Vec<(&Path, bool)> = search
            .searched
            .iter()
            .map(|root| (root.root.as_path(), root.unreadable.is_some()))
            .collect();
        assert_eq!(
            searched,
            [
                (root.as_path(), false),
                (worker_root.as_path(), false),
                (unreadable.as_path(), fs::read_dir(&unreadable).is_err())
            ]
        );
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn members_are_found_by_their_first_line_and_nothing_else() {
        let root = tempfile::tempdir().unwrap();
        let formation = FormationId::mint();
        let other = FormationId::mint();
        let ended = session_after(&formation, 10, "a");
        let killed = session_after(&formation, 20, "b");
        let standalone = session_after(&formation, 30, "c");
        let stranger = session_after(&formation, 40, "d");
        let older = session_after(&formation, -1_000, "e");
        let drained = session_after(&formation, 50, "f");
        let empty = session_after(&formation, 60, "0");
        let child_here = session_after(&formation, 70, "1");
        let child_away = session_after(&formation, 80, "2");

        write_trace(
            root.path(),
            &ended,
            &[
                start(&ended, Some(formation.as_str()), None),
                delegation(&child_here),
                delegation(&child_away),
                end("completed"),
                end("a second ending is not the first"),
            ],
        );
        write_trace(
            root.path(),
            &killed,
            &[
                start(&killed, Some(formation.as_str()), None),
                json!({"event_type": "task_start", "task_id": "tsk_1"}).to_string(),
                r#"{"event_type":"inference","tur"#.to_string(),
            ],
        );
        write_trace(
            root.path(),
            &child_here,
            &[
                start(&child_here, Some(formation.as_str()), Some(&ended)),
                end("completed"),
            ],
        );
        write_trace(
            root.path(),
            &standalone,
            &[start(&standalone, None, None), end("completed")],
        );
        write_trace(
            root.path(),
            &stranger,
            &[
                start(&stranger, Some(other.as_str()), None),
                end("completed"),
            ],
        );
        // Older than the formation: never opened, so even a matching first line is not read.
        write_trace(
            root.path(),
            &older,
            &[
                start(&older, Some(formation.as_str()), None),
                end("completed"),
            ],
        );
        // A script capsule's drain-only trace: the formation is named, but not on a session_start.
        write_trace(
            root.path(),
            &drained,
            &[
                json!({"event_type": "tool_call", "formation_id": formation.as_str()}).to_string(),
                start(&drained, Some(formation.as_str()), None),
            ],
        );
        fs::create_dir_all(root.path().join(&empty)).unwrap();
        fs::create_dir_all(root.path().join("not-a-session")).unwrap();

        let found = recorded_members(root.path(), &formation).unwrap();
        assert_eq!(
            found.members,
            vec![
                RecordedMember {
                    session_id: ended.clone(),
                    capsule: "member@0.1.0".to_string(),
                    spawned_by: None,
                    exit_status: Some("completed".to_string()),
                    delegated_children: vec![child_here.clone(), child_away.clone()],
                },
                RecordedMember {
                    session_id: killed.clone(),
                    capsule: "member@0.1.0".to_string(),
                    spawned_by: None,
                    exit_status: None,
                    delegated_children: Vec::new(),
                },
                RecordedMember {
                    session_id: child_here.clone(),
                    capsule: "member@0.1.0".to_string(),
                    spawned_by: Some(ended.clone()),
                    exit_status: Some("completed".to_string()),
                    delegated_children: Vec::new(),
                },
            ]
        );
        assert_eq!(found.children_elsewhere, vec![child_away]);

        assert_eq!(
            session_formation_id(&root.path().join(&ended)).as_deref(),
            Some(formation.as_str())
        );
        assert_eq!(session_formation_id(&root.path().join(&standalone)), None);
        assert_eq!(session_formation_id(&root.path().join(&drained)), None);
        assert_eq!(session_formation_id(&root.path().join(&empty)), None);
    }

    #[test]
    fn a_missing_root_holds_no_member() {
        let root = tempfile::tempdir().unwrap();
        let found =
            recorded_members(&root.path().join("never-created"), &FormationId::mint()).unwrap();
        assert_eq!(found, RecordedMembers::default());
    }

    #[test]
    fn a_first_line_that_is_not_json_names_no_formation() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("ses_x");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("trace.jsonl"), "\n\n{\"event_type\":\"session_sta").unwrap();
        assert_eq!(session_formation_id(&dir), None);
    }
}
