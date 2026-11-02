use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{Router, serve as axum_serve};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};

use crate::{cli::HttpOptions, mcp::FagbrevServer};

/// Serve the same tool router as `mcp`, using rmcp's Streamable HTTP transport.
pub async fn serve(options: HttpOptions) -> Result<()> {
    let bind_addr = options.bind_addr()?;
    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .with_context(|| format!("could not bind Fagbrev MCP HTTP server to {bind_addr}"))?;
    let actual_addr = listener
        .local_addr()
        .context("could not determine the HTTP server's bound address")?;

    let service = StreamableHttpService::new(
        || Ok(FagbrevServer::default()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );
    let app = Router::new().route_service("/mcp", service);

    eprintln!("Fagbrev MCP Streamable HTTP listening on http://{actual_addr}/mcp (Ctrl-C to stop)");
    crate::browser::enable_shared_browser();
    let result = axum_serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("Fagbrev MCP HTTP server stopped unexpectedly");
    crate::browser::shutdown_shared_browser().await;
    result
}

async fn shutdown_signal() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::warn!(%error, "could not install Ctrl-C handler; HTTP server will stop when its listener closes");
    }
}
