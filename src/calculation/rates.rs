/// Decimal-aware rate estimator + WETH price chain.
///
/// # Design (4-step pipeline)
///
/// ```text
/// Step 1 — Seed:    seed_from_raw_pools() — pre-compute normalized rates for all pools
/// Step 2 — Filter:  is_profitable(path)   — multiply rates along path, reject if ≤ 1e18
/// Step 3 — Simulate: quoter_revm           — exact REVM quote on survivors
/// Step 4 — Execute:  tx_sender_pipeline    — submit profitable bundle
/// ```
///
/// # Decimal normalization
///
/// All stored rates are in "normalized unit value" space:
///   rate = (amount_out / 10^out_dec) / (1 unit of token_in) * 1e18
///        = amount_out * 1e18 / 10^out_dec   (when reference input = 1 unit = 10^in_dec)
///
/// This makes rates consistent across pools regardless of token decimal mismatches.
/// Example — WETH(18)→USDC(6)→DAI(18)→WETH(18):
///   hop1: 3000e6 USDC per 1e18 WETH → rate = 3000e6 * 1e18 / 1e6 = 3000e18
///   hop2: 1e18  DAI per 1e6 USDC    → rate = 1e18  * 1e18 / 1e18 = 1e18
///   hop3: 333e12 WETH per 1e18 DAI  → rate = 333e12 * 1e18 / 1e18 = 333e12
///   cumul: 3000e18 * 1e18 * 333e12 / 1e18² = ~1e18 (break-even ✓)

use alloy::primitives::{Address, U256};
use std::collections::HashMap;

use super::v2;
use super::v3;
use super::aerodrome as aero;
use crate::pool_loader::RawPool;
use crate::state_db::BlockStateDB;
use crate::swap_types::{PoolProtocol, SwapPath};

// ── Constants ─────────────────────────────────────────────────────────────────

/// 1e18 — the "unit rate" (break-even).
pub const RATE_SCALE: U256 = U256::from_limbs([1_000_000_000_000_000_000u64, 0, 0, 0]);

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Returns the canonical "1 unit" reference amount for a token.
/// WETH(18)→1e18, USDC(6)→1e6, WBTC(8)→1e8, etc.
#[inline]
pub fn decimal_reference(decimals: u8) -> U256 {
    U256::from(10u64).pow(U256::from(decimals))
}

/// Compute a decimal-normalized rate.
/// rate = amount_out * 1e18 / 10^out_dec
#[inline]
fn normalized_rate(amount_out: U256, out_decimals: u8) -> U256 {
    let dec = decimal_reference(out_decimals);
    if dec.is_zero() || amount_out.is_zero() {
        return U256::ZERO;
    }
    if let Some(scaled) = amount_out.checked_mul(RATE_SCALE) {
        scaled / dec
    } else {
        // overflow: pre-scale amount_out down
        let shifted_a: U256 = amount_out >> 64;
        let sdec:     U256 = dec >> 64;
        if sdec.is_zero() { return U256::ZERO; }
        shifted_a * RATE_SCALE / sdec
    }
}

// ── RateEstimator ─────────────────────────────────────────────────────────────

/// Per-pool per-direction normalized rate cache.
pub struct RateEstimator {
    /// Key: (pool_address, zero_for_one)
    /// Value: normalized rate (1e18 = break-even)
    pub rates: HashMap<(Address, bool), U256>,

    /// Single-hop WETH price per token.
    /// Key: non-WETH token address
    /// Value: how many WETH wei is 1 canonical unit of this token worth (scaled 1e18)
    /// e.g. USDC → ~333_333_000_000_000 (= 1/3000 WETH per USDC, scaled 1e18)
    pub token_weth_prices: HashMap<Address, U256>,
}

impl RateEstimator {
    pub fn new() -> Self {
        Self {
            rates: HashMap::with_capacity(8192),
            token_weth_prices: HashMap::with_capacity(512),
        }
    }

    // ── Rate update methods ───────────────────────────────────────────────────

