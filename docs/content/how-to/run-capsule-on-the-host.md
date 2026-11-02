# How to run a capsule on the host

A capsule's shell commands normally run contained: a home folder of their own, a stripped
environment and whatever the host's kernel lets the runtime withhold. When the capsule is your own
coding agent working on your own machine, you may want it to run the way a local coding agent
does — as you, with your home folder, your environment and `sudo`. Running it unconfined does
that, and keeps the trace, the spend ceilings, the turn limit and the policy hooks.

??? definition "What is the unconfined stance?"
    **Unconfined** is a stance only the operator chooses, with `mur run --unconfined` or
    `containment: unconfined` in `~/.murmur/config.yaml`. The capsule's shell commands, plan shell
    steps, native tools and process harness then run as your account, with nothing installed
    between them and the host.

The relevant manifest options are:

| Option | Controls |
|---|---|
| [capabilities.shell.allow](../reference/manifest.md#field-capabilities) | Which programs the model may start as a tool |
| [capabilities.containment](../reference/containment.md#field-containment) | The floor a capsule declares, which refuses the unconfined stance |
| [capabilities.resources](../reference/resource-limits.md) | The only resource limits an unconfined capsule's commands get |

---

## What running unconfined gives up

Read this before you choose it.

| Contained | Unconfined |
|---|---|
| `HOME` is the session's own home folder | `HOME` is yours, so `~/.ssh`, `~/.aws` and every dotfile are reachable |
| Credential-shaped variables are stripped | Your whole environment, credential variables included, reaches every command |
| The filesystem and network outside the grants are refused where the host can refuse them | The host filesystem and network are reachable as you reach them |
| `no_new_privs` stops `sudo` | `sudo` works wherever your sudoers allows it |
| Default resource limits apply | Only the limits `capabilities.resources` declares |
| The trace and the records directory are read-only to commands where the host can enforce it | Commands can change them |

A process a command moves out of its process group, with `setsid` for example, can outlive the
session. WASM artifacts keep their own boundary either way.

## Step 1 — create murmur.yaml

Create a `murmur.yaml` file for a coding agent that may run `bash`. It declares no
`capabilities.containment`: a declared floor refuses the unconfined stance.

```yaml
name: host-agent
version: "0.1.0"

artifacts:
  - name: murmur/driver-anthropic
    version: "{{ v.murmur_driver_anthropic }}"
    runtime: driver
    gateway:
      endpoint: https://api.anthropic.com
      api_key: ${ANTHROPIC_API_KEY}

capabilities:
  shell:
    allow:
      - bash

inference:
  transport: http
  model: {{ v.model_anthropic }}
  driver:
    artifact: murmur/driver-anthropic
  max_turns: 30
```

## Step 2 — install dependencies

```bash
mur install
```

--8<-- "includes/mur-pull-info.md"

## Step 3 — check what the capsule will reach

Ask `mur run` what an unconfined launch would run under, without launching it:

```bash
mur run --unconfined --explain-scope
```

```text
Containment
  declared:  unconfined
  achieved:  unconfined
  host can back: scoped
  floor met: yes
  mechanism: unconfined
  userns grant: shipped_profile
  landlock ABI: n/a
  cgroup scope: not-required — this session runs unconfined and capabilities.resources declares no cgroup field, so a launch creates no cgroup scope for it

Routes
  shell commands    present  unconfined
    Runs as the operator, with their HOME, whole environment, filesystem, network and sudo; nothing holds it.
  plan shell steps  absent   unconfined
  native tools      absent   unconfined
  WASM tools        absent   host process, not confined by the class
  drivers           present  host process, not confined by the class (local/murmur-driver-anthropic)
    Runs inside the mur process, held by its WASI preopens and the host-side network allowlist only.
  hooks             absent   host process, not confined by the class
  process harness   absent   unconfined
  harness tools     absent   host process, not confined by the class

Unconfined
  user:      ana
  uid:       1000
  HOME:      /home/ana
  applies to: shell commands, plan shell steps, native tools, process harness
  resource limits: none declared
```

`host can back` still says what this host could contain the capsule at, should you run it
contained.

## Step 4 — run it unconfined

Create a `task.md` file with the work, then run:

```bash
mur run --unconfined --task task.md
```

```text
[capsule-runtime] warning[W-SEC-036]: this capsule runs unconfined: its shell commands, plan shell steps, native tools and process harness run as ana (uid 1000) with HOME /home/ana, the whole environment mur run was started with (credential variables included), the host filesystem and network as that account has them, and no no_new_privs, so sudo works wherever that account's sudoers allows it (https://docs.murmur.nexus/reference/diagnostics/#w-sec-036)
murmur: url localhost:52222
session: ses_019ed2af53da75c2aefee84ee10c34af
status:  ok
```

A command such as `cargo install --path . && sudo -n systemctl restart my-svc` now runs as you.
murmur does not manage `sudo`: `sudo -n` succeeds only where your sudoers lets you run that command
without a password.

## Step 5 — make it your default

To run every capsule you launch unconfined, add the key to `~/.murmur/config.yaml`:

```yaml
containment: unconfined
```

`mur run` then needs no flag. To run one launch contained, name a floor:

```bash
mur run --containment advisory --task task.md
```

Only the global file chooses it. The same key in a project's `.murmur/config.yaml` is refused with
[E-CFG-005](../reference/diagnostics.md#e-cfg-005), so a repository you clone cannot run its own
capsule unconfined.

## Step 6 — confirm it in the trace

```bash
mur trace show
```

--8<-- "includes/mur-trace-show-info.md"

--8<-- "includes/mur-trace-explore.md"

The `containment:` line reads `unconfined → unconfined (commands ran as ana with HOME /home/ana)`.
`mur ps` names a running unconfined capsule on a line below its table.

## What stays contained

The stance never spreads to a capsule you did not launch unconfined yourself:

| Launch | Runs |
|---|---|
| A sub-capsule your capsule delegates to, or a plan `capsule` step | Contained |
| A capsule `mur deploy` starts | Contained |
| A member of `mur run --roster` | Contained, or unconfined with `mur run --roster --unconfined` |

A capsule that declares `capabilities.containment`, or a project whose config declares a floor, is
refused with [E-RUN-059](../reference/diagnostics.md#e-run-059): a declared floor is never lowered.

## Summary

| Feature / setting | How it works |
|---|---|
| `mur run --unconfined` | Runs this launch as you: your `HOME`, environment, filesystem, network and `sudo` |
| `containment: unconfined` in `~/.murmur/config.yaml` | Runs every launch you make unconfined; `--containment <class>` runs one contained |
| A declared floor | Refuses the unconfined stance with `E-RUN-059` |
| What stays on | The trace, spend ceilings, `inference.max_turns`, deadlines, policy hooks, the untrusted-content fence and WASM artifacts' boundaries |
| What never spreads | Delegated children, plan `capsule` steps and deployed capsules run contained |

See [Running unconfined](../reference/containment.md#unconfined) for every rule.
