//! The `programs:` block of a `runtime: tool` artifact's own `murmur.yaml`: what each host program
//! the tool runs needs from the host, named portably.
//!
//! ```yaml
//! programs:
//!   render:
//!     needs:
//!       - library: libexample.so.1
//!         packages:
//!           debian: libexample1
//!           fedora: example-libs
//!       - file: libexample.chk
//!         beside: libexample.so.1
//!       - program_folder: .
//!       - system: fonts
//!       - etc: fonts/fonts.conf
//! ```
//!
//! A need names *what* the program needs, never *where* a host keeps it: a soname, a file beside
//! a declared library, a path relative to the program's own folder, a named system location, or a
//! name under `/etc`. [`parse_program_needs`] refuses every other form, naming the program, the
//! need's index, the value and why. `mur build` (`E-BLD-008`) and staging (`E-RUN-062`) call the
//! same parser; resolving a need against a host is the runtime's.

use serde_norway::{Mapping, Value};
use std::collections::BTreeMap;

use crate::manifest::{optional_string, parse_manifest_value, yaml_type_name};
use crate::ManifestError;

/// The top-level key of the block.
pub const PROGRAMS_KEY: &str = "programs";

/// The `/etc` locations that hold credentials. No `etc:` need may name one, an entry beneath one,
/// or a folder that holds one; the runtime moves a need that resolves to any of them to `missing`.
/// Relative to `/etc`.
pub const ETC_CREDENTIAL_LOCATIONS: &[&str] = &[
    "shadow",
    "shadow-",
    "gshadow",
    "gshadow-",
    "sudoers",
    "sudoers.d",
    "ssh",
    "ssl/private",
    "pki/tls/private",
    "krb5.keytab",
    "security",
    "ipsec.secrets",
    "ipsec.d",
    "ppp",
    "wpa_supplicant",
    "NetworkManager/system-connections",
];

/// The needs one artifact declares for one program, in declaration order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramNeeds {
    /// The program key: a bare name, as `capabilities.shell.allow` spells it.
    pub program: String,
    pub needs: Vec<DeclaredNeed>,
}

/// One entry of a program's `needs:` list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredNeed {
    pub form: NeedForm,
    /// `packages:`, optional: an os-release `ID` to the package that provides the need on that
    /// distribution. Empty when the entry declares none.
    pub packages: BTreeMap<String, String>,
}

/// The one form a need takes.
///
/// Displayed as the form key and its value, the label every report uses: `library <soname>`,
/// `file <name> beside <soname>`, `program_folder <path>`, `system <location>`, `etc <path>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NeedForm {
    /// A soname, resolved as the dynamic loader resolves a `DT_NEEDED` entry.
    Library(String),
    /// A file name, in the directory the program's `library:` entry `beside` resolved to.
    File { file: String, beside: String },
    /// A path relative to the folder the program resolves to; `.` is that folder.
    ProgramFolder(String),
    /// A named system location.
    System(SystemLocation),
    /// A path under `/etc`, relative to it.
    Etc(String),
}

impl std::fmt::Display for NeedForm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Library(soname) => write!(f, "library {soname}"),
            Self::File { file, beside } => write!(f, "file {file} beside {beside}"),
            Self::ProgramFolder(path) => write!(f, "program_folder {path}"),
            Self::System(location) => write!(f, "system {location}"),
            Self::Etc(path) => write!(f, "etc {path}"),
        }
    }
}

/// The system locations a `system:` need may name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SystemLocation {
    /// The host's font and fontconfig folders.
    Fonts,
    /// The host's TLS trust store.
    CaBundle,
}

impl SystemLocation {
    /// Every location, in the order a refusal lists them.
    pub const ALL: [Self; 2] = [Self::Fonts, Self::CaBundle];

    /// The location as `system:` spells it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fonts => "fonts",
            Self::CaBundle => "ca_bundle",
        }
    }
}

impl std::fmt::Display for SystemLocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The keys that each make a need entry one form.
const FORM_KEYS: [&str; 5] = ["library", "file", "program_folder", "system", "etc"];

/// What a refusal says a need entry holds.
const ONE_FORM: &str = "a need holds exactly one form: library, file (with beside), \
                        program_folder, system or etc, and optionally packages";

