# How to deploy a capsule to the cloud

!!! warning "Beta feature"

    `mur deploy` is a beta feature and is hidden by default. Enable it first with
    `mur beta enable deploy`. The same flag also enables its teardown counterpart,
    `mur destroy` (used below) — both are gated together. Behavior and flags may
    change in future releases.

`mur deploy` uploads your capsule files to an existing VM via SSH, starts the capsule, and returns a public A2A endpoint. You create and manage the VM through your cloud provider's dashboard — `mur deploy` never provisions or terminates VMs on your behalf.

The relevant manifest options are:

| Option | Controls |
|---|---|
| [mur_version](../reference/manifest-schema.md#field-mur-version) | Pins the exact `mur` binary version installed on the VM |
| [inference.driver.artifact](../reference/manifest-schema.md#field-inference) | Inference driver — required for agent capsules |
| [inference.system_prompt_file](../reference/manifest-schema.md#inference-system-prompt) | Local file uploaded alongside the manifest as the system prompt |
| [lifecycle.task_acceptance](../reference/manifest-schema.md#lifecycle-task-acceptance) | Whether the deployed capsule accepts incoming A2A tasks |
| [lifecycle.after_task](../reference/manifest-schema.md#lifecycle-after-task) | What the capsule does after a task completes |

---

## Prerequisites

- A working agent capsule that runs locally with `mur run` (an `inference` block is required)
- A VM you have created through your cloud provider, with SSH access and a public IP address

---

## Step 1 — create a VM

Log in to your cloud provider's control panel and create a new Linux VM. Supported providers include Hostinger, Hetzner, DigitalOcean, AWS, and any other provider that gives you SSH access and a public IP.

Choose **Ubuntu 22.04 LTS or 24.04 LTS** as the operating system. All major providers offer it. Other distributions (Debian, Rocky Linux, AlmaLinux, Fedora Cloud) also work. Alpine Linux is only compatible if you build `mur` with the `x86_64-unknown-linux-musl` target.

Make a note of the VM's public IP address before continuing.

---

## Step 2 — pin the mur version (recommended)

Add a `mur_version` field to your `murmur.yaml`:

```yaml
name: my-agent
version: "0.1.0"
mur_version: "X.Y.Z"

artifacts:
  - name: murmur-driver-anthropic
    version: "{{ v.murmur_driver_anthropic }}"
    runtime: driver

capabilities:
  network:
    allow:
      - https://api.anthropic.com

inference:
  transport: http
  endpoint: https://api.anthropic.com
  model: {{ v.model_anthropic }}
  api_key: ${ANTHROPIC_API_KEY}
  driver:
    artifact: murmur-driver-anthropic
```

When `mur_version` is set, `mur deploy` downloads that exact binary from GitHub releases and caches it at `~/.murmur/bin/mur-{version}-linux-x86_64`. Subsequent deploys with the same version skip the download entirely.

If `mur_version` is omitted, the version of the `mur` binary currently running on your machine is used instead.

---

## Step 3 — deploy

```bash
mur deploy \
  --host 1.2.3.4 \
  --manifest ./murmur.yaml \
  --env ANTHROPIC_API_KEY=sk-ant-...
```

`mur deploy` shows every step upfront as pending `·` before any work starts.
Steps use a two-level hierarchy: the group header at level 1, individual items indented at level 2.
When a group finishes, the level-2 items collapse — only the checkmarked summary stays.

Initial state (everything pending):

```text
  · ↓ 3 artifacts  2↓  1 cached
      · ↓ murmur-driver-anthropic@0.4.1
      · ↓ murmur-hook-compact@0.3.1
      · ↓ murmur-tool-editor@0.4.4
  · mur vX.Y.Z
  · SSH
  · ↑ mur binary
  · ↑ files
  · ↑ 3 artifacts
      · ↑ murmur-driver-anthropic@0.4.1
      · ↑ murmur-hook-compact@0.3.1
      · ↑ murmur-tool-editor@0.4.4
  · start capsule
```

During downloads (parallel, bytes / total bar per artifact being fetched):

```text
  ⠸ ↓ 3 artifacts
      ✓ ↓ murmur-driver-anthropic@0.4.1  cached
      ⠸ ↓ murmur-hook-compact@0.3.1  2/2 MB  [████████████]
      ⠸ ↓ murmur-tool-editor@0.4.4   7/14 MB [██████░░░░░░]
  · mur vX.Y.Z
  ...
```

After each group completes, level-2 items collapse. SSH disappears when the first upload starts:

```text
  ✓ ↓ 3 artifacts  16 MB  1 cached
  ✓ ↓ mur vX.Y.Z  cached
  ✓ ↑ mur binary  59 MB
  ✓ ↑ murmur.yaml · instructions.md
  ✓ ↑ 3 artifacts  21 MB
  ✓ → capsule  started · port 9000

  ┌──────────────────────────────────────────┐
  │  ∞  my-agent                            │
  │                                          │
  │  url   http://1.2.3.4:9000            │
  │  job   job_01954a3b                    │
  │  time  47s                             │
  └──────────────────────────────────────────┘
```

All output goes to **stderr**. Save the `job_id` from the box — you will need it for `mur ps` and `mur destroy`.

### All flags

| Flag | Default | Description |
|---|---|---|
| `--host` | *(required)* | VM public IP address or hostname |
| `--manifest` | `./murmur.yaml` | Path to the capsule manifest |
| `--ssh-user` | `root` | SSH username on the VM |
| `--ssh-key` | *(SSH agent)* | Path to SSH private key; falls back to SSH agent / `~/.ssh/id_*` |
| `--mur-binary` | *(auto-download)* | Path to a pre-built Linux x86_64 `mur` binary; skips the GitHub download |
| `--env` | *(none)* | `KEY=VALUE` environment variable for `mur run` on the VM; repeat for multiple |
| `--env-file` | *(auto-detect)* | Path to a `.env` file; `KEY=VALUE` or `export KEY=VALUE`; `#` comments ignored. If neither `--env` nor `--env-file` is passed, `.env` next to the manifest is loaded automatically |
| `--workdir` | *(none)* | Local directory to upload as the capsule working directory |
| `--deploy-platform` | `linux-x86_64` | Target platform for artifact resolution and staging |

### What `mur deploy` does

1. Parses the manifest and resolves every declared artifact for the target platform — fails immediately if any artifact is unavailable, before touching the network
2. Checks that every file referenced by the manifest (e.g. `inference.system_prompt_file`) exists locally — fails immediately if any file is missing
3. Validates every `--env` entry; a missing `=` or empty key causes an immediate exit
4. Resolves the `mur` binary: uses `manifest.mur_version` if set, otherwise the running version; downloads and caches if not already cached
5. Waits up to 30 seconds for SSH to become available on the VM
6. Uploads the `mur` binary to `/usr/local/bin/mur`
7. Uploads the manifest, any manifest-referenced local files, and the optional workdir
8. Uploads all pre-staged artifacts to `/root/.murmur/artifacts/`
9. Writes env vars to `/root/mur-{short_id}/.env` with mode `600`; env vars come from `--env`, `--env-file`, or `.env` next to the manifest (auto-detected); skipped when none are set
10. Sources `/root/mur-{short_id}/.env` (if present) and starts `mur run --manifest <path> --json`; waits up to 120 seconds for startup
11. Appends the deployment record to `~/.murmur/deployments.json`

Steps 1–4 all run locally before any SSH connection. A bad manifest, missing artifact, missing file, or malformed `--env` entry causes `mur deploy` to exit with a clear error message and no VM interaction.

---

## Step 4 — verify the agent card

Once `mur deploy` returns, the capsule's A2A endpoint is publicly reachable. Confirm it is alive by fetching the agent card:

```bash
curl http://1.2.3.4:9000/.well-known/agent-card.json
```

A 200 response confirms the capsule is running:

```json
{
  "name": "my-agent",
  "version": "0.1.0",
  "url": "1.2.3.4:9000",
  "capabilities": {
    "tools": ["bash"],
    "shell": true,
    "network": false,
    "streaming": true
  }
}
```

---

## Step 5 — send a task

The deployed capsule accepts standard A2A messages at its public URL. Send a task the same way you would to a local capsule:

```bash
curl -s -X POST http://1.2.3.4:9000 \
  -H "Content-Type: application/json" \
  -d '{
    "jsonrpc": "2.0",
    "id": 1,
    "method": "message/send",
    "params": {
      "message": {
        "messageId": "msg-001",
        "role": "user",
        "parts": [{"text": "Hello from the cloud."}]
      }
    }
  }'
```

See [Connect two capsules with A2A messaging](./capsules-a2a-messaging.md) for the full message protocol.

For a capsule to accept incoming tasks it must have `lifecycle.task_acceptance: single` or `queue` in its manifest. Without a lifecycle block the capsule accepts no incoming tasks after its initial run.

---

## Step 6 — list deployments

```bash
mur ps
```

```text
JOB_ID                                  PROVIDER    IP            STATUS      URL
----------------------------------------------------------------------------------------------------
job_01954a3b5c7d8e9f0a1b2c3d4e5f6a7b8c  manual      1.2.3.4       running     http://1.2.3.4:9000
```

All active deployments are stored in `~/.murmur/deployments.json`. Each row maps to one entry.

---

## Step 7 — remove from the deployment list

When you no longer need a deployment, remove it from the tracking list:

```bash
mur destroy job_01954a3b5c7d8e9f0a1b2c3d4e5f6a7b8c
```

This removes the entry from `~/.murmur/deployments.json`. It does not stop or delete the VM — shut down the VM from your cloud provider's dashboard separately.

---

## Reference: using a system prompt file

If your capsule reads its system prompt from a file rather than inline YAML, set `inference.system_prompt_file` in your manifest:

```yaml
inference:
  transport: http
  endpoint: https://api.anthropic.com
  model: {{ v.model_anthropic }}
  api_key: ${ANTHROPIC_API_KEY}
  system_prompt_file: instructions.md
  driver:
    artifact: murmur-driver-anthropic
```

`mur deploy` automatically detects `instructions.md` as a referenced local file, verifies it exists before attempting any SSH connection, and uploads it alongside the manifest to the remote deploy directory. No additional flags required.

---

## Reference: passing environment variables

Use `--env KEY=VALUE` to inject runtime secrets — API keys, tokens, database URLs — into the capsule's environment on the VM. Pass one flag per variable:

```bash
mur deploy \
  --host 1.2.3.4 \
  --manifest ./murmur.yaml \
  --env ANTHROPIC_API_KEY=sk-ant-... \
  --env DATABASE_URL=postgres://user:pass@host/db
```

Or point to a `.env` file with `--env-file`:

```bash
mur deploy --host 1.2.3.4 --env-file ./secrets.env
```

If neither `--env` nor `--env-file` is provided, `mur deploy` looks for a `.env` file in the same directory as the manifest and loads it automatically when found. This means running `mur deploy --host 1.2.3.4` from a project folder that has a `.env` file works with no extra flags.

Both `KEY=VALUE` and `export KEY=VALUE` formats are accepted; the `export ` prefix is stripped automatically.

In the manifest, reference variables with `${VAR_NAME}`:

```yaml
inference:
  transport: http
  endpoint: https://api.anthropic.com
  model: {{ v.model_anthropic }}
  api_key: ${ANTHROPIC_API_KEY}
  driver:
    artifact: murmur-driver-anthropic
```

When the capsule starts, `mur run` reads `ANTHROPIC_API_KEY` from its environment and substitutes it wherever `${ANTHROPIC_API_KEY}` appears in the manifest. If the variable is not set, `mur run` exits with `E-MAN-003`.

`mur deploy` writes all `--env` values to `/root/mur-{short_id}/.env` on the VM with mode `600` and sources that file before starting `mur run`. Values are transferred using base64 encoding, so values that contain spaces, equals signs, newlines, or shell metacharacters are preserved exactly. When no `--env` flags are passed, no `.env` file is written.

Every `--env` entry is validated before any SSH connection is made. An entry with no `=` or an empty key causes `mur deploy` to exit immediately with an error.

---

## Reference: using a pre-built binary

If you have already built a Linux `mur` binary — for example, on a CI server — pass it directly instead of letting `mur deploy` download one:

```bash
mur deploy \
  --host 1.2.3.4 \
  --manifest ./murmur.yaml \
  --mur-binary ./target/x86_64-unknown-linux-musl/release/mur
```

To build the musl binary locally:

```bash
rustup target add x86_64-unknown-linux-musl
CARGO_PROFILE_DEV_DEBUG=0 cargo build --release \
  --target x86_64-unknown-linux-musl \
  -p murmur-cli
```

!!! note "musl vs glibc"
    `x86_64-unknown-linux-musl` produces a fully static binary that runs on every Linux distribution including Alpine. `x86_64-unknown-linux-gnu` (glibc) works on Ubuntu, Debian, Rocky, and AlmaLinux, but fails on Alpine.

---

## Summary

| Feature | Detail |
|---|---|
| Command | `mur deploy --host <IP> --manifest <path>` |
| Progress | All steps shown upfront as pending; each activates in turn; artifact downloads show bytes/total bar; artifact uploads run in parallel (stderr) |
| Output | Success summary box on **stderr** showing url, job id (`job_` prefix + 8 hex chars), and elapsed time |
| VM management | You create and delete VMs through your cloud provider; `mur deploy` never provisions or terminates VMs |
| Recommended OS | Ubuntu 22.04 LTS or 24.04 LTS |
| mur binary | Auto-downloaded from GitHub releases; pin with `mur_version` in the manifest |
| Manifest files | `inference.system_prompt_file` and any other referenced local files are uploaded automatically |
| Env vars | `--env KEY=VALUE` (repeatable) · `--env-file <path>` · auto-loads `.env` next to the manifest if neither flag is passed |
| Artifact pre-staging | All manifest artifacts are resolved and uploaded before the capsule starts |
| Timeout | 30s SSH wait + 120s capsule startup wait |
| State file | `~/.murmur/deployments.json` — appended on deploy, entry removed on `mur destroy` |
| Health check | `GET {url}/.well-known/agent-card.json` returns 200 when the capsule is alive |
| Tear down | `mur destroy <job_id>` removes the tracking entry; delete the VM from your provider dashboard |
| List | `mur ps` — reads `~/.murmur/deployments.json` |
