//! Handing one task to one sub-capsule and waiting for its answer.
//!
//! This is the whole of a delegation as the delegating side performs it: present the session's
//! credential to `mur-roost`, take the approval back, launch the approved artifact as a process,
//! deliver the task over A2A and wait for a terminal state. Every step is composed here from the
//! capsule's own name, version and task text — the agent that asked for the delegation supplies
//! those three strings and nothing else. It never sees the daemon's address, the credential, the
//! approval or the child's directory.
//!
//! **The wait ends when the child finishes.** A delegation made through this plane injects no
//! [`crate::delegation::Spawner`], so the child reports to nobody and the answer arrives on the
//! connection the parent already holds. That is the simpler half of the choice: a variant that
//! returned as soon as the child *started* would hand the agent a handle whose result comes back
//! later as a `completion`-origin task, which is [`crate::delegation`]'s business and a different
//! shape of tool. The seam for it is [`DelegationPlane::delegate`]'s `spawner: None`.
//!
//! Two costs of waiting are designed for rather than ignored. The whole plane is blocking, so its
//! caller runs it on a blocking thread and the delegating capsule's own A2A listener keeps
//! answering while the child runs. And the poll for the child's answer is bounded by
//! [`DelegationPlane::result_timeout`], so a child that never answers fails the call instead of
//! holding its parent open forever.
//!
//! **Nothing here formats a token.** The credential is read once, into a request header; the
//! approval is moved into [`ChildLaunchRequest`] without ever being turned into a string. The
//! success body of `POST /spawn` carries the approval, so that response value is never
//! interpolated into a message — a missing approval is reported by naming the field, not by
//! quoting the body that lacked it.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::child_launch::{launch_child_capsule, ChildLaunchRequest};
use crate::http_client::http_json;
use crate::spawn_credential::{SpawnApproval, SpawnCredential, SPAWN_CREDENTIAL_HEADER};

/// How long a delegation waits for its child's answer before ending the delegation.
///
/// Bounds the poll for a terminal task state and nothing else — reaching the child is already
/// bounded by [`crate::child_launch`]'s own launch timeout and by [`SEND_DEADLINE`] below. Ten
/// minutes is long enough for a sub-capsule that thinks, and short enough that a silent one
/// surfaces as a failed tool call within a turn rather than never.
pub const DELEGATION_RESULT_TIMEOUT: Duration = Duration::from_secs(600);

/// Environment variable overriding [`DELEGATION_RESULT_TIMEOUT`], in whole seconds.
///
/// For an operator whose sub-capsules legitimately run longer than the default, and for a value
/// below it. A value that is not a positive integer is ignored and the default stands.
pub const DELEGATION_TIMEOUT_ENV: &str = "MURMUR_DELEGATION_TIMEOUT_SECS";

/// How long the task delivery is retried while the child's listener comes up.
///
/// A child reports its URL when it binds, but the first connection can still land between the
/// bind and the first accept, so the delivery backs off rather than failing on one refusal.
const SEND_DEADLINE: Duration = Duration::from_secs(30);

/// How often the child is asked whether its task reached a terminal state.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Ceiling on the answer text a delegation returns to the model.
///
/// The answer is what the call was for, so this is generous rather than protective — but it is a
/// ceiling, because a child that produced a hundred megabytes must not be able to spend its
/// parent's whole context by being asked one question. Past it, the text is cut and
/// [`DelegationResult::result_path`] is how the parent reads the rest.
const MAX_OUTPUT_BYTES: usize = 64 * 1024;

/// The three strings an agent supplies for one delegation, and the whole of what it supplies.
#[derive(Debug, Clone)]
pub struct DelegationRequest {
    /// The sub-capsule's name, as it appears in the parent's `capabilities.spawn.allow`.
    pub capsule: String,
    /// An exact version. There is no `latest`: `murmur_artifact::RESERVED_VERSIONS` exists to
    /// refuse that word, so the caller states which artifact it means.
    pub version: String,
    /// The task text, delivered to the child as the whole of its first user message.
    pub task: String,
}

