/// Simulator pipeline worker — receives ArbPath events, verifies them
/// with the REVM quoter, optimizes input amount via binary search, and
/// sends ValidPath events downstream.
///
/// Pipeline position: ArbPath → [Simulator] → ValidPath
///
/// Features ported from BaseBuster's simulator.rs:
/// - **Path blacklist**: paths that persistently revert are blacklisted by hash
///   so they are skipped in future blocks without wasting cycles.
/// - **SIM mode**: when `SIM=true` env variable is set, compares the calculator
///   estimate vs the REVM quoter output and logs discrepancies for debugging.

use alloy::primitives::{Address, U256};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::mpsc;

use crate::gas_station::GasStation;
use crate::pipeline_events::{ArbPath, ValidPath};
use crate::quoter_revm::RevmQuoter;
use crate::state_db::BlockStateDB;

/// Type alias for the shared receiver used by parallel simulator workers.
pub type SharedArbReceiver = Arc<tokio::sync::Mutex<mpsc::Receiver<ArbPath>>>;

/// Simulator configuration.
#[derive(Clone)]
pub struct SimulatorConfig {
    /// Minimum profit in wei to consider a path valid after gas.
    pub min_profit_wei: U256,
    /// Number of binary/ternary search steps for input optimization.
    pub optimization_steps: usize,
    /// Default minimum input amount for search (in wei).
    pub min_input: U256,
    /// Default maximum input amount for search (in wei).
    pub max_input: U256,
    /// Share of profit to allocate to priority fee (bps, e.g. 5000 = 50%).
    pub profit_share_bps: u64,
    /// Flash-loan fee in bps charged on the loan amount.
    /// Aave V3 = 5 (0.05%), PancakeSwap V3 flash = 0 (no fee).
    /// Set to 0 to disable deduction (no flash loan or fee already embedded).
    pub flash_loan_fee_bps: u64,
    /// Provider name associated with `flash_loan_fee_bps` (for logs/diagnostics).
    pub flash_loan_provider: String,
    /// RPC URL used to query chain head for stale-block skipping during catch-up.
    pub rpc_url: String,
    /// How many blocks behind the latest-seen block a path may be before it is
    /// discarded as stale without simulation.  1 means: skip anything
    ///  from block N-2 or older when a path from block N has been received.
    /// BSC ≈ 400ms blocks → 1 is tight and correct; increase for slower chains.
    pub sim_stale_blocks: u64,
}

impl Default for SimulatorConfig {
    fn default() -> Self {
        Self {
            min_profit_wei: U256::from(100_000_000_000_000u64), // 0.0001 ETH
            optimization_steps: 20,
            min_input: U256::from(10_000_000_000_000_000u64),  // 0.01 ETH
            max_input: U256::from(50_000_000_000_000_000_000u128), // 50 ETH
            profit_share_bps: 5000, // 50%
            flash_loan_fee_bps: 0,  // overridden per-chain in start_pipeline
            flash_loan_provider: "AaveV3".to_string(),
            rpc_url: String::new(),
            sim_stale_blocks: 1,
        }
    }
}

/// Simulator worker.
pub struct SimulatorWorker {
    pub config: SimulatorConfig,
    pub state_db: Arc<RwLock<BlockStateDB>>,
    pub quoter: Arc<RevmQuoter>,
    pub gas_station: Arc<GasStation>,
    /// Paths that have persistently reverted — skip them in future blocks.
    /// Uses path hash (u64) as key, matching BaseBuster's blacklist semantics.
    pub blacklisted_paths: Arc<Mutex<HashSet<u64>>>,
    /// Tokens that have repeatedly caused preflight reverts (honeypots/tax tokens).
    /// Any path containing one is skipped early to avoid wasting simulation cycles.
    pub blacklisted_tokens: Arc<Mutex<HashSet<Address>>>,
    /// SIM mode: compare calculator estimate vs quoter and log discrepancies.
    /// Enabled by env var `SIM=true`. Does not submit transactions in SIM mode.
    pub sim_mode: bool,
    /// Shared atomic tracking the highest block number seen across ALL simulator workers.
    /// When any worker picks up a path from a newer block it bumps this value, causing
    /// all other workers to immediately skip their remaining old-block work as stale.
    pub shared_latest_block: Arc<AtomicU64>,
    /// Address of the chain's native gas token (e.g. WBNB on BSC, WETH on Base).
    /// Used to normalise profit from non-native start tokens into gas-token units
    /// before comparing with `gas_cost_wei`.
    pub native_token: Address,
    /// For each non-native start token: a V2 pool address that pairs it against the
    /// native token. Used to convert profit into native-token units at sim time.
    /// key = start_token_address, value = price_pool_address
    pub token_price_pools: HashMap<Address, Address>,
}

