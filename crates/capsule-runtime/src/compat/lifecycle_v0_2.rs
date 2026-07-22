//! COMPAT-SHIM: murmur:hook/lifecycle@0.2.0
//! added:       0.3.0
//! accepts:     lifecycle interface v0.2.0 (superseded by v0.3.0)
//! remove-when: no published artifact still targets @0.2.0 (check registry first),
//!              or at the next major bump — whichever comes first
//! ref:         card bd8a67dc
//!
//! `murmur:hook` went `0.2.0 → 0.3.0` because `compaction-event` gained two
//! fields (`model`, `system-prompt`), which the canonical ABI cannot absorb
//! additively — see `wit/VERSIONING.md`. Every hook other than
//! `murmur-hook-compact` is unaffected by those fields, so the host keeps
//! loading `@0.2.0`-compiled hooks rather than forcing a fleet-wide rebuild —
//! it just sends them this 3-field twin of `compaction-event` instead. See
//! `COMPAT_SHIMS.md` at the repo root for the full shim inventory.

use crate::bindings::hook::exports::murmur::hook::lifecycle::Message;

/// Versioned instance export name a hook compiled against `murmur:hook@0.2.0`
/// still carries in its component-type section.
pub(crate) const IFACE_NAME: &str = "murmur:hook/lifecycle@0.2.0";

/// The `murmur:hook@0.2.0` shape of `compaction-event`, hand-derived because
/// bindgen only ever generates the *current* (`@0.3.0`, 5-field) record.
///
/// `TypedFunc::typed` checks a component function structurally — field order and
/// types, not names — so lowering the 5-field current `CompactionEvent` into a
/// `@0.2.0`-compiled hook's `on-compaction` fails the type check outright rather
/// than truncating. Sending this 3-field twin instead is what lets an
/// un-rebuilt hook keep receiving compaction events unchanged. `Lower` only:
/// the host builds and sends one, it never lifts one back.
#[derive(wasmtime::component::ComponentType, wasmtime::component::Lower)]
#[component(record)]
pub(crate) struct CompactionEventV02 {
    pub(crate) messages: Vec<Message>,
    #[component(name = "session-tokens")]
    pub(crate) session_tokens: u64,
    pub(crate) threshold: f64,
}
