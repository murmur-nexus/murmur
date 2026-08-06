//! `capabilities.shell.staged_runtime`: staging a pinned host runtime tree into a capsule root.
//!
//! ## The mechanism, and why it is the inverse of `interpreter_runtime`
//!
//! A path-based interpreter — CPython above all — cannot run from an allowlisted binary alone. Its
//! stdlib lives outside the workdir, at a path the `DT_NEEDED` closure cannot discover, so
//! *something* has to make that tree reachable. There are exactly two ways to do it:
//!
//!   * [`crate::sandbox`]'s `interpreter_runtime` grants widen the capsule's Landlock scope
//!     *outwards*, so the host's `/usr/lib/python3.11` stays readable from inside. That works at
//!     `scoped`, and it costs: the capsule is now coupled to one host's directory layout, host
//!     paths stay reachable, and moving to a second host with a different Python build silently
//!     changes what the capsule runs. This is why declaring one fires `W-SEC-009`.
//!   * `staged_runtime` moves the tree *inwards*. A `sealed` capsule already builds a composed
//!     root out of read-only binds ([`crate::sealed`]); staging adds one more bind, of an
//!     operator-named, operator-pinned runtime tree, at the same absolute path it has on the host.
//!     Nothing outside the root becomes reachable, and the `pin` makes "the same interpreter on
//!     both hosts" a claim a human can check rather than assume.
//!
//! The second supersedes the first for any binary that uses it, which is why the manifest parser
//! refuses both for one binary rather than merging them (see `parse_staged_runtime` in
//! `murmur-artifact`). Staging is not an additional grant layered onto a widened scope; it is the
//! thing that makes widening unnecessary.
//!
//! ## What this module contains, and how the mount reaches the launch path
//!
//! Two independent pieces, with very different reach:
//!
//!   * [`check_staged_runtime_floor`] — pure, every OS, and **live in the launch path**. It is the
//!     gate that refuses to stage a capsule declaring `staged_runtime` below an effective `sealed`
//!     floor, called from `runtime::stage_session` next to `check_containment_floor`.
//!   * [`bind_mount_staged_runtimes`] — Linux-only, and the executable statement of the mount
//!     *contract*: the re-basing rule, the two-call read-only bind, and the failure taxonomy. It
//!     is built and proven here in isolation, against a throwaway directory tree inside a mount
//!     namespace its own test creates.
//!
//! The staging mounts **are** performed on every `sealed` launch that declares a grant — but not
//! by calling that function. `sandbox::resolve_staged_runtime_dirs` collects each grant's
//! `source_path` into `ShellEnforcement::staged_runtime_dirs`, `build_sealed_root` passes them to
//! [`crate::sealed::plan_composed_root`]'s `staged_runtime_read_only` parameter, and they are
//! planned as *required* `RootOp::Bind` steps ahead of every other step — and so ahead of
//! `pivot_root`. A grant whose source is missing fails at the real `mount(2)` with `ENOENT`,
//! aborting the construction; that reaches the operator as
//! `RuntimeError::SealedRootConstructionFailed` (`E-RUN-014`) and is session-fatal.
//!
//! The function itself stays uncalled by production code because it allocates, and the composed
//! root executes inside the forked child's `pre_exec` window, which must not — see
//! [`bind_mount_staged_runtimes`]'s own doc comment. It remains valuable as an independently
//! provable statement of what the planned step must do on a real kernel, without needing a sealed
//! capsule, a `pivot_root` or the enforcement pipeline to demonstrate it.

use std::path::{Path, PathBuf};

use murmur_artifact::{ContainmentClass, StagedRuntimeGrant};

use crate::errors::RuntimeError;

