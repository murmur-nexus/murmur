# Verification — sealed containment (mount namespace + `pivot_root`)

!!! success "Status: **RUN — 2026-08-05, partial.** Steps 1–4 and 6 pass on a real host through a live capsule's shell tool; step 5 (container) was not run. Addenda: 2026-08-07, directory enumeration inside the composed root; 2026-08-08, reading its `/etc` allowlist and completing a verified TLS handshake; 2026-08-09, the first full-registry escape-conformance run at `--class sealed` against a *gated* suite (exit 0, 26 of 28 cases asserted and passing)."

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
    runtime directories — and is recorded in the same section. A second targeted run on 2026-08-08
    did the same for the composed root's `/etc` allowlist, where the same mounted-but-denied defect
    was breaking TLS certificate verification, and carries a completed `pip install` over a verified
    HTTPS connection. The **first entry** under Recording the result is now the 2026-08-09 run: the
    first time the escape-conformance suite's `sealed` column *graded* anything rather than recording
    an intention, and therefore the first `sealed`-class result whose exit code is citable.

    **The steps dated 2026-08-05 to 2026-08-09 above were run with
    `kernel.apparmor_restrict_unprivileged_userns=0`** — that is, with the host's unprivileged-userns
    hardening switched off for every binary on the machine rather than granted to `mur` by the
    profile. The runtime names that provenance (`userns grant: restriction_disabled_host_wide`,
    [`W-SEC-013`](diagnostics.md#w-sec-013)). So the composed root, its `/etc` allowlist, the
    negative control and the escape-conformance run are confirmed on a host where **the `mur-sealed`
    AppArmor profile was not what permitted the user namespace**.

    **Steps 1 and 2 through the profile — 2026-08-24.** With
    `kernel.apparmor_restrict_unprivileged_userns=1` and `/etc/apparmor.d/mur-sealed` loaded, the
    same build was run from a path the profile attaches to (`~/.local/bin/mur`) and from a checkout
    path it does not (`./target/debug/mur`):

    | Binary path | `userns grant:` | `achieved:` |
    |---|---|---|
    | `~/.local/bin/mur` (profile attaches) | `profile_confining` | `sealed` |
    | `./target/debug/mur` (no profile attaches) | `withheld` | `scoped`, refused at a declared `sealed` floor |

    A capsule declaring `capabilities.containment: sealed` ran to completion from the attached path,
    and its `session_start` event recorded `"containment_achieved": "sealed"` with
    `"userns_grant": "profile_confining"`. Steps 3, 4 and 6 have not been re-run under that posture.

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
[fd-hygiene procedure](subprocess-fd-hygiene-verification.md#what-this-deliberately-is-not) out of
the automated suite.

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
| the `mur` under test is at a path a profile attaches to | `command -v mur` | an installed path, or a checkout build with `scripts/install-dev-apparmor.sh` loaded |
| a non-root login shell | `id -u` | non-zero (the mechanism needs no host root) |
| systemd user session with cgroup delegation | see [resource-limits verification](resource-limits-manual-verification.md) | already required for any capsule that spawns a subprocess |

Steps 1 and 2 assume an Ubuntu 23.10+ host (`kernel.apparmor_restrict_unprivileged_userns=1`). On a
host without AppArmor (Fedora, Arch, Debian without the restriction), step 1 does not apply — record
that, skip to step 2, and note in the result which host you used.

The shipped profile attaches to installed paths only, so a `cargo build` binary at
`./target/debug/mur` gets no grant from it. Rather than switching the restriction off — which grants
unprivileged user namespaces to every program on the machine and is reported as
[`W-SEC-013`](diagnostics.md#w-sec-013) — generate and load a profile for this checkout's two target
paths:

```sh
scripts/install-dev-apparmor.sh --print     # inspect it; writes nothing, needs no privilege
sudo scripts/install-dev-apparmor.sh        # parse-check, install to /etc/apparmor.d/mur-sealed-dev, load
```

`mur doctor` then reports `userns grant: profile_confining`. Which grant is in effect must be
recorded with every result on this page: an `achieved: sealed` reached through the profile and one
reached with the restriction off are different results.

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
             Building out of a checkout, where the binary sits at
             ./target/{debug,release}/mur and no shipped profile attaches to it: run
             `scripts/install-dev-apparmor.sh`, which generates and loads the same grant for
             those two paths. LAST RESORT, only where a profile genuinely cannot be loaded:
             `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0` — this removes
             unprivileged-userns hardening from every program on the machine, not just from
             mur, and is not the configuration murmur ships.
  mechanism: landlock+seccomp
  userns grant: withheld
```

And the real refusal — note it exits non-zero, and that nothing was staged:

```sh
mur run; echo "exit=$?"
```

Expected:

```text
error[E-CAP-003]: declared containment class 'sealed' is not achievable on this host (achieved: 'scoped'): sealed requires an unprivileged user+mount namespace, and AppArmor's unprivileged-userns restriction is active on this host while the 'mur-sealed' profile is not confining this binary. Install and load the profile shipped with mur: `sudo install -m 644 packaging/apparmor/mur-sealed /etc/apparmor.d/mur-sealed && sudo apparmor_parser -r /etc/apparmor.d/mur-sealed` (or re-run the mur installer as root), then re-run. Building out of a checkout, where the binary sits at ./target/{debug,release}/mur and no shipped profile attaches to it: run `scripts/install-dev-apparmor.sh`, which generates and loads the same grant for those two paths. LAST RESORT, only where a profile genuinely cannot be loaded: `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0` — this removes unprivileged-userns hardening from every program on the machine, not just from mur, and is not the configuration murmur ships.
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
covered** — either install it to one of those paths, or run `sudo scripts/install-dev-apparmor.sh`,
which generates and loads a profile for this checkout's `target/debug/mur` and
`target/release/mur`.

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
  userns grant: profile_confining
```

`userns grant: profile_confining` is the line that makes this step's result meaningful. Reading
`restriction_disabled_host_wide` here means the namespace came from
`kernel.apparmor_restrict_unprivileged_userns` being off host-wide and the profile you just loaded
was not what permitted it — a different, weaker configuration that reaches the same `achieved:
sealed`. See [`W-SEC-013`](diagnostics.md#w-sec-013).

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
ls -a /tmp
```

Expected:

```text
probe
.
..
sealed-probe
```

Confirm from the **host** side, after the session ends, that the bytes landed in the workdir:

```sh
cat ~/sealed-check/workdir/.mur-tmp/sealed-probe
```

Expected:

```text
probe
```

The mount alone is not enough, and this is worth checking with the tools that actually broke.
`plan_composed_root` has bind-mounted `.mur-tmp` at `/tmp` read-write since the composed root
existed, but Landlock inside the root matches an access to `/tmp/x` against the **bind's own root
inode**, never the workdir's — so until `apply_landlock_scope` grew a rule for it, every one of
these failed `EACCES` on a genuinely writable mount:

```sh
python3 -c "open('/tmp/probe','w').write('y')"; echo "rc=$?"; cat /tmp/probe; echo
mktemp; echo "rc=$?"
printf '#include <stdio.h>\nint main(void){puts("hi");return 0;}\n' > t.c; cc t.c -o t; echo "rc=$?"
```

Expected — the Python write succeeds and reads back `y`, `mktemp` prints a path under `/tmp` and
exits 0, and `cc` exits 0 with no `Cannot create temporary file in /tmp/` line:

```text
rc=0
y
/tmp/tmp.IOF7QFO79k
rc=0
rc=0
```

!!! note "`cc t.c -o t` compiles under `sealed`; `./t` does not, and that is a different rule"

    `/tmp` carries the *same* Landlock right-set as the workdir — including the `Execute` bit,
    which is granted only when `capabilities.filesystem.workdir_exec: true`. And a capsule
    declaring `workdir_exec: true` cannot reach `sealed` at all: it is capped at `advisory` by
    `containment::achieved_containment_class`, so `mur run --explain-scope` reports
    `achieved: advisory / floor met: no` for a `sealed` manifest that declares it. A real `sealed`
    session therefore always has `workdir_exec: false`, and running the binary it just compiled —
    from the workdir *or* from `/tmp` — is refused with `Permission denied` (rc 126). Compiling is
    the part this page's `/tmp` grant restores; running the output is `workdir_exec`'s separate,
    unchanged decision, verified on
    [its own page](workdir-exec-landlock-manual-verification.md).

The size guard needs no `/tmp`-specific check and must not grow one: `.mur-tmp` is an ordinary
subdirectory of the workdir, so `resources::directory_size_bytes` already walks it. With a small
`capabilities.resources.workdir_max_bytes` declared, filling `/tmp` past the ceiling must latch the
same breach an oversized write straight into the workdir would:

```sh
python3 -c "open('/tmp/big','w').write('x'*4000000)"
```

Expected, on the *next* shell tool call (the guard checks on an interval, then refuses at the spawn
boundary rather than mid-write):

```text
workdir grew to 4000000 bytes, past the 1000000 byte ceiling (capabilities.resources.workdir_max_bytes)
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

### Run of 2026-08-09 — synthetic `/etc/passwd` and `/etc/group` (card `60e1c285`)

The narrowing the 2026-08-08 `/etc`-allowlist entry below flagged as its own residual: that entry
made the host's account databases *readable* inside a composed root (`OPEN ok /etc/passwd
bytes=2910` — 50 accounts, every login name on this machine). The parent now writes a two-line
passwd/group instead, and the composed root binds those at the same two paths.

**Host.** Same machine as every run below: `Linux 7.0.0-28-generic #28~24.04.1-Ubuntu SMP
PREEMPT_DYNAMIC Wed Jul 1 15:50:57 UTC 2 x86_64`, Ubuntu 24.04, `systemd-detect-virt` → `none`
(bare metal), non-root `uid=1000 gid=1000`, `kernel.apparmor_restrict_unprivileged_userns=0` so no
profile was needed. CPython 3.12.3. The host's own databases for comparison: `/etc/passwd` 50
lines, `/etc/group` 77 lines.

**Harness.** The same route as the 2026-08-08 entry — a real, live capsule session per run through
a built `mur`, `inference.transport: process` pointed at the escape-conformance package's
`probe-driver`, each session under `systemd-run --user --scope --property=Delegate=yes`, so the
tool call goes through `dispatch_agent_tool_async`, the real composed root, the real Landlock
ruleset and the real seccomp filter. Two debug binaries from the same worktree, side by side:
`mur-before` (branch point) and `mur-after`. One capsule: `containment: sealed`,
`shell.allow: [bash, python3]`, `interpreter_runtime` for `python3`. `mur run --explain-scope` →
`declared: sealed`, `achieved: sealed`, `floor met: yes`,
`mechanism: mountns+pivot_root+landlock+seccomp`.

#### Before — the host's account list, read from inside the capsule

```text
--0-IDENTITY--
uid=1000 gid=1000 cwd=/tmp/mur-verify/sealed/wd
--1-GETPWUID-HOME--
pw_name=agape
pw_dir=/home/agape
HOME=/tmp/mur-verify/sealed/wd/.capsule-home
MATCH=False
expanduser=/tmp/mur-verify/sealed/wd/.capsule-home
gr_name=agape gr_gid=1000
--2-CONTENT--
/etc/passwd bytes=2910 lines=50
  | root:x:0:0:root:/root:/bin/bash
  | daemon:x:1:1:daemon:/usr/sbin:/usr/sbin/nologin
  | bin:x:2:2:bin:/bin:/usr/sbin/nologin
  | sys:x:3:3:sys:/dev:/usr/sbin/nologin
  | sync:x:4:65534:sync:/bin:/bin/sync
  | games:x:5:60:games:/usr/games:/usr/sbin/nologin
  | man:x:6:12:man:/var/cache/man:/usr/sbin/nologin
  | lp:x:7:7:lp:/var/spool/lpd:/usr/sbin/nologin
  | ... (42 more lines)
/etc/passwd names=_apt,agape,avahi,backup,bin,colord,cups-browsed,cups-pk-helper,daemon,dhcpcd,
dnsmasq,fwupd-refresh,games,gdm,geoclue,gnome-initial-setup,gnome-remote-desktop,hplip,irc,
kernoops,list,lp,mail,man,messagebus,news,nm-openvpn,nobody,polkitd,proxy,root,rtkit,saned,
speech-dispatcher,sshd,sssd,sync,sys,syslog,systemd-network,systemd-oom,systemd-resolve,
systemd-timesync,tcpdump,tss,usbmux,uucp,uuidd,whoopsie,www-data
/etc/group bytes=1107 lines=77
  | root:x:0:
  | daemon:x:1:
  | bin:x:2:
  | sys:x:3:
  | adm:x:4:syslog,agape
  | tty:x:5:
  | disk:x:6:
  | lp:x:7:
  | ... (69 more lines)
--3-WRITE-REFUSAL--
/etc/passwd utime(touch) -> [Errno 30] Read-only file system
/etc/passwd append -> [Errno 13] Permission denied: '/etc/passwd'
/etc/group utime(touch) -> [Errno 30] Read-only file system
/etc/group append -> [Errno 13] Permission denied: '/etc/group'
--4-BOUNDARY--
/etc/shadow -> [Errno 2] No such file or directory: '/etc/shadow'
/etc/hosts bytes=220
/etc/ssl/certs/ca-certificates.crt bytes=182140
--END--
```

Note `pw_name=agape` and `pw_dir=/home/agape`: not only the host's whole account list, but the
launching user's login name and real home path, contradicting the `$HOME` the same subprocess was
given (`MATCH=False`).

#### After — same capsule, same probe

```text
--0-IDENTITY--
uid=1000 gid=1000 cwd=/tmp/mur-verify/sealed/wd
--1-GETPWUID-HOME--
pw_name=capsule
pw_dir=/tmp/mur-verify/sealed/wd/.capsule-home
HOME=/tmp/mur-verify/sealed/wd/.capsule-home
MATCH=True
expanduser=/tmp/mur-verify/sealed/wd/.capsule-home
gr_name=capsule gr_gid=1000
--2-CONTENT--
/etc/passwd bytes=113 lines=2
  | root:x:0:0:root:/root:/bin/sh
  | capsule:x:1000:1000:Murmur capsule:/tmp/mur-verify/sealed/wd/.capsule-home:/bin/sh
/etc/passwd names=capsule,root
/etc/group bytes=26 lines=2
  | root:x:0:
  | capsule:x:1000:
/etc/group names=capsule,root
--3-WRITE-REFUSAL--
/etc/passwd utime(touch) -> [Errno 30] Read-only file system
/etc/passwd append -> [Errno 30] Read-only file system: '/etc/passwd'
/etc/group utime(touch) -> [Errno 30] Read-only file system
/etc/group append -> [Errno 30] Read-only file system: '/etc/group'
--4-BOUNDARY--
/etc/shadow -> [Errno 2] No such file or directory: '/etc/shadow'
/etc/hosts bytes=220
/etc/ssl/certs/ca-certificates.crt bytes=182140
--END--
```

Both files still readable (`getpwuid(3)`/`getgrgid(3)` resolve, `~` expands), `pw_dir` and `$HOME`
byte-identical, two lines each, and **none of the 50 host account names or 77 host group names
appears** beyond `root`. Writes are still refused; the append now reports `Read-only file system`
where it reported `Permission denied` before — same refusal, different layer reaching it first
(the `MS_RDONLY` bind rather than Landlock, because the synthetic file's own rule carries
`ReadFile` and the host file's carried nothing that applied). `/etc/shadow`, `/etc/hosts` and the
trust store are unchanged.

#### The Landlock rule these files need, demonstrated by removing it

A Landlock rule names the **inode** an fd resolved to, not the path string it was opened by. The
`SEALED_ETC_PATHS` grant for `/etc/passwd` is taken on the *host's* `/etc/passwd`, which the
composed root no longer binds — so the synthetic file needs a rule of its own, taken on the file in
`<workdir>/.mur-etc/`. A third binary, identical to `mur-after` except that
`apply_landlock_scope` skips those two rules:

```text
--1-GETPWUID-HOME--
getpwuid FAIL KeyError('getpwuid(): uid not found: 1000')
getgrgid FAIL KeyError('getgrgid(): gid not found: 1000')
--2-CONTENT--
/etc/passwd OPEN FAIL PermissionError(13, 'Permission denied')
/etc/group OPEN FAIL PermissionError(13, 'Permission denied')
```

Mounted and unopenable — precisely the bug the 2026-08-08 entry below exists to record, one file
over. This is why this slice touched `open_landlock_fds`/`apply_landlock_scope` at all;
`resolve_sealed_etc_landlock_grants` itself is unchanged.

#### Negative control — `scoped` is untouched

The identical capsule with `containment: scoped`, run through the same path against both binaries.
`diff` of the two full transcripts is **empty** — byte-identical:

```text
--1-GETPWUID-HOME--
getpwuid FAIL KeyError('getpwuid(): uid not found: 1000')
--2-CONTENT--
/etc/passwd OPEN FAIL PermissionError(13, 'Permission denied')
/etc/group OPEN FAIL PermissionError(13, 'Permission denied')
--4-BOUNDARY--
/etc/shadow -> [Errno 13] Permission denied: '/etc/shadow'
/etc/hosts -> [Errno 13] Permission denied: '/etc/hosts'
/etc/ssl/certs/ca-certificates.crt -> [Errno 13] Permission denied
```

`Permission denied` rather than `No such file or directory` on `/etc/shadow` is the tell that this
session is looking at the **host's** real `/etc` through Landlock, with no composed root anywhere —
`applied_tier` returns `KernelFull` for a `scoped` declaration even on a host that can back
`sealed`, which is why `mur run --explain-scope` reporting the host's `achieved: sealed` is not a
contradiction. Same behaviour as the 2026-08-08 entry recorded for `scoped`.

#### Escape-conformance harness (card `4875bc97`)

Run at `--class sealed` against both binaries, 28 cases each, same host, same session. Both exit
`0` with `boundary: no boundary was crossed` and `resource exhaustion: every declared ceiling
held`; **21 boundary cases asserted and passed, 5 resource cases asserted and passed, 2 recorded
but not asserted**. `diff` of the two verdict columns is empty — every case identical before and
after, including the three cases that read `/etc/passwd` and are expected `ALLOWED`
(`symlink-escape`, `proc-self-cwd-reopen`, `proc-pid-root-reopen`: the file is still readable, only
its contents changed) and `read-etc-shadow`, still `REFUSED`.

### Run of 2026-08-09 — the first full-registry bare-metal `sealed` run against a *gated* escape-conformance suite (card `a495eacb`)

Every earlier escape-conformance run on this page — including the `4875bc97` entry below — graded
**nothing** at `sealed`. All 28 cases carried `Expectation::Documented(_)`, which
`Expectation::gates()` returns `false` for, so the column was an intention that ran, was recorded in
full, and could not fail. That is why the entry below could report "both exit `0`" while also
reporting three cases changing verdict: no expectation was being compared against anything. This run
is the one that turns the column into a gate. Its expectations are set from what this host actually
did, so the exit code now means something for `sealed`.

**Host.** Same machine as the 2026-08-05, 2026-08-07 and 2026-08-08 runs: `Linux 7.0.0-28-generic
#28~24.04.1-Ubuntu SMP PREEMPT_DYNAMIC Wed Jul 1 15:50:57 UTC 2 x86_64`, Ubuntu 24.04, bare metal
(`/.dockerenv` absent, `/run/.containerenv` absent, `/proc/1/cgroup` → `0::/init.scope`), non-root
`uid=1000`, `/proc/sys/user/max_user_namespaces` → `55249`,
`kernel.apparmor_restrict_unprivileged_userns=0` so **no AppArmor profile was needed** and a
`./target/release/mur` binary is covered (the profile attaches by executable path; with the
restriction off there is nothing to attach). Delegated cgroup v2 through `systemd-run --user --scope
--property=Delegate=yes`, which the harness applies per case by default. CPython 3.12.3.

`mur run --explain-scope` against a `containment: sealed` capsule on this host:

```text
Containment
  declared:  sealed
  achieved:  sealed
  floor met: yes
  mechanism: mountns+pivot_root+landlock+seccomp
```

**Invocation and result.** `cd crates/capsule-runtime/escape-conformance && cargo build --release`,
then `./target/release/escape-conformance --class sealed --record-dir <dir>`. **Exit code `0`.** The
dated record's `## Summary`, verbatim:

| category | asserted | passed | failed | recorded but not asserted |
|---|---|---|---|---|
| **boundary** (a failure here is an escape) | 21 | 21 | 0 | 2 |
| **resource_exhaustion** (a failure here is denial of service, never an escape) | 5 | 5 | 0 | 0 |

Twenty-six of the 28 cases print `PASS` rather than `recorded (not asserted at this class)`; the two
that do not are `hardlink-escape` and `rename-across-boundary`, for a structural reason given below.

**The promoted expectation table.** This is the `sealed` column as it now stands in
`crates/capsule-runtime/escape-conformance/src/cases.rs`. Nothing in it is `Documented(_)` any more.

| case | `sealed` expectation | actual on this run |
|---|---|---|
| `read-etc-shadow` | `Must(Refused)` | REFUSED (ENOENT — absent from the root) |
| `write-outside-workdir` | `Must(Allowed)` | ALLOWED (`/tmp` is workdir-backed) |
| `stat-outside-workdir` | `Must(Refused)` | REFUSED (ENOENT — no inode to stat) |
| `symlink-escape` | `Must(Allowed)` | ALLOWED (`/etc/passwd` is allowlisted) |
| `hardlink-escape` | `NotAsserted` | INCONCLUSIVE (EXDEV) |
| `rename-across-boundary` | `NotAsserted` | INCONCLUSIVE (EXDEV) |
| `proc-self-cwd-reopen` | `Must(Allowed)` | ALLOWED (reaches the *composed root's* `/etc/passwd`) |
| `proc-pid-root-reopen` | `Must(Allowed)` | ALLOWED (the alias is the composed root, not the host `/`) |
| `proc-self-fd-reopen` | `Must(Refused)` | REFUSED (EACCES on the O_RDWR upgrade) |
| `inherited-fd-after-exec` | `Must(Refused)` | REFUSED (no fd above stdio) |
| `mknod-block-device-in-workdir` | `Must(Refused)` | REFUSED (EACCES — Landlock, not CAP_MKNOD) |
| `exec-renamed-disallowed-binary` | `Must(Refused)` | REFUSED (EACCES from execve) |
| `connect-unlisted-tcp-host` | `Must(Refused)` | REFUSED (ECONNREFUSED — nothing listening) |
| `udp-exfiltration` | `Must(Refused)` | REFUSED (delivery-confirmed local capture) |
| `dns-exfiltration` | `Must(Refused)` | REFUSED (EAI_AGAIN from the in-namespace resolver) |
| `abstract-unix-socket-connect` | `Must(Refused)` | REFUSED (EACCES at `socket(2)`) |
| `pathname-unix-socket-connect` | `Must(Refused)` | REFUSED (EACCES at `socket(2)`) |
| `syscall-io-uring-setup` | `Must(Refused)` | REFUSED (EPERM, seccomp) |
| `syscall-userfaultfd` | `Must(Refused)` | REFUSED (EPERM, seccomp) |
| `syscall-bpf` | `Must(Refused)` | REFUSED (EPERM, seccomp) |
| `syscall-open-by-handle-at` | `Must(Refused)` | REFUSED (EPERM, seccomp) |
| `syscall-perf-event-open` | `Must(Refused)` | REFUSED (EPERM, seccomp) |
| `syscall-keyctl` | `Must(Refused)` | REFUSED (EPERM, seccomp) |
| `resource-fork-bomb` | `Must(Contained)` | CONTAINED (EAGAIN at 31 children, `pids.max` 32) |
| `resource-disk-filler-per-file` | `Must(Contained)` | CONTAINED (EFBIG at exactly 10 MiB) |
| `resource-disk-filler-aggregate` | `Must(Contained)` | CONTAINED (second spawn refused, naming `workdir_max_bytes`) |
| `resource-memory-hog` | `Must(Contained)` | CONTAINED (shell tool exit 137) |
| `resource-fd-exhauster` | `Must(Contained)` | CONTAINED (EMFILE at 125 of 128) |

Two of these — `syscall-userfaultfd` and `syscall-perf-event-open` — read their attribution sysctl
as `unknown` at this class, because `/proc/sys` is not part of the composed root's `/proc`. They are
asserted because the seccomp filter is identical on both kernel classes and `scoped` is where the
sysctl is readable; the record's per-case attribution says so rather than implying an attribution
this class cannot make.

#### The four documented-versus-actual mismatches, and how each was resolved

Each case's `attribution` field in `cases.rs` carries the full reasoning; this is the summary, not a
second copy of it.

- **`udp-exfiltration` — documented REFUSED, measured ALLOWED, resolved as REFUSED after a
  receive-side check.** This was the one mismatch that read as a genuine weakening, and it was a
  measurement artifact. Since `f163778e` the capsule runs in its own network namespace whose only
  route is `local default dev lo`, which makes *every* destination address locally deliverable — so
  `sendto` returning success proves the write succeeded, not that anything left the host. Worse, the
  probe aimed at port 53, the one UDP port the runtime itself binds inside that namespace
  (`network_namespace::bind_dns_socket`), so its datagram terminated in the runtime's own DNS
  resolver: the most contained outcome available, scored as an escape. The probe now binds a UDP
  receiver on `0.0.0.0:46053` **before** sending, in the same process and therefore the same
  namespace, and grades on what that receiver observes. It observed the identical 35-byte payload,
  with a source address of `1.1.1.1` — the destination itself, because the `local` route makes that
  address local. `REFUSED` is the delivery-confirmed verdict, and the refusal is structural (no path
  off the host) rather than an errno, which is what `DETAIL` now says. The port-53 send is still made
  and reported as context, so the old ALLOWED reading and its cause appear on the same line.
- **`proc-self-cwd-reopen` — documented ALLOWED (per the `4875bc97` entry below), measured REFUSED by
  a later run, resolved as ALLOWED.** Neither written record was wrong about what it saw; the probe
  was wrong. Its walk out of the workdir used a fixed six `..` components, so whether it reached the
  filesystem root depended on how deep the harness's own `--work-root` happened to be. That was
  reproduced deliberately on this host, same binary and same class: from `/tmp/ec1/w` the case
  measured **ALLOWED**, and from a work root eleven components deep it measured **REFUSED** with
  `ENOENT` on `<work-root-ancestor>/etc/passwd`. Path-depth arithmetic wearing a containment
  verdict's clothes — and it had been passing at `scoped` for the same wrong reason. The `..` count is
  now derived from the probe's own cwd depth (`..` at `/` resolves to `/`, so overshooting is free)
  and `DETAIL` names the count and the resolved target. The mechanism behind the honest ALLOWED is
  the one the entry below identified: the walk reaches the **composed root's** `/etc/passwd`, on
  `SEALED_ETC_PATHS` and granted read since `fb1eea97`. Everything such a walk can arrive at is
  bounded by what the composed root exposes, which is the property worth asserting. At `scoped` the
  fixed probe now refuses with `EACCES` from Landlock instead of `ENOENT` from a truncated walk — a
  strictly better assertion at that class too, verified on this host.
- **`hardlink-escape` and `rename-across-boundary` — documented REFUSED, measured INCONCLUSIVE,
  resolved as `NotAsserted`.** Both hit `EXDEV`, and `EXDEV` here is the mount layout speaking, not
  containment. `sealed` composes its root out of six independent bind/mount operations
  (`sealed::plan_composed_root`): staged runtime trees, the `/etc` allowlist, `/dev`, `/proc`, `/tmp`,
  and last the workdir at its own absolute path. `link(2)`/`rename(2)` return `EXDEV` whenever source
  and destination are on different mounts, whatever their sources' filesystems — which is why
  `rename-across-boundary` fails even though `/tmp`'s bind source is
  `workdir.join(SEALED_TMP_DIR_NAME)`, a subdirectory of the very same workdir. No destination
  avoids it: every path reachable from the workdir is either another independent bind or the base
  root the workdir bind sits on, because giving the workdir its own mount is the mechanism that makes
  everything else read-only-or-absent, and a path with no bind at all answers `ENOENT` without
  exercising a rename boundary either.

    A control run isolates the cause, because "different filesystems" could otherwise be read as an
    accident of where the harness's work root sat. `/space` on this host is `/dev/nvme0n1p7` while
    `/`, `/etc` and `/tmp` are `/dev/nvme0n1p5`, so both cases were re-run with
    `--work-root /tmp/…` — the *same block device* as their destinations. At `scoped` they then
    refuse for real: `link(/etc/passwd)` → `EPERM(1)`, `rename(… -> /tmp/…)` → `EACCES(13)`. At
    `sealed`, same work root, same device, they still report `EXDEV`. The device layout is therefore
    not what causes it at `sealed`; the composed root's independent mounts are.

    So these two are `Expectation::NotAsserted` at `sealed` — **not** for `advisory`'s reason (a
    class with no mechanism) but because the *cases' own shape* cannot reach their premise at this
    class. Both remain `Must(Refused)` at `scoped`, where there is one filesystem and Landlock is
    genuinely consulted. A related consequence, recorded because it weakens an attribution rather
    than a boundary: `protected_hardlinks` reads back as `unknown` at `sealed`, since `/proc/sys` is
    not in the composed root.

#### What this run did not change

No enforcement mechanism. Nothing under `crates/capsule-runtime/src/` was touched — the diff is the
harness's own data (`cases.rs`), two probe bodies, two stale explanatory paragraphs
(`main.rs`'s `--list-cases` trailer and `record.rs`'s cross-class preamble, which still claimed
`achieved_class_for_tier` had no `Sealed` arm) and this page. No case was added, removed or renamed:
23 boundary + 5 resource-exhaustion, as before.

**Not run.** Step 5 (the container refusal) remains unrun on this host for the same reason as every
earlier entry — no container runtime installed. A root run remains unrun too, so
`mknod-block-device-in-workdir`, `syscall-bpf` and `syscall-open-by-handle-at` keep the non-root
caveats their attributions state; `mknod` did at least answer `EACCES` here, which is Landlock and
not the missing capability.

### Run of 2026-08-08 — reading the composed root's `/etc` allowlist (`SEALED_ETC_PATHS`), and TLS

A targeted run in the same shape as the 2026-08-07 entry below, one directory over. The composed
root bind-mounts sixteen curated `/etc` entries read-only and the Landlock ruleset installed inside
that root then denied every one of them. What that broke, visibly, was **TLS certificate
verification**: the trust store was mounted and unreadable, so `pip`, `curl` and anything else that
opens `/etc/ssl/certs/ca-certificates.crt` by name failed against hosts the manifest had explicitly
allowed. Earlier entries on this page recorded `ls /etc` as `Permission denied` and read it as
containment holding; it was, but it was also hiding this.

**Host.** Same machine as the 2026-08-05 and 2026-08-07 runs: `Linux 7.0.0-28-generic
#28~24.04.1-Ubuntu SMP PREEMPT_DYNAMIC Wed Jul 1 15:50:57 UTC 2 x86_64`, Ubuntu 24.04,
`systemd-detect-virt` → `none` (bare metal), non-root `uid=1000`, `/proc/self/attr/current` →
`unconfined`, `kernel.apparmor_restrict_unprivileged_userns=0` so no profile was needed. CPython
3.12.3, curl 8.5.0, OpenSSL 3.0.13.

**Harness.** Real, live capsule sessions through the built `mur` binary, with
`inference.transport: process` pointed at the escape-conformance package's `probe-driver` — the
same route that package documents, so the tool call runs through
`CapsuleStoreState::dispatch_agent_tool_async`, the real Landlock ruleset and the real seccomp
filter, with only the *choice* of command scripted. Each session under `systemd-run --user --scope
--property=Delegate=yes`. Two binaries built from the same worktree and kept side by side:
`mur-before` (branch point) and `mur-after` (the grant in place).

**Capsule.** `containment: sealed`; `shell.allow: [bash, python3, ls, cat, touch, curl]`;
`network.allow: [https://pypi.org, https://files.pythonhosted.org]`; `interpreter_runtime` for
`python3` only. `mur run --explain-scope` → `declared: sealed`, `achieved: sealed`, `floor met:
yes`, `mechanism: mountns+pivot_root+landlock+seccomp`. `trace.jsonl` `session_start` →
`"containment_declared":"sealed","containment_achieved":"sealed"`.

#### Before — the reported bug, reproduced verbatim

```text
--1-CA-BUNDLE-READ--
PermissionError: [Errno 13] Permission denied                    rc=1
--2-TLS-PYTHON-EXPLICIT-BUNDLE--
PermissionError: [Errno 13] Permission denied                    rc=1
--3-TLS-CURL--
curl: (77) error setting certificate file: /etc/ssl/certs/ca-certificates.crt
http_code=000                                                    rc=77
--4-LS-SSL-CERTS--
ls: cannot open directory '/etc/ssl/certs': Permission denied
--5-LISTDIR-COUNTS--
LIST FAIL /etc/ssl                 [Errno 13] Permission denied: '/etc/ssl'
LIST FAIL /etc/ssl/certs           [Errno 13] Permission denied: '/etc/ssl/certs'
LIST FAIL /etc/alternatives        [Errno 13] Permission denied: '/etc/alternatives'
LIST FAIL /etc/ca-certificates     [Errno 13] Permission denied: '/etc/ca-certificates'
LIST FAIL /etc/pki                 [Errno 13] Permission denied: '/etc/pki'
LIST FAIL /etc/ld.so.conf.d        [Errno 13] Permission denied: '/etc/ld.so.conf.d'
LIST FAIL /etc/terminfo            [Errno 13] Permission denied: '/etc/terminfo'
--6-FILE-ENTRIES--
OPEN FAIL /etc/ld.so.cache         [Errno 13] Permission denied
OPEN FAIL /etc/ld.so.conf          [Errno 13] Permission denied
OPEN FAIL /etc/ca-certificates.conf [Errno 13] Permission denied
OPEN FAIL /etc/hosts               [Errno 13] Permission denied
OPEN FAIL /etc/nsswitch.conf       [Errno 13] Permission denied
OPEN ok   /etc/localtime           bytes=246
OPEN FAIL /etc/timezone            [Errno 13] Permission denied
OPEN FAIL /etc/passwd              [Errno 13] Permission denied
OPEN FAIL /etc/group               [Errno 13] Permission denied
OPEN FAIL /etc/resolv.conf         [Errno 2] No such file or directory
--7-GETPWUID--
KeyError: 'getpwuid(): uid not found: 1000'                      rc=1
```

and `pip`, which is the card's own acceptance criterion:

```text
WARNING: Retrying (Retry(total=4, …)) after connection broken by
  'SSLError(PermissionError(13, 'Permission denied'))': /simple/six/
Could not fetch URL https://pypi.org/simple/six/: There was a problem confirming the ssl
  certificate: … (Caused by SSLError(PermissionError(13, 'Permission denied'))) - skipping
ERROR: No matching distribution found for six==1.17.0                rc=1
```

#### After — same capsule, same commands

```text
--1-CA-BUNDLE-READ--   loaded, x509=121                          rc=0
--2-TLS-PYTHON-EXPLICIT-BUNDLE--
  handshake-ok cipher=TLS_AES_128_GCM_SHA256 peer-CN=pypi.org     rc=0
--3-TLS-CURL--         http_code=200                             rc=0
--4-LS-SSL-CERTS--
total 1156
drwxr-xr-x 1 nobody nogroup   9960 Jul 26 17:23 .
drwxr-xr-x 1 nobody nogroup     46 Aug  2 06:43 ..
lrwxrwxrwx 1 nobody nogroup     23 Feb  9 19:20 002c0b4f.0 -> GlobalSign_Root_R46.pem
lrwxrwxrwx 1 nobody nogroup     24 Feb  9 19:20 0179095f.0 -> BJCA_Global_Root_CA1.pem
(total lines=248)
--5-LISTDIR-COUNTS--
LIST ok   /etc/ssl                 entries=3
LIST ok   /etc/ssl/certs           entries=245
LIST ok   /etc/alternatives        entries=117
LIST ok   /etc/ca-certificates     entries=1
LIST ok   /etc/pki                 entries=2
LIST ok   /etc/ld.so.conf.d        entries=3
LIST ok   /etc/terminfo            entries=1
--6-FILE-ENTRIES--
OPEN ok   /etc/ld.so.cache         bytes=68131
OPEN ok   /etc/ld.so.conf          bytes=34
OPEN ok   /etc/ca-certificates.conf bytes=6862
OPEN ok   /etc/hosts               bytes=220
OPEN ok   /etc/nsswitch.conf       bytes=594
OPEN ok   /etc/localtime           bytes=246
OPEN ok   /etc/timezone            bytes=15
OPEN ok   /etc/passwd              bytes=2910
OPEN ok   /etc/group               bytes=1102
OPEN FAIL /etc/resolv.conf         [Errno 2] No such file or directory
--7-GETPWUID--        getpwuid=agape                             rc=0
```

```text
Collecting six==1.17.0
  Downloading six-1.17.0-py2.py3-none-any.whl.metadata (1.7 kB)
Downloading six-1.17.0-py2.py3-none-any.whl (11 kB)
Installing collected packages: six
Successfully installed six-1.17.0                                    rc=0
six 1.17.0 from <workdir>/site/six.py                                rc=0
```

**A real `pip install` from a real index over a verified TLS connection.** That is the card's
manual-acceptance criterion, met.

#### The boundary, re-checked in the same session

| probe | before | after | verdict |
|---|---|---|---|
| `ls /etc` (the root's own `/etc`, not a mounted entry) | `Permission denied` | `Permission denied` | unchanged ✅ |
| `cat /etc/shadow` | `No such file or directory` | `No such file or directory` | unchanged ✅ |
| `cat /etc/ssh/sshd_config` | `No such file or directory` | `No such file or directory` | unchanged ✅ |
| `touch /etc/hosts` | `Permission denied` | `Permission denied` | unchanged ✅ |
| `open('/etc/hosts','w')` | `[Errno 30] Read-only file system` | `[Errno 30] Read-only file system` | unchanged ✅ |
| `touch /etc/ssl/certs/evil.crt` | `Read-only file system` | `Read-only file system` | unchanged ✅ |
| `/etc/alternatives/awk --version` | `Permission denied` (126) | `Permission denied` (126) | unchanged ✅ |
| `ls /` | `Permission denied` | `Permission denied` | unchanged ✅ |
| `ls /root` | `No such file or directory` | `No such file or directory` | unchanged ✅ |
| `cat /dev/sda`, `cat /var/run/docker.sock` | absent | absent | unchanged ✅ |

**The `/etc/alternatives/awk` row is the one worth reading twice**, and it is the same finding the
2026-08-07 entry recorded for `/usr`: `/etc/alternatives` is 117 symlinks into `/usr/bin`, so a
grant that carried `Execute` would hand a `sealed` capsule a second, undeclared route to every host
binary. It is granted `ReadFile`+`ReadDir` and no `Execute`, and `awk` — readable, listable, and
not in `shell.allow` — still refuses to run.

#### Two host-specific observations, recorded rather than generalised

**`/etc/localtime` opened *before* the fix.** It is a symlink to `/usr/share/zoneinfo/…` on this
host, and `/usr` was already granted by the 2026-08-07 runtime-tree grant, so its own rule is
largely redundant here. On a host where `/etc/localtime` is a regular file it would not have been.

**`/etc/resolv.conf` is `No such file or directory` in a composed root on this host, before and
after.** It is a symlink to `../run/systemd/resolve/stub-resolv.conf`; `plan_composed_root`
reproduces symlinks as symlinks, and `/run` is not mounted into the root, so the link dangles. That
is a pre-existing property of the composed root, unrelated to this grant and unchanged by it —
worth a follow-up, since DNS configuration is one of the things this allowlist exists to provide.

**Why the trust store failed the way it did, exactly.** On Debian/Ubuntu, `/etc/ssl/certs/<hash>.0`
are symlinks to `<Name>.pem`, which are themselves symlinks to `/usr/share/ca-certificates/…`. So
OpenSSL's *hash-directory* lookup already resolved into `/usr` and worked before this fix — a
`ssl.create_default_context()` handshake succeeded on this host even with `/etc/ssl` denied. What
did not work was the *concatenated bundle*, `/etc/ssl/certs/ca-certificates.crt`, which is a real
regular file in `/etc/ssl/certs`: `curl`'s default `CAINFO`, `pip`'s, and any explicit
`load_verify_locations()` all name it, and all got `EACCES`. Enumerating `/etc/ssl/certs` failed
too. So the symptom's severity is host-layout-dependent; the defect is not.

#### Negative control — `scoped` is untouched

The identical capsule with `containment: scoped`, run through the same path against both binaries.
`diff` of the two full transcripts is **empty** — byte-identical, all sixteen paths still
`Permission denied`, `getpwuid` still failing, curl still `(77)`. Under `scoped` there is no
composed root and `/etc` is literally the host's, which is exactly what the tier gate exists to
protect.

#### Escape-conformance harness (card `4875bc97`)

Run at `--class sealed` against both binaries, 28 cases each. Both exit `0` with
`boundary: no boundary was crossed` and `resource exhaustion: every declared ceiling held`. Three
cases change verdict, all three for the same reason and all three by design:

| case | before | after |
|---|---|---|
| `symlink-escape` | REFUSED | ALLOWED |
| `proc-self-cwd-reopen` | REFUSED | ALLOWED |
| `proc-pid-root-reopen` | REFUSED | ALLOWED |

All three read **`/etc/passwd`**, which is on `SEALED_ETC_PATHS` and is therefore now readable by
declaration. `read-etc-shadow` stays REFUSED in both runs — the file that actually holds secrets is
still absent from the root. Their `sealed` expectation was updated from `Documented(Refused)` to
`Documented(Allowed)` so the record stays truthful; they are `Documented`, i.e. recorded and not
graded, so nothing about pass/fail changed.

The harness also needed one calibration change, which is a genuine consequence of this slice and is
recorded here rather than buried: its `TightResources` profile declared
`max_open_files: 64`, and sixteen more Landlock grant fds held open across the child's `pre_exec`
window put a `sealed` capsule back over that ceiling. Every resource case failed with
`sandbox: shell enforcement setup failed before exec: egress-netns: writing uid_map/gid_map failed`
instead of reporting its ceiling. Bisected by hand on this host with a `[bash, python3]` sealed
capsule: **refused at 64, spawns at 72**. The profile is now `128`, and the `resource-fd-exhauster`
case grades against the ceiling the child reads back from its own `RLIMIT_NOFILE` rather than
against a constant, so the next move of that number cannot silently mis-grade it. With those two
changes all five resource cases report `CONTAINED` on both binaries.

**Not run.** Step 5 (the container refusal) remains unrun on this host for the same reason as every
earlier entry — no container runtime installed.

### Run of 2026-08-08 — `/tmp` is writable inside the composed root (step 3d)

A targeted run, not a re-run of steps 1–6: it records step 3d's `/tmp` claim, which **no earlier run
on this page had ever exercised**. The 2026-08-05 entry tested step 3's writability through
`echo hi > "$PWD/canary.txt"` only, and the 2026-08-03 run's harness stopped before Landlock
installed. The `Expected: probe` block above was therefore aspirational, and it was wrong: it
described a mount that was bind-mounted read-write and then denied by the Landlock ruleset installed
inside the root, because that ruleset had a rule for the workdir and none for the `/tmp` bind.

**Host.** Same machine as the 2026-08-05 and 2026-08-07 runs: `Linux 7.0.0-28-generic
#28~24.04.1-Ubuntu SMP PREEMPT_DYNAMIC Wed Jul 1 15:50:57 UTC 2 x86_64`, Ubuntu 24.04,
`systemd-detect-virt` → `none` (bare metal, not a container), non-root (`uid=1000`),
`kernel.apparmor_restrict_unprivileged_userns=0` so no profile was needed, `/proc/self/attr/current`
→ `unconfined`. CPython 3.12.3, gcc 13 with `cc1` under `/usr/libexec/gcc/x86_64-linux-gnu/13`.
Sessions launched under `systemd-run --user --scope -p Delegate=yes` (without it the launch is
correctly refused with `E-RUN-012`) with `capabilities.resources.max_processes: 4096`. Capsule:
`containment: sealed`, `shell.allow: [bash, python3, mktemp, cc, gcc, as, ld, cat, ls, touch,
stat]`, one `interpreter_runtime` grant for `cc` on `/usr/libexec/gcc` (`list_dir: true`), no
`staged_runtime`. `mur run --explain-scope` → `achieved: sealed`, `floor met: yes`; `trace.jsonl`
`session_start` → `"containment_achieved":"sealed"`, `"workdir_exec":false`.

**Before (control, `mur` built without the `/tmp` rule)** — driven through `shell::execute_shell`
at the resolved `KernelSealed` tier, verbatim:

```text
--1-PY-WRITE--
Traceback (most recent call last):
  File "<string>", line 1, in <module>
PermissionError: [Errno 13] Permission denied: '/tmp/probe'
rc=1
--3-MKTEMP--
mktemp: failed to create file via template ‘/tmp/tmp.XXXXXXXXXX’: Permission denied
rc=1
--4-CC--
Cannot create temporary file in /tmp/: Permission denied
rc=134
--9-TMP-LS--
ls: cannot open directory '/tmp': Permission denied
rc=2
--7-USR-WRITE--
touch: cannot touch '/usr/testfile': Read-only file system
--8-SHADOW--
stat: cannot statx '/etc/shadow': No such file or directory
```

**After (the rule in place), through a real, live capsule session.** One real `bash` tool call, the
agent's verbatim report of stdout:

```text
1 rc=0
y
/tmp/tmp.IOF7QFO79k
3 rc=0
4 rc=0
touch: cannot touch '/usr/testfile': Read-only file system
```

(`1` is `python3 -c "open('/tmp/probe','w').write('y')"`, then `cat /tmp/probe` → `y`; `3` is
`mktemp`; `4` is `cc t.c -o t`. The call's stderr carried one expected line —
`bash: line 1: /usr/bin/head: Permission denied` — because the probe piped `stat` into `head`, which
is not in `shell.allow`.)

A second live call, covering the rest of 3d:

```text
--A--
probe
--B--
.
..
sealed-probe
--C--
stat: cannot statx '/etc/shadow': No such file or directory
--D--
touch: cannot touch '/usr/testfile': Read-only file system
--E--
heredoc-ok
```

Host side, after both sessions ended: `<workdir>/.mur-tmp/` contained `probe` (1 byte, `y`),
`sealed-probe` (`probe`), the `mktemp` file `tmp.IOF7QFO79k`, and — from the `cc` run — nothing left
behind, gcc having cleaned up its own intermediates. The compiled `t` sits in the workdir proper.
This is the bind-mount identity holding, the same way `canary.txt` demonstrated it for the workdir
in the 2026-08-05 run.

| probe | before | after | verdict |
|---|---|---|---|
| `python3 -c "open('/tmp/probe','w')…"` | `PermissionError: [Errno 13]` | rc 0, reads back `y` | fixed |
| `mktemp` | `failed to create file … Permission denied` | prints `/tmp/tmp.…`, rc 0 | fixed |
| `cc t.c -o t` | `Cannot create temporary file in /tmp/`, rc 134 | rc 0 | fixed |
| `ls -a /tmp` | `Permission denied` | `.  ..  sealed-probe` | fixed |
| `cat <<EOF` heredoc | (bash uses a pipe, so it never failed) | `heredoc-ok` | unchanged ✅ |
| `touch /usr/testfile` | `Read-only file system` | `Read-only file system` | unchanged ✅ |
| `stat /etc/shadow` | `No such file or directory` | `No such file or directory` | unchanged ✅ |

**The `Execute` axis, checked in both directions.** `/tmp` gets the workdir's right-set from a
single binding in `apply_landlock_scope`, so `Execute` on `/tmp` tracks
`capabilities.filesystem.workdir_exec` exactly. Driven at the `KernelSealed` tier with the flag
forced both ways (a state `mur run` itself will not produce — see the note in 3d — so it was
resolved directly through `ShellEnforcement::resolve`):

```text
workdir_exec=false   ./t → Permission denied (rc 126)   /tmp/t2 → Permission denied (rc 126)
workdir_exec=true    ./t → hello from cc (rc 0)         /tmp/t2 → hello from cc (rc 0)
```

Neither more nor less runnable than the workdir it is stored in, which is the whole claim: `/tmp` is
the *same storage*, so a rule that let a binary run from `/tmp` while the workdir refused it would
be a hole straight through `shell.allow`.

**The size guard still counts `/tmp`, and needed no change.** With
`workdir_max_bytes: 1000000` and a 4 MB write to `/tmp/big`, the existing `resources::WorkdirGuard`
latched on its own interval and the next shell spawn was refused:

```text
[capsule-runtime] workdir grew to 4000000 bytes, past the 1000000 byte ceiling (capabilities.resources.workdir_max_bytes)
latched breach: Some(WorkdirBreach { max_bytes: 1000000, observed_bytes: 4000000 })
refused: workdir grew to 4000000 bytes, past the 1000000 byte ceiling (capabilities.resources.workdir_max_bytes)
```

**Negative control — `scoped` is untouched.** The identical capsule with `containment: scoped`, run
through the same live shell-tool path:

```text
--A--  echo probe > /tmp/sealed-probe          rc=1   (stderr: /tmp/sealed-probe: Permission denied)
--B--  ls -a /tmp                              ls: cannot open directory '/tmp': Permission denied
--C--  stat /etc/shadow                        real metadata: Size 1357, Inode 894783, mode 0640
--D--  touch /usr/testfile                     touch: cannot touch '/usr/testfile': Permission denied
--E--  python3 -c "open('/tmp/probe','w')…"    init_fs_encoding fatal (rc=1), unrelated and pre-existing
```

and **no `.mur-tmp` directory exists in the scoped session's workdir at all** — the tier gate is on
the fd, so `scoped` neither creates the store nor grants it. The same probe run before and after the
change under `scoped` diffs empty modulo a CPython thread id, a pid and the elapsed time. Note the
contrast with the sealed column and with step 6: under `scoped`, `/usr` is the host's and reports
`Permission denied`; under `sealed` it is a read-only bind and reports `Read-only file system`.

**Not run.** Step 5 (the container refusal) remains unrun on this host — still no container runtime
installed. This entry adds nothing to it.

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
  now been run end to end through a live capsule's shell tool (see the 2026-08-05 run above, and the
  2026-08-08 run for step 3d's `/tmp` claim, which that entry had left untested).

A caution this page earned the hard way: an "Expected:" block here is a claim about a mechanism, not
evidence about one. Step 3d's `/tmp` block sat on this page for three runs, describing behaviour no
run had produced, while the mount it described was denied by Landlock on every real host. When a
step's expectation has not appeared verbatim in a *Recording the result* entry, treat it as
unverified, and say so in the next entry rather than assuming an earlier run must have covered it.

## Residuals recorded here rather than buried

These are known, deliberate gaps in what the composed root delivers. None of them is a defect to
file; each is a scoping decision this slice made explicitly.

- **No PID namespace, so `/proc` is a bind of the host's rather than a masked private `procfs`.**
  This is the one place the composed root's "outside does not exist" promise degrades to `scoped`'s
  "outside is denied". See the warning under step 3e for the kernel rule that forces it and for what
  it costs.
- **`/etc/passwd` and `/etc/group` are synthetic, and a capsule can rewrite its own copy.** They are
  no longer the host's (card `60e1c285`; the residual this bullet used to record is closed — see the
  2026-08-09 entry). What remains: the two files are staged in `<workdir>/.mur-etc/`, and the
  workdir is the capsule's one writable path, so a capsule that edits them there changes what its
  own `getpwuid(3)` reports. It reaches nothing outside itself by doing so — nothing on the host
  reads those files, and they name no id the capsule is not already running as. The bind itself
  stays read-only, so `touch /etc/passwd` is still `EROFS`.
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
