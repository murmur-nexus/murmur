//! Images in the conversation: what a tool may return, how one is checked and stored, and how the
//! driver is shown it.
//!
//! A tool writes a PNG, JPEG, GIF or WebP file into the work folder and names it in its result's
//! `attachments`. The runtime reads the file beneath the work folder, checks that its leading bytes
//! match the declared type and that it is within `context.images.max_bytes`, and stores the bytes
//! once by their sha256 in a `blobs/` directory beside the conversation. The conversation holds a
//! small reference to the blob, the [record form](record_block), and never the bytes.
//!
//! Only the request handed to the driver carries bytes: [`render_for_driver`] turns the newest
//! `context.images.keep` references into base64 `image` blocks and every older one into a line
//! naming it. The runtime decodes no image: it hashes, reads files, compares magic bytes and
//! base64-encodes, and leaves resizing and placement to the driver.

use std::path::Path;

use base64::Engine as _;
use murmur_artifact::ContextImagesConfig;
use serde_json::{json, Value};

use crate::resource_plane::{read_file_beneath_root, ResourceError, SymlinkPolicy};
use crate::trace::AttachmentRecord;
use crate::trace_blobs::{BlobStore, BLOB_DIR_NAME};
use crate::workdir_fs::WorkdirRoot;

/// The media types an image may declare, the only ones the runtime accepts.
pub(crate) const ALLOWED_MEDIA_TYPES: [&str; 4] =
    ["image/png", "image/jpeg", "image/gif", "image/webp"];

/// Whether `media_type` is one of [`ALLOWED_MEDIA_TYPES`].
pub(crate) fn is_allowed_media_type(media_type: &str) -> bool {
    ALLOWED_MEDIA_TYPES.contains(&media_type)
}

/// The image type `bytes` begin with, read from their magic bytes alone, or `None` when they begin
/// like none of the four allowed types.
pub(crate) fn sniff_media_type(bytes: &[u8]) -> Option<&'static str> {
    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    if bytes.starts_with(PNG) {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// Why an image was not attached. [`Self::code`] is the word the refusal note ends with and the
/// trace's `refused` value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ImageRefusal {
    /// The declared media type is not one of [`ALLOWED_MEDIA_TYPES`].
    MediaTypeNotAllowed { media_type: String },
    /// The bytes do not begin like the declared type.
    ContentMismatch {
        declared: String,
        found: Option<&'static str>,
    },
    /// The image is larger than `context.images.max_bytes`.
    TooLarge { bytes: Option<u64>, max_bytes: u64 },
    /// This model turn's tool calls already returned `context.images.max_per_turn` images.
    TurnLimit { max_per_turn: u32 },
    /// The entry names no path or no media type, or a native tool's entry is not an object of
    /// two strings.
    Malformed(String),
    /// The image was accepted but its blob could not be written.
    StoreFailed(String),
    /// The work-folder read refused the path: `not_found`, `outside_root`, `symlink_refused`,
    /// `not_a_regular_file` or `io_error`.
    Read(ResourceError),
}

impl ImageRefusal {
    /// The stable code: `media_type_not_allowed`, `content_mismatch`, `too_large`, `turn_limit`,
    /// `malformed`, `store_failed`, or the resource plane's own code for a refused read.
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::MediaTypeNotAllowed { .. } => "media_type_not_allowed",
            Self::ContentMismatch { .. } => "content_mismatch",
            Self::TooLarge { .. } => "too_large",
            Self::TurnLimit { .. } => "turn_limit",
            Self::Malformed(_) => "malformed",
            Self::StoreFailed(_) => "store_failed",
            Self::Read(error) => error.code(),
        }
    }

    /// The reason in words, as the refusal note states it before the code.
    pub(crate) fn reason(&self) -> String {
        match self {
            Self::MediaTypeNotAllowed { media_type } => format!(
                "media type '{media_type}' is not one of {}",
                ALLOWED_MEDIA_TYPES.join(", ")
            ),
            Self::ContentMismatch { declared, found } => match found {
                Some(found) => format!("declared {declared} but the bytes are {found}"),
                None => format!(
                    "declared {declared} but the bytes are not an image of any allowed type"
                ),
            },
            Self::TooLarge {
                bytes: Some(bytes),
                max_bytes,
            } => format!("{bytes} bytes is over context.images.max_bytes of {max_bytes} bytes"),
            Self::TooLarge {
                bytes: None,
                max_bytes,
            } => format!("larger than context.images.max_bytes of {max_bytes} bytes"),
            Self::TurnLimit { max_per_turn } => format!(
                "this turn's tool calls already returned context.images.max_per_turn \
                 ({max_per_turn}) images"
            ),
            Self::Malformed(reason) | Self::StoreFailed(reason) => reason.clone(),
            Self::Read(error) => match error {
                ResourceError::NotFound => "no such file in the work folder".to_string(),
                ResourceError::OutsideRoot => "the path is outside the work folder".to_string(),
                ResourceError::SymlinkRefused => "the path is a link".to_string(),
                ResourceError::NotARegularFile => "the path is not a regular file".to_string(),
                ResourceError::IoError(reason) => format!("it could not be read: {reason}"),
                other => other.code().replace('_', " "),
            },
        }
    }
}

