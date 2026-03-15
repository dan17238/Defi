use alloy::primitives::{Address, FixedBytes, U256};
use alloy::providers::Provider;
use alloy::rpc::types::Filter;
use alloy::sol;
use eyre::{Context, Result};
use tracing::{debug, info, warn};

use crate::protocols::{LiquidationOpportunity, Protocol};
use crate::provider;
use crate::state::position_tracker::PositionTracker;
use crate::utils::multicall::Multicall;

// --------------------------------------------------------------------------
// Radiant ABI definitions (AAVE v2 fork)
// --------------------------------------------------------------------------

sol! {
    #[sol(rpc)]
    interface IRadiantPool {
        /// Returns account data for a given user across all reserves.
        /// Same signature as AAVE v2 LendingPool.getUserAccountData.
        function getUserAccountData(address user)
            external
            view
            returns (
                uint256 totalCollateralETH,
                uint256 totalDebtETH,
                uint256 availableBorrowsETH,
                uint256 currentLiquidationThreshold,
                uint256 ltv,
                uint256 healthFactor
            );

        /// Returns the list of all active reserve token addresses.
        function getReservesList() external view returns (address[] memory);
    }

    #[sol(rpc)]
    interface IRadiantDataProvider {
        /// Returns all reserves with their symbols.
        function getAllReservesTokens()
            external
            view
            returns (TokenData[] memory);

        /// Returns user data for a specific reserve.
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
                bool usageAsCollateralEnabled
            );

        struct TokenData {
            string symbol;
            address tokenAddress;
        }
    }
}

/// Radiant protocol monitor.
///
/// Radiant is an AAVE v2 fork deployed on Arbitrum. The core mechanics are
/// similar, but contract addresses and some return types differ slightly.
pub struct RadiantProtocol<P> {
    provider: P,
    pool_address: Address,
    data_provider_address: Address,
    min_profit_usd: f64,
    position_tracker: PositionTracker,
    multicall_batch_size: usize,
}

impl<P: Provider + Clone + Send + Sync> RadiantProtocol<P> {
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

    /// Returns a reference to the internal position tracker.
    pub fn position_tracker(&self) -> &PositionTracker {
        &self.position_tracker
    }

    /// Fetch the list of all active reserves from the Radiant LendingPool.
    pub async fn fetch_reserves_list(&self) -> Result<Vec<Address>> {
        let pool = IRadiantPool::new(self.pool_address, &self.provider);
        let reserves = pool
            .getReservesList()
            .call()
            .await
            .wrap_err("Failed to fetch Radiant reserves list")?;
        Ok(reserves)
    }

    /// Batch-query health factors for tracked borrowers using Multicall3.
    ///
    /// Returns (user_address, health_factor, total_debt_eth) tuples.
    /// `total_debt_eth` is in ETH (18 decimals) for Radiant (AAVE v2 fork).
    async fn batch_query_health_factors(
        &self,
        users: &[Address],
    ) -> Result<Vec<(Address, U256, U256)>> {
        if users.is_empty() {
            return Ok(Vec::new());
        }

        let multicall = Multicall::new(&self.provider);
        let mut results: Vec<(Address, U256, U256)> = Vec::with_capacity(users.len());

        for chunk in users.chunks(self.multicall_batch_size) {
            let calls: Vec<_> = chunk
                .iter()
                .map(|user| {
                    let call_data = IRadiantPool::getUserAccountDataCall { user: *user };
                    (
                        self.pool_address,
                        alloy::sol_types::SolCall::abi_encode(&call_data),
                    )
                })
                .collect();

            let raw_results = multicall.aggregate3(calls).await?;

            for (i, raw) in raw_results.iter().enumerate() {
                if raw.success {
                    if let Ok(decoded) =
                        <IRadiantPool::getUserAccountDataCall as alloy::sol_types::SolCall>::abi_decode_returns(
                            &raw.return_data,
                        )
                    {
                        results.push((chunk[i], decoded.healthFactor, decoded.totalDebtETH));
                    } else {
                        warn!(
                            user = %chunk[i],
                            "Failed to decode Radiant getUserAccountData response"
                        );
                    }
                } else {
                    debug!(user = %chunk[i], "Radiant getUserAccountData call reverted");
                }
            }
        }

        Ok(results)
    }

