# Verification — subprocess file-descriptor hygiene

!!! danger "Status: **PENDING — not yet run.** No result has been recorded, and none should be inferred."

    The `close_range(2)` fd-hygiene step described below is implemented and compiles, but the
    procedure on this page has **not** been executed on a real Linux host. A green
    `cargo build`/`cargo test`/`cargo clippy` on a macOS dev machine is **not** evidence about
    this property and must not be reported as if it were: `close_range(2)` is Linux-only, and
    macOS resolves permanently to `EnforcementTier::EnvironmentOnly`, so nothing on that
    platform exercises a single line of the code under test.

    See [Recording the result](#recording-the-result) for what to replace when someone runs it.

## What this verifies

Both of `capsule-runtime`'s subprocess spawn paths now mark every file descriptor at or above 3
close-on-exec inside the forked child's `pre_exec` window, before anything else in that window
runs:

| path | entry point | where the step runs |
|---|---|---|
| shell tool | `shell::execute_shell` → `sandbox::prepare_enforcement` | first statement of `sandbox::linux_enforce::child_install_enforcement` |
| native tool | `runtime::dispatch_native_tool` | `sandbox::apply_fd_hygiene`, a `pre_exec` closure attached before `.spawn()` |

Both call the same function, `sandbox::linux_enforce::mark_inherited_fds_cloexec`, so the two
paths cannot drift apart on this dimension.

The property under test is a negative: **a descriptor open in the runtime process at spawn time
must not be visible inside the spawned child**. Landlock cannot substitute for this. Landlock
mediates *new* filesystem operations against the ruleset installed in that same `pre_exec`
window; an fd the child already holds was opened before the ruleset existed, so there is no
operation left for the ruleset to intercept, and a leaked fd reads straight through the
workdir scope. On the `KernelSeccompOnly` tier there is no Landlock at all — which is why the
fd-hygiene step runs on both kernel tiers rather than only under `KernelFull`.

Fds 0/1/2 are deliberately **excluded** and are expected to be present in every result below.
That is the correct outcome, not a leak: both spawn paths install `Stdio::piped()` handles
before `pre_exec` runs precisely so the exec'd program inherits them and talks to the parent
over them. See `sandbox::FD_HYGIENE_FIRST_FD` for the recorded decision.

## What this deliberately is not

**This procedure is not automated, and must not be wired into CI in any form** — no `#[test]`,
no `#[ignore]` marker intended for an automated runner, no workflow step. CI runs on
`ubuntu-latest` GitHub runners, which never resolve to a kernel-enforcement tier, and on macOS,
where `close_range(2)` does not exist. A test asserting this property would either pass
vacuously or skip — and either outcome would turn a green run into false evidence about a
security property it never touched. This is the same reasoning that keeps the
[seccomp-notify race probes](seccomp-notify-toctou-audit.md#where-they-live-and-why-they-are-invisible-to-every-automated-run)
out of the workspace.

The one automated test this work added is a content check —
`sandbox::tests::fd_hygiene_range_starts_above_stdio`, asserting `FD_HYGIENE_FIRST_FD == 3`. It
catches the single silent, catastrophic edit (a `0`, which would close stdio and break every
subprocess invocation); it asserts nothing about the kernel.

### Why not `/proc/self/fd`

The obvious way to list a process's open descriptors is `ls /proc/self/fd`. **Do not use it
here.** Under the `KernelFull` tier the child's Landlock ruleset is scoped to the capsule
workdir plus a fixed set of read/execute grants; `/proc` is not among them, so the `openat` of
`/proc/self/fd` fails with `EACCES` and the probe reports nothing — indistinguishable, from the
outside, from a clean result. The investigation that produced this work hit exactly that and had
to be rewritten.

The enumeration below therefore probes descriptors directly with `fcntl(fd, F_GETFD)` across a
range bounded by `getrlimit(RLIMIT_NOFILE)`. `fcntl` returns `-1`/`EBADF` for a closed
descriptor and `0`/flags for an open one, needs no filesystem access at all, and is in the
seccomp allowlist (`sandbox.rs`, `SECCOMP_SYSCALL_ALLOWLIST`) — as are `getrlimit`, `prlimit64`
and `pread64`, which the probe also uses.

---

## Prerequisites

- A **real Linux host**, kernel **5.11 or newer** (`close_range(2)` landed in 5.9,
  `CLOSE_RANGE_CLOEXEC` in 5.11). Confirm with `uname -r`.
- `mur` built from this branch and on `PATH`, plus a working `cc` and `cargo`.
- An inference driver artifact installed and an API key exported, since a capsule tool call is
  driven by the agent loop. The example below uses `murmur-driver-anthropic` and
  `ANTHROPIC_API_KEY`; any driver works.
- Confirm the host actually resolves to a kernel-enforcement tier before trusting anything.
  A capsule that logs `W-SEC-003` (`capabilities.shell.allow is non-empty and this host ...`)
  did **not** resolve to `KernelFull`/`KernelSeccompOnly`, and the shell half of this procedure
  proves nothing on that machine:

```bash
uname -r
mur doctor
```

---

## Step 1 — build the fd probe

The probe is the same binary in both roles: it enumerates its own descriptors, writes the
result to a file in the capsule workdir, and prints a Murmur native `ToolResult` envelope on
stdout so it also works unchanged as a native tool artifact.

Two details are load-bearing and are commented in the source: it enumerates **before** opening
its own output file (opening it first would occupy a descriptor and appear in the results), and
it probes any descriptor it does find with `pread(2)` rather than `read(2)`, because `pread`
never blocks and returns `ESPIPE` on a pipe or socket, so it is safe to point at an unknown fd.

```bash
mkdir -p /tmp/fdhygiene/src /tmp/fdhygiene/bin
cat > /tmp/fdhygiene/src/fdprobe.c <<'EOF'
/* fdprobe — enumerate this process's open file descriptors without touching /proc. */
#define _GNU_SOURCE
#include <fcntl.h>
#include <stdarg.h>
#include <stdio.h>
#include <string.h>
#include <sys/resource.h>
#include <unistd.h>

#define MAX_PROBE  65536
#define MAX_REPORT 64

static char report[8192];
static size_t off;

static void apnd(const char *fmt, ...) {
    va_list ap;
    int w;
    if (off >= sizeof report - 1) return;
    va_start(ap, fmt);
    w = vsnprintf(report + off, sizeof report - off, fmt, ap);
    va_end(ap);
    if (w < 0) return;
    off += (size_t)w;
    if (off > sizeof report - 1) off = sizeof report - 1;
}

int main(int argc, char **argv) {
    struct rlimit rl;
    unsigned long limit = 1024, fd;
    int open_fds[MAX_REPORT];
    int n = 0, extra = 0, i;
    char path[256], tag[64];
    FILE *out;

    /* Enumerate FIRST: opening the report file below would itself occupy a
       descriptor and show up in the results. */
    if (getrlimit(RLIMIT_NOFILE, &rl) == 0 && rl.rlim_cur != RLIM_INFINITY)
        limit = (unsigned long)rl.rlim_cur;
    if (limit > MAX_PROBE) limit = MAX_PROBE;

    for (fd = 0; fd < limit; fd++) {
        if (fcntl((int)fd, F_GETFD) == -1) continue;   /* EBADF: not open */
        if (fd >= 3) extra++;
        if (n < MAX_REPORT) open_fds[n++] = (int)fd;
    }

    apnd("open_fds:");
    for (i = 0; i < n; i++) apnd(" %d", open_fds[i]);
    apnd("\nfd_limit_probed: %lu\nextra_fds: %d\n", limit, extra);

    /* For anything above stdio, show what it points at. pread() never blocks and
       returns ESPIPE on a pipe/socket, so it is safe against an unknown fd. */
    for (i = 0; i < n; i++) {
        char buf[65];
        ssize_t r, k;
        if (open_fds[i] < 3) continue;
        r = pread(open_fds[i], buf, sizeof buf - 1, 0);
        if (r < 0) r = 0;
        buf[r] = '\0';
        for (k = 0; k < r; k++) if (buf[k] == '\n') buf[k] = ' ';
        apnd("LEAKED fd %d first_bytes: %s\n", open_fds[i], buf);
    }

    /* argv[1] tags the output file so the two spawn paths write separate results;
       the native path passes no arguments. */
    snprintf(tag, sizeof tag, "%s", (argc > 1 && argv[1][0]) ? argv[1] : "native");
    for (i = 0; tag[i]; i++)
        if (!((tag[i] >= 'a' && tag[i] <= 'z') || (tag[i] >= '0' && tag[i] <= '9')))
            tag[i] = '_';
    snprintf(path, sizeof path, "fdprobe-%s.txt", tag);
    out = fopen(path, "w");
    if (out) { fputs(report, out); fclose(out); }

    printf("{\"status\":\"passed\",\"summary\":\"extra_fds=%d\",\"data\":\"wrote %s\","
           "\"truncated\":false,\"metadata\":{}}\n", extra, path);
    return 0;
}
EOF
cc -static -O1 -o /tmp/fdhygiene/bin/fdprobe /tmp/fdhygiene/src/fdprobe.c
```

`-static` is deliberate: a static binary needs no dynamic loader, which removes the loader and
its shared libraries as a possible source of Landlock/exec-allowlist failures unrelated to what
is being measured. If the toolchain cannot link statically, drop `-static` — the shell tool's
derived Landlock grants cover the ELF interpreter and its libraries — and re-run.

Sanity-check the probe outside the capsule, with a descriptor you know is open:

```bash
cd /tmp && exec 7</etc/hostname && /tmp/fdhygiene/bin/fdprobe baseline; cat /tmp/fdprobe-baseline.txt; exec 7<&-
```

Expected — the probe must *find* fd 7 here, otherwise it cannot prove anything later:

```text
open_fds: 0 1 2 7
fd_limit_probed: 1024
extra_fds: 1
LEAKED fd 7 first_bytes: <your hostname>
```

---

## Step 2 — install the probe in both roles

### 2a — as a `capabilities.shell.allow` binary

`shell.allow` entries are resolved against the launching process's `PATH`, so the probe must be
on it under a bare name:

```bash
sudo install -m 0755 /tmp/fdhygiene/bin/fdprobe /usr/local/bin/fdprobe
command -v fdprobe
```

### 2b — as a native tool artifact

The runtime reads a native artifact's binary from the archive entry `bin/<artifact-name>` and
stages it to `<workdir>/tools/<artifact-name>/<artifact-name>`, so the file name must match the
artifact name exactly:

```bash
mkdir -p /tmp/fdhygiene/tool/bin
cp /tmp/fdhygiene/bin/fdprobe /tmp/fdhygiene/tool/bin/fdprobe-native
cat > /tmp/fdhygiene/tool/murmur.yaml <<'EOF'
name: fdprobe-native
version: "0.1.0"
runtime: tool
implementation: native
description: "Enumerates its own open file descriptors and writes them to the capsule workdir."
input_schema: '{"type":"object","properties":{},"required":[]}'
requires_files:
  - bin/fdprobe-native
EOF
cd /tmp/fdhygiene/tool
mur build .
mur publish fdprobe-native-0.1.0.mur.zip
```

---

## Step 3 — the capsule

```bash
mkdir -p /tmp/fdhygiene/capsule
cat > /tmp/fdhygiene/capsule/murmur.yaml <<'EOF'
name: fd-hygiene-check
version: "0.1.0"

artifacts:
  - name: murmur-driver-anthropic
    version: "1.0.0"
    runtime: driver
  - name: fdprobe-native
    version: "0.1.0"
    runtime: tool

capabilities:
  network:
    allow:
      - https://api.anthropic.com
  shell:
    allow:
      - fdprobe

inference:
  transport: http
  endpoint: https://api.anthropic.com
  model: claude-sonnet-5
  api_key: ${ANTHROPIC_API_KEY}
  driver:
    artifact: murmur-driver-anthropic
  max_turns: 6
EOF
cat > /tmp/fdhygiene/capsule/task.md <<'EOF'
Call the `fdprobe` tool exactly once, with the command field set to the single word: shell
Then call the `fdprobe-native` tool exactly once, with an empty input object.
Then stop. Do not call any other tool and do not retry either call.
EOF
cd /tmp/fdhygiene/capsule
mur install
```

---

## Step 4 — run it with a deliberately leaked descriptor

`exec 7</etc/hostname` opens fd 7 in the launching shell without `FD_CLOEXEC`, so `mur` inherits
it at startup and still holds it open when it spawns the capsule's subprocesses. `/etc/hostname`
is chosen because it sits **outside** the capsule workdir and outside every Landlock grant: if
the child could see fd 7, it would be reading a file the ruleset denies it.

```bash
cd /tmp/fdhygiene/capsule
exec 7</etc/hostname
mur run --task task.md
exec 7<&-
```

Record the `workdir:` line `mur run` prints. Everything below reads from it:

```bash
WD=<the workdir path printed above>
```

---

## Step 5 — read the results

```bash
cat "$WD/fdprobe-shell.txt"
cat "$WD/fdprobe-native.txt"
```

**Expected — both files, identically:**

```text
open_fds: 0 1 2
fd_limit_probed: 1024
extra_fds: 0
```

`extra_fds: 0` and no `LEAKED` line is the pass condition, on both paths. `fd_limit_probed`
varies by host and is informational.

**A failure looks like this** — any `LEAKED` line at all, e.g.:

```text
open_fds: 0 1 2 7
fd_limit_probed: 1024
extra_fds: 1
LEAKED fd 7 first_bytes: <hostname>
```

If a file is missing entirely, the agent did not make that tool call — check
`mur trace show` for the tool calls it actually made and re-run. A missing file is **not** a
pass.

---

## Step 6 — negative control

A probe that reports "clean" on a machine where it cannot see anything is worthless. Confirm the
harness can detect the leak it is looking for by removing the fix and re-running.

In `crates/capsule-runtime/src/sandbox.rs`, comment out the first statement of
`linux_enforce::child_install_enforcement` and the `apply_fd_hygiene` call in
`runtime::dispatch_native_tool`:

```bash
# in child_install_enforcement:
#   // mark_inherited_fds_cloexec()?;
# in dispatch_native_tool:
#   // crate::sandbox::apply_fd_hygiene(&mut command);
cargo build -p capsule-runtime --release
```

Rebuild and reinstall `mur` from that tree, then repeat steps 4 and 5. **Both** files must now
show fd 7 with a `LEAKED` line whose `first_bytes` is the host's name — proving that the fd
crossed into the child, that the probe sees it, and (for the shell path) that Landlock's workdir
scope did nothing to stop a descriptor that was already open.

Restore the two lines and rebuild before recording any result.

---

## Recording the result

**PENDING — not yet run. No result has been recorded, and none should be inferred.**

No real Linux host was available when this document was written. Everything above is derived
from the code — the syscall allowlist entries, the Landlock grant shape, the artifact staging
paths — and the expected outputs are what that code implies, not observed output.

When someone runs the procedure, replace this subsection with:

- `uname -r` and the distribution, plus the `mur doctor` line confirming the enforcement tier;
- the verbatim contents of `fdprobe-shell.txt` and `fdprobe-native.txt` from step 5;
- the verbatim contents of both files from the step 6 negative control, showing the leak is
  detectable;
- any deviation from the commands as written, and why.

Then replace the callout at the top of this page with the verdict. Until that edit lands, this
page states an implemented mechanism and an **unverified** end-to-end result.

## Kernel range narrowing

Stated plainly, because it is a real, user-visible consequence rather than a footnote: on a
Linux kernel older than 5.11, `close_range(3, ~0U, CLOSE_RANGE_CLOEXEC)` fails (`ENOSYS` before
5.9, `EINVAL` for the flag on 5.9/5.10). The error propagates out of `pre_exec` and
`Command::spawn()` fails, so **every shell-tool spawn on such a host now fails** rather than
silently running without fd hygiene — on both the `KernelFull` and `KernelSeccompOnly` tiers.

That is deliberate and it is this module's established fail-closed discipline:
`drop_all_capabilities`, `install_seccomp_filter` and `apply_landlock_scope` all abort the spawn
on an unexpected error rather than degrade to a weaker sandbox. A security property that
silently omits itself when the kernel is old is not a property. The same applies to the native
tool path via `sandbox::apply_fd_hygiene`.
