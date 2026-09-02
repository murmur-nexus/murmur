//! Handing one task to one sub-capsule and waiting for its answer.
//!
//! This is the whole of a delegation as the delegating side performs it: present the session's
//! credential to `mur-roost`, take the approval back, launch the approved artifact as a process,
//! deliver the task over A2A and wait for a terminal state. Every step is composed here from the
//! capsule's own name, version and task text — the agent that asked for the delegation supplies
//! those three strings and nothing else. It never sees the daemon's address, the credential, the
//! approval or the child's directory.
//!
//! **The wait ends when the child finishes.** A delegation made through this plane injects a
//! spawner with no [`crate::delegation::CompletionAddress`], so the child knows which session
//! spawned it and reports to nobody: the answer arrives on the connection the parent already
//! holds, rather than as a `completion`-origin task in [`crate::delegation`]'s lane.
//!
//! Waiting costs two things. The whole plane is blocking, so its caller runs it on a blocking
//! thread and the delegating capsule's own A2A listener keeps answering while the child runs. And
//! the poll for the child's answer is bounded by [`DelegationPlane::result_timeout`], so a child
//! that never answers fails the call instead of holding its parent open forever.
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
use crate::delegation::Spawner;
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
/// A child that produced a hundred megabytes must not be able to spend its parent's whole context
/// by being asked one question. Past this, the text is cut and [`DelegationResult::result_path`]
/// is how the parent reads the rest.
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
    /// ([`crate::delegation::new_delegation_id`]). Minted by the launcher, one per launched
    /// child, and empty whenever no child was launched — a refusal, or a process that never
    /// started. A delegation that was never made names no delegation.
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
    ///
    /// Two ways to reach it, and they name the same nothing: the daemon refused, or the approved
    /// child's process never started. A delegation id is minted by the launcher, so neither has
    /// one to report.
    fn unmade(request: &DelegationRequest, status: DelegationStatus, reason: String) -> Self {
        let truncated = reason.len() > MAX_OUTPUT_BYTES;
        Self {
            delegation_id: String::new(),
            session_id: String::new(),
            capsule: request.capsule.clone(),
            version: request.version.clone(),
            status,
            output: if truncated {
                bound_output(reason)
            } else {
                reason
            },
            result_path: None,
            truncated,
        }
    }

    /// A delegation the daemon refused, by the spawn envelope or by one of its own bounds.
    fn refused(request: &DelegationRequest, reason: String) -> Self {
        Self::unmade(request, DelegationStatus::Refused, reason)
    }
}

/// What the parent knows the moment one child is up, handed back while the delegation is still
/// running so the parent's trace names the child before it can hang, crash or be timed out.
#[derive(Debug, Clone)]
pub struct DelegationLaunch {
    /// The `dlg_` id the launcher minted for this launch.
    pub delegation_id: String,
    pub capsule: String,
    pub version: String,
    /// The session id the child's runtime minted for itself and reported on its launch line.
    pub child_session_id: String,
    /// The child's directory, relative to the parent's accessible workdir — the path a reader of
    /// the parent's trace joins to find the child's own `trace.jsonl`.
    pub child_workdir: String,
}

/// The parent's side of one delegation, beyond the three strings its agent supplied.
///
/// Per call rather than per plane: a launch that mints one context id per task has no
/// launch-scoped conversation to name, so the id is only knowable once a task is running.
#[derive(Debug, Clone, Default)]
pub struct DelegationOrigin {
    /// The conversation the delegation was made from. Empty when the caller has none, which
    /// injects no handle at all rather than one naming a conversation that does not exist.
    pub context_id: String,
    /// Where the launch notice goes, or `None` for a caller that records no `delegation_start`.
    pub launched: Option<tokio::sync::mpsc::UnboundedSender<DelegationLaunch>>,
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
    /// The delegating session's own id, written into every child's injected handle and from there
    /// into that child's `session_start.spawned_by`. Empty for a caller that does not know it,
    /// which injects no handle.
    session_id: String,
}

