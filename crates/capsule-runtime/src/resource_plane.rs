//! The resource plane: the read-only file surface a capsule declares under `exports.files`.
//!
//! Two verbs, `list` and `read`, served host-side off the ordinary host path the runtime already
//! holds for the workdir. No wasm is instantiated, no store is locked and no running turn is
//! consulted, so the plane answers whether the capsule is idle or mid-task — and a read never
//! costs an inference turn.
//!
//! Three responsibilities are kept apart:
//!
//! * The **agent** owns what a file contains. Nothing here asks it anything.
//! * The **runtime** owns who may read it. Path resolution lives entirely in this module: a
//!   caller never sanitises a path, because a gateway that normalises `..` into something
//!   servable has taken over the containment boundary, and the component holding that boundary
//!   must not think. Every escaping request is refused, never repaired.
//! * The **trace** owns the record. Every read and every refusal lands in `trace.jsonl` at the
//!   moment of the request, because the agent has been removed from the loop and the trace is
//!   the only account of what left the capsule and when.
//!
//! Declaring an export does not change the achieved containment class — see
//! [`crate::containment`]. Containment describes the guest's reach outward; an export widens the
//! operator's reach inward and hands the guest nothing.
//!
//! The filesystem work is synchronous `std::fs` run on [`tokio::task::spawn_blocking`], and the
//! resolve-then-verify-prefix path is the only one: `openat2(RESOLVE_BENEATH)` is Linux-only and
//! could never be more than an extra layer over a portable path that has to be correct by
//! itself, so this module keeps the portable path alone rather than two that can disagree.

use std::{
    io::Read,
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::UNIX_EPOCH,
};

use murmur_artifact::{ContainmentClass, ExportMode, FileExport};
use serde::Serialize;

use crate::{errors::RuntimeError, trace::ResourceTraceAppender};

/// Path prefix the plane answers under. Everything below it is `GET`-only.
pub const RESOURCE_PATH_PREFIX: &str = "/resources/";

/// The one file-surface route, relative to [`RESOURCE_PATH_PREFIX`].
const FILES_ROUTE: &str = "files";

// ── Declared export ───────────────────────────────────────────────────────────

/// One `exports.files` block, with its root already resolved against the session's accessible
/// workdir.
///
/// The root is *joined*, not canonicalised, at construction: `exports.files.root` is not required
/// to exist when the capsule launches, because the agent may create it during a task. Every
/// request canonicalises it afresh, which is also what lets a `list` answer `200` with no entries
/// before the agent has written anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredExport {
    /// `exports.files.root` verbatim, as the operator wrote it (`out/`). Echoed in every
    /// response and every trace line; never used to touch the filesystem.
    pub declared_root: String,
    /// `accessible_workdir.join(declared_root)`.
    pub root: PathBuf,
    pub mode: ExportMode,
    /// Per-file read ceiling. A file above it is still *listed*, with its real size — discovery
    /// must not lie about what is there — and refused on read.
    pub max_bytes: u64,
}

impl DeclaredExport {
    /// Resolves a manifest's `exports.files` block against the session's accessible workdir —
    /// the directory the agent's own tools see at `.`, and therefore the one `out/result.txt`
    /// means. Not `StagedSession::workdir`, which is the internal `.murmur/<session_id>`
    /// bookkeeping directory when `--workdir` is given, and holds `trace.jsonl`.
    pub fn resolve(accessible_workdir: &Path, export: &FileExport) -> Self {
        Self {
            declared_root: export.root.clone(),
            root: accessible_workdir.join(&export.root),
            mode: export.mode,
            max_bytes: export.max_bytes,
        }
    }
}

/// Refuses a launch whose declared export root already resolves outside the accessible workdir.
///
/// Called from `stage_session` before the workdir is created, so a capsule whose `out/` is a
/// symlink into someone else's directory never runs at all — the alternative is discovering it
/// one served file at a time, by which point the first file has left. A root that does not yet
/// exist is accepted: the agent may create it during a task, and every request resolves the root
/// afresh.
pub fn check_export_root(
    accessible_workdir: &Path,
    export: &FileExport,
) -> Result<(), RuntimeError> {
    let Ok(workdir_canon) = std::fs::canonicalize(accessible_workdir) else {
        // No `--workdir`: the session directory does not exist yet, so nothing under it can
        // already point elsewhere.
        return Ok(());
    };
    let Ok(resolved) = std::fs::canonicalize(workdir_canon.join(&export.root)) else {
        return Ok(());
    };
    if !resolved.starts_with(&workdir_canon) {
        return Err(RuntimeError::ExportRootOutsideWorkdir {
            declared: export.root.clone(),
            resolved: resolved.display().to_string(),
            workdir: workdir_canon.display().to_string(),
        });
    }
    Ok(())
}

/// Everything the resource plane needs to answer a request, and nothing that a running task
/// leaves behind.
///
/// Deliberately constructible from an export declaration, a workdir path and a containment class
/// alone: a later reader-only launch mode must be able to build one over an existing workdir
/// without starting the engine, and would be correct to report `generation: 0` there, since no
/// task has completed *in that process*.
pub struct ResourcePlane {
    /// `None` means the capsule declared no `exports.files`, which is the deny case: every
    /// request answers `no_resource_plane` and still lands in the trace.
    export: Option<DeclaredExport>,
    /// The class this session actually achieved — never the declared floor. Reported on every
    /// response and used to key the symlink decision (see [`symlink_policy`]).
    containment_achieved: ContainmentClass,
    /// Incremented by the agent loop when a task reaches a terminal state. Provenance, never a
    /// pin: see [`ResourcePlane::generation`].
    generation: Arc<AtomicU64>,
    /// `None` when there is nowhere to write a record — a plane built outside a session. A
    /// session-backed plane always has one.
    trace: Option<Arc<ResourceTraceAppender>>,
}

