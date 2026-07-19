wit_bindgen::generate!({
    path: "../../../../../../capsule-runtime/wit/guest",
    world: "capsule",
    generate_all,
});

struct Capsule;

/// Spins in a pure-compute loop that never returns and never calls the host.
///
/// This is the "before: unkillable, after: bounded" fixture: with no epoch deadline the
/// host has no way to stop it and `mur run` hangs forever. `black_box` keeps LLVM from
/// deleting the loop body, and the loop's back-edge is where wasmtime's epoch check lands.
impl exports::murmur::capsule::run::Guest for Capsule {
    fn run() {
        let mut counter: u64 = 0;
        loop {
            counter = counter.wrapping_add(1);
            std::hint::black_box(counter);
        }
    }
}

export!(Capsule);
