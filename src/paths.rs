use ethers::types::{H160, U256};
use std::collections::HashMap;

use crate::bundler::PathParam;
use crate::multi::Reserve;
use crate::pools::{AnyPool, DexVariant, PoolType, Pool};
use crate::algebra::AlgebraPoolFull;
use crate::lfj::{LFJPoolInfo, LFJState};
use crate::concentrated_liquidity;
use crate::simulator::{SolidlyStableSimulator, UniswapV2Simulator};

/// Simulate an LFJ Liquidity Book swap with multi-bin traversal.
///
/// LFJ bins are constant-sum: within a single bin, the exchange rate is fixed at
/// `price(bin_id) = (1 + bin_step/10000)^(bin_id - 8388608)`.
///
/// When `zero_for_one` (sell tokenX for tokenY):
///   - At the active bin, we can extract up to `reserve_y` by depositing tokenX at `price`.
///   - If the swap exhausts tokenY in this bin, we move to the next **lower** bin (lower price).
///
/// When `!zero_for_one` (sell tokenY for tokenX):
///   - At the active bin, we can extract up to `reserve_x` by depositing tokenY.
///   - If the swap exhausts tokenX in this bin, we move to the next **higher** bin (higher price).
///
/// Fees are applied once to the entire input amount before bin traversal.
fn simulate_lfj_swap(
    amount_in: U256,
    zero_for_one: bool,
    lp: &LFJPoolInfo,
    state: &LFJState,
) -> Option<U256> {
    const BASE_ID: i64 = 8_388_608;
    const PRICE_SCALE: u64 = 1_000_000_000;
    const MAX_BIN_CROSSES: usize = 10;

    if state.bins.is_empty() {
        return None;
    }

    // Apply fee to the total input once.
    let fee_factor = U256::from(10_000u32.saturating_sub(lp.base_fee_bps));
    let mut remaining_in = amount_in
        .checked_mul(fee_factor)?
        .checked_div(U256::from(10_000u64))?;

    if remaining_in.is_zero() {
        return Some(U256::zero());
    }

    // Build a sorted list of bins in the traversal direction.
    // zero_for_one → iterate bins from active_id downward (decreasing price)
    // !zero_for_one → iterate bins from active_id upward (increasing price)
    let ordered_bins: Vec<&(u32, U256, U256)> = if zero_for_one {
        // Bins with id <= active_id, sorted descending (start at active, go lower)
        let mut v: Vec<_> = state.bins.iter().filter(|(id, _, _)| *id <= state.active_id).collect();
        v.sort_by(|a, b| b.0.cmp(&a.0));
        v
    } else {
        // Bins with id >= active_id, sorted ascending (start at active, go higher)
        let mut v: Vec<_> = state.bins.iter().filter(|(id, _, _)| *id >= state.active_id).collect();
        v.sort_by_key(|(id, _, _)| *id);
        v
    };

    let mut total_out = U256::zero();

    for (crosses, (bin_id, bin_rx, bin_ry)) in ordered_bins.iter().enumerate() {
        if crosses >= MAX_BIN_CROSSES || remaining_in.is_zero() {
            break;
        }

        let delta = (*bin_id as i64) - BASE_ID;
        let delta_i32 = delta.clamp(-100_000, 100_000) as i32;
        let price_f64 = (1.0_f64 + lp.bin_step as f64 / 10_000.0).powi(delta_i32);
        if !price_f64.is_finite() || price_f64 <= 0.0 {
            break;
        }

        let price_scaled = (price_f64 * PRICE_SCALE as f64) as u128;
        if price_scaled == 0 {
            break;
        }

        if zero_for_one {
            // Selling X for Y: output = input_x * price; capped at bin's reserve_y
            let max_out_from_remaining = remaining_in
                .checked_mul(U256::from(price_scaled))?
                .checked_div(U256::from(PRICE_SCALE))?;

            if max_out_from_remaining <= *bin_ry {
                // Fully consumed in this bin
                total_out = total_out.checked_add(max_out_from_remaining)?;
                remaining_in = U256::zero();
            } else {
                // Exhaust this bin's Y reserve, continue to next bin
                total_out = total_out.checked_add(*bin_ry)?;
                // How much X was consumed to deplete bin_ry: consumed_x = bin_ry / price
                let consumed_x = bin_ry
                    .checked_mul(U256::from(PRICE_SCALE))?
                    .checked_div(U256::from(price_scaled))?;
                remaining_in = remaining_in.saturating_sub(consumed_x.max(U256::one()));
            }
        } else {
            // Selling Y for X: output = input_y / price; capped at bin's reserve_x
            let max_out_from_remaining = remaining_in
                .checked_mul(U256::from(PRICE_SCALE))?
                .checked_div(U256::from(price_scaled))?;

            if max_out_from_remaining <= *bin_rx {
                // Fully consumed in this bin
                total_out = total_out.checked_add(max_out_from_remaining)?;
                remaining_in = U256::zero();
            } else {
                // Exhaust this bin's X reserve, continue to next bin
                total_out = total_out.checked_add(*bin_rx)?;
                // How much Y was consumed to deplete bin_rx: consumed_y = bin_rx * price
                let consumed_y = bin_rx
                    .checked_mul(U256::from(price_scaled))?
                    .checked_div(U256::from(PRICE_SCALE))?;
                remaining_in = remaining_in.saturating_sub(consumed_y.max(U256::one()));
            }
        }
    }

    if total_out.is_zero() {
        return None;
    }
    Some(total_out)
}

