wit_bindgen::generate!({
    path: "../../../../../../capsule-runtime/wit/guest",
    world: "capsule",
    generate_all,
});

struct Capsule;

impl exports::murmur::capsule::run::Guest for Capsule {
    fn run() {
        std::fs::write("../../escape.txt", "escaped").expect("escape write should be denied");
    }
}

export!(Capsule);