/// Refuses a capsule that declares `capabilities.shell.staged_runtime` without an effective
/// `sealed` containment floor.
///
/// Compares against the **declared** floor — the already-combined strongest of manifest,
/// workspace config and `--containment` — and never probes the host. That is the whole point of
/// keeping it separate from [`crate::containment::check_containment_floor`], which asks the
/// opposite question (can this host back what was declared?) and has the opposite remedy. A
/// machine that could deliver `sealed` still fails this check when the capsule did not ask for
/// `sealed`, because the composed root is only built for a capsule that declared it: launching
/// anyway would produce a capsule whose interpreter is simply not there, diagnosed as a missing
/// binary somewhere deep in a task rather than as the policy error it is.
///
/// Pure, so the decision is testable on any OS and at any enforcement tier.
pub fn check_staged_runtime_floor(
    grants: &[StagedRuntimeGrant],
    declared: ContainmentClass,
) -> Result<(), RuntimeError> {
    if grants.is_empty() || declared >= ContainmentClass::Sealed {
        return Ok(());
    }

    Err(RuntimeError::StagedRuntimeRequiresSealed {
        // Every offending binary, not just the first: an operator fixing this should not have to
        // re-run to discover the second grant.
        binaries: grants
            .iter()
            .map(|grant| grant.binary.clone())
            .collect(),
        declared,
    })
}

/// Where [`bind_mount_staged_runtimes`] failed, so the error names a step rather than a bare
/// errno.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StagedRuntimeMountStage {
    /// Creating the mount point inside the target root.
    CreateMountPoint,
    /// The `MS_BIND | MS_REC` mount itself.
    Bind,
    /// The second, `MS_REMOUNT | MS_BIND | MS_RDONLY` call that actually makes the bind read-only.
    RemountReadOnly,
}

#[cfg(target_os = "linux")]
impl std::fmt::Display for StagedRuntimeMountStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::CreateMountPoint => "create mount point",
            Self::Bind => "bind mount",
            Self::RemountReadOnly => "remount read-only",
        };
        f.write_str(name)
    }
}

/// One staged-runtime bind mount that could not be established.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone)]
pub struct StagedRuntimeMountError {
    /// The `staged_runtime` grant's binary, so the failure names the capsule's own declaration
    /// rather than only a path.
    pub binary: String,
    /// The host tree that was being staged.
    pub source_path: PathBuf,
    /// Where it was being staged to, inside the target root.
    pub target_path: PathBuf,
    pub stage: StagedRuntimeMountStage,
    /// `errno` from the failing syscall.
    pub errno: i32,
}

#[cfg(target_os = "linux")]
impl std::fmt::Display for StagedRuntimeMountError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "staged runtime for '{}': {} of {} at {} failed (errno {})",
            self.binary,
            self.stage,
            self.source_path.display(),
            self.target_path.display(),
            self.errno
        )
    }
}

#[cfg(target_os = "linux")]
impl std::error::Error for StagedRuntimeMountError {}

/// Re-bases an absolute host path under `root`, preserving it verbatim: `/opt/py` under `/newroot`
/// is `/newroot/opt/py`.
///
/// Identity of the path inside and outside the root is the property the whole mechanism rests on —
/// `sys.prefix`, shebang lines, `PYTHONHOME` and anything a previous turn wrote to disk all keep
/// resolving, and the capsule never has to be told its interpreter moved. It is the same rule
/// [`crate::sealed`] applies to the session workdir, via the same [`crate::sealed::rebase`]
/// function — reused here rather than reimplemented, so there is exactly one re-basing rule.
#[cfg(target_os = "linux")]
fn target_under_root(root: &Path, source_path: &Path) -> PathBuf {
    crate::sealed::rebase(root, source_path)
}

