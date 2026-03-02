/// Searcher pipeline worker — receives PoolsTouched events, updates rates,
/// evaluates affected paths with rayon parallel iteration, and sends the
/// most promising ArbPath downstream.
///
/// Pipeline position: PoolsTouched → [Searcher] → ArbPath

use alloy::primitives::Address;
use rand::seq::SliceRandom;
use rayon::prelude::*;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tokio::sync::mpsc;

use crate::cache_v2::PathRateCache;
use crate::calculation::rates::{RateEstimator, RATE_SCALE};
use crate::gas_station::GasStation;
use crate::pipeline_events::{ArbPath, PoolsTouched};
use crate::state_db::{BlockStateDB, V2_RESERVES_SLOT};
use crate::swap_types::{PoolProtocol, SwapPath};

/// Pool metadata needed for rate updates.
#[derive(Debug, Clone)]
pub struct PoolMeta {
    pub address: Address,
    pub token0: Address,
    pub token1: Address,
    pub decimals0: u8,
    pub decimals1: u8,
    pub is_v3: bool,
    pub fee: u32,
    pub tick_spacing: i32,
    pub protocol: crate::swap_types::PoolProtocol,
}

/// Searcher configuration.
pub struct SearcherConfig {
    /// Maximum number of paths to evaluate per block.
    pub max_paths_per_block: usize,
    /// Rough gas estimate per path used for pre-screening (gas units).
    /// Only affects the ArbPath.gas_estimate field pushed downstream;
    /// the simulator re-measures actual gas via REVM.
    pub searcher_gas_estimate: u64,
    /// Maximum number of paths to forward to the simulator per evaluation.
    /// Paths are sorted by estimated profit (descending) so only the most
    /// promising candidates reach REVM.  Keeping this small ensures simulations
    /// complete within a few BSC blocks and arbs reach TxSender while fresh.
    /// Rule-of-thumb: (block_time_ms / revm_ms_per_path) * num_simulators.
    /// BSC ≈ 500ms / ~180ms × 8 workers ≈ 22 per block; 128 covers ~3 blocks.
    pub max_sim_paths: usize,
}

impl Default for SearcherConfig {
    fn default() -> Self {
        Self {
            max_paths_per_block: 50_000,
            searcher_gas_estimate: 250_000,
            // BSC ~400ms blocks, REVM ~185ms/path, 8 workers → 8*(400/185)=17 paths/block
            // sustainable throughput.  Top-16 sorted by estimated profit ensures
            // simulators finish within one block and arbs reach TxSender fresh.
            max_sim_paths: 16,
        }
    }
}

/// Searcher worker.
pub struct SearcherWorker {
    /// HTTP RPC URL used for the startup reserve prefetch.
    pub rpc_url: String,
    pub config: SearcherConfig,
    pub state_db: Arc<RwLock<BlockStateDB>>,
    pub estimator: Arc<RwLock<RateEstimator>>,
    pub gas_station: Arc<GasStation>,
    pub rate_cache: Arc<PathRateCache>,
    /// All known paths, indexed by the pools they contain.
    /// pool_address → [path_indices]
    pub pool_to_paths: HashMap<Address, Vec<usize>>,
    /// All paths.
    pub all_paths: Vec<SwapPath>,
    /// Pool metadata for rate updates.
    pub pool_meta: HashMap<Address, PoolMeta>,
}

impl SearcherWorker {
    pub fn new(
        rpc_url: String,
        config: SearcherConfig,
        state_db: Arc<RwLock<BlockStateDB>>,
        estimator: Arc<RwLock<RateEstimator>>,
        gas_station: Arc<GasStation>,
        rate_cache: Arc<PathRateCache>,
    ) -> Self {
        Self {
            rpc_url,
            config,
            state_db,
            estimator,
            gas_station,
            rate_cache,
            pool_to_paths: HashMap::new(),
            all_paths: Vec::new(),
            pool_meta: HashMap::new(),
        }
    }

    /// Register all paths and build the pool→paths index.
    pub fn register_paths(&mut self, paths: Vec<SwapPath>) {
        self.all_paths = paths;
        self.pool_to_paths.clear();

        for (idx, path) in self.all_paths.iter().enumerate() {
            for step in &path.steps {
                self.pool_to_paths
                    .entry(step.pool_address)
                    .or_insert_with(Vec::new)
                    .push(idx);
            }
        }
    }

