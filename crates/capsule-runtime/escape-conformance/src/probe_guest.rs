//! The probe guest the `wasm-guest` route calls, carried inside the gate binary.
//!
//! `probe-guest/` holds its source, its `murmur.yaml` and the built component. The gate packs the
//! embedded copies with `mur build` once per run, as it packs the driver, and installs the
//! artifact into the project store of each case that runs on [`crate::cases::Route::WasmGuest`].
//! The guest only hands its input back as an `escape-probe` request; the probe bodies run in the
//! guest worker's host side, which only a `mur` built with the test-only `escape-probe` feature
//! has (`capsule-runtime/src/guest_worker/escape_probe.rs`).

use std::path::{Path, PathBuf};

/// The probe guest's artifact name, as `probe-guest/murmur.yaml` declares it, and the name the
/// tool bridge offers it under.
pub const PROBE_GUEST_NAME: &str = "escape-conformance-probe-guest";
/// The full name it is filed under once installed from its `.mur.zip` file, in the `local` scope.
pub const PROBE_GUEST_ARTIFACT: &str = "local/escape-conformance-probe-guest";
/// The probe guest's artifact version, as `probe-guest/murmur.yaml` declares it.
pub const PROBE_GUEST_VERSION: &str = "0.1.0";
/// The committed component, built from `probe-guest/src/lib.rs`.
pub const PROBE_GUEST_WASM: &[u8] =
    include_bytes!("../probe-guest/escape_conformance_probe_guest.wasm");
/// The probe guest's own manifest. Its `requires_files` names the component.
pub const PROBE_GUEST_MANIFEST: &str = include_str!("../probe-guest/murmur.yaml");

const PROBE_GUEST_WASM_FILE: &str = "escape_conformance_probe_guest.wasm";

/// Packs the embedded probe guest with `<mur> build` into
/// `<work_root>/escape-conformance-probe-guest-<version>.mur.zip`, which it returns. Errors carry
/// `mur`'s stderr.
pub fn build_artifact(mur: &Path, work_root: &Path) -> Result<PathBuf, String> {
    crate::driver_artifact::pack(
        mur,
        &work_root.join("probe-guest-artifact"),
        &work_root.join(format!("{PROBE_GUEST_NAME}-{PROBE_GUEST_VERSION}.mur.zip")),
        PROBE_GUEST_MANIFEST,
        PROBE_GUEST_WASM_FILE,
        PROBE_GUEST_WASM,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The constants name what the embedded manifest declares, and it is a tool.
    #[test]
    fn the_constants_match_the_embedded_manifest() {
        let lines: Vec<&str> = PROBE_GUEST_MANIFEST.lines().collect();
        assert!(lines.contains(&format!("name: {PROBE_GUEST_NAME}").as_str()));
        assert!(lines.contains(&format!("version: {PROBE_GUEST_VERSION}").as_str()));
        assert!(lines.contains(&"runtime: tool"));
        assert!(lines.contains(&format!("  - {PROBE_GUEST_WASM_FILE}").as_str()));
        assert_eq!(PROBE_GUEST_ARTIFACT, format!("local/{PROBE_GUEST_NAME}"));
    }
}
