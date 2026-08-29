//! How much of each turn's wire payload a session's trace keeps, and the resolution of the
//! manifest keys that decide it.
//!
//! Two manifest keys reach [`resolve_trace_capture`]: `trace.capture`, which names a
//! [`TraceCapture`] directly, and the retired `trace.include_tool_output` boolean. The boolean is
//! accepted as an alias — `true` maps to [`TraceCapture::Content`] and `false` to
//! [`TraceCapture::Meta`] — and its use prints one `warning:` line to stderr naming
//! `trace.capture` as the replacement. Setting both
//! keys is rejected, including when the two agree, rather than resolved by precedence.

use crate::runtime_manifest::RuntimeManifestError;

/// The `trace.capture` values a manifest may name, in the order this crate reports them.
pub const TRACE_CAPTURE_ACCEPTED_VALUES: &[&str] = &["none", "meta", "content"];

/// How much of each turn's driver request a session's trace keeps.
///
/// Every mode records what the runtime *sent*, not what the model *saw*: provider-side prompt
/// injection, tokenizer differences and safety layers happen past the wire and are invisible here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TraceCapture {
    /// No content hashes on `inference`, no `blobs/` directory, no `tool_call.output`. Every
    /// other field on an `inference` event is written exactly as the other modes write it.
    None,
    /// Content hashes on every `inference` event, and no bodies on disk. The default.
    #[default]
    Meta,
    /// Hashes plus the bodies behind them, written once each to `<session>/blobs/<sha256>`, and
    /// `tool_call.output` alongside them.
    Content,
}

impl TraceCapture {
    /// The YAML spelling of this mode — the string `trace.capture` accepts for it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Meta => "meta",
            Self::Content => "content",
        }
    }

    /// Whether this mode writes bodies: blob files under `<session>/blobs/` and the
    /// `tool_call.output` text.
    #[must_use]
    pub fn captures_content(self) -> bool {
        matches!(self, Self::Content)
    }

    /// Whether this mode hashes the wire payload at all. `false` only for [`Self::None`].
    #[must_use]
    pub fn captures_hashes(self) -> bool {
        !matches!(self, Self::None)
    }
}

impl std::str::FromStr for TraceCapture {
    type Err = ParseTraceCaptureError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim() {
            "none" => Ok(Self::None),
            "meta" => Ok(Self::Meta),
            "content" => Ok(Self::Content),
            other => Err(ParseTraceCaptureError {
                value: other.to_string(),
            }),
        }
    }
}

/// A string that is not one of the three [`TraceCapture`] names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseTraceCaptureError {
    pub value: String,
}

impl std::fmt::Display for ParseTraceCaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "must be one of: {}; got '{}'",
            TRACE_CAPTURE_ACCEPTED_VALUES.join(", "),
            self.value
        )
    }
}

impl std::error::Error for ParseTraceCaptureError {}

/// Resolve a manifest `trace:` block's two capture keys into one [`TraceCapture`].
///
/// Accepts `trace.capture` naming a mode, or the retired `trace.include_tool_output` boolean —
/// `true` resolving to [`TraceCapture::Content`], `false` to [`TraceCapture::Meta`] — and prints
/// one deprecation `warning:` to stderr whenever the boolean is what supplied the answer. A
/// `trace:` block that sets neither key resolves to [`TraceCapture::default`].
///
/// Both keys set is a [`RuntimeManifestError::InvalidTraceConfig`], even when they agree: a
/// manifest that names the same setting twice has an author who believes one of them does
/// something the other does not, and picking a winner hides that.
pub fn resolve_trace_capture(
    capture: Option<&str>,
    include_tool_output: Option<bool>,
) -> Result<TraceCapture, RuntimeManifestError> {
    match (capture, include_tool_output) {
        (Some(_), Some(_)) => Err(RuntimeManifestError::InvalidTraceConfig {
            field: "trace.include_tool_output".to_string(),
            message: "'trace.capture' and 'trace.include_tool_output' are both set; \
                      'trace.include_tool_output' is retired — remove it and keep 'trace.capture'"
                .to_string(),
        }),
        (Some(value), None) => value.parse().map_err(|e: ParseTraceCaptureError| {
            RuntimeManifestError::InvalidTraceConfig {
                field: "trace.capture".to_string(),
                message: e.to_string(),
            }
        }),
        (None, Some(flag)) => {
            let replacement = if flag {
                TraceCapture::Content
            } else {
                TraceCapture::Meta
            };
            eprintln!(
                "warning: 'trace.include_tool_output: {flag}' is retired; use \
                 'trace.capture: {}' instead",
                replacement.as_str()
            );
            Ok(replacement)
        }
        (None, None) => Ok(TraceCapture::default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_keys_resolve_to_meta() {
        assert_eq!(
            resolve_trace_capture(None, None).unwrap(),
            TraceCapture::Meta
        );
        assert_eq!(TraceCapture::default(), TraceCapture::Meta);
    }

    #[test]
    fn each_capture_name_parses() {
        assert_eq!(
            resolve_trace_capture(Some("none"), None).unwrap(),
            TraceCapture::None
        );
        assert_eq!(
            resolve_trace_capture(Some("meta"), None).unwrap(),
            TraceCapture::Meta
        );
        assert_eq!(
            resolve_trace_capture(Some("content"), None).unwrap(),
            TraceCapture::Content
        );
    }

    /// The alias maps in both directions: `true` to `Content`, `false` to `Meta`.
    #[test]
    fn include_tool_output_aliases_content_and_meta() {
        assert_eq!(
            resolve_trace_capture(None, Some(true)).unwrap(),
            TraceCapture::Content
        );
        assert_eq!(
            resolve_trace_capture(None, Some(false)).unwrap(),
            TraceCapture::Meta
        );
    }

    #[test]
    fn both_keys_are_refused_even_when_they_agree() {
        let err = resolve_trace_capture(Some("content"), Some(true)).unwrap_err();
        let rendered = err.to_string();
        assert!(rendered.contains("trace.capture"), "{rendered}");
        assert!(rendered.contains("trace.include_tool_output"), "{rendered}");
    }

    #[test]
    fn unknown_capture_value_names_the_field_and_the_accepted_values() {
        let err = resolve_trace_capture(Some("verbose"), None).unwrap_err();
        let rendered = err.to_string();
        assert!(rendered.contains("trace.capture"), "{rendered}");
        for accepted in TRACE_CAPTURE_ACCEPTED_VALUES {
            assert!(rendered.contains(accepted), "{rendered}");
        }
        assert!(rendered.contains("verbose"), "{rendered}");
    }
}
