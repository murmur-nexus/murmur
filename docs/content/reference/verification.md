# Verification — how the containment claims are checked

Every containment claim murmur makes — Landlock scoping, seccomp filtering, `pivot_root` onto a
composed root, cgroup v2 resource ceilings, file-descriptor hygiene across `exec` — is a claim
about what a *kernel* does. Each claim can be verified manually against a real Linux host, and
this page lists the procedures for doing so: what each one covers, and where to read it. Every
procedure carries its own run status, recorded in the procedure itself.

## Why these are not automated

A containment boundary can only be asserted on a host that has the mechanism behind it. On a host
without one — an unprivileged container, a kernel below 5.13, macOS — a test asserting that a fork
bomb was contained, or that a path outside the composed root does not exist, passes vacuously or
skips. Either outcome reports on a boundary the run never touched. So these properties are checked
on a real host, with the observed output recorded in the procedure itself.

The automated tests assert the *decision* logic instead: which enforcement tier a probe resolves
to, which containment class a tier achieves, that a zero limit value is rejected.

## The procedures

Each procedure lives in the murmur repository on the `main` branch; the name links to it.

| Procedure | What it covers | When it is run |
|---|---|---|
| [**Network namespace + egress proxy**](https://github.com/murmur-nexus/murmur/blob/main/docs/content/reference/network-namespace-egress-proxy-manual-verification.md) | That a capsule's native subprocess tree runs inside its own network namespace, and that the only way out is a proxy in the runtime process applying `capabilities.network.allow`. Includes the `E-CAP-005` refusal on a host that cannot provide a namespace. | Whenever the namespace setup, the egress proxy, or the allow-list enforcement path changes. |
| [**Sealed containment**](https://github.com/murmur-nexus/murmur/blob/main/docs/content/reference/sealed-containment-manual-verification.md) | The `sealed` class end to end: the `E-CAP-003` refusal when the AppArmor profile is absent, the composed root observed from inside a live capsule's shell tool (paths outside it return `ENOENT`, not `EACCES`), `"containment_achieved":"sealed"` in `trace.jsonl`, and the refusal to run at a weaker class inside a plain container. | Release gate for the `sealed` class. Re-run whenever the composed-root construction, the host probe, or the tier→class mapping changes. |
| [**Resource limits**](https://github.com/murmur-nexus/murmur/blob/main/docs/content/reference/resource-limits-manual-verification.md) | The three mechanisms bounding the native subprocess tree: `setrlimit(2)` per-process ceilings, the cgroup v2 scope (fork bomb, memory hog, CPU, I/O), and the periodic workdir-size check. Ten scenarios, including the `E-RUN-012` fail-closed launch refusal and the macOS gap behind `W-SEC-010`. | On a Linux host with systemd user cgroup delegation configured, whenever `capabilities.resources` enforcement or the cgroup delegation path changes. |
| [**Subprocess fd hygiene**](https://github.com/murmur-nexus/murmur/blob/main/docs/content/reference/subprocess-fd-hygiene-verification.md) | The negative property that a descriptor open in the runtime process at spawn time is not visible inside the spawned child, across both spawn paths (shell tool and native tool), on both kernel tiers. Landlock cannot substitute for this: an inherited fd was opened before the ruleset existed. | Whenever either spawn path's pre-exec window changes. |
| [**Workdir `Execute` rights and declared `workdir_exec`**](https://github.com/murmur-nexus/murmur/blob/main/docs/content/reference/workdir-exec-landlock-manual-verification.md) | That `capabilities.shell.allow` is *complete*: with the default `capabilities.filesystem.workdir_exec: false`, a binary planted in the session workdir under an allowlisted basename does not execute, because the workdir's Landlock rule carries no `Execute` right. Also the declared opt-in's whole visible surface — the binary runs, the achieved class drops to `advisory`, `--explain-scope` and `trace.jsonl` say so, and `containment: scoped` alongside it refuses with `E-CAP-003`. | Whenever the workdir grant's right set, the exec-grant derivation, or the tier→class mapping changes. |
| [**Shell-binary reachability under `sealed`**](https://github.com/murmur-nexus/murmur/blob/main/docs/content/reference/shell-binary-reachability-manual-verification.md) | That a `capabilities.shell.allow` grant which cannot actually *function* inside a composed root fails at launch rather than deep in a run: the `E-CAP-006` refusal for an interpreted entrypoint whose package tree nothing declared reaches, the `W-SEC-012` warning for a compiler driver whose `cc1`/`as`/`ld` helpers have no `Execute` grant, and the two negative controls — a system `/usr` interpreter that needs no grant, and a declared `interpreter_runtime` that makes a real compile succeed. | Whenever the reachability checks, the fixed sealed runtime tree's right set, or the known-driver registry changes. |
| [**Workdir device-node escape**](https://github.com/murmur-nexus/murmur/blob/main/docs/content/reference/workdir-device-node-manual-verification.md) | That a capsule cannot create a character- or block-device node inside its own workdir and read the raw host filesystem through it — the Landlock workdir grant withholding those rights, and the shell child's capability drop, which are two independent mechanisms for the same refusal. Also that FIFO and ordinary file creation still work, and that the child is left non-dumpable. | Whenever the workdir grant's right set, the child's capability drop, or the pre-exec hardening sequence changes. |
| [**Unmediated `AF_UNIX` sockets**](https://github.com/murmur-nexus/murmur/blob/main/docs/content/reference/af-unix-sockets-manual-verification.md) | That a capsule cannot open a unix-domain socket by default and so cannot reach a host daemon socket such as `/var/run/docker.sock`; that `AF_NETLINK` and `AF_PACKET` are refused with and without the opt-in; that `capabilities.network.unix_sockets: true` really does hand the family back; and whether real workloads survive the default deny. | Whenever the `socket(2)`-domain rule, the `unix_sockets` opt-in, or the set of denied address families changes. |
| [**The fixed capsule device set**](https://github.com/murmur-nexus/murmur/blob/main/docs/content/reference/capsule-device-set-manual-verification.md) | That `/dev/null` is readable *and* writable, `/dev/zero` and `/dev/urandom` readable but not writable, every other device refused, and `/dev` itself not enumerable — plus that a host missing one of the three degrades rather than failing the launch, and whether three devices are enough for real workloads. | Whenever the fixed device set, its per-device rights, or the missing-device fallback changes. |

## Related

- [Containment class](manifest-schema.md#field-containment) — what `advisory`, `scoped`, and `sealed`
  each require of the host, and what each one enforces.
- [Host resource limits](manifest-schema.md#host-resource-limits) — the `capabilities.resources`
  fields, their defaults, and their platform behavior.
- [Security warnings](security-warnings.md) — the residual gaps the runtime reports at runtime,
  including the permanent macOS ones.
