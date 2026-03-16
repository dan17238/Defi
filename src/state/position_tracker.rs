use alloy::primitives::{Address, U256};
use dashmap::DashMap;
use std::sync::atomic::{AtomicU64, Ordering};

/// Tracked state for a single borrower position.
#[derive(Debug, Clone)]
pub struct TrackedPosition {
    /// Last known health factor (scaled by 1e18).
    pub health_factor: U256,
    /// Block number at which the health factor was last updated.
    pub last_updated_block: u64,
}

/// Concurrent position tracker backed by DashMap.
///
/// This is shared across protocol monitors and the liquidation executor.
/// It provides lock-free concurrent reads and fine-grained write locking
/// per entry, which is ideal for the high-throughput scanning loop.
pub struct PositionTracker {
    /// Map of borrower address -> tracked position data.
    positions: DashMap<Address, TrackedPosition>,
    /// The last block number at which a full scan was completed.
    last_scanned_block: AtomicU64,
}

impl PositionTracker {
    /// Create a new empty position tracker.
    pub fn new() -> Self {
        Self {
            positions: DashMap::new(),
            last_scanned_block: AtomicU64::new(0),
        }
    }

    /// Register a borrower for tracking. If already tracked, this is a no-op.
    pub fn add_borrower(&self, address: Address) {
        self.positions.entry(address).or_insert(TrackedPosition {
            health_factor: U256::MAX,
            last_updated_block: 0,
        });
    }

    /// Register multiple borrowers at once.
    pub fn add_borrowers(&self, addresses: &[Address]) {
        for addr in addresses {
            self.add_borrower(*addr);
        }
    }

    /// Remove a borrower from tracking (e.g. after they repaid all debt).
    pub fn remove_borrower(&self, address: &Address) {
        self.positions.remove(address);
    }

    /// Get a list of all tracked borrower addresses.
    pub fn get_all_borrowers(&self) -> Vec<Address> {
        self.positions.iter().map(|entry| *entry.key()).collect()
    }

    /// Return the number of tracked borrowers.
    pub fn borrower_count(&self) -> usize {
        self.positions.len()
    }

    /// Update the health factor for a borrower. Creates the entry if it
    /// does not exist.
    pub fn update_health_factor(&self, address: Address, health_factor: U256) {
        self.positions
            .entry(address)
            .and_modify(|pos| {
                pos.health_factor = health_factor;
            })
            .or_insert(TrackedPosition {
                health_factor,
                last_updated_block: 0,
            });
    }

    /// Update the health factor and the block at which it was observed.
    pub fn update_health_factor_at_block(
        &self,
        address: Address,
        health_factor: U256,
        block_number: u64,
    ) {
        self.positions
            .entry(address)
            .and_modify(|pos| {
                pos.health_factor = health_factor;
                pos.last_updated_block = block_number;
            })
            .or_insert(TrackedPosition {
                health_factor,
                last_updated_block: block_number,
            });
    }

    /// Get the tracked position for a specific borrower.
    pub fn get_position(&self, address: &Address) -> Option<TrackedPosition> {
        self.positions.get(address).map(|entry| entry.clone())
    }

    /// Get all positions whose health factor is below the given threshold.
    pub fn get_positions_below_threshold(
        &self,
        threshold: U256,
    ) -> Vec<(Address, TrackedPosition)> {
        self.positions
            .iter()
            .filter(|entry| entry.value().health_factor < threshold)
            .map(|entry| (*entry.key(), entry.value().clone()))
            .collect()
    }

    /// Record the last block number at which a full scan was performed.
    pub fn set_last_scanned_block(&self, block: u64) {
        self.last_scanned_block.store(block, Ordering::Release);
    }

    /// Get the last fully-scanned block number.
    pub fn last_scanned_block(&self) -> u64 {
        self.last_scanned_block.load(Ordering::Acquire)
    }

    /// Prune positions that haven't been updated since `stale_before_block`.
    /// Useful for garbage-collecting positions that are no longer active.
    pub fn prune_stale(&self, stale_before_block: u64) -> usize {
        let stale_keys: Vec<Address> = self
            .positions
            .iter()
            .filter(|entry| entry.value().last_updated_block < stale_before_block)
            .map(|entry| *entry.key())
            .collect();
        let count = stale_keys.len();
        for key in stale_keys {
            self.positions.remove(&key);
        }
        count
    }
}

impl Default for PositionTracker {
    fn default() -> Self {
        Self::new()
    }
}
