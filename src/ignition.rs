/// Pipeline orchestrator — creates channels, spawns all workers, and runs the
/// channel-based pipeline.
///
/// Architecture:
/// ```text
/// NewBlock → [broadcast] → MarketState → [mpsc: PoolsTouched] → Searcher
///   → [mpsc: ArbPath] → Simulator → [mpsc: ValidPath] → TxSender
/// ```
///
/// Each worker runs in its own tokio task. Communication is via bounded
/// mpsc channels (and one broadcast channel for new blocks).

use alloy::primitives::{Address, Bytes, U256};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use crate::cache_v2::PathRateCache;
use crate::calculation::rates::RateEstimator;
use crate::config::GasParamsConfig;
use crate::gas_station::GasStation;
use crate::gen_alloy::FLASH_QUOTER_DEPLOYED_BYTECODE;
use crate::market_state::{MarketStateConfig, MarketStateWorker};
use crate::mempool::{self, MempoolConfig};
use crate::pipeline_events::{ArbPath, PipelineEvent, PoolsTouched, ValidPath};
use crate::quoter_revm::RevmQuoter;
use crate::searcher_pipeline::{PoolMeta, SearcherConfig, SearcherWorker};
use crate::simulator_pipeline::{SharedArbReceiver, SimulatorConfig, SimulatorWorker};
use crate::state_db::BlockStateDB;
use crate::swap_types::SwapPath;
use crate::tx_sender_pipeline::{TxSenderConfig, TxSenderWorker};

/// Pipeline configuration.
pub struct PipelineConfig {
    /// HTTP RPC URL for tracing + lazy state fetches.
    pub rpc_url: String,
    /// WS RPC URL for new block subscriptions.
    pub ws_url: String,
    /// Sequencer URL for transaction submission.
    pub sequencer_url: String,
    /// Bot contract address.
    pub bot_address: Address,
    /// Signer private key.
    pub signer_key: String,
    /// Chain ID (Base = 8453).
    pub chain_id: u64,
    /// Channel buffer sizes.
    pub channel_buffer: usize,
    /// Broadcast capacity for new block events.
    pub broadcast_capacity: usize,
    /// Whether to run in dry-run mode (no actual submissions).
    pub dry_run: bool,
    /// Last block the state DB was populated up to (enables catch-up on startup).
    /// Set to 0 to skip catch-up.
    pub last_synced_block: u64,
    /// Whether a flash loan is used for arb execution.
    /// When true, `flash_loan_fee_bps` is deducted from profit in the simulator.
    pub use_flashloan: bool,
    /// Flash loan fee in basis points (e.g. 5 = Aave V3 0.05%).
    /// Only applied when `use_flashloan` is true.
    pub flash_loan_fee_bps: u64,
    /// Runtime-selected flash-loan provider name used for fee deduction.
    pub flash_loan_provider: String,
    /// Address of the chain's native gas token (e.g. WBNB on BSC, WETH on Base/Avax).
    /// Profit from non-native start tokens is price-converted to this unit.
    pub native_token: Address,
    /// Maps non-native start token addresses to a V2 price pool pairing them with the native token.
    /// Used to normalise arb profit into native-token units before comparing with gas cost.
    pub token_price_pools: HashMap<Address, Address>,
    // —— Execution tuning (from ExecutionConfig) ———————————————————————————————
    /// Maximum gas limit for arb transactions.
    pub max_gas_limit: u64,
    /// Share of profit to allocate to the priority fee (bps; 5000 = 50%).
    pub profit_share_bps: u64,
    /// Minimum net profit to actually submit a transaction (wei).
    pub min_submit_profit_wei: u64,
    /// Minimum profit for a path to pass the simulator stage (wei).
    pub min_sim_profit_wei: u64,
    /// Number of binary search steps for input amount optimisation.
    pub optimization_steps: usize,
    /// Minimum flash-loan input amount (ETH units; e.g. 0.01).
    pub min_input_eth: f64,
    /// Maximum flash-loan input amount (ETH units; e.g. 50.0).
    pub max_input_eth: f64,
    /// Maximum paths the searcher evaluates per block.
    pub max_paths_per_block: usize,
    /// Number of parallel simulator workers (REVM instances).
    pub num_simulators: usize,
    /// Rough gas estimate used by the searcher for candidate pre-screening.
    pub searcher_gas_estimate: u64,

