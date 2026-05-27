//! OTLP trace export for sweep observability (issue #310).
//!
//! # Design
//!
//! Spans are emitted at span-close time (after each instance finishes), which
//! keeps the implementation consistent with most OTel SDKs and avoids
//! complexity around partial span state.
//!
//! When no endpoint is configured (neither `--otlp-endpoint` nor
//! `OTEL_EXPORTER_OTLP_ENDPOINT` is set), the tracer is a pure no-op: no
//! sockets are opened, no allocations beyond the `TraceId` string.
//!
//! Export uses OTLP/HTTP with JSON encoding (no heavy protobuf/tonic deps).
//! `reqwest` is already a project dependency so no new crate is required.
//!
//! Export failures (collector down, network drop, slow consumer) are caught,
//! logged at `WARN`, and counted in a `span_export_dropped` atomic.  The
//! sweep always continues regardless of export outcome.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// Trace / Span IDs
// ---------------------------------------------------------------------------

/// A 128-bit OTel trace ID encoded as 32 lowercase hex chars.
pub type TraceId = String;

/// A 64-bit OTel span ID encoded as 16 lowercase hex chars.
pub type SpanId = String;

/// Generates a deterministic but collision-resistant 128-bit trace ID by
/// hashing `instance_id + sweep_id + monotonic_nanos`.
pub fn new_trace_id(instance_id: &str, sweep_id: &str) -> TraceId {
    let mut hasher = Sha256::new();
    hasher.update(instance_id.as_bytes());
    hasher.update(b"\x00");
    hasher.update(sweep_id.as_bytes());
    let hash = hasher.finalize();
    // Use the first 16 bytes (128 bits) as the trace ID.
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&hash[..16]);
    format!("{:032x}", u128::from_be_bytes(bytes))
}

/// Generates a 64-bit span ID by taking the first 8 bytes of a SHA-256 hash.
pub fn new_span_id(salt: &str, trace_id: &TraceId) -> SpanId {
    let mut hasher = Sha256::new();
    hasher.update(salt.as_bytes());
    hasher.update(b"\x01");
    hasher.update(trace_id.as_bytes());
    let hash = hasher.finalize();
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&hash[..8]);
    format!("{:016x}", u64::from_be_bytes(bytes))
}

fn now_unix_nanos() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
    )
    .unwrap_or(u64::MAX)
}

// ---------------------------------------------------------------------------
// Span data (attributes follow OTel GenAI conventions where applicable)
// ---------------------------------------------------------------------------

/// Span data collected for the root sweep span.
pub struct SweepSpanData {
    pub sweep_id: String,
    pub dataset: String,
    pub model: String,
    pub instance_count: u64,
    pub resolved_count: u64,
    pub total_cost_usd: f64,
    pub harness_version: String,
    pub git_sha: Option<String>,
    pub start_nanos: u64,
    pub end_nanos: u64,
}

/// Span data for one instance run.
pub struct InstanceSpanData {
    pub trace_id: TraceId,
    pub sweep_span_id: SpanId,
    pub instance_id: String,
    pub repo: String,
    pub outcome: String,
    pub cost_usd: f64,
    pub step_count: u64,
    pub final_patch_bytes: u64,
    pub start_nanos: u64,
    pub end_nanos: u64,
    pub model_calls: Vec<ModelCallSpanData>,
    pub tool_calls: Vec<ToolCallSpanData>,
}

/// Span data for one model call.
///
/// Sensitive trajectory content (raw prompts, observations) is explicitly
/// excluded; only numeric telemetry and non-sensitive metadata are recorded.
pub struct ModelCallSpanData {
    pub model: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub latency_ms: u64,
    pub finish_reason: String,
    pub start_nanos: u64,
    pub end_nanos: u64,
}

/// Span data for one tool invocation.
///
/// `observation_bytes` is the byte length; the actual content is not recorded
/// in spans to keep sensitive output out of the observability pipeline.
pub struct ToolCallSpanData {
    pub tool_name: String,
    pub exit_code: i32,
    pub observation_bytes: u64,
    pub duration_ms: u64,
    pub truncated: bool,
    pub start_nanos: u64,
    pub end_nanos: u64,
}

