use alloy::network::{Ethereum, EthereumWallet};
use alloy::providers::fillers::{
    BlobGasFiller, ChainIdFiller, FillProvider, GasFiller, JoinFill, NonceFiller, WalletFiller,
};
use alloy::providers::{Provider, ProviderBuilder, RootProvider, WsConnect};
use eyre::{Context, Result};
use tracing::info;

#[cfg(unix)]
use alloy::providers::IpcConnect;

/// Concrete type for a signed HTTP provider (with wallet filler).
/// This is the type returned by `ProviderBuilder::new().wallet(w).connect_http(url)`.
pub type SignedHttpProvider = FillProvider<
    JoinFill<
        JoinFill<
            alloy::providers::Identity,
            JoinFill<GasFiller, JoinFill<BlobGasFiller, JoinFill<NonceFiller, ChainIdFiller>>>,
        >,
        WalletFiller<EthereumWallet>,
    >,
    RootProvider,
    Ethereum,
>;

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

/// Create an HTTP provider with a wallet signer attached.
///
/// This provider can send signed transactions. Used for the execution provider
/// that submits liquidation transactions to the sequencer.
pub fn create_signed_http_provider(
    http_url: &str,
    wallet: EthereumWallet,
) -> Result<SignedHttpProvider> {
    info!(url = http_url, "Creating signed HTTP provider");
    let url = http_url.parse().wrap_err("Invalid HTTP RPC URL for signed provider")?;
    let provider = ProviderBuilder::new()
        .wallet(wallet)
        .connect_http(url);
    Ok(provider)
}

/// Create an IPC provider for lowest-latency local node access (<0.3ms).
///
/// Falls back to HTTP if IPC connection fails.
#[cfg(unix)]
pub async fn create_ipc_provider(ipc_path: &str) -> Result<RootProvider> {
    info!(path = ipc_path, "Connecting to IPC");
    let ipc_connect: IpcConnect<std::path::PathBuf> = IpcConnect::new(ipc_path.into());
    let provider = ProviderBuilder::default()
        .connect_ipc(ipc_connect)
        .await
        .wrap_err_with(|| format!("Failed to connect IPC at {}", ipc_path))?;
    let chain_id = provider.get_chain_id().await.wrap_err("Failed to fetch chain ID via IPC")?;
    info!(chain_id, "IPC provider connected");
    Ok(provider)
}

/// Create the best available read provider: IPC > HTTP.
pub async fn create_best_read_provider(
    http_url: &str,
    ipc_path: Option<&str>,
) -> Result<RootProvider> {
    #[cfg(unix)]
    if let Some(path) = ipc_path {
        match create_ipc_provider(path).await {
            Ok(provider) => {
                info!("Using IPC provider for reads (fastest)");
                return Ok(provider);
            }
            Err(e) => {
                tracing::warn!(error = %e, "IPC not available, falling back to HTTP");
            }
        }
    }
    create_http_provider(http_url)
}

/// Fetch the latest block number from the provider.
pub async fn get_latest_block_number<P: Provider>(provider: &P) -> Result<u64> {
    let block_number = provider
        .get_block_number()
        .await
        .wrap_err("Failed to fetch latest block number")?;
    Ok(block_number)
}
