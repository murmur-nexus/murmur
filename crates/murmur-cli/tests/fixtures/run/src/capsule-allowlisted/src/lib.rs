wit_bindgen::generate!({
    path: "../../../../../../capsule-runtime/wit/guest",
    world: "capsule",
    generate_all,
});

struct Capsule;

impl exports::murmur::capsule::run::Guest for Capsule {
    fn run() {
        let input = murmur::tool::run::ToolInput {
            data: Some("hello".to_string()),
            log_path: None,
        };

        let result = murmur::tool_registry::invoke::invoke("echo-tool", &input)
            .expect("invoke should succeed");
        let summary = result.summary.unwrap_or_else(|| "missing".to_string());

        std::fs::create_dir_all("./out").expect("create output directory");
        std::fs::write("./out/result.txt", summary).expect("write result file");
    }
}

export!(Capsule);
