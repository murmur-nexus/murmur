//! The case registry — every case the roadmap's minimum case set names, with its expected
//! verdict per containment class.
//!
//! # There is no disable mechanism, deliberately
//!
//! [`REGISTRY`] is a flat `const` slice. There is no `enabled` flag, no filter file, no
//! environment variable that drops a case, and [`ALL_CASES`]/[`BOUNDARY_CASE_COUNT`]/
//! [`RESOURCE_CASE_COUNT`] are asserted against it by the tests at the bottom of this file. The
//! roadmap card requires `exec-renamed-disallowed-binary` to be permanent; the way that is held
//! is that removing it changes a count assertion, so the deletion cannot land without appearing
//! in a diff review. [`PERMANENT_CASE_IDS`] pins the same property by name.
//!
//! `--only` exists on the CLI for iterating on one case by hand, but a `--only` run is stamped
//! `PARTIAL RUN` in the record and cannot be cited as evidence — see `record.rs`.
//!
//! # How a class's expectations are chosen
//!
//! * **`scoped`** is the class this suite exists to assert. Every boundary case is
//!   `Must(Refused)` except `stat-outside-workdir`, which is `Must(Succeeded)`.
//! * **`advisory`** is convention-only: `achieved_class_for_tier` maps both `EnvironmentOnly` and
//!   `KernelSeccompOnly` to it, so it can promise nothing about the filesystem and cannot promise
//!   seccomp either. Kernel-mediated cases are therefore [`Expectation::NotAsserted`] — they
//!   still run and are still recorded, but a class that provides no mechanism cannot be graded on
//!   one. Cases that hold *without* any kernel mediation (metadata visibility, the per-process
//!   `setrlimit` ceilings, the periodic workdir check) stay asserted at `advisory`.
//! * **`sealed`** is reachable now — a Linux host with a usable Landlock ABI, unprivileged user
//!   namespaces and the shipped `mur-sealed` AppArmor profile resolves to
//!   `EnforcementTier::KernelSealed` — but its column is still [`Expectation::Documented`]: the
//!   verdicts run and are recorded in full, and gate nothing, because nobody has yet validated
//!   them against a real composed root. See that variant's doc comment, and
//!   `docs/content/reference/sealed-containment-manual-verification.md`.

use crate::verdict::{Category, Expectation, Verdict};
use murmur_artifact::ContainmentClass;

use Expectation::{Documented, Must, NotAsserted};
use Verdict::{Contained, Refused, Succeeded};

/// Work the harness performs on the host, before `mur run`, to set a case up.
///
/// These steps run *outside* the sandbox on purpose. `exec-renamed-disallowed-binary` needs a
/// disallowed binary to already exist inside the workdir under an allowlisted basename; a capsule
/// that could stage that for itself would first have to read a binary it has no grant on, so the
/// case would fail during setup for a reason unrelated to the property under test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prepare {
    /// Nothing beyond staging the probe.
    None,
    /// Copy the first readable path in `sources` into the case workdir as `dest`, mode 0755.
    CopyBinaryAs {
        sources: &'static [&'static str],
        dest: &'static str,
    },
    /// Launch `mur` from a shell that has already opened `path` on fd 7 without `FD_CLOEXEC`,
    /// mirroring `subprocess-fd-hygiene-verification.md` step 4 exactly.
    LeakFdIntoMur { fd: u32, path: &'static str },
}

/// Where a case's verdict is read from once the capsule has run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Evidence {
    /// The `VERDICT=`/`DETAIL=` lines the probe wrote into the capsule workdir. Missing file =
    /// `INCONCLUSIVE`, never a pass.
    ProbeFile,
    /// A **second** shell tool call, made after the case body, must be refused, and its refusal
    /// text must contain this needle.
    ///
    /// The workdir ceiling is not a session kill. `ShellEnforcement::check_workdir_budget` latches
    /// the periodic check's breach and refuses the *next* `Command::spawn()`, so the mechanism is
    /// only observable if there is a later spawn for it to refuse. A case that made one tool call
    /// would report UNCONTAINED against a ceiling that works perfectly.
    SecondSpawnRefused(&'static str),
    /// The shell tool's own exit code, as the tool result reported it. Used only where the
    /// expected outcome is that the probe is *killed* and so cannot write a verdict file — an
    /// OOM kill is a SIGKILL, and a probe that could report it would not have been killed.
    /// `contained` lists the exit codes that mean the ceiling bit (the runtime keeps a signal
    /// legible as `128 + signo`).
    ShellExit { contained: &'static [i32] },
}

/// Which manifest shape a case is launched under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// Default resource ceilings. Every boundary case: the ceilings are not what is under test
    /// and a tight one could kill a probe before it reports.
    Boundary,
    /// The deliberately tight `capabilities.resources` block from
    /// `resource-limits-manual-verification.md`'s test capsule, so each ceiling trips in seconds.
    TightResources,
}

/// One conformance case.
#[derive(Debug, Clone, Copy)]
pub struct Case {
    /// Stable identifier. Appears in the stdout verdict line, the record's per-case table and
    /// `--only`. Never renamed without a note in the build summary.
    pub id: &'static str,
    pub category: Category,
    /// One line, present tense: what the case does.
    pub summary: &'static str,
    /// What a reader must know to interpret this case's verdict correctly — which mechanism a
    /// refusal is attributable to, and where attribution is ambiguous on this host.
    pub attribution: &'static str,
    pub advisory: Expectation,
    pub scoped: Expectation,
    pub sealed: Expectation,
    pub prepare: Prepare,
    pub evidence: Evidence,
    pub profile: Profile,
    /// The Python body. Must define `main()`; see `probe.rs` for the helpers in scope.
    pub body: &'static str,
}

impl Case {
    /// This case's expectation under `class`.
    pub fn expectation(&self, class: ContainmentClass) -> Expectation {
        match class {
            ContainmentClass::Advisory => self.advisory,
            ContainmentClass::Scoped => self.scoped,
            ContainmentClass::Sealed => self.sealed,
        }
    }
}

/// A trivial capsule run, used only to answer "can a probe start at all on this host?".
///
/// Deliberately **not** in [`REGISTRY`]: it asserts nothing about containment, and counting it
/// would corrupt the roadmap's minimum-case-set arithmetic. It exists because the alternative is
/// far worse than a missing case. If the shell-tool path cannot execute anything — a cgroup that
/// cannot be joined, an interpreter the exec allowlist will not resolve, a supervisor that cannot
/// read the child's memory — then every case reports `INCONCLUSIVE`, every asserted case fails,
/// and the run looks like twenty-eight escapes when the truth is that nothing was measured.
/// Refusing on that, before any case runs and without writing a record, is the same discipline
/// the containment-class gate follows.
pub const PREFLIGHT: Case = Case {
    id: "preflight",
    category: Category::Boundary,
    summary: "starts the interpreter inside a capsule and confirms a probe can report at all",
    attribution: "Infrastructure check, not a containment case. It asserts only that the shell \
                  tool path is live on this host.",
    advisory: Must(Succeeded),
    scoped: Must(Succeeded),
    sealed: Must(Succeeded),
    prepare: Prepare::None,
    evidence: Evidence::ProbeFile,
    profile: Profile::Boundary,
    body: r#"
def main():
    verdict("SUCCESS", "python3 started inside the capsule as uid %d in %s -- the shell tool path "
                       "is live, so a case that reports nothing is reporting about containment "
                       "and not about this harness" % (os.geteuid(), os.getcwd()))
"#,
};

