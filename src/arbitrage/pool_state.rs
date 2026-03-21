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

    /// Algebra V2 pool interface (used by Camelot V3).
    /// globalState() returns sqrtPriceX96, tick, fee, etc. in one call.
    interface IAlgebraPool {
        function globalState() external view returns (
            uint160 sqrtPriceX96,
            int24 tick,
            uint16 fee,
            uint16 timepointIndex,
            uint8 communityFeeToken0,
            uint16 communityFeeToken1,
            bool unlocked
        );
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
/// Also supports Algebra V2 pools (Camelot V3) which use `globalState()`.
pub struct PoolStateCache {
    states: DashMap<Address, UniV3PoolState>,
    pool_addresses: Vec<Address>,
    /// Pools that use Algebra `globalState()` instead of UniV3 `slot0()` + `fee()`.
    algebra_pools: DashMap<Address, ()>,
}

impl PoolStateCache {
    pub fn new(pool_addresses: Vec<Address>) -> Self {
        Self {
            states: DashMap::new(),
            pool_addresses,
            algebra_pools: DashMap::new(),
        }
    }

    /// Get the cached state for a pool address.
    pub fn get(&self, addr: &Address) -> Option<UniV3PoolState> {
        self.states.get(addr).map(|v| v.clone())
    }

    /// Check if an address is one of our monitored pools.
    pub fn contains(&self, addr: &Address) -> bool {
        self.states.contains_key(addr)
    }

    /// Initialize pool states by reading static fields (token0, token1, fee)
    /// and current slot0 + liquidity via Multicall.
    /// Auto-detects Algebra pools (Camelot V3) by trying slot0+fee first,
    /// falling back to globalState() if those fail.
    pub async fn initialize<P: Provider + Send + Sync>(&self, provider: &P) -> Result<()> {
        let mc = Multicall::new(provider);

        // 6 calls per pool: token0, token1, fee, slot0, liquidity, globalState
        let mut calls = Vec::with_capacity(self.pool_addresses.len() * 6);
        for addr in &self.pool_addresses {
            calls.push((*addr, IUniV3Pool::token0Call {}.abi_encode()));       // 0
            calls.push((*addr, IUniV3Pool::token1Call {}.abi_encode()));       // 1
            calls.push((*addr, IUniV3Pool::feeCall {}.abi_encode()));          // 2
            calls.push((*addr, IUniV3Pool::slot0Call {}.abi_encode()));        // 3
            calls.push((*addr, IUniV3Pool::liquidityCall {}.abi_encode()));    // 4
            calls.push((*addr, IAlgebraPool::globalStateCall {}.abi_encode()));// 5
        }

        let results = mc
            .aggregate3(calls)
            .await
            .wrap_err("Pool state init multicall failed")?;

        for (i, addr) in self.pool_addresses.iter().enumerate() {
            let base = i * 6;

            if base + 5 >= results.len() {
                warn!(pool = %addr, "Incomplete multicall results, skipping");
                continue;
            }

            // token0 and token1 must succeed (shared by both UniV3 and Algebra)
            if !results[base].success || !results[base + 1].success {
                warn!(pool = %addr, "Failed to read token0/token1, skipping");
                continue;
            }

            let token0: Address =
                match IUniV3Pool::token0Call::abi_decode_returns(&results[base].return_data) {
                    Ok(ret) => ret,
                    Err(e) => {
                        warn!(pool = %addr, error = %e, "Failed to decode token0");
                        continue;
                    }
                };

            let token1: Address =
                match IUniV3Pool::token1Call::abi_decode_returns(&results[base + 1].return_data) {
                    Ok(ret) => ret,
                    Err(e) => {
                        warn!(pool = %addr, error = %e, "Failed to decode token1");
                        continue;
                    }
                };

            // Try UniV3 path first: fee() + slot0()
            let univ3_ok = results[base + 2].success && results[base + 3].success;

            let (sqrt_price_x96, tick, fee, liquidity, is_algebra) = if univ3_ok {
                // UniV3 / SushiV3 / PancakeSwapV3
                let fee = IUniV3Pool::feeCall::abi_decode_returns(&results[base + 2].return_data)
                    .map(|r| r.to::<u32>())
                    .unwrap_or(0);
                let slot0 =
                    match IUniV3Pool::slot0Call::abi_decode_returns(&results[base + 3].return_data) {
                        Ok(ret) => ret,
                        Err(e) => {
                            warn!(pool = %addr, error = %e, "Failed to decode slot0");
                            continue;
                        }
                    };
                let liq: u128 = if results[base + 4].success {
                    IUniV3Pool::liquidityCall::abi_decode_returns(&results[base + 4].return_data)
                        .unwrap_or(0)
                } else {
                    0
                };
                (U256::from(slot0.sqrtPriceX96), slot0.tick.unchecked_into(), fee, liq, false)
            } else if results[base + 5].success {
                // Algebra pool (Camelot V3): use globalState()
                let gs = match IAlgebraPool::globalStateCall::abi_decode_returns(
                    &results[base + 5].return_data,
                ) {
                    Ok(ret) => ret,
                    Err(e) => {
                        warn!(pool = %addr, error = %e, "Failed to decode globalState");
                        continue;
                    }
                };
                let liq: u128 = if results[base + 4].success {
                    IUniV3Pool::liquidityCall::abi_decode_returns(&results[base + 4].return_data)
                        .unwrap_or(0)
                } else {
                    0
                };
                (U256::from(gs.sqrtPriceX96), gs.tick.unchecked_into(), gs.fee as u32, liq, true)
            } else {
                warn!(pool = %addr, "Neither slot0 nor globalState succeeded, skipping");
                continue;
            };

            if is_algebra {
                self.algebra_pools.insert(*addr, ());
                debug!(pool = %addr, fee, "Detected Algebra pool (Camelot V3)");
            }

            self.states.insert(
                *addr,
                UniV3PoolState {
                    sqrt_price_x96,
                    tick,
                    liquidity,
                    fee,
                    token0,
                    token1,
                },
            );

            debug!(
                pool = %addr,
                sqrt_price = %sqrt_price_x96,
                tick,
                liquidity,
                fee,
                algebra = is_algebra,
                "Pool state initialized"
            );
        }

        Ok(())
    }

    /// Refresh price and liquidity for all monitored pools via Multicall.
    /// Uses slot0() for UniV3 pools and globalState() for Algebra pools.
    /// Also updates fee for Algebra pools (dynamic fees).
    pub async fn refresh<P: Provider + Send + Sync>(&self, provider: &P) -> Result<()> {
        let mc = Multicall::new(provider);

        // For each pool: send the appropriate price call + liquidity
        let mut calls = Vec::with_capacity(self.pool_addresses.len() * 2);
        for addr in &self.pool_addresses {
            if self.algebra_pools.contains_key(addr) {
                calls.push((*addr, IAlgebraPool::globalStateCall {}.abi_encode()));
            } else {
                calls.push((*addr, IUniV3Pool::slot0Call {}.abi_encode()));
            }
            calls.push((*addr, IUniV3Pool::liquidityCall {}.abi_encode()));
        }

        let results = mc
            .aggregate3(calls)
            .await
            .wrap_err("Pool state refresh multicall failed")?;

        for (i, addr) in self.pool_addresses.iter().enumerate() {
            let base = i * 2;
            if base + 1 >= results.len() || !results[base].success {
                continue;
            }

            let is_algebra = self.algebra_pools.contains_key(addr);
            let (sqrt_price_x96, tick, fee_update) = if is_algebra {
                match IAlgebraPool::globalStateCall::abi_decode_returns(&results[base].return_data) {
                    Ok(gs) => (U256::from(gs.sqrtPriceX96), gs.tick.unchecked_into(), Some(gs.fee as u32)),
                    Err(_) => continue,
                }
            } else {
                match IUniV3Pool::slot0Call::abi_decode_returns(&results[base].return_data) {
                    Ok(s) => (U256::from(s.sqrtPriceX96), s.tick.unchecked_into(), None),
                    Err(_) => continue,
                }
            };

            let liquidity: u128 = if results[base + 1].success {
                match IUniV3Pool::liquidityCall::abi_decode_returns(&results[base + 1].return_data)
                {
                    Ok(v) => v,
                    Err(_) => 0,
                }
            } else {
                0
            };

            if let Some(mut state) = self.states.get_mut(addr) {
                state.sqrt_price_x96 = sqrt_price_x96;
                state.tick = tick;
                state.liquidity = liquidity;
                if let Some(fee) = fee_update {
                    state.fee = fee;
                }
            }
        }

        Ok(())
    }
}
