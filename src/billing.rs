use crate::AppState;
use crate::db;
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse},
    Json,
};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::{Arc, Mutex};
use tracing::warn;

// ── Response types ──

#[derive(Serialize)]
pub struct BalanceResponse {
    pub client_id: i64,
    pub name: String,
    pub key_prefix: String,
    pub is_active: bool,
    pub quota_limit: i64,
    pub quota_used: i64,
    pub quota_remaining: i64,
    pub total_requests: i64,
    pub is_unlimited: bool,
}

#[derive(Serialize)]
pub struct ClientBillingInfo {
    pub id: i64,
    pub name: String,
    pub key_prefix: String,
    pub is_active: bool,
    pub token_limit: i64,
    pub token_used: i64,
    pub usage_percent: f64,
    pub total_requests: i64,
    pub created_at: String,
    pub last_used_at: Option<String>,
}

#[derive(Serialize)]
pub struct UsageLogEntry {
    pub id: i64,
    pub model: String,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub cost: f64,
    pub upstream: String,
    pub latency_ms: i64,
    pub created_at: String,
}

#[derive(Serialize)]
pub struct AdminStats {
    pub total_clients: i64,
    pub active_clients: i64,
    pub total_requests: i64,
    pub total_tokens_used: i64,
    pub total_revenue: f64,
    pub today_requests: i64,
    pub today_tokens: i64,
}

#[derive(Deserialize)]
pub struct TopupRequest {
    pub client_id: i64,
    pub amount: i64,
    pub note: Option<String>,
}

#[derive(Deserialize)]
pub struct TopupKeyRequest {
    pub amount: i64,  // tokens to add
    pub client_id: Option<i64>,
}

#[derive(Serialize)]
pub struct TopupKeyResponse {
    pub token: String,
    pub client_id: i64,
    pub amount: i64,
    pub expires_at: String,
}

// ── Client-facing balance endpoint ──