// ---------------------------------------------------------------------------
// Tracer — the main public surface
// ---------------------------------------------------------------------------

/// OTLP tracer shared across all workers in a sweep.
///
/// Internally wraps a `reqwest::Client` and a `span_export_dropped` counter.
/// Cheaply cloneable (wraps an `Arc`).
#[derive(Clone)]
pub struct Tracer(Option<Arc<TracerInner>>);

struct TracerInner {
    endpoint: String,
    client: reqwest::Client,
    dropped: Arc<AtomicU64>,
    /// Extra HTTP headers to send with every export request (e.g. auth tokens
    /// from `OTEL_EXPORTER_OTLP_TRACES_HEADERS` / `OTEL_EXPORTER_OTLP_HEADERS`).
    headers: Vec<(String, String)>,
}

/// Decode a percent-encoded string (W3C Baggage / OTel header value encoding).
///
/// `%XX` sequences are replaced with the corresponding byte value; other
/// characters are passed through as-is.  Non-UTF-8 sequences are dropped.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (
                (bytes[i + 1] as char).to_digit(16),
                (bytes[i + 2] as char).to_digit(16),
            ) {
                // hi and lo are each 0..=15, so (hi*16)+lo is 0..=255.
                out.push(u8::try_from(hi * 16 + lo).unwrap_or_default());
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_default()
}

/// Parse a comma-separated OTel header env var value into `(name, value)` pairs.
///
/// Format: `"key1=val1,key2=val2"`.  Values are percent-decoded per the W3C
/// Baggage / OTel spec (e.g. `Bearer%20token` → `Bearer token`).
/// Pairs that do not contain `=` are skipped.
pub fn parse_otlp_header_env(raw: &str) -> Vec<(String, String)> {
    raw.split(',')
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            let k = k.trim().to_owned();
            let v = percent_decode(v.trim());
            if k.is_empty() { None } else { Some((k, v)) }
        })
        .collect()
}

/// Read OTLP export headers from standard OTel env vars.
///
/// `OTEL_EXPORTER_OTLP_TRACES_HEADERS` takes precedence; falls back to
/// `OTEL_EXPORTER_OTLP_HEADERS`.  Returns an empty vec when neither is set.
pub fn resolve_otlp_headers() -> Vec<(String, String)> {
    for var in &[
        "OTEL_EXPORTER_OTLP_TRACES_HEADERS",
        "OTEL_EXPORTER_OTLP_HEADERS",
    ] {
        if let Ok(val) = std::env::var(var) {
            if !val.is_empty() {
                return parse_otlp_header_env(&val);
            }
        }
    }
    vec![]
}

/// Count total spans in a sweep export (sweep root + per-instance tree).
fn count_spans(instances: &[InstanceSpanData]) -> u64 {
    1 + instances
        .iter()
        .map(|i| 1 + i.model_calls.len() as u64 + i.tool_calls.len() as u64)
        .sum::<u64>()
}

impl Tracer {
    /// Construct a no-op tracer when OTLP is not configured.
    pub fn noop() -> Self {
        Self(None)
    }

    /// Construct an active tracer that exports to `endpoint`.
    ///
    /// The endpoint should be an OTLP/HTTP base URL, e.g.
    /// `http://localhost:4318`.  The tracer will POST to
    /// `{endpoint}/v1/traces`.
    pub fn new(endpoint: impl Into<String>, dropped: Arc<AtomicU64>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap_or_default();
        Self(Some(Arc::new(TracerInner {
            endpoint: endpoint.into(),
            client,
            dropped,
            headers: resolve_otlp_headers(),
        })))
    }

    /// Returns `true` when a real OTLP exporter is configured.
    pub fn is_active(&self) -> bool {
        self.0.is_some()
    }

