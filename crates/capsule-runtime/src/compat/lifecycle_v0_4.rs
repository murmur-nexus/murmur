//! COMPAT-SHIM: murmur:hook/lifecycle@0.4.0 (and @0.3.0, @0.2.0) shell-event
//! added:       0.5.0
//! accepts:     the pre-@0.5.0 eight-field `shell-event` shape, shared by every hook
//!              compiled against @0.2.0, @0.3.0 or @0.4.0
//! remove-when: no published hook artifact still targets @0.2.0, @0.3.0 or @0.4.0
//!              (check the registry first), or at the next major bump — whichever
//!              comes first
//! ref:         card 4ccaec63
//!
//! `murmur:hook` went `0.4.0 → 0.5.0` because `shell-event` gained a `binary` field —
//! the canonicalized path of the program the shell tool actually invoked, which the
//! record previously omitted entirely (it carried only `command`, the argument list).
//! Adding a field to an existing `record` is always a major bump: the canonical ABI is
//! positional and `TypedFunc::typed` checks a component function structurally, so
//! lowering the 9-field record into a pre-@0.5.0 hook's `on-shell` fails the type check
//! outright rather than truncating (the same failure mode the `lifecycle_v0_3` header
//! records empirically for `hook-output`). See `wit/VERSIONING.md`.
//!
//! `shell-event` is shape-identical across @0.2.0, @0.3.0 and @0.4.0, so this one twin
//! serves all three legacy tiers: the host sends them [`ShellEventV04`] instead, and
//! they keep receiving shell events exactly as they did before this bump — no rebuild.
//! Those hooks simply never learn which binary ran, which is correct: `binary` is a
//! @0.5.0 capability. See `COMPAT_SHIMS.md` at the repo root for the full inventory.

/// Versioned instance export name a hook compiled against `murmur:hook@0.4.0` still
/// carries in its component-type section. (`@0.3.0`'s and `@0.2.0`'s names live in
/// [`super::lifecycle_v0_3::IFACE_NAME`] and [`super::lifecycle_v0_2::IFACE_NAME`].)
pub(crate) const IFACE_NAME: &str = "murmur:hook/lifecycle@0.4.0";

/// The pre-@0.5.0 shape of `shell-event`, hand-derived because bindgen only ever
/// generates the *current* (`@0.5.0`, 9-field) record.
///
/// Field order and types match the retired WIT exactly — `TypedFunc::typed` compares
/// structurally, so a reordering here is a silent wire break. `Lower` only: the host
/// builds and sends one, it never lifts one back.
#[derive(wasmtime::component::ComponentType, wasmtime::component::Lower)]
#[component(record)]
pub(crate) struct ShellEventV04 {
    pub(crate) turn: u32,
    pub(crate) command: String,
    #[component(name = "exit-code")]
    pub(crate) exit_code: i32,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    #[component(name = "stdout-bytes")]
    pub(crate) stdout_bytes: u64,
    #[component(name = "stderr-bytes")]
    pub(crate) stderr_bytes: u64,
    #[component(name = "duration-ms")]
    pub(crate) duration_ms: u64,
}