/// Reads the top-level `programs:` block from an artifact's bundled manifest text.
///
/// `Ok` with an empty list means the manifest has no `programs` key. `Err` names the dotted field
/// at fault (`programs.<program>.needs[<index>].<key>`) and the value: the block on a `driver`,
/// `hook` or `skill` artifact; a program key that is not a bare name; a need holding no form, two
/// forms or a key other than a form key, `beside` and `packages`; `library` not a soname; `file`
/// not a bare file name, or `beside` naming no `library:` entry of the same program;
/// `program_folder` or `etc` an absolute path, a home folder, an environment expansion or a path
/// climbing with `..`; `etc` naming a credential location ([`ETC_CREDENTIAL_LOCATIONS`]);
/// `system` not one of [`SystemLocation::ALL`]; `packages` not a map of lowercase os-release IDs
/// to package names; a program holding neither `needs` nor `download`, or a malformed `download`
/// entry ([`crate::parse_program_downloads`] reads those). A program with only `download` has no
/// needs.
pub fn parse_program_needs(manifest_yaml: &str) -> Result<Vec<ProgramNeeds>, ManifestError> {
    let value = parse_manifest_value(manifest_yaml)?;
    let Some(root) = value.as_mapping() else {
        return Ok(Vec::new());
    };
    let Some(block) = root.get(Value::String(PROGRAMS_KEY.to_string())) else {
        return Ok(Vec::new());
    };
    let runtime = optional_string(root, "runtime").unwrap_or_default();
    if matches!(runtime.as_str(), "driver" | "hook" | "skill") {
        return Err(invalid(
            PROGRAMS_KEY.to_string(),
            format!(
                "is only valid on an artifact whose runtime is tool; this one is runtime: \
                 {runtime}"
            ),
        ));
    }
    let block = mapping(block, PROGRAMS_KEY)?;
    let mut programs = Vec::with_capacity(block.len());
    for (key, entry) in block {
        let Some(program) = key.as_str() else {
            return Err(ManifestError::InvalidType {
                field: format!("{PROGRAMS_KEY} key"),
                expected: "string".to_string(),
                got: yaml_type_name(key),
            });
        };
        let field = format!("{PROGRAMS_KEY}.{program}");
        if program.is_empty() || program.contains('/') || program == "." || program == ".." {
            return Err(invalid(
                format!("{PROGRAMS_KEY} key '{program}'"),
                "is not a bare program name; a program is named as capabilities.shell.allow \
                 names it: no '/', not '.' or '..'"
                    .to_string(),
            ));
        }
        programs.push(ProgramNeeds {
            program: program.to_string(),
            needs: parse_program(program, mapping(entry, &field)?, &field)?,
        });
    }
    Ok(programs)
}

fn parse_program(
    program: &str,
    entry: &Mapping,
    field: &str,
) -> Result<Vec<DeclaredNeed>, ManifestError> {
    if let Some(extra) = entry.keys().find(|key| {
        !matches!(
            key.as_str(),
            Some("needs") | Some(crate::program_download::DOWNLOAD_KEY)
        )
    }) {
        return Err(invalid(
            format!("{field}.{}", key_text(extra)),
            "is not a key of a program; a program holds needs, download or both".to_string(),
        ));
    }
    let download = entry.get(Value::String(
        crate::program_download::DOWNLOAD_KEY.to_string(),
    ));
    if let Some(download) = download {
        crate::program_download::parse_download(
            download,
            &format!("{field}.{}", crate::program_download::DOWNLOAD_KEY),
        )?;
    }
    let needs_field = format!("{field}.needs");
    let Some(needs) = entry.get(Value::String("needs".to_string())) else {
        if download.is_some() {
            return Ok(Vec::new());
        }
        return Err(invalid(
            field.to_string(),
            "holds neither needs nor download; a program holds needs, download or both".to_string(),
        ));
    };
    let needs = needs
        .as_sequence()
        .ok_or_else(|| ManifestError::InvalidType {
            field: needs_field.clone(),
            expected: "sequence".to_string(),
            got: yaml_type_name(needs),
        })?;
    let mut parsed = Vec::with_capacity(needs.len());
    for (index, need) in needs.iter().enumerate() {
        let need_field = format!("{needs_field}[{index}]");
        parsed.push(parse_need(mapping(need, &need_field)?, &need_field)?);
    }
    for (index, need) in parsed.iter().enumerate() {
        if let NeedForm::File { beside, .. } = &need.form {
            let declared = parsed
                .iter()
                .any(|other| matches!(&other.form, NeedForm::Library(soname) if soname == beside));
            if !declared {
                return Err(invalid(
                    format!("{needs_field}[{index}].beside"),
                    format!(
                        "is '{beside}', which names no library: entry of program '{program}'; \
                         beside names the library: value whose folder holds the file"
                    ),
                ));
            }
        }
    }
    Ok(parsed)
}