    /// Update both directions for a V2-style pool.
    /// `fee_factor` = 10000 − fee_bps (e.g. 9975 for PancakeSwap 0.25%, 9970 for UniV2 0.30%).
    /// Pass the value derived from the CSV `fee` column: `10000 - pool.fee as u64`.
    pub fn update_v2_rate(
        &mut self,
        state_db: &BlockStateDB,
        pool: &Address,
        decimals0: u8,
        decimals1: u8,
        fee_factor: u64,
    ) {
        let Some((raw_r0, raw_r1)) = state_db.read_v2_reserves(pool) else { return };
        if raw_r0.is_zero() || raw_r1.is_zero() {
            return;
        }
        let reserve0: U256 = raw_r0.into();
        let reserve1: U256 = raw_r1.into();
        let fee = fee_factor;

        // zero_for_one (selling token0 → token1)
        let ref0 = decimal_reference(decimals0);
        let out_0to1 = v2::get_amount_out_v2(ref0, reserve0, reserve1, fee);
        self.rates.insert((*pool, true),  normalized_rate(out_0to1, decimals1));

        // one_for_zero (selling token1 → token0)
        let ref1 = decimal_reference(decimals1);
        let out_1to0 = v2::get_amount_out_v2(ref1, reserve1, reserve0, fee);
        self.rates.insert((*pool, false), normalized_rate(out_1to0, decimals0));
    }

    /// Update both directions for a V3 concentrated-liquidity pool.
    pub fn update_v3_rate(
        &mut self,
        state_db: &BlockStateDB,
        pool: &Address,
        decimals0: u8,
        decimals1: u8,
        fee: u32,
        tick_spacing: i32,
    ) {
        let ref0 = decimal_reference(decimals0);
        if let Some(out) = v3::get_amount_out_v3(state_db, pool, ref0, true, fee, tick_spacing) {
            if !out.is_zero() {
                self.rates.insert((*pool, true),  normalized_rate(out, decimals1));
            }
        }

        let ref1 = decimal_reference(decimals1);
        if let Some(out) = v3::get_amount_out_v3(state_db, pool, ref1, false, fee, tick_spacing) {
            if !out.is_zero() {
                self.rates.insert((*pool, false), normalized_rate(out, decimals0));
            }
        }
    }

    /// Store pass-through rates (RATE_SCALE = 1e18) for both swap directions
    /// of an exotic pool (Balancer V2, Curve, Maverick V2).
    ///
    /// This ensures:
    /// - The pool is NOT silently filtered out (zero-rate short-circuit)
    /// - The exotic hop contributes a neutral 1:1 multiplier at the pre-filter stage
    /// - Actual profitability is determined by the REVM quoter (Step 3)
    ///
    /// A path is still rejected at pre-filter if the non-exotic hops together
    /// don't show a cumulative rate > 1e18.
    pub fn update_exotic_passthrough(&mut self, pool: &Address) {
        self.rates.insert((*pool, true),  RATE_SCALE);
        self.rates.insert((*pool, false), RATE_SCALE);
    }

    /// Store fee-discounted rates for a V3 pool whose tick data isn't loaded.
    ///
    /// Uses rate = RATE_SCALE × (1_000_000 − fee) / 1_000_000 so the searcher
    /// correctly accounts for the pool's base swap fee even when the full V3
    /// price simulation isn't possible. This eliminates the vast majority of
    /// false-positive paths that appear profitable only because V3 hops were
    /// treated as lossless.
    pub fn update_v3_passthrough_with_fee(&mut self, pool: &Address, fee: u32) {
        // fee is in hundredths of a bip: 500 = 0.05%, 3000 = 0.3%, 10000 = 1%
        let fee_u64 = fee as u64;
        let discounted = if fee_u64 < 1_000_000 {
            RATE_SCALE * U256::from(1_000_000u64 - fee_u64) / U256::from(1_000_000u64)
        } else {
            U256::ZERO // degenerate: fee >= 100%
        };
        self.rates.insert((*pool, true),  discounted);
        self.rates.insert((*pool, false), discounted);
    }

