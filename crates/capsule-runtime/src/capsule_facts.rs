//! The manifest-fixed facts an agent session's model is told about its own capsule: where it may
//! write, where it may connect, and the limits the runtime enforces on it.
//!
//! One value, built at staging from the manifest as resolved there, feeds all three texts that
//! state them: the `[Capsule]` block (`transport: http`), the `transport: process` system prompt
//! and MURMUR.md. The limits are rendered for all three by [`CapsuleFacts::limit_parts`].
//!
//! Nothing here varies between launches of one manifest. Left out on purpose: the per-turn output
//! cap, which a `control:` block can change mid-session; the delegation deadline, which
//! `MURMUR_DELEGATION_TIMEOUT_SECS` can change per launch; and any wall-clock limit, because the
//! runtime enforces none.

use murmur_artifact::{InferenceConfig, LifecycleConfig};

use crate::types::CapabilityPolicy;

/// What one agent session's model is told about its capsule. See the module documentation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CapsuleFacts {
    /// `capabilities.filesystem.read_only`, verbatim and in declaration order.
    pub(crate) read_only_paths: Vec<String>,
    /// `capabilities.network.allow`, verbatim and in declaration order.
    pub(crate) network_allow: Vec<String>,
    /// `inference.max_turns`.
    pub(crate) max_turns: u32,
    /// `inference.max_session_tokens`.
    pub(crate) max_session_tokens: Option<u64>,
    /// The limits on what the runtime itself spawns. `None` on `transport: process`: the harness
    /// is spawned without the session's rlimits and runs its own commands outside them.
    pub(crate) dispatch_limits: Option<DispatchLimits>,
}

/// The limits the runtime applies to every shell command and native tool it spawns, and to the
/// work folder they write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DispatchLimits {
    /// `HostResourceLimits::memory_bytes`: `RLIMIT_AS` (`RLIMIT_DATA` on macOS) per command.
    pub(crate) memory_bytes: u64,
    /// `HostResourceLimits::cpu_seconds`: `RLIMIT_CPU` per command.
    pub(crate) cpu_seconds: u64,
    /// `HostResourceLimits::workdir_max_bytes`.
    pub(crate) workdir_max_bytes: u64,
    /// `lifecycle.shell_grace_secs`, when the model's tool list holds a runtime shell tool
    /// (`capabilities.shell.allow` is not empty).
    pub(crate) shell_grace_secs: Option<u64>,
}

impl CapsuleFacts {
    /// The facts for an agent session staged from `policy`, `inference` and the resolved
    /// `lifecycle`. The `--lifecycle-*` overrides change neither field read from `lifecycle`.
    pub(crate) fn resolve(
        policy: &CapabilityPolicy,
        inference: &InferenceConfig,
        lifecycle: &LifecycleConfig,
    ) -> Self {
        let dispatch_limits = (inference.transport != "process").then(|| DispatchLimits {
            memory_bytes: policy.resources.memory_bytes,
            cpu_seconds: policy.resources.cpu_seconds,
            workdir_max_bytes: policy.resources.workdir_max_bytes,
            shell_grace_secs: (!policy.shell_allow.is_empty())
                .then_some(lifecycle.shell_grace_secs),
        });
        Self {
            read_only_paths: policy.read_only_paths.clone(),
            network_allow: policy.network_allow.clone(),
            max_turns: inference.max_turns,
            max_session_tokens: inference.max_session_tokens,
            dispatch_limits,
        }
    }

    /// Each limit the runtime enforces on this session, as a sentence fragment, in a fixed order:
    /// turns, session tokens, per-command resources, the work folder, shell demotion. The
    /// `[Capsule]` block and the process prompt join them into one sentence; MURMUR.md lists
    /// them.
    pub(crate) fn limit_parts(&self) -> Vec<String> {
        let mut parts = vec![format!("{} turns per task", self.max_turns)];
        if let Some(tokens) = self.max_session_tokens {
            parts.push(format!("{tokens} tokens per session"));
        }
        if let Some(limits) = self.dispatch_limits {
            parts.push(format!(
                "{} memory and {} s CPU time per command",
                render_bytes(limits.memory_bytes),
                limits.cpu_seconds
            ));
            parts.push(format!(
                "{} in the work folder",
                render_bytes(limits.workdir_max_bytes)
            ));
            if let Some(grace) = limits.shell_grace_secs {
                parts.push(format!(
                    "a shell command still running after {grace} s moves to the background and \
                     its result arrives in a later message"
                ));
            }
        }
        parts
    }

