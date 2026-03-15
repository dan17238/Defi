use alloy::providers::{Provider, ProviderBuilder, RootProvider, WsConnect};
use eyre::{Context, Result};
use tracing::info;

/// Create a WebSocket provider connected to the given URL.
///
/// The WS provider is used for block subscriptions and real-time event monitoring.
/// Uses ProviderBuilder::default() (no fillers) to get a bare RootProvider.
pub async fn create_ws_provider(ws_url: &str) -> Result<RootProvider> {
    info!(url = ws_url, "Connecting to WebSocket RPC");
    let ws_connect = WsConnect::new(ws_url.to_string());
    let provider = ProviderBuilder::default()
        .connect_ws(ws_connect)
        .await
        .wrap_err("Failed to connect WebSocket provider")?;
    let chain_id = provider.get_chain_id().await.wrap_err("Failed to fetch chain ID")?;
    info!(chain_id, "WebSocket provider connected");
    Ok(provider)
}

/// Create an HTTP provider connected to the given URL.
///
/// The HTTP provider is used for standard RPC calls and multicall batching.
/// Uses ProviderBuilder::default() (no fillers) to get a bare RootProvider.
pub fn create_http_provider(http_url: &str) -> Result<RootProvider> {
    info!(url = http_url, "Creating HTTP RPC provider");
    let url = http_url.parse().wrap_err("Invalid HTTP RPC URL")?;
    let provider = ProviderBuilder::default().connect_http(url);
    Ok(provider)
}

/// Create a dedicated HTTP provider for the Arbitrum Sequencer endpoint.
///
/// Uses a persistent HTTP connection (via reqwest's connection pooling) to
/// minimize TLS handshake overhead on repeated transaction submissions.
pub fn create_sequencer_provider(rpc_url: &str) -> Result<RootProvider> {
    info!(url = rpc_url, "Creating Sequencer RPC provider (persistent connection)");
    let url = rpc_url.parse().wrap_err("Invalid Sequencer RPC URL")?;
    let provider = ProviderBuilder::default().connect_http(url);
    Ok(provider)
}

/// Fetch the latest block number from the provider.
pub async fn get_latest_block_number<P: Provider>(provider: &P) -> Result<u64> {
    let block_number = provider
        .get_block_number()
        .await
        .wrap_err("Failed to fetch latest block number")?;
    Ok(block_number)
}