impl ResourcePlane {
    /// The five inputs a plane needs, and the complete list of them: a host path, the declared
    /// export (`None` = undeclared = deny), the achieved containment class, the generation
    /// counter and somewhere to write the record. None of them is state a completed task left
    /// behind, which is what lets a later reader-only launch mode build one over an existing
    /// workdir without starting the engine.
    pub fn new(
        accessible_workdir: &Path,
        export: Option<&FileExport>,
        containment_achieved: ContainmentClass,
        generation: Arc<AtomicU64>,
        trace: Option<Arc<ResourceTraceAppender>>,
    ) -> Self {
        Self::with_export(
            export.map(|export| DeclaredExport::resolve(accessible_workdir, export)),
            containment_achieved,
            generation,
            trace,
        )
    }

    /// [`Self::new`] with the root already resolved.
    pub fn with_export(
        export: Option<DeclaredExport>,
        containment_achieved: ContainmentClass,
        generation: Arc<AtomicU64>,
        trace: Option<Arc<ResourceTraceAppender>>,
    ) -> Self {
        Self {
            export,
            containment_achieved,
            generation,
            trace,
        }
    }

    /// Which turn the bytes belong to: `0` until the first task in this process reaches a
    /// terminal state, then one more per completed task.
    ///
    /// It answers *these bytes are as of turn N* and nothing else. It is not an argument — no
    /// request selects a generation, no response conflicts because the generation has moved on,
    /// and superseded bytes are not retained. A capsule that is finished but alive and rewrites a
    /// file two turns later simply serves the newer file; the `etag` is how a caller notices.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    pub fn containment_achieved(&self) -> ContainmentClass {
        self.containment_achieved
    }
}

// ── Wire shapes ───────────────────────────────────────────────────────────────

/// A status, headers and a body — everything a transport needs and nothing about the transport.
///
/// [`handle_resource_request`] returns this rather than writing to a socket so a later card can
/// bind the same logic on its own listener behind its own authoriser without touching any of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl ResourceResponse {
    /// Builds a response with `content-length` taken from the body it carries.
    ///
    /// Framing is settled here rather than left to the transport so that a later listener binding
    /// [`handle_resource_request`] gets a self-delimiting response without having to know it owed
    /// one. A body delimited only by the connection closing is a body a caller cannot tell apart
    /// from a truncated one, which would undo the point of serving a validator alongside it.
    fn framed(status: u16, mut headers: Vec<(String, String)>, body: Vec<u8>) -> Self {
        headers.push(("content-length".to_string(), body.len().to_string()));
        Self {
            status,
            headers,
            body,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ListResponse {
    pub root: String,
    pub mode: ExportMode,
    pub max_bytes: u64,
    pub generation: u64,
    pub containment_achieved: ContainmentClass,
    pub entries: Vec<ListEntry>,
}

/// One regular file under the export root. `size_bytes`, `mtime_ms` and `sha256` all come from a
/// single open of that file, so they describe one version of it and never a mixture of two.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ListEntry {
    /// Relative to the export root, `/`-separated.
    pub path: String,
    pub size_bytes: u64,
    pub mtime_ms: u64,
    pub sha256: String,
}

/// The bytes of one file together with the validator that describes *those* bytes.
///
/// The hash is computed from the buffer actually served, not from a second pass over the path, so
/// the `etag` and the body are the same version by construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadResponse {
    pub bytes: Vec<u8>,
    pub sha256: String,
    pub mtime_ms: u64,
}

// ── Errors ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceError {
    /// The manifest declares no `exports.files`. Absent means deny.
    NoResourcePlane,
    NotFound,
    OutsideRoot,
    /// A symlink was encountered and the achieved class is `scoped`.
    SymlinkRefused,
    NotARegularFile,
    TooLarge {
        max_bytes: u64,
    },
    MethodNotAllowed,
    IoError(String),
}

impl ResourceError {
    /// The stable `error` string in the JSON body and the `outcome` in the trace — one vocabulary
    /// for both, so a refusal an auditor reads is spelled the way the caller saw it.
    pub fn code(&self) -> &'static str {
        match self {
            ResourceError::NoResourcePlane => "no_resource_plane",
            ResourceError::NotFound => "not_found",
            ResourceError::OutsideRoot => "outside_root",
            ResourceError::SymlinkRefused => "symlink_refused",
            ResourceError::NotARegularFile => "not_a_regular_file",
            ResourceError::TooLarge { .. } => "too_large",
            ResourceError::MethodNotAllowed => "method_not_allowed",
            ResourceError::IoError(_) => "io_error",
        }
    }

    pub fn status(&self) -> u16 {
        match self {
            ResourceError::NoResourcePlane | ResourceError::NotFound => 404,
            ResourceError::OutsideRoot
            | ResourceError::SymlinkRefused
            | ResourceError::NotARegularFile => 403,
            ResourceError::TooLarge { .. } => 413,
            ResourceError::MethodNotAllowed => 405,
            ResourceError::IoError(_) => 500,
        }
    }

    pub fn message(&self) -> String {
        match self {
            ResourceError::NoResourcePlane => {
                "this capsule declares no exports.files block, so it has no resource plane"
                    .to_string()
            }
            ResourceError::NotFound => "no such file under the export root".to_string(),
            ResourceError::OutsideRoot => {
                "the requested path resolves outside the export root".to_string()
            }
            ResourceError::SymlinkRefused => {
                "a symlink was encountered and the achieved containment class is scoped".to_string()
            }
            ResourceError::NotARegularFile => {
                "the requested path is not a regular file".to_string()
            }
            ResourceError::TooLarge { max_bytes } => {
                format!("file exceeds the declared exports.files.max_bytes of {max_bytes} bytes")
            }
            ResourceError::MethodNotAllowed => {
                "the resource plane serves GET only; it has no write path".to_string()
            }
            ResourceError::IoError(detail) => format!("read failed: {detail}"),
        }
    }
}

