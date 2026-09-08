//! Timings for the two allocation bursts a session actually pays for, exposed for the
//! `mur-alloc-bench` harness that chose `mur`'s global allocator.
//!
//! Both functions build their engine through [`crate::runtime::build_engine`], the single
//! `Engine` every session runs every guest on. A hand-rolled second `Config` here would be
//! measuring a configuration nobody ships: `epoch_interruption` alone changes what Cranelift
//! emits for every loop back-edge and function entry.
//!
//! No wasmtime type appears in either signature. `murmur-cli` hosts the bench binary and must
//! not gain a direct `wasmtime` dependency to run it.
//!
//! Compiled only under the `alloc-bench` feature, so a normal build of the workspace carries
//! neither this module nor a second binary.

use std::hint::black_box;
use std::time::Instant;

use wasmtime::component::Component;
use wasmtime::Store;

use crate::runtime::build_engine;

/// Compiles `wasm` `iterations` times on one engine, returning the nanoseconds each compile
/// took. Dropping the compiled artifact happens outside the timed region.
///
/// Panics on a component that does not compile: this is a hand-run harness, and a failure here
/// makes every number taken after it meaningless rather than merely absent.
pub fn compile_component_nanos(wasm: &[u8], iterations: usize) -> Vec<u128> {
    let engine = build_engine().expect("bench engine builds");
    let mut per_compile = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let started = Instant::now();
        let component = Component::new(&engine, wasm).expect("fixture component compiles");
        per_compile.push(started.elapsed().as_nanos());
        drop(component);
    }
    per_compile
}

/// Creates and drops `iterations` stores on one engine, returning the total nanoseconds.
///
/// The store data is `()`. What varies with the allocator is wasmtime's own per-`Store`
/// allocation churn, not the size of whatever the host hangs off it, and a session's real
/// store state is reachable only from a staged session.
pub fn store_churn_nanos(iterations: usize) -> u128 {
    let engine = build_engine().expect("bench engine builds");
    let started = Instant::now();
    for _ in 0..iterations {
        let store = Store::new(&engine, ());
        black_box(&store);
    }
    started.elapsed().as_nanos()
}
