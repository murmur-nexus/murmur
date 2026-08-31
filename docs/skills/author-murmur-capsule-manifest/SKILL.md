---
name: author-murmur-capsule-manifest
description: Author or revise a Murmur runtime capsule manifest (murmur.yaml) for `mur run`, translating a plain-language use case into the smallest correct capsule definition — identity, artifacts, capabilities, inference, and context. Use when creating or changing a runtime capsule manifest; do not use for an artifact build or publish manifest.
---

# Murmur Capsule Manifest Authoring

Use this skill when you need to create or revise a **runtime capsule manifest** (`murmur.yaml`) for `mur run` from a plain-language use case.

Assume the requester may know nothing about Murmur. Translate their desired workload into the smallest correct capsule definition. Do not make them learn the schema before you can help them.

Schema reference: https://docs.murmur.nexus/reference/manifest/

## What you are creating

A Murmur capsule manifest describes **one runtime assembly**:

- the capsule's identity;
- the artifacts participating in this launch;
- what those artifacts and the capsule may reach or execute;
- how model inference is performed, if a model is needed;
- context, lifecycle, observability, persistence, peer exchange, and exported files when the use case requires them.

Do **not** confuse this with an artifact build/publish manifest. The capsule manifest says what this launch assembles and permits. An artifact's own manifest says what that artifact is and how it is packaged or implemented.

The manifest is not merely configuration. Treat it simultaneously as:

1. an executable security boundary;
2. a reproducibility contract;
3. a capability declaration;
4. an incident/audit record;
5. an engineering agreement that another person should be able to inspect without reading the implementation.

That changes how you author it: every extra declaration needs a reason.

---

## Authoring doctrine

Apply these rules before thinking about individual fields.

### Start from absence

Murmur is intentionally sparse. Do not begin from a large template and delete things. Begin with identity and the artifacts that are definitely required, then add capabilities only because the workload needs them.

Omission is often meaningful: no network allow-list means no IP destinations; no shell allow-list means no shell binaries; a hook without per-hook grants starts with no network or filesystem access.

When you can run the capsule, prefer this loop:

1. write the minimum;
2. run it;
3. inspect the failure or warning;
4. add the smallest declaration that resolves the real requirement;
5. repeat.

### Declare roles, not implementations

In the capsule manifest, describe the role an artifact plays:

- `tool` — a capability the capsule can invoke;
- `driver` — the inference adapter used by `transport: http`;
- `hook` — lifecycle/runtime behavior supplied by an artifact;
- `skill` — packaged guidance or instructions.

Do not put artifact implementation details such as WASM/native packaging into the capsule manifest. Those belong to the artifact's own manifest.

### Pin what must be reproducible

For registry artifacts, use concrete versions. Treat a version as identity and a channel such as `latest`, `stable`, or `edge` as a moving pointer. A production or shared capsule should be reproducible from its manifest plus lock state.

Do not invent artifact names or versions. Use the artifacts provided by the requester, already present in project context, or discoverable from the relevant registry/configuration. If you cannot resolve a required artifact, state the unresolved dependency instead of fabricating one.

### Secrets are references, never manifest data

Never place credentials, tokens, passwords, or API keys literally in `murmur.yaml`.

Use environment references such as:

```yaml
api_key: ${PROVIDER_API_KEY}
```

A manifest is plaintext that will be committed, diffed, shared, logged, and pasted into incidents. A literal secret in it is already a disclosure.

### One question gets one answer

Do not configure the same semantic choice through multiple mechanisms.

Examples:

- choose exactly one of `inference.system_prompt`, `system_prompt_file`, or `system_prompt_artifact`;
- choose either `interpreter_runtime` or `staged_runtime` for a given binary, never both;
- do not keep a broad shell path as a "fallback" when a narrower structured tool already performs the operation.

Redundancy is not resilience when one path is broader than the other. The broader path becomes the real security surface.

### Modifiers never imply their gate

A field that refines an existing capability does not grant the capability itself.

Examples:

