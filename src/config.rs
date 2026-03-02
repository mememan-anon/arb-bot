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
    pub execution: ExecutionConfig,
    /// REVM startup filter router mapping (protocol -> router address).
    /// Set per-chain in TOML `[revm_filter.routers]`.
    #[serde(default)]
    pub revm_filter: RevmFilterConfig,
    /// Chain-specific EIP-1559 base-fee calculation parameters.
    /// Controls how the gas station predicts next-block base fees.
    /// Set per-chain in the TOML [gas_params] section.
    #[serde(default)]
    pub gas_params: GasParamsConfig,
    /// AAVE V3 — optional; needed for Tier-3 flash loans and oracle price fetches.
    #[serde(default)]
    pub aave_v3: Option<AaveV3Config>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct RevmFilterConfig {
    /// Lowercase protocol key -> router address.
    /// Example keys: uniswapv2, pancakeswapv2, aerodrome, uniswapv3, pancakeswapv3.
    #[serde(default)]
    pub routers: HashMap<String, String>,
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
    /// Optional L2 sequencer submission endpoint.
    /// Set for chains with a dedicated sequencer (e.g. Base, OP).
    /// Absent on BSC, Avalanche, etc. — falls back to https_url.
    #[serde(default)]
    pub sequencer_url: Option<String>,
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

/// Minimal AAVE V3 config kept in arb-bot for:
///   - Tier-3 flash loan fallback in the arb executor/bundler
///   - Optional Chainlink-backed price oracle for start-token USD valuation
#[derive(Debug, Clone, Deserialize)]
pub struct AaveV3Config {
    /// AAVE V3 Pool contract address (used for flashLoanSimple).
    pub pool: String,
    /// AAVE V3 Oracle address (Chainlink-backed, optional — price logic skipped if absent).
    #[serde(default)]
    pub oracle: Option<String>,
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

#[derive(Debug, Clone, Deserialize)]
pub struct ExecutionConfig {
    #[serde(default = "default_true")]
    pub use_flashloan: bool,
    #[serde(default = "default_flashloan_provider")]
    pub default_flashloan_provider: String,
    #[serde(default = "default_true")]
    pub dynamic_flashloan_routing: bool,
    #[serde(default = "default_flashloan_providers")]
    pub flashloan_providers: Vec<String>,
    #[serde(default = "default_flash_loan_fee_uniswap_v3_bps")]
    pub flash_loan_fee_uniswap_v3_bps: u64,
    #[serde(default = "default_flash_loan_fee_pancakeswap_v3_bps")]
    pub flash_loan_fee_pancakeswap_v3_bps: u64,
    /// Private sequencer / builder RPC URL for MEV-protection.
    /// When set, transactions are submitted ONLY here — not to the public mempool.
    /// Can also be overridden at runtime via the PRIVATE_RPC_URL env var.
    #[serde(default)]
    pub private_rpc_url: Option<String>,
    /// Maximum gas limit for arb transactions (prevents gas grief on reverts).
    #[serde(default = "default_max_gas_limit")]
    pub max_gas_limit: u64,
    /// Share of profit to allocate to the priority fee (bps; 5000 = 50%).
    #[serde(default = "default_profit_share_bps")]
    pub profit_share_bps: u64,
    /// Minimum net profit required to submit a transaction (wei).
    #[serde(default = "default_min_submit_profit_wei")]
    pub min_submit_profit_wei: u64,
    /// Minimum net profit for a path to pass the simulator stage (wei).
    #[serde(default = "default_min_sim_profit_wei")]
    pub min_sim_profit_wei: u64,
    /// Number of binary search steps for input amount optimisation.
    #[serde(default = "default_optimization_steps")]
    pub optimization_steps: usize,
    /// Minimum flash-loan input amount (native token units; e.g. 0.01 = 0.01 ETH/BNB/AVAX).
    #[serde(default = "default_min_input_eth")]
    pub min_input_eth: f64,
    /// Maximum flash-loan input amount (native token units).
    #[serde(default = "default_max_input_eth")]
    pub max_input_eth: f64,
    /// Maximum number of hops in an arb path (e.g. 4 = up to 4-leg paths).
    #[serde(default = "default_max_hops")]
    pub max_hops: usize,
    /// Maximum arb paths generated per start token during startup.
    #[serde(default = "default_max_paths_per_token")]
    pub max_paths_per_token: usize,
    /// Maximum paths the searcher evaluates per block.
    #[serde(default = "default_max_paths_per_block")]
    pub max_paths_per_block: usize,
    /// Rough gas estimate used by the searcher for pre-screening paths (gas units).
    #[serde(default = "default_searcher_gas_estimate")]
    pub searcher_gas_estimate: u64,
    /// Number of parallel simulator workers (REVM). More workers = higher throughput
    /// at the cost of CPU. Recommended: num_cpus or half of it.
    #[serde(default = "default_num_simulators")]
    pub num_simulators: usize,
    /// Channel buffer depth for inter-worker mpsc queues.
    /// Not chain-specific — tunes server memory vs backpressure. Default suits most hardware.
    #[serde(default = "default_channel_buffer")]
    pub channel_buffer: usize,
    /// Broadcast channel capacity for new-block events.
    /// Not chain-specific — same rationale as channel_buffer.
    #[serde(default = "default_broadcast_capacity")]
    pub broadcast_capacity: usize,
    /// Aave V3 flash-loan fee in basis points (5 = 0.05%).
    /// Aave governance can change this — update here if it changes on-chain.
    #[serde(default = "default_flash_loan_fee_aave_bps")]
    pub flash_loan_fee_aave_bps: u64,
    /// Enable the mempool listener (pending tx subscription).
    /// When true, the pipeline subscribes to pending DEX swaps and triggers
    /// the searcher for affected pools before the next block is mined.
    #[serde(default)]
    pub enable_mempool: bool,
    /// Max allowed simulator-to-head lag (in blocks) in TxSender stale check.
    #[serde(default = "default_max_stale_blocks")]
    pub max_stale_blocks: u64,
    /// Unified strike threshold for path/token blacklisting.
    #[serde(default = "default_strike_threshold")]
    pub strike_threshold: u32,
    /// Maximum number of paths forwarded to simulators per block.
    /// Paths are sorted by estimated profit (descending) so only the top candidates
    /// reach REVM.  Rule-of-thumb: (block_time_ms / revm_ms_per_path) * num_simulators.
    /// BSC: ~400ms / ~185ms × 8 workers ≈ 17; default 16 keeps sims within one block.
    #[serde(default = "default_max_sim_paths")]
    pub max_sim_paths: usize,
    /// Simulator stale-block tolerance: paths from blocks older than
    /// (latest_seen - sim_stale_blocks) are discarded without simulation.
    /// BSC: 1 is correct (400ms blocks). Increase for slower chains.
    #[serde(default = "default_sim_stale_blocks")]
    pub sim_stale_blocks: u64,
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            use_flashloan: default_true(),
            default_flashloan_provider: default_flashloan_provider(),
            dynamic_flashloan_routing: default_true(),
            flashloan_providers: default_flashloan_providers(),
            flash_loan_fee_uniswap_v3_bps: default_flash_loan_fee_uniswap_v3_bps(),
            flash_loan_fee_pancakeswap_v3_bps: default_flash_loan_fee_pancakeswap_v3_bps(),
            private_rpc_url: None,
            max_gas_limit: default_max_gas_limit(),
            profit_share_bps: default_profit_share_bps(),
            min_submit_profit_wei: default_min_submit_profit_wei(),
            min_sim_profit_wei: default_min_sim_profit_wei(),
            optimization_steps: default_optimization_steps(),
            min_input_eth: default_min_input_eth(),
            max_input_eth: default_max_input_eth(),
            max_hops: default_max_hops(),
            max_paths_per_token: default_max_paths_per_token(),
            max_paths_per_block: default_max_paths_per_block(),
            searcher_gas_estimate: default_searcher_gas_estimate(),
            num_simulators: default_num_simulators(),
            channel_buffer: default_channel_buffer(),
            broadcast_capacity: default_broadcast_capacity(),
            flash_loan_fee_aave_bps: default_flash_loan_fee_aave_bps(),
            enable_mempool: false,
            max_stale_blocks: default_max_stale_blocks(),
            strike_threshold: default_strike_threshold(),
            max_sim_paths: default_max_sim_paths(),
            sim_stale_blocks: default_sim_stale_blocks(),
        }
    }
}

