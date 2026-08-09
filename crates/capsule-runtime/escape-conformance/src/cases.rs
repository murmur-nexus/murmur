//! The case registry — the minimum case set, with each case's expected
//! verdict per containment class.
//!
//! # There is no disable mechanism, deliberately
//!
//! [`REGISTRY`] is a flat `const` slice. There is no `enabled` flag, no filter file, no
//! environment variable that drops a case, and [`ALL_CASES`]/[`BOUNDARY_CASE_COUNT`]/
//! [`RESOURCE_CASE_COUNT`] are asserted against it by the tests at the bottom of this file. The
//! `exec-renamed-disallowed-binary` is permanent; the way that is held
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
//! * **`sealed`** is asserted. Its column was validated against a real composed root on
//!   2026-08-09 — an uncontainerised `KernelSealed` host, kernel `7.0.0-28-generic`, uid 1000 —
//!   and every value in it is now [`Expectation::Must`] of the verdict that run actually
//!   produced, with two exceptions named below. Nothing in the column is
//!   [`Expectation::Documented`] any more: that variant survives for the *next* containment class
//!   that exists but has not been measured, not for this one. The run, the four
//!   documented-versus-actual mismatches it resolved and the two probe fixes it needed are
//!   recorded under "Recording the result" in
//!   `docs/content/reference/sealed-containment-manual-verification.md`.
//!
//!   The two exceptions are `hardlink-escape` and `rename-across-boundary`, which are
//!   [`Expectation::NotAsserted`] at `sealed` for a reason unlike `advisory`'s. `advisory` is
//!   not-asserted because the *class* has no mechanism; these two are not-asserted because the
//!   *cases' own shape* cannot reach their premise at this class — `sealed` composes its root out
//!   of independent bind mounts, so `link(2)`/`rename(2)` hit `EXDEV` at the mount boundary before
//!   Landlock is ever consulted, for every destination reachable from the workdir. See each case's
//!   `attribution`.
//!
//! # Adding a fourth containment class
//!
//! Give it its own field and its own column, and start it at [`Expectation::Documented`] — record
//! what the mechanism is meant to do, grade nothing, and promote the column only once someone has
//! run the suite against the real thing and can say what it does. That is exactly the sequence
//! `sealed` went through.

use crate::verdict::{Category, Expectation, Verdict};
use murmur_artifact::ContainmentClass;

use Expectation::{Must, NotAsserted};
use Verdict::{Allowed, Contained, Refused, Succeeded};

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
/// would corrupt the minimum-case-set arithmetic. It exists because the alternative is
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

