# Artifacts

Artifacts include tools, drivers, hooks, and skills. Capsules declare required artifacts by full name and version in the manifest. A full name is `<scope>/<name>` — `murmur/tool-git` is the default artifact `tool-git` — and a session offers each artifact to the model by its bare name, the part after the `/`. See [Artifact names](registry.md#artifact-names).

## Extension points

Agent capsules declare these extension points in their manifest:

| Extension point | Declared as | Required? | If absent |
|---|---|---|---|
| Inference driver | `inference.driver.artifact: <scope>/<name>` | Yes | Error at launch |
| System prompt | `inference.system_prompt*` or artifact | No | Built-in default used |
| Tools | `artifacts:` with `runtime: tool` | No | Agent has no callable tools |
| Hooks | `artifacts:` with `runtime: hook` | No | No hook behavior |

The [session loop](session-loop.md) itself does not change when artifacts are added or
removed.
