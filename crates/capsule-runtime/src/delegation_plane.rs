//! Handing one task to one sub-capsule, and learning what became of it.
//!
//! This is the whole of a delegation as the delegating side performs it: present the session's
//! credential to `mur-roost`, take the approval back, launch the approved artifact as a process
//! and deliver the task over A2A. Every step is composed here from the capsule's own name, version
//! and task text — the agent that asked for the delegation supplies those three strings and
//! nothing else. It never sees the daemon's address, the credential, the approval or the child's
//! directory.
//!
//! **Two methods, and they differ only in when they return.**
//!
//! | Method | Returns when | Statuses it can produce | Its caller |
//! |---|---|---|---|
//! | [`DelegationPlane::start`] | the child is running and holding its task | `started`, `failed`, `refused` | the agent-facing `delegate-task` tool |
//! | [`DelegationPlane::delegate`] | the child's task reaches a terminal state | `completed`, `timed_out`, `failed`, `refused` | a plan's `capsule` step |
//!
//! [`DelegationPlane::start`] is the one an agent reaches. It releases the child and returns, and
//! the outcome arrives at the parent later as a `completion`-origin task in the background lane —
//! which is what [`crate::delegation`] exists for, and why a plane that was never told its own
//! address through [`DelegationPlane::reporting_to`] refuses to start anything.
//!
//! [`DelegationPlane::delegate`] blocks because its caller has nowhere to be told: a plan step
//! holds no task loop, no conversation id and no A2A door, so a handle naming a delegation in
//! flight would be a result it could never collect. It therefore injects a spawner with no
//! [`crate::delegation::CompletionAddress`] — the child knows which session spawned it and reports
//! to nobody, because the answer arrives on the connection the step already holds.
//!
//! Both are blocking calls, so their caller runs them on a blocking thread and the delegating
//! capsule's own A2A listener keeps answering throughout.
//!
//! **Nothing here formats a token.** The credential is read once, into a request header; the
//! approval is moved into [`ChildLaunchRequest`] without ever being turned into a string. The
//! success body of `POST /spawn` carries the approval, so that response value is never
//! interpolated into a message — a missing approval is reported by naming the field, not by
//! quoting the body that lacked it.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::child_launch::{launch_child_capsule, workdir_relative_to, ChildLaunchRequest};
use crate::delegation::{CompletionAddress, Spawner};
use crate::errors::RuntimeError;
use crate::http_client::http_json;
use crate::origin::TrustClass;
use crate::spawn_credential::{SpawnApproval, SpawnCredential, SPAWN_CREDENTIAL_HEADER};

/// The deadline a session delegates under when nothing declares one.
///
/// **The single delegation bound**, and the only knob on either of the two waits a delegation
/// still has: [`DelegationPlane::delegate`]'s poll for a terminal task state, and the completion
/// watcher's observation of a child [`DelegationPlane::start`] released. Everything else in a
/// launch is bounded elsewhere and separately — reaching the child by [`crate::child_launch`]'s
/// own launch timeout, delivering its task by [`SEND_DEADLINE`] below.
///
/// The same number `murmur_artifact::LifecycleConfig::default().delegation_deadline_secs` carries,
/// and the value a caller with no manifest to read passes in; the declared key and
/// [`DELEGATION_TIMEOUT_ENV`] are two ways of setting this one bound, not two bounds. Ten minutes
/// is long enough for a sub-capsule that thinks, and short enough that a wedged one is reported
/// within the hour rather than never.
pub const DELEGATION_RESULT_TIMEOUT: Duration = Duration::from_secs(600);

/// Environment variable setting [`DELEGATION_RESULT_TIMEOUT`] for this process, in whole seconds,
/// over both the declared `lifecycle.delegation_deadline_secs` and the default.
///
/// For an operator whose sub-capsules legitimately run longer than the capsule declared, and for a
/// value below it. A value that is not a positive integer is ignored and the declared bound stands.
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

