# How to let a coding capsule pull the tools it needs

A coding agent does not always know at launch which tools a task will need. Declaring every tool up front gives the model a long tool list it mostly ignores, and a task that needs one more tool fails. With `capabilities.install`, the operator names the tools a capsule may add, and the model adds them itself: it lists what it may pull, pulls the one it needs, calls it on its next turn, and removes it when it is done. Every step is pinned in `murmur.lock` and written to the session trace.

??? definition "What is a pulled artifact?"
    A **pulled artifact** is a skill or tool installed into a running session by the capsule itself, rather than declared in `murmur.yaml` by the operator. Its `murmur.lock` pin carries `origin: runtime`, and the model is told it was not vetted by the operator.

The relevant manifest options are:

| Option | Controls |
|---|---|
| [capabilities.shell.allow](../reference/manifest.md#shell-allow) | Which binaries the agent may invoke as shell tools |
| [capabilities.install.tool](../reference/manifest.md#field-install) | Which tool artifacts the capsule may pull while it runs |
| [capabilities.install.from](../reference/manifest.md#field-install) | Whether a pull may fetch from the scope's GitHub source |

---

## Step 1 — create the manifest

Create a `murmur.yaml` file for a coding capsule that runs `git` and `cargo`, and may pull any Murmur tool:

=== "Anthropic"

    ```yaml
    name: coding-agent
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
          - git
          - cargo
      install:
        tool: ["murmur/tool-*"]
        from: [stores, sources]

    inference:
      transport: http
      model: {{ v.model_anthropic }}
      driver:
        artifact: murmur/driver-anthropic
    ```

=== "OpenAI"

    ```yaml
    name: coding-agent
    version: "0.1.0"

    artifacts:
      - name: murmur/driver-openai
        version: "{{ v.murmur_driver_openai }}"
        runtime: driver
        gateway:
          endpoint: https://api.openai.com/v1
          api_key: ${OPENAI_API_KEY}

    capabilities:
      shell:
        allow:
          - git
          - cargo
      install:
        tool: ["murmur/tool-*"]
        from: [stores, sources]

    inference:
      transport: http
      model: {{ v.model_openai }}
      driver:
        artifact: murmur/driver-openai
    ```

The `install` block gives the model two tools, `pull-artifact` and `remove-artifact`. Without it neither tool exists. `tool: ["murmur/tool-*"]` admits every tool in the `murmur` scope whose name starts with `tool-`, and no skill, hook or driver. `from: [stores, sources]` lets a pull that neither the project store nor the global store can answer be fetched from the scope's GitHub source by the runtime itself; leave `from` out and a pull is answered by the stores alone.

`capabilities.install` works only with `transport: http`. Under `transport: process` the harness owns the tool list, and the manifest is refused at load.

## Step 2 — install dependencies

```bash
mur install
```

--8<-- "includes/mur-pull-info.md"

`murmur/tool-edit` is not declared, so `mur install` does not install it. The capsule pulls it.

## Step 3 — write the task

Create a `task.md` file:

```
The test `parse_empty_input` in this repository fails. Find the cause, fix it with a file-editing
tool you pull for the purpose, run `cargo test`, and remove the tool when you are done.
```

## Step 4 — run the capsule

```bash
mur run --task task.md -v
```

```
murmur: url localhost:52222
session: ses_019ed2af76b57c42912b612f07dd4d51
workdir: /home/you/coding-agent/workdir/ses_019ed2af76b57c42912b612f07dd4d51
manifest: coding-agent v0.1.0
driver: murmur-driver-anthropic ({{ v.model_anthropic }})
status:  ok
```

## Step 5 — follow what the model did

The model's first request offers `pull-artifact` and `remove-artifact` beside the shell tools. Their descriptions name what this capsule may pull and from where.

The model lists what it may pull by passing a pattern ending in `*`:

```json
{"artifact": "murmur/tool-*"}
```

The result names each tool, its versions newest first, the version a pull would choose, and where it is held. A pattern pulls nothing:

```json
{"pattern": "murmur/tool-*", "matches": [{"artifact": "murmur/tool-edit", "runtime": "tool", "description": "Replaces text in a file. Edits apply in order, each to the result of the one before; each old must match exactly once. All or nothing: if one fails, the file is unchanged and the error names its position, from 1. path is relative to the work folder; an absolute path is refused.\n", "versions": ["{{ v.murmur_tool_edit }}"], "newest_fitting": "{{ v.murmur_tool_edit }}", "held_in": ["github:murmur-nexus/default-artifacts"]}], "omitted": 0, "notes": []}
```

It pulls `tool-edit` by name. With no version, the runtime takes the newest version that fits this `mur`:

```json
{"artifact": "murmur/tool-edit"}
```

```text
<untrusted-content source=tool:pull-artifact>
{"artifact":"murmur/tool-edit","fetched_from":"github:murmur-nexus/default-artifacts","runtime":"tool","tool_name":"tool-edit","version":"{{ v.murmur_tool_edit }}"}
</untrusted-content>
tool-edit is in your tool list from your next turn.
```

On its next turn the model is offered `tool-edit`, its description opening with `[origin: runtime, trust: untrusted]`, and calls it to patch the file. Once `cargo test` passes, it removes the tool:

```json
{"artifact": "tool-edit"}
```

```text
<untrusted-content source=tool:remove-artifact>
{"artifact":"murmur/tool-edit","removed":true}
</untrusted-content>
tool-edit is gone from your tool list from your next turn; a call to it is refused.
```

A pull the grant does not admit, of a version that does not fit this `mur`, or of a hook or driver, comes back as a tool error opening with `not-granted:`, `not-found:`, `does-not-fit:` or `not-fetched:`, and writes nothing. The full list is in [`pull-artifact`](../reference/runtime-provided-tools.md#pull-artifact-refusals).

## Step 6 — read the lock and the trace

While the tool was installed, `murmur.lock` held its pin, written by the runtime:

```yaml
  - name: murmur/tool-edit
    resolved_version: "{{ v.murmur_tool_edit }}"
    sha256:
      any: "<sha256>"
    origin: runtime
    session: ses_019ed2af76b57c42912b612f07dd4d51
    owner_id: 190196424
```

`origin: runtime` and `session` say which session pulled it. `owner_id` is the GitHub account that owns the `murmur` scope; a later pull or install from another owner of the scope is refused. The removal deleted the entry. An artifact the capsule pulled and did not remove keeps its pin, but the next launch stages it only if `murmur.yaml` declares it — see [Pin origin](../reference/workdir.md#lock-origin).

Read the trace:

```bash
mur trace show
```

--8<-- "includes/mur-trace-show-info.md"

--8<-- "includes/mur-trace-explore.md"

```
── Pulled at runtime ────────────────────────────
  murmur/tool-edit@{{ v.murmur_tool_edit }}  tool  runtime/untrusted  from github:murmur-nexus/default-artifacts
```

`trace.jsonl` holds the detail: an `artifact_pulled` line with `fetched_from` and `owner_id`, and a `tools_refreshed` line with `trigger: "agent"` before each inference call whose tool list the model changed — see [Session trace](../reference/observability-schemas.md#tools-refreshed).

!!! warning "A pulled artifact is untrusted"
    The operator never vetted a pulled artifact. The model is told so in its tool list, and its results are fenced as untrusted data. Two controls sit on top of the grant:

    - An `on-tool-call` [policy hook](../concepts/hooks.md) decides on every `pull-artifact` call, as on any other tool call, so it can deny a pull by name or version.
    - To make a pulled tool part of the capsule, declare it in `murmur.yaml` and run `mur install murmur/tool-edit@{{ v.murmur_tool_edit }}`. The install adopts the pin as `origin: operator`, and the tool is no longer marked untrusted.

---

## Summary

| Feature / setting | How it works |
|---|---|
| `capabilities.install.tool` | Names the tools the model may pull; hooks and drivers are never pullable |
| `capabilities.install.from` | `[stores]` by default; `[stores, sources]` also fetches from the scope's GitHub source, writing to no store |
| `pull-artifact` | Pulls `<scope>/<name>` or `<scope>/<name>@<version>`, usable from the next turn; a pattern ending in `*` lists instead |
| `remove-artifact` | Removes a tool the session pulled; later calls to it are refused |
| `murmur.lock` | Pins each pull with `origin: runtime`, the session and the scope's owner id |
| `trace.jsonl` | `artifact_pulled` per pull, `tools_refreshed` with `trigger: "agent"` per change |
| Trust | Pulled artifacts are marked untrusted; a policy hook can gate pulls, and `mur install` adopts a pin |
