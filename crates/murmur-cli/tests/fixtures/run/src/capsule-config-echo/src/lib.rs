// Invokes the config-reporting tool and publishes what it reported into the session workdir, the
// only place the test harness can read. The capsule holds no config of its own — `config:` is
// declared on an artifact entry, and a capsule is not one — so every byte written out came back
// over the tool boundary.
//
// Two tool names are attempted, not one: the scoping scenario declares a separate `config:` block
// on each of two entries, and each result is written to its own file. A manifest that declares
// only the first leaves `SECOND` uninvokable, which the runtime reports as an error rather than a
// trap, so that run simply writes no second file.
wit_bindgen::generate!({
    path: "../../../../../../capsule-runtime/wit/guest",
    world: "capsule",
    generate_all,
});

/// The artifact the single-tool scenarios declare; its result lands in `out/result.txt`.
const FIRST: &str = "config-echo";

/// The second artifact the scoping scenario declares; its result lands in `out/result-b.txt`.
const SECOND: &str = "config-echo-b";

struct Capsule;

impl exports::murmur::capsule::run::Guest for Capsule {
    fn run() {
        std::fs::create_dir_all("./out").expect("create output directory");

        let input = murmur::tool::run::ToolInput {
            data: None,
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
