use chrono::Utc;
use ethers::{
    providers::{Middleware, Provider, Ws},
    types::{H160, U256},
};
use log::{info, warn};
use serde_json::json;
use std::{collections::HashMap, io::Write as _, str::FromStr, sync::Arc};
use tokio::sync::broadcast::Sender;

use crate::algebra::{fetch_algebra_states, load_algebra_pools_from_v3_csv};
use crate::uniswapv3cl::{fetch_full_uniswapv3cl_pools, fetch_uniswapv3cl_states, load_uniswapv3cl_pools_from_csv};
use crate::bundler::Bundler;
use crate::config::{Config, TokenConfig};
use crate::constants::{get_blacklist_tokens, Env, WEI};
use crate::executor::{ExecutionParams, Executor};
use crate::multi::{batch_get_uniswap_v2_reserves, Reserve};
use crate::paths::{generate_triangular_paths_mixed, generate_cross_dex_paths};
use crate::pools::{load_all_pools_from_v2, AnyPool, PoolType};
use crate::simulator::UniswapV2Simulator;
use crate::streams::Event;
use crate::utils::get_touched_pool_reserves;
use dashmap::DashMap;

pub async fn event_handler(
    provider: Arc<Provider<Ws>>,
    event_sender: Sender<Event>,
    config: Config,
) {
    let env = Env::from_config(&config);
    let debug_spreads = std::env::var("BOT_DEBUG_SPREADS").ok().as_deref() == Some("1");
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
    //   2. Seed the per-block DashMap so the event loop doesn't need a
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
                }
                Err(e) => {
                    info!("Failed to load Algebra pools: {:?}", e);
                }
            }
        }
    }

    // ── UniswapV3 CL pools ──────────────────────────────────────────────
    let mut v3cl_pools_full = Vec::new();
    let mut v3cl_pools_info = Vec::new();

    if let Some(rv3) = config.dexes.uniswapv3cl.as_ref() {
        if rv3.enabled {
            match load_uniswapv3cl_pools_from_csv(cache_dir) {
                Ok(pools) => {
                    info!("Loading full data for {} UniswapV3CL pools...", pools.len());
                    v3cl_pools_info = pools;
                    v3cl_pools_full = fetch_full_uniswapv3cl_pools(
                        provider.clone(),
                        &v3cl_pools_info,
                    ).await;
                    let before_v3cl = v3cl_pools_full.len();
                    v3cl_pools_full.retain(|p| !p.liquidity.is_zero());
                    info!(
                        "Loaded {} UniswapV3CL pools fully ({} with liquidity > 0)",
                        before_v3cl,
                        v3cl_pools_full.len()
                    );
                }
                Err(e) => {
                    info!("Failed to load UniswapV3CL pools (cache may not exist yet): {:?}", e);
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
            start_address
        );
        let cross_paths = generate_cross_dex_paths(
            &pools_vec,
            &algebra_pools_full,
            &v3cl_pools_full,
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
        }
    }

    // Seed the DashMap from the reserves we already fetched during pool filtering.
    // Only keep entries relevant to the active (path-participating) pools.
    let reserves = DashMap::new();
    for pool in &v2_pools_vec {
        if let Some(reserve) = initial_reserves.get(&pool.address) {
            reserves.insert(pool.address, reserve.clone());
        }
    }

    if debug_spreads {
        let reserves_snapshot: HashMap<H160, Reserve> =
            reserves.iter().map(|r| (*r.key(), r.value().clone())).collect();

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
                match path.simulate_mixed_path(U256::from(1u64), &reserves_snapshot, &algebra_pools_map) {
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
    loop {
        match event_receiver.recv().await {
            Ok(event) => match event {
                Event::Block(block) => {
                    info!("{:?}", block);

                    // 1. Update Algebra states
                    if !algebra_pools_info.is_empty() {
                        let states = fetch_algebra_states(provider.clone(), &algebra_pools_info).await;
                        for (addr, state) in states {
                            if let Some(pool) = algebra_pools_map.get_mut(&addr) {
                                pool.sqrt_price_x96 = state.sqrt_price_x96;
                                pool.liquidity = state.liquidity;
                                pool.tick = state.tick;
                                // Update dynamic fee (lastFee from globalState).
                                // Keep existing fee if state returned 0 (no dynamic update).
                                if state.fee > 0 {
                                    pool.fee = state.fee;
                                }
                            }
                        }
                    }

                    // 1b. Update UniswapV3CL states
                    if !v3cl_pools_info.is_empty() {
                        let states = fetch_uniswapv3cl_states(provider.clone(), &v3cl_pools_info).await;
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

                    // 2. Update V2 reserves
                    let touched_reserves =
                        match get_touched_pool_reserves(provider.clone(), block.block_number).await
                        {
                            Ok(response) => response,
                            Err(e) => {
                                if !is_unknown_block_error(&e) {
                                    info!("Error from get_touched_pool_reserves: {:?}", e);
                                }
                                HashMap::new()
                            }
                        };
                    let mut touched_pools = Vec::new();
                    for (address, reserve) in touched_reserves.into_iter() {
                        if reserves.contains_key(&address) {
                            reserves.insert(address, reserve);
                            touched_pools.push(address);
                        }
                    }
                    if !touched_pools.is_empty() {
                        info!("Touched {} V2 pools", touched_pools.len());
                    } else {
                        info!("No touched V2 pools; evaluating all paths this block.");
                    }

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

                        let mut spreads = HashMap::new();
                        
                        // Create snapshot of V2 reserves for simulation
                        let reserves_snapshot: HashMap<H160, Reserve> =
                                    reserves.iter().map(|r| (*r.key(), r.value().clone())).collect();

                        let evaluate_all = touched_pools.is_empty();
                        for &idx in token_path_indices {
                            let path = &paths[idx];
                            
                            // Check if path is touched either by V2 update OR involves Algebra (which we update every block)
                            // Ideally track touched Algebra pools too. For now assume Algebra updates might affect any path using them.
                            // Or just simulate all paths if Algebra is enabled? No, too slow.
                            // Let's assume we simulate if any pool in path is touched.
                            // For Algebra, we updated all of them, so we should check if values CHANGED.
                            // But here we didn't track changes explicitly.
                            // Let's assume we simulate if path has Algebra pool OR touched V2 pool.
                            
                            let has_algebra = path.get_pool(0).pool_type.is_cl()
                                || path.get_pool(1).pool_type.is_cl()
                                || path.get_pool(2).pool_type.is_cl();
                            
                            let is_touched_v2 = touched_pools
                                .iter()
                                .any(|pool| path.has_pool(pool));

                            if force_seeded_v2
                                && !seed_addrs.is_empty()
                                && !seed_addrs.iter().all(|addr| path.has_pool(addr))
                            {
                                continue;
                            }

                            if evaluate_all || has_algebra || is_touched_v2 {
                                let one_token_in = U256::from(1);

                                let simulated = path.simulate_mixed_path(one_token_in, &reserves_snapshot, &algebra_pools_map);

                                match simulated {
                                    Some(price_quote) => {
                                        // Use pre-computed decimal multiplier
                                        let one_token_in_scaled = one_token_in * decimal_multipliers[&start_address];

                                        // Dust-pool sanity cap: skip paths where output exceeds the cap.
                                        // Scale: 10 = 1× (break even), 20 = 2×, 50 = 5×.
                                        // Real arb returns ~1.001–1.02×; dust pools return ~235,000×.
                                        let max_pct_num = config.chain.max_profit_pct as u64;
                                        if price_quote * U256::from(10) > one_token_in_scaled * U256::from(max_pct_num) {
                                            continue;
                                        }

                                        let _out = price_quote.as_u128() as i128;
                                        let _in = one_token_in_scaled.as_u128() as i128;
                                        let spread = _out - _in;

                                        if spread > 0 {
                                            spreads.insert(idx, spread);
                                        }
                                    }
                                    None => {}
                                }
                            }
                        }

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

                        // Estimate gas cost in start token units using configured gas pricing pool.
                        let gas_cost_in_token = estimate_gas_cost_in_start_token(
                            &config,
                            &active_pools, 
                            &reserves_snapshot,
                            block.next_base_fee,
                            start_token,
                        );

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
                            let path = &paths[path_idx];
                            
                            let opt = path.optimize_amount_in_mixed(
                                U256::zero(), // ceiling auto-detected by exponential probe
                                &reserves_snapshot,
                                &algebra_pools_map,
                            );
                            
                            // Deduct Aave flashloan premium (5 bps on borrowed principal).
                            // opt.0 is the borrow amount in whole tokens; multiply by the
                            // token's decimal unit to get raw units, then apply 5/10000.
                            let aave_fee_raw =
                                if config.execution.default_flashloan_provider == "AaveV3" {
                                    opt.0 * decimal_multipliers[&start_address]
                                        * U256::from(5u64)
                                        / U256::from(10000u64)
                                } else {
                                    U256::zero()
                                };

                            // Use U256 saturating subtraction to avoid i128 cast overflow.
                            let excess_profit = opt
                                .1
                                .saturating_sub(gas_cost_in_token)
                                .saturating_sub(aave_fee_raw);

                            if excess_profit > U256::zero() {
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

                                info!(
                                    "Profitable path found: token={} idx={} profit={} {} amount_in={} tokens",
                                    start_token.symbol,
                                    path_idx,
                                    profit_display,
                                    start_token.symbol,
                                    opt.0
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
                                            "{ts} | block={block} | token={tok} | idx={idx} | profit={profit} {tok} | amount_in={amt} | p1={p1} | p2={p2} | p3={p3}",
                                            ts   = Utc::now().format("%Y-%m-%dT%H:%M:%SZ"),
                                            block = block.block_number,
                                            tok  = start_token.symbol,
                                            idx  = path_idx,
                                            profit = profit_display,
                                            amt  = opt.0,
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
                                if executor.check_profit_threshold(profit_u256, start_token) {
                                    // opt.0 is in whole tokens; convert to raw atoms so the
                                    // flash-loan contract receives the correct borrow amount.
                                    let amount_in_raw = opt.0 * decimal_multipliers[&start_address];
                                    // Build execution parameters
                                    let exec_params = ExecutionParams {
                                        path: path.clone(),
                                        amount_in: amount_in_raw,
                                        expected_profit: profit_u256,
                                        v2_router,
                                        algebra_router,
                                        start_token: start_token.clone(),
                                        base_fee: block.next_base_fee,
                                    };

                                    // Build execution plan
                                    let mut executed_successfully = false;
                                    match executor.build_execution_tx(&exec_params) {
                                        Ok(plan) => {
                                            executor.log_execution(&exec_params, &plan);
                                            if let Some(bundler) = &bundler {
                                                match bundler
                                                    .order_tx(
                                                        plan.path_params.clone(),
                                                        plan.amount_in,
                                                        plan.flashloan_type.clone(),
                                                        plan.loan_pool,
                                                        plan.max_priority_fee,
                                                        plan.max_fee_per_gas,
                                                    )
                                                    .await
                                                {
                                                    Ok(tx) => {
                                                        if config.execution.simulation_required {
                                                            match bundler
                                                                .provider
                                                                .call(&tx.clone().into(), None)
                                                                .await
                                                            {
                                                                Ok(_) => {
                                                                    match bundler.send_tx(tx).await {
                                                                        Ok(hash) => {
                                                                            info!("✅ Executed tx: {:?}", hash);
                                                                            executed_successfully = true;
                                                                        }
                                                                        Err(e) => info!("❌ Failed to send tx: {:?}", e),
                                                                    }
                                                                }
                                                                Err(e) => {
                                                                    info!("⚠️ Simulation failed for idx={}; trying next path: {:?}", path_idx, e);
                                                                    info!(
                                                                        "Simulation tx details: to={:?} from={:?} value={:?} gas={:?} data=0x{}",
                                                                        tx.to,
                                                                        tx.from,
                                                                        tx.value,
                                                                        tx.gas,
                                                                        hex::encode(tx.data.clone().unwrap_or_default())
                                                                    );

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
                                                                    executed_successfully = true;
                                                                }
                                                                Err(e) => info!("❌ Failed to send tx: {:?}", e),
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

fn estimate_gas_cost_in_start_token(
    config: &Config,
    pools: &HashMap<H160, AnyPool>,
    reserves: &HashMap<H160, Reserve>,
    base_fee: U256,
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
            // Use f64 for approximation
            let p_f = (sqrt_p.as_u128() as f64) / (q96.as_u128() as f64);
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
    };

    let estimated_gas_usage = U256::from(config.execution.estimated_gas);
    // Rest of calculation
    let gas_cost_in_wei = base_fee * estimated_gas_usage;
    let gas_cost_in_native =
        (gas_cost_in_wei.as_u64() as f64) / ((*WEI).as_u64() as f64);

    let gas_cost_in_stable = gas_token_price_in_stable * gas_cost_in_native;
    let stable_decimals = config.gas.stable_decimals.unwrap_or(start.decimals) as i32;
    // Cap at u64 max for safety, though U256 can hold more
    let gas_cost_in_stable_u64 = (gas_cost_in_stable * ((10 as f64).powi(stable_decimals))) as u64;
    let gas_cost_in_stable = U256::from(gas_cost_in_stable_u64);

    if start.address == config.gas.stable_address.clone().unwrap_or_default() {
        return gas_cost_in_stable;
    }

    // If start token is not the stable token, we should convert...
    // But currently returning stable cost as approximation if we can't convert.
    // Ideally we recursively find price for start token.
    
    gas_cost_in_stable
}
