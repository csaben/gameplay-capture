//! HTTP handlers.

use crate::auth::{self, AuthUser, PollOutcome};
use crate::error::{ApiError, ApiResult};
use crate::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use cap_types::files;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::time::Duration;
use uuid::Uuid;

pub const DEVICE_CODE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/auth/device", post(auth_device))
        .route("/config", get(get_config))
        .route("/sessions", post(create_session))
        .route("/sessions/{id}/segments/{n}/upload", post(segment_upload))
        .route("/sessions/{id}/segments/{n}/complete", post(segment_complete))
        .route("/me/data", delete(delete_my_data))
        .with_state(state)
}

/// `[A-Za-z0-9._-]{1,128}`, no leading dot: safe as an object key component.
pub fn valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && !s.starts_with('.')
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
}

fn segment_prefix(user_id: Uuid, session_id: &str, idx: u32) -> String {
    format!("raw/{user_id}/{session_id}/{}", cap_types::segment_dir_name(idx))
}

// ------------------------------------------------------------ /auth/device

#[derive(Debug, Deserialize)]
struct DeviceAuthRequest {
    grant_type: Option<String>,
    device_code: Option<String>,
    refresh_token: Option<String>,
    device_name: Option<String>,
}

async fn auth_device(State(st): State<AppState>, Json(req): Json<DeviceAuthRequest>) -> ApiResult<Json<Value>> {
    match req.grant_type.as_deref() {
        None | Some("") | Some("start") => {
            let start = st.provider.start().await.map_err(|e| {
                tracing::warn!(error = %e, "device start failed");
                ApiError::new(StatusCode::BAD_GATEWAY, "provider_error", e.to_string())
            })?;
            Ok(Json(serde_json::to_value(start).unwrap()))
        }
        Some(DEVICE_CODE_GRANT) => {
            let code = req.device_code.as_deref().ok_or_else(|| ApiError::bad_request("invalid_request", "device_code required"))?;
            let outcome = st.provider.poll(code).await.map_err(|e| {
                tracing::warn!(error = %e, "device poll failed");
                ApiError::new(StatusCode::BAD_GATEWAY, "provider_error", e.to_string())
            })?;
            match outcome {
                PollOutcome::Pending => Err(ApiError::bad_request("authorization_pending", "waiting for the user")),
                PollOutcome::SlowDown => Err(ApiError::bad_request("slow_down", "poll less often")),
                PollOutcome::Denied => Err(ApiError::bad_request("access_denied", "the user denied the request")),
                PollOutcome::Expired => Err(ApiError::bad_request("expired_token", "device code expired")),
                PollOutcome::Approved(ident) => {
                    let name = req.device_name.as_deref().unwrap_or("unnamed").trim();
                    let name: String = name.chars().take(64).collect();
                    let (user_id, device_id) = auth::upsert_user_device(&st, &ident, &name).await?;
                    tracing::info!(%user_id, %device_id, "device login");
                    Ok(Json(serde_json::to_value(auth::issue_tokens(&st, user_id, device_id).await?).unwrap()))
                }
            }
        }
        Some("refresh_token") => {
            let rt = req.refresh_token.as_deref().ok_or_else(|| ApiError::bad_request("invalid_request", "refresh_token required"))?;
            Ok(Json(serde_json::to_value(auth::refresh(&st, rt).await?).unwrap()))
        }
        Some(other) => Err(ApiError::bad_request("unsupported_grant_type", other.to_string())),
    }
}

// ----------------------------------------------------------------- /config

async fn get_config(State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let games: Vec<(String, String, Option<String>, Option<String>)> =
        sqlx::query_as("SELECT id, status, publisher, notes FROM games ORDER BY id").fetch_all(&st.db).await?;
    let entry = |(id, _, publisher, notes): &(String, String, Option<String>, Option<String>)| {
        json!({ "game_id": id, "publisher": publisher, "notes": notes })
    };
    let blocklist: Vec<Value> = games.iter().filter(|g| g.1 == "blocked").map(entry).collect();
    let allowlist: Vec<Value> = games.iter().filter(|g| g.1 == "allowed").map(entry).collect();
    let c = &st.cfg;
    Ok(Json(json!({
        "blocklist": blocklist,
        "allowlist": allowlist,
        "default_deny": c.default_deny,
        "allowed_encoders": c.allowed_encoders,
        "rate_hz": c.rate_hz,
        "width": c.width,
        "height": c.height,
        "min_client_version": c.min_client_version,
        "consent_version": c.consent_version,
    })))
}

