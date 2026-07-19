wit_bindgen::generate!({
    path: "../../../../../../capsule-runtime/wit/guest",
    world: "capsule",
    generate_all,
});

use std::time::Duration;

struct Capsule;

impl exports::murmur::capsule::run::Guest for Capsule {
    fn run() {
        let input = std::fs::read_to_string("./task.md").expect("read task.md");
        std::thread::sleep(Duration::from_secs(2));

        std::fs::create_dir_all("./out").expect("create output directory");
        std::fs::write("./out/result.txt", input).expect("write result file");
    }
}

export!(Capsule);