    /// [`Self::limit_parts`] as one sentence.
    pub(crate) fn limits_sentence(&self) -> String {
        format!("{}.", self.limit_parts().join("; "))
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
        max_turns: 10,
        max_tokens: None,
        max_session_tokens: None,
        tool_refresh: murmur_artifact::ToolRefresh::Compaction,
        alternates: Vec::new(),
    }
}

/// The facts of an http capsule declaring nothing optional.
#[cfg(test)]
pub(crate) fn test_default_facts() -> CapsuleFacts {
    CapsuleFacts::resolve(
        &CapabilityPolicy::default(),
        &test_http_inference(),
        &LifecycleConfig::default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resources::HostResourceLimits;

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
            "10 turns per task; 2 GiB memory and 3600 s CPU time per command; 10 GiB in the work \
             folder."
        );
        assert_eq!(facts.network_allowlist(), "none");
        assert!(facts.read_only_paths.is_empty());
    }

    /// S3: every value is the one the runtime enforces, read from the staged policy, inference
    /// block and lifecycle.
    #[test]
    fn capsule_facts_equal_the_enforced_values() {
        let resources = HostResourceLimits {
            memory_bytes: 1_073_741_824,
            cpu_seconds: 120,
            workdir_max_bytes: 536_870_912,
            ..HostResourceLimits::resolve(None)
        };
        let policy = CapabilityPolicy {
            shell_allow: vec!["bash".to_string()],
            resources,
            ..CapabilityPolicy::default()
        };
        let inference = InferenceConfig {
            max_turns: 7,
            max_session_tokens: Some(50_000),
            ..test_http_inference()
        };
        let lifecycle = LifecycleConfig {
            shell_grace_secs: 30,
            ..LifecycleConfig::default()
        };
        let facts = CapsuleFacts::resolve(&policy, &inference, &lifecycle);
        assert_eq!(
            facts.dispatch_limits,
            Some(DispatchLimits {
                memory_bytes: policy.resources.memory_bytes,
                cpu_seconds: policy.resources.cpu_seconds,
                workdir_max_bytes: policy.resources.workdir_max_bytes,
                shell_grace_secs: Some(lifecycle.shell_grace_secs),
            })
        );
        assert_eq!(facts.max_turns, inference.max_turns);
        assert_eq!(facts.max_session_tokens, inference.max_session_tokens);
        assert_eq!(
            facts.limits_sentence(),
            "7 turns per task; 50000 tokens per session; 1 GiB memory and 120 s CPU time per \
             command; 512 MiB in the work folder; a shell command still running after 30 s moves \
             to the background and its result arrives in a later message."
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
        let facts = CapsuleFacts::resolve(&policy, &inference, &LifecycleConfig::default());
        assert_eq!(facts.dispatch_limits, None);
        assert_eq!(
            facts.limits_sentence(),
            "10 turns per task; 9000 tokens per session."
        );
    }

    #[test]
    fn capsule_facts_keep_declaration_order() {
        let policy = CapabilityPolicy {
            network_allow: vec!["b.example".to_string(), "a.example:443".to_string()],
            read_only_paths: vec!["src".to_string(), "Cargo.lock".to_string()],
            ..CapabilityPolicy::default()
        };
        let facts =
            CapsuleFacts::resolve(&policy, &test_http_inference(), &LifecycleConfig::default());
        assert_eq!(facts.network_allowlist(), "b.example, a.example:443");
        assert_eq!(facts.read_only_paths, ["src", "Cargo.lock"]);
    }
}
