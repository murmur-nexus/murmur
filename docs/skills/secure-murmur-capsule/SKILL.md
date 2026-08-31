---
name: secure-murmur-capsule
description: Design, harden, or review a Murmur capsule's runtime authority by deriving least-privilege manifest capabilities, separating untrusted ingestion from action, verifying host enforcement, and validating warnings and traces. Use for new or existing Murmur capsules and murmur.yaml security reviews; do not use for host hardening unrelated to a capsule.
---

# Secure a Murmur Capsule

Produce a capsule that can complete its actual workload while making unintended action structurally unavailable. Do not equate a restrictive-looking `murmur.yaml` with enforced containment.

When schema or runtime behavior may have changed, consult the current Murmur documentation before editing:

- [Lock down a capsule's capabilities](https://docs.murmur.nexus/how-to/lock-down-capsule/)
- [Manifest reference](https://docs.murmur.nexus/reference/manifest/)
- [Containment reference](https://docs.murmur.nexus/reference/containment/)
- [Access-control threat model](https://docs.murmur.nexus/concepts/access-control/)
- [Diagnostics reference](https://docs.murmur.nexus/reference/diagnostics/)

Treat the documentation and installed runtime as authoritative for field names and current enforcement behavior. Treat this skill as the decision procedure.

## Security model

Apply these rules throughout the work:

1. **Authority is the boundary.** Trust labels, prompt instructions, sanitization, warnings, and traces help interpretation and investigation; they do not reduce what an injected or mistaken model action can do.
2. **Assume confused-deputy behavior.** Ask: if attacker-influenced content makes the agent use every granted capability for the wrong reason, what can it damage or disclose?
3. **The top-level capability block is the ceiling.** Lower it before tuning artifacts. An artifact's effective grant is its declaration intersected with this ceiling; it can subtract but cannot widen it.
4. **Declaration and enforcement are separate facts.** A manifest states intended authority. The target host and runtime mechanisms determine which parts are walls. Never report a capability as enforced without evidence from the deployment class being assessed.
5. **Subprocess creation changes the boundary.** WASM component filesystem scopes are runtime-enforced across supported platforms. A shell binary, spawn grant, or native tool moves work into an OS process whose filesystem, exec, network, and resource confinement depends on host support.
6. **Break the maximum-risk pair first.** A general shell plus any path for external content is the critical combination. Count fetches performed by tools, drivers, peer retrieval, package managers, version-control commands, and pre/post hooks—not only entries visible under the capsule network allowlist.
7. **Reduce authority before adding detection.** Prefer removing a grant, narrowing a scope, or moving work into a constrained artifact. Observability is still required, but it is evidence rather than protection.

## Gather the security context

Inspect the existing capsule, its artifacts, and its real deployment. Derive or ask only for facts that materially change the result:

- intended task and required outputs;
- `murmur.yaml` and referenced artifacts;
- production OS, kernel/runtime enforcement tier, and whether another container or VM supplies controls;
- input origins, including web pages, repositories, documents, tool output, tasks, peer artifacts, and user uploads;
- files, directories, hosts, binaries, environment variables, credentials, durable state, sockets, interpreters, and exports actually required;
- whether the workload must execute bytes created inside the workdir;
- expected concurrency, runtime, memory, process, file, and disk needs.

If the production host is unknown, continue with a conditional design, but label subprocess restrictions as declarations whose enforcement is unverified. Do not certify the capsule as secure.

## Derive authority from the workload

Build a small capability inventory before changing YAML. For each operation, record:

| Operation | Input trust | Artifact or phase | Required read/write scope | Required hosts | Process/exec need | Secret/state need |
| --- | --- | --- | --- | --- | --- | --- |

Reject grants justified only by convenience, anticipated future use, or an artifact publisher's own manifest. Capability authority comes only from the operator's capsule manifest. Publisher trust does not substitute for least privilege.

Distinguish directions of authority:

- network egress changes where data or actions may leave;
- fetch and peer retrieval change what untrusted bytes may enter;
- filesystem and durable-state grants change what may persist or be disclosed;
- sockets can expose host services and their delegated authority;
- exports disclose data to the operator but do not grant the agent more power;
- interpreter and executable-workdir features can turn written data into executable authority.

## Choose a safer architecture before tuning fields

Use the first viable option in this order:

1. No subprocesses.
2. Scoped WASM tools and drivers for discrete operations.
3. Specific native binaries without a general shell, while acknowledging that interpreters and programs with execution options can still launch arbitrary code unless the host enforces exec mediation.
4. A general shell only when the workload cannot be expressed otherwise.

For any external or attacker-influenced content, separate ingestion from action:

- The gathering capsule or phase may fetch and read, but receives no shell, write, mutation, credential, or side-effect authority it does not need.
- It emits a bounded, typed, lossy structure containing only fields required downstream.
- The action capsule or phase receives that structure, not the raw source bytes, and has no fetch path back to them.

Temporal separation alone is insufficient. Passing raw text, HTML, repository instructions, or arbitrary tool output into the action context recreates the original prompt-injection channel.

## Lower the capsule-wide ceiling

Edit the top-level `capabilities:` block first:

1. **Network:** allow only exact required endpoints. Prefer scheme-bound entries such as `https://api.example.com`; include a port only when required. Usually the inference endpoint is the first and only host.
2. **Filesystem:** set `capabilities.filesystem.scope` to the smallest session-workdir-relative subtree that contains the workload. Do not use host-absolute paths or traversal.
3. **Shell:** omit shell access if possible. Otherwise replace a general shell with the smallest set of bare binary names the task genuinely needs.

Do not confuse a specific binary list with confinement. `python3 -c`, `find -exec`, version-control hooks or transport overrides, and similar features can execute other programs. Swapping `bash` for another shell changes a warning pattern, not the risk.

If the workload must execute files created in the workdir, identify the relevant manifest feature from the current reference and explicitly report that it limits the achievable containment claim. Compile-and-run workloads may require this tradeoff; it must never be implicit.

## Constrain the subprocess environment

Subprocesses start from Murmur's small baseline rather than the complete host environment. Harden it deliberately:

- use `capabilities.shell.strip_env` for baseline values the workload does not need;
- use `capabilities.shell.baseline_env` only for additional non-secret host values the workload truly needs;
- reference credentials as `${ENV_VAR}` values in the manifest; never put literal secrets in YAML;
- remember that built-in credential-shaped variables are stripped before spawn;
- remember the composition order: baseline, explicit additions, removals, then synthetic `HOME`/`USERPROFILE`; removals win and the synthetic home cannot be overridden.

The same environment construction applies to native tool subprocesses even when `shell.allow` is absent. Passing a secret to an inference driver or constrained tool is different from exposing it to a shell environment; grant it only to the component that uses it.

## Narrow artifacts below the ceiling

Do this after the ceiling is minimal.

- A tool or driver with no `capabilities:` block inherits the full capsule ceiling.
- An explicit per-artifact `network.allow: []` narrows that artifact to no outbound network; omission inherits the ceiling and is not equivalent.
- Per-artifact network entries must be at least as specific as the corresponding ceiling entry. Match scheme, host, and port exactly when possible.
- Use per-artifact `filesystem.scope` to give each WASM tool only its required subtree.
- In current Murmur behavior, only per-artifact `network` and `filesystem` narrowing is effective. Per-artifact `shell`, `spawn`, `env`, `limits`, `resources`, or `containment` entries are inert and trigger `W-SEC-008`; put subprocess controls at capsule scope or move the operation into WASM.
- Hooks start with no network, directory, or task visibility unless granted. Tools and drivers start from the opposite default by inheriting the ceiling. Audit unnarrowed tools and widened hooks.

Example shape; adapt endpoints, scopes, artifact names, versions, and provider fields to the actual use case:

```yaml
artifacts:
  - name: murmur-driver-example
    version: "1.0.0"
    runtime: driver
    capabilities:
      network:
        allow:
          - https://api.example.com

  - name: murmur-tool-reader
    version: "1.0.0"
    runtime: tool
    capabilities:
      network:
        allow: []
      filesystem:
        scope: inputs

capabilities:
  network:
    allow:
      - https://api.example.com
  filesystem:
    scope: inputs

inference:
  endpoint: https://api.example.com
  api_key: ${EXAMPLE_API_KEY}
```

Do not copy this example unchanged. It demonstrates a ceiling and explicit narrowing, not a complete manifest.

## Require the intended enforcement level

Consult the current containment reference and set a containment floor appropriate to the risk. The floor means “refuse to launch below this class”; it does not strengthen the host or grant authority.

Keep these facts separate in the assessment:

- **declared floor:** minimum enforcement requested by manifest, CLI, or workspace policy;
- **achieved class:** what probing shows the host can support;
- **installed mechanism:** what the capsule actually requested and ran under.

A stronger-capable host does not silently upgrade a weaker requested mechanism. A missing floor permits launch at the weakest class. Test on the production host class, outside any extra container boundary when determining what Murmur itself enforces.

Do not overstate guarantees. Filesystem mediation may govern read/write/execute operations without hiding path metadata. Strong root isolation can make outside paths absent while host process metadata may remain visible. Record exceptions that matter to the workload.

## Bound denial-of-service authority

Set resource limits using the current manifest schema. Cover at least process count, memory, open files, CPU/runtime where supported, and workdir disk usage.

Distinguish:

- per-process limits, which do not necessarily bound the whole process tree;
- aggregate subprocess-tree limits, which require host support;
- sampled workdir-size enforcement, which can detect a breach after it occurs rather than preventing the first excess byte.

Verify each limit on the production platform. A declared memory or process limit may be clamped, unsupported, or advisory depending on the host. Absence of a limit-attribution event does not prove no limit was involved.

## Validate the effective posture

Do not stop after editing YAML.

1. Run the capsule on the target host type and capture stderr from staging onward.
2. Inspect `workdir/<session_id>/MURMUR.md` for the inventory of model-visible shell commands and effective session context.
3. Inspect behavior with `mur trace show` and `mur trace steps --verbose`; use `mur trace diff` when comparing pre- and post-hardening runs.
4. Verify the achieved containment class and the mechanism actually installed, not only the class the host could support.
5. Exercise safe negative tests against each intended boundary: an unallowed host, a path outside scope, an unlisted binary, a stripped environment variable, and controlled resource-limit cases. A positive task succeeding proves functionality, not containment.
6. Re-run the real workload and confirm that no removed capability is required. Add back only the narrowest capability supported by a concrete failure.

Treat these diagnostics as security-significant:

- `W-SEC-001` / `W-SEC-003`: declared subprocess filesystem or network restrictions are not fully enforced on this host;
- `W-SEC-004`: a literal secret appears in the manifest;
- `W-SEC-005`: exec allowlisting is not kernel-enforced on this host;
- `W-SEC-007`: an artifact network entry was outside the ceiling and was dropped;
- `W-SEC-008`: an artifact-local capability key is inert or the artifact cannot be narrowed that way;
- `E-CAP-002`: an artifact filesystem scope used an absolute path or escaped its allowed root.

Warnings are the difference between intended and effective policy. Capture them in automation; do not suppress, ignore, or “fix” them by renaming an equally powerful primitive.

## Use-case patterns

### Coding capsule

- Treat externally fetched repositories, dependency metadata, issues, and build output as potentially attacker-influenced.
- Prefer a pre-staged, scoped working copy and no network during the action phase.
- Replace shell operations with scoped version-control, editor, and test-runner tools where practical.
- If arbitrary builds or generated executables must run, state that the workload inherently expands execution authority and require a host/container boundary appropriate to hostile code.

### Research capsule

- Give a gathering capsule allowlisted fetch/read capabilities and no shell or write authority.
- Emit citations and bounded extracted facts, not raw pages, to a synthesis or action capsule.
- Give the synthesis capsule only its inference endpoint and output scope; set artifact network access explicitly to empty where unused.

### Data or reporting capsule

- Put database access behind a query tool using read-only, narrowly scoped credentials.
- Give the report writer no database credential and no network unless delivery is part of its explicit role.
- Separate report generation from sending or publishing so mutation authority is independently reviewable.

## Deliver the result

For an implementation request, provide:

1. the edited manifest or a focused patch;
2. an authority inventory mapping every remaining grant to a workload requirement;
3. an enforcement report separating declared, achieved, and unverified controls;
4. validation evidence, including warnings and negative tests;
5. residual risks and the narrowest next architectural improvement.

For a review-only request, do not modify files. Rank findings by authority and exploit path, starting with:

1. executable workdir or equivalent containment-capping features;
2. general shell plus external-content ingestion;
3. subprocess use on an insufficient or unverified host tier;
4. broad ceiling grants, durable state, sockets, or secrets exposure;
5. unnarrowed tools/drivers and widened hooks;
6. missing containment floor or aggregate resource bounds;
7. warnings not captured or investigated.

Never conclude only that a manifest “looks secure.” State what the capsule can do, what mechanism prevents everything else, where that mechanism is absent, and how this was tested.
