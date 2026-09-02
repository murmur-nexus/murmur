# How to lock down a capsule's capabilities

An agent that runs with free rein over the host is one bug or one prompt injection away from unintended consequences — reading files it shouldn't, reaching hosts it shouldn't, or acting on attacker-influenced content. A production agent should instead run inside a capsule with precise scope grants, so it can carry out any use case within a boundary you control. Securing a capsule means granting the minimum each part needs — a capsule-wide ceiling of hosts, filesystem scope, and shell binaries. Optionally, you can narrow individual artifacts below that ceiling to further enforce access control.

This guide uses a **coding capsule** — an agent that clones a repository, edits files, and runs builds and tests through a shell — to explore those scope grants. That work needs real authority: shell access and a broad view of the filesystem. Granting it bluntly, with `bash` and an unrestricted filesystem and network, gets the job done, but it is exactly the free-rein posture above. The guide locks the capsule down step by step while keeping it able to do its job.

??? definition "What is the capsule ceiling?"
    The **ceiling** is the capsule-wide top-level `capabilities:` block in `murmur.yaml`. It is the outer security boundary: the full set of hosts every artifact may reach, the filesystem scope every artifact may see, and the shell binaries the agent may invoke. A per-artifact grant can only ever *narrow* below it — subtract authority within the ceiling, never widen past it. The ceiling always wins.

The relevant manifest options are:

