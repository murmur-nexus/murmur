use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    process::Command as SysCommand,
    time::{SystemTime, UNIX_EPOCH},
};

use clap::Args;

use serde::{Deserialize, Serialize};

use crate::error::{CliError, E_IO_003};

const E_TOP_001: &str = "E-TOP-001"; // endpoint unreachable or invalid window
const E_TOP_002: &str = "E-TOP-002"; // Tempo query failed
const E_TOP_003: &str = "E-TOP-003"; // response parse error

const VIS_JS_CDN: &str = "https://cdnjs.cloudflare.com/ajax/libs/vis-network/9.1.9/dist/vis-network.min.js";
const VIS_CSS_CDN: &str = "https://cdnjs.cloudflare.com/ajax/libs/vis-network/9.1.9/dist/dist/vis-network.min.css";

// ── CLI args ──────────────────────────────────────────────────────────────────

#[derive(Debug, Args)]
pub(crate) struct TopologyArgs {
    /// Grafana Tempo HTTP query API endpoint (e.g. http://localhost:3200)
    #[arg(long, env = "MURMUR_OTEL_ENDPOINT")]
    pub otel_endpoint: String,

    /// Time window: 30m, 1h, 6h, 24h, 7d (default: 1h)
    #[arg(long, default_value = "1h")]
    pub window: String,

    /// Write HTML to this path instead of opening browser
    #[arg(long)]
    pub output: Option<PathBuf>,

    /// Serve on this local port instead of writing a temp file
    #[arg(long)]
    pub port: Option<u16>,
}

// ── Tempo API response types ──────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct TempoBuildInfo {
    version: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TempoSearchResponse {
    traces: Option<Vec<TempoTraceResult>>,
}

#[derive(Debug, Deserialize)]
struct TempoTraceResult {
    #[serde(rename = "traceID")]
    trace_id: String,
}

#[derive(Debug, Deserialize)]
struct OtlpTraceResponse {
    batches: Vec<OtlpBatch>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OtlpBatch {
    resource: Option<OtlpResource>,
    scope_spans: Vec<OtlpScopeSpans>,
}

#[derive(Debug, Deserialize)]
struct OtlpResource {
    attributes: Vec<OtlpAttribute>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OtlpScopeSpans {
    spans: Vec<OtlpSpan>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct OtlpSpan {
    span_id: String,
    #[serde(default)]
    parent_span_id: String,
    name: String,
    #[serde(default)]
    start_time_unix_nano: String,
    #[serde(default)]
    end_time_unix_nano: String,
    #[serde(default)]
    attributes: Vec<OtlpAttribute>,
}

#[derive(Debug, Deserialize, Clone)]
struct OtlpAttribute {
    key: String,
    value: OtlpAttributeValue,
}

#[derive(Debug, Deserialize, Clone)]
struct OtlpAttributeValue {
    #[serde(rename = "stringValue")]
    string_value: Option<String>,
}

// ── Graph types ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
struct CapsuleNode {
    id: String,
    capsule_name: String,
    capsule_version: String,
    exit_status: String,
    start_time_ms: i64,
    duration_ms: u64,
    inference_ms: u64,
    tool_ms: u64,
    shell_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
struct CapsuleEdge {
    from: String,
    to: String,
    weight: u32,
}

#[derive(Serialize)]
struct TopologyData<'a> {
    nodes: &'a [CapsuleNode],
    edges: &'a [CapsuleEdge],
}

// ── Tempo client ──────────────────────────────────────────────────────────────

struct TempoClient {
    base_url: String,
    client: ureq::Agent,
}

impl TempoClient {
    fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client: crate::registry_client::blocking_agent(std::time::Duration::from_secs(10)),
        }
    }

    fn check_reachable(&self) -> Result<(), CliError> {
        let url = format!("{}/ready", self.base_url);
        self.client.get(&url).call().map_err(|e| {
            CliError::new(
                E_TOP_001,
                format!("cannot reach Tempo endpoint {}: {e}", self.base_url),
            )
        })?;
        Ok(())
    }

    /// Best-effort: returns the Tempo version string if the /status/buildinfo
    /// endpoint exists and returns a parseable response. Returns None silently
    /// on any error so it never blocks the main flow.
    fn detect_version(&self) -> Option<String> {
        let url = format!("{}/status/buildinfo", self.base_url);
        let mut resp = self.client.get(&url).call().ok()?;
        let info: TempoBuildInfo = resp.body_mut().read_json().ok()?;
        info.version
    }

