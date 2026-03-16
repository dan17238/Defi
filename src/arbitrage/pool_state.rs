use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::sol;
use alloy::sol_types::SolCall;
use dashmap::DashMap;
use eyre::{Context, Result};
use tracing::{debug, warn};

use crate::utils::multicall::Multicall;

sol! {
    interface IUniV3Pool {
        function slot0() external view returns (
            uint160 sqrtPriceX96,
            int24 tick,
            uint16 observationIndex,
            uint16 observationCardinality,
            uint16 observationCardinalityNext,
            uint8 feeProtocol,
            bool unlocked
        );
        function token0() external view returns (address);
        function token1() external view returns (address);
        function fee() external view returns (uint24);
        function liquidity() external view returns (uint128);
    }
}

/// Cached state of a UniswapV3 pool.
#[derive(Debug, Clone)]
pub struct UniV3PoolState {
    pub sqrt_price_x96: U256,
    pub tick: i32,
    pub liquidity: u128,
    pub fee: u32,
    pub token0: Address,
    pub token1: Address,
}

/// Thread-safe cache of UniV3 pool states backed by DashMap.
/// Supports batch initialization and refresh via Multicall.
pub struct PoolStateCache {
    states: DashMap<Address, UniV3PoolState>,
    pool_addresses: Vec<Address>,
}

impl PoolStateCache {
    pub fn new(pool_addresses: Vec<Address>) -> Self {
        Self {
            states: DashMap::new(),
            pool_addresses,
        }
    }

    /// Get the cached state for a pool address.
    pub fn get(&self, addr: &Address) -> Option<UniV3PoolState> {
        self.states.get(addr).map(|v| v.clone())
    }

    /// Initialize pool states by reading static fields (token0, token1, fee)
    /// and current slot0 + liquidity via Multicall.
    pub async fn initialize<P: Provider + Send + Sync>(&self, provider: &P) -> Result<()> {
        let mc = Multicall::new(provider);

        // Build calls: for each pool read token0, token1, fee, slot0, liquidity
        let mut calls = Vec::with_capacity(self.pool_addresses.len() * 5);
        for addr in &self.pool_addresses {
            calls.push((*addr, IUniV3Pool::token0Call {}.abi_encode()));
            calls.push((*addr, IUniV3Pool::token1Call {}.abi_encode()));
            calls.push((*addr, IUniV3Pool::feeCall {}.abi_encode()));
            calls.push((*addr, IUniV3Pool::slot0Call {}.abi_encode()));
            calls.push((*addr, IUniV3Pool::liquidityCall {}.abi_encode()));
        }

        let results = mc.aggregate3(calls).await.wrap_err("Pool state init multicall failed")?;

        for (i, addr) in self.pool_addresses.iter().enumerate() {
            let base = i * 5;

            if base + 4 >= results.len() {
                warn!(pool = %addr, "Incomplete multicall results, skipping");
                continue;
            }

            if !results[base].success || !results[base + 1].success
                || !results[base + 2].success || !results[base + 3].success
                || !results[base + 4].success
            {
                warn!(pool = %addr, "Failed to read pool state, skipping");
                continue;
            }

            let token0: Address = match IUniV3Pool::token0Call::abi_decode_returns(&results[base].return_data) {
                Ok(ret) => ret,
                Err(e) => {
                    warn!(pool = %addr, error = %e, "Failed to decode token0");
                    continue;
                }
            };

            let token1: Address = match IUniV3Pool::token1Call::abi_decode_returns(&results[base + 1].return_data) {
                Ok(ret) => ret,
                Err(e) => {
                    warn!(pool = %addr, error = %e, "Failed to decode token1");
                    continue;
                }
            };

            let fee: u32 = match IUniV3Pool::feeCall::abi_decode_returns(&results[base + 2].return_data) {
                Ok(ret) => ret.to::<u32>(),
                Err(e) => {
                    warn!(pool = %addr, error = %e, "Failed to decode fee");
                    continue;
                }
            };

            let slot0 = match IUniV3Pool::slot0Call::abi_decode_returns(&results[base + 3].return_data) {
                Ok(ret) => ret,
                Err(e) => {
                    warn!(pool = %addr, error = %e, "Failed to decode slot0");
                    continue;
                }
            };

            let liquidity: u128 = match IUniV3Pool::liquidityCall::abi_decode_returns(&results[base + 4].return_data) {
                Ok(ret) => ret,
                Err(e) => {
                    warn!(pool = %addr, error = %e, "Failed to decode liquidity");
                    continue;
                }
            };

            let sqrt_price_x96 = U256::from(slot0.sqrtPriceX96);
            let tick = slot0.tick.unchecked_into();

            self.states.insert(*addr, UniV3PoolState {
                sqrt_price_x96,
                tick,
                liquidity,
                fee,
                token0,
                token1,
            });

            debug!(
                pool = %addr,
                sqrt_price = %sqrt_price_x96,
                tick,
                liquidity,
                fee,
                "Pool state initialized"
            );
        }

        Ok(())
    }

    /// Refresh slot0 and liquidity for all monitored pools via Multicall.
    /// Only updates dynamic fields (sqrtPriceX96, tick, liquidity).
    pub async fn refresh<P: Provider + Send + Sync>(&self, provider: &P) -> Result<()> {
        let mc = Multicall::new(provider);

        let mut calls = Vec::with_capacity(self.pool_addresses.len() * 2);
        for addr in &self.pool_addresses {
            calls.push((*addr, IUniV3Pool::slot0Call {}.abi_encode()));
            calls.push((*addr, IUniV3Pool::liquidityCall {}.abi_encode()));
        }

        let results = mc.aggregate3(calls).await.wrap_err("Pool state refresh multicall failed")?;

        for (i, addr) in self.pool_addresses.iter().enumerate() {
            let base = i * 2;
            if base + 1 >= results.len() || !results[base].success {
                continue;
            }

            let slot0 = match IUniV3Pool::slot0Call::abi_decode_returns(&results[base].return_data) {
                Ok(ret) => ret,
                Err(_) => continue,
            };

            let liquidity: u128 = if results[base + 1].success {
                IUniV3Pool::liquidityCall::abi_decode_returns(&results[base + 1].return_data)
                    .unwrap_or(0)
            } else {
                0
            };

            if let Some(mut state) = self.states.get_mut(addr) {
                state.sqrt_price_x96 = U256::from(slot0.sqrtPriceX96);
                state.tick = slot0.tick.unchecked_into();
                state.liquidity = liquidity;
            }
        }

        Ok(())
    }
}