fn default_true() -> bool { true }
fn default_flashloan_provider() -> String { "AaveV3".to_string() }
fn default_flashloan_providers() -> Vec<String> {
    vec![
        "AaveV3".to_string(),
        "UniswapV3".to_string(),
        "PancakeSwapV3".to_string(),
    ]
}
fn default_v2_fee_bps() -> u32 { 30 }
fn default_max_profit_pct() -> f64 { 50.0 }
// ExecutionConfig defaults
fn default_max_gas_limit() -> u64 { 1_000_000 }
fn default_profit_share_bps() -> u64 { 5000 }
fn default_min_submit_profit_wei() -> u64 { 50_000_000_000_000 }   // 0.00005 ETH
fn default_min_sim_profit_wei() -> u64 { 100_000_000_000_000 }    // 0.0001 ETH
fn default_optimization_steps() -> usize { 20 }
fn default_min_input_eth() -> f64 { 0.01 }
fn default_max_input_eth() -> f64 { 50.0 }
fn default_max_hops() -> usize { 4 }
fn default_max_paths_per_token() -> usize { 200_000 }
fn default_max_paths_per_block() -> usize { 50_000 }
fn default_searcher_gas_estimate() -> u64 { 250_000 }
fn default_num_simulators() -> usize { 4 }
fn default_channel_buffer() -> usize { 256 }
fn default_broadcast_capacity() -> usize { 128 }
fn default_flash_loan_fee_aave_bps() -> u64 { 5 }
fn default_flash_loan_fee_uniswap_v3_bps() -> u64 { 0 }
fn default_flash_loan_fee_pancakeswap_v3_bps() -> u64 { 0 }
fn default_max_stale_blocks() -> u64 { 3 }
fn default_strike_threshold() -> u32 { 3 }
fn default_max_sim_paths() -> usize { 16 }
fn default_sim_stale_blocks() -> u64 { 1 }