#[derive(Debug, Clone)]
pub struct ArbPath {
    pub nhop: u8,
    pub pool_1: AnyPool,
    pub pool_2: AnyPool,
    pub pool_3: AnyPool,
    pub zero_for_one_1: bool,
    pub zero_for_one_2: bool,
    pub zero_for_one_3: bool,
}

impl ArbPath {
    pub fn has_pool(&self, pool_addr: &H160) -> bool {
        let is_pool_1 = self.pool_1.address == *pool_addr;
        let is_pool_2 = self.pool_2.address == *pool_addr;
        let is_pool_3 = self.pool_3.address == *pool_addr;
        is_pool_1 || is_pool_2 || is_pool_3
    }

    pub fn get_pool(&self, i: u8) -> &AnyPool {
        debug_assert!(self.nhop <= 3, "nhop={} exceeds max of 3", self.nhop);
        match i {
            0 => &self.pool_1,
            1 => &self.pool_2,
            2 => &self.pool_3,
            _ => unreachable!("get_pool called with i={} but nhop={}", i, self.nhop),
        }
    }

    pub fn get_zero_for_one(&self, i: u8) -> bool {
        debug_assert!(self.nhop <= 3, "nhop={} exceeds max of 3", self.nhop);
        match i {
            0 => self.zero_for_one_1,
            1 => self.zero_for_one_2,
            2 => self.zero_for_one_3,
            _ => unreachable!("get_zero_for_one called with i={} but nhop={}", i, self.nhop),
        }
    }

    pub fn should_blacklist(&self, blacklist_tokens: &[H160]) -> bool {
        for i in 0..self.nhop {
            let pool = self.get_pool(i);
            if blacklist_tokens.contains(&pool.token0) || blacklist_tokens.contains(&pool.token1) {
                return true;
            }
        }
        false
    }
    
