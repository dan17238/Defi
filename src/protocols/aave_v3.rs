use std::sync::atomic::{AtomicU64, Ordering};

use alloy::primitives::{Address, FixedBytes, U256};
use alloy::providers::Provider;
use alloy::rpc::types::Filter;
use alloy::sol;
use eyre::{Context, Result};
use tracing::{debug, info, warn};

use dashmap::{DashMap, DashSet};

use crate::liquidator::flash_loan::tokens;
use crate::protocols::{LiquidationOpportunity, Protocol};
use crate::provider;
use crate::state::position_tracker::PositionTracker;
use crate::utils::multicall::Multicall;

/// Convert a raw token amount to approximate USD value.
/// Delegates to centralized token registry in flash_loan::tokens.
fn token_value_usd(amount: U256, token: Address) -> Option<f64> {
    tokens::token_value_usd(amount, token)
}

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

        /// Returns the addresses provider for this pool.
        function ADDRESSES_PROVIDER() external view returns (address);
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
                uint40 stableRateLastUpdated,
                bool usageAsCollateralEnabled
            );

        /// Get reserve configuration data including liquidation bonus.
        function getReserveConfigurationData(address asset)
            external
            view
            returns (
                uint256 decimals,
                uint256 ltv,
                uint256 liquidationThreshold,
                uint256 liquidationBonus,
                uint256 reserveFactor,
                bool usageAsCollateralEnabled,
                bool borrowingEnabled,
                bool stableBorrowRateEnabled,
                bool isActive,
                bool isFrozen
            );

        struct TokenData {
            string symbol;
            address tokenAddress;
        }
    }

    #[sol(rpc)]
    interface IPoolAddressesProvider {
        function getPriceOracle() external view returns (address);
    }

    #[sol(rpc)]
    interface IAaveOracle {
        function getSourceOfAsset(address asset) external view returns (address);
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
    discovery_start_block: AtomicU64,
    next_discovery_block: AtomicU64,
    rescan_targets: DashSet<Address>,
    /// Cached liquidation bonus per collateral asset (value from on-chain, e.g. 10500 = 5% bonus).
    /// Populated lazily on first use, rarely changes (governance only).
    liq_bonus_cache: DashMap<Address, u64>,
}

impl<P: Provider + Clone + Send + Sync> AaveV3Protocol<P> {
    pub fn new(
        provider: P,
        pool_address: Address,
        data_provider_address: Address,
        min_profit_usd: f64,
        multicall_batch_size: usize,
    ) -> Self {
        let rescan_targets = DashSet::new();
        rescan_targets.insert(pool_address);

        Self {
            provider,
            pool_address,
            data_provider_address,
            min_profit_usd,
            position_tracker: PositionTracker::new(),
            multicall_batch_size,
            discovery_start_block: AtomicU64::new(u64::MAX),
            next_discovery_block: AtomicU64::new(u64::MAX),
            rescan_targets,
            liq_bonus_cache: DashMap::new(),
        }
    }

    /// Read the on-chain liquidation bonus for a collateral asset.
    /// Returns bonus in bps (e.g. 500.0 for 5%). Cached after first read.
    async fn get_liquidation_bonus(&self, collateral: Address, block_number: Option<u64>) -> f64 {
        // Check cache first
        if let Some(cached) = self.liq_bonus_cache.get(&collateral) {
            let raw = *cached;
            return if raw > 10_000 {
                (raw - 10_000) as f64
            } else {
                500.0
            };
        }

        // Read from on-chain
        let data_provider = IPoolDataProvider::new(self.data_provider_address, &self.provider);
        let call = data_provider.getReserveConfigurationData(collateral);
        let result = match block_number {
            Some(block) => call.block(block.into()).call().await,
            None => call.call().await,
        };
        match result {
            Ok(config) => {
                let raw = config.liquidationBonus.saturating_to::<u64>();
                self.liq_bonus_cache.insert(collateral, raw);
                if raw > 10_000 {
                    (raw - 10_000) as f64
                } else {
                    500.0 // fallback
                }
            }
            Err(e) => {
                debug!(
                    collateral = %collateral,
                    error = %e,
                    "Failed to read liquidation bonus, using 5% default"
                );
                500.0
            }
        }
    }

