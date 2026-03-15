use std::time::Duration;
use futures::StreamExt;
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tracing::{error, info, warn};

#[derive(Debug, Clone, Deserialize)]
pub struct BroadcastMessage {
    #[serde(default)]
    pub messages: Vec<FeedMessage>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedMessage {
    pub sequence_number: u64,
    #[serde(default)]
    pub message: serde_json::Value,
}

/// Event sent from the feed to the main loop
#[derive(Debug, Clone)]
pub struct SequencerEvent {
    pub sequence_number: u64,
    pub received_at: std::time::Instant,
}

/// Start the sequencer feed listener. Sends events through the channel.
/// Reconnects automatically on failure.
pub async fn run_sequencer_feed(
    feed_url: String,
    tx: mpsc::UnboundedSender<SequencerEvent>,
) {
    let mut backoff = Duration::from_millis(100);
    let max_backoff = Duration::from_secs(30);

    loop {
        info!(url = %feed_url, "Connecting to Sequencer Feed");

        match connect_async(&feed_url).await {
            Ok((ws_stream, _)) => {
                info!("Sequencer Feed connected");
                backoff = Duration::from_millis(100); // reset backoff

                let (_write, mut read) = ws_stream.split();
                let mut msg_count: u64 = 0;
                let start = std::time::Instant::now();

                loop {
                    match read.next().await {
                        Some(Ok(msg)) => {
                            if msg.is_text() || msg.is_binary() {
                                let data = msg.into_data();
                                match serde_json::from_slice::<BroadcastMessage>(&data) {
                                    Ok(broadcast) => {
                                        for feed_msg in broadcast.messages {
                                            msg_count += 1;
                                            let event = SequencerEvent {
                                                sequence_number: feed_msg.sequence_number,
                                                received_at: std::time::Instant::now(),
                                            };
                                            if tx.send(event).is_err() {
                                                info!("Feed channel closed, stopping");
                                                return;
                                            }
                                        }
                                        // Log throughput every 100 messages
                                        if msg_count % 100 == 0 {
                                            let elapsed = start.elapsed().as_secs_f64();
                                            info!(
                                                messages = msg_count,
                                                rate = format!("{:.1}/s", msg_count as f64 / elapsed),
                                                "Sequencer Feed throughput"
                                            );
                                        }
                                    }
                                    Err(e) => {
                                        warn!(error = %e, "Failed to parse feed message");
                                    }
                                }
                            }
                        }
                        Some(Err(e)) => {
                            warn!(error = %e, "Sequencer Feed error");
                            break;
                        }
                        None => {
                            warn!("Sequencer Feed stream ended");
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                error!(error = %e, "Failed to connect to Sequencer Feed");
            }
        }

        // Reconnect with backoff
        warn!(backoff_ms = backoff.as_millis() as u64, "Reconnecting to Sequencer Feed");
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(max_backoff);
    }
}
