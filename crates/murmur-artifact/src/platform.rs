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

/// Every platform tag [`current_platform`] can return, excluding `"unknown"`.
///
/// The one list of recognised platform strings: a store path, a release-asset name and a
/// `murmur.lock` platform key are all tagged with a member of this set, and a tag outside it is
/// not a platform tag. A second list open-coded elsewhere would recognise a different set than
/// the one `current_platform` produces, which is how a `darwin-x86_64` payload gets filed where
/// no `darwin-x86_64` host looks for it.
pub const SUPPORTED_PLATFORMS: [&str; 4] = [
    "darwin-aarch64",
    "darwin-x86_64",
    "linux-aarch64",
    "linux-x86_64",
];

/// Split a platform tag into its `(os, arch)` halves.
///
/// Accepts any `os-arch` shape, not just [`SUPPORTED_PLATFORMS`], because
/// [`crate::registry::ArtifactMeta::platforms`] records what a publisher declared rather than
/// what this build recognises. `None` when either half is empty or there is no `-`.
#[must_use]
pub fn split_platform_tag(platform: &str) -> Option<(&str, &str)> {
    let (os, arch) = platform.split_once('-')?;
    if os.trim().is_empty() || arch.trim().is_empty() {
        return None;
    }
    Some((os, arch))
}

/// Split a recognised platform tag off a `.mur.zip` file or release-asset name.
///
/// Returns the name with both the `-{platform}` tag and the `.mur.zip` extension removed, plus
/// the tag itself: `"tool-0.1.0-linux-x86_64.mur.zip"` → `("tool-0.1.0", "linux-x86_64")`.
/// `None` when the name does not end in `.mur.zip`, or ends in it with no tag from
/// [`SUPPORTED_PLATFORMS`] — an untagged payload every host resolves.
#[must_use]
pub fn split_platform_suffix(file_name: &str) -> Option<(&str, &'static str)> {
    let stem = file_name.strip_suffix(".mur.zip")?;
    SUPPORTED_PLATFORMS.iter().find_map(|platform| {
        stem.strip_suffix(platform)
            .and_then(|head| head.strip_suffix('-'))
            .filter(|head| !head.is_empty())
            .map(|head| (head, *platform))
    })
}

/// A fat Mach-O carries images for more than one architecture, so identifying one settles the
/// operating system and nothing else. [`native_binary_verdict`] treats it as runnable on any
/// `darwin-*` host.
pub const DARWIN_ANY_ARCH: &str = "darwin";

/// Identify the platform a native executable image was built for, from its header alone.
///
/// Reads fixed offsets inside the first 64 bytes of `bytes` and makes no syscalls, so a caller
/// that already holds the image in memory pays only those reads. Recognises the formats murmur's
/// four platform targets produce:
///
/// | Format         | Discriminator                      | Result                      |
/// |----------------|------------------------------------|-----------------------------|
/// | ELF64          | `e_machine` at offset 18           | `linux-x86_64`, `linux-aarch64` |
/// | Mach-O 64      | `cputype` at offset 4              | `darwin-x86_64`, `darwin-aarch64` |
/// | Fat Mach-O     | `0xCAFEBABE` magic                 | [`DARWIN_ANY_ARCH`]         |
///
/// Returns `None` for anything else — a shell script, a WASM module, an ELF32 image, an
/// architecture outside the four targets, or a buffer too short for the offsets the format needs.
/// `None` is the answer for "this helper cannot tell", never for "this cannot run here": a caller
/// deciding whether to refuse an image must not read it as a mismatch.
pub fn binary_platform(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x7fELF") {
        return elf_platform(bytes);
    }
    // A fat header is stored big-endian, so its magic is these four bytes in this order.
    if bytes.starts_with(&[0xCA, 0xFE, 0xBA, 0xBE]) {
        return Some(DARWIN_ANY_ARCH);
    }
    macho_platform(bytes)
}