/// Where a delegation got to, as the delegating caller sees it.
///
/// The value vocabulary of the `delegation` trace event's `outcome` and of the `delegate-task`
/// tool result's `status`, which are the same five words. Which of the two plane methods can
/// produce each one is stated on the variant, because a caller that reads a word this method
/// cannot produce is reading the wrong contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelegationStatus {
    /// The child is running and holding its task, and nothing is known yet about what it will do
    /// with it. Produced only by [`DelegationPlane::start`]; the outcome arrives afterwards as a
    /// `completion`-origin task.
    Started,
    /// The child's task reached `completed` and its answer is in hand. Produced only by
    /// [`DelegationPlane::delegate`].
    Completed,
    /// A child ran and did not answer: its task ended `failed` or `rejected`, or it could not be
    /// reached, launched or handed its task after the daemon had approved it. Produced by both
    /// methods.
    Failed,
    /// No terminal state within [`DelegationPlane::result_timeout`]. Produced only by
    /// [`DelegationPlane::delegate`]: a child [`DelegationPlane::start`] released is bounded by
    /// its watcher instead, which ends it and posts a `terminated` completion.
    TimedOut,
    /// The daemon refused, so no child was launched and no child directory exists. Produced by
    /// both methods.
    Refused,
}

impl DelegationStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Started => "started",
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
    /// The child's directory, relative to the delegating capsule's own accessible workdir — the
    /// path a parent joins to reach the child's `trace.jsonl` and, once the child ends, its
    /// result file.
    ///
    /// `Some` only on [`DelegationStatus::Started`], which is the one status where a directory is
    /// all there is to name.
    pub child_workdir: Option<String>,
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
            child_workdir: None,
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
    /// The trust class of the task that made the delegation, inherited by the completion this
    /// delegation eventually posts.
    ///
    /// `None` means [`TrustClass::Untrusted`], which is what an inbound task that claimed no class
    /// is already given. Read only by [`DelegationPlane::start`]: a delegation that posts no
    /// completion has no trust to carry.
    pub trust: Option<TrustClass>,
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
    /// This plane's bound on the wait for an answer: the declared
    /// `lifecycle.delegation_deadline_secs`, or [`DELEGATION_TIMEOUT_ENV`] where it names a
    /// positive number of seconds. Read once at construction so every delegation in a session is
    /// bounded the same way.
    result_timeout: Duration,
    /// The delegating session's own id, written into every child's injected handle and from there
    /// into that child's `session_start.spawned_by`. Empty for a caller that does not know it,
    /// which injects no handle.
    session_id: String,
    /// Where a child's own manifest is read from, so the parent knows which host variables that
    /// child declares. The same store the child's runtime will resolve the artifact out of, so
    /// the manifest read here is the manifest the child will load.
    registry: std::sync::Arc<dyn murmur_artifact::Registry>,
    /// The delegating capsule's own `capabilities.env.allow`. Every child's declaration is
    /// clamped to this list, so a delegation can never widen the set of host variables the
    /// parent's own manifest declares.
    parent_env_allow: Vec<String>,
    /// This capsule's own A2A endpoint, `http://host:port`, where a started delegation's
    /// completion is posted. Empty until [`DelegationPlane::reporting_to`] names it, and a plane
    /// that never names one cannot [`DelegationPlane::start`] anything: a delegation whose outcome
    /// has nowhere to arrive is a way to lose work.
    own_url: String,
}