impl SimulatorWorker {
    pub fn new(
        config: SimulatorConfig,
        state_db: Arc<RwLock<BlockStateDB>>,
        quoter: Arc<RevmQuoter>,
        gas_station: Arc<GasStation>,
        native_token: Address,
        token_price_pools: HashMap<Address, Address>,
    ) -> Self {
        let sim_mode = std::env::var("SIM")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false);
        Self {
            config,
            state_db,
            quoter,
            gas_station,
            blacklisted_paths: Arc::new(Mutex::new(HashSet::new())),
            blacklisted_tokens: Arc::new(Mutex::new(HashSet::new())),
            sim_mode,
            shared_latest_block: Arc::new(AtomicU64::new(0)),
            native_token,
            token_price_pools,
        }
    }

    /// Query the current block number from the RPC node.
    async fn get_chain_head(&self) -> Option<u64> {
        use alloy::providers::{Provider, ProviderBuilder};
        let url = self.config.rpc_url.parse().ok()?;
        let provider = ProviderBuilder::new().on_http(url);
        provider.get_block_number().await.ok()
    }

    /// Run the simulator worker.
    ///
    /// `arb_rx` is a shared receiver — multiple workers pull items concurrently.
    /// `worker_id` is used for logging to distinguish parallel workers.
    pub async fn run(
        &self,
        arb_rx: SharedArbReceiver,
        valid_tx: mpsc::Sender<ValidPath>,
        worker_id: usize,
    ) {
        log::info!("[Simulator-{worker_id}] Worker started (sim_mode={})", self.sim_mode);
        log::info!(
            "[Simulator-{worker_id}] Flash-loan provider={} fee_bps={}",
            self.config.flash_loan_provider,
            self.config.flash_loan_fee_bps
        );
        // Share the latest-block atomic with all sibling workers for cross-worker stale detection.
        let latest_block = Arc::clone(&self.shared_latest_block);
        let mut current_block: u64 = 0;
        let mut block_total: u64 = 0;
        let mut block_profitable: u64 = 0;
        let mut block_unprofitable: u64 = 0;
        let mut block_skipped: u64 = 0;
        let mut block_stale: u64 = 0;

        // Track chain head for absolute stale-block skipping during catch-up.
        let max_sim_stale_blocks = self.config.sim_stale_blocks;
        let mut last_head_check = std::time::Instant::now();
        let mut cached_head: u64 = self.get_chain_head().await.unwrap_or(0);
        log::info!("[Simulator-{worker_id}] Chain head at startup: {cached_head}");

        loop {
            // Acquire the shared receiver lock, recv one item, then release.
            // Processing (REVM simulation) happens outside the lock so other
            // workers can recv concurrently.
            let arb = {
                let mut rx = arb_rx.lock().await;
                match rx.recv().await {
                    Some(arb) => arb,
                    None => break, // channel closed
                }
            };
            // Track latest block to detect stale paths
            latest_block.fetch_max(arb.block_number, Ordering::Relaxed);

            // Detect block boundary — flush summary for previous block
            if arb.block_number != current_block {
                if current_block > 0 && block_total > 0 {
                    log::info!(
                        "[Simulator-{worker_id}] Block {}: {} simulated → {} profitable, {} unprofitable, {} blacklisted, {} stale",
                        current_block, block_total, block_profitable, block_unprofitable, block_skipped, block_stale
                    );
                }
                current_block = arb.block_number;
                block_total = 0;
                block_profitable = 0;
                block_unprofitable = 0;
                block_skipped = 0;
                block_stale = 0;

                // Refresh chain head at each block boundary (cheap, ~1ms)
                if last_head_check.elapsed() > std::time::Duration::from_secs(1) {
                    if let Some(head) = self.get_chain_head().await {
                        cached_head = head;
                    }
                    last_head_check = std::time::Instant::now();
                }
            }

            // Absolute stale check — skip entire blocks that are behind chain head.
            // This prevents the simulator from wasting minutes on catch-up blocks.
            if cached_head > 0 && arb.block_number + max_sim_stale_blocks < cached_head {
                block_stale += 1;
                continue;
            }

            // Skip stale paths (from blocks too far behind the newest seen by any worker)
            let latest = latest_block.load(Ordering::Relaxed);
            if arb.block_number + max_sim_stale_blocks < latest {
                block_stale += 1;
                continue;
            }

            // Skip blacklisted paths — they have previously reverted
            {
                let bl = self.blacklisted_paths.lock().unwrap();
                if bl.contains(&arb.path.hash) {
                    block_skipped += 1;
                    log::debug!("[Simulator-{worker_id}] Skipping blacklisted path {:x}", arb.path.hash);
                    continue;
                }
            }
            // Skip paths containing blacklisted tokens (honeypots/tax tokens)
            {
                let bt = self.blacklisted_tokens.lock().unwrap();
                if !bt.is_empty() {
                    let has_toxic = arb.path.steps.iter().any(|s| {
                        bt.contains(&s.token_in) || bt.contains(&s.token_out)
                    });
                    if has_toxic {
                        block_skipped += 1;
                        continue;
                    }
                }
            }

            if self.sim_mode {
                // SIM mode: verify calc estimate vs quoter, do NOT submit
                self.run_sim_verification(&arb);
                continue;
            }

            block_total += 1;
            match self.simulate_and_optimize(&arb) {
                Some(valid) => {
                    block_profitable += 1;
                    log::info!(
                        "[Simulator-{worker_id}] *** ARB FOUND *** Block {} path {:x}: net_profit={} amount_in={} amount_out={}",
                        arb.block_number, arb.path.hash, valid.net_profit, valid.amount_in, valid.amount_out
                    );
                    if valid_tx.send(valid).await.is_err() {
                        log::error!("[Simulator-{worker_id}] Channel closed");
                        return;
                    }
                }
                None => {
                    block_unprofitable += 1;
                    log::debug!("[Simulator-{worker_id}] Path {:x} not profitable", arb.path.hash);
                }
            }
        }

        // Final flush on shutdown
        if block_total > 0 {
            log::info!(
                "[Simulator-{worker_id}] Block {}: {} simulated → {} profitable, {} unprofitable, {} blacklisted, {} stale",
                current_block, block_total, block_profitable, block_unprofitable, block_skipped, block_stale
            );
        }

        log::info!("[Simulator-{worker_id}] Channel closed, shutting down");
    }

    /// SIM mode: compare off-chain calculator estimate vs REVM quoter output.
    /// Logs differences to help identify calculation errors. No tx submission.
    /// Mirrors BaseBuster's `if sim { ... }` block in simulate_paths().
    fn run_sim_verification(&self, arb: &ArbPath) {
        let estimated_input = self.estimate_initial_input(arb);
        let expected_out = arb.expected_profit + estimated_input; // rough expected output

        match self.quoter.quote_path(&self.state_db, &arb.path, estimated_input) {
            Some(quoted_out) => {
                if quoted_out == expected_out {
                    log::info!(
                        "[SIM] ✓ Path {:x} matches: estimated={} quoted={}",
                        arb.path.hash, expected_out, quoted_out
                    );
                } else {
                    log::warn!(
                        "[SIM] ✗ Path {:x} MISMATCH: calculated={} quoted={} diff={}",
                        arb.path.hash,
                        expected_out,
                        quoted_out,
                        if quoted_out > expected_out {
                            quoted_out - expected_out
                        } else {
                            expected_out - quoted_out
                        }
                    );
                    // Debug the full path calculation
                    let db = self.state_db.read().unwrap();
                    let mut current = estimated_input;
                    for (i, step) in arb.path.steps.iter().enumerate() {
                        let zero_for_one = step.token_in < step.token_out;
                        let step_out = if step.protocol.is_v3() {
                            crate::calculation::v3::get_amount_out_v3(
                                &db, &step.pool_address, current, zero_for_one, step.fee, 1,
                            ).unwrap_or(U256::ZERO)
                        } else {
                            db.read_v2_reserves(&step.pool_address)
                                .map(|(r0, r1)| {
                                    let (ri, ro) = if zero_for_one { (r0, r1) } else { (r1, r0) };
                                    crate::calculation::v2::get_amount_out_v2(
                                        current, ri, ro, step.protocol.v2_fee_factor(),
                                    )
                                })
                                .unwrap_or(U256::ZERO)
                        };
                        log::debug!(
                            "[SIM]   step[{i}] pool={:?} in={} out={}",
                            step.pool_address, current, step_out
                        );
                        current = step_out;
                    }
                }
            }
            None => {
                log::debug!(
                    "[SIM] Path {:x}: quoter returned None (no bytecode or revert)",
                    arb.path.hash
                );
            }
        }
    }

    /// Simulate a path and optimize the input amount.
    fn simulate_and_optimize(&self, arb: &ArbPath) -> Option<ValidPath> {
        // Create ONE SimDb snapshot and reuse for all REVM calls on this path.
        let mut sim_db = {
            let db = self.state_db.read().ok()?;
            crate::sim_db::SimDb::snapshot(&*db)
        }; // read lock released

        // 1. Quick viability check at min_input.
        //    If the path loses money at tiny size (minimal price impact),
        //    it will lose at every size → skip expensive optimizer.
        let probe_input = self.config.min_input;
        let probe_output = self.quoter.quote_with_db(&mut sim_db, &arb.path, probe_input);
        match probe_output {
            None => {
                log::debug!("[Simulator] quote_path returned None for {:x} (bytecode missing or revert)", arb.path.hash);
                return None;
            }
            Some(out) if out <= probe_input => {
                log::debug!("[Simulator] path {:x} not viable: min_input={} gives out={}", arb.path.hash, probe_input, out);
                return None;
            }
            Some(out) if out > probe_input * U256::from(3u64) / U256::from(2u64) => {
                // Sanity: output > 1.5× input in an arb cycle is almost certainly bogus.
                // Real DEX arbs return at most 1-5%. The V3 analytical math can
                // overestimate for low-liquidity pools where swaps cross ticks.
                log::debug!("[Simulator] path {:x} bogus output: in={} out={} (>1.5x, skipping)", arb.path.hash, probe_input, out);
                return None;
            }
            _ => {} // profitable at min_input, proceed to optimize
        }

        // 2. Optimize input via REVM quoter — reuses the same SimDb (fast).
        let (best_input, best_output) = self.quoter.optimize_input_with_db(
            &mut sim_db,
            &arb.path,
            self.config.min_input,
            self.config.max_input,
            self.config.optimization_steps,
        );

        // Write back any lazy-fetched RPC data to the shared state_db so
        // future snapshots include it (no repeated RPC calls for the same slots).
        {
            if let Ok(mut db) = self.state_db.write() {
                sim_db.write_back(&mut *db);
            }
        }

        if best_output <= best_input {
            log::debug!("[Simulator] path {:x} not profitable after optimize: in={} out={}", arb.path.hash, best_input, best_output);
            return None; // not profitable
        }

        // Sanity check: reject outputs that are unreasonably large (V3 math artifact)
        if best_output > best_input * U256::from(3u64) / U256::from(2u64) {
            log::debug!("[Simulator] path {:x} bogus optimized output: in={} out={} (>1.5x)", arb.path.hash, best_input, best_output);
            return None;
        }

        let gross_profit = best_output - best_input;

        // 4a. Deduct Aave (or other) flash loan fee on the borrowed amount.
        //     fee = amount_in * flash_loan_fee_bps / 10_000
        //     Aave V3 = 5 bps (0.05%), PancakeSwap flash = 0.
        let flash_fee = if self.config.flash_loan_fee_bps > 0 {
            best_input * U256::from(self.config.flash_loan_fee_bps) / U256::from(10_000u64)
        } else {
            U256::ZERO
        };

        // Profit after flash-loan fee, still in start-token units.
        let profit_after_fee = if gross_profit > flash_fee {
            gross_profit - flash_fee
        } else {
            log::debug!("[Simulator] path {:x} flash fee {} wipes gross profit {}", arb.path.hash, flash_fee, gross_profit);
            return None; // flash fee alone wipes the profit
        };

        // 4b. Normalise profit to native-gas-token units.
        //     For WBNB-start arbs this is a no-op.
        //     For BTCB/ETH-start arbs we convert using the V2 price pool reserves.
        let start_token = arb.path.steps.first().map(|s| s.token_in).unwrap_or(self.native_token);
        let profit_native = if start_token == self.native_token {
            profit_after_fee
        } else if let Some(&price_pool) = self.token_price_pools.get(&start_token) {
            let db = self.state_db.read().unwrap();
            // Auto-detect pool type: try V2 reserves first, then V3 sqrtPriceX96.
            // This means `price_pool` in the TOML can be either a V2 or V3 pair address.
            if let Some((r0, r1)) = db.read_v2_reserves(&price_pool) {
                let (r_token, r_native) = if start_token < self.native_token {
                    (r0, r1) // start_token is token0, native is token1
                } else {
                    (r1, r0) // native is token0, start_token is token1
                };
                if r_token.is_zero() {
                    log::debug!("[Simulator] price_pool V2 has zero reserve for start token {:?}", start_token);
                    return None;
                }
                profit_after_fee * r_native / r_token
            } else if let Some((sqrt_price_x96, _)) = db.read_v3_slot0(&price_pool) {
                // V3 concentrated liquidity pool — use sqrtPriceX96 for spot price.
                let start_is_token0 = start_token < self.native_token;
                match v3_spot_price_convert(profit_after_fee, sqrt_price_x96, start_is_token0) {
                    Some(converted) => converted,
                    None => {
                        log::debug!("[Simulator] V3 spot price conversion failed for {:?}", start_token);
                        return None;
                    }
                }
            } else {
                // Pool not yet in state DB (neither V2 nor V3 data loaded yet).
                log::debug!("[Simulator] price_pool for {:?} not in state DB yet; using raw profit", start_token);
                profit_after_fee
            }
        } else {
            // No price pool configured — compare profits as raw token units.
            // This is only safe when start_token ≈ native_token in value (e.g. stablecoin arbs).
            log::debug!("[Simulator] No price_pool for start token {:?}; raw profit comparison", start_token);
            profit_after_fee
        };

        // 4c. Deduct gas cost (already in native token units).
        let gas_cost_wei = U256::from(
            self.gas_station
                .estimate_gas_cost_wei(arb.gas_estimate),
        );

        if profit_native <= gas_cost_wei {
            log::debug!("[Simulator] path {:x} gas {} eats profit_native {}", arb.path.hash, gas_cost_wei, profit_native);
            return None; // gas eats all profit
        }

        let net_profit = profit_native - gas_cost_wei;

        if net_profit < self.config.min_profit_wei {
            log::debug!("[Simulator] path {:x} net_profit {} below min {}", arb.path.hash, net_profit, self.config.min_profit_wei);
            return None; // below minimum threshold
        }

        Some(ValidPath {
            arb: arb.clone(),
            amount_in: best_input,
            amount_out: best_output,
            gas_cost: gas_cost_wei,
            net_profit,
        })
    }

    /// Estimate a reasonable starting input amount from the arb's expected profit.
    fn estimate_initial_input(&self, _arb: &ArbPath) -> U256 {
        // Start with 1 ETH as a baseline, adjust based on profit expectation
        let base = U256::from(1_000_000_000_000_000_000u64); // 1 ETH
        // Clamp to [min_input, max_input]
        if base < self.config.min_input {
            self.config.min_input
        } else if base > self.config.max_input {
            self.config.max_input
        } else {
            base
        }
    }

}

