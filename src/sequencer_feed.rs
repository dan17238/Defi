use alloy::primitives::Address;
use futures::StreamExt;
use serde::Deserialize;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio_tungstenite::connect_async;
use tracing::{error, info, trace, warn};

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

/// Event sent from the feed to subscribers (main loop + arb monitor).
#[derive(Debug, Clone)]
pub struct SequencerEvent {
    pub sequence_number: u64,
    pub received_at: std::time::Instant,
    /// Transaction target address parsed from the feed message (best-effort).
    /// Used by the arb monitor to quickly filter non-pool transactions.
    pub tx_to: Option<Address>,
}

/// Try to extract the `to` address from a feed message.
///
/// Arbitrum feed message structure:
///   message.message.header.kind = 3 (L2 user tx)
///   message.message.l2Msg = base64-encoded bytes
///
/// The l2Msg is: [tx_type_byte][rlp-encoded-tx]
/// For EIP-1559 (type 2): RLP([chainId, nonce, maxPriorityFee, maxFee, gasLimit, to, value, data, ...])
/// For Legacy (type 0): RLP([nonce, gasPrice, gasLimit, to, value, data, ...])
///
/// We only need `to` (20 bytes address), so we do minimal RLP parsing.
fn parse_tx_to(msg: &serde_json::Value) -> Option<Address> {
    // Navigate: message.message.l2Msg
    let l2msg_b64 = msg.get("message")?.get("l2Msg")?.as_str()?;

    // Check header kind = 3 (L2MessageKind_SignedTx)
    let kind = msg.get("message")?.get("header")?.get("kind")?.as_u64()?;
    if kind != 3 {
        return None;
    }

    // Decode base64
    use base64::Engine;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(l2msg_b64)
        .ok()?;
    if raw.is_empty() {
        return None;
    }

    // First byte is the tx type (or start of legacy RLP)
    let (tx_type, rlp_data) = if raw[0] >= 0x80 {
        // Legacy tx: no type prefix, entire thing is RLP
        (0u8, &raw[..])
    } else {
        // Typed tx: first byte is type (1=AccessList, 2=EIP1559, 3=EIP4844)
        (raw[0], &raw[1..])
    };

    // Parse RLP list to find the `to` field
    // Legacy: [nonce, gasPrice, gasLimit, TO, value, data, v, r, s]  → index 3
    // Type 1: [chainId, nonce, gasPrice, gasLimit, TO, value, data, accessList, v, r, s] → index 4
    // Type 2: [chainId, nonce, maxPriorityFee, maxFee, gasLimit, TO, value, data, accessList, v, r, s] → index 5
    let to_index = match tx_type {
        0 => 3,
        1 => 4,
        2 => 5,
        _ => return None,
    };

    // Minimal RLP list parsing: skip the list header, then skip `to_index` items
    let data = skip_rlp_list_header(rlp_data)?;
    let mut pos = 0;
    for _ in 0..to_index {
        pos += rlp_item_len(data, pos)?;
    }

    // Read the `to` field (should be 20 bytes for a contract call, empty for create)
    let (to_bytes, _) = read_rlp_item(data, pos)?;
    if to_bytes.len() == 20 {
        Some(Address::from_slice(to_bytes))
    } else {
        None // Contract creation or empty
    }
}

/// Skip the RLP list header, return the inner data slice.
fn skip_rlp_list_header(data: &[u8]) -> Option<&[u8]> {
    if data.is_empty() {
        return None;
    }
    let first = data[0];
    if first >= 0xc0 && first <= 0xf7 {
        // Short list: length = first - 0xc0
        Some(&data[1..])
    } else if first > 0xf7 {
        // Long list: next (first - 0xf7) bytes encode the length
        let len_bytes = (first - 0xf7) as usize;
        if data.len() < 1 + len_bytes {
            return None;
        }
        Some(&data[1 + len_bytes..])
    } else {
        // Not a list (single byte or string) — malformed tx
        None
    }
}

