use alloy::primitives::{Address, U256};
use tracing::debug;

use super::pairs::PoolPair;
use super::pool_state::{PoolStateCache, UniV3PoolState};

/// A detected arbitrage opportunity ready for simulation.
#[derive(Debug, Clone)]
pub struct ArbitrageOpportunity {
    pub pool_a: Address,
    pub pool_b: Address,
    pub zero_for_one: bool,
    pub amount_in: U256,
    pub estimated_profit_bps: f64,
    pub pair_name: String,
}

/// Detects arbitrage opportunities by comparing sqrtPriceX96 between pool pairs.
pub struct ArbitrageDetector {
    pairs: Vec<PoolPair>,
    /// Extra margin in basis points to cover gas costs.
    gas_margin_bps: f64,
}

impl ArbitrageDetector {
    pub fn new(pairs: Vec<PoolPair>, gas_margin_bps: f64) -> Self {
        Self {
            pairs,
            gas_margin_bps,
        }
    }

    /// Access the configured pairs (for dashboard snapshots).
    pub fn pairs(&self) -> &[PoolPair] {
        &self.pairs
    }

    /// Scan all configured pairs for arbitrage opportunities.
    pub fn scan_all_pairs(&self, cache: &PoolStateCache) -> Vec<ArbitrageOpportunity> {
        let mut opportunities = Vec::new();

        for pair in &self.pairs {
            let state_a = match cache.get(&pair.pool_a) {
                Some(s) => s,
                None => continue,
            };
            let state_b = match cache.get(&pair.pool_b) {
                Some(s) => s,
                None => continue,
            };

            if let Some(opp) = self.detect_pair(pair, &state_a, &state_b) {
                debug!(
                    pair = %pair.name,
                    spread_bps = opp.estimated_profit_bps,
                    zero_for_one = opp.zero_for_one,
                    amount_in = %opp.amount_in,
                    "Arbitrage opportunity detected"
                );
                opportunities.push(opp);
            }
        }

        opportunities
    }

    /// Check if a single pair has a profitable spread.
    fn detect_pair(
        &self,
        pair: &PoolPair,
        state_a: &UniV3PoolState,
        state_b: &UniV3PoolState,
    ) -> Option<ArbitrageOpportunity> {
        // Skip pools with zero liquidity
        if state_a.liquidity == 0 || state_b.liquidity == 0 {
            return None;
        }

        // Compare sqrtPriceX96 values directly.
        // sqrtPriceX96 = sqrt(price) * 2^96, where price = token1/token0.
        // Higher sqrtPriceX96 → token0 is more expensive in terms of token1.
        let sqrt_a = &state_a.sqrt_price_x96;
        let sqrt_b = &state_b.sqrt_price_x96;

        if sqrt_a.is_zero() || sqrt_b.is_zero() {
            return None;
        }

        // Compute spread using f64 for simplicity.
        // price ratio = (sqrtP_A / sqrtP_B)^2
        // spread = |1 - ratio|
        let sqrt_a_f = sqrt_price_to_f64(sqrt_a);
        let sqrt_b_f = sqrt_price_to_f64(sqrt_b);

        if sqrt_a_f == 0.0 || sqrt_b_f == 0.0 {
            return None;
        }

        let ratio = sqrt_a_f / sqrt_b_f;
        let price_ratio = ratio * ratio;
        let spread = (1.0 - price_ratio).abs();
        let spread_bps = spread * 10_000.0;

        let fee_threshold_bps = pair.total_fee_bps() + self.gas_margin_bps;

        if spread_bps <= fee_threshold_bps {
            return None;
        }

        // Determine direction:
        // pool_a in the opportunity = the more expensive pool (higher sqrtPriceX96).
        // We always sell token0 on the expensive pool → zeroForOne is always true.
        let a_is_expensive = sqrt_a > sqrt_b;
        let (expensive_pool, cheap_pool) = if a_is_expensive {
            (pair.pool_a, pair.pool_b)
        } else {
            (pair.pool_b, pair.pool_a)
        };

        let amount_in = compute_optimal_amount(state_a, state_b, spread_bps, fee_threshold_bps);

        Some(ArbitrageOpportunity {
            pool_a: expensive_pool,
            pool_b: cheap_pool,
            zero_for_one: true, // always sell token0 on the expensive pool
            amount_in,
            estimated_profit_bps: spread_bps - fee_threshold_bps,
            pair_name: pair.name.clone(),
        })
    }
}