| Option | Controls |
|---|---|
| [capabilities.network.allow](../reference/manifest.md#field-capabilities) | The capsule-wide network ceiling every artifact clamps to |
| [capabilities.filesystem.scope](../reference/manifest.md#field-capabilities) | The capsule-wide filesystem scope declaration |
| [capabilities.shell.allow](../reference/manifest.md#field-capabilities) | Which binaries the agent may invoke as shell tools |
| [capabilities.shell.strip_env](../reference/manifest.md#field-capabilities) | Glob patterns for host env vars to remove from the subprocess environment |
| [capabilities.shell.baseline_env](../reference/manifest.md#field-capabilities) | Additional host env vars to expose beyond the default baseline |
| [artifacts[].capabilities](../reference/manifest.md#tool-capabilities) | Optional per-artifact grant that narrows one tool or driver below the ceiling |
| [artifacts[].capabilities.network.allow](../reference/manifest.md#tool-capabilities) | Hosts one artifact may reach, intersected with the ceiling |
| [artifacts[].capabilities.filesystem.scope](../reference/manifest.md#tool-capabilities) | Workdir subtree one artifact reaches instead of the whole workdir |

---

## The starting point: an unrestricted coding capsule

Create a `murmur.yaml` file. This version does the job — the agent has `bash`, so it can clone, edit, build, and test — but it grants that authority bluntly:

=== "Anthropic"

    ```yaml
    name: coding-agent
    version: "0.1.0"

    artifacts:
      - name: murmur-driver-anthropic
        version: "{{ v.murmur_driver_anthropic }}"
        runtime: driver

    capabilities:
      network:
        allow:
          - https://api.anthropic.com
      shell:
        allow:
          - bash

    inference:
      transport: http
      endpoint: https://api.anthropic.com
      model: {{ v.model_anthropic }}
      api_key: ${ANTHROPIC_API_KEY}
      driver:
        artifact: murmur-driver-anthropic
    ```

=== "OpenAI"

    ```yaml
    name: coding-agent
    version: "0.1.0"

    artifacts:
      - name: murmur-driver-openai
        version: "{{ v.murmur_driver_openai }}"
        runtime: driver

    capabilities:
      network:
        allow:
          - https://api.openai.com
      shell:
        allow:
          - bash

    inference:
      transport: http
      endpoint: https://api.openai.com
      model: {{ v.model_openai }}
      api_key: ${OPENAI_API_KEY}
      driver:
        artifact: murmur-driver-openai
    ```

=== "DeepSeek"

    ```yaml
    name: coding-agent
    version: "0.1.0"

    artifacts:
      - name: murmur-driver-deepseek
        version: "{{ v.murmur_driver_deepseek }}"
        runtime: driver

    capabilities:
      network:
        allow:
          - https://api.deepseek.com
      shell:
        allow:
          - bash

    inference:
      transport: http
      endpoint: https://api.deepseek.com
      model: {{ v.model_deepseek }}
      api_key: ${DEEPSEEK_API_KEY}
      driver:
        artifact: murmur-driver-deepseek
    ```

There is no `filesystem.scope`, so the capsule works out of the whole session workdir, and `bash` can run any command against the entire machine. The rest of this guide applies specific capabilities to constrain what the agent can reach — without taking away its ability to do the coding task.

??? info "What are binaries?"
    A **binary** is a standalone executable program on the filesystem that your shell can invoke by name when it appears on the system `PATH`. They differ from shell built-ins (like `cd`) that only exist inside the shell process itself and cannot be called as subprocesses.

    Common examples by category:

    - **Version Control & Repo Management:** `git`, `gh` (GitHub CLI)
    - **Environment & Scripting:** `python3`, `node`
    - **File & Directory Navigation:** `ls`, `pwd`, `find`, `grep`
    - **File Manipulation:** `cat`, `echo`, `rm`, `mv`, `cp`, `chmod`, `chown`
    - **Network Operations:** `curl`, `wget`
    - **System Information:** `whoami`, `uname`, `hostname`

Each entry in `shell.allow` is a **bare binary name** — `bash`, `jq`, `python3`. Paths are not accepted, and the binary must exist on the host's `PATH`.

---

## Step 1 — set the capsule's capability ceiling

The top-level `capabilities:` block is the ceiling. Add a `filesystem.scope` so the capsule works out of the repo subtree of the session workdir rather than the whole workdir, and keep `network.allow` scoped to the single host the runtime needs for inference:

```yaml
capabilities:
  network:
    allow:
      - https://api.anthropic.com   # your provider's inference endpoint
  filesystem:
    scope: repo
  shell:
    allow:
      - bash
```

`filesystem.scope` is a path relative to the session workdir — here `<workdir>/repo`, created on demand, and the subtree the coding agent checks the repository out into.

WASM tools, drivers and hooks do not inherit it. Each of those works out of the directory its own entry's `capabilities.filesystem.scope` names — Step 4 declares them — and a tool or driver entry that names none works out of the whole accessible workdir. Read what each one resolves to with `mur run --explain-scope`, and see [The filesystem default](../concepts/access-control.md#filesystem-default).

!!! warning "`bash` still reaches the whole machine on this platform"
    The ceiling's `filesystem.scope` and `network.allow` are only a real boundary for a `bash` subprocess when a kernel sandbox enforces them, and whether one does is a property of the host — see [Subprocess enforcement tiers](../reference/containment.md#subprocess-enforcement-tiers) for which hosts enforce what. Where nothing enforces them, treat these settings as advisory for `bash`: it can read and write files outside `repo` and open connections to hosts outside `network.allow` regardless of what the manifest declares ([`W-SEC-001`](../reference/diagnostics.md#w-sec-001), [`W-SEC-003`](../reference/diagnostics.md#w-sec-003)). Pairing `bash` with any external-fetch capability is the [maximum-risk combination](../concepts/access-control.md#threat-model) the threat model describes — Step 4 shows how to move work into artifacts whose scope *is* enforced on every platform.

### Prefer specific binaries over a full shell

`bash` is a full interpreter: it can run any command, chain and pipe them, and open its own connections. Granting it hands the model the widest possible shell surface. If the coding task can be expressed as calls to specific programs, list those instead — each listed binary becomes its own model-visible tool, and the arbitrary-command surface of a general shell is gone:

```yaml
capabilities:
  shell:
    allow:
      - git
      - python3
      - jq
```

The agent can now run `git`, `python3`, and `jq` but has no general shell. Any bare binary name on the host `PATH` is accepted; there is no fixed list to choose from. This also removes the `bash`-specific network-bypass warning, which matches the literal string `bash` — declaring `sh` or `zsh` instead would silence [`W-SEC-003`](../reference/diagnostics.md#w-sec-003) without reducing the risk, so swapping one shell for another is not the point. Dropping the general shell is.

!!! warning "A narrower binary is still an arbitrary-execution vector"
    Listing specific binaries shrinks the *tool surface the model sees*, not what a running binary may do. A general-purpose program can still execute other programs — `git -c core.sshCommand=...`, `python3 -c ...`, and `find -exec` all run arbitrary commands. The kernel exec-allowlist that confines a granted binary to executing only other allowlisted binaries is Linux-only ([`W-SEC-005`](../reference/diagnostics.md#w-sec-005)), so on a typical host this narrowing is least-privilege *intent*, not a containment boundary. Declare the narrowest set the task genuinely needs, and push anything you can into the scoped tool artifacts of Step 4, whose filesystem and network scope is enforced on every platform.

---

## Step 2 — manage the subprocess environment

The subprocess does not inherit your full host environment. It starts with a minimal baseline: `PATH`, `HOME`, `USER`, `LANG`, `LC_ALL`, `TMPDIR`, `TEMP`, `TMP`, `CARGO_HOME`, `RUSTUP_HOME`, and `TERM`. Known credential-shaped variables are always stripped before the subprocess spawns, regardless of any other configuration — this includes exact names (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GOOGLE_API_KEY`, `HUGGING_FACE_HUB_TOKEN`, `NEXUS_API_KEY`, `GITHUB_TOKEN`, `GH_TOKEN`, `KUBECONFIG`, `NPM_TOKEN`, `PYPI_TOKEN`, `CARGO_REGISTRY_TOKEN`) and glob patterns (`AWS_*`, `DOCKER_*`, `*_API_KEY`).

!!! info "HOME is always synthetic"
    `HOME` (and, on Windows, `USERPROFILE`) is never the real host home directory. The runtime always replaces it with a session-scoped directory (`<workdir>/.capsule-home`, created on demand) before the subprocess spawns. Neither `baseline_env` nor any environment override the agent supplies can restore the real host value — the synthetic path always wins.

!!! info "Native tool artifacts get the same treatment"
    Everything in this step — the synthetic `HOME`, the credential-pattern strip list, `strip_env`, and `baseline_env` — also applies to `runtime: tool` artifacts declared with `implementation: native` (see [Python Tool Quickstart (Native)](../language-guides/python-native.md)), even if the capsule declares no `capabilities.shell.allow` entries at all. Both subprocess spawn paths build their environment through the same internal function, so there's no separate configuration surface for native tools.

Use `strip_env` to remove baseline variables the agent doesn't need:

```yaml
capabilities:
  shell:
    allow:
      - bash
    strip_env:
      - CARGO_HOME
      - RUSTUP_HOME
```

Each entry is an exact variable name or a glob with the `*` at the start, the end, or both:

```yaml
strip_env:
  - AWS_*        # matches AWS_DEFAULT_REGION, AWS_PROFILE, and any other AWS_-prefixed var
  - MY_CORP_*    # matches any var starting with MY_CORP_
  - "*_TOKEN"    # matches any var ending in _TOKEN
  - "*SECRET*"   # matches any var with SECRET anywhere in the name
```

---

## Step 3 — add host variables beyond the default baseline

The default baseline only includes variables the runtime considers universally safe. Any host variable outside that fixed set — a database URL, an API endpoint, an application config path — is excluded unless you explicitly add it.

??? info "Default baseline variables"
    | Variable | Purpose |
    |---|---|
    | `PATH` | Locates executables on the host |
    | `HOME` | Session-scoped synthetic directory, not the real host home (see above) |
    | `USER` | Current username |
    | `LANG` | System locale |
    | `LC_ALL` | Locale override for all categories |
    | `TMPDIR` | Preferred temporary directory (macOS/BSD) |
    | `TEMP` | Preferred temporary directory (Windows) |
    | `TMP` | Fallback temporary directory |
    | `CARGO_HOME` | Rust package cache directory |
    | `RUSTUP_HOME` | Rust toolchain installation directory |
    | `TERM` | Terminal type identifier |

Use `baseline_env` to bring additional host variables into the subprocess environment:

```yaml
capabilities:
  shell:
    allow:
      - bash
    baseline_env:
      - DATABASE_URL
      - APP_CONFIG_PATH
```

The runtime builds the final subprocess environment in order: it starts from the default baseline, appends any variables listed in `baseline_env`, then applies `strip_env` removals (chained with the built-in credential patterns), and finally sets `HOME`/`USERPROFILE` to the synthetic session directory. This means `strip_env` always wins over `baseline_env` for everything except `HOME`/`USERPROFILE`, which are fixed last and can't be reintroduced by either setting — a variable that appears in both lists is not passed to the subprocess:

```yaml
capabilities:
  shell:
    allow:
      - bash
    baseline_env:
      - DATABASE_URL
    strip_env:
      - CARGO_HOME
      - RUSTUP_HOME
```

This example adds `DATABASE_URL` from the host and removes `CARGO_HOME` and `RUSTUP_HOME` from the default baseline.

---

## Step 4 — narrow individual tools and drivers below the ceiling

The strongest way to constrain a coding agent is to do less through `bash` and more through scoped tool artifacts. A per-artifact `filesystem.scope` on a WASM tool is a real directory grant, enforced on every platform, macOS included, where `bash`'s own confinement is only advisory. The capsule's containment class leaves that grant unchanged — see [What bounds a WASM artifact](../reference/containment.md#artifact-boundary). Moving a task out of `bash` and into a scoped tool turns an advisory boundary into an enforced one.

Add the WASM tools this agent uses to the `artifacts:` list, add the one extra host they need to the ceiling, and scope each artifact to only its slice:

```yaml
artifacts:
  - name: murmur-driver-anthropic
    version: "{{ v.murmur_driver_anthropic }}"
    runtime: driver
    capabilities:
      network:
        allow:
          - https://api.anthropic.com
  - name: murmur-tool-git
    version: "{{ v.murmur_tool_git }}"
    runtime: tool
    capabilities:
      network:
        allow:
          - https://github.com
  - name: murmur-tool-editor
    version: "{{ v.murmur_tool_editor }}"
    runtime: tool
    capabilities:
      network:
        allow: []
      filesystem:
        scope: repo

capabilities:
  network:
    allow:
      - https://api.anthropic.com
      - https://github.com
  filesystem:
    scope: repo
  shell:
    allow:
      - bash
```

The effective grant is the intersection of what an artifact declares and the ceiling — it can only ever subtract. Each block does one job:

- **The driver** reaches only `api.anthropic.com`, dropping `github.com` from the ceiling. A driver narrows through the same path as any WASM tool, so this also applies to a driver call a hook's `run-inference` makes.
- **The git tool** reaches only `github.com`, dropping the inference endpoint it never calls.
- **The editor tool** gets `network.allow: []` — a real narrowing to zero outbound HTTP, distinct from omitting the key — and `filesystem.scope: repo`, which gives it only `<workdir>/repo` as its current directory. An absolute path, or one that escapes via `..`, fails at staging (`E-CAP-002`) before the tool runs.

An artifact with **no** `capabilities:` block inherits the full ceiling. Grants are read only from your capsule manifest's artifact entry, never from the artifact's own bundled `murmur.yaml`, so an untrusted artifact cannot scope itself up.

!!! warning "Write the narrowing at least as specific as the ceiling entry"
    A bare host like `github.com` spans both schemes and every port, so it is *broader* than a `https://github.com` ceiling entry and does **not** fit under it. The runtime drops the uncovered entry and prints a [`W-SEC-007`](../reference/diagnostics.md#w-sec-007) warning naming the artifact and the dropped entry — the artifact ends up with less access than asked for, never more. Match the ceiling entry exactly. This is the most common way to trip the warning by accident.

---

## Step 5 — install and run

```bash
mur install
```

--8<-- "includes/mur-pull-info.md"

Create a `task.md` file describing the coding task:

```markdown
Clone the repository, run the test suite, and report any failures.
```

Run the capsule:

```
mur run --task task.md
murmur: url localhost:52222
session: ses_019ed2af53da75c2aefee84ee10c34af
status:  ok
```

Check `workdir/<session_id>/MURMUR.md`. The **Running Shell Commands** section lists the binaries the agent has access to:

```text
## Running Shell Commands

Declared shell binaries: bash
Call them via the shell-execution interface or directly from bash if `bash` is in the allowlist.
```

Each shell invocation the agent makes appears in `workdir/<session_id>/trace.jsonl` as a `shell` event:

```json
{"event_type":"shell","turn":0,"command":"git status","exit_code":0,"stdout_bytes":124,"stderr_bytes":0,"duration_ms":12}
```

---

## Step 6 — watch for warnings and keep secrets out of the manifest

Per-artifact grants resolve at staging, before the session workdir exists, so any capability warning goes to stderr as the run starts. Two are worth knowing:

- **`W-SEC-007`** — a per-artifact `network.allow` entry the ceiling does not itself cover was dropped, not granted. Nothing is ever widened, but a tool that silently cannot reach a host it looks "granted" is hard to trace back to the manifest. See [W-SEC-007](../reference/diagnostics.md#w-sec-007).
- **`W-SEC-008`** — a per-artifact block declared `shell`, `spawn`, `env`, `limits`, `resources` or `containment`, or sat on a tool with a native (non-WASM) implementation. Only `network` and `filesystem` narrow; the rest parse but are inert. Scope a native tool through the capsule-wide `capabilities.shell.*` block instead, or ship it as WASM if you need per-artifact narrowing. See [W-SEC-008](../reference/diagnostics.md#w-sec-008).

Keep credentials out of the manifest itself. Reference them by environment variable — `api_key: ${ANTHROPIC_API_KEY}`, as every example above does — never as a literal string. A literal secret prints a [`W-SEC-004`](../reference/diagnostics.md#w-sec-004) warning and risks leaking into version control.

To confirm what each artifact actually did during a run, inspect the trace:

```bash
mur trace show
```

--8<-- "includes/mur-trace-show-info.md"

--8<-- "includes/mur-trace-explore.md"

---

## Summary

| Manifest setting | Effect |
|---|---|
| `capabilities.network.allow` (top-level) | The ceiling; every artifact clamps to it and none can widen past it |
| `capabilities.filesystem.scope` (top-level) | The workdir subtree the capsule works out of; enforced for `bash` only under a verified kernel sandbox |
| `shell.allow: [bash]` | Exposes `bash` as a model-visible tool; unconfined on hosts without kernel enforcement |
| `shell.allow: [git, python3, jq]` | Grants specific binaries instead of a full shell; shrinks the model's tool surface, though each remains an arbitrary-execution vector |
| `shell.strip_env: [CARGO_HOME]` | Removes a specific variable from the default baseline |
| `shell.strip_env: [AWS_*]` | Removes all vars whose name starts with `AWS_` |
| `shell.baseline_env: [DATABASE_URL]` | Exposes an additional host variable not in the default baseline |
| `HOME` / `USERPROFILE` | Always the synthetic session directory; cannot be overridden by `baseline_env` or a tool-supplied env value |
| `artifacts[].capabilities` present | Narrows that one tool or driver; effective grant = declaration ∩ ceiling |
| `artifacts[].capabilities` absent | Artifact inherits the full ceiling |
| `network.allow: []` on an artifact | Denies that one artifact all outbound HTTP; siblings keep theirs |
| `filesystem.scope` on a WASM artifact | A real directory grant, enforced on every platform including macOS |
| Bare host under a scheme-bound ceiling | Dropped with `W-SEC-007`; write the entry as specifically as the ceiling |
| `shell`/`spawn`/`env`/`limits` in a per-artifact block | Parsed but inert; prints `W-SEC-008` |
| `api_key: ${ENV_VAR}` | Keeps secrets out of the manifest; a literal triggers `W-SEC-004` |
| Non-zero exit code | Passed back to the model as data; the session continues |
