wit_bindgen::generate!({
    path: "../../../../../../capsule-runtime/wit/guest",
    world: "capsule",
    generate_all,
});

use std::time::Duration;

struct Capsule;

/// A run that lasts eight seconds and then leaves `out/done.txt`, so a test can end it partway
/// through and tell a finished run from an interrupted one.
impl exports::murmur::capsule::run::Guest for Capsule {
    fn run() {
        std::thread::sleep(Duration::from_secs(8));
        std::fs::create_dir_all("./out").expect("create output directory");
        std::fs::write("./out/done.txt", "done").expect("write the marker");
    }
}

export!(Capsule);
