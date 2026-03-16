use alloy::primitives::{address, Address, TxKind, U256};
use alloy::providers::Provider;
use eyre::{Context, Result};
use revm::database::{AlloyDB, BlockId, CacheDB, WrapDatabaseAsync};
use revm::context_interface::JournalTr;
use revm::handler::MainnetContext;
use revm::primitives::hardfork::SpecId;
use revm::MainBuilder;
use tracing::{debug, info, warn};

use crate::protocols::LiquidationOpportunity;

use super::flash_loan;

/// Key contract addresses to prewarm in the revm cache.
/// Preloading these avoids RPC round-trips during simulation.
const PREWARM_ADDRESSES: &[Address] = &[
    address!("794a61358D6845594F94dc1DB02A252b5b4814aD"), // AAVE v3 Pool
    address!("F4B1486DD74D07706052A33d31d7c0AAFD0659E1"), // Radiant LendingPool
    address!("E592427A0AEce92De3Edee1F18E0157C05861564"), // Uniswap V3 Router
    address!("c873fEcbd354f5A56E00E710B90EF4201db2448d"), // Camelot Router
    address!("82aF49447D8a07e3bd95BD0d56f35241523fBab1"), // WETH
    address!("af88d065e77c8cC2239327C5EDb3A432268e5831"), // USDC
    address!("FF970A61A04b1cA14834A43f5dE4533eBDDB5CC8"), // USDC.e
    address!("Fd086bC7CD5C481DCC9C85ebE478A1C0b69FCbb9"), // USDT
    address!("2f2a2543B76A4166549F7aaB2e75Bef0aefC5B0f"), // WBTC
];

/// Result of a liquidation simulation via revm.
#[derive(Debug, Clone)]
pub struct SimulationResult {
    /// Whether the simulation indicates the liquidation is profitable.
    pub profitable: bool,
    /// Estimated profit in USD (after gas costs).
    pub profit_usd: f64,
    /// Gas used by the simulated transaction.
    pub gas_used: u64,
    /// Whether the transaction reverted in simulation.
    pub reverted: bool,
}

/// Simulates liquidation transactions using a local revm fork of the chain state.
pub struct Simulator<P> {
    provider: P,
    /// The wallet address (EOA) that owns the flash liquidator contract.
    /// Used as the `caller` in simulation so that `onlyOwner` checks pass.
    wallet_address: Address,
}

impl<P: Provider + Clone + Send + Sync> Simulator<P> {
    pub fn new(provider: P, wallet_address: Address) -> Self {
        Self { provider, wallet_address }
    }

    /// Prewarm the CacheDB by loading bytecode for key contracts.
    /// This avoids blocking RPC fetches during the hot simulation path.
    fn prewarm_cache<DB: revm::database::DatabaseRef>(
        cache_db: &mut CacheDB<DB>,
        flash_liquidator: Address,
        opportunity: &LiquidationOpportunity,
    ) {
        use revm::database::DatabaseRef;

        let mut addresses_to_warm: Vec<Address> = PREWARM_ADDRESSES.to_vec();
        addresses_to_warm.push(flash_liquidator);
        addresses_to_warm.push(opportunity.user);
        addresses_to_warm.push(opportunity.collateral_asset);
        addresses_to_warm.push(opportunity.debt_asset);

        for addr in &addresses_to_warm {
            // Touch each address — CacheDB fetches from underlying DB on miss,
            // then serves from memory during simulation.
            let _ = cache_db.basic_ref(*addr);
        }
    }