- a `source:` path only works when local sourcing is enabled for that artifact;
- `interpreter_runtime` or `staged_runtime` only applies to a binary already listed in `shell.allow`;
- `system_prompt_artifact` requires an artifact declared in `artifacts:` and eligible as prompt payload.

When a refinement seems to have no effect, look for the missing gate rather than widening unrelated permissions.

### Configure mechanisms completely

A threshold, budget, or policy is useful only if the mechanism that acts on it exists.

Before adding a knob, ask: **what runtime component reads or enforces this?** If nothing does, omit it. Do not create a manifest that looks safer or more controlled than the runtime behavior actually is.

### Choose a mode before its fields

Some fields belong to a specific operating mode. Pick the mode first and write only that mode's dialect.

The most important partition is inference:

- `transport: http` uses a Murmur driver artifact, endpoint, model, and optional API-key reference;
- `transport: process` invokes a host CLI command and does not use the HTTP driver fields;
- no `inference:` block means the capsule runs without model inference.

Do not mix fields from both transports. Accepted-but-inert fields are still bad manifest design because they mislead the next reader.

### Do not restate mechanical defaults without a reason

Explicitness has a maintenance cost. Declare a default only when changing it, or when a security-relevant choice must be visible in the file.

Examples of defaults you often do **not** need to restate:

- `inference.transport: http`;
- `inference.max_turns: 10`;
- `inference.max_tokens: 8192`;
- `lifecycle.task_acceptance: single`;
- `lifecycle.after_task: exit`;
- `trace.capture: meta`.

When omission and an explicit empty collection mean different things, preserve that distinction. For example, a tool/driver `network.allow: []` is a deliberate narrowing to zero, while omitting that per-artifact network block means it inherits the capsule ceiling.

### The weakest declaration defines the posture

Do not assess a manifest by counting narrow grants. Find the declaration that most weakens containment.

The canonical example is:

```yaml
capabilities:
  filesystem:
    workdir_exec: true
```

This is needed when the workload must execute binaries it produced inside the workdir, such as compiled test binaries. It also makes `shell.allow` unenforceable for workdir-produced executables and caps the achieved containment class at `advisory`.

Use it only when the workload genuinely needs compile-and-run behavior. Interpreted workflows can often keep it `false` by running source through an allowlisted interpreter.

### Know what is enforced and where

For every security-relevant declaration, be able to answer:

- what component enforces it;
- on which platform(s) that enforcement exists;
- whether it is a hard runtime wall or merely a provider/application parameter.

Examples:

- `inference.max_turns` is a Murmur turn ceiling;
- `inference.max_tokens` is passed to the provider and an unsupported upper value may fail at the provider;
- cgroup resource controls are Linux-specific;
- containment guarantees differ by platform and host capability.

Never rely on an advisory value for a property that must be impossible to exceed.

### Treat ingress separately from egress

A capability that lets data **enter** the capsule deserves its own decision even if the same host is already reachable outbound.

`capabilities.peer_fetch` is deliberately separate from `capabilities.network`: redeeming a peer file writes content into the capsule workdir and therefore creates an ingestion and prompt-injection surface.

Do not infer ingress permission from network permission or vice versa.

---

## How to turn a use case into a manifest

Do not begin by asking the requester for Murmur field names. Build a capability brief in ordinary workload terms.

### 1. Identify the task boundary

Determine:

- What is the capsule expected to do?
- What inputs does it receive?
- What outputs must survive or be exposed?
- Is it one task and exit, or a service that accepts many tasks?
- Does it need model inference at all?

Examples:

- **research capsule**: receive a question, retrieve allowed sources, synthesize a report, write results;
- **coding capsule**: inspect a repository, edit files, run selected developer tools/tests, return a patch or result;
- **review capsule**: read existing files/diffs and produce findings without modifying the project;
- **worker capsule**: accept queued tasks, process them repeatedly, and sleep between tasks.

### 2. Build the artifact inventory

List every artifact that participates. The list is explicit; do not assume transitive artifacts will appear for you.

For each artifact decide:

- role: `tool`, `driver`, `hook`, or `skill`;
- registry version, or local source when intentionally authoring a local skill;
- whether it needs artifact-specific `config:`;
- whether its effective capabilities should be narrower than the capsule ceiling;
- whether it needs durable `state`.

