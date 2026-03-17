use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Json};
use axum::routing::get;
use axum::Router;
use serde_json::Value;
use tower_http::cors::CorsLayer;
use tracing::info;

use crate::arbitrage::dashboard::ArbDashboard;
use crate::utils::metrics::Metrics;

/// Shared state accessible by all HTTP handlers.
#[derive(Clone)]
pub struct AppState {
    pub metrics: Metrics,
    pub arb_dashboard: ArbDashboard,
    pub python_dashboard_base: Arc<str>,
}

/// Start the dashboard web server on the given port.
pub async fn start_dashboard(
    metrics: Metrics,
    arb_dashboard: ArbDashboard,
    port: u16,
    python_dashboard_port: u16,
) -> eyre::Result<()> {
    let state = Arc::new(AppState {
        metrics,
        arb_dashboard,
        python_dashboard_base: Arc::<str>::from(format!(
            "http://127.0.0.1:{python_dashboard_port}"
        )),
    });

    let app = Router::new()
        .route("/", get(serve_dashboard))
        .route("/api/metrics", get(api_metrics))
        .route("/api/health", get(api_health))
        .route("/api/arb", get(api_arb))
        .route("/api/latency", get(api_latency))
        .route("/api/chain", get(api_chain))
        .layer(CorsLayer::permissive())
        .with_state(state);

    // Bind to 127.0.0.1 by default for security (use SSH tunnel for remote access)
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
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

/// Return current arbitrage dashboard state.
async fn api_arb(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(state.arb_dashboard.to_json())
}

async fn proxy_python_json(base: &str, path: &str) -> Option<Value> {
    let url = format!("{base}{path}");
    let resp = reqwest::get(url).await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json::<Value>().await.ok()
}

/// /api/latency — prefer the Python latency probe service, fall back to an empty payload.
async fn api_latency(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(
        proxy_python_json(state.python_dashboard_base.as_ref(), "/api/latency")
            .await
            .unwrap_or_else(|| {
                serde_json::json!({
                    "probes": {},
                    "history": {"read": [], "sequencer": [], "total": []}
                })
            }),
    )
}

/// /api/chain — prefer the Python chain-data service, fall back to an empty payload.
async fn api_chain(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(
        proxy_python_json(state.python_dashboard_base.as_ref(), "/api/chain")
            .await
            .unwrap_or_else(|| {
                serde_json::json!({
                    "market_liquidations": [],
                    "competitors": [],
                    "near_liquidation": [],
                    "last_updated": 0
                })
            }),
    )
}
