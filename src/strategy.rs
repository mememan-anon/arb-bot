use chrono::Utc;
use ethers::{
    contract::abigen,
    providers::{Middleware, Provider, Ws},
    types::{H160, U256},
};
use log::{debug, info, warn};
use serde_json::json;
use std::{collections::HashMap, io::Write as _, str::FromStr, sync::Arc};
use std::sync::RwLock;
use tokio::sync::broadcast::Sender;

use crate::algebra::{fetch_algebra_states, load_algebra_pools_from_v3_csv};
use crate::uniswapv3cl::{enrich_tick_data, fetch_full_uniswapv3cl_pools, fetch_uniswapv3cl_states, fetch_v3cl_oracle_pools, load_uniswapv3cl_full_from_csv};
use crate::lfj::{fetch_lfj_states, load_lfj_pools_from_csv, LFJState};
use crate::bundler::{Bundler, Flashloan};
use crate::config::{Config, TokenConfig};
use crate::constants::{get_blacklist_tokens, Env, WEI};
use crate::executor::{ExecutionParams, Executor};
use crate::multi::{batch_get_uniswap_v2_reserves, get_uniswap_v2_reserves, Reserve};
use crate::paths::{generate_triangular_paths_mixed, generate_cross_dex_paths};
use crate::pools::{load_all_pools_from_v2, AnyPool, PoolType};
use crate::simulator::UniswapV2Simulator;
use crate::streams::Event;
use crate::utils::{get_touched_pool_reserves, get_touched_v3cl_pools};
use rayon::prelude::*;
use std::collections::HashSet;

// ── Aave price oracle (Chainlink-backed) ────────────────────────────────────
abigen!(
    AaveOracle,
    r#"[function getAssetPrice(address asset) view returns (uint256)]"#
);

/// Fetch USD prices for all start-token addresses from the Aave V3 price oracle.
/// Returns 8-decimal prices converted to f64 (e.g. 3000.0 for $3000).
/// Called at startup and refreshed every ~10 minutes (300 Base blocks ≈ 10 min).
async fn fetch_aave_oracle_prices(
    config: &Config,
    provider: Arc<Provider<Ws>>,
) -> HashMap<H160, f64> {
    let mut map = HashMap::new();
    let oracle_addr = match config.aave_v3
        .as_ref()
        .and_then(|a| a.oracle.as_deref())
        .and_then(|s| H160::from_str(s).ok())
    {
        Some(a) => a,
        None => {
            info!("Aave oracle address not configured — skipping oracle price fetch");
            return map;
        }
    };

    let oracle = AaveOracle::new(oracle_addr, provider);
    for token in &config.start_tokens {
        let token_addr = match H160::from_str(&token.address) {
            Ok(a) => a,
            Err(_) => continue,
        };
        match oracle.get_asset_price(token_addr).call().await {
            Ok(raw) => {
                // Aave oracle returns price with 8 decimals (1e8 = $1.00)
                let price = raw.as_u128() as f64 / 1e8;
                if price > 0.0 {
                    info!("Aave oracle: {} = ${:.4}", token.symbol, price);
                    map.insert(token_addr, price);
                }
            }
            Err(e) => {
                warn!("Aave oracle getAssetPrice({}) failed: {:?}", token.symbol, e);
            }
        }
    }
    map
}