fn io_to_resource_error(error: &std::io::Error) -> ResourceError {
    match error.kind() {
        std::io::ErrorKind::NotFound => ResourceError::NotFound,
        // A component of the path is not a directory, or O_NOFOLLOW hit a symlink on the final
        // component: both mean the name does not designate a servable regular file here.
        std::io::ErrorKind::NotADirectory => ResourceError::NotFound,
        std::io::ErrorKind::PermissionDenied => ResourceError::IoError(error.to_string()),
        _ if error.raw_os_error() == Some(libc::ELOOP) => ResourceError::SymlinkRefused,
        _ => ResourceError::IoError(error.to_string()),
    }
}

// ── The class-keyed symlink decision ──────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymlinkPolicy {
    /// Refuse any symlink encountered on the resolved path, and omit symlinked entries from a
    /// listing.
    Refuse,
    /// Follow symlinks, and serve only when the fully-resolved target is still beneath the export
    /// root.
    FollowWithinRoot,
}

/// What a symlink under the export root means at each achieved containment class.
///
/// Pure and class-keyed, mirroring the seam `containment::achieved_class_for_tier` uses, so the
/// decision is testable across all three variants without a host probe — which matters because
/// `scoped` is unreachable on darwin and on any Linux without a usable Landlock ABI.
///
/// * `scoped` — host-path grants are possible and the filesystem's shape stays visible, so a
///   symlink under the export root could target a granted host path. Refuse it outright.
/// * `sealed` — the workdir is the only writable path and there is no outside to name, so
///   everything under the export root is capsule-authored. Follow, and check the target.
/// * `advisory` — a convention on top of a convention. Same rule as `sealed`, and every response
///   says `advisory` so a caller knows what it is trusting.
pub fn symlink_policy(class: ContainmentClass) -> SymlinkPolicy {
    match class {
        ContainmentClass::Scoped => SymlinkPolicy::Refuse,
        ContainmentClass::Sealed | ContainmentClass::Advisory => SymlinkPolicy::FollowWithinRoot,
    }
}

// ── Path resolution ───────────────────────────────────────────────────────────

/// Percent-decoding, applied **before** any validation.
///
/// Order is the whole point: validating first is exactly what lets `%2e%2e%2f` through as an
/// opaque-looking component that the filesystem then reads as `../`. An incomplete or non-hex
/// escape is left as the literal bytes it was written with rather than rejected here — whatever
/// it decodes to still has to survive [`export_relpath_components`].
fn percent_decode(raw: &str) -> Vec<u8> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3])
                .ok()
                .and_then(|pair| u8::from_str_radix(pair, 16).ok());
            if let Some(byte) = hex {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    out
}

/// Decodes a request path and lowers it to the components that may be joined onto the export
/// root, refusing everything else without touching the filesystem.
///
/// Refused: an empty path, an absolute path, a NUL byte, bytes that are not UTF-8, and any
/// component equal to `""`, `"."` or `".."`. `.` is refused rather than skipped for the same
/// reason `..` is: the runtime does not repair a request into something servable, and a caller
/// that wanted `a/b` should ask for `a/b`.
pub(crate) fn export_relpath_components(raw: &str) -> Result<Vec<String>, ResourceError> {
    let decoded = percent_decode(raw);
    if decoded.is_empty() || decoded.contains(&0) {
        return Err(ResourceError::OutsideRoot);
    }
    let decoded = String::from_utf8(decoded).map_err(|_| ResourceError::OutsideRoot)?;
    if decoded.starts_with('/') || Path::new(&decoded).is_absolute() {
        return Err(ResourceError::OutsideRoot);
    }
    // One trailing slash names nothing new, so it is dropped before splitting. Every empty segment
    // that survives is a `//` inside the path and is refused with the rest.
    let mut components = Vec::new();
    for part in decoded.strip_suffix('/').unwrap_or(&decoded).split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return Err(ResourceError::OutsideRoot);
        }
        components.push(part.to_string());
    }
    if components.is_empty() {
        return Err(ResourceError::OutsideRoot);
    }
    // Nothing above can produce a `..`, a root or a Windows prefix, but the join below is the
    // step that would act on one, so the invariant is asserted where it is relied upon.
    for component in &components {
        if Path::new(component)
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(ResourceError::OutsideRoot);
        }
    }
    Ok(components)
}

/// Canonicalises the export root and the target and requires the target to be the root or beneath
/// it — the portable resolve-then-verify-prefix path, and the only one.
///
/// `Path::starts_with` compares whole components, so `/w/outside/x` is correctly *not* beneath
/// `/w/out`.
fn resolve_beneath(
    root_canon: &Path,
    components: &[String],
    policy: SymlinkPolicy,
) -> Result<PathBuf, ResourceError> {
    let mut target = root_canon.to_path_buf();
    if policy == SymlinkPolicy::Refuse {
        // Walk component by component so a symlink is caught wherever it sits on the path, not
        // only on the final element — an intermediate symlinked directory redirects everything
        // below it just as effectively.
        for component in components {
            target.push(component);
            let metadata =
                std::fs::symlink_metadata(&target).map_err(|e| io_to_resource_error(&e))?;
            if metadata.file_type().is_symlink() {
                return Err(ResourceError::SymlinkRefused);
            }
        }
    } else {
        for component in components {
            target.push(component);
        }
    }
    let canonical = std::fs::canonicalize(&target).map_err(|e| io_to_resource_error(&e))?;
    if !canonical.starts_with(root_canon) {
        return Err(ResourceError::OutsideRoot);
    }
    Ok(canonical)
}

/// Opens `path` without following a symlink on its final component and without blocking on a
/// FIFO, then confirms from the open descriptor that it is a regular file.
///
/// `O_NONBLOCK` is what keeps a FIFO under the export root from parking the request until a
/// writer appears; the `fstat` behind [`std::fs::File::metadata`] is what turns it, and a socket
/// or device node, into `not_a_regular_file`.
fn open_regular_file(path: &Path) -> Result<(std::fs::File, std::fs::Metadata), ResourceError> {
    use std::os::unix::fs::OpenOptionsExt;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| io_to_resource_error(&e))?;
    let metadata = file.metadata().map_err(|e| io_to_resource_error(&e))?;
    if !metadata.is_file() {
        return Err(ResourceError::NotARegularFile);
    }
    Ok((file, metadata))
}

