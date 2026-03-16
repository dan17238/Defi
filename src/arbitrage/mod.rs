pub mod dashboard;
pub mod detector;
pub mod pairs;
pub mod pool_state;

use std::time::Instant;

use alloy::network::ReceiptResponse;
use alloy::primitives::{Address, Bytes, FixedBytes, I256, TxKind, U256};
use alloy::providers::Provider;
use alloy::rpc::types::{TransactionInput, TransactionRequest};
use alloy::sol;
use alloy::sol_types::{SolCall, SolEvent};
use eyre::{Context, Result};
use revm::context_interface::JournalTr;
use revm::database::{AlloyDB, BlockId, CacheDB, WrapDatabaseAsync};
use revm::handler::MainnetContext;
use revm::primitives::hardfork::SpecId;
use revm::MainBuilder;
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

use crate::config::ArbitrageConfig;
use crate::sequencer_feed::SequencerEvent;
use crate::utils::metrics::Metrics;

use self::dashboard::{ArbDashboard, ArbPairSnapshot};
use self::detector::{ArbitrageDetector, ArbitrageOpportunity};
use self::pairs::PoolPair;
use self::pool_state::PoolStateCache;

// ---------------------------------------------------------------------------
// FlashArbitrage ABI (must match FlashArbitrage.sol)
// ---------------------------------------------------------------------------

sol! {
    #[sol(rpc)]
    interface IFlashArbitrage {
        struct ArbParams {
            address poolA;
            address poolB;
            bool zeroForOne;
            int256 amountIn;
            uint256 minProfit;
        }

        function executeArbitrage(ArbParams calldata params) external;

        event ArbitrageExecuted(
            address indexed poolA,
            address indexed poolB,
            address tokenProfit,
            uint256 profit
        );
    }
}

const ARB_EXECUTED_TOPIC: FixedBytes<32> = IFlashArbitrage::ArbitrageExecuted::SIGNATURE_HASH;

// ---------------------------------------------------------------------------
// ArbitrageMonitor
// ---------------------------------------------------------------------------

/// Orchestrates DEX arbitrage: listens for sequencer events, refreshes pool
/// states, detects price spreads, simulates via revm, and executes.
pub struct ArbitrageMonitor<R, E> {
    read_provider: R,
    exec_provider: E,
    config: ArbitrageConfig,
    pool_cache: PoolStateCache,
    detector: ArbitrageDetector,
    metrics: Metrics,
    wallet_address: Address,
    flash_arb_contract: Address,
    dashboard: ArbDashboard,
}

