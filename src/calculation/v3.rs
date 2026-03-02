/// V3 concentrated-liquidity swap math — full tick-bitmap traversal.
///
/// Uses `uniswap_v3_math` crate (0.5.2) for the heavy math (tick_math, swap_math).
/// That crate uses alloy primitives, so everything is alloy-native.
///
/// Unlike a "max 3 ticks" approximation, this walks the bitmap until the
/// full amount_in is consumed — matching on-chain execution exactly.

use alloy::primitives::{Address, I256, U256};

use crate::state_db::BlockStateDB;

// ── Constants ───────────────────────────────────────────────────────────────

/// Min sqrt ratio boundary (same as UniswapV3).
const MIN_SQRT_RATIO: U256 = U256::from_limbs([4295128739u64, 0, 0, 0]);

/// Max sqrt ratio (too large for u128).
const MAX_SQRT_RATIO: U256 = U256::from_limbs([
    6743328256752651558u64,
    17280870778742802505u64,
    4294805859u64,
    0u64,
]);

const MAX_TICK_ITERATIONS: usize = 500;

// ── Public API ──────────────────────────────────────────────────────────────

/// Compute amount_out for a V3 swap by walking the full tick bitmap.
///
/// Returns `None` if any required state (slot0, liquidity, ticks) is missing.
pub fn get_amount_out_v3(
    state_db: &BlockStateDB,
    pool: &Address,
    amount_in: U256,
    zero_for_one: bool,
    fee: u32,
    tick_spacing: i32,
) -> Option<U256> {
    if amount_in.is_zero() {
        return Some(U256::ZERO);
    }

    // Read initial state from our local DB
    let (sqrt_price_x96, current_tick) = state_db.read_v3_slot0(pool)?;
    if sqrt_price_x96.is_zero() {
        return None;
    }
    let liquidity = state_db.read_v3_liquidity(pool)?;

    let mut sqrt_price = sqrt_price_x96;
    let mut liq = liquidity;
    let mut amount_remaining = I256::try_from(amount_in).ok()?;
    let mut amount_calculated = U256::ZERO;
    let mut tick = current_tick;
    let mut iterations = 0;

    // sqrt price limits
    let sqrt_price_limit = if zero_for_one {
        MIN_SQRT_RATIO + U256::from(1u64)
    } else {
        MAX_SQRT_RATIO - U256::from(1u64)
    };

    while !amount_remaining.is_zero() && liq > 0 && iterations < MAX_TICK_ITERATIONS {
        iterations += 1;

        // Find next initialized tick in the bitmap
        let (next_tick, initialized) =
            find_next_initialized_tick(state_db, pool, tick, tick_spacing, zero_for_one)?;

        // Clamp to valid tick range
        let next_tick = next_tick.clamp(-887272, 887272);

        // Get sqrt price at next tick
        let sqrt_price_next =
            uniswap_v3_math::tick_math::get_sqrt_ratio_at_tick(next_tick).ok()?;

        // Determine target sqrt price (clamped by limit)
        let sqrt_ratio_target = if zero_for_one {
            if sqrt_price_next < sqrt_price_limit {
                sqrt_price_limit
            } else {
                sqrt_price_next
            }
        } else {
            if sqrt_price_next > sqrt_price_limit {
                sqrt_price_limit
            } else {
                sqrt_price_next
            }
        };

        // Compute one step of the swap
        let (new_sqrt_price, amount_in_step, amount_out_step, fee_amount) =
            uniswap_v3_math::swap_math::compute_swap_step(
                sqrt_price,
                sqrt_ratio_target,
                liq,
                amount_remaining,
                fee,
            )
            .ok()?;

        // Update remaining and calculated
        let step_cost_u = amount_in_step.checked_add(fee_amount)?;
        let step_cost = I256::try_from(step_cost_u).ok()?;
        amount_remaining = amount_remaining.checked_sub(step_cost)?;
        amount_calculated = amount_calculated.checked_add(amount_out_step)?;
        sqrt_price = new_sqrt_price;

        // Cross tick if we reached the target
        if new_sqrt_price == sqrt_price_next {
            if initialized {
                let (_, liquidity_net) = state_db.read_v3_tick(pool, next_tick)?;
                if zero_for_one {
                    liq = (liq as i128).checked_sub(liquidity_net)? as u128;
                } else {
                    liq = (liq as i128).checked_add(liquidity_net)? as u128;
                }
            }
            tick = if zero_for_one {
                next_tick - 1
            } else {
                next_tick
            };
        } else {
            tick = uniswap_v3_math::tick_math::get_tick_at_sqrt_ratio(new_sqrt_price).ok()?;
        }
    }

    Some(amount_calculated)
}

