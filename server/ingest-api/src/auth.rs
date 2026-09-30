//! Device-code login (pluggable provider), bearer tokens, auth extractor.

use crate::error::{ApiError, ApiResult};
use crate::{AppState, Config};
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use base64::Engine;
use chrono::{Duration as ChronoDuration, Utc};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use uuid::Uuid;

/// What the provider returns when a device login starts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceStart {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification_uri_complete: Option<String>,
    pub expires_in: u64,
    pub interval: u64,
}

/// Authenticated identity from the provider.
#[derive(Debug, Clone)]
pub struct Identity {
    pub subject: String,
    pub email: Option<String>,
}

#[derive(Debug, Clone)]
pub enum PollOutcome {
    Pending,
    SlowDown,
    Denied,
    Expired,
    Approved(Identity),
}

pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = anyhow::Result<T>> + Send + 'a>>;

/// An OAuth 2.0 device-authorization-grant provider (RFC 8628).
pub trait DeviceAuthProvider: Send + Sync {
    /// Stored in `users.provider`.
    fn name(&self) -> &str;
    fn start(&self) -> BoxFut<'_, DeviceStart>;
    fn poll<'a>(&'a self, device_code: &'a str) -> BoxFut<'a, PollOutcome>;
}

pub fn provider_from_config(cfg: &Config) -> anyhow::Result<Arc<dyn DeviceAuthProvider>> {
    match cfg.auth_provider.as_str() {
        "dev" => {
            let loopback = cfg.bind.starts_with("127.") || cfg.bind.starts_with("localhost:") || cfg.bind.starts_with("[::1]");
            if !loopback && std::env::var("DEV_AUTH_ALLOW_REMOTE").as_deref() != Ok("1") {
                anyhow::bail!("AUTH_PROVIDER=dev only binds to loopback (set DEV_AUTH_ALLOW_REMOTE=1 to override)");
            }
            tracing::warn!("AUTH_PROVIDER=dev: every device login is auto-approved (local testing only)");
            Ok(Arc::new(DevProvider { subject: cfg.dev_subject.clone() }))
        }
        "oauth2" => {
            let req = |v: &Option<String>, n: &str| v.clone().ok_or_else(|| anyhow::anyhow!("{n} is required for AUTH_PROVIDER=oauth2"));
            Ok(Arc::new(OAuth2DeviceProvider {
                name: format!("oauth2:{}", cfg.oauth_name),
                device_authorization_url: req(&cfg.oauth_device_authorization_url, "OAUTH_DEVICE_AUTHORIZATION_URL")?,
                token_url: req(&cfg.oauth_token_url, "OAUTH_TOKEN_URL")?,
                userinfo_url: cfg.oauth_userinfo_url.clone(),
                client_id: req(&cfg.oauth_client_id, "OAUTH_CLIENT_ID")?,
                client_secret: cfg.oauth_client_secret.clone(),
                scope: cfg.oauth_scope.clone(),
                http: reqwest::Client::new(),
            }))
        }
        other => anyhow::bail!("unknown AUTH_PROVIDER {other:?} (expected dev or oauth2)"),
    }
}

/// Auto-approves every login as `subject`. Never enable in production.
pub struct DevProvider {
    pub subject: String,
}

impl DeviceAuthProvider for DevProvider {
    fn name(&self) -> &str {
        "dev"
    }
    fn start(&self) -> BoxFut<'_, DeviceStart> {
        Box::pin(async move {
            let code = random_token(16);
            let user_code = random_token(4).to_uppercase();
            Ok(DeviceStart {
                device_code: format!("dev_{code}"),
                user_code: format!("{}-{}", &user_code[..4], &user_code[4..]),
                verification_uri: "http://localhost/dev-auto-approved".into(),
                verification_uri_complete: None,
                expires_in: 600,
                interval: 1,
            })
        })
    }
    fn poll<'a>(&'a self, device_code: &'a str) -> BoxFut<'a, PollOutcome> {
        Box::pin(async move {
            if device_code.starts_with("dev_") {
                Ok(PollOutcome::Approved(Identity { subject: self.subject.clone(), email: None }))
            } else {
                Ok(PollOutcome::Expired)
            }
        })
    }
}

