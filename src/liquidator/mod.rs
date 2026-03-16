pub mod executor;
pub mod flash_loan;
pub mod simulator;

use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use eyre::Result;
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
        // Set minProfit to cover the flash loan premium (0.09% for non-whitelisted)
        // plus a 2x safety margin, so the on-chain contract reverts
        // rather than executing an unprofitable liquidation.
        let flash_loan_premium = opportunity.debt_to_cover * U256::from(9) / U256::from(10000); // 0.09%
        let min_profit = flash_loan_premium * U256::from(2); // 2x premium as safety margin

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

    /// Process a batch of liquidation opportunities, executing the most
    /// profitable ones first.
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
        let mut executed = 0usize;

        for opportunity in &opportunities {
            match self.process_opportunity(opportunity).await {
                Ok(true) => executed += 1,
                Ok(false) => {}
                Err(e) => {
                    error!(
                        protocol = %opportunity.protocol,
                        user = %opportunity.user,
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
