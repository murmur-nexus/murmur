# Diagnostics

`mur` reports every problem it finds with a code. An `E-*` code is a failure that stops the
command; a `W-*` code is a warning that lets it continue. The index lists every code with the
section that explains it.

## Index { #index }

| Code | Meaning | Details |
|---|---|---|
| `E-BLD-001` | Manifest `name:` is not a valid artifact identifier | [E-BLD-001](#e-bld-001) |
| `E-BLD-002` | `requires_files:` entry is unsafe (absolute, `..`, symlink) or collides inside the archive | [E-BLD-002](#e-bld-002) |
| `E-BLD-003` | Packed entry set is not a launchable wasm payload | [E-BLD-003](#e-bld-003) |
| `E-CAP-001` | A `capabilities.network.allow` entry could not be parsed | [E-CAP-001](#e-cap-001) |
| `E-CAP-002` | A `filesystem.scope` is not relative to the workdir, or escapes it via `..` | [E-CAP-002](#e-cap-002) |
| `E-CAP-003` | Declared containment floor (`advisory`\|`scoped`\|`sealed`) is not achievable on this host | [E-CAP-003](#e-cap-003) |
| `E-CAP-004` | `capabilities.shell.staged_runtime` is declared below an effective `sealed` containment floor | [E-CAP-004](#e-cap-004) |
| `E-CAP-005` | This host cannot give the capsule's native subprocess tree its own network namespace, so `capabilities.network.allow` cannot be enforced for it | [E-CAP-005](#e-cap-005) |
| `E-CAP-006` | Nothing declared makes an allowlisted interpreted entrypoint's package tree reachable inside a `sealed` composed root | [E-CAP-006](#e-cap-006) |
| `E-CAP-007` | an export root resolves outside the accessible workdir | [E-CAP-007](#e-cap-007) |
| `E-CAP-008` | a persistent capsule declares `exports.peer_files` without a short enough `max_ttl` | [E-CAP-008](#e-cap-008) |
| `E-CAP-009` | `capabilities.state.store` does not name a usable durable store | [E-CAP-009](#e-cap-009) |
| `E-CAP-010` | An artifact entry's `config:` block cannot be delivered as `MURMUR_ARTIFACT_CONFIG` | [E-CAP-010](#e-cap-010) |
| `E-CAP-011` | `context.record_store`, or `mur run --context`, does not name one conversation record directory | [E-CAP-011](#e-cap-011) |
| `E-CAP-012` | A `capabilities.filesystem.read_only` entry is not a usable workdir subpath | [E-CAP-012](#e-cap-012) |
| `E-CAP-013` | An artifact claims the name of a tool the runtime provides itself | [E-CAP-013](#e-cap-013) |
| `E-CNV-001` | No such record store or context id under `~/.murmur/conversations/` | [E-CNV-001](#e-cnv-001) |
| `E-CNV-002` | A context id is present under more than one record store | [E-CNV-002](#e-cnv-002) |
| `E-CNV-003` | `mur conversation truncate --keep` is not a usable number of messages to keep | [E-CNV-003](#e-cnv-003) |
| `E-CFG-001` | No inference provider configured and wizard cannot run in non-interactive mode | [`mur new`](cli.md#mur-new) |
| `E-CFG-002` | `mur config set` given an unsupported dotted key | [`mur config`](cli.md#mur-config) |
| `E-DEPLOY-001` | No `--host` given, or an `--env` value is not `KEY=VALUE` | [`mur deploy`](cli.md#mur-deploy) |
| `E-DEPLOY-003` | SSH connection or remote command failed | [`mur deploy`](cli.md#mur-deploy) |
| `E-DEPLOY-004` | Capsule did not emit usable startup JSON within 120s | [`mur deploy`](cli.md#mur-deploy) |
| `E-DEPLOY-006` | The pinned `mur` release could not be fetched from GitHub | [`mur deploy`](cli.md#mur-deploy) |
| `E-EVAL-001` | Eval file parse error (malformed JSON, unknown `record_type`, missing required field); message includes `:line:` number | [`eval.jsonl` schema](observability-schemas.md#structured-evaluation-evaljsonl) |
| `E-EVAL-002` | No eval file found for the session named on the command line | [`mur eval`](cli.md#mur-eval) |
| `E-IO-001` | File or directory not found | — |
| `E-IO-002` | Permission denied on a host path (the host's own permissions, not a capsule capability) | — |
| `E-IO-003` | General I/O error (read/write failure) | — |
| `E-MAN-001` | Missing required manifest field | — |
| `E-MAN-002` | YAML syntax error in manifest | — |
| `E-MAN-003` | Field type mismatch in manifest, or a structurally valid value the runtime rejects (artifact entry, inference config, capability config) | — |
| `E-NEW-001` | The generator agent produced no `out/murmur.yaml` | [`mur new`](cli.md#mur-new) |
| `E-REG-001` | Artifact not found in registry | [`mur install`](cli.md#mur-install) |
| `E-REG-002` | Installed artifact bytes do not match the sha256 recorded for them | [Lockfile](workdir.md#lockfile-murmurlock) |
| `E-REG-003` | An artifact of that name and version is already published | [`mur publish`](cli.md#mur-publish) |
| `E-REG-004` | Reserved version string (`latest`, `stable`, `edge`) | [`mur publish`](cli.md#mur-publish) |
| `E-REG-005` | A registry-resolved artifact's version or hash disagrees with the `murmur.lock` entry | [Lockfile](workdir.md#lockfile-murmurlock) |
| `E-RUN-001` | Capsule crashed, compile failure, missing component export, execution deadline exceeded (`capabilities.limits.deadline_seconds`), or resource limit exceeded (`capabilities.limits.memory_bytes`/`table_elements`) | [Execution limits](resource-limits.md#execution-limits) |
| `E-RUN-002` | Missing WASI import (linker error) | — |
| `E-RUN-003` | Lock version mismatch or missing lock entry | [Lockfile](workdir.md#lockfile-murmurlock) |
| `E-RUN-004` | Capsule WASM not found at expected path | — |
| `E-RUN-005` | Inference driver not configured in manifest | [Inference configuration](manifest.md#inference-config) |
| `E-RUN-006` | Inference driver artifact not installed, or `inference.command` is not on `PATH` | [Inference configuration](manifest.md#inference-config) |
| `E-RUN-007` | Agent loop failed at runtime | — |
| `E-RUN-008` | Required artifact not installed locally | [`mur run`](cli.md#mur-run) |
| `E-RUN-009` | `inference.system_prompt_file` (or the compaction system-prompt file) could not be read | [`inference.system_prompt`](manifest.md#inference-system-prompt) |
| `E-RUN-010` | `network.internal_port` is already bound | — |
| `E-RUN-011` | A native subprocess was killed for exceeding a `capabilities.resources` limit | [Which limit a subprocess hit](resource-limits.md#which-limit) |
| `E-RUN-012` | The capsule can spawn native subprocesses but no cgroup v2 scope could be delegated to bound them (Linux only) | [Platform behavior](resource-limits.md#platform-behavior) |
| `E-RUN-013` | Session workdir grew past `capabilities.resources.workdir_max_bytes` | [Host resource limits](resource-limits.md#host-resource-limits) |
| `E-RUN-014` | A `sealed` session cleared the host probe at launch but its composed root could not be built for a subprocess | [Containment class](containment.md#field-containment) |
| `E-RUN-015` | `mur run --resume` and `--context` were both given | [E-RUN-015](#e-run-015) |
| `E-RUN-016` | The session `--resume` named records no `task_start` carrying a context id | [E-RUN-016](#e-run-016) |
| `E-RUN-017` | The context `--resume` resolved to kept no conversation record | [E-RUN-017](#e-run-017) |
| `E-RUN-018` | `--resume-mode compact` with no hook bound to `on-compaction` | [E-RUN-018](#e-run-018) |
| `E-RUN-019` | A session that can delegate could not register with `mur-roost` | [E-RUN-019](#e-run-019) |
| `E-RUN-020` | `MURMUR_SPAWNER` is set to something that is not a spawner handle | [E-RUN-020](#e-run-020) |
| `E-RUN-021` | A staged native tool's binary is built for another operating system or CPU architecture | [E-RUN-021](#e-run-021) |
| `E-TOP-001` | Tempo endpoint unreachable, or invalid `--window` format | [`mur topology`](cli.md#mur-topology) |
| `E-TOP-002` | Tempo HTTP query failed (search or trace fetch) | [`mur topology`](cli.md#mur-topology) |
| `E-TOP-003` | Tempo response JSON parse failure | [`mur topology`](cli.md#mur-topology) |
| `E-TRC-001` | Trace file parse error (malformed JSON, missing required `session_start`/`session_end`, empty file); unknown event types are silently skipped. Also a `mur trace show --body` selector that names no recorded hash, or a hash whose body was never stored | [`trace.jsonl` schema](observability-schemas.md#session-trace-tracejsonl), [`mur trace show --body`](cli.md#mur-trace-show-body) |
| `E-TRC-002` | No session found in the workdir, or a session selector matched none or several | [`mur trace`](cli.md#mur-trace) |
| `W-BLD-001` | A declaration names an archive entry the packer already fills | [W-BLD-001](#w-bld-001) |
| `W-BLD-002` | `capsule.wasm` shadows another root `*.wasm` | [W-BLD-002](#w-bld-002) |
| `W-BLD-003` | A compiled artifact packages build inputs | [W-BLD-003](#w-bld-003) |
| `W-SEC-001` | No kernel-level subprocess sandbox on this platform | [W-SEC-001](#w-sec-001) |
| `W-SEC-002` | Linux host without Landlock — filesystem scope and exec unenforced | [W-SEC-002](#w-sec-002) |
| `W-SEC-003` | `network.allow` doesn't constrain bash's own outbound connections | [W-SEC-003](#w-sec-003) |
| `W-SEC-004` | Literal secret value found in a manifest field | [W-SEC-004](#w-sec-004) |
| `W-SEC-005` | Linux kernel enforcement is in force — what it covers, and the one key that re-widens it | [W-SEC-005](#w-sec-005) |
| `W-SEC-006` | A hook's `capabilities:` block declares a sub-key that is inert on hooks | [W-SEC-006](#w-sec-006) |
| `W-SEC-007` | A tool/driver narrowed to a host the capsule-wide ceiling does not allow — the entry was dropped | [W-SEC-007](#w-sec-007) |
| `W-SEC-008` | A tool/driver `capabilities:` block declares something per-artifact narrowing does not apply | [W-SEC-008](#w-sec-008) |
| `W-SEC-009` | `capabilities.shell.interpreter_runtime` couples the capsule to a specific host interpreter-version layout | [W-SEC-009](#w-sec-009) |
| `W-SEC-010` | No cgroup on this platform — the subprocess tree has no aggregate memory/pids/cpu bound | [W-SEC-010](#w-sec-010) |
| `W-SEC-011` | An executable workdir makes `capabilities.shell.allow` advisory | [W-SEC-011](#w-sec-011) |
| `W-SEC-012` | A compiler driver's helper binaries have no `Execute` grant under `sealed` | [W-SEC-012](#w-sec-012) |
| `W-SEC-013` | Unprivileged user namespaces are unrestricted host-wide, not granted to `mur` by the shipped AppArmor profile | [W-SEC-013](#w-sec-013) |
| `W-SEC-014` | A capsule-wide `capabilities.state` block grants nothing — a durable store is granted per artifact | [W-SEC-014](#w-sec-014) |
| `W-SEC-015` | A `config:` block on a native tool delivers nothing — config reaches WASM tools, drivers and hooks | [W-SEC-015](#w-sec-015) |
| `W-SEC-016` | A capsule-wide `capabilities.conversation` block grants nothing — the grant is per hook | [W-SEC-016](#w-sec-016) |
| `W-SEC-017` | `capabilities.filesystem.read_only` is advisory for an allowlisted interpreter | [W-SEC-017](#w-sec-017) |
| `W-SEC-018` | An installed tool's `input_schema` says nothing about its destinations, so its calls are judged by key name | [W-SEC-018](#w-sec-018) |

---

## Capability errors

### E-CAP-001 — invalid `network.allow` entry { #e-cap-001 }

An entry in a `capabilities.network.allow` list could not be parsed, so the capsule is refused
before any registry pull, artifact compile or workdir creation:

```text
error[E-CAP-001]: invalid network allow entry '<entry>': <reason>
```

The accepted forms are a full URL, a bare host, and a host with a port. A path, query or fragment
on the entry, and any scheme other than `http`/`https`, are rejected — see
[Network allow entries](manifest.md#network-allow-entries).

### E-CAP-002 — invalid `filesystem.scope` { #e-cap-002 }

A `filesystem.scope` value is not a usable subdirectory of the session workdir:

```text
error[E-CAP-002]: invalid filesystem scope '<scope>': scope must be relative to the workdir
error[E-CAP-002]: invalid filesystem scope '<scope>': scope cannot escape the workdir via '..'
```

The same two rules apply wherever a scope is declared: the capsule-wide
`capabilities.filesystem.scope`, a per-hook grant, and a per-tool or per-driver narrowing. Like
`E-CAP-001`, the check runs before any registry pull, artifact compile or workdir creation.
`scope: "."` is the explicit "whole workdir" grant. See
[Filesystem scope](manifest.md#filesystem-scope).

### E-CAP-003 — declared floor not achievable on this host { #e-cap-003 }

If the achieved class is weaker than the effective declared floor, `mur run` refuses to launch
before any registry pull, artifact compile, or workdir creation. The message names both classes
and the reason; the hint is always the same pair of remedies — lower the declared floor
(`capabilities.containment` in `murmur.yaml`, `containment` in `.murmur/config.yaml`, or
`--containment`), or run on a host that provides the declared class.

```text
error[E-CAP-003]: declared containment class 'scoped' is not achievable on this host (achieved: 'advisory'): scoped requires Landlock filesystem mediation (Linux 5.13+ with a usable Landlock ABI); this host provides no kernel filesystem mediation, so paths outside the workdir are constrained by convention only
  hint: lower the declared floor to 'advisory' (capabilities.containment in murmur.yaml, containment in .murmur/config.yaml, or --containment), or run on a host that provides 'scoped'
```

For `sealed` the reason names the *specific* missing piece and the command that fixes it, because
"no mount namespace here" has several completely different causes. The runtime never falls back to
a weaker class, whichever one it reports:

| Reason reported | What to do |
|---|---|
| This platform has no mount namespace and never will | Nothing — macOS and every other non-Linux host stay at `advisory` permanently |
| AppArmor's unprivileged-userns restriction is active while the `mur-sealed` profile is not confining this binary | Install and load the profile shipped with `mur`: `sudo install -m 644 packaging/apparmor/mur-sealed /etc/apparmor.d/mur-sealed && sudo apparmor_parser -r /etc/apparmor.d/mur-sealed`, or re-run the `mur` installer as root. Running a checkout build out of `./target`, which no shipped profile attaches to: `sudo scripts/install-dev-apparmor.sh`. Last resort, only where a profile genuinely cannot be loaded: `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0`, which removes unprivileged-userns hardening from every program on the machine and reports [`W-SEC-013`](#w-sec-013) |
| `unshare(CLONE_NEWUSER \| CLONE_NEWNS)` was refused — the usual answer inside a container, where `CAP_SYS_ADMIN` is absent or the container's own seccomp filter blocks `unshare(2)` | Add `--cap-add SYS_ADMIN` to the container invocation, or establish the mount namespace outside the container and run `mur` inside it |
| The namespace was created but its identity `uid_map`/`gid_map` could not be written, so the process cannot own it | Check that `/proc/sys/user/max_user_namespaces` is non-zero, that no LSM policy blocks `uid_map` writes for this binary, and that `mur` is not already running inside an unmapped user namespace |
| The namespace was created but `mount(2)` inside it was refused — a confinement that permits `userns_create` and then withholds `CAP_SYS_ADMIN` | Load the shipped profile with `sudo apparmor_parser -r /etc/apparmor.d/mur-sealed`; inside a container, add `--cap-add SYS_ADMIN`, or establish the mount namespace outside it |
| The kernel provides no unprivileged user namespaces at all (`CONFIG_USER_NS=n`, or `user.max_user_namespaces=0`) | `sudo sysctl -w user.max_user_namespaces=10000` if the sysctl is merely zeroed, otherwise run on a kernel built with `CONFIG_USER_NS=y` |
| No usable Landlock ABI, which `sealed` keeps inside the composed root as defence in depth | Run on Linux 5.13+ — a host that cannot back `scoped` cannot back `sealed` either |

**One case names the manifest rather than the host**, because no host can satisfy it:
[`capabilities.filesystem.workdir_exec: true`](containment.md#field-workdir-exec) declared
alongside `capabilities.containment: scoped` (or `sealed`). An executable workdir keeps the
Landlock `Execute` right on the session workdir, so a binary the capsule compiles, downloads or
renames inside it runs regardless of `capabilities.shell.allow` — the allowlist stops being an
enforceable property of the capsule, and the achieved class is capped at `advisory` on every host.
Remove `workdir_exec`, or lower the declared floor to `advisory`.

A `sealed` host that clears the probe at launch and *then* fails to build the composed root for a
particular subprocess is a different event and gets its own code, `E-RUN-014`. Two causes reach it,
and the message names the failing step and its errno:

- A [`capabilities.shell.staged_runtime`](containment.md#field-staged-runtime) grant whose
  `source_path` does not exist on this host. Each grant is planned as a *required* bind ahead of
  `pivot_root`, so the `mount(2)` fails with `ENOENT` and the session aborts. Fix the path, or drop
  the grant.
- Something moved underneath the runtime mid-session — an AppArmor profile reloaded, a container
  policy changed. Re-probe with `mur run --explain-scope`.

Neither means the declared floor was wrong; `E-CAP-003` covers that.

### E-RUN-015 — `--resume` and `--context` together { #e-run-015 }

[`mur run --resume <session>`](cli.md#mur-run) resolves a session address to the context id that
session ran under, and then runs exactly as `--context <id>` would. The two name the same thing two
ways, so passing both is refused rather than resolved by precedence:

```text
error[E-RUN-015]: --resume and --context name the same thing two ways
  hint: --resume <session> resolves that session's context id for you; --context <id> names one directly. Pass whichever you have, not both — see docs/content/reference/cli.md
```

Refused before the session address is resolved and before anything is staged, so no session
directory appears.

### E-RUN-016 — the resumed session ran no task { #e-run-016 }

`--resume` reads the context id off the named session's `trace.jsonl`, from the first
[`task_start`](observability-schemas.md#session-trace-tracejsonl) line. A session that never
reached a task carries none, and there is no conversation to continue:

```text
error[E-RUN-016]: cannot resume session ses_0193f2…: its trace.jsonl records no task_start carrying a context id
  hint: only a session that actually ran a task has a conversation to continue. Run `mur trace show <session>` to see what it did, and resume one that reached a task — see docs/content/reference/cli.md
```

Run [`mur trace show <session>`](cli.md#mur-trace-show) to see what that session did.

### E-RUN-017 — the resumed context kept no record { #e-run-017 }

The context id `--resume` resolved has no
[conversation record](workdir.md#the-conversation-record) on disk. Resuming it would start a fresh
conversation while reporting success, so it is refused instead, naming the session, the context and
which of the reasons applies:

```text
error[E-RUN-017]: cannot resume session ses_0193f2…: context 'ctx_0193f2…' has no conversation record (the capsule declares inference.transport: process, whose CLI owns its own conversation, and kept no conversation record)
  hint: a session is resumable only if its capsule kept a conversation record: an http-transport capsule that did not declare context.record: off. Run `mur trace show <session>` to see what that session did, and omit --resume to start a fresh conversation — see docs/content/reference/cli.md
```

The reasons: [`context.record: off`](manifest.md#field-context),
[`inference.transport: process`](manifest.md#inference-config) — whose CLI owns its own
conversation — a capsule with no `inference:` block, a host whose home directory cannot be
resolved, and a record path that resolves but holds no file. Refused at staging, before this
launch's session directory is created.

### E-RUN-018 — `--resume-mode compact` with no compaction hook { #e-run-018 }

`--resume-mode compact` runs the capsule's `on-compaction` hook over the loaded record and
continues from its summary. With nothing bound to that event there is nothing to produce the
summary, and quietly serving `full` instead would give the operator a mode they did not ask for:

```text
error[E-RUN-018]: --resume-mode compact needs a hook bound to on-compaction; this capsule declares none
  hint: declare a hook artifact whose binding is on-compaction (or all) with commit_policy: replace-context, or use --resume-mode full, which loads the record verbatim and needs no hook — see docs/content/reference/cli.md
```

`--resume-mode full` is often the cheaper mode anyway: a verbatim reload can hit the provider's
prompt cache, while compaction changes the prefix from the first altered token, guarantees a cache
miss, and costs an extra inference call to produce the summary.

### E-RUN-019 — the session could not register with mur-roost { #e-run-019 }

A capsule whose manifest declares `capabilities.spawn.allow` is refereed by `mur-roost` every time
it asks to spawn something, and the daemon can only referee a session whose grants it holds. The
session announces itself at launch (see
[`POST /register`](roost-api.md#post-register)); a registration it cannot complete refuses the
launch rather than leaving a capsule that can delegate running outside the knowledge of the daemon
that bounds it:

```text
error[E-RUN-019]: failed to register this session with mur-roost at http://127.0.0.1:7700: failed to connect to 127.0.0.1:7700: Connection refused (os error 111); a capsule declaring capabilities.spawn.allow must be known to the daemon that referees its spawns, so the launch is refused rather than run unrefereed
  hint: start the daemon the capsule registers with — `mur-roost --port 7700 --registry-path <store>` — and set MURMUR_ROOST_URL to its base URL, or drop capabilities.spawn.allow from the manifest if this capsule spawns nothing
```

`MURMUR_ROOST_URL` unset or blank reports the same code with `MURMUR_ROOST_URL is not set` as the
reason. The message names the daemon and the reason, never a token.

A capsule that declares no spawn capability never reaches this — it opens no connection to the
daemon at all, and `mur run` succeeds with nothing listening.

### E-RUN-020 — the spawner handle could not be read { #e-run-020 }

A capsule launched as a delegated child is handed `MURMUR_SPAWNER`, which names where its outcome
is reported and under which delegation id (see
[The completion path](roost-api.md#the-completion-path)). A child that cannot read it can tell
nobody that it finished, so the launch is refused before a session is staged:

```text
error[E-RUN-020]: MURMUR_SPAWNER does not carry a readable spawner handle: it is not JSON: expected value at line 1 column 1; a delegated child must be able to tell its spawner that it finished, so the launch is refused rather than run unreportable
  hint: MURMUR_SPAWNER is injected by a parent capsule's runtime at launch; unset it to run this capsule directly
```

The reason names what about the value could not be read — that it is not JSON, that a field is
missing, or that `trust` is not a trust class — and never the value itself, which names another
capsule's address and session.

An unset or blank `MURMUR_SPAWNER` is not this error: it is the ordinary case of a capsule nobody
delegated, which reports to nobody and runs exactly as it would have.

### E-RUN-021 — the native binary is built for another platform { #e-run-021 }

A native tool artifact carries a host executable at `bin/<name>`. Staging compares the platform
that binary was built for against this host's, before any of the session's native binaries is
written to the workdir:

```text
error[E-RUN-021]: native tool 'murmur-tool-git' cannot run on this host: its binary is built for darwin-aarch64, this host is linux-x86_64
  hint: the installed artifact holds a binary for another platform — reinstall it on this host with `mur install <name>@<version>`, or publish a build for this host's platform
```

The check reads the binary's header and recognises these formats:

| Format | Identified from | Platforms |
|---|---|---|
| ELF64 | `e_machine` | `linux-x86_64`, `linux-aarch64` |
| Mach-O 64-bit | `cputype` | `darwin-x86_64`, `darwin-aarch64` |
| Fat Mach-O | `0xCAFEBABE` magic | any `darwin-*` host |

Architecture is compared as strictly as operating system: an `x86_64` ELF on an `aarch64` Linux
host is refused on the same terms as a Mach-O on Linux.

Refusal requires a positive identification of both sides. A payload in a format the check does not
recognise — a shell script, a WASM module, a 32-bit image — stages and runs, and so does any
payload on a host outside the four platform targets.

When a capsule declares several native tools, one unrunnable binary refuses all of them: a refused
session leaves no tool binaries in its workdir.

[`mur doctor`](cli.md#mur-doctor) reads the same header of the same installed bytes and fails that
artifact's line, so the mismatch is reportable without launching a session.

### E-CAP-004 — staged runtime below the `sealed` floor { #e-cap-004 }

A `staged_runtime` grant is staged into a composed root, and a composed root is built only for a
capsule that asked for `sealed` (see
[A weaker declaration is never silently upgraded](containment.md#field-containment)). A capsule
declaring `staged_runtime` below an effective `sealed` floor is therefore refused before any
registry pull, artifact compile or workdir creation:

```text
error[E-CAP-004]: capabilities.shell.staged_runtime is declared for python3 but the effective containment floor is 'scoped' — staging a runtime tree requires the 'sealed' floor, because there is no composed root to bind-mount it into below that
  hint: set `capabilities.containment: sealed` in murmur.yaml (or pass `--containment sealed`) so the capsule gets a composed root to stage the runtime into, or remove the capabilities.shell.staged_runtime grant.
```

The check reads the declared floor alone, so it fires identically on a host that could deliver
`sealed`. That is what separates it from `E-CAP-003`, whose remedy points the opposite way:

| | `E-CAP-003` | `E-CAP-004` |
|---|---|---|
| What went wrong | the host is too weak for what the capsule declared | the capsule declared too little for what it asked for |
| Remedy | lower the floor, or move hosts | raise the floor, or drop the grant |

`mur doctor` surfaces the same condition as a warning ahead of a run.

### E-CAP-005 — no network namespace for the subprocess tree { #e-cap-005 }

`capabilities.network.allow` is enforced for a native subprocess by putting its whole tree in a
network namespace of its own, whose only way out is an egress proxy in the runtime process. A
Linux host that cannot create that namespace refuses the launch:

```text
error[E-CAP-005]: this host cannot give the capsule's subprocess tree its own network namespace, so capabilities.network.allow cannot be enforced for it: <reason>
```

Two host conditions produce it, and the refusal text names which one:

| Reason reported | What to do |
|---|---|
| The kernel provides unprivileged user namespaces but this host withholds them — AppArmor's `restrict_unprivileged_userns` is on and the shipped profile is not confining `mur`, `unshare` was refused outright (the container case), or the namespace could not be owned or configured | Install and load the AppArmor profile shipped with `mur`, or run outside the container restriction. The refusal text names the exact command for the host it printed on |
| The kernel does not provide the mechanism at all — `CONFIG_USER_NS=n`, or `user.max_user_namespaces=0` | Enable user namespaces on the host |

This applies to **every** Linux capsule that can spawn a subprocess, including one whose
`capabilities.network.allow` is empty: with the namespace missing, an empty allowlist would mean
unrestricted egress rather than none. There is no path that continues at reduced enforcement.

It is not part of the containment ladder — every class needs the namespace equally — so neither
raising nor lowering a declared floor changes this answer. `mur doctor` reports the same condition
ahead of a run.

### E-CAP-006 — interpreted entrypoint unreachable under `sealed` { #e-cap-006 }

Staging a `shell.allow` binary's own ELF dependency closure into the composed root is what makes
that binary runnable, and for an interpreted entrypoint it stages nothing useful. A console script
such as `pip` at `~/.local/bin/pip` is a `#!` script, not an ELF image, so its dependency closure
is *empty* — and the package it needs (`~/.local/lib/python3.12/site-packages/pip`) is a different
directory nothing derives. The capsule would launch cleanly and fail deep into a run, so under a
declared `sealed` floor this is decided at staging, before any registry pull, artifact compile or
workdir creation.

`mur run` refuses unless one of the following holds: the script already resolves under a fixed
sealed runtime path (`/usr`, `/bin`, `/sbin`, `/lib`, `/lib32`, `/lib64`, `/libx32`), or it lives
inside a directory a `staged_runtime`/`interpreter_runtime` grant already names, or some such grant
names the script or its shebang interpreter:

```text
error[E-CAP-006]: capabilities.shell.allow grants 'pip' (/home/dev/.local/bin/pip, a script run by 'python3') under the 'sealed' containment floor, but nothing declared makes the interpreted entrypoint's own package tree reachable inside the composed root — ...
  hint: declare `capabilities.shell.interpreter_runtime` (or `staged_runtime`) for the interpreter named above, listing the directories its import machinery actually reads — measure them on this host with `strace -f -e trace=openat,getdents64 <the command>` rather than guessing ...
```

The name match is deliberately loose, and the guarantee is correspondingly narrow: declaring
`interpreter_runtime` for `python3` satisfies every `python3` script, whatever directories the
grant names. murmur does not derive an interpreted program's import closure — `sys.path`, `.pth`
files and whatever the script does at runtime make that undecidable in general — so this check
verifies you declared *something*, never that the directory you declared is the right one.
Measuring it is still yours to do, which is why the hint names the `strace` invocation.

The check reads only the *declared* floor, never a host probe, and is inert at `scoped` and
`advisory`: below `sealed` there is no composed root, and the host filesystem is simply the host
filesystem. `mur doctor` surfaces the same condition ahead of a run, as a warning.

Two neighbouring codes are easy to confuse with this one:

| Code | Fires when | Say |
|---|---|---|
| [`E-CAP-004`](#e-cap-004) | a grant exists at too low a floor | raise the floor |
| `E-CAP-006` | the floor is already `sealed` and no grant exists | add a grant |
| [`W-SEC-012`](#w-sec-012) | an allowlisted compiler driver's helpers have no `Execute` grant | name the helper's directory in a grant |

### E-CAP-007 — export root outside the workdir { #e-cap-007 }

[`exports.files.root`](manifest.md#field-exports) and
[`exports.peer_files.root`](manifest.md#field-exports-peer-files) each name a subtree of the
[accessible workdir](workdir.md). When one of those paths already exists and resolves somewhere
else — because it is a symlink pointing out of the workdir — `mur run` refuses at staging, before
the workdir is created and before any session runs:

```text
error[E-CAP-007]: exports.files.root 'out/' resolves to '/srv/elsewhere', which is outside the capsule workdir '/home/dev/project'
  hint: point the export root at a directory inside the capsule workdir. A root that already exists as a symlink out of the workdir is refused whole rather than followed — see docs/content/reference/resource-plane.md
```

The root is refused whole rather than followed one file at a time: a per-request check would let
the first file leave before anyone noticed. A root that does not exist yet is accepted — the agent
may create it during a task.

This is not a containment shortfall, and no change to `capabilities.containment` affects it. An
export is a disclosure the operator makes, and the path named simply is not inside the capsule. See
[Resource plane](resource-plane.md) and
[Containment and disclosure](containment.md#containment-and-disclosure).

### E-CAP-008 — a persistent capsule needs a short handle lifetime { #e-cap-008 }

A [peer-file handle](resource-plane.md#peer-plane) is bounded by the minting capsule's own
lifetime: the key it is verified with is generated in memory at launch and destroyed at teardown,
so an ephemeral capsule (`lifecycle.after_task: exit`, the default) needs no declared ceiling at
all.

`lifecycle.after_task: sleep` withdraws that bound deliberately. The capsule stays alive, its
instance key stays alive, and a handle sitting in persisted A2A message history stays redeemable —
so the declared lifetime becomes the only one there is. It must therefore be declared, and it must
be at most `15m`:

```text
error[E-CAP-008]: exports.peer_files with lifecycle.after_task: sleep requires exports.peer_files.max_ttl to be declared and at most 900s (declared 1800s); a handle's lifetime is not a durability mechanism
  hint: declare `exports.peer_files.max_ttl: 15m` or shorter, or drop `lifecycle.after_task: sleep` so teardown bounds every handle instead. A consumer that needs these bytes after the capsule is gone should have the operator relaunch the runtime against the still-present workdir and request again — see docs/content/reference/resource-plane.md
```

`mur run` refuses at staging, before the workdir is created and before any session runs, so no
`trace.jsonl` appears.

A handle's lifetime is not a durability mechanism, and the remedy is never a longer one. Workdirs
persist past teardown: a consumer that needs the bytes after the capsule is gone should have the
operator relaunch the runtime against the same workdir and request again — see
[The minting key](resource-plane.md#minting-key).

### E-CAP-009 — `capabilities.state.store` does not name a usable store { #e-cap-009 }

A [durable state store](workdir.md#state-store) is one directory under `~/.murmur/state/`, so its
name is one path segment: non-empty, no `/`, not `.` or `..`, not beginning with `.`, and not
absolute. Anything else names either a directory tree or somewhere outside the state root, and
neither is a store:

```text
error[E-CAP-009]: invalid state store name '../escape': a store name is a single path segment and must not contain '/'
  hint: capabilities.state.store names one directory under ~/.murmur/state/, so it must be a single path segment: no '/', no '.' or '..', not absolute, and not starting with a dot. Omit `store:` to use the capsule name — see docs/content/reference/workdir.md
```

The same code covers a well-formed name whose directory this host cannot supply — an unresolvable
home directory, or a `~/.murmur/state/` that cannot be created as a `0700` directory:

```text
error[E-CAP-009]: state store 'shey' is unavailable at /home/dev/.murmur/state: failed to create the directory: File exists (os error 17)
  hint: a capsule declaring capabilities.state needs a resolvable home directory: run with HOME set to an absolute path, and make sure ~/.murmur/state/ can be created as a 0700 directory — see docs/content/reference/workdir.md
```

Both are decided at staging, before any registry pull, workdir creation or component
instantiation, so nothing is created under `~/.murmur/state/` and no `trace.jsonl` appears.
`mur run --explain-scope` refuses the same declarations with the same code, so a manifest that
would not launch does not pass the diagnostic either.

Neither is a containment shortfall, and no floor change fixes one: the remedy is the store name or
the host's home directory, never `capabilities.containment`.

### E-CAP-010 — an artifact's `config:` block cannot be delivered { #e-cap-010 }

[`config:`](manifest.md#artifact-config) on an artifact entry travels to that artifact as one
environment variable holding JSON, so the block has to be a string-keyed mapping that serializes to
JSON within 65536 bytes. Each rule refuses by name, quoting what the entry declared:

```text
error[E-CAP-010]: invalid config for artifact 'murmur-tool-corpus': 'config:' must be a mapping of keys to values, but this entry declares a sequence
  hint: config: on an artifact entry must be a mapping with string keys that serializes to at most 65536 bytes of JSON; it is delivered to that artifact alone as MURMUR_ARTIFACT_CONFIG. Omit the key entirely to deliver no variable, and keep secrets out of it — see docs/content/reference/manifest.md
```

An oversized block is refused rather than truncated, and the message names the size it serialized
to alongside the limit:

```text
error[E-CAP-010]: invalid config for artifact 'murmur-tool-corpus': 'config:' serializes to 70011 bytes of JSON, over the 65536-byte limit for MURMUR_ARTIFACT_CONFIG
```

`config:` written with no value under it is an empty block, refused on the same terms. Omit the key
to deliver no variable at all.

The refusal is decided at staging, before any registry pull, workdir creation or component
instantiation, so no session workdir appears and no `trace.jsonl` is written.
`mur run --explain-scope` refuses the same blocks with the same code.

The runtime checks the shape and not the meaning: which keys a given artifact requires is that
artifact's own business, and a missing one surfaces as that artifact's error rather than this one.

### E-CAP-011 — a conversation record path segment is not usable { #e-cap-011 }

The [conversation record](workdir.md#the-conversation-record) lives at
`~/.murmur/conversations/<record>/<context-id>/`, and both segments come from the operator:
[`context.record_store`](manifest.md#field-context) and
[`mur run --context`](cli.md#mur-run). Each must be a single path segment, and a value that is not
refuses the launch, quoting what was written:

```text
error[E-CAP-011]: invalid context.record_store 'a/b': must be a single path segment: no '/', no '.' or '..', not absolute, and not starting with a dot
  hint: context.record_store names one directory under ~/.murmur/conversations/, and --context names one directory beneath that, so each must be a single path segment. Omit context.record_store to use the capsule name, and omit --context to get a fresh id per task — see docs/content/reference/manifest.md
```

The refusal is decided at staging, before any registry pull, workdir creation or component
instantiation, so nothing is created under `~/.murmur/conversations/`.

Distinct from [`E-CAP-009`](#e-cap-009), which is the same shape rule applied to
`capabilities.state.store` and points at a different key and a different directory.

A `contextId` an A2A client sends is not an operator value and never refuses a launch: a task whose
context id is not a usable segment simply goes unrecorded, reported once to stderr and to
`logs/bootstrap.log`.

### E-CAP-012 — invalid `filesystem.read_only` entry { #e-cap-012 }

A `capabilities.filesystem.read_only` entry is not a usable subdirectory of the session workdir:

```text
error[E-CAP-012]: invalid read-only path '/etc': read-only path must be relative to the workdir
error[E-CAP-012]: invalid read-only path '../outside': read-only path cannot escape the workdir via '..'
```

The same two rules [`E-CAP-002`](#e-cap-002) applies to `filesystem.scope`, applied to each entry
of the read-only list. The check runs at staging, before any registry pull, workdir creation or
component instantiation, so no session directory is created and no call is ever checked against a
rule the runtime could not build. An empty or whitespace-only entry is dropped at manifest parse
rather than refused. See [Read-only paths](manifest.md#read-only-paths).

### E-CAP-013 — an artifact claims a runtime-provided tool name { #e-cap-013 }

`share-file`, `fetch-peer-file` and `delegate-task` are answered by the runtime itself, so an
artifact cannot be declared under any of them:

```text
error[E-CAP-013]: artifact 'delegate-task' collides with a tool the runtime provides itself; the reserved names are share-file, fetch-peer-file, delegate-task
  hint: the runtime answers these names itself, so an artifact under one of them would be shadowed at dispatch whatever the tool allowlist said. Rename the artifact, or drop the dependency if the runtime-provided tool is what you wanted — see docs/content/reference/runtime-provided-tools.md
```

The check runs at staging, ahead of every artifact in the manifest, so no artifact is resolved,
pulled or hash-verified and no session directory is created. `mur run --explain-scope` refuses the
same manifest by the same code. An in-session `manage.pull()` of one of these names is refused the
same way, and the running capsule receives that refusal as an error string.

Shell binary names are not reserved: they come from `capabilities.shell.allow`. See
[Runtime-provided tools](runtime-provided-tools.md#reserved-names).

---

## Conversation record errors

[`mur conversation`](cli.md#mur-conversation) acts on the
[record store](workdir.md#the-conversation-record) directly. All three refusals happen before
anything is read or written.

### E-CNV-001 — no such record or context { #e-cnv-001 }

`rm` and `truncate` name one context id, optionally narrowed with `--record`. Nothing matching it
is refused naming what was looked for and where:

```text
error[E-CNV-001]: no context 'ctx_nowhere' in any record under ~/.murmur/conversations/
  hint: `mur conversation ls` lists every record and context on this host
```

### E-CNV-002 — a context id is ambiguous across record stores { #e-cnv-002 }

A context id is unique inside one record store and nowhere else: two capsules can be handed the
same one, and two capsules can be pointed at one
[`context.record_store`](manifest.md#field-context). An id present under more than one store is
refused naming every store it is in, rather than guessing which conversation to delete or rewrite:

```text
error[E-CNV-002]: context 'ctx_fixed' is present under 2 record stores: store-a, store-b
  hint: pass --record <NAME> to say which one, e.g. --record store-a
```

### E-CNV-003 — invalid `--keep` { #e-cnv-003 }

`mur conversation truncate --keep 0` is refused, because truncating a record to nothing is
[`mur conversation rm`](cli.md#mur-conversation-rm):

```text
error[E-CNV-003]: --keep must be at least 1
  hint: truncating a record to nothing is `mur conversation rm ctx_fixed`
```

---

## Build lints

`mur build` checks the file set it is about to pack before it writes a byte of zip. What it
finds is reported two ways:

- **`E-BLD-NNN` errors** stop the build. No `.mur.zip` is written — not a partial one, not an
  empty one.
- **`W-BLD-NNN` warnings** go to stderr and the build completes. They flag a packaging mistake
  that still produces a working artifact.

Both classes are about *packaging*: what ends up inside the archive, and whether the runtime
will be able to launch it. They are distinct from the [`W-SEC-NNN`](#security-warnings)
warnings, which are about capability and enforcement posture.

### E-BLD-001 — invalid artifact name { #e-bld-001 }

```text
error[E-BLD-001]: invalid artifact name '<name>': <reason>
```

An artifact `name:` becomes a filename (`<name>-<version>.mur.zip`), a registry key and a
directory in the local store, so it is held to the lowest common denominator of all three:

- non-empty, at most 100 characters
- ASCII lowercase letters, digits and `-` only
- no leading or trailing `-`

Uppercase letters, spaces, `/`, `\` and `_` are rejected by the character rule. This is a
*format* check only — it reserves no prefix and no namespace. `murmur-hook-compact` and
`murmur-tool-git` are ordinary valid names; who may publish a given name is a registry
question, not a packaging one.

```yaml
name: my-tool        # ok
name: My Tool        # E-BLD-001
name: my_tool        # E-BLD-001
name: -my-tool       # E-BLD-001
```

### E-BLD-002 — unsafe or colliding `requires_files:` entry { #e-bld-002 }

```text
error[E-BLD-002]: requires_files entry '<entry>' has an unsafe path: <reason>
error[E-BLD-002]: requires_files entry '<entry>' is a symlink (<path>); declare the file it points to instead
error[E-BLD-002]: requires_files entries '<a>' and '<b>' both pack as the archive entry '<name>'
```

Every `requires_files:` entry must be a plain relative path to a real file inside the source
directory, and the entries must not collide once they are inside the archive:

- **No absolute paths.** `/etc/hosts` joined onto the source directory *is* `/etc/hosts`.
- **No `..` components.** This is the same rule a `.mur.zip` reader applies to an entry name it
  unpacks, applied at authoring time.
- **No symlinks.** The link is not followed: packing it would ship whatever it resolves to —
  plausibly from outside the source tree — under the declared name.
- **No two declarations claiming one archive entry.** The archive name is a rewrite of the
  declared path (`\` → `/`), so two distinct files can otherwise land in the same slot, where
  one silently overwrites the other on unpack.

A `requires_files:` entry that redundantly names `murmur.yaml` is *not* a collision — it is
dropped before packing, and reported as [`W-BLD-001`](#w-bld-001) instead.

### E-BLD-003 — packed entry set is not a launchable payload { #e-bld-003 }

```text
error[E-BLD-003]: missing root .wasm file (expected capsule.wasm or one root *.wasm)
error[E-BLD-003]: multiple root .wasm files found: <names>
```

The artifact's `runtime:`/`execution:` resolve to a **wasm** artifact, but the entry set about
to be packed does not contain exactly one payload the runtime could select. `mur build` never
compiles anything, so a `.wasm` must already exist on disk *and* be declared in
`requires_files:`.

The rule and the message are the runtime's own, so a build that passes this check cannot fail
payload selection at `mur run` time:

- exactly one root `*.wasm` entry → selected
- a root `capsule.wasm` → always selected, however many other root `*.wasm` entries exist
  (which is why that case is a warning, not an error — see [`W-BLD-002`](#w-bld-002))
- zero root `*.wasm` entries → `missing root .wasm file`
- two or more, none named `capsule.wasm` → `multiple root .wasm files found`

A `*.wasm` in a subdirectory is not a root entry and does not count. This check applies only to
wasm artifacts — native (`implementation: native`) and static (`runtime: skill`,
`execution: static`) artifacts are packed without it.

### W-BLD-001 — declaration names a reserved archive entry { #w-bld-001 }

```text
warning[W-BLD-001]: requires_files entry '<entry>' names the reserved archive entry '<name>', which mur build already packs
```

Two root entries have a fixed meaning inside a `.mur.zip`: `murmur.yaml` is seeded by the
packer itself, and `capsule.wasm` is the payload the runtime prefers over every other root
`*.wasm`. Declaring `murmur.yaml` in `requires_files:` packs nothing extra — the entry is
deduplicated away and the artifact is byte-identical without it.

Remove the declaration.

### W-BLD-002 — `capsule.wasm` shadows another root payload { #w-bld-002 }

```text
warning[W-BLD-002]: root 'capsule.wasm' is always selected as the payload, so <names> ships but never runs
```

The artifact carries a root `capsule.wasm` *and* at least one other root `*.wasm`. The build
succeeds — `capsule.wasm` makes payload selection unambiguous — but the other file is shipped
and never executed.

Keep exactly one root `*.wasm`: drop `capsule.wasm`, or rename the payload you meant to run.

### W-BLD-003 — compiled artifact packages build inputs { #w-bld-003 }

```text
warning[W-BLD-003]: compiled artifact packages build inputs: <names>
```

A wasm or native artifact declares an obvious build input in `requires_files:` — `Cargo.toml`,
`Cargo.lock`, a `*.rs` file, or anything under a `target/` path. A compiled artifact ships its
payload, not the sources it was built from, so this is almost always a stray declaration.

Static artifacts (`runtime: skill`) are exempt: their files *are* their content.

---

## Security warnings

`mur run`, `mur build` and `mur doctor` print non-fatal warnings about capability and enforcement
gaps in a manifest or host. Each one carries a `W-SEC-NNN` code and a link back to its section on
this page:

```text
[capsule-runtime] warning[W-SEC-001]: capabilities.shell.allow is non-empty but this platform
has no kernel-level subprocess sandbox (Landlock/seccomp are Linux-only) — enforcement is
environment-only (synthetic HOME + credential env-stripping). This is permanent on this
platform. (https://docs.murmur.nexus/murmur-nexus/murmur/reference/diagnostics/#w-sec-001)
```

Where a warning is written depends on whether a session workdir exists yet:

| Warning | Written to |
|---|---|
| `W-SEC-001`, `W-SEC-002`, `W-SEC-003`, `W-SEC-005`, `W-SEC-010` — decided at launch | stderr and `workdir/<session_id>/logs/bootstrap.log` |
| `W-SEC-006` to `W-SEC-009`, `W-SEC-011` to `W-SEC-018` — decided at staging, before the workdir exists | stderr |
| `W-SEC-004` — from `mur build` | stderr |

### W-SEC-001 — No kernel sandbox on this platform { #w-sec-001 }

**Fires when:** `capabilities.shell.allow` is non-empty and the host resolves to the
Environment-only tier — see [Subprocess enforcement tiers](containment.md#subprocess-enforcement-tiers) for which
hosts those are and why it is permanent there.

**Why it matters:** shell subprocesses on this host get environment-level protection only — a
synthetic `HOME` and credential env-stripping. No kernel enforcement constrains what they can read,
execute, or reach on the network.

**What to do:** treat `capabilities.shell.allow` and `capabilities.network.allow` as advisory on
this platform, not a security boundary. For kernel enforcement, run the capsule on a host that
resolves to an enforcing tier (see
[Subprocess enforcement tiers](containment.md#subprocess-enforcement-tiers)). If the capsule ingests untrusted
external content, use the
[data/action phase-separation pattern](../concepts/access-control.md#threat-model) instead of relying on the
allowlists to contain a compromised subprocess.

---

### W-SEC-002 — Landlock unavailable, filesystem scope unenforced { #w-sec-002 }

**Fires when:** `capabilities.shell.allow` is non-empty and the host resolves to the Seccomp-only
tier — see [Subprocess enforcement tiers](containment.md#subprocess-enforcement-tiers).

**Why it matters:** filesystem reads and writes outside the capsule workdir are not kernel-enforced
at all on this tier. **Nor is exec:** `capabilities.shell.allow` is enforced by granting the
Landlock `Execute` right on exactly the allowlisted binaries, so without Landlock there is no exec
mediation here and a shell subprocess can run any binary its uid can reach.

The [fixed capsule device set](containment.md#capsule-device-set) is a Landlock rule list, so it
does not apply here either — and the direction of that gap is worth being explicit about. Without
Landlock there is nothing to grant *and* nothing to deny: a capsule here can open `/dev/null`,
`/dev/zero` and `/dev/urandom`, but equally `/dev/random`, `/dev/mem` and any raw block device its
uid can reach.

What survives on this tier is everything that needs no Landlock — the shell child's capability drop
and the `socket(2)` domain denial, both described under
[What every Linux tier grants](containment.md#subprocess-enforcement-tiers). A host stuck below
kernel 5.13 is therefore not exposed to the `/var/run/docker.sock` escape.

**What to do:** upgrade the host kernel to move to the Full tier. Until then, treat filesystem scope
and exec scope as advisory on this host — neither has a mechanism here. This is also why this tier
cannot reach the `scoped` containment class.

---

### W-SEC-003 — `bash` bypasses the network allowlist { #w-sec-003 }

**Fires when:** `capabilities.shell.allow` contains the literal entry `"bash"` and
`capabilities.network.allow` is non-empty, on a host that resolves to the Environment-only tier
(see [W-SEC-001](#w-sec-001)). On the enforcing tiers a `bash` subprocess's own outbound
connections *are* constrained by the same allowlist, so this warning does not fire there.

**Why it matters:** `capabilities.network.allow` constrains requests the runtime itself makes
(WASI HTTP calls from tool/driver components). It does not constrain a `bash` subprocess's own
outbound connections on this tier — `bash` can reach any host regardless of what
`network.allow` declares.

**Maximum-risk combination:** `bash` in `shell.allow` combined with any external-fetch
capability (`network.allow`, or a tool/driver artifact that fetches independently) gives a
capsule both exposure to untrusted content and unchecked shell authority to act on it — see the
[threat model](../concepts/access-control.md#threat-model) for the full picture alongside prompt
injection.

**What to do:** run on a host that resolves to an enforcing tier (see
[Subprocess enforcement tiers](containment.md#subprocess-enforcement-tiers)), or avoid pairing `bash` with a
non-empty `network.allow` on platforms without one. Declaring `sh` or `zsh` instead only silences
the warning: the check matches the literal string `bash`, and the exposure is the same.

---

### W-SEC-004 — Literal secret in manifest { #w-sec-004 }

**Fires when:** `mur build` scans `murmur.yaml` and finds a credential-shaped field — one whose
key contains `api_key`, `token`, `secret` or `password` — holding a string that is not a
`${VAR_NAME}` reference and is either longer than 8 characters or begins with a known API-key
prefix (`sk-`, `sk-ant-`).

**Why it matters:** a literal secret in `murmur.yaml` ships inside the built artifact and is easy
to accidentally commit to version control.

**What to do:** replace the literal value with a `${VAR_NAME}` reference and inject the real
value via environment at run time. The build still succeeds — this is a warning, not a blocker —
but the artifact should not be published or committed until the literal is removed.

---

### W-SEC-005 — Linux kernel enforcement is in force { #w-sec-005 }

**Fires when:** `capabilities.shell.allow` is non-empty and the host resolves to the **Full** or
the **Sealed** tier (see
[Subprocess enforcement tiers](containment.md#subprocess-enforcement-tiers)). The strongest tiers
warn rather than stay silent, because each is a scope with documented exceptions and silence would
imply it has none.

**What the message says.** On the Full tier it names what the Landlock scope, the fixed device set,
the `socket(2)` domain deny and the default-deny syscall allowlist cover — see
[Subprocess enforcement tiers](containment.md#subprocess-enforcement-tiers) for the full grant. On
the Sealed tier it names what the composed root adds and states its one exception: `/proc` is a bind
of the host's rather than a masked private procfs, because mounting one unprivileged needs a PID
namespace this tier does not create, so host process metadata stays visible inside the root exactly
as it is under `scoped`.

**The one documented exception a manifest controls.**
[`capabilities.network.unix_sockets: true`](manifest.md#field-capabilities) re-widens the
`AF_UNIX` half of the domain deny. Nothing else on the list has an opt-out:

| `socket(2)` domain | Default | Can a manifest widen it? |
|---|---|---|
| `AF_UNIX` | denied (`EACCES`) | yes — `capabilities.network.unix_sockets: true` |
| `AF_NETLINK` | denied (`EACCES`) | **no** |
| `AF_PACKET` | denied (`EACCES`) | **no** |
| `AF_INET`, `AF_INET6`, everything else | unaffected | governed by `capabilities.network.allow` — TCP and UDP by IP destination |

That key is coarse on purpose: it is a whole address family, not a per-socket-path allowlist, so
declaring it `true` re-exposes every unix socket the process can reach — `/var/run/docker.sock`,
which is host root, included. Landlock cannot substitute for it at any ABI, because ABI v6's
`LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET` scopes *abstract* unix sockets only and `docker.sock` is a
pathname socket. Declare it only when a shell tool genuinely needs a local daemon socket. `AF_UNIX`
is also how glibc's NSS reaches `nscd` and how `syslog(3)` reaches `/dev/log`, so the default deny
is the mechanism on this list most likely to interfere with an existing workload.

**Side effect worth knowing about:** the shell child's capability drop also removes
`CAP_DAC_OVERRIDE` from a root-run capsule's subprocess, so it no longer bypasses ordinary
file-permission checks. That is intended, but it is a real behavior change for root deployments
whose shell steps relied on root's usual "can read anything" posture.

**What to do:** keep `shell.allow`, `network.allow` and `filesystem.scope` as narrow as the task
genuinely needs, and don't run `mur run` as root if you can avoid it — a non-root capsule never had
the `CAP_MKNOD` the workdir device-node restriction exists to backstop.

---

### W-SEC-006 — Inert sub-key in a hook's `capabilities:` block { #w-sec-006 }

**Fires when:** a `runtime: hook` artifact entry's `capabilities:` block declares `shell`,
`spawn`, `env`, `limits`, `resources` or `containment`. Per-hook grants (see
[`artifacts[].capabilities`](manifest.md#hook-capabilities)) only read `network`, `filesystem`,
`state` and `task_io` — the other sub-blocks are structurally accepted but nothing enforces them
per-hook.

**Why it matters:** an operator who declares, say, `capabilities.shell.allow` on a hook entry
expecting it to scope that hook's shell access would otherwise have no signal that the runtime
never reads it there — it is silently inert rather than rejected.

**What to do:** remove the inert sub-key from the hook's entry. Shell, spawn, env, limits,
resources and the containment floor are capsule-wide concerns — declare them in the top-level
`capabilities:` block instead. See [Hook capabilities](manifest.md#hook-capabilities) for
the full rules on what a per-hook grant does and does not cover.

---

### W-SEC-007 — Per-artifact network entry outside the capsule ceiling { #w-sec-007 }

**Fires when:** a `runtime: tool` or `runtime: driver` entry's `capabilities.network.allow` names
an entry the capsule-wide top-level `capabilities.network.allow` does not itself allow. Per-artifact
capabilities *narrow* (see [Tool and driver capabilities](manifest.md#tool-capabilities)):
the effective grant is `declaration ∩ ceiling`, so the uncovered entry is dropped from that
artifact's grant rather than granted. Staging continues.

A bare host (`api.example.com`) is broader than a scheme-bound ceiling entry
(`https://api.example.com`) because it spans both schemes and every port, so it is *not* covered
and will be dropped — this is the most common way to hit this warning by accident.

**Why it matters:** the artifact ends up with **less** access than the entry asked for. Nothing is
widened, so this is never an escalation — but a tool that silently cannot reach a host it was
"granted" fails in a way that is hard to trace back to the manifest.

**What to do:** either add the host to the capsule-wide `capabilities.network.allow` (if the whole
capsule should be able to reach it), or make the per-artifact entry at least as specific as the
ceiling entry it should sit under, or delete it if the drop was what you actually wanted.

---

### W-SEC-008 — Unapplied per-artifact grant on a tool or driver { #w-sec-008 }

**Fires when:** either

- a `runtime: tool`/`runtime: driver` entry's `capabilities:` block declares `shell`, `spawn`,
  `env`, `limits`, `resources` or `containment` — a per-artifact grant only reads `network`,
  `filesystem` and `state` ([`W-SEC-006`](#w-sec-006) is the hook-side twin of this case, where
  `task_io` is honored as well); or
- a `runtime: tool` entry with a **native** (non-WASM) implementation declares `capabilities:` at
  all. A native tool runs as a host subprocess under the capsule-wide shell/sandbox machinery, not
  through the WASI tool path narrowing is applied on, so the whole block is inert.

**Why it matters:** a declared-but-unenforced grant reads like a scoped artifact. In the native
case in particular, the tool keeps the full capsule ceiling despite an entry that looks like it
locked it down.

**What to do:** for an inert sub-key, remove it and declare it in the top-level `capabilities:`
block instead. For a native tool, scope it through `capabilities.shell.*` on the capsule-wide
block, or ship the tool as WASM if you need per-artifact narrowing.

---

### W-SEC-009 — Interpreter-runtime grant couples the capsule to a host layout { #w-sec-009 }

**Fires when:** the capsule's top-level `capabilities.shell.interpreter_runtime` declares one or
more grants. Fires once per grant, from both `mur run` (at staging) and `mur doctor`.

**Why it matters:** an `interpreter_runtime` grant widens an already-allowlisted binary's Landlock
scope to specific host directories *outside* the workdir so a path-based interpreter (e.g. CPython)
can reach its standard library. That makes the grant necessary to run such an interpreter, and it
**couples the capsule to a specific host distro/interpreter-version layout**: a grant naming
`/usr/lib/python3.11` stops resolving the moment the host ships Python 3.12, and a capsule that
runs on Debian may not run on Alpine. The durable fix is
[`capabilities.shell.staged_runtime`](containment.md#field-staged-runtime), which bind-mounts a
pinned runtime tree into the capsule's own composed root instead of reaching out to the host's; it
requires an effective `sealed` floor, while `interpreter_runtime` works at `scoped` too.

**What it grants, exactly:** one Landlock rule per named directory, and nothing else. Each directory
carries its own required `list_dir`:

| `list_dir` | Rights granted | Effect |
|---|---|---|
| `true` | `Execute + ReadFile + ReadDir` | The directory's own entries are enumerable — what CPython's `FileFinder` needs for a `sys.path` entry |
| `false` | `Execute + ReadFile` | Files inside can still be opened **by exact name**, but the directory itself cannot be listed |

There is no field that accepts a prefix and expands it, and `ReadDir` is never inferred — it is
granted only where an author wrote `list_dir: true`, and only on that one directory, never on its
parent or siblings. A directory not named in the manifest receives no rule at all.

**What to do:** name the narrowest set of directories that actually works — measure the real
requirement with `strace -f -e trace=openat,getdents64 <interpreter> -c "import ..."` rather than
guessing, and set `list_dir: false` on any directory you only open known files inside. Accept
that the capsule is now pinned to this host's interpreter layout, or switch to
`capabilities.shell.staged_runtime` if the capsule can run at an effective `sealed` floor — it
carries no host-layout coupling and fires no `W-SEC-009`.

**Parse-time rejections.** A malformed `interpreter_runtime` fails `mur run`/`mur doctor` with
[`E-MAN-003`](#index) at manifest parse time, naming the offending value:

- a `binary` not present in the same block's `shell.allow` — this mechanism narrows filesystem
  access alongside an exec grant that already exists, and never itself grants exec
- a `dirs[].path` that is not absolute (does not start with `/`)
- a `dirs[]` entry that omits `list_dir` — enumerability is never inferred
- an `interpreter_runtime[]` entry with an empty `dirs` list

---

### W-SEC-010 — No aggregate bound on the subprocess tree { #w-sec-010 }

**Fires when:** the capsule can spawn a native subprocess by any route
(`capabilities.shell.allow`, `capabilities.spawn.allow`, or a native-implementation artifact) and
the host is not Linux — so no cgroup v2 scope can exist for its process tree.

**Why it matters:** `capabilities.resources` bounds subprocesses with two independent mechanisms,
and only the weaker one survives on this platform. Per-process `setrlimit(2)` ceilings still
apply, and still apply as **hard** limits the capsule cannot raise. But every *aggregate* bound —
`cgroup_memory_bytes`, `cgroup_pids_max`, `cgroup_cpu_percent`, `cgroup_io_bytes_per_sec` — is a
cgroup v2 feature, and cgroups are Linux-only. Nothing caps the subprocess tree's total memory,
task count, or CPU.

**`RLIMIT_NPROC` is not a substitute, and this is the specific reason cgroups are required.** It
is a per-**uid** ceiling, not a per-tree one: a fork bomb of distinct, short-lived processes that
fork and exit faster than the count is observed slips past it in practice even when it is set
correctly. Only a cgroup's `pids.max` bounds the tree as a whole. Two further gaps on macOS
specifically: it has no `RLIMIT_AS`, and its `RLIMIT_DATA` is present in the headers but not
enforceable (the kernel rejects any finite value with `EINVAL`), so
`capabilities.resources.memory_bytes` has no effect there either — a subprocess's memory is
bounded neither per-process nor in aggregate.

**The failure mode is denial of service.** A capsule that exhausts host resources has not read,
written, or reached anything outside its granted scope.

**What to do:** run capsules that spawn subprocesses on a Linux host with systemd user cgroup
delegation configured, where the aggregate bounds are real — see
[Verification](containment.md#verification) for how those bounds are checked by hand. On this platform,
treat `capabilities.resources`' `cgroup_*` fields as declared-but-inert and do not rely on them.
This is **permanent** on this platform, exactly like [`W-SEC-001`](#w-sec-001): a kernel without
cgroups cannot grow them.

Note the asymmetry with Linux, which is deliberate: there, the same condition (can spawn a
subprocess, no cgroup scope available) is a **refused launch** with `E-RUN-012`, not a warning —
on Linux a missing scope is a host misconfiguration an operator can fix, while here it is a
property of the platform.

---

### W-SEC-011 — Executable workdir makes `shell.allow` advisory { #w-sec-011 }

**Fires when:** the manifest declares
[`capabilities.filesystem.workdir_exec: true`](containment.md#field-workdir-exec). Once, at
staging, on stderr — from both `mur run` and `mur doctor`.

**Why it matters:** `capabilities.shell.allow` is enforced by granting the Landlock `Execute` right
on exactly the allowlisted binaries' own paths and withholding it everywhere the capsule can write
— above all its own session workdir. `workdir_exec: true` gives that right back to the workdir. From
then on, a binary the agent compiles, downloads, unpacks or renames inside its workdir executes
regardless of what the allowlist says. There is no name check to defeat, because there is no name
check: the kernel is granting `Execute` on the path, and the path is inside the granted directory.

This is a *stated trade*, not a defect. Compile-and-run workloads — a capsule that runs
`gcc`/`cargo build` in its workdir and then executes the artefact — need it, and there is no
narrower form of the grant that distinguishes "a binary this capsule compiled" from "a binary this
capsule downloaded". What the runtime refuses to do is let the trade be silent.

**What the runtime does about it, beyond this warning:**

* the capsule's achieved containment class is **`advisory`**, on every host — including a
  Landlock-capable one that would otherwise report `scoped` or `sealed`;
* `mur run --explain-scope` prints `workdir exec: true` next to that `advisory`;
* `trace.jsonl`'s `session_start` event carries `workdir_exec: true`, so a completed session's
  record shows which guarantee was in force;
* declaring it alongside `capabilities.containment: scoped` (or `sealed`) refuses the launch with
  `E-CAP-003`, before any registry pull, artifact compile or workdir creation. The refusal text
  names the manifest key rather than a host mechanism, because no host can satisfy the pair.

**What to do:** remove `workdir_exec` if the capsule does not genuinely need to run something it
produced — that is the default, and it makes `capabilities.shell.allow` a boundary the kernel
enforces rather than a convention. If the capsule does need it, keep the declared containment floor
at `advisory` and treat `shell.allow` as documentation of intent, not as containment: use the
[data/action phase-separation pattern](../concepts/access-control.md#threat-model) to bound what the capsule
can be induced to build and run, and give it no network allowlist entry it does not need.

---

### W-SEC-012 — A compiler driver's toolchain has no `Execute` grant { #w-sec-012 }

**Fires when:** the capsule's declared containment floor is
[`sealed`](containment.md#field-containment) **and** `capabilities.shell.allow` names a known
compiler driver — `cc`, `gcc`, `g++`, `c++` — whose helper binaries have no grant carrying the
Landlock `Execute` right. Once per uncovered helper, at staging, on stderr, before any registry
pull or workdir creation. Also printed by `mur doctor`, which never launches anything.

**Why it matters:** `cc` compiles nothing. It forks and execs a front end (`cc1` for C, `cc1plus`
for C++), an assembler (`as`), a linker (`ld`) and a linker wrapper (`collect2`). Those are
separate binaries, and none of them appears in `cc`'s own `DT_NEEDED` closure — so the derivation
that stages an allowlisted binary's shared libraries into the composed root stages none of them.

They are still *present* inside the root, because they live under `/usr`, which
[`sealed`](containment.md#field-containment) bind-mounts read-only into every composed root.
What they do not have is permission to run: that fixed tree is granted `ReadFile + ReadDir` and
deliberately **not** `Execute`, because it is the one grant covering whole host trees the manifest
never named — it has to make them readable without making them runnable. The result is a capsule
where `cc --version` prints a version and the first real compile dies partway through, with an
exec failure on a path that is demonstrably right there.

**Example.** A capsule declaring `containment: sealed` and `shell.allow: [cc]` on a Debian-family
host gets, at staging:

```text
[capsule-runtime] warning[W-SEC-012]: capabilities.shell.allow grants the compiler driver 'cc',
but its helper 'cc1' at /usr/libexec/gcc/x86_64-linux-gnu/13/cc1 has no grant carrying the
Landlock Execute right under the 'sealed' composed root — ...
```

**What to do:** declare an
[`interpreter_runtime`](manifest.md#field-capabilities) (or
[`staged_runtime`](containment.md#field-staged-runtime)) grant for the driver, naming the
directories its helpers live in. Both grant shapes carry `Execute`, which is exactly what is
missing. Measure the directories on the host rather than guessing them:

```bash
cc -print-prog-name=cc1        # /usr/libexec/gcc/x86_64-linux-gnu/13/cc1
cc -print-prog-name=as         # `as` — deferred to PATH, i.e. /usr/bin/as
```

The warning itself names the helper's containing directory, so the common case is a copy-paste.

#### Why this warns where `E-CAP-006` refuses { #w-sec-012-vs-e-cap-006 }

Both fire only under a declared `sealed` floor, and both are about a `shell.allow` binary that
starts and then cannot finish. They differ in what is known:

| | [`E-CAP-006`](#e-cap-006) (refusal) | `W-SEC-012` (warning) |
|---|---|---|
| Trigger | an allowlisted `#!` script with no covering grant | an allowlisted compiler driver with an uncovered helper |
| Evidence | categorical — a script's ELF closure is *empty*, so staging it stages nothing it imports | heuristic — a `-print-prog-name=` probe over a fixed table of four driver names |
| Failure it predicts | a missing-module error inside the root, not a denial | an exec failure partway through a compile |
| Outcome | launch refused before any registry pull | launch proceeds, operator warned |

A capsule may compile successfully without every probed helper — a link-only workload never reaches
`cc1` — and a driver family outside the table is not probed at all, so refusing a launch on this
evidence would block capsules that would have worked.

---

### W-SEC-013 — Unprivileged user namespaces are unrestricted host-wide { #w-sec-013 }

**Fires when:** AppArmor is enabled on this host and
`kernel.apparmor_restrict_unprivileged_userns` is `0`. Once, at staging, on stderr — from both
`mur run` and `mur doctor`.

**Why it matters:** `capabilities.containment: sealed` and the capsule network namespace both need
an unprivileged user namespace, and on an AppArmor host something has to permit it. This warning is
about which of the two permitting postures the host is in — they differ enormously in blast radius:

| Grant | What it permits |
|---|---|
| `profile_confining` | `mur` alone creates unprivileged user namespaces; every other binary stays restricted |
| `restriction_disabled_host_wide` | **every** program on the machine creates them |

The other two values, and the line that prints them, are in
[Where the user namespace comes from](containment.md#userns-grant).

Ubuntu 23.10 and later ship the restriction on precisely because unprivileged user namespaces are a
recurring local privilege-escalation surface. Switching it off works, and on a host where no profile
can be loaded it is the right answer — but it is not the configuration murmur ships, and a `sealed`
result obtained that way is a different security statement from one obtained through the profile.

**What the runtime does about it, beyond this warning:** nothing is refused, and no exit code
changes — `mur doctor` on a project whose artifacts all check out still exits `0`. The grant is
*recorded* in the three places it can be read back:

* `mur doctor` prints an `AppArmor / user namespaces` block naming the grant;
* `mur run --explain-scope` prints `userns grant:` in its Containment block, and
  `--explain-scope --json` carries `userns_grant`;
* `trace.jsonl`'s `session_start` event carries `userns_grant` next to `containment_achieved`, so a
  finished session's record shows which of the two mechanisms was in force.

**What to do:** if the narrow grant is available to you, take it —

```sh
# 1. restore the hardening, removing any drop-in that disables it
sudo rm -f /etc/sysctl.d/*-userns.conf
sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=1

# 2. install and load the profile shipped with mur
sudo install -m 644 packaging/apparmor/mur-sealed /etc/apparmor.d/mur-sealed
sudo apparmor_parser -r /etc/apparmor.d/mur-sealed
```

Running a build out of a checkout, where the binary is `./target/debug/mur` or
`./target/release/mur` and no shipped profile attaches to it, use
`sudo scripts/install-dev-apparmor.sh` — it generates and loads the same grant for those two paths,
so a from-source developer never needs the host-wide sysctl. Then re-run `mur doctor` and expect
`userns grant: profile_confining`.

If the host genuinely cannot load a profile, keep the sysctl and keep this warning: it is a record
of the posture, not a defect to silence.

#### The installed-profile comparison { #w-sec-013-profile-drift }

`mur doctor` also compares `/etc/apparmor.d/mur-sealed` byte-for-byte against the profile the
running `mur` build ships, and prints both SHA-256 digests when they differ. That is a **file**
finding and nothing more: AppArmor loads from the kernel's policy cache, so a file can be edited
without `apparmor_parser -r` ever running, and a loaded profile can outlive the file it came from.
The `userns grant` line is the behavioural answer and stays the source of truth — the comparison
never changes a containment class and never changes an exit code.

Local customisation belongs in `/etc/apparmor.d/local/mur-sealed`, which both shipped profiles
`include if exists` and which is deliberately not hashed.

---

### W-SEC-014 — a capsule-wide `capabilities.state` block grants nothing { #w-sec-014 }

**Fires when:** the capsule's own top-level `capabilities:` block declares `state`. Once, at
staging, on stderr.

**Why it matters:** a [durable state store](workdir.md#state-store) is granted per *artifact* — it
is the `runtime: tool`, `runtime: driver` or `runtime: hook` entry that receives the second preopen.
The capsule's own guest is built with no artifact grant at all, so a top-level declaration reaches
nothing: no directory is created and no `state` path exists for anybody. Without this warning the
only signal is a store that never appears.

```text
[capsule-runtime] warning[W-SEC-014]: capsule-wide capabilities.state is declared, but a durable state store is granted per artifact — nothing reads a top-level declaration, so no store was created and no 'state' preopen exists. Move the block onto the tool, driver or hook entry that needs it (https://docs.murmur.nexus/murmur-nexus/murmur/reference/diagnostics/#w-sec-014)
```

**What the runtime does about it:** nothing is refused and no exit code changes. The block is
structurally valid, exactly as an inert `shell` block on a hook entry is
([`W-SEC-006`](#w-sec-006)), so it is reported rather than rejected.

**What to do:** move the block onto the artifact entry that needs the store —

```yaml
artifacts:
  - name: murmur-tool-corpus
    version: 0.1.0
    runtime: tool
    capabilities:
      state:
        store: shey
```

Two artifacts that need the same store each declare it, with the same `store:` name. There is no
capsule-wide form: sharing a store is written once per artifact, so reading any one entry tells you
what that artifact reaches.

---

### W-SEC-015 — a `config:` block on a native tool delivers nothing { #w-sec-015 }

**Fires when:** a `runtime: tool` entry declares [`config:`](manifest.md#artifact-config) and the
artifact ships a native (non-WASM) implementation. Once per artifact, at staging, on stderr.

**Why it matters:** config arrives in the environment the runtime builds for one WASM component.
A native tool runs as a host subprocess under the capsule-wide shell environment, which is shared
rather than per-artifact, so no `MURMUR_ARTIFACT_CONFIG` is delivered anywhere. Without this
warning the only signal is a tool that behaves as though it were never configured.

```text
[capsule-runtime] warning[W-SEC-015]: artifact 'murmur-tool-fixture' declares 'config:' but ships a native implementation — a native tool runs as a host subprocess and reads no per-artifact config, so no MURMUR_ARTIFACT_CONFIG is delivered (https://docs.murmur.nexus/murmur-nexus/murmur/reference/diagnostics/#w-sec-015)
```

**What the runtime does about it:** nothing is refused and no exit code changes, exactly as for a
`capabilities:` block on the same entry ([`W-SEC-008`](#w-sec-008)). `mur run --explain-scope`
still lists the artifact under `artifact config:`, because that reports what the manifest declares.

**What to do:** move the settings into whatever the native tool already reads — its command-line
arguments or a file it is pointed at — or drop the block. `config:` on a `runtime: tool` entry
backed by a WASM component, on a `runtime: driver` entry, or on a `runtime: hook` entry is
delivered normally.

---

### W-SEC-016 — a capsule-wide `capabilities.conversation` block grants nothing { #w-sec-016 }

**Fires when:** the capsule's own top-level `capabilities:` block declares `conversation`. Once, at
staging, on stderr.

**Why it matters:** the `murmur:conversation/read` grant is per *hook* — it is the `runtime: hook`
entry whose component imports the interface that receives it. The capsule's own guest holds no
artifact grant and compiles against a world with no such import, so a top-level declaration reaches
nothing and no artifact can read the [conversation record](workdir.md#the-conversation-record).

```text
[capsule-runtime] warning[W-SEC-016]: capsule-wide capabilities.conversation is declared, but the murmur:conversation/read grant is per artifact — nothing reads a top-level declaration, so no artifact can read the conversation record. Move the block onto the hook entry that needs it (https://docs.murmur.nexus/murmur-nexus/murmur/reference/diagnostics/#w-sec-016)
```

**What the runtime does about it:** nothing is refused and no exit code changes, on the same terms
as [`W-SEC-014`](#w-sec-014).

**What to do:** move the block onto the hook entry that reads the record —

```yaml
artifacts:
  - name: murmur-hook-recall
    version: 0.1.0
    runtime: hook
    capabilities:
      conversation:
        read: true
```

---

### W-SEC-017 — `read_only` is advisory for an allowlisted interpreter { #w-sec-017 }

**Fires when:** `capabilities.filesystem.read_only` is non-empty and `capabilities.shell.allow`
names an interpreter — a shell (`bash`, `sh`, `zsh`, `fish`, `dash`, `ksh`) or a general-purpose
one (`python`, `python3`, `perl`, `ruby`, `node`, `deno`, `bun`, `php`, `awk`, `gawk`, `mawk`,
`lua`, `tclsh`, `Rscript`). Once per such binary, at staging, on stderr.

**Why it matters:** the dispatch-time check reads a shell call's argv and its `-c` script body, and
flags what it can positively identify as a write — a redirection, or an argument in a write-target
position of a binary it knows. An interpreter's own file I/O is in neither:
`python3 -c "open(p,'w').write(x)"` is one opaque argument that names no redirection and no
recognized verb. The interpreter can construct a write into a declared read-only subtree, and it
will not be refused.

```text
[capsule-runtime] warning[W-SEC-017]: capabilities.filesystem.read_only is declared and capabilities.shell.allow includes 'python3', an interpreter that can construct a write the dispatch check cannot read — the declaration is advisory for that binary. It still holds for every tool call and for every shell command whose write the dispatch check can identify (https://docs.murmur.nexus/murmur-nexus/murmur/reference/diagnostics/#w-sec-017)
```

**What the runtime does about it:** nothing is refused and no exit code changes. The declaration
still applies in full to every tool call and to every shell command whose write the dispatch check can
identify, and every refusal is still recorded as
[`protected_path_denied`](observability-schemas.md#protected-path-denied).

**What to do:** treat the declaration as advisory for the named binary and decide whether that is
acceptable for this capsule. Removing the interpreter from `capabilities.shell.allow` closes the
gap; keeping it does not weaken any other part of the declaration. See
[Read-only paths](manifest.md#read-only-paths).

---

### W-SEC-018 — a tool's input schema does not say where it writes { #w-sec-018 }

**Fires when:** `capabilities.filesystem.read_only` is non-empty and an installed tool's
`input_schema` names a path-shaped property (`path`, `file_path`, `filepath`, `filename`, `file`)
or a destination-shaped one (`dest`, `dest_path`, `destination`, `destination_path`, `target_path`,
`output_path`, `out_path`, `new_path`, `to`) and carries no `murmur-destination` or `murmur-opaque`
annotation. Once per such tool, at staging, on stderr.

**Why it matters:** with nothing declared, the dispatch-time check guesses which of a tool's inputs
are filesystem destinations from those property names. The guess is wrong in both directions: a
note the tool merely stores, carrying a `{file, text}` pair, is refused as a write, and a
destination under a name no table carries is never checked.

```text
[capsule-runtime] warning[W-SEC-018]: capabilities.filesystem.read_only is declared and the tool 'guessed-tool' declares the property 'file_path' with no murmur format annotation — its calls are judged by key name. Annotate a destination property with "format": "murmur-destination", and any object the tool only stores with "format": "murmur-opaque" (https://docs.murmur.nexus/murmur-nexus/murmur/reference/diagnostics/#w-sec-018)
```

**What the runtime does about it:** nothing is refused and no exit code changes. The tool keeps
the key-name rules.

**What to do:** annotate the tool's `input_schema` — `"format": "murmur-destination"` on each
string property whose value is a file the tool writes, `"format": "murmur-opaque"` on each object
or array it only stores. The warning is the tool author's to answer, not the operator's: a capsule
cannot annotate a tool it installs. See [Read-only paths](manifest.md#read-only-paths).