impl DelegationPlane {
    /// The plane for a registered session.
    ///
    /// `roost_url`, `credential` and `session_id` come from the session's own registration, never
    /// from anything the agent said. `declared_deadline` is the capsule's own
    /// `lifecycle.delegation_deadline_secs`; [`DELEGATION_TIMEOUT_ENV`] overrides it where it
    /// names a positive number of seconds. `registry` is the store a child's manifest is read
    /// from and `parent_env_allow` the delegating capsule's own `capabilities.env.allow`, which
    /// together decide the host variables a child is handed.
    pub fn new(
        roost_url: String,
        credential: SpawnCredential,
        accessible_workdir: PathBuf,
        session_id: String,
        declared_deadline: Duration,
        registry: std::sync::Arc<dyn murmur_artifact::Registry>,
        parent_env_allow: Vec<String>,
    ) -> Self {
        Self {
            roost_url: roost_url.trim_end_matches('/').to_string(),
            credential,
            accessible_workdir,
            result_timeout: configured_result_timeout(declared_deadline),
            session_id,
            registry,
            parent_env_allow,
            own_url: String::new(),
        }
    }

    /// Name this capsule's own A2A endpoint, `http://host:port`, as where a started delegation's
    /// completion is posted.
    ///
    /// Separate from [`DelegationPlane::new`] because only a caller that runs a task loop behind
    /// that address can act on a completion. A plane built without it still delegates through
    /// [`DelegationPlane::delegate`], which is answered on the connection it opens; it refuses
    /// [`DelegationPlane::start`], which is not.
    pub fn reporting_to(mut self, own_url: String) -> Self {
        self.own_url = own_url;
        self
    }

    /// This plane's bound on the wait for a child's answer.
    pub fn result_timeout(&self) -> Duration {
        self.result_timeout
    }

    /// The host variables this launch copies into the child: what the child's own manifest
    /// declares under `capabilities.env.allow`, clamped to what the parent's declares.
    ///
    /// The manifest is read narrowly, out of the packed artifact, because a full parse resolves
    /// the child's `${VAR}` references against this process's environment and would refuse a
    /// manifest naming a variable the parent has not been given. An artifact that cannot be
    /// resolved, or whose declaration cannot be read, is an error naming the capsule and version
    /// rather than an empty list: a child launched without what it declares fails later and less
    /// legibly.
    fn child_env_allow(&self, capsule: &str, version: &str) -> Result<Vec<String>, RuntimeError> {
        let resolved = self
            .registry
            .resolve_with_platform(capsule, version, Some(murmur_artifact::current_platform()))
            .map_err(|error| {
                RuntimeError::Runtime(format!(
                    "cannot read what '{capsule}@{version}' declares under \
                     capabilities.env.allow: {error}"
                ))
            })?;
        let manifest_yaml =
            crate::artifact::extract_manifest_yaml(capsule, version, &resolved.bytes)?;
        let declared =
            crate::artifact::extract_declared_env_allow(capsule, version, &manifest_yaml)?;
        Ok(env_allow_intersection(&declared, &self.parent_env_allow))
    }

    /// A child's directory named from the parent's accessible workdir, which is the only root the
    /// parent's own tools address.
    fn workdir_relative(&self, workdir: &Path) -> String {
        workdir_relative_to(workdir, &self.accessible_workdir)
    }

    /// Ask the daemon whether this session may spawn that capsule, and take the approval back.
    ///
    /// One request, and the only one: the daemon judges the session the credential names, runs the
    /// referee, and answers with an approval naming the exact artifact it resolved. It launches
    /// nothing. `Err` is the referee's own sentence, which both methods return to their caller as
    /// a [`DelegationStatus::Refused`] result unchanged.
    ///
    /// The credential's one reading. Every other route out of [`SpawnCredential`] is closed — no
    /// `Display`, no `Serialize`, redacted `Debug` — so the token cannot reach a tool result, a
    /// trace or a workdir file by being formatted somewhere.
    fn approval_for(&self, request: &DelegationRequest) -> Result<SpawnApproval, String> {
        let spawn_body = json!({ "name": request.capsule, "version": request.version }).to_string();
        let permission = match http_json(
            "POST",
            &format!("{}/spawn", self.roost_url),
            Some(&spawn_body),
            &[(SPAWN_CREDENTIAL_HEADER, self.credential.expose())],
        ) {
            Ok(value) => value,
            Err(error) => return Err(daemon_refusal(&error)),
        };
        let Some(approval) = permission.get("approval").and_then(Value::as_str) else {
            // Names the missing field and never the body: the body of a *successful* spawn
            // response is where the approval is.
            return Err("mur-roost spawn response missing approval".to_string());
        };
        Ok(SpawnApproval::new(approval.to_string()))
    }