    /// Returns a reference to the internal position tracker for external
    /// borrower registration (e.g. from event logs).
    pub fn position_tracker(&self) -> &PositionTracker {
        &self.position_tracker
    }

    /// Fetch the list of all active reserves from the Pool contract.
    pub async fn fetch_reserves_list(&self, block_number: Option<u64>) -> Result<Vec<Address>> {
        let pool = IPool::new(self.pool_address, &self.provider);
        let call = pool.getReservesList();
        let reserves = match block_number {
            Some(block) => call
                .block(block.into())
                .call()
                .await
                .wrap_err("Failed to fetch AAVE v3 reserves list")?,
            None => call
                .call()
                .await
                .wrap_err("Failed to fetch AAVE v3 reserves list")?,
        };
        Ok(reserves)
    }

    async fn discovery_start_block(&self) -> Result<u64> {
        let cached = self.discovery_start_block.load(Ordering::Acquire);
        if cached != u64::MAX {
            return Ok(cached);
        }

        let start_block =
            provider::find_contract_deployment_block(&self.provider, self.pool_address).await?;
        self.discovery_start_block
            .store(start_block, Ordering::Release);
        Ok(start_block)
    }

    async fn next_discovery_block(&self) -> Result<u64> {
        let cached = self.next_discovery_block.load(Ordering::Acquire);
        if cached != u64::MAX {
            return Ok(cached);
        }

        let start_block = self.discovery_start_block().await?;
        self.next_discovery_block
            .store(start_block, Ordering::Release);
        Ok(start_block)
    }

    async fn refresh_rescan_targets(&self, block_number: Option<u64>) {
        if self.rescan_targets.len() > 1 {
            return;
        }

        let pool = IPool::new(self.pool_address, &self.provider);
        let provider_address = match pool.ADDRESSES_PROVIDER().call().await {
            Ok(addr) => addr,
            Err(e) => {
                debug!(error = %e, "Failed to fetch AAVE addresses provider for feed targets");
                return;
            }
        };

        let addresses_provider = IPoolAddressesProvider::new(provider_address, &self.provider);
        let oracle_address = match addresses_provider.getPriceOracle().call().await {
            Ok(addr) => addr,
            Err(e) => {
                debug!(error = %e, "Failed to fetch AAVE oracle for feed targets");
                return;
            }
        };

        let reserves = match self.fetch_reserves_list(block_number).await {
            Ok(reserves) => reserves,
            Err(e) => {
                debug!(error = %e, "Failed to fetch AAVE reserves for feed targets");
                return;
            }
        };

        let oracle = IAaveOracle::new(oracle_address, &self.provider);
        self.rescan_targets.insert(oracle_address);
        for reserve in reserves {
            match oracle.getSourceOfAsset(reserve).call().await {
                Ok(source) if source != Address::ZERO => {
                    self.rescan_targets.insert(source);
                }
                Ok(_) => {}
                Err(e) => {
                    debug!(reserve = %reserve, error = %e, "Failed to fetch AAVE oracle source");
                }
            }
        }
    }

