wit_bindgen::generate!({
    path: "../../../../../../capsule-runtime/wit/guest",
    world: "tool",
    generate_all,
});

struct StreamingDriver;

impl exports::murmur::tool::run::Guest for StreamingDriver {
    fn run(
        _input: exports::murmur::tool::run::ToolInput,
    ) -> exports::murmur::tool::run::ToolResult {
        // Emit three text chunks to exercise the emit-chunk host function.
        murmur::text::chunks::emit_chunk("chunk_one ");
        murmur::text::chunks::emit_chunk("chunk_two ");
        murmur::text::chunks::emit_chunk("chunk_three");

        // Return a valid end_turn driver response (matches the capsule runtime's expected format).
        let response = concat!(
            r#"{"stop_reason":"end_turn","content":"#,
            r#"[{"type":"text","text":"chunk_one chunk_two chunk_three"}]}"#
        );

        exports::murmur::tool::run::ToolResult {
            status: exports::murmur::tool::run::Status::Passed,
            summary: Some("streaming driver done".to_string()),
            data: Some(response.to_string()),
            data_path: None,
            truncated: false,
            metadata: Vec::new(),
        }
    }
}

export!(StreamingDriver);