    /// Simulate a flash-loan-based liquidation against the current on-chain state.
    ///
    /// Forks the latest block state into a CacheDB backed by AlloyDB (via
    /// WrapDatabaseAsync), then executes the flash liquidation calldata in revm.
    pub async fn simulate_liquidation(
        &self,
        opportunity: &LiquidationOpportunity,
        flash_liquidator: Address,
    ) -> Result<SimulationResult> {
        debug!(
            protocol = %opportunity.protocol,
            user = %opportunity.user,
            "Starting revm simulation"
        );

        // Encode the flash liquidation call
        let calldata = flash_loan::encode_flash_liquidation(
            opportunity,
            flash_liquidator,
            alloy::primitives::U256::ZERO,
        );

        // Build the revm database:
        // AlloyDB (async, fetches state from RPC on cache miss)
        //   -> WrapDatabaseAsync (bridges async to sync DatabaseRef)
        //     -> CacheDB (in-memory cache layer, implements Database)
        let alloy_db = AlloyDB::new(self.provider.clone(), BlockId::latest());
        let wrapped_db = WrapDatabaseAsync::new(alloy_db)
            .ok_or_else(|| eyre::eyre!("Failed to create WrapDatabaseAsync - no tokio runtime available"))?;
        let mut cache_db = CacheDB::new(wrapped_db);

        // Prewarm cache: preload key contract bytecode to avoid RPC calls during simulation.
        let prewarm_start = std::time::Instant::now();
        Self::prewarm_cache(&mut cache_db, flash_liquidator, opportunity);
        debug!(prewarm_ms = prewarm_start.elapsed().as_millis(), "Cache prewarmed");

        // Build the EVM context with Arbitrum chain id (42161).
        type SimDB<P> = CacheDB<WrapDatabaseAsync<AlloyDB<alloy::network::Ethereum, P>>>;

        let tx = revm::context::TxEnv::builder()
            .caller(self.wallet_address)
            .kind(TxKind::Call(flash_liquidator))
            .data(calldata.clone())
            .gas_limit(6_000_000)
            .gas_price(100_000_000) // 0.1 gwei
            .value(U256::ZERO)
            .nonce(0)
            .build_fill();

        // Use MainnetContext to fully specify the type parameters.
        let ctx: MainnetContext<SimDB<P>> = {
            let mut c: MainnetContext<SimDB<P>> = revm::context::Context {
                tx: Default::default(),
                block: Default::default(),
                cfg: revm::context::CfgEnv::new_with_spec(SpecId::CANCUN)
                    .with_chain_id(42161),
                journaled_state: revm::Journal::new(cache_db),
                chain: (),
                local: Default::default(),
                error: Ok(()),
            };
            c.tx = tx;
            c.block = revm::context::BlockEnv {
                number: U256::ZERO,
                ..Default::default()
            };
            c
        };

        let mut evm = ctx.build_mainnet();

        // Execute the transaction via transact().
        // transact() takes the Tx (TxEnv) and returns ExecResultAndState.
        let tx_for_exec = revm::context::TxEnv::builder()
            .caller(self.wallet_address)
            .kind(TxKind::Call(flash_liquidator))
            .data(calldata)
            .gas_limit(6_000_000)
            .gas_price(100_000_000)
            .value(U256::ZERO)
            .nonce(0)
            .build_fill();

        let result = revm::ExecuteEvm::transact(&mut evm, tx_for_exec)
            .wrap_err("revm transact failed")?;

        let exec_result = result.result;

        match exec_result {
            revm::context_interface::result::ExecutionResult::Success {
                gas, ..
            } => {
                let gas_used = gas.used();
                debug!(gas_used, "Simulation succeeded");

                // Estimate gas cost in USD.
                // Use cached ETH price from Chainlink (updated by Radiant protocol monitor).
                // Arbitrum L2 gas ~0.1 gwei + L1 data posting ~$0.03 per tx.
                let gas_price_gwei = 0.1_f64;
                let gas_cost_eth = gas_used as f64 * gas_price_gwei / 1e9;
                let eth_price = crate::protocols::radiant::CACHED_ETH_PRICE_CENTS
                    .load(std::sync::atomic::Ordering::Relaxed) as f64 / 100.0;
                let eth_price = if eth_price > 100.0 { eth_price } else { 3500.0 };
                let l1_data_cost_usd = 0.03;
                let gas_cost_usd = gas_cost_eth * eth_price + l1_data_cost_usd;
                let profit_usd = opportunity.expected_profit_usd - gas_cost_usd;

                Ok(SimulationResult {
                    profitable: profit_usd > 0.0,
                    profit_usd,
                    gas_used,
                    reverted: false,
                })
            }
            revm::context_interface::result::ExecutionResult::Revert { gas, output, .. } => {
                let gas_used = gas.used();
                debug!(
                    gas_used,
                    output = %output,
                    "Simulation reverted"
                );
                Ok(SimulationResult {
                    profitable: false,
                    profit_usd: 0.0,
                    gas_used,
                    reverted: true,
                })
            }
            revm::context_interface::result::ExecutionResult::Halt { reason, gas, .. } => {
                let gas_used = gas.used();
                warn!(?reason, gas_used, "Simulation halted");
                Ok(SimulationResult {
                    profitable: false,
                    profit_usd: 0.0,
                    gas_used,
                    reverted: true,
                })
            }
        }
    }
}
