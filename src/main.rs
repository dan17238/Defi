mod arbitrage;
mod config;
mod liquidator;
mod protocols;
mod provider;
mod sequencer_feed;
mod state;
mod utils;
mod web;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use alloy::network::EthereumWallet;
use alloy::primitives::Address;
use alloy::providers::Provider;
use alloy::signers::local::PrivateKeySigner;
use eyre::{Context, Result};
use futures::StreamExt;
use tokio::signal;
use tokio::sync::{broadcast, Notify};
use tracing::{error, info, warn};

use crate::arbitrage::ArbitrageMonitor;
use crate::config::AppConfig;
use crate::liquidator::Liquidator;
use crate::protocols::aave_v3::AaveV3Protocol;
use crate::protocols::radiant::RadiantProtocol;
use crate::protocols::silo::SiloProtocol;
use crate::protocols::Protocol;
use crate::sequencer_feed::SequencerEvent;
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

    // CLI --dry-run overrides config (both execution and arbitrage)
    if let Some(dry_run) = args.dry_run {
        config.execution.dry_run = dry_run;
        if let Some(ref mut arb) = config.arbitrage {
            arb.dry_run = dry_run;
        }
    }

    // Initialize tracing subscriber with the configured log level
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&config.monitoring.log_level));

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

    // --- Wallet setup ---
    // Resolve the private key from environment and create a signer.
    let private_key_hex = config.resolve_private_key()?;
    let signer: PrivateKeySigner = private_key_hex
        .parse()
        .wrap_err("Failed to parse private key into signer")?;
    let wallet_address = signer.address();
    let wallet = EthereumWallet::from(signer);
    info!(wallet = %wallet_address, "Wallet loaded");

    // --- Providers ---
    // Read-only provider: IPC (fastest) > HTTP (fallback)
    let mut ws_provider = provider::create_ws_provider(&config.rpc.ws_url).await?;
    let read_provider =
        provider::create_best_read_provider(&config.rpc.http_url, config.rpc.ipc_path.as_deref())
            .await?;

    // Execution provider (signed with wallet, points to sequencer for lowest latency)
    let exec_provider = provider::create_signed_http_provider(&config.sequencer.rpc_url, wallet)?;

    let block_number = provider::get_latest_block_number(&read_provider).await?;
    info!(block_number, "Connected to Arbitrum");

    // Shared latest block number, updated by the main loop, read by protocol monitors.
    let latest_block = Arc::new(AtomicU64::new(block_number));

    // Parse contract addresses
    let flash_liquidator_address: Address = config
        .contracts
        .flash_liquidator
        .parse()
        .wrap_err("Invalid flash_liquidator address")?;

    // Initialize metrics
    let metrics = Metrics::new();

    // Clone exec_provider for the arb monitor before moving it into Liquidator.
    // In the current alloy stack the cloned provider shares the cached nonce
    // manager, so liquidation and arbitrage submissions stay on one nonce
    // sequence instead of maintaining independent local counters.
    let arb_exec_provider = exec_provider.clone();

    // Initialize the liquidator orchestrator
    // read_provider is used for simulation; exec_provider for sending transactions.
    let liquidator = Arc::new(Liquidator::new(
        read_provider.clone(),
        exec_provider,
        config.execution.clone(),
        flash_liquidator_address,
        wallet_address,
        metrics.clone(),
    ));

    // Shutdown signal channel
    let (shutdown_tx, _) = broadcast::channel::<()>(1);

    // Notify used by sequencer feed to trigger protocol rescans
    let rescan_notify = Arc::new(Notify::new());

    // Spawn protocol monitors
    let mut protocol_handles = Vec::new();

    // --- AAVE v3 ---
    if let Some(ref aave_config) = config.protocols.aave_v3 {
        if aave_config.enabled {
            let pool: Address = aave_config
                .pool
                .parse()
                .wrap_err("Invalid AAVE v3 pool address")?;
            let data_provider: Address = aave_config
                .data_provider
                .parse()
                .wrap_err("Invalid AAVE v3 data_provider address")?;

            let protocol = AaveV3Protocol::new(
                read_provider.clone(),
                pool,
                data_provider,
                aave_config.min_profit_usd,
                config.execution.multicall_batch_size,
            );

            let liquidator = liquidator.clone();
            let metrics = metrics.clone();
            let mut shutdown_rx = shutdown_tx.subscribe();
            let latest_block = latest_block.clone();
            let rescan_notify = rescan_notify.clone();

            let handle = tokio::spawn(async move {
                info!("AAVE v3 monitor started");
                run_protocol_monitor(
                    protocol,
                    liquidator,
                    metrics,
                    &mut shutdown_rx,
                    latest_block,
                    rescan_notify,
                )
                .await;
                info!("AAVE v3 monitor stopped");
            });
            protocol_handles.push(handle);
        }
    }

    // --- Radiant ---
    if let Some(ref radiant_config) = config.protocols.radiant {
        if radiant_config.enabled {
            let pool: Address = radiant_config
                .pool
                .parse()
                .wrap_err("Invalid Radiant pool address")?;
            let data_provider: Address = radiant_config
                .data_provider
                .parse()
                .wrap_err("Invalid Radiant data_provider address")?;

            let protocol = RadiantProtocol::new(
                read_provider.clone(),
                pool,
                data_provider,
                radiant_config.min_profit_usd,
                config.execution.multicall_batch_size,
            );

            let liquidator = liquidator.clone();
            let metrics = metrics.clone();
            let mut shutdown_rx = shutdown_tx.subscribe();
            let latest_block = latest_block.clone();
            let rescan_notify = rescan_notify.clone();

            let handle = tokio::spawn(async move {
                info!("Radiant monitor started");
                run_protocol_monitor(
                    protocol,
                    liquidator,
                    metrics,
                    &mut shutdown_rx,
                    latest_block,
                    rescan_notify,
                )
                .await;
                info!("Radiant monitor stopped");
            });
            protocol_handles.push(handle);
        }
    }

    // --- Silo (Phase 4 skeleton) ---
    if let Some(ref silo_config) = config.protocols.silo {
        if silo_config.enabled {
            let lens: Address = silo_config
                .lens
                .parse()
                .wrap_err("Invalid Silo lens address")?;
            let repository: Address = silo_config
                .repository
                .parse()
                .wrap_err("Invalid Silo repository address")?;

            let protocol = SiloProtocol::new(
                read_provider.clone(),
                lens,
                repository,
                silo_config.min_profit_usd,
            );

            let liquidator = liquidator.clone();
            let metrics = metrics.clone();
            let mut shutdown_rx = shutdown_tx.subscribe();
            let latest_block = latest_block.clone();
            let rescan_notify = rescan_notify.clone();

            let handle = tokio::spawn(async move {
                info!("Silo monitor started");
                run_protocol_monitor(
                    protocol,
                    liquidator,
                    metrics,
                    &mut shutdown_rx,
                    latest_block,
                    rescan_notify,
                )
                .await;
                info!("Silo monitor stopped");
            });
            protocol_handles.push(handle);
        }
    }

    // Shared arb dashboard state
    let arb_dashboard = crate::arbitrage::dashboard::ArbDashboard::new();

    // --- Dashboard web server ---
    {
        let metrics = metrics.clone();
        let arb_dashboard = arb_dashboard.clone();
        let port = config.monitoring.dashboard_port;
        tokio::spawn(async move {
            if let Err(e) = web::start_dashboard(metrics, arb_dashboard, port).await {
                error!(error = %e, "Dashboard server failed");
            }
        });
    }

    // --- Sequencer Feed (broadcast channel: arb monitor + main loop each subscribe) ---
    let (feed_tx, _) = tokio::sync::broadcast::channel::<SequencerEvent>(1024);
    let mut feed_rx = feed_tx.subscribe();
    {
        let feed_url = config.sequencer.feed_url.clone();
        let feed_tx = feed_tx.clone();
        tokio::spawn(async move {
            sequencer_feed::run_sequencer_feed(feed_url, feed_tx).await;
        });
    }

    // Now spawn the arbitrage monitor with its own feed subscriber
    if let Some(ref arb_config) = config.arbitrage {
        if arb_config.enabled {
            let flash_arb_address: Address = arb_config
                .flash_arbitrage_contract
                .parse()
                .wrap_err("Invalid flash_arbitrage_contract address (arb spawn)")?;

            let arb_monitor = ArbitrageMonitor::new(
                read_provider.clone(),
                arb_exec_provider.clone(),
                arb_config.clone(),
                wallet_address,
                flash_arb_address,
                metrics.clone(),
                arb_dashboard.clone(),
            )
            .await
            .wrap_err("Failed to initialize ArbitrageMonitor")?;

            let mut arb_feed_rx = feed_tx.subscribe();
            let mut arb_shutdown_rx = shutdown_tx.subscribe();

            let handle = tokio::spawn(async move {
                info!("Arbitrage monitor started");
                arb_monitor
                    .run(&mut arb_feed_rx, &mut arb_shutdown_rx)
                    .await;
                info!("Arbitrage monitor stopped");
            });
            protocol_handles.push(handle);
        }
    }

    // --- Main event loop with block subscription reconnect ---
    let mut metrics_interval = tokio::time::interval(std::time::Duration::from_secs(60));
    let ws_url = config.rpc.ws_url.clone();

    info!("Entering main event loop");

    // Subscribe to blocks (reconnects on stream end)
    let mut block_stream = {
        let sub = ws_provider
            .subscribe_blocks()
            .await
            .wrap_err("Failed to subscribe to new blocks")?;
        Box::pin(sub.into_stream())
    };

    loop {
        tokio::select! {
            // New block received
            block_opt = block_stream.next() => {
                match block_opt {
                    Some(block) => {
                        let block_num = block.inner.number;
                        latest_block.store(block_num, Ordering::Release);
                        metrics.record_block_processed();
                    }
                    None => {
                        // Stream ended — reconnect instead of shutting down
                        warn!("Block subscription stream ended, reconnecting...");
                        match provider::create_ws_provider(&ws_url).await {
                            Ok(new_ws_provider) => {
                                ws_provider = new_ws_provider;
                                match ws_provider.subscribe_blocks().await {
                                    Ok(sub) => {
                                        block_stream = Box::pin(sub.into_stream());
                                        info!("Block subscription reconnected");
                                    }
                                    Err(e) => {
                                        error!(error = %e, "Failed to resubscribe to blocks");
                                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                                    }
                                }
                            }
                            Err(e) => {
                                error!(error = %e, "Failed to reconnect WS provider");
                                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                            }
                        }
                    }
                }
            }
            // Sequencer feed event - fastest signal for new transactions
            Ok(event) = feed_rx.recv() => {
                let _ = event;
                rescan_notify.notify_waiters();
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
/// each time it is triggered by timer, sequencer feed notification, or both.
///
/// Listens for the shutdown signal to terminate gracefully.
async fn run_protocol_monitor<Proto, R, E>(
    protocol: Proto,
    liquidator: Arc<Liquidator<R, E>>,
    metrics: Metrics,
    shutdown_rx: &mut broadcast::Receiver<()>,
    latest_block: Arc<AtomicU64>,
    rescan_notify: Arc<Notify>,
) where
    Proto: Protocol,
    R: Provider + Clone + Send + Sync,
    E: Provider + Clone + Send + Sync + 'static,
{
    // Run initial borrower discovery before entering the scan loop (Fix 6).
    info!(
        protocol = protocol.name(),
        "Running initial borrower discovery"
    );
    if let Err(e) = protocol.discover_borrowers().await {
        warn!(
            protocol = protocol.name(),
            error = %e,
            "Initial borrower discovery failed; will retry on next scan"
        );
    }

    // Scan interval: Arbitrum has ~250ms blocks, so we scan every few seconds
    // to avoid overwhelming the RPC.
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));

    // Periodic borrower re-discovery interval (every 10 minutes) to catch new
    // borrowers that appeared after the initial startup scan.
    let mut discovery_interval = tokio::time::interval(std::time::Duration::from_secs(600));

    loop {
        tokio::select! {
            _ = interval.tick() => {
                // Read latest block from shared state (Fix 5)
                let block_number = latest_block.load(Ordering::Acquire);

                match protocol.get_liquidatable_positions(block_number).await {
                    Ok(opportunities) => {
                        // Record that a scan happened (1 per scan, not per opportunity)
                        metrics.record_positions_scanned(1);

                        if !opportunities.is_empty() {
                            info!(
                                protocol = protocol.name(),
                                count = opportunities.len(),
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
            // Sequencer feed triggered a rescan
            _ = rescan_notify.notified() => {
                let block_number = latest_block.load(Ordering::Acquire);

                match protocol.get_liquidatable_positions(block_number).await {
                    Ok(opportunities) => {
                        metrics.record_positions_scanned(1);

                        if !opportunities.is_empty() {
                            info!(
                                protocol = protocol.name(),
                                count = opportunities.len(),
                                "Found liquidation opportunities (sequencer trigger)"
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
                            "Error scanning for liquidatable positions (sequencer trigger)"
                        );
                        metrics.record_error();
                    }
                }
            }
            // Periodic borrower re-discovery to catch new borrowers
            _ = discovery_interval.tick() => {
                info!(protocol = protocol.name(), "Running periodic borrower re-discovery");
                if let Err(e) = protocol.discover_borrowers().await {
                    warn!(
                        protocol = protocol.name(),
                        error = %e,
                        "Periodic borrower discovery failed"
                    );
                }
            }
            _ = shutdown_rx.recv() => {
                info!(protocol = protocol.name(), "Received shutdown signal");
                break;
            }
        }
    }
}
