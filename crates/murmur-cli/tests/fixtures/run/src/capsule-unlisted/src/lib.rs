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

        let err = match murmur::tool_registry::invoke::invoke("undeclared-tool", &input) {
            Ok(_) => "unexpected-ok".to_string(),
            Err(err) => err,
        };

        std::fs::create_dir_all("./out").expect("create output directory");
        std::fs::write("./out/result.txt", err).expect("write result file");
    }
}

export!(Capsule);