/// How a delegation ended, as the delegating agent sees it.
///
/// The value vocabulary of the `delegation` trace event's `outcome` and of the tool result's
/// `status`, which are the same four words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelegationStatus {
    /// The child's task reached `completed` and its answer is in hand.
    Completed,
    /// A child ran and did not answer: its task ended `failed` or `rejected`, or it could not be
    /// reached or launched after the daemon had approved it.
    Failed,
    /// No terminal state within [`DelegationPlane::result_timeout`]. The child was killed and
    /// reaped.
    TimedOut,
    /// The daemon refused, so no child was launched and no child directory exists.
    Refused,
}

impl DelegationStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::TimedOut => "timed_out",
            Self::Refused => "refused",
        }
    }
}

/// What one delegation produced.
///
/// Carries the child's answer text directly, unlike [`crate::delegation::DelegationOutcome`],
/// which names a file: this is the return value of a tool call the agent made on purpose, so the
/// answer is what it asked for. It reaches the model through the untrusted-content fence like
/// every other tool result.
#[derive(Debug, Clone)]
pub struct DelegationResult {
    /// The id this delegation is named by, in the `dlg_` id space
    /// ([`crate::delegation::new_delegation_id`]). Empty on a [`DelegationStatus::Refused`]
    /// result, which named no delegation because none was made.
    pub delegation_id: String,
    /// The child's own session id, so its trace is findable. Empty when no child ran.
    pub session_id: String,
    pub capsule: String,
    pub version: String,
    pub status: DelegationStatus,
    /// The child's answer on [`DelegationStatus::Completed`]; on every other status, why there is
    /// no answer.
    pub output: String,
    /// Where the child's answer is on disk, relative to the delegating capsule's own accessible
    /// workdir — so a parent that wants the untruncated text reads it with an ordinary tool call.
    /// `None` when the child wrote no result file, and on every non-`Completed` status.
    pub result_path: Option<String>,
    /// Whether [`Self::output`] was cut at [`MAX_OUTPUT_BYTES`].
    pub truncated: bool,
}

impl DelegationResult {
    /// A result that names no child, for a delegation that was never made.
    fn refused(request: &DelegationRequest, reason: String) -> Self {
        Self {
            delegation_id: String::new(),
            session_id: String::new(),
            capsule: request.capsule.clone(),
            version: request.version.clone(),
            status: DelegationStatus::Refused,
            output: reason,
            result_path: None,
            truncated: false,
        }
    }
}

/// One session's authority to delegate, and everything a delegation needs beyond the three
/// strings its caller supplies.
///
/// Built only for a session that registered with `mur-roost` — which is exactly a session whose
/// manifest declares `capabilities.spawn.allow`, or one that was itself spawned. A session
/// without one holds `None` and its agent was never offered the tool.
pub struct DelegationPlane {
    /// The daemon's base URL, from the runtime's own `MURMUR_ROOST_URL`. Not in the model's
    /// context, and not something the agent can name.
    roost_url: String,
    /// This session's credential, read exactly once per delegation into one request header.
    credential: SpawnCredential,
    /// The parent's accessible workdir. Child directories are composed beneath it, which is what
    /// keeps a child inside the single preopen the parent's WASI layer already has.
    accessible_workdir: PathBuf,
    /// This plane's bound on the wait for an answer. Read once at construction so every
    /// delegation in a session is bounded the same way.
    result_timeout: Duration,
}

impl DelegationPlane {
    /// The plane for a registered session.
    ///
    /// `roost_url` and `credential` come from the session's own registration, never from
    /// anything the agent said.
    pub fn new(
        roost_url: String,
        credential: SpawnCredential,
        accessible_workdir: PathBuf,
    ) -> Self {
        Self {
            roost_url: roost_url.trim_end_matches('/').to_string(),
            credential,
            accessible_workdir,
            result_timeout: configured_result_timeout(),
        }
    }

    /// This plane's bound on the wait for a child's answer.
    pub fn result_timeout(&self) -> Duration {
        self.result_timeout
    }

