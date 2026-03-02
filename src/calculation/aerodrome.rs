/// Aerodrome / Velodrome stable and volatile AMM math.
///
/// Ported from BaseBuster's `calculation/aerodrome.rs`.
/// Standalone pure functions — no DB or provider dependency.
///
/// Aerodrome has two pool types:
///   - **volatile**: standard constant-product (x*y=k), effectively identical to
///     UniswapV2 but with a different default fee tier.
///   - **stable**: x³y + xy³ = k invariant, normalised by token decimals. Used for
///     correlated pairs (stablecoins, LSTs, etc.).
///
/// Fee convention: `fee_bps` is the raw basis-point fee (e.g. 30 = 0.30%, 5 = 0.05%).

use alloy::primitives::U256;

// ── Constants ────────────────────────────────────────────────────────────────

/// Default fee for volatile Aerodrome pools (0.30% = 30 bps → factor 9970).
pub const AERODROME_VOLATILE_FEE_BPS: u64 = 30;
/// Default fee for stable Aerodrome pools (0.05% = 5 bps).
pub const AERODROME_STABLE_FEE_BPS: u64 = 5;

/// 1e18 as U256 — used for stable pool decimal normalisation.
const ONE_E18: U256 = U256::from_limbs([1_000_000_000_000_000_000u64, 0, 0, 0]);

// ── Public API ───────────────────────────────────────────────────────────────

/// Compute amount_out for an Aerodrome pool (volatile or stable).
///
/// # Parameters
/// - `amount_in`: token amount being sold (in token units, not normalised)
/// - `reserve0`, `reserve1`: raw on-chain reserves
/// - `token0_decimals`, `token1_decimals`: ERC-20 decimals for each token
/// - `stable`: true → stable invariant; false → volatile (constant-product)
/// - `fee_bps`: pool fee in basis points (e.g. 30 = 0.30%)
/// - `zero_for_one`: true = selling token0, false = selling token1
pub fn get_amount_out_aerodrome(
    amount_in: U256,
    reserve0: U256,
    reserve1: U256,
    token0_decimals: u8,
    token1_decimals: u8,
    stable: bool,
    fee_bps: u64,
    zero_for_one: bool,
) -> U256 {
    if amount_in.is_zero() || reserve0.is_zero() || reserve1.is_zero() {
        return U256::ZERO;
    }

    // Deduct fee from amount_in
    let fee_numerator = U256::from(10_000u64 - fee_bps);
    let amount_in_after_fee = (amount_in * fee_numerator) / U256::from(10_000u64);

    let dec0 = U256::from(10u64).pow(U256::from(token0_decimals));
    let dec1 = U256::from(10u64).pow(U256::from(token1_decimals));

    if stable {
        get_amount_out_stable(
            amount_in_after_fee,
            reserve0,
            reserve1,
            dec0,
            dec1,
            zero_for_one,
        )
    } else {
        get_amount_out_volatile(amount_in_after_fee, reserve0, reserve1, zero_for_one)
    }
}

/// Rate (scaled 1e18) for a single aerodrome pool swap, used by the estimator.
pub fn aerodrome_rate_1e18(
    reference_amount: U256,
    reserve0: U256,
    reserve1: U256,
    token0_decimals: u8,
    token1_decimals: u8,
    stable: bool,
    fee_bps: u64,
    zero_for_one: bool,
) -> U256 {
    let out = get_amount_out_aerodrome(
        reference_amount,
        reserve0,
        reserve1,
        token0_decimals,
        token1_decimals,
        stable,
        fee_bps,
        zero_for_one,
    );
    if reference_amount.is_zero() {
        return U256::ZERO;
    }
    (out * ONE_E18) / reference_amount
}

// ── Internal helpers ─────────────────────────────────────────────────────────

/// Volatile pool: constant-product (x*y=k), fee already deducted from amount_in.
#[inline]
fn get_amount_out_volatile(
    amount_in: U256, // fee already deducted
    reserve0: U256,
    reserve1: U256,
    zero_for_one: bool,
) -> U256 {
    let (reserve_in, reserve_out) = if zero_for_one {
        (reserve0, reserve1)
    } else {
        (reserve1, reserve0)
    };

    // Standard constant-product: out = amount_in * reserve_out / (reserve_in + amount_in)
    if reserve_in.is_zero() {
        return U256::ZERO;
    }
    (amount_in * reserve_out) / (reserve_in + amount_in)
}