    /// Export a complete sweep + all its instances in one OTLP request.
    ///
    /// Errors are caught, logged at WARN, and counted.
    pub async fn export_sweep(&self, sweep: &SweepSpanData, instances: &[InstanceSpanData]) {
        let Some(inner) = &self.0 else { return };

        // inner.endpoint is the fully-resolved traces URL (resolve_endpoint normalises it).
        let url = inner.endpoint.clone();
        let body = build_otlp_json(sweep, instances);

        let mut req = inner
            .client
            .post(&url)
            .header("Content-Type", "application/json");
        for (k, v) in &inner.headers {
            req = req.header(k.as_str(), v.as_str());
        }
        let result = req.body(body).send().await;

        match result {
            Ok(resp) if resp.status().is_success() => {
                // Check for OTLP partial success: HTTP 200 but some spans rejected.
                if let Ok(text) = resp.text().await {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                        let rejected = json
                            .get("partialSuccess")
                            .and_then(|ps| ps.get("rejectedSpans"))
                            .and_then(|v| {
                                // OTLP/JSON encodes int64 fields as decimal strings.
                                v.as_u64()
                                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                            })
                            .unwrap_or(0);
                        if rejected > 0 {
                            tracing::warn!(
                                endpoint = %url,
                                rejected_spans = rejected,
                                "OTLP partial success: collector rejected some spans"
                            );
                            inner.dropped.fetch_add(rejected, Ordering::Relaxed);
                        }
                    }
                }
            }
            Ok(resp) => {
                let status = resp.status();
                tracing::warn!(
                    endpoint = %url,
                    http_status = %status,
                    "OTLP export failed: non-2xx response"
                );
                inner
                    .dropped
                    .fetch_add(count_spans(instances), Ordering::Relaxed);
            }
            Err(e) => {
                tracing::warn!(
                    endpoint = %url,
                    error = %e,
                    "OTLP export failed: network error"
                );
                inner
                    .dropped
                    .fetch_add(count_spans(instances), Ordering::Relaxed);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// OTLP/HTTP/JSON serialisation
//
// Implements the OTLP JSON encoding for trace export.
// https://opentelemetry.io/docs/specs/otlp/#otlphttp
// ---------------------------------------------------------------------------

fn attr(key: &str, value: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "key": key, "value": value })
}

fn str_attr(key: &str, val: &str) -> serde_json::Value {
    attr(key, &serde_json::json!({ "stringValue": val }))
}

fn int_attr(key: &str, val: i64) -> serde_json::Value {
    attr(key, &serde_json::json!({ "intValue": val.to_string() }))
}

fn double_attr(key: &str, val: f64) -> serde_json::Value {
    attr(key, &serde_json::json!({ "doubleValue": val }))
}

fn bool_attr(key: &str, val: bool) -> serde_json::Value {
    attr(key, &serde_json::json!({ "boolValue": val }))
}

#[allow(clippy::too_many_arguments)]
fn span_json(
    trace_id: &str,
    span_id: &str,
    parent_span_id: Option<&str>,
    name: &str,
    start_nanos: u64,
    end_nanos: u64,
    attributes: &[serde_json::Value],
    links: &[serde_json::Value],
    is_error: bool,
) -> serde_json::Value {
    // STATUS_CODE_ERROR = 2, STATUS_CODE_OK = 1 (OTel proto).
    let status_code = if is_error { 2u8 } else { 1u8 };
    let mut span = serde_json::json!({
        "traceId": trace_id,
        "spanId": span_id,
        "name": name,
        "kind": 1,  // SPAN_KIND_INTERNAL
        "startTimeUnixNano": start_nanos.to_string(),
        "endTimeUnixNano": end_nanos.to_string(),
        "attributes": attributes,
        "status": { "code": status_code }
    });
    if let Some(pid) = parent_span_id {
        span["parentSpanId"] = serde_json::json!(pid);
    }
    if !links.is_empty() {
        span["links"] = serde_json::json!(links);
    }
    span
}

fn infer_gen_ai_system(model: &str) -> &'static str {
    // Inspect every path component so nested LiteLLM/OpenRouter prefixes like
    // "openrouter/anthropic/claude-opus-4-7" are handled correctly.
    for part in model.split('/') {
        if part == "anthropic" {
            return "anthropic";
        }
        if part == "openai" {
            return "openai";
        }
        if part == "gemini" || part.contains("vertex") {
            return "google_vertexai";
        }
    }
    // Fall back to model-name heuristics on the leaf segment.
    let bare = model.split('/').next_back().unwrap_or(model);
    if bare.starts_with("claude") {
        "anthropic"
    } else if bare.starts_with("gpt") || bare.starts_with("o1") || bare.starts_with("o3") {
        "openai"
    } else if bare.starts_with("gemini") {
        "google_vertexai"
    } else if !model.contains('/') {
        // Bare model names with no provider prefix route to OpenAI in the
        // model layer (parse_provider in model/litellm.rs); mirror that here
        // so `o4-mini`, `o3-mini`, etc. get the correct gen_ai.system.
        "openai"
    } else {
        "unknown"
    }
}