A registry artifact normally looks like:

```yaml
artifacts:
  - name: <artifact-name>
    version: "<pinned-version>"
    runtime: tool
```

A local skill can look like:

```yaml
artifacts:
  - name: project-conventions
    source: ./skills/project-conventions/
    runtime: skill
```

For a local `runtime: skill`, `local_source` defaults to enabled. Do not add a meaningless registry `version` beside `source:`; local-source versions are ignored.

### 3. Decide inference architecture

#### No model

Omit `inference:` entirely when the capsule only runs tools or infrastructure behavior.

#### HTTP inference

Use when model calls should go through a Murmur driver artifact.

Required structure:

```yaml
artifacts:
  - name: <driver-artifact>
    version: "<pinned-version>"
    runtime: driver

capabilities:
  network:
    allow:
      - https://<provider-host>

inference:
  endpoint: https://<provider-host>
  model: <model-id>
  api_key: ${PROVIDER_API_KEY}
  driver:
    artifact: <driver-artifact>
```

Rules:

- remote endpoints must use `https://`; `http://` is only valid for loopback hosts;
- the driver artifact named under `inference.driver.artifact` must also appear in `artifacts:` as `runtime: driver`;
- add only the provider/network destinations actually required;
- use `inference.driver.config` for settings that make sense for the driver role generally;
- use the driver's own artifact `config:` for implementation-specific settings;
- if no secret is required, omit `api_key`.

#### Process inference

Use when the capsule intentionally delegates inference to a host CLI such as a supported Claude Code or Codex-compatible command.

```yaml
inference:
  transport: process
  command: <cli-command>
  model: <optional-model-id>
```

Do not add `endpoint`, `driver`, or `api_key` under `transport: process`.

Treat process inference as a deliberate host-process boundary. Keep the rest of the capsule's shell, filesystem, environment, and resource posture narrow.

### 4. Derive capsule-wide capabilities from actual operations

Think in verbs, not fields.

#### Network

Ask: **Which exact destinations must this workload contact?**

```yaml
capabilities:
  network:
    allow:
      - https://api.example.com
      - https://registry.example.org
```

Prefer scheme + host, and port when needed. Do not add broad domains "just in case".

`network.unix_sockets` defaults to `false`. Set it to `true` only when the workload genuinely needs local daemon sockets. It is coarse-grained and can expose sensitive sockets reachable by the process, so it is a major posture change.

#### Filesystem

Ask: **Which part of the session workdir does the capsule actually need?**

```yaml
capabilities:
  filesystem:
    scope: project
```

`scope` must be relative and cannot escape via `..`. If the capsule should see its full accessible workdir, the top-level scope can normally be omitted.

Do not confuse workdir access with durable state. They are different grants.

#### Shell

Ask: **Which host binaries are truly necessary?**

```yaml
capabilities:
  shell:
    allow:
      - git
      - rg
      - python3
```

Prefer a structured Murmur tool over a raw binary when both accomplish the same job. Add shell only when the workload needs the general host executable.

Use `strip_env` to remove sensitive/unnecessary subprocess environment variables. Keep only the baseline environment genuinely required by allowed commands.

If an allowlisted interpreter needs host runtime directories outside the workdir, use one of:

- `interpreter_runtime` — explicitly grants the required host runtime directories;
- `staged_runtime` — mounts an already-pinned runtime tree into a `sealed` capsule.

Both require the binary to already be in `shell.allow`. They are mutually exclusive per binary.

#### Environment

`capabilities.env.allow` exposes selected **non-secret** host environment values to WASM guests. Do not use it as a secret-delivery mechanism; credential-shaped variables are intentionally filtered.

#### Resource bounds

Use `capabilities.limits` for component-level WASM limits and `capabilities.resources` for native subprocess-tree limits.

Only override defaults when the workload needs a different hard ceiling. Useful native-process controls include:

- process/task count;
- open files;
- single-file size;
- CPU seconds;
- per-process memory;
- Linux cgroup aggregate memory, PID, CPU, and I/O limits;
- total workdir size.

