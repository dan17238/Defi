pub mod aave_v3;
pub mod radiant;
pub mod silo;

use alloy::primitives::Address;
use eyre::Result;

/// Represents a single liquidation opportunity discovered by a protocol monitor.
#[derive(Debug, Clone)]
pub struct LiquidationOpportunity {
    /// Name of the protocol (e.g. "aave_v3", "radiant").
    pub protocol: String,
    /// Address of the borrower whose position is underwater.
    pub user: Address,
    /// Collateral token to seize.
    pub collateral_asset: Address,
    /// Debt token to repay.
    pub debt_asset: Address,
    /// Amount of debt to cover in the liquidation (in debt token decimals).
    pub debt_to_cover: alloy::primitives::U256,
    /// Estimated profit in USD (before gas costs).
    pub expected_profit_usd: f64,
    /// Health factor of the position (scaled by 1e18; < 1e18 means liquidatable).
    pub health_factor: alloy::primitives::U256,
}

/// Trait that all protocol monitors must implement.
///
/// Each protocol (AAVE v3, Radiant, Silo, etc.) implements this trait to
/// provide a unified interface for discovering liquidation opportunities.
pub trait Protocol: Send + Sync {
    /// Human-readable name of the protocol.
    fn name(&self) -> &str;

    /// Scan for liquidatable positions at the given block number.
    ///
    /// Returns a list of all positions whose health factor is below the
    /// liquidation threshold.
    fn get_liquidatable_positions(
        &self,
        block_number: u64,
    ) -> impl std::future::Future<Output = Result<Vec<LiquidationOpportunity>>> + Send;

    /// Discover borrowers by scanning recent on-chain events (e.g. Borrow events).
    ///
    /// Called once at startup and can be called periodically to refresh the
    /// borrower list. Implementations should add discovered addresses to their
    /// internal position tracker.
    fn discover_borrowers(&self) -> impl std::future::Future<Output = Result<()>> + Send;
}
