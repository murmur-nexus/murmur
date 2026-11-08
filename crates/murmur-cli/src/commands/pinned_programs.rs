//! Pinned program downloads on the CLI side: `mur install` fetches each one a stored tool pins for
//! this host into `~/.murmur/programs/` ([`fetch_pinned_downloads`]), and `mur run
//! --explain-scope` and `mur doctor` read which copy a launch would run without staging
//! ([`installed_pinned_programs`]). The store itself — commit, verify, the source choice — is
//! [`capsule_runtime::pinned_programs`]; this module does the HTTP.

use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

use capsule_runtime::pinned_programs::{
    commit_download, ensure_bin_link, ensure_program_store, remove_entry, verify_entry,
    DeclaredToolDownloads, EntryState, PartialDownload, PinnedStoreError, ResolvedPinnedProgram,
    MAX_PINNED_DOWNLOAD_BYTES,
};
use capsule_runtime::RuntimeError;
use murmur_artifact::{
    ArtifactName, ArtifactRuntime, LocalRegistry, LockOrigin, MurmurLock, PinnedDownload, Registry,
    RuntimeArtifact,
};

use crate::error::{CliError, E_IO_003, E_REG_013, E_REG_014, E_REG_015};
use crate::stores::FallbackRegistry;

/// How long connecting to a download's host, and waiting for its response to start, may take.
/// The body itself has no deadline: a pinned program can run to hundreds of megabytes, and
/// [`MAX_PINNED_DOWNLOAD_BYTES`] bounds it instead.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(120);

/// What the pinned-download step of `mur install` did for one stored artifact: the lines to print
/// under the install's own output, and the failure, reported under the artifact like any other.
#[derive(Default)]
pub(crate) struct PinnedOutcome {
    pub(crate) lines: Vec<String>,
    pub(crate) failure: Option<CliError>,
}

/// This host's platform as a download is pinned for it, or `<os>-<arch>` on one no download names.
pub(crate) fn platform_label() -> String {
    murmur_artifact::host_download_platform().map_or_else(
        || format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        str::to_string,
    )
}

/// Fetches every download the `runtime: tool` artifact `name@version` (its stored bytes are
/// `payload`) pins for this host's platform into `~/.murmur/programs/`, after the artifact is
/// stored and pinned.
///
/// Per program: an entry that verifies in full is linked and not fetched; one that fails is
/// removed and fetched again; none is fetched, hashed while streaming, checked and committed. A
/// program with downloads for other platforms only gets one line and nothing else. The first
/// failure stops the artifact's remaining programs: `E-REG-013` (transport, status, size),
/// `E-REG-014` (checksum, nothing stored), `E-REG-015` (unpack).
pub(crate) fn fetch_pinned_downloads(name: &str, version: &str, payload: &[u8]) -> PinnedOutcome {
    let mut outcome = PinnedOutcome::default();
    let Ok(manifest_yaml) = murmur_artifact::load_manifest_yaml_from_artifact_bytes(payload) else {
        return outcome;
    };
    if !manifest_yaml.contains(murmur_artifact::PROGRAMS_KEY) {
        return outcome;
    }
    let is_tool = murmur_artifact::declared_runtime_from_artifact_bytes(payload)
        .is_some_and(|declared| declared.artifact_runtime == "tool");
    if !is_tool {
        return outcome;
    }
    let programs = match murmur_artifact::parse_program_downloads(&manifest_yaml) {
        Ok(programs) => programs,
        Err(error) => {
            outcome.failure = Some(CliError::from(RuntimeError::ProgramNeedsMalformed {
                artifact: format!("{name}@{version}"),
                reason: error.to_string(),
            }));
            return outcome;
        }
    };
    if programs.is_empty() {
        return outcome;
    }
    let platform = platform_label();
    let host_platform = murmur_artifact::host_download_platform();
    let mut store_root = None;
    for program in &programs {
        let Some(download) = host_platform.and_then(|platform| program.for_platform(platform))
        else {
            outcome.lines.push(format!(
                "  {} ({name}): no pinned download for {platform}; a host program of that name is \
                 used if the capsule grants it",
                program.program
            ));
            continue;
        };
        let root = match &store_root {
            Some(root) => root,
            None => match ensure_program_store() {
                Ok(root) => store_root.insert(root),
                Err(reason) => {
                    outcome.failure = Some(CliError::new(
                        E_IO_003,
                        format!("the program store cannot be created: {reason}"),
                    ));
                    return outcome;
                }
            },
        };
        let subject = Subject {
            program: &program.program,
            platform: &platform,
            artifact: name,
            version,
            download,
        };
        match subject.install(root) {
            Ok(line) => outcome.lines.push(line),
            Err(error) => {
                outcome.failure = Some(error);
                return outcome;
            }
        }
    }
    outcome
}