/// `e_machine` of an ELF64 image, mapped to a Linux platform string.
///
/// An ELF64 header is 64 bytes, so a shorter buffer is not one. ELF32 yields `None`: both Linux
/// targets are 64-bit, so a 32-bit image is not an image this host would exec and identifying it
/// further buys nothing.
fn elf_platform(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() < 64 {
        return None;
    }
    // EI_CLASS: 2 == ELFCLASS64. EI_DATA: 1 == little-endian, 2 == big-endian.
    if bytes[4] != 2 {
        return None;
    }
    let le = match bytes[5] {
        1 => true,
        2 => false,
        _ => return None,
    };
    let raw = [bytes[18], bytes[19]];
    let e_machine = if le {
        u16::from_le_bytes(raw)
    } else {
        u16::from_be_bytes(raw)
    };
    match e_machine {
        0x3E => Some("linux-x86_64"),  // EM_X86_64
        0xB7 => Some("linux-aarch64"), // EM_AARCH64
        _ => None,
    }
}

/// `cputype` of a thin 64-bit Mach-O image, mapped to a darwin platform string.
///
/// The magic doubles as the byte-order mark: `0xFEEDFACF` read little-endian is a little-endian
/// image, and the byte-swapped `0xCFFAEDFE` is a big-endian one. `cputype` is then read in that
/// same order.
fn macho_platform(bytes: &[u8]) -> Option<&'static str> {
    let magic = u32::from_le_bytes(bytes.get(0..4)?.try_into().ok()?);
    let le = match magic {
        0xFEED_FACF => true, // MH_MAGIC_64
        0xCFFA_EDFE => false,
        _ => return None,
    };
    let raw: [u8; 4] = bytes.get(4..8)?.try_into().ok()?;
    let cputype = if le {
        i32::from_le_bytes(raw)
    } else {
        i32::from_be_bytes(raw)
    };
    match cputype {
        0x0100_0007 => Some("darwin-x86_64"),  // CPU_TYPE_X86_64
        0x0100_000C => Some("darwin-aarch64"), // CPU_TYPE_ARM64
        _ => None,
    }
}

/// Whether a native executable image can run on a given host.
///
/// Produced by [`native_binary_verdict`], which is the single decision both the staging refusal in
/// `capsule-runtime` and `mur doctor` read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeBinaryVerdict {
    /// Identified, and it is this host's platform.
    Runnable,
    /// Not an image this helper recognises, or the host platform is `"unknown"`.
    Indeterminate,
    /// Identified, and built for something other than this host.
    Mismatch { binary_platform: &'static str },
}