    /// Compute actual V3 spot rates from the prefetched sqrtPriceX96 (slot0).
    ///
    /// This encodes the **real exchange rate** of the pool (not just the fee),
    /// eliminating the massive false-positive problem where the searcher treats
    /// all V3 pools as 1:1-minus-fee regardless of actual price.
    ///
    /// Math: sqrtPriceX96 = sqrt(token1/token0) × 2^96
    ///   0→1: out = in × sqrtP² / 2^192 × (1 − fee)
    ///   1→0: out = in × 2^192 / sqrtP² × (1 − fee)
    /// We split into two steps to avoid U256 overflow.
    pub fn update_v3_spot_rate(
        &mut self,
        state_db: &BlockStateDB,
        pool: &Address,
        decimals0: u8,
        decimals1: u8,
        fee: u32,
    ) {
        let Some((sqrt_price_x96, _tick)) = state_db.read_v3_slot0(pool) else { return };
        if sqrt_price_x96.is_zero() { return; }

        let q96 = U256::from(1u64) << 96;
        let fee_num = U256::from(1_000_000u64.saturating_sub(fee.min(999_999) as u64));
        let fee_den = U256::from(1_000_000u64);

        // 0→1: out = ref0 × sqrtP / 2^96 × sqrtP / 2^96 × (1−fee)
        let ref0 = decimal_reference(decimals0);
        if let Some(step1) = ref0.checked_mul(sqrt_price_x96) {
            let s1: U256 = step1 >> 96;
            if let Some(step2) = s1.checked_mul(sqrt_price_x96) {
                let raw_out: U256 = step2 >> 96;
                let out_after_fee = raw_out * fee_num / fee_den;
                let rate = normalized_rate(out_after_fee, decimals1);
                if !rate.is_zero() {
                    self.rates.insert((*pool, true), rate);
                }
            }
        }

        // 1→0: out = ref1 × 2^96 / sqrtP × 2^96 / sqrtP × (1−fee)
        let ref1 = decimal_reference(decimals1);
        if let Some(step1) = ref1.checked_mul(q96) {
            let s1: U256 = step1 / sqrt_price_x96;
            if let Some(step2) = s1.checked_mul(q96) {
                let raw_out: U256 = step2 / sqrt_price_x96;
                let out_after_fee = raw_out * fee_num / fee_den;
                let rate = normalized_rate(out_after_fee, decimals0);
                if !rate.is_zero() {
                    self.rates.insert((*pool, false), rate);
                }
            }
        }
    }

    /// Update both directions for an Aerodrome/Velodrome pool.
    pub fn update_aerodrome_rate(
        &mut self,
        state_db: &BlockStateDB,
        pool: &Address,
        decimals0: u8,
        decimals1: u8,
        stable: bool,
        fee_bps: u64,
    ) {
        let Some((raw_r0, raw_r1)) = state_db.read_v2_reserves(pool) else { return };
        if raw_r0.is_zero() || raw_r1.is_zero() { return; }
        let reserve0: U256 = raw_r0.into();
        let reserve1: U256 = raw_r1.into();

        let ref0 = decimal_reference(decimals0);
        let out_0to1 = aero::get_amount_out_aerodrome(
            ref0, reserve0, reserve1, decimals0, decimals1, stable, fee_bps, true,
        );
        self.rates.insert((*pool, true),  normalized_rate(out_0to1, decimals1));

        let ref1 = decimal_reference(decimals1);
        let out_1to0 = aero::get_amount_out_aerodrome(
            ref1, reserve0, reserve1, decimals0, decimals1, stable, fee_bps, false,
        );
        self.rates.insert((*pool, false), normalized_rate(out_1to0, decimals0));
    }

    // ── Seeding ───────────────────────────────────────────────────────────────