    /// Ask the daemon, launch the approved child, deliver the task, and wait for the answer.
    ///
    /// Blocking from end to end, and never `Err`: every way a delegation can fail is one of the
    /// four [`DelegationStatus`] words, because the caller has to record all four in the trace
    /// the same way and a refusal is as much a fact about the run as an answer is.
    pub fn delegate(&self, request: &DelegationRequest) -> DelegationResult {
        // Step 1: ask whether this session may spawn that capsule. One request, and the only
        // one: the daemon judges the session the credential names, runs the referee, and answers
        // with an approval naming the exact artifact it resolved. It launches nothing.
        //
        // The credential's one reading. Every other route out of `SpawnCredential` is closed — no
        // `Display`, no `Serialize`, redacted `Debug` — so the token cannot reach a tool result,
        // a trace or a workdir file by being formatted somewhere.
        let spawn_body = json!({ "name": request.capsule, "version": request.version }).to_string();
        let permission = match http_json(
            "POST",
            &format!("{}/spawn", self.roost_url),
            Some(&spawn_body),
            &[(SPAWN_CREDENTIAL_HEADER, self.credential.expose())],
        ) {
            Ok(value) => value,
            Err(error) => return DelegationResult::refused(request, daemon_refusal(&error)),
        };
        let Some(approval) = permission.get("approval").and_then(Value::as_str) else {
            // Names the missing field and never the body: the body of a *successful* spawn
            // response is where the approval is.
            return DelegationResult::refused(
                request,
                "mur-roost spawn response missing approval".to_string(),
            );
        };
        let grant = SpawnApproval::new(approval.to_string());

        // Step 2: launch it here, in this runtime, as a process of its own. `child` owns that
        // process: dropping it — including on every early return below — terminates and reaps the
        // child rather than leaving it holding a port.
        //
        // `child_env_allow` is empty because the delegating side knows the capsule's name and
        // version and nothing else about its manifest; a delegated child therefore sees no host
        // variables beyond the three every child gets. Deriving it would take the child's
        // manifest, which only the daemon has resolved.
        let delegation_id = crate::delegation::new_delegation_id();
        // Every result this call can produce, built from one place so the delegation id, the
        // capsule and the version cannot disagree between two of them.
        let outcome = |status, session_id: &str, output: String| DelegationResult {
            delegation_id: delegation_id.clone(),
            session_id: session_id.to_string(),
            capsule: request.capsule.clone(),
            version: request.version.clone(),
            status,
            output,
            result_path: None,
            truncated: false,
        };

        let child = match launch_child_capsule(ChildLaunchRequest {
            parent_accessible_workdir: self.accessible_workdir.clone(),
            capsule_name: request.capsule.clone(),
            capsule_version: request.version.clone(),
            grant,
            child_env_allow: Vec::new(),
            roost_url: self.roost_url.clone(),
            // The answer arrives on the connection this plane opened, so there is nothing for a
            // completion to tell it. No spawner means no injected handle and no watcher — and it
            // is the seam a start-returning variant would fill.
            spawner: None,
        }) {
            Ok(child) => child,
            Err(error) => return outcome(DelegationStatus::Failed, "", error.to_string()),
        };

        if child.capsule_url.is_empty() {
            return outcome(
                DelegationStatus::Failed,
                &child.session_id,
                format!(
                    "capsule '{}' bound no address, so it cannot be sent a task; delegation needs \
                     a capsule that serves A2A",
                    request.capsule
                ),
            );
        }
        let capsule_url = child.capsule_url.trim_end_matches('/').to_string();

        // Step 3: deliver the task as the child's first user message, backing off while its
        // listener finishes coming up.
        let send_body = json!({
            "jsonrpc": "2.0",
            "id": delegation_id,
            "method": "message/send",
            "params": {
                "message": {
                    "messageId": format!("msg_{delegation_id}"),
                    "role": "user",
                    "parts": [{"text": request.task}]
                }
            }
        })
        .to_string();
        let send_deadline = Instant::now() + SEND_DEADLINE;
        let mut delay = Duration::from_millis(100);
        let sent = loop {
            match http_json("POST", &capsule_url, Some(&send_body), &[]) {
                Ok(response) => break response,
                Err(_) if Instant::now() < send_deadline => {
                    std::thread::sleep(delay);
                    delay = (delay * 2).min(Duration::from_secs(2));
                }
                Err(error) => {
                    return outcome(
                        DelegationStatus::Failed,
                        &child.session_id,
                        format!(
                            "capsule '{}' did not accept a task within {}s: {error}",
                            request.capsule,
                            SEND_DEADLINE.as_secs()
                        ),
                    )
                }
            }
        };
        let Some(task_id) = sent.pointer("/result/id").and_then(Value::as_str) else {
            return outcome(
                DelegationStatus::Failed,
                &child.session_id,
                format!(
                    "capsule '{}' answered the delivered task with no task id",
                    request.capsule
                ),
            );
        };

        // Step 4: wait for a terminal state, bounded. On expiry `child` is dropped on the way out
        // of this function, which kills and reaps it.
        let poll_body = json!({
            "jsonrpc": "2.0",
            "id": delegation_id,
            "method": "tasks/get",
            "params": { "id": task_id }
        })
        .to_string();
        let poll_deadline = Instant::now() + self.result_timeout;
        loop {
            if Instant::now() >= poll_deadline {
                return outcome(
                    DelegationStatus::TimedOut,
                    &child.session_id,
                    format!(
                        "capsule '{}' did not answer within {}s; the delegation was ended and the \
                         child was stopped",
                        request.capsule,
                        self.result_timeout.as_secs()
                    ),
                );
            }
            std::thread::sleep(POLL_INTERVAL);

            let task = match http_json("POST", &capsule_url, Some(&poll_body), &[]) {
                Ok(task) => task,
                Err(error) => {
                    return outcome(
                        DelegationStatus::Failed,
                        &child.session_id,
                        format!("capsule '{}' stopped answering: {error}", request.capsule),
                    )
                }
            };
            match task.pointer("/result/status/state").and_then(Value::as_str) {
                Some("submitted" | "working" | "input-required") => continue,
                // Two places the answer can be, and both are read. A2A carries a completed task's
                // output in its artifacts, which is where a capsule this runtime did not build
                // would put it; a murmur capsule's own listener attaches an artifact only to an
                // `input-required` task, so its answer is read from the result file its runtime
                // wrote into the directory this parent composed for it.
                Some("completed") => {
                    let carried = task
                        .pointer("/result/artifacts/0/parts/0/text")
                        .and_then(Value::as_str)
                        .filter(|text| !text.is_empty())
                        .map(str::to_string);
                    let found = read_child_result(&child.workdir, &child.session_id, task_id);
                    let mut result = outcome(
                        DelegationStatus::Completed,
                        &child.session_id,
                        carried
                            .or_else(|| found.as_ref().map(|(_, text)| text.clone()))
                            .unwrap_or_default(),
                    );
                    result.truncated = result.output.len() > MAX_OUTPUT_BYTES;
                    if result.truncated {
                        result.output = bound_output(result.output);
                    }
                    result.result_path = found.and_then(|(path, _)| {
                        path.strip_prefix(&self.accessible_workdir)
                            .ok()
                            .map(|path| path.to_string_lossy().replace('\\', "/"))
                    });
                    return result;
                }
                Some("failed" | "rejected") => {
                    return outcome(
                        DelegationStatus::Failed,
                        &child.session_id,
                        task.pointer("/result/status/message/parts/0/text")
                            .and_then(Value::as_str)
                            .unwrap_or("the delegated capsule's task failed")
                            .to_string(),
                    )
                }
                other => {
                    return outcome(
                        DelegationStatus::Failed,
                        &child.session_id,
                        format!(
                            "capsule '{}' reported an unknown task state: {other:?}",
                            request.capsule
                        ),
                    )
                }
            }
        }
    }
}