/// GET /v1/balance — returns balance for the authenticated client.
pub async fn balance_check(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<BalanceResponse>, AppBillingError> {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .strip_prefix("Bearer ")
        .unwrap_or("");

    let client = db::verify_key(&state.db, auth).map_err(|e| {
        warn!("Balance auth failed: {e}");
        AppBillingError::unauthorized(e)
    })?;

    let result = get_balance_helper(&state.db, client.id, &client.name, &client.key_prefix,
        client.is_active, client.quota_limit, client.quota_used)
        .map_err(|e| AppBillingError::internal(e))?;

    Ok(Json(result))
}

fn get_balance_helper(
    db: &Mutex<Connection>,
    client_id: i64,
    name: &str,
    key_prefix: &str,
    is_active: bool,
    quota_limit: i64,
    quota_used: i64,
) -> Result<BalanceResponse, String> {
    let conn = db.lock().map_err(|e| format!("DB lock: {e}"))?;

    let total_requests: i64 = conn
        .query_row(
            "SELECT COALESCE(requests, 0) FROM clients WHERE id=?1",
            rusqlite::params![client_id],
            |row| row.get(0),
        )
        .unwrap_or(0);

    let remaining = if quota_limit <= 0 {
        -1
    } else {
        quota_limit - quota_used
    };

    Ok(BalanceResponse {
        client_id,
        name: name.to_string(),
        key_prefix: key_prefix.to_string(),
        is_active,
        quota_limit,
        quota_used,
        quota_remaining: remaining,
        total_requests,
        is_unlimited: quota_limit <= 0,
    })
}

// ── Admin verification ──

fn verify_admin(headers: &HeaderMap, admin_key: &str) -> Result<(), String> {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .strip_prefix("Bearer ")
        .unwrap_or("");

    if auth.is_empty() || auth != admin_key {
        return Err("Invalid admin API key".to_string());
    }
    Ok(())
}

// ── Admin API endpoints ──

/// GET /admin/api/stats — overall usage statistics.
pub async fn admin_stats(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<AdminStats>, AppBillingError> {
    verify_admin(&headers, &state.config.admin_key).map_err(|e| {
        AppBillingError::unauthorized(e)
    })?;

    let conn = state.db.lock().map_err(|e| AppBillingError::internal(e))?;

    let total_clients: i64 = conn
        .query_row("SELECT COUNT(*) FROM clients", [], |r| r.get(0))
        .unwrap_or(0);
    let active_clients: i64 = conn
        .query_row("SELECT COUNT(*) FROM clients WHERE is_active=1", [], |r| r.get(0))
        .unwrap_or(0);
    let total_requests: i64 = conn
        .query_row("SELECT COALESCE(SUM(requests), 0) FROM clients", [], |r| r.get(0))
        .unwrap_or(0);
    let total_tokens_used: i64 = conn
        .query_row("SELECT COALESCE(SUM(token_used), 0) FROM client_quotas", [], |r| r.get(0))
        .unwrap_or(0);

    // Revenue from usage_logs (cost is per-request cost)
    let total_revenue: f64 = conn
        .query_row(
            "SELECT COALESCE(SUM(cost), 0) FROM usage_logs",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0.0);

    // Today
    let today_requests: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM usage_logs WHERE created_at >= datetime('now', '-1 day')",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let today_tokens: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(tokens_in + tokens_out), 0) FROM usage_logs WHERE created_at >= datetime('now', '-1 day')",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);

    Ok(Json(AdminStats {
        total_clients,
        active_clients,
        total_requests,
        total_tokens_used,
        total_revenue,
        today_requests,
        today_tokens,
    }))
}

/// GET /admin/api/clients — list all clients.
pub async fn admin_clients(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Vec<ClientBillingInfo>>, AppBillingError> {
    verify_admin(&headers, &state.config.admin_key).map_err(|e| {
        AppBillingError::unauthorized(e)
    })?;

    let conn = state.db.lock().map_err(|e| AppBillingError::internal(e))?;
    let search = params.get("search").map(|s| s.as_str()).unwrap_or("");
    let limit: i64 = params
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(100);

    let query = if search.is_empty() {
        format!(
            "SELECT c.id, c.name, c.key_prefix, c.is_active,
                    COALESCE(cq.token_limit, c.total_token_limit, 0),
                    COALESCE(cq.token_used, 0),
                    COALESCE(c.requests, 0),
                    c.created_at, c.last_used_at
             FROM clients c
             LEFT JOIN client_quotas cq ON c.id = cq.client_id
             ORDER BY c.id DESC
             LIMIT {}",
            limit
        )
    } else {
        format!(
            "SELECT c.id, c.name, c.key_prefix, c.is_active,
                    COALESCE(cq.token_limit, c.total_token_limit, 0),
                    COALESCE(cq.token_used, 0),
                    COALESCE(c.requests, 0),
                    c.created_at, c.last_used_at
             FROM clients c
             LEFT JOIN client_quotas cq ON c.id = cq.client_id
             WHERE c.name LIKE '%{}%' OR c.key_prefix LIKE '%{}%'
             ORDER BY c.id DESC
             LIMIT {}",
            search, search, limit
        )
    };

    let mut stmt = conn.prepare(&query).map_err(|e| AppBillingError::internal(e))?;

    let clients: Vec<ClientBillingInfo> = stmt
        .query_map([], |row| {
            let limit: i64 = row.get(4)?;
            let used: i64 = row.get(5)?;
            let pct = if limit > 0 {
                (used as f64 / limit as f64) * 100.0
            } else {
                0.0
            };
            Ok(ClientBillingInfo {
                id: row.get(0)?,
                name: row.get(1)?,
                key_prefix: row.get(2)?,
                is_active: row.get::<_, i32>(3)? == 1,
                token_limit: limit,
                token_used: used,
                usage_percent: pct,
                total_requests: row.get(6)?,
                created_at: row.get(7)?,
                last_used_at: row.get(8)?,
            })
        })
        .map_err(|e| AppBillingError::internal(e))?
        .filter_map(|r| r.ok())
        .collect();

    Ok(Json(clients))
}

/// GET /admin/api/clients/:id — single client detail with usage.
pub async fn admin_client_detail(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, AppBillingError> {
    verify_admin(&headers, &state.config.admin_key).map_err(|e| {
        AppBillingError::unauthorized(e)
    })?;

    let conn = state.db.lock().map_err(|e| AppBillingError::internal(e))?;

    // Client info
    let client: ClientBillingInfo = conn
        .query_row(
            "SELECT c.id, c.name, c.key_prefix, c.is_active,
                    COALESCE(cq.token_limit, c.total_token_limit, 0),
                    COALESCE(cq.token_used, 0),
                    COALESCE(c.requests, 0),
                    c.created_at, c.last_used_at
             FROM clients c
             LEFT JOIN client_quotas cq ON c.id = cq.client_id
             WHERE c.id=?1",
            rusqlite::params![id],
            |row| {
                let limit: i64 = row.get(4)?;
                let used: i64 = row.get(5)?;
                let pct = if limit > 0 {
                    (used as f64 / limit as f64) * 100.0
                } else {
                    0.0
                };
                Ok(ClientBillingInfo {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    key_prefix: row.get(2)?,
                    is_active: row.get::<_, i32>(3)? == 1,
                    token_limit: limit,
                    token_used: used,
                    usage_percent: pct,
                    total_requests: row.get(6)?,
                    created_at: row.get(7)?,
                    last_used_at: row.get(8)?,
                })
            },
        )
        .map_err(|_| AppBillingError::not_found("Client not found"))?;

    // Recent usage logs
    let mut stmt = conn
        .prepare(
            "SELECT id, model, tokens_in, tokens_out, cost, upstream, latency_ms, created_at
             FROM usage_logs
             WHERE client_id=?1
             ORDER BY created_at DESC
             LIMIT 50",
        )
        .map_err(|e| AppBillingError::internal(e))?;

    let usage: Vec<UsageLogEntry> = stmt
        .query_map(rusqlite::params![id], |row| {
            Ok(UsageLogEntry {
                id: row.get(0)?,
                model: row.get(1)?,
                tokens_in: row.get(2)?,
                tokens_out: row.get(3)?,
                cost: row.get(4)?,
                upstream: row.get(5)?,
                latency_ms: row.get(6)?,
                created_at: row.get(7)?,
            })
        })
        .map_err(|e| AppBillingError::internal(e))?
        .filter_map(|r| r.ok())
        .collect();

    Ok(Json(json!({
        "client": client,
        "usage": usage,
    })))
}

/// POST /admin/api/topup — add tokens to a client.
pub async fn admin_topup(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<TopupRequest>,
) -> Result<Json<serde_json::Value>, AppBillingError> {
    verify_admin(&headers, &state.config.admin_key).map_err(|e| {
        AppBillingError::unauthorized(e)
    })?;

    if req.amount <= 0 {
        return Err(AppBillingError::bad_request("Amount must be positive"));
    }

    let conn = state.db.lock().map_err(|e| AppBillingError::internal(e))?;

    // Verify client exists
    let exists: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM clients WHERE id=?1",
            rusqlite::params![req.client_id],
            |row| row.get::<_, i64>(0),
        )
        .map(|c| c > 0)
        .unwrap_or(false);

    if !exists {
        return Err(AppBillingError::not_found("Client not found"));
    }

    // Update token_limit in client_quotas
    let rows = conn
        .execute(
            "UPDATE client_quotas SET token_limit = token_limit + ?1, last_topup_at = datetime('now', '+8 hours'), updated_at = datetime('now', '+8 hours') WHERE client_id = ?2",
            rusqlite::params![req.amount, req.client_id],
        )
        .map_err(|e| AppBillingError::internal(e))?;

    if rows == 0 {
        conn.execute(
            "INSERT INTO client_quotas (client_id, token_limit, token_used) VALUES (?1, ?2, 0)",
            rusqlite::params![req.client_id, req.amount],
        )
        .map_err(|e| AppBillingError::internal(e))?;
    }

    // Log to topup_tokens table
    let note = req.note.unwrap_or_else(|| "Admin topup".into());
    conn.execute(
        "INSERT INTO topup_tokens (token, client_id, created_at, expires_at) VALUES (?1, ?2, datetime('now', '+8 hours'), datetime('now', '+1 year'))",
        rusqlite::params![format!("admin-{}-{}", req.client_id, req.amount), req.client_id],
    )
    .ok();

    Ok(Json(json!({
        "success": true,
        "client_id": req.client_id,
        "amount": req.amount,
        "note": note,
    })))
}

/// GET /admin/api/usage/recent — recent usage across all clients.
pub async fn admin_usage_recent(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Vec<UsageLogEntry>>, AppBillingError> {
    verify_admin(&headers, &state.config.admin_key).map_err(|e| {
        AppBillingError::unauthorized(e)
    })?;

    let limit: i64 = params
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(50);

    let conn = state.db.lock().map_err(|e| AppBillingError::internal(e))?;

    let mut stmt = conn
        .prepare(
            "SELECT id, model, tokens_in, tokens_out, cost, upstream, latency_ms, created_at
             FROM usage_logs
             ORDER BY created_at DESC
             LIMIT ?1",
        )
        .map_err(|e| AppBillingError::internal(e))?;

    let entries: Vec<UsageLogEntry> = stmt
        .query_map(rusqlite::params![limit], |row| {
            Ok(UsageLogEntry {
                id: row.get(0)?,
                model: row.get(1)?,
                tokens_in: row.get(2)?,
                tokens_out: row.get(3)?,
                cost: row.get(4)?,
                upstream: row.get(5)?,
                latency_ms: row.get(6)?,
                created_at: row.get(7)?,
            })
        })
        .map_err(|e| AppBillingError::internal(e))?
        .filter_map(|r| r.ok())
        .collect();

    Ok(Json(entries))
}

/// GET /v2/admin/ — serve admin dashboard HTML.
pub async fn admin_dashboard() -> Html<&'static str> {
    Html(include_str!("../admin.html"))
}

// ── Error handling ──

pub struct AppBillingError {
    status: StatusCode,
    message: String,
}

impl AppBillingError {
    fn unauthorized(msg: impl std::fmt::Display) -> Self {
        Self { status: StatusCode::UNAUTHORIZED, message: msg.to_string() }
    }
    fn bad_request(msg: impl std::fmt::Display) -> Self {
        Self { status: StatusCode::BAD_REQUEST, message: msg.to_string() }
    }
    fn not_found(msg: impl std::fmt::Display) -> Self {
        Self { status: StatusCode::NOT_FOUND, message: msg.to_string() }
    }
    fn internal(msg: impl std::fmt::Display) -> Self {
        Self { status: StatusCode::INTERNAL_SERVER_ERROR, message: msg.to_string() }
    }
}

impl IntoResponse for AppBillingError {
    fn into_response(self) -> axum::response::Response {
        let body = json!({
            "error": {
                "message": self.message,
                "type": "billing_error",
                "code": self.status.as_u16(),
            }
        });
        let body_str = serde_json::to_string(&body).unwrap_or_default();
        axum::response::Response::builder()
            .status(self.status)
            .header("Content-Type", "application/json")
            .body(axum::body::Body::from(body_str))
            .unwrap()
    }
}
