//! Client for `ingest-api` (Phase 2): device login, sessions, presigned uploads.
//!
//! The wire types here are the contract with `server/ingest-api`.

use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;

pub const DEVICE_CODE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
pub const REFRESH_GRANT: &str = "refresh_token";

/// Body of `POST /auth/device`. Without `grant_type` it starts a device-code
/// login; with [`DEVICE_CODE_GRANT`] + `device_code` it polls; with
/// [`REFRESH_GRANT`] + `refresh_token` it exchanges a refresh token.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeviceAuthRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Human-readable machine name, recorded in `devices`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification_uri_complete: Option<String>,
    pub expires_in: u64,
    pub interval: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub token_type: String,
    pub expires_in: u64,
    pub refresh_token: String,
    pub refresh_expires_in: u64,
    pub user_id: String,
    pub device_id: String,
}

/// OAuth-style error body (`{"error": "authorization_pending"}`); also used for
/// every other API error.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiError {
    pub error: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSessionRequest {
    /// Client-chosen id (the local `sessions/<session_id>` folder name). The
    /// server generates one if absent. Idempotent for the same user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub game_id: String,
    pub client_version: String,
    pub consent_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionResponse {
    pub session_id: String,
    pub user_id: String,
    /// `raw/<user_id>/<session_id>/`
    pub key_prefix: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileInfo {
    pub name: String,
    pub size: u64,
    pub blake3: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadRequest {
    pub files: Vec<FileInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresignedFile {
    pub name: String,
    pub key: String,
    pub url: String,
    pub method: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadResponse {
    pub expires_in: u64,
    pub files: Vec<PresignedFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompleteRequest {
    pub files: Vec<FileInfo>,
    pub dropped_frames: u64,
    pub frame_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompleteResponse {
    pub segment_id: String,
    pub state: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteResponse {
    pub deletion_id: String,
    pub segments: u64,
    pub status: String,
}

/// Small typed client over `ingest-api`.
#[derive(Clone)]
pub struct ApiClient {
    http: reqwest::Client,
    base: String,
}

impl ApiClient {
    pub fn new(api_base: impl Into<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .build()
            .map_err(|e| Error::Http(e.to_string()))?;
        Ok(Self { http, base: api_base.into().trim_end_matches('/').to_string() })
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    async fn send_json<T: for<'de> Deserialize<'de>>(&self, rb: reqwest::RequestBuilder) -> Result<T> {
        let resp = rb.send().await.map_err(|e| Error::Http(e.to_string()))?;
        decode(resp).await
    }

    /// Start a device-code login.
    pub async fn device_start(&self, device_name: &str) -> Result<DeviceCode> {
        let body = DeviceAuthRequest { device_name: Some(device_name.into()), ..Default::default() };
        self.send_json(self.http.post(self.url("/auth/device")).json(&body)).await
    }

    /// Poll once. `Ok(None)` while authorization is pending.
    pub async fn device_poll(&self, device_code: &str, device_name: &str) -> Result<Option<TokenResponse>> {
        let body = DeviceAuthRequest {
            grant_type: Some(DEVICE_CODE_GRANT.into()),
            device_code: Some(device_code.into()),
            device_name: Some(device_name.into()),
            ..Default::default()
        };
        match self.send_json(self.http.post(self.url("/auth/device")).json(&body)).await {
            Ok(t) => Ok(Some(t)),
            Err(Error::Api { ref code, .. }) if code == "authorization_pending" || code == "slow_down" => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Full device login: start, show the code via `prompt`, poll until approved.
    pub async fn device_login(&self, device_name: &str, prompt: impl FnOnce(&DeviceCode)) -> Result<TokenResponse> {
        let code = self.device_start(device_name).await?;
        prompt(&code);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(code.expires_in.max(1));
        let interval = Duration::from_secs(code.interval.max(1));
        loop {
            if let Some(t) = self.device_poll(&code.device_code, device_name).await? {
                return Ok(t);
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(Error::Api { status: 400, code: "expired_token".into(), message: "device code expired".into() });
            }
            tokio::time::sleep(interval).await;
        }
    }

    /// Exchange a refresh token for a new access token.
    pub async fn refresh(&self, refresh_token: &str) -> Result<TokenResponse> {
        let body = DeviceAuthRequest {
            grant_type: Some(REFRESH_GRANT.into()),
            refresh_token: Some(refresh_token.into()),
            ..Default::default()
        };
        self.send_json(self.http.post(self.url("/auth/device")).json(&body)).await
    }

    pub async fn config(&self) -> Result<serde_json::Value> {
        self.send_json(self.http.get(self.url("/config"))).await
    }

    pub async fn create_session(&self, token: &str, req: &CreateSessionRequest) -> Result<SessionResponse> {
        self.send_json(self.http.post(self.url("/sessions")).bearer_auth(token).json(req)).await
    }

    pub async fn request_upload(&self, token: &str, session_id: &str, idx: u32, req: &UploadRequest) -> Result<UploadResponse> {
        let url = self.url(&format!("/sessions/{session_id}/segments/{idx}/upload"));
        self.send_json(self.http.post(url).bearer_auth(token).json(req)).await
    }

    pub async fn complete(&self, token: &str, session_id: &str, idx: u32, req: &CompleteRequest) -> Result<CompleteResponse> {
        let url = self.url(&format!("/sessions/{session_id}/segments/{idx}/complete"));
        self.send_json(self.http.post(url).bearer_auth(token).json(req)).await
    }

    pub async fn delete_my_data(&self, token: &str) -> Result<DeleteResponse> {
        self.send_json(self.http.delete(self.url("/me/data")).bearer_auth(token)).await
    }
}

async fn decode<T: for<'de> Deserialize<'de>>(resp: reqwest::Response) -> Result<T> {
    let status = resp.status();
    let bytes = resp.bytes().await.map_err(|e| Error::Http(e.to_string()))?;
    if status.is_success() {
        return serde_json::from_slice(&bytes)
            .map_err(|e| Error::Http(format!("bad response body ({e}): {}", String::from_utf8_lossy(&bytes))));
    }
    let (code, message) = match serde_json::from_slice::<ApiError>(&bytes) {
        Ok(e) => (e.error, e.error_description.unwrap_or_default()),
        Err(_) => ("http_error".to_string(), String::from_utf8_lossy(&bytes).into_owned()),
    };
    Err(Error::Api { status: status.as_u16(), code, message })
}
