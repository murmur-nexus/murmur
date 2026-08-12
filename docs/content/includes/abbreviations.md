<!-- Manifest property tooltips — auto-appended to every page via pymdownx.snippets -->

*[artifacts[].runtime]: wasm: tool visible to model · default | driver: inference driver · hidden | hook: lifecycle observer · hidden | native: binary tool · visible | skill: guidance file · agent reads voluntarily

*[artifacts[].source]: local path to skill.md or directory · skill artifacts only · bypasses registry

*[capabilities.filesystem.allow]: path prefixes the tool may use as a repo root · validated before any git operation

*[capabilities.network.allow]: host/URL patterns the agent may connect to

*[capabilities.filesystem.scope]: relative workdir subtree preopened as current directory

*[artifacts[].capabilities]: optional per-artifact grant · narrows one tool/driver below ceiling · absent = full ceiling

*[artifacts[].capabilities.network.allow]: hosts one artifact may reach · intersected with ceiling · [] = deny all

*[artifacts[].capabilities.filesystem.scope]: workdir subtree one artifact preopens instead of whole workdir

*[capabilities.shell.allow]: bare binary names (e.g. bash · jq) the agent may invoke as tools

*[capabilities.shell.strip_env]: glob patterns for env vars to strip from subprocesses

*[capabilities.shell.baseline_env]: env var patterns to keep after strip_env runs

*[context.max_tokens]: token budget · required to enable compaction · omit to disable

*[inference.max_tokens]: per-turn output cap · sent to the driver as max_tokens · default 8192 · http transport only · not the compaction budget

*[inference.model]: model identifier string · e.g. claude-sonnet-4-6 · claude-opus-4-7 · claude-haiku-4-5-20251001

*[inference.driver.artifact]: inference driver artifact name · must use runtime: driver

*[inference.compaction.threshold]: float 0.0–1.0 · fraction of context.max_tokens that triggers compaction · default 0.98

*[inference.compaction.model]: model override for compaction calls · defaults to primary inference model

*[inference.compaction.system_prompt]: system prompt override for compaction calls · passed verbatim to the compaction hook · defaults to none, hook picks its own default

*[inference.system_prompt]: inline system prompt · injected on every API call · exclusive with system_prompt_file

*[inference.system_prompt_file]: file path relative to murmur.yaml · used as system prompt · exclusive with system_prompt

*[network.internal_port]: fixed port the worker binds on · OS-assigned when omitted · errors if port already in use

*[lifecycle.task_acceptance]: none: task.md only | single: one task then exit · default | queue: serial task queue

*[lifecycle.after_task]: exit: stop after task · default | sleep: wait for next task · use with queue

*[lifecycle.queue_depth]: integer · pending task buffer · default 1 · excess tasks rejected

*[mur_version]: pins mur binary version installed on the VM · omit to use the running version

*[lifecycle.conversation]: stateless: each task independent · default | threaded: tasks share history per contextId

*[observability.otel_endpoint]: OTLP/HTTP endpoint for span export · absent = no external telemetry

*[observability.eval.dataset_id]: labels dataset_run records in eval.jsonl

*[observability.eval.scorers]: scorer types: exit_ok | max_turns | max_tokens | tool_sequence

*[capabilities.containment]: advisory: no kernel enforcement · default | scoped: Landlock + seccomp | sealed: composed root · refuses launch when host falls short