/// Convert sqrtPriceX96 (U256) to f64 for comparison.
/// sqrtPriceX96 = sqrt(price) * 2^96
pub fn sqrt_price_to_f64(sqrt_price: &U256) -> f64 {
    // Convert U256 to f64. For typical UniV3 prices, the value fits
    // within f64 precision (sqrtPriceX96 is typically ~1e28 to ~1e30).
    let limbs = sqrt_price.as_limbs();
    let mut result = 0.0_f64;
    for (i, &limb) in limbs.iter().enumerate() {
        result += limb as f64 * 2.0_f64.powi(64 * i as i32);
    }
    result
}

/// Compute a reasonable trade amount based on spread and liquidity.
///
/// UniV3 `liquidity` (L) is not a token amount — it's sqrt(x·y). To estimate
/// the token0 amount available near the current tick:
///   token0_available ≈ L / sqrtPrice
/// This gives a rough upper bound of swappable token0 without crossing ticks.
///
/// The exact profitability is verified by revm simulation.
fn compute_optimal_amount(
    state_a: &UniV3PoolState,
    state_b: &UniV3PoolState,
    spread_bps: f64,
    fee_threshold_bps: f64,
) -> U256 {
    let liq_a = state_a.liquidity as f64;
    let liq_b = state_b.liquidity as f64;
    let sqrt_a = sqrt_price_to_f64(&state_a.sqrt_price_x96);
    let sqrt_b = sqrt_price_to_f64(&state_b.sqrt_price_x96);

    if sqrt_a == 0.0 || sqrt_b == 0.0 {
        return U256::from(1u64);
    }

    // Convert liquidity to approximate token0 available: L / sqrtPrice
    // sqrtPriceX96 = sqrtPrice * 2^96, so sqrtPrice = sqrtPriceX96 / 2^96
    let two_96: f64 = 2.0_f64.powi(96);
    let token0_a = liq_a / (sqrt_a / two_96);
    let token0_b = liq_b / (sqrt_b / two_96);
    let max_token0 = token0_a.min(token0_b);

    // Scale by excess spread ratio, capped at 10x
    let excess_ratio = ((spread_bps / fee_threshold_bps) - 1.0).min(10.0).max(0.1);

    // Use ~1% of available token0, scaled by spread excess
    let raw = max_token0 * 0.01 * excess_ratio;

    // Clamp to reasonable range (no hardcoded minimum — let revm decide viability)
    let clamped = raw.max(1.0).min(max_token0 * 0.5);

    U256::from(clamped as u128)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    fn make_pool_state(sqrt_price_x96: u128, liquidity: u128, fee: u32) -> UniV3PoolState {
        UniV3PoolState {
            sqrt_price_x96: U256::from(sqrt_price_x96),
            tick: 0,
            liquidity,
            fee,
            token0: address!("82aF49447D8a07e3bd95BD0d56f35241523fBab1"), // WETH
            token1: address!("af88d065e77c8cC2239327C5EDb3A432268e5831"), // USDC
        }
    }

    fn make_pair(fee_a: u32, fee_b: u32) -> PoolPair {
        PoolPair {
            name: "test_pair".to_string(),
            pool_a: address!("C6962004f452bE9203591991D15f6b388e09E8D0"),
            pool_b: address!("C31E54c7a869B9FcBEcc14363CF510d1c41fa443"),
            token0: address!("82aF49447D8a07e3bd95BD0d56f35241523fBab1"),
            token1: address!("af88d065e77c8cC2239327C5EDb3A432268e5831"),
            fee_a,
            fee_b,
        }
    }

    #[test]
    fn detects_profitable_spread() {
        let detector = ArbitrageDetector::new(vec![make_pair(500, 3000)], 1.0);

        // sqrtPriceX96 values with ~1% price difference (100 bps spread)
        // fee threshold = (500+3000)/100 = 35 bps + 1 bps margin = 36 bps
        // 100 bps > 36 bps → should detect
        let base_sqrt = 1_000_000_000_000_000_000_000_000_000u128; // ~1e27
        let state_a = make_pool_state(base_sqrt, 1_000_000_000_000_000_000, 500);
        // ~0.5% higher price → sqrt is ~0.25% higher → ~1.0025 ratio
        let state_b = make_pool_state(
            (base_sqrt as f64 * 1.0025) as u128,
            1_000_000_000_000_000_000,
            3000,
        );

        let result = detector.detect_pair(&make_pair(500, 3000), &state_a, &state_b);
        assert!(result.is_some(), "Should detect profitable spread");
    }

    #[test]
    fn ignores_small_spread() {
        let detector = ArbitrageDetector::new(vec![make_pair(500, 3000)], 1.0);

        // Spread = ~1 bp (0.01%), well below the 36 bps threshold
        let base_sqrt = 1_000_000_000_000_000_000_000_000_000u128;
        let state_a = make_pool_state(base_sqrt, 1_000_000_000_000_000_000, 500);
        let state_b = make_pool_state(
            (base_sqrt as f64 * 1.00005) as u128,
            1_000_000_000_000_000_000,
            3000,
        );

        let result = detector.detect_pair(&make_pair(500, 3000), &state_a, &state_b);
        assert!(result.is_none(), "Should ignore small spread");
    }

    #[test]
    fn correct_direction_when_a_higher() {
        let pair = make_pair(500, 500);
        let detector = ArbitrageDetector::new(vec![pair.clone()], 0.5);

        let base_sqrt = 1_000_000_000_000_000_000_000_000_000u128;
        // Pool A has higher sqrtPriceX96 → token0 more expensive on A
        let state_a = make_pool_state(
            (base_sqrt as f64 * 1.01) as u128,
            1_000_000_000_000_000_000,
            500,
        );
        let state_b = make_pool_state(base_sqrt, 1_000_000_000_000_000_000, 500);

        let result = detector.detect_pair(&pair, &state_a, &state_b);
        assert!(result.is_some());
        let opp = result.unwrap();
        assert!(opp.zero_for_one, "zeroForOne must always be true");
        // pool_a in the opportunity should be the expensive pool (pair.pool_a)
        assert_eq!(opp.pool_a, pair.pool_a, "pool_a should be the expensive pool");
    }

    #[test]
    fn correct_direction_when_b_higher() {
        let pair = make_pair(500, 500);
        let detector = ArbitrageDetector::new(vec![pair.clone()], 0.5);

        let base_sqrt = 1_000_000_000_000_000_000_000_000_000u128;
        // Pool B has higher sqrtPriceX96 → token0 more expensive on B
        let state_a = make_pool_state(base_sqrt, 1_000_000_000_000_000_000, 500);
        let state_b = make_pool_state(
            (base_sqrt as f64 * 1.01) as u128,
            1_000_000_000_000_000_000,
            500,
        );

        let result = detector.detect_pair(&pair, &state_a, &state_b);
        assert!(result.is_some());
        let opp = result.unwrap();
        assert!(opp.zero_for_one, "zeroForOne must always be true");
        // pool_a in the opportunity should be the expensive pool (pair.pool_b)
        assert_eq!(opp.pool_a, pair.pool_b, "pool_a should be the expensive pool (pool_b)");
        assert_eq!(opp.pool_b, pair.pool_a, "pool_b should be the cheap pool (pool_a)");
    }

    #[test]
    fn skips_zero_liquidity() {
        let detector = ArbitrageDetector::new(vec![make_pair(500, 3000)], 1.0);

        let base_sqrt = 1_000_000_000_000_000_000_000_000_000u128;
        let state_a = make_pool_state(base_sqrt, 0, 500); // zero liquidity
        let state_b = make_pool_state(
            (base_sqrt as f64 * 1.01) as u128,
            1_000_000_000_000_000_000,
            3000,
        );

        let result = detector.detect_pair(&make_pair(500, 3000), &state_a, &state_b);
        assert!(result.is_none(), "Should skip zero-liquidity pools");
    }
}
