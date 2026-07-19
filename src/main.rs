mod billing;
mod config;
mod db;
mod routing;

use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use bytes::Bytes;
use config::Config;
use db::{check_quota, deduct_tokens, verify_key};
use futures::StreamExt;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::RwLock;
use tower_http::cors::CorsLayer;
use tracing::{error, info, warn};

/// In-memory rate limiter state.
#[derive(Default)]
struct RateLimiter {
    /// Map of client_id -> list of request timestamps (seconds)
    requests: std::collections::HashMap<i64, Vec<f64>>,
}

impl RateLimiter {
    fn check(&mut self, client_id: i64, max_rpm: u32) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        let entries = self.requests.entry(client_id).or_default();
        entries.retain(|t| now - t < 60.0);
        if entries.len() >= max_rpm as usize {
            return false;
        }
        entries.push(now);
        true
    }
}

/// Shared application state.
pub struct AppState {
    pub config: Config,
    pub db: Mutex<Connection>,
    rate_limiter: RwLock<RateLimiter>,
    /// Circuit breaker: upstream_name → cooldown_until (Instant).
    /// Upstreams in cooldown are skipped during failover to avoid wasting time.
    pub upstream_health: RwLock<HashMap<String, std::time::Instant>>,
}

#[derive(Deserialize)]
struct ChatRequest {
    model: Option<String>,
    messages: Option<Vec<Value>>,
    stream: Option<bool>,
    max_tokens: Option<i64>,
    temperature: Option<f64>,
    #[serde(flatten)]
    extra: std::collections::HashMap<String, Value>,
}

#[derive(Serialize)]
struct ModelInfo {
    id: String,
    object: String,
    #[serde(rename = "owned_by")]
    owned_by: String,
}

#[derive(Serialize)]
struct ModelsResponse {
    object: String,
    data: Vec<ModelInfo>,
}

/// Health check endpoint.
async fn health() -> &'static str {
    "OK"
}

