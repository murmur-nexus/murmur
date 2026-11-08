//! The `download:` entry of a program in a `runtime: tool` artifact's `programs:` block: a pinned
//! upstream download of that program, one per platform.
//!
//! ```yaml
//! programs:
//!   render:
//!     download:
//!       linux-x86_64:
//!         url: https://example.org/render/1.2.0/render-linux-x86_64.zip
//!         sha256: 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
//!         run: render-linux-x86_64/render
//! ```
//!
//! `mur install` fetches the URL once into the host's program store and checks the SHA-256; mur
//! never re-hosts the bytes. `run`, when present, makes the download a zip and names the file to
//! run inside it; without `run` the downloaded bytes are the program. [`parse_program_downloads`]
//! reads the entries; [`crate::parse_program_needs`] refuses a malformed one through the same
//! validator, so `mur build` (`E-BLD-008`) and staging (`E-RUN-062`) agree.

use serde_norway::{Mapping, Value};

use crate::manifest::{parse_manifest_value, yaml_type_name};
use crate::program_needs::{invalid, key_text, mapping, relative_path, PROGRAMS_KEY};
use crate::ManifestError;

/// The key of a program's pinned downloads.
pub const DOWNLOAD_KEY: &str = "download";

/// Every platform a download may be pinned for, as `<os>-<arch>`, sorted.
pub const PINNED_DOWNLOAD_PLATFORMS: [&str; 4] = [
    "darwin-aarch64",
    "darwin-x86_64",
    "linux-aarch64",
    "linux-x86_64",
];

/// The keys of one platform's download.
const DOWNLOAD_ENTRY_KEYS: [&str; 3] = ["url", "sha256", "run"];

/// The hosts an `http://` download URL may name.
const LOOPBACK_HOSTS: [&str; 3] = ["127.0.0.1", "[::1]", "localhost"];

/// This host's platform as [`PINNED_DOWNLOAD_PLATFORMS`] spells it, or `None` on a platform no
/// download may be pinned for.
#[must_use]
pub fn host_download_platform() -> Option<&'static str> {
    platform_of(std::env::consts::OS, std::env::consts::ARCH)
}

fn platform_of(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("linux", "x86_64") => Some("linux-x86_64"),
        ("linux", "aarch64") => Some("linux-aarch64"),
        ("macos", "aarch64") => Some("darwin-aarch64"),
        ("macos", "x86_64") => Some("darwin-x86_64"),
        _ => None,
    }
}

/// One platform's pinned download of a program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedDownload {
    /// One of [`PINNED_DOWNLOAD_PLATFORMS`].
    pub platform: String,
    /// `https://`, or `http://` to a loopback host; no user information, no fragment.
    pub url: String,
    /// The SHA-256 of the downloaded bytes, 64 lowercase hex digits.
    pub sha256: String,
    /// The file to run inside the zip, a relative path of plain components. `None`: the
    /// downloaded bytes are the program itself.
    pub run: Option<String>,
}

/// The pinned downloads one artifact declares for one program, in declaration order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramDownloads {
    /// The program key: a bare name, as `capabilities.shell.allow` spells it.
    pub program: String,
    pub downloads: Vec<PinnedDownload>,
}

impl ProgramDownloads {
    /// The download pinned for `platform`, if any.
    #[must_use]
    pub fn for_platform(&self, platform: &str) -> Option<&PinnedDownload> {
        self.downloads
            .iter()
            .find(|download| download.platform == platform)
    }
}

/// Reads every program's `download:` entry from an artifact's bundled manifest text.
///
/// One [`ProgramDownloads`] per program that holds a `download` key, in declaration order; `Ok`
/// with an empty list when there is no `programs` key or no program pins a download. The whole
/// block is validated first ([`crate::parse_program_needs`]), so `Err` is the same refusal a
/// build gives: a dotted field (`programs.<program>.download.<platform>.<key>`), the value and why.
pub fn parse_program_downloads(
    manifest_yaml: &str,
) -> Result<Vec<ProgramDownloads>, ManifestError> {
    crate::parse_program_needs(manifest_yaml)?;
    let value = parse_manifest_value(manifest_yaml)?;
    let Some(block) = value
        .as_mapping()
        .and_then(|root| root.get(Value::String(PROGRAMS_KEY.to_string())))
        .and_then(Value::as_mapping)
    else {
        return Ok(Vec::new());
    };
    let mut programs = Vec::new();
    for (key, entry) in block {
        let (Some(program), Some(entry)) = (key.as_str(), entry.as_mapping()) else {
            continue;
        };
        let Some(download) = entry.get(Value::String(DOWNLOAD_KEY.to_string())) else {
            continue;
        };
        let field = format!("{PROGRAMS_KEY}.{program}.{DOWNLOAD_KEY}");
        programs.push(ProgramDownloads {
            program: program.to_string(),
            downloads: parse_download(download, &field)?,
        });
    }
    Ok(programs)
}

