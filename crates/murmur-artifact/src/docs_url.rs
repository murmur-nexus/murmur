//! The reference section of the published docs site. Every docs link `mur` and the capsule
//! runtime print builds from [`docs_reference_url!`], so the host and path prefix are written in
//! one place.

/// The reference section of the published docs site, as a `&'static str` usable in `const`.
///
/// With no argument it expands to the base, with no trailing slash. With a string literal it
/// expands to the base, a `/`, and that literal, e.g. `docs_reference_url!("diagnostics/")`.
///
/// The agent-card extension URIs in `capsule-runtime` derive from this macro, but clients match
/// on them as identifiers: a change to the base renames those extensions.
#[macro_export]
macro_rules! docs_reference_url {
    () => {
        "https://docs.murmur.nexus/reference"
    };
    ($path:literal) => {
        concat!($crate::docs_reference_url!(), "/", $path)
    };
}

/// The reference section of the published docs site, with no trailing slash.
pub const DOCS_REFERENCE_URL: &str = docs_reference_url!();

/// The link to a diagnostic code's entry on the diagnostics reference page. The anchor is the
/// code lowercased, which is the `{ #… }` id each entry carries in `diagnostics.md`.
pub fn diagnostic_link(code: &str) -> String {
    format!(
        "{}#{}",
        docs_reference_url!("diagnostics/"),
        code.to_lowercase()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_base_and_diagnostic_link_are_on_the_served_path() {
        assert_eq!(DOCS_REFERENCE_URL, "https://docs.murmur.nexus/reference");
        assert_eq!(
            diagnostic_link("W-SEC-024"),
            "https://docs.murmur.nexus/reference/diagnostics/#w-sec-024"
        );
    }
}