fn parse_need(need: &Mapping, field: &str) -> Result<DeclaredNeed, ManifestError> {
    if let Some(extra) = need.keys().find(|key| {
        !key.as_str()
            .is_some_and(|k| FORM_KEYS.contains(&k) || k == "beside" || k == "packages")
    }) {
        return Err(invalid(
            format!("{field}.{}", key_text(extra)),
            format!("is not a need key; {ONE_FORM}"),
        ));
    }
    let forms: Vec<&str> = FORM_KEYS
        .into_iter()
        .filter(|key| need.contains_key(Value::String((*key).to_string())))
        .collect();
    let form_key = match forms.as_slice() {
        [one] => *one,
        [] => {
            return Err(invalid(
                field.to_string(),
                format!("holds no form; {ONE_FORM}"),
            ))
        }
        several => {
            return Err(invalid(
                field.to_string(),
                format!("holds {}; {ONE_FORM}", several.join(" and ")),
            ))
        }
    };
    let text = |key: &str| -> Result<String, ManifestError> {
        let value = need.get(Value::String(key.to_string())).ok_or_else(|| {
            ManifestError::MissingField {
                field: format!("{field}.{key}"),
            }
        })?;
        value
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| ManifestError::InvalidType {
                field: format!("{field}.{key}"),
                expected: "string".to_string(),
                got: yaml_type_name(value),
            })
    };
    if form_key != "file" && need.contains_key(Value::String("beside".to_string())) {
        return Err(invalid(
            format!("{field}.beside"),
            format!("goes only with file; this need's form is {form_key}"),
        ));
    }
    let refuse = |key: &str, value: &str, reason: String| {
        invalid(format!("{field}.{key}"), format!("is '{value}', {reason}"))
    };
    let form = match form_key {
        "library" => {
            let soname = text("library")?;
            if let Some(reason) = host_path_reason(&soname) {
                return Err(refuse("library", &soname, reason.to_string()));
            }
            if soname.contains('/') {
                return Err(refuse(
                    "library",
                    &soname,
                    "a path; a library is named by its soname, a file name containing .so"
                        .to_string(),
                ));
            }
            if !soname.contains(".so") {
                return Err(refuse(
                    "library",
                    &soname,
                    "not a soname; a library is named by a file name containing .so".to_string(),
                ));
            }
            NeedForm::Library(soname)
        }
        "file" => {
            let file = text("file")?;
            let beside = text("beside")?;
            for (key, value) in [("file", &file), ("beside", &beside)] {
                if let Some(reason) = host_path_reason(value) {
                    return Err(refuse(key, value, reason.to_string()));
                }
                if value.is_empty() || value.contains('/') || value == "." || value == ".." {
                    return Err(refuse(
                        key,
                        value,
                        "not a bare file name; it names a file in the folder its beside library \
                         resolved to"
                            .to_string(),
                    ));
                }
            }
            NeedForm::File { file, beside }
        }
        "program_folder" => {
            let path = text("program_folder")?;
            if path != "." {
                relative_path("program_folder", &path, &refuse)?;
            }
            NeedForm::ProgramFolder(path)
        }
        "system" => {
            let name = text("system")?;
            let location = SystemLocation::ALL
                .into_iter()
                .find(|location| location.as_str() == name)
                .ok_or_else(|| {
                    refuse(
                        "system",
                        &name,
                        format!(
                            "which is not a system location; the known locations are: {}",
                            SystemLocation::ALL.map(SystemLocation::as_str).join(", ")
                        ),
                    )
                })?;
            NeedForm::System(location)
        }
        _ => {
            let path = text("etc")?;
            relative_path("etc", &path, &refuse)?;
            if let Some(location) = credential_location_of(&path) {
                return Err(refuse(
                    "etc",
                    &path,
                    format!(
                        "a credential location: /etc/{location} is never granted, nor a folder \
                         holding it"
                    ),
                ));
            }
            NeedForm::Etc(path)
        }
    };
    Ok(DeclaredNeed {
        form,
        packages: parse_packages(need, field)?,
    })
}

