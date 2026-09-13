//! Weavedraw server library: room registry, WebSocket sessions, persistence.
//!
//! The binary in `main.rs` is a thin wrapper; everything here is also driven
//! by the integration tests in `tests/`.

pub mod config;
pub mod registry;
pub mod room;
pub mod ws;

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::routing::get;
use tokio::sync::watch;

pub use config::Config;
pub use registry::Registry;

#[derive(Clone)]
pub struct AppState {
    pub registry: Arc<Registry>,
    /// Flips to `true` on shutdown so long-lived sessions close promptly.
    pub shutdown: watch::Receiver<bool>,
}

/// Build the axum router. Routes:
/// - `GET /ws`      — WebSocket upgrade; the room is chosen in `Hello`.
/// - `GET /rooms`   — JSON list of open rooms.
/// - `GET /health`  — liveness probe.
pub fn app(registry: Arc<Registry>, shutdown: watch::Receiver<bool>) -> Router {
    Router::new()
        .route("/ws", get(ws::ws_handler))
        .route("/rooms", get(list_rooms))
        .route("/health", get(|| async { "ok" }))
        .with_state(AppState { registry, shutdown })
}

async fn list_rooms(State(state): State<AppState>) -> axum::Json<Vec<registry::RoomInfo>> {
    axum::Json(state.registry.list().await)
}

/// Periodically write dirty rooms to disk until the shutdown flag flips.
pub async fn flush_loop(registry: Arc<Registry>, mut shutdown: watch::Receiver<bool>) {
    let mut ticker = tokio::time::interval(registry.config().flush_interval);
    loop {
        tokio::select! {
            _ = ticker.tick() => { registry.flush_all().await; }
            _ = shutdown.changed() => break,
        }
    }
}