impl<R, E> ArbitrageMonitor<R, E>
where
    R: Provider + Clone + Send + Sync + 'static,
    E: Provider + Clone + Send + Sync + 'static,
{
    /// Create and initialize a new ArbitrageMonitor.
    pub async fn new(
        read_provider: R,
        exec_provider: E,
        config: ArbitrageConfig,
        wallet_address: Address,
        flash_arb_contract: Address,
        metrics: Metrics,
        dashboard: ArbDashboard,
    ) -> Result<Self> {
        let pairs = pairs::parse_pairs(&config.pairs)?;

        // Collect all unique pool addresses for the state cache
        let pool_addresses: Vec<Address> = pairs
            .iter()
            .flat_map(PoolPair::pool_addresses)
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        let pool_cache = PoolStateCache::new(pool_addresses);

        // Initialize pool states (batch read via Multicall)
        pool_cache
            .initialize(&read_provider)
            .await
            .wrap_err("Failed to initialize pool state cache")?;

        // Validate pair metadata against on-chain pool state up front so we fail
        // closed on stale config instead of discovering it only after simulation.
        for pair in &pairs {
            let state_a = pool_cache.get(&pair.pool_a).ok_or_else(|| {
                eyre::eyre!("Pair '{}': failed to read pool_a {} state", pair.name, pair.pool_a)
            })?;
            let state_b = pool_cache.get(&pair.pool_b).ok_or_else(|| {
                eyre::eyre!("Pair '{}': failed to read pool_b {} state", pair.name, pair.pool_b)
            })?;
            if pair.pool_a == pair.pool_b {
                eyre::bail!("Pair '{}': pool_a and pool_b must be different pools", pair.name);
            }
            if state_a.token0 != state_b.token0 || state_a.token1 != state_b.token1 {
                eyre::bail!(
                    "Pair '{}': pools have different tokens! pool_a=({},{}) pool_b=({},{})",
                    pair.name, state_a.token0, state_a.token1, state_b.token0, state_b.token1
                );
            }
            if state_a.token0 != pair.token0 || state_a.token1 != pair.token1 {
                eyre::bail!(
                    "Pair '{}': configured tokens ({},{}) do not match on-chain pool_a ({},{})",
                    pair.name, pair.token0, pair.token1, state_a.token0, state_a.token1
                );
            }
            if state_a.fee != pair.fee_a || state_b.fee != pair.fee_b {
                eyre::bail!(
                    "Pair '{}': configured fees ({},{}) do not match on-chain fees ({},{})",
                    pair.name, pair.fee_a, pair.fee_b, state_a.fee, state_b.fee
                );
            }
        }

        // Gas margin: ~5 bps covers typical Arbitrum L2 gas costs
        let detector = ArbitrageDetector::new(pairs, 5.0);

        info!(
            pairs = config.pairs.len(),
            contract = %flash_arb_contract,
            "Arbitrage monitor initialized"
        );

        Ok(Self {
            read_provider,
            exec_provider,
            config,
            pool_cache,
            detector,
            metrics,
            wallet_address,
            flash_arb_contract,
            dashboard,
        })
    }

    /// Main event loop: listens for sequencer feed events and shutdown signal.
    pub async fn run(
        &self,
        feed_rx: &mut broadcast::Receiver<SequencerEvent>,
        shutdown_rx: &mut broadcast::Receiver<()>,
    ) {
        loop {
            tokio::select! {
                result = feed_rx.recv() => {
                    match result {
                        Ok(event) => {
                            self.on_sequencer_event(&event).await;
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            warn!(skipped = n, "Arb monitor lagged behind sequencer feed");
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            info!("Sequencer feed closed, stopping arb monitor");
                            break;
                        }
                    }
                }
                _ = shutdown_rx.recv() => {
                    info!("Arb monitor received shutdown signal");
                    break;
                }
            }
        }
    }

    /// Handle a sequencer event: refresh pools → detect → simulate → execute.
    async fn on_sequencer_event(&self, event: &SequencerEvent) {
        let start = Instant::now();

        // 1. Refresh pool states via Multicall (~0.3ms over IPC)
        if let Err(e) = self.pool_cache.refresh(&self.read_provider).await {
            warn!(error = %e, "Failed to refresh pool states");
            return;
        }

        // 2. Detect arbitrage opportunities (<0.1ms)
        self.dashboard.record_scan();
        self.push_pair_snapshots();
        let opportunities = self.detector.scan_all_pairs(&self.pool_cache);
        if opportunities.is_empty() {
            return;
        }

        debug!(
            count = opportunities.len(),
            latency_us = start.elapsed().as_micros() as u64,
            "Detected {} potential arbitrage opportunities",
            opportunities.len()
        );

        // 3. Process each opportunity: simulate then execute
        for opp in opportunities {
            if let Err(e) = self.process_opportunity(&opp).await {
                debug!(
                    pair = %opp.pair_name,
                    error = %e,
                    "Arbitrage opportunity not viable"
                );
            }
        }

        let total_latency = start.elapsed();
        debug!(
            latency_ms = total_latency.as_millis() as u64,
            seq = event.sequence_number,
            "Arbitrage scan complete"
        );
    }

    /// Simulate and execute a single arbitrage opportunity.
    async fn process_opportunity(&self, opp: &ArbitrageOpportunity) -> Result<()> {
        self.dashboard.record_detected(&opp.pair_name, opp.estimated_profit_bps, 0.0);

        // Build calldata for FlashArbitrage.executeArbitrage()
        let min_profit_tokens = match self.compute_min_profit_tokens(opp) {
            Ok(value) => value,
            Err(e) => {
                warn!(pair = %opp.pair_name, error = %e, "Skipping arb with unsupported profit token");
                return Ok(());
            }
        };
        let calldata = self.encode_arb_calldata(opp, min_profit_tokens);

        // Simulate via revm
        let sim_result = self
            .simulate_arbitrage(calldata.clone(), opp)
            .await
            .wrap_err("revm simulation failed")?;

        self.dashboard.record_simulated(
            &opp.pair_name,
            sim_result.profit_usd,
            sim_result.gas_used,
            sim_result.reverted || !sim_result.profitable,
        );

        if !sim_result.profitable {
            if sim_result.reverted {
                debug!(pair = %opp.pair_name, "Simulation reverted");
            }
            return Ok(());
        }

        info!(
            pair = %opp.pair_name,
            profit_usd = sim_result.profit_usd,
            gas_used = sim_result.gas_used,
            "Profitable arbitrage found, executing"
        );

        if self.config.dry_run {
            info!(
                pair = %opp.pair_name,
                profit_usd = sim_result.profit_usd,
                gas_gwei = sim_result.selected_gas_price_gwei,
                "DRY RUN: would execute arbitrage"
            );
            return Ok(());
        }

        // Execute on-chain
        match self
            .send_arb_tx(
                calldata,
                sim_result.selected_gas_price_gwei,
                opp.pair_name.clone(),
                sim_result.profit_usd,
            )
            .await
        {
            Ok(tx_hash) => {
                self.metrics.record_arbitrage_attempt();
                info!(
                    pair = %opp.pair_name,
                    tx = %tx_hash,
                    profit_usd = sim_result.profit_usd,
                    "Arbitrage transaction submitted"
                );
            }
            Err(e) => {
                error!(pair = %opp.pair_name, error = %e, "Failed to submit arb tx");
                self.metrics.record_error();
            }
        }

        Ok(())
    }

    /// Encode FlashArbitrage.executeArbitrage() calldata.
    fn encode_arb_calldata(&self, opp: &ArbitrageOpportunity, min_profit: U256) -> Bytes {
        let params = IFlashArbitrage::ArbParams {
            poolA: opp.pool_a,
            poolB: opp.pool_b,
            zeroForOne: opp.zero_for_one,
            amountIn: I256::try_from(opp.amount_in).unwrap_or(I256::ZERO),
            minProfit: min_profit,
        };
        let call = IFlashArbitrage::executeArbitrageCall { params };
        Bytes::from(call.abi_encode())
    }

    /// Compute minimum profit in token units from config USD threshold.
    /// The profit token is determined by the arb direction:
    ///   zeroForOne=true on pool_a → profit is in token0.
    fn compute_min_profit_tokens(&self, opp: &ArbitrageOpportunity) -> Result<U256> {
        let profit_token = if let Some(state) = self.pool_cache.get(&opp.pool_a) {
            if opp.zero_for_one { state.token0 } else { state.token1 }
        } else {
            eyre::bail!("missing cached state for pool {}", opp.pool_a);
        };

        crate::liquidator::flash_loan::tokens::usd_to_token_units(profit_token, self.config.min_profit_usd)
            .ok_or_else(|| eyre::eyre!("unsupported profit token {}", profit_token))
    }

    /// Simulate the arbitrage transaction via revm.
    async fn simulate_arbitrage(
        &self,
        calldata: Bytes,
        opp: &ArbitrageOpportunity,
    ) -> Result<ArbSimResult> {
        let alloy_db = AlloyDB::new(self.read_provider.clone(), BlockId::latest());
        let wrapped_db = WrapDatabaseAsync::new(alloy_db)
            .ok_or_else(|| eyre::eyre!("No tokio runtime for WrapDatabaseAsync"))?;
        let cache_db = CacheDB::new(wrapped_db);

        // Prewarm cache: contract + both pools + both tokens.
        // Without this, ERC20 transfers in the callback trigger on-demand RPC fetches.
        {
            use revm::database::DatabaseRef;
            let mut addrs = vec![
                self.flash_arb_contract,
                opp.pool_a,
                opp.pool_b,
            ];
            if let Some(state) = self.pool_cache.get(&opp.pool_a) {
                addrs.push(state.token0);
                addrs.push(state.token1);
            }
            for addr in &addrs {
                let _ = cache_db.basic_ref(*addr);
            }
        }

        type SimDB<P> = CacheDB<WrapDatabaseAsync<AlloyDB<alloy::network::Ethereum, P>>>;

        let tx = revm::context::TxEnv::builder()
            .caller(self.wallet_address)
            .kind(TxKind::Call(self.flash_arb_contract))
            .data(calldata.clone())
            .gas_limit(3_000_000)
            .gas_price(100_000_000) // 0.1 gwei for sim
            .value(U256::ZERO)
            .nonce(0)
            .build_fill();

        let ctx: MainnetContext<SimDB<R>> = revm::context::Context {
            tx,
            block: revm::context::BlockEnv {
                number: U256::ZERO,
                ..Default::default()
            },
            cfg: revm::context::CfgEnv::new_with_spec(SpecId::CANCUN)
                .with_chain_id(42161),
            journaled_state: revm::Journal::new(cache_db),
            chain: (),
            local: Default::default(),
            error: Ok(()),
        };

        let mut evm = ctx.build_mainnet();

        let tx_for_exec = revm::context::TxEnv::builder()
            .caller(self.wallet_address)
            .kind(TxKind::Call(self.flash_arb_contract))
            .data(calldata)
            .gas_limit(3_000_000)
            .gas_price(100_000_000)
            .value(U256::ZERO)
            .nonce(0)
            .build_fill();

        let result = revm::ExecuteEvm::transact(&mut evm, tx_for_exec)
            .wrap_err("revm transact failed")?;

        match result.result {
            revm::context_interface::result::ExecutionResult::Success {
                gas, logs, ..
            } => {
                let gas_used = gas.used();

                // Extract realized token profit from ArbitrageExecuted.
                let mut gross_profit_usd = None;
                for log in &logs {
                    if log.topics().first() == Some(&ARB_EXECUTED_TOPIC) {
                        let data = log.data.data.as_ref();
                        if let Some((token_profit, profit_tokens)) = Self::decode_profit_event_data(data) {
                            gross_profit_usd =
                                crate::liquidator::flash_loan::tokens::token_value_usd(profit_tokens, token_profit);
                        }
                        break;
                    }
                }

                let gross_profit_usd = gross_profit_usd.unwrap_or(0.0);
                let (selected_gas_price_gwei, profit_usd) = estimate_net_profit_after_gas(
                    gross_profit_usd,
                    gas_used,
                    self.config.max_gas_price_gwei,
                );

                Ok(ArbSimResult {
                    profitable: profit_usd >= self.config.min_profit_usd,
                    profit_usd,
                    gas_used,
                    reverted: false,
                    selected_gas_price_gwei,
                })
            }
            revm::context_interface::result::ExecutionResult::Revert { gas, .. } => {
                Ok(ArbSimResult {
                    profitable: false,
                    profit_usd: 0.0,
                    gas_used: gas.used(),
                    reverted: true,
                    selected_gas_price_gwei: 0.0,
                })
            }
            revm::context_interface::result::ExecutionResult::Halt { gas, .. } => {
                Ok(ArbSimResult {
                    profitable: false,
                    profit_usd: 0.0,
                    gas_used: gas.used(),
                    reverted: true,
                    selected_gas_price_gwei: 0.0,
                })
            }
        }
    }

    /// Send the arbitrage transaction to the sequencer.
    async fn send_arb_tx(
        &self,
        calldata: Bytes,
        gas_price_gwei: f64,
        pair_name: String,
        expected_profit_usd: f64,
    ) -> Result<FixedBytes<32>> {
        let gas_price_wei = (gas_price_gwei * 1e9) as u128;

        let tx_request = TransactionRequest::default()
            .to(self.flash_arb_contract)
            .input(TransactionInput::new(calldata))
            .gas_price(gas_price_wei);

        let send_start = Instant::now();

        let pending = self
            .exec_provider
            .send_transaction(tx_request)
            .await
            .wrap_err("Failed to send arbitrage transaction")?;

        let tx_hash = *pending.tx_hash();
        let latency = send_start.elapsed();

        info!(
            tx = %tx_hash,
            latency_ms = latency.as_millis() as u64,
            gas_gwei = gas_price_gwei,
            "Arb tx submitted"
        );

        self.metrics.record_latency_us(latency.as_micros() as u64);

        let metrics = self.metrics.clone();
        let dash = self.dashboard.clone();
        let tx_str = format!("{tx_hash:#x}");
        dash.record_submitted(&pair_name, expected_profit_usd, &tx_str, latency.as_millis() as u64);

        tokio::spawn(async move {
            match pending.get_receipt().await {
                Ok(receipt) if receipt.status() => {
                    let gas_cost_usd = Self::gas_cost_usd(receipt.gas_used(), receipt.effective_gas_price());
                    let gross_profit_usd = Self::extract_realized_profit(&receipt);
                    if gross_profit_usd.is_none() {
                        warn!(
                            pair = %pair_name,
                            tx = %tx_hash,
                            "Confirmed arb tx missing ArbitrageExecuted profit data; recording gas-only net profit"
                        );
                    }
                    let net_profit_usd = gross_profit_usd.unwrap_or(0.0) - gas_cost_usd;
                    metrics.record_arbitrage_success(net_profit_usd);
                    dash.record_confirmed(&pair_name, net_profit_usd, &tx_str, receipt.gas_used());
                    info!(
                        pair = %pair_name, tx = %tx_hash,
                        gas_used = receipt.gas_used(),
                        net_profit_usd,
                        "Arb tx confirmed"
                    );
                }
                Ok(receipt) => {
                    metrics.record_error();
                    dash.record_reverted(&pair_name, &tx_str, receipt.gas_used());
                    warn!(pair = %pair_name, tx = %tx_hash, gas_used = receipt.gas_used(), "Arb tx reverted on-chain");
                }
                Err(e) => {
                    metrics.record_error();
                    warn!(pair = %pair_name, tx = %tx_hash, error = %e, "Failed to fetch arb receipt");
                }
            }
        });

        Ok(tx_hash)
    }

    /// Extract realized gross profit in USD from ArbitrageExecuted.
    fn extract_realized_profit(receipt: &alloy::rpc::types::TransactionReceipt) -> Option<f64> {
        for log in receipt.inner.logs() {
            if log.topics().first() == Some(&ARB_EXECUTED_TOPIC) {
                let data = log.data().data.as_ref();
                if let Some((token_profit, profit_tokens)) = Self::decode_profit_event_data(data) {
                    return crate::liquidator::flash_loan::tokens::token_value_usd(profit_tokens, token_profit);
                }
            }
        }
        None
    }

    /// Decode ArbitrageExecuted non-indexed data.
    /// ABI layout: tokenProfit (address, 32 bytes) + profit (uint256, 32 bytes)
    fn decode_profit_event_data(data: &[u8]) -> Option<(Address, U256)> {
        if data.len() < 64 {
            return None;
        }
        let token_profit = Address::from_slice(&data[12..32]);
        let profit_tokens = U256::from_be_slice(&data[32..64]);
        Some((token_profit, profit_tokens))
    }

    fn eth_price_usd() -> f64 {
        let eth_price = crate::protocols::radiant::CACHED_ETH_PRICE_CENTS
            .load(std::sync::atomic::Ordering::Relaxed) as f64 / 100.0;
        if eth_price > 100.0 { eth_price } else { 3500.0 }
    }

    fn gas_cost_usd(gas_used: u64, gas_price_wei: u128) -> f64 {
        let gas_cost_eth = gas_used as f64 * gas_price_wei as f64 / 1e18;
        gas_cost_eth * Self::eth_price_usd()
    }

    fn display_pool_price(sqrt_price_x96: &U256, token0: Address, token1: Address) -> f64 {
        let (decimals0, _) = match crate::liquidator::flash_loan::tokens::token_info(token0) {
            Some(info) => info,
            None => return 0.0,
        };
        let (decimals1, _) = match crate::liquidator::flash_loan::tokens::token_info(token1) {
            Some(info) => info,
            None => return 0.0,
        };

        let sqrt = detector::sqrt_price_to_f64(sqrt_price_x96);
        let two96 = 2.0_f64.powi(96);
        if sqrt <= 0.0 || two96 <= 0.0 {
            return 0.0;
        }

        let raw_price = (sqrt / two96) * (sqrt / two96);
        raw_price * 10_f64.powi(decimals0 as i32 - decimals1 as i32)
    }

    /// Push current pool pair snapshots to the dashboard.
    fn push_pair_snapshots(&self) {
        let pairs = self.detector.pairs();
        let mut snapshots = Vec::with_capacity(pairs.len());
        for pair in pairs {
            let (price_a, price_b, liq_a, liq_b) =
                if let (Some(a), Some(b)) = (self.pool_cache.get(&pair.pool_a), self.pool_cache.get(&pair.pool_b)) {
                    let pa = Self::display_pool_price(&a.sqrt_price_x96, a.token0, a.token1);
                    let pb = Self::display_pool_price(&b.sqrt_price_x96, b.token0, b.token1);
                    (pa, pb, a.liquidity, b.liquidity)
                } else {
                    continue;
                };

            let spread = if price_a.max(price_b) > 0.0 {
                ((price_a - price_b).abs() / price_a.max(price_b)) * 10_000.0
            } else { 0.0 };
            let fee_threshold = pair.total_fee_bps() + 5.0;

            snapshots.push(ArbPairSnapshot {
                name: pair.name.clone(),
                pool_a: format!("{:#x}", pair.pool_a),
                pool_b: format!("{:#x}", pair.pool_b),
                price_a,
                price_b,
                spread_bps: spread,
                fee_threshold_bps: fee_threshold,
                liquidity_a: format!("{}", liq_a),
                liquidity_b: format!("{}", liq_b),
                profitable: spread > fee_threshold,
            });
        }
        self.dashboard.update_pairs(snapshots);
    }
}