    fn search_capsule_sessions(&self, start: i64, end: i64, limit: u32) -> Result<Vec<String>, CliError> {
        let url = format!("{}/api/search", self.base_url);
        let mut resp = self
            .client
            .get(&url)
            .query_pairs([
                ("q", r#"{ name = "capsule.session" }"#),
                ("limit", limit.to_string().as_str()),
                ("start", start.to_string().as_str()),
                ("end", end.to_string().as_str()),
            ])
            .call()
            .map_err(|e| CliError::new(E_TOP_002, format!("Tempo search query failed: {e}")))?;

        let body: TempoSearchResponse = resp
            .body_mut()
            .read_json()
            .map_err(|e| CliError::new(E_TOP_003, format!("failed to parse Tempo search response: {e}")))?;

        Ok(body.traces.unwrap_or_default().into_iter().map(|t| t.trace_id).collect())
    }

    fn get_trace(&self, trace_id: &str) -> Result<OtlpTraceResponse, CliError> {
        let url = format!("{}/api/traces/{}", self.base_url, trace_id);
        let mut resp = self
            .client
            .get(&url)
            .call()
            .map_err(|e| CliError::new(E_TOP_002, format!("failed to fetch trace {trace_id}: {e}")))?;

        resp.body_mut()
            .read_json()
            .map_err(|e| CliError::new(E_TOP_003, format!("failed to parse trace {trace_id}: {e}")))
    }
}

// ── Window parsing ────────────────────────────────────────────────────────────

fn parse_window_to_seconds(window: &str) -> Result<i64, CliError> {
    let invalid = || CliError::new(E_TOP_001, format!("invalid window '{window}'; accepted: 30m, 1h, 6h, 24h, 7d"));

    if let Some(n) = window.strip_suffix('d') {
        return n.parse::<i64>().map(|d| d * 86400).map_err(|_| invalid());
    }
    if let Some(n) = window.strip_suffix('h') {
        return n.parse::<i64>().map(|h| h * 3600).map_err(|_| invalid());
    }
    if let Some(n) = window.strip_suffix('m') {
        return n.parse::<i64>().map(|m| m * 60).map_err(|_| invalid());
    }
    Err(invalid())
}

// ── Graph reconstruction ──────────────────────────────────────────────────────

fn get_attr_str(attrs: &[OtlpAttribute], key: &str) -> Option<String> {
    attrs.iter().find(|a| a.key == key)?.value.string_value.clone()
}

fn nanos_to_ms(s: &str) -> u64 {
    s.parse::<u64>().unwrap_or(0) / 1_000_000
}

fn span_duration_ms(span: &OtlpSpan) -> u64 {
    if span.end_time_unix_nano.is_empty() || span.start_time_unix_nano.is_empty() {
        return 0;
    }
    let end = span.end_time_unix_nano.parse::<u64>().unwrap_or(0);
    let start = span.start_time_unix_nano.parse::<u64>().unwrap_or(0);
    end.saturating_sub(start) / 1_000_000
}

fn build_graph(traces: &[(String, OtlpTraceResponse)]) -> (Vec<CapsuleNode>, Vec<CapsuleEdge>) {
    // First pass: index every spanId → traceId across all traces
    let mut span_owner: HashMap<String, String> = HashMap::new();
    for (trace_id, otlp) in traces {
        for batch in &otlp.batches {
            for ss in &batch.scope_spans {
                for span in &ss.spans {
                    span_owner.insert(span.span_id.clone(), trace_id.clone());
                }
            }
        }
    }

    struct RootInfo {
        trace_id: String,
        parent_span_id: String,
        node: CapsuleNode,
    }

    let mut root_infos: Vec<RootInfo> = Vec::new();

    for (trace_id, otlp) in traces {
        let mut resource_attrs: Vec<OtlpAttribute> = Vec::new();
        let mut session_span: Option<OtlpSpan> = None;
        let mut inference_ms: u64 = 0;
        let mut tool_ms: u64 = 0;
        let mut shell_ms: u64 = 0;

        for batch in &otlp.batches {
            if let Some(res) = &batch.resource {
                resource_attrs.extend(res.attributes.clone());
            }
            for ss in &batch.scope_spans {
                for span in &ss.spans {
                    match span.name.as_str() {
                        "capsule.session" => session_span = Some(span.clone()),
                        "capsule.inference" => inference_ms += span_duration_ms(span),
                        "capsule.tool_call" => tool_ms += span_duration_ms(span),
                        "capsule.shell" => shell_ms += span_duration_ms(span),
                        _ => {}
                    }
                }
            }
        }

        if let Some(span) = session_span {
            let capsule_name = get_attr_str(&resource_attrs, "service.name")
                .or_else(|| get_attr_str(&span.attributes, "service.name"))
                .unwrap_or_else(|| "unknown".to_string());
            let capsule_version = get_attr_str(&resource_attrs, "service.version")
                .or_else(|| get_attr_str(&span.attributes, "service.version"))
                .unwrap_or_else(|| "unknown".to_string());
            let exit_status = get_attr_str(&span.attributes, "exit_status")
                .unwrap_or_else(|| "running".to_string());
            let start_time_ms = nanos_to_ms(&span.start_time_unix_nano) as i64;
            let duration_ms = span_duration_ms(&span);

            root_infos.push(RootInfo {
                trace_id: trace_id.clone(),
                parent_span_id: span.parent_span_id.clone(),
                node: CapsuleNode {
                    id: trace_id.clone(),
                    capsule_name,
                    capsule_version,
                    exit_status,
                    start_time_ms,
                    duration_ms,
                    inference_ms,
                    tool_ms,
                    shell_ms,
                },
            });
        }
    }

    // Build edges: child's root span has parentSpanId from a different trace
    let mut edge_map: HashMap<(String, String), u32> = HashMap::new();
    for info in &root_infos {
        if !info.parent_span_id.is_empty() {
            if let Some(parent_trace) = span_owner.get(&info.parent_span_id) {
                if parent_trace != &info.trace_id {
                    *edge_map
                        .entry((parent_trace.clone(), info.trace_id.clone()))
                        .or_insert(0) += 1;
                }
            }
        }
    }

    let nodes = root_infos.into_iter().map(|i| i.node).collect();
    let edges = edge_map
        .into_iter()
        .map(|((from, to), weight)| CapsuleEdge { from, to, weight })
        .collect();

    (nodes, edges)
}

// ── HTML generation ───────────────────────────────────────────────────────────

fn generate_html(nodes: &[CapsuleNode], edges: &[CapsuleEdge], window: &str) -> String {
    let data = TopologyData { nodes, edges };
    let json_data = serde_json::to_string(&data).unwrap_or_else(|_| r#"{"nodes":[],"edges":[]}"#.to_string());

    let empty_msg = if nodes.is_empty() {
        "<div id=\"empty-msg\">No capsule sessions found in the selected time window.</div>"
    } else {
        ""
    };

    let count = nodes.len();
    format!(
        r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>Murmur Topology</title>
<link rel="stylesheet" href="{vis_css}">
<style>
*{{box-sizing:border-box;margin:0;padding:0}}
html,body{{height:100%;overflow:hidden}}
body{{display:grid;grid-template-rows:auto 1fr;font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",Helvetica,Arial,sans-serif;background:#0e0e0e;color:#d0d0d0}}
header{{background:#161616;border-bottom:1px solid #282828;padding:14px 20px;display:flex;align-items:center;gap:12px}}
.app-name{{font-size:.75rem;font-weight:700;letter-spacing:.12em;text-transform:uppercase;color:#26a69a}}
.app-meta{{font-size:.8rem;color:#555}}
#topology-graph{{min-height:0;background:#0e0e0e}}
#empty-msg{{position:fixed;top:50%;left:50%;transform:translate(-50%,-50%);color:#444;font-size:.9rem;letter-spacing:.05em}}
div.vis-tooltip{{background:transparent!important;border:none!important;padding:0!important;box-shadow:none!important;border-radius:0!important}}
</style>
</head>
<body>
<header>
  <span class="app-name">Murmur Topology</span>
  <span class="app-meta">window: {window} &mdash; {count} session(s)</span>
</header>
{empty_msg}
<div id="topology-graph"></div>
<script src="{vis_js}"></script>
<script>
window.TOPOLOGY_DATA = {json_data};
(function(){{
  var data=window.TOPOLOGY_DATA;
  var colorMap={{}};

  function makeColor(bg,bd,hbg,hbd){{
    return{{background:bg,border:bd,hover:{{background:hbg,border:hbd}},highlight:{{background:hbg,border:hbd}}}};
  }}
  function nodeColor(s){{
    if(s==="ok"||s==="completed")return makeColor("#26a69a","#1d7a74","#2dbdb4","#26a69a");
    if(s==="failed")return makeColor("#ef5350","#c62828","#f27272","#ef5350");
    if(s==="running")return makeColor("#ffd54f","#f9a825","#ffe082","#ffd54f");
    return makeColor("#ffb74d","#e65100","#ffcc80","#ffb74d");
  }}

  function makeTooltip(n){{
    var d=document.createElement("div");
    var ts=n.start_time_ms?new Date(n.start_time_ms).toLocaleString():"-";
    d.style.cssText="background:#1a1a1a;border:1px solid #2e2e2e;border-radius:8px;padding:12px 16px;"
      +"font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',sans-serif;font-size:12px;"
      +"line-height:1.7;min-width:220px;box-shadow:0 8px 24px rgba(0,0,0,.7)";
    d.innerHTML=
      "<div style='color:#fff;font-weight:600;font-size:13px;margin-bottom:4px'>"+n.capsule_name
      +" <span style='color:#555;font-weight:400'>v"+n.capsule_version+"</span></div>"
      +"<div style='color:#888'>Started&nbsp;<span style='color:#ccc'>"+ts+"</span></div>"
      +"<div style='color:#888'>Status&nbsp;<span style='color:#ccc'>"+n.exit_status+"</span></div>"
      +"<div style='color:#888'>Duration&nbsp;<span style='color:#ccc'>"+n.duration_ms+"ms</span></div>"
      +"<div style='color:#888;margin-top:6px'>Inference&nbsp;<span style='color:#ccc'>"+n.inference_ms+"ms</span>"
      +" &ensp;Tool&nbsp;<span style='color:#ccc'>"+n.tool_ms+"ms</span>"
      +" &ensp;Shell&nbsp;<span style='color:#ccc'>"+n.shell_ms+"ms</span></div>";
    return d;
  }}

  function drawPill(ctx,x,y,w,h,bg,bd,lw){{
    var r=h/2;
    ctx.beginPath();
    ctx.moveTo(x-w/2+r,y-h/2);
    ctx.lineTo(x+w/2-r,y-h/2);
    ctx.arc(x+w/2-r,y,r,-Math.PI/2,Math.PI/2);
    ctx.lineTo(x-w/2+r,y+h/2);
    ctx.arc(x-w/2+r,y,r,Math.PI/2,-Math.PI/2);
    ctx.closePath();
    ctx.fillStyle=bg;
    ctx.fill();
    ctx.strokeStyle=bd;
    ctx.lineWidth=lw;
    ctx.stroke();
  }}

  function pillRenderer(params){{
    var ctx=params.ctx,x=params.x,y=params.y;
    var hover=params.state.hover||params.state.selected;
    var col=colorMap[params.id]||{{background:"#555",border:"#333",hover:{{background:"#777",border:"#555"}}}};
    var lines=(params.label||"").split("\n");
    return{{
      drawNode:function(){{
        drawPill(ctx,x,y,52,24,
          hover?col.hover.background:col.background,
          hover?col.hover.border:col.border,
          hover?2.5:1.5);
      }},
      drawExternalLabel:function(){{
        ctx.save();
        ctx.font="11px -apple-system,BlinkMacSystemFont,'Segoe UI',sans-serif";
        ctx.fillStyle="#d0d0d0";
        ctx.textAlign="center";
        ctx.textBaseline="top";
        lines.forEach(function(line,i){{
          ctx.fillText(line,x,y+15+i*15);
        }});
        ctx.restore();
      }},
      nodeDimensions:{{width:52,height:24}}
    }};
  }}

  var nodes=data.nodes.map(function(n){{
    var c=nodeColor(n.exit_status);
    colorMap[n.id]=c;
    return{{
      id:n.id,
      label:n.capsule_name+"\n"+n.exit_status+"  "+n.duration_ms+"ms",
      title:makeTooltip(n),
      shape:"custom",
      ctxRenderer:pillRenderer,
      color:c
    }};
  }});

  var edges=data.edges.map(function(e){{
    var tc=(colorMap[e.to]||{{background:"#888"}}).background;
    return{{
      from:e.from,to:e.to,
      width:Math.max(1,e.weight),
      arrows:"to",
      color:{{color:"#3a3a3a",highlight:tc,hover:tc,inherit:false}}
    }};
  }});

  if(nodes.length>0){{
    var container=document.getElementById("topology-graph");
    new vis.Network(container,{{nodes:nodes,edges:edges}},{{
      physics:{{enabled:true,stabilization:{{iterations:150}}}},
      interaction:{{hover:true,tooltipDelay:80}},
      edges:{{smooth:{{type:"dynamic"}}}}
    }});
  }}
}})();
</script>
</body>
</html>"##,
        vis_css = VIS_CSS_CDN,
        vis_js = VIS_JS_CDN,
        window = window,
        count = count,
        empty_msg = empty_msg,
        json_data = json_data,
    )
}

// ── Output helpers ────────────────────────────────────────────────────────────

fn open_browser(target: &str) {
    if cfg!(target_os = "macos") {
        SysCommand::new("open").arg(target).spawn().ok();
    } else {
        SysCommand::new("xdg-open").arg(target).spawn().ok();
    }
}

fn write_html_file(path: &PathBuf, html: &str) -> Result<(), CliError> {
    fs::write(path, html)
        .map_err(|e| CliError::new(E_IO_003, format!("failed to write HTML to {}: {e}", path.display())))
}

fn serve_on_port(html: &str, port: u16) -> Result<(), CliError> {
    use std::{
        io::{Read, Write},
        net::TcpListener,
    };

    let listener = TcpListener::bind(format!("127.0.0.1:{port}"))
        .map_err(|e| CliError::new(E_IO_003, format!("failed to bind port {port}: {e}")))?;
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}");
    eprintln!("murmur: serving topology at {url}  (Ctrl+C to stop)");
    open_browser(&url);

    while let Ok((mut stream, _)) = listener.accept() {
        stream.set_read_timeout(Some(std::time::Duration::from_secs(2))).ok();
        let mut buf = [0u8; 4096];
        let _ = stream.read(&mut buf);
        let resp = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            html.len(),
            html
        );
        let _ = stream.write_all(resp.as_bytes());
    }
    Ok(())
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub(crate) fn run_topology(args: &TopologyArgs) -> Result<(), CliError> {
    let window_secs = parse_window_to_seconds(&args.window)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let start = now - window_secs;
    let end = now;

    let tempo = TempoClient::new(&args.otel_endpoint);
    tempo.check_reachable()?;

    let tempo_version = tempo.detect_version();
    if let Some(ref v) = tempo_version {
        eprintln!("murmur: Tempo {v}");
    }

    let trace_ids = tempo.search_capsule_sessions(start, end, 500)?;

    if trace_ids.is_empty() {
        let is_v3 = tempo_version.as_deref().map(|v| v.starts_with('3')).unwrap_or(false);
        if is_v3 {
            eprintln!(
                "murmur: hint: Tempo v3 requires 'block: version: vParquet4' under \
                 storage.trace in tempo.yaml — restart with \
                 'docker compose down -v && docker compose up -d' after editing"
            );
        } else {
            eprintln!(
                "murmur: hint: WAL flush takes 60–90 s after spans are posted — \
                 retry if sessions were just recorded; also confirm tempo.yaml has \
                 'block: version: vParquet3' under storage.trace"
            );
        }
    }

    let mut traces: Vec<(String, OtlpTraceResponse)> = Vec::new();
    for trace_id in &trace_ids {
        match tempo.get_trace(trace_id) {
            Ok(t) => traces.push((trace_id.clone(), t)),
            Err(e) => eprintln!("murmur: warning: {e}"),
        }
    }

    let (nodes, edges) = build_graph(&traces);
    let html = generate_html(&nodes, &edges, &args.window);

    if let Some(port) = args.port {
        serve_on_port(&html, port)?;
    } else if let Some(output_path) = &args.output {
        write_html_file(output_path, &html)?;
        println!("murmur: topology written to {}", output_path.display());
    } else {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let tmp_path = PathBuf::from(format!("/tmp/murmur-topology-{ts}.html"));
        write_html_file(&tmp_path, &html)?;
        println!("murmur: opening topology at {}", tmp_path.display());
        open_browser(tmp_path.to_str().unwrap_or_default());
    }

    Ok(())
}
