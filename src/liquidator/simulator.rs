use alloy::primitives::{Address, TxKind, U256};
use alloy::providers::Provider;
use eyre::{Context, Result};
use revm::database::{AlloyDB, BlockId, CacheDB, WrapDatabaseAsync};
use revm::context_interface::JournalTr;
use revm::handler::MainnetContext;
use revm::primitives::hardfork::SpecId;
use revm::MainBuilder;
use tracing::{debug, warn};

use crate::protocols::LiquidationOpportunity;

use super::flash_loan;

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
}

impl<P: Provider + Clone + Send + Sync> Simulator<P> {
    pub fn new(provider: P) -> Self {
        Self { provider }
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
        let calldata = flash_loan::encode_flash_liquidation(opportunity, flash_liquidator);

        // Build the revm database:
        // AlloyDB (async, fetches state from RPC on cache miss)
        //   -> WrapDatabaseAsync (bridges async to sync DatabaseRef)
        //     -> CacheDB (in-memory cache layer, implements Database)
        let alloy_db = AlloyDB::new(self.provider.clone(), BlockId::latest());
        let wrapped_db = WrapDatabaseAsync::new(alloy_db)
            .ok_or_else(|| eyre::eyre!("Failed to create WrapDatabaseAsync - no tokio runtime available"))?;
        let cache_db = CacheDB::new(wrapped_db);

        // Build the EVM context with Arbitrum chain id (42161).
        type SimDB<P> = CacheDB<WrapDatabaseAsync<AlloyDB<alloy::network::Ethereum, P>>>;

        let tx = revm::context::TxEnv::builder()
            .caller(flash_liquidator)
            .kind(TxKind::Call(flash_liquidator))
            .data(calldata.clone())
            .gas_limit(3_000_000)
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
            .caller(flash_liquidator)
            .kind(TxKind::Call(flash_liquidator))
            .data(calldata)
            .gas_limit(3_000_000)
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
                // Arbitrum gas is cheap (~0.1 gwei). For a rough estimate:
                // gas_cost_eth = gas_used * 0.1 gwei = gas_used * 1e-10 ETH
                // At ~$3000/ETH, gas_cost_usd = gas_used * 3e-7
                let gas_cost_usd = gas_used as f64 * 3e-7;
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