pub async fn event_handler(
    provider: Arc<Provider<Ws>>,
    event_sender: Sender<Event>,
    config: Config,
) {
    let env = Env::from_config(&config);
    let debug_spreads = std::env::var("BOT_DEBUG_SPREADS").ok().as_deref() == Some("1");
    let _debug_stale = std::env::var("BOT_DEBUG_STALE").ok().as_deref() == Some("1");
    let force_seeded_v2 = std::env::var("BOT_FORCE_V2_TRI").ok().as_deref() == Some("1");
    // Debug seed pools for path filtering (used with BOT_FORCE_V2_TRI=1).
    // Set BOT_SEED_P1 / BOT_SEED_P2 / BOT_SEED_P3 to the pool addresses you want to isolate.
    let seed_addrs: Vec<H160> = ["BOT_SEED_P1", "BOT_SEED_P2", "BOT_SEED_P3"]
        .iter()
        .filter_map(|var| std::env::var(var).ok().and_then(|s| H160::from_str(&s).ok()))
        .collect();
    info!("RPC https_url={}, wss_url={}", env.https_url, env.wss_url);

    let v2 = match config.dexes.v2.as_ref() {
        Some(v2) if v2.enabled => v2,
        _ => {
            info!("V2 config missing or disabled. Nothing to do.");
            return;
        }
    };

    if v2.factories.len() != v2.from_blocks.len() {
        info!("V2 config error: factories and from_blocks length mismatch.");
        return;
    }
    if v2.factories.is_empty() || v2.routers.is_empty() {
        info!("V2 config error: factories/routers are empty.");
        return;
    }

    let cache_dir = config
        .chain
        .cache_dir
        .as_deref()
        .unwrap_or(&config.chain.name);

    let pools_vec_raw = load_all_pools_from_v2(
        env.wss_url.clone(),
        v2.factories.iter().map(|s| s.as_str()).collect(),
        v2.from_blocks.clone(),
        cache_dir,
    )
    .await
    .unwrap();
    info!("Initial V2 pool count: {}", pools_vec_raw.len());

    // ── Reserve pre-fetch & pool quality filter ─────────────────────────────
    // Fetch reserves for EVERY loaded pool before path generation.
    // This lets us:
    //   1. Filter out dust / honeypot pools (near-zero reserves produce
    //      astronomically-large fake "profits" from the AMM math).
    //   2. Seed the per-block reserves map so the event loop doesn't need a
    //      second full RPC round-trip.
    let initial_reserves = batch_get_uniswap_v2_reserves(
        env.https_url.clone(),
        pools_vec_raw.clone(),
    ).await;

    let pools_vec = pools_vec_raw;
    info!("Loaded {} V2 pools", pools_vec.len());

    let mut algebra_pools_full = Vec::new();
    // Keep info for later state updates
    let mut algebra_pools_info = Vec::new(); 

    if let Some(algebra) = config.dexes.algebra.as_ref() {
        if algebra.enabled {
            match load_algebra_pools_from_v3_csv(cache_dir) {
                Ok(pools) => {
                    info!("Loading full data for {} Algebra pools...", pools.len());
                    algebra_pools_info = pools;
                    algebra_pools_full = crate::algebra::fetch_full_algebra_pools(
                        provider.clone(), 
                        &algebra_pools_info
                    ).await;
                    // Drop CL pools that have zero liquidity — they are off-range or empty
                    // and produce the same garbage profit signals as V2 dust pools.
                    let before_alg = algebra_pools_full.len();
                    algebra_pools_full.retain(|p| !p.liquidity.is_zero());
                    info!(
                        "Loaded {} Algebra pools fully ({} with liquidity > 0)",
                        before_alg,
                        algebra_pools_full.len()
                    );
                    // Enrich Algebra pools with tick-level liquidityNet data
                    enrich_tick_data(provider.clone(), &mut algebra_pools_full).await;
                }
                Err(e) => {
                    info!("Failed to load Algebra pools: {:?}", e);
                }
            }
        }
    }

    // ── UniswapV3 CL pools ──────────────────────────────────────────────
    let mut v3cl_pools_full = Vec::new();
    let v3cl_min_liquidity: u64 = config
        .dexes
        .uniswapv3cl
        .as_ref()
        .map(|c| c.min_liquidity)
        .unwrap_or(0);

    if let Some(rv3) = config.dexes.uniswapv3cl.as_ref() {
        if rv3.enabled {
            match load_uniswapv3cl_full_from_csv(cache_dir) {
                Ok(pools_static) => {
                    info!("Loading live state for {} UniswapV3CL pools (slot0+liquidity only)...", pools_static.len());
                    v3cl_pools_full = fetch_full_uniswapv3cl_pools(
                        provider.clone(),
                        pools_static,
                    ).await;
                    let in_range = v3cl_pools_full.iter()
                        .filter(|p| p.liquidity.as_u128() >= v3cl_min_liquidity.max(1) as u128)
                        .count();
                    info!(
                        "Loaded {} UniswapV3CL pools ({} above liquidity floor {} at startup)",
                        v3cl_pools_full.len(),
                        in_range,
                        v3cl_min_liquidity,
                    );
                    // Enrich V3CL pools with tick-level liquidityNet data
                    enrich_tick_data(provider.clone(), &mut v3cl_pools_full).await;
                }
                Err(e) => {
                    info!("Failed to load UniswapV3CL pools (cache may not exist yet): {:?}", e);
                }
            }
        }
    }

    // ── Bootstrap explicit price-oracle pools ────────────────────────────────
    // Any start_token with `price_pool = "0x..."` in config is fetched from
    // chain if not already present in v3cl_pools_full (i.e. not in the CSV).
    {
        let already_loaded: std::collections::HashSet<H160> =
            v3cl_pools_full.iter().map(|p| p.address).collect();

        let missing: Vec<H160> = config
            .start_tokens
            .iter()
            .filter_map(|t| t.price_pool.as_deref())
            .filter_map(|s| H160::from_str(s).ok())
            .filter(|a| !already_loaded.contains(a))
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        if !missing.is_empty() {
            info!("Fetching {} price-oracle pool(s) from chain...", missing.len());
            let oracle_pools = fetch_v3cl_oracle_pools(provider.clone(), missing).await;
            info!("Bootstrapped {} price-oracle pool(s)", oracle_pools.len());
            v3cl_pools_full.extend(oracle_pools);
        }
    }
    let mut lfj_pools_full = Vec::new();

    if let Some(lfj_cfg) = config.dexes.lfj.as_ref() {
        if lfj_cfg.enabled {
            match load_lfj_pools_from_csv(cache_dir) {
                Ok(pools) => {
                    info!("Loaded {} LFJ pools from CSV cache", pools.len());
                    lfj_pools_full = pools;
                }
                Err(e) => {
                    info!("Failed to load LFJ pools (run scripts/pull_lfj.py first): {:?}", e);
                }
            }
        }
    }

    // Generate paths for ALL configured start tokens (multi-token support)
    let mut all_paths = Vec::new();
    let mut token_to_paths: HashMap<H160, Vec<usize>> = HashMap::new();

    if config.start_tokens.is_empty() {
        info!("No start_tokens configured.");
        return;
    }

    for start_token in &config.start_tokens {
        let start_address = match H160::from_str(&start_token.address) {
            Ok(addr) => addr,
            Err(_) => {
                info!("Invalid start token address: {}", start_token.address);
                continue;
            }
        };

        let tri_paths = generate_triangular_paths_mixed(
            &pools_vec,
            &algebra_pools_full,
            &v3cl_pools_full,
            &lfj_pools_full,
            start_address
        );
        let cross_paths = generate_cross_dex_paths(
            &pools_vec,
            &algebra_pools_full,
            &v3cl_pools_full,
            &lfj_pools_full,
            start_address
        );
        let mut token_paths = tri_paths;
        token_paths.extend(cross_paths.into_iter());
        info!(
            "Generated {} paths for token {} ({}) ({} triangular + {} cross-dex 2-hop)",
            token_paths.len(),
            start_token.symbol,
            start_token.address,
            token_paths.iter().filter(|p| p.nhop == 3).count(),
            token_paths.iter().filter(|p| p.nhop == 2).count()
        );

        let start_idx = all_paths.len();
        all_paths.extend(token_paths);
        let end_idx = all_paths.len();

        // Map token to its path indices
        let path_indices: Vec<usize> = (start_idx..end_idx).collect();
        token_to_paths.insert(start_address, path_indices);
    }

    info!("Total paths across all start tokens: {}", all_paths.len());
    let paths = all_paths;

    let blacklist_tokens = get_blacklist_tokens(&config.chain.blacklisted_tokens);

    // Runtime auto-blacklist: populated when a simulation fails with
    // "transfer amount exceeds balance" (fee-on-transfer tokens).
    // Arc<RwLock<…>> allows many rayon readers with rare writer on failure.
    let runtime_blacklist: Arc<RwLock<HashSet<H160>>> = Arc::new(RwLock::new(HashSet::new()));

    // Tick-enrich backoff for pools that repeatedly return no new tick data.
    // (Currently only used by the periodic re-enrichment; confirm-phase enrichment
    // is bypassed in favour of fast-path eth_call simulation.)
    const _TICK_ENRICH_ZERO_THRESHOLD: u8 = 3;
    const _TICK_ENRICH_BACKOFF_BLOCKS: u64 = 20;
    let mut _tick_enrich_zero_streak: HashMap<H160, u8> = HashMap::new();
    let tick_enrich_backoff_until: HashMap<H160, u64> = HashMap::new();

    // Path-level cooldown for simulation slippage failures.
    // Any path that fails eth_call with "slippage" is suppressed for
    // SLIPPAGE_COOLDOWN_BLOCKS blocks before being retried.  This prevents
    // burning repeated gas on phantom arbs that arise from stale CL-pool state
    // or thin margins on highly-active token pairs (e.g. VIRTUAL/WETH).
    //
    // THRESHOLD = 1 → ban immediately on first failure (no second chance).
    // COOLDOWN  = 500 blocks ≈ 17 min on Base (2 s / block).  Long enough
    //             for the underlying price discrepancy to genuinely resolve.
    const SLIPPAGE_FAIL_THRESHOLD: u8 = 1;
    const SLIPPAGE_COOLDOWN_BLOCKS: u64 = 500;
    let mut slippage_fail_count: HashMap<usize, u8> = HashMap::new();
    let mut slippage_cooldown_until: HashMap<usize, u64> = HashMap::new();
    // Path-level stale-signal backoff: paths that confirm as stale N times in a row
    // are suppressed from the initial scan for a short window, eliminating wasted
    // per-path RPC confirm calls for persistently-dead signals.
    const _STALE_STREAK_THRESHOLD: u8 = 3;
    const _STALE_BACKOFF_BLOCKS: u64 = 10;
    let mut stale_streak: HashMap<usize, u8> = HashMap::new();
    let mut stale_backoff_until: HashMap<usize, u64> = HashMap::new();

    // Combined cold-scan + tick re-enrichment interval (50 blocks ≈ 100s on Base).
    // Every REFRESH_INTERVAL blocks:
    //   1. Re-fetch slot0+liquidity for ALL CL pools from CSV
    //   2. Promote newly-liquid pools into algebra_pools_map (hot set)
    //   3. Re-enrich tick data for hot pools whose tick drifted out of range
    // Path generation is NOT repeated — paths are fixed at startup.
    // To add new pools/tokens, restart the bot.
    const REFRESH_INTERVAL: u64 = 50;
    let mut blocks_since_refresh: u64 = 0;

    // Active pools participating in non-blacklisted paths.
    let mut active_pools = HashMap::new();

    for path in &paths {
        if !path.should_blacklist(&blacklist_tokens) {
            active_pools.insert(path.pool_1.address, path.pool_1.clone());
            active_pools.insert(path.pool_2.address, path.pool_2.clone());
            active_pools.insert(path.pool_3.address, path.pool_3.clone());
        }
    }
    info!("Total unique pools in paths: {}", active_pools.len());

    // Separate V2 pools for reserve fetching
    let mut v2_pools_vec = Vec::new();
    let mut algebra_pools_map = HashMap::new();

    for pool in active_pools.values() {
        match &pool.pool_type {
            PoolType::V2(v2) => v2_pools_vec.push(v2.clone()),
            PoolType::Algebra(alg) => {
                algebra_pools_map.insert(pool.address, alg.clone());
            },
            PoolType::UniswapV3CL(v3cl) => {
                // Reuse the same map — both Algebra and UniswapV3CL states are
                // keyed by pool address and share AlgebraPoolFull fields.
                algebra_pools_map.insert(pool.address, v3cl.clone());
            },
            PoolType::LFJ(_) => {
                // LFJ state is fetched separately via fetch_lfj_states.
                // static LFJPoolInfo is already embedded in the path's pool_type.
            },
        }
    }

    // Seed the reserves map from the reserves we already fetched during pool filtering.
    // Only keep entries relevant to the active (path-participating) pools.
    let mut reserves: HashMap<H160, Reserve> = HashMap::new();
    for pool in &v2_pools_vec {
        if let Some(reserve) = initial_reserves.get(&pool.address) {
            reserves.insert(pool.address, reserve.clone());
        }
    }

    // LFJ per-block state map (initially empty, refreshed every block).
    let mut lfj_states_map: HashMap<H160, LFJState> = HashMap::new();
    if debug_spreads {
        let reserves_snapshot: HashMap<H160, Reserve> = reserves.clone();

        let mut found_paths = 0;
        for (idx, path) in paths.iter().enumerate() {
            if seed_addrs.iter().all(|addr| path.has_pool(addr)) {
                warn!(
                    "Seeded path idx={} p1={} p2={} p3={} z1={} z2={} z3={}",
                    idx,
                    path.pool_1.address,
                    path.pool_2.address,
                    path.pool_3.address,
                    path.zero_for_one_1,
                    path.zero_for_one_2,
                    path.zero_for_one_3
                );
                match path.simulate_mixed_path(U256::from(1u64), &reserves_snapshot, &algebra_pools_map, &lfj_states_map) {
                    Some(out) => warn!("Seeded path idx={} simulate_out={}", idx, out),
                    None => warn!("Seeded path idx={} simulate_out=None", idx),
                }
                found_paths += 1;
            }
        }
        if found_paths == 0 {
            warn!("No seeded paths found in generated paths.");
        }

        for addr in &seed_addrs {
            if let Some(reserve) = reserves.get(addr) {
                warn!(
                    "Seeded reserves {} -> r0={} r1={}",
                    addr,
                    reserve.reserve0,
                    reserve.reserve1
                );
            } else {
                warn!("Seeded reserves missing for {}", addr);
            }
        }
    }

    // Pre-compute decimal multipliers for performance
    let decimal_multipliers: HashMap<H160, U256> = config
        .start_tokens
        .iter()
        .filter_map(|t| {
            H160::from_str(&t.address).ok().map(|addr| {
                let mult = U256::from(10).pow(U256::from(t.decimals));
                (addr, mult)
            })
        })
        .collect();

    // Initialize executor for transaction execution
    let executor = Executor::new(config.clone());

    let v2_router = match v2
        .routers
        .first()
        .and_then(|r| H160::from_str(r).ok())
    {
        Some(router) => router,
        None => {
            info!("No valid V2 router configured");
            return;
        }
    };

    let algebra_router = config
        .dexes
        .algebra
        .as_ref()
        .and_then(|alg| alg.routers.first())
        .and_then(|r| H160::from_str(r).ok());

    if config.execution.enabled && algebra_router.is_none() {
        info!("Execution enabled but no Algebra router configured; Algebra hops will fallback to V2 router and may fail on-chain");
    }

    let bundler = if config.execution.enabled {
        match Bundler::new() {
            Ok(b) => Some(b),
            Err(e) => {
                info!("Bundler init failed (check PRIVATE_KEY / SIGNING_KEY / BOT_ADDRESS): {e}");
                info!("Continuing in detection-only mode.");
                None
            }
        }
    } else {
        None
    };

    if config.execution.enabled && bundler.is_none() {
        info!("Execution is enabled but disabled at runtime due to missing/invalid bundler env.");
    }

    if config.execution.enabled && bundler.is_some() {
        info!("Execution mode active: profitable opportunities will be submitted.");
    }

    if config.execution.enabled && bundler.is_some() && algebra_router.is_some() {
        info!("Mixed-route execution active for V2 and Algebra hops.");
    }

    if !config.execution.enabled {
        info!("Execution disabled in config; running in detection-only mode.");
    }

    let mut event_receiver = event_sender.subscribe();

    // ── Chainlink/Aave oracle price cache ────────────────────────────────────
    // Fetched once at startup.
    // Used as final fallback in get_token_price_usd when no on-chain pool
    // has a token/stable pair loaded.
    let oracle_prices = fetch_aave_oracle_prices(&config, provider.clone()).await;

    loop {
        match event_receiver.recv().await {
            Ok(event) => match event {
                Event::Block(block) => {
                    let block_start = std::time::Instant::now();

                    // Swap event tracking via eth_getLogs is only needed when LFJ
                    // pools are active (they use event-driven refresh).  CL and V2
                    // pools use full per-block multicall refresh instead.
                    let cl_swapped: HashSet<H160> = if !lfj_pools_full.is_empty() {
                        match get_touched_v3cl_pools(provider.clone(), block.block_number).await {
                            Ok(s) => s,
                            Err(e) => {
                                if !is_unknown_block_error(&e) {
                                    info!("get_touched_cl_pools error: {:?}", e);
                                }
                                HashSet::new()
                            }
                        }
                    } else {
                        HashSet::new()
                    };

                    // ── Step 1: Refresh all hot CL pools (slot0+liquidity) ────────
                    let cl_refresh_start = std::time::Instant::now();

                    // Algebra pools (globalState ABI) — only if Algebra is enabled
                    {
                        let alg_active: Vec<_> = algebra_pools_info
                            .iter()
                            .filter(|p| algebra_pools_map.contains_key(&p.address))
                            .cloned()
                            .collect();
                        if !alg_active.is_empty() {
                            let states = fetch_algebra_states(provider.clone(), &alg_active).await;
                            for (addr, state) in states {
                                if let Some(pool) = algebra_pools_map.get_mut(&addr) {
                                    pool.sqrt_price_x96 = state.sqrt_price_x96;
                                    pool.liquidity = state.liquidity;
                                    pool.tick = state.tick;
                                    if state.fee > 0 {
                                        pool.fee = state.fee;
                                    }
                                }
                            }
                        }
                    }

                    // V3CL pools (slot0 ABI) — build address list without cloning full structs
                    {
                        let v3cl_active: Vec<_> = v3cl_pools_full
                            .iter()
                            .filter(|p| algebra_pools_map.contains_key(&p.address))
                            .cloned()
                            .collect();
                        if !v3cl_active.is_empty() {
                            let states = fetch_uniswapv3cl_states(provider.clone(), &v3cl_active).await;
                            for (addr, state) in states {
                                if let Some(pool) = algebra_pools_map.get_mut(&addr) {
                                    pool.sqrt_price_x96 = state.sqrt_price_x96;
                                    pool.liquidity = state.liquidity;
                                    pool.tick = state.tick;
                                    if state.fee > 0 { pool.fee = state.fee; }
                                }
                            }
                        }
                    }
                    let cl_refresh_ms = cl_refresh_start.elapsed().as_millis();

                    // 1c. Refresh LFJ pools — event-driven via Swap topic filtering.
                    // LFJ Swap topic: keccak256("Swap(address,address,uint24,bytes32,bytes32,uint24,uint128,uint128)")
                    if !lfj_pools_full.is_empty() {
                        let lfj_swapped: Vec<_> = lfj_pools_full.iter()
                            .filter(|p| cl_swapped.contains(&p.address))
                            .cloned()
                            .collect();
                        if lfj_swapped.is_empty() {
                            // No LFJ pools swapped — also check via V2 Sync events
                            // as a fallback (LFJ doesn't emit V3 Swap topic).
                            // Refresh all LFJ pools only on first block, then skip.
                            // For now: only refresh LFJ pools that appear in touched_reserves
                            // (handled below after V2 reserve update).
                        } else {
                            lfj_states_map = fetch_lfj_states(provider.clone(), &lfj_swapped).await;
                        }
                    }

                    // ── Step 2: Refresh all hot V2 reserves ─────────────────────
                    let v2_refresh_start = std::time::Instant::now();
                    match get_uniswap_v2_reserves(
                        env.https_url.clone(),
                        v2_pools_vec.clone(),
                    ).await {
                        Ok(fresh_reserves) => {
                            for (addr, res) in fresh_reserves {
                                reserves.insert(addr, res);
                            }
                        }
                        Err(e) => {
                            warn!("V2 full-refresh multicall failed, falling back to event-driven: {:?}", e);
                            // Fallback: use event-driven approach
                            let touched_reserves =
                                match get_touched_pool_reserves(provider.clone(), block.block_number).await {
                                    Ok(r) => r,
                                    Err(e2) => {
                                        if !is_unknown_block_error(&e2) {
                                            info!("Error from get_touched_pool_reserves: {:?}", e2);
                                        }
                                        HashMap::new()
                                    }
                                };
                            for (address, reserve) in touched_reserves.into_iter() {
                                if reserves.contains_key(&address) {
                                    reserves.insert(address, reserve);
                                }
                            }
                        }
                    }

                    let v2_refresh_ms = v2_refresh_start.elapsed().as_millis();

                    // ── Step 3: Combined cold-scan + tick refresh (every 50 blocks) ──
                    // Fetch ALL CL pools' state, promote newly-liquid ones to hot set,
                    // and re-enrich tick data for drifted hot pools — all in one pass.
                    blocks_since_refresh += 1;
                    if blocks_since_refresh >= REFRESH_INTERVAL {
                        blocks_since_refresh = 0;
                        let refresh_start = std::time::Instant::now();

                        // 3a. Fetch slot0+liquidity for ALL V3CL pools via multicall
                        let all_states = fetch_uniswapv3cl_states(
                            provider.clone(), &v3cl_pools_full,
                        ).await;

                        // Update v3cl_pools_full with fresh state
                        for pool in v3cl_pools_full.iter_mut() {
                            if let Some(state) = all_states.get(&pool.address) {
                                pool.sqrt_price_x96 = state.sqrt_price_x96;
                                pool.liquidity = state.liquidity;
                                pool.tick = state.tick;
                            }
                        }

                        // 3b. Promote newly-liquid pools into the hot set
                        let hot_addrs: HashSet<H160> = algebra_pools_map.keys().cloned().collect();
                        let mut promoted = 0u32;
                        for pool in &v3cl_pools_full {
                            if pool.liquidity.as_u128() >= v3cl_min_liquidity.max(1) as u128
                                && !hot_addrs.contains(&pool.address)
                                && active_pools.contains_key(&pool.address)
                            {
                                // This pool is in a path and now has liquidity — promote it
                                algebra_pools_map.insert(pool.address, pool.clone());
                                promoted += 1;
                            }
                        }

                        let fetch_ms = refresh_start.elapsed().as_millis();

                        // 3c. Re-enrich tick data for hot pools whose tick drifted
                        //     outside their enriched window.
                        let mut drifted: Vec<_> = algebra_pools_map
                            .values()
                            .filter(|p| {
                                if p.tick_data.is_empty() { return true; }
                                let min_key = *p.tick_data.keys().min().unwrap();
                                let max_key = *p.tick_data.keys().max().unwrap();
                                p.tick < min_key || p.tick > max_key
                            })
                            .cloned()
                            .collect();
                        if !drifted.is_empty() {
                            enrich_tick_data(provider.clone(), &mut drifted).await;
                            for refreshed in &drifted {
                                if let Some(pool) = algebra_pools_map.get_mut(&refreshed.address) {
                                    if !refreshed.tick_data.is_empty() {
                                        pool.tick_data = refreshed.tick_data.clone();
                                    }
                                }
                            }
                        }

                        info!(
                            "[refresh] block={} | fetch:{:.0}ms enrich:{} drifted | promoted:{} | total:{:.0}ms",
                            block.block_number,
                            fetch_ms,
                            drifted.len(),
                            promoted,
                            refresh_start.elapsed().as_millis(),
                        );
                    }

                    let reserves_snapshot: HashMap<H160, Reserve> = reserves.clone();

                    let scan_block = block.block_number.as_u64();
                    // Skip pools with empty tick_data (phantom spreads) or in backoff.
                    let cl_pool_skip_set: HashSet<H160> = tick_enrich_backoff_until
                        .iter()
                        .filter(|(_, &until)| scan_block <= until)
                        .map(|(addr, _)| *addr)
                        .chain(
                            algebra_pools_map
                                .iter()
                                .filter(|(_, p)| p.tick_data.is_empty())
                                .map(|(addr, _)| *addr),
                        )
                        .collect();

                    // Detection gas model
                    let max_priority_fee_wei = U256::from((config.execution.max_priority_fee_gwei * 1e9) as u128);
                    let max_base_fee_cap_wei = U256::from((config.execution.max_base_fee_gwei * 1e9) as u128);
                    let effective_gas_price = std::cmp::min(
                        (block.next_base_fee * U256::from(2u64)).saturating_add(max_priority_fee_wei),
                        max_base_fee_cap_wei,
                    );

                    let runtime_blacklist_snapshot = runtime_blacklist
                        .read()
                        .map(|bl| bl.clone())
                        .unwrap_or_default();

                    // ── Step 4: Rayon scan all paths ─────────────────────────────
                    let scan_start = std::time::Instant::now();

                    // Check all paths grouped by start token
                    for start_token in &config.start_tokens {
                        let start_address = match H160::from_str(&start_token.address) {
                            Ok(addr) => addr,
                            Err(_) => continue,
                        };

                        // Get paths for this specific token
                        let token_path_indices = match token_to_paths.get(&start_address) {
                            Some(indices) => indices,
                            None => continue,
                        };

                        // Parallel spread evaluation using rayon.
                        // All CL+V2 state is refreshed per block, so evaluate every path.
                        // Each path is independently simulated; results are collected
                        // into a HashMap afterwards.
                        let spreads: HashMap<usize, i128> = token_path_indices
                            .par_iter()
                            .filter_map(|&idx| {
                            let path = &paths[idx];

                            // Skip paths that contain runtime-detected FoT tokens.
                            if !runtime_blacklist_snapshot.is_empty() {
                                let path_has_runtime_blacklisted = (0..path.nhop).any(|hop| {
                                    let pool = path.get_pool(hop);
                                    runtime_blacklist_snapshot.contains(&pool.token0)
                                        || runtime_blacklist_snapshot.contains(&pool.token1)
                                });
                                if path_has_runtime_blacklisted {
                                    return None;
                                }
                            }

                            let has_algebra = path.get_pool(0).pool_type.is_cl()
                                || path.get_pool(1).pool_type.is_cl()
                                || path.get_pool(2).pool_type.is_cl();

                            if force_seeded_v2
                                && !seed_addrs.is_empty()
                                && !seed_addrs.iter().all(|addr| path.has_pool(addr))
                            {
                                return None;
                            }

                            if has_algebra {
                                let cl_in_range = [0u8, 1u8, 2u8].iter().all(|&hop| {
                                    let p = path.get_pool(hop);
                                    if p.pool_type.is_cl() {
                                        algebra_pools_map
                                            .get(&p.address)
                                            .map(|s| s.liquidity.as_u128() >= v3cl_min_liquidity.max(1) as u128)
                                            .unwrap_or(false)
                                    } else {
                                        true
                                    }
                                });
                                if !cl_in_range {
                                    return None;
                                }
                                // Skip paths containing CL pools with empty tick_data or in
                                // tick-enrich backoff — they generate phantom spreads because
                                // the CL simulator assumes constant liquidity with no tick data.
                                if (0..path.nhop).any(|i| {
                                    let p = path.get_pool(i);
                                    p.pool_type.is_cl() && cl_pool_skip_set.contains(&p.address)
                                }) {
                                    return None;
                                }
                            }

                            let one_token_in = U256::from(1);
                            let simulated = path.simulate_mixed_path(one_token_in, &reserves_snapshot, &algebra_pools_map, &lfj_states_map);

                            match simulated {
                                Some(price_quote) => {
                                    let one_token_in_scaled = one_token_in * decimal_multipliers[&start_address];

                                    let max_pct_num = config.chain.max_profit_pct as u64;
                                    if price_quote * U256::from(10) > one_token_in_scaled * U256::from(max_pct_num) {
                                        return None;
                                    }

                                    let _out = price_quote.as_u128() as i128;
                                    let _in = one_token_in_scaled.as_u128() as i128;
                                    let spread = _out - _in;

                                    if spread > 0 {
                                        Some((idx, spread))
                                    } else {
                                        None
                                    }
                                }
                                None => None,
                            }
                        }).collect();

                        if spreads.is_empty() {
                            if debug_spreads {
                                info!(
                                    "No positive spreads for {} at block {}",
                                    start_token.symbol,
                                    block.block_number
                                );
                            }
                            continue;
                        }

                        let mut sorted_spreads: Vec<_> = spreads.iter().collect();
                        sorted_spreads.sort_by_key(|x| x.1);
                        sorted_spreads.reverse();

                        if debug_spreads {
                            if let Some((idx, spread)) = sorted_spreads.first() {
                                info!(
                                    "Top spread for {} at block {}: idx={} spread={}",
                                    start_token.symbol,
                                    block.block_number,
                                    idx,
                                    spread
                                );
                            }
                        }

                        // Process profitable opportunities
                        for spread in sorted_spreads {
                            let path_idx = *spread.0;
                            let current_block = block.block_number.as_u64();
                            if let Some(&until_block) = slippage_cooldown_until.get(&path_idx) {
                                if current_block <= until_block {
                                    continue;
                                }
                                slippage_cooldown_until.remove(&path_idx);
                                slippage_fail_count.remove(&path_idx);
                            }
                            if let Some(&until_block) = stale_backoff_until.get(&path_idx) {
                                if current_block <= until_block {
                                    continue;
                                }
                                stale_backoff_until.remove(&path_idx);
                                stale_streak.remove(&path_idx);
                            }
                            let path = &paths[path_idx];

                            let gas_units_for_path = estimate_gas_units_for_path(path);
                            let gas_cost_in_wei = effective_gas_price.saturating_mul(U256::from(gas_units_for_path));
                            let gas_cost_in_token = estimate_gas_cost_in_start_token(
                                &config,
                                &active_pools,
                                &reserves_snapshot,
                                gas_cost_in_wei,
                                start_token,
                            );
                            
                            let opt = path.optimize_amount_in_mixed(
                                U256::zero(), // ceiling auto-detected by exponential probe
                                &reserves_snapshot,
                                &algebra_pools_map,
                                &lfj_states_map,
                            );
                            
                            // Deduct flash-loan fee (Aave = 5 bps, Balancer = 0).
                            // opt.0 is now the raw atomic borrow amount (e.g. wei).
                            let fl_fee_raw =
                                if config.execution.default_flashloan_provider == "AaveV3" {
                                    opt.0
                                        * U256::from(5u64)
                                        / U256::from(10000u64)
                                } else {
                                    U256::zero() // Balancer: no fee
                                };

                            // Use U256 saturating subtraction to avoid i128 cast overflow.
                            let excess_profit = opt
                                .1
                                .saturating_sub(gas_cost_in_token)
                                .saturating_sub(fl_fee_raw);

                            if excess_profit > U256::zero() {
                                // ── No confirm phase: state is fresh per-block ───────
                                // All hot CL pools refreshed at block start, tick data
                                // re-enriched every 50 blocks.  The eth_call simulation
                                // before send is the authoritative guard against stale
                                // state — no intermediate re-fetch needed.

                                let profit_u256 = excess_profit;
                                let token_unit = decimal_multipliers[&start_address];
                                let profit_whole = profit_u256 / token_unit;
                                let profit_frac = profit_u256 % token_unit;
                                // Show as "X.YYYYYY tokens" using the token's decimals
                                let frac_digits = start_token.decimals as usize;
                                let profit_frac_padded = format!("{:0>width$}", profit_frac, width = frac_digits);
                                let profit_frac_str = profit_frac_padded.trim_end_matches('0');
                                let profit_display = if profit_frac_str.is_empty() {
                                    format!("{}", profit_whole)
                                } else {
                                    format!("{}.{}", profit_whole, profit_frac_str)
                                };

                                // ── Detection-time threshold filter ──────────────────────────────
                                // Only surface (log + execute) paths whose net profit exceeds
                                // min_profit_threshold (in USD).  We price the start token via the
                                // gas-oracle pool so the threshold is token-agnostic.
                                let profit_in_token_f64 = (profit_u256.as_u128() as f64)
                                    / ((10_u128.pow(start_token.decimals as u32)) as f64);
                                let token_price_usd = get_token_price_usd(
                                    &config, &active_pools, &reserves_snapshot, start_token, &oracle_prices,
                                );
                                let profit_usd = if token_price_usd > 0.0 {
                                    profit_in_token_f64 * token_price_usd
                                } else {
                                    profit_in_token_f64
                                };
                                if profit_usd < config.execution.min_profit_threshold {
                                    continue;
                                }

                                // ── Path-type label ───────────────────────────────────────────────
                                let path_kind = if path.nhop == 2 {
                                    "Cross-dex"
                                } else {
                                    "Triangular"
                                };

                                // Short DEX name for each hop
                                let dex_label = |pool: &crate::pools::AnyPool| -> &'static str {
                                    match &pool.pool_type {
                                        crate::pools::PoolType::V2(_)          => "V2",
                                        crate::pools::PoolType::Algebra(_)     => "Algebra",
                                        crate::pools::PoolType::UniswapV3CL(_) => "V3CL",
                                        crate::pools::PoolType::LFJ(_)        => "LFJ",
                                    }
                                };
                                let hops = if path.nhop == 2 {
                                    format!("{}→{}", dex_label(&path.pool_1), dex_label(&path.pool_2))
                                } else {
                                    format!("{}→{}→{}", dex_label(&path.pool_1), dex_label(&path.pool_2), dex_label(&path.pool_3))
                                };

                                // Display amount_in in whole-token units
                                let amount_in_whole = opt.0 / decimal_multipliers[&start_address];
                                let amount_in_frac = opt.0 % decimal_multipliers[&start_address];
                                let amount_in_frac_padded = format!("{:0>width$}", amount_in_frac, width = start_token.decimals as usize);
                                let amount_in_frac_str = amount_in_frac_padded.trim_end_matches('0');
                                let amount_in_display = if amount_in_frac_str.is_empty() {
                                    format!("{}", amount_in_whole)
                                } else {
                                    format!("{}.{}", amount_in_whole, amount_in_frac_str)
                                };

                                info!(
                                    "[{}] Profitable path found: token={} idx={} profit={} {} amount_in={} {} [{}]",
                                    path_kind,
                                    start_token.symbol,
                                    path_idx,
                                    profit_display,
                                    start_token.symbol,
                                    amount_in_display,
                                    start_token.symbol,
                                    hops
                                );

                                // --- Persist to opportunities.log ---
                                {
                                    let cache_dir = config.chain.cache_dir.as_deref().unwrap_or("avax");
                                    let log_path = format!("cache/{}/opportunities.log", cache_dir);
                                    let _ = std::fs::create_dir_all(format!("cache/{}", cache_dir));
                                    if let Ok(mut f) = std::fs::OpenOptions::new()
                                        .create(true)
                                        .append(true)
                                        .open(&log_path)
                                    {
                                        let _ = writeln!(
                                            f,
                                            "{ts} | block={block} | kind={kind} | hops={hops} | token={tok} | idx={idx} | profit={profit} {tok} | amount_in={amt} | p1={p1} | p2={p2} | p3={p3}",
                                            ts   = Utc::now().format("%Y-%m-%dT%H:%M:%SZ"),
                                            block = block.block_number,
                                            kind = path_kind,
                                            hops = hops,
                                            tok  = start_token.symbol,
                                            idx  = path_idx,
                                            profit = profit_display,
                                            amt  = amount_in_display,
                                            p1   = path.pool_1.address,
                                            p2   = path.pool_2.address,
                                            p3   = path.pool_3.address,
                                        );
                                    }
                                }

                                if let Some(filter_idx) = std::env::var("BOT_DEBUG_PATH_IDX")
                                    .ok()
                                    .and_then(|s| s.parse::<usize>().ok())
                                {
                                    if filter_idx == path_idx {
                                        info!(
                                            "Path idx={} pools: p1={} type1={:?} z1={} p2={} type2={:?} z2={} p3={} type3={:?} z3={}",
                                            path_idx,
                                            path.pool_1.address,
                                            path.pool_1.pool_type,
                                            path.zero_for_one_1,
                                            path.pool_2.address,
                                            path.pool_2.pool_type,
                                            path.zero_for_one_2,
                                            path.pool_3.address,
                                            path.pool_3.pool_type,
                                            path.zero_for_one_3
                                        );
                                        info!(
                                            "Path idx={} tokens: p1=({:?},{:?}) p2=({:?},{:?}) p3=({:?},{:?})",
                                            path_idx,
                                            path.pool_1.token0,
                                            path.pool_1.token1,
                                            path.pool_2.token0,
                                            path.pool_2.token1,
                                            path.pool_3.token0,
                                            path.pool_3.token1
                                        );
                                    }
                                }

                                // Check if profit meets threshold
                                if executor.check_profit_threshold(profit_u256, start_token, token_price_usd) {
                                    // opt.0 is already in raw atomic units (e.g. wei).
                                    let amount_in_raw = opt.0;
                                    // Build execution parameters
                                    let exec_params = ExecutionParams {
                                        path: path.clone(),
                                        amount_in: amount_in_raw,
                                        expected_profit: profit_u256,
                                        v2_router,
                                        algebra_router,
                                        start_token: start_token.clone(),
                                        base_fee: block.next_base_fee,
                                        token_price_usd,
                                    };

                                    // Build execution plan
                                    let mut executed_successfully = false;
                                    match executor.build_execution_tx(&exec_params) {
                                        Ok(plan) => {
                                            executor.log_execution(&exec_params, &plan);
                                            if let Some(bundler) = &bundler {
                                                // Slippage protection: min_out must be at least the
                                                // flash-loan repayment (principal + premium), otherwise
                                                // the transferFrom in the flash-loan callback reverts
                                                // with a bare revert (data:None) that's hard to debug.
                                                let expected_out = plan.amount_in + exec_params.expected_profit;
                                                let slippage_floor = expected_out * U256::from(95u64) / U256::from(100u64);

                                                let repay_amount = match &plan.flashloan_type {
                                                    Flashloan::AaveV3 => {
                                                        // 0.05% premium (5 bps)
                                                        plan.amount_in + (plan.amount_in * U256::from(5u64)) / U256::from(10_000u64)
                                                    }
                                                    Flashloan::UniswapV2 => {
                                                        let fee_bps = plan.path_params.first().map(|p| p.fee).unwrap_or(300);
                                                        plan.amount_in + (plan.amount_in * U256::from(fee_bps)) / U256::from(10_000u64 - fee_bps) + U256::one()
                                                    }
                                                    _ => plan.amount_in, // Balancer: 0 premium
                                                };

                                                let min_out = std::cmp::max(slippage_floor, repay_amount);
                                                match bundler
                                                    .order_tx(
                                                        plan.path_params.clone(),
                                                        plan.amount_in,
                                                        plan.flashloan_type.clone(),
                                                        plan.loan_pool,
                                                        plan.max_priority_fee,
                                                        plan.max_fee_per_gas,
                                                        min_out,
                                                    )
                                                    .await
                                                {
                                                    Ok(tx) => {
                                                        if config.execution.simulation_required {
                                                            // Pool state is fresh from block-start
                                                            // multicall.  eth_call is the final guard.
                                                            match bundler
                                                                .provider
                                                                .call(&tx.clone().into(), None)
                                                                .await
                                                            {
                                                                Ok(_) => {
                                                                    match bundler.send_tx(tx).await {
                                                                        Ok(hash) => {
                                                                            info!("✅ Executed tx: {:?}", hash);
                                                                            slippage_fail_count.remove(&path_idx);
                                                                            slippage_cooldown_until.remove(&path_idx);
                                                                            executed_successfully = true;
                                                                        }
                                                                        Err(e) => {
                                                                            let err_str = format!("{:?}", e);
                                                                            if err_str.to_lowercase().contains("reverted") {
                                                                                // On-chain revert despite passing eth_call.
                                                                                // Most common cause: front-run within the same block.
                                                                                // Apply the same slippage-cooldown so we don't keep
                                                                                // submitting the identical path until conditions change.
                                                                                info!("❌ On-chain revert idx={}: {:?}", path_idx, e);
                                                                                let fail_count = slippage_fail_count.entry(path_idx).or_insert(0);
                                                                                *fail_count = fail_count.saturating_add(1);
                                                                                if *fail_count >= SLIPPAGE_FAIL_THRESHOLD {
                                                                                    let until_block = current_block.saturating_add(SLIPPAGE_COOLDOWN_BLOCKS);
                                                                                    slippage_cooldown_until.insert(path_idx, until_block);
                                                                                    info!(
                                                                                        "[cooldown] idx={} on-chain revert {}x; skipping until block {}",
                                                                                        path_idx, *fail_count, until_block
                                                                                    );
                                                                                    slippage_fail_count.insert(path_idx, 0);
                                                                                }
                                                                            } else {
                                                                                info!("❌ Failed to send tx: {:?}", e);
                                                                            }
                                                                        }
                                                                    }
                                                                }
                                                                Err(e) => {
                                                                    info!("⚠️ Simulation failed for idx={}; trying next path: {:?}", path_idx, e);
                                                                    info!(
                                                                        "  sim_block={} min_out={} amount_in={} expected_profit={}",
                                                                        block.block_number,
                                                                        min_out,
                                                                        plan.amount_in,
                                                                        exec_params.expected_profit,
                                                                    );
                                                                    // Re-simulate the path off-chain to detect stale state.
                                                                    let resim = path.simulate_mixed_path_raw(
                                                                        plan.amount_in,
                                                                        &reserves_snapshot,
                                                                        &algebra_pools_map,
                                                                        &lfj_states_map,
                                                                    );
                                                                    info!(
                                                                        "  off-chain resim output={:?} (need >= {})",
                                                                        resim, min_out,
                                                                    );
                                                                    info!(
                                                                        "Simulation tx details: to={:?} from={:?} value={:?} gas={:?} data=0x{}",
                                                                        tx.to,
                                                                        tx.from,
                                                                        tx.value,
                                                                        tx.gas,
                                                                        hex::encode(tx.data.clone().unwrap_or_default())
                                                                    );

                                                                    // ── Auto-detect fee-on-transfer tokens ──
                                                                    let err_str = format!("{:?}", e).to_lowercase();
                                                                    if err_str.contains("slippage") {
                                                                        let fail_count = slippage_fail_count.entry(path_idx).or_insert(0);
                                                                        *fail_count = fail_count.saturating_add(1);
                                                                        if *fail_count >= SLIPPAGE_FAIL_THRESHOLD {
                                                                            let until_block = current_block.saturating_add(SLIPPAGE_COOLDOWN_BLOCKS);
                                                                            slippage_cooldown_until.insert(path_idx, until_block);
                                                                            info!(
                                                                                "[cooldown] idx={} slippage repeated {}x; skipping until block {}",
                                                                                path_idx,
                                                                                *fail_count,
                                                                                until_block
                                                                            );
                                                                            slippage_fail_count.insert(path_idx, 0);
                                                                        }
                                                                    }
                                                                    // K() = 0xa932492f is the Uniswap/Solidly V2 invariant
                                                                    // failure that fires when a fee-on-transfer token reduces
                                                                    // the pool's actual receipt, breaking the xy=k check.
                                                                    let is_fot_revert = err_str.contains("transfer amount exceeds balance")
                                                                        || err_str.contains("0xa932492f");  // K()
                                                                    if is_fot_revert {
                                                                        let path = &paths[path_idx];
                                                                        let start_addr = H160::from_str(&start_token.address).unwrap_or_default();
                                                                        // Collect intermediate tokens (everything that isn't the start token).
                                                                        let mut suspects: Vec<H160> = Vec::new();
                                                                        for hop in 0..path.nhop {
                                                                            let pool = path.get_pool(hop);
                                                                            if pool.token0 != start_addr && !suspects.contains(&pool.token0) {
                                                                                suspects.push(pool.token0);
                                                                            }
                                                                            if pool.token1 != start_addr && !suspects.contains(&pool.token1) {
                                                                                suspects.push(pool.token1);
                                                                            }
                                                                        }
                                                                        if let Ok(mut bl) = runtime_blacklist.write() {
                                                                            for token in &suspects {
                                                                                if bl.insert(*token) {
                                                                                    info!(
                                                                                        "🚫 Auto-blacklisted suspected FoT token: {:#x}",
                                                                                        token
                                                                                    );
                                                                                }
                                                                            }
                                                                        }
                                                                    }

                                                                    if std::env::var("BOT_DEBUG_TRACE").ok().as_deref() == Some("1") {
                                                                        let call = json!({
                                                                            "from": tx.from,
                                                                            "to": tx.to,
                                                                            "data": tx.data,
                                                                            "value": tx.value,
                                                                            "gas": tx.gas,
                                                                            "maxFeePerGas": tx.max_fee_per_gas,
                                                                            "maxPriorityFeePerGas": tx.max_priority_fee_per_gas,
                                                                        });
                                                                        let trace_opts = json!({"tracer": "callTracer", "timeout": "30s"});
                                                                        match bundler
                                                                            .provider
                                                                            .provider()
                                                                            .request::<_, serde_json::Value>(
                                                                                "debug_traceCall",
                                                                                (call, "latest", trace_opts),
                                                                            )
                                                                            .await
                                                                        {
                                                                            Ok(trace) => info!("debug_traceCall: {}", trace),
                                                                            Err(trace_err) => info!("debug_traceCall failed: {:?}", trace_err),
                                                                        }
                                                                    }
                                                                }
                                                            }
                                                        } else {
                                                            match bundler.send_tx(tx).await {
                                                                Ok(hash) => {
                                                                    info!("✅ Executed tx: {:?}", hash);
                                                                    slippage_fail_count.remove(&path_idx);
                                                                    slippage_cooldown_until.remove(&path_idx);
                                                                    stale_streak.remove(&path_idx);
                                                                    stale_backoff_until.remove(&path_idx);
                                                                    executed_successfully = true;
                                                                }
                                                                Err(e) => {
                                                                    let err_str = format!("{:?}", e);
                                                                    if err_str.to_lowercase().contains("reverted") {
                                                                        info!("❌ On-chain revert idx={}: {:?}", path_idx, e);
                                                                        let fail_count = slippage_fail_count.entry(path_idx).or_insert(0);
                                                                        *fail_count = fail_count.saturating_add(1);
                                                                        if *fail_count >= SLIPPAGE_FAIL_THRESHOLD {
                                                                            let until_block = current_block.saturating_add(SLIPPAGE_COOLDOWN_BLOCKS);
                                                                            slippage_cooldown_until.insert(path_idx, until_block);
                                                                            info!(
                                                                                "[cooldown] idx={} on-chain revert {}x; skipping until block {}",
                                                                                path_idx, *fail_count, until_block
                                                                            );
                                                                            slippage_fail_count.insert(path_idx, 0);
                                                                        }
                                                                    } else {
                                                                        info!("❌ Failed to send tx: {:?}", e);
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    }
                                                    Err(e) => info!("❌ Failed to build tx for execution: {:?}", e),
                                                }
                                            } else {
                                                // No bundler — treat as success to avoid retry loop
                                                executed_successfully = true;
                                            }
                                        }
                                        Err(e) => {
                                            info!("Failed to build execution plan: {:?}", e);
                                        }
                                    }

                                    // Only stop after successful execution; keep trying on simulation failure
                                    if executed_successfully {
                                        break;
                                    }
                                }
                            }
                        }
                    }

                    // ── Per-block summary ────────────────────────────────────────
                    let block_total_ms = block_start.elapsed().as_millis();
                    let scan_ms = scan_start.elapsed().as_millis();
                    info!(
                        "[block {}] CL:{:.0}ms V2:{:.0}ms scan:{:.0}ms total:{:.0}ms | hot CL:{} V2:{} paths:{} skip:{}",
                        block.block_number,
                        cl_refresh_ms,
                        v2_refresh_ms,
                        scan_ms,
                        block_total_ms,
                        algebra_pools_map.len(),
                        v2_pools_vec.len(),
                        paths.len(),
                        cl_pool_skip_set.len(),
                    );
                }
                Event::PendingTx(_) => {
                    // not using pending tx
                }
                Event::Log(_) => {
                    // not using logs
                }
            },
            Err(_) => {}
        }
    }
}

fn is_unknown_block_error(err: &anyhow::Error) -> bool {
    err.to_string().contains("Unknown block")
}

/// Lossless-ish U256 → f64 conversion.
///
/// `as_u128()` silently truncates values > 2^128 — sqrtPriceX96 can exceed
/// that for extreme token pairs (e.g. SHIB/ETH). This function splits the
/// U256 into high and low 128-bit halves to produce a correct f64 approximation.
fn u256_to_f64(v: U256) -> f64 {
    let lo = v.low_u128() as f64;
    let hi = (v >> 128).low_u128() as f64;
    hi * (2.0f64).powi(128) + lo
}

/// Return the USD price of one whole unit of `token`.
///
/// Priority:
///   1. Token IS the stable coin                   → $1.00
///   2. `price_pool` explicitly configured          → use that specific pool (logged)
///   3. Scan all V2 pools for token ↔ stable pair  (first match, logged)
///   4. Scan all V3CL/Algebra pools for token ↔ stable pair (first match, logged)
///   5. Aave/Chainlink oracle cache (refreshed every 300 blocks) (logged)
///   6. Return 0.0 — no price source found (logged)
fn get_token_price_usd(
    config: &Config,
    pools: &HashMap<H160, AnyPool>,
    reserves: &HashMap<H160, Reserve>,
    token: &TokenConfig,
    oracle_prices: &HashMap<H160, f64>,
) -> f64 {
    use crate::algebra::AlgebraPoolFull;
    use std::str::FromStr;

    let stable_addr = match config.gas.stable_address.as_deref()
        .and_then(|s| H160::from_str(s).ok())
    {
        Some(a) if a != H160::zero() => a,
        _ => return 0.0,
    };
    let token_addr = match H160::from_str(&token.address) {
        Ok(a) => a,
        Err(_) => return 0.0,
    };

    // 1. Token IS the stable → $1.00
    if token_addr == stable_addr {
        return 1.0;
    }

    // Shared helper: sqrtPriceX96 → USD price of token relative to stable
    let cl_price = |alg: &AlgebraPoolFull, tok_is_t0: bool| -> f64 {
        if alg.sqrt_price_x96.is_zero() { return 0.0; }
        let q96 = U256::from(1u64) << 96;
        let p_f = u256_to_f64(alg.sqrt_price_x96) / u256_to_f64(q96);
        let price_raw = p_f * p_f;
        let adj = (10.0f64).powi(alg.decimals0 as i32 - alg.decimals1 as i32);
        let p = price_raw * adj;
        if tok_is_t0 { p } else if p == 0.0 { 0.0 } else { 1.0 / p }
    };

    // 2. Explicit price_pool → fastest path, always preferred
    if let Some(pp_str) = token.price_pool.as_deref() {
        if let Ok(pp_addr) = H160::from_str(pp_str) {
            if let Some(pool) = pools.get(&pp_addr) {
                let tok_is_t0 = pool.token0 == token_addr;
                let price: f64 = match &pool.pool_type {
                    PoolType::V2(v2) => {
                        reserves.get(&pp_addr).map_or(0.0, |r| {
                            UniswapV2Simulator::reserves_to_price(
                                r.reserve0, r.reserve1,
                                v2.decimals0, v2.decimals1,
                                tok_is_t0,
                            )
                        })
                    }
                    PoolType::Algebra(alg) | PoolType::UniswapV3CL(alg) => cl_price(alg, tok_is_t0),
                    PoolType::LFJ(_) => 0.0,
                };
                if price > 0.0 && price.is_finite() {
                    debug!("{} ${:.4}  ← price_pool {:?}", token.symbol, price, pp_addr);
                    return price;
                }
            }
        }
    }

    // 3. Scan V2 pools for any token ↔ stable pair
    for (addr, pool) in pools.iter() {
        if let PoolType::V2(v2) = &pool.pool_type {
            let tok_is_t0 = pool.token0 == token_addr && pool.token1 == stable_addr;
            let tok_is_t1 = pool.token1 == token_addr && pool.token0 == stable_addr;
            if !tok_is_t0 && !tok_is_t1 { continue; }
            if let Some(r) = reserves.get(addr) {
                let price = UniswapV2Simulator::reserves_to_price(
                    r.reserve0, r.reserve1,
                    v2.decimals0, v2.decimals1,
                    tok_is_t0,
                );
                if price > 0.0 && price.is_finite() {
                    debug!("{} ${:.4}  ← V2 pool {:?}", token.symbol, price, addr);
                    return price;
                }
            }
        }
    }

    // 4. Scan V3CL/Algebra pools for any token ↔ stable pair
    for (addr, pool) in pools.iter() {
        match &pool.pool_type {
            PoolType::Algebra(alg) | PoolType::UniswapV3CL(alg) => {
                let tok_is_t0 = pool.token0 == token_addr && pool.token1 == stable_addr;
                let tok_is_t1 = pool.token1 == token_addr && pool.token0 == stable_addr;
                if !tok_is_t0 && !tok_is_t1 { continue; }
                let price = cl_price(alg, tok_is_t0);
                if price > 0.0 && price.is_finite() {
                    debug!("{} ${:.4}  ← V3CL pool {:?}", token.symbol, price, addr);
                    return price;
                }
            }
            _ => {}
        }
    }

    debug!("{} price unknown — no token/stable pool loaded; checking oracle cache", token.symbol);

    // 5. Aave/Chainlink oracle cache — last resort before giving up
    if let Some(&p) = oracle_prices.get(&token_addr) {
        if p > 0.0 {
            debug!("{} ${:.4}  ← Chainlink/Aave oracle cache", token.symbol, p);
            return p;
        }
    }

    debug!("{} price genuinely unknown — returning 0.0 (profit will be in token units)", token.symbol);
    0.0
}

fn estimate_gas_cost_in_start_token(
    config: &Config,
    pools: &HashMap<H160, AnyPool>,
    reserves: &HashMap<H160, Reserve>,
    gas_cost_in_wei: U256,
    start: &TokenConfig,
) -> U256 {
    if !config.gas.enabled {
        return U256::zero();
    }

    let gas_pool = match &config.gas.pool_address {
        Some(addr) => H160::from_str(addr).ok(),
        None => {
             // Can't estimate without gas pool (usually Native/Stable pool)
             return U256::zero();
        },
    };
    let gas_pool = match gas_pool {
        Some(addr) => addr,
        None => {
            info!("Gas config missing pool_address.");
            return U256::zero();
        }
    };

    let pool = match pools.get(&gas_pool) {
        Some(p) => p,
        None => {
            // It's possible gas pool is not in the arbitrage path pools set.
            // But we should have loaded it if it was in config.
            // If main loader didn't load it (because it wasn't relevant to paths?), we might miss it.
            // But usually gas pool IS relevant or loaded separately.
            info!("Gas pool not found in loaded pools.");
            return U256::zero();
        }
    };

    let token_is_token0 = config.gas.token_is_token0.unwrap_or(false);
    
    let gas_token_price_in_stable = match &pool.pool_type {
        PoolType::V2(v2) => {
            let reserve = match reserves.get(&gas_pool) {
                Some(r) => r,
                None => {
                    info!("Gas V2 pool reserves not found.");
                    return U256::zero();
                }
            };
            UniswapV2Simulator::reserves_to_price(
                reserve.reserve0,
                reserve.reserve1,
                v2.decimals0,
                v2.decimals1,
                token_is_token0,
            )
        },
        PoolType::Algebra(alg) | PoolType::UniswapV3CL(alg) => {
            // Calculate price from sqrtPriceX96
            // Price = (sqrtPrice / 2^96)^2
            // Adjusted for decimals: price = price * 10^(decimals0 - decimals1)
            let sqrt_p = alg.sqrt_price_x96;
            if sqrt_p.is_zero() { return U256::zero(); }
            
            let q96 = U256::from(1) << 96;
            // Use f64 for approximation — safe conversion avoids u128 truncation
            let p_f = u256_to_f64(sqrt_p) / u256_to_f64(q96);
            let price_raw = p_f * p_f;
            
            // Decimal adjustment
            let d0 = alg.decimals0 as i32;
            let d1 = alg.decimals1 as i32;
            let adj = (10.0f64).powi(d0 - d1);
            
            let price = price_raw * adj;
            
            if token_is_token0 {
                price
            } else {
                if price == 0.0 { 0.0 } else { 1.0 / price }
            }
        }
        PoolType::LFJ(_) => {
            // LFJ pools are unlikely to be the gas oracle; return 0.
            return U256::zero();
        }
    };

    let gas_cost_in_native = u256_to_f64(gas_cost_in_wei) / u256_to_f64(*WEI);

    let gas_cost_in_stable = gas_token_price_in_stable * gas_cost_in_native;
    let stable_decimals = config.gas.stable_decimals.unwrap_or(start.decimals) as i32;
    // Cap at u64 max for safety, though U256 can hold more
    let gas_cost_in_stable_u64 = (gas_cost_in_stable * ((10 as f64).powi(stable_decimals))) as u64;
    let gas_cost_in_stable = U256::from(gas_cost_in_stable_u64);

    if start.address == config.gas.stable_address.clone().unwrap_or_default() {
        return gas_cost_in_stable;
    }

    // Start token is not the stable — convert stable gas cost → start token units.
    // We need the start-token price in stable. Use the same gas pool if it pairs
    // start_token/stable; otherwise fall back to a simple ratio from any V2 reserve
    // that prices start_token in stable.
    // Strategy: look for a V2 pool whose token0 or token1 is start_token and whose
    // other token is stable; compute price and invert gas_cost_in_stable.
    let stable_addr = match config.gas.stable_address.as_deref().and_then(|s| H160::from_str(s).ok()) {
        Some(a) => a,
        None => return gas_cost_in_stable, // can't convert — return as-is (wrong units, but safe)
    };
    let start_addr = match H160::from_str(&start.address) {
        Ok(a) => a,
        Err(_) => return gas_cost_in_stable,
    };

    // Search loaded V2 reserves for a pool that bridges start_token<→stable.
    for (addr, reserve) in reserves.iter() {
        if let Some(pool) = pools.get(addr) {
            if let PoolType::V2(v2) = &pool.pool_type {
                let t0 = pool.token0;
                let t1 = pool.token1;
                let (tok_is_t0, stable_present) = if t0 == start_addr && t1 == stable_addr {
                    (true, true)
                } else if t1 == start_addr && t0 == stable_addr {
                    (false, true)
                } else {
                    (false, false)
                };
                if !stable_present { continue; }

                // price = start_token per stable (how many start_token units equal 1 stable unit)
                let price_start_per_stable = UniswapV2Simulator::reserves_to_price(
                    reserve.reserve0,
                    reserve.reserve1,
                    v2.decimals0,
                    v2.decimals1,
                    !tok_is_t0, // we want stable→start direction
                );
                if price_start_per_stable <= 0.0 || !price_start_per_stable.is_finite() {
                    continue;
                }
                // gas_cost_in_stable is in atomic stable units (e.g. 1e6 per USDC token).
                // Convert to start-token atomic units.
                let stable_dec = config.gas.stable_decimals.unwrap_or(6) as i32;
                let start_dec = start.decimals as i32;
                let gas_stable_f64 = u256_to_f64(gas_cost_in_stable);
                // Normalize to whole stable tokens, apply conversion, re-scale to start atoms.
                let gas_in_start_atoms = gas_stable_f64
                    / (10.0f64).powi(stable_dec)
                    * price_start_per_stable
                    * (10.0f64).powi(start_dec);
                return U256::from(gas_in_start_atoms as u64);
            }
        }
    }

    // No bridging pool found — return 0 rather than a nonsensical stable amount.
    // This is safe (never rejects a good opportunity) but means gas isn't charged.
    U256::zero()
}

fn estimate_gas_units_for_path(path: &crate::paths::ArbPath) -> u64 {
    let base_gas: u64 = 350_000;
    let mut per_hop: u64 = 0;
    for hop in 0..path.nhop {
        let pool = path.get_pool(hop);
        per_hop += match &pool.pool_type {
            PoolType::Algebra(_) | PoolType::UniswapV3CL(_) => 300_000,
            PoolType::LFJ(_) => 280_000,
            _ => 180_000,
        };
    }
    base_gas + per_hop
}