If the capsule runs untrusted or potentially runaway developer commands, resource limits are part of the security design, not performance tuning.

#### Containment

Use:

```yaml
capabilities:
  containment: sealed
```

only when the workload requires that minimum class and the deployment host can provide it. Valid floors are `advisory`, `scoped`, and `sealed` in ascending strength.

Do not claim a stronger posture than another field makes possible. In particular, `workdir_exec: true` collapses the achieved posture to `advisory`.

#### Spawn

Use `capabilities.spawn.allow` only when this capsule may create named sub-capsules. List exact allowed capsule names. Do not use shell permissions as a substitute for Murmur capsule spawning.

#### Peer file ingestion

Use `capabilities.peer_fetch.allow` only when the capsule must redeem file handles from specific peers. This is separate from general network access and its `allow` list must be non-empty when present.

### 5. Decide per-artifact grants correctly

This is one of the most important Murmur distinctions.

#### Hooks: start at zero

A hook inherits neither the capsule network ceiling nor the capsule filesystem. Grant each hook exactly what it needs on its own artifact entry.

```yaml
artifacts:
  - name: <hook-artifact>
    version: "<version>"
    runtime: hook
    capabilities:
      network:
        allow:
          - https://telemetry.example.com
      filesystem:
        scope: hook-state
```

Hook-specific grants may also include:

- `state` for durable state;
- `task_io.read: true` to read task input/result through the host interface;
- `conversation.read: true` to read the capsule conversation record through the host interface.

Do not put hook-only grants at capsule scope or on tools/drivers.

#### Tools and drivers: inherit and clamp

A tool or driver with no per-artifact capability block inherits the capsule-wide ceiling. A per-artifact block narrows it:

```yaml
capabilities:
  network:
    allow:
      - https://api.example.com
      - https://other.example.com

artifacts:
  - name: focused-tool
    version: "<version>"
    runtime: tool
    capabilities:
      network:
        allow:
          - https://api.example.com
      filesystem:
        scope: cache
```

The effective network/filesystem grant is the intersection of the artifact declaration and capsule ceiling. A narrowing can subtract, not widen.

Write a narrowing at least as specific as its ceiling. For example, a bare `api.example.com` does not fit beneath a ceiling of `https://api.example.com` because the bare host implies more schemes/ports.

`state` is the deliberate exception: on tools/drivers/hooks it opens a separate durable state directory and therefore widens storage beyond the workdir rather than narrowing it.

### 6. Add artifact configuration only where it belongs

Use `config:` on an `artifacts:` entry for settings specific to that exact artifact implementation:

```yaml
artifacts:
  - name: corpus-tool
    version: "<version>"
    runtime: tool
    config:
      read_recent:
        default: 20
        max: 100
```

Use `inference.driver.config` for provider-agnostic driver-role behavior such as generic timeout/retry policy **only when the selected driver actually documents and supports those keys**.

The runtime validates the shape of these config blocks, not their semantics. Do not invent config keys.

### 7. Add context only for a reason

`context.max_tokens` is a session context budget and is distinct from `inference.max_tokens`, which is the per-turn model output cap.

For HTTP inference, context compaction requires `context.max_tokens`. The compaction settings refine that mechanism; do not add them to a process-inference capsule where they are inert.

Example pattern:

```yaml
context:
  max_tokens: 120000

inference:
  # ...HTTP inference...
  compaction:
    threshold: 0.85
```

The actual compaction behavior also depends on a suitable hook artifact whose binding is defined by the artifact itself. The capsule operator includes the hook but does not rewrite the hook's binding contract in the capsule manifest.

Use `context.record: off` when durable conversation recording is intentionally unwanted. Otherwise the current default is `on` for HTTP sessions.

Use retention only when you have an actual retention requirement. Omission means records are not automatically deleted.

### 8. Choose lifecycle from workload shape

For a normal one-shot capsule, defaults are usually sufficient:

- accepts one task;
- exits after it finishes.

For a long-running worker:

```yaml
lifecycle:
  task_acceptance: queue
  after_task: sleep
  queue_depth: 4
```

