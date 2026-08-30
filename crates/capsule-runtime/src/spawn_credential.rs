//! The two secrets a delegated spawn travels on, and the headers they travel in.
//!
//! `mur-roost` mints both; the runtime carries them and presents them. They live here, in the
//! crate both sides already depend on, so the daemon and its one client read a single definition
//! of each header name rather than two string literals that can drift apart.
//!
//! * A **credential** names the session that holds it. It is minted once, when a session whose
//!   manifest declares `capabilities.spawn.allow` is staged, and it is what makes a spawn request
//!   answerable at all — the daemon judges the session the credential names, never the one a
//!   request body claims.
//! * An **approval** names one resolved artifact, for one session, for one launch. It is what
//!   `POST /delegate` returns and what `POST /spawn` redeems.
//!
//! Both are opaque to this crate. Neither type parses, compares or validates what it holds: the
//! authority that minted a token is the only thing that can read it, and everything here exists to
//! make the string hard to spill on the way there.
//!
//! **Neither implements `Display` or `Serialize`, and both print redacted under `Debug`.** A token
//! reaches a trace, a log line or a tool result the moment one of those exists — the formatting
//! call site does not have to be about the token for the token to end up in the record. Reading one
//! takes the deliberate step of calling [`SpawnCredential::expose`] or [`SpawnApproval::expose`],
//! and those have exactly one call site each: the request headers in `plan::dispatch_capsule_step`.

/// Request header carrying the credential that names the calling session. Required on
/// `POST /delegate` and on any `POST /spawn` that redeems an approval.
pub const SPAWN_CREDENTIAL_HEADER: &str = "x-murmur-spawn-credential";

/// Request header carrying the approval a `POST /delegate` returned. Required on any
/// `POST /spawn` other than the operator's own top-level path.
pub const SPAWN_APPROVAL_HEADER: &str = "x-murmur-spawn-approval";

/// The opaque, per-session token that proves which session is asking.
///
/// Handed to a session's runtime at launch and held in memory for the life of that runtime. It is
/// never written to the workdir, never placed in an environment variable, never returned in a tool
/// result and never rendered into an error the model sees.
#[derive(Clone)]
pub struct SpawnCredential(String);

impl SpawnCredential {
    pub fn new(token: String) -> Self {
        Self(token)
    }

    /// The token text, for placing in a request header and nothing else.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SpawnCredential {
    /// Prints no token material.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SpawnCredential(<redacted>)")
    }
}

/// The opaque, single-use token that names one approved launch of one resolved artifact.
///
/// Bound to the session that earned it as well as to the artifact, so redeeming it requires
/// re-presenting the credential alongside it.
#[derive(Clone)]
pub struct SpawnApproval(String);

impl SpawnApproval {
    pub fn new(token: String) -> Self {
        Self(token)
    }

    /// The token text, for placing in a request header and nothing else.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SpawnApproval {
    /// Prints no token material.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SpawnApproval(<redacted>)")
    }
}
