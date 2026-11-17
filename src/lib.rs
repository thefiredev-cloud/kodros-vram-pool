//! vram-pool: one OpenAI-compatible endpoint in front of every device's VRAM or unified memory.

#![forbid(unsafe_code)]

pub mod api;
pub mod breaker;
pub mod config;
pub mod fleet;
pub mod keys;
pub mod probe;
pub mod proxy;
pub mod route;
pub mod state;
pub mod swap;
pub mod ui;
pub mod util;

use std::sync::Arc;

/// Serves the pool on an already-bound listener until `shutdown` resolves.
pub async fn serve(
    pool: Arc<state::Pool>,
    listener: tokio::net::TcpListener,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    axum::serve(listener, api::router(pool))
        .with_graceful_shutdown(shutdown)
        .await
}