// ---------------------------------------------------------------- sessions

#[derive(Debug, Deserialize)]
struct CreateSession {
    session_id: Option<String>,
    game_id: String,
    client_version: String,
    consent_version: String,
}

/// Compare dotted numeric versions (`0.10.2` > `0.9`); non-numeric parts count as 0.
pub fn version_at_least(have: &str, min: &str) -> bool {
    let parse = |s: &str| -> Vec<u64> {
        s.split(['-', '+']).next().unwrap_or("").split('.').map(|p| p.parse().unwrap_or(0)).collect()
    };
    let (mut a, mut b) = (parse(have), parse(min));
    let n = a.len().max(b.len());
    a.resize(n, 0);
    b.resize(n, 0);
    a >= b
}

async fn create_session(State(st): State<AppState>, user: AuthUser, Json(req): Json<CreateSession>) -> ApiResult<Json<Value>> {
    let session_id = req.session_id.clone().unwrap_or_else(|| Uuid::new_v4().to_string());
    if !valid_id(&session_id) {
        return Err(ApiError::bad_request("invalid_session_id", "session_id must match [A-Za-z0-9._-]{1,128}"));
    }
    if req.consent_version != st.cfg.consent_version {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "consent_required",
            format!("current consent version is {}", st.cfg.consent_version),
        ));
    }
    if !version_at_least(&req.client_version, &st.cfg.min_client_version) {
        return Err(ApiError::forbidden("client_too_old", format!("minimum client version is {}", st.cfg.min_client_version)));
    }
    let status: Option<String> = sqlx::query_scalar("SELECT status FROM games WHERE id = $1")
        .bind(&req.game_id)
        .fetch_optional(&st.db)
        .await?;
    match status.as_deref() {
        Some("blocked") => return Err(ApiError::forbidden("game_blocked", format!("{} is blocked", req.game_id))),
        Some("allowed") => {}
        _ if st.cfg.default_deny => {
            return Err(ApiError::forbidden("game_not_allowed", format!("{} is not on the allowlist", req.game_id)))
        }
        _ => {}
    }

    let mut tx = st.db.begin().await?;
    sqlx::query("INSERT INTO consents (user_id, version, device_id) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING")
        .bind(user.user_id)
        .bind(&req.consent_version)
        .bind(user.device_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO sessions (id, user_id, device_id, game_id, client_version, consent_version)
         VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (id) DO NOTHING",
    )
    .bind(&session_id)
    .bind(user.user_id)
    .bind(user.device_id)
    .bind(&req.game_id)
    .bind(&req.client_version)
    .bind(&req.consent_version)
    .execute(&mut *tx)
    .await?;
    let owner: Uuid = sqlx::query_scalar("SELECT user_id FROM sessions WHERE id = $1")
        .bind(&session_id)
        .fetch_one(&mut *tx)
        .await?;
    if owner != user.user_id {
        return Err(ApiError::conflict("session_exists", "session id belongs to another user"));
    }
    tx.commit().await?;
    Ok(Json(json!({
        "session_id": session_id,
        "user_id": user.user_id.to_string(),
        "key_prefix": format!("raw/{}/{}/", user.user_id, session_id),
    })))
}

// ---------------------------------------------------------------- segments

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
struct FileInfo {
    name: String,
    size: u64,
    blake3: String,
}

#[derive(Debug, Deserialize)]
struct UploadReq {
    files: Vec<FileInfo>,
}

fn allowed_file(name: &str) -> bool {
    name == files::MANIFEST || files::DATA.contains(&name)
}

fn validate_files(st: &AppState, fs: &[FileInfo]) -> ApiResult<()> {
    let mut seen = HashSet::new();
    for f in fs {
        if !allowed_file(&f.name) {
            return Err(ApiError::bad_request("invalid_file", format!("unexpected file {:?}", f.name)));
        }
        if !seen.insert(f.name.as_str()) {
            return Err(ApiError::bad_request("invalid_file", format!("duplicate file {}", f.name)));
        }
        if f.size > st.cfg.max_file_bytes {
            return Err(ApiError::bad_request("file_too_large", format!("{} is {} bytes", f.name, f.size)));
        }
        if f.blake3.len() != 64 || !f.blake3.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ApiError::bad_request("invalid_hash", format!("{}: blake3 must be 64 hex chars", f.name)));
        }
    }
    if !seen.contains(files::MANIFEST) {
        return Err(ApiError::bad_request("invalid_file", "manifest.json must be included"));
    }
    Ok(())
}

