//! The manifest-fixed facts an agent session's model is told about its own capsule: where it may
//! write, where it may connect, the limits the runtime enforces on it, and the names its tools
//! reach the runtime directory by.
//!
//! One value, built at staging from the manifest as resolved there, feeds all three texts that
//! state them: the `[Capsule]` block (`transport: http`), the `transport: process` system prompt
//! and MURMUR.md. The limits are rendered for all three by [`CapsuleFacts::limit_parts`].
//!
//! Nothing here varies between launches of one manifest. Only the limits in force are stated: a
//! manifest with no `capabilities.resources.cpu_seconds` sets no CPU-time limit, and one with no
//! `lifecycle.shell_grace_secs` demotes no shell command on a clock, so neither is mentioned.
//! Left out on purpose: the per-turn output cap, which a `control:` block can change mid-session;
//! the delegation deadline, which `MURMUR_DELEGATION_TIMEOUT_SECS` can change per launch; whether
//! a demoted shell command's result is delivered, which the `--lifecycle-*` flags decide; the
//! work-folder ceiling, which bounds the work folder only for a session launched without
//! `--workdir`; and any wall-clock limit, because the runtime enforces none.

use murmur_artifact::{InferenceConfig, LifecycleConfig};

use crate::types::CapabilityPolicy;

/// What one agent session's model is told about its capsule. See the module documentation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CapsuleFacts {
    /// `capabilities.filesystem.read_only`, verbatim and in declaration order.
    pub(crate) read_only_paths: Vec<String>,
    /// `capabilities.network.allow`, verbatim and in declaration order.
    pub(crate) network_allow: Vec<String>,
    /// `inference.max_turns`; `None` sets no turn limit.
    pub(crate) max_turns: Option<u32>,
    /// `inference.max_session_tokens`.
    pub(crate) max_session_tokens: Option<u64>,
    /// The limits on what the runtime itself spawns. `None` on `transport: process`: the harness
    /// is spawned without the session's rlimits and runs its own commands outside them.
    pub(crate) dispatch_limits: Option<DispatchLimits>,
    /// Whether `capabilities.shell.allow` names a shell interpreter. The interpreter's tool runs
    /// its `command` through `-c` with `$MURMUR_RUNTIME_DIR` set, so the variable expands there; a
    /// binary allowed on its own is handed its arguments unexpanded.
    pub(crate) shell_interpreter: bool,
    /// The staged tools whose guest is preopened at `murmur-runtime/`, sorted
    /// ([`crate::runtime_dir::runtime_dir_preopened_tools`]). A tool pulled mid-session is not
    /// added: MURMUR.md lists the current set.
    pub(crate) runtime_dir_tools: Vec<String>,
}

/// The limits the runtime applies to every shell command and native tool it spawns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DispatchLimits {
    /// `HostResourceLimits::memory_bytes`: `RLIMIT_AS` (`RLIMIT_DATA` on macOS) per command.
    pub(crate) memory_bytes: u64,
    /// `HostResourceLimits::cpu_seconds`: `RLIMIT_CPU` per command. `None` sets none.
    pub(crate) cpu_seconds: Option<u64>,
    /// `lifecycle.shell_grace_secs`, when the model's tool list holds a runtime shell tool
    /// (`capabilities.shell.allow` is not empty). `None` without a shell tool and without a
    /// declared grace: either way no command is demoted on a clock.
    pub(crate) shell_grace_secs: Option<u64>,
}

impl CapsuleFacts {
    /// The facts for an agent session staged from `policy`, `inference` and the resolved
    /// `lifecycle`, whose staged tools preopened at `murmur-runtime/` are `runtime_dir_tools`. The
    /// `--lifecycle-*` overrides change neither field read from `lifecycle`.
    pub(crate) fn resolve(
        policy: &CapabilityPolicy,
        inference: &InferenceConfig,
        lifecycle: &LifecycleConfig,
        mut runtime_dir_tools: Vec<String>,
    ) -> Self {
        runtime_dir_tools.sort();
        runtime_dir_tools.dedup();
        let dispatch_limits = (inference.transport != "process").then(|| DispatchLimits {
            memory_bytes: policy.resources.memory_bytes,
            cpu_seconds: policy.resources.cpu_seconds,
            shell_grace_secs: lifecycle
                .shell_grace_secs
                .filter(|_| !policy.shell_allow.is_empty()),
        });
        Self {
            read_only_paths: policy.read_only_paths.clone(),
            network_allow: policy.network_allow.clone(),
            max_turns: inference.max_turns,
            max_session_tokens: inference.max_session_tokens,
            dispatch_limits,
            shell_interpreter: policy
                .shell_allow
                .iter()
                .any(|binary| crate::shell::is_shell_interpreter(binary)),
            runtime_dir_tools,
        }
    }