    /// Simulate the full multi-hop swap starting from raw atomic units.
    ///
    /// Unlike `simulate_mixed_path` this does **not** multiply `amount_in_raw`
    /// by 10^decimals — the caller is expected to have already done so.
    pub fn simulate_mixed_path_raw(
        &self,
        amount_in_raw: U256,
        v2_reserves: &HashMap<H160, Reserve>,
        algebra_states: &HashMap<H160, AlgebraPoolFull>,
        lfj_states: &HashMap<H160, LFJState>,
    ) -> Option<U256> {
        if self.nhop == 0 || self.nhop > 3 {
            return None;
        }
        if amount_in_raw.is_zero() {
            return Some(U256::zero());
        }
        let mut amount = amount_in_raw;

        for i in 0..self.nhop {
            let pool = self.get_pool(i);
            let zero_for_one = self.get_zero_for_one(i);
            
            match &pool.pool_type {
                PoolType::V2(v2_pool) => {
                     if let Some(reserve) = v2_reserves.get(&pool.address) {
                         let (reserve_in, reserve_out) = if zero_for_one {
                             (reserve.reserve0, reserve.reserve1)
                         } else {
                             (reserve.reserve1, reserve.reserve0)
                         };
                         let fee = U256::from(v2_pool.fee);

                         if v2_pool.version == DexVariant::SolidlyStable {
                             let (decimals_in, decimals_out) = if zero_for_one {
                                 (v2_pool.decimals0, v2_pool.decimals1)
                             } else {
                                 (v2_pool.decimals1, v2_pool.decimals0)
                             };
                             amount = SolidlyStableSimulator::get_amount_out(
                                 amount, reserve_in, reserve_out, decimals_in, decimals_out, v2_pool.fee,
                             )?;
                         } else {
                             amount = UniswapV2Simulator::get_amount_out(amount, reserve_in, reserve_out, fee)?;
                         }
                     } else {
                         return None;
                     }
                },
                PoolType::Algebra(pool_full) => {
                     if let Some(state) = algebra_states.get(&pool.address) {
                         if let Some(out) = concentrated_liquidity::calculate_amount_out(
                             amount,
                             state.sqrt_price_x96,
                             state.liquidity,
                             pool_full.fee,
                             zero_for_one,
                             state.tick,
                             pool_full.tick_spacing,
                             &state.tick_data,
                         ) {
                             amount = out;
                         } else {
                             return None;
                         }
                     } else {
                         return None;
                     }
                }
                PoolType::UniswapV3CL(pool_full) => {
                     if let Some(state) = algebra_states.get(&pool.address) {
                         if let Some(out) = concentrated_liquidity::calculate_amount_out(
                             amount,
                             state.sqrt_price_x96,
                             state.liquidity,
                             pool_full.fee,
                             zero_for_one,
                             state.tick,
                             pool_full.tick_spacing,
                             &state.tick_data,
                         ) {
                             amount = out;
                         } else {
                             return None;
                         }
                     } else {
                         return None;
                     }
                }
                PoolType::LFJ(lp) => {
                    if let Some(state) = lfj_states.get(&pool.address) {
                        amount = simulate_lfj_swap(amount, zero_for_one, lp, state)?;
                    } else {
                        return None;
                    }
                }
            }
        }
        Some(amount)
    }

    pub fn simulate_mixed_path(
        &self,
        amount_in: U256,
        v2_reserves: &HashMap<H160, Reserve>,
        algebra_states: &HashMap<H160, AlgebraPoolFull>,
        lfj_states: &HashMap<H160, LFJState>,
    ) -> Option<U256> {
        if self.nhop == 0 || self.nhop > 3 {
            return None;
        }
        let first_pool = self.get_pool(0);
        let decimals = if self.zero_for_one_1 {
            first_pool.decimals0
        } else {
            first_pool.decimals1
        };
        
        let unit = U256::from(10).pow(U256::from(decimals));
        self.simulate_mixed_path_raw(amount_in * unit, v2_reserves, algebra_states, lfj_states)
    }

    /// Sub-token granularity for the optimizer.
    /// 1000 = millitoken precision (search in 0.001-token increments).
    const OPTIMIZER_GRANULARITY: u64 = 1000;

    /// Compute net profit (amount_out - cost) for a given sub-token amount.
    ///
    /// `sub_tokens` is in millitoken units (when OPTIMIZER_GRANULARITY = 1000).
    /// Returns 0 if the simulation returns None or amount_out <= cost.
    fn profit_at(
        &self,
        sub_tokens: u64,
        token_in_decimals: u8,
        reserves: &HashMap<H160, Reserve>,
        algebra_states: &HashMap<H160, AlgebraPoolFull>,
        lfj_states: &HashMap<H160, LFJState>,
    ) -> u128 {
        let unit = U256::from(10u64).pow(U256::from(token_in_decimals));
        let amount_raw = U256::from(sub_tokens).saturating_mul(unit)
            / U256::from(Self::OPTIMIZER_GRANULARITY);
        if amount_raw.is_zero() {
            return 0;
        }
        match self.simulate_mixed_path_raw(amount_raw, reserves, algebra_states, lfj_states) {
            Some(out) if out > amount_raw => (out - amount_raw).as_u128(),
            _ => 0,
        }
    }

