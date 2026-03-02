/// Balancer V2 weighted pool math.
///
/// Ported from BaseBuster's `calculation/balancer.rs`.
/// Standalone pure functions — no DB dependency.
///
/// Implements:
/// - `get_amount_out_balancer` — weighted pool swap output via weighted formula
/// - `LogExpMath` — Balancer's logarithmic exponentiation library (pow, exp, ln)
///
/// Reference: https://github.com/balancer/balancer-v2-monorepo

use alloy::primitives::{I256, U256};
use std::str::FromStr;

// ── Public API ───────────────────────────────────────────────────────────────

/// Compute amount_out for a Balancer V2 weighted pool swap.
///
/// # Parameters (all 1e18-scaled unless noted)
/// - `amount_in`: raw token amount in
/// - `balance_in`: pool balance of token_in (1e18 normalised)
/// - `balance_out`: pool balance of token_out (1e18 normalised)
/// - `weight_in`: normalised weight of token_in (1e18 scale, e.g. 0.5e18)
/// - `weight_out`: normalised weight of token_out (1e18 scale)
/// - `swap_fee`: swap fee as 1e18 fraction (e.g. 0.003e18 = 0.3%)
/// - `token_in_decimals`: decimals of the input token (for scaling)
pub fn get_amount_out_balancer(
    amount_in: U256,
    balance_in: U256,
    balance_out: U256,
    weight_in: U256,
    weight_out: U256,
    swap_fee: U256,
    token_in_decimals: u8,
) -> U256 {
    if amount_in.is_zero() || balance_in.is_zero() || balance_out.is_zero() {
        return U256::ZERO;
    }

    let one = U256::from(1_000_000_000_000_000_000u64); // 1e18

    // Scale amount_in to 1e18 normalisation
    let scaling_factor = 18i8 - token_in_decimals as i8;
    let scaled_amount_in = scale(amount_in, scaling_factor);

    // Deduct swap fee (fee is 1e18-scaled fraction, e.g. 0.003e18 = 0.3%)
    let amount_in_without_fees =
        sub_checked(scaled_amount_in, mul_up(scaled_amount_in, swap_fee));

    // Weighted formula: amount_out = balance_out * (1 - (balance_in / (balance_in + amount_in_without_fees))^(weight_in/weight_out))
    // NOTE: do NOT scale again — amount_in_without_fees is already 1e18-normalised
    let denominator = balance_in + amount_in_without_fees;
    if denominator.is_zero() {
        return U256::ZERO;
    }
    let base = div_up(balance_in, denominator);
    let exponent = div_down(weight_in, weight_out);

    // Guard against edge cases
    if exponent.is_zero() || base >= one {
        return U256::ZERO;
    }

    let power = pow_up(base, exponent);
    mul_down(balance_out, complement(power))
}

// ── Internal fixed-point math ────────────────────────────────────────────────

const ONE_18: u128 = 1_000_000_000_000_000_000; // 1e18

fn scale(value: U256, decimals: i8) -> U256 {
    if decimals >= 0 {
        let factor = U256::from(10u64).pow(U256::from(decimals as u8));
        value.saturating_mul(factor)
    } else {
        let factor = U256::from(10u64).pow(U256::from((-decimals) as u8));
        value / factor
    }
}

fn sub_checked(a: U256, b: U256) -> U256 {
    a.saturating_sub(b)
}

fn div_up(a: U256, b: U256) -> U256 {
    let one = U256::from(ONE_18);
    if a.is_zero() || b.is_zero() {
        return U256::ZERO;
    }
    let a_inflated = a * one;
    ((a_inflated - U256::from(1u64)) / b) + U256::from(1u64)
}

fn div_down(a: U256, b: U256) -> U256 {
    let one = U256::from(ONE_18);
    if a.is_zero() || b.is_zero() {
        return U256::ZERO;
    }
    (a * one) / b
}

fn mul_up(a: U256, b: U256) -> U256 {
    let one = U256::from(ONE_18);
    let product = a * b;
    if product.is_zero() {
        U256::ZERO
    } else {
        ((product - U256::from(1u64)) / one) + U256::from(1u64)
    }
}

fn mul_down(a: U256, b: U256) -> U256 {
    let one = U256::from(ONE_18);
    (a * b) / one
}

