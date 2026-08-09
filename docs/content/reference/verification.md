# Verification — how the containment claims are checked

Every containment claim murmur makes — Landlock scoping, seccomp filtering, `pivot_root` onto a
composed root, cgroup v2 resource ceilings, file-descriptor hygiene across `exec` — is a claim about
what a *kernel* does. Those claims are checked by a person, by hand, against a real Linux host, and
those procedures are not published as part of this site.

This page exists so that you can find them anyway and know what each one covers. Each procedure
carries its own run status; that status lives in the procedure, not here and not on the
[security warnings](security-warnings.md) page.

## Why these are not automated

A test suite that skips its way to green certifies nothing.

CI runners for this project are themselves containerised. They have no `CAP_SYS_ADMIN` over their
own user namespace, no AppArmor profile loaded, no delegated cgroup v2 subtree, and — on the macOS
half of the matrix — no Landlock, no seccomp, and no `close_range(2)` at all. A test that asserted
"the fork bomb was contained" or "the path outside the composed root does not exist" would, on those
hosts, either pass vacuously or skip. Both outcomes turn a green run into evidence about a security
property the run never touched, which is worse than having no test: it is a false assurance that
someone will later cite.

So the properties that only a real host can exhibit are checked the only way they can honestly be
checked — by hand, on a host that has the mechanism, with the observed output recorded verbatim in
the procedure itself. Each procedure carries a status box at the top saying whether it has been run,
when, and what was *not* covered. Read that box before treating any of these claims as verified.

The automated tests that do exist around this code assert the *decision* logic — which enforcement
tier a probe resolves to, which containment class a tier achieves, that a limit value is rejected
when it is zero. None of them touches a kernel boundary, and none of them is offered as evidence
that one holds.

!!! warning "These pages are excluded from the docs build"

    Because they are listed under `exclude_docs:` in `mkdocs.yml`, links *inside* these
    documents are no longer validated by `mkdocs build --strict`. A broken cross-reference or a
    stale anchor within a procedure will not fail the build. If you edit one, check its links by
    hand — the safety net that covers the rest of this site does not cover them.

## The procedures

| Procedure | What it covers | When it is run |
|---|---|---|
| **Sealed containment — manual verification** | The `sealed` class end to end: the `E-CAP-003` refusal when the AppArmor profile is absent, the composed root observed from inside a live capsule's shell tool (paths outside it return `ENOENT`, not `EACCES`), `"containment_achieved":"sealed"` in `trace.jsonl`, and the refusal to run at a weaker class inside a plain container. | Release gate for the `sealed` class — see [below](#the-sealed-release-gate). Re-run whenever the composed-root construction, the host probe, or the tier→class mapping changes. |
| **Resource limits — manual verification** | The three mechanisms bounding the native subprocess tree: `setrlimit(2)` per-process ceilings, the cgroup v2 scope (fork bomb, memory hog, CPU, I/O), and the periodic workdir-size check. Ten scenarios, including the `E-RUN-012` fail-closed launch refusal and the macOS gap behind `W-SEC-010`. | On a Linux host with systemd user cgroup delegation configured, whenever `capabilities.resources` enforcement or the cgroup delegation path changes. |
| **Subprocess fd hygiene — verification** | The negative property that a descriptor open in the runtime process at spawn time is not visible inside the spawned child, across both spawn paths (shell tool and native tool), on both kernel tiers. Landlock cannot substitute for this: an inherited fd was opened before the ruleset existed. | Whenever either spawn path's pre-exec window changes. |
| **Workdir `Execute` rights and declared `workdir_exec` — manual verification** | That `capabilities.shell.allow` is *complete*: with the default `capabilities.filesystem.workdir_exec: false`, a binary planted in the session workdir under an allowlisted basename does not execute, because the workdir's Landlock rule carries no `Execute` right. Also the declared opt-in's whole visible surface — the binary runs, the achieved class drops to `advisory`, `--explain-scope` and `trace.jsonl` say so, and `containment: scoped` alongside it refuses with `E-CAP-003`. | Whenever the workdir grant's right set, the exec-grant derivation, or the tier→class mapping changes. |
| **Shell-binary reachability under `sealed` — manual verification** | That a `capabilities.shell.allow` grant which cannot actually *function* inside a composed root fails at launch rather than deep in a run: the `E-CAP-006` refusal for an interpreted entrypoint whose package tree nothing declared reaches, the `W-SEC-012` warning for a compiler driver whose `cc1`/`as`/`ld` helpers have no `Execute` grant, and the two negative controls — a system `/usr` interpreter that needs no grant, and a declared `interpreter_runtime` that makes a real compile succeed. | Whenever the reachability checks, the fixed sealed runtime tree's right set, or the known-driver registry changes. |
| **Workdir device-node escape — manual verification** | That a capsule cannot create a character- or block-device node inside its own workdir and read the raw host filesystem through it — the Landlock workdir grant withholding those rights, and the shell child's capability drop, which are two independent mechanisms for the same refusal. Also that FIFO and ordinary file creation still work, and that the child is left non-dumpable. | Whenever the workdir grant's right set, the child's capability drop, or the pre-exec hardening sequence changes. |
| **Unmediated `AF_UNIX` sockets — manual verification** | That a capsule cannot open a unix-domain socket by default and so cannot reach a host daemon socket such as `/var/run/docker.sock`; that `AF_NETLINK` and `AF_PACKET` are refused with and without the opt-in; that `capabilities.network.unix_sockets: true` really does hand the family back; and whether real workloads survive the default deny. | Whenever the `socket(2)`-domain rule, the `unix_sockets` opt-in, or the set of denied address families changes. |
| **The fixed capsule device set — manual verification** | That `/dev/null` is readable *and* writable, `/dev/zero` and `/dev/urandom` readable but not writable, every other device refused, and `/dev` itself not enumerable — plus that a host missing one of the three degrades rather than failing the launch, and whether three devices are enough for real workloads. | Whenever the fixed device set, its per-device rights, or the missing-device fallback changes. |
| **Seccomp-notify TOCTOU audit** | A recorded architectural verdict, not a pass/fail run: a seccomp-notify supervisor read `execve`/`execveat`/`connect`/`sendto` pointer arguments out of the notifying task's memory and then answered `SECCOMP_USER_NOTIF_FLAG_CONTINUE`, so the kernel dereferenced the same pointer again after the decision. A hostile multithreaded subprocess could have one decision computed and a different one enforced. Ships with race probes that reproduce it. **Both halves of that supervisor have since been deleted** — network enforcement moved to a namespace + egress proxy, exec enforcement to Landlock `Execute` rights — so this is now a historical record of why, not a description of live code. | Closed. Re-open it only if a `Notify` rule is ever added back to `install_seccomp_filter`. |

