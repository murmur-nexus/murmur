# How to run a Playwright suite in a capsule

A capsule that changes a web front end should prove its change by running the project's browser
tests, inside the same boundary as everything else it runs. This guide builds a `scoped` capsule
that runs a [Playwright](https://playwright.dev) suite against a headless Chrome on your host, with
only the grants Node and Chrome need: Chrome's own folder, the fonts it draws with, the system
library folder it loads modules from, the PCI device tree its GPU process reads, and an
address-space limit V8 can run under. The suite's commands go through the capsule's shell, so
Chrome runs as a [shell command's descendant](../reference/containment.md#wasm-tool-programs),
under the same containment as every other program the capsule runs.

The relevant manifest options are:

| Option | Controls |
|---|---|
| [capabilities.containment](../reference/manifest.md#field-capabilities) | The containment floor the capsule refuses to launch below |
| [capabilities.shell.allow](../reference/manifest.md#field-capabilities) | Which binaries the capsule's commands may run |
| [capabilities.shell.interpreter_runtime](../reference/manifest.md#field-capabilities) | Host folders an allowlisted binary reads and loads from outside the workdir |
| [capabilities.resources.memory_bytes](../reference/manifest.md#field-capabilities) | The address-space limit on each command |
| [capabilities.resources.cgroup_pids_max](../reference/manifest.md#field-capabilities) | How many processes the capsule's whole command tree may hold |
| [capabilities.resources.cgroup_memory_bytes](../reference/manifest.md#field-capabilities) | The memory the capsule's whole command tree may use |

---

## Step 1 — find the browser

Playwright's `chromium-headless-shell` is a single folder holding the `chrome-headless-shell`
binary and the libraries it loads beside it. Install it once on the host, outside the capsule:

```bash
npx playwright install chromium-headless-shell
```

The folder is `~/.cache/ms-playwright/chromium_headless_shell-<build>/chrome-headless-shell-linux64`.
The guide uses build `1208` and a home folder of `/home/you`; put your own path in each place it
appears.

`capabilities.shell.allow` names a binary by name, so the folder must be on the `PATH` that
`mur run` starts with:

```bash
export PATH="/home/you/.cache/ms-playwright/chromium_headless_shell-1208/chrome-headless-shell-linux64:$PATH"
```

## Step 2 — point Playwright at that browser

Create a `playwright.config.mjs` file next to your project's `package.json`:

```js
import { defineConfig } from '@playwright/test';

export default defineConfig({
  testDir: './tests',
  use: {
    launchOptions: {
      executablePath: '/home/you/.cache/ms-playwright/chromium_headless_shell-1208/chrome-headless-shell-linux64/chrome-headless-shell',
      args: ['--no-sandbox', '--disable-dev-shm-usage', '--disable-gpu'],
    },
  },
});
```

| Line | Why |
|---|---|
| `executablePath` | The binary the manifest grants. Playwright's own download location is not granted, and nothing downloads inside the capsule |
| `--no-sandbox` | A capsule command runs with `no_new_privs` set and no capabilities, which Chrome's own sandbox cannot start under; the capsule is the boundary |
| `--disable-dev-shm-usage` | A capsule command cannot write `/dev/shm`; Chrome keeps its shared memory in the temporary folder instead |
| `--disable-gpu` | The manifest grants no GPU device; Chrome renders in software |

## Step 3 — write the manifest

Create a `murmur.yaml` file next to `playwright.config.mjs`, with your browser folder in place of
the one below:

```yaml
name: playwright-tests
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
      - node
      - chrome-headless-shell
    interpreter_runtime:
      - binary: node
        dirs:
          - path: /etc/ssl/openssl.cnf
            list_dir: false
      - binary: chrome-headless-shell
        dirs:
          - path: /home/you/.cache/ms-playwright/chromium_headless_shell-1208/chrome-headless-shell-linux64
            list_dir: true
          - path: /proc
            list_dir: true
          - path: /etc/fonts
            list_dir: true
          - path: /usr/share/fonts
            list_dir: true
          - path: /usr/lib/x86_64-linux-gnu
            list_dir: false
          - path: /sys/bus/pci
            list_dir: true
          - path: /sys/devices
            list_dir: true
  resources:
    memory_bytes: 274877906944
    cgroup_pids_max: 1024
    cgroup_memory_bytes: 4294967296

inference:
  transport: http
  model: {{ v.model_openai }}
  driver:
    artifact: murmur/driver-openai
```

What each grant is for:

| Grant | Why |
|---|---|
| `node` in `shell.allow` | Runs Playwright's test runner, and Playwright starts Chrome from it |
| `chrome-headless-shell` in `shell.allow` | Chrome is executed by `node`, and every program a capsule command executes must be allowed, at any depth |
| `/etc/ssl/openssl.cnf` on `node` | `node` reads OpenSSL's configuration file at startup and exits with code 13 when it is refused; see [Native addons](../reference/containment.md#native-addons) |
| The browser folder on `chrome-headless-shell` | Chrome's data files, locales and the graphics libraries it loads from its own folder |
| `/etc/fonts` and `/usr/share/fonts` | The fonts Chrome draws pages with |
| `/usr/lib/x86_64-linux-gnu` | Chrome loads its NSS crypto modules from the system library folder with `dlopen`, so they are not among the libraries the binary names; without them an `https` page stops Chrome. This grant carries execute and draws [`W-SEC-009`](../reference/diagnostics.md#w-sec-009) |
| `/sys/bus/pci` and `/sys/devices` | With the library folder granted, Chrome's GPU process loads `libpci`, which reads the PCI device tree and exits the process when it is refused; see [A library a grant brings in reach](../reference/containment.md#library-grant-reach) |
| `memory_bytes: 274877906944` | `memory_bytes` limits address space, and V8 reserves tens of GiB of it in every Chrome and Node process |
| `cgroup_pids_max` and `cgroup_memory_bytes` | The real caps on the command tree: Chrome runs dozens of processes per page |

Without the two `/sys` grants, Chrome starts and stops at once, and Playwright reports:

```text
Error: browserContext.newPage: Target page, context or browser has been closed
```

with Chrome's log naming the cause:

```text
pcilib: Cannot open /sys/bus/pci/devices
ERROR:content/browser/gpu/gpu_process_host.cc] GPU process exited unexpectedly: exit_code=256
FATAL:content/browser/gpu/gpu_data_manager_impl_private.cc] GPU process isn't usable. Goodbye.
```

## Step 4 — check the manifest

```bash
mur install
```

--8<-- "includes/mur-pull-info.md"

```bash
mur doctor
```

`mur doctor` checks the manifest against this host before any model call.

## Step 5 — run the suite the way `scoped` allows

`scoped` does not run files from the workdir, and every file under `node_modules` is in the workdir.
Run Playwright's test runner through `node` rather than through `npx playwright` or
`node_modules/.bin/playwright`, each of which executes a file under `node_modules`:

```bash
node node_modules/@playwright/test/cli.js test
```

Install the project's dependencies on the host before the run, or with `npm ci --ignore-scripts`
inside the capsule as [How to run Node tests in a capsule](run-node-tests-in-capsule.md) shows.

Create a `task.md` file asking for the change you want and naming that command as the way to
check it, then run the capsule with the project folder as the workdir:

```bash
mur run --workdir . --task task.md
```

```
murmur: url localhost:52222
session: ses_019ed2af53da75c2aefee84ee10c34af
status:  ok
```

```bash
mur trace show
```

--8<-- "includes/mur-trace-show-info.md"

--8<-- "includes/mur-trace-explore.md"

## Summary

| Feature / setting | How it works |
|---|---|
| `capabilities.containment: scoped` | Landlock and seccomp over the host filesystem; Chrome runs under the same containment as the shell command that started it |
| `executablePath` in `playwright.config.mjs` | Starts the granted `chrome-headless-shell` instead of a Playwright download |
| `--no-sandbox`, `--disable-dev-shm-usage`, `--disable-gpu` | Start Chrome without its own sandbox, without `/dev/shm` and without a GPU device |
| `interpreter_runtime` on `chrome-headless-shell` | Grants the browser folder, the fonts, the system library folder and the PCI device tree |
| `interpreter_runtime` on `node` | Grants OpenSSL's configuration file |
| `memory_bytes`, `cgroup_pids_max`, `cgroup_memory_bytes` | An address-space limit V8 runs under, and caps on the whole command tree |
| `node node_modules/@playwright/test/cli.js test` | Runs the suite without executing a file from the workdir |
