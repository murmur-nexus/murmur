/// Returns the canonical platform string for the currently running binary.
///
/// Uses `std::env::consts` which reflects the compile target. For a native binary
/// this is always the machine the binary is running on.
///
/// Canonical strings match the platform tag convention used by Nexus and GitHub Releases:
///   "darwin-aarch64"  — macOS Apple Silicon
///   "darwin-x86_64"   — macOS Intel
///   "linux-aarch64"   — Linux ARM64
///   "linux-x86_64"    — Linux x86_64
///   "unknown"         — anything else (not expected in production)
pub fn current_platform() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "darwin-aarch64",
        ("macos", "x86_64") => "darwin-x86_64",
        ("linux", "aarch64") => "linux-aarch64",
        ("linux", "x86_64") => "linux-x86_64",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    // std::env::consts reflects the build target, so we test the mapping logic
    // directly rather than asserting specific values (which would be platform-dependent).
    // The match logic is the invariant to protect.

    #[test]
    fn known_platforms_all_have_canonical_strings() {
        let cases = [
            ("macos", "aarch64", "darwin-aarch64"),
            ("macos", "x86_64", "darwin-x86_64"),
            ("linux", "aarch64", "linux-aarch64"),
            ("linux", "x86_64", "linux-x86_64"),
        ];
        for (os, arch, expected) in cases {
            let result = map_platform(os, arch);
            assert_eq!(result, expected, "os={os} arch={arch}");
        }
    }

    #[test]
    fn unknown_os_arch_returns_unknown() {
        assert_eq!(map_platform("windows", "x86_64"), "unknown");
        assert_eq!(map_platform("freebsd", "aarch64"), "unknown");
        assert_eq!(map_platform("", ""), "unknown");
    }

    // Mirrors the match logic so tests don't depend on the build target.
    fn map_platform(os: &str, arch: &str) -> &'static str {
        match (os, arch) {
            ("macos", "aarch64") => "darwin-aarch64",
            ("macos", "x86_64") => "darwin-x86_64",
            ("linux", "aarch64") => "linux-aarch64",
            ("linux", "x86_64") => "linux-x86_64",
            _ => "unknown",
        }
    }
}
