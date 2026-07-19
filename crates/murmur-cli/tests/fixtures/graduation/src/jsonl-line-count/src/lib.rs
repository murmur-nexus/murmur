wit_bindgen::generate!({
    path: "../../../../../../capsule-runtime/wit/guest",
    world: "tool",
    generate_all,
});

struct JsonlLineCount;

impl exports::murmur::tool::run::Guest for JsonlLineCount {
    fn run(input: exports::murmur::tool::run::ToolInput) -> exports::murmur::tool::run::ToolResult {
        let Some(path) = input.data else {
            return exports::murmur::tool::run::ToolResult {
                status: exports::murmur::tool::run::Status::Error,
                summary: Some("missing input path in tool-input.data".to_string()),
                data: None,
                data_path: None,
                truncated: false,
                metadata: Vec::new(),
            };
        };

        match std::fs::read_to_string(&path) {
            Ok(content) => {
                let count = content
                    .lines()
                    .filter(|line| !line.trim().is_empty())
                    .count();
                let count_text = count.to_string();

                exports::murmur::tool::run::ToolResult {
                    status: exports::murmur::tool::run::Status::Passed,
                    summary: Some(format!("{count} lines")),
                    data: Some(count_text),
                    data_path: None,
                    truncated: false,
                    metadata: Vec::new(),
                }
            }
            Err(error) => exports::murmur::tool::run::ToolResult {
                status: exports::murmur::tool::run::Status::Error,
                summary: Some(format!("failed to read '{path}': {error}")),
                data: None,
                data_path: None,
                truncated: false,
                metadata: Vec::new(),
            },
        }
    }
}

export!(JsonlLineCount);