### The `sealed` release gate

The sealed class's automated conformance checks are **advisory until a hand-run pass confirms them
on a real host**. Each check records the verdict it expects for each containment class and reports
what it observed, but for `sealed` that expectation gates nothing: grading a release on a column
nobody has validated against a real composed root would be exactly the false assurance the checks
exist to prevent.

Promoting those expectations to gating ones belongs to whoever runs the sealed-containment procedure
and can say what a real sealed host actually does.

## Where to read them

The documents live in the repository, at the paths below, on the `main` branch:

- **Network namespace + egress proxy — manual verification** —
  [`docs/content/reference/network-namespace-egress-proxy-manual-verification.md`](https://github.com/murmur-nexus/murmur/blob/main/docs/content/reference/network-namespace-egress-proxy-manual-verification.md)
- **Sealed containment — manual verification** —
  [`docs/content/reference/sealed-containment-manual-verification.md`](https://github.com/murmur-nexus/murmur/blob/main/docs/content/reference/sealed-containment-manual-verification.md)
- **Resource limits — manual verification** —
  [`docs/content/reference/resource-limits-manual-verification.md`](https://github.com/murmur-nexus/murmur/blob/main/docs/content/reference/resource-limits-manual-verification.md)
- **Subprocess fd hygiene — verification** —
  [`docs/content/reference/subprocess-fd-hygiene-verification.md`](https://github.com/murmur-nexus/murmur/blob/main/docs/content/reference/subprocess-fd-hygiene-verification.md)
- **Workdir `Execute` rights and declared `workdir_exec` — manual verification** —
  [`docs/content/reference/workdir-exec-landlock-manual-verification.md`](https://github.com/murmur-nexus/murmur/blob/main/docs/content/reference/workdir-exec-landlock-manual-verification.md)
- **Shell-binary reachability under `sealed` — manual verification** —
  [`docs/content/reference/shell-binary-reachability-manual-verification.md`](https://github.com/murmur-nexus/murmur/blob/main/docs/content/reference/shell-binary-reachability-manual-verification.md)
- **Workdir device-node escape — manual verification** —
  [`docs/content/reference/workdir-device-node-manual-verification.md`](https://github.com/murmur-nexus/murmur/blob/main/docs/content/reference/workdir-device-node-manual-verification.md)
- **Unmediated `AF_UNIX` sockets — manual verification** —
  [`docs/content/reference/af-unix-sockets-manual-verification.md`](https://github.com/murmur-nexus/murmur/blob/main/docs/content/reference/af-unix-sockets-manual-verification.md)
- **The fixed capsule device set — manual verification** —
  [`docs/content/reference/capsule-device-set-manual-verification.md`](https://github.com/murmur-nexus/murmur/blob/main/docs/content/reference/capsule-device-set-manual-verification.md)
- **Seccomp-notify TOCTOU audit** —
  [`docs/content/reference/seccomp-notify-toctou-audit.md`](https://github.com/murmur-nexus/murmur/blob/main/docs/content/reference/seccomp-notify-toctou-audit.md)

They stay at those paths deliberately: code comments and test doc-comments across the runtime and
CLI crates cite them by path as the reason a given piece of enforcement is shaped the way it is.

## Related

- [Containment class](manifest-schema.md#field-containment) — what `advisory`, `scoped`, and `sealed`
  each require of the host, and what each one enforces.
- [Host resource limits](manifest-schema.md#host-resource-limits) — the `capabilities.resources`
  fields, their defaults, and their platform behavior.
- [Security warnings](security-warnings.md) — the residual gaps the runtime reports at runtime,
  including the permanent macOS ones.
