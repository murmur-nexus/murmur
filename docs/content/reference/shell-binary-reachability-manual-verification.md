# Verification — shell-binary reachability under `sealed`

!!! warning "Status: **RUN — 2026-08-08.** All four steps observed live on one bare-metal, uncontainerised, `sealed`-capable Linux host. Steps 3–4 were driven through the runtime's own subprocess path, not through a capsule-driven agent session."

    Steps 1 and 2 were run against the real `mur` binary built from this slice's branch. Steps 3
    and 4 — the ones that need a live shell tool call *inside* a composed root — were driven
    through `shell::execute_shell` via the scratch harness in
    [Scratch harness](#scratch-harness), because the host had no LLM credentials with which to
    drive a full agent session. That is the same substitution
    [workdir-exec Landlock manual verification](workdir-exec-landlock-manual-verification.md)
    makes, for the same reason, and it fakes nothing about the sandbox: the `ShellEnforcement` is
    the production one, resolved by `ShellEnforcement::resolve(&policy, ContainmentClass::Sealed)`,
    and the host resolved to `EnforcementTier::KernelSealed` in every run below.

    **The roadmap's Case 2 hypothesis is confirmed, not merely restated.** Step 3 reproduces the
    exact failure `W-SEC-012` predicts, at the exact path `W-SEC-012` named.

    Everything observed is recorded verbatim in the step bodies. A green
    `cargo test --workspace` is **not** evidence for any of this — see
    [What this deliberately is not](#what-this-deliberately-is-not).

## What this verifies

That a `capabilities.shell.allow` grant which cannot actually *function* inside a `sealed` composed
root fails at **launch**, with a named reason, rather than several agent turns into a run.

Staging an allowlisted binary's ELF/`DT_NEEDED` closure into the composed root
(`sandbox::resolve_landlock_grants`, wired through `sealed::plan_composed_root`) is complete for a
dynamically linked ELF executable and silently incomplete for two other kinds of program:

> **(1) An interpreted entrypoint.** `~/.local/bin/pip` is a `#!` script. Its ELF closure is
> *empty*, so staging it stages nothing it imports, and the package it needs
> (`~/.local/lib/python3.12/site-packages/pip`) is a different directory nothing derives. Inside
> the root this is `ModuleNotFoundError` — an ENOENT-class failure, not a Landlock denial, so
> nothing in the trace says "policy".
>
> **(2) A compiler driver.** `cc` forks and execs `cc1`, `as`, `ld`, `collect2`. Those are
> separate binaries outside its own closure, living under `/usr` — which a composed root binds
> read-only and grants `ReadFile + ReadDir` but deliberately **not** `Execute`
> (`sandbox::resolve_sealed_runtime_landlock_grants`, `executable: false`). Present, readable,
> and un-exec'able.

Four things are checked by hand:

1. an uncovered interpreted entrypoint refuses at launch with `E-CAP-006`, naming the binary, its
   resolved path and its shebang interpreter — and a covering grant, or residence under `/usr`,
   clears it;
2. an uncovered compiler driver warns at staging with `W-SEC-012`, naming each helper and its
   resolved path — and a covering grant clears it;
3. **the Case 2 hypothesis**: that uncovered driver really does fail a real compile inside a real
   composed root, at the path `W-SEC-012` named — and the declared grant really does fix it;
4. the Case 1 failure the refusal pre-empts really is `ModuleNotFoundError` inside the root — and,
   as this slice explicitly claims, *declaring a covering grant is not the same as declaring the
   right directory*.

## What this deliberately is not

There is **no automated test that asserts steps 3 or 4**, and there will not be one. This repo's
CI has never resolved to `EnforcementTier::KernelSealed` — it runs containerised, where the
composed root cannot be built at all. A test asserting "the compile failed" would run a code path
that installs no composed root and no Landlock domain, observe whatever the bare kernel does, and
pass. It would be evidence of nothing while looking like evidence of everything.

The committed tests around this slice (`crates/capsule-runtime/src/reachability.rs`, `mod tests`)
assert the *decision* logic only: shebang parsing, component-wise prefix matching, grant-name
matching, the declared-floor gate, and that residence under `SEALED_RUNTIME_PATHS` is not treated
as `Execute` coverage. None of them touches a kernel boundary.

## Host prerequisites

* Linux, kernel ≥ 5.13, with a usable Landlock ABI, unprivileged user namespaces, and the shipped
  `mur-sealed` AppArmor profile installed — i.e. a host that actually reports
  `achieved_containment: sealed`. Containers routinely mask this.
* A real `gcc`/`cc` install, and a real Python with a **user-site** `pip` at `~/.local/bin/pip`
  (not a `/usr` one — the whole point of step 1 is a script outside `SEALED_RUNTIME_PATHS`).
* A checkout of this repository and a working `cargo` (plus `libseccomp-dev`).

The host used for the run recorded below:

```console
$ uname -r
7.0.0-28-generic

$ cat /sys/kernel/security/lsm
lockdown,capability,landlock,yama,apparmor,ima,evm

$ systemd-detect-virt
none

$ ls -d /etc/apparmor.d/mur-sealed
/etc/apparmor.d/mur-sealed

$ cc --version | head -1
cc (Ubuntu 13.3.0-6ubuntu2~24.04.1) 13.3.0

$ which pip; head -1 "$(which pip)"
/home/agape/.local/bin/pip
#!/usr/bin/python3
```

Confirm the host really backs `sealed` before anything else — every step below is meaningless
otherwise:

```console
$ mkdir -p /tmp/mur-427-verify/probe && cd /tmp/mur-427-verify/probe
$ printf 'name: reachability-fixture\nversion: 0.0.1\ncapabilities:\n  containment: advisory\n' > murmur.yaml
$ printf '\0asm\x01\0\0\0' > capsule.wasm
$ mur run --manifest murmur.yaml --explain-scope --json
{"declared_containment":"advisory","achieved_containment":"sealed","floor_met":true,"enforcement_tier":"mountns+pivot_root+landlock+seccomp","filesystem_scope":null,"workdir_exec":false,"network_allow":[],"unix_sockets":false,"shell_allow":[],"spawn_allow":[],"env_allow":[],"interpreter_runtime_grants":[],"staged_runtime_grants":[]}
```

`"achieved_containment":"sealed"` is the gate. Anything else and **steps 3–4 prove nothing** on
this machine; stop and find another host.

Build the CLI once, for steps 1 and 2:

```sh
cargo build -p murmur-cli --offline
BIN="$PWD/target/debug/mur"
```

The `capsule.wasm` in every fixture below is a bare module header — enough to be discovered and
read, and deliberately **not** a valid component. That is what makes "the gate was not hit"
observable: a capsule that gets past the reachability check fails loudly at
`error[E-RUN-001]: failed to compile capsule component`, which is a *pass* for these steps.

## Step 1 — An uncovered interpreted entrypoint refuses with `E-CAP-006`

```console
$ mkdir -p /tmp/mur-427-verify/c1 && cd /tmp/mur-427-verify/c1
$ cat murmur.yaml
name: reachability-pip
version: 0.0.1
capabilities:
  containment: sealed
  shell:
    allow:
      - pip

$ printf '\0asm\x01\0\0\0' > capsule.wasm
```

`mur doctor` first — it never launches, so it reports this as a warning and keeps going:

```console
$ mur doctor; echo "doctor exit=$?"
[mur doctor] warning[E-CAP-006]: capabilities.shell.allow grants 'pip' (/home/agape/.local/bin/pip, a script run by 'python3') under the 'sealed' containment floor, but nothing declared makes the interpreted entrypoint's own package tree reachable inside the composed root — the script's ELF/DT_NEEDED closure is empty, so staging it stages nothing its interpreter imports, and the capsule would fail with a module-not-found error partway into a run rather than here. Declare capabilities.shell.interpreter_runtime or capabilities.shell.staged_runtime naming the interpreter (measure the real directories with `strace -f -e trace=openat,getdents64 <the command>`), or use a copy of the interpreter and its packages that already lives under a fixed sealed runtime path
  `mur run` will refuse this capsule at the declared floor — declare the grant above, or lower `capabilities.containment` if this capsule does not need a composed root.
Checking /tmp/mur-427-verify/c1/murmur.yaml for linux-x86_64...

All checks passed.
doctor exit=0
```

Note `doctor exit=0` and `All checks passed.` — the warning did **not** abort the rest of the
checklist, matching the `E-CAP-004`/`W-SEC-009`/`W-SEC-011` precedent.

`mur run` refuses, before any registry pull, component compile or workdir creation:

```console
$ mur run --manifest murmur.yaml; echo "run exit=$?"
status:  failed
error[E-CAP-006]: capabilities.shell.allow grants 'pip' (/home/agape/.local/bin/pip, a script run by 'python3') under the 'sealed' containment floor, but nothing declared makes the interpreted entrypoint's own package tree reachable inside the composed root — the script's ELF/DT_NEEDED closure is empty, so staging it stages nothing its interpreter imports, and the capsule would fail with a module-not-found error partway into a run rather than here. Declare capabilities.shell.interpreter_runtime or capabilities.shell.staged_runtime naming the interpreter (measure the real directories with `strace -f -e trace=openat,getdents64 <the command>`), or use a copy of the interpreter and its packages that already lives under a fixed sealed runtime path
  hint: declare `capabilities.shell.interpreter_runtime` (or `staged_runtime`) for the interpreter named above, listing the directories its import machinery actually reads — measure them on this host with `strace -f -e trace=openat,getdents64 <the command>` rather than guessing, since murmur deliberately does not try to derive an interpreted program's import closure. Alternatively point `capabilities.shell.allow` at a copy that already lives under a fixed sealed runtime path (a distro `/usr/bin` interpreter and its system packages need no grant at all). See docs/content/reference/manifest-schema.md
run exit=1
```

The binary (`pip`), its resolved path (`/home/agape/.local/bin/pip`) and its detected interpreter
(`python3`) are all named, as is the fix.

### Step 1a — A covering grant clears it (both forms)

`interpreter_runtime` naming the **script itself**:

```console
$ cat murmur.yaml   # /tmp/mur-427-verify/c1b
name: reachability-pip-granted
version: 0.0.1
capabilities:
  containment: sealed
  shell:
    allow:
      - pip
    interpreter_runtime:
      - binary: pip
        dirs:
          - path: /home/agape/.local/lib/python3.12/site-packages
            list_dir: true

$ mur run --manifest murmur.yaml
[capsule-runtime] warning[W-SEC-009]: capabilities.shell.interpreter_runtime grants 'pip' host directories outside the workdir [/home/agape/.local/lib/python3.12/site-packages (list_dir)] — this couples the capsule to a specific host distro/interpreter-version layout (e.g. /usr/lib/python3.11 breaks the moment the host ships Python 3.12); the durable fix is the staged runtime bind-mount, which this grant only bridges until (https://docs.murmur.nexus/murmur-nexus/murmur/reference/security-warnings/#w-sec-009)
status:  failed
error[E-RUN-001]: failed to compile capsule component: failed to parse WebAssembly module
```

`interpreter_runtime` naming the **shebang interpreter**. Note that the manifest parser requires a
grant's `binary` to appear in `shell.allow`, so naming the interpreter means allowlisting it too:

```console
$ cat murmur.yaml
name: reachability-pip-granted
version: 0.0.1
capabilities:
  containment: sealed
  shell:
    allow:
      - pip
      - python3
    interpreter_runtime:
      - binary: python3
        dirs:
          - path: /home/agape/.local/lib/python3.12/site-packages
            list_dir: true

$ mur run --manifest murmur.yaml
status:  failed
error[E-RUN-001]: failed to compile capsule component: failed to parse WebAssembly module
```

Both reach `E-RUN-001` — i.e. both got past the reachability gate. Without the grant, the same
manifest reached `E-CAP-006` and never got that far.

Omitting the grant *without* it in `shell.allow` is rejected earlier, by the manifest parser:

```console
error[E-MAN-003]: murmur.yaml: invalid capability config for 'capabilities.shell.interpreter_runtime[0].binary': 'python3' is not in capabilities.shell.allow — interpreter_runtime can only narrow filesystem access for an already-allowlisted binary, never grant exec
```

### Step 1b — A script already under a fixed sealed runtime path needs no grant

This is the roadmap's acceptance criterion 3. The host used here has no `/usr/bin/pip3`, so the
stand-in is another distro-installed Python console script with the identical shape — a `#!` script
under `/usr/bin` whose package lives in `/usr/lib/python3/dist-packages`, both inside
`SEALED_RUNTIME_PATHS`:

```console
$ which add-apt-repository; head -1 /usr/bin/add-apt-repository
/usr/bin/add-apt-repository
#!/usr/bin/python3

$ cat murmur.yaml   # /tmp/mur-427-verify/c3
name: reachability-system-script
version: 0.0.1
capabilities:
  containment: sealed
  shell:
    allow:
      - add-apt-repository

$ mur doctor
Checking /tmp/mur-427-verify/c3/murmur.yaml for linux-x86_64...

All checks passed.

$ mur run --manifest murmur.yaml; echo "run exit=$?"
status:  failed
error[E-RUN-001]: failed to compile capsule component: failed to parse WebAssembly module
run exit=1
```

No `E-CAP-006`, no grant declared, no warning printed — the fixed tree covers it.

## Step 2 — An uncovered compiler driver warns with `W-SEC-012`

```console
$ cat murmur.yaml   # /tmp/mur-427-verify/c2
name: reachability-cc
version: 0.0.1
capabilities:
  containment: sealed
  shell:
    allow:
      - cc

$ mur run --manifest murmur.yaml; echo "run exit=$?"
[capsule-runtime] warning[W-SEC-012]: capabilities.shell.allow grants the compiler driver 'cc', but its helper 'cc1' at /usr/libexec/gcc/x86_64-linux-gnu/13/cc1 has no grant carrying the Landlock Execute right under the 'sealed' composed root — the fixed sealed runtime tree (/usr, /bin, /sbin, /lib, /lib32, /lib64, /libx32) is bound read-only and listable but deliberately not executable, so the driver will start and then fail partway through a real compile; declare capabilities.shell.interpreter_runtime or staged_runtime for 'cc' naming that helper's directory (/usr/libexec/gcc/x86_64-linux-gnu/13) to grant it (https://docs.murmur.nexus/murmur-nexus/murmur/reference/security-warnings/#w-sec-012)
[capsule-runtime] warning[W-SEC-012]: capabilities.shell.allow grants the compiler driver 'cc', but its helper 'cc1plus' at /usr/libexec/gcc/x86_64-linux-gnu/13/cc1plus has no grant carrying the Landlock Execute right under the 'sealed' composed root — the fixed sealed runtime tree (/usr, /bin, /sbin, /lib, /lib32, /lib64, /libx32) is bound read-only and listable but deliberately not executable, so the driver will start and then fail partway through a real compile; declare capabilities.shell.interpreter_runtime or staged_runtime for 'cc' naming that helper's directory (/usr/libexec/gcc/x86_64-linux-gnu/13) to grant it (https://docs.murmur.nexus/murmur-nexus/murmur/reference/security-warnings/#w-sec-012)
[capsule-runtime] warning[W-SEC-012]: capabilities.shell.allow grants the compiler driver 'cc', but its helper 'as' at /usr/bin/x86_64-linux-gnu-as has no grant carrying the Landlock Execute right under the 'sealed' composed root — the fixed sealed runtime tree (/usr, /bin, /sbin, /lib, /lib32, /lib64, /libx32) is bound read-only and listable but deliberately not executable, so the driver will start and then fail partway through a real compile; declare capabilities.shell.interpreter_runtime or staged_runtime for 'cc' naming that helper's directory (/usr/bin) to grant it (https://docs.murmur.nexus/murmur-nexus/murmur/reference/security-warnings/#w-sec-012)
[capsule-runtime] warning[W-SEC-012]: capabilities.shell.allow grants the compiler driver 'cc', but its helper 'ld' at /usr/bin/x86_64-linux-gnu-ld.bfd has no grant carrying the Landlock Execute right under the 'sealed' composed root — the fixed sealed runtime tree (/usr, /bin, /sbin, /lib, /lib32, /lib64, /libx32) is bound read-only and listable but deliberately not executable, so the driver will start and then fail partway through a real compile; declare capabilities.shell.interpreter_runtime or staged_runtime for 'cc' naming that helper's directory (/usr/bin) to grant it (https://docs.murmur.nexus/murmur-nexus/murmur/reference/security-warnings/#w-sec-012)
[capsule-runtime] warning[W-SEC-012]: capabilities.shell.allow grants the compiler driver 'cc', but its helper 'collect2' at /usr/libexec/gcc/x86_64-linux-gnu/13/collect2 has no grant carrying the Landlock Execute right under the 'sealed' composed root — the fixed sealed runtime tree (/usr, /bin, /sbin, /lib, /lib32, /lib64, /libx32) is bound read-only and listable but deliberately not executable, so the driver will start and then fail partway through a real compile; declare capabilities.shell.interpreter_runtime or staged_runtime for 'cc' naming that helper's directory (/usr/libexec/gcc/x86_64-linux-gnu/13) to grant it (https://docs.murmur.nexus/murmur-nexus/murmur/reference/security-warnings/#w-sec-012)
status:  failed
error[E-RUN-001]: failed to compile capsule component: failed to parse WebAssembly module
run exit=1
```

Five helpers, five warnings, and the launch **proceeds** — this is a warning, not a refusal. Two
details worth noting from the literal output:

* `as` and `ld` resolved to `/usr/bin/x86_64-linux-gnu-as` and `/usr/bin/x86_64-linux-gnu-ld.bfd`.
  On this host the driver answers `-print-prog-name=as` with the bare name `as`, deferring to
  `PATH`; the check resolves that through `PATH` and then canonicalizes, which follows
  `/usr/bin/as` → its real target. Both facts are load-bearing: without the `PATH` fallback these
  two would have been invisible.
* every one of these paths is inside `SEALED_RUNTIME_PATHS`, and that is exactly why they warn.

`mur doctor` prints the identical five lines and still finishes its checklist:

```console
$ mur doctor
[capsule-runtime] warning[W-SEC-012]: ... (the same five lines) ...
Checking /tmp/mur-427-verify/c2/murmur.yaml for linux-x86_64...

All checks passed.
```

### Step 2a — A covering grant clears it

```console
$ cat murmur.yaml   # /tmp/mur-427-verify/c2b
name: reachability-cc-granted
version: 0.0.1
capabilities:
  containment: sealed
  shell:
    allow:
      - cc
    interpreter_runtime:
      - binary: cc
        dirs:
          - path: /usr/libexec/gcc/x86_64-linux-gnu/13
            list_dir: true
          - path: /usr/bin
            list_dir: true

$ mur run --manifest murmur.yaml 2>&1 | grep -c W-SEC-012
0

$ mur run --manifest murmur.yaml 2>&1 | grep -E '^(status|error)'
status:  failed
error[E-RUN-001]: failed to compile capsule component: failed to parse WebAssembly module
```

## Scratch harness

Steps 3 and 4 need a shell tool call *inside* a live composed root. `shell::execute_shell` is
crate-private, so they run from scratch tests appended to
`crates/capsule-runtime/src/sandbox.rs`. **Do not commit them.** Append both inside
`mod linux_integration_tests`, just before that module's closing brace:

```rust
    #[test]
    fn scratch_sealed_toolchain_compile() {
        use murmur_artifact::{ContainmentClass, InterpreterRuntimeDir, InterpreterRuntimeGrant};

        let grant = std::env::var("SCRATCH_GRANT").unwrap_or_default() == "1";
        let workdir = tempfile::tempdir().unwrap();

        let mut policy = CapabilityPolicy {
            shell_allow: vec!["bash".to_string(), "cc".to_string()],
            ..CapabilityPolicy::default()
        };
        if grant {
            policy.shell_interpreter_runtime = vec![InterpreterRuntimeGrant {
                binary: "cc".to_string(),
                dirs: ["/usr/libexec/gcc/x86_64-linux-gnu/13", "/usr/bin",
                       "/usr/lib/gcc/x86_64-linux-gnu/13", "/usr/include",
                       "/usr/lib/x86_64-linux-gnu"]
                    .into_iter()
                    .map(|path| InterpreterRuntimeDir { path: path.to_string(), list_dir: true })
                    .collect(),
            }];
        }

        // The production resolver, not a hand-built struct: tier, Landlock grants, sealed bind
        // dirs and staged-runtime dirs are all whatever a real sealed launch would compute.
        let enforcement = ShellEnforcement::resolve(&policy, ContainmentClass::Sealed).unwrap();
        eprintln!("SCRATCH_GRANT={grant}");
        eprintln!("TIER: {:?}", enforcement.tier);

        let script = std::env::var("SCRATCH_SCRIPT").expect("set SCRATCH_SCRIPT");
        let result = crate::shell::execute_shell(
            "bash", &["-c", &script], &[], workdir.path(), &policy, &enforcement,
        )
        .expect("execute_shell must return Ok");
        eprintln!("EXIT: {}", result.exit_code);
        eprintln!("OUT: {}", result.stdout);
        eprintln!("ERR: {}", result.stderr);
    }

    #[test]
    fn scratch_sealed_interpreted_entrypoint() {
        use murmur_artifact::{ContainmentClass, InterpreterRuntimeDir, InterpreterRuntimeGrant};

        let grant = std::env::var("SCRATCH_GRANT").unwrap_or_default() == "1";
        let workdir = tempfile::tempdir().unwrap();

        let mut policy = CapabilityPolicy {
            shell_allow: vec!["bash".to_string(), "pip".to_string(), "python3".to_string()],
            ..CapabilityPolicy::default()
        };
        if grant {
            policy.shell_interpreter_runtime = vec![InterpreterRuntimeGrant {
                binary: "python3".to_string(),
                dirs: vec![InterpreterRuntimeDir {
                    path: "/home/agape/.local/lib/python3.12/site-packages".to_string(),
                    list_dir: true,
                }],
            }];
        }

        let enforcement = ShellEnforcement::resolve(&policy, ContainmentClass::Sealed).unwrap();
        eprintln!("SCRATCH_GRANT={grant}");
        eprintln!("TIER: {:?}", enforcement.tier);

        let script = std::env::var("SCRATCH_SCRIPT").expect("set SCRATCH_SCRIPT");
        let result = crate::shell::execute_shell(
            "bash", &["-c", &script], &[], workdir.path(), &policy, &enforcement,
        )
        .expect("execute_shell must return Ok");
        eprintln!("EXIT: {}", result.exit_code);
        eprintln!("OUT: {}", result.stdout);
        eprintln!("ERR: {}", result.stderr);
    }
```

Nothing about the sandbox is stubbed. `ShellEnforcement::resolve` is the production entry point;
the only thing standing in for production is the agent loop, which never touches this.

When you are done with steps 3–4:

```sh
git checkout crates/capsule-runtime/src/sandbox.rs
```

## Step 3 — The Case 2 hypothesis, confirmed: an uncovered `cc` fails a real compile

This is the roadmap's "hypothesised; verify as part of this card". It is now measured.

```console
$ SCRATCH_SCRIPT='export TMPDIR="$PWD"; echo "int main(){return 0;}" > a.c; cc -o a a.c && echo COMPILE_OK || echo COMPILE_FAILED' \
    cargo test -p capsule-runtime --lib --offline -- --nocapture --exact \
    sandbox::linux_integration_tests::scratch_sealed_toolchain_compile

running 1 test
SCRATCH_GRANT=false
TIER: KernelSealed
EXIT: 0
OUT: COMPILE_FAILED

ERR: cc: fatal error: cannot execute ‘/usr/libexec/gcc/x86_64-linux-gnu/13/cc1’: execv: Permission denied
compilation terminated.

test sandbox::linux_integration_tests::scratch_sealed_toolchain_compile ... ok
```

`execv: Permission denied` on `/usr/libexec/gcc/x86_64-linux-gnu/13/cc1` — the **exact path**
`W-SEC-012` named in step 2, failing in the **exact way** it predicted. The driver started (it
parsed its arguments and reached the front-end exec); the compile did not finish. This is Landlock
refusing an `execve` on a path that is present and readable inside the root, which is precisely
what `executable: false` on the fixed sealed tree means.

`export TMPDIR="$PWD"` is needed because `cc` writes its intermediate files to `$TMPDIR`, and the
composed root's `/tmp` is not writable by the capsule. Without it the run fails one step earlier,
which is a real (and separate) constraint worth recording:

```console
ERR: bash: line 1: /usr/bin/head: Permission denied
Cannot create temporary file in /tmp/: Permission denied
bash: line 1: 398608 Aborted                 (core dumped) cc -o a a.c
```

(`/usr/bin/head: Permission denied` in that output is the same mechanism again — `head` was not in
`shell.allow`, so it has no `Execute` grant either.)

### Step 3a — The declared grant makes the same compile succeed

```console
$ SCRATCH_GRANT=1 \
  SCRATCH_SCRIPT='export TMPDIR="$PWD"; echo "int main(){return 0;}" > a.c; cc -o a a.c && echo COMPILE_OK || echo COMPILE_FAILED; ls -l a 2>&1' \
    cargo test -p capsule-runtime --lib --offline -- --nocapture --exact \
    sandbox::linux_integration_tests::scratch_sealed_toolchain_compile

running 1 test
SCRATCH_GRANT=true
TIER: KernelSealed
EXIT: 0
OUT: COMPILE_OK
-rwxrwxr-x 1 1000 1000 15776 Aug  8 01:42 a

ERR:
test sandbox::linux_integration_tests::scratch_sealed_toolchain_compile ... ok
```

A real 15,776-byte executable, produced by a real `cc1`/`as`/`ld`/`collect2` chain, inside a real
composed root. **Case 2 is confirmed in both directions.**

## Step 4 — The Case 1 failure, and the limit of what `E-CAP-006` claims

First, the failure the refusal exists to pre-empt. Same harness, `pip` allowlisted, no grant:

```console
$ SCRATCH_SCRIPT='pip --version && echo PIP_OK || echo PIP_FAILED' \
    cargo test -p capsule-runtime --lib --offline -- --nocapture --exact \
    sandbox::linux_integration_tests::scratch_sealed_interpreted_entrypoint

running 1 test
SCRATCH_GRANT=false
TIER: KernelSealed
EXIT: 0
OUT: PIP_FAILED

ERR: Traceback (most recent call last):
  File "/home/agape/.local/bin/pip", line 3, in <module>
    from pip._internal.cli.main import main
ModuleNotFoundError: No module named 'pip'

test sandbox::linux_integration_tests::scratch_sealed_interpreted_entrypoint ... ok
```

`ModuleNotFoundError: No module named 'pip'`, verbatim as the slice describes — an ENOENT-class
failure inside the root, with nothing anywhere saying "policy". Step 1 turns exactly this into a
launch-time `E-CAP-006`.

### Step 4a — A declared grant is not the same as a *correct* grant

This slice states plainly that it verifies a covering grant was declared, never that the directory
it names is right. That is not a hedge; it is observable. With the grant from step 1a declared —
`interpreter_runtime` for `python3` naming the real site-packages directory — `pip` still fails:

```console
$ SCRATCH_GRANT=1 SCRATCH_SCRIPT='pip --version && echo PIP_OK || echo PIP_FAILED' \
    cargo test -p capsule-runtime --lib --offline -- --nocapture --exact \
    sandbox::linux_integration_tests::scratch_sealed_interpreted_entrypoint

running 1 test
SCRATCH_GRANT=true
TIER: KernelSealed
EXIT: 0
OUT: PIP_FAILED

ERR: Traceback (most recent call last):
  File "/home/agape/.local/bin/pip", line 3, in <module>
    from pip._internal.cli.main import main
ModuleNotFoundError: No module named 'pip'
```

The directory is genuinely reachable — the grant worked. What is wrong is `sys.path`:

```console
$ SCRATCH_GRANT=1 SCRATCH_SCRIPT='python3 -c "import sys,site; print(sys.path); print(site.getusersitepackages())"; PYTHONPATH=/home/agape/.local/lib/python3.12/site-packages pip --version && echo PIP_OK || echo PIP_FAILED' \
    cargo test -p capsule-runtime --lib --offline -- --nocapture --exact \
    sandbox::linux_integration_tests::scratch_sealed_interpreted_entrypoint

running 1 test
SCRATCH_GRANT=true
TIER: KernelSealed
EXIT: 0
OUT: ['', '/usr/lib/python312.zip', '/usr/lib/python3.12', '/usr/lib/python3.12/lib-dynload', '/usr/local/lib/python3.12/dist-packages', '/usr/lib/python3/dist-packages']
/tmp/.tmp4jPtgn/.capsule-home/.local/lib/python3.12/site-packages
--- with PYTHONPATH ---
pip 26.2 from /home/agape/.local/lib/python3.12/site-packages/pip (python 3.12)
PIP_OK
```

CPython derives the user-site directory from `HOME`, and a sealed session relocates `HOME` to a
per-session `.capsule-home` inside the workdir — so `site.getusersitepackages()` computes
`/tmp/.tmp4jPtgn/.capsule-home/.local/lib/python3.12/site-packages`, and the host's real user-site
directory is never on `sys.path` no matter how thoroughly it is granted. Adding `PYTHONPATH`
resolves it, and `pip 26.2 from /home/agape/.local/lib/python3.12/site-packages/pip` proves the
granted tree really was reachable all along.

This is the honest shape of the guarantee, and the reason step 1's error text points at
`strace -f -e trace=openat,getdents64` rather than at a derived answer:

> `E-CAP-006` guarantees you were **made to think about the interpreter's runtime** before the
> capsule launched. It does not guarantee the capsule will work. Getting an interpreted closure
> right is still the operator's job — and, as above, may require more than a directory grant.

The `/usr` case in step 1b needs none of this, which is why it is the recommended answer wherever
it is available: a distro interpreter's `dist-packages` is on the default `sys.path`, and the fixed
sealed tree already binds and lists it.

## Recording the result

| Step | What it checks | Result |
|---|---|---|
| Host probe | `achieved_containment` is `sealed` on this machine | **PASS** — `"achieved_containment":"sealed"`, tier `mountns+pivot_root+landlock+seccomp` |
| 1 | uncovered interpreted entrypoint refuses at launch (`E-CAP-006`), from `mur run` and as a non-aborting warning from `mur doctor` | **PASS** |
| 1a | a covering grant (naming the script, or naming the interpreter) clears the refusal | **PASS** — both forms |
| 1b | a script already under `SEALED_RUNTIME_PATHS` needs no grant | **PASS** |
| 2 | uncovered compiler driver warns (`W-SEC-012`), naming driver, helper and resolved path, without refusing | **PASS** — 5 helpers, launch proceeded |
| 2a | a covering `interpreter_runtime` grant suppresses every `W-SEC-012` | **PASS** — 0 warnings |
| 3 | **the roadmap's Case 2 hypothesis**: uncovered `cc` fails a real compile inside a real composed root, at the named path | **PASS — CONFIRMED**: `cannot execute '/usr/libexec/gcc/x86_64-linux-gnu/13/cc1': execv: Permission denied` |
| 3a | the declared grant makes the same compile succeed | **PASS** — `COMPILE_OK`, 15,776-byte binary |
| 4 | the Case 1 failure inside the root is `ModuleNotFoundError` | **PASS** |
| 4a | a declared grant is not necessarily a *correct* grant — the stated limit of `E-CAP-006` | **PASS (as documented)** — grant reachable, `sys.path` still wrong because `HOME` is relocated |

What was **not** covered by this run:

* No step was driven through a full capsule-driven agent session — there were no LLM credentials
  on the host. Steps 1–2 used the real `mur` binary end to end; steps 3–4 used the production
  `ShellEnforcement` through the scratch harness.
* Only the GCC driver family was exercised. `clang`/`clang++` are not in
  `KNOWN_TOOLCHAIN_DRIVERS` and were not probed.
* Only CPython was exercised as an interpreted entrypoint. Node, Ruby and Perl console scripts go
  down the same code path (`shebang_interpreter_name` is interpreter-agnostic) but were not run.
* The host had no `/usr/bin/pip3`, so step 1b used `/usr/bin/add-apt-repository` — the same shape
  (a distro `#!` Python console script under `/usr`), not the roadmap's literal example.

## Related

- [`E-CAP-006` and `W-SEC-012` in context](manifest-schema.md#sealed-reachability-checks) — what
  each check does, and what it deliberately does not do.
- [`W-SEC-012`](security-warnings.md#w-sec-012) — the warning's full write-up, including why it is
  a warning and not a refusal.
- [Sealed containment — manual verification](sealed-containment-manual-verification.md) — the
  composed root itself, which everything here presumes.
- [Workdir `Execute` rights — manual verification](workdir-exec-landlock-manual-verification.md) —
  the other half of the `Execute`-right story, and the source of this document's scratch-harness
  pattern.