impl DelegationPlane {
    /// The plane for a registered session.
    ///
    /// `roost_url`, `credential` and `session_id` come from the session's own registration, never
    /// from anything the agent said.
    pub fn new(
        roost_url: String,
        credential: SpawnCredential,
        accessible_workdir: PathBuf,
        session_id: String,
    ) -> Self {
        Self {
            roost_url: roost_url.trim_end_matches('/').to_string(),
            credential,
            accessible_workdir,
            result_timeout: configured_result_timeout(),
            session_id,
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
    pub fn delegate(
        &self,
        request: &DelegationRequest,
        origin: &DelegationOrigin,
    ) -> DelegationResult {
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
        //
        // The spawner is lineage and nothing else. The answer arrives on the connection this
        // plane opened, so there is nothing for a completion to tell it: no address means no
        // watcher and no post, while the child still learns which session spawned it.
        let spawner =
            (!self.session_id.is_empty() && !origin.context_id.is_empty()).then(|| Spawner {
                session_id: self.session_id.clone(),
                context_id: origin.context_id.clone(),
                report_to: None,
            });
        let child = match launch_child_capsule(ChildLaunchRequest {
            parent_accessible_workdir: self.accessible_workdir.clone(),
            capsule_name: request.capsule.clone(),
            capsule_version: request.version.clone(),
            grant,
            child_env_allow: Vec::new(),
            roost_url: self.roost_url.clone(),
            spawner,
        }) {
            Ok(child) => child,
            // A child whose process never started named no delegation, the same as one the daemon
            // refused: the id is minted by the launcher, and this launch reached no launcher.
            Err(error) => {
                return DelegationResult::unmade(
                    request,
                    DelegationStatus::Failed,
                    error.to_string(),
                )
            }
        };
        // Adopted, never minted here: one launch, one id, and the same string the child was
        // injected with and wrote into its own `session_start`.
        let delegation_id = child.delegation_id.clone().unwrap_or_default();

        // Handed back the moment the child is up and has reported its session id, so a child that
        // then hangs, crashes or is timed out is already attributable from the parent's side.
        //
        // An unnamed delegation is not announced. A caller with no session id or no conversation
        // to name injects no handle, so the launcher minted no id and there is nothing for the
        // terminal `delegation` line — which writes `null` rather than an empty id — to join
        // against. `delegation_start.delegation_id` is required to be a `dlg_` id; a launch that
        // has none writes no line at all.
        if let Some(launched) = origin
            .launched
            .as_ref()
            .filter(|_| !delegation_id.is_empty())
        {
            let _ = launched.send(DelegationLaunch {
                delegation_id: delegation_id.clone(),
                capsule: request.capsule.clone(),
                version: request.version.clone(),
                child_session_id: child.session_id.clone(),
                child_workdir: child
                    .workdir
                    .strip_prefix(&self.accessible_workdir)
                    .map(|path| path.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_else(|_| child.workdir.to_string_lossy().replace('\\', "/")),
            });
        }

        // Every result this call can produce past the launch, built from one place so the
        // delegation id, the capsule and the version cannot disagree between two of them — and so
        // [`MAX_OUTPUT_BYTES`] bounds every branch. A child's *failure* message is as much its own
        // text as its answer is, so it is cut on the same terms.
        let outcome = |status, session_id: &str, output: String| {
            let truncated = output.len() > MAX_OUTPUT_BYTES;
            DelegationResult {
                delegation_id: delegation_id.clone(),
                session_id: session_id.to_string(),
                capsule: request.capsule.clone(),
                version: request.version.clone(),
                status,
                output: if truncated {
                    bound_output(output)
                } else {
                    output
                },
                result_path: None,
                truncated,
            }
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
    result_timeout_from(std::env::var(DELEGATION_TIMEOUT_ENV).ok().as_deref())
}

/// [`configured_result_timeout`]'s rule, without the environment read, so it is testable without
/// mutating process-wide state that every other test in this binary shares.
fn result_timeout_from(value: Option<&str>) -> Duration {
    value
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

    /// The bound is the constant unless the override names another, and a value that is not a
    /// positive number of seconds does not name one.
    #[test]
    fn the_result_timeout_reads_its_override() {
        assert_eq!(result_timeout_from(Some(" 20 ")), Duration::from_secs(20));
        for ignored in [None, Some(""), Some("0"), Some("-5"), Some("later")] {
            assert_eq!(
                result_timeout_from(ignored),
                DELEGATION_RESULT_TIMEOUT,
                "{ignored:?} is not a positive number of seconds"
            );
        }
    }

    /// The ceiling holds on every status, not only on an answer: a child's failure message and a
    /// daemon's refusal reach the same model context an answer does.
    #[test]
    fn the_output_ceiling_bounds_a_refusal_too() {
        let request = DelegationRequest {
            capsule: "worker".to_string(),
            version: "0.1.0".to_string(),
            task: "t".to_string(),
        };
        let result = DelegationResult::refused(&request, "x".repeat(MAX_OUTPUT_BYTES + 4096));
        assert!(result.truncated);
        assert!(result.output.len() <= MAX_OUTPUT_BYTES + " […]".len());
        assert!(result.output.ends_with(" […]"));

        let short = DelegationResult::refused(&request, "no".to_string());
        assert!(!short.truncated);
        assert_eq!(short.output, "no");
    }

    /// A cut lands on a character boundary rather than splitting a multi-byte character.
    #[test]
    fn a_cut_answer_stays_valid_utf8() {
        let output = "é".repeat(MAX_OUTPUT_BYTES);
        let bounded = bound_output(output);
        assert!(bounded.ends_with(" […]"));
        assert!(bounded.len() <= MAX_OUTPUT_BYTES + " […]".len());
    }

    /// The trailing slash is taken off once, so no request is built against `//spawn`.
    #[test]
    fn a_trailing_slash_on_the_daemon_url_is_trimmed_once() {
        let plane = DelegationPlane::new(
            "http://127.0.0.1:7700/".to_string(),
            SpawnCredential::new("msc1.test".to_string()),
            PathBuf::from("/tmp"),
            "ses_parent".to_string(),
        );
        assert_eq!(plane.roost_url, "http://127.0.0.1:7700");
    }
}
