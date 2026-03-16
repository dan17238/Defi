pub mod executor;
pub mod flash_loan;
pub mod simulator;

use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use eyre::Result;
use futures::future::join_all;
use tracing::{error, info, warn};

use crate::config::ExecutionConfig;
use crate::protocols::LiquidationOpportunity;
use crate::utils::metrics::Metrics;

use self::executor::Executor;
use self::simulator::Simulator;

/// Liquidation orchestrator.
///
/// Receives liquidation opportunities from protocol monitors, simulates them
/// via revm, and if profitable, submits the on-chain transaction.
///
/// Generic over two provider types:
/// - `R`: read-only provider for simulation (unsigned, points to fast RPC)
/// - `E`: execution provider for sending transactions (signed with wallet, points to sequencer)
pub struct Liquidator<R, E> {
    simulator: Simulator<R>,
    executor: Executor<E>,
    config: ExecutionConfig,
    metrics: Metrics,
    flash_liquidator_address: Address,
}

impl<R, E> Liquidator<R, E>
where
    R: Provider + Clone + Send + Sync,
    E: Provider + Clone + Send + Sync,
{
    pub fn new(
        read_provider: R,
        exec_provider: E,
        config: ExecutionConfig,
        flash_liquidator_address: Address,
        wallet_address: Address,
        metrics: Metrics,
    ) -> Self {
        Self {
            simulator: Simulator::new(read_provider, wallet_address),
            executor: Executor::new(exec_provider, metrics.clone()),
            config,
            metrics,
            flash_liquidator_address,
        }
    }

    /// Process a single liquidation opportunity.
    ///
    /// 1. Simulate the liquidation using revm to verify profitability.
    /// 2. If simulation succeeds and profit exceeds threshold, execute on-chain.
    /// 3. Log and record metrics for the outcome.
    pub async fn process_opportunity(
        &self,
        opportunity: &LiquidationOpportunity,
    ) -> Result<bool> {
        info!(
            protocol = %opportunity.protocol,
            user = %opportunity.user,
            collateral = %opportunity.collateral_asset,
            debt = %opportunity.debt_asset,
            health_factor = %opportunity.health_factor,
            "Processing liquidation opportunity"
        );

        // Step 1: Simulate
        let sim_result = self
            .simulator
            .simulate_liquidation(opportunity, self.flash_liquidator_address)
            .await;

        let sim_result = match sim_result {
            Ok(r) => r,
            Err(e) => {
                warn!(
                    protocol = %opportunity.protocol,
                    user = %opportunity.user,
                    error = %e,
                    "Simulation failed"
                );
                self.metrics.record_error();
                return Ok(false);
            }
        };

        info!(
            protocol = %opportunity.protocol,
            user = %opportunity.user,
            profitable = sim_result.profitable,
            profit_usd = sim_result.profit_usd,
            gas_cost = sim_result.gas_used,
            "Simulation result"
        );

        if !sim_result.profitable {
            info!(
                protocol = %opportunity.protocol,
                user = %opportunity.user,
                "Simulation shows unprofitable, skipping"
            );
            return Ok(false);
        }

        if sim_result.profit_usd < self.config.min_profit_usd {
            info!(
                protocol = %opportunity.protocol,
                user = %opportunity.user,
                profit_usd = sim_result.profit_usd,
                min = self.config.min_profit_usd,
                "Profit below minimum threshold, skipping"
            );
            return Ok(false);
        }

        // Step 2: Execute (or dry-run)
        if self.config.dry_run {
            info!(
                protocol = %opportunity.protocol,
                user = %opportunity.user,
                profit_usd = sim_result.profit_usd,
                "DRY RUN: Would execute liquidation"
            );
            self.metrics.record_liquidation(sim_result.profit_usd, true);
            return Ok(true);
        }

        // Build and send the transaction
        // Set minProfit to cover: flash loan premium + estimated gas cost + profit margin.
        // The on-chain contract will revert if actual profit is below this threshold.
        //
        // 1. Flash loan premium (0.09% for non-whitelisted borrowers)
        let flash_loan_premium = opportunity.debt_to_cover * U256::from(9) / U256::from(10000);

        // 2. Detect debt token decimals and compute ~$1 worth in token units.
        let debt_decimals = match opportunity.debt_asset {
            t if t == flash_loan::tokens::WETH => 18u8,
            t if t == flash_loan::tokens::WBTC => 8u8,
            t if t == flash_loan::tokens::DAI => 18u8,
            _ => 6u8, // stablecoins (USDC, USDC.e, USDT)
        };

        let one_dollar_in_tokens = match debt_decimals {
            18 => {
                // For WETH: $1 / eth_price * 1e18
                let eth_price = crate::protocols::radiant::CACHED_ETH_PRICE_CENTS
                    .load(std::sync::atomic::Ordering::Relaxed) as f64 / 100.0;
                let eth_price = if eth_price > 100.0 { eth_price } else { 3500.0 };
                U256::from((1e18 / eth_price) as u128)
            }
            8 => {
                // For WBTC: $1 / $95000 * 1e8
                U256::from(1052u64)
            }
            _ => U256::from(1_000_000u64), // 6 decimals, $1
        };

        // 2b. Gas buffer: ~$1 worth of the debt token
        let gas_buffer = one_dollar_in_tokens;
        // 3. Config minimum profit threshold converted to token terms.
        let config_min = one_dollar_in_tokens * U256::from(self.config.min_profit_usd as u64);
        let min_profit = flash_loan_premium + gas_buffer + config_min;

        let calldata = flash_loan::encode_flash_liquidation(
            opportunity,
            self.flash_liquidator_address,
            min_profit,
        );

        match self
            .executor
            .execute(
                self.flash_liquidator_address,
                calldata,
                self.config.max_gas_price_gwei,
            )
            .await
        {
            Ok(tx_hash) => {
                info!(
                    protocol = %opportunity.protocol,
                    user = %opportunity.user,
                    tx_hash = %tx_hash,
                    profit_usd = sim_result.profit_usd,
                    "Liquidation executed successfully"
                );
                self.metrics.record_liquidation(sim_result.profit_usd, true);
                Ok(true)
            }
            Err(e) => {
                error!(
                    protocol = %opportunity.protocol,
                    user = %opportunity.user,
                    error = %e,
                    "Liquidation execution failed"
                );
                self.metrics.record_error();
                Ok(false)
            }
        }
    }

    /// Process a batch of liquidation opportunities concurrently, sorted by
    /// expected profit descending (most profitable first).
    pub async fn process_batch(
        &self,
        mut opportunities: Vec<LiquidationOpportunity>,
    ) -> Result<usize> {
        // Sort by expected profit descending - most profitable first.
        opportunities.sort_by(|a, b| {
            b.expected_profit_usd
                .partial_cmp(&a.expected_profit_usd)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let total = opportunities.len();

        // Process all opportunities concurrently instead of sequentially.
        // Each opportunity is independent (different borrower positions), so
        // there is no ordering dependency between them.
        let futures: Vec<_> = opportunities
            .iter()
            .map(|opp| self.process_opportunity(opp))
            .collect();

        let results = join_all(futures).await;

        let mut executed = 0usize;
        for (i, result) in results.into_iter().enumerate() {
            match result {
                Ok(true) => executed += 1,
                Ok(false) => {}
                Err(e) => {
                    error!(
                        protocol = %opportunities[i].protocol,
                        user = %opportunities[i].user,
                        error = %e,
                        "Error processing opportunity"
                    );
                }
            }
        }

        info!(
            total_opportunities = total,
            executed,
            "Batch processing complete"
        );

        Ok(executed)
    }
}
