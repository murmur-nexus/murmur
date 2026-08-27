//! Host implementation of `murmur:runtime/tokens@0.3.0#count`.
//!
//! A hook that imports this interface measures text with the host's own
//! `cl100k_base` tokenizer — [`crate::agent::count_tokens`], the counter behind
//! the compaction trigger and [`crate::agent::ContextOccupancy`]. That shared
//! definition is the point: a hook sizing a payload against a budget and the
//! host enforcing that budget agree on what the payload costs, which two
//! independent tokenizers would not.
//!
//! Counting is pure computation — no side effect, no host resource, nothing to
//! withhold — so the import is registered on every hook linker unconditionally
//! and no capability grant gates it. See `wit/hook/tokens.wit`.

use wasmtime::component::Linker;

/// The versioned instance name the host provides `count` under. Hook components
/// that do not import it simply ignore the registration.
pub(crate) const TOKENS_IFACE_VERSIONED: &str = "murmur:runtime/tokens@0.3.0";

/// Register `murmur:runtime/tokens@0.3.0#count` on a hook linker.
///
/// Must be called on **every** linker a hook component instantiates against.
/// The `hook` world declares this import, so a linker missing it turns any hook
/// that actually calls `count` into an instantiation failure — the one outcome
/// a purely additive import must not produce.
pub(crate) fn add_tokens_to_linker<T: 'static>(linker: &mut Linker<T>) -> Result<(), String> {
    linker
        .instance(TOKENS_IFACE_VERSIONED)
        .map_err(|e| format!("failed to define {TOKENS_IFACE_VERSIONED}: {e}"))?
        .func_wrap(
            "count",
            |_store: wasmtime::StoreContextMut<'_, T>, (text,): (String,)| {
                Ok((u64::from(crate::agent::count_tokens(&text)),))
            },
        )
        .map_err(|e| format!("failed to register {TOKENS_IFACE_VERSIONED}#count: {e}"))?;
    Ok(())
}
