use std::sync::Arc;
use std::time::Duration;

use alloy::network::ReceiptResponse;
use alloy::primitives::{Address, Bytes, FixedBytes, U256};
use alloy::providers::Provider;
use alloy::sol;
use alloy::sol_types::SolEvent;
use dashmap::DashSet;
use eyre::{Context, Result};
use tracing::{debug, info, warn};

use crate::utils::metrics::Metrics;

sol! {
    event LiquidationExecuted(
        uint8 indexed protocol,
        address indexed user,
        address collateralAsset,
        address debtAsset,
        uint256 debtRepaid,
        uint256 collateralReceived,
        uint256 profit
    );
}

const LIQUIDATION_EXECUTED_TOPIC: FixedBytes<32> = LiquidationExecuted::SIGNATURE_HASH;
const RECEIPT_POLL_INTERVAL: Duration = Duration::from_secs(5);
const RECEIPT_MAX_POLLS: usize = 24;
const RECEIPT_RECHECK_INTERVAL: Duration = Duration::from_secs(10);
const RECEIPT_MAX_RECHECKS: usize = 100;
const RECEIPT_MISSING_RELEASES: usize = 3;

/// Submits liquidation transactions on-chain without blocking for confirmation.
///
/// The provider must have a wallet signer attached so that transactions are
/// signed before submission. It should point to the sequencer RPC for
/// lowest-latency transaction submission.
///
/// After submission, the tx hash is returned immediately. The flash loan is
/// atomic, so if it reverts on-chain we only lose gas (~$0.05). This lets
/// the bot move to the next opportunity without waiting for a receipt.
pub struct Executor<P> {
    provider: P,
    metrics: Metrics,
    telegram: Option<crate::utils::telegram::Telegram>,
}

