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

        let url = format!("{}/v1/traces", inner.endpoint.trim_end_matches('/'));
        let body = build_otlp_json(sweep, instances);

        let result = inner
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await;

        match result {
            Ok(resp) if resp.status().is_success() => {}
            Ok(resp) => {
                let status = resp.status();
                tracing::warn!(
                    endpoint = %url,
                    http_status = %status,
                    "OTLP export failed: non-2xx response"
                );
                inner.dropped.fetch_add(
                    1 + instances.len() as u64 * 3, // sweep + instances + child spans est.
                    Ordering::Relaxed,
                );
            }
            Err(e) => {
                tracing::warn!(
                    endpoint = %url,
                    error = %e,
                    "OTLP export failed: network error"
                );
                inner
                    .dropped
                    .fetch_add(1 + instances.len() as u64 * 3, Ordering::Relaxed);
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

fn span_json(
    trace_id: &str,
    span_id: &str,
    parent_span_id: Option<&str>,
    name: &str,
    start_nanos: u64,
    end_nanos: u64,
    attributes: &[serde_json::Value],
) -> serde_json::Value {
    let mut span = serde_json::json!({
        "traceId": trace_id,
        "spanId": span_id,
        "name": name,
        "kind": 1,  // SPAN_KIND_INTERNAL
        "startTimeUnixNano": start_nanos.to_string(),
        "endTimeUnixNano": end_nanos.to_string(),
        "attributes": attributes,
        "status": { "code": 1 }  // STATUS_CODE_OK
    });
    if let Some(pid) = parent_span_id {
        span["parentSpanId"] = serde_json::json!(pid);
    }
    span
}

fn infer_gen_ai_system(model: &str) -> &'static str {
    if model.starts_with("claude") {
        "anthropic"
    } else if model.starts_with("gpt") || model.starts_with("o1") || model.starts_with("o3") {
        "openai"
    } else if model.starts_with("gemini") {
        "google_vertexai"
    } else {
        "unknown"
    }
}

fn u64_to_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn build_instance_spans(sweep_id: &str, inst: &InstanceSpanData) -> Vec<serde_json::Value> {
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
    // Each instance is its own independent OTel trace (different traceId from the
    // sweep span). OTel requires all spans in a parent-child relationship to share
    // the same traceId, so we keep instance spans as independent root spans and
    // reference the sweep via an attribute rather than a span link.
    spans.push(span_json(
        &inst.trace_id,
        &inst_span_id,
        None, // root of its own trace — no cross-trace parent link
        "instance",
        inst.start_nanos,
        inst.end_nanos,
        &inst_attrs,
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
    )];

    for inst in instances {
        spans.extend(build_instance_spans(&sweep.sweep_id, inst));
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

/// Returns the active OTLP endpoint, preferring the CLI flag over the
/// `OTEL_EXPORTER_OTLP_ENDPOINT` environment variable.
pub fn resolve_endpoint(cli_flag: Option<&str>) -> Option<String> {
    if let Some(ep) = cli_flag {
        return Some(ep.to_owned());
    }
    std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .ok()
        .filter(|s| !s.is_empty())
}

// ---------------------------------------------------------------------------
// Build instance span data from a completed trajectory
// ---------------------------------------------------------------------------

pub use build::instance_span_data_from_trajectory;

pub(crate) mod build {
    use super::{InstanceSpanData, ModelCallSpanData, ToolCallSpanData, now_unix_nanos};
    use crate::trajectory::Trajectory;

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
        let end_nanos = now_unix_nanos();

        let mut model_calls = vec![];
        let mut tool_calls = vec![];

        // Walk messages in order, assigning sequential non-overlapping time windows
        // starting from start_nanos. Absolute timestamps aren't stored in the
        // trajectory, so we reconstruct a plausible ordering from latency data.
        let mut cursor = start_nanos;
        for msg in &traj.messages {
            if let Some(latency_ms) = msg.extra.model_latency_ms {
                let mc_start = cursor;
                let mc_end = cursor + latency_ms * 1_000_000;
                cursor = mc_end;

                let (prompt_tokens, completion_tokens, cache_read, cache_creation) =
                    if let Some(resp_val) = &msg.extra.response {
                        parse_usage_from_response(resp_val)
                    } else {
                        (0, 0, 0, 0)
                    };

                let model = traj.info.model_name.clone().unwrap_or_default();
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
                let tc_start = cursor;
                let tc_end = cursor + tool_latency_ms * 1_000_000;
                cursor = tc_end;

                // Extract tool name from actions list (first action = command type).
                let tool_name = msg
                    .extra
                    .actions
                    .as_ref()
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
        let cache_read = usage
            .get("cache_read_tokens")
            .or_else(|| usage.get("cache_read_input_tokens"))
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
        // Actions are shell commands or tool invocations. Return just the first word.
        action
            .split_whitespace()
            .next()
            .unwrap_or("bash")
            .to_owned()
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
            let truncated = run_result
                .get("truncated")
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
        // SAFETY: unit test, single-threaded.
        unsafe {
            std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", "http://env-host:4318");
        }
        let ep = resolve_endpoint(Some("http://cli-host:4318"));
        unsafe {
            std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT");
        }
        assert_eq!(ep.as_deref(), Some("http://cli-host:4318"));
    }

    #[test]
    fn resolve_endpoint_falls_back_to_env_var() {
        // SAFETY: unit test, single-threaded.
        unsafe {
            std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", "http://env-host:4318");
        }
        let ep = resolve_endpoint(None);
        unsafe {
            std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT");
        }
        assert_eq!(ep.as_deref(), Some("http://env-host:4318"));
    }

    #[test]
    fn resolve_endpoint_returns_none_when_unset() {
        // SAFETY: unit test, single-threaded.
        unsafe {
            std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT");
        }
        let ep = resolve_endpoint(None);
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