fn mtime_ms(metadata: &std::fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ── read ──────────────────────────────────────────────────────────────────────

/// Serves one file's current bytes.
///
/// Atomicity against a concurrent rewrite rests on two rules and no locking: the file is opened
/// exactly once and its bytes, size and mtime all come from that one descriptor — a path can name
/// a different inode by the time it is looked at twice — and the hash is taken over the buffer
/// actually returned. An agent that rewrites by writing a temp file and renaming it into place
/// therefore holds this reader on the old inode for the whole read; one that truncates and
/// rewrites in place can still be read mid-write. That is an authoring convention, documented for
/// capsule authors, not a mechanism enforced here.
fn read_export_file_with_policy(
    export: &DeclaredExport,
    relpath: &str,
    policy: SymlinkPolicy,
) -> Result<ReadResponse, ResourceError> {
    let components = export_relpath_components(relpath)?;
    let root_canon = std::fs::canonicalize(&export.root).map_err(|e| io_to_resource_error(&e))?;
    let target = resolve_beneath(&root_canon, &components, policy)?;

    let (file, metadata) = open_regular_file(&target)?;
    if metadata.len() > export.max_bytes {
        return Err(ResourceError::TooLarge {
            max_bytes: export.max_bytes,
        });
    }
    let mtime = mtime_ms(&metadata);

    // Bounded by the declared ceiling plus one byte, so a file that grew between the fstat above
    // and this read is refused rather than served past the ceiling.
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    let mut limited = file.take(export.max_bytes.saturating_add(1));
    limited
        .read_to_end(&mut bytes)
        .map_err(|e| io_to_resource_error(&e))?;
    if bytes.len() as u64 > export.max_bytes {
        return Err(ResourceError::TooLarge {
            max_bytes: export.max_bytes,
        });
    }

    let sha256 = murmur_artifact::sha256_hex(&bytes);
    Ok(ReadResponse {
        bytes,
        sha256,
        mtime_ms: mtime,
    })
}

// ── list ──────────────────────────────────────────────────────────────────────

/// Every regular file under the export root, sorted by path.
///
/// A missing root is `Ok(vec![])`, never a `404`: `exports.files.root` is not required to exist at
/// launch, and only an *undeclared* plane refuses a listing. A file above `max_bytes` is listed
/// with its real size — discovery must not lie about what is there — and refused on read.
fn list_export_files(
    export: &DeclaredExport,
    policy: SymlinkPolicy,
) -> Result<Vec<ListEntry>, ResourceError> {
    let root_canon = match std::fs::canonicalize(&export.root) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(io_to_resource_error(&error)),
    };
    if !root_canon.is_dir() {
        return Ok(Vec::new());
    }

    let mut entries = Vec::new();
    let mut pending = vec![(root_canon.clone(), String::new())];
    while let Some((dir, prefix)) = pending.pop() {
        let reader = match std::fs::read_dir(&dir) {
            Ok(reader) => reader,
            // A directory that vanished mid-walk (the agent is still working) drops out of the
            // listing rather than failing the whole request.
            Err(_) => continue,
        };
        for entry in reader.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let relative = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let path = entry.path();

            if file_type.is_symlink() {
                if policy == SymlinkPolicy::Refuse {
                    continue;
                }
                // Follow, but only onto a regular file still beneath the root. A symlinked
                // *directory* is never descended into: it would list the same bytes twice under
                // two names and, if it pointed at an ancestor, would not terminate.
                let Ok(canonical) = std::fs::canonicalize(&path) else {
                    continue;
                };
                if !canonical.starts_with(&root_canon) {
                    continue;
                }
                if let Some(entry) = describe_entry(&canonical, relative) {
                    entries.push(entry);
                }
                continue;
            }

            if file_type.is_dir() {
                pending.push((path, relative));
            } else if file_type.is_file() {
                if let Some(entry) = describe_entry(&path, relative) {
                    entries.push(entry);
                }
            }
        }
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(entries)
}

/// One listing entry, taken from one open of the file: the size is the number of bytes hashed,
/// so `size_bytes` and `sha256` describe the same version of it. `None` when the file went away
/// or turned out not to be a regular file between the walk and the open.
fn describe_entry(path: &Path, relative: String) -> Option<ListEntry> {
    let (mut file, metadata) = open_regular_file(path).ok()?;
    let mtime = mtime_ms(&metadata);
    // Streamed rather than buffered: a listing is not bounded by `max_bytes` — an oversized file
    // is still listed — so it must not be bounded by memory either.
    let (size_bytes, sha256) = murmur_artifact::sha256_hex_of_reader(&mut file).ok()?;
    Some(ListEntry {
        path: relative,
        size_bytes,
        mtime_ms: mtime,
        sha256,
    })
}

// ── The transport-agnostic handler ────────────────────────────────────────────

/// Answers one resource-plane request: a method, a raw request path, and nothing else.
///
/// Knows no socket, no framing and no authoriser, so a later card can bind it on a second
/// listener with its own authentication by calling exactly this function.
///
/// `raw_path` is the request target verbatim, still percent-encoded and possibly carrying a query
/// string. Decoding is this function's job, never the caller's.
pub async fn handle_resource_request(
    plane: &ResourcePlane,
    method: &str,
    raw_path: &str,
) -> ResourceResponse {
    let path = raw_path.split('?').next().unwrap_or("");
    let Some(rest) = path.strip_prefix(RESOURCE_PATH_PREFIX) else {
        return error_response(&ResourceError::NotFound, None);
    };

    if rest == FILES_ROUTE || rest == "files/" {
        return handle_list(plane, method).await;
    }
    if let Some(relpath) = rest.strip_prefix("files/") {
        return handle_read(plane, method, relpath).await;
    }
    // No route to name, so nothing is traced — but the method still answers first. A `PUT` at an
    // unknown path under the prefix is an attempt to write, and `405` with an `allow: GET` says
    // there is no write path anywhere here, where a `404` would only say "not that one".
    if method != "GET" {
        return error_response(&ResourceError::MethodNotAllowed, None);
    }
    error_response(&ResourceError::NotFound, None)
}

