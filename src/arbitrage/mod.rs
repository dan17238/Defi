pub mod detector;
pub mod pairs;
pub mod pool_state;

use std::time::Instant;

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
}

impl<R, E> ArbitrageMonitor<R, E>
where
    R: Provider + Clone + Send + Sync,
    E: Provider + Clone + Send + Sync,
{
    /// Create and initialize a new ArbitrageMonitor.
    pub async fn new(
        read_provider: R,
        exec_provider: E,
        config: ArbitrageConfig,
        wallet_address: Address,
        flash_arb_contract: Address,
        metrics: Metrics,
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

        // Validate that both pools in each pair share the same token0/token1.
        // FlashArbitrage assumes identical token pairs; mismatched tokens will revert.
        for pair in &pairs {
            let state_a = pool_cache.get(&pair.pool_a).ok_or_else(|| {
                eyre::eyre!("Pair '{}': failed to read pool_a {} state", pair.name, pair.pool_a)
            })?;
            let state_b = pool_cache.get(&pair.pool_b).ok_or_else(|| {
                eyre::eyre!("Pair '{}': failed to read pool_b {} state", pair.name, pair.pool_b)
            })?;
            if state_a.token0 != state_b.token0 || state_a.token1 != state_b.token1 {
                eyre::bail!(
                    "Pair '{}': pools have different tokens! pool_a=({},{}) pool_b=({},{})",
                    pair.name, state_a.token0, state_a.token1, state_b.token0, state_b.token1
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
        // Build calldata for FlashArbitrage.executeArbitrage()
        let min_profit_tokens = self.compute_min_profit_tokens(opp);
        let calldata = self.encode_arb_calldata(opp, min_profit_tokens);

        // Simulate via revm
        let sim_result = self
            .simulate_arbitrage(calldata.clone(), opp)
            .await
            .wrap_err("revm simulation failed")?;

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

        // Determine gas price based on profit
        let gas_price_gwei = select_gas_price(sim_result.profit_usd, self.config.max_gas_price_gwei);

        if self.config.dry_run {
            info!(
                pair = %opp.pair_name,
                profit_usd = sim_result.profit_usd,
                gas_gwei = gas_price_gwei,
                "DRY RUN: would execute arbitrage"
            );
            return Ok(());
        }

        // Execute on-chain
        match self.send_arb_tx(calldata, gas_price_gwei).await {
            Ok(tx_hash) => {
                info!(
                    pair = %opp.pair_name,
                    tx = %tx_hash,
                    profit_usd = sim_result.profit_usd,
                    "Arbitrage transaction submitted"
                );
                self.metrics.record_arbitrage(sim_result.profit_usd, true);
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
    fn compute_min_profit_tokens(&self, opp: &ArbitrageOpportunity) -> U256 {
        let profit_token = if let Some(state) = self.pool_cache.get(&opp.pool_a) {
            if opp.zero_for_one { state.token0 } else { state.token1 }
        } else {
            return U256::ZERO;
        };

        crate::liquidator::flash_loan::tokens::usd_to_token_units(
            profit_token,
            self.config.min_profit_usd,
        )
        .unwrap_or(U256::ZERO)
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

                // Extract profit from ArbitrageExecuted event
                let mut profit_tokens = U256::ZERO;
                for log in &logs {
                    if log.topics().first() == Some(&ARB_EXECUTED_TOPIC) {
                        let data = log.data.data.as_ref();
                        // Event data: tokenProfit (address, 32 bytes) + profit (uint256, 32 bytes)
                        if data.len() >= 64 {
                            profit_tokens = U256::from_be_slice(&data[32..64]);
                        }
                        break;
                    }
                }

                // Estimate gas cost in USD
                let eth_price = crate::protocols::radiant::CACHED_ETH_PRICE_CENTS
                    .load(std::sync::atomic::Ordering::Relaxed) as f64 / 100.0;
                let eth_price = if eth_price > 100.0 { eth_price } else { 3500.0 };
                let gas_cost_usd = gas_used as f64 * 0.1 / 1e9 * eth_price + 0.03;

                // Rough profit estimate in USD using token pricing
                let profit_usd = if !profit_tokens.is_zero() {
                    crate::liquidator::flash_loan::tokens::token_value_usd(
                        profit_tokens,
                        // The profit token depends on direction; use a rough estimate
                        // by trying both tokens of the pair
                        if let Some(state) = self.pool_cache.get(&opp.pool_a) {
                            if opp.zero_for_one { state.token0 } else { state.token1 }
                        } else {
                            Address::ZERO
                        },
                    )
                    .unwrap_or(0.0)
                    - gas_cost_usd
                } else {
                    -gas_cost_usd
                };

                Ok(ArbSimResult {
                    profitable: profit_usd >= self.config.min_profit_usd,
                    profit_usd,
                    gas_used,
                    reverted: false,
                })
            }
            revm::context_interface::result::ExecutionResult::Revert { gas, .. } => {
                Ok(ArbSimResult {
                    profitable: false,
                    profit_usd: 0.0,
                    gas_used: gas.used(),
                    reverted: true,
                })
            }
            revm::context_interface::result::ExecutionResult::Halt { gas, .. } => {
                Ok(ArbSimResult {
                    profitable: false,
                    profit_usd: 0.0,
                    gas_used: gas.used(),
                    reverted: true,
                })
            }
        }
    }

    /// Send the arbitrage transaction to the sequencer.
    async fn send_arb_tx(
        &self,
        calldata: Bytes,
        gas_price_gwei: f64,
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

        Ok(tx_hash)
    }
}

/// Result of an arbitrage simulation.
#[derive(Debug)]
struct ArbSimResult {
    profitable: bool,
    profit_usd: f64,
    gas_used: u64,
    reverted: bool,
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
