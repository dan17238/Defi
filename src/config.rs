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
}

fn default_dashboard_port() -> u16 {
    3000
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
        if self.contracts.flash_liquidator == "0x0000000000000000000000000000000000000000" {
            eyre::bail!("contracts.flash_liquidator is zero address — deploy the contract first");
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
