use eyre::{Context, Result};
use serde::Deserialize;
use std::path::Path;

/// Top-level application configuration, deserialized from TOML.
#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub rpc: RpcConfig,
    pub wallet: WalletConfig,
    pub contracts: ContractsConfig,
    pub protocols: ProtocolsConfig,
    pub execution: ExecutionConfig,
    pub sequencer: SequencerConfig,
    pub monitoring: MonitoringConfig,
    #[serde(default)]
    pub arbitrage: Option<ArbitrageConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RpcConfig {
    pub http_url: String,
    pub ws_url: String,
    /// Optional IPC path for local node (fastest, <0.3ms).
    /// If set, used instead of http_url for reads.
    #[serde(default)]
    pub ipc_path: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WalletConfig {
    /// Name of the environment variable holding the private key.
    pub private_key_env: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ContractsConfig {
    /// Address of the deployed FlashLiquidator contract.
    pub flash_liquidator: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProtocolsConfig {
    pub aave_v3: Option<AaveV3Config>,
    pub radiant: Option<RadiantConfig>,
    pub silo: Option<SiloConfig>,
}

impl ProtocolsConfig {
    pub fn any_enabled(&self) -> bool {
        self.aave_v3.as_ref().is_some_and(|cfg| cfg.enabled)
            || self.radiant.as_ref().is_some_and(|cfg| cfg.enabled)
            || self.silo.as_ref().is_some_and(|cfg| cfg.enabled)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct AaveV3Config {
    pub enabled: bool,
    pub pool: String,
    pub data_provider: String,
    pub min_profit_usd: f64,
    /// Optional: extra reserve tokens to monitor (addresses).
    #[serde(default)]
    pub extra_reserves: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RadiantConfig {
    pub enabled: bool,
    pub pool: String,
    pub data_provider: String,
    pub min_profit_usd: f64,
    #[serde(default)]
    pub extra_reserves: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SiloConfig {
    pub enabled: bool,
    pub lens: String,
    pub repository: String,
    pub min_profit_usd: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SequencerConfig {
    pub feed_url: String,
    pub rpc_url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExecutionConfig {
    pub dry_run: bool,
    pub max_gas_price_gwei: f64,
    pub min_profit_usd: f64,
    pub multicall_batch_size: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MonitoringConfig {
    pub metrics_port: u16,
    #[serde(default = "default_dashboard_port")]
    pub dashboard_port: u16,
    pub log_level: String,
    /// Telegram bot token (from @BotFather). Empty = notifications disabled.
    #[serde(default)]
    pub telegram_bot_token: String,
    /// Telegram chat ID to send notifications to.
    #[serde(default)]
    pub telegram_chat_id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ArbitrageConfig {
    pub enabled: bool,
    pub min_profit_usd: f64,
    pub max_gas_price_gwei: f64,
    pub flash_arbitrage_contract: String,
    #[serde(default)]
    pub dry_run: bool,
    /// Legacy 2-pool pairs (converted to routes internally)
    #[serde(default)]
    pub pairs: Vec<ArbitragePairConfig>,
    /// Multi-hop routes (N pools, circular path)
    #[serde(default)]
    pub routes: Vec<ArbitrageRouteConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ArbitragePairConfig {
    pub name: String,
    pub pool_a: String,
    pub pool_b: String,
    pub token0: String,
    pub token1: String,
    pub fee_a: u32,
    pub fee_b: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ArbitrageRouteConfig {
    pub name: String,
    /// Ordered pool addresses forming a circular route
    pub pools: Vec<String>,
    /// Token that pool[0] wants back (profit token). The code auto-computes
    /// zeroForOne for each hop by reading on-chain token0/token1.
    pub input_token: String,
}

fn default_dashboard_port() -> u16 {
    3001
}

impl AppConfig {
    /// Load configuration from a TOML file at the given path.
    pub fn load(path: &Path) -> Result<Self> {
        let contents = std::fs::read_to_string(path)
            .wrap_err_with(|| format!("Failed to read config file: {}", path.display()))?;
        let config: AppConfig =
            toml::from_str(&contents).wrap_err("Failed to parse config TOML")?;
        config.validate()?;
        Ok(config)
    }

    /// Basic validation of required configuration fields.
    fn validate(&self) -> Result<()> {
        if self.rpc.ws_url.is_empty() {
            eyre::bail!("rpc.ws_url must not be empty");
        }
        if self.rpc.http_url.is_empty() {
            eyre::bail!("rpc.http_url must not be empty");
        }
        if self.execution.multicall_batch_size == 0 {
            eyre::bail!("execution.multicall_batch_size must be > 0");
        }
        if self.protocols.any_enabled()
            && self.contracts.flash_liquidator == "0x0000000000000000000000000000000000000000"
        {
            eyre::bail!("contracts.flash_liquidator is zero address — deploy the contract first");
        }
        if let Some(silo) = &self.protocols.silo {
            if silo.enabled {
                eyre::bail!("protocols.silo.enabled=true but Silo support is not implemented yet");
            }
        }
        if let Some(arb) = &self.arbitrage {
            if arb.enabled {
                if arb.flash_arbitrage_contract == "0x0000000000000000000000000000000000000000" {
                    eyre::bail!(
                        "arbitrage.flash_arbitrage_contract is zero address — deploy the contract first"
                    );
                }
                if arb.pairs.is_empty() && arb.routes.is_empty() {
                    eyre::bail!("arbitrage.enabled=true but no pairs or routes are configured");
                }
                if arb.max_gas_price_gwei <= 0.0 {
                    eyre::bail!("arbitrage.max_gas_price_gwei must be > 0");
                }
                if arb.min_profit_usd < 0.0 {
                    eyre::bail!("arbitrage.min_profit_usd must be >= 0");
                }
            }
        }
        Ok(())
    }

    /// Resolve the wallet private key from the environment variable specified in config.
    pub fn resolve_private_key(&self) -> Result<String> {
        let var_name = &self.wallet.private_key_env;
        std::env::var(var_name)
            .wrap_err_with(|| format!("Environment variable '{}' not set", var_name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_config() -> AppConfig {
        AppConfig {
            rpc: RpcConfig {
                http_url: "http://localhost:8545".to_string(),
                ws_url: "ws://localhost:8546".to_string(),
                ipc_path: None,
            },
            wallet: WalletConfig {
                private_key_env: "TEST_KEY".to_string(),
            },
            contracts: ContractsConfig {
                flash_liquidator: "0x1111111111111111111111111111111111111111".to_string(),
            },
            protocols: ProtocolsConfig {
                aave_v3: None,
                radiant: None,
                silo: Some(SiloConfig {
                    enabled: false,
                    lens: "0x0000000000000000000000000000000000000001".to_string(),
                    repository: "0x0000000000000000000000000000000000000002".to_string(),
                    min_profit_usd: 1.0,
                }),
            },
            execution: ExecutionConfig {
                dry_run: true,
                max_gas_price_gwei: 1.0,
                min_profit_usd: 0.5,
                multicall_batch_size: 100,
            },
            sequencer: SequencerConfig {
                feed_url: "wss://example.com/feed".to_string(),
                rpc_url: "https://example.com/rpc".to_string(),
            },
            monitoring: MonitoringConfig {
                metrics_port: 9090,
                dashboard_port: 3000,
                log_level: "info".to_string(),
                telegram_bot_token: String::new(),
                telegram_chat_id: String::new(),
            },
            arbitrage: None,
        }
    }

    #[test]
    fn rejects_enabled_silo_until_implemented() {
        let mut config = base_config();
        config.protocols.silo.as_mut().unwrap().enabled = true;

        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("Silo support is not implemented"));
    }

    #[test]
    fn allows_zero_flash_liquidator_when_only_arbitrage_is_enabled() {
        let mut config = base_config();
        config.contracts.flash_liquidator =
            "0x0000000000000000000000000000000000000000".to_string();
        config.arbitrage = Some(ArbitrageConfig {
            enabled: true,
            min_profit_usd: 0.5,
            max_gas_price_gwei: 1.0,
            flash_arbitrage_contract: "0x2222222222222222222222222222222222222222".to_string(),
            dry_run: true,
            pairs: vec![ArbitragePairConfig {
                name: "test".to_string(),
                pool_a: "0x1111111111111111111111111111111111111111".to_string(),
                pool_b: "0x2222222222222222222222222222222222222222".to_string(),
                token0: "0x3333333333333333333333333333333333333333".to_string(),
                token1: "0x4444444444444444444444444444444444444444".to_string(),
                fee_a: 500,
                fee_b: 3000,
            }],
            routes: vec![],
        });

        config.validate().unwrap();
    }

    #[test]
    fn rejects_zero_flash_liquidator_when_liquidation_is_enabled() {
        let mut config = base_config();
        config.contracts.flash_liquidator =
            "0x0000000000000000000000000000000000000000".to_string();
        config.protocols.aave_v3 = Some(AaveV3Config {
            enabled: true,
            pool: "0x1111111111111111111111111111111111111111".to_string(),
            data_provider: "0x2222222222222222222222222222222222222222".to_string(),
            min_profit_usd: 1.0,
            extra_reserves: vec![],
        });

        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("flash_liquidator is zero address"));
    }
}