/// Generic RFC 8628 provider configured by URLs + client id (Google, GitHub,
/// Auth0, Okta, Keycloak, ...).
pub struct OAuth2DeviceProvider {
    pub name: String,
    pub device_authorization_url: String,
    pub token_url: String,
    pub userinfo_url: Option<String>,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub scope: String,
    pub http: reqwest::Client,
}

#[derive(Deserialize)]
struct RawDeviceStart {
    device_code: String,
    user_code: String,
    #[serde(alias = "verification_url")]
    verification_uri: String,
    verification_uri_complete: Option<String>,
    expires_in: u64,
    interval: Option<u64>,
}

#[derive(Deserialize)]
struct RawToken {
    access_token: Option<String>,
    id_token: Option<String>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct Claims {
    sub: Option<serde_json::Value>,
    id: Option<serde_json::Value>,
    email: Option<String>,
}

impl OAuth2DeviceProvider {
    async fn identity(&self, tok: &RawToken) -> anyhow::Result<Identity> {
        let claims: Claims = if let Some(url) = &self.userinfo_url {
            let at = tok.access_token.as_deref().ok_or_else(|| anyhow::anyhow!("no access_token"))?;
            self.http
                .get(url)
                .bearer_auth(at)
                .header("accept", "application/json")
                .header("user-agent", "ingest-api")
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?
        } else {
            // id_token came straight from the token endpoint over TLS, so its
            // payload is trusted without verifying the signature.
            let idt = tok.id_token.as_deref().ok_or_else(|| anyhow::anyhow!("no id_token and no OAUTH_USERINFO_URL"))?;
            let payload = idt.split('.').nth(1).ok_or_else(|| anyhow::anyhow!("malformed id_token"))?;
            let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload.trim_end_matches('='))?;
            serde_json::from_slice(&bytes)?
        };
        let subject = claims
            .sub
            .or(claims.id) // GitHub /user returns numeric `id`
            .map(|v| match v {
                serde_json::Value::String(s) => s,
                other => other.to_string(),
            })
            .ok_or_else(|| anyhow::anyhow!("identity has no sub/id"))?;
        Ok(Identity { subject, email: claims.email })
    }
}

impl DeviceAuthProvider for OAuth2DeviceProvider {
    fn name(&self) -> &str {
        &self.name
    }
    fn start(&self) -> BoxFut<'_, DeviceStart> {
        Box::pin(async move {
            let mut form = vec![("client_id", self.client_id.as_str()), ("scope", self.scope.as_str())];
            if let Some(s) = &self.client_secret {
                form.push(("client_secret", s));
            }
            let raw: RawDeviceStart = self
                .http
                .post(&self.device_authorization_url)
                .header("accept", "application/json")
                .form(&form)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            Ok(DeviceStart {
                device_code: raw.device_code,
                user_code: raw.user_code,
                verification_uri: raw.verification_uri,
                verification_uri_complete: raw.verification_uri_complete,
                expires_in: raw.expires_in,
                interval: raw.interval.unwrap_or(5),
            })
        })
    }
    fn poll<'a>(&'a self, device_code: &'a str) -> BoxFut<'a, PollOutcome> {
        Box::pin(async move {
            let mut form = vec![
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("device_code", device_code),
                ("client_id", self.client_id.as_str()),
            ];
            if let Some(s) = &self.client_secret {
                form.push(("client_secret", s));
            }
            let resp = self.http.post(&self.token_url).header("accept", "application/json").form(&form).send().await?;
            let tok: RawToken = resp.json().await?;
            match tok.error.as_deref() {
                Some("authorization_pending") => Ok(PollOutcome::Pending),
                Some("slow_down") => Ok(PollOutcome::SlowDown),
                Some("access_denied") => Ok(PollOutcome::Denied),
                Some("expired_token") => Ok(PollOutcome::Expired),
                Some(other) => anyhow::bail!("token endpoint error: {other}"),
                None => Ok(PollOutcome::Approved(self.identity(&tok).await?)),
            }
        })
    }
}

// ---------------------------------------------------------------- tokens

/// Random URL-safe string with `bytes` bytes of entropy.
pub fn random_token(bytes: usize) -> String {
    let mut b = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut b);
    hex::encode(b)
}