    /// Find the amount_in that maximises profit using ternary search.
    ///
    /// Searches in sub-token units (millitokens for GRANULARITY=1000) for fine
    /// precision. Returns `(best_amount_in_raw_atomic, best_profit_raw_atomic)`.
    pub fn optimize_amount_in(
        &self,
        max_sub_tokens: u64,
        _step_size: usize,           // kept for API compatibility, ignored
        reserves: &HashMap<H160, Reserve>,
        algebra_states: &HashMap<H160, AlgebraPoolFull>,
        lfj_states: &HashMap<H160, LFJState>,
    ) -> (U256, U256) {
        let token_in_decimals = if self.zero_for_one_1 {
            self.pool_1.decimals0
        } else {
            self.pool_1.decimals1
        };

        // --- Phase 1: ternary search over [0, max_sub_tokens] ---
        let mut lo: u64 = 0;
        let mut hi: u64 = max_sub_tokens;

        while hi.saturating_sub(lo) > 2 {
            let third = (hi - lo) / 3;
            let m1 = lo + third;
            let m2 = hi - third;
            let f1 = self.profit_at(m1, token_in_decimals, reserves, algebra_states, lfj_states);
            let f2 = self.profit_at(m2, token_in_decimals, reserves, algebra_states, lfj_states);
            if f1 < f2 {
                lo = m1;
            } else {
                hi = m2;
            }
        }

        // --- Phase 2: exhaustive scan over the tiny residual window ---
        let mut best_sub: u64 = 0;
        let mut best_profit_raw: u128 = 0;
        for t in lo..=hi {
            let p = self.profit_at(t, token_in_decimals, reserves, algebra_states, lfj_states);
            if p > best_profit_raw {
                best_profit_raw = p;
                best_sub = t;
            }
        }

        if best_profit_raw == 0 {
            return (U256::zero(), U256::zero());
        }

        let unit = U256::from(10u64).pow(U256::from(token_in_decimals));
        // Convert sub-tokens to raw atomic amount
        let best_raw = U256::from(best_sub).saturating_mul(unit)
            / U256::from(Self::OPTIMIZER_GRANULARITY);
        // cross-check: re-simulate to get actual out, derive profit cleanly
        let profit = match self.simulate_mixed_path_raw(best_raw, reserves, algebra_states, lfj_states) {
            Some(out) if out > best_raw => out - best_raw,
            _ => U256::from(best_profit_raw),
        };

        (best_raw, profit)
    }

