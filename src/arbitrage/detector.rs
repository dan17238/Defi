use alloy::primitives::{Address, U256};
use tracing::debug;

use super::pairs::PoolPair;
use super::pool_state::{PoolStateCache, UniV3PoolState};

/// A detected arbitrage opportunity ready for simulation.
/// Supports both 2-pool pairs and N-pool multi-hop routes.
#[derive(Debug, Clone)]
pub struct ArbitrageOpportunity {
    /// Ordered pools in the route (2 for legacy pairs, N for multi-hop)
    pub pools: Vec<Address>,
    /// Swap direction for each pool
    pub zero_for_one: Vec<bool>,
    pub amount_in: U256,
    pub estimated_profit_bps: f64,
    pub pair_name: String,
}

/// A pre-computed multi-hop route with resolved zeroForOne directions.
#[derive(Debug, Clone)]
pub struct ResolvedRoute {
    pub name: String,
    pub pools: Vec<Address>,
    pub zero_for_one: Vec<bool>,
    pub total_fee_bps: f64,
}

/// Detects arbitrage opportunities by comparing sqrtPriceX96 between pool pairs
/// and scanning multi-hop routes for circular profit.
pub struct ArbitrageDetector {
    pairs: Vec<PoolPair>,
    routes: Vec<ResolvedRoute>,
    gas_margin_bps: f64,
}

impl ArbitrageDetector {
    pub fn new(pairs: Vec<PoolPair>, gas_margin_bps: f64) -> Self {
        Self {
            pairs,
            routes: Vec::new(),
            gas_margin_bps,
        }
    }

    /// Set resolved multi-hop routes (called after pool state init).
    pub fn set_routes(&mut self, routes: Vec<ResolvedRoute>) {
        self.routes = routes;
    }

    /// Access the configured pairs (for dashboard snapshots).
    pub fn pairs(&self) -> &[PoolPair] {
        &self.pairs
    }

    /// Access the configured routes.
    pub fn routes(&self) -> &[ResolvedRoute] {
        &self.routes
    }