    /// Each limit the runtime enforces on this session, as a sentence fragment, in a fixed order:
    /// turns, session tokens, per-command resources, shell demotion. The
    /// `[Capsule]` block and the process prompt join them into one sentence; MURMUR.md lists
    /// them.
    pub(crate) fn limit_parts(&self) -> Vec<String> {
        let mut parts: Vec<String> = self
            .max_turns
            .map(|turns| format!("{turns} turns per task"))
            .into_iter()
            .collect();
        if let Some(tokens) = self.max_session_tokens {
            parts.push(format!("{tokens} tokens per session"));
        }
        if let Some(limits) = self.dispatch_limits {
            let memory = render_bytes(limits.memory_bytes);
            parts.push(match limits.cpu_seconds {
                Some(cpu) => format!("{memory} memory and {cpu} s CPU time per command"),
                None => format!("{memory} memory per command"),
            });
            if let Some(grace) = limits.shell_grace_secs {
                parts.push(format!(
                    "a shell command still running after {grace} s moves to the background"
                ));
            }
        }
        parts
    }

    /// [`Self::limit_parts`] as one sentence, or `none` when the session declares no limit the
    /// text can state (a `transport: process` capsule with neither turn nor token limit).
    pub(crate) fn limits_sentence(&self) -> String {
        let parts = self.limit_parts();
        if parts.is_empty() {
            "none".to_string()
        } else {
            format!("{}.", parts.join("; "))
        }
    }

    /// `capabilities.network.allow` as a list, or `none`.
    pub(crate) fn network_allowlist(&self) -> String {
        if self.network_allow.is_empty() {
            "none".to_string()
        } else {
            self.network_allow.join(", ")
        }
    }
}

/// The work-folder ceiling as a limit part, in the form [`CapsuleFacts::limit_parts`] renders,
/// naming what `WorkdirGuard` measures against it: the work folder, the commands' `$HOME` and
/// their `$TMPDIR` together. MURMUR.md alone states it.
pub(crate) fn work_folder_ceiling_part(workdir_max_bytes: u64) -> String {
    format!(
        "{} across the work folder, $HOME and $TMPDIR",
        render_bytes(workdir_max_bytes)
    )
}

/// `bytes` as a whole number of GiB or MiB when it is one, else as bytes.
fn render_bytes(bytes: u64) -> String {
    const MIB: u64 = 1 << 20;
    const GIB: u64 = 1 << 30;
    if bytes != 0 && bytes.is_multiple_of(GIB) {
        format!("{} GiB", bytes / GIB)
    } else if bytes != 0 && bytes.is_multiple_of(MIB) {
        format!("{} MiB", bytes / MIB)
    } else {
        format!("{bytes} bytes")
    }
}

/// An http inference block with every optional field left out.
#[cfg(test)]
pub(crate) fn test_http_inference() -> InferenceConfig {
    InferenceConfig {
        transport: "http".into(),
        model: "test-model".into(),
        driver: Some(murmur_artifact::InferenceDriver {
            artifact: "test-driver".into(),
            config: None,
        }),
        command: None,
        compaction: None,
        system_prompt: None,
        system_prompt_file: None,
        system_prompt_artifact: None,
        max_turns: Some(10),
        max_tokens: None,
        max_session_tokens: None,
        tool_refresh: murmur_artifact::ToolRefresh::Compaction,
        alternates: Vec::new(),
        retry: murmur_artifact::InferenceRetry::default(),
    }
}

/// The facts of an http capsule declaring nothing optional.
#[cfg(test)]
pub(crate) fn test_default_facts() -> CapsuleFacts {
    CapsuleFacts::resolve(
        &CapabilityPolicy::default(),
        &test_http_inference(),
        &LifecycleConfig::default(),
        Vec::new(),
    )
}