impl<P: Provider + Clone + Send + Sync + 'static> Executor<P> {
    pub fn new(
        provider: P,
        metrics: Metrics,
        telegram: Option<crate::utils::telegram::Telegram>,
    ) -> Self {
        Self {
            provider,
            metrics,
            telegram,
        }
    }

    pub async fn current_gas_price(&self, max_gas_price_gwei: f64) -> Result<u128> {
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

        Ok(gas_price)
    }

    /// Build, sign, and send a transaction to the flash liquidator contract.
    ///
    /// Returns the transaction hash immediately after submission without
    /// waiting for the receipt. The simulation already verified the
    /// transaction will succeed; if it reverts on-chain the flash loan is
    /// atomic so we only lose gas.
    pub async fn execute(
        &self,
        to: Address,
        calldata: Bytes,
        gas_price: u128,
        inflight: Arc<DashSet<String>>,
        inflight_key: String,
    ) -> Result<FixedBytes<32>> {
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
            .gas_limit(6_000_000)
            .gas_price(gas_price);

        // Send the signed transaction via the provider (which has a wallet attached
        // and points to the sequencer RPC for lowest latency).
        let send_start = std::time::Instant::now();

        let pending = self
            .provider
            .send_transaction(tx_request)
            .await
            .wrap_err("Failed to send liquidation transaction to sequencer")?;

        let tx_hash = *pending.tx_hash();
        let send_latency = send_start.elapsed();

        info!(
            tx_hash = %tx_hash,
            latency_ms = send_latency.as_millis() as u64,
            "Transaction submitted to Sequencer (not waiting for receipt)"
        );
        self.metrics
            .record_latency_us(send_latency.as_micros() as u64);

        let metrics = self.metrics.clone();
        let receipt_provider = self.provider.clone();
        let tg = self.telegram.clone();
        let tx_hash_for_task = tx_hash;
        tokio::spawn(async move {
            let mut receipt = None;
            let mut last_error = None;

            for attempt in 1..=RECEIPT_MAX_POLLS {
                match receipt_provider
                    .get_transaction_receipt(tx_hash_for_task)
                    .await
                {
                    Ok(Some(found_receipt)) => {
                        receipt = Some(found_receipt);
                        break;
                    }
                    Ok(None) => {
                        if attempt == 1 {
                            debug!(
                                tx_hash = %tx_hash_for_task,
                                "Waiting for liquidation receipt confirmation"
                            );
                        }
                    }
                    Err(e) => {
                        last_error = Some(e.to_string());
                        warn!(
                            tx_hash = %tx_hash_for_task,
                            attempt,
                            error = %e,
                            "Receipt polling failed for liquidation tx"
                        );
                    }
                }

                if attempt < RECEIPT_MAX_POLLS {
                    tokio::time::sleep(RECEIPT_POLL_INTERVAL).await;
                }
            }

            if receipt.is_none() {
                let mut missing_tx_count = 0usize;
                let mut recheck_attempt = 0usize;
                loop {
                    recheck_attempt += 1;
                    match receipt_provider
                        .get_transaction_receipt(tx_hash_for_task)
                        .await
                    {
                        Ok(Some(found_receipt)) => {
                            receipt = Some(found_receipt);
                            break;
                        }
                        Ok(None) => match receipt_provider
                            .get_transaction_by_hash(tx_hash_for_task)
                            .await
                        {
                            Ok(Some(_)) => {
                                missing_tx_count = 0;
                                debug!(
                                    tx_hash = %tx_hash_for_task,
                                    attempt = recheck_attempt,
                                    "Liquidation tx still pending, keeping inflight lock"
                                );
                            }
                            Ok(None) => {
                                missing_tx_count += 1;
                                warn!(
                                    tx_hash = %tx_hash_for_task,
                                    attempt = recheck_attempt,
                                    missing = missing_tx_count,
                                    "Liquidation tx missing from tx lookup and has no receipt"
                                );
                                if missing_tx_count >= RECEIPT_MISSING_RELEASES {
                                    break;
                                }
                            }
                            Err(e) => {
                                last_error = Some(e.to_string());
                                missing_tx_count = 0;
                                warn!(
                                    tx_hash = %tx_hash_for_task,
                                    attempt = recheck_attempt,
                                    error = %e,
                                    "Failed to query liquidation tx after receipt timeout"
                                );
                            }
                        },
                        Err(e) => {
                            last_error = Some(e.to_string());
                            missing_tx_count = 0;
                            warn!(
                                tx_hash = %tx_hash_for_task,
                                attempt = recheck_attempt,
                                error = %e,
                                "Receipt recheck failed for liquidation tx"
                            );
                        }
                    }

                    if receipt.is_some() {
                        break;
                    }
                    if recheck_attempt >= RECEIPT_MAX_RECHECKS {
                        warn!(
                            tx_hash = %tx_hash_for_task,
                            attempts = recheck_attempt,
                            "Releasing liquidation inflight lock after extended pending receipt timeout"
                        );
                        break;
                    }
                    tokio::time::sleep(RECEIPT_RECHECK_INTERVAL).await;
                }
            }

            match receipt {
                Some(receipt) if receipt.status() => {
                    let gas_cost_usd = crate::utils::gas::arbitrum_gas_cost_usd(
                        receipt.gas_used(),
                        receipt.effective_gas_price(),
                    );
                    let gross_profit_usd = Self::extract_realized_profit(&receipt);
                    if gross_profit_usd.is_none() {
                        warn!(
                            tx_hash = %tx_hash_for_task,
                            "Confirmed liquidation tx missing LiquidationExecuted profit data; recording gas-only net profit"
                        );
                    }
                    let net_profit_usd = gross_profit_usd.unwrap_or(0.0) - gas_cost_usd;
                    metrics.record_liquidation_success(net_profit_usd);
                    info!(
                        tx_hash = %tx_hash_for_task,
                        gas_used = receipt.gas_used(),
                        net_profit_usd,
                        "Liquidation tx confirmed"
                    );
                    if let Some(ref tg) = tg {
                        tg.profit(
                            "清算",
                            "liquidation",
                            net_profit_usd,
                            &format!("{tx_hash_for_task:#x}"),
                        );
                    }
                }
                Some(receipt) => {
                    metrics.record_error();
                    warn!(
                        tx_hash = %tx_hash_for_task,
                        gas_used = receipt.gas_used(),
                        "Liquidation tx reverted on-chain"
                    );
                    if let Some(ref tg) = tg {
                        tg.revert("清算", "liquidation", &format!("{tx_hash_for_task:#x}"));
                    }
                }
                None => {
                    metrics.record_error();
                    match last_error {
                        Some(error) => warn!(
                            tx_hash = %tx_hash_for_task,
                            error = %error,
                            attempts = RECEIPT_MAX_POLLS,
                            "Giving up on liquidation receipt after repeated polling errors/timeouts"
                        ),
                        None => warn!(
                            tx_hash = %tx_hash_for_task,
                            attempts = RECEIPT_MAX_POLLS,
                            "Giving up on liquidation receipt after repeated polling with no confirmation"
                        ),
                    }
                }
            }

            inflight.remove(&inflight_key);
        });

        Ok(tx_hash)
    }

    fn extract_realized_profit(receipt: &alloy::rpc::types::TransactionReceipt) -> Option<f64> {
        for log in receipt.inner.logs() {
            if log.topics().first() == Some(&LIQUIDATION_EXECUTED_TOPIC) {
                let data = log.data().data.as_ref();
                if let Some((debt_asset, profit_tokens)) = Self::decode_profit_event_data(data) {
                    return crate::liquidator::flash_loan::tokens::token_value_usd(
                        profit_tokens,
                        debt_asset,
                    );
                }
            }
        }
        None
    }

    fn decode_profit_event_data(data: &[u8]) -> Option<(Address, U256)> {
        if data.len() < 160 {
            return None;
        }
        let debt_asset = Address::from_slice(&data[44..64]);
        let profit_tokens = U256::from_be_slice(&data[128..160]);
        Some((debt_asset, profit_tokens))
    }

    /// Estimate gas for a liquidation call without sending it.
    pub async fn estimate_gas(&self, to: Address, calldata: Bytes) -> Result<u64> {
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