    /// Automatically find the profit-maximising amount_in without a fixed ceiling.
    ///
    /// **Phase 1 — exponential probe**: start at 1 millitoken and double until profit
    /// stops growing (or a hard safety cap is hit). This brackets the peak.
    ///
    /// **Phase 2 — ternary search** in that bracket to locate the exact peak.
    ///
    /// **Phase 3 — exhaustive scan** over the tiny residual window (≤ 3 steps wide).
    ///
    /// Returns `(raw_amount_in_atomic, raw_profit_atomic)`.
    pub fn optimize_amount_in_mixed(
        &self,
        _ignored: U256,                   // kept for call-site compatibility
        v2_reserves: &HashMap<H160, Reserve>,
        algebra_states: &HashMap<H160, AlgebraPoolFull>,
        lfj_states: &HashMap<H160, LFJState>,
    ) -> (U256, U256) {
        let token_in_decimals = if self.zero_for_one_1 {
            self.pool_1.decimals0
        } else {
            self.pool_1.decimals1
        };

        // Hard cap in sub-token units: 1M tokens × GRANULARITY
        const HARD_CAP: u64 = 1_000_000 * 1000; // 1B sub-tokens = 1M whole tokens

        // --- Phase 1: exponential probe to bracket the peak ---
        let mut prev_t: u64 = 0;
        let mut prev_p: u128 = 0;
        // Start at 1 whole token worth of sub-tokens
        let mut probe: u64 = Self::OPTIMIZER_GRANULARITY;
        loop {
            let p = self.profit_at(probe, token_in_decimals, v2_reserves, algebra_states, lfj_states);
            if p <= prev_p {
                // profit stopped growing — peak is in [prev_t, probe]
                break;
            }
            prev_p = p;
            prev_t = probe;
            if probe >= HARD_CAP {
                break;
            }
            probe = (probe * 2).min(HARD_CAP);
        }

        // If profit never went positive just return zero.
        if prev_p == 0 && self.profit_at(probe, token_in_decimals, v2_reserves, algebra_states, lfj_states) == 0 {
            return (U256::zero(), U256::zero());
        }

        // --- Phase 2: ternary search in [prev_t, probe] (in sub-token units) ---
        let result = self.optimize_amount_in(
            probe,
            1,
            v2_reserves,
            algebra_states,
            lfj_states,
        );
        // If the ternary search found nothing better than prev_t, return prev_t directly.
        if result.1 == U256::zero() && prev_p > 0 {
            let unit = U256::from(10u64).pow(U256::from(token_in_decimals));
            let prev_raw = U256::from(prev_t).saturating_mul(unit)
                / U256::from(Self::OPTIMIZER_GRANULARITY);
            let out = self.simulate_mixed_path_raw(prev_raw, v2_reserves, algebra_states, lfj_states);
            let profit = out.map(|o| if o > prev_raw { o - prev_raw } else { U256::zero() }).unwrap_or(U256::zero());
            return (prev_raw, profit);
        }
        result
    }

    pub fn to_path_params(
        &self,
        v2_router: H160,
        _algebra_router: Option<H160>,
        direct_pair: bool,
    ) -> Vec<PathParam> {
        let mut path_params = Vec::new();
        for i in 0..self.nhop {
            let pool = self.get_pool(i);
            let zero_for_one = self.get_zero_for_one(i);

            // Access token addresses from AnyPool
            let (token_in, token_out) = if zero_for_one {
                (pool.token0, pool.token1)
            } else {
                (pool.token1, pool.token0)
            };

            // pool_type encoding understood by V2ArbBot.sol _execute():
            //   0 = V2 / Solidly volatile — _swapV2Pair (x*y=k on-chain calc)
            //   1 = Algebra CL            — _swapAlgebraPool
            //   2 = Solidly stable        — _swapSolidlyStablePair (pending contract update)
            //   3 = UniswapV3CL / Aerodrome CL — _swapUniswapV3Pool (Uniswap V3 fork ABI)
            let (pool_type, router) = match &pool.pool_type {
                PoolType::Algebra(_) => {
                    // Direct pool address; fee handled internally
                    (1u8, pool.address)
                }
                PoolType::UniswapV3CL(_) => {
                    // Direct pool address; pool_type=3 → _swapUniswapV3Pool (Uniswap V3 ABI)
                    (3u8, pool.address)
                }
                PoolType::V2(v2) if v2.version == DexVariant::SolidlyStable => {
                    // Stable pair — same ABI as V2 but uses different on-chain math.
                    // pool_type=2 signals this to the contract (pending support).
                    let r = if direct_pair { pool.address } else { v2_router };
                    (2u8, r)
                }
                PoolType::LFJ(_) => {
                    // LFJ Liquidity Book: call pair directly.
                    // pool_type=4 → _swapLFJPair() in V2ArbBot.sol
                    (4u8, pool.address)
                }
                PoolType::V2(_) => {
                    let r = if direct_pair { pool.address } else { v2_router };
                    (0u8, r)
                }
            };

            // Pass fee in bps for V2/stable/LFJ swaps; 0 for CL pools (fee is in the pool state).
            let fee = match &pool.pool_type {
                PoolType::V2(v2) => v2.fee as u64,
                PoolType::LFJ(lp) => lp.base_fee_bps as u64,
                PoolType::Algebra(_) | PoolType::UniswapV3CL(_) => 0,
            };

            let param = PathParam {
                router,
                token_in,
                token_out,
                pool_type,
                fee,
            };
            path_params.push(param);
        }
        path_params
    }
}