fn pow_up(x: U256, y: U256) -> U256 {
    let one = U256::from(ONE_18);
    let two = one * U256::from(2u64);
    let four = one * U256::from(4u64);

    if y == one {
        return x;
    } else if y == two {
        return mul_up(x, x);
    } else if y == four {
        let square = mul_up(x, x);
        return mul_up(square, square);
    }

    let raw = LogExpMath::pow(x, y);
    let max_pow_relative_error = U256::from(10_000u64); // 1e-14 in 1e18 scale
    let max_error = mul_up(raw, max_pow_relative_error) + U256::from(1u64);
    raw + max_error
}

fn complement(x: U256) -> U256 {
    let one = U256::from(ONE_18);
    if x < one {
        one - x
    } else {
        U256::ZERO
    }
}

// ── LogExpMath ───────────────────────────────────────────────────────────────

/// Balancer's logarithmic exponentiation library, ported verbatim.
/// Computes x^y using natural logarithms: x^y = exp(y * ln(x)).
pub struct LogExpMath;

impl LogExpMath {
    fn one_18() -> I256  { I256::from_raw(U256::from(1_000_000_000_000_000_000u64)) }
    fn one_20() -> I256  { I256::from_raw(U256::from(100_000_000_000_000_000_000u128)) }
    fn one_36() -> I256  { I256::from_raw(U256::from_str("1000000000000000000000000000000000000").unwrap()) }

    fn max_natural_exponent() -> I256 { I256::from_raw(U256::from(130_000_000_000_000_000_000u128)) }
    fn min_natural_exponent() -> I256 { -I256::from_raw(U256::from(41_000_000_000_000_000_000u128)) }

    fn ln_36_lower_bound() -> I256 { I256::from_raw(U256::from(900_000_000_000_000_000u64)) }
    fn ln_36_upper_bound() -> I256 { I256::from_raw(U256::from(1_100_000_000_000_000_000u64)) }

    fn mild_exponent_bound() -> U256 { U256::from(2u64).pow(U256::from(254u64)) / U256::from(100_000_000_000_000_000_000u128) }

    fn x0()  -> I256 { I256::from_raw(U256::from(128_000_000_000_000_000_000u128)) }
    fn a0()  -> I256 { I256::from_raw(U256::from_str("38877084059945950922200000000000000000000000000000000000").unwrap()) }
    fn x1()  -> I256 { I256::from_raw(U256::from(64_000_000_000_000_000_000u128)) }
    fn a1()  -> I256 { I256::from_raw(U256::from(6235149080811616882910000000u128)) }
    fn x2()  -> I256 { I256::from_raw(U256::from(3_200_000_000_000_000_000_000u128)) }
    fn a2()  -> I256 { I256::from_raw(U256::from_str("7896296018268069516100000000000000").unwrap()) }
    fn x3()  -> I256 { I256::from_raw(U256::from(1_600_000_000_000_000_000_000u128)) }
    fn a3()  -> I256 { I256::from_raw(U256::from(888611052050787263676000000u128)) }
    fn x4()  -> I256 { I256::from_raw(U256::from(800_000_000_000_000_000_000u128)) }
    fn a4()  -> I256 { I256::from_raw(U256::from(298095798704172827474000u128)) }
    fn x5()  -> I256 { I256::from_raw(U256::from(400_000_000_000_000_000_000u128)) }
    fn a5()  -> I256 { I256::from_raw(U256::from(5459815003314423907810u128)) }
    fn x6()  -> I256 { I256::from_raw(U256::from(200_000_000_000_000_000_000u128)) }
    fn a6()  -> I256 { I256::from_raw(U256::from(738905609893065022723u128)) }
    fn x7()  -> I256 { I256::from_raw(U256::from(100_000_000_000_000_000_000u128)) }
    fn a7()  -> I256 { I256::from_raw(U256::from(271828182845904523536u128)) }
    fn x8()  -> I256 { I256::from_raw(U256::from(50_000_000_000_000_000_000u128)) }
    fn a8()  -> I256 { I256::from_raw(U256::from(164872127070012814685u128)) }
    fn x9()  -> I256 { I256::from_raw(U256::from(25_000_000_000_000_000_000u128)) }
    fn a9()  -> I256 { I256::from_raw(U256::from(128402541668774148407u128)) }
    fn x10() -> I256 { I256::from_raw(U256::from(12_500_000_000_000_000_000u128)) }
    fn a10() -> I256 { I256::from_raw(U256::from(113314845306682631683u128)) }
    fn x11() -> I256 { I256::from_raw(U256::from(6_250_000_000_000_000_000u128)) }
    fn a11() -> I256 { I256::from_raw(U256::from(106449445891785942956u128)) }