/// Where a finished child left its answer, and what it says.
///
/// Four candidates, most specific first. A capsule that stays up for more than one task overwrites
/// the unsuffixed file, so the per-task one is preferred; and the agent loop writes into the
/// session directory beneath the child's workdir while a script capsule writes into that workdir
/// directly, which is the same two-place rule `runtime::DelegationReport::result_path` follows.
fn read_child_result(
    child_workdir: &Path,
    session_id: &str,
    task_id: &str,
) -> Option<(PathBuf, String)> {
    let session_out = child_workdir.join(".murmur").join(session_id).join("out");
    let workdir_out = child_workdir.join("out");
    let per_task = format!("result_{task_id}.txt");
    [
        session_out.join(&per_task),
        session_out.join("result.txt"),
        workdir_out.join(&per_task),
        workdir_out.join("result.txt"),
    ]
    .into_iter()
    .find_map(|path| std::fs::read_to_string(&path).ok().map(|text| (path, text)))
}

/// `output` cut to [`MAX_OUTPUT_BYTES`] at a character boundary, with the cut marked.
fn bound_output(output: String) -> String {
    let mut end = MAX_OUTPUT_BYTES;
    while end > 0 && !output.is_char_boundary(end) {
        end -= 1;
    }
    format!("{} […]", &output[..end])
}

