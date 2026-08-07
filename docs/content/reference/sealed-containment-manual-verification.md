# Verification — sealed containment (mount namespace + `pivot_root`)

!!! success "Status: **RUN — 2026-08-05, partial.** Steps 1–4 and 6 pass on a real host through a live capsule's shell tool; step 5 (container) was not run. Addendum 2026-08-07: directory enumeration inside the composed root."

    Steps 0–4 and 6 were executed on a bare Ubuntu host on 2026-08-05, driven through a real,
    live capsule session (`claude` as the inference driver, a real `bash` tool call), and every
    expected result on this page was observed — including the negative control (step 6), which was
    not previously reproducible. The `harden_process_dumpable`/seccomp-notify defect that forced the
    2026-08-03 run to fall back to a hand-run harness for step 3 has since been fixed
    (`security::harden_process_dumpable` / `sandbox::linux_enforce::restore_child_dumpable`); step 3
    is therefore now also confirmed end to end, including Landlock and seccomp running correctly
    **inside** the composed root, which the harness-driven run could not observe. Step 5 (the
    container refusal) is still **not** run — no container runtime is installed on this host — and
    remains the one acceptance criterion on this page with no observed result. Recorded verbatim,
    with exact evidence, in [Recording the result](#recording-the-result). Read that section before
    treating this page as a clean pass. A later, targeted run on 2026-08-07 added the one behaviour
    no earlier run recorded — `ls` on a path that *is* one of the composed root's bind-mounted
    runtime directories — and is recorded in the same section.

    A green `cargo build` / `cargo test` / `cargo clippy` is **not** evidence about the containment
    boundary and must not be reported as if it were. See
    [What this deliberately is not](#what-this-deliberately-is-not).

## What this verifies

A capsule declaring `capabilities.containment: sealed` runs its native subprocess tree inside a
private mount namespace pivoted onto a composed root. The property under test is a **negative and
absolute** one, stronger than anything `scoped` can state:

> A path outside the composed root is not access-denied. It does not exist. There is no name for it.

Concretely, this page checks four things by hand:

1. **The refusal when the AppArmor profile is absent** — `E-CAP-003`, naming the profile and the
   command to load it, before any registry pull or workdir creation.
2. **The happy path** — the capsule launches, and `stat`/`ls`/`cat` against a fixed target list
   outside the root fail with `No such file or directory` (not `Permission denied`).
3. **`trace.jsonl`** records `"containment_achieved":"sealed"` on the `session_start` event.
4. **The container refusal** — the identical capsule inside a plain `docker run` (no
   `--cap-add SYS_ADMIN`) refuses with the container remediation, and never runs at a weaker class.

`No such file or directory` versus `Permission denied` is the whole point of the exercise and is
worth stating before the commands: under `scoped`, `stat /etc/shadow` fails with `EACCES` because
Landlock denied it — the file is there and the capsule knows it is there. Under `sealed`, it fails
with `ENOENT` because there is no `/etc/shadow` in this namespace at all.

## What this deliberately is not

**This procedure is not automated, and must not be wired into CI in any form** — no `#[test]`, no
`#[ignore]` marker intended for an automated runner, no workflow step. CI runs inside containers
that have neither `CAP_SYS_ADMIN` nor an AppArmor profile, so they resolve to `KernelFull` at best
and never execute a single line of the composed-root construction. A test asserting this property
there would pass vacuously or skip, and either outcome turns a green run into false evidence about
a security property it never touched. This is the same reasoning that keeps the
[fd-hygiene procedure](subprocess-fd-hygiene-verification.md#what-this-deliberately-is-not) and the
[seccomp-notify race probes](seccomp-notify-toctou-audit.md) out of the automated suite.

The automated tests this work added assert only the *decision* logic — that
`sandbox::tier_from_probe` reaches `KernelSealed` exactly when all four preconditions hold, that
`containment::achieved_class_for_tier` maps that tier to `sealed`, that `applied_tier` refuses to
install a composed root under a weaker declaration, and that `sealed::plan_composed_root` produces
the intended layout against a synthetic host. None of them touches a kernel.

## Host prerequisites

| requirement | check | expected |
|---|---|---|
| Linux, uncontainerised | `systemd-detect-virt --container` | `none` (and see the note below) |
| kernel 5.13+ (Landlock ABI) | `uname -r` | `5.13` or newer |
| unprivileged user namespaces | `cat /proc/sys/user/max_user_namespaces` | a non-zero number |
| AppArmor userspace tools (AppArmor hosts only) | `command -v apparmor_parser` | a path |
| a non-root login shell | `id -u` | non-zero (the mechanism needs no host root) |
| systemd user session with cgroup delegation | see [resource-limits verification](resource-limits-manual-verification.md) | already required for any capsule that spawns a subprocess |

Steps 1 and 2 assume an Ubuntu 23.10+ host (`kernel.apparmor_restrict_unprivileged_userns=1`). On a
host without AppArmor (Fedora, Arch, Debian without the restriction), step 1 does not apply — record
that, skip to step 2, and note in the result which host you used.

`systemd-detect-virt --container` reporting `none` is necessary but **not** sufficient: a sandbox
built on user namespaces rather than on a container runtime reports `none` and still cannot create
a nested one. The reliable check is the mechanism itself — run this and expect every line to
succeed:

```sh
unshare --user --mount --map-current-user --propagation private -- \
  sh -c 'mount -t tmpfs tmpfs /mnt && echo MOUNT_OK'
```

Expected:

```text
MOUNT_OK
```

If it fails at `write failed /proc/self/uid_map` or at the `mount`, this host cannot back `sealed`
and the rest of this page will not run. That is exactly what the runtime's own probe measures
(`sealed::probe_namespace`), so a failure here and a `sealed` refusal from `mur` agreeing with each
other is the expected, correct behaviour — not a bug in either.

## Step 0 — build the test capsule

Start from a real, runnable capsule rather than a bare `murmur.yaml`. `mur` has no `init`
subcommand — create the project directory and write `murmur.yaml` by hand:

```sh
mkdir -p ~/sealed-check && cd ~/sealed-check
```

An **agent** capsule (one that declares `inference:`) is manifest-only — `mur run` never looks for
a root `*.wasm` for it, so no placeholder component is needed. `capabilities.containment: sealed`
and a `bash` shell allowlist are what this procedure needs; `inference.transport: process` drives
the agent loop through a locally installed provider CLI (`command: claude` here) rather than a
hosted API key, the same pattern as the
[quickstart](../getting-started/quickstart.md#want-to-use-a-subscription) — swap in whatever CLI
the host has installed (`command: codex` for OpenAI's Codex CLI, etc.):

```yaml
name: sealed-check
version: 0.1.0

capabilities:
  containment: sealed
  shell:
    allow:
      - bash

inference:
  transport: process
  command: claude
  model: claude-sonnet-4-5
  max_turns: 4
```

(A **script** capsule — one with no `inference:` block — needs a root `capsule.wasm` instead, and
`mur run` resolves it *before* the containment gate: a directory with only a manifest and no
component reports `error[E-RUN-004]` and never reaches the refusal this page is about. The agent
capsule above sidesteps that path entirely.)

Confirm the declared floor is what you think it is, without launching anything. `--explain-scope`
reads and validates the manifest and probes the host, but does not launch the capsule or start an
agent turn:

```sh
mur run --explain-scope
```

## Step 1 — refusal when the AppArmor profile is not loaded

Make sure the profile is *not* loaded, and that the restriction *is* on:

```sh
sudo apparmor_parser -R /etc/apparmor.d/mur-sealed 2>/dev/null || true
sudo rm -f /etc/apparmor.d/mur-sealed
cat /sys/module/apparmor/parameters/enabled
cat /sys/module/apparmor/parameters/restrict_unprivileged_userns 2>&1
cat /proc/sys/kernel/apparmor_restrict_unprivileged_userns 2>&1
cat /proc/self/attr/current
```

Expected — the runtime reads the module parameter *and* the sysctl, in that order, because
which of the two exists varies by kernel build (on Ubuntu 24.04 / 6.x the module parameter is
absent and only the sysctl is present):

```text
Y
cat: /sys/module/apparmor/parameters/restrict_unprivileged_userns: No such file or directory
1
unconfined
```

The last line is the one that decides the outcome: `unconfined` means no `mur-sealed*` profile is
confining this binary, so with the restriction on, the mechanism is unavailable.

Then:

```sh
mur run --explain-scope
```

Expected (the `reason:` line is one long line; wrapped here for the page):

```text
Containment
  declared:  sealed
  achieved:  scoped
  floor met: no
  reason:    sealed requires an unprivileged user+mount namespace, and AppArmor's
             unprivileged-userns restriction is active on this host while the 'mur-sealed'
             profile is not confining this binary. Install and load the profile shipped with
             mur: `sudo install -m 644 packaging/apparmor/mur-sealed
             /etc/apparmor.d/mur-sealed && sudo apparmor_parser -r
             /etc/apparmor.d/mur-sealed` (or re-run the mur installer as root), then re-run.
             To turn the restriction off host-wide instead: `sudo sysctl -w
             kernel.apparmor_restrict_unprivileged_userns=0`.
  mechanism: landlock+seccomp
```

And the real refusal — note it exits non-zero, and that nothing was staged:

```sh
mur run; echo "exit=$?"
```

Expected:

```text
error[E-CAP-003]: declared containment class 'sealed' is not achievable on this host (achieved: 'scoped'): sealed requires an unprivileged user+mount namespace, and AppArmor's unprivileged-userns restriction is active on this host while the 'mur-sealed' profile is not confining this binary. Install and load the profile shipped with mur: `sudo install -m 644 packaging/apparmor/mur-sealed /etc/apparmor.d/mur-sealed && sudo apparmor_parser -r /etc/apparmor.d/mur-sealed` (or re-run the mur installer as root), then re-run. To turn the restriction off host-wide instead: `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0`.
  hint: lower the declared floor to 'scoped' (capabilities.containment in murmur.yaml, containment in .murmur/config.yaml, or --containment), or run on a host that provides 'sealed'
exit=1
```

The refusal must be `achieved: 'scoped'`, **not** a session that ran. A run that succeeded here is
the single most important failure this page can catch.

## Step 2 — load the profile and confirm the host now reaches `sealed`

```sh
sudo install -m 644 packaging/apparmor/mur-sealed /etc/apparmor.d/mur-sealed
sudo apparmor_parser -r /etc/apparmor.d/mur-sealed
sudo aa-status | grep -c mur-sealed
```

Expected: `2` (the two profiles in the file — `mur-sealed` for system installs and
`mur-sealed-home` for `~/.local/bin` and `~/.cargo/bin`).

The profile attaches by executable path. Confirm the `mur` you are about to run is one it covers:

```sh
command -v mur
```

Expected: one of `/usr/local/bin/mur`, `/usr/bin/mur`, `/opt/mur/bin/mur`, `~/.local/bin/mur`,
`~/.cargo/bin/mur`. **If you are running a `cargo build` binary out of `./target`, it is not
covered** — either install it to one of those paths, or add a third profile as shown in the comment
header of `packaging/apparmor/mur-sealed`.

Then:

```sh
mur run --explain-scope
```

Expected:

```text
Containment
  declared:  sealed
  achieved:  sealed
  floor met: yes
  mechanism: mountns+pivot_root+landlock+seccomp
```

## Step 3 — the composed root, from inside

Run each probe as a shell tool inside the capsule. The exact invocation depends on how you drive the
capsule; the reliable path is a plan step or a single agent turn asking for the command verbatim.
What matters is that the command runs through `capabilities.shell.allow`, i.e. through
`shell::execute_shell`, which is the only path this mechanism covers.

### 3a — the fixed outside-the-root target list

```sh
for p in /etc/shadow /root /var/run/docker.sock /run/docker.sock /dev/sda /dev/nvme0n1 /home /etc/sudoers /etc/ssh; do
  printf '%-24s ' "$p"
  stat -c '%n exists' "$p" 2>&1 | tail -1
done
```

Expected — every line `No such file or directory`, and **not** `Permission denied`:

```text
/etc/shadow              stat: cannot statx '/etc/shadow': No such file or directory
/root                    stat: cannot statx '/root': No such file or directory
/var/run/docker.sock     stat: cannot statx '/var/run/docker.sock': No such file or directory
/run/docker.sock         stat: cannot statx '/run/docker.sock': No such file or directory
/dev/sda                 stat: cannot statx '/dev/sda': No such file or directory
/dev/nvme0n1             stat: cannot statx '/dev/nvme0n1': No such file or directory
/home                    stat: cannot statx '/home': No such file or directory
/etc/sudoers             stat: cannot statx '/etc/sudoers': No such file or directory
/etc/ssh                 stat: cannot statx '/etc/ssh': No such file or directory
```

!!! note "`/home` is the one line to read carefully"

    If the session workdir lives under `/home` — the usual case — then `/home` and every directory
    component down to the workdir **does** exist inside the composed root, because the workdir is
    reproduced at its own absolute path. Expect `/home` to `stat` successfully in that case, and
    check instead that it is empty apart from the single path leading to the workdir:

    ```sh
    find /home -maxdepth 4 | sort
    ```

    Expected: only the components of the workdir path, and nothing else — no other user's home, no
    `.ssh`, no dotfiles.

`open` as well as `stat`, since a denial and an absence can differ between the two:

```sh
cat /etc/shadow; echo "exit=$?"
```

Expected:

```text
cat: /etc/shadow: No such file or directory
exit=1
```

### 3b — the root's own shape

```sh
ls -1 /
```

Expected — the composed root and nothing else (entries vary with the host's usrmerge layout; `bin`,
`sbin`, `lib*` appear as symlinks into `usr` on a modern distro):

```text
bin
dev
etc
home
lib
lib64
proc
sbin
tmp
usr
```

```sh
ls -1 /etc
```

Expected — the allowlist from `sealed::SEALED_ETC_PATHS`, minus whatever this host does not have,
and **nothing else**:

```text
alternatives
ca-certificates
ca-certificates.conf
group
hosts
ld.so.cache
ld.so.conf
ld.so.conf.d
localtime
nsswitch.conf
passwd
resolv.conf
ssl
terminfo
```

### 3c — the private `/dev`

```sh
ls -1 /dev
```

Expected — the OCI default device set, the `devpts` mount, and the OCI symlinks:

```text
fd
full
null
ptmx
pts
random
stderr
stdin
stdout
tty
urandom
zero
```

```sh
stat -c '%n %F %t:%T' /dev/null /dev/zero /dev/full /dev/random /dev/urandom
```

Expected (major:minor in hex — `1:3`, `1:5`, `1:7`, `1:8`, `1:9`):

```text
/dev/null character special file 1:3
/dev/zero character special file 1:5
/dev/full character special file 1:7
/dev/random character special file 1:8
/dev/urandom character special file 1:9
```

`/dev/null` must still be writable even though `/dev` is a read-only mount — writing to a character
special file bypasses the mount's read-only check, which is why the mount can be sealed without
breaking `2>/dev/null`:

```sh
echo hello > /dev/null; echo "exit=$?"
```

Expected:

```text
exit=0
```

And no block device reachable at all:

```sh
ls /dev/sd* /dev/nvme* /dev/loop* 2>&1
```

Expected:

```text
ls: cannot access '/dev/sd*': No such file or directory
ls: cannot access '/dev/nvme*': No such file or directory
ls: cannot access '/dev/loop*': No such file or directory
```

### 3d — writability

The session workdir is the only writable path. `$PWD` is the workdir, at the same absolute path it
has on the host:

```sh
pwd
touch ./writable-here && echo "workdir: writable"
touch /usr/writable-there 2>&1
touch /etc/writable-there 2>&1
touch /writable-there 2>&1
```

Expected (the `pwd` line is the host path of the session workdir — record it verbatim):

```text
/home/<you>/sealed-check/workdir
workdir: writable
touch: cannot touch '/usr/writable-there': Read-only file system
touch: cannot touch '/etc/writable-there': Read-only file system
touch: cannot touch '/writable-there': Read-only file system
```

`/tmp` is writable, and it is backed by a directory *inside* the workdir — so it counts against
`capabilities.resources.workdir_max_bytes` and is discarded with the session:

```sh
echo probe > /tmp/sealed-probe && cat /tmp/sealed-probe
```

Expected:

```text
probe
```

Confirm from the **host** side, after the session ends, that the bytes landed in the workdir:

```sh
cat ~/sealed-check/workdir/.mur-tmp/sealed-probe
```

Expected:

```text
probe
```

### 3e — `/proc`

```sh
grep " /proc " /proc/self/mountinfo
ls /proc | grep -c '^[0-9]'
```

Expected: `/proc` exists (so tooling that reads `/proc/self/*` keeps working), carries
`nosuid,nodev,noexec`, and — on a bare host — is a **bind of the host's `/proc`**, so the pid count
on the second line is the host's:

```text
2096 2072 0:26 / /proc rw,nosuid,nodev,noexec,relatime - proc proc rw
356
```

!!! warning "Known limitation, not a defect to file: `/proc` is not masked on a bare host"

    Mounting a private `procfs` needs `CAP_SYS_ADMIN` over the user namespace owning the **PID**
    namespace. `unshare(CLONE_NEWUSER | CLONE_NEWNS)` leaves the process in the host's initial PID
    namespace, owned by the initial user namespace, where an unprivileged process has nothing — so
    all three `hidepid` spellings the runtime tries (`hidepid=2`, `hidepid=invisible`, unmasked;
    `sealed::PROC_HIDEPID_OPTIONS`) return `EPERM` and the executor falls back to binding the
    host's `/proc`. Adding `CLONE_NEWPID` does not fix it: `unshare` moves only *future children*
    into the new PID namespace, so the mounting process is still judged against the old one. The
    real fix is to fork so the child becomes PID 1, which changes reaping and signal semantics for
    the whole capsule subprocess tree and is deliberately out of scope here.

    What this costs, stated plainly: host PIDs are enumerable and `/proc/<pid>/cmdline`,
    `/proc/<pid>/root` and `/proc/<pid>/cwd` are nameable from inside the composed root. `/proc` is
    the **one** part of the root where "outside does not exist" degrades to `scoped`'s "outside is
    denied" — Landlock's ruleset covers no path under `/proc`, so opens through it are refused, and
    `ptrace_may_access` gates the rest. Every other axis (steps 3a–3d) is absolute. Verify it
    yourself here rather than taking this paragraph's word for it:

    ```sh
    cat /proc/1/cmdline; echo
    cat /proc/1/environ 2>&1 | head -c 60; echo
    ```

    Expected — the host's init is *nameable* but its environment is not readable:

    ```text
    /sbin/init splash
    cat: /proc/1/environ: Permission denied
    ```

    On a host where `mount -t proc` *does* succeed (running as root, or in a container started with
    `--cap-add SYS_ADMIN`), the mountinfo line reads `- proc proc rw,hidepid=...` instead and the
    pid count drops to the capsule's own processes. Record which of the two you got.

## Step 4 — `trace.jsonl`

The trace lives under `<workdir>/.murmur/<session-id>/trace.jsonl`, one directory per session — not
at the top of the workdir. From the host, after the session:

```sh
find ~/sealed-check/workdir -name trace.jsonl \
  -exec grep -o '"containment_[a-z]*":"[a-z]*"' {} \; | head -2
```

Expected:

```text
"containment_declared":"sealed"
"containment_achieved":"sealed"
```

Launching at all needs a delegated cgroup scope, exactly as
[resource-limits verification](resource-limits-manual-verification.md) requires — without it the
launch refuses with `E-RUN-012` before reaching `session_start`, and the workdir must already
exist:

```sh
mkdir -p ~/sealed-check/workdir
systemd-run --user --scope --property=Delegate=yes -q \
  mur run --workdir ~/sealed-check/workdir
```

## Step 5 — the container refusal

The same capsule, in a plain container with no added capabilities. Run from the directory holding
`murmur.yaml`:

```sh
docker run --rm -v "$PWD":/work -w /work -v "$(command -v mur)":/usr/local/bin/mur:ro \
  ubuntu:24.04 mur run; echo "exit=$?"
```

Expected — a refusal naming both remediations, and **no session**:

```text
error[E-CAP-003]: declared containment class 'sealed' is not achievable on this host (achieved: 'scoped'): sealed requires unshare(CLONE_NEWUSER | CLONE_NEWNS), which this host refused. This is the usual answer inside a container: CAP_SYS_ADMIN is absent, or the container's own seccomp filter blocks unshare(2). Either add `--cap-add SYS_ADMIN` to the container invocation, or establish the mount namespace outside the container and run mur inside it. The runtime will not fall back to a weaker class.
  hint: lower the declared floor to 'scoped' (capabilities.containment in murmur.yaml, containment in .murmur/config.yaml, or --containment), or run on a host that provides 'sealed'
exit=1
```

The reason string may instead be the `MountDenied` variant, if this container's runtime lets
`unshare` through and refuses the subsequent `mount(2)`. Both are correct; record which one you got.

Then confirm the stated remediation actually works:

```sh
docker run --rm --cap-add SYS_ADMIN -v "$PWD":/work -w /work \
  -v "$(command -v mur)":/usr/local/bin/mur:ro ubuntu:24.04 mur run --explain-scope
```

Expected:

```text
Containment
  declared:  sealed
  achieved:  sealed
  floor met: yes
  mechanism: mountns+pivot_root+landlock+seccomp
```

If it still refuses with `--cap-add SYS_ADMIN`, the container's own seccomp profile is blocking
`unshare(2)`; add `--security-opt seccomp=unconfined` and record that you needed it.

## Step 6 — negative control

A procedure that cannot fail proves nothing. Confirm the probes above would have caught an
unsealed root: run the identical commands from step 3a with the floor lowered, so the capsule runs
at `scoped` over the host filesystem.

```sh
mur run --containment scoped
```

Under `scoped`, `stat /etc/shadow` must fail with **`Permission denied`** (Landlock denying an
existing file) rather than `No such file or directory`, and `ls /` must show the host's real root.
If step 3a's output is identical under both classes, the probes are not measuring what this page
claims and the result must not be recorded as a pass.

## Recording the result

### Run of 2026-08-07 — directory enumeration inside the composed root (`SEALED_RUNTIME_PATHS`)

A targeted run, not a re-run of steps 1–6: it records what happens for `ls` on a path that *is* one
of the composed root's bind-mounted runtime directories, which no earlier run on this page covered.
The 2026-08-05 entry recorded `ls /` and `ls /etc` as `Permission denied` and cited that as evidence
Landlock still mediates *inside* the root. Both remain true. What was not recorded is that
`ls /usr/lib/python3.12` failed the same way — and that this made a `sealed` capsule with `python3`
in `shell.allow` unable to start Python at all.

**Host.** Same machine as the 2026-08-05 run: `Linux 7.0.0-28-generic #28~24.04.1-Ubuntu SMP
PREEMPT_DYNAMIC Wed Jul 1 15:50:57 UTC 2 x86_64`, Ubuntu 24.04, `systemd-detect-virt` → `none`,
non-root (`uid=1000`), `kernel.apparmor_restrict_unprivileged_userns=0` (so no profile was needed),
CPython 3.12.3 under `/usr/lib/python3.12`. Both binaries built from the same worktree; sessions
launched under `systemd-run --user --scope -p Delegate=yes` with
`capabilities.resources.max_processes: 4096` (the `bfb08018` `RLIMIT_NPROC` baseline defect —
unrelated to this page). Capsule: `containment: sealed`, `shell.allow: [bash, python3, ls, touch]`,
**no** `interpreter_runtime` and **no** `staged_runtime`, so the fixed bind list is the only thing
under test. `mur run --explain-scope` → `achieved: sealed`; `trace.jsonl` `session_start` →
`"containment_achieved":"sealed"`.

**Before (control, `mur` built without the grant)** — one real `bash` tool call, verbatim:

```text
--1-PY--
Python path configuration:
  ...
  stdlib dir = '/usr/lib/python3.12'
  sys.path = [
    '/usr/lib/python312.zip',
    '/usr/lib/python3.12',
    '/usr/lib/python3.12/lib-dynload',
  ]
Fatal Python error: init_fs_encoding: failed to get the Python codec of the filesystem encoding
Python runtime state: core initialized
ModuleNotFoundError: No module named 'encodings'
rc=1
--5-WRITE--
touch: cannot touch '/usr/testfile': Read-only file system
```

Every `ls` in that same run produced no output at all, which is itself informative: the pipeline's
`head` could not `execve` either, because `/usr/bin/head` is not in `shell.allow`. Probed directly,
verbatim:

```text
bash: line 1: /usr/bin/whoami: Permission denied     rc=126
bash: line 1: /usr/bin/basename: Permission denied   rc=126
bash: line 1: /bin/sh: Permission denied             rc=126
```

**After (the grant in place)** — same capsule, same command:

```text
--1-PY--
ok
rc=0
--2-DLOPEN--
dlopen-ok            (python3 -c "import ssl, zlib, _ctypes, json, sqlite3")
rc=0
--4-LSROOTDIR--
ls: cannot open directory '/': Permission denied
--5-LSROOT--
ls: cannot access '/root': No such file or directory
--6-WRITE--
touch: cannot touch '/usr/testfile': Read-only file system
--7-LSUSR--
bin games include lib lib64 libexec local sbin share src
--8-EXEC--
bash: line 1: /bin/sh: Permission denied             rc=126
bash: line 1: /usr/bin/basename: Permission denied   rc=126
```

and, in a second call (using bash builtins to count, since `wc`/`head` are correctly still
un-runnable):

```text
--A-LSDIR--
total 56
drwxr-xr-x 1 65534 65534   118 Jul 26 17:20 .
drwxr-xr-x 1 65534 65534  4080 Jul 26 17:20 ..
-rw-r--r-- 1 65534 65534 12473 Jun 19 07:46 decoder.py
-rw-r--r-- 1 65534 65534 16070 Jun 19 07:46 encoder.py
-rw-r--r-- 1 65534 65534 14020 Jun 19 07:46 __init__.py
drwxr-xr-x 1 65534 65534   226 Jul 26 17:20 __pycache__
-rw-r--r-- 1 65534 65534  2425 Jun 19 07:46 scanner.py
-rw-r--r-- 1 65534 65534  3339 Jun 19 07:46 tool.py
--B-COUNT--
entries=200
```

Read against the four boundaries this page cares about:

| probe | before | after | verdict |
|---|---|---|---|
| `python3 -c "import ast; print('ok')"` | `init_fs_encoding` fatal, rc=1 | `ok`, rc=0 | fixed |
| `ls -la` inside `/usr/lib/python3.12` | denied | real entries (200 in the stdlib dir) | fixed |
| `ls /` (composed root's own top level) | `Permission denied` | `Permission denied` | unchanged ✅ |
| `ls /root` (never mounted) | absent | `No such file or directory` | unchanged ✅ |
| `touch /usr/testfile` | `Read-only file system` | `Read-only file system` | unchanged ✅ |
| `/bin/sh`, `/usr/bin/basename` (not in `shell.allow`) | `Permission denied`, rc=126 | `Permission denied`, rc=126 | unchanged ✅ |

**The last row is the one worth reading twice.** Granting these paths `Execute` alongside `ReadDir`
— the obvious implementation, and the one every other grant in the runtime uses — makes `/bin/sh`
and every other host binary runnable inside a `sealed` capsule, because Landlock `Execute` *is* this
runtime's exec allowlist since the seccomp `execve` supervisor was retired. That was observed on
this host during this run, not theorised, and is why the grant withholds `Execute`. `dlopen(3)` is
unaffected (row 2 above: `ssl`, `zlib`, `_ctypes` and `sqlite3` all import), because mapping a
shared object `PROT_EXEC` needs `ReadFile`, not `Execute`.

**Negative control — `scoped` is untouched.** The identical capsule with `containment: scoped`,
run through the same live shell-tool path, before and after:

```text
--1-LSROOTDIR--  ls: cannot open directory '/': Permission denied                   rc=2
--2-LSUSR--      ls: cannot open directory '/usr': Permission denied                rc=2
--3-LSPY--       ls: cannot open directory '/usr/lib/python3.12': Permission denied rc=2
--4-PY--         Fatal Python error: init_fs_encoding: ...                          rc=1
--5-EXEC--       bash: line 1: /bin/sh: Permission denied                           rc=126
```

`diff` of the two transcripts is empty (modulo CPython's thread-id line): **byte-identical before
and after.** This is the property the tier gate exists for — under `scoped` there is no composed
root, `/usr` is literally the host's, and no new host directory became enumerable.

**Not run.** Step 5 (the container refusal) remains unrun on this host for the same reason as
before — no container runtime installed. This entry adds nothing to it.

### Run of 2026-08-05 — steps 1–4 and 6 pass through a live capsule, step 5 not run

**Host.** Same machine as the 2026-08-03 run: `Linux 7.0.0-28-generic #28~24.04.1-Ubuntu SMP`,
Ubuntu 24.04, no container runtime, non-root (`uid=1000`). Profile reloaded and verified
byte-identical to `packaging/apparmor/mur-sealed` before relying on it.

**Step 1 — PASS**, reproduced live: profile unloaded, restriction on → `error[E-CAP-003]`,
`achieved: 'scoped'`, naming the profile and the exact remediation, no workdir created. Host
restored (`kernel.apparmor_restrict_unprivileged_userns=0`, profile reloaded) immediately after.

**Step 2 — PASS**, reproduced live via `mur run --explain-scope`.

**Step 3 — PASS, driven through a real, live capsule session (not a harness).** A capsule
declaring `containment: sealed` with `shell.allow: [bash, ls, cat, stat, readlink]`, launched
under `systemd-run --user --scope --property=Delegate=yes`, with the `claude` CLI as
`inference.command`, issued real `bash` tool calls. Observed, byte for byte:

- `cat /etc/shadow` → `No such file or directory`.
- The full 3a target list (`/etc/shadow`, `/root`, `/var/run/docker.sock`, `/run/docker.sock`,
  `/dev/sda`, `/dev/nvme0n1`, `/etc/sudoers`, `/etc/ssh`) → `stat` reports every one absent
  (`cannot statx ...: No such file or directory`); `/home` alone reports present, which is the
  documented workdir-scaffold exception.
- `echo hi > /usr/testfile` → `Read-only file system`; `echo hi > "$PWD/canary.txt"` → succeeds,
  and the byte written is visible at the same path on the host (the bind-mount identity holds).
- `ls /` and `ls /etc` → `Permission denied` (Landlock denying `ReadDir` on paths outside the
  workdir and outside any derived exec grant) — a stricter outcome than a bare "absent" claim
  requires, and direct, live evidence that Landlock is still mediating access **inside** the
  composed root, not just at the mount-namespace boundary. This closes the one gap the
  2026-08-03 run left open (the harness stopped before Landlock/seccomp installed).
- One environmental note for future runs on a busy host: `capabilities.resources.max_processes`
  (default 128 headroom) is computed against a *process* count, not the uid's total *thread*
  count; on a host running many multi-threaded processes under the same uid, `RLIMIT_NPROC` can
  land below the uid's actual task count and every `fork()` inside the sandbox fails with
  `bash: fork: retry: Resource temporarily unavailable` — reproduced identically at `scoped`, so
  it is unrelated to the composed root. Declaring a larger `max_processes` in the test manifest
  works around it. This is pre-existing (`resources::uid_process_count`, untouched by this
  slice) and out of scope here; flagged for a follow-up.

**Step 4 — PASS**, reproduced live: `trace.jsonl`'s `session_start` carries
`"containment_declared":"sealed"` / `"containment_achieved":"sealed"`.

**Step 5 — still NOT RUN.** No container runtime is installed on this host and none could be
installed. Still the one acceptance criterion on this page with no observed result.

**Step 6 — PASS, reproduced live for the first time.** A second capsule declaring
`containment: scoped` (otherwise identical), run through the same live shell-tool path:
`cat /etc/shadow` → `Permission denied`; `stat /etc/shadow` → succeeds, with real metadata
(`size=1338 mode=100640 uid=0`). This is the exact contrast step 6 requires — `sealed` reports
absence, `scoped` reports permission-denied-on-a-visible-file — and it had not been produced
through a live capsule before this run.

### Run of 2026-08-03 — steps 1–4 pass, step 5 not run, step 3 driven by a harness

**Host.** `Linux 7.0.0-28-generic #28~24.04.1-Ubuntu SMP PREEMPT_DYNAMIC x86_64`, Ubuntu 24.04,
`systemd-detect-virt --container` → `none`, `CapBnd: 000001ffffffffff`, `Seccomp: 0`, PID 1 is
`systemd`, non-root (`uid=1000`). `/sys/module/apparmor/parameters/restrict_unprivileged_userns`
does not exist on this kernel build; `/proc/sys/kernel/apparmor_restrict_unprivileged_userns` is the
live knob, as step 1 anticipates. Profiles loaded: `mur-sealed.166`, `mur-sealed-home.167`.

**Step 1 — AppArmor-absent refusal: PASS.** With the profile unloaded and the restriction on,
`mur run` refused with `error[E-CAP-003]`, `achieved: 'scoped'`, naming the profile and the exact
`apparmor_parser -r` command, exit 1, and no workdir or trace was created.

**Step 2 — the host reaches `sealed`: PASS.**

```text
Containment
  declared:  sealed
  achieved:  sealed
  floor met: yes
  mechanism: mountns+pivot_root+landlock+seccomp
```

**Step 3 — the composed root: PASS, with the deviation below.** Every probe in 3a–3e produced the
output this page states. Notably: all nine 3a targets reported `No such file or directory` (never
`Permission denied`); `ls -1 /` produced exactly `bin dev etc home lib lib64 proc sbin tmp usr`;
`ls -1 /etc` produced only the `SEALED_ETC_PATHS` entries this host has; `/dev` carried exactly the
OCI default set at `1:3 1:5 1:7 1:8 1:9` with `/dev/null` still writable and no block device
reachable; the workdir was writable at its own absolute path while `/usr`, `/etc` and `/` reported
`Read-only file system`; and `/tmp` was backed by `<workdir>/.mur-tmp`. `/proc` came out as the
bind fallback (`- proc proc rw`, `nosuid,nodev,noexec`, 356 host PIDs visible), which is the
documented outcome for a bare host — see the warning under step 3e.

*Deviation:* the 3a–3e probes were run through a temporary in-crate harness that calls
`sealed::plan_composed_root` → `build_sealed_root_spec` → `construct_composed_root` in a forked
child and then `execve`s `/bin/sh`, rather than through a live capsule's shell tool. The harness was
deleted after the run. The reason is a **separate, pre-existing defect that is not specific to
`sealed`**: `mur` calls `security::harden_process_dumpable()` at startup, so the seccomp-notify
supervisor cannot read `/proc/<child>/mem`, `classify_and_decide` fail-closes, and every
allowlisted `execve` is refused with `EACCES`. It reproduces identically at `--class scoped`
(`escape-conformance --class scoped` refuses at preflight with the same message) and the
`escape-conformance` runner on `main` already documents it. Because of it, no capsule on this host
can run a shell tool at any class, so step 3 could not be driven the way this page describes.
Consequence for the reader: the composed root itself is verified; what is *not* verified end-to-end
here is that Landlock and seccomp behave correctly **inside** it, since the harness stops before
those steps.

**Step 4 — `trace.jsonl`: PASS.**

```text
"containment_declared":"sealed"
"containment_achieved":"sealed"
```

Read from `~/sealed-check/workdir/.murmur/ses_019fc9e0643f7b218186aa07204443f6/trace.jsonl`, from a
session launched under `systemd-run --user --scope --property=Delegate=yes`.

**Step 5 — the container refusal: NOT RUN.** No container runtime is installed on this host
(`command -v docker podman` finds nothing) and none could be installed. This is the one acceptance
criterion on this page with no observed result; it must be run before the container path is treated
as verified.

**Step 6 — negative control: NOT RUN**, for the same reason step 3 needed a harness — `scoped`
cannot execute a shell tool on this host either, so the `Permission denied`-vs-`No such file or
directory` contrast could not be produced through a capsule. The contrast *was* observed in the
other direction: the identical 3a probe list returns `No such file or directory` inside the composed
root while the same paths exist and `stat` cleanly outside it.

### What still needs a hand-run

- Step 5, on a host with a container runtime. This is the only remaining gap; every other step has
  now been run end to end through a live capsule's shell tool (see the 2026-08-05 run above).

## Residuals recorded here rather than buried

These are known, deliberate gaps in what the composed root delivers. None of them is a defect to
file; each is a scoping decision this slice made explicitly.

- **No PID namespace, so `/proc` is a bind of the host's rather than a masked private `procfs`.**
  This is the one place the composed root's "outside does not exist" promise degrades to `scoped`'s
  "outside is denied". See the warning under step 3e for the kernel rule that forces it and for what
  it costs.
- **`/etc/passwd` and `/etc/group` are bind-mounted from the host**, so the host's account names are
  visible inside the root. They are world-readable on every distribution and `getpwuid(3)` needs
  them; synthesising a two-line pair in the parent is the obvious follow-up. See the doc comment on
  `sealed::SEALED_ETC_PATHS`.
- **Read-only binds are not recursively read-only.** A submount underneath a bound directory (a
  separate `/usr/local` partition, say) keeps its own mount flags. `MS_REMOUNT | MS_BIND | MS_RDONLY`
  applies to one mount; walking the subtree is a follow-up.
- **`devpts` and `/proc` are new inodes the parent could not pre-open**, so Landlock — which builds
  its rules from parent-opened file descriptors — has no rule covering them. `/dev/ptmx` and
  `/proc/self/*` therefore exist in the root but are not openable. This matches the existing
  `scoped` tier, which grants neither, so it is not a regression; it is worth knowing before
  debugging a pty failure.
- **The composed root applies only to the shell-tool subprocess path.** `runtime::dispatch_native_tool`
  attaches fd hygiene but not the full enforcement chain — a pre-existing, documented gap that
  predates this work and is unchanged by it.