    /// Register pool metadata.
    pub fn register_pool_meta(&mut self, meta: Vec<PoolMeta>) {
        for m in meta {
            self.pool_meta.insert(m.address, m);
        }
    }

    /// Run the searcher worker.
    pub async fn run(
        &self,
        mut pools_rx: mpsc::Receiver<PoolsTouched>,
        arb_tx: mpsc::Sender<ArbPath>,
    ) {
        log::info!("[Searcher] Worker started with {} paths", self.all_paths.len());

        // Prefetch all pool reserves from RPC before the first block arrives.
        self.seed_initial_rates().await;

        let mut latest_block: u64 = 0;

        // Carry-forward: if new PoolsTouched events arrived while rayon was evaluating,
        // we drain them (apply pool-state updates), then skip sending stale results and
        // re-evaluate immediately with the freshest merged-pool set — no extra recv() wait.
        let mut carry_forward: Option<(std::collections::HashSet<Address>, u64)> = None;

        'outer: loop {
            // Build initial merged-pool set from carry-forward state (no recv wait)
            // or a fresh event from the channel.
            let (mut merged_pools, mut final_block, is_mempool_event) =
                if let Some((pools, block)) = carry_forward.take() {
                    (pools, block, false)
                } else {
                    let event = match pools_rx.recv().await {
                        Some(e) => e,
                        None => break 'outer,
                    };
                    let is_mempool = event.block_number == 0;
                    let block = if is_mempool {
                        latest_block.saturating_add(1)
                    } else {
                        latest_block = latest_block.max(event.block_number);
                        event.block_number
                    };
                    self.update_rates_for_pools(&event.touched_pools);
                    let merged: std::collections::HashSet<Address> =
                        event.touched_pools.iter().cloned().collect();
                    (merged, block, is_mempool)
                };

            // Pre-evaluation drain: absorb any events that queued while we were last
            // evaluating.  Apply pool-state updates for each so state stays accurate.
            let mut pre_drained = 0u32;
            loop {
                match pools_rx.try_recv() {
                    Ok(next) => {
                        pre_drained += 1;
                        let next_block = if next.block_number == 0 {
                            latest_block.saturating_add(1)
                        } else {
                            latest_block = latest_block.max(next.block_number);
                            next.block_number
                        };
                        self.update_rates_for_pools(&next.touched_pools);
                        for p in next.touched_pools {
                            merged_pools.insert(p);
                        }
                        final_block = final_block.max(next_block);
                    }
                    Err(_) => break,
                }
            }
            if pre_drained > 0 {
                log::info!(
                    "[Searcher] Block {final_block}: drained {pre_drained} queued events, evaluating merged set of {} pools",
                    merged_pools.len()
                );
            }

            let merged_vec: Vec<Address> = merged_pools.into_iter().collect();

            // 2. Collect affected path indices (deduped) from merged pool set
            let affected_indices = self.get_affected_paths(&merged_vec, is_mempool_event);

            if affected_indices.is_empty() {
                continue 'outer;
            }

            // 3. Evaluate affected paths in parallel with rayon
            let results = self.evaluate_paths_parallel(&affected_indices, final_block);

            // Post-evaluation freshness check: rayon evaluation can take 100-500ms.
            // If new PoolsTouched events arrived during that time, our results are for
            // a stale block.  Drain those events, apply pool-state updates, then carry
            // the fresh merged-pool set forward and re-evaluate instead of sending stale paths.
            let pre_final = final_block;
            let mut post_merged: std::collections::HashSet<Address> = std::collections::HashSet::new();
            let mut post_drained = 0u32;
            loop {
                match pools_rx.try_recv() {
                    Ok(next) => {
                        post_drained += 1;
                        let nb = if next.block_number == 0 {
                            latest_block.saturating_add(1)
                        } else {
                            latest_block = latest_block.max(next.block_number);
                            next.block_number
                        };
                        self.update_rates_for_pools(&next.touched_pools);
                        for p in next.touched_pools {
                            post_merged.insert(p);
                        }
                        final_block = final_block.max(nb);
                    }
                    Err(_) => break,
                }
            }
            if post_drained > 0 && final_block > pre_final {
                // Newer blocks arrived while evaluating — discard stale results and
                // re-evaluate immediately with the FULL merged state.
                // Union the original block's pools with post-drain pools so paths
                // that involve the original block's pools are not skipped on re-eval.
                let mut all_pools: std::collections::HashSet<Address> =
                    merged_vec.iter().cloned().collect();
                all_pools.extend(post_merged);
                log::info!(
                    "[Searcher] Block {pre_final}: eval outdated by {post_drained} events → block {final_block}, re-evaluating {} pools",
                    all_pools.len()
                );
                carry_forward = Some((all_pools, final_block));
                continue 'outer;
            }

            // 4. Send the top-N profitable paths (sorted by estimated profit).
            // Capping here ensures simulators finish within a few blocks so arbs
            // reach TxSender while still fresh.  Results are already sorted DESC.
            let cap = self.config.max_sim_paths;
            let capped = results.len().min(cap);
            if results.len() > cap {
                log::info!(
                    "[Searcher] Block {final_block}: capping simulator send {}->{cap} (top profit paths)",
                    results.len()
                );
            }
            let mut sent = 0usize;
            for arb in results.into_iter().take(capped) {
                if arb_tx.send(arb).await.is_err() {
                    log::error!("[Searcher] Channel closed");
                    return;
                }
                sent += 1;
            }

            if sent > 0 {
                log::info!("[Searcher] Block {final_block}: sent {sent} arb paths");
            }
        }

