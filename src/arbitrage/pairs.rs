use alloy::primitives::Address;
use eyre::{Context, Result};

use crate::config::ArbitragePairConfig;

/// A configured pair of UniV3 pools to monitor for arbitrage.
#[derive(Debug, Clone)]
pub struct PoolPair {
    pub name: String,
    pub pool_a: Address,
    pub pool_b: Address,
    pub token0: Address,
    pub token1: Address,
    pub fee_a: u32,
    pub fee_b: u32,
}

impl PoolPair {
    /// Parse a PoolPair from the config representation.
    /// Both pools must share the same token0/token1 — FlashArbitrage assumes
    /// identical token pairs and will revert if they differ.
    pub fn from_config(cfg: &ArbitragePairConfig) -> Result<Self> {
        let token0: Address = cfg.token0.parse().wrap_err("Invalid token0 address")?;
        let token1: Address = cfg.token1.parse().wrap_err("Invalid token1 address")?;

        // Validate UniV3 token ordering (token0 < token1)
        if token0 >= token1 {
            eyre::bail!(
                "Pair '{}': token0 ({}) must be < token1 ({}) per UniV3 convention",
                cfg.name,
                cfg.token0,
                cfg.token1
            );
        }

        Ok(Self {
            name: cfg.name.clone(),
            pool_a: cfg.pool_a.parse().wrap_err("Invalid pool_a address")?,
            pool_b: cfg.pool_b.parse().wrap_err("Invalid pool_b address")?,
            token0,
            token1,
            fee_a: cfg.fee_a,
            fee_b: cfg.fee_b,
        })
    }

    /// Total fee spread in basis points (1 bp = 0.01%).
    /// Both fees are in UniV3 units (e.g., 500 = 0.05%, 3000 = 0.30%).
    pub fn total_fee_bps(&self) -> f64 {
        (self.fee_a + self.fee_b) as f64 / 100.0
    }

    /// Total fee as a fraction (e.g., 0.0035 for 0.05% + 0.30%).
    pub fn total_fee_fraction(&self) -> f64 {
        (self.fee_a + self.fee_b) as f64 / 1_000_000.0
    }

    /// All unique pool addresses for this pair (for Multicall).
    pub fn pool_addresses(&self) -> Vec<Address> {
        vec![self.pool_a, self.pool_b]
    }
}

/// Parse all configured pairs.
pub fn parse_pairs(configs: &[ArbitragePairConfig]) -> Result<Vec<PoolPair>> {
    configs
        .iter()
        .map(PoolPair::from_config)
        .collect::<Result<Vec<_>>>()
        .wrap_err("Failed to parse arbitrage pair configs")
}
