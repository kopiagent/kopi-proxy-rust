use crate::config::Config;
use crate::db;
use crate::AppState;
use axum::{
    body::Body,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use futures::StreamExt;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Instant;
use tracing::{error, info, warn};

/// Maximum number of upstream attempts per request.
const MAX_FAILOVER_ATTEMPTS: usize = 4;

/// Circuit breaker: how long to skip an upstream after it fails (429/5xx/conn error).
pub const CIRCUIT_BREAKER_COOLDOWN_SECS: u64 = 60;

/// Check if an upstream is currently in circuit-breaker cooldown.
async fn is_in_cooldown(state: &AppState, upstream_name: &str) -> bool {
    let health = state.upstream_health.read().await;
    if let Some(until) = health.get(upstream_name) {
        return std::time::Instant::now() < *until;
    }
    false
}

/// Mark an upstream as unhealthy — skip it for CIRCUIT_BREAKER_COOLDOWN_SECS.
async fn mark_unhealthy(state: &AppState, upstream_name: &str) {
    let mut health = state.upstream_health.write().await;
    let until = std::time::Instant::now()
        + std::time::Duration::from_secs(CIRCUIT_BREAKER_COOLDOWN_SECS);
    health.insert(upstream_name.to_string(), until);
    // Purge expired entries to keep memory bounded
    let now = std::time::Instant::now();
    health.retain(|_, until| *until > now);
    warn!(
        "CIRCUIT_BREAKER upstream={} cooldown={}s — will skip until recovered",
        upstream_name, CIRCUIT_BREAKER_COOLDOWN_SECS
    );
}

/// Result of a failover execution.
#[derive(Debug)]
pub struct FailoverError {
    pub message: String,
}

/// Build the failover attempt list: [primary, ...fallbacks excluding primary's upstream].
pub fn build_attempt_list(
    config: &Config,
    upstream_name: &str,
    upstream_model: &str,
) -> Vec<(String, String)> {
    let mut attempts = Vec::new();
    attempts.push((upstream_name.to_string(), upstream_model.to_string()));
    for (fb_upstream, fb_model) in &config.global_fallback {
        if *fb_upstream != upstream_name {
            attempts.push((fb_upstream.clone(), fb_model.clone()));
        }
    }
    attempts.truncate(MAX_FAILOVER_ATTEMPTS);
    attempts
}

/// Pick the optimal model based on payload analysis.
pub fn smart_select_model(state: &AppState, payload: &Value) -> String {
    let hint = Config::analyze_payload(payload);
    let selected = state.config.smart_select_model(&hint);

    info!(
        "SMART_ROUTE selected={} has_images={} has_code={} needs_reasoning={} estimated_tokens={}",
        selected, hint.has_images, hint.has_code, hint.needs_reasoning, hint.estimated_tokens
    );

    selected
}

/// Main entry point: try the request across primary + fallback upstreams.
/// Returns Ok(response) on first success, or Err(FailoverError) if all fail.
///
/// For streaming: tries primary first; if the HTTP request ITSELF fails
/// (connection error, 5xx, 429) before SSE starts, it tries the next upstream.
/// Once streaming has started, mid-stream errors are unrecoverable.
///
/// For non-streaming: full failover loop with token usage tracking.
pub async fn try_requests(
    state: Arc<AppState>,
    req_model: String,
    body: Bytes,
    headers: HeaderMap,
    request_id: String,
    attempts: Vec<(String, String)>,
    is_stream: bool,
    client_id: i64,
    start: Instant,
) -> Result<Response<Body>, FailoverError> {
    let total_attempts = attempts.len();
    let mut last_err_msg = String::new();

    for (attempt_idx, (upstream_name, upstream_model)) in attempts.iter().enumerate() {
        let attempt_num = attempt_idx + 1;
        let is_first = attempt_idx == 0;

        // Get upstream config
        let upstream = match state.config.upstreams.get(upstream_name) {
            Some(u) => u.clone(),
            None => {
                last_err_msg = format!("Unknown upstream '{}'", upstream_name);
                warn!("FAILOVER attempt={}/{} {} — skip", attempt_num, total_attempts, last_err_msg);
                continue;
            }
        };

        // Circuit breaker: skip upstreams that recently failed (429/5xx/conn error).
        // Exception: if this is the LAST option, still try it (better than returning 503).
        let remaining = attempts.len() - attempt_idx - 1;
        if remaining > 0 && is_in_cooldown(&state, upstream_name).await {
            info!(
                "CIRCUIT_BREAKER_SKIP upstream={} attempt={}/{} — in cooldown, trying next",
                upstream_name, attempt_num, total_attempts
            );
            continue;
        }

        // Remap model name in request body
        let mut remapped: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        let user_id = remapped
            .get("user")
            .and_then(|u| u.as_str())
            .unwrap_or("")
            .to_string(); // clone to avoid borrow conflict

        remapped["model"] = json!(upstream_model);
        if !user_id.is_empty() {
            let suffix = format!("{}_{}", upstream_model, user_id.replace('-', ""));
            remapped["model"] = json!(suffix);
        }

        // kopi_mcp upstream (8933) does NOT support SSE streaming — force stream=false
        // The proxy will still return SSE to the client via the non-streaming path.
        let effective_stream = if upstream_name == "kopi_mcp" {
            false
        } else {
            is_stream
        };
        if upstream_name == "kopi_mcp" {
            remapped["stream"] = json!(false);
        }

        let remapped_body = serde_json::to_vec(&remapped).unwrap_or_else(|_| body.to_vec());

        if !is_first {
            info!(
                "FAILOVER_ATTEMPT model={} attempt={}/{} upstream={} model_native={} req_id={}",
                req_model, attempt_num, total_attempts, upstream_name, upstream_model, request_id
            );
        }

        // Build HTTP request — image models need longer timeout (GPT Image 2 takes 200s+)
        let is_image_model = upstream_model.contains("image") || upstream_model.contains("gpt-5.4");
        let timeout_secs = if effective_stream { 300 } else if is_image_model { 420 } else { 120 };
        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .build()
            .map_err(|e| FailoverError {
                message: format!("HTTP client: {e}"),
            })?;

        let upstream_url = format!(
            "{}/chat/completions",
            upstream.base_url.trim_end_matches('/')
        );

        let mut req_builder = http_client
            .post(&upstream_url)
            .header("Authorization", format!("Bearer {}", upstream.api_key))
            .header("Content-Type", "application/json");

        // Forward relevant headers (NOT accept-encoding — let reqwest handle
        // compression transparently: sends its own Accept-Encoding, upstream
        // gzips, reqwest auto-decompresses, proxy gets clean text).
        for (k, v) in &headers {
            let k_str = k.as_str();
            if matches!(
                k_str,
                "user-agent"
                    | "accept"
                    | "x-stainless-arch"
                    | "x-stainless-lang"
                    | "x-stainless-package-version"
                    | "x-stainless-os"
                    | "x-stainless-runtime"
                    | "x-stainless-runtime-version"
            ) {
                if let Ok(val) = v.to_str() {
                    req_builder = req_builder.header(k.as_str(), val);
                }
            }
        }

        req_builder = req_builder.body(remapped_body);

        // Send request
        let upstream_resp = match req_builder.send().await {
            Ok(r) => r,
            Err(e) => {
                let msg = format!("{e}");
                last_err_msg = msg.clone();
                mark_unhealthy(&state, upstream_name).await;
                warn!("FAILOVER attempt={}/{} upstream={} error={} — trying next", attempt_num, total_attempts, upstream_name, msg);
                continue;
            }
        };

        let status = upstream_resp.status();

        // Check if we should failover based on response status
        let should_failover = match status.as_u16() {
            s if s >= 500 && s < 600 => true,  // 5xx
            429 => true,                         // rate limit
            s if s >= 400 && s < 500 => false,   // 4xx (except 429) — won't help
            _ => false,                          // success or other
        };

        if !status.is_success() && should_failover {
            let error_body = upstream_resp.text().await.unwrap_or_default();
            last_err_msg = format!("status={} {}",
                status,
                &error_body[..error_body.len().min(200)]
            );
            // Only 5xx and connection errors trigger circuit breaker.
            // 429 = transient rate limit — skip this attempt but DON'T mark unhealthy
            // (the upstream is fine, just temporarily busy).
            if status.as_u16() != 429 {
                mark_unhealthy(&state, upstream_name).await;
            }
            warn!(
                "FAILOVER attempt={}/{} upstream={} status={} — trying next",
                attempt_num, total_attempts, upstream_name, status
            );
            continue;
        }

        if !status.is_success() && !should_failover {
            // Non-failover 4xx — return as-is
            let error_body = upstream_resp.text().await.unwrap_or_default();
            let resp = Response::builder()
                .status(status)
                .header("Content-Type", "application/json")
                .header("X-Request-Id", &request_id)
                .header("X-KOPI-Proxy", "rust-v2")
                .body(Body::from(error_body))
                .unwrap();
            return Ok(resp);
        }

        // ─── SUCCESS — handle streaming vs non-streaming ───
        let is_fallback = !is_first;
        let latency_ms = start.elapsed().as_millis() as i64;

        if effective_stream {
            // Streaming: forward SSE stream with line buffering
            // TCP can split SSE lines across chunks — buffer incomplete lines
            let stream = upstream_resp.bytes_stream();
            let resp_model = req_model.clone();
            let upstream_name_clone = upstream_name.clone();

            let line_buffer = std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));
            let mapped = stream.then({
                let line_buffer = line_buffer.clone();
                move |chunk_result| {
                    let resp_model = resp_model.clone();
                    let upstream_name_clone = upstream_name_clone.clone();
                    let line_buffer = line_buffer.clone();
                    async move {
                        let chunk = match chunk_result {
                            Ok(c) => c,
                            Err(e) => {
                                error!("STREAM_ERROR model={resp_model} upstream={upstream_name_clone}: {e}");
                                return Err(std::io::Error::new(std::io::ErrorKind::Other, format!("{e}")));
                            }
                        };

                        let mut buf = line_buffer.lock().await;
                        buf.push_str(&String::from_utf8_lossy(&chunk));

                        let mut rewritten = String::with_capacity(chunk.len());
                        let mut remaining = buf.split_off(0); // take all

                        while let Some(newline_pos) = remaining.find('\n') {
                            let line = remaining[..newline_pos].to_string();
                            remaining = remaining[newline_pos + 1..].to_string();

                            if let Some(json_str) = line.strip_prefix("data: ") {
                                if json_str.trim() == "[DONE]" {
                                    rewritten.push_str("data: [DONE]\n");
                                } else if let Ok(mut val) = serde_json::from_str::<serde_json::Value>(json_str) {
                                    if let Some(obj) = val.as_object_mut() {
                                        obj.insert("model".to_string(), serde_json::Value::String(resp_model.clone()));
                                        obj.remove("provider");
                                    }
                                    rewritten.push_str("data: ");
                                    rewritten.push_str(&serde_json::to_string(&val).unwrap_or(json_str.to_string()));
                                    rewritten.push('\n');
                                } else {
                                    rewritten.push_str(&line);
                                    rewritten.push('\n');
                                }
                            } else {
                                rewritten.push_str(&line);
                                rewritten.push('\n');
                            }
                        }

                        // Save incomplete trailing line for next chunk
                        *buf = remaining;

                        Ok(bytes::Bytes::from(rewritten))
                    }
                }
            });

            let mut resp_builder = Response::builder()
                .header("Content-Type", "text/event-stream")
                .header("Cache-Control", "no-cache")
                .header("Connection", "keep-alive")
                .header("X-Request-Id", &request_id)
                .header("X-KOPI-Proxy", "rust-v2")
                .header("X-Kopi-Route", &req_model);

            if is_fallback {
                resp_builder = resp_builder.header("X-KOPI-Failover", upstream_name.as_str());
            }

            // Log streaming usage estimate (1 attempt = 1 billable call)
            if client_id > 0 {
                let tokens_in = body
                    .as_ref()
                    .iter()
                    .filter(|&&b| b == b' ')
                    .count() as i64;
                let _ = db::deduct_tokens(
                    &state.db, client_id, &req_model, upstream_name,
                    tokens_in, 0, latency_ms,
                );
            }

            return Ok(resp_builder.body(Body::from_stream(mapped)).unwrap());
        } else {
            // Non-streaming: read full body
            let resp_text = match upstream_resp.text().await {
                Ok(t) => t,
                Err(e) => {
                    let msg = format!("Failed to read response body: {e}");
                    last_err_msg = msg.clone();
                    warn!("FAILOVER attempt={}/{} upstream={} read_error={} — trying next", attempt_num, total_attempts, upstream_name, msg);
                    continue;
                }
            };

            // Count tokens for billing
            let tokens_in = if client_id > 0 {
                count_messages(&body)
            } else {
                0
            };
            let tokens_out = if client_id > 0 {
                serde_json::from_str::<Value>(&resp_text)
                    .ok()
                    .and_then(|v| {
                        v.get("usage")
                            .and_then(|u| u.get("completion_tokens"))
                            .and_then(|c| c.as_i64())
                    })
                    .unwrap_or(0)
            } else {
                0
            };

            // Log token usage
            if client_id > 0 {
                if let Err(e) = db::deduct_tokens(
                    &state.db,
                    client_id,
                    &req_model,
                    upstream_name,
                    tokens_in,
                    tokens_out,
                    latency_ms,
                ) {
                    warn!("Failed to log token usage on {upstream_name}: {e}");
                }
            }

            let total_latency = start.elapsed().as_millis() as i64;
            let attempt_label = if is_fallback {
                format!(" (fallback #{attempt_num})")
            } else {
                String::new()
            };
            info!(
                "SUCCESS model={} upstream={}{} tokens_in={} tokens_out={} latency={}ms req_id={}",
                req_model,
                upstream_name,
                attempt_label,
                tokens_in,
                tokens_out,
                total_latency,
                request_id
            );

            // Build response
            let mut resp_builder = Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", "application/json")
                .header("X-Request-Id", &request_id)
                .header("X-KOPI-Proxy", "rust-v2")
                .header("X-Kopi-Route", &req_model);

            if is_fallback {
                resp_builder = resp_builder.header("X-KOPI-Failover", upstream_name.as_str());
            }

            // Rewrite model field to KOPI brand name (hide upstream identity)
            let rewritten = if let Ok(mut json_val) = serde_json::from_str::<serde_json::Value>(&resp_text) {
                if let Some(obj) = json_val.as_object_mut() {
                    obj.insert("model".to_string(), serde_json::Value::String(req_model.clone()));
                    obj.remove("provider");
                }
                serde_json::to_string(&json_val).unwrap_or(resp_text.clone())
            } else {
                resp_text.clone()
            };

            // If client requested streaming but upstream doesn't support it (kopi_mcp),
            // convert the non-streaming JSON response to SSE format
            if is_stream && !effective_stream {
                let sse_body = format!("data: {}\n\ndata: [DONE]\n\n", rewritten);
                let sse_resp = Response::builder()
                    .status(StatusCode::OK)
                    .header("Content-Type", "text/event-stream")
                    .header("Cache-Control", "no-cache")
                    .header("Connection", "keep-alive")
                    .header("X-Request-Id", &request_id)
                    .header("X-KOPI-Proxy", "rust-v2")
                    .header("X-Kopi-Route", &req_model);
                let mut sse_resp = sse_resp;
                if is_fallback {
                    sse_resp = sse_resp.header("X-KOPI-Failover", upstream_name.as_str());
                }
                return Ok(sse_resp.body(Body::from(sse_body)).unwrap());
            }

            return Ok(resp_builder.body(Body::from(rewritten)).unwrap());
        }
    }

    // All attempts exhausted
    let total_latency = start.elapsed().as_millis() as i64;
    error!(
        "ALL_FAILED model={} last_error={} latency={}ms req_id={}",
        req_model, last_err_msg, total_latency, request_id
    );

    Err(FailoverError {
        message: format!("All upstreams unavailable. Last error: {last_err_msg}"),
    })
}

/// Quick token estimate from raw JSON body.
fn count_messages(body: &Bytes) -> i64 {
    // Parse and count chars in all message content
    if let Ok(v) = serde_json::from_slice::<Value>(body) {
        if let Some(messages) = v.get("messages").and_then(|m| m.as_array()) {
            let text: String = messages
                .iter()
                .filter_map(|m| m.get("content"))
                .filter_map(|c| c.as_str())
                .collect();
            return (text.len() as f64 / 2.5).ceil() as i64;
        }
    }
    0
}
