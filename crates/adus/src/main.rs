use std::sync::Arc;

use adus_core::{config::Config, crypto::Cipher};
use adus_server::{AppState, router};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = Config::from_env()?;
    let cipher = Cipher::from_base64_key(&config.secret_key)?;
    let db = adus_db::connect(&config.database_url).await?;
    let listener = tokio::net::TcpListener::bind(&config.bind_addr).await?;
    tracing::info!("listening on {}", config.bind_addr);
    let state = AppState {
        db,
        cipher: Arc::new(cipher),
        config: Arc::new(config),
    };
    axum::serve(listener, router(state)).await?;
    Ok(())
}