pub fn hash_token(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

#[derive(Debug, Clone, Serialize)]
pub struct IssuedTokens {
    pub access_token: String,
    pub token_type: &'static str,
    pub expires_in: u64,
    pub refresh_token: String,
    pub refresh_expires_in: u64,
    pub user_id: String,
    pub device_id: String,
}

pub async fn issue_tokens(state: &AppState, user_id: Uuid, device_id: Uuid) -> ApiResult<IssuedTokens> {
    let access = format!("gca_{}", random_token(32));
    let refresh = format!("gcr_{}", random_token(32));
    let now = Utc::now();
    let a_ttl = state.cfg.access_token_ttl_secs;
    let r_ttl = state.cfg.refresh_token_ttl_secs;
    let mut tx = state.db.begin().await?;
    for (tok, kind, ttl) in [(&access, "access", a_ttl), (&refresh, "refresh", r_ttl)] {
        sqlx::query("INSERT INTO tokens (token_hash, kind, user_id, device_id, expires_at) VALUES ($1, $2, $3, $4, $5)")
            .bind(hash_token(tok))
            .bind(kind)
            .bind(user_id)
            .bind(device_id)
            .bind(now + ChronoDuration::seconds(ttl as i64))
            .execute(&mut *tx)
            .await?;
    }
    // Opportunistic cleanup of long-expired tokens.
    sqlx::query("DELETE FROM tokens WHERE expires_at < now() - interval '7 days'").execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(IssuedTokens {
        access_token: access,
        token_type: "Bearer",
        expires_in: a_ttl,
        refresh_token: refresh,
        refresh_expires_in: r_ttl,
        user_id: user_id.to_string(),
        device_id: device_id.to_string(),
    })
}

/// Upsert the user for a provider identity and the named device.
pub async fn upsert_user_device(state: &AppState, ident: &Identity, device_name: &str) -> ApiResult<(Uuid, Uuid)> {
    let provider = state.provider.name().to_string();
    let user_id: Uuid = sqlx::query_scalar(
        "INSERT INTO users (id, provider, subject, email) VALUES ($1, $2, $3, $4)
         ON CONFLICT (provider, subject) DO UPDATE SET email = COALESCE(EXCLUDED.email, users.email)
         RETURNING id",
    )
    .bind(Uuid::new_v4())
    .bind(&provider)
    .bind(&ident.subject)
    .bind(&ident.email)
    .fetch_one(&state.db)
    .await?;
    let device_id: Uuid = sqlx::query_scalar(
        "INSERT INTO devices (id, user_id, name) VALUES ($1, $2, $3)
         ON CONFLICT (user_id, name) DO UPDATE SET last_seen_at = now()
         RETURNING id",
    )
    .bind(Uuid::new_v4())
    .bind(user_id)
    .bind(device_name)
    .fetch_one(&state.db)
    .await?;
    Ok((user_id, device_id))
}

/// Exchange (and rotate) a refresh token.
pub async fn refresh(state: &AppState, refresh_token: &str) -> ApiResult<IssuedTokens> {
    let row: Option<(Uuid, Option<Uuid>)> = sqlx::query_as(
        "UPDATE tokens SET revoked_at = now()
         WHERE token_hash = $1 AND kind = 'refresh' AND revoked_at IS NULL AND expires_at > now()
         RETURNING user_id, device_id",
    )
    .bind(hash_token(refresh_token))
    .fetch_optional(&state.db)
    .await?;
    let Some((user_id, Some(device_id))) = row else {
        return Err(ApiError::bad_request("invalid_grant", "refresh token invalid or expired"));
    };
    issue_tokens(state, user_id, device_id).await
}

/// Authenticated caller (valid, unexpired access token).
#[derive(Debug, Clone, Copy)]
pub struct AuthUser {
    pub user_id: Uuid,
    pub device_id: Option<Uuid>,
}

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let header = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| ApiError::unauthorized("missing bearer token"))?;
        let token = header
            .strip_prefix("Bearer ")
            .or_else(|| header.strip_prefix("bearer "))
            .ok_or_else(|| ApiError::unauthorized("expected Bearer token"))?;
        let row: Option<(Uuid, Option<Uuid>)> = sqlx::query_as(
            "SELECT user_id, device_id FROM tokens
             WHERE token_hash = $1 AND kind = 'access' AND revoked_at IS NULL AND expires_at > now()",
        )
        .bind(hash_token(token.trim()))
        .fetch_optional(&state.db)
        .await?;
        let (user_id, device_id) = row.ok_or_else(|| ApiError::unauthorized("token invalid or expired"))?;
        Ok(AuthUser { user_id, device_id })
    }
}
