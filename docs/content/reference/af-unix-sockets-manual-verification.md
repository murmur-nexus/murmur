# Verification — unmediated AF_UNIX sockets

!!! warning "Status: **NOT RUN.** Implemented and unit-tested; never executed on a real Linux host."

    This page, not the published [security warnings](diagnostics.md) reference, is where the
    run status of this mechanism lives. Record the outcome of each scenario below, verbatim, in
    [Recording the result](#recording-the-result) — including the date and the host it ran on.

This procedure confirms the `socket(2)` domain denial described under [W-SEC-005](diagnostics.md#w-sec-005) — that
a capsule cannot reach `/var/run/docker.sock`, and cannot open netlink or packet sockets at all — on
real hardware. **It is deliberately not automated.** There is deliberately *no* committed test that
asserts "`socket(AF_UNIX, ...)` is refused", and a green `cargo test`/CI run is not evidence: this
repo's CI has never resolved to a Linux enforcement tier where the rule is even installed. The
committed unit tests (`sandbox::tests::denied_socket_domains_*`) assert only what a pure Rust
function returns, not what a kernel does. Until someone runs the steps below and records the result,
the mechanism is *implemented*, not *verified*.

Scenario 5 is a compatibility check rather than a security check, and it is the one most likely to
come back negative. Run it before treating the default-deny as safe to rely on broadly.

### Prerequisites

- A **real, uncontainerized** Linux host. Not Docker, not a rootless container, not WSL. Scenario 2
  needs a real host Docker daemon socket to exist and be reachable *without* the sandbox; inside a
  container you will get a refusal for the wrong reason and a false pass.
- Either Linux tier works. This rule is pure seccomp — it does not involve Landlock — so a
  kernel <5.13 (`KernelSeccompOnly`) exercises exactly the same code path as kernel ≥5.13
  (`KernelFull`). Scenario 1 is the only step that needs a Landlock-capable kernel.
- A checkout of this repository and a working `cargo`.
- Docker installed and running, with `/var/run/docker.sock` (or `/run/docker.sock`) present.
- `root` (or a user in the `docker` group) for scenario 2's "before" half — the point is to confirm
  the socket is genuinely reachable outside the sandbox, so that a refusal inside it means
  something.

Confirm the tier the host resolves to before anything else:

```bash
cd /path/to/murmur
sudo -E cargo test -p capsule-runtime --lib \
  sandbox::linux_integration_tests::kernel_tier_allows_exec_within_shell_allowlist \
  -- --nocapture
```

A `SKIP — PROVES NOTHING` line in that output means the host is not on a kernel tier and **the rest
of this procedure is meaningless on this machine**. Stop and find a different host.

### Scratch harness

Scenarios 2–4 drive `shell::execute_shell` directly, which is crate-private, so they run from a
scratch test appended to `crates/capsule-runtime/src/sandbox.rs`. **Do not commit it.** Append this
to the end of `mod linux_integration_tests` (just before that module's closing brace):

```rust
    #[test]
    fn scratch_unix_socket_acceptance() {
        let tier = detect_enforcement_tier();
        eprintln!("TIER: {tier:?}");

        let workdir = tempfile::tempdir().unwrap();
        let policy = CapabilityPolicy {
            shell_allow: vec!["bash".to_string(), "python3".to_string()],
            // Flip to `true` to exercise the declared-opt-in path (scenario 3).
            unix_sockets_allowed: std::env::var("SCRATCH_UNIX_SOCKETS").is_ok(),
            ..CapabilityPolicy::default()
        };
        eprintln!("UNIX_SOCKETS_ALLOWED: {}", policy.unix_sockets_allowed);
        let exec_allow_paths = resolve_exec_allowlist(&policy.shell_allow);
        let enforcement = ShellEnforcement {
            tier,
            network_allow_ips: Vec::new(),
            unix_sockets_allowed: policy.unix_sockets_allowed,
            landlock_grants: LandlockGrant::non_listable_files(resolve_landlock_grants(
                &exec_allow_paths,
            )),
            exec_allow_paths,
        };

        let script = std::env::var("SCRATCH_SCRIPT").expect("set SCRATCH_SCRIPT");
        let result = crate::shell::execute_shell(
            "bash",
            &["-c", &script],
            &[],
            workdir.path(),
            &policy,
            &enforcement,
        )
        .expect("execute_shell must return Ok, not Err");
        eprintln!("EXIT: {}", result.exit_code);
        eprintln!("OUT: {}", result.stdout);
        eprintln!("ERR: {}", result.stderr);
    }
```

Run one scenario at a time with:

```bash
sudo -E SCRATCH_SCRIPT='<the script for this scenario>' \
  cargo test -p capsule-runtime --lib \
  sandbox::linux_integration_tests::scratch_unix_socket_acceptance -- --nocapture --exact
```

When you are done: `git checkout crates/capsule-runtime/src/sandbox.rs`.

### Scenario 1 — does Landlock mediate `connect()` to a *pathname* unix socket?

**This is a documented finding, not a pass/fail gate.** The fix does not depend on the answer — it
denies the domain at `socket()` creation, before any `connect()` — but the answer must be on record
so nobody spends a future card waiting for a kernel upgrade that cannot help. Run it on the
highest-ABI kernel available.

First, establish what this kernel and this checkout can even express:

```bash
cd /path/to/murmur
uname -r                                              # ABI v6 needs 6.12+; v5 needs 6.7+
grep -n '^landlock' crates/capsule-runtime/Cargo.toml # the pinned crate bounds the ABI we can name
```

Then probe the kernel and the socket directly, with no murmur code involved — the raw syscall, so no
crate version affects the answer:

```bash
# Requires `python3` and a kernel with Landlock. Uses the raw syscalls so no crate version matters.
sudo python3 - <<'EOF'
import ctypes, os, socket, struct, sys
libc = ctypes.CDLL("libc.so.6", use_errno=True)
# landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION) -> highest supported ABI
abi = libc.syscall(444, None, 0, 1)
print("kernel Landlock ABI:", abi)
if abi < 1:
    print("no Landlock on this kernel — record that and stop"); sys.exit(0)
print("LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET exists at ABI >= 6:", abi >= 6)
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
try:
    s.connect("/var/run/docker.sock")
    print("UNSANDBOXED_CONNECT_OK — the socket is reachable on this host")
except OSError as e:
    print("UNSANDBOXED_CONNECT_FAILED:", e, "— fix this before scenario 2, or its result is meaningless")
EOF
```

- **Expected finding, to record verbatim in the build summary and here:** even at ABI v6, Landlock's
  `LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET` covers *abstract* unix sockets only. `docker.sock` is a
  pathname socket, so a Landlock filesystem domain does not mediate `connect()` to it at any ABI.
- **If the run contradicts that** (a Landlock domain does block the connect), record it — it does not
  change this fix, but it changes what a future per-path allowlist could be built on.

### Scenario 2 — the default deny: `docker.sock` is unreachable

This is the escape itself.

```bash
SCRATCH_SCRIPT='python3 -c "
import socket
try:
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
except OSError as e:
    print(\"SOCKET_REFUSED\", e.errno, e.strerror); raise SystemExit(0)
try:
    s.connect(\"/var/run/docker.sock\")
    s.sendall(b\"GET /version HTTP/1.0\r\n\r\n\")
    print(\"DOCKER_REACHED\", s.recv(256)[:64])
except OSError as e:
    print(\"CONNECT_REFUSED\", e.errno, e.strerror)
" 2>&1 | tail -3'
```

- **Expected (fixed):** `SOCKET_REFUSED 13 Permission denied`. Errno 13 is `EACCES`, returned by the
  `socket()`-domain filter described above, on purpose. The refusal happens at `socket()`, so no
  `connect()` is ever attempted — independent of whatever mechanism governs a blocked `AF_INET`/
  `AF_INET6` destination (today, the capsule's network namespace and egress proxy).
- **Regression (unfixed):** `DOCKER_REACHED` followed by an HTTP response from the daemon. That
  response is host root.

Repeat against the other conventional path, which some distros use instead:

```bash
SCRATCH_SCRIPT='python3 -c "
import socket
try:
    socket.socket(socket.AF_UNIX, socket.SOCK_STREAM).connect(\"/run/docker.sock\")
    print(\"DOCKER_REACHED\")
except OSError as e:
    print(\"REFUSED\", e.errno, e.strerror)
" 2>&1 | tail -2'
```

Confirm the refusal is domain-specific and did not break IP sockets — `AF_INET` must still be
governed by `capabilities.network.allow` exactly as before, not by this rule:

```bash
SCRATCH_SCRIPT='python3 -c "
import socket
s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)   # creation must SUCCEED
print(\"AF_INET_SOCKET_OK\")
try:
    s.connect((\"127.0.0.1\", 9))
    print(\"CONNECT_OK\")
except OSError as e:
    print(\"CONNECT_REFUSED\", e.errno, e.strerror)      # refusal here is the capsule's own network namespace + egress proxy, not this AF_UNIX-domain filter
" 2>&1 | tail -2'
```

- **Expected:** `AF_INET_SOCKET_OK` first. If `socket(AF_INET, ...)` itself fails, the new rule is
  matching the wrong argument and the whole network path is broken.

### Scenario 3 — the declared opt-in works

Same script as scenario 2, with the opt-in on. This confirms the grant is real and not a
one-way ratchet.

```bash
sudo -E SCRATCH_UNIX_SOCKETS=1 SCRATCH_SCRIPT='python3 -c "
import socket
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
print(\"SOCKET_OK\")
try:
    s.connect(\"/var/run/docker.sock\")
    print(\"DOCKER_REACHED\")
except OSError as e:
    print(\"CONNECT_FAILED\", e.errno, e.strerror)
" 2>&1 | tail -2' \
  cargo test -p capsule-runtime --lib \
  sandbox::linux_integration_tests::scratch_unix_socket_acceptance -- --nocapture --exact
```

- **Expected:** `UNIX_SOCKETS_ALLOWED: true` in the output, then `SOCKET_OK`, then `DOCKER_REACHED`.
  A capsule that declares `capabilities.network.unix_sockets: true` really does get the daemon
  socket back — that is what makes the grant a deliberate, auditable widening rather than a
  decoration.
- **Regression:** `SOCKET_OK` missing while `UNIX_SOCKETS_ALLOWED: true` — the flag is not reaching
  the filter, so the opt-in is inert and operators cannot un-break a legitimate workload.

### Scenario 4 — netlink and packet sockets are denied with *and* without the opt-in

There is no manifest key for either, so the result must be identical in both runs.

```bash
# Run this twice: once as-is, once with SCRATCH_UNIX_SOCKETS=1 prefixed.
SCRATCH_SCRIPT='python3 -c "
import socket
for name, dom in ((\"AF_NETLINK\", 16), (\"AF_PACKET\", 17)):
    try:
        socket.socket(dom, socket.SOCK_RAW)
        print(name, \"CREATED\")
    except OSError as e:
        print(name, \"REFUSED\", e.errno, e.strerror)
" 2>&1 | tail -3'
```

- **Expected, both runs, identical:** `AF_NETLINK REFUSED 13 Permission denied` and
  `AF_PACKET REFUSED 13 Permission denied`.
- **Note:** `AF_PACKET` also needs `CAP_NET_RAW`, which the child capability drop already removes —
  so a bare `EPERM` (errno 1) means you are seeing the capability drop, not this rule. Errno 13
  (`EACCES`) is the seccomp rule. Both are refusals; only errno 13 confirms *this* mechanism.
- **Regression:** either family created in either run, or `AF_NETLINK` refused without the opt-in but
  created with it — the flag must not touch these two.

### Scenario 5 — full SWE-bench workload compatibility (the one that may fail)

`AF_UNIX` is how glibc's NSS reaches `nscd`, how `syslog(3)` reaches `/dev/log`, and how some locale
and D-Bus paths work. Denying it by default is the correct posture, but whether the workloads this
project actually runs survive it is an empirical question. **Run the full SWE-bench workload, not a
sample**, on a host resolving to a kernel tier:

```bash
cd /path/to/murmur
# Baseline first — the same workload with the rule disabled, so a failure can be attributed.
# Temporarily declare the opt-in in the capsule manifest under test:
#     capabilities:
#       network:
#         unix_sockets: true
sudo -E <your-swe-bench-invocation> 2>&1 | tee /tmp/swebench-unix-allowed.log

# Then the shipped default — remove the `unix_sockets:` key entirely (do not set it to false;
# absent is the shape real manifests will have).
sudo -E <your-swe-bench-invocation> 2>&1 | tee /tmp/swebench-unix-denied.log

# Compare resolved/failed counts and diff the failure sets.
diff <(grep -E '^(PASS|FAIL)' /tmp/swebench-unix-allowed.log | sort) \
     <(grep -E '^(PASS|FAIL)' /tmp/swebench-unix-denied.log | sort)
```

Also check for the specific suspects, which fail quietly rather than loudly:

```bash
grep -iE 'nscd|getpwnam|getaddrinfo|Name or service not known|syslog|/dev/log|locale' \
  /tmp/swebench-unix-denied.log | head -40
```

- **Expected:** an empty diff — identical resolved/failed sets. That is what makes the default-deny
  safe to rely on.
- **If the diff is non-empty:** record it here, in full, with the failing task IDs and the mechanism
  (NSS? syslog? something else?). Then consider a **narrower rule** — for example denying only
  `SOCK_STREAM`/`SOCK_SEQPACKET` unix sockets while leaving `SOCK_DGRAM` (syslog's shape) alone, via
  a second `ScmpArgCompare` on `socket()`'s arg 1. **Do not** respond by flipping the default to
  allow: that restores the `docker.sock` escape in full, and the roadmap explicitly rules it out.

### Recording the result

Record what actually happened **in this file**, under a dated `### Run of YYYY-MM-DD` heading —
including the scenario-1 Landlock finding verbatim, the scenario-5 compatibility outcome, and the
host, kernel version and resolved tier the run was made on. Then update the status box at the top of
this page.

Run status does not go on the published [security warnings](diagnostics.md) page. That page
states what the `socket(2)`-domain rule enforces and what
[`capabilities.network.unix_sockets`](manifest.md#field-capabilities) re-widens, and links
here for anyone who wants to check it by hand; it carries no verification status, no dated run, and
no scenario. If scenario 5 forces a **narrower** rule, the published description of the rule changes
with it — but a compatibility failure is never answered by flipping the default back to allow.
