// Invokes the state-writing tool and publishes what it reported into the session workdir, which
// is the only place the test harness can read. The capsule itself holds no state grant — the
// count it writes out came back over the tool boundary, not from a preopen of its own.
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

        let summary = match murmur::tool_registry::invoke::invoke("state-writer", &input) {
            Ok(result) => result.summary.unwrap_or_else(|| "missing".to_string()),
            Err(err) => format!("invoke-failed: {err}"),
        };

        std::fs::create_dir_all("./out").expect("create output directory");
        std::fs::write("./out/result.txt", summary).expect("write result file");
    }
}

export!(Capsule);
