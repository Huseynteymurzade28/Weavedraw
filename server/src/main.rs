//! Weavedraw WebSocket server — room-based broadcasting with snapshot persistence.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use server::{Config, Registry, app, flush_loop};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,server=debug".into()),
        )
        .init();

    let config = Config::from_env()?;
    tokio::fs::create_dir_all(&config.data_dir)
        .await
        .with_context(|| format!("creating {}", config.data_dir.display()))?;

    let listener = TcpListener::bind(config.addr)
        .await
        .with_context(|| format!("binding {}", config.addr))?;
    info!(
        addr = %listener.local_addr()?,
        data_dir = %config.data_dir.display(),
        wire = common::codec::wire_format(),
        protocol = common::PROTOCOL_VERSION,
        "weavedraw-server listening"
    );

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let registry = Arc::new(Registry::new(config));
    let flusher = tokio::spawn(flush_loop(registry.clone(), shutdown_rx.clone()));

    axum::serve(
        listener,
        app(registry.clone(), shutdown_rx).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        let _ = tokio::signal::ctrl_c().await;
        info!("shutdown requested");
        let _ = shutdown_tx.send(true);
    })
    .await?;

    let _ = flusher.await;
    let written = registry.flush_all().await;
    info!(rooms_written = written, "final flush complete; bye");
    Ok(())
}