    /// Stale-block cutoff for both simulators and tx-sender (blocks behind chain head).
    pub max_stale_blocks: u64,
    /// Unified strike threshold for path/token blacklisting.
    pub strike_threshold: u32,
    // —— Chain EIP-1559 parameters (from GasParamsConfig) —————————————————————
    /// Chain-specific base-fee prediction parameters.
    pub gas_params: GasParamsConfig,
    /// Live base fee seeded from `eth_gasPrice` at startup (wei).
    /// When Some, overrides the `min_base_fee_wei` config seed in GasStation.
    /// Critical for chains like BSC where block headers always report baseFeePerGas=0.
    pub live_base_fee_wei: Option<u64>,
    /// Path to the opportunities log file (e.g. "cache/bsc/opportunities.log").
    /// Every ValidPath that passes the profit threshold is appended here.
    pub opportunities_log: String,
    /// Enable the mempool listener (pending tx subscription for DEX swaps).
    pub enable_mempool: bool,
    /// Router addresses for mempool filtering (from revm_filter.routers config).
    /// Values are hex-encoded addresses.
    pub revm_filter_routers: HashMap<String, String>,
    /// Path to persistent toxic-token blacklist file (e.g. "cache/bsc/.toxic-tokens.toml").
    /// Tokens listed here are pre-loaded into the blacklist at startup and never simulated.
    /// New toxic tokens discovered at runtime are appended to this file.
    pub toxic_tokens_path: String,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            rpc_url: String::new(),
            ws_url: String::new(),
            sequencer_url: String::new(),
            bot_address: Address::ZERO,
            signer_key: String::new(),
            chain_id: 0,
            channel_buffer: 256,
            broadcast_capacity: 16,
            dry_run: true,
            last_synced_block: 0,
            use_flashloan: false,
            flash_loan_fee_bps: 0,
            flash_loan_provider: "AaveV3".to_string(),
            native_token: Address::ZERO,
            token_price_pools: HashMap::new(),
            max_gas_limit: 1_000_000,
            profit_share_bps: 5000,
            min_submit_profit_wei: 5_000_000_000_000,
            min_sim_profit_wei: 10_000_000_000_000,
            optimization_steps: 20,
            min_input_eth: 0.01,
            max_input_eth: 50.0,
            max_paths_per_block: 50_000,
            num_simulators: 4,
            searcher_gas_estimate: 250_000,

            max_stale_blocks: 3,
            strike_threshold: 3,
            gas_params: GasParamsConfig::default(),
            live_base_fee_wei: None,
            opportunities_log: String::new(),
            enable_mempool: false,
            revm_filter_routers: HashMap::new(),
            toxic_tokens_path: String::new(),
        }
    }
}

/// Running pipeline — holds all worker handles and channels.
pub struct Pipeline {
    /// Broadcast sender for new block events (feed this from the block stream).
    pub block_tx: broadcast::Sender<PipelineEvent>,
    /// Worker task handles.
    pub handles: Vec<JoinHandle<()>>,
    /// Shared state.
    pub state_db: Arc<RwLock<BlockStateDB>>,
    pub gas_station: Arc<GasStation>,
}

impl Pipeline {
    /// Shut down the pipeline by dropping the broadcast sender.
    pub fn shutdown(self) {
        drop(self.block_tx);
        // Workers will exit when their channels close.
        for handle in self.handles {
            handle.abort();
        }
    }
}

