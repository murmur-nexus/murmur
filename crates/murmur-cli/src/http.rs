//! The CLI's blocking HTTP agent.

/// Blocking HTTP agent: total request timeout, redirects followed, non-2xx statuses returned as
/// responses (not errors) so call sites can match on them.
pub(crate) fn blocking_agent(timeout: std::time::Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(timeout))
        .build()
        .into()
}