/// The note a tool result gains, after its fence's closing marker, for one refused attachment:
/// `attachment '<path>' was not attached: <reason> (<code>)`.
pub(crate) fn refusal_note(path: &str, refusal: &ImageRefusal) -> String {
    format!(
        "attachment '{path}' was not attached: {} ({})",
        refusal.reason(),
        refusal.code()
    )
}

/// An image that passed every check: its declared type matched its bytes and it was within
/// `context.images.max_bytes`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CheckedImage {
    pub(crate) media_type: String,
    pub(crate) bytes: Vec<u8>,
    /// Lowercase-hex sha256 of [`Self::bytes`], the blob's name.
    pub(crate) sha256: String,
}

impl CheckedImage {
    pub(crate) fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    /// The image block the conversation holds for this image once its blob is stored:
    /// `{"type":"image","source":{"type":"file","sha256":…,"media_type":…,"bytes":…}}`.
    pub(crate) fn record_block(&self) -> Value {
        json!({
            "type": "image",
            "source": {
                "type": "file",
                "sha256": self.sha256,
                "media_type": self.media_type,
                "bytes": self.len(),
            }
        })
    }

    /// The trace's entry for this image, under `path`.
    pub(crate) fn trace_record(&self, path: &str) -> AttachmentRecord {
        AttachmentRecord {
            path: path.to_string(),
            media_type: self.media_type.clone(),
            bytes: Some(self.len()),
            sha256: Some(self.sha256.clone()),
            refused: None,
        }
    }
}

/// The trace's entry for an image refused with `refusal`.
pub(crate) fn refused_trace_record(
    path: &str,
    media_type: &str,
    refusal: &ImageRefusal,
) -> AttachmentRecord {
    AttachmentRecord {
        path: path.to_string(),
        media_type: media_type.to_string(),
        bytes: None,
        sha256: None,
        refused: Some(refusal.code()),
    }
}

/// Check `bytes` declared as `media_type` against the allowed types, `max_bytes` and their own
/// magic bytes, in that order.
pub(crate) fn check_image(
    media_type: &str,
    bytes: Vec<u8>,
    max_bytes: u64,
) -> Result<CheckedImage, ImageRefusal> {
    if !is_allowed_media_type(media_type) {
        return Err(ImageRefusal::MediaTypeNotAllowed {
            media_type: media_type.to_string(),
        });
    }
    if bytes.len() as u64 > max_bytes {
        return Err(ImageRefusal::TooLarge {
            bytes: Some(bytes.len() as u64),
            max_bytes,
        });
    }
    let found = sniff_media_type(&bytes);
    if found != Some(media_type) {
        return Err(ImageRefusal::ContentMismatch {
            declared: media_type.to_string(),
            found,
        });
    }
    let sha256 = murmur_artifact::sha256_hex(&bytes);
    Ok(CheckedImage {
        media_type: media_type.to_string(),
        bytes,
        sha256,
    })
}