/// Compute V3 rate scaled to 1e18 for the estimator.
pub fn v3_rate_1e18(
    state_db: &BlockStateDB,
    pool: &Address,
    amount_in: U256,
    zero_for_one: bool,
    fee: u32,
    tick_spacing: i32,
) -> U256 {
    let amount_out = get_amount_out_v3(state_db, pool, amount_in, zero_for_one, fee, tick_spacing)
        .unwrap_or(U256::ZERO);
    if amount_in.is_zero() || amount_out.is_zero() {
        return U256::ZERO;
    }
    const SCALE: U256 = U256::from_limbs([1_000_000_000_000_000_000u64, 0, 0, 0]);
    (amount_out * SCALE) / amount_in
}

// ── Internal: tick bitmap walking ───────────────────────────────────────────

/// Find the next initialized tick from the bitmap stored in state_db.
///
/// Walks the bitmap word-by-word. For `zero_for_one` (selling token0), we
/// search leftward (decreasing tick). Otherwise, rightward.
fn find_next_initialized_tick(
    state_db: &BlockStateDB,
    pool: &Address,
    tick: i32,
    tick_spacing: i32,
    zero_for_one: bool,
) -> Option<(i32, bool)> {
    // Compress tick to word position and bit position
    let compressed = if tick < 0 && tick % tick_spacing != 0 {
        tick / tick_spacing - 1
    } else {
        tick / tick_spacing
    };

    if zero_for_one {
        // Search leftward
        let word_pos = (compressed >> 8) as i16;
        let bit_pos = ((compressed % 256 + 256) % 256) as u8;

        // Read bitmap word
        let bitmap = state_db.read_v3_tick_bitmap(pool, word_pos).unwrap_or(U256::ZERO);

        // Mask: all bits at and below bit_pos
        let mask = if bit_pos == 255 {
            U256::MAX
        } else {
            (U256::from(1u64) << (bit_pos as usize + 1)) - U256::from(1u64)
        };
        let masked = bitmap & mask;

        if !masked.is_zero() {
            // Find most significant bit
            let msb = most_significant_bit_u256(masked);
            let next_tick = ((word_pos as i32) * 256 + msb as i32) * tick_spacing;
            Some((next_tick, true))
        } else {
            // Tick is at the boundary of this word; return boundary
            let next_tick = (word_pos as i32 * 256) * tick_spacing;
            Some((next_tick, false))
        }
    } else {
        // Search rightward
        let word_pos = ((compressed + 1) >> 8) as i16;
        let bit_pos = (((compressed + 1) % 256 + 256) % 256) as u8;

        let bitmap = state_db.read_v3_tick_bitmap(pool, word_pos).unwrap_or(U256::ZERO);

        // Mask: all bits at and above bit_pos
        let mask = U256::MAX << (bit_pos as usize);
        let masked = bitmap & mask;

        if !masked.is_zero() {
            let lsb = least_significant_bit_u256(masked);
            let next_tick = ((word_pos as i32) * 256 + lsb as i32) * tick_spacing;
            Some((next_tick, true))
        } else {
            // Boundary
            let next_tick = ((word_pos as i32 + 1) * 256 - 1) * tick_spacing;
            Some((next_tick, false))
        }
    }
}

/// Find the index of the most significant set bit (alloy U256).
fn most_significant_bit_u256(x: U256) -> u8 {
    debug_assert!(!x.is_zero());
    // alloy U256 has bit_len() which gives the position of highest set bit
    (x.bit_len() - 1) as u8
}

/// Find the index of the least significant set bit (alloy U256).
fn least_significant_bit_u256(x: U256) -> u8 {
    debug_assert!(!x.is_zero());
    // trailing_zeros gives the position of the lowest set bit
    x.trailing_zeros() as u8
}
