use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tracing::info;

/// Simple in-memory metrics tracker for the liquidator bot.
///
/// Thread-safe via atomic counters. Designed to be cloned and shared
/// across tasks (wraps an inner Arc).
#[derive(Debug, Clone)]
pub struct Metrics {
    inner: Arc<MetricsInner>,
}

#[derive(Debug)]
struct MetricsInner {
    liquidation_count: AtomicU64,
    successful_count: AtomicU64,
    total_profit_usd_micros: AtomicU64,
    error_count: AtomicU64,
    blocks_processed: AtomicU64,
    positions_scanned: AtomicU64,
    total_latency_us: AtomicU64,
    latency_samples: AtomicU64,
    // Arbitrage metrics
    arb_count: AtomicU64,
    arb_successful_count: AtomicU64,
    arb_profit_usd_micros: AtomicI64,
    started_at: Instant,
}

impl Metrics {
    /// Create a new metrics tracker.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(MetricsInner {
                liquidation_count: AtomicU64::new(0),
                successful_count: AtomicU64::new(0),
                total_profit_usd_micros: AtomicU64::new(0),
                error_count: AtomicU64::new(0),
                blocks_processed: AtomicU64::new(0),
                positions_scanned: AtomicU64::new(0),
                total_latency_us: AtomicU64::new(0),
                latency_samples: AtomicU64::new(0),
                arb_count: AtomicU64::new(0),
                arb_successful_count: AtomicU64::new(0),
                arb_profit_usd_micros: AtomicI64::new(0),
                started_at: Instant::now(),
            }),
        }
    }

    pub fn record_liquidation(&self, profit_usd: f64, success: bool) {
        self.inner.liquidation_count.fetch_add(1, Ordering::Relaxed);
        if success {
            self.inner.successful_count.fetch_add(1, Ordering::Relaxed);
        }
        let micros = (profit_usd * 1_000_000.0) as u64;
        self.inner
            .total_profit_usd_micros
            .fetch_add(micros, Ordering::Relaxed);
    }

    pub fn record_latency_us(&self, us: u64) {
        self.inner.total_latency_us.fetch_add(us, Ordering::Relaxed);
        self.inner.latency_samples.fetch_add(1, Ordering::Relaxed);
    }

    /// Record an error.
    pub fn record_error(&self) {
        self.inner.error_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Record that a block was processed.
    pub fn record_block_processed(&self) {
        self.inner.blocks_processed.fetch_add(1, Ordering::Relaxed);
    }

    /// Record that positions were scanned.
    pub fn record_positions_scanned(&self, count: u64) {
        self.inner
            .positions_scanned
            .fetch_add(count, Ordering::Relaxed);
    }

    /// Record an arbitrage submission attempt.
    pub fn record_arbitrage_attempt(&self) {
        self.inner.arb_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a confirmed successful arbitrage and its realized profit.
    pub fn record_arbitrage_success(&self, profit_usd: f64) {
        self.inner
            .arb_successful_count
            .fetch_add(1, Ordering::Relaxed);
        let micros = (profit_usd * 1_000_000.0).round() as i64;
        self.inner
            .arb_profit_usd_micros
            .fetch_add(micros, Ordering::Relaxed);
    }

    /// Get the total arbitrage attempt count.
    pub fn arb_count(&self) -> u64 {
        self.inner.arb_count.load(Ordering::Relaxed)
    }

    /// Get total arbitrage profit in USD.
    pub fn arb_profit_usd(&self) -> f64 {
        let micros = self.inner.arb_profit_usd_micros.load(Ordering::Relaxed);
        micros as f64 / 1_000_000.0
    }

    /// Get the total confirmed arbitrage success count.
    pub fn arb_successful_count(&self) -> u64 {
        self.inner.arb_successful_count.load(Ordering::Relaxed)
    }

    /// Get the current liquidation count.
    pub fn liquidation_count(&self) -> u64 {
        self.inner.liquidation_count.load(Ordering::Relaxed)
    }

    /// Get total profit in USD.
    pub fn total_profit_usd(&self) -> f64 {
        let micros = self.inner.total_profit_usd_micros.load(Ordering::Relaxed);
        micros as f64 / 1_000_000.0
    }

    /// Get the error count.
    pub fn error_count(&self) -> u64 {
        self.inner.error_count.load(Ordering::Relaxed)
    }

    /// Get the number of blocks processed.
    pub fn blocks_processed(&self) -> u64 {
        self.inner.blocks_processed.load(Ordering::Relaxed)
    }

    /// Get the number of positions scanned.
    pub fn positions_scanned(&self) -> u64 {
        self.inner.positions_scanned.load(Ordering::Relaxed)
    }

    /// Log a summary of all current metrics.
    pub fn log_summary(&self) {
        info!(
            liquidations = self.liquidation_count(),
            total_profit_usd = self.total_profit_usd(),
            arb_attempts = self.arb_count(),
            arb_successes = self.arb_successful_count(),
            arb_profit_usd = self.arb_profit_usd(),
            errors = self.error_count(),
            blocks = self.blocks_processed(),
            positions = self.positions_scanned(),
            "Metrics summary"
        );
    }

    pub fn successful_count(&self) -> u64 {
        self.inner.successful_count.load(Ordering::Relaxed)
    }

    pub fn avg_latency_ms(&self) -> f64 {
        let samples = self.inner.latency_samples.load(Ordering::Relaxed);
        if samples == 0 {
            return 0.0;
        }
        let total = self.inner.total_latency_us.load(Ordering::Relaxed);
        (total as f64 / samples as f64) / 1000.0
    }

    pub fn success_rate(&self) -> f64 {
        let total = self.liquidation_count();
        if total == 0 {
            return 100.0;
        }
        (self.successful_count() as f64 / total as f64) * 100.0
    }

    pub fn uptime_secs(&self) -> u64 {
        self.inner.started_at.elapsed().as_secs()
    }

    pub fn to_json(&self) -> serde_json::Value {
        let uptime = self.uptime_secs();
        let days = uptime / 86400;
        let hours = (uptime % 86400) / 3600;
        let mins = (uptime % 3600) / 60;
        let uptime_str = format!("{}d {}h {}m", days, hours, mins);

        serde_json::json!({
            "status": "running",
            "total_liquidations": self.liquidation_count(),
            "successful_liquidations": self.successful_count(),
            "total_profit_usd": self.total_profit_usd(),
            "success_rate": self.success_rate(),
            "avg_latency_ms": self.avg_latency_ms(),
            "errors": self.error_count(),
            "blocks_processed": self.blocks_processed(),
            "positions_scanned": self.positions_scanned(),
            "arb_attempts": self.arb_count(),
            "arb_successes": self.arb_successful_count(),
            "arb_profit_usd": self.arb_profit_usd(),
            "uptime": uptime_str,
            "uptime_secs": uptime
        })
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}