/// Why `value` names a host location rather than something portable, for every form: `None`
/// when it is none of a home folder, an environment expansion, `/proc` or an absolute path.
fn host_path_reason(value: &str) -> Option<&'static str> {
    if value.starts_with('~') {
        Some("a home folder; a need names what the program needs, never where a host keeps it")
    } else if value.contains('$') {
        Some(
            "an environment expansion; a need names what the program needs, never where a host \
             keeps it",
        )
    } else if value == "/proc" || value.starts_with("/proc/") {
        Some("under /proc, and /proc is the runtime's rule, never a need")
    } else if value.starts_with('/') {
        Some(
            "an absolute host path; a need names what the program needs, never where a host \
             keeps it",
        )
    } else {
        None
    }
}

/// Refuses `path` unless it is a relative path of normal components.
pub(crate) fn relative_path(
    key: &str,
    path: &str,
    refuse: &impl Fn(&str, &str, String) -> ManifestError,
) -> Result<(), ManifestError> {
    if let Some(reason) = host_path_reason(path) {
        return Err(refuse(key, path, reason.to_string()));
    }
    if path.split('/').any(|component| component == "..") {
        return Err(refuse(
            key,
            path,
            "a path climbing with '..'; it must stay inside the folder it is relative to"
                .to_string(),
        ));
    }
    if path
        .split('/')
        .any(|component| component.is_empty() || component == ".")
    {
        return Err(refuse(
            key,
            path,
            "not a relative path of plain components (no empty, '.' or '..' component)".to_string(),
        ));
    }
    Ok(())
}

/// The [`ETC_CREDENTIAL_LOCATIONS`] entry `path` (relative to `/etc`) equals, lies beneath, or
/// encloses.
fn credential_location_of(path: &str) -> Option<&'static str> {
    let components: Vec<&str> = path.split('/').collect();
    ETC_CREDENTIAL_LOCATIONS.iter().copied().find(|location| {
        let location: Vec<&str> = location.split('/').collect();
        components.starts_with(&location) || location.starts_with(&components)
    })
}

fn parse_packages(need: &Mapping, field: &str) -> Result<BTreeMap<String, String>, ManifestError> {
    let Some(packages) = need.get(Value::String("packages".to_string())) else {
        return Ok(BTreeMap::new());
    };
    let field = format!("{field}.packages");
    let packages = mapping(packages, &field)?;
    let mut parsed = BTreeMap::new();
    for (id, package) in packages {
        let id_text = id.as_str().unwrap_or_default();
        let valid_id = !id_text.is_empty()
            && id_text.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)
            });
        if !valid_id {
            return Err(invalid(
                format!("{field}.{}", key_text(id)),
                "is not an os-release ID; a packages key is a lowercase ID of [a-z0-9._-], as \
                 /etc/os-release spells ID and ID_LIKE"
                    .to_string(),
            ));
        }
        let entry_field = format!("{field}.{id_text}");
        let package = package.as_str().ok_or_else(|| ManifestError::InvalidType {
            field: entry_field.clone(),
            expected: "string".to_string(),
            got: yaml_type_name(package),
        })?;
        if package.is_empty() || package.chars().any(char::is_whitespace) {
            return Err(invalid(
                entry_field,
                format!("is '{package}', which is not a package name: non-empty, no whitespace"),
            ));
        }
        parsed.insert(id_text.to_string(), package.to_string());
    }
    Ok(parsed)
}

pub(crate) fn mapping<'a>(value: &'a Value, field: &str) -> Result<&'a Mapping, ManifestError> {
    value
        .as_mapping()
        .ok_or_else(|| ManifestError::InvalidType {
            field: field.to_string(),
            expected: "mapping".to_string(),
            got: yaml_type_name(value),
        })
}

pub(crate) fn key_text(key: &Value) -> String {
    key.as_str()
        .map_or_else(|| yaml_type_name(key), str::to_string)
}