fn u64_to_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn build_instance_spans(
    sweep_id: &str,
    sweep_trace_id: &str,
    sweep_span_id: &str,
    inst: &InstanceSpanData,
) -> Vec<serde_json::Value> {
    let inst_span_id = new_span_id(&format!("inst_{}", inst.instance_id), &inst.trace_id);
    let mut spans = vec![];

    let inst_attrs = vec![
        str_attr("sweep_id", sweep_id),
        str_attr("instance_id", &inst.instance_id),
        str_attr("repo", &inst.repo),
        str_attr("outcome", &inst.outcome),
        double_attr("cost_usd", inst.cost_usd),
        int_attr("step_count", u64_to_i64(inst.step_count)),
        int_attr("final_patch_bytes", u64_to_i64(inst.final_patch_bytes)),
    ];
    // Each instance is its own independent OTel trace. A span link back to the
    // sweep span lets Jaeger/Tempo show the sweep→instance relationship without
    // violating the OTel invariant that parent-child spans share a traceId.
    let sweep_link = serde_json::json!({
        "traceId": sweep_trace_id,
        "spanId": sweep_span_id,
        "attributes": [str_attr("relationship", "part_of_sweep")],
        "flags": 1
    });
    let inst_is_error = !matches!(inst.outcome.as_str(), "submitted" | "");
    spans.push(span_json(
        &inst.trace_id,
        &inst_span_id,
        None, // root of its own trace — linked to sweep via span link
        "instance",
        inst.start_nanos,
        inst.end_nanos,
        &inst_attrs,
        &[sweep_link],
        inst_is_error,
    ));

    for (i, mc) in inst.model_calls.iter().enumerate() {
        let mc_span_id = new_span_id(&format!("mc_{}_{i}", inst.instance_id), &inst.trace_id);
        let mc_attrs = vec![
            str_attr("gen_ai.system", infer_gen_ai_system(&mc.model)),
            str_attr("gen_ai.request.model", &mc.model),
            int_attr("gen_ai.usage.input_tokens", u64_to_i64(mc.prompt_tokens)),
            int_attr(
                "gen_ai.usage.output_tokens",
                u64_to_i64(mc.completion_tokens),
            ),
            int_attr(
                "gen_ai.usage.cache_read_input_tokens",
                u64_to_i64(mc.cache_read_tokens),
            ),
            int_attr(
                "gen_ai.usage.cache_creation_input_tokens",
                u64_to_i64(mc.cache_creation_tokens),
            ),
            int_attr("latency_ms", u64_to_i64(mc.latency_ms)),
            str_attr("gen_ai.response.finish_reasons", &mc.finish_reason),
        ];
        spans.push(span_json(
            &inst.trace_id,
            &mc_span_id,
            Some(&inst_span_id),
            "model_call",
            mc.start_nanos,
            mc.end_nanos,
            &mc_attrs,
            &[],
            false,
        ));
    }

    for (i, tc) in inst.tool_calls.iter().enumerate() {
        let tc_span_id = new_span_id(&format!("tc_{}_{i}", inst.instance_id), &inst.trace_id);
        let tc_attrs = vec![
            str_attr("tool_name", &tc.tool_name),
            int_attr("exit_code", i64::from(tc.exit_code)),
            int_attr("observation_bytes", u64_to_i64(tc.observation_bytes)),
            int_attr("duration_ms", u64_to_i64(tc.duration_ms)),
            bool_attr("truncated", tc.truncated),
        ];
        spans.push(span_json(
            &inst.trace_id,
            &tc_span_id,
            Some(&inst_span_id),
            "tool_call",
            tc.start_nanos,
            tc.end_nanos,
            &tc_attrs,
            &[],
            tc.exit_code != 0,
        ));
    }

    spans
}

