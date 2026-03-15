mod config;
mod liquidator;
mod protocols;
mod provider;
mod sequencer_feed;
mod state;
mod utils;
mod web;

use std::path::PathBuf;
use std::sync::Arc;

use alloy::primitives::Address;
use alloy::providers::Provider;
use eyre::{Context, Result};
use futures::StreamExt;
use tokio::signal;
use tokio::sync::broadcast;
use tracing::{error, info, warn};

use crate::config::AppConfig;
use crate::liquidator::Liquidator;
use crate::protocols::aave_v3::AaveV3Protocol;
use crate::protocols::radiant::RadiantProtocol;
use crate::protocols::silo::SiloProtocol;
use crate::protocols::Protocol;
use crate::utils::metrics::Metrics;

/// Command-line arguments (parsed manually to avoid extra dependencies).
struct CliArgs {
    config_path: PathBuf,
    dry_run: Option<bool>,
}

fn parse_args() -> CliArgs {
    let mut config_path = PathBuf::from("config/default.toml");
    let mut dry_run = None;

    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--config" => {
                i += 1;
                if i < args.len() {
                    config_path = PathBuf::from(&args[i]);
                }
            }
            "--dry-run" => {
                dry_run = Some(true);
            }
            "--no-dry-run" => {
                dry_run = Some(false);
            }
            other => {
                eprintln!("Unknown argument: {other}");
                eprintln!("Usage: arbitrum-liquidator [--config <path>] [--dry-run]");
                std::process::exit(1);
            }
        }
        i += 1;
    }

    CliArgs {
        config_path,
        dry_run,
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;

    let args = parse_args();

    // Load configuration
    let mut config = AppConfig::load(&args.config_path)?;

    // CLI --dry-run overrides config
    if let Some(dry_run) = args.dry_run {
        config.execution.dry_run = dry_run;
    }

    // Initialize tracing subscriber with the configured log level
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| {
            tracing_subscriber::EnvFilter::new(&config.monitoring.log_level)
        });

    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(true)
        .with_thread_ids(true)
        .with_file(true)
        .with_line_number(true)
        .init();

    info!("Arbitrum Liquidator starting");
    info!(
        dry_run = config.execution.dry_run,
        config_path = %args.config_path.display(),
        "Configuration loaded"
    );

    // Connect to the WebSocket RPC endpoint
    let ws_provider = provider::create_ws_provider(&config.rpc.ws_url).await?;
    let http_provider = provider::create_http_provider(&config.rpc.http_url)?;

    let block_number = provider::get_latest_block_number(&http_provider).await?;
    info!(block_number, "Connected to Arbitrum");

    // Parse contract addresses
    let flash_liquidator_address: Address = config
        .contracts
        .flash_liquidator
        .parse()
        .wrap_err("Invalid flash_liquidator address")?;

    // Initialize metrics
    let metrics = Metrics::new();

    // Initialize the liquidator orchestrator
    let liquidator = Arc::new(Liquidator::new(
        http_provider.clone(),
        config.execution.clone(),
        flash_liquidator_address,
        metrics.clone(),
        &config.sequencer.rpc_url,
    ));

    // Shutdown signal channel
    let (shutdown_tx, _) = broadcast::channel::<()>(1);

    // Spawn protocol monitors
    let mut protocol_handles = Vec::new();

    // --- AAVE v3 ---
    if let Some(ref aave_config) = config.protocols.aave_v3 {
        if aave_config.enabled {
            let pool: Address = aave_config.pool.parse().wrap_err("Invalid AAVE v3 pool address")?;
            let data_provider: Address = aave_config
                .data_provider
                .parse()
                .wrap_err("Invalid AAVE v3 data_provider address")?;

            let protocol = AaveV3Protocol::new(
                http_provider.clone(),
                pool,
                data_provider,
                aave_config.min_profit_usd,
                config.execution.multicall_batch_size,
            );

            let liquidator = liquidator.clone();
            let metrics = metrics.clone();
            let mut shutdown_rx = shutdown_tx.subscribe();

            let handle = tokio::spawn(async move {
                info!("AAVE v3 monitor started");
                run_protocol_monitor(protocol, liquidator, metrics, &mut shutdown_rx).await;
                info!("AAVE v3 monitor stopped");
            });
            protocol_handles.push(handle);
        }
    }

    // --- Radiant ---
    if let Some(ref radiant_config) = config.protocols.radiant {
        if radiant_config.enabled {
            let pool: Address = radiant_config.pool.parse().wrap_err("Invalid Radiant pool address")?;
            let data_provider: Address = radiant_config
                .data_provider
                .parse()
                .wrap_err("Invalid Radiant data_provider address")?;

            let protocol = RadiantProtocol::new(
                http_provider.clone(),
                pool,
                data_provider,
                radiant_config.min_profit_usd,
                config.execution.multicall_batch_size,
            );

            let liquidator = liquidator.clone();
            let metrics = metrics.clone();
            let mut shutdown_rx = shutdown_tx.subscribe();

            let handle = tokio::spawn(async move {
                info!("Radiant monitor started");
                run_protocol_monitor(protocol, liquidator, metrics, &mut shutdown_rx).await;
                info!("Radiant monitor stopped");
            });
            protocol_handles.push(handle);
        }
    }

    // --- Silo (Phase 4 skeleton) ---
    if let Some(ref silo_config) = config.protocols.silo {
        if silo_config.enabled {
            let lens: Address = silo_config.lens.parse().wrap_err("Invalid Silo lens address")?;
            let repository: Address = silo_config
                .repository
                .parse()
                .wrap_err("Invalid Silo repository address")?;

            let protocol = SiloProtocol::new(
                http_provider.clone(),
                lens,
                repository,
                silo_config.min_profit_usd,
            );

            let liquidator = liquidator.clone();
            let metrics = metrics.clone();
            let mut shutdown_rx = shutdown_tx.subscribe();

            let handle = tokio::spawn(async move {
                info!("Silo monitor started");
                run_protocol_monitor(protocol, liquidator, metrics, &mut shutdown_rx).await;
                info!("Silo monitor stopped");
            });
            protocol_handles.push(handle);
        }
    }

    // --- Dashboard web server ---
    {
        let metrics = metrics.clone();
        let port = config.monitoring.dashboard_port;
        tokio::spawn(async move {
            if let Err(e) = web::start_dashboard(metrics, port).await {
                error!(error = %e, "Dashboard server failed");
            }
        });
    }

    // --- Sequencer Feed ---
    let (feed_tx, mut feed_rx) = tokio::sync::mpsc::unbounded_channel();
    {
        let feed_url = config.sequencer.feed_url.clone();
        tokio::spawn(async move {
            sequencer_feed::run_sequencer_feed(feed_url, feed_tx).await;
        });
    }

    // --- Main block subscription loop ---
    info!("Subscribing to new blocks via WebSocket");
    let sub = ws_provider
        .subscribe_blocks()
        .await
        .wrap_err("Failed to subscribe to new blocks")?;

    let block_stream = sub.into_stream();
    // Pin the stream for use in tokio::select!
    let mut block_stream = std::pin::pin!(block_stream);
    let mut metrics_interval = tokio::time::interval(std::time::Duration::from_secs(60));

    info!("Entering main event loop");

    loop {
        tokio::select! {
            // New block received
            Some(block) = block_stream.next() => {
                let block_num = block.inner.number;
                info!(block = block_num, "New block received");
                metrics.record_block_processed();
            }
            // Sequencer feed event - fastest signal for new transactions
            Some(event) = feed_rx.recv() => {
                // Sequencer feed event received - a new transaction was sequenced
                // This is our fastest signal to re-check positions
                let _ = event;
                metrics.record_block_processed();
            }
            // Periodic metrics logging
            _ = metrics_interval.tick() => {
                metrics.log_summary();
            }
            // Graceful shutdown on SIGINT (Ctrl+C)
            _ = signal::ctrl_c() => {
                info!("Received SIGINT, initiating shutdown");
                let _ = shutdown_tx.send(());
                break;
            }
        }
    }

    // Wait for all protocol monitors to finish
    info!("Waiting for protocol monitors to stop...");
    for handle in protocol_handles {
        let _ = handle.await;
    }

    // Final metrics summary
    metrics.log_summary();
    info!("Arbitrum Liquidator shutdown complete");

    Ok(())
}