    /// Compute x^y using exp(y * ln(x)).
    pub fn pow(x: U256, y: U256) -> U256 {
        if y.is_zero() {
            return U256::from(ONE_18);
        }
        if x.is_zero() {
            return U256::ZERO;
        }
        if x >= U256::from(2u64).pow(U256::from(255u64)) {
            return U256::ZERO; // X_OUT_OF_BOUNDS
        }

        let x_int256 = I256::from_raw(x);
        if y >= Self::mild_exponent_bound() {
            return U256::ZERO; // Y_OUT_OF_BOUNDS
        }
        let y_int256 = I256::from_raw(y);

        let logx_times_y = if Self::ln_36_lower_bound() < x_int256 && x_int256 < Self::ln_36_upper_bound() {
            let ln_36_x = Self::_ln_36(x_int256);
            (ln_36_x / Self::one_18()) * y_int256 + ((ln_36_x % Self::one_18()) * y_int256) / Self::one_18()
        } else {
            Self::_ln(x_int256) * y_int256
        } / Self::one_18();

        if logx_times_y < Self::min_natural_exponent() || logx_times_y > Self::max_natural_exponent() {
            return U256::ZERO; // PRODUCT_OUT_OF_BOUNDS
        }

        U256::try_from(Self::exp(logx_times_y).abs()).unwrap_or(U256::ZERO)
    }

    pub fn exp(x: I256) -> I256 {
        if x < Self::min_natural_exponent() || x > Self::max_natural_exponent() {
            return I256::ZERO;
        }
        if x.is_negative() {
            return (Self::one_18() * Self::one_18()) / Self::exp(-x);
        }

        let mut x = x;
        let first_an;
        if x >= Self::x0() {
            x -= Self::x0();
            first_an = Self::a0();
        } else if x >= Self::x1() {
            x -= Self::x1();
            first_an = Self::a1();
        } else {
            first_an = I256::try_from(1i64).unwrap();
        }

        x *= I256::from_raw(U256::from(100u64));
        let mut product = Self::one_20();

        if x >= Self::x2()  { x -= Self::x2();  product = (product * Self::a2())  / Self::one_20(); }
        if x >= Self::x3()  { x -= Self::x3();  product = (product * Self::a3())  / Self::one_20(); }
        if x >= Self::x4()  { x -= Self::x4();  product = (product * Self::a4())  / Self::one_20(); }
        if x >= Self::x5()  { x -= Self::x5();  product = (product * Self::a5())  / Self::one_20(); }
        if x >= Self::x6()  { x -= Self::x6();  product = (product * Self::a6())  / Self::one_20(); }
        if x >= Self::x7()  { x -= Self::x7();  product = (product * Self::a7())  / Self::one_20(); }
        if x >= Self::x8()  { x -= Self::x8();  product = (product * Self::a8())  / Self::one_20(); }
        if x >= Self::x9()  { x -= Self::x9();  product = (product * Self::a9())  / Self::one_20(); }

        let mut series_sum = Self::one_20();
        let mut term = x;
        series_sum += term;
        term = ((term * x) / Self::one_20()) / I256::from_raw(U256::from(2u64));  series_sum += term;
        term = ((term * x) / Self::one_20()) / I256::from_raw(U256::from(3u64));  series_sum += term;
        term = ((term * x) / Self::one_20()) / I256::from_raw(U256::from(4u64));  series_sum += term;
        term = ((term * x) / Self::one_20()) / I256::from_raw(U256::from(5u64));  series_sum += term;
        term = ((term * x) / Self::one_20()) / I256::from_raw(U256::from(6u64));  series_sum += term;
        term = ((term * x) / Self::one_20()) / I256::from_raw(U256::from(7u64));  series_sum += term;
        term = ((term * x) / Self::one_20()) / I256::from_raw(U256::from(8u64));  series_sum += term;
        term = ((term * x) / Self::one_20()) / I256::from_raw(U256::from(9u64));  series_sum += term;
        term = ((term * x) / Self::one_20()) / I256::from_raw(U256::from(10u64)); series_sum += term;
        term = ((term * x) / Self::one_20()) / I256::from_raw(U256::from(11u64)); series_sum += term;
        term = ((term * x) / Self::one_20()) / I256::from_raw(U256::from(12u64)); series_sum += term;

        (((product * series_sum) / Self::one_20()) * first_an) / I256::from_raw(U256::from(100u64))
    }

