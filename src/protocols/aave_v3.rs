use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::sol;
use eyre::{Context, Result};
use tracing::{debug, info, warn};

use crate::protocols::{LiquidationOpportunity, Protocol};
use crate::state::position_tracker::PositionTracker;
use crate::utils::multicall::Multicall;

// --------------------------------------------------------------------------
// ABI definitions via the sol! macro
// --------------------------------------------------------------------------

sol! {
    #[sol(rpc)]
    interface IPool {
        /// Returns account data across all reserves for a given user.
        function getUserAccountData(address user)
            external
            view
            returns (
                uint256 totalCollateralBase,
                uint256 totalDebtBase,
                uint256 availableBorrowsBase,
                uint256 currentLiquidationThreshold,
                uint256 ltv,
                uint256 healthFactor
            );

        /// Returns the list of all active reserve token addresses.
        function getReservesList() external view returns (address[] memory);
    }

    #[sol(rpc)]
    interface IPoolDataProvider {
        /// Get all users who have borrowed from any reserve.
        function getAllReservesTokens()
            external
            view
            returns (TokenData[] memory);

        /// Get user reserve data for a specific asset.
        function getUserReserveData(address asset, address user)
            external
            view
            returns (
                uint256 currentATokenBalance,
                uint256 currentStableDebt,
                uint256 currentVariableDebt,
                uint256 principalStableDebt,
                uint256 scaledVariableDebt,
                uint256 stableBorrowRate,
                uint256 liquidityRate,
                bool usageAsCollateralEnabled,
                uint40 stableRateModeTimestamp
            );

        struct TokenData {
            string symbol;
            address tokenAddress;
        }
    }
}

/// AAVE v3 protocol monitor.
///
/// Monitors borrower positions on AAVE v3 (Arbitrum) and discovers
/// liquidation opportunities by batching health factor queries.
pub struct AaveV3Protocol<P> {
    provider: P,
    pool_address: Address,
    data_provider_address: Address,
    min_profit_usd: f64,
    position_tracker: PositionTracker,
    multicall_batch_size: usize,
}

impl<P: Provider + Clone + Send + Sync> AaveV3Protocol<P> {
    pub fn new(
        provider: P,
        pool_address: Address,
        data_provider_address: Address,
        min_profit_usd: f64,
        multicall_batch_size: usize,
    ) -> Self {
        Self {
            provider,
            pool_address,
            data_provider_address,
            min_profit_usd,
            position_tracker: PositionTracker::new(),
            multicall_batch_size,
        }
    }

    /// Returns a reference to the internal position tracker for external
    /// borrower registration (e.g. from event logs).
    pub fn position_tracker(&self) -> &PositionTracker {
        &self.position_tracker
    }

    /// Fetch the list of all active reserves from the Pool contract.
    pub async fn fetch_reserves_list(&self) -> Result<Vec<Address>> {
        let pool = IPool::new(self.pool_address, &self.provider);
        let reserves = pool
            .getReservesList()
            .call()
            .await
            .wrap_err("Failed to fetch AAVE v3 reserves list")?;
        Ok(reserves)
    }

    /// Batch-query health factors for a list of users using Multicall3.
    ///
    /// Returns (user_address, health_factor) pairs.
    async fn batch_query_health_factors(
        &self,
        users: &[Address],
    ) -> Result<Vec<(Address, U256)>> {
        if users.is_empty() {
            return Ok(Vec::new());
        }

        let multicall = Multicall::new(&self.provider);
        let mut results: Vec<(Address, U256)> = Vec::with_capacity(users.len());

        // Process in batches to avoid gas limits on the multicall
        for chunk in users.chunks(self.multicall_batch_size) {
            let calls: Vec<_> = chunk
                .iter()
                .map(|user| {
                    let call_data = IPool::getUserAccountDataCall { user: *user };
                    (self.pool_address, alloy::sol_types::SolCall::abi_encode(&call_data))
                })
                .collect();

            let raw_results = multicall.aggregate3(calls).await?;

            for (i, raw) in raw_results.iter().enumerate() {
                if raw.success {
                    if let Ok(decoded) =
                        <IPool::getUserAccountDataCall as alloy::sol_types::SolCall>::abi_decode_returns(&raw.return_data)
                    {
                        results.push((chunk[i], decoded.healthFactor));
                    } else {
                        warn!(
                            user = %chunk[i],
                            "Failed to decode getUserAccountData response"
                        );
                    }
                } else {
                    debug!(user = %chunk[i], "getUserAccountData call reverted");
                }
            }
        }

        Ok(results)
    }