/// Read the file a tool named at `path`, beneath `workdir`, and check it as `media_type`.
///
/// The type is checked before anything is read. The read refuses a link and a path outside the
/// work folder, and reads at most `max_bytes`.
pub(crate) fn read_attachment(
    workdir: &WorkdirRoot,
    path: &str,
    media_type: &str,
    max_bytes: u64,
) -> Result<CheckedImage, ImageRefusal> {
    if path.trim().is_empty() || media_type.trim().is_empty() {
        return Err(ImageRefusal::Malformed(
            "an attachment needs a non-empty media_type and path".to_string(),
        ));
    }
    if !is_allowed_media_type(media_type) {
        return Err(ImageRefusal::MediaTypeNotAllowed {
            media_type: media_type.to_string(),
        });
    }
    let read = read_file_beneath_root(workdir.path(), path, max_bytes, SymlinkPolicy::Refuse)
        .map_err(|error| match error {
            // The read stopped at the limit on a regular file, so its length names the real
            // size; a file that changed in between names none.
            ResourceError::TooLarge { max_bytes } => ImageRefusal::TooLarge {
                bytes: workdir.file_len(path).ok().flatten(),
                max_bytes,
            },
            other => ImageRefusal::Read(other),
        })?;
    check_image(media_type, read.bytes, max_bytes)
}

/// What one tool call's attachments came to.
#[derive(Debug, Default)]
pub(crate) struct AcceptedAttachments {
    /// The record form of each accepted image, in the order the call named them.
    pub(crate) blocks: Vec<Value>,
    /// Every accepted image, beside its block.
    pub(crate) images: Vec<CheckedImage>,
    /// One [`refusal_note`] per refused attachment.
    pub(crate) notes: Vec<String>,
    /// The trace's `attachments`, accepted and refused, in the order the call named them.
    pub(crate) trace: Vec<AttachmentRecord>,
}

/// Accept a tool call's `attachments`, each a `(media_type, path)` pair, and store every image
/// that passes into `store`.
///
/// `accepted_this_turn` counts the images this model turn's tool calls have already had accepted;
/// an attachment past `limits.max_per_turn` is refused with `turn_limit` without being read.
pub(crate) async fn accept_attachments<'a>(
    attachments: impl IntoIterator<Item = (&'a str, &'a str)>,
    workdir: &WorkdirRoot,
    limits: &ContextImagesConfig,
    accepted_this_turn: &mut u32,
    store: &ImageStore,
) -> AcceptedAttachments {
    let mut outcome = AcceptedAttachments::default();
    for (media_type, path) in attachments {
        let checked = if *accepted_this_turn >= limits.max_per_turn {
            Err(ImageRefusal::TurnLimit {
                max_per_turn: limits.max_per_turn,
            })
        } else {
            read_attachment(workdir, path, media_type, limits.max_bytes)
        };
        let stored = match checked {
            Ok(image) => store.put(&image).await.map(|()| image),
            Err(refusal) => Err(refusal),
        };
        match stored {
            Ok(image) => {
                *accepted_this_turn += 1;
                outcome.blocks.push(image.record_block());
                outcome.trace.push(image.trace_record(path));
                outcome.images.push(image);
            }
            Err(refusal) => {
                outcome.notes.push(refusal_note(path, &refusal));
                outcome
                    .trace
                    .push(refused_trace_record(path, media_type, &refusal));
            }
        }
    }
    outcome
}

/// Where a conversation's image blobs live: `blobs/` in its context directory, or in the session's
/// records directory for a conversation that keeps no record.
pub(crate) struct ImageStore {
    root: WorkdirRoot,
}

impl ImageStore {
    /// The store for a conversation whose context directory is `context_dir`, or, when it keeps no
    /// record, for the session whose records directory is `records_root`. Opens the context
    /// directory, creating it private when it does not exist yet.
    pub(crate) fn open(
        context_dir: Option<&Path>,
        records_root: &WorkdirRoot,
    ) -> Result<Self, String> {
        let root = match context_dir {
            Some(dir) => {
                crate::conversation::ensure_context_dirs(dir)?;
                WorkdirRoot::open(dir).map_err(|error| error.to_string())?
            }
            None => records_root.clone(),
        };
        Ok(Self { root })
    }

    /// A store at `root`'s `blobs/`.
    #[cfg(test)]
    pub(crate) fn at(root: &WorkdirRoot) -> Self {
        Self { root: root.clone() }
    }