    fn _ln(mut a: I256) -> I256 {
        if a < Self::one_18() {
            return -Self::_ln((Self::one_18() * Self::one_18()) / a);
        }

        let mut sum = I256::ZERO;
        if a >= Self::a0() * Self::one_18() { a /= Self::a0(); sum += Self::x0(); }
        if a >= Self::a1() * Self::one_18() { a /= Self::a1(); sum += Self::x1(); }

        sum *= I256::from_raw(U256::from(100u64));
        a   *= I256::from_raw(U256::from(100u64));

        if a >= Self::a2()  { a = (a * Self::one_20()) / Self::a2();  sum += Self::x2();  }
        if a >= Self::a3()  { a = (a * Self::one_20()) / Self::a3();  sum += Self::x3();  }
        if a >= Self::a4()  { a = (a * Self::one_20()) / Self::a4();  sum += Self::x4();  }
        if a >= Self::a5()  { a = (a * Self::one_20()) / Self::a5();  sum += Self::x5();  }
        if a >= Self::a6()  { a = (a * Self::one_20()) / Self::a6();  sum += Self::x6();  }
        if a >= Self::a7()  { a = (a * Self::one_20()) / Self::a7();  sum += Self::x7();  }
        if a >= Self::a8()  { a = (a * Self::one_20()) / Self::a8();  sum += Self::x8();  }
        if a >= Self::a9()  { a = (a * Self::one_20()) / Self::a9();  sum += Self::x9();  }
        if a >= Self::a10() { a = (a * Self::one_20()) / Self::a10(); sum += Self::x10(); }
        if a >= Self::a11() { a = (a * Self::one_20()) / Self::a11(); sum += Self::x11(); }

        let z = ((a - Self::one_20()) * Self::one_20()) / (a + Self::one_20());
        let z_squared = (z * z) / Self::one_20();
        let mut num = z;
        let mut series_sum = num;

        num = (num * z_squared) / Self::one_20(); series_sum += num / I256::from_raw(U256::from(3u64));
        num = (num * z_squared) / Self::one_20(); series_sum += num / I256::from_raw(U256::from(5u64));
        num = (num * z_squared) / Self::one_20(); series_sum += num / I256::from_raw(U256::from(7u64));
        num = (num * z_squared) / Self::one_20(); series_sum += num / I256::from_raw(U256::from(9u64));
        num = (num * z_squared) / Self::one_20(); series_sum += num / I256::from_raw(U256::from(11u64));

        series_sum *= I256::from_raw(U256::from(2u64));
        (sum + series_sum) / I256::from_raw(U256::from(100u64))
    }

    fn _ln_36(x: I256) -> I256 {
        let x = x * Self::one_18();
        let z = ((x - Self::one_36()) * Self::one_36()) / (x + Self::one_36());
        let z_squared = (z * z) / Self::one_36();
        let mut num = z;
        let mut series_sum = num;

        for n in 1..=7u64 {
            num = (num * z_squared) / Self::one_36();
            series_sum += num / I256::from_raw(U256::from(2 * n + 1));
        }

        series_sum * I256::from_raw(U256::from(2u64))
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_log_exp_pow_identity() {
        // x^1 = x
        let x = U256::from(2_000_000_000_000_000_000u128); // 2e18
        let y = U256::from(1_000_000_000_000_000_000u64);  // 1e18
        let result = LogExpMath::pow(x, y);
        // Should be approximately 2e18 ± rounding
        let diff = if result > x { result - x } else { x - result };
        assert!(diff < U256::from(1_000_000u64), "pow(x,1) should equal x, diff={diff}");
    }

    #[test]
    fn test_balancer_out_nonzero() {
        let balance = U256::from(1_000_000_000_000_000_000_000u128); // 1000 tokens
        let weight = U256::from(500_000_000_000_000_000u64); // 50% weight
        let fee = U256::from(3_000_000_000_000_000u64); // 0.3% fee
        let amount_in = U256::from(10_000_000_000_000_000_000u128); // 10 tokens

        let out = get_amount_out_balancer(amount_in, balance, balance, weight, weight, fee, 18);
        assert!(out > U256::ZERO);
        assert!(out < amount_in); // due to fee
    }
}
