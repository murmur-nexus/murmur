# What a contained capsule cannot do

Every limit on this page applies to a capsule whose `mur run --explain-scope` reports
`achieved: scoped` or `achieved: sealed`. On a Linux host with Landlock that is every capsule that
does not set `capabilities.filesystem.workdir_exec: true`, whatever class it declares; with that key
the capsule reports `achieved: advisory` and keeps every limit but
[Programs the capsule writes](#workdir-exec). An [unconfined launch](containment.md#unconfined)
lifts every limit except the resource limits the manifest declares, and a host with no kernel
containment enforces only what the [tier table](containment.md#subprocess-enforcement-tiers) gives
it.

`mur doctor --programs` starts programs inside the sandbox this project's capsule would get on this
host and prints, for each one it cannot run, a `cause` and the grant that lifts it:

```text
Programs (probed inside the sandbox: scoped, landlock+seccomp)
  PATH: /usr/local/bin:/usr/bin:/bin:/home/u/.cargo/bin
  ✓  bash      runs · /usr/bin/bash
  ✗  curl      refused (not-in-shell-allow) · /usr/bin/curl
        Probe: curl --version → exit 0
        Reason: curl runs only once capabilities.shell.allow names it
        Grant: add curl to capabilities.shell.allow
        Note: reaches only hosts in capabilities.network.allow (none declared)
  ✗  unshare   refused (namespaces-refused) · /usr/bin/unshare
        Probe: unshare --user --map-root-user true → exit 1
        Reason: the capsule's seccomp filter refuses unshare, setns, mount, pivot_root and chroot at every contained floor; unshare exited 1
        Grant: no manifest grant lifts this; only an unconfined launch does (mur run --unconfined, or containment: unconfined in ~/.murmur/config.yaml)
        Output: unshare: unshare failed: Operation not permitted
```

[`mur doctor --programs`](cli.md#doctor-programs) lists the programs it probes and every line it
prints.

## Doctor causes { #doctor-causes }

| Cause | Limit |
|---|---|
| `not-installed` | [Programs not on `mur run`'s `PATH`](#not-installed) |
| `not-in-shell-allow` | [Programs `shell.allow` does not name](#shell-allow), [`curl`](#curl) |
| `snap-wrapper` | [`curl`: not granted, or a snap wrapper](#curl) |
| `namespaces-refused` | [Namespaces](#namespaces) |
| `no-new-privileges` | [Privilege](#privileges) |
| `host-path-needed` | [Cargo and rustup](#cargo) |
| `interpreter-runtime-needed` | [Files a program loads or runs from outside its grant](#runtime-files), [Compiler helpers](#compiler-helpers), [Cargo and rustup](#cargo) |
| `interpreter-not-allowed` | [A script's interpreters](#interpreter-chain) |
| `helper-not-allowed` | [Compiler helpers](#compiler-helpers) |
| `no-sandbox` | Not a capsule limit: [the host cannot build the sandbox](#no-sandbox) |
| `timed-out` | Not a capsule limit: [a probe that hangs or fails otherwise](#probe-failed) |
| `failed` | Not a capsule limit: [a probe that hangs or fails otherwise](#probe-failed) |

## Programs { #programs }

### Programs `shell.allow` does not name { #shell-allow }

| | |
|---|---|
| What fails | The program exits 126 with `Permission denied` naming its path: `bash: line 1: /usr/bin/curl: Permission denied` |
| Why | Landlock gives the execute right only to each `capabilities.shell.allow` binary, its loader and its shared libraries. See [Subprocess enforcement tiers](containment.md#subprocess-enforcement-tiers) |
| Lifted by | The program's bare name in [`capabilities.shell.allow`](manifest.md#shell-allow) |
| Shown by | `mur doctor --programs <name>`; the `shell allow:` list under `Effective grants` in `mur run --explain-scope` |
| Doctor | `not-in-shell-allow` |

### Programs not on `mur run`'s `PATH` { #not-installed }

| | |
|---|---|
| What fails | `bash: line 1: node: command not found`, exit 127 |
| Why | A capsule runs the binary of each `shell.allow` name found on the `PATH` `mur run` starts with. See [Shell allow](manifest.md#shell-allow) |
| Lifted by | Installing the program on the host, or putting its folder on `PATH` when you start `mur run` |
| Shown by | `mur doctor --programs <name>`, which prints `not on PATH` and the `PATH:` it searched |
| Doctor | `not-installed` |

### Programs the capsule writes { #workdir-exec }

| | |
|---|---|
| What fails | A binary written to the work folder, the temporary folder or the home folder exits 126: `bash: ./hello: Permission denied`. A Cargo build script fails with ``could not execute process `…/build-script-build` … Permission denied (os error 13)`` |
| Why | Those three folders carry no execute right, so nothing the capsule produces runs. See [Executable workdirs](containment.md#field-workdir-exec) |
| Lifted by | `capabilities.filesystem.workdir_exec: true`, which caps the achieved class at `advisory` ([`W-SEC-011`](diagnostics.md#w-sec-011)). With `capabilities.containment: scoped` or `sealed` the launch is refused with [`E-CAP-003`](diagnostics.md#e-cap-003) |
| Shown by | `workdir exec:` under `Effective grants` and `achieved:` under `Containment` in `mur run --explain-scope`; with a `scoped` or `sealed` floor, `floor met: no` |
| Doctor | Not reported by `mur doctor --programs`. `mur doctor` prints `W-SEC-011` when the key is `true` |

### `curl`: not granted, or a snap wrapper { #curl }

| | |
|---|---|
| What fails | Not granted: as [Programs `shell.allow` does not name](#shell-allow). Granted, with `/snap/bin` first on `PATH`: `curl` exits 46, `internal error, please report: running "curl" failed: timeout waiting for snap system profiles to get updated` |
| Why | `/snap/bin/curl` is a link to the snap launcher, which cannot start a snap inside the capsule. See [Causes](cli.md#doctor-programs-causes) |
| Lifted by | Not granted: `curl` in `capabilities.shell.allow`, and the hosts it reaches in `capabilities.network.allow` ([No network](#no-network)). Snap: the folder of a non-snap copy ahead of `/snap/bin` on `mur run`'s `PATH`, or the distribution's package installed |
| Shown by | `mur doctor --programs curl` |
| Doctor | `not-in-shell-allow`, `snap-wrapper` |

### A script's interpreters { #interpreter-chain }

| | |
|---|---|
| What fails | A `#!` script such as `npm` (`#!/usr/bin/env node`) exits 126 with `Permission denied` when `shell.allow` names the script and not every interpreter its `#!` line runs |
| Why | The kernel runs each interpreter on the `#!` line, and each needs the execute right a `shell.allow` entry gives. See [Subprocess enforcement tiers](containment.md#subprocess-enforcement-tiers) |
| Lifted by | Every interpreter in the chain (`env`, `node`) in `capabilities.shell.allow` |
| Shown by | `mur doctor --programs <script>` |
| Doctor | `interpreter-not-allowed` |

### Files a program loads or runs from outside its grant { #runtime-files }

| | |
|---|---|
| What fails | `error while loading shared libraries: libz.so.1: cannot open shared object file`, or a program the command runs by path refused with `Permission denied` |
| Why | A `shell.allow` binary is granted its own shared libraries, file by file; a library loaded later or another program it runs lies outside every grant, and nothing in a [host path](containment.md#host-paths) is executable. See [Native addons](containment.md#native-addons) |
| Lifted by | [`capabilities.shell.interpreter_runtime`](manifest.md#field-capabilities) naming the folder that holds the file, on a binary `shell.allow` names ([`W-SEC-009`](diagnostics.md#w-sec-009)). Every command of the session reads what it grants |
| Shown by | `mur doctor --programs <name>`, whose `Grant:` names the folder; the `interpreter runtime:` list under `Effective grants` in `mur run --explain-scope` |
| Doctor | `interpreter-runtime-needed` |

### Compiler helpers { #compiler-helpers }

| | |
|---|---|
| What fails | `cc: fatal error: cannot execute '/usr/libexec/gcc/x86_64-linux-gnu/13/cc1': execv: Permission denied`; `fatal error: /usr/include/stdc-predef.h: Permission denied`; `cc: fatal error: cannot execute 'as': execvp: Permission denied`; `collect2: fatal error: execvp: Permission denied` (`ld`); ``linker `cc` not found`` from `rustc` |
| Why | A compiler runs helper programs (`cc1`, `as`, `collect2`, `ld`) and reads headers and start files by path, all outside the compiler's own grant. See [A library a grant brings in reach](containment.md#library-grant-reach) |
| Lifted by | The helpers found on `PATH` (`as`, `ld`, `ar`) in `capabilities.shell.allow`, and the folders the compiler reaches by path in an `interpreter_runtime` entry for `cc`. The folders below are one host's |
| Shown by | `mur doctor --programs cc`, which names one missing piece per run: follow its `Grant:` and probe again |
| Doctor | `interpreter-runtime-needed`, `helper-not-allowed` |

On Ubuntu 24.04 with gcc 13, `cc` runs inside a `scoped` capsule with:

```yaml
capabilities:
  shell:
    allow: [bash, cc, as, ld, ar]
    interpreter_runtime:
      - binary: cc
        dirs:
          - path: /usr/libexec/gcc/x86_64-linux-gnu/13   # cc1, collect2
            list_dir: false
          - path: /usr/lib/gcc/x86_64-linux-gnu/13       # crtbeginS.o, libgcc
            list_dir: false
          - path: /usr/lib/x86_64-linux-gnu              # crti.o, libc.so, libisl
            list_dir: false
          - path: /usr/include                           # stdc-predef.h
            list_dir: false
```

Find the folders on your own host:

| Folder | Command |
|---|---|
| Where `cc1` and `collect2` live | `cc -print-prog-name=cc1` |
| Where gcc's start files and `libgcc` live | `cc -print-libgcc-file-name` |
| Where the C library's start files live | `cc -print-file-name=crti.o` |
| The header folders | `cc -E -Wp,-v -xc /dev/null` |

### Cargo and rustup { #cargo }

| | |
|---|---|
| What fails | `error: could not read settings file: '/home/u/.rustup/settings.toml': Permission denied (os error 13)` from the rustup proxies in `~/.cargo/bin`; with the homes lent, `error: command failed: 'cargo': Permission denied (os error 13)`; a crate with build scripts, as [Programs the capsule writes](#workdir-exec); a large crate, `memory allocation of … bytes failed` and `SIGABRT` ([Address space](#memory-bytes)) |
| Why | `cargo` and `rustc` in `~/.cargo/bin` are rustup proxies that read `RUSTUP_HOME` and `CARGO_HOME`, which lie outside the capsule's folders, and run a toolchain that a host path cannot make executable. See [What an entry lends](containment.md#host-path-reach) |
| Lifted by | The recipe below: both homes lent `read` through [`host_paths`](config.md#host-paths), `mur run` started with `RUSTUP_HOME` and `CARGO_HOME` set, the toolchain folder in `interpreter_runtime`, the [compiler helpers](#compiler-helpers), `workdir_exec: true` for build scripts, and a larger `memory_bytes` |
| Shown by | `mur doctor --programs cargo`, then `mur doctor --programs cc`, run one at a time |
| Doctor | `host-path-needed`, then `interpreter-runtime-needed` and `helper-not-allowed` |

`~/.murmur/config.yaml`:

```yaml
host_paths:
  - path: ~/.rustup
    access: read
  - path: ~/.cargo
    access: read
```

`~/.cargo` must be `read` at `scoped` while `~/.cargo/bin` is on `mur run`'s `PATH`: a `read-write`
entry there is refused with [`E-CFG-004`](diagnostics.md#e-cfg-004). Cargo reads
`~/.cargo/config.toml`, and every `.cargo/config.toml` above the work folder, and stops with
`Permission denied` on one it cannot open.

`murmur.yaml`, for crates with build scripts or procedural macros:

```yaml
capabilities:
  filesystem:
    workdir_exec: true                 # build scripts run; the achieved class is advisory
  shell:
    allow: [bash, cargo, rustc, cc, as, ld, ar]
    interpreter_runtime:
      - binary: cargo
        dirs:
          - path: /home/u/.rustup/toolchains/stable-x86_64-unknown-linux-gnu
            list_dir: true
          - path: /usr/lib/x86_64-linux-gnu      # libz.so.1
            list_dir: false
      - binary: cc
        dirs:
          - path: /usr/libexec/gcc/x86_64-linux-gnu/13
            list_dir: false
          - path: /usr/lib/gcc/x86_64-linux-gnu/13
            list_dir: false
          - path: /usr/include
            list_dir: false
  resources:
    memory_bytes: 17179869184          # 16 GiB of address space for rustc
    cgroup_memory_bytes: 8589934592    # 8 GiB of RAM for the whole tree
```

For crates with no build script, drop `workdir_exec` and declare `containment: scoped` or
`containment: sealed`.

```bash
RUSTUP_HOME=$HOME/.rustup CARGO_HOME=$HOME/.cargo mur run --task task.md
```

| To find | Run |
|---|---|
| The toolchain folder | `rustc --print sysroot` |
| What the next probe needs | `mur doctor --programs cargo` |
| The compiler folders | The commands under [Compiler helpers](#compiler-helpers) |

Every clone builds its dependencies from cold;
[How to share a build cache between capsules](../how-to/share-a-build-cache.md) seeds them. In a
`bash` command `time` is a shell keyword: `command time -v` runs `/usr/bin/time`, which `time` in
`shell.allow` grants.

## Files and folders { #files-and-folders }

### Writes outside the session's folders { #writes }

| | |
|---|---|
| What fails | At `scoped`, `Permission denied` (`EACCES`). At `sealed`, a path outside the composed root does not exist (`No such file or directory`) |
| Why | The capsule writes only its work folder, its [home folder](containment.md#home-folder) and its [temporary folder](containment.md#temp-folder). See [Host paths](containment.md#host-paths) |
| Lifted by | A [`host_paths`](config.md#host-paths) entry with `access: read-write` in `~/.murmur/config.yaml`. `/usr`, `/etc`, `/tmp` and the other system folders, and any folder containing `~/.murmur/keys`, are refused with `E-CFG-004`; see [What `mur run` refuses](containment.md#host-path-refusals) |
| Shown by | `host paths:` under `Effective grants` in `mur run --explain-scope`; the `Host paths` block of `mur doctor` |
| Doctor | The `Host paths` block of `mur doctor` |

### Reads outside the granted set { #reads }

| | |
|---|---|
| What fails | At `scoped`, `Permission denied` on open: `wget: Cannot read /etc/wgetrc (Permission denied).` At `sealed`, the path does not exist |
| Why | A command reads its session's folders, each `shell.allow` binary's own files, the [derived interpreter runtimes](containment.md#derived-interpreter-runtime), the [trust store](containment.md#trust-store), the name-resolution files and what the grants below add. A refused path still answers `stat` at `scoped` |
| Lifted by | A `host_paths` entry with `access: read` for a folder outside the system folders; an `interpreter_runtime` entry for a file under `/usr` or `/etc` |
| Shown by | The `Effective grants` block of `mur run --explain-scope` (`host paths`, `interpreter runtime`, `derived interpreter runtime`, `trust store`, `name resolution`); the `Output:` lines of `mur doctor --programs <name>` |
| Doctor | No cause of its own: a refused read shows in `Output:`, and a refused absolute path a program needs is `interpreter-runtime-needed` |

### `HOME` is the session's own folder { #home-folder }

| | |
|---|---|
| What fails | `~/.ssh`, `~/.gitconfig`, `~/.cargo` and `~/.config/gh` are `No such file or directory`: `HOME` is `home/` in the session's records directory, empty at start |
| Why | `HOME` is the session's home folder on every tier; no manifest key, `capabilities.shell.baseline_env` entry or host value changes it. See [The session's home folder](containment.md#home-folder) |
| Lifted by | Nothing changes `HOME`. Lend the folder through `host_paths` and point the tool at it with a variable: `CARGO_HOME` and `RUSTUP_HOME` pass from `mur run`'s environment; another variable, such as `GIT_CONFIG_GLOBAL`, passes when `capabilities.shell.baseline_env` names it and `mur run` is started with it set |
| Shown by | The `HOME is set to the session's home folder` line under `Not protected here` in `mur run --explain-scope` |
| Doctor | Not reported by `mur doctor` |

### A private `/tmp` { #tmp }

| | |
|---|---|
| What fails | The host's files under `/tmp` are absent, a host daemon's socket under `/tmp` is unreachable, and `rename(2)` from `/tmp` into the work folder fails with `EXDEV` (`Invalid cross-device link`) |
| Why | `/tmp` is the session's temporary folder, a separate mount. See [Known properties](containment.md#temp-folder-properties) |
| Lifted by | No grant: a `host_paths` entry under `/tmp` is refused. Only an unconfined launch, which keeps the host's `/tmp` |
| Shown by | The `Temporary folder` block of `mur run --explain-scope` or `mur doctor`: `/tmp:      mapped (mount-namespace)`, `mapped (composed-root)` at `sealed`, or `not mapped — <reason>` on a host without a mount namespace |
| Doctor | The `Temporary folder` block of `mur doctor` |

### A private `/proc` { #proc }

| | |
|---|---|
| What fails | `ps` lists only the session's processes; `/proc/<host pid>` is `No such file or directory` |
| Why | Each session has its own PID namespace and `procfs`. See [A capsule's own `/proc`](containment.md#capsule-proc) |
| Lifted by | No grant. Only an unconfined launch |
| Shown by | The `Process view` block of `mur run --explain-scope` or `mur doctor`: `/proc:     private (pid-namespace)`, or `host — <reason>` on a host that cannot mount one |
| Doctor | The `Process view` block, and `private proc:` under `AppArmor / user namespaces` |

### A baseline environment { #environment }

| | |
|---|---|
| What fails | A host variable the command expects is unset: `AWS_PROFILE`, `SSH_AUTH_SOCK`, `NODE_OPTIONS` |
| Why | A shell command starts from an empty environment plus a fixed baseline, with every credential-shaped name removed. See [Process environment](manifest.md#process-environment) |
| Lifted by | The name in `capabilities.shell.baseline_env`, with `mur run` started with it set. A name that matches a credential pattern or `capabilities.shell.strip_env` is always removed, and `HOME`, `TMPDIR`, `TMP` and `TEMP` are always the session's |
| Shown by | No `mur` command prints a command's environment; `capabilities.shell.baseline_env` and `strip_env` in `murmur.yaml` are the whole of it |
| Doctor | Not reported by `mur doctor` |

## Network { #network }

### No network { #no-network }

| | |
|---|---|
| What fails | A name lookup is refused (`curl: (6) Could not resolve host: example.com`) and a connection to any address is `Connection refused` |
| Why | Commands run in the session's network namespace, whose only interface is loopback; the runtime relays only the names and ports `capabilities.network.allow` names. See [Network reach limits](containment.md#network-reach-limits) |
| Lifted by | Entries in [`capabilities.network.allow`](manifest.md#network-allow-entries) |
| Shown by | `network allow:` under `Effective grants` in `mur run --explain-scope`; `Note: reaches only hosts in capabilities.network.allow (none declared)` under `curl`, `wget` and `git` in `mur doctor --programs` |
| Doctor | The `Note:` line under `curl`, `wget` and `git`; no cause |

### Ports and IPv6 { #network-reach }

| | |
|---|---|
| What fails | A port no allow entry implies is `Connection refused`; an IPv6 destination is `ENETUNREACH` (`Network is unreachable`); ports past the 16th are unreachable |
| Why | The runtime listens only on the ports the allowlist implies, at most 16, and the namespace has no IPv6 route. See [Network reach limits](containment.md#network-reach-limits) |
| Lifted by | The port named in an allow entry (`host:port`). No grant lifts the IPv4-only and 16-port limits |
| Shown by | `network allow:` under `Effective grants` in `mur run --explain-scope` |
| Doctor | Not reported by `mur doctor` |

### Held ports { #held-ports }

| | |
|---|---|
| What fails | A server started on a port an allow entry implies fails with `EADDRINUSE`, and the shell result ends with a `held_port:` line; a bind below 1024 fails with `EACCES` |
| Why | The runtime holds each allowlisted port for the whole session, and commands hold no capabilities. See [Held ports](containment.md#held-ports) |
| Lifted by | Serving on a port no allow entry names. No grant lifts the ports below 1024 |
| Shown by | `network allow:` under `Effective grants` in `mur run --explain-scope`: each entry holds the ports [Held ports](containment.md#held-ports) lists |
| Doctor | Not reported by `mur doctor` |

### Loopback servers { #loopback }

| | |
|---|---|
| What fails | A server the capsule starts is `Connection refused` from the host, other sessions, delegated children and formation members |
| Why | Each session has its own network namespace, so its loopback is its own. See [Network reach limits](containment.md#network-reach-limits) |
| Lifted by | No grant. Only an unconfined launch, which installs no network namespace |
| Shown by | `network namespace: created` under `AppArmor / user namespaces` in `mur doctor` |
| Doctor | The `network namespace:` line of `mur doctor` |

### Unix sockets { #unix-sockets }

| | |
|---|---|
| What fails | Creating a Unix socket fails with `EACCES` (`Permission denied`): a `docker` client, an `ssh-agent` client, anything that talks to a local daemon |
| Why | The seccomp filter refuses `socket(AF_UNIX, …)` unless the manifest grants it, since `/var/run/docker.sock` is root on the host. See [`capabilities.network.unix_sockets`](manifest.md#field-capabilities) |
| Lifted by | `capabilities.network.unix_sockets: true`, which reaches every socket the process can open. A socket under the host's `/tmp` stays hidden ([A private `/tmp`](#tmp)) |
| Shown by | `unix sockets:` under `Effective grants` in `mur run --explain-scope` |
| Doctor | Not reported by `mur doctor` |

### The TLS trust store { #trust-store }

| | |
|---|---|
| What fails | A TLS client cannot open the CA certificates (`Permission denied` on `/etc/ssl/certs/ca-certificates.crt`) and fails to verify the server |
| Why | The host's trust store is granted only to a capsule with both `shell.allow` and `network.allow` non-empty, from a fixed list of locations. See [The trust store](containment.md#trust-store) |
| Lifted by | A non-empty `capabilities.network.allow`; a trust store outside the list needs an `interpreter_runtime` folder |
| Shown by | `trust store:` under `Effective grants` in `mur run --explain-scope` |
| Doctor | Not reported by `mur doctor` |

## Resources { #resources }

### Address space { #memory-bytes }

| | |
|---|---|
| What fails | `memory allocation of … bytes failed` and `SIGABRT` from `rustc`; `Cannot allocate Wasm memory` from `node`; a JVM that cannot start its heap |
| Why | `capabilities.resources.memory_bytes` is `RLIMIT_AS`, the address space each process may reserve, 2 GiB by default. See [`memory_bytes` and `cgroup_memory_bytes`](resource-limits.md#address-space-and-memory) |
| Lifted by | A larger `capabilities.resources.memory_bytes` (`68719476736` runs `node`), with RAM capped by `cgroup_memory_bytes` |
| Shown by | No `mur` command prints the value; it is `memory_bytes` in `murmur.yaml`, or the default. `mur doctor` prints [`W-RUN-010`](diagnostics.md#w-run-010) when `shell.allow` names `node` under 32 GiB |
| Doctor | `W-RUN-010`, for `node` and `nodejs` only |

### Work folder size { #workdir-size }

| | |
|---|---|
| What fails | The session ends with `E-RUN-013`: `work folder, $HOME and $TMPDIR together grew to <N> bytes, past the <M> byte ceiling (capabilities.resources.workdir_max_bytes)` |
| Why | The work folder, home folder and temporary folder together are held under one ceiling, 10 GiB by default. See [Host resource limits](resource-limits.md#host-resource-limits) |
| Lifted by | A larger `capabilities.resources.workdir_max_bytes` |
| Shown by | No `mur` command prints the value; it is `workdir_max_bytes` in `murmur.yaml`, or the default |
| Doctor | Not reported by `mur doctor` |

### The cgroup scope { #cgroup-limits }

| | |
|---|---|
| What fails | Past `cgroup_memory_bytes` the tree is OOM-killed; past `cgroup_pids_max` a fork fails with `EAGAIN`; each names its limit in a `resource_limit:` line. `cgroup_cpu_percent` and `cgroup_io_bytes_per_sec` slow the tree down |
| Why | A cgroup scope bounds the whole subprocess tree. See [Host resource limits](resource-limits.md#host-resource-limits) and [Which limit a subprocess hit](resource-limits.md#which-limit) |
| Lifted by | A larger value of each `capabilities.resources.cgroup_*` field |
| Shown by | The `Memory bound` and `Cgroup scope` blocks of `mur doctor`; `io.max:` under `Resource plane` in `mur run --explain-scope` |
| Doctor | The `Memory bound` and `Cgroup scope` blocks |

### Per-process limits { #process-limits }

| | |
|---|---|
| What fails | `EMFILE` past `max_open_files`; `SIGXFSZ` past `max_file_size_bytes`; `SIGXCPU`, then `SIGKILL`, past `cpu_seconds`; `EAGAIN` from `fork` past `max_processes` where no cgroup scope applies |
| Why | Each command runs under hard `setrlimit` ceilings. See [Host resource limits](resource-limits.md#host-resource-limits) |
| Lifted by | A larger value of the field under `capabilities.resources` |
| Shown by | No `mur` command prints the values; the trace's `shell` event names `cpu_seconds` and `max_file_size_bytes` in `resource_limit` ([Which limit a subprocess hit](resource-limits.md#which-limit)) |
| Doctor | Not reported by `mur doctor` |

## Always refused { #always-refused }

### Namespaces { #namespaces }

| | |
|---|---|
| What fails | `unshare: unshare failed: Operation not permitted`; `bwrap`, `podman` and anything else that creates a namespace or mounts fails the same way |
| Why | The seccomp filter refuses `unshare`, `setns`, `mount`, `pivot_root` and `chroot` at every contained class. See [Default-deny syscall allowlist](containment.md#default-deny-syscall-allowlist) |
| Lifted by | No manifest grant. Only an unconfined launch: `mur run --unconfined`, or `containment: unconfined` in `~/.murmur/config.yaml`; see [Running unconfined](containment.md#unconfined) |
| Shown by | `mur doctor --programs unshare` |
| Doctor | `namespaces-refused` |

### Privilege { #privileges }

| | |
|---|---|
| What fails | `sudo: The "no new privileges" flag is set, which prevents sudo from running as root.`; a setuid program runs with no privilege gained |
| Why | Every command runs with `no_new_privs`. See [Running unconfined](containment.md#unconfined) |
| Lifted by | No manifest grant. Only an unconfined launch, where `sudo` works as your sudoers allows; never as root, which is refused with [`E-RUN-063`](diagnostics.md#e-run-063) |
| Shown by | `mur doctor --programs sudo` |
| Doctor | `no-new-privileges` |

### Syscalls outside the allowlist { #syscalls }

| | |
|---|---|
| What fails | The syscall returns `EPERM` (`Operation not permitted`): `seccomp(2)` itself, `ptrace`, `bpf`, `io_uring_setup`, `perf_event_open`, `keyctl` |
| Why | The seccomp filter allows a fixed list of syscalls and denies the rest; widening the list is a change to the runtime, never to a manifest. See [Default-deny syscall allowlist](containment.md#default-deny-syscall-allowlist) |
| Lifted by | No manifest grant. Only an unconfined launch, which installs no filter |
| Shown by | `mur doctor --programs unshare` shows the filter at work; the kernel log names a refused syscall where the host logs seccomp actions |
| Doctor | Not reported by `mur doctor`, apart from the namespace calls (`namespaces-refused`) |

## Not a capsule limit { #not-a-limit }

### The host cannot build the sandbox { #no-sandbox }

| | |
|---|---|
| What fails | `mur run` refuses the launch before the capsule starts, with [`E-CAP-005`](diagnostics.md#e-cap-005) when the host cannot create the session's network namespace or `E-RUN-012` when it cannot create the cgroup scope; every probe reports `no-sandbox` |
| Why | The host, not the capsule, lacks a mechanism the class needs. See [Telling host conditions apart](containment.md#telling-host-conditions-apart) and [Whether the launch gets its cgroup scope](containment.md#cgroup-scope) |
| Lifted by | The refusal's hint, which names the host change |
| Shown by | The `AppArmor / user namespaces` and `Cgroup scope` blocks of `mur doctor` |
| Doctor | `no-sandbox` |

### A probe that hangs or fails otherwise { #probe-failed }

| | |
|---|---|
| What fails | The probe step outran its timeout and was killed, or exited non-zero for a reason no other cause names |
| Why | The program's own failure, not a limit of the capsule |
| Lifted by | Whatever the program's `Output:` lines name |
| Shown by | `mur doctor --programs <name>`: the `Probe:` and `Output:` lines |
| Doctor | `timed-out`, `failed` |
