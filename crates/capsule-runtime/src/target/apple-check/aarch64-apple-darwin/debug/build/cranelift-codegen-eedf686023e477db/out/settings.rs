#[derive(Clone, PartialEq, Hash)] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:397
/// Flags group `shared`.
pub struct Flags {
    bytes: [u8; 12], // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:400
}
impl Flags {
    /// Create flags shared settings group.
    #[allow(unused_variables, reason = "generated code")] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:24
    pub fn new(builder: Builder) -> Self {
        let bvec = builder.state_for("shared"); // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:29
        let mut shared = Self { bytes: [0; 12] }; // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:30
        debug_assert_eq!(bvec.len(), 12); // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:36
        shared.bytes[0..12].copy_from_slice(&bvec); // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:41
        shared // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:48
    }
}
impl Flags {
    /// Iterates the setting values.
    pub fn iter(&self) -> impl Iterator<Item = Value> + use<> {
        let mut bytes = [0; 12]; // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:58
        bytes.copy_from_slice(&self.bytes[0..12]); // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:59
        DESCRIPTORS.iter().filter_map(move |d| {
            let values = match &d.detail {
                detail::Detail::Preset => return None, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:62
                detail::Detail::Enum { last, enumerators } => Some(TEMPLATE.enums(*last, *enumerators)), // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:63
                _ => None // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:64
            }
            ; // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:66
            Some(Value { name: d.name, detail: d.detail, values, value: bytes[d.offset as usize] }) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:67
        }
        ) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:69
    }
}
/// Values for `shared.regalloc_algorithm`.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:131
pub enum RegallocAlgorithm {
    /// `backtracking`.
    Backtracking, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:135
    /// `single_pass`.
    SinglePass, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:135
}
impl RegallocAlgorithm {
    /// Returns a slice with all possible [RegallocAlgorithm] values. // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:76
    pub fn all() -> &'static [RegallocAlgorithm] {
        &[ // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:82
            Self::Backtracking, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:85
            Self::SinglePass, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:85
        ] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:88
    }
}
impl fmt::Display for RegallocAlgorithm {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match *self {
            Self::Backtracking => "backtracking", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:100
            Self::SinglePass => "single_pass", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:100
        }
        ) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:103
    }
}
impl core::str::FromStr for RegallocAlgorithm {
    type Err = (); // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:109
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "backtracking" => Ok(Self::Backtracking), // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:113
            "single_pass" => Ok(Self::SinglePass), // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:113
            _ => Err(()), // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:115
        }
    }
}
/// Values for `shared.opt_level`.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:131
pub enum OptLevel {
    /// `none`.
    None, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:135
    /// `speed`.
    Speed, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:135
    /// `speed_and_size`.
    SpeedAndSize, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:135
}
impl OptLevel {
    /// Returns a slice with all possible [OptLevel] values. // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:76
    pub fn all() -> &'static [OptLevel] {
        &[ // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:82
            Self::None, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:85
            Self::Speed, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:85
            Self::SpeedAndSize, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:85
        ] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:88
    }
}
impl fmt::Display for OptLevel {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match *self {
            Self::None => "none", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:100
            Self::Speed => "speed", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:100
            Self::SpeedAndSize => "speed_and_size", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:100
        }
        ) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:103
    }
}
impl core::str::FromStr for OptLevel {
    type Err = (); // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:109
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "none" => Ok(Self::None), // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:113
            "speed" => Ok(Self::Speed), // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:113
            "speed_and_size" => Ok(Self::SpeedAndSize), // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:113
            _ => Err(()), // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:115
        }
    }
}
/// Values for `shared.tls_model`.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:131
pub enum TlsModel {
    /// `none`.
    None, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:135
    /// `elf_gd`.
    ElfGd, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:135
    /// `macho`.
    Macho, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:135
    /// `coff`.
    Coff, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:135
}
impl TlsModel {
    /// Returns a slice with all possible [TlsModel] values. // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:76
    pub fn all() -> &'static [TlsModel] {
        &[ // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:82
            Self::None, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:85
            Self::ElfGd, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:85
            Self::Macho, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:85
            Self::Coff, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:85
        ] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:88
    }
}
impl fmt::Display for TlsModel {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match *self {
            Self::None => "none", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:100
            Self::ElfGd => "elf_gd", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:100
            Self::Macho => "macho", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:100
            Self::Coff => "coff", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:100
        }
        ) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:103
    }
}
impl core::str::FromStr for TlsModel {
    type Err = (); // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:109
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "none" => Ok(Self::None), // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:113
            "elf_gd" => Ok(Self::ElfGd), // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:113
            "macho" => Ok(Self::Macho), // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:113
            "coff" => Ok(Self::Coff), // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:113
            _ => Err(()), // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:115
        }
    }
}
/// Values for `shared.stack_switch_model`.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:131
pub enum StackSwitchModel {
    /// `none`.
    None, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:135
    /// `basic`.
    Basic, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:135
    /// `update_windows_tib`.
    UpdateWindowsTib, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:135
}
impl StackSwitchModel {
    /// Returns a slice with all possible [StackSwitchModel] values. // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:76
    pub fn all() -> &'static [StackSwitchModel] {
        &[ // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:82
            Self::None, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:85
            Self::Basic, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:85
            Self::UpdateWindowsTib, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:85
        ] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:88
    }
}
impl fmt::Display for StackSwitchModel {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match *self {
            Self::None => "none", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:100
            Self::Basic => "basic", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:100
            Self::UpdateWindowsTib => "update_windows_tib", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:100
        }
        ) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:103
    }
}
impl core::str::FromStr for StackSwitchModel {
    type Err = (); // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:109
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "none" => Ok(Self::None), // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:113
            "basic" => Ok(Self::Basic), // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:113
            "update_windows_tib" => Ok(Self::UpdateWindowsTib), // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:113
            _ => Err(()), // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:115
        }
    }
}
/// Values for `shared.libcall_call_conv`.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:131
pub enum LibcallCallConv {
    /// `isa_default`.
    IsaDefault, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:135
    /// `fast`.
    Fast, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:135
    /// `system_v`.
    SystemV, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:135
    /// `windows_fastcall`.
    WindowsFastcall, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:135
    /// `apple_aarch64`.
    AppleAarch64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:135
    /// `probestack`.
    Probestack, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:135
    /// `preserve_all`.
    PreserveAll, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:135
}
impl LibcallCallConv {
    /// Returns a slice with all possible [LibcallCallConv] values. // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:76
    pub fn all() -> &'static [LibcallCallConv] {
        &[ // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:82
            Self::IsaDefault, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:85
            Self::Fast, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:85
            Self::SystemV, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:85
            Self::WindowsFastcall, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:85
            Self::AppleAarch64, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:85
            Self::Probestack, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:85
            Self::PreserveAll, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:85
        ] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:88
    }
}
impl fmt::Display for LibcallCallConv {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match *self {
            Self::IsaDefault => "isa_default", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:100
            Self::Fast => "fast", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:100
            Self::SystemV => "system_v", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:100
            Self::WindowsFastcall => "windows_fastcall", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:100
            Self::AppleAarch64 => "apple_aarch64", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:100
            Self::Probestack => "probestack", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:100
            Self::PreserveAll => "preserve_all", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:100
        }
        ) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:103
    }
}
impl core::str::FromStr for LibcallCallConv {
    type Err = (); // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:109
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "isa_default" => Ok(Self::IsaDefault), // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:113
            "fast" => Ok(Self::Fast), // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:113
            "system_v" => Ok(Self::SystemV), // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:113
            "windows_fastcall" => Ok(Self::WindowsFastcall), // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:113
            "apple_aarch64" => Ok(Self::AppleAarch64), // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:113
            "probestack" => Ok(Self::Probestack), // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:113
            "preserve_all" => Ok(Self::PreserveAll), // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:113
            _ => Err(()), // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:115
        }
    }
}
/// Values for `shared.probestack_strategy`.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:131
pub enum ProbestackStrategy {
    /// `outline`.
    Outline, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:135
    /// `inline`.
    Inline, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:135
}
impl ProbestackStrategy {
    /// Returns a slice with all possible [ProbestackStrategy] values. // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:76
    pub fn all() -> &'static [ProbestackStrategy] {
        &[ // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:82
            Self::Outline, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:85
            Self::Inline, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:85
        ] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:88
    }
}
impl fmt::Display for ProbestackStrategy {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match *self {
            Self::Outline => "outline", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:100
            Self::Inline => "inline", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:100
        }
        ) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:103
    }
}
impl core::str::FromStr for ProbestackStrategy {
    type Err = (); // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:109
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "outline" => Ok(Self::Outline), // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:113
            "inline" => Ok(Self::Inline), // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:113
            _ => Err(()), // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:115
        }
    }
}
/// User-defined settings.
#[allow(dead_code, reason = "generated code")] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:183
impl Flags {
    /// Dynamic numbered predicate getter.
    fn numbered_predicate(&self, p: usize) -> bool {
        self.bytes[9 + p / 8] & (1 << (p % 8)) != 0 // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:188
    }
    /// Algorithm to use in register allocator.
    ///
    /// Supported options:
    ///
    /// - `backtracking`: A backtracking allocator with range splitting; more expensive
    ///                   but generates better code.
    /// - `single_pass`: A single-pass algorithm that yields quick compilation but
    ///                  results in code with more register spills and moves.
    pub fn regalloc_algorithm(&self) -> RegallocAlgorithm {
        match self.bytes[0] {
            0 => {
                RegallocAlgorithm::Backtracking
            }
            1 => {
                RegallocAlgorithm::SinglePass
            }
            _ => {
                panic!("Invalid enum value")
            }
        }
    }
    /// Optimization level for generated code.
    ///
    /// Supported levels:
    ///
    /// - `none`: Minimise compile time by disabling most optimizations.
    /// - `speed`: Generate the fastest possible code
    /// - `speed_and_size`: like "speed", but also perform transformations aimed at reducing code size.
    pub fn opt_level(&self) -> OptLevel {
        match self.bytes[1] {
            0 => {
                OptLevel::None
            }
            1 => {
                OptLevel::Speed
            }
            2 => {
                OptLevel::SpeedAndSize
            }
            _ => {
                panic!("Invalid enum value")
            }
        }
    }
    /// Defines the model used to perform TLS accesses.
    pub fn tls_model(&self) -> TlsModel {
        match self.bytes[2] {
            3 => {
                TlsModel::Coff
            }
            1 => {
                TlsModel::ElfGd
            }
            2 => {
                TlsModel::Macho
            }
            0 => {
                TlsModel::None
            }
            _ => {
                panic!("Invalid enum value")
            }
        }
    }
    /// Defines the model used to performing stack switching.
    ///
    /// This determines the compilation of `stack_switch` instructions. If
    /// set to `basic`, we simply save all registers, update stack pointer
    /// and frame pointer (if needed), and jump to the target IP.
    /// If set to `update_windows_tib`, we *additionally* update information
    /// about the active stack in Windows' Thread Information Block.
    pub fn stack_switch_model(&self) -> StackSwitchModel {
        match self.bytes[3] {
            1 => {
                StackSwitchModel::Basic
            }
            0 => {
                StackSwitchModel::None
            }
            2 => {
                StackSwitchModel::UpdateWindowsTib
            }
            _ => {
                panic!("Invalid enum value")
            }
        }
    }
    /// Defines the calling convention to use for LibCalls call expansion.
    ///
    /// This may be different from the ISA default calling convention.
    ///
    /// The default value is to use the same calling convention as the ISA
    /// default calling convention.
    ///
    /// This list should be kept in sync with the list of calling
    /// conventions available in isa/call_conv.rs.
    pub fn libcall_call_conv(&self) -> LibcallCallConv {
        match self.bytes[4] {
            4 => {
                LibcallCallConv::AppleAarch64
            }
            1 => {
                LibcallCallConv::Fast
            }
            0 => {
                LibcallCallConv::IsaDefault
            }
            6 => {
                LibcallCallConv::PreserveAll
            }
            5 => {
                LibcallCallConv::Probestack
            }
            2 => {
                LibcallCallConv::SystemV
            }
            3 => {
                LibcallCallConv::WindowsFastcall
            }
            _ => {
                panic!("Invalid enum value")
            }
        }
    }
    /// The log2 of the size of the stack guard region.
    ///
    /// Stack frames larger than this size will have stack overflow checked
    /// by calling the probestack function.
    ///
    /// The default is 12, which translates to a size of 4096.
    pub fn probestack_size_log2(&self) -> u8 {
        self.bytes[5] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:174
    }
    /// Controls what kinds of stack probes are emitted.
    ///
    /// Supported strategies:
    ///
    /// - `outline`: Always emits stack probes as calls to a probe stack function.
    /// - `inline`: Always emits inline stack probes.
    pub fn probestack_strategy(&self) -> ProbestackStrategy {
        match self.bytes[6] {
            1 => {
                ProbestackStrategy::Inline
            }
            0 => {
                ProbestackStrategy::Outline
            }
            _ => {
                panic!("Invalid enum value")
            }
        }
    }
    /// The log2 of the size to insert dummy padding between basic blocks
    ///
    /// This is a debugging option for stressing various cases during code
    /// generation without requiring large functions. This will insert
    /// 0-byte padding between basic blocks of the specified size.
    ///
    /// The amount of padding inserted two raised to the power of this value
    /// minus one. If this value is 0 then no padding is inserted.
    ///
    /// The default for this option is 0 to insert no padding as it's only
    /// intended for testing and development.
    pub fn bb_padding_log2_minus_one(&self) -> u8 {
        self.bytes[7] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:174
    }
    /// The log2 of the minimum alignment of functions
    /// The bigger of this value and the default alignment will be used as actual alignment.
    pub fn log2_min_function_alignment(&self) -> u8 {
        self.bytes[8] // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:174
    }
    /// Enable the symbolic checker for register allocation.
    ///
    /// This performs a verification that the register allocator preserves
    /// equivalent dataflow with respect to the original (pre-regalloc)
    /// program. This analysis is somewhat expensive. However, if it succeeds,
    /// it provides independent evidence (by a carefully-reviewed, from-first-principles
    /// analysis) that no regalloc bugs were triggered for the particular compilations
    /// performed. This is a valuable assurance to have as regalloc bugs can be
    /// very dangerous and difficult to debug.
    pub fn regalloc_checker(&self) -> bool {
        self.numbered_predicate(0) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:155
    }
    /// Enable verbose debug logs for regalloc2.
    ///
    /// This adds extra logging for regalloc2 output, that is quite valuable to understand
    /// decisions taken by the register allocator as well as debugging it. It is disabled by
    /// default, as it can cause many log calls which can slow down compilation by a large
    /// amount.
    pub fn regalloc_verbose_logs(&self) -> bool {
        self.numbered_predicate(1) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:155
    }
    /// Do redundant-load optimizations with alias analysis.
    ///
    /// This enables the use of a simple alias analysis to optimize away redundant loads.
    /// Only effective when `opt_level` is `speed` or `speed_and_size`.
    pub fn enable_alias_analysis(&self) -> bool {
        self.numbered_predicate(2) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:155
    }
    /// Run the Cranelift IR verifier at strategic times during compilation.
    ///
    /// This makes compilation slower but catches many bugs. The verifier is always enabled by
    /// default, which is useful during development.
    pub fn enable_verifier(&self) -> bool {
        self.numbered_predicate(3) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:155
    }
    /// Enable Position-Independent Code generation.
    pub fn is_pic(&self) -> bool {
        self.numbered_predicate(4) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:155
    }
    /// Use colocated libcalls.
    ///
    /// Generate code that assumes that libcalls can be declared "colocated",
    /// meaning they will be defined along with the current function, such that
    /// they can use more efficient addressing.
    pub fn use_colocated_libcalls(&self) -> bool {
        self.numbered_predicate(5) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:155
    }
    /// Enable NaN canonicalization.
    ///
    /// This replaces NaNs with a single canonical value, for users requiring
    /// entirely deterministic WebAssembly computation. This is not required
    /// by the WebAssembly spec, so it is not enabled by default.
    pub fn enable_nan_canonicalization(&self) -> bool {
        self.numbered_predicate(6) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:155
    }
    /// Enable the use of the pinned register.
    ///
    /// This register is excluded from register allocation, and is completely under the control of
    /// the end-user. It is possible to read it via the get_pinned_reg instruction, and to set it
    /// with the set_pinned_reg instruction.
    pub fn enable_pinned_reg(&self) -> bool {
        self.numbered_predicate(7) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:155
    }
    /// Enable various ABI extensions defined by LLVM's behavior.
    ///
    /// In some cases, LLVM's implementation of an ABI (calling convention)
    /// goes beyond a standard and supports additional argument types or
    /// behavior. This option instructs Cranelift codegen to follow LLVM's
    /// behavior where applicable.
    ///
    /// Currently, this applies only to Windows Fastcall on x86-64, and
    /// allows an `i128` argument to be spread across two 64-bit integer
    /// registers. The Fastcall implementation otherwise does not support
    /// `i128` arguments, and will panic if they are present and this
    /// option is not set.
    pub fn enable_llvm_abi_extensions(&self) -> bool {
        self.numbered_predicate(8) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:155
    }
    /// Enable support for sret arg introduction when there are too many ret vals.
    ///
    /// When there are more returns than available return registers, the
    /// return value has to be returned through the introduction of a
    /// return area pointer. Normally this return area pointer has to be
    /// introduced as `ArgumentPurpose::StructReturn` parameter, but for
    /// backward compatibility reasons Cranelift also supports implicitly
    /// introducing this parameter and writing the return values through it.
    ///
    /// **This option currently does not conform to platform ABIs and the
    /// used ABI should not be assumed to remain the same between Cranelift
    /// versions.**
    ///
    /// This option is **deprecated** and will be removed in the future.
    ///
    /// Because of the above issues, and complexities of native ABI support
    /// for the concept in general, Cranelift's support for multiple return
    /// values may also be removed in the future (#9510). For the most
    /// robust solution, it is recommended to build a convention on top of
    /// Cranelift's primitives for passing multiple return values, for
    /// example by allocating a stackslot in the caller, passing it as an
    /// explicit StructReturn argument, storing return values in the callee,
    /// and loading results in the caller.
    pub fn enable_multi_ret_implicit_sret(&self) -> bool {
        self.numbered_predicate(9) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:155
    }
    /// Generate unwind information.
    ///
    /// This increases metadata size and compile time, but allows for the
    /// debugger to trace frames, is needed for GC tracing that relies on
    /// libunwind (such as in Wasmtime), and is unconditionally needed on
    /// certain platforms (such as Windows) that must always be able to unwind.
    pub fn unwind_info(&self) -> bool {
        self.numbered_predicate(10) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:155
    }
    /// Preserve frame pointers
    ///
    /// Preserving frame pointers -- even inside leaf functions -- makes it
    /// easy to capture the stack of a running program, without requiring any
    /// side tables or metadata (like `.eh_frame` sections). Many sampling
    /// profilers and similar tools walk frame pointers to capture stacks.
    /// Enabling this option will play nice with those tools.
    pub fn preserve_frame_pointers(&self) -> bool {
        self.numbered_predicate(11) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:155
    }
    /// Generate CFG metadata for machine code.
    ///
    /// This increases metadata size and compile time, but allows for the
    /// embedder to more easily post-process or analyze the generated
    /// machine code. It provides code offsets for the start of each
    /// basic block in the generated machine code, and a list of CFG
    /// edges (with blocks identified by start offsets) between them.
    /// This is useful for, e.g., machine-code analyses that verify certain
    /// properties of the generated code.
    pub fn machine_code_cfg_info(&self) -> bool {
        self.numbered_predicate(12) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:155
    }
    /// Enable the use of stack probes for supported calling conventions.
    pub fn enable_probestack(&self) -> bool {
        self.numbered_predicate(13) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:155
    }
    /// Enable Spectre mitigation on heap bounds checks.
    ///
    /// This is a no-op for any heap that needs no bounds checks; e.g.,
    /// if the limit is static and the guard region is large enough that
    /// the index cannot reach past it.
    ///
    /// This option is enabled by default because it is highly
    /// recommended for secure sandboxing. The embedder should consider
    /// the security implications carefully before disabling this option.
    pub fn enable_heap_access_spectre_mitigation(&self) -> bool {
        self.numbered_predicate(14) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:155
    }
    /// Enable Spectre mitigation on table bounds checks.
    ///
    /// This option uses a conditional move to ensure that when a table
    /// access index is bounds-checked and a conditional branch is used
    /// for the out-of-bounds case, a misspeculation of that conditional
    /// branch (falsely predicted in-bounds) will select an in-bounds
    /// index to load on the speculative path.
    ///
    /// This option is enabled by default because it is highly
    /// recommended for secure sandboxing. The embedder should consider
    /// the security implications carefully before disabling this option.
    pub fn enable_table_access_spectre_mitigation(&self) -> bool {
        self.numbered_predicate(15) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:155
    }
    /// Enable additional checks for debugging the incremental compilation cache.
    ///
    /// Enables additional checks that are useful during development of the incremental
    /// compilation cache. This should be mostly useful for Cranelift hackers, as well as for
    /// helping to debug false incremental cache positives for embedders.
    ///
    /// This option is disabled by default and requires enabling the "incremental-cache" Cargo
    /// feature in cranelift-codegen.
    pub fn enable_incremental_compilation_cache_checks(&self) -> bool {
        self.numbered_predicate(16) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:155
    }
    /// Enable Mach-O compact unwind compatible code emission.
    ///
    /// This constrains function prologues and epilogues to the ABI shape
    /// that Mach-O compact unwind can encode. Unwind instructions describe
    /// which pairs of callee-save registers are saved and restored, but the
    /// canonical stack frame layout is still expected: nonvolatile registers
    /// must be placed near the top of the frame, immediately below the
    /// return address.
    ///
    /// The unwind encoding only supports register pairs emitted with `stp`;
    /// it cannot encode individual registers. This may result in slightly
    /// larger stack frames.
    pub fn enable_compact_unwind_abi(&self) -> bool {
        self.numbered_predicate(17) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:155
    }
}
static DESCRIPTORS: [detail::Descriptor; 27] = [ // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:224
    detail::Descriptor {
        name: "regalloc_algorithm", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:232
        description: "Algorithm to use in register allocator.", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:233
        offset: 0, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:234
        detail: detail::Detail::Enum { last: 1, enumerators: 0 }, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:245
    }
    , // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:259
    detail::Descriptor {
        name: "opt_level", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:232
        description: "Optimization level for generated code.", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:233
        offset: 1, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:234
        detail: detail::Detail::Enum { last: 2, enumerators: 2 }, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:245
    }
    , // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:259
    detail::Descriptor {
        name: "tls_model", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:232
        description: "Defines the model used to perform TLS accesses.", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:233
        offset: 2, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:234
        detail: detail::Detail::Enum { last: 3, enumerators: 5 }, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:245
    }
    , // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:259
    detail::Descriptor {
        name: "stack_switch_model", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:232
        description: "Defines the model used to performing stack switching.", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:233
        offset: 3, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:234
        detail: detail::Detail::Enum { last: 2, enumerators: 9 }, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:245
    }
    , // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:259
    detail::Descriptor {
        name: "libcall_call_conv", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:232
        description: "Defines the calling convention to use for LibCalls call expansion.", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:233
        offset: 4, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:234
        detail: detail::Detail::Enum { last: 6, enumerators: 12 }, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:245
    }
    , // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:259
    detail::Descriptor {
        name: "probestack_size_log2", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:232
        description: "The log2 of the size of the stack guard region.", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:233
        offset: 5, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:234
        detail: detail::Detail::Num, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:253
    }
    , // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:259
    detail::Descriptor {
        name: "probestack_strategy", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:232
        description: "Controls what kinds of stack probes are emitted.", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:233
        offset: 6, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:234
        detail: detail::Detail::Enum { last: 1, enumerators: 19 }, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:245
    }
    , // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:259
    detail::Descriptor {
        name: "bb_padding_log2_minus_one", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:232
        description: "The log2 of the size to insert dummy padding between basic blocks", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:233
        offset: 7, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:234
        detail: detail::Detail::Num, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:253
    }
    , // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:259
    detail::Descriptor {
        name: "log2_min_function_alignment", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:232
        description: "The log2 of the minimum alignment of functions", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:233
        offset: 8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:234
        detail: detail::Detail::Num, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:253
    }
    , // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:259
    detail::Descriptor {
        name: "regalloc_checker", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:232
        description: "Enable the symbolic checker for register allocation.", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:233
        offset: 9, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:234
        detail: detail::Detail::Bool { bit: 0 }, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:237
    }
    , // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:259
    detail::Descriptor {
        name: "regalloc_verbose_logs", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:232
        description: "Enable verbose debug logs for regalloc2.", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:233
        offset: 9, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:234
        detail: detail::Detail::Bool { bit: 1 }, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:237
    }
    , // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:259
    detail::Descriptor {
        name: "enable_alias_analysis", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:232
        description: "Do redundant-load optimizations with alias analysis.", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:233
        offset: 9, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:234
        detail: detail::Detail::Bool { bit: 2 }, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:237
    }
    , // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:259
    detail::Descriptor {
        name: "enable_verifier", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:232
        description: "Run the Cranelift IR verifier at strategic times during compilation.", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:233
        offset: 9, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:234
        detail: detail::Detail::Bool { bit: 3 }, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:237
    }
    , // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:259
    detail::Descriptor {
        name: "is_pic", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:232
        description: "Enable Position-Independent Code generation.", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:233
        offset: 9, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:234
        detail: detail::Detail::Bool { bit: 4 }, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:237
    }
    , // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:259
    detail::Descriptor {
        name: "use_colocated_libcalls", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:232
        description: "Use colocated libcalls.", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:233
        offset: 9, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:234
        detail: detail::Detail::Bool { bit: 5 }, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:237
    }
    , // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:259
    detail::Descriptor {
        name: "enable_nan_canonicalization", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:232
        description: "Enable NaN canonicalization.", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:233
        offset: 9, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:234
        detail: detail::Detail::Bool { bit: 6 }, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:237
    }
    , // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:259
    detail::Descriptor {
        name: "enable_pinned_reg", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:232
        description: "Enable the use of the pinned register.", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:233
        offset: 9, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:234
        detail: detail::Detail::Bool { bit: 7 }, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:237
    }
    , // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:259
    detail::Descriptor {
        name: "enable_llvm_abi_extensions", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:232
        description: "Enable various ABI extensions defined by LLVM's behavior.", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:233
        offset: 10, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:234
        detail: detail::Detail::Bool { bit: 0 }, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:237
    }
    , // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:259
    detail::Descriptor {
        name: "enable_multi_ret_implicit_sret", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:232
        description: "Enable support for sret arg introduction when there are too many ret vals.", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:233
        offset: 10, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:234
        detail: detail::Detail::Bool { bit: 1 }, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:237
    }
    , // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:259
    detail::Descriptor {
        name: "unwind_info", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:232
        description: "Generate unwind information.", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:233
        offset: 10, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:234
        detail: detail::Detail::Bool { bit: 2 }, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:237
    }
    , // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:259
    detail::Descriptor {
        name: "preserve_frame_pointers", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:232
        description: "Preserve frame pointers", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:233
        offset: 10, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:234
        detail: detail::Detail::Bool { bit: 3 }, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:237
    }
    , // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:259
    detail::Descriptor {
        name: "machine_code_cfg_info", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:232
        description: "Generate CFG metadata for machine code.", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:233
        offset: 10, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:234
        detail: detail::Detail::Bool { bit: 4 }, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:237
    }
    , // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:259
    detail::Descriptor {
        name: "enable_probestack", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:232
        description: "Enable the use of stack probes for supported calling conventions.", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:233
        offset: 10, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:234
        detail: detail::Detail::Bool { bit: 5 }, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:237
    }
    , // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:259
    detail::Descriptor {
        name: "enable_heap_access_spectre_mitigation", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:232
        description: "Enable Spectre mitigation on heap bounds checks.", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:233
        offset: 10, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:234
        detail: detail::Detail::Bool { bit: 6 }, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:237
    }
    , // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:259
    detail::Descriptor {
        name: "enable_table_access_spectre_mitigation", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:232
        description: "Enable Spectre mitigation on table bounds checks.", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:233
        offset: 10, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:234
        detail: detail::Detail::Bool { bit: 7 }, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:237
    }
    , // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:259
    detail::Descriptor {
        name: "enable_incremental_compilation_cache_checks", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:232
        description: "Enable additional checks for debugging the incremental compilation cache.", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:233
        offset: 11, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:234
        detail: detail::Detail::Bool { bit: 0 }, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:237
    }
    , // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:259
    detail::Descriptor {
        name: "enable_compact_unwind_abi", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:232
        description: "Enable Mach-O compact unwind compatible code emission.", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:233
        offset: 11, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:234
        detail: detail::Detail::Bool { bit: 1 }, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:237
    }
    , // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:259
]; // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:275
static ENUMERATORS: [&str; 21] = [ // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:278
    "backtracking", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:281
    "single_pass", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:281
    "none", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:281
    "speed", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:281
    "speed_and_size", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:281
    "none", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:281
    "elf_gd", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:281
    "macho", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:281
    "coff", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:281
    "none", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:281
    "basic", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:281
    "update_windows_tib", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:281
    "isa_default", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:281
    "fast", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:281
    "system_v", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:281
    "windows_fastcall", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:281
    "apple_aarch64", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:281
    "probestack", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:281
    "preserve_all", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:281
    "outline", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:281
    "inline", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:281
]; // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:284
static HASH_TABLE: [u16; 64] = [ // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:294
    0xffff, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:306
    0xffff, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:306
    0xffff, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:306
    2, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:298
    11, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:298
    0xffff, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:306
    24, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:298
    26, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:298
    20, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:298
    0xffff, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:306
    0xffff, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:306
    0xffff, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:306
    25, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:298
    0xffff, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:306
    0xffff, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:306
    0xffff, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:306
    16, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:298
    0xffff, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:306
    0xffff, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:306
    0xffff, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:306
    0xffff, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:306
    0xffff, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:306
    0xffff, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:306
    0xffff, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:306
    7, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:298
    0xffff, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:306
    0xffff, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:306
    6, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:298
    1, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:298
    0, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:298
    3, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:298
    13, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:298
    0xffff, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:306
    18, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:298
    22, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:298
    8, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:298
    15, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:298
    12, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:298
    17, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:298
    0xffff, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:306
    9, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:298
    0xffff, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:306
    0xffff, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:306
    19, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:298
    5, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:298
    0xffff, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:306
    23, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:298
    0xffff, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:306
    0xffff, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:306
    0xffff, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:306
    21, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:298
    0xffff, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:306
    0xffff, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:306
    10, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:298
    14, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:298
    4, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:298
    0xffff, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:306
    0xffff, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:306
    0xffff, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:306
    0xffff, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:306
    0xffff, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:306
    0xffff, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:306
    0xffff, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:306
    0xffff, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:306
]; // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:310
static PRESETS: [(u8, u8); 0] = [ // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:313
]; // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:330
static TEMPLATE: detail::Template = detail::Template {
    name: "shared", // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:345
    descriptors: &DESCRIPTORS, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:346
    enumerators: &ENUMERATORS, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:347
    hash_table: &HASH_TABLE, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:348
    defaults: &[0x00, 0x00, 0x00, 0x00, 0x00, 0x0c, 0x00, 0x00, 0x00, 0x0c, 0xc4, 0x00], // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:349
    presets: &PRESETS, // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:350
}
; // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:353
/// Create a `settings::Builder` for the shared settings group.
pub fn builder() -> Builder {
    Builder::new(&TEMPLATE) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:360
}
impl fmt::Display for Flags {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        writeln!(f, "[shared]")?; // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:369
        for d in &DESCRIPTORS {
            if !d.detail.is_preset() {
                write!(f, "{} = ", d.name)?; // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:372
                TEMPLATE.format_toml_value(d.detail, self.bytes[d.offset as usize], f)?; // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:373
                writeln!(f)?; // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:377
            }
        }
        Ok(()) // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:380
    }
}
impl Flags {
    /// Get the flag values as raw bytes for hashing.
    pub fn hash_key(&self) -> &[u8] {
        &self.bytes // /home/agape/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cranelift-codegen-meta-0.133.3/src/gen_settings.rs:390
    }
}
