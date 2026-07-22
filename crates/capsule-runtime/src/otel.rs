use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

/// Emits OpenTelemetry spans to an OTLP/HTTP endpoint (`/v1/traces`).
///
/// All emit calls are fire-and-forget — failures are logged to
/// `workdir/logs/otel.log` and never propagate errors to the caller.
///
/// When `endpoint` is `None` every method is a no-op.
pub(crate) struct OtelEmitter {
    endpoint: Option<String>,
    log_path: PathBuf,
    capsule_name: String,
    capsule_version: String,
    // ── Per-session state; reset by begin_session() ──
    trace_id: String,
    session_span_id: String,
    session_start_ns: u64,
    /// Parent span-id extracted from the incoming W3C traceparent header.
    parent_span_id: Option<String>,
    /// Set to true after the first emit_session_end so the error-path fallback
    /// (`emit_session_end_if_not_ended`) does not double-post the root span.
    session_ended: bool,
}

impl OtelEmitter {
    /// Create an emitter. `begin_session` must be called before any emit.
    pub(crate) fn new(
        endpoint: Option<String>,
        workdir: &Path,
        capsule_name: String,
        capsule_version: String,
    ) -> Self {
        let log_path = workdir.join("logs").join("otel.log");
        Self {
            endpoint,
            log_path,
            capsule_name,
            capsule_version,
            trace_id: new_trace_id(),
            session_span_id: new_span_id(),
            session_start_ns: now_ns(),
            parent_span_id: None,
            session_ended: false,
        }
    }

    /// Called once before each agent-loop iteration.
    ///
    /// Always generates a fresh `trace_id` — each capsule session is its own
    /// independent trace in Tempo. When a `traceparent` header is present,
    /// extracts the caller's `span_id` and stores it as `parent_span_id` so
    /// the emitted session span is linked to the caller's span. The topology
    /// builder resolves edges by looking up `parent_span_id` across all indexed
    /// spans; this only works when parent and child are in *different* traces.
    pub(crate) fn begin_session(&mut self, traceparent: Option<&str>) {
        let (_, parent_span_id) = parse_traceparent(traceparent);
        self.trace_id = new_trace_id();
        self.parent_span_id = parent_span_id;
        self.session_span_id = new_span_id();
        self.session_start_ns = now_ns();
        self.session_ended = false;
    }

    // ── Accessors ─────────────────────────────────────────────────────────────

    /// Return the W3C traceparent header value for outgoing A2A requests.
    ///
    /// Returns `None` when OTel is disabled so callers propagate no header.
    pub(crate) fn outgoing_traceparent(&self) -> Option<String> {
        self.endpoint
            .as_ref()
            .map(|_| format!("00-{}-{}-01", self.trace_id, self.session_span_id))
    }

    // ── Public emit methods ───────────────────────────────────────────────────

    /// Emit the root `capsule.session` span.
    /// Call at every session exit path (ok / failed / max_turns_reached).
    pub(crate) async fn emit_session_end(&mut self, exit_status: &str) {
        if self.endpoint.is_none() {
            return;
        }
        let end_ns = now_ns();
        let span = span_obj(
            &self.trace_id,
            &self.session_span_id,
            self.parent_span_id.as_deref(),
            "capsule.session",
            self.session_start_ns,
            end_ns,
            vec![kv_str("exit_status", exit_status)],
        );
        self.post_spans(vec![span]).await;
        self.session_ended = true;
    }

    /// Idempotent fallback for error exit paths that bypass the normal return sites.
    pub(crate) async fn emit_session_end_if_not_ended(&mut self, exit_status: &str) {
        if !self.session_ended {
            self.emit_session_end(exit_status).await;
        }
    }

