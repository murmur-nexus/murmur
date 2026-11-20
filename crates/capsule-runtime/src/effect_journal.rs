//! What a resumed launch learns about the calls the session it resumes started and never finished.
//!
//! Before the runtime dispatches a call it writes an `effect_started` line naming a fresh
//! `effect_id`, and every outcome line written for that call — `tool_call` or `skill_call`, and
//! `shell` for a shell tool — names the same id. An `effect_started` that no outcome line names is
//! a call that began and left no record of how it ended: the runtime was killed while it ran, or
//! died before it could write the outcome.
//!
//! The job is to state that fact to the resumed agent, not to settle it. Nothing is rerun, nothing
//! is marked done, no liveness is checked and nothing is written into the resumed-from trace, so
//! every resume of the same session reports the same calls again.

use std::collections::{BTreeMap, HashSet};

use serde_json::Value;

use crate::{
    origin::{TaskOrigin, TaskProvenance, TrustClass},
    workdir_fs::WorkdirRoot,
};

/// The first line of [`UnfinishedEffects::fact_text`].
pub(crate) const UNFINISHED_EFFECTS_OPENING: &str =
    "Effects started in an earlier session have no recorded outcome.";

/// The most of a command or an input [`UnfinishedEffects::fact_text`] shows, in bytes.
const FACT_FIELD_MAX_BYTES: usize = 1024;

/// One `effect_started` line no outcome line names.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct UnfinishedEffect {
    pub(crate) effect_id: String,
    /// `shell` or `tool_call`, as the line names it.
    pub(crate) kind: String,
    pub(crate) tool_name: String,
    pub(crate) tool_call_id: Option<String>,
    /// Set for a `shell` effect.
    pub(crate) binary: Option<String>,
    /// Set for a `shell` effect.
    pub(crate) command: Option<String>,
    pub(crate) input: Value,
    pub(crate) input_bytes: u64,
    pub(crate) turn: u32,
    pub(crate) task_id: Option<String>,
    /// The `effect_started` line's own `timestamp`.
    pub(crate) started_at_ms: u64,
}

/// The calls one resumed-from session started and recorded no outcome for, in the order it
/// started them.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct UnfinishedEffects {
    /// The session whose trace was read: its own `session_start.session_id`, or the id
    /// `--resume` named when the trace holds no `session_start`.
    pub(crate) from_session: String,
    /// Never empty.
    pub(crate) effects: Vec<UnfinishedEffect>,
    /// The lowest trust across [`Self::effects`], joined through each effect's `task_id`.
    pub(crate) trust: TrustClass,
}

impl UnfinishedEffects {
    /// The runtime message the resumed launch's first task reads ahead of its task text.
    ///
    /// States what the trace shows and nothing else: the calls started, no outcome was recorded,
    /// whether each took effect is unknown, and none was run again.
    pub(crate) fn fact_text(&self) -> String {
        let mut text = format!(
            "{UNFINISHED_EFFECTS_OPENING}\n\n\
             Session {} recorded that each call below was about to run, and recorded nothing \
             after it: no result, no exit code, no error. Whether each call took effect is \
             unknown. The runtime has not run any of them again.\n",
            self.from_session
        );
        for effect in &self.effects {
            text.push_str(&format!(
                "\neffect_id: {}\ntool: {}\n",
                effect.effect_id, effect.tool_name
            ));
            match (&effect.binary, &effect.command) {
                (Some(binary), Some(command)) => {
                    text.push_str(&format!(
                        "binary: {binary}\ncommand: {}\n",
                        self.capped(command, &effect.effect_id)
                    ));
                }
                _ => {
                    let input =
                        serde_json::to_string(&effect.input).unwrap_or_else(|_| "null".to_string());
                    text.push_str(&format!(
                        "input: {}\n",
                        self.capped(&input, &effect.effect_id)
                    ));
                }
            }
            text.push_str(&format!("started_at_ms: {}\n", effect.started_at_ms));
        }
        text.truncate(text.trim_end().len());
        text
    }

