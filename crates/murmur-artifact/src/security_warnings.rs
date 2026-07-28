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
/// exec/network are seccomp-enforced, `socket(AF_UNIX)` is refused unless
/// `capabilities.network.unix_sockets` is declared (and `AF_NETLINK`/`AF_PACKET` always are, with
/// no opt-in), and the shell child still drops every Linux capability before `execve` — but
/// filesystem scope is not enforced at all. The socket-domain rule is pure seccomp, so it is
/// identical on this tier and on `W_SEC_005`'s; the fixed capsule device set is the opposite —
/// pure Landlock, so on this tier it does not apply and every device under `/dev` is reachable.
pub const W_SEC_002: &str = "W-SEC-002";

/// `capabilities.shell.allow` includes `"bash"` and `capabilities.network.allow` is non-empty,
/// but nothing on this host constrains bash's own outbound network access.
pub const W_SEC_003: &str = "W-SEC-003";

/// A manifest field that looks credential-shaped (`api_key`, `token`, `secret`, `password`)
/// holds a literal value instead of a `${VAR_NAME}` reference.
pub const W_SEC_004: &str = "W-SEC-004";

/// The host resolved to a Linux kernel-enforcement tier (Landlock/seccomp). Landlock now grants a
/// narrow, derived read+execute scope outside the workdir (the allowlisted binaries, their loader,
/// and their shared libraries — nothing writable), so allowlisted programs can actually run; on top
/// of that it grants a fixed, non-manifest-derived device set — `/dev/null` read+write (the sole
/// writable path outside the workdir), `/dev/zero` and `/dev/urandom` read-only, every other device
/// including `/dev/random` denied; the
/// workdir's own grant withholds character-device, block-device and unix-socket creation, so a
/// capsule cannot `mknod` a raw disk node inside it; a classic seccomp rule on `socket(2)`'s
/// `domain` refuses `AF_UNIX` unless `capabilities.network.unix_sockets` is declared and refuses
/// `AF_NETLINK`/`AF_PACKET` unconditionally, so a capsule cannot reach a host daemon socket such as
/// `/var/run/docker.sock`; and the forked shell child drops every Linux
/// capability and sets `no_new_privs` before `execve`. None of this has yet been verified by the
/// team on real Landlock-capable Linux hardware — treat it as not-yet-confirmed rather than a
/// hardened boundary. Fires on both Linux tiers so the "full" tier is not silently assumed to be
/// confirmed-enforced.
pub const W_SEC_005: &str = "W-SEC-005";

/// A `runtime: hook` artifact entry declares `capabilities.shell`/`.spawn`/`.env`/`.limits`,
/// but a per-hook grant only reads `network` and `filesystem` — the other sub-blocks are
/// structurally accepted and silently inert.
pub const W_SEC_006: &str = "W-SEC-006";

/// A `runtime: tool`/`runtime: driver` artifact entry declares a `capabilities.network.allow`
/// entry the capsule-wide ceiling does not itself allow. Narrowing can only subtract, so the
/// entry is dropped from that artifact's effective grant rather than granted.
pub const W_SEC_007: &str = "W-SEC-007";

/// A `runtime: tool`/`runtime: driver` artifact entry declares `capabilities.shell`/`.spawn`/
/// `.env`/`.limits`, but per-artifact narrowing only reads `network` and `filesystem` — the
/// other sub-blocks are structurally accepted and silently inert.
pub const W_SEC_008: &str = "W-SEC-008";

/// `capabilities.shell.interpreter_runtime` grants an allowlisted binary specific host
/// directories outside the workdir. This couples the capsule to a specific host
/// distro/interpreter-version layout (e.g. `/usr/lib/python3.11` breaks the moment the host
/// ships Python 3.12); the durable fix is the still-unbuilt staged runtime bind-mount, which
/// this grant only bridges until. Fires once per declared grant, at staging.
pub const W_SEC_009: &str = "W-SEC-009";

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
