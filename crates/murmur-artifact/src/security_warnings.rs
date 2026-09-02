//! Registry of `W-SEC-*` codes for non-fatal, security-relevant runtime warnings.
//!
//! Mirrors the `E-<CATEGORY>-NNN` convention `murmur-cli`'s `CliError` uses for fatal errors
//! (see `murmur_cli::error`), but for warnings: printed and the session continues. Each code
//! has a matching `#w-sec-nnn` anchor on the diagnostics reference page, so callers can
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
/// but a per-hook grant only reads `network`, `filesystem` and `task_io` — the other
/// sub-blocks are structurally accepted and silently inert.
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

/// The capsule can spawn native subprocesses on a host where no cgroup v2 scope can exist
/// (macOS, or any non-Linux target). Per-process `rlimit(2)` ceilings still apply, but every
/// *aggregate* bound does not: nothing caps memory, pid count or CPU across the subprocess tree
/// as a whole. `RLIMIT_NPROC` is not a substitute — it is per-**uid**, so a fork bomb of
/// distinct, short-lived processes evades it. Permanent on this platform, like [`W_SEC_001`].
pub const W_SEC_010: &str = "W-SEC-010";

/// `capabilities.filesystem.workdir_exec: true` keeps the Landlock `Execute` right on the session
/// workdir, so anything the capsule writes there can run regardless of `capabilities.shell.allow`.
/// The allowlist is still applied to what the *agent asks* to run, but it stops being an
/// enforceable property of the capsule: a binary compiled, downloaded or renamed inside the workdir
/// executes on its own. Because of that the capsule can never achieve
/// `ContainmentClass::Scoped` — it reports `advisory` on every host, including a Landlock-capable
/// one — and pairing it with `capabilities.containment: scoped` refuses the launch with `E-CAP-003`.
/// Fires once, at staging, only for a capsule that declared it.
pub const W_SEC_011: &str = "W-SEC-011";

/// A `sealed` capsule's `capabilities.shell.allow` names a known compiler driver (`cc`, `gcc`,
/// `g++`, `c++`) whose helper subprocess — `cc1`, `cc1plus`, `as`, `ld`, `collect2` — has no grant
/// carrying the Landlock `Execute` right. The driver forks and execs those helpers itself; they
/// are separate binaries outside its own `DT_NEEDED` closure, so nothing derives them, and they
/// live under the fixed sealed runtime tree (`/usr`, `/bin`, …), which is bind-mounted read-only
/// and granted `list_dir: true, executable: false` — present and readable, but not runnable. The
/// driver therefore starts, answers `--version`, and then fails partway through the first real
/// compile. The fix is an `interpreter_runtime` or `staged_runtime` grant naming the helper's
/// containing directory, both of which carry `Execute`. A warning rather than a refusal because
/// the `<driver> -print-prog-name=<helper>` probe behind it is a heuristic about one driver
/// family: a hard refusal built on it could block a capsule that would in fact have worked. Fires
/// once per uncovered helper, at staging, only under a declared `sealed` floor.
pub const W_SEC_012: &str = "W-SEC-012";

/// AppArmor is enabled on this host and `kernel.apparmor_restrict_unprivileged_userns` is off, so
/// unprivileged user namespaces are unrestricted for *every* binary on the machine rather than for
/// `mur` alone. That is what makes `capabilities.containment: sealed` and the capsule network
/// namespace work here, and it is not the configuration murmur ships: the shipped `mur-sealed`
/// AppArmor profile grants the same permission to one binary. A legitimate operator choice and on
/// some hosts the only one, so it never refuses a run and never changes an exit code — but a
/// `sealed` result obtained this way and one obtained through the profile must not read the same.
/// Fires once, from the host probe, at staging and from `mur doctor`.
pub const W_SEC_013: &str = "W-SEC-013";

/// A `capabilities.state` block was declared somewhere nothing reads it, so no durable store was
/// created and no `state/` preopen exists.
///
/// The grant is applied per *artifact*, on the tool, driver or hook entry that will actually run
/// with the second preopen. A capsule-wide, top-level `capabilities.state` reaches no such entry:
/// the capsule's own guest is built with no artifact grant at all. Structurally valid and
/// therefore warned rather than refused, on the same terms as `W-SEC-006`/`W-SEC-008` — but stated
/// plainly, because an operator who believes their capsule has durable state and finds an empty
/// directory has no other signal that the declaration went nowhere.
/// Fires at staging, before any session workdir exists.
pub const W_SEC_014: &str = "W-SEC-014";

/// A `config:` block was declared on a `runtime: tool` entry whose artifact ships a native
/// (non-WASM) implementation, so no `MURMUR_ARTIFACT_CONFIG` variable is delivered anywhere.
///
/// Config travels in the per-artifact WASI environment the runtime builds for a WASM guest. A
/// native tool runs as a host subprocess under the capsule-wide shell environment, which is not
/// per-artifact and which the runtime will not write an operator's config block into. Structurally
/// valid and therefore warned rather than refused, on the same terms `capabilities:` on a native
/// tool already warns `W-SEC-008`. Fires at staging, before any session workdir exists.
pub const W_SEC_015: &str = "W-SEC-015";