/// Convert `profit` (in start_token units) to native-token units using a V3 pool's
/// `sqrtPriceX96` (Uniswap V3 / PancakeSwap V3 slot0 encoding: `sqrt(p) * 2^96`).
///
/// Uses two-step shifted multiplication to stay within U256 bounds:
/// - If `start_is_token0`: native = token1 → price = sqrtP² / 2¹⁹²
///   `profit_native = (profit * sqrtP >> 96) * sqrtP >> 96`
/// - Otherwise: native = token0 → price = 1 / (sqrtP² / 2¹⁹²)
///   `profit_native = (profit * 2^96 / sqrtP) * 2^96 / sqrtP`
///
/// Returns `None` only if an arithmetic overflow is detected; callers should
/// treat that as "no price data" and fall back to raw units.
fn v3_spot_price_convert(profit: U256, sqrt_price_x96: U256, start_is_token0: bool) -> Option<U256> {
    if sqrt_price_x96.is_zero() || profit.is_zero() {
        return Some(U256::ZERO);
    }
    if start_is_token0 {
        // native is token1 — more valuable token is in the numerator of price
        // profit_native = profit * sqrtP / 2^96 * sqrtP / 2^96
        let step1: U256 = profit.checked_mul(sqrt_price_x96)? >> 96;
        Some(step1.checked_mul(sqrt_price_x96)? >> 96)
    } else {
        // native is token0 — invert the price
        // profit_native = profit * 2^96 / sqrtP * 2^96 / sqrtP
        let q96: U256 = U256::from(1u64) << 96;
        let step1: U256 = profit.checked_mul(q96)? / sqrt_price_x96;
        Some(step1.checked_mul(q96)? / sqrt_price_x96)
    }
}
