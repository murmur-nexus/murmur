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

*[context.max_tokens]: context window the input budget is cut from · default: the driver's models: entry for the model · no window, no compaction

*[inference.max_tokens]: per-turn output cap · sent to the driver as max_tokens · default: the driver's models: entry for the model · http transport only · not the context window

*[inference.model]: model identifier string · e.g. claude-sonnet-4-6 · claude-opus-4-7 · claude-haiku-4-5-20251001

*[inference.driver.artifact]: inference driver artifact name · must use runtime: driver

*[inference.compaction.threshold]: float 0.0–1.0 · fraction of the input budget that triggers compaction · default 0.98

*[inference.compaction.model]: model override for compaction calls · defaults to primary inference model

*[inference.compaction.max_tokens]: output cap of the compaction call · default: driver's ceiling for the model · refused above it (E-RUN-054)

*[inference.compaction.driver_config]: keys laid over inference.driver.config · compaction call only · shallow

*[inference.compaction.system_prompt]: system prompt override for compaction calls · passed verbatim to the compaction hook · defaults to none, hook picks its own default

*[inference.system_prompt]: inline system prompt · injected on every API call · exclusive with system_prompt_file

*[inference.system_prompt_file]: file path relative to murmur.yaml · used as system prompt · exclusive with system_prompt

*[network.internal_port]: fixed port the worker binds on · OS-assigned when omitted · errors if port already in use

*[exports.peer_tasks.accept]: true: serve tasks other capsules send | false: refuse them 403 · default

*[network.authentication]: door refuses callers without token · bearer only · required on every member of roster with edges

*[lifecycle.task_acceptance]: none: task.md only | single: one task then exit · default | queue: serial task queue

*[lifecycle.after_task]: exit: stop after task · default | sleep: wait for next task · use with queue

*[lifecycle.queue_depth]: integer · pending task buffer · default 1 · excess tasks rejected

*[mur_version]: the deploy.mur_version pin — the mur release mur deploy installs on the target · omit to use the running version

*[lifecycle.conversation]: stateless: each task independent · default | threaded: tasks share history per contextId

*[observability.otel_endpoint]: OTLP/HTTP endpoint for span export · absent = no external telemetry

*[observability.eval.dataset_id]: labels dataset_run records in eval.jsonl

*[observability.eval.scorers]: scorer types: exit_ok | max_turns | max_tokens | tool_sequence

*[capabilities.containment]: advisory: no kernel enforcement · default | scoped: Landlock + seccomp | sealed: composed root · refuses launch when host falls short

*[context.record]: on: keep durable conversation record · default | off: nothing written

*[context.record_store]: directory under ~/.murmur/conversations/ · default: capsule name

*[context.seed_budget]: float 0.0–1.0 · fraction of the input budget a task-start seed may occupy · default 0.10

*[context.seed_overflow_margin]: float 0.0–1.0 · slack above seed_budget before trimming or summarizing · default 0.10

*[capabilities.conversation.read]: bool · grants a hook read access to the conversation record · required, never inferred

*[capabilities.state]: durable directory outside the workdir · keyed by capsule · unreachable by the capsule's own file tools

*[inference.transport]: http: WASM driver calls a provider API · default | process: process driver runs a harness CLI as a subprocess

*[inference.command]: executable run instead of the one the process driver's describe() names · process transport only

*[inference.max_turns]: model steps one task may take · a message or a tool call is one turn · default: no cap

*[capabilities.env.allow]: environment variable names the harness subprocess may see · the whole environment it gets · process transport only
*[capabilities.install.tool]: tool artifacts a running capsule may pull · exact name | scope/prefix* | *
*[capabilities.install.from]: stores: project then global store · default | stores, sources: also scope's GitHub source