    /// `text` whole when it fits [`FACT_FIELD_MAX_BYTES`]; otherwise cut on a character boundary
    /// and followed by where the whole of it is.
    fn capped(&self, text: &str, effect_id: &str) -> String {
        if text.len() <= FACT_FIELD_MAX_BYTES {
            return text.to_string();
        }
        format!(
            "{} … ({} bytes; the full text is on the effect_started line {effect_id} in session \
             {})",
            crate::agent::truncate_on_char_boundary(text, FACT_FIELD_MAX_BYTES),
            text.len(),
            self.from_session
        )
    }

    /// The fence the fact travels under: a `completion`-origin task of [`Self::trust`], so it is
    /// fenced whenever any reported effect belonged to an untrusted or unjoined task.
    pub(crate) fn provenance(&self) -> TaskProvenance {
        TaskProvenance::derive(TaskOrigin::Completion, Some(self.trust))
    }
}

/// The calls the session whose `trace.jsonl` is in `trace_dir` started and recorded no outcome
/// for, or `None` when there are none.
///
/// `resumed_from` is the session `--resume` named, used only when the trace holds no
/// `session_start`. Reads the file and nothing else: the trace is never written. Fails open: a
/// trace that is missing or cannot be read is one line on stderr and `None`.
pub(crate) fn read_unfinished_effects(
    trace_dir: &WorkdirRoot,
    resumed_from: &str,
) -> Option<UnfinishedEffects> {
    let trace_file = crate::trace::TRACE_FILE_NAME;
    let contents = match trace_dir.read_to_string(trace_file) {
        Ok(Some(contents)) => contents,
        Ok(None) => {
            crate::runtime_err!(
                "[capsule-runtime] could not read {} (it does not exist); calls the resumed \
                 session started without an outcome are not reported",
                trace_dir.display_path(trace_file)
            );
            return None;
        }
        Err(error) => {
            crate::runtime_err!(
                "[capsule-runtime] could not read {error}; calls the resumed session started \
                 without an outcome are not reported"
            );
            return None;
        }
    };
    scan(&contents, resumed_from)
}

