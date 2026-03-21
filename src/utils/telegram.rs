use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// Non-blocking Telegram notifier. Messages are queued and sent in a
/// background task so the hot path is never blocked by HTTP.
#[derive(Clone)]
pub struct Telegram {
    tx: mpsc::UnboundedSender<String>,
}

struct TelegramInner {
    client: reqwest::Client,
    url: String,
    chat_id: String,
}

impl Telegram {
    /// Create a new notifier. Spawns a background task that drains the queue.
    /// Returns `None` if token or chat_id is empty (notifications disabled).
    pub fn new(bot_token: &str, chat_id: &str) -> Option<Self> {
        if bot_token.is_empty() || chat_id.is_empty() {
            return None;
        }

        let inner = Arc::new(TelegramInner {
            client: reqwest::Client::new(),
            url: format!("https://api.telegram.org/bot{}/sendMessage", bot_token),
            chat_id: chat_id.to_string(),
        });

        let (tx, mut rx) = mpsc::unbounded_channel::<String>();

        tokio::spawn(async move {
            while let Some(text) = rx.recv().await {
                if let Err(e) = inner.send(&text).await {
                    warn!(error = %e, "Telegram send failed");
                }
            }
        });

        Some(Self { tx })
    }

    /// Queue a message (never blocks).
    pub fn notify(&self, msg: impl Into<String>) {
        let _ = self.tx.send(msg.into());
    }

    /// Convenience: profit notification
    pub fn profit(&self, module: &str, pair: &str, profit_usd: f64, tx_hash: &str) {
        self.notify(format!(
            "\u{2705} *{}* 盈利\n对: `{}`\n利润: *${:.2}*\nTX: [arbiscan](https://arbiscan.io/tx/{})",
            module, pair, profit_usd, tx_hash
        ));
    }

    /// Convenience: revert notification
    pub fn revert(&self, module: &str, pair: &str, tx_hash: &str) {
        self.notify(format!(
            "\u{274C} *{}* 回滚\n对: `{}`\nTX: [arbiscan](https://arbiscan.io/tx/{})",
            module, pair, tx_hash
        ));
    }

    /// Convenience: error notification
    pub fn error(&self, msg: &str) {
        self.notify(format!("\u{26A0}\u{FE0F} *错误*: {}", msg));
    }

    /// Convenience: startup notification
    pub fn startup(&self, pairs: usize, routes: usize) {
        self.notify(format!(
            "\u{1F680} *MEV Bot 启动*\n\
            \n\
            套利: {} 池对 + {} 路由\n\
            链: Arbitrum One\n\
            合约: 已部署\n\
            清算: AAVE V3 + Radiant\n\
            \n\
            扫描开始...",
            pairs, routes
        ));
    }

    /// Convenience: periodic status report
    pub fn status_report(
        &self,
        uptime: &str,
        scans: u64,
        opportunities: u64,
        arb_attempts: u64,
        arb_successes: u64,
        arb_profit_usd: f64,
        liq_profit_usd: f64,
        reverts: u64,
        revert_gas_usd: f64,
        positions: u64,
        errors: u64,
        scan_latency_ms: f64,
        best_pair: &str,
        best_spread: f64,
        best_threshold: f64,
        top_pairs: &[(String, f64, f64)], // [(name, spread, threshold)]
    ) {
        let total_pnl = arb_profit_usd + liq_profit_usd;
        let pnl_icon = if total_pnl > 0.0 { "\u{1F4B0}" } else { "\u{1F4CA}" };
        let pnl_sign = if total_pnl >= 0.0 { "+" } else { "" };

        let mut top_str = String::new();
        for (name, spread, threshold) in top_pairs.iter().take(5) {
            let pct = spread / threshold.max(0.1) * 100.0;
            top_str.push_str(&format!(
                "  `{:.1}/{:.0}` bps ({:.0}%) {}\n",
                spread, threshold, pct, name
            ));
        }
        if top_str.is_empty() {
            top_str = "  无活跃价差\n".to_string();
        }

        self.notify(format!(
            "{} *MEV Bot 状态报告*\n\
            \n\
            *运行* {} | 延迟 {:.1}ms\n\
            \n\
            *PnL: {}${:.2}*\n\
            套利利润: ${:.2}\n\
            清算利润: ${:.2}\n\
            Gas 消耗: -${:.3} ({} 次 revert)\n\
            \n\
            *套利*\n\
            扫描: {} | 机会: {}\n\
            交易: {} 发送 / {} 成功\n\
            \n\
            *Top 5 价差*\n\
            {}\
            *清算*\n\
            监控仓位: {} | 错误: {}",
            pnl_icon,
            uptime, scan_latency_ms,
            pnl_sign, total_pnl,
            arb_profit_usd,
            liq_profit_usd,
            revert_gas_usd, reverts,
            Self::fmt_num(scans), opportunities,
            arb_attempts, arb_successes,
            top_str,
            positions, errors,
        ));
    }

    fn fmt_num(n: u64) -> String {
        if n >= 1_000_000 { format!("{:.1}M", n as f64 / 1_000_000.0) }
        else if n >= 1_000 { format!("{:.1}K", n as f64 / 1_000.0) }
        else { n.to_string() }
    }
}

impl TelegramInner {
    async fn send(&self, text: &str) -> eyre::Result<()> {
        let resp = self
            .client
            .post(&self.url)
            .json(&serde_json::json!({
                "chat_id": self.chat_id,
                "text": text,
                "parse_mode": "Markdown",
                "disable_web_page_preview": true,
            }))
            .send()
            .await?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            debug!(body, "Telegram API error");
        }

        Ok(())
    }
}
