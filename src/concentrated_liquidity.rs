use ethers::types::U256;

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

/// Calculates amount out for a single-tick CL swap approximation.
///
/// **Tick-boundary guard**: the single-tick formula assumes the entire
/// `liquidity` is available throughout the swap.  When the swap would push
/// the price past the current tick's boundary, the formula over-estimates
/// output because real liquidity changes at each tick.  This function
/// returns `None` in that case, signalling to the off-chain optimizer that
/// the requested `amount_in` is too large for this tick — it will
/// automatically find the maximum amount that stays within the tick.
///
/// `tick` and `tick_spacing` must come from the pool's current on-chain
/// state.  Pass `tick_spacing = 0` to skip the guard (legacy behaviour).
pub fn calculate_amount_out(
    amount_in: U256,
    sqrt_price_x96: U256,
    liquidity: U256,
    fee_pips: u32,
    zero_for_one: bool,
    tick: i32,
    tick_spacing: i32,
) -> Option<U256> {
    if amount_in.is_zero() || sqrt_price_x96.is_zero() || liquidity.is_zero() {
        return Some(U256::zero());
    }

    let fee_percent = U256::from(fee_pips);
    let one_million = U256::from(1_000_000);

    if fee_percent >= one_million {
        return None;
    }

    let amount_in_less_fee = mul_div(amount_in, one_million - fee_percent, one_million)?;
    if amount_in_less_fee.is_zero() {
        return Some(U256::zero());
    }

    if zero_for_one {
        // token0 in -> token1 out
        // sqrtP' = (L*sqrtP) / (L + amountIn*sqrtP/Q96)
        let amount_term = mul_div(amount_in_less_fee, sqrt_price_x96, Q96)?;
        let denominator = liquidity.checked_add(amount_term)?;
        if denominator.is_zero() {
            return None;
        }

        let next_sqrt = mul_div(liquidity, sqrt_price_x96, denominator)?;
        if sqrt_price_x96 <= next_sqrt {
            return Some(U256::zero());
        }

        // Tick-boundary guard: reject if the swap crosses the current tick's
        // lower boundary (price would move through a tick with different liquidity).
        if tick_spacing > 0 {
            let tick_lower = floor_tick(tick, tick_spacing);
            let sqrt_lower = calculate_sqrt_price_from_tick(tick_lower);
            if next_sqrt < sqrt_lower {
                return None; // swap crosses tick boundary — amount_in is too large
            }
        }

        // amount1 out = L * (sqrtP - sqrtP') / Q96
        mul_div(liquidity, sqrt_price_x96 - next_sqrt, Q96)
    } else {
        // token1 in -> token0 out
        // sqrtP' = sqrtP + amountIn*Q96/L
        let delta = mul_div(amount_in_less_fee, Q96, liquidity)?;
        let next_sqrt = sqrt_price_x96.checked_add(delta)?;

        if next_sqrt <= sqrt_price_x96 {
            return Some(U256::zero());
        }

        // Tick-boundary guard: reject if the swap crosses the current tick's
        // upper boundary (price would move through a tick with different liquidity).
        if tick_spacing > 0 {
            let tick_lower = floor_tick(tick, tick_spacing);
            let sqrt_upper = calculate_sqrt_price_from_tick(tick_lower + tick_spacing);
            if next_sqrt > sqrt_upper {
                return None; // swap crosses tick boundary — amount_in is too large
            }
        }

        // amount0 out = (L<<96) * (sqrtP' - sqrtP) / (sqrtP' * sqrtP)
        let num = liquidity
            .checked_mul(Q96)?
            .checked_mul(next_sqrt - sqrt_price_x96)?;
        let den = next_sqrt.checked_mul(sqrt_price_x96)?;
        if den.is_zero() {
            return None;
        }
        num.checked_div(den)
    }
}

pub fn simulate_cl_swap(
    amount_in: U256,
    sqrt_price_x96: U256,
    liquidity: U256,
    fee: u32,
    zero_for_one: bool,
    tick: i32,
    tick_spacing: i32,
) -> Option<U256> {
    calculate_amount_out(amount_in, sqrt_price_x96, liquidity, fee, zero_for_one, tick, tick_spacing)
}
