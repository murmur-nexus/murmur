//! What a session tells `mur-roost` about itself at launch, and what it is handed back.
//!
//! The daemon no longer launches anything, so it cannot know what a session holds by having
//! staged it. A session that declares `capabilities.spawn.allow` registers instead: it names
//! which artifact is running under which session id, and the daemon resolves that artifact from
//! its *own* registry and lowers the manifest into a [`crate::SpawnEnvelope`] itself. The
//! registrant never states its grants — a registrant that could would be a registrant that could
//! declare its own ceiling.
//!
//! What comes back is the session's [`SpawnCredential`], which is the only thing that makes a
//! later `POST /spawn` answerable. It is held in runtime memory for the life of the session and
//! reaches no workdir file, no environment variable and no trace record.
//!
//! Registration is required for a delegating session and fatal when it fails
//! ([`RuntimeError::SpawnRegistrationFailed`]): a capsule that can delegate but that the daemon
//! has never heard of is a capsule spawning outside the only thing that bounds it. Deregistration
//! is the mirror image and is best effort — a session that has already finished has nothing left
//! to refuse.

use serde_json::json;

use crate::errors::RuntimeError;
use crate::http_client::http_json;
use crate::spawn_credential::{
    SpawnApproval, SpawnCredential, SPAWN_APPROVAL_HEADER, SPAWN_CREDENTIAL_HEADER,
};

/// How a session ended, as `POST /deregister` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionOutcome {
    Complete,
    Failed,
}

impl SessionOutcome {
    /// The wire value, and the `status` `GET /status/{session_id}` reports afterwards.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Failed => "failed",
        }
    }
}

/// Announce a running session to the daemon and take the credential it mints.
///
/// `grant` is the approval a delegated child was launched with; it is presented in
/// [`SPAWN_APPROVAL_HEADER`] and redeemed exactly once by the daemon against the artifact it
/// resolves for `capsule_name`/`capsule_version`. `None` is a top-level launch, which the daemon
/// admits only for a name in its own `--spawn-allow`.
pub fn register_session(
    roost_url: &str,
    session_id: &str,
    capsule_name: &str,
    capsule_version: &str,
    grant: Option<&SpawnApproval>,
) -> Result<SpawnCredential, RuntimeError> {
    let roost_url = roost_url.trim_end_matches('/');
    let body = json!({
        "session_id": session_id,
        "name": capsule_name,
        "version": capsule_version,
    })
    .to_string();

    // The approval's one reading. `SpawnApproval` has no `Display`, no `Serialize` and a redacted
    // `Debug`, so this is the only route by which the token can reach the wire — and it reaches a
    // request header, never the body and never the process environment.
    let headers: Vec<(&str, &str)> = match grant {
        Some(grant) => vec![(SPAWN_APPROVAL_HEADER, grant.expose())],
        None => Vec::new(),
    };

    let response = http_json(
        "POST",
        &format!("{roost_url}/register"),
        Some(&body),
        &headers,
    )
    .map_err(|reason| RuntimeError::SpawnRegistrationFailed {
        roost_url: roost_url.to_string(),
        reason,
    })?;

    let credential = response
        .get("credential")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| RuntimeError::SpawnRegistrationFailed {
            roost_url: roost_url.to_string(),
            reason: "the register response carried no credential".to_string(),
        })?;

    Ok(SpawnCredential::new(credential.to_string()))
}

/// Tell the daemon this session has ended, so its credential stops verifying against anything.
///
/// Best effort by construction: the session is over, and a daemon that has restarted or gone away
/// already holds no record to retire. A failure here is reported on stderr and returns nothing —
/// failing a completed session because its bookkeeping call did not land would turn a successful
/// run into a failed one.
pub fn deregister_session(roost_url: &str, credential: &SpawnCredential, outcome: SessionOutcome) {
    let roost_url = roost_url.trim_end_matches('/');
    let body = json!({ "outcome": outcome.as_str() }).to_string();
    if let Err(reason) = http_json(
        "POST",
        &format!("{roost_url}/deregister"),
        Some(&body),
        &[(SPAWN_CREDENTIAL_HEADER, credential.expose())],
    ) {
        eprintln!("[capsule-runtime] could not deregister from mur-roost at {roost_url}: {reason}");
    }
}
