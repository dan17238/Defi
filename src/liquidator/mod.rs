pub mod executor;
pub mod flash_loan;
pub mod simulator;

use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use dashmap::DashSet;
use eyre::Result;
use futures::stream::{self, StreamExt};
use std::sync::Arc;
use tracing::{error, info, warn};

use crate::config::ExecutionConfig;
use crate::protocols::LiquidationOpportunity;
use crate::utils::metrics::Metrics;

use self::executor::Executor;
use self::simulator::Simulator;

const MAX_CONCURRENT_LIQUIDATION_SIMULATIONS: usize = 3;

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
    inflight: Arc<DashSet<String>>,
}

impl<R, E> Liquidator<R, E>
where
    R: Provider + Clone + Send + Sync,
    E: Provider + Clone + Send + Sync + 'static,
{
    pub fn new(
        read_provider: R,
        exec_provider: E,
        config: ExecutionConfig,
        flash_liquidator_address: Address,
        wallet_address: Address,
        metrics: Metrics,
        telegram: Option<crate::utils::telegram::Telegram>,
    ) -> Self {
        Self {
            simulator: Simulator::new(read_provider, wallet_address),
            executor: Executor::new(exec_provider, metrics.clone(), telegram),
            config,
            metrics,
            flash_liquidator_address,
            inflight: Arc::new(DashSet::new()),
        }
    }

    fn opportunity_key(opportunity: &LiquidationOpportunity) -> String {
        format!(
            "{}:{:#x}:{:#x}:{:#x}",
            opportunity.protocol,
            opportunity.user,
            opportunity.collateral_asset,
            opportunity.debt_asset
        )
    }

    /// Process a single liquidation opportunity.
    ///
    /// 1. Simulate the liquidation using revm to verify profitability.
    /// 2. If simulation succeeds and profit exceeds threshold, execute on-chain.
    /// 3. Log and record metrics for the outcome.
    pub async fn process_opportunity(&self, opportunity: &LiquidationOpportunity) -> Result<bool> {
        info!(
            protocol = %opportunity.protocol,
            user = %opportunity.user,
            collateral = %opportunity.collateral_asset,
            debt = %opportunity.debt_asset,
            health_factor = %opportunity.health_factor,
            "Processing liquidation opportunity"
        );

        // Build the transaction thresholds before simulation so unsupported debt
        // assets are filtered early and the simulator sees the exact same
        // minProfit that the live transaction will use.
        let one_dollar = match flash_loan::tokens::one_dollar_in_tokens(opportunity.debt_asset) {
            Some(value) => value,
            None => {
                warn!(
                    protocol = %opportunity.protocol,
                    user = %opportunity.user,
                    debt_asset = %opportunity.debt_asset,
                    "Skipping opportunity with unsupported debt asset pricing"
                );
                return Ok(false);
            }
        };
        let gas_buffer = one_dollar;
        let config_min = flash_loan::tokens::usd_to_token_units(
            opportunity.debt_asset,
            self.config.min_profit_usd,
        )
        .unwrap_or(U256::ZERO);
        let min_profit = gas_buffer + config_min;

        let inflight_key = Self::opportunity_key(opportunity);
        if !self.inflight.insert(inflight_key.clone()) {
            info!(
                protocol = %opportunity.protocol,
                user = %opportunity.user,
                "Skipping duplicate liquidation while previous tx is still in flight"
            );
            return Ok(false);
        }

        let gas_price_wei = match self
            .executor
            .current_gas_price(self.config.max_gas_price_gwei)
            .await
        {
            Ok(gas_price) => gas_price,
            Err(e) => {
                self.inflight.remove(&inflight_key);
                self.metrics.record_error();
                return Err(e);
            }
        };

        // Step 1: Simulate
        let sim_result = self
            .simulator
            .simulate_liquidation(
                opportunity,
                self.flash_liquidator_address,
                min_profit,
                gas_price_wei,
            )
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
                self.inflight.remove(&inflight_key);
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
            self.inflight.remove(&inflight_key);
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
            self.inflight.remove(&inflight_key);
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
            self.inflight.remove(&inflight_key);
            return Ok(false);
        }

        // Build and send the transaction using the exact same gas price that
        // was used during simulation.
        let calldata = flash_loan::encode_flash_liquidation(
            opportunity,
            self.flash_liquidator_address,
            min_profit,
        )?;

        match self
            .executor
            .execute(
                self.flash_liquidator_address,
                calldata,
                gas_price_wei,
                self.inflight.clone(),
                inflight_key.clone(),
            )
            .await
        {
            Ok(tx_hash) => {
                self.metrics.record_liquidation_attempt();
                info!(
                    protocol = %opportunity.protocol,
                    user = %opportunity.user,
                    tx_hash = %tx_hash,
                    profit_usd = sim_result.profit_usd,
                    "Liquidation executed successfully"
                );
                Ok(true)
            }
            Err(e) => {
                self.inflight.remove(&inflight_key);
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

        // Simulate a few opportunities in parallel, but cap concurrency so a
        // large batch does not starve the node or slow the best candidates.
        let results = stream::iter(opportunities.into_iter().enumerate())
            .map(|(_i, opp)| async move {
                let protocol = opp.protocol.clone();
                let user = opp.user;
                let result = self.process_opportunity(&opp).await;
                (protocol, user, result)
            })
            .buffer_unordered(MAX_CONCURRENT_LIQUIDATION_SIMULATIONS)
            .collect::<Vec<_>>()
            .await;

        let mut executed = 0usize;
        for (protocol, user, result) in results {
            match result {
                Ok(true) => executed += 1,
                Ok(false) => {}
                Err(e) => {
                    error!(
                        protocol = %protocol,
                        user = %user,
                        error = %e,
                        "Error processing opportunity"
                    );
                }
            }
        }

        info!(
            total_opportunities = total,
            executed, "Batch processing complete"
        );

        Ok(executed)
    }
}
