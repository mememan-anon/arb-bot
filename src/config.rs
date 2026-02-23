use anyhow::{anyhow, Result};
use serde::Deserialize;
use std::{env, fs, path::PathBuf};

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub chain: ChainConfig,
    #[serde(default)]
    pub start_tokens: Vec<TokenConfig>,
    pub dexes: DexesConfig,
    #[serde(default)]
    pub gas: GasConfig,
    #[serde(default)]
    pub aave_v3: Option<AaveV3Config>,
    #[serde(default)]
    pub execution: ExecutionConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChainConfig {
    pub name: String,
    pub chain_id: u64,
    pub https_url: String,
    pub wss_url: String,
    #[serde(default)]
    pub wss_urls: Vec<String>,
    #[serde(default)]
    pub cache_dir: Option<String>,
    /// Token addresses to ignore during path generation (scam/honeypot tokens).
    #[serde(default)]
    pub blacklisted_tokens: Vec<String>,
    /// Output cap multiplier. Scale: 10 = 1×, 20 = 2×, 50 = 5×.
    /// Real arb returns ~1.001–1.02×; dust-pool fakes return ~235,000×.
    /// Default 50 = 5× cap — safe headroom above real arb.
    #[serde(default = "default_max_profit_pct")]
    pub max_profit_pct: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TokenConfig {
    pub symbol: String,
    pub address: String,
    pub decimals: u8,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct DexesConfig {
    pub v2: Option<V2Config>,
    pub v3: Option<V3Config>,
    pub algebra: Option<AlgebraConfig>,
    /// UniswapV3 CL (any V3 fork) — slot0() ABI concentrated liquidity.
    pub uniswapv3cl: Option<UniswapV3CLConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct V2Config {
    pub enabled: bool,
    #[serde(default)]
    pub factories: Vec<String>,
    #[serde(default)]
    pub routers: Vec<String>,
    #[serde(default)]
    pub from_blocks: Vec<u64>,
    /// Fee in basis points (e.g. 30 = 0.3%, 71 = 0.71%). Blackhole V2 uses 71.
    #[serde(default = "default_v2_fee_bps")]
    pub fee_bps: u32,
    /// Call the pair contract directly instead of going through the router.
    #[serde(default)]
    pub direct_pair: bool,
}

/// Uniswap V3-style concentrated liquidity config (currently unused — set enabled=false)
#[derive(Debug, Clone, Deserialize, Default)]
pub struct V3Config {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub factories: Vec<String>,
    #[serde(default)]
    pub routers: Vec<String>,
    #[serde(default)]
    pub quoter: Option<String>,
    #[serde(default)]
    pub fee_tiers: Vec<u32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AlgebraConfig {
    pub enabled: bool,
    #[serde(default)]
    pub factories: Vec<String>,
    #[serde(default)]
    pub routers: Vec<String>,
    #[serde(default)]
    pub quoter: Option<String>,
}

/// UniswapV3 CL (Uniswap V3 fork) configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct UniswapV3CLConfig {
    pub enabled: bool,
    /// UniswapV3CL factory address.
    #[serde(default)]
    pub factory: Option<String>,
    /// SwapRouter address.
    #[serde(default)]
    pub router: Option<String>,
    /// Quoter / QuoterV2 address (optional, used for off-chain price queries).
    #[serde(default)]
    pub quoter: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct GasConfig {
    pub enabled: bool,
    #[serde(default)]
    pub token_symbol: Option<String>,
    #[serde(default)]
    pub token_address: Option<String>,
    #[serde(default)]
    pub token_decimals: Option<u8>,
    #[serde(default)]
    pub stable_symbol: Option<String>,
    #[serde(default)]
    pub stable_address: Option<String>,
    #[serde(default)]
    pub stable_decimals: Option<u8>,
    #[serde(default)]
    pub pool_address: Option<String>,
    #[serde(default)]
    pub token_is_token0: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AaveV3Config {
    pub pool: String,
    #[serde(default)]
    pub pool_addresses_provider: Option<String>,
    #[serde(default)]
    pub oracle: Option<String>,
    #[serde(default)]
    pub data_provider: Option<String>,
    #[serde(default)]
    pub allowed_assets: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExecutionConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_min_profit_threshold")]
    pub min_profit_threshold: f64,
    #[serde(default = "default_max_position_size")]
    pub max_position_size: f64,
    #[serde(default = "default_true")]
    pub use_flashloan: bool,
    #[serde(default = "default_flashloan_provider")]
    pub default_flashloan_provider: String,
    #[serde(default = "default_max_priority_fee_gwei")]
    pub max_priority_fee_gwei: u64,
    #[serde(default = "default_max_base_fee_gwei")]
    pub max_base_fee_gwei: u64,
    #[serde(default = "default_true")]
    pub simulation_required: bool,
    /// Estimated gas units for a single arb transaction (used for profit netting).
    #[serde(default = "default_estimated_gas")]
    pub estimated_gas: u64,
    /// Balancer V2 Vault address for Balancer flash loans.
    /// On Avalanche: 0xBA12222222228d8Ba445958a75a0704d566BF2C8
    #[serde(default)]
    pub balancer_vault: Option<String>,
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            min_profit_threshold: default_min_profit_threshold(),
            max_position_size: default_max_position_size(),
            use_flashloan: default_true(),
            default_flashloan_provider: default_flashloan_provider(),
            max_priority_fee_gwei: default_max_priority_fee_gwei(),
            max_base_fee_gwei: default_max_base_fee_gwei(),
            simulation_required: default_true(),
            estimated_gas: default_estimated_gas(),
            balancer_vault: None,
        }
    }
}

fn default_min_profit_threshold() -> f64 { 5.0 }
fn default_max_position_size() -> f64 { 10000.0 }
fn default_true() -> bool { true }
fn default_flashloan_provider() -> String { "AaveV3".to_string() }
fn default_max_priority_fee_gwei() -> u64 { 50 }
fn default_max_base_fee_gwei() -> u64 { 100 }
fn default_estimated_gas() -> u64 { 550_000 }
fn default_v2_fee_bps() -> u32 { 30 }
fn default_max_profit_pct() -> f64 { 50.0 }

impl Config {
    pub fn load() -> Result<Self> {
        let config_path = resolve_config_path()?;
        let raw = fs::read_to_string(&config_path)
            .map_err(|e| anyhow!("failed to read config {}: {e}", config_path.display()))?;
        let cfg: Config =
            toml::from_str(&raw).map_err(|e| anyhow!("failed to parse config: {e}"))?;
        Ok(cfg)
    }
}

fn resolve_config_path() -> Result<PathBuf> {
    if let Ok(path) = env::var("BOT_CONFIG") {
        return Ok(PathBuf::from(path));
    }

    let chain = env::var("BOT_CHAIN").unwrap_or_else(|_| "avax".to_string());
    Ok(PathBuf::from(format!("config/{chain}.toml")))
}