    /// Seed rates from a `RawPool` slice.
    ///
    /// Phase 1: compute & store normalized rates for every pool/direction.
    /// Phase 2: for every pool containing `weth_addr`, also store the other
    ///          token's direct WETH price in `token_weth_prices`.
    ///
    /// This replaces the old `seed_from_pool_list` tuple API.
    pub fn seed_from_raw_pools(
        &mut self,
        pools: &[RawPool],
        weth_addr: Address,
        state_db: &BlockStateDB,
    ) {
        // ── Phase 1: all rates ────────────────────────────────────────────────
        for pool in pools {
            if pool.protocol.is_v3() {
                self.update_v3_rate(
                    state_db, &pool.address,
                    pool.decimals0, pool.decimals1,
                    pool.fee, pool.tick_spacing,
                );
            } else if matches!(pool.protocol, PoolProtocol::Aerodrome) {
                // Determine stable flag from tick_spacing convention (0 = stable, else volatile)
                let stable = pool.tick_spacing == 0;
                let fee_bps = pool.fee as u64;
                self.update_aerodrome_rate(
                    state_db, &pool.address,
                    pool.decimals0, pool.decimals1, stable, fee_bps,
                );
            } else if matches!(
                pool.protocol,
                PoolProtocol::BalancerV2
                    | PoolProtocol::CurveTwoCrypto
                    | PoolProtocol::CurveTriCrypto
                    | PoolProtocol::MaverickV2
            ) {
                // Exotic pools: store neutral pass-through so they are not silently
                // dropped by the cheap pre-filter. Exact amounts are evaluated by
                // the REVM-based quoter in Step 3.
                self.update_exotic_passthrough(&pool.address);
            } else {
                // Derive fee_factor from the CSV fee field (in basis-points).
                // e.g. PancakeSwap V2 BSC: fee=25 → 9975; UniswapV2: fee=30 → 9970.
                // Clamp to [9900, 10000) to guard against bad CSV data; fall back to
                // protocol default if the CSV field is 0 or implausible.
                let csv_factor = 10000u64.saturating_sub(pool.fee as u64);
                let fee_factor = if csv_factor >= 9900 && csv_factor < 10000 {
                    csv_factor
                } else {
                    pool.protocol.v2_fee_factor()
                };
                self.update_v2_rate(
                    state_db, &pool.address,
                    pool.decimals0, pool.decimals1, fee_factor,
                );
            }
        }

        // ── Phase 2: WETH price for tokens paired directly with WETH ─────────
        for pool in pools {
            let other_token;
            let other_dec;
            let zero_for_one; // direction: other → WETH
            if pool.token0 == weth_addr {
                other_token = pool.token1;
                other_dec   = pool.decimals1;
                zero_for_one = false; // selling token1 for token0=WETH
            } else if pool.token1 == weth_addr {
                other_token = pool.token0;
                other_dec   = pool.decimals0;
                zero_for_one = true;  // selling token0 for token1=WETH
            } else {
                continue;
            }
            // Look up already-computed rate for this direction
            let key = (pool.address, zero_for_one);
            if let Some(&rate) = self.rates.get(&key) {
                if !rate.is_zero() {
                    // rate is in "WETH per 1 canonical unit of other_token" terms
                    self.token_weth_prices.entry(other_token).or_insert(rate);
                }
            }
            let _ = other_dec; // used for future multi-hop expansion
        }

        log::debug!(
            "[RateEstimator] Seeded {} rate entries, {} WETH prices from {} pools",
            self.rates.len(),
            self.token_weth_prices.len(),
            pools.len(),
        );
    }

    // ── Path evaluation ───────────────────────────────────────────────────────

    /// Evaluate a SwapPath by multiplying per-step rates.
    /// Returns cumulative rate (1e18 = break-even). > 1e18 → profitable before gas.
    pub fn evaluate_path(&self, path: &SwapPath) -> U256 {
        let mut cumulative = RATE_SCALE;
        for step in &path.steps {
            let zero_for_one = step.token_in < step.token_out;
            let rate = self
                .rates
                .get(&(step.pool_address, zero_for_one))
                .copied()
                .unwrap_or(U256::ZERO);
            if rate.is_zero() {
                return U256::ZERO;
            }
            cumulative = mul_div(cumulative, rate, RATE_SCALE);
            if cumulative.is_zero() {
                return U256::ZERO;
            }
        }
        cumulative
    }

