use ethers::types::U256;
use std::collections::HashMap;

pub const Q96: U256 = U256([0, 1u64 << 32, 0, 0]); // 2^96
pub const MIN_TICK: i32 = -887272;
pub const MAX_TICK: i32 = 887272;

/// Calculates sqrt(1.0001^tick) * 2^96
pub fn calculate_sqrt_price_from_tick(tick: i32) -> U256 {
    if tick < MIN_TICK || tick > MAX_TICK {
        return U256::zero();
    }

    let abs_tick = if tick < 0 { -tick } else { tick };

    let mut ratio = if (abs_tick & 0x1) != 0 {
        U256::from("0xfffcb933bd6fad37aa2d162d1a594001")
    } else {
        U256::from("0x100000000000000000000000000000000")
    };
    
    if (abs_tick & 0x2) != 0 {
        ratio = (ratio * U256::from("0xfff97272373d413259a46990580e213a")) >> 128;
    }
    if (abs_tick & 0x4) != 0 {
        ratio = (ratio * U256::from("0xfff2e50f5f656932ef12357cf3c7fdcc")) >> 128;
    }
    if (abs_tick & 0x8) != 0 {
        ratio = (ratio * U256::from("0xffe5caca7e10e4e61c3624eaa0941cd0")) >> 128;
    }
    if (abs_tick & 0x10) != 0 {
        ratio = (ratio * U256::from("0xffcb9843d60f6159c9db58835c926644")) >> 128;
    }
    if (abs_tick & 0x20) != 0 {
        ratio = (ratio * U256::from("0xff973b41fa98c081472e6896dfb254c0")) >> 128;
    }
    if (abs_tick & 0x40) != 0 {
        ratio = (ratio * U256::from("0xff2ea16466c96a3843ec78b326b52861")) >> 128;
    }
    if (abs_tick & 0x80) != 0 {
        ratio = (ratio * U256::from("0xfe5dee046a99a2a811c461f1969c3053")) >> 128;
    }
    if (abs_tick & 0x100) != 0 {
        ratio = (ratio * U256::from("0xfcbe86c7900a88aedcffc5d73274632a")) >> 128;
    }
    if (abs_tick & 0x200) != 0 {
        ratio = (ratio * U256::from("0xf987a7253ac413176f2b074cf7815e54")) >> 128;
    }
    if (abs_tick & 0x400) != 0 {
        ratio = (ratio * U256::from("0xf3392b0822b700059404f63973283853")) >> 128;
    }
    if (abs_tick & 0x800) != 0 {
        ratio = (ratio * U256::from("0xe7159475a2c29b7443b29c7fa6e889d9")) >> 128;
    }
    if (abs_tick & 0x1000) != 0 {
        ratio = (ratio * U256::from("0xd097f3bdfd2022b8845ad8f792aa5825")) >> 128;
    }
    if (abs_tick & 0x2000) != 0 {
        ratio = (ratio * U256::from("0xa9f746462d870fdf8a65dc1f90e061e5")) >> 128;
    }
    if (abs_tick & 0x4000) != 0 {
        ratio = (ratio * U256::from("0x70d869a156d2a1b890bb3df62baf32f7")) >> 128;
    }
    if (abs_tick & 0x8000) != 0 {
        ratio = (ratio * U256::from("0x31be135f97d08fd981231505542fcfa6")) >> 128;
    }
    if (abs_tick & 0x10000) != 0 {
        ratio = (ratio * U256::from("0x9aa508b5b7a84e1c677de54f3e99bc9")) >> 128;
    }
    if (abs_tick & 0x20000) != 0 {
        ratio = (ratio * U256::from("0x5d6af8dedb81196699c329225ee604")) >> 128;
    }
    if (abs_tick & 0x40000) != 0 {
        ratio = (ratio * U256::from("0x2216e584f5fa1ea926041bedfe98")) >> 128;
    }
    if (abs_tick & 0x80000) != 0 {
        ratio = (ratio * U256::from("0x48a170391f7dc42444e8fa2")) >> 128;
    }

    if tick > 0 {
        ratio = U256::MAX / ratio;
    }

    ratio >> 32
}

