/// V2 constant-product AMM math — pure alloy U256.
///
/// Supports per-DEX fee factors (UniswapV2 = 9970, PancakeSwap = 9975, etc.)

use alloy::primitives::U256;

/// Compute amount_out for a V2-style constant-product swap.
///
/// `fee_factor` = 10000 - (fee_bps) scaled so that the numerator includes the fee.
/// e.g. UniswapV2 = 9970 (0.3% fee), PancakeSwapV2 = 9975 (0.25%), AlienBase = 9984.
#[inline]
pub fn get_amount_out_v2(
    amount_in: U256,
    reserve_in: U256,
    reserve_out: U256,
    fee_factor: u64,
) -> U256 {
    if amount_in.is_zero() || reserve_in.is_zero() || reserve_out.is_zero() {
        return U256::ZERO;
    }
    let amount_in_with_fee = amount_in * U256::from(fee_factor);
    let numerator = amount_in_with_fee * reserve_out;
    let denominator = reserve_in * U256::from(10_000u64) + amount_in_with_fee;
    if denominator.is_zero() {
        return U256::ZERO;
    }
    numerator / denominator
}

/// Compute amount_in required to receive `amount_out` from a V2-style swap.
#[inline]
pub fn get_amount_in_v2(
    amount_out: U256,
    reserve_in: U256,
    reserve_out: U256,
    fee_factor: u64,
) -> U256 {
    if amount_out.is_zero() || reserve_in.is_zero() || reserve_out <= amount_out {
        return U256::MAX; // impossible
    }
    let numerator = reserve_in * amount_out * U256::from(10_000u64);
    let denominator = (reserve_out - amount_out) * U256::from(fee_factor);
    if denominator.is_zero() {
        return U256::MAX;
    }
    numerator / denominator + U256::from(1u64) // round up
}

/// Compute the rate (scaled by 1e18) for a V2 pool.
/// rate = (amount_out * 1e18) / amount_in
///
/// Used by the estimator to quickly rank paths.
#[inline]
pub fn v2_rate_1e18(
    amount_in: U256,
    reserve_in: U256,
    reserve_out: U256,
    fee_factor: u64,
) -> U256 {
    let amount_out = get_amount_out_v2(amount_in, reserve_in, reserve_out, fee_factor);
    if amount_in.is_zero() {
        return U256::ZERO;
    }
    const SCALE: U256 = U256::from_limbs([1_000_000_000_000_000_000u64, 0, 0, 0]); // 1e18
    (amount_out * SCALE) / amount_in
}

/// Compute the effective rate without specifying amount_in.
/// Uses the standard formula: rate = (fee_factor * reserve_out) / (10000 * reserve_in + fee_factor * 1)
/// This approximates rate for infinitesimally small trades.
#[inline]
pub fn v2_marginal_rate_1e18(
    reserve_in: U256,
    reserve_out: U256,
    fee_factor: u64,
) -> U256 {
    if reserve_in.is_zero() {
        return U256::ZERO;
    }
    const SCALE: U256 = U256::from_limbs([1_000_000_000_000_000_000u64, 0, 0, 0]);
    (U256::from(fee_factor) * reserve_out * SCALE) / (reserve_in * U256::from(10_000u64))
}

/// Compute optimal amount_in (Sqrt-based approach) to maximize profit
/// for an arb path where this pool is the first hop.
///
/// The formula for optimal input in a 2-hop arb is:
///   amount_in* = sqrt(r0 * r1 * f0 * f1 / 10000^2) - r0 * 10000 / f0
/// but for multi-hop we use a ternary/binary search in the simulator.
///
/// This gives a rough starting point for a single V2 pool.
#[inline]
pub fn optimal_v2_input(
    reserve_in: U256,
    reserve_out: U256,
    fee_factor: u64,
) -> U256 {
    // Simplified: sqrt(reserve_in * reserve_out * fee_factor / 10000) - reserve_in
    let product = reserve_in * reserve_out * U256::from(fee_factor);
    let sqrt_prod = isqrt(product / U256::from(10_000u64));
    if sqrt_prod > reserve_in {
        sqrt_prod - reserve_in
    } else {
        U256::ZERO
    }
}

/// Integer square root using Newton's method.
pub fn isqrt(n: U256) -> U256 {
    if n.is_zero() {
        return U256::ZERO;
    }
    let mut x = n;
    let mut y = (x + U256::from(1u64)) >> 1;
    while y < x {
        x = y;
        y = (x + n / x) >> 1;
    }
    x
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_v2_amount_out() {
        let reserve_in = U256::from(1_000_000u64); // 1M
        let reserve_out = U256::from(2_000_000u64); // 2M
        let amount_in = U256::from(1000u64);
        let out = get_amount_out_v2(amount_in, reserve_in, reserve_out, 9970);
        assert!(out > U256::ZERO);
        assert!(out < U256::from(2000u64)); // should be less than 2x
    }

    #[test]
    fn test_v2_roundtrip() {
        let r0 = U256::from(10_000_000u64);
        let r1 = U256::from(20_000_000u64);
        let amount_in = U256::from(50_000u64);
        let out = get_amount_out_v2(amount_in, r0, r1, 9970);
        let back = get_amount_in_v2(out, r0 + amount_in, r1 - out, 9970);
        // back should be close to amount_in (minus fees)
        assert!(back > U256::ZERO);
    }

    #[test]
    fn test_isqrt() {
        assert_eq!(isqrt(U256::from(100u64)), U256::from(10u64));
        assert_eq!(isqrt(U256::from(0u64)), U256::from(0u64));
        assert_eq!(isqrt(U256::from(1u64)), U256::from(1u64));
        assert_eq!(isqrt(U256::from(99u64)), U256::from(9u64));
    }
}
