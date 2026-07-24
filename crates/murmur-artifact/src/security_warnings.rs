//! Registry of `W-SEC-*` codes for non-fatal, security-relevant runtime warnings.
//!
//! Mirrors the `E-<CATEGORY>-NNN` convention `murmur-cli`'s `CliError` uses for fatal errors
//! (see `murmur_cli::error`), but for warnings: printed and the session continues. Each code
//! has a matching `#w-sec-nnn` anchor on the security-warnings reference page, so callers can
//! append [`security_warning_link`] to the message instead of re-explaining the issue inline.

/// `capabilities.shell.allow` is non-empty but the host has no kernel-level subprocess
/// sandboxing primitive (Landlock/seccomp are Linux-only) — enforcement is environment-only.
pub const W_SEC_001: &str = "W-SEC-001";

/// `capabilities.shell.allow` is non-empty on a Linux host without Landlock (kernel <5.13) —
/// exec/network are seccomp-enforced but filesystem scope is not.
pub const W_SEC_002: &str = "W-SEC-002";

/// `capabilities.shell.allow` includes `"bash"` and `capabilities.network.allow` is non-empty,
/// but nothing on this host constrains bash's own outbound network access.
pub const W_SEC_003: &str = "W-SEC-003";

/// A manifest field that looks credential-shaped (`api_key`, `token`, `secret`, `password`)
/// holds a literal value instead of a `${VAR_NAME}` reference.
pub const W_SEC_004: &str = "W-SEC-004";

/// The host resolved to a Linux kernel-enforcement tier (Landlock/seccomp), but that
/// enforcement has never been verified on real Linux hardware — treat it as experimental,
/// not a security boundary. Fires on both Linux tiers so the "full" tier is not silently
/// assumed to be enforced.
pub const W_SEC_005: &str = "W-SEC-005";

/// A `runtime: hook` artifact entry declares `capabilities.shell`/`.spawn`/`.env`/`.limits`,
/// but a per-hook grant only reads `network` and `filesystem` — the other sub-blocks are
/// structurally accepted and silently inert.
pub const W_SEC_006: &str = "W-SEC-006";

const SECURITY_WARNINGS_DOC_URL: &str =
    "https://docs.murmur.nexus/murmur-nexus/murmur/reference/security-warnings/";

/// Builds the doc link for a `W-SEC-*` code, e.g. `.../security-warnings/#w-sec-001`.
pub fn security_warning_link(code: &str) -> String {
    format!("{SECURITY_WARNINGS_DOC_URL}#{}", code.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_lowercases_the_code_into_the_anchor() {
        assert_eq!(
            security_warning_link(W_SEC_001),
            "https://docs.murmur.nexus/murmur-nexus/murmur/reference/security-warnings/#w-sec-001"
        );
    }
}
