/// Conservative fixed L1 data posting cost on Arbitrum, in USD.
///
/// Post EIP-4844, Arbitrum L1 data posting uses blobs which cost
/// ~$0.0001-0.001 per tx. We use $0.005 as a conservative upper bound.
///
/// We use the same constant in both simulation and receipt accounting so net
/// profit filtering and realized PnL stay on the same footing.
pub const ARBITRUM_L1_DATA_FEE_USD: f64 = 0.005;

/// Best-effort ETH/USD price used for gas-cost estimation.
pub fn eth_price_usd() -> f64 {
    let eth_price = crate::protocols::radiant::CACHED_ETH_PRICE_CENTS
        .load(std::sync::atomic::Ordering::Relaxed) as f64
        / 100.0;
    if eth_price > 100.0 {
        eth_price
    } else {
        3500.0
    }
}

/// Estimate the total gas cost on Arbitrum in USD.
pub fn arbitrum_gas_cost_usd(gas_used: u64, gas_price_wei: u128) -> f64 {
    let gas_cost_eth = gas_used as f64 * gas_price_wei as f64 / 1e18;
    gas_cost_eth * eth_price_usd() + ARBITRUM_L1_DATA_FEE_USD
}

/// Convenience helper for simulation code that works in gwei instead of wei.
pub fn arbitrum_gas_cost_usd_from_gwei(gas_used: u64, gas_price_gwei: f64) -> f64 {
    let gas_price_wei = (gas_price_gwei * 1e9).round() as u128;
    arbitrum_gas_cost_usd(gas_used, gas_price_wei)
}