    /// Hand the launch back to the caller the moment the child is up and has reported its session
    /// id, so a child that then hangs, crashes or is ended is already attributable.
    ///
    /// An unnamed delegation is not announced. A caller with no session id or no conversation to
    /// name injects no handle, so the launcher minted no id and there is nothing for the terminal
    /// `delegation` line — which writes `null` rather than an empty id — to join against.
    /// `delegation_start.delegation_id` is required to be a `dlg_` id; a launch that has none
    /// writes no line at all.
    fn announce(
        &self,
        request: &DelegationRequest,
        origin: &DelegationOrigin,
        child: &crate::child_launch::LaunchedChild,
        delegation_id: &str,
    ) {
        if let Some(launched) = origin
            .launched
            .as_ref()
            .filter(|_| !delegation_id.is_empty())
        {
            let _ = launched.send(DelegationLaunch {
                delegation_id: delegation_id.to_string(),
                capsule: request.capsule.clone(),
                version: request.version.clone(),
                child_session_id: child.session_id.clone(),
                child_workdir: self.workdir_relative(&child.workdir),
            });
        }
    }

    /// Ask the daemon, launch the approved child, hand it its task, and return.
    ///
    /// **The agent-facing `delegate-task` tool's method.** It returns as soon as the child is
    /// running *and holding its task*, so a turn can issue several delegations and carry on; what
    /// the child eventually did arrives at the parent afterwards as a `completion`-origin task
    /// carrying this delegation's id, posted by the child itself or, for a child that could not
    /// speak for itself, by the watcher [`launch_child_capsule`] started behind it.
    ///
    /// Blocking and never `Err`, like [`DelegationPlane::delegate`], but only three
    /// [`DelegationStatus`] words can come out of it: `started`, `failed` and `refused`.
    ///
    /// "Started" means the child holds its task, not merely that a process exists. Everything
    /// before that point kills and reaps the child on the way out, because a child that will never
    /// be given work will never report and would strand a process nothing waits on. Only the last
    /// step releases it, and from then on the completion watcher is its sole observer.
    pub fn start(
        &self,
        request: &DelegationRequest,
        origin: &DelegationOrigin,
    ) -> DelegationResult {
        // Refused before the daemon is touched, because there is nothing to ask about: a
        // delegation started here would run to completion and post its outcome nowhere, which is
        // a way to lose work rather than a way to do it. No `POST /spawn` is made and no process
        // is launched.
        if self.own_url.is_empty() || self.session_id.is_empty() || origin.context_id.is_empty() {
            return DelegationResult::unmade(
                request,
                DelegationStatus::Failed,
                "this capsule cannot start a delegation: a sub-capsule's outcome arrives as a \
                 task addressed to this runtime's own endpoint, session and conversation, and \
                 this session does not hold all three — so the outcome would have nowhere to be \
                 reported"
                    .to_string(),
            );
        }

        let grant = match self.approval_for(request) {
            Ok(grant) => grant,
            Err(reason) => return DelegationResult::refused(request, reason),
        };

        // The production caller of the completion path: this spawner names where the outcome goes
        // and under which trust, which is what starts the watcher behind the child. The deadline
        // is this plane's single bound, and here it bounds the watch rather than a poll.
        let child = match launch_child_capsule(ChildLaunchRequest {
            parent_accessible_workdir: self.accessible_workdir.clone(),
            capsule_name: request.capsule.clone(),
            capsule_version: request.version.clone(),
            grant,
            child_env_allow: Vec::new(),
            roost_url: self.roost_url.clone(),
            spawner: Some(Spawner {
                session_id: self.session_id.clone(),
                context_id: origin.context_id.clone(),
                report_to: Some(CompletionAddress {
                    url: self.own_url.clone(),
                    trust: origin.trust.unwrap_or(TrustClass::Untrusted),
                }),
            }),
            completion_deadline: Some(self.result_timeout),
        }) {
            Ok(child) => child,
            Err(error) => {
                return DelegationResult::unmade(
                    request,
                    DelegationStatus::Failed,
                    error.to_string(),
                )
            }
        };
        let delegation_id = child.delegation_id.clone().unwrap_or_default();
        self.announce(request, origin, &child, &delegation_id);

        let child_workdir = self.workdir_relative(&child.workdir);
        let failed = |session_id: &str, output: String| {
            let truncated = output.len() > MAX_OUTPUT_BYTES;
            DelegationResult {
                delegation_id: delegation_id.clone(),
                session_id: session_id.to_string(),
                capsule: request.capsule.clone(),
                version: request.version.clone(),
                status: DelegationStatus::Failed,
                output: if truncated {
                    bound_output(output)
                } else {
                    output
                },
                result_path: None,
                truncated,
                child_workdir: None,
            }
        };

        if child.capsule_url.is_empty() {
            return failed(
                &child.session_id,
                format!(
                    "capsule '{}' bound no address, so it cannot be sent a task; delegation needs \
                     a capsule that serves A2A",
                    request.capsule
                ),
            );
        }
        let capsule_url = child.capsule_url.trim_end_matches('/').to_string();
        if let Err(reason) = deliver_task(&capsule_url, &delegation_id, request) {
            return failed(&child.session_id, reason);
        }

        let session_id = child.session_id.clone();
        // Past this line the process outlives its handle, and the watcher thread is the only
        // thing that will ever observe or reap it.
        child.release();
        DelegationResult {
            delegation_id,
            session_id,
            capsule: request.capsule.clone(),
            version: request.version.clone(),
            status: DelegationStatus::Started,
            // Nothing has been produced yet, and saying so is the point of the status word.
            output: String::new(),
            result_path: None,
            truncated: false,
            child_workdir: Some(child_workdir),
        }
    }

