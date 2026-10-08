//! Registry of `W-ROS-*` codes for non-fatal warnings from roster admission.
//!
//! `mur run --roster` and `mur doctor` print these about a roster that admission accepted. A
//! `W-ROS-*` code never refuses a roster and never changes an exit code: the formation still
//! launches and `mur doctor` still passes. Each code has a matching `#w-ros-nnn` anchor on the
//! diagnostics reference page, so callers append [`roster_warning_link`] instead of re-explaining
//! the issue inline.

/// A roster member that more members may call than it holds tasks at once.
///
/// Callers are counted from the roster's expanded reachability, one per calling member, and
/// compared with what the called member's `lifecycle` holds at once. A call that arrives while
/// the member is full waits and is offered again, and is rejected only if the member stays full
/// until the call's deadline; calls may never overlap in practice, so the roster is admitted and
/// the formation launches.
pub const W_ROS_001: &str = "W-ROS-001";

/// Builds the doc link for a `W-ROS-*` code, e.g. `.../diagnostics/#w-ros-001`.
pub fn roster_warning_link(code: &str) -> String {
    crate::diagnostic_link(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every code is distinct and spelled in the `W-ROS-NNN` shape the diagnostics page anchors
    /// on. A duplicated or misspelled constant would silently point two warnings at one anchor.
    #[test]
    fn every_code_is_unique_and_well_formed() {
        let codes = [W_ROS_001];
        for code in codes {
            assert!(code.starts_with("W-ROS-"), "malformed code: {code}");
            assert_eq!(code.len(), 9, "malformed code: {code}");
        }
        let mut unique = codes.to_vec();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), codes.len(), "two W-ROS codes are equal");
    }

    #[test]
    fn link_lowercases_the_code_into_the_anchor() {
        assert_eq!(
            roster_warning_link(W_ROS_001),
            "https://docs.murmur.nexus/reference/diagnostics/#w-ros-001"
        );
    }
}