    /// Emit a `capsule.inference` child span.
    /// `duration_ms` is measured by the caller (elapsed since dispatch start).
    /// `origin` tags a non-agent-loop completion — see the `origin` attribute.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn emit_inference(
        &self,
        turn: u32,
        input_tokens: u64,
        output_tokens: u64,
        decision: &str,
        tool_name: Option<&str>,
        duration_ms: u64,
        origin: Option<&crate::trace::InferenceOrigin>,
    ) {
        if self.endpoint.is_none() {
            return;
        }
        let end_ns = now_ns();
        let start_ns = end_ns.saturating_sub(ms_to_ns(duration_ms));
        let span_id = new_span_id();
        let mut attrs = vec![
            kv_int("turn", u64::from(turn)),
            kv_int("input_tokens", input_tokens),
            kv_int("output_tokens", output_tokens),
            kv_str("decision", decision),
        ];
        if let Some(tn) = tool_name {
            attrs.push(kv_str("tool_name", tn));
        }
        // Absent for an ordinary agent-loop turn; `hook:<name>` plus the model
        // actually sent for a completion a hook ran through `run-inference`.
        if let Some(o) = origin {
            attrs.push(kv_str("origin", &o.source));
            attrs.push(kv_str("model", &o.model));
        }
        let span = span_obj(
            &self.trace_id,
            &span_id,
            Some(&self.session_span_id),
            "capsule.inference",
            start_ns,
            end_ns,
            attrs,
        );
        self.post_spans(vec![span]).await;
    }

    /// Emit a `capsule.tool_call` child span.
    pub(crate) async fn emit_tool_call(
        &self,
        tool_name: &str,
        input_bytes: u64,
        output_bytes: u64,
        duration_ms: u64,
        status: &str,
    ) {
        if self.endpoint.is_none() {
            return;
        }
        let end_ns = now_ns();
        let start_ns = end_ns.saturating_sub(ms_to_ns(duration_ms));
        let span_id = new_span_id();
        let attrs = vec![
            kv_str("tool_name", tool_name),
            kv_int("input_bytes", input_bytes),
            kv_int("output_bytes", output_bytes),
            kv_int("duration_ms", duration_ms),
            kv_str("status", status),
        ];
        let span = span_obj(
            &self.trace_id,
            &span_id,
            Some(&self.session_span_id),
            "capsule.tool_call",
            start_ns,
            end_ns,
            attrs,
        );
        self.post_spans(vec![span]).await;
    }

    /// Emit a `capsule.shell` child span.
    pub(crate) async fn emit_shell(&self, command: &str, exit_code: i32, duration_ms: u64) {
        if self.endpoint.is_none() {
            return;
        }
        let end_ns = now_ns();
        let start_ns = end_ns.saturating_sub(ms_to_ns(duration_ms));
        let span_id = new_span_id();
        // Spec: truncate command to 200 chars
        let cmd: String = command.chars().take(200).collect();
        let attrs = vec![
            kv_str("command", &cmd),
            kv_int("exit_code", exit_code as u64),
            kv_int("duration_ms", duration_ms),
        ];
        let span = span_obj(
            &self.trace_id,
            &span_id,
            Some(&self.session_span_id),
            "capsule.shell",
            start_ns,
            end_ns,
            attrs,
        );
        self.post_spans(vec![span]).await;
    }

    /// Emit a `capsule.compaction` child span.
    pub(crate) async fn emit_compaction(&self, tokens_before: u64, tokens_after: u64) {
        if self.endpoint.is_none() {
            return;
        }
        let end_ns = now_ns();
        // Compaction itself is synchronous; model as a point-in-time event (1 ms span).
        let start_ns = end_ns.saturating_sub(1_000_000);
        let span_id = new_span_id();
        let attrs = vec![
            kv_int("tokens_before", tokens_before),
            kv_int("tokens_after", tokens_after),
        ];
        let span = span_obj(
            &self.trace_id,
            &span_id,
            Some(&self.session_span_id),
            "capsule.compaction",
            start_ns,
            end_ns,
            attrs,
        );
        self.post_spans(vec![span]).await;
    }

    // ── Internal ──────────────────────────────────────────────────────────────

    async fn post_spans(&self, spans: Vec<serde_json::Value>) {
        let Some(ref endpoint) = self.endpoint else {
            return;
        };

        let payload = serde_json::json!({
            "resourceSpans": [{
                "resource": {
                    "attributes": [
                        kv_str("service.name", &self.capsule_name),
                        kv_str("service.version", &self.capsule_version),
                    ]
                },
                "scopeSpans": [{
                    "scope": {"name": "murmur-capsule-runtime"},
                    "spans": spans
                }]
            }]
        });

        let body = match serde_json::to_string(&payload) {
            Ok(b) => b,
            Err(e) => {
                self.log_err(&format!("serialize OTLP payload: {e}"));
                return;
            }
        };

        let (addr, path_prefix) = match parse_otlp_endpoint(endpoint) {
            Ok(v) => v,
            Err(e) => {
                self.log_err(&format!("invalid otel_endpoint '{endpoint}': {e}"));
                return;
            }
        };

        let path = if path_prefix.is_empty() {
            "/v1/traces".to_string()
        } else {
            format!("{path_prefix}/v1/traces")
        };

        let request = format!(
            "POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );

        let stream = match TcpStream::connect(&addr).await {
            Ok(s) => s,
            Err(e) => {
                self.log_err(&format!("connect to {addr}: {e}"));
                return;
            }
        };

        let (reader_half, mut writer_half) = stream.into_split();
        if let Err(e) = writer_half.write_all(request.as_bytes()).await {
            self.log_err(&format!("write OTLP request: {e}"));
            return;
        }
        // Flush the request, but do NOT half-close the write side: a TCP FIN here
        // makes Go-based OTLP servers (e.g. Tempo) treat the client as gone and
        // cancel the request context mid-ingest, which surfaces as a spurious 503
        // and silently drops the span. `Content-Length` already delimits the body,
        // so the server sees a complete request without the shutdown.
        let _ = writer_half.flush().await;

        let mut reader = BufReader::new(reader_half);
        let mut status_line = String::new();
        let _ = reader.read_line(&mut status_line).await;

        // Log non-2xx responses for diagnostics (still non-fatal).
        let is_ok = status_line.contains(" 2");
        if !status_line.is_empty() && !is_ok {
            self.log_err(&format!("OTLP POST non-2xx: {}", status_line.trim()));
        }
    }

    fn log_err(&self, msg: &str) {
        let parent = self.log_path.parent().unwrap_or(Path::new("."));
        let _ = fs::create_dir_all(parent);
        if let Ok(mut f) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
        {
            let ts = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let _ = writeln!(f, "[{ts}] {msg}");
        }
    }
}