/// Validates and reads one program's `download:` value; `field` is `programs.<program>.download`.
pub(crate) fn parse_download(
    value: &Value,
    field: &str,
) -> Result<Vec<PinnedDownload>, ManifestError> {
    let platforms = mapping(value, field)?;
    if platforms.is_empty() {
        return Err(invalid(
            field.to_string(),
            format!(
                "is an empty mapping; a download names at least one platform: {}",
                PINNED_DOWNLOAD_PLATFORMS.join(", ")
            ),
        ));
    }
    let mut downloads = Vec::with_capacity(platforms.len());
    for (key, entry) in platforms {
        let platform = key_text(key);
        if !PINNED_DOWNLOAD_PLATFORMS.contains(&platform.as_str()) {
            return Err(invalid(
                format!("{field}.{platform}"),
                format!(
                    "is not a platform a download is pinned for; the platforms are: {}",
                    PINNED_DOWNLOAD_PLATFORMS.join(", ")
                ),
            ));
        }
        let entry_field = format!("{field}.{platform}");
        downloads.push(parse_entry(
            platform,
            mapping(entry, &entry_field)?,
            &entry_field,
        )?);
    }
    Ok(downloads)
}

fn parse_entry(
    platform: String,
    entry: &Mapping,
    field: &str,
) -> Result<PinnedDownload, ManifestError> {
    if let Some(extra) = entry.keys().find(|key| {
        !key.as_str()
            .is_some_and(|k| DOWNLOAD_ENTRY_KEYS.contains(&k))
    }) {
        return Err(invalid(
            format!("{field}.{}", key_text(extra)),
            "is not a key of a pinned download; a pinned download holds url, sha256 and \
             optionally run"
                .to_string(),
        ));
    }
    let text = |key: &str| -> Result<Option<String>, ManifestError> {
        let Some(value) = entry.get(Value::String(key.to_string())) else {
            return Ok(None);
        };
        value
            .as_str()
            .map(|text| Some(text.to_string()))
            .ok_or_else(|| ManifestError::InvalidType {
                field: format!("{field}.{key}"),
                expected: "string".to_string(),
                got: yaml_type_name(value),
            })
    };
    let required = |key: &str| -> Result<String, ManifestError> {
        text(key)?.ok_or_else(|| ManifestError::MissingField {
            field: format!("{field}.{key}"),
        })
    };
    let refuse = |key: &str, value: &str, reason: String| {
        invalid(format!("{field}.{key}"), format!("is '{value}', {reason}"))
    };

    let url = required("url")?;
    if let Some(reason) = url_refusal(&url) {
        return Err(refuse("url", &shown_url(&url), reason));
    }
    let sha256 = required("sha256")?;
    if !is_sha256_hex(&sha256) {
        return Err(refuse(
            "sha256",
            &sha256,
            "which is not a sha256: exactly 64 lowercase hex digits".to_string(),
        ));
    }
    let run = text("run")?;
    if let Some(run) = &run {
        relative_path("run", run, &refuse)?;
    }
    Ok(PinnedDownload {
        platform,
        url,
        sha256,
        run,
    })
}

