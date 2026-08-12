# Artifacts

Artifacts include tools, drivers, hooks, and skills. Capsules declare required artifacts by name and version in the manifest.

## Extension points

Agent capsules declare these extension points in their manifest:

| Extension point | Declared as | Required? | If absent |
|---|---|---|---|
| Inference driver | `inference.driver.artifact: <name>` | Yes | Error at launch |
| System prompt | `inference.system_prompt*` or artifact | No | Built-in default used |
| Tools | `artifacts:` with `runtime: tool` | No | Agent has no callable tools |
| Hooks | `artifacts:` with `runtime: hook` | No | No hook behavior |

The [session loop](session-loop.md) itself does not change when artifacts are added or
removed.