/// A `capabilities.conversation` block was declared in the capsule-wide `capabilities:` block,
/// where nothing reads it, so no artifact was granted `murmur:conversation/read`.
///
/// The grant is applied per *artifact*, on the `runtime: hook` entry whose component imports the
/// interface. The capsule's own guest holds no artifact grant and compiles against a world that
/// has no such import, so a top-level declaration reaches nothing. Structurally valid and
/// therefore warned rather than refused, on the same terms as `W-SEC-014`.
/// Fires at staging, before any session workdir exists.
pub const W_SEC_016: &str = "W-SEC-016";

/// `capabilities.filesystem.read_only` is declared alongside an allowlisted interpreter, so the
/// declaration is advisory for that binary.
///
/// The dispatch-time write-intent analyser reads a shell call's argv and `-c` script text. An
/// interpreter's own file I/O is not in either: `python3 -c "open(p,'w').write(x)"` is one opaque
/// argument, and nothing in it names a redirection or a write verb the analyser recognizes. The
/// declaration still holds for every call the analyser *can* read, and it still holds against the
/// tool path — this names the one route around the shell half rather than leaving it implied.
/// Fires once per allowlisted interpreter, at staging, only for a capsule that declared
/// `read_only`.
pub const W_SEC_017: &str = "W-SEC-017";

/// `capabilities.filesystem.read_only` is declared and an installed tool's `input_schema` names a
/// path-shaped or destination-shaped property without saying which of its inputs are filesystem
/// destinations, so its calls are judged by key name.
///
/// The dispatch-time write-intent analyser guesses from property names when a tool tells it
/// nothing: an input pairing a path-shaped key with a content-shaped key is refused, wherever in
/// the input that pair sits. The guess is wrong in both directions — a note a tool merely stores
/// is refused as a write, and a destination under an unrecognized name is never checked. A tool
/// silences this by annotating its schema: `"format": "murmur-destination"` on a destination
/// string, `"format": "murmur-opaque"` on an object or array it only stores. Fires once per
/// installed tool, at staging, only for a capsule that declared `read_only`.
pub const W_SEC_018: &str = "W-SEC-018";

/// A key in `murmur.yaml` that this build does not recognize, captured during parse instead of
/// dropped.
///
/// Every `Raw*` deserialization struct carries a `#[serde(flatten)]` overflow map, so a key no
/// field claims lands there rather than vanishing. Fires once per captured key, naming the key,
/// the dotted path of the block that held it, and — when one is near enough by edit distance —
/// the recognized key of that block it most resembles. A near match is worded as a spelling
/// problem; no near match is worded as a key this build does not know, which a newer `mur` may.
///
/// One further line is emitted when `mur_version` pins a numeric triple higher than the running
/// binary's, naming both versions and the count, so a stale binary is stated rather than inferred
/// from unfamiliar key names.
///
/// Never a refusal: `#[serde(deny_unknown_fields)]` is deliberately set nowhere in the manifest
/// parser, because refusing an unknown key makes a manifest written for a newer `mur` unloadable
/// by an older one. Fires from `mur run` and from `mur doctor`, in the same words.
pub const W_SEC_019: &str = "W-SEC-019";

const DIAGNOSTICS_DOC_URL: &str =
    "https://docs.murmur.nexus/murmur-nexus/murmur/reference/diagnostics/";

/// Builds the doc link for a `W-SEC-*` code, e.g. `.../diagnostics/#w-sec-001`.
pub fn security_warning_link(code: &str) -> String {
    format!("{DIAGNOSTICS_DOC_URL}#{}", code.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every code is distinct and spelled in the `W-SEC-NNN` shape the diagnostics page anchors
    /// on. A duplicated or misspelled constant would silently point two warnings at one anchor.
    #[test]
    fn every_code_is_unique_and_well_formed() {
        let codes = [
            W_SEC_001, W_SEC_002, W_SEC_003, W_SEC_004, W_SEC_005, W_SEC_006, W_SEC_007, W_SEC_008,
            W_SEC_009, W_SEC_010, W_SEC_011, W_SEC_012, W_SEC_013, W_SEC_014, W_SEC_015, W_SEC_016,
            W_SEC_017, W_SEC_018, W_SEC_019,
        ];
        for code in codes {
            assert!(code.starts_with("W-SEC-"), "malformed code: {code}");
            assert_eq!(code.len(), 9, "malformed code: {code}");
        }
        let mut unique = codes.to_vec();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), codes.len(), "two W-SEC codes are equal");
    }

    #[test]
    fn link_lowercases_the_code_into_the_anchor() {
        assert_eq!(
            security_warning_link(W_SEC_001),
            "https://docs.murmur.nexus/murmur-nexus/murmur/reference/diagnostics/#w-sec-001"
        );
    }
}