        log::info!("[Searcher] Channel closed, shutting down");
    }

    /// One-time startup: prefetch slot-8 reserves for all V2/Aerodrome pools via batched
    /// eth_getStorageAt, then seed initial rates. V3 and exotic pools get a 1:1 pass-through
    /// so paths containing them aren't silently zero-filtered on the first block.
    async fn seed_initial_rates(&self) {
        use serde_json::Value;

        let total = self.pool_meta.len();
        if total == 0 || self.rpc_url.is_empty() {
            return;
        }
        log::info!("[Searcher] Startup reserve seed: {} pools…", total);
        let t0 = std::time::Instant::now();

        // Partition: slot-8 (V2-style, including Aerodrome) vs passthrough (V3 + exotic).
        let mut slot8_pools: Vec<Address> = Vec::new();
        let mut passthrough_pools: Vec<Address> = Vec::new();
        for (addr, meta) in &self.pool_meta {
            if meta.is_v3
                || matches!(
                    meta.protocol,
                    PoolProtocol::BalancerV2
                        | PoolProtocol::CurveTwoCrypto
                        | PoolProtocol::CurveTriCrypto
                        | PoolProtocol::MaverickV2
                )
            {
                passthrough_pools.push(*addr);
            } else {
                slot8_pools.push(*addr);
            }
        }

        // Seed V3 pools with actual spot rates (from prefetched sqrtPriceX96),
        // exotic pools with neutral 1:1 pass-through.
        {
            let db = self.state_db.read().unwrap();
            let mut est = self.estimator.write().unwrap();
            let mut spot_ok = 0usize;
            let mut spot_fallback = 0usize;
            for addr in &passthrough_pools {
                if let Some(meta) = self.pool_meta.get(addr) {
                    if meta.is_v3 {
                        // Try actual spot rate from sqrtPriceX96 first
                        let before = est.pool_count();
                        est.update_v3_spot_rate(&db, addr, meta.decimals0, meta.decimals1, meta.fee);
                        if est.pool_count() > before {
                            spot_ok += 1;
                        } else if meta.fee > 0 {
                            est.update_v3_passthrough_with_fee(addr, meta.fee);
                            spot_fallback += 1;
                        } else {
                            est.update_exotic_passthrough(addr);
                            spot_fallback += 1;
                        }
                    } else {
                        est.update_exotic_passthrough(addr);
                    }
                } else {
                    est.update_exotic_passthrough(addr);
                }
            }
            log::info!(
                "[Searcher] Seeded {} V3/exotic pools: {} with spot rates, {} fee-discounted fallback",
                passthrough_pools.len(), spot_ok, spot_fallback
            );
        }

        // Batch eth_getStorageAt (slot 8 = V2 packed reserves) for all V2-style pools.
        let slot8_hex = format!("0x{:0>64}", "8");
        let client = reqwest::Client::new();
        const CHUNK: usize = 200;
        let mut populated = 0usize;

        for chunk in slot8_pools.chunks(CHUNK) {
            let batch: Vec<Value> = chunk
                .iter()
                .enumerate()
                .map(|(i, addr)| {
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": i,
                        "method": "eth_getStorageAt",
                        "params": [
                            format!("0x{}", hex::encode(addr.as_slice())),
                            &slot8_hex,
                            "latest"
                        ]
                    })
                })
                .collect();

            let resp = match client.post(&self.rpc_url).json(&batch).send().await {
                Ok(r) => r,
                Err(e) => {
                    log::warn!("[Searcher] Seed batch RPC error: {e}");
                    continue;
                }
            };
            let results: Vec<Value> = match resp.json().await {
                Ok(v) => v,
                Err(e) => {
                    log::warn!("[Searcher] Seed batch JSON decode error: {e}");
                    continue;
                }
            };

            {
                let mut db = self.state_db.write().unwrap();
                for (i, result) in results.iter().enumerate() {
                    if i >= chunk.len() {
                        break;
                    }
                    let hex_str = match result.get("result").and_then(|v| v.as_str()) {
                        Some(s) => s.trim_start_matches("0x"),
                        None => continue,
                    };
                    let padded = format!("{:0>64}", hex_str);
                    if let Ok(bytes) = hex::decode(&padded) {
                        let mut arr = [0u8; 32];
                        let offset = 32usize.saturating_sub(bytes.len().min(32));
                        arr[offset..].copy_from_slice(&bytes[..bytes.len().min(32)]);
                        let val = alloy::primitives::U256::from_be_bytes(arr);
                        if !val.is_zero() {
                            db.update_slot(chunk[i], V2_RESERVES_SLOT, val);
                            populated += 1;
                        }
                    }
                }
            }
        }

        log::info!(
            "[Searcher] Prefetched {}/{} V2 reserves — computing initial rates…",
            populated,
            slot8_pools.len()
        );

        // Compute V2 rates from the freshly-populated reserves.
        self.update_rates_for_pools(&slot8_pools);

        let rate_count = self.estimator.read().map(|e| e.pool_count()).unwrap_or(0);
        log::info!(
            "[Searcher] Startup seed complete: {} rate entries in {:.1}s",
            rate_count,
            t0.elapsed().as_secs_f32()
        );
    }

    /// Update estimator rates for the given pool addresses.
    fn update_rates_for_pools(&self, pools: &[Address]) {
        let db = match self.state_db.read() {
            Ok(db) => db,
            Err(_) => return,
        };
        let mut est = match self.estimator.write() {
            Ok(est) => est,
            Err(_) => return,
        };

        for pool_addr in pools {
            if let Some(meta) = self.pool_meta.get(pool_addr) {
                est.invalidate_pool(pool_addr);
                if meta.protocol.is_v3() {
                    // Try actual spot rate from sqrtPriceX96 first
                    est.update_v3_spot_rate(
                        &db, pool_addr,
                        meta.decimals0, meta.decimals1,
                        meta.fee,
                    );
                    // If spot rate didn't produce entries (sqrtPriceX96 not loaded),
                    // try full tick simulation, then fall back to fee-discounted.
                    if !est.rates.contains_key(&(*pool_addr, true)) {
                        est.update_v3_rate(
                            &db, pool_addr,
                            meta.decimals0, meta.decimals1,
                            meta.fee, meta.tick_spacing,
                        );
                    }
                    if !est.rates.contains_key(&(*pool_addr, true)) {
                        est.update_v3_passthrough_with_fee(pool_addr, meta.fee);
                    }
                } else if matches!(meta.protocol, PoolProtocol::Aerodrome) {
                    // tick_spacing == 0 → stable pool, else volatile
                    let stable = meta.tick_spacing == 0;
                    let fee_bps = meta.fee as u64;
                    est.update_aerodrome_rate(
                        &db, pool_addr,
                        meta.decimals0, meta.decimals1,
                        stable, fee_bps,
                    );
                } else if matches!(
                    meta.protocol,
                    PoolProtocol::BalancerV2
                        | PoolProtocol::CurveTwoCrypto
                        | PoolProtocol::CurveTriCrypto
                        | PoolProtocol::MaverickV2
                ) {
                    // Exotic pools: store neutral pass-through rate (exact amounts
                    // are verified by the REVM quoter in the simulator step).
                    est.update_exotic_passthrough(pool_addr);
                } else {
                    let csv_factor = 10000u64.saturating_sub(meta.fee as u64);
                    let fee_factor = if csv_factor >= 9900 && csv_factor < 10000 {
                        csv_factor
                    } else {
                        meta.protocol.v2_fee_factor()
                    };
                    est.update_v2_rate(
                        &db, pool_addr,
                        meta.decimals0, meta.decimals1,
                        fee_factor,
                    );
                }
            }
        }
    }

    /// Get indices of paths affected by the given pool changes.
    fn get_affected_paths(&self, pools: &[Address], is_mempool_event: bool) -> Vec<usize> {
        let mut seen = std::collections::HashSet::new();
        let mut indices = Vec::new();

        for pool in pools {
            if let Some(path_indices) = self.pool_to_paths.get(pool) {
                for &idx in path_indices {
                    if seen.insert(idx) {
                        indices.push(idx);
                    }
                }
            }
        }

        if !is_mempool_event {
            // Block-triggered events: shuffle before capping to get a diverse random sample.
            let mut rng = rand::thread_rng();
            indices.shuffle(&mut rng);
        }

        // Cap at max_paths_per_block
        let found = indices.len();
        let cap = self.config.max_paths_per_block;
        let capped = found > cap;
        if capped {
            indices.truncate(cap);
        }
        log::info!(
            "[Searcher] {} paths: {} touched pools → {} paths found{}",
            if is_mempool_event { "mempool" } else { "block" },
            pools.len(),
            found,
            if capped { format!(" (CAPPED at {})", cap) } else { " (under cap)".to_string() },
        );

        indices
    }

    /// Evaluate paths in parallel using rayon.
    fn evaluate_paths_parallel(&self, indices: &[usize], block_number: u64) -> Vec<ArbPath> {
        let est = match self.estimator.read() {
            Ok(est) => est,
            Err(_) => return vec![],
        };

        let min_rate = RATE_SCALE;
        let _base_fee = self.gas_station.current_base_fee();

        use std::sync::atomic::{AtomicUsize, Ordering};
        let zero_rate   = AtomicUsize::new(0); // rate == 0  (missing rate entry — pool never seeded)
        let below_min   = AtomicUsize::new(0); // 0 < rate < min_rate (real but unprofitable)
        let from_cache  = AtomicUsize::new(0); // skipped via rate cache

        // Parallel evaluation with rayon
        let results: Vec<ArbPath> = indices
            .par_iter()
            .filter_map(|&idx| {
                let path = &self.all_paths[idx];

                // Check cache first
                if let Some(cached_rate) = self.rate_cache.get_fresh(path.hash, block_number) {
                    from_cache.fetch_add(1, Ordering::Relaxed);
                    if cached_rate < min_rate {
                        return None;
                    }
                }

                // Evaluate path
                let rate = est.evaluate_path(path);

                // Cache the result
                self.rate_cache.insert(path.hash, rate, block_number);

                if rate.is_zero() {
                    zero_rate.fetch_add(1, Ordering::Relaxed);
                    return None;
                }
                if rate < min_rate {
                    below_min.fetch_add(1, Ordering::Relaxed);
                    return None;
                }

                // Estimate profit (very rough: excess rate * 1 ETH reference)
                let excess = rate - RATE_SCALE;

                // Per-path gas estimate: base + per-hop cost.
                // REVM FlashQuoter measures ~308-337K for 4-hop V3 paths.
                // executeArbitrage overhead + flash loan ≈ 60K base.
                // V3 hops ~80K, V2 hops ~50K each (from REVM gas_used analysis).
                let base_gas: u64 = 60_000; // flash loan + contract overhead
                let hop_gas: u64 = path.steps.iter().map(|s| {
                    if s.protocol.is_v3() { 80_000u64 } else { 50_000u64 }
                }).sum();
                let est_gas = base_gas + hop_gas;

                Some(ArbPath {
                    path: path.clone(),
                    expected_profit: excess,
                    gas_estimate: est_gas,
                    block_number,
                })
            })
            .collect();

        let total = indices.len();
        let cached = from_cache.load(Ordering::Relaxed);
        let zero   = zero_rate.load(Ordering::Relaxed);
        let below  = below_min.load(Ordering::Relaxed);
        let passed = results.len();

        // Sort by estimated profit descending so the simulator always sees the
        // most promising candidates first, regardless of how many are forwarded.
        let mut results = results;
        results.sort_unstable_by(|a, b| b.expected_profit.cmp(&a.expected_profit));

        log::info!(
            "[Searcher] eval {total} paths: {cached} cached, {zero} zero-rate (unseeded), {below} below-min, {passed} passed → simulator",
        );

        results
    }
}