/// An http capsule declaring every limit: `max_turns: 7`, `max_session_tokens: 50000`, 1 GiB
/// memory, 120 s CPU, a 512 MiB `workdir_max_bytes`, `shell_grace_secs: 30` and `shell.allow:
/// [bash]`.
#[cfg(test)]
pub(crate) fn test_enforced_limits() -> (CapabilityPolicy, InferenceConfig, LifecycleConfig) {
    let policy = CapabilityPolicy {
        shell_allow: vec!["bash".to_string()],
        resources: crate::resources::HostResourceLimits {
            memory_bytes: 1_073_741_824,
            cpu_seconds: Some(120),
            workdir_max_bytes: 536_870_912,
            ..crate::resources::HostResourceLimits::resolve(None)
        },
        ..CapabilityPolicy::default()
    };
    let inference = InferenceConfig {
        max_turns: Some(7),
        max_session_tokens: Some(50_000),
        ..test_http_inference()
    };
    let lifecycle = LifecycleConfig {
        shell_grace_secs: Some(30),
        ..LifecycleConfig::default()
    };
    (policy, inference, lifecycle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capsule_facts_bytes_render_in_the_largest_whole_unit() {
        assert_eq!(render_bytes(2 << 30), "2 GiB");
        assert_eq!(render_bytes(512 << 20), "512 MiB");
        assert_eq!(render_bytes(1536 << 20), "1536 MiB");
        assert_eq!(render_bytes(1000), "1000 bytes");
        assert_eq!(render_bytes(0), "0 bytes");
    }

    #[test]
    fn capsule_facts_of_a_capsule_declaring_nothing_optional() {
        let facts = test_default_facts();
        assert_eq!(
            facts.limits_sentence(),
            "10 turns per task; 2 GiB memory per command."
        );
        assert_eq!(facts.network_allowlist(), "none");
        assert!(facts.read_only_paths.is_empty());
    }

    /// A shell grant with no declared grace demotes nothing on a clock, so it adds no limit.
    #[test]
    fn capsule_facts_of_a_shell_grant_without_a_grace_state_no_demotion() {
        let policy = CapabilityPolicy {
            shell_allow: vec!["bash".to_string()],
            ..CapabilityPolicy::default()
        };
        let facts = CapsuleFacts::resolve(
            &policy,
            &test_http_inference(),
            &LifecycleConfig::default(),
            Vec::new(),
        );
        assert_eq!(facts.dispatch_limits.unwrap().shell_grace_secs, None);
        assert_eq!(
            facts.limits_sentence(),
            "10 turns per task; 2 GiB memory per command."
        );

        // A grace with no shell grant has no shell tool to apply to.
        let facts = CapsuleFacts::resolve(
            &CapabilityPolicy::default(),
            &test_http_inference(),
            &LifecycleConfig {
                shell_grace_secs: Some(5),
                ..LifecycleConfig::default()
            },
            Vec::new(),
        );
        assert_eq!(facts.dispatch_limits.unwrap().shell_grace_secs, None);
    }

    /// Every value is the one the runtime enforces, read from the staged policy, inference
    /// block and lifecycle.
    #[test]
    fn capsule_facts_equal_the_enforced_values() {
        let (policy, inference, lifecycle) = test_enforced_limits();
        let facts = CapsuleFacts::resolve(&policy, &inference, &lifecycle, Vec::new());
        assert_eq!(
            facts.dispatch_limits,
            Some(DispatchLimits {
                memory_bytes: policy.resources.memory_bytes,
                cpu_seconds: policy.resources.cpu_seconds,
                shell_grace_secs: lifecycle.shell_grace_secs,
            })
        );
        assert_eq!(facts.max_turns, inference.max_turns);
        assert_eq!(facts.max_session_tokens, inference.max_session_tokens);
        assert_eq!(
            facts.limits_sentence(),
            "7 turns per task; 50000 tokens per session; 1 GiB memory and 120 s CPU time per \
             command; a shell command still running after 30 s moves to the background."
        );
        assert_eq!(
            work_folder_ceiling_part(policy.resources.workdir_max_bytes),
            "512 MiB across the work folder, $HOME and $TMPDIR"
        );
    }

    /// The harness of a process capsule is spawned without the session's rlimits, and its own
    /// commands are neither bounded nor demoted by the runtime.
    #[test]
    fn capsule_facts_of_a_process_capsule_carry_turns_and_tokens_only() {
        let policy = CapabilityPolicy {
            shell_allow: vec!["bash".to_string()],
            ..CapabilityPolicy::default()
        };
        let inference = InferenceConfig {
            transport: "process".into(),
            max_session_tokens: Some(9_000),
            ..test_http_inference()
        };
        let facts =
            CapsuleFacts::resolve(&policy, &inference, &LifecycleConfig::default(), Vec::new());
        assert_eq!(facts.dispatch_limits, None);
        assert_eq!(
            facts.limits_sentence(),
            "10 turns per task; 9000 tokens per session."
        );
    }

    /// An absent `inference.max_turns` sets no turn limit, so none is stated; a process capsule
    /// with no turn or token limit states `none` rather than an empty sentence.
    #[test]
    fn capsule_facts_state_no_turn_limit_when_none_is_set() {
        let http = InferenceConfig {
            max_turns: None,
            ..test_http_inference()
        };
        let facts = CapsuleFacts::resolve(
            &CapabilityPolicy::default(),
            &http,
            &LifecycleConfig::default(),
            Vec::new(),
        );
        assert!(!facts.limits_sentence().contains("turns"));
        let process = InferenceConfig {
            transport: "process".into(),
            ..http
        };
        let facts = CapsuleFacts::resolve(
            &CapabilityPolicy::default(),
            &process,
            &LifecycleConfig::default(),
            Vec::new(),
        );
        assert_eq!(facts.limits_sentence(), "none");
    }

    #[test]
    fn capsule_facts_keep_declaration_order() {
        let policy = CapabilityPolicy {
            network_allow: vec!["b.example".to_string(), "a.example:443".to_string()],
            read_only_paths: vec!["src".to_string(), "Cargo.lock".to_string()],
            ..CapabilityPolicy::default()
        };
        let facts = CapsuleFacts::resolve(
            &policy,
            &test_http_inference(),
            &LifecycleConfig::default(),
            Vec::new(),
        );
        assert_eq!(facts.network_allowlist(), "b.example, a.example:443");
        assert_eq!(facts.read_only_paths, ["src", "Cargo.lock"]);
    }
}
