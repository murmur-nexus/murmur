wit_bindgen::generate!({
    path: "../../../../../../capsule-runtime/wit/guest",
    world: "capsule",
    generate_all,
});

struct Capsule;

impl exports::murmur::capsule::run::Guest for Capsule {
    fn run() {
        let outcome = match std::fs::write("../../outside.txt", "escape") {
            Ok(()) => "unexpected-ok".to_string(),
            Err(_) => "blocked".to_string(),
        };

        std::fs::create_dir_all("./out").expect("create output directory");
        std::fs::write("./out/result.txt", outcome).expect("write result file");
    }
}

export!(Capsule);
