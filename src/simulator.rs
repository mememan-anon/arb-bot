use ethers::types::U256;

pub struct UniswapV2Simulator;

/// Solidly stable AMM simulator (x³y + xy³ = k invariant).
///
/// Used for Pharaoh (and compatible) stable pairs.
/// Reserves must be passed as raw uint112 and normalized internally by decimals.
pub struct SolidlyStableSimulator;

impl SolidlyStableSimulator {
    /// Compute the Solidly stable invariant k = x_n³y_n + x_ny_n³
    /// where x_n, y_n are reserves normalized to 18 decimal precision.
    #[inline]
    fn invariant(x: f64, y: f64) -> f64 {
        x * y * (x * x + y * y)
    }

    /// Newton's-method solver: given constant k and new x1, find y1 such that
    /// k(x1, y1) == k.  Returns y1.
    fn solve_y(x1: f64, y0: f64, k: f64) -> f64 {
        let mut y = y0;
        // 256 iterations is more than enough for convergence
        for _ in 0..256 {
            let f_val = x1 * y * (x1 * x1 + y * y) - k;
            let f_der = x1 * (x1 * x1 + 3.0 * y * y);
            if f_der == 0.0 {
                break;
            }
            let y_new = y - f_val / f_der;
            if (y_new - y).abs() < 1e-12 {
                return y_new;
            }
            y = y_new;
        }
        y
    }

    /// Compute amountOut for a Solidly stable pair swap.
    ///
    /// `fee_bps` is the pool fee in basis points (e.g. 50 for 0.50%).
    /// Decimals are required to normalize reserves before applying the invariant.
    pub fn get_amount_out(
        amount_in: U256,
        reserve_in: U256,
        reserve_out: U256,
        decimals_in: u8,
        decimals_out: u8,
        fee_bps: u32,
    ) -> Option<U256> {
        if amount_in.is_zero() || reserve_in.is_zero() || reserve_out.is_zero() {
            return Some(U256::zero());
        }

        let scale_in = (10.0f64).powi(decimals_in as i32);
        let scale_out = (10.0f64).powi(decimals_out as i32);

        // Apply fee in integer space first to avoid float precision loss for large amounts.
        if fee_bps >= 10000 {
            return None;
        }
        let amount_in_after_fee_u128 = amount_in
            .saturating_mul(U256::from(10000 - fee_bps))
            / U256::from(10000u32);
        let amount_in_after_fee = amount_in_after_fee_u128.as_u128() as f64;
        let r_in = reserve_in.as_u128() as f64;
        let r_out = reserve_out.as_u128() as f64;

        let x0 = r_in / scale_in;
        let y0 = r_out / scale_out;
        let dx = amount_in_after_fee / scale_in;

        let k = Self::invariant(x0, y0);
        let x1 = x0 + dx;
        let y1 = Self::solve_y(x1, y0, k);

        let dy = y0 - y1; // normalized output
        if dy <= 0.0 {
            return Some(U256::zero());
        }

        let amount_out_raw = (dy * scale_out).floor() as u128;
        Some(U256::from(amount_out_raw))
    }
}

impl UniswapV2Simulator {
    pub fn reserves_to_price(
        reserve0: U256,
        reserve1: U256,
        decimals0: u8,
        decimals1: u8,
        token0_in: bool,
    ) -> f64 {
        let r0 = reserve0.as_u128() as f64;
        let r1 = reserve1.as_u128() as f64;
        let d0 = decimals0 as i32;
        let d1 = decimals1 as i32;
        let mult = (10.0 as f64).powi(d0 - d1);

        if r1 == 0.0 || r0 == 0.0 {
            return 0.0;
        }

        let price = (r1 / r0) * mult;
        if price == 0.0 || price.is_infinite() || price.is_nan() {
            return 0.0;
        }
        if token0_in {
            price
        } else {
            1.0_f64 / price
        }
    }

    pub fn get_amount_out(
        amount_in: U256,
        reserve_in: U256,
        reserve_out: U256,
        fee: U256,
    ) -> Option<U256> {
        // fee is in bps (e.g. 71 for 0.71%, 300 for 3%)
        // Use 10000 base to avoid precision loss from /100 rounding
        if fee >= U256::from(10000) {
            return None; // invalid fee — would underflow or produce 0 output
        }
        let fee_numerator = U256::from(10000) - fee;
        let amount_in_with_fee = amount_in.checked_mul(fee_numerator)?;
        let numerator = amount_in_with_fee.checked_mul(reserve_out)?;
        let denominator = reserve_in
            .checked_mul(U256::from(10000))?
            .checked_add(amount_in_with_fee)?;
        numerator.checked_div(denominator)
    }
}