/// Get the total encoded length of an RLP item starting at `offset`.
fn rlp_item_len(data: &[u8], offset: usize) -> Option<usize> {
    if offset >= data.len() {
        return None;
    }
    let first = data[offset];
    if first < 0x80 {
        // Single byte
        Some(1)
    } else if first <= 0xb7 {
        // Short string: length = first - 0x80
        let len = (first - 0x80) as usize;
        Some(1 + len)
    } else if first <= 0xbf {
        // Long string
        let len_bytes = (first - 0xb7) as usize;
        if offset + 1 + len_bytes > data.len() {
            return None;
        }
        let mut len = 0usize;
        for i in 0..len_bytes {
            len = (len << 8) | data[offset + 1 + i] as usize;
        }
        Some(1 + len_bytes + len)
    } else if first <= 0xf7 {
        // Short list
        let len = (first - 0xc0) as usize;
        Some(1 + len)
    } else {
        // Long list
        let len_bytes = (first - 0xf7) as usize;
        if offset + 1 + len_bytes > data.len() {
            return None;
        }
        let mut len = 0usize;
        for i in 0..len_bytes {
            len = (len << 8) | data[offset + 1 + i] as usize;
        }
        Some(1 + len_bytes + len)
    }
}

/// Read the raw bytes of an RLP item at `offset`. Returns (bytes, next_offset).
fn read_rlp_item(data: &[u8], offset: usize) -> Option<(&[u8], usize)> {
    if offset >= data.len() {
        return None;
    }
    let first = data[offset];
    if first < 0x80 {
        Some((&data[offset..offset + 1], offset + 1))
    } else if first <= 0xb7 {
        let len = (first - 0x80) as usize;
        let start = offset + 1;
        let end = start + len;
        if end > data.len() {
            return None;
        }
        Some((&data[start..end], end))
    } else if first <= 0xbf {
        let len_bytes = (first - 0xb7) as usize;
        if offset + 1 + len_bytes > data.len() {
            return None;
        }
        let mut len = 0usize;
        for i in 0..len_bytes {
            len = (len << 8) | data[offset + 1 + i] as usize;
        }
        let start = offset + 1 + len_bytes;
        let end = start + len;
        if end > data.len() {
            return None;
        }
        Some((&data[start..end], end))
    } else {
        // Lists: return the whole thing
        let total = rlp_item_len(data, offset)?;
        let end = offset + total;
        if end > data.len() {
            return None;
        }
        Some((&data[offset..end], end))
    }
}

/// Start the sequencer feed listener. Sends events through the broadcast channel.
/// Reconnects automatically on failure.
pub async fn run_sequencer_feed(feed_url: String, tx: broadcast::Sender<SequencerEvent>) {
    let mut backoff = Duration::from_millis(100);
    let max_backoff = Duration::from_secs(30);

    loop {
        info!(url = %feed_url, "Connecting to Sequencer Feed");

        match connect_async(&feed_url).await {
            Ok((ws_stream, _)) => {
                info!("Sequencer Feed connected");
                backoff = Duration::from_millis(100);

                let (_write, mut read) = ws_stream.split();
                let mut msg_count: u64 = 0;
                let mut decoded_count: u64 = 0;
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

                                            // Best-effort decode tx target address
                                            let tx_to = parse_tx_to(&feed_msg.message);
                                            if tx_to.is_some() {
                                                decoded_count += 1;
                                            }

                                            let event = SequencerEvent {
                                                sequence_number: feed_msg.sequence_number,
                                                received_at: std::time::Instant::now(),
                                                tx_to,
                                            };
                                            if tx.send(event).is_err() {
                                                info!("Feed channel closed, stopping");
                                                return;
                                            }
                                        }
                                        if msg_count % 100 == 0 {
                                            let elapsed = start.elapsed().as_secs_f64();
                                            let decode_rate = if msg_count > 0 {
                                                decoded_count as f64 / msg_count as f64 * 100.0
                                            } else {
                                                0.0
                                            };
                                            trace!(
                                                messages = msg_count,
                                                rate =
                                                    format!("{:.1}/s", msg_count as f64 / elapsed),
                                                decode_pct = format!("{:.0}%", decode_rate),
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

        warn!(
            backoff_ms = backoff.as_millis() as u64,
            "Reconnecting to Sequencer Feed"
        );
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(max_backoff);
    }
}