    /// Batch-query health factors for a list of users using Multicall3.
    ///
    /// Returns (user_address, health_factor, total_debt_base) tuples.
    /// `total_debt_base` is in the protocol's base currency (USD, 8 decimals).
    async fn batch_query_health_factors(
        &self,
        users: &[Address],
        block_number: Option<u64>,
    ) -> Result<Vec<(Address, U256, U256)>> {
        if users.is_empty() {
            return Ok(Vec::new());
        }

        let multicall = Multicall::new(&self.provider);
        let mut results: Vec<(Address, U256, U256)> = Vec::with_capacity(users.len());

        // Process in batches to avoid gas limits on the multicall
        for chunk in users.chunks(self.multicall_batch_size) {
            let calls: Vec<_> = chunk
                .iter()
                .map(|user| {
                    let call_data = IPool::getUserAccountDataCall { user: *user };
                    (
                        self.pool_address,
                        alloy::sol_types::SolCall::abi_encode(&call_data),
                    )
                })
                .collect();

            let raw_results = multicall.aggregate3_at_block(calls, block_number).await?;

            for (i, raw) in raw_results.iter().enumerate() {
                if raw.success {
                    if let Ok(decoded) =
                        <IPool::getUserAccountDataCall as alloy::sol_types::SolCall>::abi_decode_returns(&raw.return_data)
                    {
                        results.push((chunk[i], decoded.healthFactor, decoded.totalDebtBase));
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
        _total_debt_base: U256,
        block_number: Option<u64>,
    ) -> Result<Option<LiquidationOpportunity>> {
        let reserves = self.fetch_reserves_list(block_number).await?;
        let data_provider = IPoolDataProvider::new(self.data_provider_address, &self.provider);

        let mut best_collateral = Address::ZERO;
        let mut best_debt = Address::ZERO;
        let mut max_collateral_usd: f64 = 0.0;
        let mut max_debt_usd: f64 = 0.0;
        let mut max_debt_raw = U256::ZERO;

        // Find the asset with the largest collateral and the largest debt for this user.
        // Compare by USD value (not raw token amounts) to handle different decimals.
        for reserve in &reserves {
            let call = data_provider.getUserReserveData(*reserve, user);
            let user_data = match block_number {
                Some(block) => call.block(block.into()).call().await,
                None => call.call().await,
            };

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
            if user_data.usageAsCollateralEnabled {
                if let Some(collateral_usd) =
                    token_value_usd(user_data.currentATokenBalance, *reserve)
                {
                    if collateral_usd > max_collateral_usd {
                        max_collateral_usd = collateral_usd;
                        best_collateral = *reserve;
                    }
                } else {
                    debug!(
                        reserve = %reserve,
                        "Skipping AAVE collateral reserve with unknown pricing"
                    );
                }
            }

            // Debt: sum of stable + variable debt
            let total_debt = user_data
                .currentStableDebt
                .checked_add(user_data.currentVariableDebt)
                .unwrap_or(U256::ZERO);
            if let Some(debt_usd) = token_value_usd(total_debt, *reserve) {
                if debt_usd > max_debt_usd {
                    max_debt_usd = debt_usd;
                    max_debt_raw = total_debt;
                    best_debt = *reserve;
                }
            } else {
                debug!(
                    reserve = %reserve,
                    "Skipping AAVE debt reserve with unknown pricing"
                );
            }
        }

        if best_collateral == Address::ZERO
            || best_debt == Address::ZERO
            || max_debt_raw == U256::ZERO
        {
            debug!(user = %user, "No suitable collateral/debt pair found");
            return Ok(None);
        }

        // AAVE v3 allows liquidating up to 50% of the debt (close factor = 0.5).
        // When health factor < 0.95e18, the close factor becomes 100%.
        let close_factor_threshold = U256::from(950_000_000_000_000_000u64); // 0.95e18
        let close_factor = if health_factor < close_factor_threshold {
            1.0
        } else {
            0.5
        };
        let debt_to_cover = if health_factor < close_factor_threshold {
            max_debt_raw
        } else {
            max_debt_raw / U256::from(2)
        };

        // Read liquidation bonus from on-chain (cached, rarely changes).
        let bonus_bps = self
            .get_liquidation_bonus(best_collateral, block_number)
            .await;
        let estimated_profit_usd = max_debt_usd * close_factor * (bonus_bps / 10_000.0);

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

    /// Discover borrowers by scanning recent Borrow events from the pool.
    ///
    /// Queries `eth_getLogs` for Borrow events over the last `scan_blocks` blocks
    /// and registers the borrower addresses in the position tracker.
    async fn discover_borrowers_from_events(&self) -> Result<()> {
        // AAVE v3 Borrow event topic0:
        // Borrow(address,address,address,uint256,uint8,uint256,uint16)
        // = 0xb3d084820fb1a9decffb176436bd02558d15fac9b0ddfed8c465bc7359d7dce0
        let borrow_topic: FixedBytes<32> =
            "0xb3d084820fb1a9decffb176436bd02558d15fac9b0ddfed8c465bc7359d7dce0"
                .parse()
                .wrap_err("Invalid borrow event topic")?;

        let latest = provider::get_latest_block_number(&self.provider).await?;
        let from_block = self.next_discovery_block().await?;
        if from_block > latest {
            debug!(
                protocol = "aave_v3",
                next_block = from_block,
                latest,
                "Borrower discovery already up to date"
            );
            return Ok(());
        }

        info!(
            protocol = "aave_v3",
            from_block,
            to_block = latest,
            "Scanning for Borrow events to discover borrowers"
        );

        // Query in batches of 100,000 blocks to avoid RPC response size limits.
        let batch_size: u64 = 100_000;
        let mut logs = Vec::new();
        let mut batch_start = from_block;
        let mut next_block = from_block;
        while batch_start <= latest {
            let batch_end = (batch_start + batch_size - 1).min(latest);
            let filter = Filter::new()
                .address(self.pool_address)
                .event_signature(borrow_topic)
                .from_block(batch_start)
                .to_block(batch_end);

            match self.provider.get_logs(&filter).await {
                Ok(batch_logs) => {
                    logs.extend(batch_logs);
                    next_block = batch_end.saturating_add(1);
                }
                Err(e) => {
                    warn!(
                        protocol = "aave_v3",
                        from = batch_start,
                        to = batch_end,
                        error = %e,
                        "Failed to fetch Borrow logs for batch, stopping incremental replay"
                    );
                    break;
                }
            }
            batch_start = batch_end + 1;
        }

        self.next_discovery_block
            .store(next_block, Ordering::Release);

        let mut count = 0usize;
        for log in &logs {
            // In the Borrow event, topic[2] is the `onBehalfOf` address (the actual borrower).
            if log.topics().len() >= 3 {
                let borrower = Address::from_word(log.topics()[2]);
                self.position_tracker.add_borrower(borrower);
                count += 1;
            }
        }

        info!(
            protocol = "aave_v3",
            events = logs.len(),
            unique_borrowers = self.position_tracker.borrower_count(),
            "Borrower discovery complete (added {} entries)",
            count
        );

        Ok(())
    }
}

impl<P: Provider + Clone + Send + Sync> Protocol for AaveV3Protocol<P> {
    fn name(&self) -> &str {
        "aave_v3"
    }

    async fn get_liquidatable_positions(
        &self,
        block_number: Option<u64>,
    ) -> Result<Vec<LiquidationOpportunity>> {
        if let Some(block_number) = block_number {
            info!(
                protocol = self.name(),
                block = block_number,
                "Scanning for liquidatable positions"
            );
        } else {
            info!(
                protocol = self.name(),
                "Scanning for liquidatable positions at latest provider state"
            );
        }

        let borrowers = self.position_tracker.get_all_borrowers();
        if borrowers.is_empty() {
            debug!(
                protocol = self.name(),
                "No tracked borrowers, skipping scan"
            );
            return Ok(Vec::new());
        }

        info!(
            protocol = self.name(),
            count = borrowers.len(),
            "Querying health factors"
        );

        // 1e18 threshold: health factor below this means the position is liquidatable.
        let threshold = U256::from(1_000_000_000_000_000_000u64);

        let health_factors = self
            .batch_query_health_factors(&borrowers, block_number)
            .await?;

        let mut opportunities = Vec::new();
        for (user, hf, total_debt_base) in health_factors {
            self.position_tracker.update_health_factor(user, hf);

            if hf < threshold && hf > U256::ZERO {
                info!(
                    protocol = self.name(),
                    user = %user,
                    health_factor = %hf,
                    "Liquidatable position found"
                );

                match self
                    .build_opportunity(user, hf, total_debt_base, block_number)
                    .await
                {
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
            found = opportunities.len(),
            "Scan complete"
        );

        Ok(opportunities)
    }

    async fn discover_borrowers(&self) -> Result<()> {
        self.refresh_rescan_targets(None).await;
        self.discover_borrowers_from_events().await
    }

    fn should_rescan_on_event(&self, event: &crate::sequencer_feed::SequencerEvent) -> bool {
        matches!(event.tx_to, Some(target) if self.rescan_targets.contains(&target))
    }
}