/// Bind-mounts every declared `staged_runtime` tree read-only into `root`, each at its own
/// absolute host path re-based under `root`. Returns the target paths it established, in order.
///
/// # Where the launch path performs this, and why it is not here
///
/// The staging mounts are live: every `sealed` launch that declares a grant establishes them
/// before `pivot_root`. They are just not established by calling this function, and the reason is
/// worth stating rather than leaving for someone to rediscover.
///
/// [`crate::sealed`] executes its mounts inside the forked child's `pre_exec` window, where
/// allocation can deadlock on a malloc lock another thread of the parent held at `fork()`. It
/// avoids that by splitting the decision (`plan_composed_root`, pure, parent-side) from the
/// execution (`construct_composed_root`, raw syscalls over `CString`s built before the fork). This
/// function allocates — it joins paths and creates directories — so calling it from that window
/// as-is would break the invariant that split exists to protect.
///
/// The wiring is therefore a *plan entry*, not a call: each grant's `source_path` is fed to
/// [`crate::sealed::plan_composed_root`]'s `staged_runtime_read_only` parameter, which lowers to
/// an allocation-free, **required** `RootOp::Bind { read_only: true }`, planned ahead of every
/// other step. Note that this is a dedicated parameter and not the pre-existing `extra_read_only`:
/// entries there are planned via `mirror`, which schedules *nothing at all* for a path the host
/// does not have, so a missing source would have launched a capsule into a root silently lacking
/// its runtime tree — the opposite of the guarantee below.
///
/// What remains here is the executable, independently provable statement of what that plan entry
/// must *do*: same re-basing rule, same two-call read-only bind, same failure taxonomy. Its test
/// is what pins that behaviour down on a real kernel without requiring a sealed capsule, a
/// `pivot_root`, or the enforcement pipeline.
///
/// # Preconditions
///
/// The caller must already be inside a mount namespace it is willing to modify — this function
/// does not create one, on purpose, so it composes with a namespace built for other reasons. Each
/// `source_path` must exist on the host; a grant naming a missing tree fails at
/// [`StagedRuntimeMountStage::Bind`] with `ENOENT` rather than being skipped, because a capsule
/// that declared a pinned runtime and did not get it must not proceed as though it had.
///
/// # Read-only takes two calls
///
/// A single `mount(MS_BIND | MS_RDONLY)` does **not** produce a read-only bind — the kernel
/// applies the flag to the new mount only on a follow-up `MS_REMOUNT | MS_BIND`. Both are issued
/// here, and `MS_NOSUID` rides along on the remount, matching [`crate::sealed`]'s binds.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
pub fn bind_mount_staged_runtimes(
    root: &Path,
    grants: &[StagedRuntimeGrant],
) -> Result<Vec<PathBuf>, StagedRuntimeMountError> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let mut staged = Vec::with_capacity(grants.len());

    for grant in grants {
        let source = PathBuf::from(&grant.source_path);
        let target = target_under_root(root, &source);
        let fail = |stage: StagedRuntimeMountStage, errno: i32| StagedRuntimeMountError {
            binary: grant.binary.clone(),
            source_path: source.clone(),
            target_path: target.clone(),
            stage,
            errno,
        };

        // The mount point has to exist before anything can be bound over it. A runtime tree is a
        // directory, so this is `mkdir -p`; `EEXIST` is success, which is what `create_dir_all`
        // already reports.
        std::fs::create_dir_all(&target).map_err(|error| {
            fail(
                StagedRuntimeMountStage::CreateMountPoint,
                error.raw_os_error().unwrap_or(0),
            )
        })?;

        // A NUL byte cannot reach here from a parsed manifest (serde_yaml rejects it in a scalar),
        // but the conversion is fallible, so report it as the mount-point failure it functionally
        // is rather than unwrapping in a function that may one day run near a fork.
        let c_source = CString::new(source.as_os_str().as_bytes())
            .map_err(|_| fail(StagedRuntimeMountStage::CreateMountPoint, libc::EINVAL))?;
        let c_target = CString::new(target.as_os_str().as_bytes())
            .map_err(|_| fail(StagedRuntimeMountStage::CreateMountPoint, libc::EINVAL))?;

        // SAFETY: both pointers are NUL-terminated and outlive each call; the filesystem-type and
        // data arguments are unused for a bind mount and are passed as NULL, as `mount(2)`
        // specifies.
        unsafe {
            if libc::mount(
                c_source.as_ptr(),
                c_target.as_ptr(),
                std::ptr::null(),
                libc::MS_BIND | libc::MS_REC,
                std::ptr::null(),
            ) != 0
            {
                return Err(fail(StagedRuntimeMountStage::Bind, errno()));
            }

            // The call that actually makes it read-only. Without this the tree above is writable.
            if libc::mount(
                std::ptr::null(),
                c_target.as_ptr(),
                std::ptr::null(),
                libc::MS_REMOUNT | libc::MS_BIND | libc::MS_RDONLY | libc::MS_NOSUID,
                std::ptr::null(),
            ) != 0
            {
                return Err(fail(StagedRuntimeMountStage::RemountReadOnly, errno()));
            }
        }

        staged.push(target);
    }

    Ok(staged)
}