    /// Quick check: is a path profitable (cumulative rate > 1e18)?
    #[inline]
    pub fn is_profitable(&self, path: &SwapPath) -> bool {
        self.evaluate_path(path) > RATE_SCALE
    }

    /// Expected profit margin: (cumulative_rate - 1e18), clamped to 0.
    pub fn expected_profit_rate(&self, path: &SwapPath) -> U256 {
        let rate = self.evaluate_path(path);
        if rate > RATE_SCALE { rate - RATE_SCALE } else { U256::ZERO }
    }

    // ── WETH pricing ──────────────────────────────────────────────────────────

    /// Return the stored WETH price for `token` if a direct pool was found.
    /// Value = "WETH wei received per 1 canonical unit of `token`" (1e18 scale).
    pub fn weth_price_of(&self, token: &Address) -> Option<U256> {
        self.token_weth_prices.get(token).copied()
    }

    /// Convert an amount of `token` (raw units, `decimals` decimal places) to
    /// its approximate WETH value using the stored direct price.
    ///
    /// Returns `None` if no WETH price is known for the token.
    pub fn weth_value_of(&self, token: &Address, amount: U256, decimals: u8) -> Option<U256> {
        let price = self.weth_price_of(token)?; // WETH per 1 canonical unit, scaled 1e18
        let ref_unit = decimal_reference(decimals);
        if ref_unit.is_zero() { return None; }
        // weth_out = amount * price / ref_unit
        Some(mul_div(amount, price, ref_unit))
    }

    // ── Cache management ──────────────────────────────────────────────────────

    /// Invalidate all rates for a single pool (call when its state changes).
    pub fn invalidate_pool(&mut self, pool: &Address) {
        self.rates.remove(&(*pool, true));
        self.rates.remove(&(*pool, false));
    }

    /// Number of cached rate entries (diagnostics / tests).
    #[inline]
    pub fn pool_count(&self) -> usize {
        self.rates.len()
    }

    /// Number of tokens with a known WETH price.
    #[inline]
    pub fn weth_price_count(&self) -> usize {
        self.token_weth_prices.len()
    }

    /// Clear all cached rates and WETH prices.
    pub fn clear(&mut self) {
        self.rates.clear();
        self.token_weth_prices.clear();
    }
}

// ── FullMath mul_div ──────────────────────────────────────────────────────────

/// (a * b) / d with overflow protection (Uniswap FullMath pattern).
fn mul_div(a: U256, b: U256, d: U256) -> U256 {
    if d.is_zero() { return U256::ZERO; }
    if let Some(product) = a.checked_mul(b) {
        product / d
    } else {
        let shifted_a: U256 = a >> 64;
        let shifted_d: U256 = d >> 64;
        if shifted_d.is_zero() { return U256::MAX; }
        (shifted_a * b) / shifted_d
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mul_div_basic() {
        let a = U256::from(1_000_000_000_000_000_000u64); // 1e18
        let b = U256::from(2_000_000_000_000_000_000u128); // 2e18
        let d = U256::from(1_000_000_000_000_000_000u64); // 1e18
        assert_eq!(mul_div(a, b, d), U256::from(2_000_000_000_000_000_000u128));
    }

    #[test]
    fn test_profitable_detection() {
        let estimator = RateEstimator::new();
        let path = SwapPath::new(vec![]);
        let rate = estimator.evaluate_path(&path);
        assert_eq!(rate, RATE_SCALE); // empty path = 1:1
    }

    #[test]
    fn test_decimal_reference() {
        assert_eq!(decimal_reference(18), U256::from(1_000_000_000_000_000_000u64));
        assert_eq!(decimal_reference(6),  U256::from(1_000_000u64));
        assert_eq!(decimal_reference(8),  U256::from(100_000_000u64));
    }
}