/// Stable pool: x³y + xy³ = k invariant, decimal-normalised.
fn get_amount_out_stable(
    amount_in: U256, // fee already deducted
    reserve0: U256,
    reserve1: U256,
    dec0: U256,
    dec1: U256,
    zero_for_one: bool,
) -> U256 {
    // Compute the k invariant
    let xy = k_stable(reserve0, reserve1, dec0, dec1);

    // Normalise reserves to 1e18
    let r0_norm = if dec0.is_zero() {
        U256::ZERO
    } else {
        (reserve0 * ONE_E18) / dec0
    };
    let r1_norm = if dec1.is_zero() {
        U256::ZERO
    } else {
        (reserve1 * ONE_E18) / dec1
    };

    // Normalise amount_in
    let (amount_in_norm, reserve_a_norm, reserve_b_norm, dec_out) = if zero_for_one {
        let a = if dec0.is_zero() {
            U256::ZERO
        } else {
            (amount_in * ONE_E18) / dec0
        };
        (a, r0_norm, r1_norm, dec1)
    } else {
        let a = if dec1.is_zero() {
            U256::ZERO
        } else {
            (amount_in * ONE_E18) / dec1
        };
        (a, r1_norm, r0_norm, dec0)
    };

    // Solve: find y such that f(reserve_a + amount_in, y) = xy
    let new_reserve_a = reserve_a_norm + amount_in_norm;
    let y = get_y_stable(new_reserve_a, xy, reserve_b_norm);

    if reserve_b_norm <= y {
        return U256::ZERO;
    }

    let dy_norm = reserve_b_norm - y;

    // De-normalise output
    (dy_norm * dec_out) / ONE_E18
}

/// Stable invariant: k = x³y + xy³ (normalised).
fn k_stable(x: U256, y: U256, dec0: U256, dec1: U256) -> U256 {
    if dec0.is_zero() || dec1.is_zero() {
        return U256::ZERO;
    }
    let _x = (x * ONE_E18) / dec0;
    let _y = (y * ONE_E18) / dec1;
    let _a = (_x * _y) / ONE_E18; // xy
    let _b = (_x * _x) / ONE_E18 + (_y * _y) / ONE_E18; // x² + y²
    (_a * _b) / ONE_E18 // xy(x² + y²)
}

/// Newton's method: find y given x0 (new reserve_a) and the invariant xy.
fn get_y_stable(x0: U256, xy: U256, y: U256) -> U256 {
    let mut y = y;

    for _ in 0..255 {
        let k = f_stable(x0, y);
        let d = d_stable(x0, y);
        if d.is_zero() {
            return U256::ZERO;
        }

        if k < xy {
            let mut dy = ((xy - k) * ONE_E18) / d;
            if dy.is_zero() {
                if k == xy {
                    return y;
                }
                dy = U256::from(1u64);
            }
            y = y.saturating_add(dy);
        } else {
            let mut dy = ((k - xy) * ONE_E18) / d;
            if dy.is_zero() {
                if k == xy || f_stable(x0, y.saturating_sub(U256::from(1u64))) < xy {
                    return y;
                }
                dy = U256::from(1u64);
            }
            y = y.saturating_sub(dy);
        }
    }

    U256::ZERO
}

/// f(x0, y) = x0³y + x0y³ (the stable invariant function value).
fn f_stable(x0: U256, y: U256) -> U256 {
    // f = x0 * y / 1e18 * (x0² + y²) / 1e18
    let a = (x0 * y) / ONE_E18;
    let b = (x0 * x0) / ONE_E18 + (y * y) / ONE_E18;
    (a * b) / ONE_E18
}

/// d f / d y = x0 * (3y²) + x0³ (derivative w.r.t. y, normalised).
fn d_stable(x0: U256, y: U256) -> U256 {
    // d = 3 * x0 * y² / 1e18 / 1e18 + x0³ / 1e18 / 1e18
    let y2 = (y * y) / ONE_E18;
    let x0_3 = (x0 * x0 / ONE_E18) * x0 / ONE_E18;
    U256::from(3u64) * x0 * y2 / ONE_E18 + x0_3
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_volatile_basic() {
        // 1000 in, reserves 100_000 / 200_000, 0.3% fee
        let r0 = U256::from(100_000_000_000_000_000_000u128); // 100 ETH
        let r1 = U256::from(200_000_000_000_000_000_000u128); // 200 USDC-equivalent
        let amount_in = U256::from(1_000_000_000_000_000_000u128); // 1 ETH in
        let out = get_amount_out_aerodrome(amount_in, r0, r1, 18, 18, false, 30, true);
        assert!(out > U256::ZERO);
        // Should be approximately 2 ETH-equivalent minus fees
        assert!(out < U256::from(2_000_000_000_000_000_000u128));
    }

    #[test]
    fn test_stable_small() {
        // Stable pool: 1:1 pair with equal reserves, 5 bps fee
        let reserve = U256::from(1_000_000_000_000_000_000_000u128); // 1000 tokens
        let amount_in = U256::from(1_000_000_000_000_000_000u128); // 1 token
        let out = get_amount_out_aerodrome(amount_in, reserve, reserve, 18, 18, true, 5, true);
        // Should get nearly 1 token back (minus 5 bps fee)
        assert!(out > U256::from(990_000_000_000_000_000u128)); // > 0.99
        assert!(out < amount_in); // but less than in
    }
}
