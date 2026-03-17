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
            "\u{1F680} *MEV Bot 启动*\n池对: {}\n路由: {}\n模式: 运行中",
            pairs, routes
        ));
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
