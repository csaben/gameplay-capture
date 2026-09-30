//! ingest-api: presigned uploads, device auth, session metadata, blocklist, deletion.
//!
//! See README.md for endpoints, configuration and the deletion contract with
//! the shard pipeline.

pub mod auth;
pub mod config;
pub mod deletion;
pub mod error;
pub mod routes;
pub mod storage;

use std::sync::Arc;

pub use config::Config;
pub use error::ApiError;

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Shared handler state.
#[derive(Clone)]
pub struct AppState {
    pub db: sqlx::PgPool,
    pub cfg: Arc<Config>,
    pub storage: Arc<storage::Storage>,
    pub provider: Arc<dyn auth::DeviceAuthProvider>,
    pub deletion_notify: Arc<tokio::sync::Notify>,
}

impl AppState {
    /// Connect to Postgres, run migrations, build storage and the auth provider.
    pub async fn from_config(cfg: Config) -> anyhow::Result<Self> {
        let db = sqlx::postgres::PgPoolOptions::new()
            .max_connections(cfg.db_max_connections)
            .connect(&cfg.database_url)
            .await?;
        MIGRATOR.run(&db).await?;
        let storage = storage::Storage::from_config(&cfg)?;
        let provider = auth::provider_from_config(&cfg)?;
        Ok(Self {
            db,
            cfg: Arc::new(cfg),
            storage: Arc::new(storage),
            provider,
            deletion_notify: Arc::new(tokio::sync::Notify::new()),
        })
    }
}

/// The full router.
pub fn router(state: AppState) -> axum::Router {
    routes::router(state)
}

/// Serve on `listener` and run the deletion worker until the future is dropped.
pub async fn serve(state: AppState, listener: tokio::net::TcpListener) -> anyhow::Result<()> {
    let worker = tokio::spawn(deletion::run_worker(state.clone()));
    let res = axum::serve(listener, router(state)).await;
    worker.abort();
    Ok(res?)
}
