use clap::Parser;
use ingest_api::{AppState, Config};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();
    let cfg = Config::parse();
    let bind = cfg.bind.clone();
    let state = AppState::from_config(cfg).await?;
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!(addr = %listener.local_addr()?, "ingest-api listening");
    ingest_api::serve(state, listener).await
}
