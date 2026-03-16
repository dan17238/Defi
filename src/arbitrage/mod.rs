pub mod dashboard;
pub mod detector;
pub mod pairs;
pub mod pool_state;

use std::time::Instant;

use alloy::network::ReceiptResponse;
use alloy::primitives::{Address, Bytes, FixedBytes, TxKind, I256, U256};
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
use self::detector::{
    estimate_route_gross_spread_bps, ArbitrageDetector, ArbitrageOpportunity, ResolvedRoute,
};
use self::pairs::PoolPair;
use self::pool_state::PoolStateCache;

use crate::config::ArbitrageRouteConfig;

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

        struct MultiHopParams {
            address[] pools;
            bool[] zeroForOne;
            int256 amountIn;
            uint256 minProfit;
        }

        function executeArbitrage(ArbParams calldata params) external;
        function executeMultiHop(MultiHopParams calldata params) external;

        event ArbitrageExecuted(
            address indexed poolFirst,
            address indexed poolLast,
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

        // Collect all unique pool addresses from pairs AND routes
        let mut all_pools: std::collections::HashSet<Address> =
            pairs.iter().flat_map(PoolPair::pool_addresses).collect();
        for route_cfg in &config.routes {
            for pool_str in &route_cfg.pools {
                if let Ok(addr) = pool_str.parse::<Address>() {
                    all_pools.insert(addr);
                }
            }
        }
        let pool_addresses: Vec<Address> = all_pools.into_iter().collect();

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
                eyre::eyre!(
                    "Pair '{}': failed to read pool_a {} state",
                    pair.name,
                    pair.pool_a
                )
            })?;
            let state_b = pool_cache.get(&pair.pool_b).ok_or_else(|| {
                eyre::eyre!(
                    "Pair '{}': failed to read pool_b {} state",
                    pair.name,
                    pair.pool_b
                )
            })?;
            if pair.pool_a == pair.pool_b {
                eyre::bail!(
                    "Pair '{}': pool_a and pool_b must be different pools",
                    pair.name
                );
            }
            if state_a.token0 != state_b.token0 || state_a.token1 != state_b.token1 {
                eyre::bail!(
                    "Pair '{}': pools have different tokens! pool_a=({},{}) pool_b=({},{})",
                    pair.name,
                    state_a.token0,
                    state_a.token1,
                    state_b.token0,
                    state_b.token1
                );
            }
            if state_a.token0 != pair.token0 || state_a.token1 != pair.token1 {
                eyre::bail!(
                    "Pair '{}': configured tokens ({},{}) do not match on-chain pool_a ({},{})",
                    pair.name,
                    pair.token0,
                    pair.token1,
                    state_a.token0,
                    state_a.token1
                );
            }
            if state_a.fee != pair.fee_a || state_b.fee != pair.fee_b {
                eyre::bail!(
                    "Pair '{}': configured fees ({},{}) do not match on-chain fees ({},{})",
                    pair.name,
                    pair.fee_a,
                    pair.fee_b,
                    state_a.fee,
                    state_b.fee
                );
            }
        }

        // Gas margin: ~5 bps covers typical Arbitrum L2 gas costs
        let mut detector = ArbitrageDetector::new(pairs, 5.0);

        // Resolve multi-hop routes: read each pool's token0/token1 from cache,
        // auto-compute zeroForOne directions, validate circular path.
        let mut resolved_routes = Vec::new();
        for route_cfg in &config.routes {
            match Self::resolve_route(route_cfg, &pool_cache) {
                Ok(route) => {
                    info!(
                        route = %route.name,
                        hops = route.pools.len(),
                        total_fee = route.total_fee_bps,
                        "Route resolved"
                    );
                    resolved_routes.push(route);
                }
                Err(e) => {
                    eyre::bail!("Route '{}': {}", route_cfg.name, e);
                }
            }
        }
        detector.set_routes(resolved_routes);

        info!(
            pairs = config.pairs.len(),
            routes = config.routes.len(),
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
        self.dashboard
            .record_detected(&opp.pair_name, opp.estimated_profit_bps, 0.0);

        // Build calldata for FlashArbitrage.executeArbitrage()
        let min_profit_tokens = match self.compute_min_profit_tokens(opp) {
            Ok(value) => value,
            Err(e) => {
                warn!(pair = %opp.pair_name, error = %e, "Skipping arb with unsupported profit token");
                return Ok(());
            }
        };
        let calldata = self.encode_arb_calldata(opp, min_profit_tokens)?;

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

    /// Encode calldata — uses executeMultiHop for all routes (including 2-pool).
    fn encode_arb_calldata(&self, opp: &ArbitrageOpportunity, min_profit: U256) -> Result<Bytes> {
        let amount_in = I256::try_from(opp.amount_in)
            .map_err(|_| eyre::eyre!("amount_in {} overflows I256", opp.amount_in))?;
        let params = IFlashArbitrage::MultiHopParams {
            pools: opp.pools.clone(),
            zeroForOne: opp.zero_for_one.clone(),
            amountIn: amount_in,
            minProfit: min_profit,
        };
        let call = IFlashArbitrage::executeMultiHopCall { params };
        Ok(Bytes::from(call.abi_encode()))
    }

    /// Compute minimum profit in token units from config USD threshold.
    /// Profit token = what pool[0] wants back (determined by zeroForOne[0]).
    fn compute_min_profit_tokens(&self, opp: &ArbitrageOpportunity) -> Result<U256> {
        let first_pool = opp
            .pools
            .first()
            .ok_or_else(|| eyre::eyre!("empty route"))?;
        let profit_token = if let Some(state) = self.pool_cache.get(first_pool) {
            if opp.zero_for_one[0] {
                state.token0
            } else {
                state.token1
            }
        } else {
            eyre::bail!("missing cached state for pool {}", first_pool);
        };

        crate::liquidator::flash_loan::tokens::usd_to_token_units(
            profit_token,
            self.config.min_profit_usd,
        )
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

        // Prewarm cache: contract + all route pools + all tokens.
        {
            use revm::database::DatabaseRef;
            let mut addrs = vec![self.flash_arb_contract];
            for pool in &opp.pools {
                addrs.push(*pool);
                if let Some(state) = self.pool_cache.get(pool) {
                    addrs.push(state.token0);
                    addrs.push(state.token1);
                }
            }
            // Deduplicate
            addrs.sort();
            addrs.dedup();
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
            cfg: revm::context::CfgEnv::new_with_spec(SpecId::CANCUN).with_chain_id(42161),
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

        let result =
            revm::ExecuteEvm::transact(&mut evm, tx_for_exec).wrap_err("revm transact failed")?;

        match result.result {
            revm::context_interface::result::ExecutionResult::Success { gas, logs, .. } => {
                let gas_used = gas.used();

                // Extract realized token profit from ArbitrageExecuted.
                let mut gross_profit_usd = None;
                for log in &logs {
                    if log.topics().first() == Some(&ARB_EXECUTED_TOPIC) {
                        let data = log.data.data.as_ref();
                        if let Some((token_profit, profit_tokens)) =
                            Self::decode_profit_event_data(data)
                        {
                            gross_profit_usd =
                                crate::liquidator::flash_loan::tokens::token_value_usd(
                                    profit_tokens,
                                    token_profit,
                                );
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
        let gas_price_wei = (gas_price_gwei * 1e9).round() as u128;

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
        dash.record_submitted(
            &pair_name,
            expected_profit_usd,
            &tx_str,
            latency.as_millis() as u64,
        );

        tokio::spawn(async move {
            match pending.get_receipt().await {
                Ok(receipt) if receipt.status() => {
                    let gas_cost_usd =
                        Self::gas_cost_usd(receipt.gas_used(), receipt.effective_gas_price());
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
                    return crate::liquidator::flash_loan::tokens::token_value_usd(
                        profit_tokens,
                        token_profit,
                    );
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
            .load(std::sync::atomic::Ordering::Relaxed) as f64
            / 100.0;
        if eth_price > 100.0 {
            eth_price
        } else {
            3500.0
        }
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

    /// Resolve a route config into a ResolvedRoute by reading on-chain token0/token1
    /// and auto-computing zeroForOne for each hop. Validates the route is circular.
    fn resolve_route(cfg: &ArbitrageRouteConfig, cache: &PoolStateCache) -> Result<ResolvedRoute> {
        let input_token: Address = cfg
            .input_token
            .parse()
            .wrap_err("invalid input_token address")?;
        let pools: Vec<Address> = cfg
            .pools
            .iter()
            .map(|p| p.parse().wrap_err("invalid pool address"))
            .collect::<Result<Vec<_>>>()?;

        if pools.len() < 2 {
            eyre::bail!("route needs at least 2 pools");
        }

        // Check for duplicate pool addresses
        let unique: std::collections::HashSet<_> = pools.iter().collect();
        if unique.len() != pools.len() {
            eyre::bail!("route contains duplicate pool addresses");
        }

        // Read token0/token1 for the first pool to determine initial direction
        let first_state = cache
            .get(&pools[0])
            .ok_or_else(|| eyre::eyre!("pool {} not in cache", pools[0]))?;

        // pool[0] wants input_token back. Determine zeroForOne[0]:
        // If input_token == token0 → zeroForOne = true (pool gives token1, wants token0)
        // If input_token == token1 → zeroForOne = false (pool gives token0, wants token1)
        let zfo_0 = if input_token == first_state.token0 {
            true
        } else if input_token == first_state.token1 {
            false
        } else {
            eyre::bail!(
                "input_token {} not found in pool[0] ({},{})",
                input_token,
                first_state.token0,
                first_state.token1
            );
        };

        // Track current token flowing through the route
        let mut current_token = if zfo_0 {
            first_state.token1
        } else {
            first_state.token0
        };
        let mut zero_for_one = vec![zfo_0];
        let mut total_fee = first_state.fee as f64;

        // Resolve remaining hops
        for (i, pool) in pools.iter().enumerate().skip(1) {
            let state = cache
                .get(pool)
                .ok_or_else(|| eyre::eyre!("pool {} not in cache", pool))?;

            let zfo = if current_token == state.token0 {
                true // swap token0→token1
            } else if current_token == state.token1 {
                false // swap token1→token0
            } else {
                eyre::bail!(
                    "hop {}: current_token {} not in pool ({},{})",
                    i,
                    current_token,
                    state.token0,
                    state.token1
                );
            };

            current_token = if zfo { state.token1 } else { state.token0 };
            zero_for_one.push(zfo);
            total_fee += state.fee as f64;
        }

        // Validate circular: last output must equal input_token
        if current_token != input_token {
            eyre::bail!(
                "route not circular: starts with {} but ends with {}",
                input_token,
                current_token
            );
        }

        Ok(ResolvedRoute {
            name: cfg.name.clone(),
            pools,
            zero_for_one,
            total_fee_bps: total_fee / 100.0, // convert from UniV3 units to bps
        })
    }

    fn token_label(addr: Address) -> String {
        use crate::liquidator::flash_loan::tokens;

        match addr {
            a if a == tokens::WETH => "WETH".to_string(),
            a if a == tokens::USDC => "USDC".to_string(),
            a if a == tokens::USDC_E => "USDC.e".to_string(),
            a if a == tokens::USDT => "USDT".to_string(),
            a if a == tokens::WBTC => "WBTC".to_string(),
            a if a == tokens::ARB => "ARB".to_string(),
            a if a == tokens::DAI => "DAI".to_string(),
            a if a == tokens::LINK => "LINK".to_string(),
            a if a == tokens::GMX => "GMX".to_string(),
            a if a == tokens::WSTETH => "wstETH".to_string(),
            a if a == tokens::MAGIC => "MAGIC".to_string(),
            a if a == tokens::FRAX => "FRAX".to_string(),
            _ => {
                let s = format!("{addr:#x}");
                format!("{}...{}", &s[..6], &s[s.len() - 4..])
            }
        }
    }

    fn route_path(route: &ResolvedRoute, states: &[self::pool_state::UniV3PoolState]) -> String {
        if states.is_empty() || states.len() != route.zero_for_one.len() {
            return route.name.clone();
        }

        let mut labels = Vec::with_capacity(states.len() + 1);
        let mut current = if route.zero_for_one[0] {
            states[0].token0
        } else {
            states[0].token1
        };
        labels.push(Self::token_label(current));

        for (state, zero_for_one) in states.iter().zip(route.zero_for_one.iter().copied()) {
            current = if zero_for_one {
                state.token1
            } else {
                state.token0
            };
            labels.push(Self::token_label(current));
        }

        labels.join(" -> ")
    }

    /// Push current pool pair and route snapshots to the dashboard.
    fn push_pair_snapshots(&self) {
        let pairs = self.detector.pairs();
        let routes = self.detector.routes();
        let mut snapshots = Vec::with_capacity(pairs.len() + routes.len());
        for pair in pairs {
            let (price_a, price_b, liq_a, liq_b) = if let (Some(a), Some(b)) = (
                self.pool_cache.get(&pair.pool_a),
                self.pool_cache.get(&pair.pool_b),
            ) {
                let pa = Self::display_pool_price(&a.sqrt_price_x96, a.token0, a.token1);
                let pb = Self::display_pool_price(&b.sqrt_price_x96, b.token0, b.token1);
                (pa, pb, a.liquidity, b.liquidity)
            } else {
                continue;
            };

            let spread = if price_a.max(price_b) > 0.0 {
                ((price_a - price_b).abs() / price_a.max(price_b)) * 10_000.0
            } else {
                0.0
            };
            let fee_threshold = pair.total_fee_bps() + 5.0;

            snapshots.push(ArbPairSnapshot {
                kind: "pair".to_string(),
                name: pair.name.clone(),
                hop_count: 2,
                path: format!(
                    "{} -> {}",
                    Self::token_label(pair.token0),
                    Self::token_label(pair.token1)
                ),
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

        for route in routes {
            let states: Vec<_> = route
                .pools
                .iter()
                .filter_map(|pool| self.pool_cache.get(pool))
                .collect();
            if states.len() != route.pools.len() {
                continue;
            }

            let Some(spread) = estimate_route_gross_spread_bps(route, &self.pool_cache) else {
                continue;
            };
            let fee_threshold = route.total_fee_bps + 5.0;
            let first_liquidity = states.first().map(|s| s.liquidity).unwrap_or(0);
            let min_liquidity = states
                .iter()
                .map(|s| s.liquidity)
                .min()
                .unwrap_or(first_liquidity);

            snapshots.push(ArbPairSnapshot {
                kind: "route".to_string(),
                name: route.name.clone(),
                hop_count: route.pools.len(),
                path: Self::route_path(route, &states),
                pool_a: format!("{:#x}", route.pools[0]),
                pool_b: format!("{:#x}", route.pools[route.pools.len() - 1]),
                price_a: 0.0,
                price_b: 0.0,
                spread_bps: spread,
                fee_threshold_bps: fee_threshold,
                liquidity_a: format!("{}", first_liquidity),
                liquidity_b: format!("{}", min_liquidity),
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
    let gas_price_wei = (selected_gas_price_gwei * 1e9).round() as u128;
    let eth_price = crate::protocols::radiant::CACHED_ETH_PRICE_CENTS
        .load(std::sync::atomic::Ordering::Relaxed) as f64
        / 100.0;
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
