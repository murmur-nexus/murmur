wit_bindgen::generate!({
    path: "../../../../../../capsule-runtime/wit/guest",
    world: "tool",
    generate_all,
});

struct EchoTool;

impl exports::murmur::tool::run::Guest for EchoTool {
    fn run(input: exports::murmur::tool::run::ToolInput) -> exports::murmur::tool::run::ToolResult {
        exports::murmur::tool::run::ToolResult {
            status: exports::murmur::tool::run::Status::Passed,
            summary: Some("ok".to_string()),
            data: input.data,
            data_path: None,
            truncated: false,
            metadata: Vec::new(),
        }
    }
}

export!(EchoTool);
