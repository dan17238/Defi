use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// Shared state for the arbitrage dashboard. Thread-safe, lock-free reads
/// for counters, Mutex only for the event log and pair snapshots.
#[derive(Clone)]
pub struct ArbDashboard {
    inner: Arc<Inner>,
}

struct Inner {
    events: Mutex<VecDeque<ArbEvent>>,
    pairs: Mutex<Vec<ArbPairSnapshot>>,
    spread_history: Mutex<Vec<SpreadPoint>>,
    scan_count: AtomicU64,
    opportunity_count: AtomicU64,
    revert_count: AtomicU64,
    revert_gas_cost_usd_micros: AtomicU64,
    /// Last scan latency in microseconds (feed event → scan complete).
    last_scan_latency_us: AtomicU64,
}

const MAX_EVENTS: usize = 200;
const MAX_SPREAD_HISTORY: usize = 500;

#[derive(Debug, Clone, serde::Serialize)]
pub struct ArbPairSnapshot {
    pub kind: String,
    pub name: String,
    pub hop_count: usize,
    pub path: String,
    pub pool_a: String,
    pub pool_b: String,
    pub price_a: f64,
    pub price_b: f64,
    pub spread_bps: f64,
    pub fee_threshold_bps: f64,
    pub liquidity_a: String,
    pub liquidity_b: String,
    pub profitable: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ArbEvent {
    pub ts: u64,
    pub pair: String,
    pub kind: String, // "detected", "simulated", "submitted", "confirmed", "reverted", "skipped"
    pub spread_bps: f64,
    pub profit_usd: f64,
    pub gas_used: u64,
    pub gas_cost_usd: f64,
    pub tx_hash: String,
    pub latency_ms: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SpreadPoint {
    pub ts: u64,
    pub pair: String,
    pub spread_bps: f64,
}

impl ArbDashboard {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                events: Mutex::new(VecDeque::new()),
                pairs: Mutex::new(Vec::new()),
                spread_history: Mutex::new(Vec::new()),
                scan_count: AtomicU64::new(0),
                opportunity_count: AtomicU64::new(0),
                revert_count: AtomicU64::new(0),
                revert_gas_cost_usd_micros: AtomicU64::new(0),
                last_scan_latency_us: AtomicU64::new(0),
            }),
        }
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }

    pub fn push_event(&self, event: ArbEvent) {
        if let Ok(mut q) = self.inner.events.lock() {
            q.push_front(event);
            while q.len() > MAX_EVENTS {
                q.pop_back();
            }
        }
    }

    pub fn record_detected(&self, pair: &str, spread_bps: f64, profit_usd: f64) {
        self.inner.opportunity_count.fetch_add(1, Ordering::Relaxed);
        self.push_event(ArbEvent {
            ts: Self::now_ms(),
            pair: pair.to_string(),
            kind: "detected".to_string(),
            spread_bps,
            profit_usd,
            gas_used: 0,
            gas_cost_usd: 0.0,
            tx_hash: String::new(),
            latency_ms: 0,
        });
    }

    pub fn record_simulated(&self, pair: &str, profit_usd: f64, gas_used: u64, reverted: bool) {
        self.push_event(ArbEvent {
            ts: Self::now_ms(),
            pair: pair.to_string(),
            kind: if reverted {
                "skipped".to_string()
            } else {
                "simulated".to_string()
            },
            spread_bps: 0.0,
            profit_usd,
            gas_used,
            gas_cost_usd: 0.0,
            tx_hash: String::new(),
            latency_ms: 0,
        });
    }

    pub fn record_submitted(&self, pair: &str, profit_usd: f64, tx_hash: &str, latency_ms: u64) {
        self.push_event(ArbEvent {
            ts: Self::now_ms(),
            pair: pair.to_string(),
            kind: "submitted".to_string(),
            spread_bps: 0.0,
            profit_usd,
            gas_used: 0,
            gas_cost_usd: 0.0,
            tx_hash: tx_hash.to_string(),
            latency_ms,
        });
    }

    pub fn record_confirmed(
        &self,
        pair: &str,
        profit_usd: f64,
        tx_hash: &str,
        gas_used: u64,
        gas_cost_usd: f64,
    ) {
        self.push_event(ArbEvent {
            ts: Self::now_ms(),
            pair: pair.to_string(),
            kind: "confirmed".to_string(),
            spread_bps: 0.0,
            profit_usd,
            gas_used,
            gas_cost_usd,
            tx_hash: tx_hash.to_string(),
            latency_ms: 0,
        });
    }

    pub fn record_reverted(&self, pair: &str, tx_hash: &str, gas_used: u64, gas_cost_usd: f64) {
        self.inner.revert_count.fetch_add(1, Ordering::Relaxed);
        let micros = (gas_cost_usd * 1_000_000.0).round().max(0.0) as u64;
        self.inner
            .revert_gas_cost_usd_micros
            .fetch_add(micros, Ordering::Relaxed);
        self.push_event(ArbEvent {
            ts: Self::now_ms(),
            pair: pair.to_string(),
            kind: "reverted".to_string(),
            spread_bps: 0.0,
            profit_usd: 0.0,
            gas_used,
            gas_cost_usd,
            tx_hash: tx_hash.to_string(),
            latency_ms: 0,
        });
    }

    pub fn record_scan(&self) {
        self.inner.scan_count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_scan_latency_us(&self, us: u64) {
        self.inner.last_scan_latency_us.store(us, Ordering::Relaxed);
    }

    pub fn update_pairs(&self, snapshots: Vec<ArbPairSnapshot>) {
        // Also record spread history
        let now = Self::now_ms();
        if let Ok(mut hist) = self.inner.spread_history.lock() {
            for s in &snapshots {
                hist.push(SpreadPoint {
                    ts: now,
                    pair: s.name.clone(),
                    spread_bps: s.spread_bps,
                });
            }
            let excess = hist.len().saturating_sub(MAX_SPREAD_HISTORY);
            if excess > 0 {
                hist.drain(..excess);
            }
        }
        if let Ok(mut p) = self.inner.pairs.lock() {
            *p = snapshots;
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        let events: Vec<ArbEvent> = self
            .inner
            .events
            .lock()
            .map(|q| q.iter().cloned().collect())
            .unwrap_or_default();

        let pairs: Vec<ArbPairSnapshot> = self
            .inner
            .pairs
            .lock()
            .map(|p| p.clone())
            .unwrap_or_default();

        let spread_history: Vec<SpreadPoint> = self
            .inner
            .spread_history
            .lock()
            .map(|h| h.clone())
            .unwrap_or_default();

        serde_json::json!({
            "scans": self.inner.scan_count.load(Ordering::Relaxed),
            "opportunities": self.inner.opportunity_count.load(Ordering::Relaxed),
            "revert_count": self.inner.revert_count.load(Ordering::Relaxed),
            "revert_gas_cost_usd": self.inner.revert_gas_cost_usd_micros.load(Ordering::Relaxed) as f64 / 1_000_000.0,
            "scan_latency_ms": self.inner.last_scan_latency_us.load(Ordering::Relaxed) as f64 / 1000.0,
            "pairs": pairs,
            "events": events,
            "spread_history": spread_history,
        })
    }
}