/// GET /auth/verify — internal endpoint for nginx auth_request.
/// Checks Authorization: Bearer <key> against kopi.db clients table.
async fn auth_verify(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> StatusCode {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .strip_prefix("Bearer ")
        .unwrap_or("");

    if auth.is_empty() {
        return StatusCode::UNAUTHORIZED;
    }

    match verify_key(&state.db, auth) {
        Ok(_) => StatusCode::OK,
        Err(_) => StatusCode::UNAUTHORIZED,
    }
}

/// GET /v1/pricing — model pricing & $10 tier info. Includes client quota when authenticated.
async fn pricing(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Json<serde_json::Value> {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .strip_prefix("Bearer ")
        .unwrap_or("");

    let mut result = serde_json::json!({
        "currency": "USD",
        "unit": "per 1M tokens",
        "models": {
            "kopi-gpt5":    { "tier": 1, "input": 5.00,  "output": 15.00, "context": "1.05M", "desc": "GPT-5.5 · strongest reasoning" },
            "kopi-opus":    { "tier": 1, "input": 7.00,  "output": 35.00, "context": "1M",    "desc": "Claude Opus 4.6 · deep analysis" },
            "kopi-o-pro":   { "tier": 1, "input": 3.00,  "output": 10.00, "context": "1M",    "desc": "GLM-5.2 · flagship reasoning" },
            "kopi-o":       { "tier": 2, "input": 1.50,  "output": 5.00,  "context": "262K",  "desc": "MiMo v2.5 Pro · daily driver" },
            "kopi-qwen":    { "tier": 2, "input": 1.25,  "output": 5.00,  "context": "1M",    "desc": "Qwen 3.7 Max · long context" },
            "kopi-kimi":    { "tier": 2, "input": 0.74,  "output": 2.50,  "context": "262K",  "desc": "Kimi K2.7 · code specialist" },
            "kopi-gau":     { "tier": 2, "input": 1.50,  "output": 5.00,  "context": "262K",  "desc": "MiMo v2.5 Pro · versatile" },
            "kopi-gemini":  { "tier": 3, "input": 1.50,  "output": 5.00,  "context": "1M",    "desc": "Gemini 3.5 Flash · fast" },
            "kopi-flash":   { "tier": 3, "input": 0.50,  "output": 1.50,  "context": "128K",  "desc": "DeepSeek V4 Pro · ultra fast" },
            "kopi-o-flash": { "tier": 3, "input": 0.80,  "output": 2.00,  "context": "262K",  "desc": "MiMo v2.5 · fast + capable" }
        },
        "estimates_with_10usd": {
            "kopi-flash":   "~20M input tokens",
            "kopi-o":       "~6.6M input tokens",
            "kopi-gpt5":    "~2M input tokens",
            "note": "actual usage varies by input/output ratio"
        }
    });

    if !auth.is_empty() {
        if let Ok(client) = verify_key(&state.db, auth) {
            let remaining = client.quota_limit - client.quota_used;
            result["account"] = serde_json::json!({
                "client": client.name,
                "key_prefix": client.key_prefix,
                "quota_limit": client.quota_limit,
                "quota_used": client.quota_used,
                "quota_remaining": remaining,
                "unit": "tokens"
            });
        }
    }

    Json(result)
}

/// GET /v2/docs — API documentation. Includes client quota when authenticated.
async fn v2_docs(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Json<serde_json::Value> {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .strip_prefix("Bearer ")
        .unwrap_or("");

    let mut result = serde_json::json!({
        "name": "KOPI AI API",
        "version": "2.0",
        "base_url": "https://kopiaiagent.com/v2",
        "auth": {
            "type": "Bearer",
            "header": "Authorization: Bearer <your_api_key>",
            "get_key": "contact@kopiaiagent.com"
        },
        "endpoints": {
            "POST /v1/chat/completions": {
                "desc": "Chat completion (OpenAI compatible)",
                "params": {
                    "model": "required — model name (see /v1/models)",
                    "messages": "required — array of {role, content}",
                    "max_tokens": "optional — max output tokens",
                    "stream": "optional — true for SSE streaming",
                    "temperature": "optional — 0~2"
                },
                "example": {
                    "model": "kopi-o",
                    "messages": [{"role": "user", "content": "Hello"}],
                    "max_tokens": 500,
                    "stream": false
                }
            },
            "GET /v1/models": "List available models",
            "GET /v1/pricing": "Model pricing & $10 quota breakdown",
            "GET /v1/balance": "Check your token balance (requires auth)"
        },
        "models": {
            "tier_1_flagship": {
                "kopi-gpt5":  "GPT-5.5 · strongest reasoning · 1.05M ctx",
                "kopi-opus":  "Claude Opus 4.6 · deep analysis · 1M ctx",
                "kopi-o-pro": "GLM-5.2 · flagship reasoning · 1M ctx"
            },
            "tier_2_standard": {
                "kopi-o":    "MiMo v2.5 Pro · daily driver · 262K ctx",
                "kopi-qwen": "Qwen 3.7 Max · long context · 1M ctx",
                "kopi-kimi": "Kimi K2.7 · code specialist · 262K ctx",
                "kopi-gau":  "MiMo v2.5 Pro · versatile · 262K ctx"
            },
            "tier_3_fast": {
                "kopi-gemini":  "Gemini 3.5 Flash · fast · 1M ctx",
                "kopi-flash":   "DeepSeek V4 Pro · ultra fast · 128K ctx",
                "kopi-o-flash": "MiMo v2.5 · fast + capable · 262K ctx"
            }
        },
        "pricing": {
            "unit": "per 1M tokens",
            "default_quota": "$10 USD",
            "tiers": {
                "tier_1": { "input": "$3~5", "output": "$10~15" },
                "tier_2": { "input": "$0.74~1.5", "output": "$2.5~5" },
                "tier_3": { "input": "$0.5~1.5", "output": "$1.5~5" }
            }
        },
        "limits": {
            "rate_limit": "60 req/min",
            "max_context": "up to 1.05M tokens",
            "streaming": "SSE supported on all models"
        },
        "company": "Kopi Ai Agent Pte Ltd · Singapore"
    });

    if !auth.is_empty() {
        if let Ok(client) = verify_key(&state.db, auth) {
            let remaining = client.quota_limit - client.quota_used;
            result["account"] = serde_json::json!({
                "client": client.name,
                "key_prefix": client.key_prefix,
                "quota_limit": client.quota_limit,
                "quota_used": client.quota_used,
                "quota_remaining": remaining,
                "unit": "tokens"
            });
        }
    }

    Json(result)
}

/// GET /v1/models — list available models.
async fn list_models(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<ModelsResponse>, AppError> {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .strip_prefix("Bearer ")
        .unwrap_or("");

    let _client = verify_key(&state.db, auth).map_err(|e| {
        warn!("Auth failed: {e}");
        AppError::unauthorized(e)
    })?;

    let models: Vec<ModelInfo> = state
        .config
        .model_map
        .keys()
        .map(|id| ModelInfo {
            id: id.clone(),
            object: "model".into(),
            owned_by: "kopi-agent".into(),
        })
        .collect();

    Ok(Json(ModelsResponse {
        object: "list".into(),
        data: models,
    }))
}

/// POST /v1/chat/completions — the main proxy endpoint with smart routing + failover.
async fn chat_completions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response<Body>, AppError> {
    let start = Instant::now();
    let ip = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .or_else(|| headers.get("x-real-ip").and_then(|v| v.to_str().ok()))
        .unwrap_or("unknown")
        .to_string();

    let _ua = headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    // 1. Authenticate
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .strip_prefix("Bearer ")
        .unwrap_or("");

    let client = verify_key(&state.db, auth).map_err(|e| {
        warn!("Auth failed: {e}");
        AppError::unauthorized(e)
    })?;

    // 2. Rate limit check
    {
        let mut rl = state.rate_limiter.write().await;
        if !rl.check(client.id, state.config.rate_limit_rpm) {
            return Err(AppError::rate_limit());
        }
    }

    // 3. Parse request
    let payload: Value = serde_json::from_slice(&body).map_err(|e| {
        warn!("Invalid JSON body: {e}");
        AppError::bad_request(format!("Invalid JSON: {e}"))
    })?;

    let is_stream = payload
        .get("stream")
        .and_then(|s| s.as_bool())
        .unwrap_or(false);

    let request_id = format!("chatcmpl-{}", uuid::Uuid::new_v4().to_string()[..12].to_string());

    // ─── Quota Check ───
    {
        let estimated_cost = if let Some(msgs) = payload.get("messages").and_then(|m| m.as_array())
        {
            count_messages_tokens(msgs).max(1) + 100
        } else {
            150
        };
        match check_quota(&state.db, &client, estimated_cost) {
            Ok(true) => {} // enough quota, proceed
            Ok(false) => {
                warn!(
                    "QUOTA_EXCEEDED client={} used={} limit={}",
                    client.name, client.quota_used, client.quota_limit
                );
                return Err(AppError {
                    status: StatusCode::PAYMENT_REQUIRED,
                    message: format!(
                        "Insufficient token quota. Used {}/{} tokens. Estimated cost: {}.",
                        client.quota_used, client.quota_limit, estimated_cost
                    ),
                });
            }
            Err(e) => {
                warn!("Quota check error: {e} — allowing through");
            }
        }
    }

    // ─── Smart Routing ───
    // If model is "auto" or not specified, use smart selection
    let req_model_raw = payload
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("auto");

    let req_model = if req_model_raw == "auto" {
        let selected = routing::smart_select_model(&state, &payload);
        info!(
            "SMART_ROUTE client={} from=auto to={} ip={}",
            client.name, selected, ip
        );
        selected
    } else {
        req_model_raw.to_string()
    };

    // ─── Failover Setup ───
    let (upstream_name, upstream_model) = match state.config.model_map.get(&req_model) {
        Some(route) => (route.upstream.clone(), route.model.clone()),
        None => {
            // Unknown model — send as-is to default upstream (mimo3)
            ("mimo3".to_string(), req_model.clone())
        }
    };

    let attempts = routing::build_attempt_list(&state.config, &upstream_name, &upstream_model);

    info!(
        "PROXY client={} model={} upstream_primary={} stream={} ip={} attempts={}",
        client.name,
        req_model,
        upstream_name,
        is_stream,
        ip,
        attempts.len()
    );

    // ─── Execute with Failover ───
    // For streaming: try primary, failover only on request error (not mid-stream)
    // For non-streaming: full failover loop with token tracking
    let result = routing::try_requests(
        state.clone(),
        req_model.clone(),
        body,
        headers,
        request_id.clone(),
        attempts,
        is_stream,
        client.id,
        start,
    )
    .await;

    result.map_err(|e| {
        error!(
            "ALL_ATTEMPTS_FAILED model={} client={} ip={}",
            req_model, client.name, ip
        );
        AppError::service_unavailable(format!("All upstreams unavailable: {}", e.message))
    })
}

/// Rough token count for messages (chars / 2.5 for mixed Chinese/English).
fn count_messages_tokens(messages: &Vec<Value>) -> i64 {
    let text: String = messages
        .iter()
        .filter_map(|m| m.get("content"))
        .filter_map(|c| c.as_str())
        .collect();
    (text.len() as f64 / 2.5).ceil() as i64
}

// ── Error handling ──

struct AppError {
    status: StatusCode,
    message: String,
}

impl AppError {
    fn unauthorized(msg: String) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: msg,
        }
    }
    fn bad_request(msg: String) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: msg,
        }
    }
    fn rate_limit() -> Self {
        Self {
            status: StatusCode::TOO_MANY_REQUESTS,
            message: "Rate limit exceeded. Please slow down.".into(),
        }
    }
    fn internal(msg: String) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: msg,
        }
    }
    fn bad_gateway(msg: String) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            message: msg,
        }
    }
    fn service_unavailable(msg: String) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: msg,
        }
    }
    fn upstream_error(status: u16, body: String) -> Self {
        Self {
            status: StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
            message: body,
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response<Body> {
        let body = json!({
            "error": {
                "message": self.message,
                "type": "proxy_error",
                "code": self.status.as_u16(),
            }
        });
        let body_str = serde_json::to_string(&body).unwrap_or_default();
        Response::builder()
            .status(self.status)
            .header("Content-Type", "application/json")
            .body(Body::from(body_str))
            .unwrap()
    }
}

// ── Main ──

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "kopi_proxy_rust=info,tower_http=info".into()),
        )
        .init();

    // Load .env if present
    let _ = dotenvy::dotenv();

    let config = Config::from_env();
    let port = config.port;

    info!("{brand}", brand = config.brand);
    info!("Starting KOPI Proxy v2 on port {port}");
    info!("Smart routing: ENABLED | Failover: ENABLED | Circuit Breaker: {}s cooldown | Fallback chain: {} providers",
          routing::CIRCUIT_BREAKER_COOLDOWN_SECS, config.global_fallback.len() + 1);
    info!("Admin dashboard: http://0.0.0.0:{port}/v2/admin/", port = port);

    // Open SQLite
    let db = Mutex::new(
        Connection::open(&config.db_path)
            .expect("Failed to open SQLite database"),
    );

    // WAL mode for concurrent access
    {
        let conn = db.lock().unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")
            .ok();
    }

    let state = Arc::new(AppState {
        config,
        db,
        rate_limiter: RwLock::new(RateLimiter::default()),
        upstream_health: RwLock::new(HashMap::new()),
    });

    let app = Router::new()
        .route("/health", get(health))
        .route("/v1/health", get(health))
        .route("/auth/verify", get(auth_verify))
        .route("/v1/pricing", get(pricing))
        .route("/v1/docs", get(v2_docs))
        .route("/v1/models", get(list_models))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/chat/completions/{model}", post(chat_completions))
        // Billing API
        .route("/v1/balance", get(billing::balance_check))
        .route("/v1/admin", get(billing::admin_dashboard))
        .route("/v1/admin/", get(billing::admin_dashboard))
        .route("/v1/admin/api/stats", get(billing::admin_stats))
        .route("/v1/admin/api/clients", get(billing::admin_clients))
        .route("/v1/admin/api/clients/{id}", get(billing::admin_client_detail))
        .route("/v1/admin/api/topup", post(billing::admin_topup))
        .route("/v1/admin/api/usage/recent", get(billing::admin_usage_recent))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let addr = format!("0.0.0.0:{port}");
    info!("Listening on {addr}");

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("Failed to bind address");

    axum::serve(listener, app)
        .await
        .expect("Server error");
}