/// [`read_unfinished_effects`] over the file's contents.
///
/// A line that does not parse is skipped: a torn tail is the expected shape of a file whose
/// writer was killed, which is exactly the file this reads.
fn scan(contents: &str, resumed_from: &str) -> Option<UnfinishedEffects> {
    let mut session_id: Option<String> = None;
    let mut started: Vec<UnfinishedEffect> = Vec::new();
    let mut finished: HashSet<String> = HashSet::new();
    let mut task_trust: BTreeMap<String, TrustClass> = BTreeMap::new();

    for line in contents.lines() {
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match record["event_type"].as_str().unwrap_or_default() {
            "session_start" if session_id.is_none() => {
                session_id = record["session_id"]
                    .as_str()
                    .filter(|id| !id.is_empty())
                    .map(str::to_string);
            }
            "task_start" => {
                if let (Some(task_id), Some(trust)) = (
                    record["task_id"].as_str(),
                    record["trust"].as_str().and_then(TrustClass::parse),
                ) {
                    task_trust.insert(task_id.to_string(), trust);
                }
            }
            "effect_started" => {
                let Some(effect_id) = record["effect_id"].as_str().filter(|id| !id.is_empty())
                else {
                    continue;
                };
                let text = |key: &str| record[key].as_str().map(str::to_string);
                started.push(UnfinishedEffect {
                    effect_id: effect_id.to_string(),
                    kind: text("kind")
                        .unwrap_or_else(|| crate::trace::EFFECT_KIND_TOOL_CALL.to_string()),
                    tool_name: text("tool_name").unwrap_or_default(),
                    tool_call_id: text("tool_call_id"),
                    binary: text("binary"),
                    command: text("command"),
                    input: record["input"].clone(),
                    input_bytes: record["input_bytes"].as_u64().unwrap_or_default(),
                    turn: record["turn"]
                        .as_u64()
                        .and_then(|turn| u32::try_from(turn).ok())
                        .unwrap_or_default(),
                    task_id: text("task_id"),
                    started_at_ms: record["timestamp"].as_u64().unwrap_or_default(),
                });
            }
            // The outcome lines a journaled call writes; any one of them finishes its effect,
            // wherever in the file it lands.
            "tool_call" | "skill_call" | "shell" => {
                if let Some(effect_id) = record["effect_id"].as_str() {
                    finished.insert(effect_id.to_string());
                }
            }
            _ => {}
        }
    }

    let mut trust = TrustClass::Trusted;
    let effects: Vec<UnfinishedEffect> = started
        .into_iter()
        .filter(|effect| !finished.contains(&effect.effect_id))
        .inspect(|effect| {
            // A join that finds nothing yields untrusted: an absent claim is not a trusted one.
            let effect_trust = effect
                .task_id
                .as_ref()
                .and_then(|task_id| task_trust.get(task_id).copied())
                .unwrap_or(TrustClass::Untrusted);
            if effect_trust == TrustClass::Untrusted {
                trust = TrustClass::Untrusted;
            }
        })
        .collect();
    if effects.is_empty() {
        return None;
    }
    Some(UnfinishedEffects {
        from_session: session_id.unwrap_or_else(|| resumed_from.to_string()),
        effects,
        trust,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION_START: &str = r#"{"event_type":"session_start","event_id":"evt_s","session_id":"ses_prior","timestamp":1}"#;
    const TASK_TRUSTED: &str = r#"{"event_type":"task_start","event_id":"evt_t","session_id":"ses_prior","timestamp":2,"task_id":"tsk_1","trust":"trusted"}"#;
    const TASK_UNTRUSTED: &str = r#"{"event_type":"task_start","event_id":"evt_t2","session_id":"ses_prior","timestamp":2,"task_id":"tsk_2","trust":"untrusted"}"#;

    fn shell_started(effect_id: &str, task_id: &str, command: &str) -> String {
        serde_json::json!({
            "event_type": "effect_started",
            "event_id": format!("evt_{effect_id}"),
            "parent_id": "evt_t",
            "session_id": "ses_prior",
            "timestamp": 1750,
            "turn": 1,
            "task_id": task_id,
            "effect_id": effect_id,
            "kind": "shell",
            "tool_name": "bash",
            "tool_call_id": "toolu_1",
            "binary": "/usr/bin/bash",
            "command": command,
            "input": {"command": command},
            "input_bytes": 20,
        })
        .to_string()
    }

    fn tool_started(effect_id: &str, task_id: Option<&str>, input: Value) -> String {
        serde_json::json!({
            "event_type": "effect_started",
            "event_id": format!("evt_{effect_id}"),
            "session_id": "ses_prior",
            "timestamp": 1800,
            "turn": 2,
            "task_id": task_id,
            "effect_id": effect_id,
            "kind": "tool_call",
            "tool_name": "write-file",
            "tool_call_id": null,
            "input": input,
            "input_bytes": 9,
        })
        .to_string()
    }

    fn outcome(event_type: &str, effect_id: &str) -> String {
        serde_json::json!({
            "event_type": event_type,
            "event_id": format!("evt_out_{effect_id}"),
            "session_id": "ses_prior",
            "timestamp": 1900,
            "effect_id": effect_id,
        })
        .to_string()
    }

    fn trace(lines: &[&str]) -> String {
        let mut text = lines.join("\n");
        text.push('\n');
        text
    }

    fn read(contents: &str, resumed_from: &str) -> Option<UnfinishedEffects> {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("trace.jsonl"), contents).unwrap();
        read_unfinished_effects(&WorkdirRoot::open(dir.path()).unwrap(), resumed_from)
    }

    /// Any of the three outcome lines finishes the effect it names, wherever it lands; only the
    /// effects none names are reported, in the order they started.
    #[test]
    fn effects_unfinished_pairing_by_tool_call_skill_call_and_shell_leaves_the_rest_in_order() {
        let contents = trace(&[
            SESSION_START,
            TASK_TRUSTED,
            &shell_started("eff_a", "tsk_1", "echo a"),
            &shell_started("eff_b", "tsk_1", "sleep 30"),
            &tool_started("eff_c", Some("tsk_1"), serde_json::json!({"path": "c"})),
            &tool_started("eff_d", Some("tsk_1"), serde_json::json!({"path": "d"})),
            &shell_started("eff_e", "tsk_1", "echo e"),
            &shell_started("eff_f", "tsk_1", "make"),
            &outcome("tool_call", "eff_a"),
            &outcome("skill_call", "eff_c"),
            &outcome("shell", "eff_e"),
            // An unrelated line naming an effect id finishes nothing.
            &outcome("shell_detached", "eff_f"),
        ]);
        let found = read(&contents, "ses_named").expect("two effects are unfinished");
        let ids: Vec<&str> = found.effects.iter().map(|e| e.effect_id.as_str()).collect();
        assert_eq!(ids, ["eff_b", "eff_d", "eff_f"]);
        assert_eq!(found.from_session, "ses_prior");
        assert_eq!(found.trust, TrustClass::Trusted);

        let b = &found.effects[0];
        assert_eq!(b.kind, "shell");
        assert_eq!(b.tool_name, "bash");
        assert_eq!(b.tool_call_id.as_deref(), Some("toolu_1"));
        assert_eq!(b.binary.as_deref(), Some("/usr/bin/bash"));
        assert_eq!(b.command.as_deref(), Some("sleep 30"));
        assert_eq!(b.turn, 1);
        assert_eq!(b.task_id.as_deref(), Some("tsk_1"));
        assert_eq!(b.started_at_ms, 1750);
        assert_eq!(b.input_bytes, 20);

        let d = &found.effects[1];
        assert_eq!(d.kind, "tool_call");
        assert_eq!(d.tool_call_id, None);
        assert_eq!(d.binary, None);
        assert_eq!(d.input, serde_json::json!({"path": "d"}));
    }

    /// Every effect paired: nothing to report, and nothing said about it.
    #[test]
    fn effects_unfinished_is_none_when_every_effect_has_an_outcome() {
        let contents = trace(&[
            SESSION_START,
            TASK_TRUSTED,
            &shell_started("eff_a", "tsk_1", "echo a"),
            &outcome("tool_call", "eff_a"),
            &outcome("shell", "eff_a"),
        ]);
        assert_eq!(read(&contents, "ses_named"), None);
        assert_eq!(read(&trace(&[SESSION_START]), "ses_named"), None);
    }

    /// A torn tail and a malformed line are skipped; the lines around them still count.
    #[test]
    fn effects_unfinished_skips_a_torn_tail_and_malformed_lines() {
        let mut contents = trace(&[
            SESSION_START,
            "not json at all",
            TASK_TRUSTED,
            &shell_started("eff_a", "tsk_1", "sleep 30"),
        ]);
        contents.push_str(r#"{"event_type":"tool_call","effect_id":"eff_a","sess"#);
        let found = read(&contents, "ses_named").expect("the torn outcome finishes nothing");
        assert_eq!(found.effects.len(), 1);
        assert_eq!(found.effects[0].effect_id, "eff_a");
    }

    /// A trace with no `session_start` reports under the session `--resume` named.
    #[test]
    fn effects_unfinished_without_session_start_names_the_resumed_session() {
        let contents = trace(&[TASK_TRUSTED, &shell_started("eff_a", "tsk_1", "sleep 30")]);
        let found = read(&contents, "ses_named").unwrap();
        assert_eq!(found.from_session, "ses_named");
    }

    /// Trust joins `task_id` to `task_start.trust`; an unjoined or `null` task id is untrusted,
    /// and one untrusted effect makes the whole report untrusted.
    #[test]
    fn effects_unfinished_trust_joins_task_start_and_one_untrusted_effect_decides() {
        let all_trusted = trace(&[
            SESSION_START,
            TASK_TRUSTED,
            &shell_started("eff_a", "tsk_1", "sleep 30"),
        ]);
        let found = read(&all_trusted, "s").unwrap();
        assert_eq!(found.trust, TrustClass::Trusted);
        assert_eq!(found.provenance().trust(), TrustClass::Trusted);
        assert_eq!(found.provenance().origin(), TaskOrigin::Completion);

        for unjoined in [
            trace(&[
                SESSION_START,
                TASK_TRUSTED,
                TASK_UNTRUSTED,
                &shell_started("eff_a", "tsk_1", "sleep 30"),
                &shell_started("eff_b", "tsk_2", "sleep 30"),
            ]),
            trace(&[
                SESSION_START,
                TASK_TRUSTED,
                &shell_started("eff_a", "tsk_1", "sleep 30"),
                &shell_started("eff_b", "tsk_missing", "sleep 30"),
            ]),
            trace(&[
                SESSION_START,
                TASK_TRUSTED,
                &tool_started("eff_a", None, Value::Null),
            ]),
        ] {
            let found = read(&unjoined, "s").unwrap();
            assert_eq!(found.trust, TrustClass::Untrusted, "{unjoined}");
            assert_eq!(found.provenance().trust(), TrustClass::Untrusted);
            assert_eq!(found.provenance().origin(), TaskOrigin::Completion);
        }
    }

    /// A trace that does not exist reports nothing and stops nothing.
    #[test]
    fn effects_unfinished_missing_trace_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            read_unfinished_effects(&WorkdirRoot::open(dir.path()).unwrap(), "ses_named"),
            None
        );
    }

    fn one_of_each() -> UnfinishedEffects {
        let contents = trace(&[
            SESSION_START,
            TASK_TRUSTED,
            &shell_started("eff_a", "tsk_1", "sleep 30"),
            &tool_started(
                "eff_b",
                Some("tsk_1"),
                serde_json::json!({"path": "notes.md", "content": "hi"}),
            ),
        ]);
        read(&contents, "s").unwrap()
    }

    /// The fact opens with the constant, names the session, and gives each effect's fields on
    /// their own lines: the command for a shell effect, the compact input for any other.
    #[test]
    fn effects_unfinished_fact_text_states_the_opening_and_every_field() {
        let text = one_of_each().fact_text();
        assert_eq!(
            text,
            "Effects started in an earlier session have no recorded outcome.\n\
             \n\
             Session ses_prior recorded that each call below was about to run, and recorded \
             nothing after it: no result, no exit code, no error. Whether each call took effect \
             is unknown. The runtime has not run any of them again.\n\
             \n\
             effect_id: eff_a\n\
             tool: bash\n\
             binary: /usr/bin/bash\n\
             command: sleep 30\n\
             started_at_ms: 1750\n\
             \n\
             effect_id: eff_b\n\
             tool: write-file\n\
             input: {\"content\":\"hi\",\"path\":\"notes.md\"}\n\
             started_at_ms: 1800"
        );
    }

    /// The fact states and never directs: no sentence tells the agent what to do.
    #[test]
    fn effects_unfinished_fact_text_gives_no_instruction() {
        let text = one_of_each().fact_text().to_lowercase();
        for directive in [
            "should",
            "must",
            "please",
            "re-run",
            "rerun",
            "retry",
            "check whether",
            "verify",
            "you ",
            "consider",
            "make sure",
        ] {
            assert!(!text.contains(directive), "{directive:?} in {text}");
        }
    }

    /// A command or an input past 1,024 bytes is cut on a character boundary and followed by
    /// where the whole text is.
    #[test]
    fn effects_unfinished_fact_text_truncates_long_fields_on_a_char_boundary() {
        // Three-byte characters, so byte 1,024 falls inside one.
        let command = "é€".repeat(400);
        let mut effects = one_of_each();
        effects.effects[0].command = Some(command.clone());
        effects.effects[1].input = serde_json::json!({"content": "x".repeat(2000)});
        let text = effects.fact_text();

        let command_line = text
            .lines()
            .find(|line| line.starts_with("command: "))
            .unwrap();
        let shown = command_line.strip_prefix("command: ").unwrap();
        let (cut, note) = shown.split_once(" … (").unwrap();
        assert!(cut.len() <= 1024 && cut.len() > 1020, "{}", cut.len());
        assert!(command.starts_with(cut));
        assert_eq!(
            note,
            format!(
                "{} bytes; the full text is on the effect_started line eff_a in session ses_prior)",
                command.len()
            )
        );

        let input_line = text
            .lines()
            .find(|line| line.starts_with("input: "))
            .unwrap();
        assert!(
            input_line.ends_with(
                " bytes; the full text is on the effect_started line eff_b in session ses_prior)"
            ),
            "{input_line}"
        );
        assert!(input_line.len() < 1024 + 200, "{}", input_line.len());
    }
}