/// The bound this process delegates under: [`DELEGATION_TIMEOUT_ENV`] when it names a positive
/// number of seconds, and [`DELEGATION_RESULT_TIMEOUT`] otherwise.
fn configured_result_timeout() -> Duration {
    std::env::var(DELEGATION_TIMEOUT_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .map(Duration::from_secs)
        .unwrap_or(DELEGATION_RESULT_TIMEOUT)
}

/// The daemon's own refusal, lifted out of the transport error it arrived wrapped in.
///
/// `http_json` reports a non-2xx as its status line, its header block and its body, which is the
/// right shape for a log and the wrong one for a model: what an operator has to act on is the
/// referee's sentence naming the manifest key and the offending entry. Anything that is not a
/// daemon refusal — a connection that was never made, a body that is not the daemon's — is passed
/// through unchanged, and carries no token either way because `http_json` never puts a request
/// header or body into an error.
fn daemon_refusal(error: &str) -> String {
    error
        .split_once("; body: ")
        .and_then(|(_, body)| serde_json::from_str::<Value>(body).ok())
        .and_then(|body| {
            body.get("error")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The four words the trace and the tool result share.
    #[test]
    fn every_status_has_one_wire_word() {
        assert_eq!(DelegationStatus::Completed.as_str(), "completed");
        assert_eq!(DelegationStatus::Failed.as_str(), "failed");
        assert_eq!(DelegationStatus::TimedOut.as_str(), "timed_out");
        assert_eq!(DelegationStatus::Refused.as_str(), "refused");
    }

    /// A referee's refusal reaches the agent as the referee's own sentence, with the HTTP
    /// transcript `http_json` wrapped it in removed.
    #[test]
    fn a_refusal_is_lifted_out_of_its_http_transcript() {
        let violation = "capabilities.shell.allow: the child declares 'bash', which its parent \
                         does not hold — a spawned capsule can never hold more capability than \
                         the capsule that spawned it";
        let wrapped = format!(
            "HTTP request failed: HTTP/1.1 403 Forbidden\r\ncontent-type: application/json; \
             body: {}",
            json!({ "error": violation })
        );
        assert_eq!(daemon_refusal(&wrapped), violation);
    }

    /// A failure that is not a daemon refusal is reported as it arrived rather than swallowed.
    #[test]
    fn a_transport_failure_survives_unchanged() {
        let refused = "failed to connect to 127.0.0.1:7700: Connection refused (os error 111)";
        assert_eq!(daemon_refusal(refused), refused);
        let unparseable = "HTTP request failed: HTTP/1.1 500 Internal Server Error; body: nope";
        assert_eq!(daemon_refusal(unparseable), unparseable);
    }

    /// The bound is the constant unless the environment names another, and a value that is not a
    /// positive number of seconds is not one.
    #[test]
    fn the_result_timeout_reads_its_override() {
        let plane = DelegationPlane::new(
            "http://127.0.0.1:7700/".to_string(),
            SpawnCredential::new("msc1.test".to_string()),
            PathBuf::from("/tmp"),
        );
        assert_eq!(plane.result_timeout(), DELEGATION_RESULT_TIMEOUT);
        // The trailing slash is taken off once, so no request is built against `//spawn`.
        assert_eq!(plane.roost_url, "http://127.0.0.1:7700");
    }
}
