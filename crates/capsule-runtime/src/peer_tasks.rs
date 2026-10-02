//! Whether the A2A door serves a task another capsule's runtime sent, and the refusal it answers
//! with when it does not.
//!
//! The decision has two inputs: the [`TaskProvenance`] the door classified from the request's
//! headers, and the capsule's own `exports.peer_tasks.accept`, read through
//! [`murmur_artifact::RuntimeManifest::accepts_peer_tasks`]. Nothing else — no flag, environment
//! variable, control setting or launcher argument — reaches it.
//!
//! "Peer" here is the origin [`crate::origin::from_wire`] classifies, which a murmur runtime
//! stamps on what it sends and which no guest can set. It is still the caller's claim: a caller
//! that sends no origin header is classified `event` and is governed by the door's authentication
//! alone, because consent does not turn an event into a peer.
//!
//! The refusal is one fixed response. It carries no `www-authenticate` challenge, since no
//! credential would change the answer, and it names no task, file, session or credential, so the
//! same bytes answer every method, path and body.

use crate::origin::{TaskOrigin, TaskProvenance};
use crate::resource_plane::ResourceResponse;

/// The `error` code of a refused peer task.
pub(crate) const PEER_NOT_ACCEPTED_CODE: &str = "peer_not_accepted";

/// The `message` of a refused peer task.
pub(crate) const PEER_NOT_ACCEPTED_MESSAGE: &str = "this capsule does not accept tasks from peers";

/// Whether the door refuses a request of `provenance`: exactly when it is peer-origin and the
/// capsule does not consent. A `completion` is not a peer task — it is a delegated child
/// reporting to the parent that delegated to it — and is never refused here.
pub(crate) fn refuses(provenance: TaskProvenance, accepts_peer_tasks: bool) -> bool {
    provenance.origin() == TaskOrigin::Peer && !accepts_peer_tasks
}

/// The `403` a refused peer task is answered with, byte-identical for every request it refuses.
pub(crate) fn refusal() -> ResourceResponse {
    let body = serde_json::json!({
        "error": PEER_NOT_ACCEPTED_CODE,
        "message": PEER_NOT_ACCEPTED_MESSAGE,
    });
    ResourceResponse::framed(
        403,
        vec![("content-type".to_string(), "application/json".to_string())],
        serde_json::to_vec(&body).unwrap_or_default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::origin::TrustClass;

    const ALL_ORIGINS: [TaskOrigin; 6] = [
        TaskOrigin::User,
        TaskOrigin::Peer,
        TaskOrigin::Schedule,
        TaskOrigin::Event,
        TaskOrigin::Completion,
        TaskOrigin::System,
    ];

    #[test]
    fn only_a_peer_task_to_a_capsule_that_does_not_consent_is_refused() {
        for origin in ALL_ORIGINS {
            for inherited in [None, Some(TrustClass::Trusted), Some(TrustClass::Untrusted)] {
                let provenance = TaskProvenance::derive(origin, inherited);
                for accepts in [false, true] {
                    assert_eq!(
                        refuses(provenance, accepts),
                        origin == TaskOrigin::Peer && !accepts,
                        "{origin:?} / {inherited:?} / accepts {accepts}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_refusal_is_a_fixed_403_with_no_challenge() {
        let response = refusal();
        assert_eq!(response.status, 403);
        assert_eq!(
            response.headers,
            vec![
                ("content-type".to_string(), "application/json".to_string()),
                (
                    "content-length".to_string(),
                    response.body.len().to_string()
                ),
            ]
        );
        assert!(response
            .headers
            .iter()
            .all(|(name, _)| !name.eq_ignore_ascii_case("www-authenticate")));
        assert_eq!(
            String::from_utf8(response.body.clone()).unwrap(),
            r#"{"error":"peer_not_accepted","message":"this capsule does not accept tasks from peers"}"#
        );
        assert_eq!(refusal(), response, "every refusal is the same bytes");
    }
}