    /// Ask the daemon, launch the approved child, deliver the task, and wait for the answer.
    ///
    /// **A plan `capsule` step's method, and not the agent-facing one.** A step has no task loop,
    /// no conversation id and no A2A door of its own, so the only place its answer can arrive is
    /// the connection this call is already holding. That is why the spawner injected below names
    /// no [`CompletionAddress`]: there is nothing for a completion to tell, and a watcher posting
    /// into a session that is not listening would only write an undeliverable record. The
    /// agent-facing tool calls [`DelegationPlane::start`] instead.
    ///
    /// Blocking from end to end, and never `Err`: every way a delegation can end here is one of
    /// four [`DelegationStatus`] words — `completed`, `timed_out`, `failed`, `refused` — because
    /// the caller has to record all four in the trace the same way and a refusal is as much a
    /// fact about the run as an answer is.
    pub fn delegate(
        &self,
        request: &DelegationRequest,
        origin: &DelegationOrigin,
    ) -> DelegationResult {
        // Step 1: ask whether this session may spawn that capsule.
        let grant = match self.approval_for(request) {
            Ok(grant) => grant,
            Err(reason) => return DelegationResult::refused(request, reason),
        };

        // Step 2: launch it here, in this runtime, as a process of its own. `child` owns that
        // process: dropping it — including on every early return below, and on the deadline in
        // step 4 — terminates and reaps the child rather than leaving it holding a port.
        //
        // `child_env_allow` is what both manifests declare and nothing else, read here rather
        // than taken from the daemon's answer: the referee answers whether a spawn may happen, and
        // the clamp has to hold whatever it answered. A read that fails starts no child — one
        // launched without the variables its manifest names dies at its own manifest load, inside
        // a process nobody is reading.
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
        let child_env_allow = match self.child_env_allow(&request.capsule, &request.version) {
            Ok(names) => names,
            Err(error) => {
                return DelegationResult::unmade(
                    request,
                    DelegationStatus::Failed,
                    error.to_string(),
                )
            }
        };
        let child = match launch_child_capsule(ChildLaunchRequest {
            parent_accessible_workdir: self.accessible_workdir.clone(),
            capsule_name: request.capsule.clone(),
            capsule_version: request.version.clone(),
            grant,
            child_env_allow,
            roost_url: self.roost_url.clone(),
            spawner,
            // No watcher runs for a spawner that names no address, so there is nothing here for a
            // deadline to bound; the poll in step 4 is this method's bound.
            completion_deadline: None,
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

        self.announce(request, origin, &child, &delegation_id);

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
                child_workdir: None,
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

        // Step 3: deliver the task as the child's first user message.
        let task_id = match deliver_task(&capsule_url, &delegation_id, request) {
            Ok(task_id) => task_id,
            Err(reason) => return outcome(DelegationStatus::Failed, &child.session_id, reason),
        };

        // Step 4: wait for a terminal state, bounded. On expiry the parent stops waiting; what
        // happens to the child is decided below.
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
                // The wait and the child end together. This caller has nowhere for a later outcome
                // to arrive — that is why it is waiting on this connection at all — so a child left
                // running past it would be a process nothing would ever collect from or reap.
                // `child` is dropped on the way out, which kills and reaps it.
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
                    let found = read_child_result(&child.workdir, &child.session_id, &task_id);
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

/// Deliver the task as the child's first user message, backing off while its listener comes up.
///
/// A child reports its URL when it binds, but the first connection can still land between the bind
/// and the first accept, so this backs off rather than failing on one refusal. `Ok` carries the id
/// the child gave the task it accepted, which is what makes the child's per-task result file
/// findable; `Err` is the sentence the delegating caller reports, already naming the capsule.
fn deliver_task(
    capsule_url: &str,
    delegation_id: &str,
    request: &DelegationRequest,
) -> Result<String, String> {
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
        match http_json("POST", capsule_url, Some(&send_body), &[]) {
            Ok(response) => break response,
            Err(_) if Instant::now() < send_deadline => {
                std::thread::sleep(delay);
                delay = (delay * 2).min(Duration::from_secs(2));
            }
            Err(error) => {
                return Err(format!(
                    "capsule '{}' did not accept a task within {}s: {error}",
                    request.capsule,
                    SEND_DEADLINE.as_secs()
                ))
            }
        }
    };
    sent.pointer("/result/id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            format!(
                "capsule '{}' answered the delivered task with no task id",
                request.capsule
            )
        })
}

/// Where a finished child left its answer, and what it says.
///
/// The candidate list is [`crate::delegation::child_result_candidates`], so the file this plane
/// reads and the file the released-child watcher names are found by one rule.
fn read_child_result(
    child_workdir: &Path,
    session_id: &str,
    task_id: &str,
) -> Option<(PathBuf, String)> {
    crate::delegation::child_result_candidates(child_workdir, session_id, task_id)
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

/// The host variables a child is handed: the names it declares that its parent declares too.
///
/// Order follows the child's declaration and no name appears twice, so what the launcher copies
/// reads as the child's own list. The clamp is the parent's second, independent statement of the
/// `capabilities.env.allow` envelope axis `mur-roost` already referees — a child can never hold a
/// variable the capsule that spawned it does not itself declare, whatever a daemon answered.
fn env_allow_intersection(child: &[String], parent: &[String]) -> Vec<String> {
    let mut allowed: Vec<String> = Vec::new();
    for name in child {
        if parent.contains(name) && !allowed.contains(name) {
            allowed.push(name.clone());
        }
    }
    allowed
}

/// The bound this session delegates under: [`DELEGATION_TIMEOUT_ENV`] when it names a positive
/// number of seconds, and `declared` — the capsule's own `lifecycle.delegation_deadline_secs` —
/// otherwise.
fn configured_result_timeout(declared: Duration) -> Duration {
    result_timeout_from(
        std::env::var(DELEGATION_TIMEOUT_ENV).ok().as_deref(),
        declared,
    )
}

/// [`configured_result_timeout`]'s rule, without the environment read, so it is testable without
/// mutating process-wide state that every other test in this binary shares.
///
/// `declared` is taken as written, including zero: a capsule that declares `0` means the first
/// poll gives up. The override is only honoured for a positive integer, because an operator who
/// exported nonsense meant to change nothing rather than to remove the bound.
fn result_timeout_from(value: Option<&str>, declared: Duration) -> Duration {
    value
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .map(Duration::from_secs)
        .unwrap_or(declared)
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

    /// The five words the trace and the tool result share.
    #[test]
    fn every_status_has_one_wire_word() {
        assert_eq!(DelegationStatus::Started.as_str(), "started");
        assert_eq!(DelegationStatus::Completed.as_str(), "completed");
        assert_eq!(DelegationStatus::Failed.as_str(), "failed");
        assert_eq!(DelegationStatus::TimedOut.as_str(), "timed_out");
        assert_eq!(DelegationStatus::Refused.as_str(), "refused");
    }

    fn plane() -> DelegationPlane {
        DelegationPlane::new(
            // Nothing here is reachable: every case below refuses before a request is built.
            "http://127.0.0.1:1".to_string(),
            SpawnCredential::new("msc1.test".to_string()),
            PathBuf::from("/tmp"),
            "ses_parent".to_string(),
            DELEGATION_RESULT_TIMEOUT,
            std::sync::Arc::new(murmur_artifact::LocalRegistry::new("/tmp")),
            Vec::new(),
        )
    }

    fn request() -> DelegationRequest {
        DelegationRequest {
            capsule: "worker".to_string(),
            version: "0.1.0".to_string(),
            task: "t".to_string(),
        }
    }

    /// A plane that was never told this capsule's own address cannot start a delegation: the
    /// child would run and post its outcome nowhere. Refused before the daemon is asked, so no
    /// child directory and no `POST /spawn` exist to clean up.
    #[test]
    fn a_start_with_nowhere_to_report_is_refused_before_the_daemon_is_asked() {
        let result = plane().start(
            &request(),
            &DelegationOrigin {
                context_id: "ctx_parent".to_string(),
                ..DelegationOrigin::default()
            },
        );

        assert_eq!(result.status, DelegationStatus::Failed);
        assert!(result.delegation_id.is_empty(), "{result:?}");
        assert!(result.child_workdir.is_none(), "{result:?}");
        assert!(
            result.output.contains("nowhere to be reported"),
            "the sentence says why: {}",
            result.output
        );
    }

    /// The conversation is half the address a completion is delivered against, so a delegation
    /// made from no conversation is refused on the same terms as one made from no address.
    #[test]
    fn a_start_with_no_conversation_is_refused_too() {
        let result = plane()
            .reporting_to("http://127.0.0.1:7000".to_string())
            .start(&request(), &DelegationOrigin::default());

        assert_eq!(result.status, DelegationStatus::Failed);
        assert!(
            result.output.contains("nowhere to be reported"),
            "the sentence says why: {}",
            result.output
        );
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
        let declared = Duration::from_secs(30);
        assert_eq!(
            result_timeout_from(Some(" 20 "), declared),
            Duration::from_secs(20)
        );
        for ignored in [None, Some(""), Some("0"), Some("-5"), Some("later")] {
            assert_eq!(
                result_timeout_from(ignored, declared),
                declared,
                "{ignored:?} is not a positive number of seconds, so the declared bound stands"
            );
        }
    }

    /// The declared bound is the whole of the fallback, and a capsule that declares `0` means it.
    #[test]
    fn a_declared_deadline_of_zero_is_a_bound_and_not_an_absence() {
        assert_eq!(
            result_timeout_from(None, Duration::ZERO),
            Duration::ZERO,
            "0 gives up at the first poll rather than falling back to the default"
        );
        assert_eq!(
            result_timeout_from(None, DELEGATION_RESULT_TIMEOUT),
            DELEGATION_RESULT_TIMEOUT
        );
    }

    /// The ceiling holds on every status, not only on an answer: a child's failure message and a
    /// daemon's refusal reach the same model context an answer does.
    #[test]
    fn the_output_ceiling_bounds_a_refusal_too() {
        let request = request();
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

    /// A child is handed the names both manifests declare, in the order the child declared them.
    #[test]
    fn a_child_is_handed_only_what_both_manifests_declare() {
        let names =
            |list: &[&str]| -> Vec<String> { list.iter().map(|name| name.to_string()).collect() };

        assert_eq!(
            env_allow_intersection(&names(&["A", "B", "C"]), &names(&["B", "C", "D"])),
            names(&["B", "C"])
        );
        assert_eq!(
            env_allow_intersection(&names(&["A"]), &names(&[])),
            Vec::<String>::new(),
            "a parent that declares nothing hands nothing on"
        );
        assert_eq!(
            env_allow_intersection(&names(&[]), &names(&["A"])),
            Vec::<String>::new(),
            "a child that declares nothing asks for nothing"
        );
    }

    /// The order is the child's, and a name it declared twice is copied once.
    #[test]
    fn the_intersection_keeps_the_childs_order_and_names_nothing_twice() {
        let names =
            |list: &[&str]| -> Vec<String> { list.iter().map(|name| name.to_string()).collect() };

        assert_eq!(
            env_allow_intersection(&names(&["C", "A", "C"]), &names(&["A", "B", "C"])),
            names(&["C", "A"])
        );
    }

    /// A capsule the parent's own store cannot resolve fails the read by name.
    ///
    /// The delegation ends there rather than launching a child without the variables its manifest
    /// declares: that child would die at its own manifest load, in a process nobody is reading.
    #[test]
    fn a_capsule_the_store_cannot_resolve_fails_the_read_by_name() {
        let store = tempfile::tempdir().unwrap();
        let plane = DelegationPlane::new(
            "http://127.0.0.1:7700".to_string(),
            SpawnCredential::new("msc1.test".to_string()),
            PathBuf::from("/tmp"),
            "ses_parent".to_string(),
            DELEGATION_RESULT_TIMEOUT,
            std::sync::Arc::new(murmur_artifact::LocalRegistry::new(store.path())),
            vec!["MURMUR_TEST_PROVIDER_KEY".to_string()],
        );

        let error = plane
            .child_env_allow("worker", "0.1.0")
            .expect_err("an empty store resolves nothing");
        let message = error.to_string();
        assert!(message.contains("worker"), "{message}");
        assert!(message.contains("0.1.0"), "{message}");
        assert!(message.contains("capabilities.env.allow"), "{message}");
    }

    /// The trailing slash is taken off once, so no request is built against `//spawn`.
    #[test]
    fn a_trailing_slash_on_the_daemon_url_is_trimmed_once() {
        let plane = DelegationPlane::new(
            "http://127.0.0.1:7700/".to_string(),
            SpawnCredential::new("msc1.test".to_string()),
            PathBuf::from("/tmp"),
            "ses_parent".to_string(),
            DELEGATION_RESULT_TIMEOUT,
            std::sync::Arc::new(murmur_artifact::LocalRegistry::new("/tmp")),
            Vec::new(),
        );
        assert_eq!(plane.roost_url, "http://127.0.0.1:7700");
    }
}
