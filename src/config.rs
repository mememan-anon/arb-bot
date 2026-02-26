use anyhow::{anyhow, Result};
use serde::Deserialize;
use std::{collections::HashMap, env, fs, path::{Path, PathBuf}};

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
    #[serde(default)]
    pub liquidation: Option<LiquidationConfig>,
    /// Master kill switch for the arbitrage sub-system.
    /// Set to false to run liquidation-only without any arb scanning.
    /// Defaults to true so existing configs that omit this field keep working.
    #[serde(default = "default_true")]
    pub arbitrage_enabled: bool,
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
    /// Optional explicit V3CL pool used as a live USD price oracle.
    /// Fetched from chain at startup if not already in the CSV cache.
    /// e.g. WETH: "0x6c561B446416E1A00E8E93E221854d6eA4171372" (WETH/USDC Uni V3)
    ///      cbBTC: "0xfBB6Eed8e7aa03B138556eeDaF5D271A5E1e43ef" (USDC/cbBTC Uni V3)
    pub price_pool: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct DexesConfig {
    pub v2: Option<V2Config>,
    pub v3: Option<V3Config>,
    pub algebra: Option<AlgebraConfig>,
    /// UniswapV3 CL (any V3 fork) — slot0() ABI concentrated liquidity.
    pub uniswapv3cl: Option<UniswapV3CLConfig>,
    /// LFJ Liquidity Book V2.1/V2.2 bin-based AMM.
    pub lfj: Option<LFJConfig>,
}

/// LFJ (Liquidity Book) V2.1/V2.2 configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct LFJConfig {
    pub enabled: bool,
    /// LFJ LBRouter V2.2 address.
    #[serde(default)]
    pub router_v22: Option<String>,
    /// LFJ LBRouter V2.1 address.
    #[serde(default)]
    pub router_v21: Option<String>,
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
/// Supports multiple factories (e.g. Uniswap V3 + PancakeSwap V3 on the same chain).
#[derive(Debug, Clone, Deserialize)]
pub struct UniswapV3CLConfig {
    pub enabled: bool,
    /// Primary factory address (single-factory backward compat).
    /// If `factories` is also set, both are merged.
    #[serde(default)]
    pub factory: Option<String>,
    /// Additional factory addresses (e.g. PancakeSwap V3).
    /// The pull script iterates all of them; pools land in the same CSV.
    #[serde(default)]
    pub factories: Vec<String>,
    /// SwapRouter address (primary, used for Uniswap V3 routing).
    #[serde(default)]
    pub router: Option<String>,
    /// Additional routers indexed parallel to `factories`.
    /// Index 0 = router for factory[0], etc.  Shorter than factories → last entry reused.
    #[serde(default)]
    pub routers: Vec<String>,
    /// Quoter / QuoterV2 address (optional, used for off-chain price queries).
    #[serde(default)]
    pub quoter: Option<String>,
    /// Minimum raw Uniswap V3 liquidity (L = sqrt(x*y) in atomic units) for a
    /// pool to be considered active each block.  This is NOT a USD value.
    /// 0 = only skip zero-liquidity pools (default).
    /// 1_000_000_000 (1e9) = filter out dust while keeping any real pool.
    #[serde(default)]
    pub min_liquidity: u64,
}

impl UniswapV3CLConfig {
    /// All factory addresses: merges the legacy `factory` field with `factories`.
    pub fn all_factories(&self) -> Vec<&str> {
        let mut out: Vec<&str> = self.factory.as_deref().into_iter().collect();
        for f in &self.factories {
            if !out.contains(&f.as_str()) {
                out.push(f.as_str());
            }
        }
        out
    }

