wit_bindgen::generate!({
    path: "../../../../../../capsule-runtime/wit/guest",
    world: "capsule",
    generate_all,
});

struct GraduationCapsule;

const INPUT_JSONL: &str = "\
{\"id\":1,\"message\":\"alpha\"}\n\
{\"id\":2,\"message\":\"beta\"}\n\
{\"id\":3,\"message\":\"gamma\"}\n\
{\"id\":4,\"message\":\"delta\"}\n\
{\"id\":5,\"message\":\"epsilon\"}\n";

impl exports::murmur::capsule::run::Guest for GraduationCapsule {
    fn run() {
        if let Err(error) = std::fs::write("input.jsonl", INPUT_JSONL) {
            eprintln!("failed to write input.jsonl: {error}");
            return;
        }

        let input = murmur::tool::run::ToolInput {
            data: Some("input.jsonl".to_string()),
            log_path: None,
        };

        let output = match murmur::tool_registry::invoke::invoke("jsonl-line-count", &input) {
            Ok(result) => {
                let fallback = result
                    .summary
                    .clone()
                    .unwrap_or_else(|| "tool produced no summary".to_string());

                match result.status {
                    murmur::tool::run::Status::Passed => result.data.unwrap_or(fallback),
                    murmur::tool::run::Status::Failed | murmur::tool::run::Status::Error => {
                        fallback
                    }
                }
            }
            Err(error) => error,
        };

        if let Err(error) = std::fs::create_dir_all("./out") {
            eprintln!("failed to create output directory: {error}");
            return;
        }

        if let Err(error) = std::fs::write("./out/result.txt", output) {
            eprintln!("failed to write output file: {error}");
        }
    }
}

export!(GraduationCapsule);
