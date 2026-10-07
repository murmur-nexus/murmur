//! `mur token`: print one door token of a running capsule.
//!
//! `mur run` prints no token, because its output is so often redirected to a log. Every token a
//! session declaring `network.authentication` minted is in its running record, held at `0600`, and
//! this command is how a person reads one out: the operator token by default, or a declared
//! credential's by name. Standard output is the token and a newline, nothing else, so
//! `MURMUR_DOOR_TOKEN=$(mur token)` holds exactly the token.

use capsule_runtime::{DoorToken, RunningRecord};
use murmur_artifact::OPERATOR_CREDENTIAL;

use crate::error::{CliError, E_RUN_048, E_RUN_049};
use crate::live_address::{resolve_live_process, LATEST};

/// Print the token `credential` names for the session `session` names, `@1` when omitted.
///
/// Resolved as far as the process only: the token is read from the record, and a door that is slow
/// to answer still has the token it minted.
pub(crate) fn run_token(session: Option<&str>, credential: &str) -> Result<(), CliError> {
    let record = resolve_live_process(session.unwrap_or(LATEST))?;
    let token = token_of(&record, credential)?;
    // A reader that has gone away has nothing left to be told; the refused line is dropped.
    capsule_runtime::diagnostic::credential_to_stdout(token.expose());
    Ok(())
}

/// The token `credential` names in `record`.
fn token_of<'a>(record: &'a RunningRecord, credential: &str) -> Result<&'a DoorToken, CliError> {
    let Some(operator) = &record.door_token else {
        return Err(CliError::new(
            E_RUN_048,
            format!(
                "{} has a public door: its capsule declares no network.authentication, so it \
                 minted no token",
                record.session_id
            ),
        ));
    };
    if credential == OPERATOR_CREDENTIAL {
        return Ok(operator);
    }
    record.credentials.get(credential).ok_or_else(|| {
        let names: Vec<&str> = std::iter::once(OPERATOR_CREDENTIAL)
            .chain(record.credentials.keys().map(String::as_str))
            .collect();
        CliError::new(
            E_RUN_049,
            format!(
                "{} minted no credential '{credential}'; it has: {}",
                record.session_id,
                names.join(", ")
            ),
        )
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use super::*;

    fn record(door_token: Option<&str>, credentials: &[(&str, &str)]) -> RunningRecord {
        RunningRecord {
            session_id: "ses_0199c4e2f1b7712a9d3e4f5061728394".to_string(),
            url: "127.0.0.1:1".to_string(),
            pid: 1,
            process_start: "42".to_string(),
            capsule_name: "c".to_string(),
            capsule_version: "0.1.0".to_string(),
            workdir: PathBuf::from("/tmp/c"),
            outlives_launcher: false,
            started_at: "2026-01-01T00:00:00Z".to_string(),
            door_token: door_token.map(|token| DoorToken::new(token.to_string())),
            credentials: credentials
                .iter()
                .map(|(name, token)| (name.to_string(), DoorToken::new(token.to_string())))
                .collect::<BTreeMap<_, _>>(),
            formation_id: None,
            formation_lifeline: false,
            formation_launcher: None,
            spawned_by: None,
        }
    }

    #[test]
    fn the_operator_credential_is_the_records_door_token() {
        let record = record(Some("mdt1.op"), &[("watcher", "mdt1.w")]);
        assert_eq!(token_of(&record, "operator").unwrap().expose(), "mdt1.op");
    }

    #[test]
    fn a_declared_credential_is_read_by_name() {
        let record = record(Some("mdt1.op"), &[("watcher", "mdt1.w")]);
        assert_eq!(token_of(&record, "watcher").unwrap().expose(), "mdt1.w");
    }

    #[test]
    fn a_public_door_is_e_run_048_whatever_credential_is_asked_for() {
        let record = record(None, &[]);
        for credential in ["operator", "watcher"] {
            let error = token_of(&record, credential).unwrap_err();
            assert_eq!(error.code, E_RUN_048);
            assert_eq!(
                error.message,
                "ses_0199c4e2f1b7712a9d3e4f5061728394 has a public door: its capsule declares no \
                 network.authentication, so it minted no token"
            );
        }
    }

    #[test]
    fn an_undeclared_credential_is_e_run_049_naming_operator_first_then_the_rest() {
        let record = record(
            Some("mdt1.op"),
            &[("watcher", "mdt1.w"), ("reader", "mdt1.r")],
        );
        let error = token_of(&record, "admin").unwrap_err();
        assert_eq!(error.code, E_RUN_049);
        assert_eq!(
            error.message,
            "ses_0199c4e2f1b7712a9d3e4f5061728394 minted no credential 'admin'; it has: \
             operator, reader, watcher"
        );
        assert!(!error.message.contains("mdt1."));
    }
}
