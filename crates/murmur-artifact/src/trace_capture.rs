//! How much of each turn's wire payload a session's trace keeps, and the resolution of the
//! `trace.capture` manifest key that decides it.

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

/// Resolve a manifest's `trace.capture` into a [`TraceCapture`]. An absent key resolves to
/// [`TraceCapture::default`].
///
/// A value outside [`TRACE_CAPTURE_ACCEPTED_VALUES`] is a
/// [`RuntimeManifestError::InvalidTraceConfig`] naming the field and the accepted values.
pub fn resolve_trace_capture(capture: Option<&str>) -> Result<TraceCapture, RuntimeManifestError> {
    let Some(value) = capture else {
        return Ok(TraceCapture::default());
    };
    value.parse().map_err(
        |e: ParseTraceCaptureError| RuntimeManifestError::InvalidTraceConfig {
            field: "trace.capture".to_string(),
            message: e.to_string(),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_keys_resolve_to_meta() {
        assert_eq!(resolve_trace_capture(None).unwrap(), TraceCapture::Meta);
        assert_eq!(TraceCapture::default(), TraceCapture::Meta);
    }

    #[test]
    fn each_capture_name_parses() {
        assert_eq!(
            resolve_trace_capture(Some("none")).unwrap(),
            TraceCapture::None
        );
        assert_eq!(
            resolve_trace_capture(Some("meta")).unwrap(),
            TraceCapture::Meta
        );
        assert_eq!(
            resolve_trace_capture(Some("content")).unwrap(),
            TraceCapture::Content
        );
    }

    #[test]
    fn unknown_capture_value_names_the_field_and_the_accepted_values() {
        let err = resolve_trace_capture(Some("verbose")).unwrap_err();
        let rendered = err.to_string();
        assert!(rendered.contains("trace.capture"), "{rendered}");
        for accepted in TRACE_CAPTURE_ACCEPTED_VALUES {
            assert!(rendered.contains(accepted), "{rendered}");
        }
        assert!(rendered.contains("verbose"), "{rendered}");
    }
}
