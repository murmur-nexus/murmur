# The sandboxed child is dumpable — what that opens, and why it is accepted

`mur` (and `mur-roost`) marks **its own process** non-dumpable at startup and keeps it that way for
its entire life. The **shell-tool subprocess it forks** deliberately does the opposite: it
re-enables its own `dumpable` flag before it `execve`s the allowlisted binary.

This page records that asymmetry: what it costs, why the runtime cannot work without it, and why
the trade is judged acceptable.

## The two `prctl` calls

| call | where | lifetime | effect |
|---|---|---|---|
| `prctl(PR_SET_DUMPABLE, 0)` | `capsule_runtime::security::harden_process_dumpable`, first statement of `main()` in `mur` and `mur-roost` | the whole runtime process | nobody — not even a same-UID process — can read the runtime's `/proc/<pid>/environ` or `/proc/<pid>/mem` |
| `prctl(PR_SET_DUMPABLE, 1)` | `sandbox::linux_enforce::restore_child_dumpable`, inside the forked child's `pre_exec` | that one shell-tool subprocess | undoes, for the child only, the flag it inherited through `fork()` |

The first is unchanged and unconditional. The second exists only in the child.

## Why the child cannot stay non-dumpable

Kernel enforcement of `capabilities.shell.allow` is implemented with seccomp user notification: the
child's `execve` traps, and a supervisor thread **inside the runtime process** reads the `execve`
pathname out of the stopped child's memory (`sandbox::linux_enforce::read_cstr_from_child`, which
opens `/proc/<pid>/mem`) so it can compare it against the allowlist.

Every `/proc/<pid>/*` read is gated by the kernel's `ptrace_may_access` check, and that check fails
for a **non-dumpable** target unless the reader is root or holds `CAP_SYS_PTRACE` — sharing the
target's UID is not enough, and neither is being its parent. `fork()` copies the `mm` flags, so
before this fix the child inherited `dumpable = 0` from the runtime and the supervisor's own read of
its own child failed with `EACCES`. The supervisor fails closed, so *every* allowlisted `execve` was
denied, and a non-root user saw exactly one thing from every shell tool call:

```text
Permission denied (os error 13)
```

That held at every containment class (`advisory`, `scoped`) and on both kernel enforcement tiers
(`KernelFull`, `KernelSeccompOnly`). Running `mur` as root hid it, because root reads any
`/proc/<pid>/mem` regardless of the target's flag.

## What is now reachable

For as long as a shell-tool subprocess runs, any **other process on the host running as the same
UID** can:

- `ptrace`-attach it (`PTRACE_ATTACH`, `PTRACE_SEIZE`), subject to whatever `yama.ptrace_scope`
  allows on that host;
- read its `/proc/<pid>/mem` — its full address space, including anything the command has read from
  a file or received over the network;
- read its `/proc/<pid>/environ` — the environment it was `execve`d with;
- obtain a core dump of it, where core dumps are enabled.

This is the same side channel that `harden_process_dumpable` closes for the runtime process. It is
open again for the child, deliberately, and only for the child's lifetime.

It is **not** a way back into the runtime process: `mur`'s own `/proc/<pid>/environ` and
`/proc/<pid>/mem` stay unreadable to the same reader, because `mur` itself never becomes dumpable.

## Why it is accepted

The two processes hold different things, and that is the whole argument.

- **The runtime process may hold raw secrets.** Its kernel-recorded environment is whatever the
  operator's shell, `.env` file or CI secret injection put there, unfiltered. That is what
  `harden_process_dumpable` was added to protect, and it still does.
- **The child holds a pre-filtered subset.** `shell::build_shell_env` reduces the child's
  environment to `shell::DEFAULT_ENV_BASELINE` (`PATH`, `HOME`, `USER`, `LANG`, `LC_ALL`, `TMPDIR`,
  `TEMP`, `TMP`, `CARGO_HOME`, `RUSTUP_HOME`, `TERM`) plus the overrides the manifest declares
  explicitly, and the `Command` is `env_clear()`ed before those are applied. Whatever a
  same-UID attacker reads out of the child, it is that set — not the runtime's own environment.
- **The alternative is no enforcement at all.** Without this, the kernel-enforced exec allowlist
  cannot function for a non-root user; the only "fix" that keeps the child non-dumpable is to stop
  running the seccomp-notify supervisor, i.e. to give up `capabilities.shell.allow` enforcement.
- **The reader must already be inside the trust boundary.** Reaching this requires code execution on
  the host as the same UID that runs `mur`. Such a reader can already read the same files `mur`
  reads, attach to any other same-UID process that has not hardened itself, and start its own `mur`.

The child remains confined in every other respect while it runs: Landlock filesystem scope
(`KernelFull`), the seccomp syscall allowlist, dropped capabilities, `no_new_privs`, CLOEXEC'd
inherited descriptors, and its cgroup/rlimit ceilings.

## Where the ordering matters

`restore_child_dumpable` runs **after** `drop_all_capabilities` and **immediately before**
`install_seccomp_filter` in `sandbox::linux_enforce::child_install_enforcement`. Both halves of that
placement are load-bearing:

- **After capability-dropping**, because `commit_creds` resets a task's `dumpable` to
  `/proc/sys/fs/suid_dumpable` whenever its permitted capability set shrinks. Restoring the flag
  first would let a root-uid capsule's capability drop silently clear it again — on a host with
  `fs.suid_dumpable` set to `0` or `2`, reintroducing this exact defect for the hosts least likely
  to notice.
- **Before the filter is installed**, because that is what the supervisor needs; and as late as
  possible in the setup sequence, so the inherited descriptors are already close-on-exec and the
  capabilities already gone by the time the child becomes attachable.

The step is fail-closed like its neighbours: an unexpected `prctl` failure writes a named message to
the `pre_exec` diagnostic pipe and aborts the spawn, rather than continuing with a child whose every
`execve` would be denied anyway.

## Platform scope

Linux only. `prctl(2)` and `/proc/<pid>/mem` do not exist on the other supported targets;
`harden_process_dumpable` is already a no-op off Linux, macOS resolves permanently to
`EnforcementTier::EnvironmentOnly`, and there is no seccomp-notify supervisor there to need this.