/// `true` when `text` is exactly 64 lowercase hex digits.
#[must_use]
pub fn is_sha256_hex(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Why `text` may not be a download URL, or `None` when it may.
fn url_refusal(text: &str) -> Option<String> {
    const WHERE: &str =
        "a pinned download is fetched over https, or over http from 127.0.0.1, [::1] or localhost";
    let url = match url::Url::parse(text) {
        Ok(url) => url,
        Err(url::ParseError::EmptyHost) => return Some(format!("a URL with no host; {WHERE}")),
        Err(err) => return Some(format!("not a URL ({err}); {WHERE}")),
    };
    let host = url.host_str().unwrap_or_default();
    match url.scheme() {
        "https" => {}
        "http" if LOOPBACK_HOSTS.contains(&host) => {}
        "http" => {
            return Some(format!(
                "an http URL to a host that is not loopback; {WHERE}"
            ))
        }
        scheme => return Some(format!("a {scheme} URL; {WHERE}")),
    }
    if host.is_empty() {
        return Some(format!("a URL with no host; {WHERE}"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Some(
            "a URL carrying user information before its host; a pinned download names no \
             credential"
                .to_string(),
        );
    }
    if url.fragment().is_some() {
        return Some(
            "a URL with a fragment; a pinned download names the bytes to fetch, nothing after '#'"
                .to_string(),
        );
    }
    None
}

/// `text` as a refusal shows it: a password in it is masked.
fn shown_url(text: &str) -> String {
    match url::Url::parse(text) {
        Ok(mut url) if url.password().is_some() => {
            let _ = url.set_password(Some("***"));
            url.to_string()
        }
        _ => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn tool(block: &str) -> String {
        format!("name: pinned-tool\nversion: 0.1.0\nruntime: tool\n{block}")
    }

    fn entry(url: &str, sha: &str, run: Option<&str>) -> String {
        let run = run.map_or_else(String::new, |run| format!("        run: '{run}'\n"));
        format!(
            "programs:\n  fixture-prog:\n    download:\n      linux-x86_64:\n        url: '{url}'\n        sha256: '{sha}'\n{run}"
        )
    }

    fn refusal(block: &str) -> String {
        let manifest = tool(block);
        let needs = crate::parse_program_needs(&manifest)
            .expect_err("parse_program_needs refuses")
            .to_string();
        let downloads = parse_program_downloads(&manifest)
            .expect_err("parse_program_downloads refuses")
            .to_string();
        assert_eq!(needs, downloads, "both parsers give one refusal");
        needs
    }

    #[test]
    fn program_download_platforms_are_sorted_and_the_host_maps_onto_one() {
        let mut sorted = PINNED_DOWNLOAD_PLATFORMS;
        sorted.sort_unstable();
        assert_eq!(sorted, PINNED_DOWNLOAD_PLATFORMS);
        assert_eq!(platform_of("linux", "x86_64"), Some("linux-x86_64"));
        assert_eq!(platform_of("linux", "aarch64"), Some("linux-aarch64"));
        assert_eq!(platform_of("macos", "aarch64"), Some("darwin-aarch64"));
        assert_eq!(platform_of("macos", "x86_64"), Some("darwin-x86_64"));
        assert_eq!(platform_of("windows", "x86_64"), None);
        if let Some(platform) = host_download_platform() {
            assert!(PINNED_DOWNLOAD_PLATFORMS.contains(&platform));
        }
    }

    #[test]
    fn program_download_two_platforms_parse_in_order_without_needs() {
        let manifest = tool(&format!(
            "programs:\n  fixture-prog:\n    download:\n      linux-x86_64:\n        url: https://example.org/f.zip\n        sha256: {SHA}\n        run: fixture/fixture-prog\n      darwin-aarch64:\n        url: http://127.0.0.1:41234/fixture-prog\n        sha256: {SHA}\n  other:\n    needs: []\n"
        ));
        let downloads = parse_program_downloads(&manifest).unwrap();
        assert_eq!(
            downloads,
            vec![ProgramDownloads {
                program: "fixture-prog".to_string(),
                downloads: vec![
                    PinnedDownload {
                        platform: "linux-x86_64".to_string(),
                        url: "https://example.org/f.zip".to_string(),
                        sha256: SHA.to_string(),
                        run: Some("fixture/fixture-prog".to_string()),
                    },
                    PinnedDownload {
                        platform: "darwin-aarch64".to_string(),
                        url: "http://127.0.0.1:41234/fixture-prog".to_string(),
                        sha256: SHA.to_string(),
                        run: None,
                    },
                ],
            }]
        );
        assert_eq!(
            downloads[0].for_platform("darwin-aarch64").unwrap().run,
            None
        );
        assert!(downloads[0].for_platform("linux-aarch64").is_none());
        let needs = crate::parse_program_needs(&manifest).unwrap();
        assert_eq!(needs.len(), 2);
        assert_eq!(needs[0].program, "fixture-prog");
        assert!(needs[0].needs.is_empty());
        assert!(parse_program_downloads(&tool("")).unwrap().is_empty());
    }

    #[test]
    fn program_download_loopback_http_urls_are_accepted() {
        for url in [
            "http://127.0.0.1:1/x.zip",
            "http://[::1]:8080/x",
            "http://localhost/x.zip",
            "https://storage.example.com/a/b/c.zip?x=1",
        ] {
            parse_program_downloads(&tool(&entry(url, SHA, None)))
                .unwrap_or_else(|err| panic!("{url}: {err}"));
        }
    }

    #[test]
    fn program_download_refuses_a_url_that_is_not_https_or_loopback_http() {
        let field = "field 'programs.fixture-prog.download.linux-x86_64.url' is";
        for (url, reason) in [
            (
                "http://example.com/x.zip",
                "an http URL to a host that is not loopback",
            ),
            ("ftp://h/x", "a ftp URL"),
            ("https://", "a URL with no host"),
            ("https://:443/x.zip", "a URL with no host"),
            ("not a url", "not a URL"),
            (
                "https://user@host.example/x.zip",
                "a URL carrying user information",
            ),
            ("https://host.example/x.zip#frag", "a URL with a fragment"),
        ] {
            let message = refusal(&entry(url, SHA, None));
            assert!(message.contains(&format!("{field} '")), "{url}: {message}");
            assert!(message.contains(reason), "{url}: {message}");
        }
        let masked = refusal(&entry("https://u:secret@h.example/x", SHA, None));
        assert!(!masked.contains("secret"), "{masked}");
    }

    #[test]
    fn program_download_refuses_a_sha256_that_is_not_64_lowercase_hex() {
        for sha in [&SHA[1..], &SHA.to_uppercase(), &SHA.replace('a', "g"), ""] {
            let message = refusal(&entry("https://h.example/x", sha, None));
            assert!(
                message.contains(&format!(
                    "field 'programs.fixture-prog.download.linux-x86_64.sha256' is '{sha}', which is not a sha256: exactly 64 lowercase hex digits"
                )),
                "{message}"
            );
        }
    }

    #[test]
    fn program_download_refuses_a_run_that_is_not_a_plain_relative_path() {
        for (run, reason) in [
            ("/abs", "an absolute host path"),
            ("a/../b", "a path climbing with '..'"),
            ("", "not a relative path of plain components"),
            ("a//b", "not a relative path of plain components"),
            ("./a", "not a relative path of plain components"),
            ("~/x", "a home folder"),
            ("$X/y", "an environment expansion"),
        ] {
            let message = refusal(&entry("https://h.example/x", SHA, Some(run)));
            assert!(
                message.contains(&format!(
                    "field 'programs.fixture-prog.download.linux-x86_64.run' is '{run}', {reason}"
                )),
                "{run}: {message}"
            );
        }
    }

    #[test]
    fn program_download_refuses_unknown_keys_platforms_and_shapes() {
        for (block, expected) in [
            (
                format!("{}        size: 3\n", entry("https://h.example/x", SHA, None)),
                "field 'programs.fixture-prog.download.linux-x86_64.size' is not a key of a pinned download".to_string(),
            ),
            (
                "programs:\n  fixture-prog:\n    download: {}\n".to_string(),
                "field 'programs.fixture-prog.download' is an empty mapping".to_string(),
            ),
            (
                format!("programs:\n  p:\n    download:\n      windows-x86_64:\n        url: https://h.example/x\n        sha256: {SHA}\n"),
                "field 'programs.p.download.windows-x86_64' is not a platform a download is pinned for; the platforms are: darwin-aarch64, darwin-x86_64, linux-aarch64, linux-x86_64".to_string(),
            ),
            (
                format!("programs:\n  p:\n    download:\n      linux-x86_64:\n        sha256: {SHA}\n"),
                "missing required field 'programs.p.download.linux-x86_64.url'".to_string(),
            ),
            (
                "programs:\n  p:\n    download:\n      linux-x86_64:\n        url: https://h.example/x\n".to_string(),
                "missing required field 'programs.p.download.linux-x86_64.sha256'".to_string(),
            ),
            (
                "programs:\n  p:\n    download: [x]\n".to_string(),
                "field 'programs.p.download' has invalid type (expected mapping".to_string(),
            ),
            (
                "programs:\n  p: {}\n".to_string(),
                "field 'programs.p' holds neither needs nor download".to_string(),
            ),
        ] {
            let message = refusal(&block);
            assert!(message.contains(&expected), "{block}: {message}");
        }
    }

    #[test]
    fn program_download_is_refused_off_a_tool() {
        let message = parse_program_downloads(&format!(
            "name: x\nversion: 0.1.0\nruntime: driver\n{}",
            entry("https://h.example/x", SHA, None)
        ))
        .unwrap_err()
        .to_string();
        assert!(
            message.contains("field 'programs' is only valid on an artifact whose runtime is tool"),
            "{message}"
        );
    }
}