fn wrap_v2(pool: &Pool) -> AnyPool {
    AnyPool {
        pool_type: PoolType::V2(pool.clone()),
        address: pool.address,
        token0: pool.token0,
        token1: pool.token1,
        decimals0: pool.decimals0,
        decimals1: pool.decimals1,
    }
}

/// Generate 2-hop cross-DEX paths for `token_in`.
///
/// Finds every pair of *different* pools that both contain `token_in` paired
/// with the same intermediate token — i.e. the classic cross-DEX case:
///
///   token_in → token_mid  (pool A, DEX 1)
///   token_mid → token_in  (pool B, DEX 2)
///
/// `pool_3` is set to a clone of `pool_1` as an unused placeholder so the
/// existing `ArbPath` struct is satisfied (it is never accessed because
/// `nhop = 2` and all hot-paths loop `for i in 0..self.nhop`).
pub fn generate_cross_dex_paths(
    v2_pools: &[Pool],
    algebra_pools: &[AlgebraPoolFull],
    v3cl_pools: &[AlgebraPoolFull],
    lfj_pools: &[LFJPoolInfo],
    token_in: H160,
) -> Vec<ArbPath> {
    let mut all_pools: Vec<AnyPool> =
        Vec::with_capacity(v2_pools.len() + algebra_pools.len() + v3cl_pools.len() + lfj_pools.len());
    for p in v2_pools {
        all_pools.push(wrap_v2(p));
    }
    for p in algebra_pools {
        all_pools.push(AnyPool {
            pool_type: PoolType::Algebra(p.clone()),
            address: p.address,
            token0: p.token0,
            token1: p.token1,
            decimals0: p.decimals0,
            decimals1: p.decimals1,
        });
    }
    for p in v3cl_pools {
        all_pools.push(AnyPool {
            pool_type: PoolType::UniswapV3CL(p.clone()),
            address: p.address,
            token0: p.token0,
            token1: p.token1,
            decimals0: p.decimals0,
            decimals1: p.decimals1,
        });
    }
    for p in lfj_pools {
        all_pools.push(AnyPool {
            pool_type: PoolType::LFJ(p.clone()),
            address: p.address,
            token0: p.token_x,
            token1: p.token_y,
            decimals0: p.decimals_x,
            decimals1: p.decimals_y,
        });
    }

    // Group pools by the *other* token (i.e. the token_mid for this token_in).
    // Key = token_mid address.
    let mut by_mid: HashMap<H160, Vec<&AnyPool>> = HashMap::new();
    for pool in &all_pools {
        if pool.token0 == token_in {
            by_mid.entry(pool.token1).or_default().push(pool);
        } else if pool.token1 == token_in {
            by_mid.entry(pool.token0).or_default().push(pool);
        }
    }

    let mut paths = Vec::new();

    for (_token_mid, group) in &by_mid {
        if group.len() < 2 {
            continue; // need at least 2 different pools for the same pair
        }
        // For every ordered pair (p1, p2) where p1 != p2:
        //   buy token_mid on p1 using token_in, sell token_mid on p2 for token_in
        for i in 0..group.len() {
            for j in 0..group.len() {
                if i == j {
                    continue;
                }
                let p1 = group[i];
                let p2 = group[j];
                // Direction on each pool
                let zfo1 = p1.token0 == token_in; // true  → token_in is token0
                // On p2 we are selling token_mid to get back token_in.
                // zero_for_one = true when token_mid is token0 of p2.
                let token_mid = if p2.token0 == token_in { p2.token1 } else { p2.token0 };
                let zfo2_correct = p2.token0 == token_mid;

                paths.push(ArbPath {
                    nhop: 2,
                    pool_1: (*p1).clone(),
                    pool_2: (*p2).clone(),
                    // pool_3 is never accessed for nhop=2; use pool_1 as inert placeholder
                    pool_3: (*p1).clone(),
                    zero_for_one_1: zfo1,
                    zero_for_one_2: zfo2_correct,
                    zero_for_one_3: false, // unused
                });
            }
        }
    }

    paths
}

