//! COMPAT-SHIM: murmur:hook/lifecycle@0.3.0 (and @0.2.0) hook-output
//! added:       0.4.0
//! accepts:     the pre-@0.4.0 four-case `hook-output` return shape, shared by every
//!              hook compiled against @0.2.0 or @0.3.0
//! remove-when: no published hook artifact still targets @0.2.0 or @0.3.0 (check the
//!              registry first), or at the next major bump — whichever comes first
//! ref:         card ac1e1848
//!
//! `murmur:hook` went `0.3.0 → 0.4.0` because `hook-output` gained a fifth case,
//! `reopen-task(string)` (the `on-task-end` control-return). `hook-output` is the
//! return type of every one of the nine lifecycle functions, so widening it changes
//! the wire shape of every export. `TypedFunc::typed` is structural and does **not**
//! admit variant subtyping across the call boundary — verified empirically: lifting a
//! four-case guest return against the five-case host type fails with "type mismatch
//! with results" (see the `v0_2_*`/`v0_3_*`/`compaction_hook_*` hook tests, which fail
//! under a bare additive change and pass once the host lifts pre-@0.4.0 hooks through
//! this twin).
//!
//! Both @0.2.0 and @0.3.0 carry the *same* four-case `hook-output` — the only shape
//! difference between those two versions is `compaction-event` (handled by the
//! separate [`super::lifecycle_v0_2`] shim). So this one twin serves both legacy
//! tiers: the host lifts their returns as [`HookOutputLegacy`] and widens the result
//! into the current [`HookOutput`]. A pre-@0.4.0 hook can never produce `reopen-task`,
//! which is exactly right — reopening is a @0.4.0 capability. See `COMPAT_SHIMS.md`
//! at the repo root for the full shim inventory.

use crate::bindings::hook::exports::murmur::hook::lifecycle::{HookOutput, Message, ToolManifest};

/// Versioned instance export name a hook compiled against `murmur:hook@0.3.0` still
/// carries in its component-type section. (`@0.2.0`'s name lives in
/// [`super::lifecycle_v0_2::IFACE_NAME`].)
pub(crate) const IFACE_NAME: &str = "murmur:hook/lifecycle@0.3.0";

/// The pre-@0.4.0 four-case `hook-output`, hand-derived because bindgen only ever
/// generates the *current* (`@0.4.0`, five-case) variant.
///
/// `Lift` only: the host reads one of these back from a pre-@0.4.0 hook's return, it
/// never sends one. Field order and case order match the retired WIT exactly, since
/// `TypedFunc::typed` checks a component function structurally.
#[derive(wasmtime::component::ComponentType, wasmtime::component::Lift)]
#[component(variant)]
pub(crate) enum HookOutputLegacy {
    #[component(name = "none")]
    None,
    #[component(name = "replace-context")]
    ReplaceContext(Vec<Message>),
    #[component(name = "write-manifests")]
    WriteManifests(Vec<ToolManifest>),
    #[component(name = "artifact")]
    Artifact(String),
}

impl From<HookOutputLegacy> for HookOutput {
    fn from(legacy: HookOutputLegacy) -> Self {
        match legacy {
            HookOutputLegacy::None => HookOutput::None,
            HookOutputLegacy::ReplaceContext(msgs) => HookOutput::ReplaceContext(msgs),
            HookOutputLegacy::WriteManifests(m) => HookOutput::WriteManifests(m),
            HookOutputLegacy::Artifact(s) => HookOutput::Artifact(s),
            // No `reopen-task`: a pre-@0.4.0 hook's `hook-output` has no such case.
        }
    }
}
