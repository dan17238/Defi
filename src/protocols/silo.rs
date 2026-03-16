use alloy::primitives::Address;
use alloy::providers::Provider;
use alloy::sol;
use eyre::Result;
use tracing::{debug, info};

use crate::protocols::{LiquidationOpportunity, Protocol};
use crate::state::position_tracker::PositionTracker;

// --------------------------------------------------------------------------
// Silo ABI definitions (Phase 4 - skeleton)
// --------------------------------------------------------------------------

sol! {
    #[sol(rpc)]
    interface ISiloLens {
        /// Check if a user's position is solvent in a given silo.
        function isSolvent(address silo, address user)
            external
            view
            returns (bool);

        /// Get the user's loan-to-value ratio.
        function getUserLTV(address silo, address user)
            external
            view
            returns (uint256);

        /// Get all markets (silos) from the repository.
        function getRawLiquidity(address silo)
            external
            view
            returns (uint256);
    }

    #[sol(rpc)]
    interface ISiloRepository {
        /// Get all registered silos.
        function getSilos() external view returns (address[] memory);

        /// Get the silo for a given asset.
        function getSilo(address asset) external view returns (address);
    }
}

/// Silo protocol monitor.
///
/// Silo Finance uses isolated lending markets. Each "silo" is an independent
/// lending pool for a specific asset pair. This implementation is a skeleton
/// for Phase 4; the core scanning logic is stubbed out.
pub struct SiloProtocol<P> {
    provider: P,
    lens_address: Address,
    repository_address: Address,
    min_profit_usd: f64,
    position_tracker: PositionTracker,
}

impl<P: Provider + Clone + Send + Sync> SiloProtocol<P> {
    pub fn new(
        provider: P,
        lens_address: Address,
        repository_address: Address,
        min_profit_usd: f64,
    ) -> Self {
        Self {
            provider,
            lens_address,
            repository_address,
            min_profit_usd,
            position_tracker: PositionTracker::new(),
        }
    }

    /// Returns a reference to the internal position tracker.
    pub fn position_tracker(&self) -> &PositionTracker {
        &self.position_tracker
    }

    /// Fetch the list of all registered silos from the repository.
    pub async fn fetch_silos(&self) -> Result<Vec<Address>> {
        let repository = ISiloRepository::new(self.repository_address, &self.provider);
        let silos = repository
            .getSilos()
            .call()
            .await
            .map_err(|e| eyre::eyre!("Failed to fetch Silo repository silos: {e}"))?;
        Ok(silos)
    }

    /// Check solvency for a given user in a given silo.
    pub async fn check_user_solvency(&self, silo: Address, user: Address) -> Result<bool> {
        let lens = ISiloLens::new(self.lens_address, &self.provider);
        let solvent = lens
            .isSolvent(silo, user)
            .call()
            .await
            .map_err(|e| eyre::eyre!("Failed to check solvency: {e}"))?;
        Ok(solvent)
    }
}

impl<P: Provider + Clone + Send + Sync> Protocol for SiloProtocol<P> {
    fn name(&self) -> &str {
        "silo"
    }

    async fn get_liquidatable_positions(
        &self,
        block_number: u64,
    ) -> Result<Vec<LiquidationOpportunity>> {
        info!(
            protocol = self.name(),
            block = block_number,
            "Silo scanning not yet implemented (Phase 4)"
        );

        // Phase 4 TODO:
        // 1. Fetch all silos from the repository
        // 2. For each silo, iterate tracked borrowers
        // 3. Check isSolvent() via multicall batching
        // 4. For insolvent positions, compute debt to cover and expected profit
        // 5. Build LiquidationOpportunity for each

        let borrowers = self.position_tracker.get_all_borrowers();
        debug!(
            protocol = self.name(),
            tracked_borrowers = borrowers.len(),
            "Silo scan skipped - not yet implemented"
        );

        Ok(Vec::new())
    }

    async fn discover_borrowers(&self) -> Result<()> {
        // Phase 4 TODO: scan Silo Borrow events
        debug!(
            protocol = self.name(),
            "Silo borrower discovery not yet implemented"
        );
        Ok(())
    }
}
