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
//! | Method | Returns when | Statuses it can produce | Who holds the child | Its caller |
//! |---|---|---|---|---|
//! | [`DelegationPlane::start`] | the child is running and holding its task | `started`, `failed`, `refused` | the caller, through [`StartedDelegation::child`] | the agent-facing `delegate-task` tool |
//! | [`DelegationPlane::delegate`] | the child's task reaches a terminal state, or [`DelegationOrigin::stop`] reports the delegating task cancelled | `completed`, `timed_out`, `failed`, `refused`, `canceled` | the method, until it returns | a plan's `capsule` step |
//!
//! [`DelegationPlane::start`] is the one an agent reaches. It hands the child's handle back, the
//! delegating task holds it until the outcome is delivered or the task ends the child, and the
//! outcome arrives at the parent's door as a completion the door hands to that task — which is
//! what [`crate::delegation`] exists for, and why a plane that was never told its own address
//! through [`DelegationPlane::reporting_to`] refuses to start anything.
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

use crate::child_launch::{
    launch_child_capsule, workdir_relative_to, ChildLaunchRequest, LaunchedChild,
};
use crate::delegation::{CompletionAddress, Spawner};
use crate::errors::RuntimeError;
use crate::formation::FormationId;
use crate::http_client::http_json;
use crate::spawn_credential::{SpawnApproval, SpawnCredential, SPAWN_CREDENTIAL_HEADER};

/// The deadline a session delegates under when nothing declares one.
///
/// **The single delegation bound**, and the only knob on either of the two waits a delegation
/// still has: [`DelegationPlane::delegate`]'s poll for a terminal task state, and the completion
/// watcher's observation of a child [`DelegationPlane::start`] launched. Everything else in a
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
pub(crate) const MAX_OUTPUT_BYTES: usize = 64 * 1024;

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
/// The value vocabulary of the `delegate-task` tool result's `status`. The `delegation` trace
/// event's `outcome` shares only `failed` and `refused` with it: a delegation that started is
/// closed from the child's own completion instead, in
/// [`crate::delegation::DelegationStatus`]'s words. Which of the two plane methods can produce
/// each variant is stated on it, because a caller that reads a word this method cannot produce is
/// reading the wrong contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelegationStatus {
    /// The child is running and holding its task, and nothing is known yet about what it will do
    /// with it. Produced only by [`DelegationPlane::start`]; the outcome is delivered afterwards
    /// into the task that made the delegation.
    Started,
    /// The child's task reached `completed` and its answer is in hand. Produced only by
    /// [`DelegationPlane::delegate`].
    Completed,
    /// A child ran and did not answer: its task ended `failed` or `rejected`, or it could not be
    /// reached, launched or handed its task after the daemon had approved it. Produced by both
    /// methods.
    Failed,
    /// No terminal state within [`DelegationPlane::result_timeout`]. Produced only by
    /// [`DelegationPlane::delegate`]: a child [`DelegationPlane::start`] launched is bounded by
    /// its watcher instead, which ends it and posts a `terminated` completion.
    TimedOut,
    /// The daemon refused, so no child was launched and no child directory exists. Produced by
    /// both methods.
    Refused,
    /// The task the delegation was made for was cancelled while the child was running, so the
    /// child was ended and its `completion.json` records `terminated`. Produced only by
    /// [`DelegationPlane::delegate`], when [`DelegationOrigin::stop`] reports true.
    ///
    /// Never written as a `delegation` line's `outcome`: the cancelled task closes that row
    /// itself, in the sub-capsule vocabulary, as `terminated`.
    Canceled,
}