/// Chain-specific EIP-1559 base-fee prediction parameters.
/// These values vary by chain and must match on-chain protocol constants.
///
/// Base / Optimism (Canyon hardfork): elasticity=6, denominator=50, min_base_fee=0.001 gwei
/// BSC / Avalanche (standard EIP-1559): elasticity=2, denominator=8, min_base_fee=1 gwei
#[derive(Debug, Clone, Deserialize)]
pub struct GasParamsConfig {
    /// EIP-1559 elasticity multiplier (gas_target = gas_limit / elasticity).
    #[serde(default = "default_eip1559_elasticity")]
    pub eip1559_elasticity_multiplier: u64,
    /// EIP-1559 base fee change denominator (controls how fast base fee adjusts).
    #[serde(default = "default_eip1559_denominator")]
    pub eip1559_base_fee_change_denominator: u64,
    /// Minimum base fee floor in wei (chain-enforced lower bound).
    #[serde(default = "default_min_base_fee_wei")]
    pub min_base_fee_wei: u64,
    /// Hard cap on priority fee in gwei (prevents runaway priority bids).
    #[serde(default = "default_priority_fee_cap_gwei")]
    pub priority_fee_cap_gwei: u64,
}
impl Default for GasParamsConfig {
    fn default() -> Self {
        Self {
            eip1559_elasticity_multiplier: default_eip1559_elasticity(),
            eip1559_base_fee_change_denominator: default_eip1559_denominator(),
            min_base_fee_wei: default_min_base_fee_wei(),
            priority_fee_cap_gwei: default_priority_fee_cap_gwei(),
        }
    }
}
fn default_eip1559_elasticity() -> u64 { 6 }     // Base/Optimism Canyon default
fn default_eip1559_denominator() -> u64 { 50 }   // Base/Optimism Canyon default
fn default_min_base_fee_wei() -> u64 { 1_000_000 } // 0.001 gwei (Base L2 floor)
fn default_priority_fee_cap_gwei() -> u64 { 10 }  // 10 gwei hard cap

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