#[cfg(target_os = "linux")]
fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant(binary: &str, source_path: &str) -> StagedRuntimeGrant {
        StagedRuntimeGrant {
            binary: binary.to_string(),
            source_path: source_path.to_string(),
            pin: "test-pin-1".to_string(),
        }
    }

    #[test]
    fn no_grants_never_requires_sealed() {
        for declared in [
            ContainmentClass::Advisory,
            ContainmentClass::Scoped,
            ContainmentClass::Sealed,
        ] {
            assert!(check_staged_runtime_floor(&[], declared).is_ok());
        }
    }

    #[test]
    fn staged_runtime_below_sealed_is_refused() {
        for declared in [ContainmentClass::Advisory, ContainmentClass::Scoped] {
            let error = check_staged_runtime_floor(&[grant("python3", "/opt/py")], declared)
                .expect_err("a staged runtime below sealed must be refused");
            match error {
                RuntimeError::StagedRuntimeRequiresSealed {
                    binaries,
                    declared: reported,
                } => {
                    assert_eq!(binaries, vec!["python3".to_string()]);
                    assert_eq!(reported, declared);
                }
                other => panic!("expected StagedRuntimeRequiresSealed, got {other:?}"),
            }
        }
    }

    #[test]
    fn staged_runtime_at_sealed_is_accepted() {
        assert!(check_staged_runtime_floor(
            &[grant("python3", "/opt/py")],
            ContainmentClass::Sealed
        )
        .is_ok());
    }

    /// Every tier, so a new variant cannot quietly escape a test that iterates "all of them".
    /// Mirrors the same list in `containment`'s tests.
    const ALL_TIERS: &[crate::sandbox::EnforcementTier] = &[
        crate::sandbox::EnforcementTier::KernelSealed,
        crate::sandbox::EnforcementTier::KernelFull,
        crate::sandbox::EnforcementTier::KernelSeccompOnly,
        crate::sandbox::EnforcementTier::EnvironmentOnly,
    ];

    /// The check is a function of the **declared** floor alone. This is the property that keeps it
    /// distinct from `check_containment_floor`, so it is asserted rather than left implicit in the
    /// signature: for every enforcement tier the host might probe as — including
    /// `KernelSealed`, where the host genuinely could deliver a composed root — a capsule that did
    /// not declare `sealed` is still refused, and one that did is still accepted.
    #[test]
    fn the_check_is_independent_of_the_achieved_tier() {
        let grants = [grant("python3", "/opt/testbed/conda")];

        for tier in ALL_TIERS {
            // The tier is deliberately unused by the call below; binding it here is what makes
            // the claim legible, and `achieved_class_for_tier` proves the tier is a real one that
            // maps to a class rather than a placeholder.
            let achieved = crate::containment::achieved_class_for_tier(*tier);

            for declared in [ContainmentClass::Advisory, ContainmentClass::Scoped] {
                assert!(
                    check_staged_runtime_floor(&grants, declared).is_err(),
                    "declared {declared} must be refused regardless of tier {tier:?} \
                     (which achieves {achieved})"
                );
            }
            assert!(
                check_staged_runtime_floor(&grants, ContainmentClass::Sealed).is_ok(),
                "a declared sealed floor must pass regardless of tier {tier:?}"
            );
        }
    }

    #[test]
    fn refusal_names_every_offending_binary() {
        let error = check_staged_runtime_floor(
            &[grant("python3", "/opt/py"), grant("node", "/opt/node")],
            ContainmentClass::Scoped,
        )
        .expect_err("must be refused");
        let rendered = error.to_string();
        assert!(rendered.contains("python3"), "{rendered}");
        assert!(rendered.contains("node"), "{rendered}");
        assert!(rendered.contains("sealed"), "{rendered}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn target_is_the_source_path_rebased_under_root() {
        assert_eq!(
            target_under_root(Path::new("/newroot"), Path::new("/opt/testbed/conda")),
            PathBuf::from("/newroot/opt/testbed/conda")
        );
        // A root with a trailing component of its own still nests the full source path.
        assert_eq!(
            target_under_root(Path::new("/tmp/mur-root"), Path::new("/usr/lib/python3.9")),
            PathBuf::from("/tmp/mur-root/usr/lib/python3.9")
        );
    }

    // ------------------------------------------------------------------ the real mount test
    //
    // `bind_mount_staged_runtimes` is only meaningful against a real kernel, so the test below
    // mounts for real inside a private mount namespace it creates itself. Two constraints shape
    // how it is run:
    //
    //   * `unshare(CLONE_NEWUSER)` returns `EINVAL` in a multi-threaded process, and libtest runs
    //     each test on its own thread by default. So the work happens in a re-executed copy of
    //     this same test binary, run with `--test-threads=1`, where libtest executes the test
    //     body on the main thread of an otherwise single-threaded process.
    //   * The inner run is `#[ignore]`d so a normal `cargo test` never picks it up directly — it
    //     is reached only through the driver below, which is what supplies the env var and the
    //     single-threaded harness.
    //
    // Nothing here uses `pivot_root`, the enforcement pipeline, or `crate::sealed`.

    /// Env var the driver sets to tell the re-executed copy it is the inner run.
    #[cfg(target_os = "linux")]
    const INNER_RUN_ENV: &str = "MURMUR_STAGED_RUNTIME_MOUNT_INNER";

    #[cfg(target_os = "linux")]
    #[test]
    fn bind_mount_staged_runtimes_mounts_read_only_in_a_private_namespace() {
        use std::process::Command;

        // The inner run needs a user namespace to get `CAP_SYS_ADMIN` for `mount(2)`. Where the
        // host forbids that, say so loudly and name what went unproven rather than passing quietly.
        if !unprivileged_userns_available() {
            eprintln!(
                "\n\
                 ==================== SKIPPED: staged-runtime bind mount ====================\n\
                 This host does not permit an unprivileged user namespace, so this test could\n\
                 NOT prove any of the following on this machine:\n\
                   - that bind_mount_staged_runtimes() establishes a real bind mount\n\
                   - that the staged tree is READ-ONLY through the target root\n\
                   - that the source tree's contents are readable through the target root\n\
                 The pure re-basing and floor-check tests above still ran. Re-run this suite on\n\
                 a Linux host with unprivileged user namespaces enabled\n\
                 (/proc/sys/kernel/unprivileged_userns_clone = 1, or run as root) before\n\
                 treating the mount behaviour as verified.\n\
                 ============================================================================\n"
            );
            return;
        }

        let exe = std::env::current_exe().expect("test binary path");
        let output = Command::new(exe)
            .args([
                "--exact",
                "staged_runtime::tests::inner_bind_mount_in_private_namespace",
                "--ignored",
                "--test-threads=1",
                "--nocapture",
            ])
            .env(INNER_RUN_ENV, "1")
            .output()
            .expect("re-exec the test binary for the single-threaded inner run");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "inner mount run failed\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
        );
        // Guard against the inner test silently not being selected: a filter typo would otherwise
        // leave a passing run that asserted nothing at all.
        assert!(
            stdout.contains("1 passed"),
            "inner run did not execute the mount test\n--- stdout ---\n{stdout}"
        );
    }

    /// True when this host lets an unprivileged process create a user namespace.
    #[cfg(target_os = "linux")]
    #[allow(unsafe_code)]
    fn unprivileged_userns_available() -> bool {
        // SAFETY: `geteuid` takes no arguments and cannot fail.
        if unsafe { libc::geteuid() } == 0 {
            return true;
        }
        // Debian/Ubuntu's knob. Absent on kernels that always allow it, so absence is not a "no".
        match std::fs::read_to_string("/proc/sys/kernel/unprivileged_userns_clone") {
            Ok(value) => value.trim() == "1",
            Err(_) => std::path::Path::new("/proc/self/ns/user").exists(),
        }
    }

    /// What the forked child reports back through its exit status. `Ok` is 0; every other value
    /// names one step, so a failure in the child is diagnosable from the parent without a pipe.
    #[cfg(target_os = "linux")]
    fn child_exit_meaning(code: i32) -> &'static str {
        match code {
            0 => "success",
            10 => "unshare(CLONE_NEWUSER|CLONE_NEWNS) failed",
            11 => "writing /proc/self/setgroups, uid_map or gid_map failed",
            12 => "making / MS_REC|MS_PRIVATE failed",
            13 => "bind_mount_staged_runtimes returned Err",
            14 => "the staged target path was not the source path re-based under the root",
            15 => "reading the marker file through the target root failed or read wrong contents",
            16 => "writing an existing file through the target root did NOT fail with EROFS",
            17 => "creating a new file through the target root did NOT fail with EROFS",
            18 => "the source tree stopped being writable on the host side",
            _ => "unrecognised child status",
        }
    }

    /// The inner run, driven by the test above. Creates a private mount namespace, stages a
    /// throwaway tree into a throwaway root, and checks both directions — reads succeed through
    /// the target, writes are refused with `EROFS`.
    ///
    /// The work happens in a `fork()`ed child for one specific reason: `unshare(CLONE_NEWUSER)`
    /// returns `EINVAL` in a multi-threaded process, and libtest runs the test body on a spawned
    /// thread even at `--test-threads=1` (the process reports `Threads: 2`). `fork` clones only
    /// the calling thread, so the child is single-threaded and the call is permitted.
    ///
    /// Allocating after `fork` in a threaded parent can deadlock on a malloc lock another thread
    /// held at fork time. That is why this runs under the driver's `--test-threads=1` re-exec: the
    /// only other thread is libtest's main thread, parked in a join and allocating nothing, so no
    /// lock can be held across the fork. Both halves of that arrangement are load-bearing — do not
    /// drop the re-exec and fork from a normal parallel test run.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "driven by bind_mount_staged_runtimes_mounts_read_only_in_a_private_namespace"]
    #[allow(unsafe_code)]
    fn inner_bind_mount_in_private_namespace() {
        if std::env::var(INNER_RUN_ENV).is_err() {
            panic!("inner run invoked without {INNER_RUN_ENV}; run the driver test instead");
        }

        // Everything the child needs, built *before* the fork. The temp dirs are owned by the
        // parent so they are cleaned up on the host: the child's mounts live only in its own
        // namespace and vanish when it exits, while the directories themselves are ordinary and
        // visible to both.
        let source_tree = tempfile::tempdir().expect("source tree");
        let root = tempfile::tempdir().expect("target root");
        let marker = source_tree.path().join("lib").join("marker.txt");
        std::fs::create_dir_all(marker.parent().unwrap()).expect("create source subdir");
        std::fs::write(&marker, "staged-runtime-contents").expect("write marker");

        let grants = vec![grant(
            "python3",
            source_tree.path().to_str().expect("utf-8 temp path"),
        )];
        let expected_target = target_under_root(root.path(), source_tree.path());
        let staged_marker = expected_target.join("lib").join("marker.txt");
        let uid = unsafe { libc::geteuid() };
        let gid = unsafe { libc::getegid() };

        // SAFETY: see the doc comment above — the child touches only its own namespace and exits
        // via `_exit`, never returning into libtest.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed: {}", std::io::Error::last_os_error());

        if pid == 0 {
            // ---- child: single-threaded, owns a private namespace, never returns ----
            let mut code = 0;

            // 1. A private mount namespace, via a user namespace so an unprivileged uid holds
            //    CAP_SYS_ADMIN inside it and nowhere else.
            if unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) } != 0 {
                unsafe { libc::_exit(10) };
            }

            // 2. Map this uid/gid one-to-one, so the staged tree's ownership still reads normally.
            //    `setgroups` must be denied before `gid_map` becomes writable at all.
            if std::fs::write("/proc/self/setgroups", "deny").is_err()
                || std::fs::write("/proc/self/uid_map", format!("{uid} {uid} 1")).is_err()
                || std::fs::write("/proc/self/gid_map", format!("{gid} {gid} 1")).is_err()
            {
                unsafe { libc::_exit(11) };
            }

            // 3. Private propagation, so nothing below leaks back into the host mount table.
            if unsafe {
                libc::mount(
                    std::ptr::null(),
                    c"/".as_ptr(),
                    std::ptr::null(),
                    libc::MS_REC | libc::MS_PRIVATE,
                    std::ptr::null(),
                )
            } != 0
            {
                unsafe { libc::_exit(12) };
            }

            // 4. The call under test. Nothing above it is `pivot_root`, the enforcement pipeline,
            //    or `crate::sealed` — the helper takes a plain directory as its root.
            match bind_mount_staged_runtimes(root.path(), &grants) {
                Ok(staged) => {
                    // The target is the source path re-based under the root, not a flattened name.
                    if staged != vec![expected_target.clone()] {
                        code = 14;
                    }
                }
                Err(_) => code = 13,
            }

            // 5a. Reading *through the target root* sees the source tree's contents. This is what
            //     proves a bind happened rather than an empty directory having been created.
            if code == 0
                && std::fs::read_to_string(&staged_marker).ok().as_deref()
                    != Some("staged-runtime-contents")
            {
                code = 15;
            }

            // 5b. Writing through the target root is refused, specifically with `EROFS`. A
            //     permission error would mean only that this uid cannot write the tree, which is a
            //     much weaker property than the mount being read-only.
            if code == 0
                && std::fs::write(&staged_marker, "tampered")
                    .err()
                    .and_then(|e| e.raw_os_error())
                    != Some(libc::EROFS)
            {
                code = 16;
            }
            if code == 0
                && std::fs::write(expected_target.join("new-file"), "nope")
                    .err()
                    .and_then(|e| e.raw_os_error())
                    != Some(libc::EROFS)
            {
                code = 17;
            }

            // 5c. The source itself stays writable: the read-only flag belongs to the new mount,
            //     not to the underlying tree.
            if code == 0 && std::fs::write(&marker, "still-writable-at-source").is_err() {
                code = 18;
            }

            unsafe { libc::_exit(code) };
        }

        // ---- parent ----
        let mut status = 0;
        assert_eq!(
            unsafe { libc::waitpid(pid, &mut status, 0) },
            pid,
            "waitpid failed: {}",
            std::io::Error::last_os_error()
        );
        assert!(
            libc::WIFEXITED(status),
            "the mount child did not exit normally (status {status})"
        );
        let code = libc::WEXITSTATUS(status);
        assert_eq!(code, 0, "mount child failed: {}", child_exit_meaning(code));

        // The child's mounts were confined to its namespace, so the host still sees an ordinary
        // empty directory here. Asserted from the parent because it is the one claim the child
        // structurally cannot make about itself.
        assert!(
            !staged_marker.exists(),
            "the staged bind mount leaked out of the child's mount namespace"
        );
    }
}
