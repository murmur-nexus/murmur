wit_bindgen::generate!({
    path: "../../../../../../capsule-runtime/wit/guest",
    world: "tool",
    generate_all,
});

struct RequestInputTool;

impl exports::murmur::tool::run::Guest for RequestInputTool {
    fn run(input: exports::murmur::tool::run::ToolInput) -> exports::murmur::tool::run::ToolResult {
        let prompt = input
            .data
            .unwrap_or_else(|| "Which option do you prefer?".to_string());

        let answer = murmur::task::task::request_input(&prompt);

        exports::murmur::tool::run::ToolResult {
            status: exports::murmur::tool::run::Status::Passed,
            summary: Some(format!("user answered: {answer}")),
            data: Some(answer),
            data_path: None,
            truncated: false,
            metadata: Vec::new(),
        }
    }
}

export!(RequestInputTool);