/// Run a protocol monitor loop that scans for liquidation opportunities
/// each time it is triggered.
///
/// Listens for the shutdown signal to terminate gracefully.
async fn run_protocol_monitor<Proto, P>(
    protocol: Proto,
    liquidator: Arc<Liquidator<P>>,
    metrics: Metrics,
    shutdown_rx: &mut broadcast::Receiver<()>,
) where
    Proto: Protocol,
    P: Provider + Clone + Send + Sync,
{
    // Scan interval: Arbitrum has ~250ms blocks, so we scan every few seconds
    // to avoid overwhelming the RPC.
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));

    loop {
        tokio::select! {
            _ = interval.tick() => {
                // Get the latest block for the scan
                let block_number = 0u64; // Placeholder; in production, read from shared state

                match protocol.get_liquidatable_positions(block_number).await {
                    Ok(opportunities) => {
                        let count = opportunities.len();
                        metrics.record_positions_scanned(count as u64);

                        if !opportunities.is_empty() {
                            info!(
                                protocol = protocol.name(),
                                count,
                                "Found liquidation opportunities"
                            );

                            if let Err(e) = liquidator.process_batch(opportunities).await {
                                error!(
                                    protocol = protocol.name(),
                                    error = %e,
                                    "Error processing liquidation batch"
                                );
                                metrics.record_error();
                            }
                        }
                    }
                    Err(e) => {
                        warn!(
                            protocol = protocol.name(),
                            error = %e,
                            "Error scanning for liquidatable positions"
                        );
                        metrics.record_error();
                    }
                }
            }
            _ = shutdown_rx.recv() => {
                info!(protocol = protocol.name(), "Received shutdown signal");
                break;
            }
        }
    }
}