/// The declared export a request may be answered from, or the refusal that stands in its place.
///
/// The method check comes first because it is a property of the request alone: a `PUT` is refused
/// identically whether or not the capsule exports anything.
fn servable_export<'a>(
    plane: &'a ResourcePlane,
    method: &str,
) -> Result<&'a DeclaredExport, ResourceError> {
    if method != "GET" {
        return Err(ResourceError::MethodNotAllowed);
    }
    plane.export.as_ref().ok_or(ResourceError::NoResourcePlane)
}

async fn handle_list(plane: &ResourcePlane, method: &str) -> ResourceResponse {
    let generation = plane.generation();
    let class = plane.containment_achieved;

    let export = match servable_export(plane, method) {
        Ok(export) => export.clone(),
        // No declared root to name: the record still says what was asked for and why it was
        // refused, which is the whole of what an auditor needs from a denied request.
        Err(error) => return refuse_list(plane, "", &error, generation, class).await,
    };

    let listed = {
        let export = export.clone();
        let policy = symlink_policy(class);
        tokio::task::spawn_blocking(move || list_export_files(&export, policy))
            .await
            .unwrap_or_else(|join| Err(ResourceError::IoError(join.to_string())))
    };

    let entries = match listed {
        Ok(entries) => entries,
        Err(error) => {
            return refuse_list(plane, &export.declared_root, &error, generation, class).await
        }
    };

    if let Some(trace) = &plane.trace {
        trace
            .write_resource_list(
                &export.declared_root,
                entries.len(),
                entries.iter().map(|entry| entry.size_bytes).sum(),
                generation,
                class,
                "ok",
                None,
            )
            .await;
    }

    let body = ListResponse {
        root: export.declared_root.clone(),
        mode: export.mode,
        max_bytes: export.max_bytes,
        generation,
        containment_achieved: class,
        entries,
    };
    ResourceResponse::framed(
        200,
        vec![
            ("content-type".to_string(), "application/json".to_string()),
            ("x-murmur-generation".to_string(), generation.to_string()),
            ("x-murmur-containment".to_string(), class.to_string()),
            ("x-murmur-export-root".to_string(), export.declared_root),
        ],
        serde_json::to_vec(&body).unwrap_or_default(),
    )
}

async fn handle_read(plane: &ResourcePlane, method: &str, relpath: &str) -> ResourceResponse {
    let generation = plane.generation();
    let class = plane.containment_achieved;
    // Decoded, but never validated or normalised: `%2e%2e%2f` and `../` are one attempt and read
    // as one finding, while a refused path appears in the record as what it asked for rather than
    // as something this module made servable.
    let traced_path = decoded_request_path(relpath);

    let export = match servable_export(plane, method) {
        Ok(export) => export.clone(),
        Err(error) => return refuse_read(plane, &traced_path, &error, generation, class).await,
    };

    let read = {
        let export = export.clone();
        let policy = symlink_policy(class);
        let relpath = relpath.to_string();
        tokio::task::spawn_blocking(move || read_export_file_with_policy(&export, &relpath, policy))
            .await
            .unwrap_or_else(|join| Err(ResourceError::IoError(join.to_string())))
    };

    let response = match read {
        Ok(response) => response,
        Err(error) => return refuse_read(plane, &traced_path, &error, generation, class).await,
    };

    if let Some(trace) = &plane.trace {
        trace
            .write_resource_read(
                &traced_path,
                "ok",
                Some(response.bytes.len() as u64),
                Some(response.sha256.clone()),
                generation,
                class,
                None,
            )
            .await;
    }

    ResourceResponse::framed(
        200,
        vec![
            (
                "content-type".to_string(),
                "application/octet-stream".to_string(),
            ),
            (
                "etag".to_string(),
                format!("\"sha256:{}\"", response.sha256),
            ),
            (
                "x-murmur-mtime-ms".to_string(),
                response.mtime_ms.to_string(),
            ),
            ("x-murmur-generation".to_string(), generation.to_string()),
            ("x-murmur-containment".to_string(), class.to_string()),
            ("x-murmur-export-root".to_string(), export.declared_root),
        ],
        response.bytes,
    )
}

async fn refuse_list(
    plane: &ResourcePlane,
    root: &str,
    error: &ResourceError,
    generation: u64,
    class: ContainmentClass,
) -> ResourceResponse {
    if let Some(trace) = &plane.trace {
        trace
            .write_resource_list(
                root,
                0,
                0,
                generation,
                class,
                error.code(),
                Some(error.message()),
            )
            .await;
    }
    error_response(error, Some(generation))
}

async fn refuse_read(
    plane: &ResourcePlane,
    path: &str,
    error: &ResourceError,
    generation: u64,
    class: ContainmentClass,
) -> ResourceResponse {
    if let Some(trace) = &plane.trace {
        trace
            .write_resource_read(
                path,
                error.code(),
                None,
                None,
                generation,
                class,
                Some(error.message()),
            )
            .await;
    }
    error_response(error, Some(generation))
}

/// The requested path as the trace records it: percent-decoded, lossily where the bytes are not
/// UTF-8, and otherwise untouched.
fn decoded_request_path(raw: &str) -> String {
    String::from_utf8_lossy(&percent_decode(raw)).into_owned()
}

fn error_response(error: &ResourceError, generation: Option<u64>) -> ResourceResponse {
    let body = serde_json::json!({"error": error.code(), "message": error.message()});
    let mut headers = vec![("content-type".to_string(), "application/json".to_string())];
    if let Some(generation) = generation {
        headers.push(("x-murmur-generation".to_string(), generation.to_string()));
    }
    if matches!(error, ResourceError::MethodNotAllowed) {
        headers.push(("allow".to_string(), "GET".to_string()));
    }
    ResourceResponse::framed(
        error.status(),
        headers,
        serde_json::to_vec(&body).unwrap_or_default(),
    )
}

