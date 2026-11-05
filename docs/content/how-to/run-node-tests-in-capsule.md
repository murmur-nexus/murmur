# How to run Node tests in a capsule

A coding capsule that changes a JavaScript project should prove its change by running the project's
tests, and it should do that inside the same boundary as everything else it runs. This guide builds
a `scoped` capsule that installs a project's dependencies with `npm ci` and runs its
[vitest](https://vitest.dev) suite, with only the grants Node needs: the npm package tree, the
system library folder native addons load from, OpenSSL's configuration file, and an address-space
limit V8 can run under. The
capsule's `/proc` and the files that resolve `localhost` come with `scoped` and need no grant.

The relevant manifest options are:

| Option | Controls |
|---|---|
| [capabilities.containment](../reference/manifest.md#field-capabilities) | The containment floor the capsule refuses to launch below |
| [capabilities.shell.allow](../reference/manifest.md#field-capabilities) | Which binaries the capsule's commands may run |
| [capabilities.shell.interpreter_runtime](../reference/manifest.md#field-capabilities) | Host folders an allowlisted binary reads and loads from outside the workdir |
| [capabilities.network.allow](../reference/manifest.md#field-capabilities) | The hosts the capsule's commands may reach |
| [capabilities.resources.memory_bytes](../reference/manifest.md#field-capabilities) | The address-space limit on each command |
| [capabilities.resources.cgroup_memory_bytes](../reference/manifest.md#field-capabilities) | The memory the capsule's whole command tree may use |
| [lifecycle.done_check](../reference/manifest.md#lifecycle-done-check) | The command that decides whether a task is done |

---

## Step 1 — find the three host paths

Node's grants name three paths on your host.

| Path | Holds | How to find it |
|---|---|---|
| The npm package tree | `npm`'s own JavaScript, which the `npm` command runs | `echo "$(npm root -g)/npm"` |
| The system library folder | The shared libraries a native addon loads, such as rollup's `librt.so.1` | `ldd node_modules/@rollup/rollup-linux-x64-gnu/rollup.linux-x64-gnu.node` after a local `npm ci`, or `ldconfig -p \| grep librt.so.1` |
| OpenSSL's configuration file | The file `node` reads at startup | The path `node` names in `node: OpenSSL configuration error: … calling fopen(<path>, rb)` when it is refused |

On an x86-64 Debian or Ubuntu host with Node installed through nvm, they are
`/home/<you>/.nvm/versions/node/<version>/lib/node_modules/npm`, `/usr/lib/x86_64-linux-gnu` and
`/etc/ssl/openssl.cnf`.

## Step 2 — write the manifest

Create a `murmur.yaml` file next to your project's `package.json` and `package-lock.json`, with
your three paths in place of the ones below:

```yaml
name: node-tests
version: "0.1.0"

artifacts:
  - name: murmur/driver-openai
    version: "{{ v.murmur_driver_openai }}"
    runtime: driver
    gateway:
      endpoint: https://api.openai.com/v1
      api_key: ${OPENAI_API_KEY}

capabilities:
  containment: scoped
  shell:
    allow:
      - bash
      - env
      - node
      - npm
    interpreter_runtime:
      - binary: npm
        dirs:
          - path: /home/you/.nvm/versions/node/v22.23.2/lib/node_modules/npm
            list_dir: true
      - binary: node
        dirs:
          - path: /usr/lib/x86_64-linux-gnu
            list_dir: false
          - path: /etc/ssl/openssl.cnf
            list_dir: false
  network:
    allow:
      - https://registry.npmjs.org
  resources:
    memory_bytes: 68719476736
    cgroup_memory_bytes: 8589934592

inference:
  transport: http
  model: {{ v.model_openai }}
  driver:
    artifact: murmur/driver-openai

lifecycle:
  done_check:
    command: "bash -c 'npm ci --ignore-scripts && node node_modules/vitest/vitest.mjs run'"
    limit: 0
```

What each grant is for:

| Grant | Why |
|---|---|
| `env` in `shell.allow` | `npm` is a script whose first line is `#!/usr/bin/env node` |
| `interpreter_runtime` on `npm` | `npm` reads its own package tree, outside the workdir |
| `interpreter_runtime` on `node`: the library folder | A native addon in `node_modules` loads system libraries the sandbox does not know about until it loads — vitest's rollup fails with `librt.so.1: cannot open shared object file` without it; see [Native addons](../reference/containment.md#native-addons). This grant carries execute and draws [`W-SEC-009`](../reference/diagnostics.md#w-sec-009) |
| `interpreter_runtime` on `node`: `openssl.cnf` | `node` reads OpenSSL's configuration file at startup and exits with code 13, `node: OpenSSL configuration error`, when it is refused. One file, not its folder |
| `memory_bytes: 68719476736` | `memory_bytes` limits address space, and V8 reserves far more of it than it uses: below 32 GiB vitest fails with `Cannot allocate Wasm memory`, and [`W-RUN-010`](../reference/diagnostics.md#w-run-010) says so |
| `cgroup_memory_bytes` | The real cap on the memory the command tree uses, here 8 GiB |

Nothing grants `/proc`, `/etc/hosts` or `/etc/nsswitch.conf`. On `scoped`, each command has
[its own `/proc`](../reference/containment.md#capsule-proc), where `os.cpus()` and vitest's worker
pool find what they read, and [reads the two name-resolution files](../reference/containment.md#command-name-resolution-files)
that resolve `localhost`.

## Step 3 — run the tests the way `scoped` allows

`scoped` does not run files from the workdir, and every file under `node_modules` is in the workdir.
That decides the two commands:

| Command | Instead of | Why |
|---|---|---|
| `npm ci --ignore-scripts` | `npm ci` | An install script runs a file from the workdir |
| `node node_modules/vitest/vitest.mjs run` | `npm test`, `npx vitest`, `node_modules/.bin/vitest` | Each of those executes a file under `node_modules`; `node` reading vitest's entry point is `node` running, which `shell.allow` permits |

A toolchain that spawns a native binary from `node_modules` — esbuild for TypeScript, swc — needs
[`capabilities.filesystem.workdir_exec: true`](../reference/containment.md#field-workdir-exec),
which caps the capsule at `advisory`. A plain-JavaScript project with no vite or vitest config
needs neither.

## Step 4 — check the manifest

```bash
mur install
```

--8<-- "includes/mur-pull-info.md"

```bash
mur doctor
```

`mur doctor` prints no `W-RUN-010` for this manifest, and its `AppArmor / user namespaces` block
reads `private proc: created (…)`. `mur run --workdir . --explain-scope` shows the same view in
its `Process view` block:

```text
Process view
  /proc:     private (pid-namespace) — the session's own processes and the kernel's world-readable files; no host process
  grant:     read and list (scoped, sealed); never execute or write
```

## Step 5 — run the capsule

Create a `task.md` file asking for the change you want, then run it with the project folder as the
workdir, so the commands run where `package.json` is:

```bash
mur run --workdir . --task task.md
```

```
murmur: url localhost:52222
session: ses_019ed2af53da75c2aefee84ee10c34af
status:  ok
```

When the agent stops, the runtime runs the done check in the work folder. The session's trace
records each run as a `done_check` event: `outcome: passed` and `exit_code: 0` when every test file
passed. With `limit` above `0`, a failing run reopens the task and hands the agent what the
command printed.

```bash
mur trace show
```

--8<-- "includes/mur-trace-show-info.md"

--8<-- "includes/mur-trace-explore.md"

## Summary

| Feature / setting | How it works |
|---|---|
| `capabilities.containment: scoped` | Landlock and seccomp over the host filesystem; each command gets its own `/proc` and reads `/etc/hosts` and `/etc/nsswitch.conf` |
| `interpreter_runtime` on `npm` | Grants `npm`'s package tree, read and list |
| `interpreter_runtime` on `node` | Grants the system library folder native addons load from and OpenSSL's configuration file; fires `W-SEC-009` |
| `memory_bytes: 68719476736` | An address-space limit V8 runs under |
| `cgroup_memory_bytes` | Caps the memory the command tree uses |
| `npm ci --ignore-scripts`, `node node_modules/vitest/vitest.mjs run` | Install and test without executing a file from the workdir |
| `lifecycle.done_check` | Runs the install and the tests when the agent stops; `limit: 0` records the result without reopening the task |