    /// For a user with health factor < 1e18, determine the best collateral/debt
    /// pair and compute the debt to cover.
    async fn build_opportunity(
        &self,
        user: Address,
        health_factor: U256,
    ) -> Result<Option<LiquidationOpportunity>> {
        let reserves = self.fetch_reserves_list().await?;
        let data_provider = IPoolDataProvider::new(self.data_provider_address, &self.provider);

        let mut best_collateral = Address::ZERO;
        let mut best_debt = Address::ZERO;
        let mut max_collateral = U256::ZERO;
        let mut max_debt = U256::ZERO;

        // Find the asset with the largest collateral and the largest debt for this user.
        for reserve in &reserves {
            let user_data = data_provider
                .getUserReserveData(*reserve, user)
                .call()
                .await;

            let user_data = match user_data {
                Ok(d) => d,
                Err(e) => {
                    debug!(
                        user = %user,
                        reserve = %reserve,
                        error = %e,
                        "Failed to fetch user reserve data"
                    );
                    continue;
                }
            };

            // Collateral: aToken balance and usage as collateral enabled
            if user_data.usageAsCollateralEnabled
                && user_data.currentATokenBalance > max_collateral
            {
                max_collateral = user_data.currentATokenBalance;
                best_collateral = *reserve;
            }

            // Debt: sum of stable + variable debt
            let total_debt = user_data
                .currentStableDebt
                .checked_add(user_data.currentVariableDebt)
                .unwrap_or(U256::ZERO);
            if total_debt > max_debt {
                max_debt = total_debt;
                best_debt = *reserve;
            }
        }

        if best_collateral == Address::ZERO || best_debt == Address::ZERO || max_debt == U256::ZERO
        {
            debug!(user = %user, "No suitable collateral/debt pair found");
            return Ok(None);
        }

        // AAVE v3 allows liquidating up to 50% of the debt (close factor = 0.5).
        // When health factor < 0.95e18, the close factor becomes 100%.
        let close_factor_threshold = U256::from(950_000_000_000_000_000u64); // 0.95e18
        let debt_to_cover = if health_factor < close_factor_threshold {
            max_debt
        } else {
            max_debt / U256::from(2)
        };

        // Rough profit estimate: liquidation bonus is typically 5-10% of collateral received.
        // A proper implementation would use oracle prices from the protocol.
        let estimated_bonus_bps: f64 = 500.0; // 5% placeholder
        let debt_as_f64 = debt_to_cover
            .to_string()
            .parse::<f64>()
            .unwrap_or(0.0);
        let estimated_profit_usd = debt_as_f64 * (estimated_bonus_bps / 10_000.0);

        if estimated_profit_usd < self.min_profit_usd {
            debug!(
                user = %user,
                estimated_profit_usd,
                min = self.min_profit_usd,
                "Opportunity below profit threshold"
            );
            return Ok(None);
        }

        Ok(Some(LiquidationOpportunity {
            protocol: "aave_v3".to_string(),
            user,
            collateral_asset: best_collateral,
            debt_asset: best_debt,
            debt_to_cover,
            expected_profit_usd: estimated_profit_usd,
            health_factor,
        }))
    }
}

impl<P: Provider + Clone + Send + Sync> Protocol for AaveV3Protocol<P> {
    fn name(&self) -> &str {
        "aave_v3"
    }

    async fn get_liquidatable_positions(
        &self,
        block_number: u64,
    ) -> Result<Vec<LiquidationOpportunity>> {
        info!(
            protocol = self.name(),
            block = block_number,
            "Scanning for liquidatable positions"
        );

        let borrowers = self.position_tracker.get_all_borrowers();
        if borrowers.is_empty() {
            debug!(protocol = self.name(), "No tracked borrowers, skipping scan");
            return Ok(Vec::new());
        }

        info!(
            protocol = self.name(),
            count = borrowers.len(),
            "Querying health factors"
        );

        // 1e18 threshold: health factor below this means the position is liquidatable.
        let threshold = U256::from(1_000_000_000_000_000_000u64);

        let health_factors = self.batch_query_health_factors(&borrowers).await?;

        let mut opportunities = Vec::new();
        for (user, hf) in health_factors {
            self.position_tracker.update_health_factor(user, hf);

            if hf < threshold && hf > U256::ZERO {
                info!(
                    protocol = self.name(),
                    user = %user,
                    health_factor = %hf,
                    "Liquidatable position found"
                );

                match self.build_opportunity(user, hf).await {
                    Ok(Some(opp)) => opportunities.push(opp),
                    Ok(None) => {}
                    Err(e) => {
                        warn!(
                            protocol = self.name(),
                            user = %user,
                            error = %e,
                            "Failed to build liquidation opportunity"
                        );
                    }
                }
            }
        }

        info!(
            protocol = self.name(),
            block = block_number,
            found = opportunities.len(),
            "Scan complete"
        );

        Ok(opportunities)
    }
}
