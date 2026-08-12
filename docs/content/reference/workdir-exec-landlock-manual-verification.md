# Verification — workdir `Execute` rights and declared `workdir_exec`

!!! warning "Status: **PARTIAL — 2026-08-06.** Steps 1–6 all observed live on one Landlock-capable Linux host. Steps 1–2 were observed through the runtime's own subprocess path, not through a capsule-driven agent session."

    The mechanism was exercised on a real Landlock host during the build of this slice. The two
    kernel-level steps (1 and 2) were driven through `shell::execute_shell` — a real `fork`, a real
    seccomp filter, a real Landlock domain, a real `bash` subprocess — via the scratch harness in
    [Scratch harness](#scratch-harness), because the host had no LLM credentials with which to
    drive a full agent session. Steps 3–6 were run against the real `mur` binary. What was observed
    is recorded verbatim in [Recording the result](#recording-the-result).

    A green `cargo build` / `cargo test` / `cargo clippy` is **not** evidence about steps 1–2 and
    must not be reported as if it were. See
    [What this deliberately is not](#what-this-deliberately-is-not).

## What this verifies

`capabilities.shell.allow` is enforced by the kernel, on the path the kernel itself resolved, with
no userspace round trip — and the enforcement is *complete*, meaning there is no route by which a
capsule can execute a binary the operator did not name.

The mechanism is one Landlock right. Each allowlisted binary gets a narrow read+execute
`PathBeneath` grant at its real host path (plus its ELF interpreter and its `DT_NEEDED` closure, so
it can actually link). The session workdir — the one place the capsule can write — gets every other
ABI v1 right *except* `Execute`, unless the manifest declares
[`capabilities.filesystem.workdir_exec: true`](containment.md#field-workdir-exec).

This replaced a seccomp-notify exec supervisor that intercepted `execve(2)`/`execveat(2)` and
compared a pathname read out of the stopped child's `/proc/<pid>/mem` against a canonical allowlist.
That mechanism is **deleted**, not demoted to a fallback: continue-based
argument inspection cannot be made sound, because the kernel dereferences the same pointer again
after the decision. See [`W-SEC-002`](diagnostics.md#w-sec-002) for what a host without
Landlock now gets instead (no exec mediation at all, and no path to the `scoped` class).

Two properties are under test, and they are opposites of each other:

> **(A)** With `workdir_exec` absent or `false`, a binary sitting in the session workdir does not
> execute — under **any** name, including a name that appears verbatim in
> `capabilities.shell.allow`. Not "the name is checked and rejected": the workdir carries no
> `Execute` right, so there is nothing to name.
>
> **(B)** With `workdir_exec: true`, that binary *does* execute — and every observable the operator
> can reach says the capsule's guarantee is weaker for it: the achieved containment class,
> `mur run --explain-scope`, `trace.jsonl`'s `session_start`, `W-SEC-011`, and an `E-CAP-003`
> refusal if the manifest also declares `capabilities.containment: scoped`.

Six things are checked by hand:

1. a disallowed binary in the workdir does not execute, under an allowlisted basename (property A);
2. the same binary executes once `workdir_exec: true` is declared (property B);
3. allowlisted binaries outside the workdir still run — the no-regression direction;
4. `mur run --explain-scope` reports `workdir exec` and the class it forced;
5. `trace.jsonl`'s `session_start` carries `workdir_exec`;
6. `workdir_exec: true` + `capabilities.containment: scoped` refuses with `E-CAP-003`.

## What this deliberately is not

There is **no automated test that asserts property A or property B**, and there will not be one.
This repo's CI has never resolved to `EnforcementTier::KernelFull` or `KernelSealed` — it runs the
`KernelSeccompOnly` path, where no Landlock domain exists at all. A test asserting "the renamed
binary cannot execute" would therefore run a code path that installs no Landlock ruleset, observe
whatever the bare kernel does, and pass. It would be evidence of nothing while looking like
evidence of everything. Steps 1–3 below are the evidence; a green suite is not.

The committed tests that *do* exist around this bit are content checks on the right-set — chiefly
`sandbox::linux_integration_tests::workdir_execute_right_is_granted_only_when_workdir_exec_is_declared`,
which asserts that `workdir_access_rights(false)` withholds `Execute` and
`workdir_access_rights(true)` grants it. That buys one thing: flipping the default back cannot
happen silently. It says nothing about whether a kernel honours the right set.

## Host prerequisites

* Linux with a usable Landlock ABI. Containers frequently mask it — a host that reports no
  Landlock can run steps 4–6 but **not** steps 1–3.
* A checkout of this repository and a working `cargo` (plus `libseccomp-dev`).
* `/bin/echo` present (used as the planted "disallowed" binary). Any small dynamically- or
  statically-linked executable works.

Confirm the host before anything else:

```sh
uname -r                       # want >= 5.13
cat /sys/kernel/security/lsm   # 'landlock' must appear in this list
```

Expected on a capable host — the exact list varies, `landlock` is the only entry that matters:

```
lockdown,capability,landlock,yama,apparmor,ima,evm
```

Then confirm which tier the runtime actually resolves to:

```sh
cd /path/to/murmur
cargo test -p capsule-runtime --lib \
  sandbox::linux_integration_tests::kernel_tier_allows_exec_within_shell_allowlist \
  -- --nocapture --exact
```

A `SKIP — PROVES NOTHING` line, or a `TIER: EnvironmentOnly`/`KernelSeccompOnly` report from the
harness below, means **steps 1–3 are meaningless on this machine**. Stop and find another host.

Build the CLI once, for steps 4–6:

```sh
cargo build -p murmur-cli
BIN="$PWD/target/debug/mur"
```

## Scratch harness

Steps 1–3 drive `shell::execute_shell` directly, which is crate-private, so they run from a scratch
test appended to `crates/capsule-runtime/src/sandbox.rs`. **Do not commit it.** Append this inside
`mod linux_integration_tests`, just before that module's closing brace:

```rust
    #[test]
    fn scratch_workdir_exec() {
        eprintln!("TIER: {:?}", detect_enforcement_tier());
        let workdir_exec = std::env::var("SCRATCH_WORKDIR_EXEC").unwrap_or_default() == "1";

        let workdir = tempfile::tempdir().unwrap();
        let policy = CapabilityPolicy {
            shell_allow: vec!["bash".to_string()],
            workdir_exec_allowed: workdir_exec,
            ..CapabilityPolicy::default()
        };
        let exec_allow_paths = resolve_exec_allowlist(&policy.shell_allow);
        let enforcement = ShellEnforcement {
            // Pinned, not probed: this is the tier the property belongs to.
            tier: EnforcementTier::KernelFull,
            network_allow_ips: Vec::new(),
            unix_sockets_allowed: false,
            workdir_exec,
            landlock_grants: LandlockGrant::non_listable_files(resolve_landlock_grants(
                &exec_allow_paths,
            )),
            ..host_bounding_base()
        };

        // Plant the imposter from the parent, before any sandbox exists: a real, working binary
        // sitting in the workdir under an ALLOWLISTED basename. This is the bypass under test.
        let imposter = workdir.path().join("bash");
        std::fs::copy("/bin/echo", &imposter).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&imposter, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

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

Note what the harness does and does not fake. `workdir_exec` is set from the environment and fed
into the *real* `ShellEnforcement`, so the Landlock rule built in the forked child is the production
one. Nothing about the exec decision is stubbed. The only thing standing in for production is the
agent loop — which never touches this bit.

When you are done with steps 1–3:

```sh
git checkout crates/capsule-runtime/src/sandbox.rs
```

## Step 1 — A workdir binary does not execute, even under an allowlisted name

This is property A, and the bypass a prior release shipped: the planted binary is named `bash`,
which *is* in `capabilities.shell.allow`.

```sh
SCRATCH_WORKDIR_EXEC=0 \
SCRATCH_SCRIPT='./bash IMPOSTER_RAN && echo RAN_OK || echo REFUSED' \
cargo test -p capsule-runtime --lib \
  -- --nocapture --exact sandbox::linux_integration_tests::scratch_workdir_exec
```

Expected — `Permission denied` from the kernel, and `REFUSED` on stdout:

```
TIER: KernelSealed
EXIT: 0
OUT: REFUSED

ERR: bash: line 1: ./bash: Permission denied
```

`bash: line 1: ./bash: Permission denied` is Landlock refusing the `execve`. It is **not**
`No such file or directory` (the file is there, mode `0755`) and **not** an errno from a userspace
supervisor — nothing in userspace was consulted.

`EXIT: 0` is correct: the exit code is the `|| echo REFUSED` branch's, not the imposter's.

Note: a plain `bash -c '...'` never opens `/etc/bash.bashrc` (verified with `strace -f -e trace=openat`
on this same host) — an earlier draft of this procedure claimed that line as an additional control
signal on stderr, but it is not something this harness reproducibly emits, so it has been dropped
from the expected output. The `./bash: Permission denied` line is the property under test and is
sufficient on its own.

A `RAN_OK` here is a **failure of this procedure** and a live bypass. Record it and stop.

## Step 2 — The same binary executes with `workdir_exec: true`

Property B. Identical command, one environment variable flipped:

```sh
SCRATCH_WORKDIR_EXEC=1 \
SCRATCH_SCRIPT='./bash IMPOSTER_RAN && echo RAN_OK || echo REFUSED' \
cargo test -p capsule-runtime --lib \
  -- --nocapture --exact sandbox::linux_integration_tests::scratch_workdir_exec
```

Expected — the planted `/bin/echo` runs and prints its argument, and the script's `&&` continues
into `echo RAN_OK`:

```
TIER: KernelSealed
EXIT: 0
OUT: IMPOSTER_RAN
RAN_OK

ERR:
```

`IMPOSTER_RAN` on stdout is the whole point: `capabilities.shell.allow` contained only `bash`, and a
copy of `/bin/echo` just ran. That is the documented, accepted cost of the declaration, and it is
what steps 4–6 make visible to an operator. `RAN_OK` on the next line simply confirms the `&&` branch
was taken, i.e. the imposter genuinely exited zero rather than merely not crashing.

A `REFUSED` here means compile-and-run workflows are broken; record it and stop.

## Step 3 — Allowlisted binaries outside the workdir still run

The no-regression direction. Nothing about steps 1–2 may cost an allowlisted binary its exec.

```sh
cargo test -p capsule-runtime --lib \
  sandbox::linux_integration_tests::kernel_tier_allows_nested_exec_of_second_allowlisted_binary \
  -- --nocapture --exact
```

Expected — `ls` (a genuinely nested, dynamically-linked `execve` from inside `bash`, both
allowlisted, both outside the workdir) succeeds:

```
test sandbox::linux_integration_tests::kernel_tier_allows_nested_exec_of_second_allowlisted_binary ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; ... filtered out
```

Unlike steps 1–2, this one *is* a committed test — an allowlisted binary failing to run is a
functional regression, which CI can legitimately catch even at a weaker tier.

## Step 4 — `--explain-scope` reports `workdir exec` and the class it forced

Steps 4–6 use the real `mur` binary and need no Landlock. Set up three fixtures:

```sh
mkdir -p /tmp/we-default /tmp/we-true /tmp/we-scoped

cat > /tmp/we-default/murmur.yaml <<'YAML'
name: workdir-exec-check
version: 0.0.1
capabilities:
  shell:
    allow:
      - bash
YAML

cat > /tmp/we-true/murmur.yaml <<'YAML'
name: workdir-exec-check
version: 0.0.1
capabilities:
  filesystem:
    workdir_exec: true
  shell:
    allow:
      - bash
YAML

cat > /tmp/we-scoped/murmur.yaml <<'YAML'
name: workdir-exec-check
version: 0.0.1
capabilities:
  containment: scoped
  filesystem:
    workdir_exec: true
  shell:
    allow:
      - bash
YAML

for d in we-default we-true we-scoped; do printf '\0asm\1\0\0\0' > "/tmp/$d/capsule.wasm"; done
```

The stub `capsule.wasm` is deliberately invalid: every check in steps 4–6 happens *before* the
component compiles, so a manifest that never reaches the compiler is the right shape for this.

First the default:

```sh
"$BIN" run --manifest /tmp/we-default/murmur.yaml --explain-scope
```

Expected — `workdir exec: false`, and the host's real class (`sealed` on a sealed-capable host,
`scoped` on a Landlock-only one, `advisory` elsewhere):

```
Containment
  declared:  advisory
  achieved:  sealed
  floor met: yes
  mechanism: mountns+pivot_root+landlock+seccomp

Effective grants
  filesystem scope: <none>
  workdir exec:     false
  network allow: <none>
  unix sockets:     false
  shell allow:
    - bash
  spawn allow: <none>
  env allow: <none>
  interpreter runtime: <none>
  staged runtime: <none>
```

Then the declaring one:

```sh
"$BIN" run --manifest /tmp/we-true/murmur.yaml --explain-scope
```

Expected — `workdir exec: true`, `achieved: advisory`, and `mechanism:` **unchanged**:

```
Containment
  declared:  advisory
  achieved:  advisory
  floor met: yes
  mechanism: mountns+pivot_root+landlock+seccomp

Effective grants
  filesystem scope: <none>
  workdir exec:     true
  network allow: <none>
  unix sockets:     false
  shell allow:
    - bash
  spawn allow: <none>
  env allow: <none>
  interpreter runtime: <none>
  staged runtime: <none>
```

The pairing is the check. `mechanism:` still names the full tier, because the *host* did not get
weaker — the capsule did. An `achieved: advisory` next to `mechanism: none` would be a different
finding (a host with no Landlock), which is why both lines are reproduced above.

`--json` emits the same facts on one line; `workdir_exec` is always present, including when false:

```sh
"$BIN" run --manifest /tmp/we-true/murmur.yaml --explain-scope --json | python3 -m json.tool
```

Expected (excerpt):

```json
{
    "declared_containment": "advisory",
    "achieved_containment": "advisory",
    "floor_met": true,
    "enforcement_tier": "mountns+pivot_root+landlock+seccomp",
    "workdir_exec": true
}
```

## Step 5 — `W-SEC-011` fires, and `trace.jsonl` records `workdir_exec`

Run the declaring capsule for real (not `--explain-scope`), from its own directory:

```sh
cd /tmp/we-true && "$BIN" run --manifest /tmp/we-true/murmur.yaml
```

Expected — `W-SEC-011` on stderr, once, before anything else:

```
[capsule-runtime] warning[W-SEC-011]: capabilities.filesystem.workdir_exec is true — the session workdir keeps its Landlock Execute right, so anything the capsule writes there can run regardless of capabilities.shell.allow; this capsule reports containment class 'advisory' on every host, including a Landlock-capable one (https://docs.murmur.nexus/murmur-nexus/murmur/reference/diagnostics/#w-sec-011)
status:  failed
error[E-RUN-001]: failed to compile capsule component: failed to parse WebAssembly module
```

The `E-RUN-001` is expected and is itself informative: the warning fired at staging, *before* the
component compile, which is where the design says it fires. `mur doctor` prints the same warning
from the same manifest without launching anything:

```sh
cd /tmp/we-true && "$BIN" doctor 2>&1 | grep W-SEC-011
```

For the `trace.jsonl` half you need a capsule that actually starts a session — a real
`capsule.wasm` and a reachable inference endpoint. With one, run it and read the first event:

```sh
"$BIN" run --manifest <a real capsule's murmur.yaml> --task 'exit'
python3 -c 'import json,sys;print(json.dumps(json.loads(open(sys.argv[1]).readline()),indent=2))' \
  <workdir>/<session_id>/trace.jsonl
```

Expected — `workdir_exec` present on `session_start`, next to the class it forced:

```json
{
  "event_type": "session_start",
  "containment_declared": "advisory",
  "containment_achieved": "advisory",
  "workdir_exec": true
}
```

The same capsule with `workdir_exec` removed must show `"workdir_exec": false` and the host's real
class. Record `PENDING` for this half if no runnable capsule was available.

## Step 6 — `workdir_exec: true` + `containment: scoped` refuses with `E-CAP-003`

```sh
"$BIN" run --manifest /tmp/we-scoped/murmur.yaml
```

Expected — refused, on a host that is fully capable of `scoped`, with a reason naming the manifest
rather than the host:

```
status:  failed
error[E-CAP-003]: declared containment class 'scoped' is not achievable on this host (achieved: 'advisory'): capabilities.filesystem.workdir_exec: true keeps the Landlock Execute right on the session workdir, so a binary the capsule compiles, downloads or renames inside it runs regardless of capabilities.shell.allow — the allowlist stops being an enforceable property of this capsule. No host can back a class above advisory for it. Either remove workdir_exec (the allowlist is then enforced by the kernel on the resolved path) or lower the declared containment floor to advisory
  hint: lower the declared floor to 'advisory' (capabilities.containment in murmur.yaml, containment in .murmur/config.yaml, or --containment), or run on a host that provides 'scoped'
```

Two things to check beyond the exit status:

* the reason must **not** say "this host provides no kernel filesystem mediation" — that is the
  host-shortfall text, and printing it here would send an operator to a different machine to fix a
  line in their own manifest;
* no `W-SEC-011` line appears. The refusal precedes the warning, deliberately: a manifest that is
  about to be rejected outright is not first advised about the thing being rejected.

Finally, the regression check that matters most — a manifest that declares nothing must be
completely unaffected:

```sh
"$BIN" run --manifest /tmp/we-default/murmur.yaml
```

Expected — reaches the component compile, i.e. was never gated and never warned:

```
status:  failed
error[E-RUN-001]: failed to compile capsule component: failed to parse WebAssembly module
```

Any `E-CAP-003` or `W-SEC-011` here is a regression against every existing manifest in the repo.

## Recording the result

Fill this table in on real hardware. `PENDING` is the correct entry for anything not run — do not
infer a result from a passing build.

| # | Check | Result | Evidence |
|---|-------|--------|----------|
| 1 | Workdir binary refused under an allowlisted name (`workdir_exec` absent) | **PASS (build stage, 2026-08-06)** | See below |
| 2 | Same binary runs with `workdir_exec: true` | **PASS (build stage, 2026-08-06)** | See below |
| 3 | Allowlisted binaries outside the workdir still run | **PASS (build stage, 2026-08-06)** | See below |
| 4 | `--explain-scope` reports `workdir exec` and the forced class | **PASS (build stage, 2026-08-06)** | See below |
| 5 | `W-SEC-011` fires at staging / `trace.jsonl` carries `workdir_exec` | **PARTIAL (build stage, 2026-08-06)** — warning observed; `trace.jsonl` half **PENDING** | See below |
| 6 | `E-CAP-003` on `workdir_exec` + `scoped`; default manifest unaffected | **PASS (build stage, 2026-08-06)** | See below |

### Run of 2026-08-06 — build-host observations

**Host.** `Linux 7.0.0-28-generic`, x86_64, Ubuntu, non-root (`uid=1000`).
`/sys/kernel/security/lsm` = `lockdown,capability,landlock,yama,apparmor,ima,evm`. The runtime
resolved to `EnforcementTier::KernelSealed`; the harness pinned `KernelFull` for steps 1–2, which is
the tier the property belongs to.

**Steps 1–2.** Run through the scratch harness above, verbatim, with a copy of `/bin/echo` planted
in the session workdir as `./bash` (mode `0755`) while `capabilities.shell.allow` contained only
`bash`. Observed:

* `SCRATCH_WORKDIR_EXEC=0` → `OUT: REFUSED`, `ERR: bash: line 1: ./bash: Permission denied`.
* `SCRATCH_WORKDIR_EXEC=1` → `OUT: IMPOSTER_RAN` followed by `RAN_OK`, `ERR:` empty.

Both outputs are reproduced verbatim in steps 1 and 2 above, and were re-confirmed independently
during review on the same host (2026-08-06): the `/etc/bash.bashrc` line quoted in an earlier draft
does not actually occur — `strace -f -e trace=openat bash -c 'true'` shows a plain `bash -c` never
opens that file — so it was dropped as a claimed control signal; the `./bash: Permission denied`
line is the property under test and stands on its own.

**Step 3.** `kernel_tier_allows_nested_exec_of_second_allowlisted_binary` passes on this host, as do
`kernel_tier_allows_exec_within_shell_allowlist`,
`kernel_full_denies_filesystem_access_outside_workdir`, and the two `interpreter_runtime` Landlock
tests — 18 of 19 `sandbox::linux_integration_tests` green (the 19th was the scratch test itself,
which fails without `SCRATCH_SCRIPT` set).

**Steps 4 and 6.** Run against `target/debug/mur` with the three `/tmp/we-*` fixtures above. All
output blocks in those steps are the real, unedited observed output.

**Step 5.** The `W-SEC-011` line is the real observed output. The `trace.jsonl` half is **PENDING**:
it needs a capsule with a valid component and a reachable inference endpoint, neither of which the
build host had. The field is covered by
`trace::tests::session_start_records_workdir_exec_next_to_the_class_it_forced`, which is a
serialization test and not a substitute for reading a real session's trace.

### What is still owed

* Step 5's `trace.jsonl` half, on a host with a runnable capsule.
* Steps 1–2 driven through a **real agent session** rather than the scratch harness — an agent
  writing a binary into its own workdir and being refused, which is the shape an operator actually
  meets.
* Step 1 on a `KernelFull`-only host (no user namespaces), to confirm the property does not depend
  on the composed root that `KernelSealed` additionally installs.

## Related

* [`W-SEC-011`](diagnostics.md#w-sec-011) — the warning this mechanism fires.
* [`capabilities.filesystem.workdir_exec`](containment.md#field-workdir-exec) — the field.
* [Verification](containment.md#verification) — the index of hand-run procedures.