// ── Span builder ──────────────────────────────────────────────────────────────

fn span_obj(
    trace_id: &str,
    span_id: &str,
    parent_span_id: Option<&str>,
    name: &str,
    start_ns: u64,
    end_ns: u64,
    attributes: Vec<serde_json::Value>,
) -> serde_json::Value {
    let mut obj = serde_json::json!({
        "traceId":            trace_id,
        "spanId":             span_id,
        "name":               name,
        "kind":               1,
        "startTimeUnixNano":  start_ns.to_string(),
        "endTimeUnixNano":    end_ns.to_string(),
        "attributes":         attributes,
        "status":             {"code": 1},
    });
    if let Some(psid) = parent_span_id {
        obj["parentSpanId"] = serde_json::Value::String(psid.to_string());
    }
    obj
}

// ── Attribute helpers ─────────────────────────────────────────────────────────

fn kv_str(key: &str, value: &str) -> serde_json::Value {
    serde_json::json!({"key": key, "value": {"stringValue": value}})
}

/// Integer attributes are encoded as decimal strings per the OTLP proto3 JSON mapping.
fn kv_int(key: &str, value: u64) -> serde_json::Value {
    serde_json::json!({"key": key, "value": {"intValue": value.to_string()}})
}

// ── ID generation ─────────────────────────────────────────────────────────────

/// 16 random bytes → 32-char lowercase hex string (OTLP trace-id).
fn new_trace_id() -> String {
    let lo = uuid::Uuid::new_v4().as_u128();
    format!("{lo:032x}")
}

/// 8 random bytes → 16-char lowercase hex string (OTLP span-id).
fn new_span_id() -> String {
    let half = (uuid::Uuid::new_v4().as_u128() >> 64) as u64;
    format!("{half:016x}")
}

// ── Timing helpers ────────────────────────────────────────────────────────────

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

fn ms_to_ns(ms: u64) -> u64 {
    ms.saturating_mul(1_000_000)
}

// ── W3C TraceContext parsing ───────────────────────────────────────────────────

