use rust::concentrated_liquidity::calculate_sqrt_price_from_tick;
use ethers::types::U256;
use rust::concentrated_liquidity::calculate_amount_out;

#[test]
fn test_tick_math_at_zero() {
    let tick = 0;
    let price = calculate_sqrt_price_from_tick(tick);
    // At tick 0, price is 1.0 (Q96 = 2^96)
    let expected = U256::one() << 96;
    assert_eq!(price, expected, "Price at tick 0 should be 2^96");
}

#[test]
fn test_tick_math_at_one() {
    let tick = 1;
    let price = calculate_sqrt_price_from_tick(tick);
    // 1.0001^1 * 2^96
    // 1.0001 approx in Q96: 79228162514264337593543950336 * 1.0001
    // = 79236085330515764027303304731
    // Let's assert it is greater than at tick 0
    let expected_min = U256::one() << 96;
    assert!(price > expected_min, "Price at tick 1 should be > 2^96");
    
    // Check rough value
}

#[test]
fn test_cl_amount_out_non_zero() {
    let amount_in = U256::from(1_000_000u64);
    let sqrt_price_x96 = U256::one() << 96;
    let liquidity = U256::from(1_000_000_000_000u64);
    let fee_pips = 3000u32;

    let out_0_to_1 = calculate_amount_out(amount_in, sqrt_price_x96, liquidity, fee_pips, true)
        .expect("amount out should compute");
    let out_1_to_0 = calculate_amount_out(amount_in, sqrt_price_x96, liquidity, fee_pips, false)
        .expect("amount out should compute");

    assert!(out_0_to_1 > U256::zero());
    assert!(out_1_to_0 > U256::zero());
}
