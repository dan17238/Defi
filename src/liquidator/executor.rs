use alloy::primitives::{Address, Bytes, FixedBytes};
use alloy::providers::{Provider, RootProvider};
use eyre::{Context, Result};
use tracing::{debug, info};

use crate::utils::metrics::Metrics;

/// Submits liquidation transactions on-chain and waits for confirmation.
///
/// Uses two providers:
/// - `provider`: for gas estimation and reads (via local node or general RPC)
/// - `sequencer_provider`: dedicated persistent connection to the Arbitrum
///   Sequencer for lowest-latency transaction submission
pub struct Executor<P> {
    provider: P,
    sequencer_provider: RootProvider,
    metrics: Metrics,
}

impl<P: Provider + Clone + Send + Sync> Executor<P> {
    pub fn new(provider: P, sequencer_rpc_url: &str, metrics: Metrics) -> Self {
        let sequencer_provider = crate::provider::create_sequencer_provider(sequencer_rpc_url)
            .expect("Failed to create sequencer provider");
        Self {
            provider,
            sequencer_provider,
            metrics,
        }
    }

    /// Build, sign, and send a transaction to the flash liquidator contract.
    ///
    /// Returns the transaction hash on success.
    pub async fn execute(
        &self,
        to: Address,
        calldata: Bytes,
        max_gas_price_gwei: f64,
    ) -> Result<FixedBytes<32>> {
        // Check current gas price and bail if too high
        let gas_price = self
            .provider
            .get_gas_price()
            .await
            .wrap_err("Failed to fetch gas price")?;

        let max_gas_price_wei = (max_gas_price_gwei * 1e9) as u128;
        if gas_price > max_gas_price_wei {
            eyre::bail!(
                "Gas price {} wei exceeds max {} wei ({} gwei)",
                gas_price,
                max_gas_price_wei,
                max_gas_price_gwei
            );
        }

        debug!(
            to = %to,
            gas_price,
            calldata_len = calldata.len(),
            "Sending liquidation transaction"
        );

        // Build the transaction request
        let tx_request = alloy::rpc::types::TransactionRequest::default()
            .to(to)
            .input(alloy::rpc::types::TransactionInput::new(calldata))
            .gas_price(gas_price);

        // Send the transaction via the sequencer provider for lowest latency.
        // The sequencer provider maintains a persistent HTTP connection to
        // arb1-sequencer.arbitrum.io, eliminating TLS handshake overhead.
        let send_start = std::time::Instant::now();

        let pending = self
            .sequencer_provider
            .send_transaction(tx_request)
            .await
            .wrap_err("Failed to send liquidation transaction to sequencer")?;

        let tx_hash = *pending.tx_hash();
        let send_latency = send_start.elapsed();

        info!(
            tx_hash = %tx_hash,
            latency_ms = send_latency.as_millis() as u64,
            "Transaction submitted to Sequencer, waiting for receipt"
        );
        self.metrics.record_latency_us(send_latency.as_micros() as u64);

        // Wait for the transaction to be mined
        let receipt = pending
            .get_receipt()
            .await
            .wrap_err("Failed to get transaction receipt")?;

        if receipt.status() {
            info!(
                tx_hash = %tx_hash,
                block = ?receipt.block_number,
                gas_used = ?receipt.gas_used,
                "Transaction confirmed successfully"
            );
        } else {
            eyre::bail!(
                "Transaction {} reverted on-chain in block {:?}",
                tx_hash,
                receipt.block_number
            );
        }

        Ok(tx_hash)
    }

    /// Estimate gas for a liquidation call without sending it.
    pub async fn estimate_gas(
        &self,
        to: Address,
        calldata: Bytes,
    ) -> Result<u64> {
        let tx_request = alloy::rpc::types::TransactionRequest::default()
            .to(to)
            .input(alloy::rpc::types::TransactionInput::new(calldata));

        let gas = self
            .provider
            .estimate_gas(tx_request)
            .await
            .wrap_err("Gas estimation failed")?;

        Ok(gas)
    }
}