/// Decide whether `bytes` is an executable image this host can run.
///
/// `host_platform` is a [`current_platform`] string. `Mismatch` requires a positive
/// identification on both sides: an unrecognised image, or a host outside the four platform
/// targets, is `Indeterminate` and a caller must let it through. A fat Mach-O is runnable on any
/// `darwin-*` host because the arch table it carries is not walked.
pub fn native_binary_verdict(bytes: &[u8], host_platform: &str) -> NativeBinaryVerdict {
    let Some(binary_platform) = binary_platform(bytes) else {
        return NativeBinaryVerdict::Indeterminate;
    };
    if host_platform == "unknown" {
        return NativeBinaryVerdict::Indeterminate;
    }
    if binary_platform == host_platform {
        return NativeBinaryVerdict::Runnable;
    }
    if binary_platform == DARWIN_ANY_ARCH && host_platform.starts_with("darwin-") {
        return NativeBinaryVerdict::Runnable;
    }
    NativeBinaryVerdict::Mismatch { binary_platform }
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

    // ── SUPPORTED_PLATFORMS / tag splitting ───────────────────────────────────

    use super::{split_platform_suffix, split_platform_tag, SUPPORTED_PLATFORMS};

    /// The constant and the mapping are the same set. A platform `current_platform` can return
    /// but the constant does not list would be filed at a path nothing recognises.
    #[test]
    fn supported_platforms_matches_what_current_platform_returns() {
        let mapped = [
            map_platform("macos", "aarch64"),
            map_platform("macos", "x86_64"),
            map_platform("linux", "aarch64"),
            map_platform("linux", "x86_64"),
        ];
        let mut expected = mapped.to_vec();
        expected.sort_unstable();
        let mut actual = SUPPORTED_PLATFORMS.to_vec();
        actual.sort_unstable();
        assert_eq!(actual, expected);
    }

    #[test]
    fn platform_tag_splits_into_os_and_arch() {
        assert_eq!(
            split_platform_tag("linux-x86_64"),
            Some(("linux", "x86_64"))
        );
        assert_eq!(
            split_platform_tag("darwin-aarch64"),
            Some(("darwin", "aarch64"))
        );
        assert_eq!(split_platform_tag("linux"), None);
        assert_eq!(split_platform_tag("-x86_64"), None);
        assert_eq!(split_platform_tag("linux-"), None);
    }

    #[test]
    fn platform_suffix_splits_off_a_recognised_tag() {
        assert_eq!(
            split_platform_suffix("nativetool-0.1.0-linux-x86_64.mur.zip"),
            Some(("nativetool-0.1.0", "linux-x86_64"))
        );
        assert_eq!(
            split_platform_suffix("nativetool-0.1.0-darwin-aarch64.mur.zip"),
            Some(("nativetool-0.1.0", "darwin-aarch64"))
        );
    }

    #[test]
    fn an_untagged_or_unrecognised_name_carries_no_platform() {
        // A WASM artifact published under its plain versioned name.
        assert_eq!(split_platform_suffix("wasmtool-0.1.0.mur.zip"), None);
        // A tag outside the recognised set is not a tag.
        assert_eq!(
            split_platform_suffix("tool-0.1.0-windows-x86_64.mur.zip"),
            None
        );
        // The tag must be preceded by a name.
        assert_eq!(split_platform_suffix("linux-x86_64.mur.zip"), None);
        // Not a .mur.zip at all.
        assert_eq!(
            split_platform_suffix("tool-0.1.0-linux-x86_64.sha256"),
            None
        );
    }

    // ── binary_platform / native_binary_verdict ───────────────────────────────
    //
    // Every fixture below is synthesised in the test body from the same fixed offsets the
    // classifier reads, so the whole block runs identically on any host and needs no real
    // executable of any architecture.

    use super::{binary_platform, native_binary_verdict, NativeBinaryVerdict, DARWIN_ANY_ARCH};

    /// A 64-byte ELF64 header: `ei_data` at offset 5, `e_machine` at offset 18.
    fn elf64(le: bool, e_machine: u16) -> Vec<u8> {
        let mut bytes = vec![0u8; 64];
        bytes[0..4].copy_from_slice(b"\x7fELF");
        bytes[4] = 2; // ELFCLASS64
        bytes[5] = if le { 1 } else { 2 };
        let machine = if le {
            e_machine.to_le_bytes()
        } else {
            e_machine.to_be_bytes()
        };
        bytes[18..20].copy_from_slice(&machine);
        bytes
    }

    /// A 32-byte thin Mach-O header: magic at offset 0, `cputype` at offset 4.
    fn macho64(le: bool, cputype: i32) -> Vec<u8> {
        let mut bytes = vec![0u8; 32];
        let magic: u32 = 0xFEED_FACF;
        let (magic_bytes, cpu_bytes) = if le {
            (magic.to_le_bytes(), cputype.to_le_bytes())
        } else {
            (magic.to_be_bytes(), cputype.to_be_bytes())
        };
        bytes[0..4].copy_from_slice(&magic_bytes);
        bytes[4..8].copy_from_slice(&cpu_bytes);
        bytes
    }

    #[test]
    fn elf64_machines_map_to_linux_platforms() {
        assert_eq!(binary_platform(&elf64(true, 0x3E)), Some("linux-x86_64"));
        assert_eq!(binary_platform(&elf64(true, 0xB7)), Some("linux-aarch64"));
    }

    #[test]
    fn elf64_big_endian_reads_e_machine_in_that_order() {
        assert_eq!(binary_platform(&elf64(false, 0x3E)), Some("linux-x86_64"));
        assert_eq!(binary_platform(&elf64(false, 0xB7)), Some("linux-aarch64"));
        // A little-endian image whose e_machine bytes are read big-endian would be 0x3E00,
        // which is not a machine this maps — so the endianness byte is doing the work.
        let mut mislabelled = elf64(true, 0x3E);
        mislabelled[5] = 2;
        assert_eq!(binary_platform(&mislabelled), None);
    }

    #[test]
    fn macho64_cputypes_map_to_darwin_platforms_in_both_byte_orders() {
        for le in [true, false] {
            assert_eq!(
                binary_platform(&macho64(le, 0x0100_0007)),
                Some("darwin-x86_64"),
                "le={le}"
            );
            assert_eq!(
                binary_platform(&macho64(le, 0x0100_000C)),
                Some("darwin-aarch64"),
                "le={le}"
            );
        }
    }

    #[test]
    fn fat_macho_is_darwin_with_no_architecture() {
        let mut bytes = vec![0u8; 32];
        bytes[0..4].copy_from_slice(&[0xCA, 0xFE, 0xBA, 0xBE]);
        assert_eq!(binary_platform(&bytes), Some(DARWIN_ANY_ARCH));
        assert_eq!(DARWIN_ANY_ARCH, "darwin");
    }

    #[test]
    fn unidentifiable_images_are_none() {
        // ELF32 — not a target this runtime execs.
        let mut elf32 = elf64(true, 0x3E);
        elf32[4] = 1;
        assert_eq!(binary_platform(&elf32), None);

        // An ELF64 header truncated below the offsets the format needs.
        assert_eq!(binary_platform(&elf64(true, 0x3E)[..20]), None);

        // An e_machine outside the two Linux targets — EM_RISCV.
        assert_eq!(binary_platform(&elf64(true, 0xF3)), None);

        // A cputype outside the two darwin targets — CPU_TYPE_POWERPC64.
        assert_eq!(binary_platform(&macho64(true, 0x0100_0012)), None);

        // Not an executable image at all.
        assert_eq!(binary_platform(b"#!/bin/sh\nexit 0\n"), None);
        assert_eq!(binary_platform(b"\0asm\x01\0\0\0"), None);
        assert_eq!(binary_platform(b""), None);
    }

    #[test]
    fn verdict_matches_host_platform() {
        assert_eq!(
            native_binary_verdict(&elf64(true, 0x3E), "linux-x86_64"),
            NativeBinaryVerdict::Runnable
        );
        assert_eq!(
            native_binary_verdict(&macho64(true, 0x0100_000C), "darwin-aarch64"),
            NativeBinaryVerdict::Runnable
        );
    }

    #[test]
    fn verdict_reports_architecture_mismatch_within_one_os() {
        assert_eq!(
            native_binary_verdict(&elf64(true, 0x3E), "linux-aarch64"),
            NativeBinaryVerdict::Mismatch {
                binary_platform: "linux-x86_64"
            }
        );
        assert_eq!(
            native_binary_verdict(&macho64(true, 0x0100_0007), "darwin-aarch64"),
            NativeBinaryVerdict::Mismatch {
                binary_platform: "darwin-x86_64"
            }
        );
    }

    #[test]
    fn verdict_reports_operating_system_mismatch() {
        assert_eq!(
            native_binary_verdict(&macho64(true, 0x0100_000C), "linux-aarch64"),
            NativeBinaryVerdict::Mismatch {
                binary_platform: "darwin-aarch64"
            }
        );
        assert_eq!(
            native_binary_verdict(&elf64(true, 0xB7), "darwin-aarch64"),
            NativeBinaryVerdict::Mismatch {
                binary_platform: "linux-aarch64"
            }
        );
    }

    #[test]
    fn fat_macho_runs_on_any_darwin_host_and_no_linux_one() {
        let mut fat = vec![0u8; 32];
        fat[0..4].copy_from_slice(&[0xCA, 0xFE, 0xBA, 0xBE]);
        assert_eq!(
            native_binary_verdict(&fat, "darwin-aarch64"),
            NativeBinaryVerdict::Runnable
        );
        assert_eq!(
            native_binary_verdict(&fat, "darwin-x86_64"),
            NativeBinaryVerdict::Runnable
        );
        assert_eq!(
            native_binary_verdict(&fat, "linux-x86_64"),
            NativeBinaryVerdict::Mismatch {
                binary_platform: DARWIN_ANY_ARCH
            }
        );
    }

    #[test]
    fn unidentified_image_or_unknown_host_is_never_a_mismatch() {
        assert_eq!(
            native_binary_verdict(b"#!/bin/sh\nexit 0\n", "linux-x86_64"),
            NativeBinaryVerdict::Indeterminate
        );
        assert_eq!(
            native_binary_verdict(&elf64(true, 0x3E), "unknown"),
            NativeBinaryVerdict::Indeterminate
        );
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