    /// Scan all pairs AND routes for arbitrage opportunities.
    pub fn scan_all_pairs(&self, cache: &PoolStateCache) -> Vec<ArbitrageOpportunity> {
        let mut opportunities = Vec::new();

        // 1. Legacy 2-pool pairs
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
                    zero_for_one = ?opp.zero_for_one,
                    amount_in = %opp.amount_in,
                    "Pair arbitrage opportunity detected"
                );
                opportunities.push(opp);
            }
        }

        // 2. Multi-hop routes
        for route in &self.routes {
            if let Some(opp) = self.detect_route(route, cache) {
                debug!(
                    route = %route.name,
                    spread_bps = opp.estimated_profit_bps,
                    hops = route.pools.len(),
                    amount_in = %opp.amount_in,
                    "Route arbitrage opportunity detected"
                );
                opportunities.push(opp);
            }
        }

        opportunities
    }

    /// Detect if a multi-hop route has a circular profit opportunity.
    /// Simulates the price impact through each hop and checks if the output
    /// exceeds input + total fees.
    fn detect_route(
        &self,
        route: &ResolvedRoute,
        cache: &PoolStateCache,
    ) -> Option<ArbitrageOpportunity> {
        let states = route_states(route, cache)?;
        let spread_bps = estimate_route_net_spread_bps(route, cache)?;
        if spread_bps <= self.gas_margin_bps {
            return None;
        }

        // Size the trade in the actual input token of the first hop.
        let first = &states[0];
        let input_available = first_hop_input_capacity(first, route.zero_for_one[0])?;
        let excess_ratio = ((spread_bps / self.gas_margin_bps) - 1.0)
            .min(10.0)
            .max(0.1);
        let raw = input_available * 0.005 * excess_ratio; // 0.5% of pool[0] available
        let amount = U256::from(raw.max(1.0).min(input_available * 0.2) as u128);

        Some(ArbitrageOpportunity {
            pools: route.pools.clone(),
            zero_for_one: route.zero_for_one.clone(),
            amount_in: amount,
            estimated_profit_bps: spread_bps - self.gas_margin_bps,
            pair_name: route.name.clone(),
        })
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
            pools: vec![expensive_pool, cheap_pool],
            zero_for_one: vec![true, false], // sell token0 on expensive, buy back on cheap
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

fn sqrt_ratio(state: &UniV3PoolState) -> Option<f64> {
    let sqrt = sqrt_price_to_f64(&state.sqrt_price_x96);
    let two96 = 2.0_f64.powi(96);
    if sqrt <= 0.0 || two96 <= 0.0 {
        return None;
    }
    Some(sqrt / two96)
}

fn price_token1_per_token0(state: &UniV3PoolState) -> Option<f64> {
    let sqrt_ratio = sqrt_ratio(state)?;
    Some(sqrt_ratio * sqrt_ratio)
}

fn route_states(route: &ResolvedRoute, cache: &PoolStateCache) -> Option<Vec<UniV3PoolState>> {
    let states: Vec<UniV3PoolState> = route.pools.iter().filter_map(|p| cache.get(p)).collect();
    if states.len() != route.pools.len() || states.iter().any(|s| s.liquidity == 0) {
        return None;
    }
    Some(states)
}

fn route_price_product(
    route: &ResolvedRoute,
    cache: &PoolStateCache,
    include_fees: bool,
) -> Option<f64> {
    let states = route_states(route, cache)?;
    let mut product = 1.0_f64;
    for (state, zero_for_one) in states.iter().zip(route.zero_for_one.iter().copied()) {
        let price = price_token1_per_token0(state)?;
        let mut rate = if zero_for_one { price } else { 1.0 / price };
        if include_fees {
            rate *= 1.0 - state.fee as f64 / 1_000_000.0;
        }
        product *= rate;
    }
    Some(product)
}

pub(crate) fn estimate_route_gross_spread_bps(
    route: &ResolvedRoute,
    cache: &PoolStateCache,
) -> Option<f64> {
    let product = route_price_product(route, cache, false)?;
    Some((product - 1.0) * 10_000.0)
}

pub(crate) fn estimate_route_net_spread_bps(
    route: &ResolvedRoute,
    cache: &PoolStateCache,
) -> Option<f64> {
    let product = route_price_product(route, cache, true)?;
    Some((product - 1.0) * 10_000.0)
}

pub(crate) fn first_hop_input_capacity(state: &UniV3PoolState, zero_for_one: bool) -> Option<f64> {
    let liq = state.liquidity as f64;
    let sqrt_ratio = sqrt_ratio(state)?;
    let amount = if zero_for_one {
        liq / sqrt_ratio
    } else {
        liq * sqrt_ratio
    };
    if amount.is_finite() && amount > 0.0 {
        Some(amount)
    } else {
        None
    }
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
        assert_eq!(
            opp.zero_for_one[0], true,
            "first hop zeroForOne must be true"
        );
        assert_eq!(
            opp.pools[0], pair.pool_a,
            "pools[0] should be the expensive pool"
        );
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
        assert_eq!(
            opp.zero_for_one[0], true,
            "first hop zeroForOne must be true"
        );
        assert_eq!(
            opp.pools[0], pair.pool_b,
            "pools[0] should be the expensive pool (pool_b)"
        );
        assert_eq!(
            opp.pools[1], pair.pool_a,
            "pools[1] should be the cheap pool (pool_a)"
        );
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

    #[test]
    fn route_input_capacity_uses_actual_first_hop_input_token() {
        let sqrt_price_x96 = U256::from(2u128) << 96; // sqrt ratio = 2
        let state = UniV3PoolState {
            sqrt_price_x96,
            tick: 0,
            liquidity: 1_000,
            fee: 500,
            token0: address!("82aF49447D8a07e3bd95BD0d56f35241523fBab1"),
            token1: address!("af88d065e77c8cC2239327C5EDb3A432268e5831"),
        };

        assert_eq!(first_hop_input_capacity(&state, true).unwrap(), 500.0);
        assert_eq!(first_hop_input_capacity(&state, false).unwrap(), 2_000.0);
    }
}
