//! The one place in the tree that declares a `#[global_allocator]`.
//!
//! `mur` links wasmtime and hosts every Cranelift compilation and every `Store` a session
//! creates, so the allocator under it is a measured choice rather than a default. The arms
//! below are selected by Cargo feature; `scripts/alloc-bench.sh` compares them.
//!
//! Precedence, and why there is any: `--all-features` turns on both candidate features at
//! once, and two `#[global_allocator]` items in one crate do not link. `jemalloc` therefore
//! wins over `mimalloc` wherever both are on. Rejecting the combination with `compile_error!`
//! would break every `--all-features` build instead.
//!
//! `default` is empty, so the shipped `mur` takes the third arm: on glibc neither candidate is
//! faster by enough to pay for linking a C allocator into the binary that hosts every sandbox
//! `fork`. Both features stay so re-running the comparison on another host is a flag rather
//! than a branch.
//!
//! That third arm names `std::alloc::System` explicitly rather than declaring nothing, so that
//! "which allocator is this binary on" has one answer in every configuration and
//! `ALLOCATOR_NAME` never has to describe an absence.

#[cfg(feature = "jemalloc")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[cfg(all(feature = "mimalloc", not(feature = "jemalloc")))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(not(any(feature = "jemalloc", feature = "mimalloc")))]
#[global_allocator]
static GLOBAL: std::alloc::System = std::alloc::System;

/// Names the arm that compiled, in the same precedence order as the arms above.
///
/// `mur-alloc-bench` prints this beside every timing line, so no measurement can be read
/// without knowing which allocator produced it. `mur` carries the constant and does not read
/// it, which is what the `dead_code` allowance is for: the value belongs to the module, not to
/// whichever binary happens to include it.
#[allow(dead_code)]
pub(crate) const ALLOCATOR_NAME: &str = if cfg!(feature = "jemalloc") {
    "jemalloc"
} else if cfg!(feature = "mimalloc") {
    "mimalloc"
} else {
    "system"
};