/// Start the full pipeline with all workers.
///
/// Returns a `Pipeline` struct containing the block event sender
/// (feed new blocks into this) and task handles.
pub fn start_pipeline(
    config: PipelineConfig,
    paths: Vec<SwapPath>,
    pool_metas: Vec<PoolMeta>,
    pools: Vec<(Address, bool)>, // (address, is_v3) for tracking
) -> Pipeline {
    // ── Shared state ────────────────────────────────────────────────────────
    let state_db = Arc::new(RwLock::new(BlockStateDB::new(
        config.rpc_url.clone(),
        0,
    )));
    let gas_station = Arc::new(GasStation::new(&config.gas_params));
    // Seed base_fee from live eth_gasPrice if provided (critical on BSC where
    // block headers always report baseFeePerGas=0x0 — the live price is the true floor).
    if let Some(live_wei) = config.live_base_fee_wei {
        gas_station.base_fee.store(live_wei, std::sync::atomic::Ordering::Relaxed);
        log::info!(
            "[GasStation] Seeded base_fee from live eth_gasPrice: {} wei ({:.4} gwei)",
            live_wei,
            live_wei as f64 / 1e9
        );
    }
    let estimator = Arc::new(RwLock::new(RateEstimator::new()));
    let rate_cache = Arc::new(PathRateCache::new());

    // ── Load FlashQuoter bytecode and deploy into REVM ──────────────────────
    let quoter = {
        // Strip the "0x" prefix from the hex string if present
        let hex_str = FLASH_QUOTER_DEPLOYED_BYTECODE.trim();
        let hex_str = hex_str.strip_prefix("0x").unwrap_or(hex_str);
        let bytecode_bytes = hex::decode(hex_str)
            .expect("Failed to decode FlashQuoter deployed bytecode hex");
        log::info!(
            "[Quoter] Loaded FlashQuoter deployed bytecode: {} bytes",
            bytecode_bytes.len()
        );
        let quoter = RevmQuoter::with_bytecode(Bytes::from(bytecode_bytes));
        // Deploy the quoter contract into the shared state_db
        {
            let mut db = state_db.write().unwrap();
            quoter
                .deploy_into(&mut db)
                .expect("Failed to deploy FlashQuoter into REVM state_db");
        }
        log::info!("[Quoter] FlashQuoter deployed into REVM at 0x1000");
        Arc::new(quoter)
    };

    // Pre-compute wei values for SimulatorConfig once to avoid repeated float mul.
    let min_input_wei = (config.min_input_eth * 1e18) as u128;
    let max_input_wei = (config.max_input_eth * 1e18) as u128;

    // Register tracked pools and pre-fetch their bytecodes
    {
        let mut db = state_db.write().unwrap();
        for (pool_addr, _) in &pools {
            db.track_pool(*pool_addr);
        }
        // Pre-fetch all pool bytecodes so REVM simulation doesn't need
        // slow lazy RPC fetches during hot-path execution.
        db.prefetch_pool_codes();

        // Pre-fetch token0/token1 storage slots so the FlashQuoter can
        // determine swap direction without per-call RPC.
        db.prefetch_token_slots();
        // Pre-fetch V3 slot0 (sqrtPriceX96, tick) and liquidity for all pools.
        // These are the first storage reads in every V3 swap() call.
        db.prefetch_v3_slots();

        // Pre-fetch V3 tick bitmap and initialized tick data near current price.
        // This enables the V3 pool contracts to execute swap() natively inside REVM
        // (the try-catch quoter path) without lazy RPC fallback for tick data.
        // Build list of (pool_address, tick_spacing, is_pancake_v3) for V3 pools only.
        use crate::swap_types::PoolProtocol;
        let v3_tick_info: Vec<(Address, i32, bool)> = pool_metas.iter()
            .filter(|m| m.is_v3 && m.tick_spacing > 0)
            .map(|m| (m.address, m.tick_spacing, m.protocol == PoolProtocol::PancakeSwapV3))
            .collect();
        if !v3_tick_info.is_empty() {
            log::info!("[Ignition] Prefetching V3 tick data for {} pools...", v3_tick_info.len());
            db.prefetch_v3_tick_data(&v3_tick_info);
        }
    }

    // ── Channels ────────────────────────────────────────────────────────────
    let (block_tx, _) = broadcast::channel::<PipelineEvent>(config.broadcast_capacity);
    let (pools_tx, pools_rx) = mpsc::channel::<PoolsTouched>(config.channel_buffer);
    let (arb_tx, arb_rx) = mpsc::channel::<ArbPath>(config.channel_buffer);
    let (valid_tx, valid_rx) = mpsc::channel::<ValidPath>(config.channel_buffer);

    // Clone pools_tx for the mempool worker (before MarketState moves it).
    let mempool_pools_tx = pools_tx.clone();

    // Build token-pair → pool lookup for the mempool before pool_metas is consumed.
    let token_pair_lookup = mempool::build_token_pair_lookup(&pool_metas);

    let mut handles = Vec::new();

    // ── Worker 1: Market State ──────────────────────────────────────────────
    let market_state = MarketStateWorker::new(
        MarketStateConfig {
            rpc_url: config.rpc_url.clone(),
            last_synced_block: config.last_synced_block,
        },
        state_db.clone(),
        gas_station.clone(),
    );
    let block_rx_1 = block_tx.subscribe();
    handles.push(tokio::spawn(async move {
        market_state.run(block_rx_1, pools_tx).await;
    }));

    // Shared latest-block atomic across searcher + all simulator workers.
    // The searcher publishes the block it's evaluating; simulators check it
    // on every path to skip stale queued items without waiting for fresh arb items.
    let shared_latest_block = Arc::new(std::sync::atomic::AtomicU64::new(0));

    // ── Worker 2: Searcher ──────────────────────────────────────────────────
    let mut searcher = SearcherWorker::new(
        config.rpc_url.clone(),
        SearcherConfig {
            max_paths_per_block: config.max_paths_per_block,
            searcher_gas_estimate: config.searcher_gas_estimate,

        },
        state_db.clone(),
        estimator.clone(),
        gas_station.clone(),
        rate_cache.clone(),
        shared_latest_block.clone(),
    );
    searcher.register_paths(paths);
    searcher.register_pool_meta(pool_metas);

    handles.push(tokio::spawn(async move {
        searcher.run(pools_rx, arb_tx).await;
    }));

    // ── Worker 3: Simulator (N parallel workers) ──────────────────────────
    let num_sims = config.num_simulators.max(1);
    let shared_arb_rx: SharedArbReceiver = Arc::new(tokio::sync::Mutex::new(arb_rx));

    let sim_config = SimulatorConfig {
        min_profit_wei: U256::from(config.min_sim_profit_wei),
        optimization_steps: config.optimization_steps,
        min_input: U256::from(min_input_wei),
        max_input: U256::from(max_input_wei),
        profit_share_bps: config.profit_share_bps,
        flash_loan_fee_bps: if config.use_flashloan { config.flash_loan_fee_bps } else { 0 },
        flash_loan_provider: config.flash_loan_provider.clone(),
        rpc_url: config.rpc_url.clone(),
        max_stale_blocks: config.max_stale_blocks,
    };

    // Shared blacklist across all workers so one worker's revert info benefits others.
    let shared_blacklist = Arc::new(Mutex::new(HashSet::<u64>::new()));
    // Shared token-level blacklist: honeypot/tax tokens that repeatedly revert.
    // Pre-seed from persistent TOML cache file if it exists.
    let mut initial_toxic: HashSet<Address> = HashSet::new();
    if !config.toxic_tokens_path.is_empty() {
        if let Ok(contents) = std::fs::read_to_string(&config.toxic_tokens_path) {
            // Preferred format:
            // [toxic_tokens]
            // addresses = ["0x...", "0x..."]
            let mut parsed_from_toml = false;
            if let Ok(value) = toml::from_str::<toml::Value>(&contents) {
                if let Some(items) = value
                    .get("toxic_tokens")
                    .and_then(|t| t.get("addresses"))
                    .and_then(|a| a.as_array())
                {
                    for item in items {
                        if let Some(addr_str) = item.as_str() {
                            if let Ok(addr) = addr_str.parse::<Address>() {
                                initial_toxic.insert(addr);
                            }
                        }
                    }
                    parsed_from_toml = true;
                }
            }

            // Backward-compat fallback: legacy line-based file (one address per line).
            if !parsed_from_toml {
                for line in contents.lines() {
                    let line = line.trim();
                    if line.is_empty() || line.starts_with('#') { continue; }
                    if let Ok(addr) = line.parse::<Address>() {
                        initial_toxic.insert(addr);
                    }
                }
            }
            if !initial_toxic.is_empty() {
                log::info!("[Ignition] Loaded {} toxic tokens from persistent cache {}", initial_toxic.len(), config.toxic_tokens_path);
            }
        }
    }
    let shared_blacklisted_tokens: Arc<Mutex<HashSet<Address>>> = Arc::new(Mutex::new(initial_toxic));
    // Start tokens that should NEVER be blacklisted (WBNB, USDT, etc.)
    let mut start_tokens: HashSet<Address> = config.token_price_pools.keys().cloned().collect();
    start_tokens.insert(config.native_token);

    for worker_id in 0..num_sims {
        let simulator = SimulatorWorker {
            config: sim_config.clone(),
            state_db: state_db.clone(),
            quoter: quoter.clone(),
            gas_station: gas_station.clone(),
            blacklisted_paths: shared_blacklist.clone(),
            blacklisted_tokens: shared_blacklisted_tokens.clone(),
            sim_mode: std::env::var("SIM")
                .map(|v| v == "true" || v == "1")
                .unwrap_or(false),
            native_token: config.native_token,
            token_price_pools: config.token_price_pools.clone(),
            shared_latest_block: shared_latest_block.clone(),
        };
        let rx = shared_arb_rx.clone();
        let tx = valid_tx.clone();
        handles.push(tokio::spawn(async move {
            simulator.run(rx, tx, worker_id).await;
        }));
    }
    // Drop the extra valid_tx clone so the TxSender sees channel-close
    // when all simulator workers exit.
    drop(valid_tx);

    // ── Worker 4: Transaction Sender ────────────────────────────────────────
    let flash_loan_provider = match config.flash_loan_provider.to_ascii_lowercase().as_str() {
        "aave" | "aavev3" => 0u8,
        "uniswap" | "uniswapv3" | "univ3" => 1u8,
        "pancake" | "pancakeswap" | "pancakeswapv3" | "pcsv3" => 2u8,
        "none" | "direct" => 255u8,
        other => {
            log::warn!(
                "[Pipeline] Unknown flash_loan_provider '{}' — falling back to direct provider selector",
                other
            );
            255u8
        }
    };

    let tx_sender = TxSenderWorker::new(
        TxSenderConfig {
            sequencer_url: config.sequencer_url,
            rpc_url: config.rpc_url.clone(),
            bot_address: config.bot_address,
            signer_key: config.signer_key,
            chain_id: config.chain_id,
            dry_run: config.dry_run,
            max_gas_limit: config.max_gas_limit,
            profit_share_bps: config.profit_share_bps,
            min_submit_profit_wei: U256::from(config.min_submit_profit_wei),
            max_stale_blocks: config.max_stale_blocks,
            strike_threshold: config.strike_threshold,
            opportunities_log: config.opportunities_log,
            flash_loan_provider,
        },
        gas_station.clone(),
        shared_blacklist.clone(),
        shared_blacklisted_tokens.clone(),
        start_tokens.clone(),
        config.toxic_tokens_path,
    );
    handles.push(tokio::spawn(async move {
        tx_sender.run(valid_rx).await;
    }));

    // ── Worker 5 (optional): Mempool listener ───────────────────────────────
    if config.enable_mempool {
        // Collect router addresses from revm_filter.routers config values.
        let mut router_set: HashSet<Address> = HashSet::new();
        for (_name, addr_hex) in &config.revm_filter_routers {
            let hex = addr_hex.trim().strip_prefix("0x").unwrap_or(addr_hex.trim());
            if let Ok(bytes) = hex::decode(hex) {
                if bytes.len() == 20 {
                    router_set.insert(Address::from_slice(&bytes));
                }
            }
        }
        if router_set.is_empty() {
            log::warn!("[Pipeline] Mempool enabled but no router addresses configured in revm_filter.routers");
        } else {
            let mempool_config = MempoolConfig {
                ws_url: config.ws_url.clone(),
                router_addresses: router_set,
                token_pair_to_pool: token_pair_lookup,
            };
            handles.push(tokio::spawn(async move {
                mempool::stream_pending_swaps(mempool_config, mempool_pools_tx).await;
            }));
            log::info!("[Pipeline] Mempool listener enabled");
        }
    }

    log::info!(
        "[Pipeline] Started with {} workers ({} simulator(s)), {} tracked pools",
        handles.len(),
        num_sims,
        pools.len(),
    );

    Pipeline {
        block_tx,
        handles,
        state_db,
        gas_station,
    }
}

/// Feed a new block event into the pipeline.
///
/// Call this from your block subscription loop.
pub fn feed_new_block(
    pipeline: &Pipeline,
    block_number: u64,
    base_fee: u64,
    gas_used: u64,
    gas_limit: u64,
    timestamp: u64,
) {
    let event = PipelineEvent::NewBlock {
        block_number,
        base_fee,
        gas_used,
        gas_limit,
        timestamp,
    };
    if let Err(e) = pipeline.block_tx.send(event) {
        log::warn!("[Pipeline] Failed to broadcast new block: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pipeline_config_default() {
        let config = PipelineConfig::default();
        assert_eq!(config.chain_id, 0);
        assert!(config.dry_run);
    }
}