    /// Build a liquidation opportunity for a user with an underwater position.
    async fn build_opportunity(
        &self,
        user: Address,
        health_factor: U256,
        total_debt_eth: U256,
    ) -> Result<Option<LiquidationOpportunity>> {
        let reserves = self.fetch_reserves_list().await?;
        let data_provider =
            IRadiantDataProvider::new(self.data_provider_address, &self.provider);

        let mut best_collateral = Address::ZERO;
        let mut best_debt = Address::ZERO;
        let mut max_collateral = U256::ZERO;
        let mut max_debt = U256::ZERO;

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
                        "Failed to fetch Radiant user reserve data"
                    );
                    continue;
                }
            };

            if user_data.usageAsCollateralEnabled
                && user_data.currentATokenBalance > max_collateral
            {
                max_collateral = user_data.currentATokenBalance;
                best_collateral = *reserve;
            }

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
            debug!(user = %user, "No suitable Radiant collateral/debt pair found");
            return Ok(None);
        }

        // Radiant (AAVE v2 fork) close factor is 50%.
        let close_factor = 0.5;
        let debt_to_cover = max_debt / U256::from(2);

        // Profit estimate using totalDebtETH from getUserAccountData.
        // totalDebtETH is denominated in ETH (18 decimals). Convert to USD
        // using a rough ETH price estimate (~$3000).
        let estimated_bonus_bps: f64 = 500.0;
        let debt_eth = total_debt_eth.to::<u128>() as f64 / 1e18;
        let eth_price_usd = 3000.0; // rough estimate; a production bot would use an oracle
        let debt_usd = debt_eth * eth_price_usd;
        let estimated_profit_usd = debt_usd * close_factor * (estimated_bonus_bps / 10_000.0);

        if estimated_profit_usd < self.min_profit_usd {
            debug!(
                user = %user,
                estimated_profit_usd,
                min = self.min_profit_usd,
                "Radiant opportunity below profit threshold"
            );
            return Ok(None);
        }

        Ok(Some(LiquidationOpportunity {
            protocol: "radiant".to_string(),
            user,
            collateral_asset: best_collateral,
            debt_asset: best_debt,
            debt_to_cover,
            expected_profit_usd: estimated_profit_usd,
            health_factor,
        }))
    }

    /// Discover borrowers by scanning recent Borrow events from the Radiant pool.
    async fn discover_borrowers_from_events(&self) -> Result<()> {
        // Radiant (AAVE v2 fork) Borrow event topic0:
        // Borrow(address,address,address,uint256,uint256,uint256,uint16)
        // Same topic hash as AAVE v2's Borrow event.
        let borrow_topic: FixedBytes<32> = "0xc6a898309e823ee50bac64e45ca8adba6690e99e7841c45d754e2a38e9019d9b"
            .parse()
            .wrap_err("Invalid Radiant borrow event topic")?;

        let latest = provider::get_latest_block_number(&self.provider).await?;
        let from_block = latest.saturating_sub(50_000);

        info!(
            protocol = "radiant",
            from_block,
            to_block = latest,
            "Scanning for Borrow events to discover borrowers"
        );

        let filter = Filter::new()
            .address(self.pool_address)
            .event_signature(borrow_topic)
            .from_block(from_block)
            .to_block(latest);

        let logs = self.provider.get_logs(&filter).await
            .wrap_err("Failed to fetch Radiant Borrow event logs")?;

        let mut count = 0usize;
        for log in &logs {
            // topic[2] is the `onBehalfOf` address (the actual borrower).
            if log.topics().len() >= 3 {
                let borrower = Address::from_word(log.topics()[2]);
                self.position_tracker.add_borrower(borrower);
                count += 1;
            }
        }

        info!(
            protocol = "radiant",
            events = logs.len(),
            unique_borrowers = self.position_tracker.borrower_count(),
            "Radiant borrower discovery complete (added {} entries)", count
        );

        Ok(())
    }
}

impl<P: Provider + Clone + Send + Sync> Protocol for RadiantProtocol<P> {
    fn name(&self) -> &str {
        "radiant"
    }

    async fn get_liquidatable_positions(
        &self,
        block_number: u64,
    ) -> Result<Vec<LiquidationOpportunity>> {
        info!(
            protocol = self.name(),
            block = block_number,
            "Scanning Radiant for liquidatable positions"
        );

        let borrowers = self.position_tracker.get_all_borrowers();
        if borrowers.is_empty() {
            debug!(protocol = self.name(), "No tracked Radiant borrowers, skipping");
            return Ok(Vec::new());
        }

        info!(
            protocol = self.name(),
            count = borrowers.len(),
            "Querying Radiant health factors"
        );

        let threshold = U256::from(1_000_000_000_000_000_000u64);
        let health_factors = self.batch_query_health_factors(&borrowers).await?;

        let mut opportunities = Vec::new();
        for (user, hf, total_debt_eth) in health_factors {
            self.position_tracker.update_health_factor(user, hf);

            if hf < threshold && hf > U256::ZERO {
                info!(
                    protocol = self.name(),
                    user = %user,
                    health_factor = %hf,
                    "Liquidatable Radiant position found"
                );

                match self.build_opportunity(user, hf, total_debt_eth).await {
                    Ok(Some(opp)) => opportunities.push(opp),
                    Ok(None) => {}
                    Err(e) => {
                        warn!(
                            protocol = self.name(),
                            user = %user,
                            error = %e,
                            "Failed to build Radiant liquidation opportunity"
                        );
                    }
                }
            }
        }

        info!(
            protocol = self.name(),
            block = block_number,
            found = opportunities.len(),
            "Radiant scan complete"
        );

        Ok(opportunities)
    }

    async fn discover_borrowers(&self) -> Result<()> {
        self.discover_borrowers_from_events().await
    }
}