/// Result of an arbitrage simulation.
#[derive(Debug)]
struct ArbSimResult {
    profitable: bool,
    profit_usd: f64,
    gas_used: u64,
    reverted: bool,
    selected_gas_price_gwei: f64,
}

/// Dynamic gas pricing based on profit.
/// Higher profit → willing to pay more gas to beat competitors.
fn select_gas_price(profit_usd: f64, max_gwei: f64) -> f64 {
    let gwei: f64 = if profit_usd > 50.0 {
        1.0
    } else if profit_usd > 10.0 {
        0.1
    } else {
        0.02
    };
    gwei.min(max_gwei)
}

fn estimate_net_profit_after_gas(
    gross_profit_usd: f64,
    gas_used: u64,
    max_gwei: f64,
) -> (f64, f64) {
    let selected_gas_price_gwei = select_gas_price(gross_profit_usd, max_gwei);
    let gas_price_wei = (selected_gas_price_gwei * 1e9) as u128;
    let eth_price = crate::protocols::radiant::CACHED_ETH_PRICE_CENTS
        .load(std::sync::atomic::Ordering::Relaxed) as f64 / 100.0;
    let eth_price = if eth_price > 100.0 { eth_price } else { 3500.0 };
    let gas_cost_usd = gas_used as f64 * gas_price_wei as f64 / 1e18 * eth_price;
    (selected_gas_price_gwei, gross_profit_usd - gas_cost_usd)
}

#[cfg(test)]
mod tests {
    use super::{estimate_net_profit_after_gas, select_gas_price};

    #[test]
    fn simulation_uses_same_high_profit_gas_tier_as_execution() {
        let (gas_gwei, net_profit) = estimate_net_profit_after_gas(60.0, 500_000, 2.0);
        assert_eq!(gas_gwei, 1.0);
        assert!(net_profit < 60.0);
    }

    #[test]
    fn gas_price_selection_respects_max_cap() {
        assert_eq!(select_gas_price(100.0, 0.2), 0.2);
        assert_eq!(select_gas_price(20.0, 2.0), 0.1);
        assert_eq!(select_gas_price(1.0, 2.0), 0.02);
    }
}