pub(crate) fn invalid(field: String, message: String) -> ManifestError {
    ManifestError::InvalidValue { field, message }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(block: &str) -> String {
        format!("name: needs-tool\nversion: 0.1.0\nruntime: tool\n{block}")
    }

    fn refusal(block: &str) -> String {
        parse_program_needs(&tool(block))
            .expect_err("the block is refused")
            .to_string()
    }

    /// The example block of the reference page.
    const EXAMPLE: &str = "programs:
  chromium:
    needs:
      - library: libsoftokn3.so
        packages:
          debian: libnss3
          fedora: nss-softokn
      - file: libsoftokn3.chk
        beside: libsoftokn3.so
      - program_folder: .
      - program_folder: lib/swiftshader
      - system: fonts
      - system: ca_bundle
      - etc: fonts/fonts.conf
";

    #[test]
    fn program_needs_absent_is_empty() {
        assert_eq!(parse_program_needs(&tool("")).unwrap(), Vec::new());
        assert_eq!(parse_program_needs("").unwrap(), Vec::new());
    }

    #[test]
    fn program_needs_every_portable_form_parses() {
        let parsed = parse_program_needs(&tool(EXAMPLE)).unwrap();
        let none = BTreeMap::new;
        let need = |form| DeclaredNeed {
            form,
            packages: none(),
        };
        assert_eq!(
            parsed,
            vec![ProgramNeeds {
                program: "chromium".to_string(),
                needs: vec![
                    DeclaredNeed {
                        form: NeedForm::Library("libsoftokn3.so".to_string()),
                        packages: BTreeMap::from([
                            ("debian".to_string(), "libnss3".to_string()),
                            ("fedora".to_string(), "nss-softokn".to_string()),
                        ]),
                    },
                    need(NeedForm::File {
                        file: "libsoftokn3.chk".to_string(),
                        beside: "libsoftokn3.so".to_string(),
                    }),
                    need(NeedForm::ProgramFolder(".".to_string())),
                    need(NeedForm::ProgramFolder("lib/swiftshader".to_string())),
                    need(NeedForm::System(SystemLocation::Fonts)),
                    need(NeedForm::System(SystemLocation::CaBundle)),
                    need(NeedForm::Etc("fonts/fonts.conf".to_string())),
                ],
            }]
        );
        let labels: Vec<String> = parsed[0].needs.iter().map(|n| n.form.to_string()).collect();
        assert_eq!(
            labels,
            [
                "library libsoftokn3.so",
                "file libsoftokn3.chk beside libsoftokn3.so",
                "program_folder .",
                "program_folder lib/swiftshader",
                "system fonts",
                "system ca_bundle",
                "etc fonts/fonts.conf",
            ]
        );
    }

    #[test]
    fn program_needs_keep_program_order_and_allow_an_empty_list() {
        let parsed = parse_program_needs(&tool(
            "programs:\n  ls:\n    needs: []\n  cat:\n    needs:\n      - library: libm.so.6\n",
        ))
        .unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(
            (parsed[0].program.as_str(), parsed[0].needs.len()),
            ("ls", 0)
        );
        assert_eq!(parsed[1].program, "cat");
    }

    #[test]
    fn program_needs_a_host_path_is_refused_naming_program_index_value_and_reason() {
        for (need, expected) in [
            (
                "program_folder: /home/someone/.fonts",
                "field 'programs.cat.needs[0].program_folder' is '/home/someone/.fonts', an \
                 absolute host path",
            ),
            (
                "library: /proc/self/maps",
                "field 'programs.cat.needs[0].library' is '/proc/self/maps', under /proc, and \
                 /proc is the runtime's rule",
            ),
            (
                "etc: shadow",
                "field 'programs.cat.needs[0].etc' is 'shadow', a credential location",
            ),
            (
                "etc: ssl",
                "field 'programs.cat.needs[0].etc' is 'ssl', a credential location: \
                 /etc/ssl/private",
            ),
            (
                "etc: ssh/sshd_config",
                "field 'programs.cat.needs[0].etc' is 'ssh/sshd_config', a credential location",
            ),
            (
                "etc: ../proc/cpuinfo",
                "field 'programs.cat.needs[0].etc' is '../proc/cpuinfo', a path climbing with '..'",
            ),
            (
                "library: ~/lib/libx.so",
                "field 'programs.cat.needs[0].library' is '~/lib/libx.so', a home folder",
            ),
            (
                "program_folder: $HOME/x",
                "field 'programs.cat.needs[0].program_folder' is '$HOME/x', an environment \
                 expansion",
            ),
            (
                "path: /usr/share/fonts",
                "field 'programs.cat.needs[0].path' is not a need key",
            ),
            (
                "system: everything",
                "field 'programs.cat.needs[0].system' is 'everything', which is not a system \
                 location; the known locations are: fonts, ca_bundle",
            ),
            (
                "library: lib/libx.so",
                "field 'programs.cat.needs[0].library' is 'lib/libx.so', a path",
            ),
            (
                "library: libx",
                "field 'programs.cat.needs[0].library' is 'libx', not a soname",
            ),
            ("program_folder: share/../..", "a path climbing with '..'"),
            (
                "program_folder: ''",
                "not a relative path of plain components",
            ),
            ("etc: .", "not a relative path of plain components"),
            ("etc: /etc/fonts", "an absolute host path"),
            (
                "file: /usr/lib/x.chk\n        beside: libm.so.6",
                "an absolute host path",
            ),
            (
                "file: x.chk",
                "missing required field 'programs.cat.needs[0].beside'",
            ),
        ] {
            let message = refusal(&format!("programs:\n  cat:\n    needs:\n      - {need}\n"));
            assert!(message.contains(expected), "{need}: {message}");
        }
    }

    #[test]
    fn program_needs_a_need_holds_exactly_one_form() {
        let both = refusal(
            "programs:\n  cat:\n    needs:\n      - library: libm.so.6\n        etc: fonts\n",
        );
        assert!(
            both.contains("field 'programs.cat.needs[0]' holds library and etc; a need holds exactly one form"),
            "{both}"
        );
        let none = refusal("programs:\n  cat:\n    needs:\n      - packages: {debian: x}\n");
        assert!(
            none.contains("holds no form; a need holds exactly one form"),
            "{none}"
        );
        let beside = refusal(
            "programs:\n  cat:\n    needs:\n      - library: libm.so.6\n      - file: x.chk\n        beside: libnone.so\n",
        );
        assert!(
            beside.contains(
                "field 'programs.cat.needs[1].beside' is 'libnone.so', which names no library: \
                 entry of program 'cat'"
            ),
            "{beside}"
        );
        let stray = refusal(
            "programs:\n  cat:\n    needs:\n      - etc: fonts\n        beside: libm.so.6\n",
        );
        assert!(stray.contains("goes only with file"), "{stray}");
    }

    #[test]
    fn program_needs_program_keys_and_packages_are_checked() {
        for (block, expected) in [
            ("programs:\n  /usr/bin/cat:\n    needs: []\n", "is not a bare program name"),
            ("programs:\n  '..':\n    needs: []\n", "is not a bare program name"),
            ("programs:\n  cat:\n    needs: []\n    other: 1\n", "programs.cat.other' is not a key of a program"),
            ("programs:\n  cat: {}\n", "field 'programs.cat' holds neither needs nor download"),
            ("programs: [cat]\n", "field 'programs' has invalid type (expected mapping"),
            (
                "programs:\n  cat:\n    needs:\n      - library: libm.so.6\n        packages: {Debian: x}\n",
                "programs.cat.needs[0].packages.Debian' is not an os-release ID",
            ),
            (
                "programs:\n  cat:\n    needs:\n      - library: libm.so.6\n        packages: {debian: 'lib m'}\n",
                "is 'lib m', which is not a package name",
            ),
            (
                "programs:\n  cat:\n    needs:\n      - library: libm.so.6\n        packages: {debian: ''}\n",
                "which is not a package name",
            ),
        ] {
            let message = refusal(block);
            assert!(message.contains(expected), "{block}: {message}");
        }
        let accepted = parse_program_needs(&tool(
            "programs:\n  cat:\n    needs:\n      - library: libm.so.6\n        packages: {opensuse-leap: libm6, rhel_8.1: glibc}\n",
        ))
        .unwrap();
        assert_eq!(accepted[0].needs[0].packages.len(), 2);
    }

    #[test]
    fn program_needs_the_block_is_refused_off_a_tool() {
        for runtime in ["driver", "hook", "skill"] {
            let message = parse_program_needs(&format!(
                "name: x\nversion: 0.1.0\nruntime: {runtime}\nprograms:\n  cat:\n    needs: []\n"
            ))
            .unwrap_err()
            .to_string();
            assert!(
                message.contains(&format!(
                    "field 'programs' is only valid on an artifact whose runtime is tool; this \
                     one is runtime: {runtime}"
                )),
                "{message}"
            );
        }
    }

    #[test]
    fn program_needs_every_credential_location_is_refused_with_its_neighbours() {
        for location in ETC_CREDENTIAL_LOCATIONS {
            for path in [location.to_string(), format!("{location}/inner")] {
                let message = refusal(&format!(
                    "programs:\n  cat:\n    needs:\n      - etc: '{path}'\n"
                ));
                assert!(
                    message.contains("a credential location"),
                    "{path}: {message}"
                );
            }
        }
        assert!(parse_program_needs(&tool(
            "programs:\n  cat:\n    needs:\n      - etc: ssl/certs\n      - etc: shadowsocks\n      - etc: pki/tls/certs\n"
        ))
        .is_ok());
    }
}
