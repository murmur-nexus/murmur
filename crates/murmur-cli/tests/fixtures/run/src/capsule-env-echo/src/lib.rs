wit_bindgen::generate!({
    path: "../../../../../../capsule-runtime/wit/guest",
    world: "capsule",
    generate_all,
});

/// Names the host test sets before invoking `mur run`, so `absent` in the output proves the
/// guest could not observe a variable that really was set in the runtime's own environment.
const PROBED_VARS: &[&str] = &["GITHUB_TOKEN", "MURMUR_TEST_ALLOWED_VAR"];

struct Capsule;

impl exports::murmur::capsule::run::Guest for Capsule {
    fn run() {
        let mut lines = Vec::new();
        for name in PROBED_VARS {
            let value = match std::env::var(name) {
                Ok(value) => format!("present:{value}"),
                Err(_) => "absent".to_string(),
            };
            lines.push(format!("{name}={value}"));
        }

        std::fs::create_dir_all("./out").expect("create output directory");
        std::fs::write("./out/result.txt", lines.join("\n")).expect("write result file");
    }
}

export!(Capsule);