/// The reason phrase for a status the plane can return. Only these appear; anything else is a
/// programming error rather than a response shape callers depend on.
pub fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        _ => "Internal Server Error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn export_for(root: &Path, max_bytes: u64) -> DeclaredExport {
        DeclaredExport {
            declared_root: "out/".to_string(),
            root: root.to_path_buf(),
            mode: ExportMode::ReadOnly,
            max_bytes,
        }
    }

    // ── percent-decoding and path rejection ──────────────────────────────────

    #[test]
    fn percent_decoding_happens_before_validation() {
        // The whole reason decoding is first: validated as written, this looks like one opaque
        // component and reaches the filesystem as `../secret.txt`.
        assert_eq!(
            export_relpath_components("%2e%2e%2fsecret.txt"),
            Err(ResourceError::OutsideRoot)
        );
        assert_eq!(
            export_relpath_components("%2E%2E/secret.txt"),
            Err(ResourceError::OutsideRoot)
        );
    }

    #[test]
    fn percent_decoding_accepts_ordinary_escapes() {
        assert_eq!(
            export_relpath_components("sub%20dir/report.md").unwrap(),
            vec!["sub dir".to_string(), "report.md".to_string()]
        );
    }

    #[test]
    fn a_lone_percent_is_left_as_written() {
        assert_eq!(percent_decode("100%"), b"100%".to_vec());
        assert_eq!(percent_decode("%zz"), b"%zz".to_vec());
    }

    #[test]
    fn every_escaping_shape_is_refused() {
        for path in [
            "",
            "..",
            "../secret.txt",
            "out/../../secret.txt",
            "/etc/passwd",
            "/",
            "./report.md",
            "a//b",
            "a/./b",
            "a/../b",
            "report.md%00.txt",
            "%00",
        ] {
            assert_eq!(
                export_relpath_components(path),
                Err(ResourceError::OutsideRoot),
                "path {path:?} must be refused"
            );
        }
    }

    #[test]
    fn a_trailing_slash_names_the_same_file() {
        assert_eq!(
            export_relpath_components("dir/report.md/").unwrap(),
            vec!["dir".to_string(), "report.md".to_string()]
        );
    }

    #[test]
    fn ordinary_relative_paths_survive() {
        assert_eq!(
            export_relpath_components("a/b/c.txt").unwrap(),
            vec!["a".to_string(), "b".to_string(), "c.txt".to_string()]
        );
    }

    // ── the class-keyed symlink decision ─────────────────────────────────────

    #[test]
    fn symlink_policy_covers_every_containment_class() {
        assert_eq!(
            symlink_policy(ContainmentClass::Scoped),
            SymlinkPolicy::Refuse
        );
        assert_eq!(
            symlink_policy(ContainmentClass::Sealed),
            SymlinkPolicy::FollowWithinRoot
        );
        assert_eq!(
            symlink_policy(ContainmentClass::Advisory),
            SymlinkPolicy::FollowWithinRoot
        );
    }

    #[test]
    fn an_escaping_symlink_is_refused_at_every_class() {
        let dir = tempfile::tempdir().unwrap();
        let secret = dir.path().join("secret.txt");
        std::fs::write(&secret, b"top secret").unwrap();
        let root = dir.path().join("out");
        std::fs::create_dir(&root).unwrap();
        std::os::unix::fs::symlink(&secret, root.join("escape.txt")).unwrap();
        let export = export_for(&root, 1024);

        for class in [
            ContainmentClass::Advisory,
            ContainmentClass::Scoped,
            ContainmentClass::Sealed,
        ] {
            let error = read_export_file_with_policy(&export, "escape.txt", symlink_policy(class))
                .expect_err("an escaping symlink must never be served");
            let expected = match class {
                ContainmentClass::Scoped => ResourceError::SymlinkRefused,
                _ => ResourceError::OutsideRoot,
            };
            assert_eq!(error, expected, "class {class}");

            let entries = list_export_files(&export, symlink_policy(class)).unwrap();
            assert!(
                entries.is_empty(),
                "class {class}: an escaping symlink must not be listed; got {entries:?}"
            );
        }
    }

    #[test]
    fn an_in_root_symlink_is_refused_under_scoped_and_followed_otherwise() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("out");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("real.txt"), b"real bytes").unwrap();
        std::os::unix::fs::symlink("real.txt", root.join("alias.txt")).unwrap();
        let export = export_for(&root, 1024);

        assert_eq!(
            read_export_file_with_policy(&export, "alias.txt", SymlinkPolicy::Refuse),
            Err(ResourceError::SymlinkRefused)
        );
        assert_eq!(
            list_export_files(&export, SymlinkPolicy::Refuse)
                .unwrap()
                .iter()
                .map(|entry| entry.path.clone())
                .collect::<Vec<_>>(),
            vec!["real.txt".to_string()]
        );

        let followed =
            read_export_file_with_policy(&export, "alias.txt", SymlinkPolicy::FollowWithinRoot)
                .unwrap();
        assert_eq!(followed.bytes, b"real bytes");
        assert_eq!(
            list_export_files(&export, SymlinkPolicy::FollowWithinRoot)
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn a_symlinked_directory_on_the_path_is_refused_under_scoped() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("out");
        std::fs::create_dir_all(root.join("real")).unwrap();
        std::fs::write(root.join("real/report.md"), b"hello").unwrap();
        std::os::unix::fs::symlink("real", root.join("alias")).unwrap();
        let export = export_for(&root, 1024);

        assert_eq!(
            read_export_file_with_policy(&export, "alias/report.md", SymlinkPolicy::Refuse),
            Err(ResourceError::SymlinkRefused)
        );
    }

    // ── read semantics ───────────────────────────────────────────────────────

    #[test]
    fn a_read_hashes_the_bytes_it_serves() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("out");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("report.md"), b"# report\n").unwrap();
        let response = read_export_file_with_policy(
            &export_for(&root, 1024),
            "report.md",
            SymlinkPolicy::FollowWithinRoot,
        )
        .unwrap();
        assert_eq!(response.bytes, b"# report\n");
        assert_eq!(response.sha256, murmur_artifact::sha256_hex(b"# report\n"));
        assert!(response.mtime_ms > 0);
    }

    #[test]
    fn max_bytes_refuses_the_read_but_not_the_listing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("out");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("small.txt"), vec![b'a'; 100]).unwrap();
        std::fs::write(root.join("big.bin"), vec![b'b'; 4096]).unwrap();
        let export = export_for(&root, 1024);

        assert_eq!(
            read_export_file_with_policy(&export, "big.bin", SymlinkPolicy::FollowWithinRoot),
            Err(ResourceError::TooLarge { max_bytes: 1024 })
        );
        assert_eq!(
            read_export_file_with_policy(&export, "small.txt", SymlinkPolicy::FollowWithinRoot)
                .unwrap()
                .bytes
                .len(),
            100
        );

        let entries = list_export_files(&export, SymlinkPolicy::FollowWithinRoot).unwrap();
        let big = entries.iter().find(|e| e.path == "big.bin").unwrap();
        assert_eq!(
            big.size_bytes, 4096,
            "discovery must not hide an oversized file"
        );
    }

    #[test]
    fn a_directory_is_not_a_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("out");
        std::fs::create_dir_all(root.join("nested")).unwrap();
        assert_eq!(
            read_export_file_with_policy(
                &export_for(&root, 1024),
                "nested",
                SymlinkPolicy::FollowWithinRoot
            ),
            Err(ResourceError::NotARegularFile)
        );
    }

    #[test]
    fn a_missing_root_lists_empty_and_reads_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("out");
        let export = export_for(&root, 1024);
        assert_eq!(
            list_export_files(&export, SymlinkPolicy::FollowWithinRoot).unwrap(),
            Vec::new()
        );
        assert_eq!(
            read_export_file_with_policy(&export, "report.md", SymlinkPolicy::FollowWithinRoot),
            Err(ResourceError::NotFound)
        );
    }

    #[test]
    fn a_listing_is_recursive_and_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("out");
        std::fs::create_dir_all(root.join("b/c")).unwrap();
        std::fs::write(root.join("z.txt"), b"z").unwrap();
        std::fs::write(root.join("b/y.txt"), b"y").unwrap();
        std::fs::write(root.join("b/c/x.txt"), b"x").unwrap();
        let entries = list_export_files(&export_for(&root, 1024), SymlinkPolicy::Refuse).unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.path.clone())
                .collect::<Vec<_>>(),
            vec![
                "b/c/x.txt".to_string(),
                "b/y.txt".to_string(),
                "z.txt".to_string()
            ]
        );
        assert_eq!(entries[2].sha256, murmur_artifact::sha256_hex(b"z"));
    }

    #[test]
    fn a_fifo_is_refused_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("out");
        std::fs::create_dir(&root).unwrap();
        let fifo = root.join("pipe");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo should be runnable");
        if !status.success() {
            return;
        }
        assert_eq!(
            read_export_file_with_policy(
                &export_for(&root, 1024),
                "pipe",
                SymlinkPolicy::FollowWithinRoot
            ),
            Err(ResourceError::NotARegularFile)
        );
        assert!(
            list_export_files(&export_for(&root, 1024), SymlinkPolicy::Refuse)
                .unwrap()
                .is_empty()
        );
    }

    // ── error table ──────────────────────────────────────────────────────────

    #[test]
    fn every_error_maps_to_its_documented_status() {
        for (error, code, status) in [
            (ResourceError::NoResourcePlane, "no_resource_plane", 404),
            (ResourceError::NotFound, "not_found", 404),
            (ResourceError::OutsideRoot, "outside_root", 403),
            (ResourceError::SymlinkRefused, "symlink_refused", 403),
            (ResourceError::NotARegularFile, "not_a_regular_file", 403),
            (ResourceError::TooLarge { max_bytes: 1 }, "too_large", 413),
            (ResourceError::MethodNotAllowed, "method_not_allowed", 405),
            (ResourceError::IoError("x".into()), "io_error", 500),
        ] {
            assert_eq!(error.code(), code);
            assert_eq!(error.status(), status);
        }
    }

    // ── the launch-time root check ───────────────────────────────────────────

    fn file_export(root: &str) -> FileExport {
        FileExport {
            root: root.to_string(),
            mode: ExportMode::ReadOnly,
            max_bytes: 1024,
        }
    }

    /// The refusal is at launch rather than per request, so the first file never leaves: a root
    /// that already exists as a symlink out of the workdir stops the session before it starts.
    #[test]
    fn a_root_that_already_escapes_the_workdir_refuses_the_launch() {
        let workdir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), workdir.path().join("out")).unwrap();

        let error = check_export_root(workdir.path(), &file_export("out/")).unwrap_err();
        let RuntimeError::ExportRootOutsideWorkdir {
            declared, resolved, ..
        } = &error
        else {
            panic!("expected ExportRootOutsideWorkdir, got {error:?}");
        };
        // Both paths are named, because "outside the workdir" is not actionable without saying
        // where the root actually landed.
        assert_eq!(declared, "out/");
        assert_eq!(
            Path::new(resolved),
            std::fs::canonicalize(outside.path()).unwrap()
        );
    }

    /// A root the agent has not created yet is not an escape. `exports.files.root` is explicitly
    /// not required to exist at launch, and every request resolves it afresh.
    #[test]
    fn a_root_that_does_not_exist_yet_is_accepted() {
        let workdir = tempfile::tempdir().unwrap();
        assert!(check_export_root(workdir.path(), &file_export("out/")).is_ok());

        std::fs::create_dir(workdir.path().join("out")).unwrap();
        assert!(check_export_root(workdir.path(), &file_export("out/")).is_ok());
    }

    /// A workdir that does not exist yet — no `--workdir`, so the session directory is created
    /// after this check — has nothing under it that could already point elsewhere.
    #[test]
    fn a_workdir_that_does_not_exist_yet_is_accepted() {
        let parent = tempfile::tempdir().unwrap();
        let missing = parent.path().join("not-created-yet");
        assert!(check_export_root(&missing, &file_export("out/")).is_ok());
    }

    /// A symlink that stays inside the workdir is not an escape: the check asks where the root
    /// landed, not how it was spelled.
    #[test]
    fn a_root_symlinked_within_the_workdir_is_accepted() {
        let workdir = tempfile::tempdir().unwrap();
        std::fs::create_dir(workdir.path().join("real")).unwrap();
        std::os::unix::fs::symlink(workdir.path().join("real"), workdir.path().join("out"))
            .unwrap();

        assert!(check_export_root(workdir.path(), &file_export("out/")).is_ok());
    }

    // ── the handler ──────────────────────────────────────────────────────────

    fn plane_for(export: Option<DeclaredExport>, class: ContainmentClass) -> ResourcePlane {
        ResourcePlane::with_export(export, class, Arc::new(AtomicU64::new(0)), None)
    }

    #[tokio::test]
    async fn an_undeclared_plane_denies_both_verbs() {
        let plane = plane_for(None, ContainmentClass::Advisory);
        for path in ["/resources/files", "/resources/files/report.md"] {
            let response = handle_resource_request(&plane, "GET", path).await;
            assert_eq!(response.status, 404);
            let body: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
            assert_eq!(body["error"], "no_resource_plane");
        }
    }

    #[tokio::test]
    async fn every_write_verb_is_refused_with_an_allow_header() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("out");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("report.md"), b"hello").unwrap();
        let plane = plane_for(Some(export_for(&root, 1024)), ContainmentClass::Advisory);

        for method in ["POST", "PUT", "PATCH", "DELETE"] {
            let response =
                handle_resource_request(&plane, method, "/resources/files/report.md").await;
            assert_eq!(response.status, 405, "{method}");
            assert!(response
                .headers
                .iter()
                .any(|(name, value)| name == "allow" && value == "GET"));
        }
        assert_eq!(
            std::fs::read(root.join("report.md")).unwrap(),
            b"hello".to_vec()
        );
        assert!(!root.join("new.txt").exists());
    }

    #[tokio::test]
    async fn a_read_carries_its_validator_and_provenance() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("out");
        std::fs::create_dir(&root).unwrap();
        let mut file = std::fs::File::create(root.join("report.md")).unwrap();
        file.write_all(b"# report\n").unwrap();
        drop(file);

        let generation = Arc::new(AtomicU64::new(7));
        let plane = ResourcePlane::with_export(
            Some(export_for(&root, 1024)),
            ContainmentClass::Scoped,
            Arc::clone(&generation),
            None,
        );
        let response = handle_resource_request(&plane, "GET", "/resources/files/report.md").await;
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"# report\n");
        let header = |name: &str| {
            response
                .headers
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
                .unwrap_or_default()
        };
        assert_eq!(
            header("etag"),
            format!("\"sha256:{}\"", murmur_artifact::sha256_hex(b"# report\n"))
        );
        assert_eq!(header("x-murmur-generation"), "7");
        assert_eq!(header("x-murmur-containment"), "scoped");
        assert_eq!(header("x-murmur-export-root"), "out/");
    }

    #[tokio::test]
    async fn a_query_string_does_not_select_a_generation() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("out");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("report.md"), b"now").unwrap();
        let plane = ResourcePlane::with_export(
            Some(export_for(&root, 1024)),
            ContainmentClass::Advisory,
            Arc::new(AtomicU64::new(3)),
            None,
        );
        let response =
            handle_resource_request(&plane, "GET", "/resources/files/report.md?generation=1").await;
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"now");
    }

    #[tokio::test]
    async fn a_list_reports_the_declared_block_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("out");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("report.md"), b"hello").unwrap();
        let plane = plane_for(Some(export_for(&root, 4096)), ContainmentClass::Advisory);
        let response = handle_resource_request(&plane, "GET", "/resources/files").await;
        assert_eq!(response.status, 200);
        let body: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(body["root"], "out/");
        assert_eq!(body["mode"], "read-only");
        assert_eq!(body["max_bytes"], 4096);
        assert_eq!(body["generation"], 0);
        assert_eq!(body["containment_achieved"], "advisory");
        assert_eq!(body["entries"][0]["path"], "report.md");
        assert_eq!(body["entries"][0]["size_bytes"], 5);
    }

    #[tokio::test]
    async fn an_unknown_route_under_the_prefix_is_not_found() {
        let plane = plane_for(None, ContainmentClass::Advisory);
        let response = handle_resource_request(&plane, "GET", "/resources/secrets").await;
        assert_eq!(response.status, 404);
        let body: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(body["error"], "not_found");
    }

    /// There is no write path anywhere under the prefix, including at a path that names no route.
    #[tokio::test]
    async fn an_unknown_route_still_answers_the_method_first() {
        let plane = plane_for(None, ContainmentClass::Advisory);
        let response = handle_resource_request(&plane, "PUT", "/resources/secrets").await;
        assert_eq!(response.status, 405);
        assert!(response
            .headers
            .iter()
            .any(|(name, value)| name == "allow" && value == "GET"));
    }

    /// Every response frames itself, so a transport never has to know it owed a `content-length`
    /// and a caller can tell a complete body from a truncated one.
    #[tokio::test]
    async fn every_response_carries_its_own_content_length() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("out");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("report.md"), b"hello").unwrap();
        let plane = plane_for(Some(export_for(&root, 1024)), ContainmentClass::Advisory);

        for (method, path) in [
            ("GET", "/resources/files"),
            ("GET", "/resources/files/report.md"),
            ("GET", "/resources/files/../secret.txt"),
            ("PUT", "/resources/files/report.md"),
        ] {
            let response = handle_resource_request(&plane, method, path).await;
            let declared = response
                .headers
                .iter()
                .find(|(name, _)| name == "content-length")
                .map(|(_, value)| value.clone())
                .unwrap_or_else(|| panic!("{method} {path} must declare a content-length"));
            assert_eq!(
                declared,
                response.body.len().to_string(),
                "{method} {path}: content-length must match the body served"
            );
        }
    }
}
