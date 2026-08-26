// A tool that reports the operator config it was handed, byte for byte, so the test asserts on
// what the *guest* observes rather than on a host-side stand-in.
//
// The absent case is a value rather than a failure: `absent` and an empty JSON object are
// different outcomes, and the whole point of an undeclared `config:` is that no variable exists
// at all.
wit_bindgen::generate!({
    path: "../../../../../../capsule-runtime/wit/guest",
    world: "tool",
    generate_all,
});

/// The one variable the runtime delivers a `config:` block in.
const ARTIFACT_CONFIG: &str = "MURMUR_ARTIFACT_CONFIG";

/// Reported when the variable is not in the environment, which is the whole of the default case.
const ABSENT: &str = "absent";

struct ConfigEcho;

impl exports::murmur::tool::run::Guest for ConfigEcho {
    fn run(_input: exports::murmur::tool::run::ToolInput) -> exports::murmur::tool::run::ToolResult {
        let summary = std::env::var(ARTIFACT_CONFIG).unwrap_or_else(|_| ABSENT.to_string());

        exports::murmur::tool::run::ToolResult {
            status: exports::murmur::tool::run::Status::Passed,
            summary: Some(summary),
            data: None,
            data_path: None,
            truncated: false,
            metadata: Vec::new(),
        }
    }
}

export!(ConfigEcho);
