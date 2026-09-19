// Invokes the gateway probe under its two installed names and publishes each report into the
// session workdir, the only place the test harness can read. The task text — the key marker,
// hex-encoded — is handed to each probe as its input, so the probe learns the marker through its
// input and never through its environment.
//
// `gateway-probe` is the entry the scenarios give a `gateway:`; `gateway-probe-b` is the same
// component installed under a second name without one. A manifest that declares only the first
// leaves the second uninvokable, which the runtime reports as an error rather than a trap, so that
// run simply writes no second file.
wit_bindgen::generate!({
    path: "../../../../../../capsule-runtime/wit/guest",
    world: "capsule",
    generate_all,
});

const FIRST: &str = "gateway-probe";
const SECOND: &str = "gateway-probe-b";

struct Capsule;

impl exports::murmur::capsule::run::Guest for Capsule {
    fn run() {
        std::fs::create_dir_all("./out").expect("create output directory");
        let task = std::fs::read_to_string("./task.md").unwrap_or_default();
        let input = murmur::tool::run::ToolInput {
            data: Some(task.trim().to_string()),
            log_path: None,
        };

        let summary = match murmur::tool_registry::invoke::invoke(FIRST, &input) {
            Ok(result) => result.summary.unwrap_or_else(|| "missing".to_string()),
            Err(err) => format!("invoke-failed: {err}"),
        };
        std::fs::write("./out/result.txt", summary).expect("write result file");

        if let Ok(result) = murmur::tool_registry::invoke::invoke(SECOND, &input) {
            let summary = result.summary.unwrap_or_else(|| "missing".to_string());
            std::fs::write("./out/result-b.txt", summary).expect("write second result file");
        }
    }
}

export!(Capsule);