/// Checks the session belongs to the caller and the index is sane.
async fn own_session(st: &AppState, user: &AuthUser, session_id: &str, n: i64) -> ApiResult<u32> {
    if !valid_id(session_id) {
        return Err(ApiError::bad_request("invalid_session_id", "bad session id"));
    }
    let idx: u32 = u32::try_from(n)
        .ok()
        .filter(|&i| i <= i32::MAX as u32)
        .ok_or_else(|| ApiError::bad_request("invalid_segment", "segment index out of range"))?;
    let owner: Option<Uuid> = sqlx::query_scalar("SELECT user_id FROM sessions WHERE id = $1")
        .bind(session_id)
        .fetch_optional(&st.db)
        .await?;
    match owner {
        Some(o) if o == user.user_id => Ok(idx),
        _ => Err(ApiError::not_found("unknown session")),
    }
}

async fn segment_upload(
    State(st): State<AppState>,
    user: AuthUser,
    Path((session_id, n)): Path<(String, i64)>,
    Json(req): Json<UploadReq>,
) -> ApiResult<Json<Value>> {
    let idx = own_session(&st, &user, &session_id, n).await?;
    validate_files(&st, &req.files)?;
    let prefix = segment_prefix(user.user_id, &session_id, idx);
    let sizes: BTreeMap<&str, u64> = req.files.iter().map(|f| (f.name.as_str(), f.size)).collect();
    let hashes: BTreeMap<&str, &str> = req.files.iter().map(|f| (f.name.as_str(), f.blake3.as_str())).collect();

    let state: Option<String> = sqlx::query_scalar(
        "INSERT INTO segments (id, session_id, segment_idx, user_id, key_prefix, state, sizes, hashes)
         VALUES ($1, $2, $3, $4, $5, 'uploading', $6, $7)
         ON CONFLICT (session_id, segment_idx) DO UPDATE
            SET state = 'uploading', sizes = EXCLUDED.sizes, hashes = EXCLUDED.hashes, completed_at = NULL
            WHERE segments.state IN ('uploading', 'ready')
         RETURNING state",
    )
    .bind(Uuid::new_v4())
    .bind(&session_id)
    .bind(idx as i32)
    .bind(user.user_id)
    .bind(&prefix)
    .bind(json!(sizes))
    .bind(json!(hashes))
    .fetch_optional(&st.db)
    .await?;
    if state.is_none() {
        return Err(ApiError::conflict("segment_deleted", "segment was deleted at the user's request"));
    }

    let ttl = Duration::from_secs(st.cfg.presign_ttl_secs);
    let mut out = Vec::with_capacity(req.files.len());
    for f in &req.files {
        let key = format!("{prefix}/{}", f.name);
        let url = st.storage.presign_put(&key, ttl).await?;
        out.push(json!({ "name": f.name, "key": key, "url": url, "method": "PUT" }));
    }
    Ok(Json(json!({ "expires_in": st.cfg.presign_ttl_secs, "files": out })))
}

#[derive(Debug, Deserialize)]
struct CompleteReq {
    files: Vec<FileInfo>,
    #[serde(default)]
    dropped_frames: u64,
    #[serde(default)]
    frame_count: u32,
}

