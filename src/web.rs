use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Json};
use axum::routing::get;
use axum::Router;
use tower_http::cors::CorsLayer;
use tracing::info;

use crate::utils::metrics::Metrics;

/// Shared state accessible by all HTTP handlers.
#[derive(Clone)]
pub struct AppState {
    pub metrics: Metrics,
}

/// Start the dashboard web server on the given port.
pub async fn start_dashboard(metrics: Metrics, port: u16) -> eyre::Result<()> {
    let state = Arc::new(AppState { metrics });

    let app = Router::new()
        .route("/", get(serve_dashboard))
        .route("/api/metrics", get(api_metrics))
        .route("/api/health", get(api_health))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    info!(%addr, "Dashboard server starting");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

/// Serve the embedded dashboard HTML.
async fn serve_dashboard() -> impl IntoResponse {
    Html(include_str!("../dashboard/index.html"))
}

/// Return current metrics as JSON.
async fn api_metrics(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(state.metrics.to_json())
}

/// Simple health check endpoint.
async fn api_health() -> impl IntoResponse {
    (StatusCode::OK, Json(serde_json::json!({ "status": "ok" })))
}