    /// Router for a given factory index.  Falls back to `routers[last]` then `router`.
    pub fn router_for(&self, idx: usize) -> Option<&str> {
        if let Some(r) = self.routers.get(idx) {
            return Some(r.as_str());
        }
        if !self.routers.is_empty() {
            return Some(self.routers.last().unwrap().as_str());
        }
        self.router.as_deref()
    }
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

/// A single Chainlink price feed mapping: the AAVE reserve token address to
/// the Chainlink proxy contract that prices it on this chain.
///
/// Used by `price_trigger.rs` to subscribe to `AnswerUpdated` events and,
/// on chains with an accessible mempool, to detect `forward(transmit())`
/// pending transactions before they confirm.
#[derive(Debug, Clone, Deserialize)]
pub struct ChainlinkFeed {
    /// Aave reserve token address (e.g. WETH on Base).
    pub asset: String,
    /// Chainlink EACAggregatorProxy contract for this asset on this chain.
    /// The price trigger resolves the underlying OCR2 aggregator at startup
    /// by calling `aggregator()` on this proxy.
    pub proxy: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct OffchainPriceConfig {
    /// Master switch for off-chain early-warning sources.
    /// Off-chain signals are advisory and should not bypass on-chain oracle checks.
    #[serde(default)]
    pub enabled: bool,
    /// If true, off-chain updates can be forwarded into the liquidation pipeline
    /// as early-warning events.
    #[serde(default)]
    pub dispatch_price_updates: bool,
    /// Keep liquidation execution gated by on-chain oracle confirmation.
    /// Defaults to true for safety.
    #[serde(default = "default_true")]
    pub require_onchain_confirmation: bool,
    #[serde(default = "default_offchain_max_staleness_secs")]
    pub max_staleness_secs: u64,
    #[serde(default = "default_offchain_max_confidence_bps")]
    pub max_confidence_bps: u32,
    #[serde(default = "default_offchain_min_rescan_interval_ms")]
    pub min_rescan_interval_ms: u64,
    #[serde(default)]
    pub replay_file: Option<String>,
    #[serde(default)]
    pub pyth_hermes: Option<PythHermesConfig>,
    #[serde(default)]
    pub chainlink_streams: Option<ChainlinkStreamsConfig>,
    #[serde(default)]
    pub assets: Vec<OffchainAssetConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PythHermesConfig {
    pub endpoint: String,
    #[serde(default = "default_pyth_poll_interval_ms")]
    pub poll_interval_ms: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChainlinkStreamsConfig {
    pub endpoint: String,
    #[serde(default = "default_chainlink_poll_interval_ms")]
    pub poll_interval_ms: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OffchainAssetConfig {
    /// Aave reserve token address.
    pub asset: String,
    /// Symbol label for logs/metrics.
    pub symbol: String,
    /// Pyth Hermes feed id (hex string, no 0x prefix).
    #[serde(default)]
    pub pyth_feed_id: Option<String>,
    /// Chainlink feed path on data.chain.link (e.g. eth-usd, usdc-usd, cbbtc-usd).
    #[serde(default)]
    pub chainlink_feed_path: Option<String>,
    /// Optional Chainlink stream/feed id when available.
    #[serde(default)]
    pub chainlink_feed_id: Option<String>,
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
    /// AAVE UiPoolDataProvider address — required for Phase 2 (health factor cache).
    /// Base mainnet: verify at https://docs.aave.com/developers/deployed-contracts/v3-mainnet/base
    #[serde(default)]
    pub ui_pool_data_provider: Option<String>,
    #[serde(default)]
    pub allowed_assets: Vec<String>,
    /// Enable the Phase-1 liquidation monitor (whistleblower task).
    /// Set to true in config to start listening for AAVE v3 events.
    #[serde(default)]
    pub monitoring_enabled: bool,
    /// Path to a flat text file (one address per line) of known AAVE v3 borrowers.
    /// Generated by `scripts/pull_aave_borrowers.py`.
    /// Phase 2 uses this to pre-seed the health-factor cache at startup so every
    /// borrower is known immediately rather than being discovered from live events.
    #[serde(default)]
    pub borrowers_file: Option<String>,
    /// Chainlink price feeds to monitor for oracle price updates.
    /// Each entry maps an Aave reserve token → its Chainlink proxy address.
    /// The price trigger (Phase 2.5) subscribes to AnswerUpdated events and,
    /// on chains with an accessible mempool, also watches pending transmit() txs.
    /// Leave empty to disable the Chainlink price trigger.
    #[serde(default)]
    pub chainlink_feeds: Vec<ChainlinkFeed>,
    /// Enable Path B (full pending-tx mempool subscription) inside price_trigger.
    /// Defaults to true, but set to false on chains (e.g. Base) where the node
    /// does not support `eth_subscribe("newPendingTransactions")` so the warning
    /// is suppressed and the attempt is skipped entirely.
    #[serde(default = "default_true")]
    pub chainlink_pending_txs: bool,
    /// Off-chain early-warning price sources (Pyth Hermes / Chainlink Streams).
    #[serde(default)]
    pub offchain_price: OffchainPriceConfig,
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
    pub max_priority_fee_gwei: f64,
    #[serde(default = "default_max_base_fee_gwei")]
    pub max_base_fee_gwei: f64,
    #[serde(default = "default_true")]
    pub simulation_required: bool,
    /// Estimated gas units for a single arb transaction (used for profit netting).
    #[serde(default = "default_estimated_gas")]
    pub estimated_gas: u64,
    #[serde(default)]
    pub balancer_vault: Option<String>,
    /// Private sequencer / builder RPC URL for MEV-protection.
    /// When set, transactions are submitted ONLY here — not to the public mempool.
    /// Recommended for Base: "https://rpc.titanbuilder.xyz/" (dominant Base builder)
    /// Can also be overridden at runtime via the PRIVATE_RPC_URL env var.
    #[serde(default)]
    pub private_rpc_url: Option<String>,
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
            private_rpc_url: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct LiquidationConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_max_position_size")]
    pub max_position_size: f64,
    #[serde(default = "default_estimated_gas")]
    pub estimated_gas: u64,
    #[serde(default)]
    pub balancer_vault: Option<String>,
    #[serde(default = "default_min_liquidation_profit_usd")]
    pub min_liquidation_profit_usd: f64,
    #[serde(default)]
    pub balancer_pool_ids: Vec<String>,
    /// Morpho Blue contract address (0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb on Base).
    /// When set, the executor checks Morpho's balance of the debt token first.
    /// If adequate, Morpho is preferred over Balancer (both charge 0% fee, but Morpho
    /// can cover assets that Balancer may not hold).
    #[serde(default)]
    pub morpho: Option<String>,
    /// AAVE V3 lending pool address used as a last-resort flash-loan source
    /// (0.05% fee, charged as `premium` in the executeOperation callback).
    /// Must match the address set via `bot.setAavePool()` in the deployed contract.
    #[serde(default)]
    pub aave_pool: Option<String>,
    /// Path to the opportunity log CSV file.
    /// Every detected liquidation event (profitable or not) is appended here.
    /// Defaults to "cache/<chain>/liquidation_opportunities.csv".
    #[serde(default)]
    pub opportunity_log: Option<String>,
}

impl Default for LiquidationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_position_size: default_max_position_size(),
            estimated_gas: default_estimated_gas(),
            balancer_vault: None,
            min_liquidation_profit_usd: default_min_liquidation_profit_usd(),
            balancer_pool_ids: Vec::new(),
            morpho: None,
            aave_pool: None,
            opportunity_log: None,
        }
    }
}

impl LiquidationConfig {
    pub fn from_execution(cfg: &ExecutionConfig) -> Self {
        Self {
            enabled: cfg.enabled,
            max_position_size: cfg.max_position_size,
            estimated_gas: cfg.estimated_gas,
            balancer_vault: cfg.balancer_vault.clone(),
            min_liquidation_profit_usd: default_min_liquidation_profit_usd(),
            balancer_pool_ids: Vec::new(),
            morpho: None,
            aave_pool: None,
            opportunity_log: None,
        }
    }
}

/// Default minimum profit threshold in USD.
/// The active config file (e.g. avax.toml) always overrides this.
/// 0.5 = $0.50 USD minimum net profit after gas and flash-loan fees.
fn default_min_profit_threshold() -> f64 { 0.5 }
/// Minimum net profit for a liquidation tx — higher than arb since gas is ~550k.
/// $5 is a comfortable floor: covers extreme gas spikes and leaves real margin.
fn default_min_liquidation_profit_usd() -> f64 { 5.0 }
fn default_max_position_size() -> f64 { 10000.0 }
fn default_true() -> bool { true }
fn default_flashloan_provider() -> String { "AaveV3".to_string() }
fn default_max_priority_fee_gwei() -> f64 { 50.0 }
fn default_max_base_fee_gwei() -> f64 { 100.0 }
fn default_estimated_gas() -> u64 { 550_000 }
fn default_v2_fee_bps() -> u32 { 30 }
fn default_max_profit_pct() -> f64 { 50.0 }
fn default_pyth_poll_interval_ms() -> u64 { 300 }
fn default_chainlink_poll_interval_ms() -> u64 { 300 }
fn default_offchain_max_staleness_secs() -> u64 { 20 }
fn default_offchain_max_confidence_bps() -> u32 { 150 }
fn default_offchain_min_rescan_interval_ms() -> u64 { 300 }

impl Config {
    pub fn load() -> Result<Self> {
        // Load .env first so env vars are available for overrides below.
        dotenv::dotenv().ok();

        let config_path = resolve_config_path()?;
        let raw = fs::read_to_string(&config_path)
            .map_err(|e| anyhow!("failed to read config {}: {e}", config_path.display()))?;
        let raw = apply_address_file_substitutions(&config_path, raw)?;
        let mut cfg: Config =
            toml::from_str(&raw).map_err(|e| anyhow!("failed to parse config: {e}"))?;

        // Env var overrides for secrets — never put API keys in TOML files.
        // WSS_URL / HTTPS_URL override the chain RPC endpoints for the active chain.
        if let Ok(v) = env::var("WSS_URL")  { cfg.chain.wss_url   = v; }
        if let Ok(v) = env::var("HTTPS_URL") { cfg.chain.https_url = v; }

        Ok(cfg)
    }
}

#[derive(Debug, Deserialize)]
struct AddressFileConfig {
    #[serde(default)]
    addresses: HashMap<String, String>,
}

fn apply_address_file_substitutions(config_path: &Path, raw: String) -> Result<String> {
    let parsed: toml::Value = toml::from_str(&raw)
        .map_err(|e| anyhow!("failed to parse config prelude {}: {e}", config_path.display()))?;

    let Some(address_file) = parsed
        .get("address_file")
        .and_then(|v| v.as_str())
    else {
        return Ok(raw);
    };

    let base_dir = config_path.parent().unwrap_or_else(|| Path::new("."));
    let address_path = base_dir.join(address_file);
    let address_raw = fs::read_to_string(&address_path)
        .map_err(|e| anyhow!("failed to read address file {}: {e}", address_path.display()))?;
    let address_cfg: AddressFileConfig = toml::from_str(&address_raw)
        .map_err(|e| anyhow!("failed to parse address file {}: {e}", address_path.display()))?;

    let mut out = raw;
    for (key, value) in address_cfg.addresses {
        let token = format!("{{{{{key}}}}}");
        out = out.replace(&token, &value);
    }

    Ok(out)
}

fn resolve_config_path() -> Result<PathBuf> {
    if let Ok(path) = env::var("BOT_CONFIG") {
        return Ok(PathBuf::from(path));
    }

    let chain = env::var("BOT_CHAIN").unwrap_or_else(|_| "avax".to_string());
    Ok(PathBuf::from(format!("config/{chain}.toml")))
}