async fn segment_complete(
    State(st): State<AppState>,
    user: AuthUser,
    Path((session_id, n)): Path<(String, i64)>,
    Json(req): Json<CompleteReq>,
) -> ApiResult<Json<Value>> {
    let idx = own_session(&st, &user, &session_id, n).await?;
    validate_files(&st, &req.files)?;
    let row: Option<(Uuid, String, String, Value, Value)> = sqlx::query_as(
        "SELECT id, state, key_prefix, sizes, hashes FROM segments WHERE session_id = $1 AND segment_idx = $2",
    )
    .bind(&session_id)
    .bind(idx as i32)
    .fetch_optional(&st.db)
    .await?;
    let (seg_id, state, prefix, sizes, hashes) = row.ok_or_else(|| ApiError::not_found("request upload URLs first"))?;
    if state == "deletion_pending" || state == "deleted" {
        // Objects PUT after the deletion ran: remove them again.
        let _ = st.storage.delete_prefix(&prefix).await;
        return Err(ApiError::conflict("segment_deleted", "segment was deleted at the user's request"));
    }
    // Reported files must match what was declared when URLs were issued.
    let declared: BTreeMap<String, (u64, String)> = serde_json::from_value::<BTreeMap<String, u64>>(sizes)
        .unwrap_or_default()
        .into_iter()
        .map(|(k, v)| {
            let h = hashes.get(&k).and_then(|h| h.as_str()).unwrap_or("").to_string();
            (k, (v, h))
        })
        .collect();
    let reported: BTreeMap<String, (u64, String)> =
        req.files.iter().map(|f| (f.name.clone(), (f.size, f.blake3.to_lowercase()))).collect();
    let declared_lc: BTreeMap<String, (u64, String)> =
        declared.into_iter().map(|(k, (s, h))| (k, (s, h.to_lowercase()))).collect();
    if reported != declared_lc {
        return Err(ApiError::bad_request("files_changed", "files differ from the upload request; request new URLs"));
    }

    let mut total = 0u64;
    for f in &req.files {
        let key = format!("{prefix}/{}", f.name);
        match st.storage.size(&key).await? {
            None => return Err(ApiError::bad_request("object_missing", format!("{key} not found"))),
            Some(s) if s != f.size => {
                return Err(ApiError::bad_request("size_mismatch", format!("{key}: stored {s} bytes, expected {}", f.size)))
            }
            Some(s) => total += s,
        }
    }
    let updated = sqlx::query(
        "UPDATE segments SET state = 'ready', completed_at = now(), total_bytes = $2, dropped_frames = $3, frame_count = $4
         WHERE id = $1 AND state IN ('uploading', 'ready')",
    )
    .bind(seg_id)
    .bind(total as i64)
    .bind(req.dropped_frames as i64)
    .bind(req.frame_count as i32)
    .execute(&st.db)
    .await?;
    if updated.rows_affected() == 0 {
        let _ = st.storage.delete_prefix(&prefix).await;
        return Err(ApiError::conflict("segment_deleted", "segment was deleted at the user's request"));
    }
    Ok(Json(json!({ "segment_id": seg_id.to_string(), "state": "ready" })))
}

// ----------------------------------------------------------------- /me/data

async fn delete_my_data(State(st): State<AppState>, user: AuthUser) -> ApiResult<(StatusCode, Json<Value>)> {
    let deletion_id = Uuid::new_v4();
    let mut tx = st.db.begin().await?;
    sqlx::query("INSERT INTO deletions (id, user_id, status) VALUES ($1, $2, 'pending')")
        .bind(deletion_id)
        .bind(user.user_id)
        .execute(&mut *tx)
        .await?;
    let n = sqlx::query(
        "INSERT INTO deletion_segments (deletion_id, segment_id, session_id, segment_idx, key_prefix)
         SELECT $1, id, session_id, segment_idx, key_prefix FROM segments
         WHERE user_id = $2 AND state IN ('uploading', 'ready')",
    )
    .bind(deletion_id)
    .bind(user.user_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    sqlx::query("UPDATE segments SET state = 'deletion_pending' WHERE user_id = $1 AND state IN ('uploading', 'ready')")
        .bind(user.user_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    st.deletion_notify.notify_one();
    tracing::info!(user_id = %user.user_id, %deletion_id, segments = n, "deletion queued");
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "deletion_id": deletion_id.to_string(), "segments": n, "status": "pending" })),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert!(version_at_least("0.1.0", "0.1.0"));
        assert!(version_at_least("0.10.0", "0.9.5"));
        assert!(version_at_least("1.0", "0.9.9"));
        assert!(!version_at_least("0.1.0", "0.2"));
        assert!(version_at_least("0.2.0-beta.1", "0.2.0"));
    }

    #[test]
    fn ids() {
        assert!(valid_id("2026-09-29T21-00-00_abc"));
        assert!(!valid_id("../x"));
        assert!(!valid_id("a/b"));
        assert!(!valid_id(""));
        assert!(!valid_id(".hidden"));
    }
}