/// Minimum case set: 23 boundary cases.
pub const BOUNDARY_CASE_COUNT: usize = 23;
/// Minimum case set: 5 resource-exhaustion cases.
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
                      ordinary file permissions and proves nothing about containment.\
                      At `sealed` that control inverts and the ambiguity above stops applying: \
                      /etc/passwd IS on `sealed::SEALED_ETC_PATHS` and is readable *by design*, \
                      while the measured refusal for /etc/shadow is ENOENT and not EACCES — the \
                      path is not on the allowlist, so it does not exist in the composed root at \
                      all. Absence is the stronger property: there is no inode left for any grant \
                      to be widened onto.",
        advisory: NotAsserted,
        scoped: Must(Refused),
        sealed: Must(Refused),
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
                      this for any uid; only kernel filesystem mediation can refuse it. Under \
                      `scoped`, a REFUSED verdict here is attributable to Landlock and nothing \
                      else. Under `sealed`, ALLOWED is the correct, intended verdict: /tmp is a \
                      bind mount of a directory inside the session workdir \
                      (`sealed::SEALED_TMP_DIR_NAME`), carrying the workdir's own Landlock \
                      rights (see 646b64ee) — the write lands at the path named in DETAIL, under \
                      the workdir, not actually outside it. Measured ALLOWED against a real \
                      composed root, which is why it is asserted that way here rather than \
                      merely predicted.",
        advisory: NotAsserted,
        scoped: Must(Refused),
        sealed: Must(Allowed),
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
                      path outside the capsule's mount namespace entirely. That is exactly what a \
                      real composed root does, and the measured errno says so: `sealed` refuses \
                      with ENOENT, not EACCES — /etc/shadow is not on `SEALED_ETC_PATHS`, so \
                      there is no inode in the namespace for stat(2) to describe. A REFUSED at \
                      `sealed` and a SUCCESS at `scoped` are therefore both correct, and the \
                      difference is the mount namespace rather than a stronger Landlock ruleset.",
        advisory: Must(Succeeded),
        scoped: Must(Succeeded),
        sealed: Must(Refused),
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
                      is never mistaken for a contained read.\
                      At `sealed` the measured verdict is ALLOWED, and that is the class working as declared rather than a crossing: `/etc/passwd` is on `sealed::SEALED_ETC_PATHS`, the curated allowlist the composed root bind-mounts read-only and (since slice fb1eea97) grants read on, so `getpwuid(3)` resolves. `/etc/shadow` is not on that list and is absent from the root entirely — see `read-etc-shadow`, which stays REFUSED. The symlink resolves *inside* the composed root, so what it can reach is bounded by what the root exposes; that is the property being asserted, not the read succeeding.",
        advisory: NotAsserted,
        scoped: Must(Refused),
        sealed: Must(Allowed),
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
                      separate case.\
                      NOT ASSERTED AT `sealed`, and for a different reason than `advisory`'s. \
                      `advisory` asserts nothing here because the *class* has no mechanism; this \
                      case asserts nothing at `sealed` because the *case's own shape* cannot reach \
                      its premise at that class. `sealed` composes its root out of independent \
                      bind mounts — the workdir at its own absolute path, each \
                      `SEALED_ETC_PATHS` entry, /dev, /proc and /tmp each its own mount (see the \
                      six-step plan in `sealed::plan_composed_root`) — and link(2) returns EXDEV \
                      whenever source and destination are on different mounts, whatever their \
                      hosts' filesystems. The real run measured exactly that: EXDEV, which the \
                      probe correctly reports as INCONCLUSIVE, before Landlock was consulted at \
                      all. No destination avoids it: every path reachable from the workdir is \
                      either another independent bind or the base root the workdir bind sits on, \
                      because giving the workdir its own mount is the mechanism that makes \
                      everything else read-only-or-absent. A control run rules out the obvious \
                      alternative reading — that the harness's work root merely sat on a different \
                      block device: re-run with the work root on the *same* device as /etc, this \
                      case refuses for real at `scoped` (EPERM) and still reports EXDEV at \
                      `sealed`. Asserting a verdict here would be asserting the mount layout, not \
                      containment. A second consequence of the \
                      same layout: /proc/sys is unreachable in the composed root, so \
                      `protected_hardlinks` reads back as `unknown` and even the EPERM/EACCES \
                      attribution above is unavailable at this class.",
        advisory: NotAsserted,
        scoped: Must(Refused),
        sealed: NotAsserted,
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
                      Landlock does mediate.\
                      NOT ASSERTED AT `sealed`, and for a different reason than `advisory`'s. \
                      `advisory` asserts nothing because the *class* has no mechanism; this case \
                      asserts nothing at `sealed` because the *case's own shape* cannot reach its \
                      premise there. /tmp in the composed root is a bind whose source is \
                      `workdir.join(SEALED_TMP_DIR_NAME)` — a subdirectory of this very workdir on \
                      the host — yet the workdir bind and the /tmp bind are two different mounts \
                      in the namespace, so rename(2) returns EXDEV even though both sides share \
                      one host filesystem. The real run measured exactly that, and the probe \
                      correctly reports INCONCLUSIVE. Landlock is never reached, and no other \
                      destination changes that: /etc/*, /dev and the base root are each their own \
                      mount too, and a path with no bind at all answers ENOENT rather than \
                      exercising a rename boundary. A control run rules out the obvious \
                      alternative reading — that the harness's work root merely sat on a different \
                      block device: re-run with the work root on the *same* device as /tmp, this \
                      case refuses for real at `scoped` (EACCES) and still reports EXDEV at \
                      `sealed`. Encoding a `Must` here would grade the mount layout instead of \
                      containment.",
        advisory: NotAsserted,
        scoped: Must(Refused),
        sealed: NotAsserted,
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
                      exists to make.\
                      At `sealed` the measured verdict is ALLOWED, and that is the class working as declared rather than a crossing: the walk reaches the *composed root's* `/etc/passwd`, which is on `sealed::SEALED_ETC_PATHS` — the curated allowlist the root bind-mounts read-only and (since slice fb1eea97) grants read on. Everything a walk out of the workdir can arrive at is bounded by what the composed root exposes, and `/etc/shadow` is not on that list and is absent from the root entirely (see `read-etc-shadow`, which stays REFUSED). \
                      This case is one of the four documented/actual mismatches the 2026-08-09 \
                      bare-metal run had to resolve, and the resolution is that the earlier \
                      REFUSED reading was a defect in *this probe*, not a containment finding. The \
                      walk used a fixed six `..` components, so whether it reached the filesystem \
                      root at all depended on how deep the harness's own `--work-root` happened to \
                      be: from a shallow work root the same case on the same host measured \
                      ALLOWED, and from a deep one it measured REFUSED with ENOENT on a path like \
                      `<work-root-ancestor>/etc/passwd` — path-depth arithmetic wearing a \
                      containment verdict's clothes. The `..` count is now derived from the \
                      probe's own cwd depth so the walk always lands on `/`, and DETAIL names the \
                      count and the resolved target. ALLOWED is therefore the measured verdict of \
                      the property this case actually claims to test.",
        advisory: NotAsserted,
        scoped: Must(Refused),
        sealed: Must(Allowed),
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

    # The `..` count is derived, never fixed. A constant count is a silent dependency on how deep
    # the harness's own work root happens to be: with too few components the walk stops inside an
    # ancestor of the workdir, `etc/passwd` under it does not exist, and the ENOENT reads as a
    # containment refusal when nothing was contained. `..` at `/` resolves to `/`, so overshooting
    # is free -- one extra component past the cwd's own depth guarantees the walk lands on the root
    # of this mount namespace no matter where the workdir sits.
    hops = os.getcwd().count("/") + 1
    target = "/proc/self/cwd/" + "../" * hops + "etc/passwd"

    # The boundary is here, not above: can the magic symlink be used as a base to reach a path
    # the ruleset does not grant?
    escaped, esc_note = probe_open(target)
    walk = "walked up %d components from %s, so the walk reached the root of this mount " \
           "namespace rather than stopping inside an ancestor of the workdir" % (hops, os.getcwd())
    if escaped is not None:
        data = os.read(escaped, 32)
        os.close(escaped)
        verdict("ALLOWED", "walked out of the workdir through /proc/self/cwd and read %d bytes of "
                           "/etc/passwd via %s ; %s ; %s ; %s"
                           % (len(data), target, walk, reopen, control))
        return
    verdict("REFUSED", "the walk out of the workdir was refused: %s ; %s ; %s ; %s"
                       % (esc_note, walk, reopen, control))
"#,
    },
    Case {
        id: "proc-pid-root-reopen",
        category: Category::Boundary,
        summary: "reads /etc/passwd through /proc/<pid>/root, the filesystem-root alias",
        attribution: "Same /proc-reachability control as the other two /proc cases. /proc/<pid>/root \
                      is the alias that would defeat a chroot-shaped boundary; against a Landlock \
                      ruleset it is only another path, and it is expected to be refused because \
                      /proc carries no grant.\
                      At `sealed` the measured verdict is ALLOWED, and the reason is worth stating precisely because it is easy to misread as the chroot-shaped escape this route is famous for. `/proc` in the composed root is a *fresh* proc mount made inside the capsule's own mount namespace (step 4 of `sealed::plan_composed_root`), so `/proc/<pid>/root` is an alias for the composed root — not for the host's `/`. What the read reaches is therefore the composed root's `/etc/passwd`, which is on `sealed::SEALED_ETC_PATHS` and is granted read since slice fb1eea97. The alias works and lands nowhere the class did not already expose; `/etc/shadow` is absent from the root entirely, so the same route cannot reach it (see `read-etc-shadow`). Note the /proc control still reports `proc_itself=denied`: ordinary /proc files carry no Landlock grant even here, which is why the reachable path is the magic alias and not /proc at large.",
        advisory: NotAsserted,
        scoped: Must(Refused),
        sealed: Must(Allowed),
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
                      all the case is INCONCLUSIVE rather than passed.\
                      At `sealed` the base descriptor is still openable — /dev/urandom is one of \
                      the OCI default device nodes the composed root bind-mounts into its private \
                      /dev tmpfs — and the measured result is the same EACCES on the O_RDWR \
                      re-open while the same-mode re-open succeeds. That pairing is the \
                      attribution: the refusal is the mode *upgrade* being denied, not the path \
                      having become unreachable, which is the only reading that makes this case \
                      evidence about anything.",
        advisory: NotAsserted,
        scoped: Must(Refused),
        sealed: Must(Refused),
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
                      hypothetical. Fds 0/1/2 are excluded by design and are not a leak.\
                      At `sealed` the measured result is identical — no fd above stdio is open in \
                      the sandboxed child — and the composed root contributes nothing to it: the \
                      descriptor is closed before either the Landlock ruleset or the pivot exists. \
                      The same verdict is asserted at both kernel classes precisely because the \
                      mechanism is the same one.",
        advisory: NotAsserted,
        scoped: Must(Refused),
        sealed: Must(Refused),
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
                      shape that was actually exposed; scenario 4 of \
                      workdir-device-node-manual-verification.md records \
                      the same caveat. This is the case the negative control flips: restore \
                      MakeBlock to WORKDIR_ACCESS_RIGHTS, rebuild, and it must go ALLOWED.\
                      At `sealed` the measured errno is EACCES, so on this class the non-root \
                      caveat above did not decide the result: the refusal is attributable to the \
                      Landlock workdir grant withholding MakeBlock, not to the missing CAP_MKNOD. \
                      The caveat still stands as a caveat — a host whose kernel answers EPERM \
                      first would leave the same verdict unattributable — which is why it is not \
                      deleted here.",
        advisory: NotAsserted,
        scoped: Must(Refused),
        sealed: Must(Refused),
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
                      exec/connect race class is separately audited and lives in racecheck/.\
                      At `sealed` the measured refusal is EACCES from execve, and two independent \
                      things hold it: the exec allowlist resolves the copy to its real path before \
                      comparing, and the workdir bind carries no execute right at all \
                      (`capabilities.filesystem.workdir_exec` is false), so nothing staged into the \
                      workdir is executable whatever it is called.",
        advisory: NotAsserted,
        scoped: Must(Refused),
        sealed: Must(Refused),
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
                      is a different probe and is out of scope here.\
                      The seccomp-notify supervisor named above was retired by slice f163778e, and \
                      on both kernel classes the mechanism is now structural rather than a filter \
                      that has to be consulted: the capsule runs in its own network namespace whose \
                      only route is `local default dev lo`, and the runtime binds a TCP listener \
                      only on the ports `capabilities.network.allow` implies. An unlisted port is \
                      delivered locally to nothing. The errno measured at `sealed` is therefore \
                      ECONNREFUSED — which is that refusal, not a host routing failure; a genuine \
                      absence of route would present as the timeout or ENETUNREACH the probe \
                      reports as INCONCLUSIVE.",
        advisory: NotAsserted,
        scoped: Must(Refused),
        sealed: Must(Refused),
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
                      destination rides on sendto(2), and a datagram send cannot be refused after \
                      the fact.\
                      GRADED ON DELIVERY, NOT ON sendto's RETURN VALUE — and that distinction is \
                      the whole of this case's resolution. Slice f163778e replaced the \
                      seccomp-notify sendto rule with a network namespace whose single route is \
                      `local default dev lo`, which makes *every* destination address locally \
                      deliverable (`network_namespace`'s module doc, point 3). A sendto to any \
                      address therefore returns success because the write succeeded, not because \
                      anything left the host, and the previous version of this probe reported \
                      ALLOWED on that return value alone. It also aimed at port 53, the one UDP \
                      port the runtime *does* bind inside the namespace \
                      (`network_namespace::bind_dns_socket`), so its datagram terminated in the \
                      runtime's own DNS resolver — the most contained outcome available, scored as \
                      an escape. \
                      The probe now binds a receiver in the same network namespace, on the same \
                      port, *before* sending, and grades on what that receiver sees. On the \
                      2026-08-09 bare-metal run the payload arrived at it byte-for-byte, with a \
                      source address of 1.1.1.1 — the destination itself, because the `local` route \
                      makes that address local: the datagram was captured inside the capsule's own \
                      namespace and never reached a wire. REFUSED is the measured verdict, matching the original \
                      documented intent — but the refusal is structural (there is no path off the \
                      host) rather than an errno, which is what DETAIL says. Had the receiver seen \
                      nothing, that would have ruled out namespace-local capture and the case would \
                      report ALLOWED with the ruling-out stated; a receiver that cannot be bound is \
                      INCONCLUSIVE, never a pass, because then the two readings cannot be told \
                      apart at all.",
        advisory: NotAsserted,
        scoped: Must(Refused),
        sealed: Must(Refused),
        prepare: Prepare::None,
        evidence: Evidence::ProbeFile,
        profile: Profile::Boundary,
        body: r#"
def main():
    # Deliberately not port 53. The runtime binds a wildcard UDP :53 resolver inside the capsule's
    # network namespace, so a datagram sent there is swallowed by the runtime's own DNS half and
    # this probe could never observe it -- and DNS as an exfiltration channel is `dns-exfiltration`,
    # a separate case. This port is in no allowlist and nothing else binds it.
    port = 46053
    payload = b"murmur-escape-conformance-udp-%d" % os.getpid()

    # Bound BEFORE the send, in this process, and therefore in the same network namespace the
    # sendto will be issued from. This is the whole mechanism: it is what tells "captured locally,
    # nothing left the host" apart from "genuinely reached the internet", which the return value of
    # sendto(2) cannot distinguish under a namespace whose only route makes every address local.
    rx = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        rx.bind(("0.0.0.0", port))
    except OSError as exc:
        rx.close()
        verdict("INCONCLUSIVE", "could not bind the same-namespace receiver on 0.0.0.0:%d (%s), so "
                                "a successful sendto could not be told apart from a real egress -- "
                                "nothing was concluded about the boundary" % (port, en(exc)))
        return
    rx.settimeout(5)

    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.settimeout(8)
    try:
        try:
            sent = sock.sendto(payload, ("1.1.1.1", port))
        except OSError as exc:
            if getattr(exc, "errno", None) in (errno.ENETUNREACH, errno.EHOSTUNREACH):
                verdict("INCONCLUSIVE", "sendto(1.1.1.1:%d) got %s -- no route at all, so nothing "
                                        "about the boundary was exercised" % (port, en(exc)))
            else:
                verdict("REFUSED", "sendto(1.1.1.1:%d) refused with %s before any datagram was "
                                   "written" % (port, en(exc)))
            return
        # Context only, never the verdict: what the retired probe measured, reported alongside so a
        # reader can see the old ALLOWED reading and its cause in the same line.
        try:
            dns = "sendto(1.1.1.1:53) also accepted %d bytes, which is the reading the previous " \
                  "version of this probe scored as an escape: 53 is bound inside this namespace " \
                  "by the runtime's own DNS resolver, so that datagram terminates there" \
                  % sock.sendto(payload, ("1.1.1.1", 53))
        except OSError as exc:
            dns = "sendto(1.1.1.1:53)=%s" % en(exc)

        try:
            seen, src = rx.recvfrom(len(payload) + 64)
        except (socket.timeout, OSError) as exc:
            verdict("ALLOWED", "sendto accepted %d bytes to 1.1.1.1:%d and the receiver bound on "
                               "0.0.0.0:%d in this same network namespace saw nothing within 5s "
                               "(%s) -- namespace-local capture is ruled out, so the datagram went "
                               "somewhere this probe cannot account for ; %s"
                               % (sent, port, port, exc.__class__.__name__, dns))
            return
        if seen == payload:
            verdict("REFUSED", "the datagram never left the host: sendto accepted %d bytes to "
                               "1.1.1.1:%d and the identical payload arrived at a receiver bound "
                               "on 0.0.0.0:%d in this same network namespace, from %s. The "
                               "namespace's `local default dev lo` route delivered it locally, so "
                               "sendto succeeding is a write that succeeded and not an egress -- "
                               "the refusal is structural, not an errno ; %s"
                               % (sent, port, port, src, dns))
            return
        verdict("INCONCLUSIVE", "the receiver on 0.0.0.0:%d saw %r from %s, which is not the "
                                "payload that was sent -- that establishes neither local capture "
                                "nor egress ; %s" % (port, seen[:32], src, dns))
    finally:
        for handle in (sock, rx):
            try:
                handle.close()
            except OSError:
                pass
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
                      EAI_* failures that are not refusals are reported verbatim in DETAIL.\
                      At `sealed` the measured failure is EAI_AGAIN (`[Errno -3] Temporary failure \
                      in name resolution`) and *not* EAI_NONAME, and the difference is the finding. \
                      The lookup did reach a resolver — the wildcard UDP :53 socket \
                      `network_namespace::bind_dns_socket` binds inside the capsule's own network \
                      namespace — and that resolver is the runtime's, which answers a name outside \
                      `capabilities.network.allow` with an rcode of REFUSED rather than forwarding \
                      it. The query and any payload in its label terminated in the runtime \
                      process. An NXDOMAIN would have meant the opposite (the label travelled), \
                      which is why the probe scores that as ALLOWED.",
        advisory: NotAsserted,
        scoped: Must(Refused),
        sealed: Must(Refused),
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
                      LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET scopes abstract sockets only.\
                      At `sealed` the measured refusal is EACCES raised by socket(2) itself, which \
                      is the mechanism W-SEC-005 describes and not a connect-time accident. The \
                      composed root changes nothing here: a register-level domain filter runs \
                      before any path exists to be mounted or not mounted.",
        advisory: NotAsserted,
        scoped: Must(Refused),
        sealed: Must(Refused),
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
                      call refused and, if socket() succeeded, which of the two paths existed.\
                      At `sealed` the measured refusal is EACCES at socket(2), before either path \
                      is tried — the expected shape. Worth stating that the composed root would \
                      also make both paths absent, since neither `/run` nor `/var/run` is on \
                      `SEALED_ETC_PATHS` or in the runtime path set: at this class two independent \
                      mechanisms close the same route, and the one that answered first is the \
                      domain filter.",
        advisory: NotAsserted,
        scoped: Must(Refused),
        sealed: Must(Refused),
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
                      create io_uring instances, so EPERM here is seccomp and nothing else.\
                      Measured REFUSED with EPERM at `sealed`, and it is asserted there for the \
                      same reason as at `scoped`: the seccomp filter is loaded on both kernel \
                      classes, so this case's mechanism does not vary between them. That it is \
                      *also* the entry that would route around the composed root's filesystem \
                      boundary is why it is asserted at the stronger class rather than assumed.",
        advisory: NotAsserted,
        scoped: Must(Refused),
        sealed: Must(Refused),
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
                      refusal from a host-policy one.\
                      Measured REFUSED with EPERM at `sealed` — but note the attribution control \
                      is *weaker* at this class, not stronger: /proc/sys is not part of the \
                      composed root's /proc, so the sysctl reads back as `unknown` and the \
                      host-policy reading cannot be excluded from inside the capsule. The verdict \
                      is asserted because the seccomp filter is identical on both kernel classes \
                      and `scoped`, where the sysctl is readable, is the run that attributes it.",
        advisory: NotAsserted,
        scoped: Must(Refused),
        sealed: Must(Refused),
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
                      DETAIL records the euid so this is never read as more than it is.\
                      Measured REFUSED with EPERM at `sealed`, on a euid-1000 run, so the same \
                      non-root caveat applies unchanged: the value is asserted because the filter \
                      is the same on both kernel classes, not because this run attributed it.",
        advisory: NotAsserted,
        scoped: Must(Refused),
        sealed: Must(Refused),
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
                      Root run required for an attributable result.\
                      Measured REFUSED with EPERM at `sealed`, on a euid-1000 run, so the caveat \
                      stands and the assertion rests on the filter being identical on both kernel \
                      classes. Its place in the table matters more at this class than at `scoped`: \
                      with CAP_DAC_READ_SEARCH it opens an inode by handle, ignoring the path — \
                      and a composed root is a path-shaped boundary.",
        advisory: NotAsserted,
        scoped: Must(Refused),
        sealed: Must(Refused),
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
                      pointer, i.e. it was permitted.\
                      Measured REFUSED with EPERM at `sealed`, with the sysctl reading back as \
                      `unknown` — /proc/sys is not in the composed root's /proc, so as with \
                      `syscall-userfaultfd` the attribution control is unavailable at this class \
                      and `scoped` is the run that supplies it.",
        advisory: NotAsserted,
        scoped: Must(Refused),
        sealed: Must(Refused),
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
                      what is asserted.\
                      Measured REFUSED with EPERM at `sealed`, and unambiguous there for the same \
                      reason it is at `scoped`: nothing about the composed root would refuse a \
                      call against the caller's own session keyring, so the seccomp default action \
                      is the only remaining explanation.",
        advisory: NotAsserted,
        scoped: Must(Refused),
        sealed: Must(Refused),
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
                      resource-limits-manual-verification.md scenario 2.\
                      Measured CONTAINED at `sealed`: fork was refused with EAGAIN after 31 live \
                      children against a declared cgroup_pids_max of 32, with RLIMIT_NPROC at 1334 \
                      — ample headroom, so the per-uid ceiling cannot be what refused it and the \
                      cgroup is. The composed root neither helps nor hinders here; the ceiling is \
                      the delegated cgroup v2 subtree, which is the same on both kernel classes.",
        advisory: NotAsserted,
        scoped: Must(Contained),
        sealed: Must(Contained),
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
                      Landlock or seccomp. Lifted from scenario 5.\
                      Measured CONTAINED at `sealed`: EFBIG at exactly 10485760 bytes, the \
                      declared 10 MiB max_file_size_bytes, so the ceiling bit on the byte rather \
                      than approximately. Asserted at all three classes for the same reason — \
                      setrlimit owes nothing to any of them.",
        advisory: Must(Contained),
        scoped: Must(Contained),
        sealed: Must(Contained),
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
                      the watcher is platform-independent. Lifted from scenario 6.\
                      Measured CONTAINED at `sealed`: the probe itself reported UNCONTAINED after \
                      writing about 1080 MiB, and the *second* spawn was then refused naming \
                      `workdir_max_bytes` (471864427 bytes past the 52428800 ceiling) — which is \
                      precisely why this case is graded on that second call and not on the probe's \
                      own line. Note the overshoot is large because the composed root's /tmp is \
                      backed by the same workdir tree, so a 10s poll interval covers a lot of \
                      writing; a breach caught late is still contained.",
        advisory: Must(Contained),
        scoped: Must(Contained),
        sealed: Must(Contained),
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
                      this path is the OOM kill. Lifted from scenario 3.\
                      Measured CONTAINED at `sealed`: the shell tool exited 137 with no probe file \
                      written at all, which is the shape a real SIGKILL leaves and the reason this \
                      case is graded on the exit code. A probe that had lived to report would have \
                      been evidence the ceiling did not bite.",
        advisory: NotAsserted,
        scoped: Must(Contained),
        sealed: Must(Contained),
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
                      The evidence is the count, graded against the ceiling the child reads back \
                      from its own RLIMIT_NOFILE rather than against a constant — it must land at \
                      or under that ceiling, and the ceiling itself must be far below the 1024 \
                      default. Asserted at `advisory` too, since setrlimit applies on every \
                      Unix platform. Lifted from scenario 1, but with the declared ceiling raised \
                      from that document's 16 to 128: see the comment on the tight resources block \
                      in runner.rs, where 16 is shown to make every spawn fail outright rather \
                      than bound anything, and where 64 is shown to have stopped being enough \
                      once slice fb1eea97 added the `SEALED_ETC_PATHS` grant fds.\
                      Measured CONTAINED at `sealed`: EMFILE after 125 descriptors against a \
                      declared max_open_files of 128, read back from the child's own \
                      RLIMIT_NOFILE. Both halves of the grading held — at or under the declared \
                      ceiling, and that ceiling far below the 1024 default — and the small gap \
                      between 125 and 128 is the composed root's own grant fds, which is exactly \
                      the effect that forced the ceiling from 64 to 128.",
        advisory: Must(Contained),
        scoped: Must(Contained),
        sealed: Must(Contained),
        prepare: Prepare::None,
        evidence: Evidence::ProbeFile,
        profile: Profile::TightResources,
        body: r#"
def main():
    import resource
    ceiling = resource.getrlimit(resource.RLIMIT_NOFILE)[0]
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
        # Graded against the ceiling the capsule actually declared, read back from the child's
        # own RLIMIT_NOFILE, rather than against a magic number calibrated for one manifest:
        # `max_open_files` has had to move twice already (16 -> 64 -> 128, see the tight
        # resources block in runner.rs), and each move silently invalidated a fixed threshold.
        # Two conditions, and both matter: EMFILE arrived at or under the declared ceiling, and
        # that ceiling is itself well below the 1024 default this case exists to rule out.
        if count <= ceiling and ceiling < 512:
            verdict("CONTAINED", "open() failed with %s after %d descriptors, at or under the "
                                 "declared max_open_files ceiling of %d -- not the 1024 default"
                                 % (en(exc), count, ceiling))
        else:
            verdict("UNCONTAINED", "open() failed with %s after %d descriptors, against a "
                                   "declared max_open_files ceiling of %d -- the ceiling did "
                                   "not bound it" % (en(exc), count, ceiling))
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

    /// Completeness check: the registry must carry exactly the
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

    /// The v0.5.7 regression case is kept permanently. Deleting it fails
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

    /// Case ids whose `sealed` expectation is [`Expectation::NotAsserted`] because the *case's own
    /// shape* cannot reach its premise at that class — not because the class lacks a mechanism.
    ///
    /// Both are `EXDEV`: `sealed` composes its root out of independent bind mounts, so `link(2)`
    /// and `rename(2)` fail at the mount boundary before Landlock is consulted, for every
    /// destination reachable from the workdir. See each case's `attribution`.
    const SEALED_NOT_ASSERTED: &[&str] = &["hardlink-escape", "rename-across-boundary"];

    /// Replaces `sealed_expectations_are_recorded_but_not_graded`, which asserted the pre-promotion
    /// invariant (every `sealed` expectation is [`Expectation::Documented`], i.e. recorded and
    /// gating nothing). The column was validated against a real composed root on 2026-08-09 and is
    /// now graded, so the invariant worth pinning is the inverse: nothing may slip *back* to
    /// `Documented`, and the only cases allowed to assert nothing at `sealed` are the two named
    /// above. A new case that quietly arrived as `Documented` — or a quiet demotion of an existing
    /// one — would put an unchecked claim, or a hole, into a column a release now gates on, which
    /// is the failure this file's "There is no disable mechanism" module doc exists to prevent.
    #[test]
    fn sealed_expectations_are_graded_except_for_the_two_structural_exdev_cases() {
        for case in REGISTRY {
            match case.sealed {
                Expectation::Documented(_) => panic!(
                    "{}: the sealed column is graded now — an expectation must be Must(_) for the \
                     verdict a real composed root produced, or NotAsserted with an attribution \
                     saying why the case cannot reach its premise at this class",
                    case.id
                ),
                NotAsserted => assert!(
                    SEALED_NOT_ASSERTED.contains(&case.id),
                    "{}: only {SEALED_NOT_ASSERTED:?} may assert nothing at sealed. Add this case \
                     to SEALED_NOT_ASSERTED, with the structural reason in its attribution, or \
                     promote it to Must(_) from a real measurement",
                    case.id
                ),
                Must(_) => {}
            }
        }
        // The other direction, so the exemption list cannot outlive the cases it names.
        for id in SEALED_NOT_ASSERTED {
            let case = find(id).unwrap_or_else(|| panic!("{id} is not in the registry"));
            assert_eq!(
                case.sealed,
                NotAsserted,
                "{id} is listed as structurally not-assertable at sealed but now asserts \
                 something — remove it from SEALED_NOT_ASSERTED if a real run can reach its premise"
            );
            assert!(
                case.attribution.contains("EXDEV"),
                "{id}: a case that asserts nothing at sealed must say why in its attribution"
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