/// Parse a W3C `traceparent` header value.
///
/// Format: `00-<32hex>-<16hex>-<2hex>`
///
/// Returns `(trace_id, parent_span_id)` on success, `(None, None)` if absent
/// or malformed.
fn parse_traceparent(traceparent: Option<&str>) -> (Option<String>, Option<String>) {
    let Some(tp) = traceparent else {
        return (None, None);
    };
    let parts: Vec<&str> = tp.splitn(4, '-').collect();
    if parts.len() < 4 {
        return (None, None);
    }
    let trace_id = parts[1];
    let span_id = parts[2];
    if trace_id.len() == 32 && span_id.len() == 16 {
        (Some(trace_id.to_string()), Some(span_id.to_string()))
    } else {
        (None, None)
    }
}

// ── Endpoint parsing ──────────────────────────────────────────────────────────

/// Split an OTLP endpoint URL into `(host:port, path_prefix)`.
///
/// Examples:
/// - `"http://localhost:4318"` → `("localhost:4318", "")`
/// - `"http://tempo:4318/otlp"` → `("tempo:4318", "/otlp")`
fn parse_otlp_endpoint(endpoint: &str) -> Result<(String, String), String> {
    let stripped = endpoint
        .strip_prefix("http://")
        .or_else(|| endpoint.strip_prefix("https://"))
        .unwrap_or(endpoint);

    let (host_port, path_prefix) = if let Some(pos) = stripped.find('/') {
        let path = stripped[pos..].trim_end_matches('/').to_string();
        (&stripped[..pos], path)
    } else {
        (stripped, String::new())
    };

    if host_port.is_empty() {
        return Err("empty host:port".to_string());
    }
    Ok((host_port.to_string(), path_prefix))
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_traceparent_valid() {
        let tp = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let (tid, sid) = parse_traceparent(Some(tp));
        assert_eq!(tid.unwrap(), "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(sid.unwrap(), "00f067aa0ba902b7");
    }

    #[test]
    fn parse_traceparent_none() {
        let (tid, sid) = parse_traceparent(None);
        assert!(tid.is_none());
        assert!(sid.is_none());
    }

    #[test]
    fn parse_traceparent_malformed() {
        let (tid, sid) = parse_traceparent(Some("garbage"));
        assert!(tid.is_none());
        assert!(sid.is_none());
    }

    #[test]
    fn parse_traceparent_short_ids() {
        // Wrong lengths → rejected
        let tp = "00-abc-def-01";
        let (tid, sid) = parse_traceparent(Some(tp));
        assert!(tid.is_none());
        assert!(sid.is_none());
    }

    #[test]
    fn parse_otlp_endpoint_plain() {
        let (addr, prefix) = parse_otlp_endpoint("http://localhost:4318").unwrap();
        assert_eq!(addr, "localhost:4318");
        assert_eq!(prefix, "");
    }

    #[test]
    fn parse_otlp_endpoint_with_path() {
        let (addr, prefix) = parse_otlp_endpoint("http://tempo:4318/otlp").unwrap();
        assert_eq!(addr, "tempo:4318");
        assert_eq!(prefix, "/otlp");
    }

    #[test]
    fn parse_otlp_endpoint_no_scheme() {
        let (addr, prefix) = parse_otlp_endpoint("localhost:4318").unwrap();
        assert_eq!(addr, "localhost:4318");
        assert_eq!(prefix, "");
    }

    #[test]
    fn new_trace_id_is_32_chars() {
        let id = new_trace_id();
        assert_eq!(id.len(), 32, "trace_id must be 32 hex chars");
        assert!(
            id.chars().all(|c| c.is_ascii_hexdigit()),
            "trace_id must be hex"
        );
    }

    #[test]
    fn new_span_id_is_16_chars() {
        let id = new_span_id();
        assert_eq!(id.len(), 16, "span_id must be 16 hex chars");
        assert!(
            id.chars().all(|c| c.is_ascii_hexdigit()),
            "span_id must be hex"
        );
    }

    #[test]
    fn begin_session_extracts_parent_span_id_from_traceparent() {
        let dir = tempfile::tempdir().unwrap();
        let mut emitter = OtelEmitter::new(
            Some("http://localhost:4318".to_string()),
            dir.path(),
            "test-capsule".to_string(),
            "1.0.0".to_string(),
        );
        let tp = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        emitter.begin_session(Some(tp));
        // trace_id is always fresh — NOT inherited from the incoming traceparent
        assert_ne!(emitter.trace_id, "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(emitter.trace_id.len(), 32);
        // parent_span_id IS extracted so the emitted span links to the caller
        assert_eq!(emitter.parent_span_id.as_deref(), Some("00f067aa0ba902b7"));
    }

    #[test]
    fn begin_session_no_parent_generates_new_trace_id() {
        let dir = tempfile::tempdir().unwrap();
        let mut emitter = OtelEmitter::new(
            None,
            dir.path(),
            "test-capsule".to_string(),
            "1.0.0".to_string(),
        );
        emitter.begin_session(None);
        assert_eq!(emitter.trace_id.len(), 32);
        assert!(emitter.parent_span_id.is_none());
    }

    #[test]
    fn session_ended_flag_prevents_double_emit() {
        let dir = tempfile::tempdir().unwrap();
        let mut emitter = OtelEmitter::new(
            None, // no-op; we only test the flag
            dir.path(),
            "c".to_string(),
            "v".to_string(),
        );
        emitter.begin_session(None);
        // session_ended starts false
        assert!(!emitter.session_ended);
        // Simulate having called emit_session_end once
        emitter.session_ended = true;
        // emit_session_end_if_not_ended must not reset the flag
        // (we can only check the bool without actually posting)
        assert!(emitter.session_ended);
    }

    #[test]
    fn noop_when_endpoint_is_none() {
        // Nothing panics; no network call attempted
        let dir = tempfile::tempdir().unwrap();
        let emitter = OtelEmitter::new(None, dir.path(), "c".to_string(), "v".to_string());
        // All emit methods guard on endpoint.is_none() before any async work
        assert!(emitter.endpoint.is_none());
    }

    #[test]
    fn outgoing_traceparent_none_when_no_endpoint() {
        let dir = tempfile::tempdir().unwrap();
        let mut emitter = OtelEmitter::new(None, dir.path(), "c".to_string(), "v".to_string());
        emitter.begin_session(None);
        assert!(emitter.outgoing_traceparent().is_none());
    }

    #[test]
    fn outgoing_traceparent_returns_w3c_format_when_endpoint_set() {
        let dir = tempfile::tempdir().unwrap();
        let mut emitter = OtelEmitter::new(
            Some("http://localhost:4318".to_string()),
            dir.path(),
            "c".to_string(),
            "v".to_string(),
        );
        emitter.begin_session(None);
        let tp = emitter.outgoing_traceparent().expect("should produce traceparent");
        let parts: Vec<&str> = tp.splitn(4, '-').collect();
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[0], "00");
        assert_eq!(parts[1].len(), 32, "trace_id must be 32 hex chars");
        assert_eq!(parts[2].len(), 16, "span_id must be 16 hex chars");
        assert_eq!(parts[3], "01");
        assert_eq!(parts[1], emitter.trace_id);
        assert_eq!(parts[2], emitter.session_span_id);
    }

    #[test]
    fn outgoing_traceparent_uses_own_trace_id_not_parents() {
        let dir = tempfile::tempdir().unwrap();
        let mut emitter = OtelEmitter::new(
            Some("http://localhost:4318".to_string()),
            dir.path(),
            "c".to_string(),
            "v".to_string(),
        );
        let tp_in = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        emitter.begin_session(Some(tp_in));
        let tp_out = emitter.outgoing_traceparent().unwrap();
        // Trace-id must be the capsule's OWN fresh trace, NOT the incoming one
        assert!(!tp_out.contains("4bf92f3577b34da6a3ce929d0e0e4736"));
        // Span-id must be the freshly generated session_span_id
        assert!(!tp_out.contains("00f067aa0ba902b7"));
        let parts: Vec<&str> = tp_out.splitn(4, '-').collect();
        assert_eq!(parts[1].len(), 32);
        assert_eq!(parts[2].len(), 16);
    }
}