/// One program's download for this platform, as install names it.
struct Subject<'a> {
    program: &'a str,
    platform: &'a str,
    artifact: &'a str,
    version: &'a str,
    download: &'a PinnedDownload,
}

impl Subject<'_> {
    /// `<program> for <platform> (<artifact>)`.
    fn label(&self) -> String {
        format!("{} for {} ({})", self.program, self.platform, self.artifact)
    }

    /// `<program> for <platform> (<artifact>@<version>)`, as an error names it.
    fn error_label(&self) -> String {
        format!(
            "{} for {} ({}@{})",
            self.program, self.platform, self.artifact, self.version
        )
    }

    fn install(&self, store_root: &Path) -> Result<String, CliError> {
        let refetched = match verify_entry(store_root, self.download) {
            EntryState::Present { .. } => {
                ensure_bin_link(store_root, self.download, self.program)
                    .map_err(|error| self.store_failure(error))?;
                return Ok(format!("  {}: already in the store", self.label()));
            }
            EntryState::ChecksumFailed { .. } => {
                remove_entry(store_root, &self.download.sha256)
                    .map_err(|error| self.store_failure(error))?;
                true
            }
            EntryState::Missing => false,
        };
        let size = self.fetch(store_root)?;
        Ok(if refetched {
            format!(
                "  {}: the stored copy failed its checksum; fetched again",
                self.label()
            )
        } else {
            format!(
                "  fetched {}: {}, sha256 verified",
                self.label(),
                super::install::format_bytes(size)
            )
        })
    }

    /// Streams the URL into a `.partial-*` folder, hashing as it reads, and commits it.
    fn fetch(&self, store_root: &Path) -> Result<u64, CliError> {
        let url = &self.download.url;
        let partial =
            PartialDownload::begin(store_root).map_err(|error| self.store_failure(error))?;
        let mut file = partial
            .create_download_file()
            .map_err(|error| self.store_failure(error))?;
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_recv_response(Some(RESPONSE_TIMEOUT))
            .build()
            .into();
        let response = agent
            .get(url)
            .header("user-agent", "murmur-cli")
            .call()
            .map_err(|error| self.fetch_failure(&error.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            return Err(self.fetch_failure(&format!("HTTP {}", status.as_u16())));
        }
        let mut capture = Capture {
            body: response.into_body().into_reader(),
            file: &mut file,
            size: 0,
            fault: None,
        };
        let hashed = murmur_artifact::sha256_hex_of_reader(&mut capture);
        let fault = capture.fault.take();
        let (size, got) = match (hashed, fault) {
            (Ok(hashed), _) => hashed,
            (Err(_), Some(CaptureFault::OverCap)) => {
                return Err(self.fetch_failure(&format!(
                    "the download is over the {} cap",
                    super::install::format_bytes(MAX_PINNED_DOWNLOAD_BYTES)
                )))
            }
            (Err(error), Some(CaptureFault::Write)) => {
                return Err(self.store_failure(PinnedStoreError::Store(error.to_string())))
            }
            (Err(error), None) => {
                return Err(self.fetch_failure(&format!("reading the body failed: {error}")))
            }
        };
        file.sync_all()
            .map_err(|error| self.store_failure(PinnedStoreError::Store(error.to_string())))?;
        drop(file);
        if got != self.download.sha256 {
            return Err(CliError::with_hint(
                E_REG_014,
                format!(
                    "the pinned download of {} from {url} has sha256 {got}, and the artifact pins \
                     {}; nothing was stored",
                    self.error_label(),
                    self.download.sha256
                ),
                format!(
                    "the upstream file is not the one {} pinned: install a version of the \
                     artifact whose pin matches, or ask its author to re-pin the download — see \
                     {}",
                    self.artifact,
                    murmur_artifact::diagnostic_link(E_REG_014)
                ),
            ));
        }
        commit_download(store_root, self.download, self.program, partial)
            .map_err(|error| self.store_failure(error))?;
        Ok(size)
    }

    fn fetch_failure(&self, reason: &str) -> CliError {
        CliError::with_hint(
            E_REG_013,
            format!(
                "could not fetch the pinned download of {} from {}: {reason}",
                self.error_label(),
                self.download.url
            ),
            format!(
                "the artifact stays installed and pinned; check this host reaches the URL and run \
                 mur install again to retry — see {}",
                murmur_artifact::diagnostic_link(E_REG_013)
            ),
        )
    }

    fn store_failure(&self, error: PinnedStoreError) -> CliError {
        match error {
            PinnedStoreError::Unpack(reason) => CliError::with_hint(
                E_REG_015,
                format!(
                    "the pinned download of {} cannot be unpacked: {reason}",
                    self.error_label()
                ),
                format!(
                    "a pinned download is a zip whose run: names a regular file, with no symlink \
                     or escaping entry and at most 2 GiB unpacked, or a single file with no run:; \
                     nothing was stored — see {}",
                    murmur_artifact::diagnostic_link(E_REG_015)
                ),
            ),
            PinnedStoreError::Store(reason) => CliError::new(
                E_IO_003,
                format!(
                    "the pinned download of {} could not be written to the program store: \
                     {reason}",
                    self.error_label()
                ),
            ),
        }
    }
}