    /// The host path of the blob named `sha256`, whether or not it exists.
    pub(crate) fn blob_path(&self, sha256: &str) -> std::path::PathBuf {
        self.root.host_path(format!("{BLOB_DIR_NAME}/{sha256}"))
    }

    /// Store `image` as `blobs/<sha256>`, once.
    pub(crate) async fn put(&self, image: &CheckedImage) -> Result<(), ImageRefusal> {
        BlobStore::new(&self.root)
            .put(&image.sha256, &image.bytes)
            .await
            .map_err(|error| {
                ImageRefusal::StoreFailed(format!("its blob could not be stored: {error}"))
            })
    }

    /// The bytes of the blob named `sha256` when it holds exactly `len` bytes, or `None` when it is
    /// missing, a link, unreadable or another length.
    pub(crate) fn read(&self, sha256: &str, len: u64) -> Option<Vec<u8>> {
        if !is_sha256_hex(sha256) {
            return None;
        }
        let bytes = self
            .root
            .read_file(format!("{BLOB_DIR_NAME}/{sha256}"), len)
            .ok()
            .flatten()?;
        (bytes.len() as u64 == len).then_some(bytes)
    }
}

/// The bytes of the blob at `blob`, refusing a link at its name, or `None` when it cannot be read.
pub(crate) fn read_blob(blob: &Path) -> Option<Vec<u8>> {
    let dir = WorkdirRoot::open(blob.parent()?).ok()?;
    dir.read_file(blob.file_name()?, u64::MAX).ok().flatten()
}

/// Whether `text` is 64 lowercase hex digits: the only blob name a record's image block may carry,
/// so a hand-edited or hook-supplied block cannot name a path.
pub(crate) fn is_sha256_hex(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The `source` of `block` when it is an image block in record form, with the three facts it
/// names: sha256, media type and length.
pub(crate) fn file_image(block: &Value) -> Option<(&str, &str, u64)> {
    if block.get("type").and_then(Value::as_str) != Some("image") {
        return None;
    }
    let source = block.get("source")?;
    if source.get("type").and_then(Value::as_str) != Some("file") {
        return None;
    }
    Some((
        source.get("sha256").and_then(Value::as_str)?,
        source
            .get("media_type")
            .and_then(Value::as_str)
            .unwrap_or(""),
        source.get("bytes").and_then(Value::as_u64)?,
    ))
}

/// The content blocks of `message`, or none when its content is a string or absent.
fn blocks(message: &Value) -> &[Value] {
    message
        .get("content")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// Whether any message holds an image block in record form. The scan for a list that entered the
/// context from outside the loop: a threaded reload, or a hook's `replace-context` or
/// `seed-context`.
pub(crate) fn has_file_image(messages: &[Value]) -> bool {
    messages.iter().any(|message| {
        blocks(message)
            .iter()
            .any(|block| file_image(block).is_some())
    })
}

/// The sha256 of every image block in record form `message` holds.
pub(crate) fn referenced_blobs(message: &Value) -> impl Iterator<Item = &str> {
    blocks(message)
        .iter()
        .filter_map(|block| file_image(block).map(|(sha256, _, _)| sha256))
}

/// The line an image older than the newest `keep` reaches the driver as.
pub(crate) fn aged_image_line(media_type: &str, bytes: u64, sha256: &str, keep: u32) -> String {
    format!(
        "[image not shown: {media_type}, {bytes} bytes, sha256 {sha256}; the {keep} newer images \
         in this conversation are shown instead]"
    )
}

/// The line an image whose blob is missing, or holds another length, reaches the driver as.
pub(crate) fn unavailable_image_line(media_type: &str, bytes: u64, sha256: &str) -> String {
    format!("[image unavailable: {media_type}, {bytes} bytes, sha256 {sha256}]")
}

/// `messages` as the driver receives them: the newest `keep` image blocks in record form, over the
/// whole list, become base64 `image` blocks read from `store`, and every older one becomes an
/// [`aged_image_line`]. One whose blob is missing or another length becomes an
/// [`unavailable_image_line`]. Every other block, including an image block of any other
/// `source.type`, passes through untouched, and the list keeps its length and order.
pub(crate) fn render_for_driver(messages: &[Value], keep: u32, store: &ImageStore) -> Vec<Value> {
    render_with(messages, keep, |sha256, len| store.read(sha256, len))
}

/// [`render_for_driver`] with no store to read from: every image within `keep` becomes an
/// [`unavailable_image_line`].
pub(crate) fn render_unavailable(messages: &[Value], keep: u32) -> Vec<Value> {
    render_with(messages, keep, |_, _| None)
}

fn render_with(
    messages: &[Value],
    keep: u32,
    read: impl Fn(&str, u64) -> Option<Vec<u8>>,
) -> Vec<Value> {
    let total = messages
        .iter()
        .map(|message| {
            blocks(message)
                .iter()
                .filter(|b| file_image(b).is_some())
                .count()
        })
        .sum::<usize>();
    let shown_from = total.saturating_sub(keep as usize);
    let mut seen = 0usize;
    messages
        .iter()
        .map(|message| {
            if !blocks(message).iter().any(|block| file_image(block).is_some()) {
                return message.clone();
            }
            let mut rendered = message.clone();
            if let Some(content) = rendered.get_mut("content").and_then(Value::as_array_mut) {
                for block in content.iter_mut() {
                    let Some((sha256, media_type, bytes)) = file_image(block) else {
                        continue;
                    };
                    let index = seen;
                    seen += 1;
                    let replacement = if index < shown_from {
                        json!({"type": "text", "text": aged_image_line(media_type, bytes, sha256, keep)})
                    } else {
                        match read(sha256, bytes) {
                            Some(data) => json!({
                                "type": "image",
                                "source": {
                                    "type": "base64",
                                    "media_type": media_type,
                                    "data": base64::engine::general_purpose::STANDARD.encode(data),
                                }
                            }),
                            None => json!({
                                "type": "text",
                                "text": unavailable_image_line(media_type, bytes, sha256),
                            }),
                        }
                    };
                    *block = replacement;
                }
            }
            rendered
        })
        .collect()
}

/// Decode the base64 `data` of an A2A `raw` part declared as `media_type` and check it as an image.
pub(crate) fn decode_raw_part(
    media_type: &str,
    data: &str,
    max_bytes: u64,
) -> Result<CheckedImage, ImageRefusal> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data.trim())
        .map_err(|error| {
            ImageRefusal::Malformed(format!("its raw bytes are not base64: {error}"))
        })?;
    check_image(media_type, bytes, max_bytes)
}

