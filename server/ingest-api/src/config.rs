use clap::Parser;

/// All settings come from flags or environment variables.
#[derive(Debug, Clone, Parser)]
#[command(name = "ingest-api", about = "Presigned uploads, device auth and metadata for gameplay capture")]
pub struct Config {
    #[arg(long, env = "BIND_ADDR", default_value = "127.0.0.1:8080")]
    pub bind: String,
    #[arg(long, env = "DATABASE_URL")]
    pub database_url: String,
    #[arg(long, env = "DB_MAX_CONNECTIONS", default_value_t = 10)]
    pub db_max_connections: u32,

    // --- object storage (Garage / R2) ---
    /// S3 endpoint the server uses for HEAD/DELETE/LIST.
    #[arg(long, env = "S3_ENDPOINT")]
    pub s3_endpoint: String,
    /// Endpoint put into presigned URLs (what clients can reach). Defaults to S3_ENDPOINT.
    #[arg(long, env = "S3_PUBLIC_ENDPOINT")]
    pub s3_public_endpoint: Option<String>,
    #[arg(long, env = "S3_REGION", default_value = "garage")]
    pub s3_region: String,
    #[arg(long, env = "S3_BUCKET", default_value = "gameplay")]
    pub s3_bucket: String,
    #[arg(long, env = "S3_ACCESS_KEY")]
    pub s3_access_key: String,
    #[arg(long, env = "S3_SECRET_KEY")]
    pub s3_secret_key: String,
    #[arg(long, env = "S3_ALLOW_HTTP", default_value_t = false)]
    pub s3_allow_http: bool,
    #[arg(long, env = "PRESIGN_TTL_SECS", default_value_t = 900)]
    pub presign_ttl_secs: u64,

    // --- auth ---
    /// `dev` (auto-approves; loopback-only, local testing) or `oauth2`.
    #[arg(long, env = "AUTH_PROVIDER", default_value = "dev")]
    pub auth_provider: String,
    /// Subject every dev-provider login maps to.
    #[arg(long, env = "DEV_SUBJECT", default_value = "dev")]
    pub dev_subject: String,
    /// Short name stored in users.provider for oauth2 users, e.g. `google`.
    #[arg(long, env = "OAUTH_NAME", default_value = "oauth2")]
    pub oauth_name: String,
    #[arg(long, env = "OAUTH_DEVICE_AUTHORIZATION_URL")]
    pub oauth_device_authorization_url: Option<String>,
    #[arg(long, env = "OAUTH_TOKEN_URL")]
    pub oauth_token_url: Option<String>,
    /// If unset, identity is read from the `id_token` returned by the token endpoint.
    #[arg(long, env = "OAUTH_USERINFO_URL")]
    pub oauth_userinfo_url: Option<String>,
    #[arg(long, env = "OAUTH_CLIENT_ID")]
    pub oauth_client_id: Option<String>,
    #[arg(long, env = "OAUTH_CLIENT_SECRET")]
    pub oauth_client_secret: Option<String>,
    #[arg(long, env = "OAUTH_SCOPE", default_value = "openid email")]
    pub oauth_scope: String,
    #[arg(long, env = "ACCESS_TOKEN_TTL_SECS", default_value_t = 3600)]
    pub access_token_ttl_secs: u64,
    #[arg(long, env = "REFRESH_TOKEN_TTL_SECS", default_value_t = 30 * 24 * 3600)]
    pub refresh_token_ttl_secs: u64,

    // --- client policy (GET /config) ---
    #[arg(long, env = "CONSENT_VERSION", default_value = "1")]
    pub consent_version: String,
    #[arg(long, env = "MIN_CLIENT_VERSION", default_value = "0.1.0")]
    pub min_client_version: String,
    #[arg(long, env = "ALLOWED_ENCODERS", value_delimiter = ',',
          default_value = "hevc_nvenc,hevc_amf,hevc_qsv,hevc_vaapi,hevc_videotoolbox,av1_nvenc")]
    pub allowed_encoders: Vec<String>,
    #[arg(long, env = "RATE_HZ", default_value_t = 20)]
    pub rate_hz: u32,
    #[arg(long, env = "WIDTH", default_value_t = 640)]
    pub width: u32,
    #[arg(long, env = "HEIGHT", default_value_t = 360)]
    pub height: u32,
    /// Record only games whose `games.status` is `allowed`.
    #[arg(long, env = "DEFAULT_DENY", default_value_t = false)]
    pub default_deny: bool,
    /// Upper bound per file accepted by the upload endpoint.
    #[arg(long, env = "MAX_FILE_BYTES", default_value_t = 4 * 1024 * 1024 * 1024)]
    pub max_file_bytes: u64,

    #[arg(long, env = "DELETION_POLL_SECS", default_value_t = 30)]
    pub deletion_poll_secs: u64,
}