Rules:

- `queue_depth` only matters in queue mode;
- `after_task: sleep` is for a capsule intended to remain available;
- use `conversation: threaded` only when tasks sharing a context should accumulate conversational history;
- add `input_timeout_secs` only when tools can request human/input replies and indefinite waiting is unacceptable.

### 9. Add observability deliberately

The runtime already produces local trace data. Add external OTel export only when needed:

```yaml
observability:
  otel_endpoint: http://localhost:4318
```

`trace.capture: content` stores unredacted inference bodies and tool outputs in blobs. Treat that as sensitive data collection, not a harmless debugging switch. Prefer the default metadata-only capture unless full content is required.

Evaluation scorer configuration is meaningful only when the corresponding evaluation hook/artifact is actually present and supports the intended scorer behavior.

### 10. Add exports only when another actor must read files

`exports.files` and `exports.peer_files` open read-only views onto explicit workdir subtrees. They do not grant the agent extra filesystem reach; they grant external readers access to data the capsule already produced.

Keep roots narrow and size/TTL ceilings explicit when the use case requires sharing.

---

## Use-case patterns

These are reasoning patterns, not copy-paste manifests. Artifact names, versions, model IDs, package registries, and required binaries are workload-specific and must be resolved rather than guessed.

### Research capsule

Typical shape:

- one inference driver;
- one or more structured research/retrieval tools;
- optional domain skill;
- network only to the model endpoint and explicitly required research sources/services;
- no shell unless local transformation genuinely needs it;
- a narrow output directory if only report files need writing;
- larger context/compaction only for long research sessions;
- one-shot lifecycle unless acting as a research service.

Security bias: prefer structured fetch/search tools over `curl` + broad network. Every additional research source is also an ingestion surface.

### Coding capsule

Typical shape:

- one inference driver or process-inference CLI;
- repository/workdir access;
- a small shell allow-list such as the exact VCS, search, formatter, compiler/runtime, and test commands needed by the project;
- no internet by default;
- add package registries, source hosts, or API endpoints only when the task explicitly requires dependency installation or remote access;
- resource limits appropriate for builds/tests;
- optional project-conventions skill;
- trace metadata by default.

Important distinction:

- editing files and running them through an interpreter usually does **not** require executable workdir files;
- compiling and then executing binaries produced inside the workdir generally **does** require `filesystem.workdir_exec: true`, which weakens containment to `advisory`.

Do not turn on `workdir_exec` merely because the capsule "codes". Turn it on because a concrete workflow must execute workdir-produced binaries.

### Read-only review capsule

Typical shape:

- model inference;
- only read-oriented structured tools where possible;
- no shell if a dedicated diff/file tool suffices;
- no outbound network beyond inference unless required;
- no durable state unless the reviewer truly needs cross-session memory;
- no exports unless findings must be exposed as files.

A review capsule should not inherit a coding capsule's broader grants merely because both operate on source code.

### Long-running worker capsule

Typical shape:

- `task_acceptance: queue`;
- `after_task: sleep`;
- bounded `queue_depth`;
- explicit resource ceilings;
- bounded conversation/trace retention if persistent operation would otherwise accumulate indefinitely;
- narrow state store only when cross-task state is required.

Long-running does not imply broad access. It increases the value of narrow grants because the exposure lasts longer.

---

## Questions to resolve when information is missing

Do not interrogate the requester field-by-field. Ask only questions that materially change the manifest, and phrase them in workload terms.

Good questions include:

- Which model/provider or local inference command should this capsule use?
- Which existing Murmur tools, hooks, skills, or driver artifacts are available, and at what versions?
- What files/directories should the capsule be able to work with?
- Which host commands must it run?
- Does it need internet access? If yes, which exact hosts/services?
- Must it install dependencies or execute compiled artifacts it creates?
- Should state or conversation survive across tasks/sessions?
- Is this one-shot or long-running/queued?
- Must other capsules/operators retrieve files it produces?
- What hard CPU/memory/process/storage ceilings are required?
- What minimum containment class must deployment guarantee?

If the requester does not know a value and a safe omission exists, prefer omission over invention.