impl DelegationStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::TimedOut => "timed_out",
            Self::Refused => "refused",
            Self::Canceled => "canceled",
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
        let (output, truncated) = bounded(reason);
        Self {
            delegation_id: String::new(),
            session_id: String::new(),
            capsule: request.capsule.clone(),
            version: request.version.clone(),
            status,
            output,
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

/// What [`DelegationPlane::start`] produced: the result the agent reads, and on
/// [`DelegationStatus::Started`] the handle that ends the child.
///
/// Dropping [`Self::child`] kills and reaps a child still running, so a caller that does not keep
/// it — a `delegate-task` call cancelled while the launch was finishing — leaves nothing behind.
#[derive(Debug)]
pub struct StartedDelegation {
    pub result: DelegationResult,
    /// `Some` exactly when [`DelegationResult::status`] is [`DelegationStatus::Started`].
    pub child: Option<LaunchedChild>,
}

impl StartedDelegation {
    /// A start that left no child running.
    fn without_child(result: DelegationResult) -> Self {
        Self {
            result,
            child: None,
        }
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
#[derive(Clone, Default)]
pub struct DelegationOrigin<'a> {
    /// The conversation the delegation was made from. Empty when the caller has none, which
    /// injects no handle at all rather than one naming a conversation that does not exist.
    pub context_id: String,
    /// What to do with the launch notice, or `None` for a caller that records no
    /// `delegation_start`. Called on the delegating thread, before the delegation has ended.
    ///
    /// A callback rather than a channel because the two callers are not the same shape:
    /// [`DelegationPlane::start`] runs on a blocking tokio worker with a task free to drain a
    /// receiver, while `plan::execute` calls [`DelegationPlane::delegate`] directly on a
    /// scheduler thread with no runtime to await on and nothing else running to drain anything.
    /// Invoked in place, both surfaces get the record on disk while the child is still in flight,
    /// which is the whole reason it is a record of its own.
    pub launched: Option<std::sync::Arc<dyn Fn(DelegationLaunch) + Send + Sync + 'a>>,
    /// Whether the task this delegation was made for has been cancelled, or `None` for a caller
    /// that cannot be. Polled, so it must be cheap and must never block.
    ///
    /// Read only by [`DelegationPlane::delegate`], which ends the child itself when it reports
    /// true. [`DelegationPlane::start`] ignores it: its caller races its own cancel against the
    /// launch, and holds the child's handle from then on.
    pub stop: Option<std::sync::Arc<dyn Fn() -> bool + Send + Sync + 'a>>,
}

/// Hand-written because [`DelegationOrigin::launched`] and [`DelegationOrigin::stop`] are trait
/// objects and cannot derive one.
/// A callback has no readable identity, so what is printed of it is whether there is one.
impl std::fmt::Debug for DelegationOrigin<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DelegationOrigin")
            .field("context_id", &self.context_id)
            .field("launched", &self.launched.is_some())
            .field("stop", &self.stop.is_some())
            .finish()
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
    /// The delegating session's formation, handed to every child as `MURMUR_FORMATION_ID` so the
    /// child joins it. `None` for a session nobody placed in a formation, whose children carry
    /// none either.
    formation_id: Option<FormationId>,
}

impl DelegationPlane {
    /// The plane for a registered session.
    ///
    /// `roost_url`, `credential` and `session_id` come from the session's own registration, never
    /// from anything the agent said. `declared_deadline` is the capsule's own
    /// `lifecycle.delegation_deadline_secs`; [`DELEGATION_TIMEOUT_ENV`] overrides it where it
    /// names a positive number of seconds. `registry` is the store a child's manifest is read
    /// from and `parent_env_allow` the delegating capsule's own `capabilities.env.allow`, which
    /// together decide the host variables a child is handed. `formation_id` is the session's own
    /// [`crate::StagedSession::formation_id`], inherited by every child whether or not it is also
    /// handed a spawner.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        roost_url: String,
        credential: SpawnCredential,
        accessible_workdir: PathBuf,
        session_id: String,
        declared_deadline: Duration,
        registry: std::sync::Arc<dyn murmur_artifact::Registry>,
        parent_env_allow: Vec<String>,
        formation_id: Option<FormationId>,
    ) -> Self {
        Self {
            roost_url: roost_url.trim_end_matches('/').to_string(),
            credential,
            accessible_workdir,
            result_timeout: handed_off_work_deadline(declared_deadline),
            session_id,
            registry,
            parent_env_allow,
            own_url: String::new(),
            formation_id,
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

    /// The formation every child this plane launches joins, or `None` for a session in none.
    pub fn formation_id(&self) -> Option<&FormationId> {
        self.formation_id.as_ref()
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

    /// This session's credential, cloned for a caller that delegates through a plane of its own.
    ///
    /// The one such caller is `plan::execute`, which builds its own [`DelegationPlane`] per
    /// `capsule` step. Handing it this token rather than storing a second copy beside the plane
    /// keeps one registration's authority in one place; the clone is still a
    /// [`SpawnCredential`], so it cannot be formatted, serialized or traced into anything.
    pub(crate) fn credential(&self) -> SpawnCredential {
        self.credential.clone()
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

    /// Compose the one launch this plane makes, from the approval the daemon just gave and the
    /// address — if any — the child's outcome is to be posted to.
    ///
    /// **The only place a [`ChildLaunchRequest`] is built.** Both methods reach a child through
    /// here, so a field that is wrong is wrong on both paths at once rather than on whichever one
    /// a later edit forgot; a source-sweep test in this file's `mod tests` holds that to one
    /// construction site. Everything but `report_to` is the plane's own state and the caller's
    /// three strings, which is why that is the single parameter: it is the whole of the
    /// difference between a delegation whose outcome is posted to this capsule's door and one
    /// whose answer arrives on the connection the caller is already holding.
    ///
    /// `completion_deadline` is derived from `report_to` rather than passed: the watcher this
    /// plane's bound applies to only runs for a spawner naming an address, so two parameters
    /// would be two ways of stating one decision and a way for them to disagree.
    ///
    /// `Err` when the child's own `capabilities.env.allow` cannot be read, which starts no
    /// process on either path: a child launched without the variables its manifest names dies at
    /// its own manifest load, inside a process nobody is reading. Called after the daemon has
    /// approved the spawn, so a refusal is still reported as a refusal rather than as a local
    /// read failure.
    fn launch_request(
        &self,
        request: &DelegationRequest,
        origin: &DelegationOrigin<'_>,
        grant: SpawnApproval,
        report_to: Option<CompletionAddress>,
    ) -> Result<ChildLaunchRequest, RuntimeError> {
        let completion_deadline = report_to.as_ref().map(|_| self.result_timeout);
        Ok(ChildLaunchRequest {
            parent_accessible_workdir: self.accessible_workdir.clone(),
            capsule_name: request.capsule.clone(),
            capsule_version: request.version.clone(),
            grant,
            // What both manifests declare and nothing else, read from the parent's own store
            // rather than taken from the daemon's answer: the referee answers whether a spawn may
            // happen, and this clamp has to hold whatever it answered.
            child_env_allow: self.child_env_allow(&request.capsule, &request.version)?,
            roost_url: self.roost_url.clone(),
            // A caller that knows neither its session nor its conversation injects no handle at
            // all, rather than one naming a lineage that does not exist. `start` holds both by
            // the time it reaches here — it refuses outright when either is empty — so this is
            // unconditional there in everything but form.
            spawner: (!self.session_id.is_empty() && !origin.context_id.is_empty()).then(|| {
                Spawner {
                    session_id: self.session_id.clone(),
                    context_id: origin.context_id.clone(),
                    report_to,
                }
            }),
            completion_deadline,
            // Independent of `spawner`: a launch that names no lineage still joins the formation.
            formation_id: self.formation_id.clone(),
        })
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
        origin: &DelegationOrigin<'_>,
        child: &crate::child_launch::LaunchedChild,
        delegation_id: &str,
    ) {
        if let Some(launched) = origin
            .launched
            .as_ref()
            .filter(|_| !delegation_id.is_empty())
        {
            launched(DelegationLaunch {
                delegation_id: delegation_id.to_string(),
                capsule: request.capsule.clone(),
                version: request.version.clone(),
                child_session_id: child.session_id.clone(),
                child_workdir: self.workdir_relative(&child.workdir),
            });
        }
    }

    /// Ask the daemon, launch the approved child, hand it its task, and return the child's handle.
    ///
    /// **The agent-facing `delegate-task` tool's method.** It returns as soon as the child is
    /// running *and holding its task*, so a turn can issue several delegations; what the child
    /// eventually did is posted to the parent's door afterwards, carrying this delegation's id, by
    /// the child itself or, for a child that could not speak for itself, by the watcher
    /// [`launch_child_capsule`] started behind it.
    ///
    /// Blocking and never `Err`, like [`DelegationPlane::delegate`], but only three
    /// [`DelegationStatus`] words can come out of it: `started`, `failed` and `refused`.
    ///
    /// "Started" means the child holds its task, not merely that a process exists. Everything
    /// before that point kills and reaps the child on the way out, because a child that will never
    /// be given work will never report and would strand a process nothing waits on. A started
    /// child's handle is returned in [`StartedDelegation::child`]: the child lives as long as that
    /// handle, or its own run, whichever ends first.
    pub fn start(
        &self,
        request: &DelegationRequest,
        origin: &DelegationOrigin<'_>,
    ) -> StartedDelegation {
        // Refused before the daemon is touched, because there is nothing to ask about: a
        // delegation started here would run to completion and post its outcome nowhere, which is
        // a way to lose work rather than a way to do it. No `POST /spawn` is made and no process
        // is launched.
        if self.own_url.is_empty() || self.session_id.is_empty() || origin.context_id.is_empty() {
            return StartedDelegation::without_child(DelegationResult::unmade(
                request,
                DelegationStatus::Failed,
                "this capsule cannot start a delegation: a sub-capsule's outcome is posted to \
                 this runtime's own endpoint, addressed to its session and conversation, and \
                 this session does not hold all three — so the outcome would have nowhere to be \
                 reported"
                    .to_string(),
            ));
        }

        let grant = match self.approval_for(request) {
            Ok(grant) => grant,
            Err(reason) => {
                return StartedDelegation::without_child(DelegationResult::refused(request, reason))
            }
        };

        // The production caller of the completion path: naming this capsule's own address is what
        // starts the watcher behind the child, and with it this plane's bound on the watch.
        let launch = match self.launch_request(
            request,
            origin,
            grant,
            Some(CompletionAddress {
                url: self.own_url.clone(),
            }),
        ) {
            Ok(launch) => launch,
            Err(error) => {
                return StartedDelegation::without_child(DelegationResult::unmade(
                    request,
                    DelegationStatus::Failed,
                    error.to_string(),
                ))
            }
        };

        let child = match launch_child_capsule(launch) {
            Ok(child) => child,
            Err(error) => {
                return StartedDelegation::without_child(DelegationResult::unmade(
                    request,
                    DelegationStatus::Failed,
                    error.to_string(),
                ))
            }
        };
        let delegation_id = child.delegation_id.clone().unwrap_or_default();
        self.announce(request, origin, &child, &delegation_id);

        let child_workdir = self.workdir_relative(&child.workdir);
        // `child` is dropped on every early return below, which kills and reaps it.
        let failed = |session_id: &str, output: String| {
            let (output, truncated) = bounded(output);
            StartedDelegation::without_child(DelegationResult {
                delegation_id: delegation_id.clone(),
                session_id: session_id.to_string(),
                capsule: request.capsule.clone(),
                version: request.version.clone(),
                status: DelegationStatus::Failed,
                output,
                result_path: None,
                truncated,
                child_workdir: None,
            })
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
        let door_token = child.door_token.clone();
        if let Err(reason) = deliver_task(
            &capsule_url,
            door_token.as_ref(),
            &delegation_id,
            request,
            &|| false,
        ) {
            return failed(&child.session_id, reason);
        }

        let session_id = child.session_id.clone();
        StartedDelegation {
            result: DelegationResult {
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
            },
            child: Some(child),
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
    /// five [`DelegationStatus`] words — `completed`, `timed_out`, `failed`, `refused`, `canceled`
    /// — because the step maps them all onto a step result, and a refusal is as much a fact about
    /// the run as an answer is.
    ///
    /// **Cancellation.** [`DelegationOrigin::stop`] is asked once the child is announced, between
    /// delivery attempts, and on every poll. When it reports true this method ends the child,
    /// records it `terminated` by the launcher in the child's `completion.json` with
    /// [`crate::delegation::PARENT_ENDED_DETAIL`], and returns [`DelegationStatus::Canceled`]. The
    /// daemon's approval and the child's launch are not interruptible, so a cancel that lands
    /// during either is honoured at the first check after them — the same bound a `delegate-task`
    /// call's launch has.
    pub fn delegate(
        &self,
        request: &DelegationRequest,
        origin: &DelegationOrigin<'_>,
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
        // The spawner this composes is lineage and nothing else. The answer arrives on the
        // connection this plane opened, so there is nothing for a completion to tell: naming no
        // address means no watcher and no post — and no completion deadline, since the poll in
        // step 4 is this method's bound — while the child still learns which session spawned it.
        //
        // A launch request that cannot be composed starts no child: a read of the child's own
        // declaration that fails would otherwise launch a capsule without the variables its
        // manifest names, which dies at its own manifest load inside a process nobody is reading.
        let launch = match self.launch_request(request, origin, grant, None) {
            Ok(launch) => launch,
            Err(error) => {
                return DelegationResult::unmade(
                    request,
                    DelegationStatus::Failed,
                    error.to_string(),
                )
            }
        };
        let launched_at = Instant::now();
        let mut child = match launch_child_capsule(launch) {
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
        let stopped = || origin.stop.as_ref().is_some_and(|stop| stop());

        // Every result this call can produce past the launch, built from one place so the
        // delegation id, the capsule and the version cannot disagree between two of them — and so
        // [`MAX_OUTPUT_BYTES`] bounds every branch. A child's *failure* message is as much its own
        // text as its answer is, so it is cut on the same terms.
        let outcome = |status, session_id: &str, output: String| {
            let (output, truncated) = bounded(output);
            DelegationResult {
                delegation_id: delegation_id.clone(),
                session_id: session_id.to_string(),
                capsule: request.capsule.clone(),
                version: request.version.clone(),
                status,
                output,
                result_path: None,
                truncated,
                child_workdir: None,
            }
        };
        // The task this delegation was made for was cancelled: end the child here, where its
        // handle is, and leave the delegation's row to the task, which closes it after its
        // `task_canceled`.
        let canceled = |child: &mut LaunchedChild| {
            end_for_canceled_task(request, &delegation_id, child, launched_at);
            outcome(
                DelegationStatus::Canceled,
                &child.session_id,
                format!(
                    "capsule '{}' was stopped because the task that started it was cancelled",
                    request.capsule
                ),
            )
        };
        if stopped() {
            return canceled(&mut child);
        }

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
        // Present on every call to a child that declares `network.authentication`.
        let authorization = child
            .door_token
            .as_ref()
            .map(crate::door_auth::bearer_header);
        let headers: Vec<(&str, &str)> = authorization
            .as_deref()
            .map(|value| ("Authorization", value))
            .into_iter()
            .collect();

        // Step 3: deliver the task as the child's first user message.
        let task_id = match deliver_task(
            &capsule_url,
            child.door_token.as_ref(),
            &delegation_id,
            request,
            &stopped,
        ) {
            Ok(task_id) => task_id,
            Err(_) if stopped() => return canceled(&mut child),
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
            if stopped() {
                return canceled(&mut child);
            }
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
            if stopped() {
                return canceled(&mut child);
            }

            let task = match http_json("POST", &capsule_url, Some(&poll_body), &headers) {
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
                // output in its artifacts: a murmur door attaches a `response` artifact to a
                // completed task that produced a response, and a capsule this runtime did not
                // build puts its answer there too. A task that ended with no response text has no
                // artifact, so the result file the child's runtime wrote into the directory this
                // parent composed for it is read as well.
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
///
/// `door_token` is the child's operator token when it declares `network.authentication`. `stop`
/// is asked before every retry, and a send it stops returns `Err` at once rather than backing off
/// to [`SEND_DEADLINE`].
fn deliver_task(
    capsule_url: &str,
    door_token: Option<&crate::door_auth::DoorToken>,
    delegation_id: &str,
    request: &DelegationRequest,
    stop: &dyn Fn() -> bool,
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
    let authorization = door_token.map(crate::door_auth::bearer_header);
    let headers: Vec<(&str, &str)> = authorization
        .as_deref()
        .map(|value| ("Authorization", value))
        .into_iter()
        .collect();
    let send_deadline = Instant::now() + SEND_DEADLINE;
    let mut delay = Duration::from_millis(100);
    let sent = loop {
        match http_json("POST", capsule_url, Some(&send_body), &headers) {
            Ok(response) => break response,
            Err(error) if stop() => {
                return Err(format!(
                    "capsule '{}' was not handed its task: the delegating task was cancelled \
                     ({error})",
                    request.capsule
                ))
            }
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

/// End a child [`DelegationPlane::delegate`] holds because the task it was made for was
/// cancelled, and record that in the child's own `completion.json`.
///
/// The record is the one a `delegate-task` child's completion watcher writes when its parent ends
/// it on purpose: this launch started no watcher, so the launcher writes it here, after the reap.
fn end_for_canceled_task(
    request: &DelegationRequest,
    delegation_id: &str,
    child: &mut LaunchedChild,
    launched_at: Instant,
) {
    if let Err(error) = child.shutdown() {
        crate::runtime_err!("[capsule-runtime] delegation {delegation_id}: {error}");
    }
    crate::delegation::record_terminated(
        &child.workdir,
        crate::delegation::DelegationOutcome {
            delegation_id: delegation_id.to_string(),
            capsule_name: request.capsule.clone(),
            capsule_version: request.version.clone(),
            session_id: child.session_id.clone(),
            status: crate::delegation::DelegationStatus::Terminated,
            result_path: None,
            workdir: child.workdir.display().to_string(),
            duration_ms: crate::member_call::elapsed_ms(launched_at),
            detail: Some(crate::delegation::PARENT_ENDED_DETAIL.to_string()),
            reported_by: crate::delegation::Reporter::Launcher,
            delivered: false,
            delivery_error: None,
        },
    );
}

/// Where a finished child left its answer, and what it says.
///
/// The candidate list is [`crate::delegation::child_result_candidates`], so the file this plane
/// reads and the file the completion watcher names are found by one rule.
fn read_child_result(
    child_workdir: &Path,
    session_id: &str,
    task_id: &str,
) -> Option<(PathBuf, String)> {
    crate::delegation::child_result_candidates(child_workdir, session_id, task_id)
        .into_iter()
        .find_map(|path| std::fs::read_to_string(&path).ok().map(|text| (path, text)))
}

/// `(output, truncated)`, with `output` cut by [`bound_output`] when it is over
/// [`MAX_OUTPUT_BYTES`].
pub(crate) fn bounded(output: String) -> (String, bool) {
    if output.len() > MAX_OUTPUT_BYTES {
        (bound_output(output), true)
    } else {
        (output, false)
    }
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

/// The bound on work this session hands off: [`DELEGATION_TIMEOUT_ENV`] when it names a positive
/// number of seconds, and `declared` — the capsule's own `lifecycle.delegation_deadline_secs` —
/// otherwise.
///
/// The one resolver for the one bound: a delegation's wait for its child, and a `call-member`
/// call's wait for the callee's answer ([`crate::member_call`]), both read it.
pub(crate) fn handed_off_work_deadline(declared: Duration) -> Duration {
    result_timeout_from(
        std::env::var(DELEGATION_TIMEOUT_ENV).ok().as_deref(),
        declared,
    )
}

/// [`handed_off_work_deadline`]'s rule, without the environment read, so it is testable without
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

    /// The five words a tool result's `status` can carry.
    #[test]
    fn every_status_has_one_wire_word() {
        assert_eq!(DelegationStatus::Started.as_str(), "started");
        assert_eq!(DelegationStatus::Completed.as_str(), "completed");
        assert_eq!(DelegationStatus::Failed.as_str(), "failed");
        assert_eq!(DelegationStatus::TimedOut.as_str(), "timed_out");
        assert_eq!(DelegationStatus::Refused.as_str(), "refused");
    }

    /// The word `delegate` returns for a child it ended because its task was cancelled. A plan
    /// step reads it to leave the delegation's row to the task, so it is never a `delegation`
    /// line's `outcome`.
    #[test]
    fn a_canceled_delegation_has_its_own_word() {
        assert_eq!(DelegationStatus::Canceled.as_str(), "canceled");
    }

    /// A stop predicate is a trait object, so `Debug` says whether there is one.
    #[test]
    fn an_origin_prints_whether_it_can_be_stopped() {
        let stoppable = DelegationOrigin {
            stop: Some(std::sync::Arc::new(|| false)),
            ..origin()
        };
        assert!(
            format!("{stoppable:?}").contains("stop: true"),
            "{stoppable:?}"
        );
        assert!(format!("{:?}", origin()).contains("stop: false"));
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
            None,
        )
    }

    fn request() -> DelegationRequest {
        DelegationRequest {
            capsule: "worker".to_string(),
            version: "0.1.0".to_string(),
            task: "t".to_string(),
        }
    }

    /// A delegated task carries no origin claim, so the child records it as `event` and a child
    /// that does not consent to peer tasks still takes it — while the same door refuses a
    /// peer-origin message. Delegation needs no `exports.peer_tasks`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_delegated_task_reaches_a_child_that_refuses_peer_tasks() {
        let (addr, _shutdown, mut task_rx) = crate::identity::serve_test_door(false).await;
        let url = format!("http://{addr}");
        let delivered = tokio::task::spawn_blocking(move || {
            deliver_task(&url, None, "dlg_0000000000000001", &request(), &|| false)
        })
        .await
        .unwrap()
        .expect("the child takes the delegated task");
        assert!(delivered.starts_with("tsk_"), "{delivered}");
        let incoming = task_rx
            .recv()
            .await
            .expect("the task reached the child's loop");
        assert_eq!(
            incoming.provenance.origin(),
            crate::origin::TaskOrigin::Event
        );

        let refused = crate::outgoing::send_a2a_message(
            &addr,
            crate::outgoing::OutgoingMessage {
                message_id: "msg_peer".to_string(),
                context_id: None,
                text: "from a peer".to_string(),
            },
            None,
            None,
            None,
        )
        .await
        .expect_err("the same door refuses a peer message");
        assert!(refused.contains("403 peer_not_accepted"), "{refused}");
    }

    /// A plane that was never told this capsule's own address cannot start a delegation: the
    /// child would run and post its outcome nowhere. Refused before the daemon is asked, so no
    /// child directory and no `POST /spawn` exist to clean up.
    #[test]
    fn a_start_with_nowhere_to_report_is_refused_before_the_daemon_is_asked() {
        let started = plane().start(
            &request(),
            &DelegationOrigin {
                context_id: "ctx_parent".to_string(),
                ..DelegationOrigin::default()
            },
        );
        assert!(started.child.is_none());
        let result = started.result;

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
        let started = plane()
            .reporting_to("http://127.0.0.1:7000".to_string())
            .start(&request(), &DelegationOrigin::default());
        assert!(started.child.is_none());
        let result = started.result;

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
        let plane = plane_over(&store, &["MURMUR_TEST_PROVIDER_KEY"]);

        let error = plane
            .child_env_allow("worker", "0.1.0")
            .expect_err("an empty store resolves nothing");
        let message = error.to_string();
        assert!(message.contains("worker"), "{message}");
        assert!(message.contains("0.1.0"), "{message}");
        assert!(message.contains("capabilities.env.allow"), "{message}");
    }

    /// A `worker@0.1.0` in a store of its own, declaring `capabilities.env.allow: [A, B, C]`.
    ///
    /// Packed the way `mur publish` packs one — a `murmur.yaml` entry in a `.mur.zip` — because
    /// [`DelegationPlane::child_env_allow`] reads the child's declaration out of the archive
    /// rather than out of a directory.
    fn store_with_worker() -> tempfile::TempDir {
        let store = tempfile::tempdir().unwrap();
        let mut zipped = std::io::Cursor::new(Vec::<u8>::new());
        {
            let mut zip = zip::ZipWriter::new(&mut zipped);
            zip.start_file(
                "murmur.yaml",
                zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Deflated),
            )
            .unwrap();
            std::io::Write::write_all(
                &mut zip,
                b"name: worker\nversion: 0.1.0\ncapabilities:\n  env:\n    allow: [A, B, C]\n",
            )
            .unwrap();
            zip.finish().unwrap();
        }
        murmur_artifact::Registry::publish(
            &murmur_artifact::LocalRegistry::new(store.path()),
            murmur_artifact::ArtifactMeta {
                name: "worker".to_string(),
                version: "0.1.0".to_string(),
                runtime: murmur_artifact::RuntimeType::Wasm,
                artifact_runtime: "capsule".to_string(),
                platforms: Vec::new(),
                description: None,
                tags: Vec::new(),
                wit_contracts: None,
            },
            &zipped.into_inner(),
        )
        .unwrap();
        store
    }

    /// A plane over the given store, holding the given `capabilities.env.allow` of its own.
    fn plane_over(store: &tempfile::TempDir, parent_env_allow: &[&str]) -> DelegationPlane {
        plane_in_formation(store, parent_env_allow, None)
    }

    /// [`plane_over`] for a session in the given formation.
    fn plane_in_formation(
        store: &tempfile::TempDir,
        parent_env_allow: &[&str],
        formation_id: Option<FormationId>,
    ) -> DelegationPlane {
        DelegationPlane::new(
            "http://127.0.0.1:1".to_string(),
            SpawnCredential::new("msc1.test".to_string()),
            PathBuf::from("/tmp"),
            "ses_parent".to_string(),
            DELEGATION_RESULT_TIMEOUT,
            std::sync::Arc::new(murmur_artifact::LocalRegistry::new(store.path())),
            parent_env_allow.iter().map(|n| n.to_string()).collect(),
            formation_id,
        )
        .reporting_to("http://127.0.0.1:7000".to_string())
    }

    /// The conversation both paths delegate from.
    fn origin() -> DelegationOrigin<'static> {
        DelegationOrigin {
            context_id: "ctx_parent".to_string(),
            ..DelegationOrigin::default()
        }
    }

    /// Where `start` tells a child to report, as it composes it.
    fn completion_address() -> CompletionAddress {
        CompletionAddress {
            url: "http://127.0.0.1:7000".to_string(),
        }
    }

    /// Every launch this crate makes is composed in one place, so a field can never be right on
    /// one path and wrong on the other.
    ///
    /// Reads every `.rs` under `src/`, drops line comments and the trailing `mod tests`, and
    /// counts the literal. `child_launch.rs` is excluded: it defines the type and builds one in
    /// its own tests, where the launcher rather than a launch path is the unit under test.
    #[test]
    fn a_launch_request_is_built_in_exactly_one_place() {
        let mut found: Vec<(String, usize)> = Vec::new();

        for (name, text) in crate::source_scan::crate_sources() {
            if name == "child_launch.rs" {
                continue;
            }
            let count = crate::source_scan::production_part(&text)
                .lines()
                .filter(|line| !line.trim_start().starts_with("//"))
                .map(|line| line.matches("ChildLaunchRequest {").count())
                .sum::<usize>();
            if count > 0 {
                found.push((name, count));
            }
        }

        found.sort();
        assert_eq!(
            found,
            vec![("delegation_plane.rs".to_string(), 1)],
            "`ChildLaunchRequest` may be built in exactly one place: \
`DelegationPlane::launch_request`, which every launch path in this crate goes through so that \
what a child is handed cannot differ between them. Found: {found:?}. Route a new launch path \
through that constructor rather than composing a second request literal."
        );
    }

    /// A child whose declaration the parent cannot read starts no process, on the path that names
    /// a completion address and on the path that does not.
    #[test]
    fn an_unreadable_child_declaration_composes_no_launch_on_either_path() {
        let store = tempfile::tempdir().unwrap();
        let plane = plane_over(&store, &["A"]);

        for report_to in [Some(completion_address()), None] {
            let named = if report_to.is_some() {
                "the started path"
            } else {
                "the plan-step path"
            };
            let error = plane
                .launch_request(
                    &request(),
                    &origin(),
                    SpawnApproval::new("approved".to_string()),
                    report_to,
                )
                .err()
                .unwrap_or_else(|| panic!("{named}: an empty store resolves nothing"));

            let message = error.to_string();
            assert!(message.contains("worker"), "{named}: {message}");
            assert!(message.contains("0.1.0"), "{named}: {message}");
            assert!(
                message.contains("capabilities.env.allow"),
                "{named}: {message}"
            );
        }
    }

    /// Every launch carries what both manifests declare, whichever path composed it. The two
    /// helper tests above pin the intersection itself; this one pins that a launch is handed its
    /// result.
    #[test]
    fn every_launch_carries_what_both_manifests_declare() {
        let store = store_with_worker();
        let plane = plane_over(&store, &["B", "C", "D"]);

        for report_to in [Some(completion_address()), None] {
            let named = if report_to.is_some() {
                "the started path"
            } else {
                "the plan-step path"
            };
            let launch = plane
                .launch_request(
                    &request(),
                    &origin(),
                    SpawnApproval::new("approved".to_string()),
                    report_to,
                )
                .unwrap_or_else(|error| panic!("{named}: {error}"));

            assert_eq!(
                launch.child_env_allow,
                vec!["B".to_string(), "C".to_string()],
                "{named} hands on what both manifests declare, in the child's order"
            );
        }
    }

    /// The two paths differ in where the outcome goes and in what bounds the wait for it. Every
    /// other thing a child is launched with is the same, because it comes from the same plane.
    #[test]
    fn the_two_paths_differ_in_the_completion_target_and_its_deadline_alone() {
        let store = store_with_worker();
        let plane = plane_over(&store, &["B", "C", "D"]);
        let compose = |report_to| {
            plane
                .launch_request(
                    &request(),
                    &origin(),
                    SpawnApproval::new("approved".to_string()),
                    report_to,
                )
                .expect("the store holds the child's declaration")
        };

        let started = compose(Some(completion_address()));
        let stepped = compose(None);

        let started_spawner = started
            .spawner
            .as_ref()
            .expect("a named session and context");
        assert_eq!(started_spawner.report_to, Some(completion_address()));
        assert_eq!(started.completion_deadline, Some(plane.result_timeout()));

        let stepped_spawner = stepped
            .spawner
            .as_ref()
            .expect("a named session and context");
        assert_eq!(stepped_spawner.report_to, None);
        assert_eq!(stepped.completion_deadline, None);

        assert_eq!(
            started.parent_accessible_workdir,
            stepped.parent_accessible_workdir
        );
        assert_eq!(started.capsule_name, stepped.capsule_name);
        assert_eq!(started.capsule_version, stepped.capsule_version);
        assert_eq!(started.grant.expose(), stepped.grant.expose());
        assert_eq!(started.child_env_allow, stepped.child_env_allow);
        assert_eq!(started.roost_url, stepped.roost_url);
        assert_eq!(started_spawner.session_id, stepped_spawner.session_id);
        assert_eq!(started_spawner.context_id, stepped_spawner.context_id);
    }

    /// Every launch joins the delegating session's formation, on both paths and whether or not
    /// the launch names a lineage; a session in no formation hands its children none.
    #[test]
    fn every_launch_carries_the_sessions_formation_id() {
        let store = store_with_worker();
        let member = FormationId::mint();
        for formation_id in [Some(member), None] {
            let plane = plane_in_formation(&store, &["B"], formation_id.clone());
            assert_eq!(plane.formation_id(), formation_id.as_ref());
            let unnamed = DelegationOrigin::default();
            for (origin, report_to) in [
                (origin(), Some(completion_address())),
                (origin(), None),
                (unnamed, None),
            ] {
                let launch = plane
                    .launch_request(
                        &request(),
                        &origin,
                        SpawnApproval::new("approved".to_string()),
                        report_to,
                    )
                    .expect("the store holds the child's declaration");
                assert_eq!(
                    launch.formation_id,
                    formation_id,
                    "spawner present: {}",
                    launch.spawner.is_some()
                );
            }
        }
    }

    /// A delegated child is never handed its parent's formation channel or callees: the request
    /// carries no such field, and the environment built from it holds neither
    /// `MURMUR_FORMATION_CHANNEL` nor `MURMUR_FORMATION_PEERS` even when the parent's process
    /// carries both and the child allowlists both names. The formation id is still handed on.
    #[test]
    fn a_launch_request_hands_a_child_no_formation_channel_or_peers() {
        let _guard = crate::formation::FORMATION_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let peers = crate::formation::FORMATION_PEERS_ENV;
        let channel = crate::formation_credentials::FORMATION_CHANNEL_ENV;
        let store = store_with_worker();
        let plane = plane_in_formation(&store, &["B", peers, channel], Some(FormationId::mint()));
        let mut launch = plane
            .launch_request(
                &request(),
                &origin(),
                SpawnApproval::new("approved".to_string()),
                Some(completion_address()),
            )
            .expect("the store holds the child's declaration");
        launch.child_env_allow.push(peers.to_string());
        launch.child_env_allow.push(channel.to_string());
        std::env::set_var(peers, "coder=http://coder.formation.invalid");
        std::env::set_var(channel, "7");
        let env = crate::child_launch::child_environment(&launch, None);
        std::env::remove_var(peers);
        std::env::remove_var(channel);
        assert!(
            env.iter().all(|(key, _)| key != peers && key != channel),
            "{env:?}"
        );
        assert!(
            env.iter()
                .any(|(key, _)| key == crate::formation::FORMATION_ID_ENV),
            "the formation id is still handed on: {env:?}"
        );
    }

    /// A delegated child is handed neither of its parent's lifelines, even when it allowlists both
    /// names and the parent's process carries both: each names a descriptor of the parent's own
    /// process. The child holds a spawner lifeline of its own, which the launch hands it beside
    /// this environment, and no formation lifeline at all. The formation id is still handed on.
    #[test]
    fn a_launch_request_hands_a_child_its_own_spawner_lifeline_and_no_formation_one() {
        let _guard = crate::formation::FORMATION_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let formation = crate::lifeline::FORMATION_LIFELINE_ENV;
        let spawner = crate::lifeline::SPAWNER_LIFELINE_ENV;
        let store = store_with_worker();
        let plane = plane_in_formation(
            &store,
            &["B", formation, spawner],
            Some(FormationId::mint()),
        );
        let mut launch = plane
            .launch_request(
                &request(),
                &origin(),
                SpawnApproval::new("approved".to_string()),
                Some(completion_address()),
            )
            .expect("the store holds the child's declaration");
        launch.child_env_allow.push(formation.to_string());
        launch.child_env_allow.push(spawner.to_string());
        std::env::set_var(formation, "7");
        std::env::set_var(spawner, "8");
        let env = crate::child_launch::child_environment(&launch, None);
        std::env::remove_var(formation);
        std::env::remove_var(spawner);
        assert!(
            env.iter()
                .all(|(key, _)| key != formation && key != spawner),
            "{env:?}"
        );
        assert!(
            env.iter().all(|(_, value)| value != "7" && value != "8"),
            "{env:?}"
        );
        assert!(
            env.iter()
                .any(|(key, _)| key == crate::formation::FORMATION_ID_ENV),
            "the formation id is still handed on: {env:?}"
        );
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
            None,
        );
        assert_eq!(plane.roost_url, "http://127.0.0.1:7700");
    }
}