fn mul_div(a: U256, b: U256, denom: U256) -> Option<U256> {
    if denom.is_zero() {
        return None;
    }
    a.checked_mul(b)?.checked_div(denom)
}

/// Floor-division of `tick` to the nearest multiple of `tick_spacing`.
///
/// Uses Euclidean remainder (always non-negative) so that negative ticks
/// are rounded toward −∞ rather than toward 0 (which Rust's `/` does for
/// integers).
///
/// Example: `floor_tick(-8500, 200)` → `-8600`, not `-8400`.
fn floor_tick(tick: i32, tick_spacing: i32) -> i32 {
    // rem_euclid is always in [0, tick_spacing), so subtraction floors correctly.
    tick - tick.rem_euclid(tick_spacing)
}

/// Calculates amount out for a CL swap, handling multi-tick crossing.
///
/// When the swap would push the price past the current tick's boundary, instead
/// of returning `None` we compute the partial fill up to the boundary, move to
/// the next tick, and adjust liquidity using `tick_data` (liquidityNet) before
/// continuing.  If `tick_data` has no entry for the crossed tick, liquidity
/// stays unchanged (conservative fallback).
///
/// `MAX_TICK_CROSSES` limits the iteration to avoid runaway computation on
/// extremely thin-liquidity pools.
pub fn calculate_amount_out(
    amount_in: U256,
    sqrt_price_x96: U256,
    liquidity: U256,
    fee_pips: u32,
    zero_for_one: bool,
    tick: i32,
    tick_spacing: i32,
    tick_data: &HashMap<i32, i128>,
) -> Option<U256> {
    if amount_in.is_zero() || sqrt_price_x96.is_zero() || liquidity.is_zero() {
        return Some(U256::zero());
    }

    let fee_percent = U256::from(fee_pips);
    let one_million = U256::from(1_000_000);

    if fee_percent >= one_million {
        return None;
    }

    // Apply fee to the total amount_in once (same as Uniswap V3 on-chain).
    let mut remaining_in = mul_div(amount_in, one_million - fee_percent, one_million)?;
    if remaining_in.is_zero() {
        return Some(U256::zero());
    }

    let mut total_out = U256::zero();
    let mut current_sqrt = sqrt_price_x96;
    let mut current_tick = tick;
    let mut current_liq = liquidity;
    const MAX_TICK_CROSSES: u32 = 10;

    for _ in 0..=MAX_TICK_CROSSES {
        if remaining_in.is_zero() || current_liq.is_zero() || current_sqrt.is_zero() {
            break;
        }

        // Compute amount_out for single-tick swap with remaining_in
        let (next_sqrt, amount_out, _amount_consumed) = if zero_for_one {
            // token0 in -> token1 out
            let amount_term = mul_div(remaining_in, current_sqrt, Q96)?;
            let denominator = current_liq.checked_add(amount_term)?;
            if denominator.is_zero() { return None; }
            let next_sqrt_raw = mul_div(current_liq, current_sqrt, denominator)?;
            if current_sqrt <= next_sqrt_raw {
                break; // no price movement
            }
            let out = mul_div(current_liq, current_sqrt - next_sqrt_raw, Q96)?;
            (next_sqrt_raw, out, remaining_in) // all remaining_in consumed in this tick
        } else {
            // token1 in -> token0 out
            let delta = mul_div(remaining_in, Q96, current_liq)?;
            let next_sqrt_raw = current_sqrt.checked_add(delta)?;
            if next_sqrt_raw <= current_sqrt {
                break;
            }
            let num = current_liq
                .checked_mul(Q96)?
                .checked_mul(next_sqrt_raw - current_sqrt)?;
            let den = next_sqrt_raw.checked_mul(current_sqrt)?;
            if den.is_zero() { return None; }
            let out = num.checked_div(den)?;
            (next_sqrt_raw, out, remaining_in)
        };

        // Check tick boundary
        if tick_spacing > 0 {
            let tick_lower = floor_tick(current_tick, tick_spacing);
            let (boundary_sqrt, next_tick) = if zero_for_one {
                (calculate_sqrt_price_from_tick(tick_lower), tick_lower - tick_spacing)
            } else {
                let upper = tick_lower + tick_spacing;
                (calculate_sqrt_price_from_tick(upper), upper)
            };

            let crosses = if zero_for_one {
                next_sqrt < boundary_sqrt
            } else {
                next_sqrt > boundary_sqrt
            };

            if crosses {
                // Partial fill up to boundary
                let partial_out = if zero_for_one {
                    if current_sqrt <= boundary_sqrt { break; }
                    // amount_in that pushes price exactly to boundary:
                    // For token0->token1: amount_in = L * (sqrtP - sqrtP_boundary) / (sqrtP * sqrtP_boundary / Q96)
                    let num_partial = current_liq
                        .checked_mul(Q96)?
                        .checked_mul(current_sqrt - boundary_sqrt)?;
                    let den_partial = current_sqrt.checked_mul(boundary_sqrt)?;
                    if den_partial.is_zero() { break; }
                    let partial_consumed = num_partial.checked_div(den_partial)?;
                    let partial_out = mul_div(current_liq, current_sqrt - boundary_sqrt, Q96)?;
                    remaining_in = remaining_in.saturating_sub(partial_consumed);
                    partial_out
                } else {
                    if boundary_sqrt <= current_sqrt { break; }
                    // amount_in that pushes price exactly to boundary:
                    // For token1->token0: amount_in = L * (sqrtP_boundary - sqrtP) / Q96
                    let partial_consumed = mul_div(current_liq, boundary_sqrt - current_sqrt, Q96)?;
                    let num = current_liq
                        .checked_mul(Q96)?
                        .checked_mul(boundary_sqrt - current_sqrt)?;
                    let den = boundary_sqrt.checked_mul(current_sqrt)?;
                    if den.is_zero() { break; }
                    let partial_out = num.checked_div(den)?;
                    remaining_in = remaining_in.saturating_sub(partial_consumed);
                    partial_out
                };
                total_out = total_out.checked_add(partial_out)?;
                current_sqrt = boundary_sqrt;
                current_tick = next_tick;
                // Apply liquidityNet delta from tick_data (if available).
                // Convention: liquidityNet is the signed change when crossing
                // a tick from left-to-right (price increasing).
                // zero_for_one (price decreasing, right-to-left) → subtract.
                // !zero_for_one (price increasing, left-to-right) → add.
                let boundary_tick = if zero_for_one {
                    tick_lower  // we crossed tick_lower going left
                } else {
                    tick_lower + tick_spacing  // we crossed upper going right
                };
                if let Some(&liq_net) = tick_data.get(&boundary_tick) {
                    let adjustment = if zero_for_one { -liq_net } else { liq_net };
                    if adjustment >= 0 {
                        current_liq = current_liq.saturating_add(U256::from(adjustment as u128));
                    } else {
                        let abs_adj = U256::from((-adjustment) as u128);
                        current_liq = current_liq.saturating_sub(abs_adj);
                    }
                }
                // If current_liq hit zero, the next iteration will break out.
                continue;
            }
        }

        // No tick crossing — consume all remaining input in this tick
        total_out = total_out.checked_add(amount_out)?;
        break;
    }

    // Conservative haircut: deduct 1 bp from the simulated output.
    //
    // On-chain V3 pools use mulDivRoundingUp for intermediate sqrtPrice
    // calculations, which systematically produces slightly less output than our
    // truncating-division simulator.  The 1 bp haircut compensates for this
    // rounding difference.  The larger source of overestimation (stale pool
    // state) is addressed by the targeted confirm-phase refresh in strategy.rs.
    let haircut = total_out / U256::from(10_000u64);
    total_out = total_out.saturating_sub(haircut);

    Some(total_out)
}


