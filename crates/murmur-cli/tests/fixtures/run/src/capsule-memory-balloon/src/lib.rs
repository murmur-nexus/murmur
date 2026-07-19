wit_bindgen::generate!({
    path: "../../../../../../capsule-runtime/wit/guest",
    world: "capsule",
    generate_all,
});

struct Capsule;

/// Grows its own linear memory without bound until the host's resource limiter stops it.
///
/// Each chunk is retained (never dropped) so the allocator must keep asking wasm for more
/// linear memory via `memory.grow` rather than recycling a freed block. Every page is
/// touched so the growth is real rather than a lazily-reserved mapping. The store's
/// limiter denies the first `memory.grow` past `capabilities.limits.memory_bytes`, which
/// traps the guest here.
impl exports::murmur::capsule::run::Guest for Capsule {
    fn run() {
        const CHUNK_BYTES: usize = 8 * 1024 * 1024;
        const WASM_PAGE_BYTES: usize = 65_536;

        let mut retained: Vec<Vec<u8>> = Vec::new();
        loop {
            let mut chunk = vec![0u8; CHUNK_BYTES];
            for offset in (0..chunk.len()).step_by(WASM_PAGE_BYTES) {
                chunk[offset] = 1;
            }
            std::hint::black_box(&chunk);
            retained.push(chunk);
        }
    }
}

export!(Capsule);