If a required artifact name/version, endpoint, model ID, or host path cannot be known from context, leave it clearly unresolved rather than fabricating a runnable-looking value.

---

## Manifest construction order

Use this order when writing the final YAML. It keeps dependencies readable and exposes posture early:

1. `name`, `version`, optional `mur_version`;
2. `artifacts`;
3. capsule-wide `capabilities`;
4. `network` only when a fixed A2A internal port is required;
5. `inference`;
6. `context`;
7. `lifecycle`;
8. `observability` / `trace`;
9. `exports`.

Do not add an empty section merely to preserve this order.

---

## Validation pass before you finish

Audit the completed manifest semantically, not just syntactically.

### Identity and inventory

- `name` and `version` are present.
- Every registry artifact has a real pinned version.
- No artifact dependency was assumed transitively.
- Runtime roles describe roles, not implementation technology.
- Local `source:` is intentional and legally gated.

### Inference

- No inference fields exist when no model is required.
- `http` and `process` fields are not mixed.
- HTTP driver is declared as an artifact.
- Remote HTTP endpoints use TLS.
- Model/output/context budgets are not confused.
- Exactly one system-prompt mechanism is used.
- API keys are environment references, never literals.

### Capability posture

- Every network destination has a concrete workload justification.
- Every shell binary has a concrete workload justification.
- `unix_sockets` is off unless explicitly needed.
- `workdir_exec` is off unless executing workdir-produced binaries is required.
- Filesystem scopes are relative and do not escape.
- Ingress via peer fetch is separately justified.
- Spawn allow-list contains only intended child capsules.
- Hard resource ceilings exist where runaway native workloads are plausible.
- The declared containment floor is compatible with the rest of the manifest.

### Per-artifact semantics

- Hook grants start from zero and are explicit.
- Tool/driver network and filesystem blocks only narrow the capsule ceiling.
- Narrowings are at least as specific as their ceiling entries.
- `state` is recognized as a separate durable-store widening, not a narrowing.
- Hook-only grants are not placed on tools/drivers or at capsule scope.
- Artifact config keys are documented by the artifact; none were invented.

### Couplings and modes

- Every modifier has its gate.
- Every threshold/budget has a mechanism that can act on it.
- No question is answered twice.
- No fields belong to a mode that was not selected.
- No derived default is restated without a reason.
- Empty collections are used only when they intentionally differ from omission.
- A narrow structured capability is not undermined by an unnecessary broader fallback.

### Audit honesty

For each meaningful line ask:

> If this capsule caused an incident, would this line accurately describe what was structurally possible and why it was permitted?

If not, remove or tighten it.

---

## Verify with Murmur

When execution is available, do not stop after writing YAML.

1. Install/resolve registry artifacts as appropriate for the project.
2. Inspect effective grants:

```bash
mur run --explain-scope
```

3. Read **warnings**, not just errors. A warning often means a declaration is inert, dropped, weaker than intended, or reducing the achieved containment class.
4. Run the capsule.
5. If it fails because a capability is absent, add the narrowest declaration that satisfies that exact requirement and run again.
6. Re-check effective grants after every security-relevant change.

A manifest that parses but contains inert or misleading declarations is not finished.

---

## Output contract

When this skill is asked to create a manifest:

1. Produce the smallest valid `murmur.yaml` you can justify from the available use-case information.
2. If you have filesystem write access, write/update `murmur.yaml`; otherwise return it in one YAML block.
3. Do not include speculative grants "for convenience".
4. Do not invent artifact names, versions, model IDs, config keys, secret names, host paths, or network destinations.
5. If essential information remains unresolved, mark the unresolved dependency clearly outside the YAML rather than disguising placeholders as a runnable manifest.
6. Briefly explain only the **non-obvious** decisions: broad shell access, executable workdir, unix sockets, durable state, peer ingestion, spawning, content traces, exports, or unusually broad network access.
7. End with the exact validation commands that are applicable, normally including `mur run --explain-scope` and `mur run`.

The goal is not a manifest with many fields. The goal is a manifest whose every line is necessary, enforceable where expected, and explainable.