pub fn generate_triangular_paths_mixed(
    v2_pools: &[Pool],
    algebra_pools: &[AlgebraPoolFull],
    v3cl_pools: &[AlgebraPoolFull],
    lfj_pools: &[LFJPoolInfo],
    token_in: H160,
) -> Vec<ArbPath> {
    // Combine all pools into AnyPool list
    let mut all_pools: Vec<AnyPool> =
        Vec::with_capacity(v2_pools.len() + algebra_pools.len() + v3cl_pools.len() + lfj_pools.len());
    
    for p in v2_pools {
        all_pools.push(wrap_v2(p));
    }
    
    for p in algebra_pools {
        all_pools.push(AnyPool {
            pool_type: PoolType::Algebra(p.clone()),
            address: p.address,
            token0: p.token0,
            token1: p.token1,
            decimals0: p.decimals0,
            decimals1: p.decimals1,
        });
    }

    for p in v3cl_pools {
        all_pools.push(AnyPool {
            pool_type: PoolType::UniswapV3CL(p.clone()),
            address: p.address,
            token0: p.token0,
            token1: p.token1,
            decimals0: p.decimals0,
            decimals1: p.decimals1,
        });
    }

    for p in lfj_pools {
        all_pools.push(AnyPool {
            pool_type: PoolType::LFJ(p.clone()),
            address: p.address,
            token0: p.token_x,
            token1: p.token_y,
            decimals0: p.decimals_x,
            decimals1: p.decimals_y,
        });
    }

    let mut paths = Vec::new();
    // Simplified triangular search (only length 3 for now as per previous logic implied)
    // Actually previous logic likely did 2 and 3 hops.
    // I'll implement a basic version that finds A->B->C->A
    
    // Group by token to speed up
    let mut pools_by_token: HashMap<H160, Vec<&AnyPool>> = HashMap::new();
    for pool in &all_pools {
        pools_by_token.entry(pool.token0).or_default().push(pool);
        pools_by_token.entry(pool.token1).or_default().push(pool);
    }
    
    // Step 1: Find pools containing token_in (Start Pools)
    // token_in -> token_a
    let start_pools = if let Some(p) = pools_by_token.get(&token_in) { p } else { return paths; };
    
    for p1 in start_pools {
        let (_token_in_1, token_out_1) = if p1.token0 == token_in {
            (p1.token0, p1.token1) 
        } else { 
            (p1.token1, p1.token0) 
        };
        
        // Step 2: Find pools containing token_out_1 (Middle Pools)
        // token_a -> token_b
        if let Some(second_pools) = pools_by_token.get(&token_out_1) {
            for p2 in second_pools {
                if p2.address == p1.address { continue; }
                
                let (_token_in_2, token_out_2) = if p2.token0 == token_out_1 {
                    (p2.token0, p2.token1)
                } else {
                    (p2.token1, p2.token0)
                };

                // Avoid 2-hop cycles masquerading as 3-hop paths.
                if token_out_2 == token_in {
                    continue;
                }

                // Step 3: Find pools connecting token_out_2 back to token_in (End Pools)
                // token_b -> token_in
                if let Some(third_pools) = pools_by_token.get(&token_out_2) {
                    for p3 in third_pools {
                        if p3.address == p2.address || p3.address == p1.address { continue; }
                        
                        // Check if p3 connects back to token_in
                        if p3.token0 == token_in || p3.token1 == token_in {
                             let zero_for_one_3 = p3.token0 == token_out_2;
                             // We found a path!
                             
                             paths.push(ArbPath {
                                 nhop: 3,
                                 pool_1: (*p1).clone(),
                                 pool_2: (*p2).clone(),
                                 pool_3: (*p3).clone(),
                                 zero_for_one_1: p1.token0 == token_in,
                                 zero_for_one_2: p2.token0 == token_out_1,
                                 zero_for_one_3: zero_for_one_3,
                             });
                        }
                    }
                }
            }
        }
    }
    
    paths
}
