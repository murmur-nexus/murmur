// Probes, from the capsule ceiling, whether a tool's durable store is reachable without a grant.
// Two attempts: the guest path a granted artifact would use, and a traversal out of the workdir
// preopen towards where the store lives on the host. Both must be blocked — a capsule holds no
// artifact grant, so it holds no descriptor that names either path.
//
// Each attempt is recorded rather than trapped, so a hole shows up as `unexpected-ok` in the
// result file instead of as a crash that could be mistaken for containment working.
wit_bindgen::generate!({
    path: "../../../../../../capsule-runtime/wit/guest",
    world: "capsule",
    generate_all,
});

struct Capsule;

impl exports::murmur::capsule::run::Guest for Capsule {
    fn run() {
        let direct = attempt("state/probe.txt");
        let traversal = attempt("../../.murmur/state/probe.txt");

        std::fs::create_dir_all("./out").expect("create output directory");
        std::fs::write("./out/result.txt", format!("{direct} {traversal}"))
            .expect("write result file");
    }
}

fn attempt(path: &str) -> &'static str {
    match std::fs::write(path, "probe") {
        Ok(()) => "unexpected-ok",
        Err(_) => "blocked",
    }
}

export!(Capsule);