/// The line the process transport's bridge appends to a tool result for one attachment it checked
/// and accepted. Nothing is stored on that transport and the harness's model is not shown the image.
pub(crate) fn bridged_attachment_line(path: &str, image: &CheckedImage) -> String {
    format!(
        "[image attached: {path}, {}, {} bytes; not shown to this model]",
        image.media_type,
        image.len()
    )
}

/// One line per attachment for a tool result the process transport's bridge returns: an
/// [`bridged_attachment_line`] for each that passes the type and size checks, a [`refusal_note`]
/// for each that does not. Nothing is stored, and no per-turn limit applies.
pub(crate) fn bridged_attachment_lines<'a>(
    attachments: impl IntoIterator<Item = (&'a str, &'a str)>,
    workdir: &WorkdirRoot,
    max_bytes: u64,
) -> Vec<String> {
    attachments
        .into_iter()
        .map(
            |(media_type, path)| match read_attachment(workdir, path, media_type, max_bytes) {
                Ok(image) => bridged_attachment_line(path, &image),
                Err(refusal) => refusal_note(path, &refusal),
            },
        )
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) const PNG: &[u8] = &[
        0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0x0D, b'I', b'H', b'D', b'R',
    ];
    const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0, 0x10, b'J', b'F', b'I', b'F'];
    const GIF87: &[u8] = b"GIF87a\x01\x00\x01\x00";
    const GIF89: &[u8] = b"GIF89a\x01\x00\x01\x00";
    const WEBP: &[u8] = b"RIFF\x24\x00\x00\x00WEBPVP8 ";

    fn limits() -> ContextImagesConfig {
        ContextImagesConfig::default()
    }

    #[test]
    fn each_type_is_accepted_by_its_own_magic_bytes() {
        for (media_type, bytes) in [
            ("image/png", PNG),
            ("image/jpeg", JPEG),
            ("image/gif", GIF87),
            ("image/gif", GIF89),
            ("image/webp", WEBP),
        ] {
            assert_eq!(sniff_media_type(bytes), Some(media_type));
            let image = check_image(media_type, bytes.to_vec(), 1024).unwrap();
            assert_eq!(image.sha256, murmur_artifact::sha256_hex(bytes));
            assert_eq!(image.len(), bytes.len() as u64);
        }
    }

    #[test]
    fn each_type_declared_over_another_types_bytes_is_a_content_mismatch() {
        let all = [
            ("image/png", PNG),
            ("image/jpeg", JPEG),
            ("image/gif", GIF89),
            ("image/webp", WEBP),
        ];
        for (declared, _) in all {
            for (actual, bytes) in all {
                if declared == actual {
                    continue;
                }
                let refusal = check_image(declared, bytes.to_vec(), 1024).unwrap_err();
                assert_eq!(
                    refusal.code(),
                    "content_mismatch",
                    "{declared} over {actual}"
                );
                assert!(refusal.reason().contains(actual), "{}", refusal.reason());
            }
        }
        let text = check_image("image/png", b"not a picture".to_vec(), 1024).unwrap_err();
        assert_eq!(text.code(), "content_mismatch");
        // A RIFF container that is not WebP, and a truncated header, are no image.
        assert_eq!(sniff_media_type(b"RIFF\x24\x00\x00\x00WAVEfmt "), None);
        assert_eq!(sniff_media_type(b"RIFF"), None);
        assert_eq!(sniff_media_type(&PNG[..7]), None);
    }

    #[test]
    fn a_media_type_outside_the_four_is_refused_before_the_bytes_are_read() {
        let dir = tempfile::tempdir().unwrap();
        let workdir = WorkdirRoot::open(dir.path()).unwrap();
        let refusal = read_attachment(&workdir, "absent.svg", "image/svg+xml", 1024).unwrap_err();
        assert_eq!(refusal.code(), "media_type_not_allowed");
        assert_eq!(
            refusal_note("absent.svg", &refusal),
            "attachment 'absent.svg' was not attached: media type 'image/svg+xml' is not one of \
             image/png, image/jpeg, image/gif, image/webp (media_type_not_allowed)"
        );
    }

    #[test]
    fn a_file_over_max_bytes_is_too_large_and_names_both_sizes() {
        let dir = tempfile::tempdir().unwrap();
        let mut big = PNG.to_vec();
        big.resize(2048, 0);
        std::fs::write(dir.path().join("big.png"), &big).unwrap();
        let workdir = WorkdirRoot::open(dir.path()).unwrap();
        let refusal = read_attachment(&workdir, "big.png", "image/png", 1024).unwrap_err();
        assert_eq!(refusal.code(), "too_large");
        let note = refusal_note("big.png", &refusal);
        assert!(
            note.contains("2048 bytes") && note.contains("1024 bytes"),
            "{note}"
        );
        assert!(note.ends_with("(too_large)"), "{note}");
    }

    #[test]
    fn a_path_outside_the_work_folder_a_link_and_a_missing_file_take_the_read_codes() {
        let outer = tempfile::tempdir().unwrap();
        let dir = outer.path().join("work");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(outer.path().join("escape.png"), PNG).unwrap();
        std::os::unix::fs::symlink(outer.path().join("escape.png"), dir.join("link.png")).unwrap();
        let workdir = WorkdirRoot::open(&dir).unwrap();
        for (path, code) in [
            ("../escape.png", "outside_root"),
            ("link.png", "symlink_refused"),
            ("missing.png", "not_found"),
        ] {
            let refusal = read_attachment(&workdir, path, "image/png", 1024).unwrap_err();
            assert_eq!(refusal.code(), code, "{path}");
        }
        assert!(
            workdir.take_refusals().is_empty(),
            "a refused attachment never latches a work-folder refusal"
        );
    }

    #[test]
    fn an_entry_without_a_path_or_type_is_malformed() {
        let dir = tempfile::tempdir().unwrap();
        let workdir = WorkdirRoot::open(dir.path()).unwrap();
        for (path, media_type) in [("", "image/png"), ("a.png", ""), ("  ", " ")] {
            let refusal = read_attachment(&workdir, path, media_type, 1024).unwrap_err();
            assert_eq!(refusal.code(), "malformed");
        }
    }

    #[tokio::test]
    async fn the_sixth_image_in_one_turn_is_refused_with_turn_limit() {
        let dir = tempfile::tempdir().unwrap();
        let records = tempfile::tempdir().unwrap();
        let mut names = Vec::new();
        for index in 0..7u8 {
            let mut png = PNG.to_vec();
            png.push(index);
            let name = format!("{index}.png");
            std::fs::write(dir.path().join(&name), &png).unwrap();
            names.push(name);
        }
        let workdir = WorkdirRoot::open(dir.path()).unwrap();
        let store = ImageStore::at(&WorkdirRoot::open(records.path()).unwrap());
        let mut count = 0;
        // Two calls in one turn: three images, then four.
        let first = accept_attachments(
            names[..3].iter().map(|name| ("image/png", name.as_str())),
            &workdir,
            &limits(),
            &mut count,
            &store,
        )
        .await;
        assert_eq!(first.blocks.len(), 3);
        let second = accept_attachments(
            names[3..].iter().map(|name| ("image/png", name.as_str())),
            &workdir,
            &limits(),
            &mut count,
            &store,
        )
        .await;
        assert_eq!(second.blocks.len(), 2);
        assert_eq!(second.notes.len(), 2);
        assert!(second.notes[0].starts_with("attachment '5.png' was not attached:"));
        assert!(
            second.notes[0].ends_with("(turn_limit)"),
            "{}",
            second.notes[0]
        );
        assert_eq!(second.trace[2].refused, Some("turn_limit"));
        assert_eq!(count, 5);
        let stored = std::fs::read_dir(records.path().join("blobs"))
            .unwrap()
            .count();
        assert_eq!(stored, 5);
    }

    #[tokio::test]
    async fn an_accepted_image_is_stored_once_and_recorded_by_reference() {
        let dir = tempfile::tempdir().unwrap();
        let records = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("shot.png"), PNG).unwrap();
        let workdir = WorkdirRoot::open(dir.path()).unwrap();
        let store = ImageStore::at(&WorkdirRoot::open(records.path()).unwrap());
        let mut count = 0;
        let outcome = accept_attachments(
            [("image/png", "shot.png"), ("image/png", "shot.png")],
            &workdir,
            &limits(),
            &mut count,
            &store,
        )
        .await;
        let sha = murmur_artifact::sha256_hex(PNG);
        assert_eq!(
            outcome.blocks[0],
            json!({"type":"image","source":{"type":"file","sha256":sha,"media_type":"image/png","bytes":PNG.len()}})
        );
        assert_eq!(std::fs::read(store.blob_path(&sha)).unwrap(), PNG);
        assert_eq!(
            std::fs::read_dir(records.path().join("blobs"))
                .unwrap()
                .count(),
            1
        );
        let trace = serde_json::to_value(&outcome.trace[0]).unwrap();
        assert_eq!(
            trace,
            json!({"path":"shot.png","media_type":"image/png","bytes":PNG.len(),"sha256":sha})
        );
    }

    fn tool_message_with(images: &[&CheckedImage]) -> Value {
        let mut content = vec![json!({"type":"text","text":"result"})];
        content.extend(images.iter().map(|image| image.record_block()));
        json!({"role":"tool","tool_call_id":"c","content":content})
    }

    #[tokio::test]
    async fn rendering_shows_the_newest_keep_images_and_names_the_older_ones() {
        let records = tempfile::tempdir().unwrap();
        let store = ImageStore::at(&WorkdirRoot::open(records.path()).unwrap());
        let images: Vec<CheckedImage> = (0..3u8)
            .map(|index| {
                let mut png = PNG.to_vec();
                png.push(index);
                check_image("image/png", png, 1024).unwrap()
            })
            .collect();
        for image in &images {
            store.put(image).await.unwrap();
        }
        let messages = vec![
            json!({"role":"user","content":[{"type":"text","text":"task"}]}),
            tool_message_with(&[&images[0]]),
            tool_message_with(&[&images[1], &images[2]]),
        ];
        let rendered = render_for_driver(&messages, 2, &store);
        assert_eq!(rendered.len(), 3);
        assert_eq!(rendered[0], messages[0]);
        assert_eq!(
            rendered[1]["content"][1],
            json!({"type":"text","text":aged_image_line("image/png", 17, &images[0].sha256, 2)})
        );
        assert!(rendered[1]["content"][1]["text"]
            .as_str()
            .unwrap()
            .starts_with("[image not shown: image/png, 17 bytes, sha256 "));
        for (block, image) in [(1, &images[1]), (2, &images[2])] {
            let source = &rendered[2]["content"][block]["source"];
            assert_eq!(source["type"], "base64");
            assert_eq!(source["media_type"], "image/png");
            let data = base64::engine::general_purpose::STANDARD
                .decode(source["data"].as_str().unwrap())
                .unwrap();
            assert_eq!(murmur_artifact::sha256_hex(&data), image.sha256);
        }
        // The record form itself is never rewritten.
        assert_eq!(messages[1]["content"][1]["source"]["type"], "file");
    }

    #[tokio::test]
    async fn a_missing_or_resized_blob_and_a_bad_name_render_as_unavailable() {
        let records = tempfile::tempdir().unwrap();
        let store = ImageStore::at(&WorkdirRoot::open(records.path()).unwrap());
        let image = check_image("image/png", PNG.to_vec(), 1024).unwrap();
        let mut wrong_len = image.record_block();
        wrong_len["source"]["bytes"] = json!(PNG.len() + 1);
        let mut bad_name = image.record_block();
        bad_name["source"]["sha256"] = json!("../../etc/passwd");
        let hook_supplied =
            json!({"type":"image","source":{"type":"url","url":"https://example.test/a.png"}});
        let messages = vec![json!({"role":"tool","content":[
            image.record_block(), wrong_len, bad_name, hook_supplied.clone()
        ]})];
        store.put(&image).await.unwrap();
        std::fs::remove_file(store.blob_path(&image.sha256)).unwrap();
        let rendered = render_for_driver(&messages, 5, &store);
        let content = rendered[0]["content"].as_array().unwrap();
        assert_eq!(
            content[0]["text"],
            unavailable_image_line("image/png", PNG.len() as u64, &image.sha256)
        );
        assert!(content[1]["text"]
            .as_str()
            .unwrap()
            .starts_with("[image unavailable: "));
        assert!(content[2]["text"]
            .as_str()
            .unwrap()
            .starts_with("[image unavailable: "));
        assert_eq!(content[3], hook_supplied);
    }

    #[test]
    fn has_file_image_sees_only_record_form_blocks() {
        let image = check_image("image/png", PNG.to_vec(), 1024).unwrap();
        assert!(!has_file_image(&[json!({"role":"user","content":"text"})]));
        assert!(!has_file_image(&[json!({"role":"user","content":[
            {"type":"image","source":{"type":"base64","media_type":"image/png","data":""}}
        ]})]));
        assert!(has_file_image(&[tool_message_with(&[&image])]));
        assert_eq!(
            referenced_blobs(&tool_message_with(&[&image])).collect::<Vec<_>>(),
            [image.sha256.as_str()]
        );
    }

    #[test]
    fn a_raw_part_decodes_from_base64_and_is_checked() {
        let data = base64::engine::general_purpose::STANDARD.encode(PNG);
        assert_eq!(
            decode_raw_part("image/png", &data, 1024).unwrap().sha256,
            murmur_artifact::sha256_hex(PNG)
        );
        assert_eq!(
            decode_raw_part("image/png", "%%%", 1024)
                .unwrap_err()
                .code(),
            "malformed"
        );
        let text = base64::engine::general_purpose::STANDARD.encode(b"hello");
        assert_eq!(
            decode_raw_part("image/png", &text, 1024)
                .unwrap_err()
                .code(),
            "content_mismatch"
        );
        assert_eq!(
            decode_raw_part("image/png", &data, 4).unwrap_err().code(),
            "too_large"
        );
    }
}
