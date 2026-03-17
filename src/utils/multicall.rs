use alloy::primitives::{address, Address, Bytes};
use alloy::providers::Provider;
use alloy::sol;
use eyre::{Context, Result};
use tracing::debug;

/// Multicall3 deployed address (same on all chains).
pub const MULTICALL3_ADDRESS: Address = address!("cA11bde05977b3631167028862bE2a173976CA11");

sol! {
    #[sol(rpc)]
    interface IMulticall3 {
        struct Call3 {
            address target;
            bool allowFailure;
            bytes callData;
        }

        struct Result {
            bool success;
            bytes returnData;
        }

        function aggregate3(Call3[] calldata calls)
            external
            payable
            returns (Result[] memory returnData);
    }
}

/// Result from a single call within a multicall batch.
#[derive(Debug, Clone)]
pub struct MulticallResult {
    pub success: bool,
    pub return_data: Bytes,
}

/// Helper for batching RPC calls through the Multicall3 contract.
///
/// This significantly reduces the number of RPC round-trips needed when
/// querying health factors for many borrower positions.
pub struct Multicall<'a, P> {
    provider: &'a P,
}

impl<'a, P: Provider + Send + Sync> Multicall<'a, P> {
    pub fn new(provider: &'a P) -> Self {
        Self { provider }
    }

    /// Execute a batch of calls through Multicall3's aggregate3.
    ///
    /// Each call is a (target_address, encoded_calldata) pair.
    /// All calls are marked with allowFailure=true so one revert does not
    /// fail the entire batch.
    pub async fn aggregate3(&self, calls: Vec<(Address, Vec<u8>)>) -> Result<Vec<MulticallResult>> {
        self.aggregate3_at_block(calls, None).await
    }

    /// Execute a batch of calls through Multicall3 at a specific block.
    pub async fn aggregate3_at_block(
        &self,
        calls: Vec<(Address, Vec<u8>)>,
        block_number: Option<u64>,
    ) -> Result<Vec<MulticallResult>> {
        if calls.is_empty() {
            return Ok(Vec::new());
        }

        debug!(count = calls.len(), "Executing Multicall3.aggregate3");

        let mc_calls: Vec<IMulticall3::Call3> = calls
            .into_iter()
            .map(|(target, call_data)| IMulticall3::Call3 {
                target,
                allowFailure: true,
                callData: call_data.into(),
            })
            .collect();

        let multicall = IMulticall3::new(MULTICALL3_ADDRESS, self.provider);
        let call = multicall.aggregate3(mc_calls);
        let raw_results = match block_number {
            Some(block) => call
                .block(block.into())
                .call()
                .await
                .wrap_err("Multicall3.aggregate3 call failed")?,
            None => call
                .call()
                .await
                .wrap_err("Multicall3.aggregate3 call failed")?,
        };

        let results = raw_results
            .into_iter()
            .map(|r| MulticallResult {
                success: r.success,
                return_data: r.returnData,
            })
            .collect();

        Ok(results)
    }

    /// Execute a batch of static calls (read-only) through Multicall3.
    ///
    /// Convenience wrapper that takes pre-encoded call data for a single
    /// target contract.
    pub async fn aggregate3_single_target(
        &self,
        target: Address,
        encoded_calls: Vec<Vec<u8>>,
    ) -> Result<Vec<MulticallResult>> {
        let calls: Vec<(Address, Vec<u8>)> = encoded_calls
            .into_iter()
            .map(|data| (target, data))
            .collect();
        self.aggregate3(calls).await
    }
}
