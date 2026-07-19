use rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::sync::Mutex;

/// A verified client from the database.
#[derive(Debug, Clone)]
pub struct Client {
    pub id: i64,
    pub name: String,
    pub key_prefix: String,
    pub is_active: bool,
    pub quota_limit: i64,  // from client_quotas.token_limit
    pub quota_used: i64,   // from client_quotas.token_used
}

/// Hash an API key for comparison with key_hash in the DB.
pub fn hash_key(api_key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(api_key.as_bytes());
    hex::encode(hasher.finalize())
}

/// Verify an API key and return the client info.
/// First checks `client_api_keys` table (multi-key support), then `clients` table.
pub fn verify_key(db: &Mutex<Connection>, api_key: &str) -> Result<Client, String> {
    let conn = db.lock().map_err(|e| format!("DB lock: {e}"))?;
    let h = hash_key(api_key);

    // Try multi-key table first (newer clients use this)
    let row = conn.query_row(
        "SELECT c.id, c.name, c.key_prefix, c.is_active
         FROM client_api_keys k
         JOIN clients c ON c.id = k.client_id
         WHERE k.key_hash=?1 AND k.is_active=1 AND c.is_active=1
         LIMIT 1",
        rusqlite::params![h],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i32>(3)?,
            ))
        },
    );

    match row {
        Ok((id, name, prefix, active)) => {
            // Get quota info
            let (quota_limit, quota_used) = get_quota(&conn, id);
            Ok(Client {
                id,
                name,
                key_prefix: prefix,
                is_active: active == 1,
                quota_limit,
                quota_used,
            })
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            // Fall back to clients table (older clients)
            let row = conn.query_row(
                "SELECT id, name, key_prefix, is_active FROM clients WHERE api_key=?1 AND is_active=1 LIMIT 1",
                rusqlite::params![h],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i32>(3)?,
                    ))
                },
            );

            match row {
                Ok((id, name, prefix, active)) => {
                    let (quota_limit, quota_used) = get_quota(&conn, id);
                    Ok(Client {
                        id,
                        name,
                        key_prefix: prefix,
                        is_active: active == 1,
                        quota_limit,
                        quota_used,
                    })
                }
                Err(rusqlite::Error::QueryReturnedNoRows) => {
                    Err("Invalid API key".to_string())
                }
                Err(e) => Err(format!("DB error: {e}")),
            }
        }
        Err(e) => Err(format!("DB error: {e}")),
    }
}

/// Returns (quota_limit, quota_used) for a client.
fn get_quota(conn: &Connection, client_id: i64) -> (i64, i64) {
    // Try client_quotas table first (newer)
    match conn.query_row(
        "SELECT token_limit, token_used FROM client_quotas WHERE client_id=?1",
        rusqlite::params![client_id],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
            ))
        },
    ) {
        Ok(result) => result,
        Err(_) => {
            // Fall back to clients.total_token_limit
            match conn.query_row(
                "SELECT total_token_limit, 0 FROM clients WHERE id=?1",
                rusqlite::params![client_id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        0i64,
                    ))
                },
            ) {
                Ok(result) => result,
                Err(_) => (0, 0),
            }
        }
    }
}

/// Check if the client has sufficient token quota remaining.
pub fn check_quota(db: &Mutex<Connection>, client: &Client, tokens_needed: i64) -> Result<bool, String> {
    if client.quota_limit <= 0 {
        return Ok(true); // unlimited
    }
    let remaining = client.quota_limit - client.quota_used;
    Ok(remaining >= tokens_needed)
}

/// Deduct tokens from a client's quota.
/// Updates client_quotas.token_used and usage_logs.
pub fn deduct_tokens(
    db: &Mutex<Connection>,
    client_id: i64,
    model: &str,
    upstream_name: &str,
    tokens_in: i64,
    tokens_out: i64,
    latency_ms: i64,
) -> Result<(), String> {
    let conn = db.lock().map_err(|e| format!("DB lock: {e}"))?;
    let total = tokens_in + tokens_out;

    // 1. Update client_quotas.token_used
    let rows = conn.execute(
        "UPDATE client_quotas SET token_used = token_used + ?1 WHERE client_id = ?2",
        rusqlite::params![total, client_id],
    ).map_err(|e| format!("DB quota update: {e}"))?;

    if rows == 0 {
        // No client_quotas row — insert one
        conn.execute(
            "INSERT OR IGNORE INTO client_quotas (client_id, token_limit, token_used) VALUES (?1, 2000000000, ?2)",
            rusqlite::params![client_id, total],
        ).map_err(|e| format!("DB quota insert: {e}"))?;

        // Try the update again
        conn.execute(
            "UPDATE client_quotas SET token_used = token_used + ?1 WHERE client_id = ?2",
            rusqlite::params![total, client_id],
        ).map_err(|e| format!("DB quota retry: {e}"))?;
    }

    // 2. Update requests counter on clients table
    conn.execute(
        "UPDATE clients SET requests = COALESCE(requests, 0) + 1, last_used_at = datetime('now') WHERE id = ?1",
        rusqlite::params![client_id],
    ).map_err(|e| format!("DB requests update: {e}"))?;

    // 3. Log to usage_logs
    conn.execute(
        "INSERT INTO usage_logs (client_id, model, tokens_in, tokens_out, upstream, latency_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            client_id,
            model,
            tokens_in,
            tokens_out,
            upstream_name,
            latency_ms,
        ],
    ).map_err(|e| format!("DB insert: {e}"))?;

    Ok(())
}