/// Cases that must never be removed. Checked by [`registry_keeps_permanent_cases`] so a deletion
/// fails this package's own `cargo test` rather than quietly shrinking the suite.
pub const PERMANENT_CASE_IDS: &[&str] = &["exec-renamed-disallowed-binary"];

/// The roadmap's minimum case set: 23 boundary cases.
pub const BOUNDARY_CASE_COUNT: usize = 23;
/// The roadmap's minimum case set: 5 resource-exhaustion cases.
pub const RESOURCE_CASE_COUNT: usize = 5;

/// Every case, boundary first. Order is the order they run and the order they are reported.
pub const REGISTRY: &[Case] = &[
    // ── Filesystem boundary ───────────────────────────────────────────────────────────────
    Case {
        id: "read-etc-shadow",
        category: Category::Boundary,
        summary: "opens /etc/shadow for reading from inside the capsule workdir",
        attribution: "Ambiguous as a lone signal on a non-root run: /etc/shadow is mode 0640 \
                      root:shadow, so an ordinary uid is refused with or without a sandbox. The \
                      probe therefore also reads /etc/passwd — world-readable, outside the \
                      workdir, covered by no grant. /etc/passwd refused too ⇒ Landlock is \
                      genuinely mediating. /etc/passwd readable ⇒ the /etc/shadow refusal is \
                      ordinary file permissions and proves nothing about containment.",
        advisory: NotAsserted,
        scoped: Must(Refused),
        sealed: Documented(Refused),
        prepare: Prepare::None,
        evidence: Evidence::ProbeFile,
        profile: Profile::Boundary,
        body: r#"
def main():
    fd, note = probe_open("/etc/shadow")
    ctl_fd, ctl_note = probe_open("/etc/passwd")
    control = "control /etc/passwd (world-readable, no grant): "
    if ctl_fd is not None:
        os.close(ctl_fd)
        control += "READABLE -- no filesystem mediation on this host"
    else:
        control += "refused -- filesystem mediation is active"
    if fd is not None:
        head = os.read(fd, 32)
        os.close(fd)
        verdict("ALLOWED", "read %d bytes of /etc/shadow; %s; %s" % (len(head), note, control))
    else:
        verdict("REFUSED", "%s ; %s ; %s" % (note, ctl_note, control))
"#,
    },
    Case {
        id: "write-outside-workdir",
        category: Category::Boundary,
        summary: "creates a file in /tmp, outside the capsule workdir",
        attribution: "Unambiguous. /tmp is world-writable, so ordinary file permissions permit \
                      this for any uid; only kernel filesystem mediation can refuse it. A \
                      REFUSED verdict here is attributable to Landlock and nothing else. If the \
                      verdict is ALLOWED the created path is named in DETAIL so it can be \
                      removed by hand.",
        advisory: NotAsserted,
        scoped: Must(Refused),
        sealed: Documented(Refused),
        prepare: Prepare::None,
        evidence: Evidence::ProbeFile,
        profile: Profile::Boundary,
        body: r#"
def main():
    target = "/tmp/murmur-escape-conformance-%d" % os.getpid()
    try:
        fd = os.open(target, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        os.write(fd, b"escaped\n")
        os.close(fd)
        verdict("ALLOWED", "wrote outside the workdir: %s (remove it by hand)" % target)
    except OSError as exc:
        verdict("REFUSED", "create %s refused with %s" % (target, en(exc)))
"#,
    },
    Case {
        id: "stat-outside-workdir",
        category: Category::Boundary,
        summary: "stat()s /etc/shadow — a metadata-only query with no read grant",
        attribution: "This case asserts SUCCESS, not refusal, and a SUCCESS verdict here is NOT \
                      a leaked finding. Landlock mediates open/read/write and the create-class \
                      actions; it does not mediate bare metadata queries at any ABI. Metadata \
                      visibility is a documented property of the `scoped` class, and a harness \
                      that reported it as a failure would be asserting something false about the \
                      class it is certifying. It is asserted at `advisory` too, where nothing is \
                      mediated at all. Only `sealed` would change the answer, by putting the \
                      path outside the capsule's mount namespace entirely.",
        advisory: Must(Succeeded),
        scoped: Must(Succeeded),
        sealed: Documented(Refused),
        prepare: Prepare::None,
        evidence: Evidence::ProbeFile,
        profile: Profile::Boundary,
        body: r#"
def main():
    try:
        st = os.stat("/etc/shadow")
    except OSError as exc:
        verdict("REFUSED", "stat(/etc/shadow) refused with %s -- unexpected: Landlock does not "
                           "mediate metadata-only syscalls" % en(exc))
        return
    verdict("SUCCESS", "stat(/etc/shadow) succeeded: size=%d mode=%o uid=%d -- metadata visible "
                       "without any read grant, which is the documented behaviour of the scoped "
                       "class, not an escape" % (st.st_size, st.st_mode, st.st_uid))
"#,
    },
    Case {
        id: "symlink-escape",
        category: Category::Boundary,
        summary: "creates a symlink in the workdir pointing at /etc/passwd and reads through it",
        attribution: "Landlock resolves symlinks before applying its rules, so the read is \
                      checked against the *target* path. Creating the link is expected to \
                      succeed (MakeSym is granted on the workdir); only the read through it is \
                      the boundary. DETAIL reports the two steps separately so a create failure \
                      is never mistaken for a contained read.",
        advisory: NotAsserted,
        scoped: Must(Refused),
        sealed: Documented(Refused),
        prepare: Prepare::None,
        evidence: Evidence::ProbeFile,
        profile: Profile::Boundary,
        body: r#"
def main():
    link = "./ec-symlink"
    try:
        os.symlink("/etc/passwd", link)
        created = "symlink created"
    except FileExistsError:
        created = "symlink already present"
    except OSError as exc:
        verdict("INCONCLUSIVE", "could not create the symlink at all: %s -- the read through it "
                                "was never attempted" % en(exc))
        return
    fd, note = probe_open(link)
    if fd is not None:
        data = os.read(fd, 32)
        os.close(fd)
        verdict("ALLOWED", "%s; read %d bytes of /etc/passwd through it" % (created, len(data)))
    else:
        verdict("REFUSED", "%s; read through it refused: %s" % (created, note))
"#,
    },
    Case {
        id: "hardlink-escape",
        category: Category::Boundary,
        summary: "hard-links /etc/passwd into the workdir and reads the link",
        attribution: "Three refusal causes must be told apart and DETAIL names which one fired: \
                      EXDEV means the workdir and /etc are on different filesystems, so the link \
                      was impossible for a reason unrelated to containment (INCONCLUSIVE, not a \
                      pass); EPERM may be the kernel's protected_hardlinks sysctl rather than \
                      Landlock; EACCES is Landlock. A hard link, unlike a symlink, would alias \
                      the inode itself and survive any later path check, which is why it is a \
                      separate case.",
        advisory: NotAsserted,
        scoped: Must(Refused),
        sealed: Documented(Refused),
        prepare: Prepare::None,
        evidence: Evidence::ProbeFile,
        profile: Profile::Boundary,
        body: r#"
def main():
    link = "./ec-hardlink"
    protected = "unknown"
    fd, _ = probe_open("/proc/sys/fs/protected_hardlinks")
    if fd is not None:
        protected = os.read(fd, 8).decode("ascii", "replace").strip()
        os.close(fd)
    try:
        os.link("/etc/passwd", link)
    except FileExistsError:
        pass
    except OSError as exc:
        if exc.errno == errno.EXDEV:
            verdict("INCONCLUSIVE", "EXDEV: the workdir and /etc are on different filesystems, "
                                    "so this host cannot attempt the hard link at all "
                                    "(protected_hardlinks=%s)" % protected)
        else:
            verdict("REFUSED", "link(/etc/passwd) refused with %s (protected_hardlinks=%s -- "
                               "EPERM may be that sysctl rather than Landlock)"
                               % (en(exc), protected))
        return
    fd, note = probe_open(link)
    if fd is not None:
        data = os.read(fd, 32)
        os.close(fd)
        verdict("ALLOWED", "hard link created and read: %d bytes of /etc/passwd aliased into the "
                           "workdir" % len(data))
    else:
        verdict("REFUSED", "hard link created but unreadable: %s" % note)
"#,
    },
    Case {
        id: "rename-across-boundary",
        category: Category::Boundary,
        summary: "renames a workdir file to a path in /tmp, outside the workdir",
        attribution: "EXDEV means the two paths are on different filesystems and rename(2) could \
                      never have worked here regardless of containment — recorded as \
                      INCONCLUSIVE rather than credited as a refusal. Anything else is \
                      attributable: rename is a create-class action on the destination, which \
                      Landlock does mediate.",
        advisory: NotAsserted,
        scoped: Must(Refused),
        sealed: Documented(Refused),
        prepare: Prepare::None,
        evidence: Evidence::ProbeFile,
        profile: Profile::Boundary,
        body: r#"
def main():
    src = "./ec-rename-src"
    dst = "/tmp/murmur-escape-conformance-rename-%d" % os.getpid()
    with open(src, "wb") as fh:
        fh.write(b"escaped\n")
    try:
        os.rename(src, dst)
    except OSError as exc:
        if exc.errno == errno.EXDEV:
            verdict("INCONCLUSIVE", "EXDEV: the workdir and /tmp are on different filesystems, "
                                    "so rename(2) across this boundary is impossible here for "
                                    "reasons unrelated to containment")
        else:
            verdict("REFUSED", "rename %s -> %s refused with %s" % (src, dst, en(exc)))
        return
    verdict("ALLOWED", "renamed a workdir file to %s, outside the workdir (remove it by hand)" % dst)
"#,
    },
    // ── /proc re-open routes ──────────────────────────────────────────────────────────────
    Case {
        id: "proc-self-cwd-reopen",
        category: Category::Boundary,
        summary: "re-opens the capsule workdir through /proc/self/cwd and walks out of it",
        attribution: "Graded on the walk *out*, not on the re-open. `/proc/self/cwd` is a magic \
                      symlink, and Landlock applies its rules to the resolved target — so \
                      re-opening it succeeds whenever the workdir itself is granted, which it \
                      always is. That is not a boundary crossing and must not be scored as one; \
                      the crossing is reaching a path outside the workdir through it, which is \
                      what this case asserts. The re-open result and the `proc_reachable()` \
                      control (ordinary /proc files carry no grant and are expected to be EACCES) \
                      are both reported in DETAIL as context — the same distinction \
                      subprocess-fd-hygiene-verification.md's 'Why not /proc/self/fd' section \
                      exists to make.",
        advisory: NotAsserted,
        scoped: Must(Refused),
        sealed: Documented(Refused),
        prepare: Prepare::None,
        evidence: Evidence::ProbeFile,
        profile: Profile::Boundary,
        body: r#"
def main():
    control = proc_reachable()
    fd, reopen = probe_open("/proc/self/cwd", os.O_RDONLY | os.O_DIRECTORY)
    if fd is not None:
        os.close(fd)
        reopen += " (re-opening the workdir through the magic symlink is expected: Landlock "
        reopen += "resolves it to the granted workdir, which is not a crossing)"

    # The boundary is here, not above: can the magic symlink be used as a base to reach a path
    # the ruleset does not grant?
    escaped, esc_note = probe_open("/proc/self/cwd/../../../../../../etc/passwd")
    if escaped is not None:
        data = os.read(escaped, 32)
        os.close(escaped)
        verdict("ALLOWED", "walked out of the workdir through /proc/self/cwd and read %d bytes of "
                           "/etc/passwd ; %s ; %s" % (len(data), reopen, control))
        return
    verdict("REFUSED", "the walk out of the workdir was refused: %s ; %s ; %s"
                       % (esc_note, reopen, control))
"#,
    },
    Case {
        id: "proc-pid-root-reopen",
        category: Category::Boundary,
        summary: "reads /etc/passwd through /proc/<pid>/root, the filesystem-root alias",
        attribution: "Same /proc-reachability control as the other two /proc cases. /proc/<pid>/root \
                      is the alias that would defeat a chroot-shaped boundary; against a Landlock \
                      ruleset it is only another path, and it is expected to be refused because \
                      /proc carries no grant.",
        advisory: NotAsserted,
        scoped: Must(Refused),
        sealed: Documented(Refused),
        prepare: Prepare::None,
        evidence: Evidence::ProbeFile,
        profile: Profile::Boundary,
        body: r#"
def main():
    control = proc_reachable()
    path = "/proc/%d/root/etc/passwd" % os.getpid()
    fd, note = probe_open(path)
    if fd is None:
        verdict("REFUSED", "%s ; %s" % (note, control))
        return
    data = os.read(fd, 32)
    os.close(fd)
    verdict("ALLOWED", "read %d bytes of /etc/passwd through %s ; %s" % (len(data), path, control))
"#,
    },
    Case {
        id: "proc-self-fd-reopen",
        category: Category::Boundary,
        summary: "re-opens an already-open descriptor through /proc/self/fd/N",
        attribution: "The /proc/self/fd/N route matters because it can *upgrade* an access mode, \
                      so the base descriptor has to be something the capsule holds under a \
                      narrower grant than it is asking for. A workdir file is useless here — the \
                      workdir is already writable, so an O_RDWR re-open widens nothing and \
                      scoring it as an escape would be a false positive. The probe therefore \
                      bases on /dev/urandom, which CAPSULE_DEVICE_GRANTS grants **read-only**: a \
                      successful O_RDWR re-open would convert a read-only device grant into a \
                      writable one, which is a real crossing. If /dev/urandom cannot be opened at \
                      all the case is INCONCLUSIVE rather than passed.",
        advisory: NotAsserted,
        scoped: Must(Refused),
        sealed: Documented(Refused),
        prepare: Prepare::None,
        evidence: Evidence::ProbeFile,
        profile: Profile::Boundary,
        body: r#"
def main():
    control = proc_reachable()
    # /dev/urandom is granted read-only by the fixed capsule device set. Widening that grant is
    # what this case is actually about.
    base, base_note = probe_open("/dev/urandom")
    if base is None:
        verdict("INCONCLUSIVE", "could not open the read-only base descriptor, so no upgrade was "
                                "attempted: %s ; %s" % (base_note, control))
        return
    path = "/proc/self/fd/%d" % base
    fd, note = probe_open(path, os.O_RDWR)
    if fd is not None:
        os.close(fd)
        os.close(base)
        verdict("ALLOWED", "re-opened the read-only /dev/urandom grant as O_RDWR through %s -- a "
                           "read-only device grant was upgraded to writable ; %s" % (path, control))
        return
    ro_fd, ro_note = probe_open(path)
    if ro_fd is not None:
        os.close(ro_fd)
    os.close(base)
    verdict("REFUSED", "the O_RDWR upgrade through %s was refused (%s); the same-mode re-open %s "
                       "; %s" % (path, note, "succeeded" if ro_fd is not None else ro_note, control))
"#,
    },
    Case {
        id: "inherited-fd-after-exec",
        category: Category::Boundary,
        summary: "uses file descriptor 7, leaked into `mur` by the launching shell, after exec",
        attribution: "Enumerated with fcntl(fd, F_GETFD), never /proc/self/fd — the fd-hygiene \
                      document records that a /proc-based enumeration under KernelFull reports \
                      nothing and is indistinguishable from a clean result. The harness runs \
                      `mur` from a shell that has already done `exec 7</etc/hostname`, exactly as \
                      that document's step 4 does, so a leak would be real rather than \
                      hypothetical. Fds 0/1/2 are excluded by design and are not a leak.",
        advisory: NotAsserted,
        scoped: Must(Refused),
        sealed: Documented(Refused),
        prepare: Prepare::LeakFdIntoMur {
            fd: 7,
            path: "/etc/hostname",
        },
        evidence: Evidence::ProbeFile,
        profile: Profile::Boundary,
        body: r#"
def main():
    import fcntl
    extra = []
    limit = min(os.sysconf("SC_OPEN_MAX"), 4096)
    for fd in range(3, limit):
        try:
            fcntl.fcntl(fd, fcntl.F_GETFD)
        except OSError:
            continue
        extra.append(fd)
    if 7 not in extra:
        verdict("REFUSED", "fd 7 is not open in the sandboxed child (open fds above stdio: %s); "
                           "the descriptor the launching shell leaked into mur did not cross the "
                           "exec boundary" % (extra or "none"))
        return
    try:
        head = os.pread(7, 64, 0)
    except OSError as exc:
        head = b"<unreadable: %s>" % en(exc).encode()
    verdict("ALLOWED", "fd 7 is open inside the sandboxed child and reads %r -- a descriptor "
                       "opened before the Landlock ruleset existed reads straight through the "
                       "workdir scope (all extra fds: %s)" % (head[:48], extra))
"#,
    },
    // ── Device nodes and exec identity ────────────────────────────────────────────────────
    Case {
        id: "mknod-block-device-in-workdir",
        category: Category::Boundary,
        summary: "mknod()s a block device for the host disk inside the workdir, then reads it",
        attribution: "Two independent mechanisms close this and DETAIL says which one answered. \
                      EACCES is the Landlock workdir grant withholding MakeBlock \
                      (WORKDIR_ACCESS_RIGHTS). EPERM is the missing CAP_MKNOD — which an \
                      ordinary uid never had, so on a non-root run this case cannot distinguish \
                      the fix from the pre-fix state. Re-run as root to exercise the deployment \
                      shape that was actually exposed; security-warnings.md scenario 4 records \
                      the same caveat. This is the case the negative control flips: restore \
                      MakeBlock to WORKDIR_ACCESS_RIGHTS, rebuild, and it must go ALLOWED.",
        advisory: NotAsserted,
        scoped: Must(Refused),
        sealed: Documented(Refused),
        prepare: Prepare::None,
        evidence: Evidence::ProbeFile,
        profile: Profile::Boundary,
        body: r#"
def main():
    node = "./ec-pwn"
    root_run = (os.geteuid() == 0)
    try:
        os.mknod(node, 0o600 | stat.S_IFBLK, os.makedev(8, 0))
    except OSError as exc:
        which = "Landlock (workdir grant withholds MakeBlock)" if exc.errno == errno.EACCES \
                else "CAP_MKNOD missing" if exc.errno == errno.EPERM \
                else "unexpected"
        note = "" if root_run else " -- NOTE: non-root run, so EPERM proves nothing about the " \
                                   "Landlock fix; re-run as root for an attributable result"
        verdict("REFUSED", "mknod(%s, blk 8:0) refused with %s: %s%s"
                           % (node, en(exc), which, note))
        return
    fd, note = probe_open(node)
    if fd is None:
        verdict("ALLOWED", "the block device node was CREATED inside the workdir (%s), though "
                           "opening it was refused: %s" % (node, note))
        return
    data = os.read(fd, 512)
    os.close(fd)
    verdict("ALLOWED", "created a block device node for major 8 minor 0 inside the workdir and "
                       "read %d raw bytes off it: %s -- this is the raw-disk escape"
                       % (len(data), data[:16].hex()))
"#,
    },
    Case {
        id: "exec-renamed-disallowed-binary",
        category: Category::Boundary,
        summary: "execs a copy of a disallowed binary placed in the workdir under an allowlisted \
                  basename",
        attribution: "PERMANENT CASE — see PERMANENT_CASE_IDS. This is the v0.5.7 regression: the \
                      exec allowlist resolves the requested binary to a canonical path before \
                      comparing, so a copy with an allowlisted basename must still be refused. \
                      `sandbox::tests::decide_exec_denies_renamed_copy_with_allowlisted_basename` \
                      pins the pure function; this case pins the same shape against the real \
                      kernel enforcement. A PermissionError from execve is the refusal; an ENOENT \
                      means the harness's own staging failed and is INCONCLUSIVE, never a pass. \
                      This is not a TOCTOU probe — it is single-threaded and does not race; the \
                      exec/connect race class is separately audited and lives in racecheck/.",
        advisory: NotAsserted,
        scoped: Must(Refused),
        sealed: Documented(Refused),
        prepare: Prepare::CopyBinaryAs {
            // First readable wins. All three are outside `shell.allow`, so each is a disallowed
            // identity wearing an allowlisted basename once copied.
            sources: &["/bin/ls", "/usr/bin/ls", "/bin/echo", "/usr/bin/echo"],
            dest: "bash",
        },
        evidence: Evidence::ProbeFile,
        profile: Profile::Boundary,
        body: r#"
def main():
    target = "./bash"
    if not os.path.exists(target):
        verdict("INCONCLUSIVE", "the harness did not stage %s -- nothing was exercised" % target)
        return
    try:
        proc = subprocess.run([target, "--version"], capture_output=True, timeout=20)
    except PermissionError as exc:
        verdict("REFUSED", "execve(%s) refused with %s -- a disallowed binary wearing the "
                           "allowlisted basename 'bash' was resolved to its real path and denied"
                           % (target, en(exc)))
        return
    except OSError as exc:
        if getattr(exc, "errno", None) == errno.ENOENT:
            verdict("INCONCLUSIVE", "execve(%s) failed with ENOENT -- staging problem, not a "
                                    "containment result" % target)
        else:
            verdict("REFUSED", "execve(%s) refused with %s" % (target, en(exc)))
        return
    except subprocess.TimeoutExpired:
        verdict("INCONCLUSIVE", "execve(%s) neither ran nor was refused within 20s" % target)
        return
    out = (proc.stdout or b"")[:60] + (proc.stderr or b"")[:60]
    verdict("ALLOWED", "execve(%s) SUCCEEDED (rc=%d, output %r) -- a disallowed binary ran "
                       "because its basename was allowlisted" % (target, proc.returncode, out))
"#,
    },
    // ── Network boundary ──────────────────────────────────────────────────────────────────
    Case {
        id: "connect-unlisted-tcp-host",
        category: Category::Boundary,
        summary: "connects to 1.1.1.1:443, a host named nowhere in capabilities.network.allow",
        attribution: "The capsule declares an empty network.allow, so every destination is \
                      unlisted. EPERM/EACCES from connect(2) is the seccomp-notify supervisor's \
                      denial. A timeout or ENETUNREACH means the host itself has no route and \
                      the case proved nothing — INCONCLUSIVE, not a pass. Single-threaded and \
                      non-racing: the TOCTOU class documented in seccomp-notify-toctou-audit.md \
                      is a different probe and is out of scope here.",
        advisory: NotAsserted,
        scoped: Must(Refused),
        sealed: Documented(Refused),
        prepare: Prepare::None,
        evidence: Evidence::ProbeFile,
        profile: Profile::Boundary,
        body: r#"
def main():
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.settimeout(8)
    try:
        sock.connect(("1.1.1.1", 443))
    except socket.timeout:
        verdict("INCONCLUSIVE", "connect(1.1.1.1:443) timed out -- no route from this host, so "
                                "nothing about the network boundary was exercised")
        return
    except OSError as exc:
        if getattr(exc, "errno", None) in (errno.ENETUNREACH, errno.EHOSTUNREACH):
            verdict("INCONCLUSIVE", "connect(1.1.1.1:443) got %s -- the host has no route, so "
                                    "nothing was exercised" % en(exc))
        else:
            verdict("REFUSED", "connect(1.1.1.1:443) refused with %s" % en(exc))
        return
    finally:
        try:
            sock.close()
        except OSError:
            pass
    verdict("ALLOWED", "connected to 1.1.1.1:443 with an empty network.allow")
"#,
    },
    Case {
        id: "udp-exfiltration",
        category: Category::Boundary,
        summary: "sendto()s a UDP datagram to an unlisted host",
        attribution: "UDP is a separate case from TCP because it never calls connect(2): the \
                      destination rides on sendto(2), which the supervisor mediates through a \
                      different rule. sendto succeeding is exfiltration even though no \
                      connection was ever established, and a datagram send cannot be refused \
                      after the fact.",
        advisory: NotAsserted,
        scoped: Must(Refused),
        sealed: Documented(Refused),
        prepare: Prepare::None,
        evidence: Evidence::ProbeFile,
        profile: Profile::Boundary,
        body: r#"
def main():
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.settimeout(8)
    try:
        sent = sock.sendto(b"murmur-escape-conformance", ("1.1.1.1", 53))
    except OSError as exc:
        if getattr(exc, "errno", None) in (errno.ENETUNREACH, errno.EHOSTUNREACH):
            verdict("INCONCLUSIVE", "sendto got %s -- the host has no route, nothing exercised"
                                    % en(exc))
        else:
            verdict("REFUSED", "sendto(1.1.1.1:53) refused with %s" % en(exc))
        return
    finally:
        try:
            sock.close()
        except OSError:
            pass
    verdict("ALLOWED", "sent %d bytes by UDP to an unlisted host without any connect(2)" % sent)
"#,
    },
    Case {
        id: "dns-exfiltration",
        category: Category::Boundary,
        summary: "encodes data into a DNS lookup for an attacker-controlled name",
        attribution: "DNS is the classic covert channel: the payload rides in the queried label, \
                      so a resolver reachable at all is an exfiltration route regardless of \
                      whether any answer comes back. NXDOMAIN therefore counts as ALLOWED — the \
                      query left the host. Only a refusal before the query leaves is containment. \
                      EAI_* failures that are not refusals are reported verbatim in DETAIL.",
        advisory: NotAsserted,
        scoped: Must(Refused),
        sealed: Documented(Refused),
        prepare: Prepare::None,
        evidence: Evidence::ProbeFile,
        profile: Profile::Boundary,
        body: r#"
def main():
    name = "ec-exfil-%d.murmur-escape-conformance.invalid" % os.getpid()
    try:
        info = socket.getaddrinfo(name, 53, proto=socket.IPPROTO_UDP)
    except socket.gaierror as exc:
        code = getattr(exc, "errno", None)
        if code in (socket.EAI_NONAME, getattr(socket, "EAI_NODATA", socket.EAI_NONAME)):
            verdict("ALLOWED", "the lookup for %s reached a resolver and came back NXDOMAIN "
                               "(%s) -- the queried label, and any payload in it, left the host"
                               % (name, exc))
        else:
            verdict("REFUSED", "resolution of %s failed before leaving the host: %s" % (name, exc))
        return
    except OSError as exc:
        verdict("REFUSED", "resolution of %s refused with %s" % (name, en(exc)))
        return
    verdict("ALLOWED", "resolved %s to %s" % (name, [i[4] for i in info][:2]))
"#,
    },
    Case {
        id: "abstract-unix-socket-connect",
        category: Category::Boundary,
        summary: "connects to an abstract-namespace AF_UNIX socket",
        attribution: "The rule keys on socket(2)'s `domain` argument, not on the socket's \
                      namespace, so the abstract and pathname forms are refused identically and \
                      at the same point — socket() creation, never connect(). A REFUSED verdict \
                      whose DETAIL says the refusal came from connect() rather than socket() \
                      would mean the mechanism is not the one W-SEC-005 describes, so the probe \
                      reports which call raised. Landlock cannot substitute: ABI v6's \
                      LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET scopes abstract sockets only.",
        advisory: NotAsserted,
        scoped: Must(Refused),
        sealed: Documented(Refused),
        prepare: Prepare::None,
        evidence: Evidence::ProbeFile,
        profile: Profile::Boundary,
        body: r#"
def main():
    try:
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    except OSError as exc:
        verdict("REFUSED", "socket(AF_UNIX) itself refused with %s -- refused at socket(), not "
                           "at connect(), which is the mechanism W-SEC-005 describes" % en(exc))
        return
    addr = "\0murmur-escape-conformance-abstract"
    try:
        sock.connect(addr)
    except OSError as exc:
        verdict("REFUSED", "socket(AF_UNIX) SUCCEEDED and the abstract connect was refused later "
                           "with %s -- note this is connect-time, not the socket()-time domain "
                           "rule" % en(exc))
        return
    finally:
        try:
            sock.close()
        except OSError:
            pass
    verdict("ALLOWED", "created an AF_UNIX socket and connected to the abstract name %r" % addr)
"#,
    },
    Case {
        id: "pathname-unix-socket-connect",
        category: Category::Boundary,
        summary: "connects to /var/run/docker.sock and /run/docker.sock",
        attribution: "This is the full-escape case: the Docker daemon socket is host root, and \
                      reaching it needs no manifest declaration of any kind. Expected refusal is \
                      at socket() creation, before either path is tried, because \
                      capabilities.network.unix_sockets defaults to false. DETAIL names which \
                      call refused and, if socket() succeeded, which of the two paths existed.",
        advisory: NotAsserted,
        scoped: Must(Refused),
        sealed: Documented(Refused),
        prepare: Prepare::None,
        evidence: Evidence::ProbeFile,
        profile: Profile::Boundary,
        body: r#"
def main():
    try:
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    except OSError as exc:
        verdict("REFUSED", "socket(AF_UNIX) itself refused with %s -- refused at socket(), not "
                           "at connect(), so neither docker.sock path was reachable" % en(exc))
        return
    notes = []
    for path in ("/var/run/docker.sock", "/run/docker.sock"):
        try:
            sock.connect(path)
        except OSError as exc:
            notes.append("%s=%s" % (path, en(exc)))
            continue
        sock.close()
        verdict("ALLOWED", "connected to the host Docker daemon socket at %s -- full sandbox "
                           "escape (%s)" % (path, "; ".join(notes)))
        return
    sock.close()
    verdict("REFUSED", "socket(AF_UNIX) SUCCEEDED; both docker.sock paths refused at connect "
                       "time (%s) -- note this is connect-time, not the socket()-time domain "
                       "rule W-SEC-005 describes" % "; ".join(notes))
"#,
    },
    // ── The dangerous-syscall table ───────────────────────────────────────────────────────
    Case {
        id: "syscall-io-uring-setup",
        category: Category::Boundary,
        summary: "calls io_uring_setup(2), which must stay denied by the seccomp allowlist",
        attribution: "The most important entry in SECCOMP_MUST_STAY_DENIED: io_uring has \
                      historically bypassed LSM path hooks, so leaving it reachable is a route \
                      around the Landlock filesystem boundary itself, not merely a wider syscall \
                      surface. Unambiguous on a non-root host — an ordinary uid may normally \
                      create io_uring instances, so EPERM here is seccomp and nothing else.",
        advisory: NotAsserted,
        scoped: Must(Refused),
        sealed: Documented(Refused),
        prepare: Prepare::None,
        evidence: Evidence::ProbeFile,
        profile: Profile::Boundary,
        body: r#"
def main():
    ret, err, note = raw_syscall("io_uring_setup", 1, 0)
    grade_syscall("io_uring_setup", note, ret, err,
                  "(a permitted call returns EFAULT for the NULL params pointer)")
"#,
    },
    Case {
        id: "syscall-userfaultfd",
        category: Category::Boundary,
        summary: "calls userfaultfd(2), which must stay denied by the seccomp allowlist",
        attribution: "Ambiguous if the host sets vm.unprivileged_userfaultfd=0, which also \
                      returns EPERM to a non-root caller. The probe reads that sysctl when it \
                      can and folds the value into DETAIL, so a reader can tell a seccomp \
                      refusal from a host-policy one.",
        advisory: NotAsserted,
        scoped: Must(Refused),
        sealed: Documented(Refused),
        prepare: Prepare::None,
        evidence: Evidence::ProbeFile,
        profile: Profile::Boundary,
        body: r#"
def main():
    sysctl = "unknown"
    fd, _ = probe_open("/proc/sys/vm/unprivileged_userfaultfd")
    if fd is not None:
        sysctl = os.read(fd, 8).decode("ascii", "replace").strip()
        os.close(fd)
    ret, err, note = raw_syscall("userfaultfd", 0)
    if ret is not None and ret >= 0:
        os.close(ret)
    grade_syscall("userfaultfd", note, ret, err,
                  "(vm.unprivileged_userfaultfd=%s -- if 0, EPERM is also this host's own "
                  "policy and not attributable to seccomp alone)" % sysctl)
"#,
    },
    Case {
        id: "syscall-bpf",
        category: Category::Boundary,
        summary: "calls bpf(2), which must stay denied by the seccomp allowlist",
        attribution: "Ambiguous on a non-root run: bpf(2) requires CAP_BPF or CAP_SYS_ADMIN for \
                      most commands and returns EPERM to an ordinary uid with no seccomp filter \
                      loaded at all. Re-run as root to attribute the refusal to the allowlist. \
                      DETAIL records the euid so this is never read as more than it is.",
        advisory: NotAsserted,
        scoped: Must(Refused),
        sealed: Documented(Refused),
        prepare: Prepare::None,
        evidence: Evidence::ProbeFile,
        profile: Profile::Boundary,
        body: r#"
def main():
    ret, err, note = raw_syscall("bpf", 0, 0, 0)
    grade_syscall("bpf", note, ret, err,
                  "(euid=%d -- non-root bpf(2) is EPERM from CAP_BPF alone, so only a root run "
                  "attributes this to seccomp)" % os.geteuid())
"#,
    },
    Case {
        id: "syscall-open-by-handle-at",
        category: Category::Boundary,
        summary: "calls open_by_handle_at(2), which must stay denied by the seccomp allowlist",
        attribution: "Ambiguous on a non-root run: open_by_handle_at requires \
                      CAP_DAC_READ_SEARCH and returns EPERM to an ordinary uid regardless of \
                      seccomp. It is in the table because with that capability it opens any \
                      inode by handle, ignoring every path-based check including Landlock's. \
                      Root run required for an attributable result.",
        advisory: NotAsserted,
        scoped: Must(Refused),
        sealed: Documented(Refused),
        prepare: Prepare::None,
        evidence: Evidence::ProbeFile,
        profile: Profile::Boundary,
        body: r#"
def main():
    ret, err, note = raw_syscall("open_by_handle_at", -100, 0, 0)
    grade_syscall("open_by_handle_at", note, ret, err,
                  "(euid=%d -- non-root open_by_handle_at is EPERM from the missing "
                  "CAP_DAC_READ_SEARCH, so only a root run attributes this to seccomp)"
                  % os.geteuid())
"#,
    },
    Case {
        id: "syscall-perf-event-open",
        category: Category::Boundary,
        summary: "calls perf_event_open(2), which must stay denied by the seccomp allowlist",
        attribution: "Partly ambiguous: kernel.perf_event_paranoid can make this EACCES or EPERM \
                      for an ordinary uid on its own. The probe reads that sysctl when it can. \
                      EFAULT would mean the call reached its implementation with the NULL attr \
                      pointer, i.e. it was permitted.",
        advisory: NotAsserted,
        scoped: Must(Refused),
        sealed: Documented(Refused),
        prepare: Prepare::None,
        evidence: Evidence::ProbeFile,
        profile: Profile::Boundary,
        body: r#"
def main():
    paranoid = "unknown"
    fd, _ = probe_open("/proc/sys/kernel/perf_event_paranoid")
    if fd is not None:
        paranoid = os.read(fd, 8).decode("ascii", "replace").strip()
        os.close(fd)
    ret, err, note = raw_syscall("perf_event_open", 0, 0, -1, -1, 0)
    grade_syscall("perf_event_open", note, ret, err,
                  "(kernel.perf_event_paranoid=%s -- a positive value also refuses an "
                  "unprivileged caller on its own)" % paranoid)
"#,
    },
    Case {
        id: "syscall-keyctl",
        category: Category::Boundary,
        summary: "calls keyctl(2), which must stay denied by the seccomp allowlist",
        attribution: "Unambiguous on a non-root host: an ordinary uid may normally call keyctl \
                      against its own session keyring, so EPERM here is the seccomp default \
                      action. add_key(2) sits beside it in SECCOMP_MUST_STAY_DENIED and shares \
                      this case's mechanism; the roadmap's minimum set names keyctl, so keyctl is \
                      what is asserted.",
        advisory: NotAsserted,
        scoped: Must(Refused),
        sealed: Documented(Refused),
        prepare: Prepare::None,
        evidence: Evidence::ProbeFile,
        profile: Profile::Boundary,
        body: r#"
def main():
    # KEYCTL_GET_KEYRING_ID (0) against KEY_SPEC_SESSION_KEYRING (-3), no create.
    ret, err, note = raw_syscall("keyctl", 0, -3, 0)
    grade_syscall("keyctl", note, ret, err,
                  "(an ordinary uid may normally read its own session keyring, so EPERM here is "
                  "attributable to seccomp)")
"#,
    },
    // ── Resource exhaustion — availability, never a boundary ──────────────────────────────
    Case {
        id: "resource-fork-bomb",
        category: Category::ResourceExhaustion,
        summary: "forks without bound until the cgroup's pids.max refuses",
        attribution: "DENIAL OF SERVICE, NOT AN ESCAPE. Nothing outside the granted scope is \
                      read, written or reached. The specific claim is the attribution: the trace \
                      must say cgroup_pids_max, not max_processes — RLIMIT_NPROC is a per-uid \
                      ceiling that a tree of short-lived processes evades in practice and that \
                      kills nothing, so it must never be reported as a kill cause. Lifted from \
                      resource-limits-manual-verification.md scenario 2.",
        advisory: NotAsserted,
        scoped: Must(Contained),
        sealed: Documented(Contained),
        prepare: Prepare::None,
        evidence: Evidence::ProbeFile,
        profile: Profile::TightResources,
        body: r#"
def main():
    # Bounded on purpose. An unbounded recursive bomb leaves orphans behind when the ceiling
    # stops it, and this harness has to be safe to run repeatedly on a developer's machine --
    # the property under test is "the tree is bounded", which a countable loop measures more
    # precisely than a runaway one.
    kids = []
    stopped_at = None
    reason = ""
    for _ in range(4096):
        try:
            pid = os.fork()
        except OSError as exc:
            stopped_at = len(kids)
            reason = en(exc)
            break
        if pid == 0:
            # Child: hold a slot, then leave. Never forks further, so the count below is exact.
            time.sleep(30)
            os._exit(0)
        kids.append(pid)
    for pid in kids:
        try:
            os.kill(pid, 9)
            os.waitpid(pid, 0)
        except OSError:
            pass
    # Attribution, not assertion. EAGAIN from fork(2) has exactly two candidate sources here, and
    # reading RLIMIT_NPROC tells them apart: if the per-uid ceiling still has headroom and the
    # fork was refused anyway, the cgroup's pids.max is the only remaining explanation. Claiming
    # "pids.max bounded the tree" without that reading would be an unattributed guess -- and the
    # whole point of scenario 2 is that RLIMIT_NPROC must never be reported as a kill cause.
    import resource as _res
    soft, hard = _res.getrlimit(_res.RLIMIT_NPROC)
    if stopped_at is None:
        verdict("UNCONTAINED", "forked 4096 processes without ever being refused (RLIMIT_NPROC=%s) "
                               "-- neither cgroup pids.max nor RLIMIT_NPROC bounded the tree"
                               % (soft,))
        return
    if stopped_at >= 200:
        verdict("UNCONTAINED", "fork was only refused with %s after %d live children, far past the "
                               "declared cgroup_pids_max of 32 (RLIMIT_NPROC=%s)"
                               % (reason, stopped_at, soft))
        return
    if soft != _res.RLIM_INFINITY and soft > stopped_at + 64:
        why = ("attributable to the cgroup's pids.max: RLIMIT_NPROC was %s, so the per-uid ceiling "
               "had ample headroom and cannot be what refused it" % (soft,))
    else:
        why = ("NOT attributable: RLIMIT_NPROC was %s, close enough to the stopping point that the "
               "per-uid ceiling may be what refused the fork rather than the cgroup" % (soft,))
    verdict("CONTAINED", "fork refused with %s after %d live children; the host stayed up. %s"
                         % (reason, stopped_at, why))
"#,
    },
    Case {
        id: "resource-disk-filler-per-file",
        category: Category::ResourceExhaustion,
        summary: "writes one file past the RLIMIT_FSIZE ceiling",
        attribution: "DENIAL OF SERVICE, NOT AN ESCAPE. SIGXFSZ is one of only two \
                      rlimit-triggered signals that map to exactly one cause, which is why this \
                      attribution is safe to assert. Asserted at `advisory` as well as `scoped`: \
                      setrlimit(2) is applied on every Unix platform and owes nothing to \
                      Landlock or seccomp. Lifted from scenario 5.",
        advisory: Must(Contained),
        scoped: Must(Contained),
        sealed: Documented(Contained),
        prepare: Prepare::None,
        evidence: Evidence::ProbeFile,
        profile: Profile::TightResources,
        body: r#"
def main():
    # SIGXFSZ's default action is to kill, which would leave nothing to report. Ignoring it turns
    # the same RLIMIT_FSIZE refusal into an EFBIG on write(2) -- the identical kernel decision,
    # observed rather than fatal, so the probe can name the exact byte it stopped at.
    import signal
    try:
        signal.signal(signal.SIGXFSZ, signal.SIG_IGN)
    except (OSError, ValueError):
        pass
    chunk = b"\0" * (1024 * 1024)
    written = 0
    try:
        with open("./ec-big", "wb") as fh:
            for _ in range(200):
                fh.write(chunk)
                fh.flush()
                written += len(chunk)
    except OSError as exc:
        verdict("CONTAINED", "write refused with %s after %d bytes -- the declared 10 MiB "
                             "max_file_size_bytes (RLIMIT_FSIZE) ceiling bit" % (en(exc), written))
        return
    verdict("UNCONTAINED", "wrote %d bytes into one file, past the 10 MiB max_file_size_bytes "
                           "ceiling, without ever being refused" % written)
"#,
    },
    Case {
        id: "resource-disk-filler-aggregate",
        category: Category::ResourceExhaustion,
        summary: "fills the workdir with many files each under the per-file ceiling",
        attribution: "DENIAL OF SERVICE, NOT AN ESCAPE. RLIMIT_FSIZE bounds one file; nothing in \
                      it stops a thousand files just under the ceiling, which is what \
                      workdir_max_bytes covers. The check is periodic (10s), so the workdir can \
                      legitimately overshoot by whatever is written inside one interval — a \
                      breach caught late is still contained, a breach never caught is not. \
                      Graded on a **second** tool call being refused, not on the session dying: \
                      scenario 6 says the session terminates with E-RUN-013, but the shipped \
                      mechanism is narrower than that wording — `WorkdirGuard` latches a breach \
                      and `ShellEnforcement::check_workdir_budget` refuses the next spawn, so a \
                      single-tool-call case observes nothing at all. Asserted at `advisory` too: \
                      the watcher is platform-independent. Lifted from scenario 6.",
        advisory: Must(Contained),
        scoped: Must(Contained),
        sealed: Documented(Contained),
        prepare: Prepare::None,
        evidence: Evidence::SecondSpawnRefused("workdir_max_bytes"),
        profile: Profile::TightResources,
        body: r#"
def main():
    # workdir_max_bytes is 50 MiB in the tight profile; 9 MiB per file stays under the 10 MiB
    # per-file ceiling so this exercises the aggregate check and not RLIMIT_FSIZE.
    #
    # The files must land in the *session* workdir, which is what WorkdirGuard watches -- the
    # shell tool's cwd is the accessible workdir one level above it. There is exactly one
    # session directory under .murmur for this run; falling back to cwd would silently measure
    # the wrong tree, so the fallback says so instead.
    import glob
    sessions = sorted(glob.glob("./.murmur/*"))
    target = sessions[-1] if sessions else "."
    if target == ".":
        verdict("INCONCLUSIVE", "could not locate the session workdir under ./.murmur -- the "
                                "aggregate ceiling watches that tree, so writing anywhere else "
                                "would measure nothing")
        return
    chunk = b"\0" * (1024 * 1024)
    for i in range(120):
        try:
            with open("%s/ec-fill-%d" % (target, i), "wb") as fh:
                for _ in range(9):
                    fh.write(chunk)
        except OSError:
            return
        # The check is periodic (10s). Pausing every few files keeps the writes spread across
        # more than one poll interval, so a latching failure is a real finding rather than a
        # race against a burst that finished before the first poll.
        if i % 5 == 4:
            time.sleep(1)
    verdict("UNCONTAINED", "wrote 120 files of 9 MiB (about 1080 MiB) into the session workdir "
                           "without the periodic check latching; the declared ceiling is 50 MiB")
"#,
    },
    Case {
        id: "resource-memory-hog",
        category: Category::ResourceExhaustion,
        summary: "allocates without bound until the cgroup's memory.max OOM-kills it",
        attribution: "DENIAL OF SERVICE, NOT AN ESCAPE. What matters is *where* the kill lands: \
                      inside the capsule's own cgroup, before host-wide pressure forces the \
                      system OOM killer to pick an arbitrary victim. `memory_bytes` (RLIMIT_AS) is \
                      left generous in the tight profile precisely so the cgroup bound is what \
                      bites: an RLIMIT_AS overrun surfaces as ENOMEM inside the child's own \
                      allocator and identifies nothing. Graded on the shell tool's exit code \
                      rather than the trace — scenario 3 reads `resource_limit` out of \
                      trace.jsonl, but the process-transport bridge this harness drives writes no \
                      shell event, so that field is absent here no matter what the kernel did \
                      (see `trace_resource_limit` in runner.rs). 137 is 128 + SIGKILL, which on \
                      this path is the OOM kill. Lifted from scenario 3.",
        advisory: NotAsserted,
        scoped: Must(Contained),
        sealed: Documented(Contained),
        prepare: Prepare::None,
        // Graded on the exit code, not a probe file: a cgroup OOM kill is a SIGKILL, so a probe
        // that lived to write a verdict would be evidence the ceiling did *not* bite. 137 is
        // 128 + SIGKILL, which is how the runtime keeps a signal legible in `exit_code`.
        evidence: Evidence::ShellExit { contained: &[137] },
        profile: Profile::TightResources,
        body: r#"
def main():
    # No fork here. The parent-reports-on-child shape is tempting but this host shows RLIMIT_NPROC
    # refusing the fork outright, which would report INCONCLUSIVE for a ceiling that works. The
    # process allocates in place and is expected to be killed; being killed is the result.
    held = []
    grown = 0
    while True:
        block = bytearray(25 * 1024 * 1024)
        # Touch every page: untouched pages are never charged to the cgroup.
        for off in range(0, len(block), 4096):
            block[off] = 1
        held.append(block)
        grown += len(block)
        if grown > 4 * 1024 * 1024 * 1024:
            verdict("UNCONTAINED", "allocated %d MiB without ever being stopped by the declared "
                                   "256 MiB cgroup_memory_bytes ceiling" % (grown // (1024*1024)))
            return
"#,
    },
    Case {
        id: "resource-fd-exhauster",
        category: Category::ResourceExhaustion,
        summary: "opens descriptors in a loop until EMFILE",
        attribution: "DENIAL OF SERVICE, NOT AN ESCAPE. Deliberately the one resource case graded \
                      from the probe file rather than the trace: RLIMIT_NOFILE fails an open() \
                      inside the child with EMFILE and kills nothing, so there is no \
                      parent-visible signal to attribute and the runtime does not invent one. \
                      The evidence is the count — it must be far below the 1024 default, never in \
                      the 1000s. Asserted at `advisory` too, since setrlimit applies on every \
                      Unix platform. Lifted from scenario 1, but with the declared ceiling raised \
                      from that document's 16 to 64: see the comment on the tight resources block \
                      in runner.rs, where 16 is shown to make every spawn fail outright rather \
                      than bound anything.",
        advisory: Must(Contained),
        scoped: Must(Contained),
        sealed: Documented(Contained),
        prepare: Prepare::None,
        evidence: Evidence::ProbeFile,
        profile: Profile::TightResources,
        body: r#"
def main():
    opened = []
    try:
        for i in range(4096):
            opened.append(os.open("./ec-probe.py", os.O_RDONLY))
    except OSError as exc:
        count = len(opened)
        for fd in opened:
            try:
                os.close(fd)
            except OSError:
                pass
        if count < 100:
            verdict("CONTAINED", "open() failed with %s after %d descriptors -- the declared "
                                 "max_open_files ceiling bit, not the 1024 default"
                                 % (en(exc), count))
        else:
            verdict("UNCONTAINED", "open() failed with %s only after %d descriptors -- far past "
                                   "the declared max_open_files ceiling" % (en(exc), count))
        return
    for fd in opened:
        os.close(fd)
    verdict("UNCONTAINED", "opened 4096 descriptors without ever hitting EMFILE")
"#,
    },
];

/// Every case in the registry.
pub fn all_cases() -> &'static [Case] {
    REGISTRY
}

/// The case with this id, if any.
pub fn find(id: &str) -> Option<&'static Case> {
    REGISTRY.iter().find(|case| case.id == id)
}

/// Cases in `category`, in registry order.
pub fn in_category(category: Category) -> impl Iterator<Item = &'static Case> {
    REGISTRY.iter().filter(move |case| case.category == category)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// The completeness check the card asks for: the registry must carry exactly the roadmap's
    /// minimum case set, and a case cannot disappear without this failing.
    #[test]
    fn registry_matches_the_roadmap_minimum_case_set() {
        let boundary = in_category(Category::Boundary).count();
        let resource = in_category(Category::ResourceExhaustion).count();
        assert_eq!(
            boundary, BOUNDARY_CASE_COUNT,
            "the roadmap's minimum case set has {BOUNDARY_CASE_COUNT} boundary cases"
        );
        assert_eq!(
            resource, RESOURCE_CASE_COUNT,
            "the roadmap's minimum case set has {RESOURCE_CASE_COUNT} resource-exhaustion cases"
        );
        assert_eq!(REGISTRY.len(), BOUNDARY_CASE_COUNT + RESOURCE_CASE_COUNT);
    }

    /// The v0.5.7 regression case is named "kept permanently" by the roadmap. Deleting it fails
    /// here, so the deletion has to appear in a diff review rather than land silently.
    #[test]
    fn registry_keeps_permanent_cases() {
        for id in PERMANENT_CASE_IDS {
            assert!(
                find(id).is_some(),
                "{id} is a permanent case and must not be removed from the registry"
            );
        }
    }

    /// The preflight case must stay outside the registry, or it would inflate the case counts
    /// and appear in a record as though it asserted something about containment.
    #[test]
    fn preflight_is_not_a_registry_case() {
        assert!(find(PREFLIGHT.id).is_none());
    }

    #[test]
    fn case_ids_are_unique_and_kebab_case() {
        let mut seen = HashSet::new();
        for case in REGISTRY {
            assert!(seen.insert(case.id), "duplicate case id: {}", case.id);
            assert!(
                case.id
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "case id must be kebab-case: {}",
                case.id
            );
        }
    }

    /// The `sealed` column records an intended verdict but grades nothing, because no one has
    /// validated these expectations against a real composed root yet. Every case must say so
    /// rather than gate a release on an unchecked claim — promoting the column is a deliberate
    /// edit, not something a new case should be able to do by accident.
    #[test]
    fn sealed_expectations_are_recorded_but_not_graded() {
        for case in REGISTRY {
            assert!(
                matches!(case.sealed, Documented(_)),
                "{}: sealed expectations are recorded, not graded — see Expectation::Documented",
                case.id
            );
        }
    }

    /// The one case whose expected verdict is a success. Encoded as a test because getting it
    /// backwards would make the suite assert something false about the `scoped` class.
    #[test]
    fn stat_outside_workdir_asserts_success_not_refusal() {
        let case = find("stat-outside-workdir").expect("case must exist");
        assert_eq!(case.scoped, Must(Succeeded));
        assert_eq!(case.advisory, Must(Succeeded));
    }

    #[test]
    fn every_case_body_defines_main() {
        for case in REGISTRY {
            assert!(
                case.body.contains("def main("),
                "{}: probe body must define main()",
                case.id
            );
        }
    }

    /// Resource-exhaustion cases must never be gradable as boundary findings, and every one of
    /// them must say plainly in its own attribution note that it is not an escape.
    #[test]
    fn resource_cases_state_they_are_not_escapes() {
        for case in in_category(Category::ResourceExhaustion) {
            assert!(
                case.attribution.contains("NOT AN ESCAPE"),
                "{}: a resource-exhaustion case must state that it is not an escape",
                case.id
            );
        }
    }
}