fn build_otlp_json(sweep: &SweepSpanData, instances: &[InstanceSpanData]) -> String {
    let sweep_trace_id = new_trace_id("sweep", &sweep.sweep_id);
    let sweep_span_id = new_span_id("sweep_span", &sweep_trace_id);

    let mut sweep_attrs = vec![
        str_attr("sweep_id", &sweep.sweep_id),
        str_attr("dataset", &sweep.dataset),
        str_attr("model", &sweep.model),
        int_attr("instance_count", u64_to_i64(sweep.instance_count)),
        int_attr("resolved_count", u64_to_i64(sweep.resolved_count)),
        double_attr("total_cost_usd", sweep.total_cost_usd),
        str_attr("harness_version", &sweep.harness_version),
    ];
    if let Some(sha) = &sweep.git_sha {
        sweep_attrs.push(str_attr("git_sha", sha));
    }

    let mut spans: Vec<serde_json::Value> = vec![span_json(
        &sweep_trace_id,
        &sweep_span_id,
        None,
        "sweep",
        sweep.start_nanos,
        sweep.end_nanos,
        &sweep_attrs,
        &[],
        false,
    )];

    for inst in instances {
        spans.extend(build_instance_spans(
            &sweep.sweep_id,
            &sweep_trace_id,
            &sweep_span_id,
            inst,
        ));
    }

    serde_json::to_string(&serde_json::json!({
        "resourceSpans": [{
            "resource": {
                "attributes": [
                    str_attr("service.name", "maxwells-daemon"),
                    str_attr("service.version", env!("CARGO_PKG_VERSION")),
                ]
            },
            "scopeSpans": [{
                "scope": {
                    "name": "maxwells-daemon",
                    "version": env!("CARGO_PKG_VERSION")
                },
                "spans": spans
            }]
        }]
    }))
    .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Resolve effective OTLP endpoint (CLI flag beats env var)
// ---------------------------------------------------------------------------

/// Returns the fully-resolved OTLP traces endpoint URL.
///
/// Resolution order (first match wins):
/// 1. CLI `--otlp-endpoint` flag — treated as a base URL; `/v1/traces` is appended.
/// 2. `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` — the OTel trace-specific env var.
///    Already a full URL (e.g. `http://host:4318/v1/traces`); used as-is.
/// 3. `OTEL_EXPORTER_OTLP_ENDPOINT` — the generic OTel base URL env var;
///    `/v1/traces` is appended.
pub fn resolve_endpoint(cli_flag: Option<&str>) -> Option<String> {
    let with_path = |base: &str| format!("{}/v1/traces", base.trim_end_matches('/'));

    if let Some(ep) = cli_flag {
        return Some(with_path(ep));
    }
    if let Ok(ep) = std::env::var("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT") {
        if !ep.is_empty() {
            return Some(ep); // already a full URL per OTel spec
        }
    }
    std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .ok()
        .filter(|s| !s.is_empty())
        .map(|base| with_path(&base))
}

// ---------------------------------------------------------------------------
// Build instance span data from a completed trajectory
// ---------------------------------------------------------------------------

pub use build::instance_span_data_from_result;
pub use build::instance_span_data_from_trajectory;

pub(crate) mod build {
    use super::{InstanceSpanData, ModelCallSpanData, ToolCallSpanData, now_unix_nanos};
    use crate::trajectory::Trajectory;

    /// Parse an ISO 8601 / RFC 3339 timestamp string into Unix nanoseconds.
    fn iso8601_to_nanos(s: &str) -> Option<u64> {
        chrono::DateTime::parse_from_rfc3339(s).ok().and_then(|dt| {
            let secs = dt.timestamp();
            if secs < 0 {
                return None;
            }
            u64::try_from(secs)
                .ok()
                .map(|s| s * 1_000_000_000 + u64::from(dt.timestamp_subsec_nanos()))
        })
    }

    /// Construct a minimal `InstanceSpanData` from an `InstanceResult` alone.
    ///
    /// Used when an instance failed before writing a trajectory file (e.g.
    /// build-env or MCP discovery errors).  The span carries no model/tool
    /// child spans; callers should prefer `instance_span_data_from_trajectory`
    /// when a trajectory is available.
    #[allow(clippy::too_many_arguments)]
    pub fn instance_span_data_from_result(
        trace_id: &str,
        sweep_span_id: &str,
        instance_id: &str,
        repo: &str,
        result: &crate::run::swebench::InstanceResult,
        final_patch_bytes: u64,
        start_nanos: u64,
        end_nanos: u64,
    ) -> InstanceSpanData {
        InstanceSpanData {
            trace_id: trace_id.to_owned(),
            sweep_span_id: sweep_span_id.to_owned(),
            instance_id: instance_id.to_owned(),
            repo: repo.to_owned(),
            outcome: result.outcome.as_deref().unwrap_or("unknown").to_owned(),
            cost_usd: result.cost_usd.unwrap_or(0.0),
            step_count: u64::from(result.steps.unwrap_or(0)),
            final_patch_bytes,
            start_nanos,
            end_nanos,
            model_calls: vec![],
            tool_calls: vec![],
        }
    }

    /// Construct `InstanceSpanData` from a completed trajectory.
    ///
    /// Sensitive content (raw tool output, patch bodies, task strings) is
    /// never placed in span attributes — only numeric counters, exit codes,
    /// and model metadata.
    pub fn instance_span_data_from_trajectory(
        trace_id: &str,
        sweep_span_id: &str,
        instance_id: &str,
        repo: &str,
        traj: &Trajectory,
        final_patch_bytes: u64,
        start_nanos: u64,
    ) -> InstanceSpanData {
        let outcome = traj.info.outcome.as_deref().unwrap_or("unknown").to_owned();
        let cost_usd = traj.info.total_cost_usd.unwrap_or(0.0);
        let step_count = u64::from(traj.info.steps.unwrap_or(0));

        // Use actual trajectory wall-clock timestamps when available so that
        // instance spans reflect the real execution window, not the sweep start.
        let start_nanos = traj
            .info
            .started_at
            .as_deref()
            .and_then(iso8601_to_nanos)
            .unwrap_or(start_nanos);
        let end_nanos = traj
            .info
            .ended_at
            .as_deref()
            .and_then(iso8601_to_nanos)
            .unwrap_or_else(now_unix_nanos);

        let mut model_calls = vec![];
        let mut tool_calls = vec![];

        // Walk messages in order, assigning sequential non-overlapping time windows
        // starting from start_nanos. Absolute timestamps aren't stored in the
        // trajectory, so we reconstruct a plausible ordering from latency data.
        //
        // Message layout: assistant turns carry model_latency_ms + actions;
        // the following user/observation turn carries tool_latency_ms.
        // We keep a reference to the prior message to read actions from the
        // correct turn when building tool_call spans.
        let mut cursor = start_nanos;
        let mut prev_msg: Option<&crate::trajectory::MessageRecord> = None;
        for msg in &traj.messages {
            if let Some(latency_ms) = msg.extra.model_latency_ms {
                // harness_overhead_ms is measured before model.query starts (see
                // agent/default.rs: assistant_harness_ms captured before model call).
                // Advance the cursor over that pre-call gap first, then place the span.
                cursor += msg.extra.harness_overhead_ms.unwrap_or(0) * 1_000_000;
                let mc_start = cursor;
                let mc_end = cursor + latency_ms * 1_000_000;
                cursor = mc_end;

                let (prompt_tokens, completion_tokens, cache_read, cache_creation) =
                    if let Some(resp_val) = &msg.extra.response {
                        parse_usage_from_response(resp_val)
                    } else {
                        (0, 0, 0, 0)
                    };

                // Use the per-turn responding model (e.g. after a fallback) when
                // available; fall back to the configured primary model from info.
                let model = msg
                    .extra
                    .sampling
                    .as_ref()
                    .map(|s| s.model.clone())
                    .or_else(|| traj.info.model_name.clone())
                    .unwrap_or_default();
                let finish_reason = extract_finish_reason(msg.extra.response.as_ref());

                model_calls.push(ModelCallSpanData {
                    model,
                    prompt_tokens,
                    completion_tokens,
                    cache_read_tokens: cache_read,
                    cache_creation_tokens: cache_creation,
                    latency_ms,
                    finish_reason,
                    start_nanos: mc_start,
                    end_nanos: mc_end,
                });
            }

            if let Some(tool_latency_ms) = msg.extra.tool_latency_ms {
                // Same ordering: harness overhead (pre-tool hooks, policy checks) runs
                // before the tool, so advance the cursor before placing the span.
                cursor += msg.extra.harness_overhead_ms.unwrap_or(0) * 1_000_000;
                let tc_start = cursor;
                let tc_end = cursor + tool_latency_ms * 1_000_000;
                cursor = tc_end;

                // The `actions` list is written on the preceding assistant turn.
                // Fall back to the current message's actions if prev is missing.
                let tool_name = prev_msg
                    .and_then(|pm| pm.extra.actions.as_ref())
                    .or(msg.extra.actions.as_ref())
                    .and_then(|a| a.first())
                    .map_or_else(|| "bash".into(), |s| extract_tool_name(s));

                let (exit_code, obs_bytes, truncated) = extract_run_result(&msg.extra);

                tool_calls.push(ToolCallSpanData {
                    tool_name,
                    exit_code,
                    observation_bytes: obs_bytes,
                    duration_ms: tool_latency_ms,
                    truncated,
                    start_nanos: tc_start,
                    end_nanos: tc_end,
                });
            }

            prev_msg = Some(msg);
        }

        InstanceSpanData {
            trace_id: trace_id.to_owned(),
            sweep_span_id: sweep_span_id.to_owned(),
            instance_id: instance_id.to_owned(),
            repo: repo.to_owned(),
            outcome,
            cost_usd,
            step_count,
            final_patch_bytes,
            start_nanos,
            end_nanos,
            model_calls,
            tool_calls,
        }
    }

    fn parse_usage_from_response(resp: &serde_json::Value) -> (u64, u64, u64, u64) {
        let usage = resp.get("usage").unwrap_or(&serde_json::Value::Null);
        let prompt = usage
            .get("input_tokens")
            .or_else(|| usage.get("prompt_tokens"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let completion = usage
            .get("output_tokens")
            .or_else(|| usage.get("completion_tokens"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        // Anthropic: cache_read_input_tokens; OpenAI: prompt_tokens_details.cached_tokens
        let cache_read = usage
            .get("cache_read_tokens")
            .or_else(|| usage.get("cache_read_input_tokens"))
            .or_else(|| {
                usage
                    .get("prompt_tokens_details")
                    .and_then(|d| d.get("cached_tokens"))
            })
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let cache_creation = usage
            .get("cache_creation_tokens")
            .or_else(|| usage.get("cache_creation_input_tokens"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        (prompt, completion, cache_read, cache_creation)
    }

    fn extract_finish_reason(resp: Option<&serde_json::Value>) -> String {
        resp.and_then(|v| {
            v.get("choices")
                .and_then(|c| c.get(0))
                .and_then(|c| c.get("finish_reason"))
                .and_then(serde_json::Value::as_str)
                .or_else(|| v.get("stop_reason").and_then(serde_json::Value::as_str))
        })
        .unwrap_or("end_turn")
        .to_owned()
    }

    fn extract_tool_name(action: &str) -> String {
        // action_label() stores MCP tools as "name:{...json...}" (no whitespace before ':')
        // and bash commands as the raw shell command.
        if let Some((name, _)) = action.split_once(':') {
            if !name.contains(char::is_whitespace) && !name.is_empty() {
                return name.to_owned();
            }
        }
        "bash".to_owned()
    }

    fn extract_run_result(extra: &crate::model::MessageExtra) -> (i32, u64, bool) {
        if let Some(run_result) = extra.other.get("run_result") {
            let exit_code = i32::try_from(
                run_result
                    .get("exit_code")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(0),
            )
            .unwrap_or(0);
            let stdout_bytes = run_result
                .get("stdout")
                .and_then(serde_json::Value::as_str)
                .map_or(0, str::len) as u64;
            let stderr_bytes = run_result
                .get("stderr")
                .and_then(serde_json::Value::as_str)
                .map_or(0, str::len) as u64;
            // Truncation is stored as "observation_truncated" on the MessageExtra,
            // not inside run_result.
            let truncated = extra
                .other
                .get("observation_truncated")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            return (exit_code, stdout_bytes + stderr_bytes, truncated);
        }
        (0, 0, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK
            .get_or_init(Mutex::default)
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn trace_id_is_32_hex_chars() {
        let tid = new_trace_id("django__django__1234", "sweep-abc");
        assert_eq!(tid.len(), 32);
        assert!(tid.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn span_id_is_16_hex_chars() {
        let tid = new_trace_id("repo__issue__1", "sweep-xyz");
        let sid = new_span_id("sweep_span", &tid);
        assert_eq!(sid.len(), 16);
        assert!(sid.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn different_instances_get_different_trace_ids() {
        let t1 = new_trace_id("inst_a", "sweep1");
        let t2 = new_trace_id("inst_b", "sweep1");
        // Highly unlikely to collide; if they do the hash is broken.
        assert_ne!(t1, t2);
    }

    #[test]
    fn noop_tracer_is_inactive() {
        let t = Tracer::noop();
        assert!(!t.is_active());
    }

    #[test]
    fn resolve_endpoint_prefers_cli_flag() {
        let _guard = env_lock();
        let ep = temp_env::with_vars(
            [
                ("OTEL_EXPORTER_OTLP_ENDPOINT", Some("http://env-host:4318")),
                ("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", None),
            ],
            || resolve_endpoint(Some("http://cli-host:4318")),
        );
        // CLI flag is a base URL; /v1/traces is appended.
        assert_eq!(ep.as_deref(), Some("http://cli-host:4318/v1/traces"));
    }

    #[test]
    fn resolve_endpoint_falls_back_to_env_var() {
        let _guard = env_lock();
        let ep = temp_env::with_vars(
            [
                ("OTEL_EXPORTER_OTLP_ENDPOINT", Some("http://env-host:4318")),
                ("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", None),
            ],
            || resolve_endpoint(None),
        );
        // Generic env var is a base URL; /v1/traces is appended.
        assert_eq!(ep.as_deref(), Some("http://env-host:4318/v1/traces"));
    }

    #[test]
    fn resolve_endpoint_traces_env_var_used_as_full_url() {
        let _guard = env_lock();
        let ep = temp_env::with_vars(
            [
                ("OTEL_EXPORTER_OTLP_ENDPOINT", None),
                (
                    "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
                    Some("http://traces-host:4318/v1/traces"),
                ),
            ],
            || resolve_endpoint(None),
        );
        // Trace-specific env var is a full URL; used as-is without appending.
        assert_eq!(ep.as_deref(), Some("http://traces-host:4318/v1/traces"));
    }

    #[test]
    fn resolve_endpoint_returns_none_when_unset() {
        let _guard = env_lock();
        let ep = temp_env::with_vars(
            [
                ("OTEL_EXPORTER_OTLP_ENDPOINT", None::<String>),
                ("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", None::<String>),
            ],
            || resolve_endpoint(None),
        );
        assert!(ep.is_none());
    }

    #[test]
    fn build_otlp_json_is_valid_json() {
        let sweep = SweepSpanData {
            sweep_id: "test-sweep".into(),
            dataset: "lite".into(),
            model: "claude-opus".into(),
            instance_count: 1,
            resolved_count: 0,
            total_cost_usd: 0.01,
            harness_version: "0.1.0".into(),
            git_sha: None,
            start_nanos: 1_000_000_000,
            end_nanos: 2_000_000_000,
        };
        let json = build_otlp_json(&sweep, &[]);
        assert!(serde_json::from_str::<serde_json::Value>(&json).is_ok());
    }
}