/// Why [`Capture`] stopped the read, when the response body itself did not fail.
enum CaptureFault {
    /// More than [`MAX_PINNED_DOWNLOAD_BYTES`] arrived.
    OverCap,
    /// Writing to the `.partial-*` download file failed.
    Write,
}

/// A response body that copies every byte it yields into the download file, so one pass both
/// stores and hashes the download ([`murmur_artifact::sha256_hex_of_reader`]), and stops past
/// [`MAX_PINNED_DOWNLOAD_BYTES`]. `fault` tells a cap or write failure from a transport one.
struct Capture<'a, R> {
    body: R,
    file: &'a mut std::fs::File,
    size: u64,
    fault: Option<CaptureFault>,
}

impl<R: Read> Read for Capture<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.body.read(buffer)?;
        self.size += read as u64;
        if self.size > MAX_PINNED_DOWNLOAD_BYTES {
            self.fault = Some(CaptureFault::OverCap);
            return Err(std::io::Error::other("over the size cap"));
        }
        if let Err(error) = self.file.write_all(&buffer[..read]) {
            self.fault = Some(CaptureFault::Write);
            return Err(error);
        }
        Ok(read)
    }
}

/// Every `runtime: tool` entry of `artifacts` that pins a program download, read from
/// `<project_dir>/.murmur/artifacts`, then the global store, at the version and with the lock
/// origin `lock` pins (the manifest's version and `operator` with no lock, as staging reads them).
///
/// Read-only. A tool neither store holds, or one `lock` does not pin, contributes nothing. A
/// malformed `programs:` block refuses with `E-RUN-062`, as the launch would.
pub(crate) fn installed_tool_downloads(
    artifacts: &[RuntimeArtifact],
    lock: Option<&MurmurLock>,
    project_dir: &Path,
) -> Result<Vec<DeclaredToolDownloads>, CliError> {
    let registry = FallbackRegistry {
        primary: LocalRegistry::new(project_dir.join(".murmur").join("artifacts")),
        secondary: LocalRegistry::from_default_home().map_err(CliError::from)?,
    };
    let mut tools = Vec::new();
    for artifact in artifacts {
        if artifact.runtime != ArtifactRuntime::Tool || artifact.source.is_some() {
            continue;
        }
        let (version, origin) = match lock {
            Some(lock) => match lock.artifact_for(&artifact.name) {
                Some(entry) => (entry.resolved_version.clone(), entry.origin.clone()),
                None => continue,
            },
            None => (artifact.version.clone(), LockOrigin::Operator),
        };
        let Ok(name) = ArtifactName::parse(&artifact.name) else {
            continue;
        };
        let Ok(resolved) = registry.resolve(&name, &version) else {
            continue;
        };
        let Ok(manifest_yaml) =
            murmur_artifact::load_manifest_yaml_from_artifact_bytes(&resolved.bytes)
        else {
            continue;
        };
        if !manifest_yaml.contains(murmur_artifact::PROGRAMS_KEY) {
            continue;
        }
        let programs =
            murmur_artifact::parse_program_downloads(&manifest_yaml).map_err(|error| {
                CliError::from(RuntimeError::ProgramNeedsMalformed {
                    artifact: format!("{}@{version}", artifact.name),
                    reason: error.to_string(),
                })
            })?;
        if !programs.is_empty() {
            tools.push(DeclaredToolDownloads {
                artifact: artifact.name.clone(),
                origin,
                programs,
            });
        }
    }
    Ok(tools)
}

/// The source [`capsule_runtime::pinned_programs::derive_pinned_programs`] chooses for each
/// program the installed tools of `artifacts` pin a download of and `shell_allow` grants: what a
/// launch would record in `session_start.effective_grants.pinned_programs`.
pub(crate) fn installed_pinned_programs(
    artifacts: &[RuntimeArtifact],
    lock: Option<&MurmurLock>,
    project_dir: &Path,
    shell_allow: &[String],
) -> Result<Vec<ResolvedPinnedProgram>, CliError> {
    let tools = installed_tool_downloads(artifacts, lock, project_dir)?;
    Ok(capsule_runtime::pinned_programs::derive_pinned_programs(
        &tools,
        shell_allow,
    ))
}
