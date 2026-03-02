/// Integration tests for the alloy-native AMM math modules.
///
/// Replaces the old ethers-based concentrated_liquidity tests.
/// Uses calculation::v2 (pure constant-product math, no state required).

use alloy::primitives::U256;
use rust::calculation::v2::{get_amount_out_v2, get_amount_in_v2};

// ── V2 AMM math ─────────────────────────────────────────────────────────────

#[test]
fn test_v2_amount_out_non_zero() {
    let amount_in  = U256::from(1_000_000u64);
    let reserve_in  = U256::from(1_000_000_000_000u64);
    let reserve_out = U256::from(1_000_000_000_000u64);
    let fee_factor = 9970u64; // UniswapV2 0.3%

    let out = get_amount_out_v2(amount_in, reserve_in, reserve_out, fee_factor);
    assert!(out > U256::ZERO, "amount out should be non-zero");
    assert!(out < amount_in, "fee should reduce output below input for 1:1 pools");
}

#[test]
fn test_v2_amount_out_zero_on_zero_input() {
    let out = get_amount_out_v2(
        U256::ZERO,
        U256::from(1_000_000u64),
        U256::from(1_000_000u64),
        9970,
    );
    assert_eq!(out, U256::ZERO);
}

#[test]
fn test_v2_amount_in_round_trip() {
    let reserve_in  = U256::from(5_000_000_000_000u64);
    let reserve_out = U256::from(2_500_000_000_000u64);
    let fee_factor  = 9970u64;
    let amount_in   = U256::from(100_000_000u64);

    let out  = get_amount_out_v2(amount_in, reserve_in, reserve_out, fee_factor);
    let back = get_amount_in_v2(out, reserve_in, reserve_out, fee_factor);
    // back should be >= amount_in (fees + rounding)
    assert!(back >= amount_in, "round-trip should require at least as much input");
}

#[test]
fn test_v2_profitable_cycle_detection() {
    // Two imbalanced pools forming a triangle with pool3.
    // Pool1: 1 WETH = 2500 USDC (cheap ETH)
    // Pool2: 1 WETH = 2600 USDC (expensive ETH)
    // Direct arb: buy ETH via pool1, sell ETH via pool2 = profit.
    let fee_factor = 9970u64;
    let re_in  = U256::from(1_000_000_000_000_000_000u64); // 1e18 WETH reserve
    let ru_in  = U256::from(2_500_000_000_000u64);          // 2500 USDC reserve (pool1)
    let re_out = U256::from(1_000_000_000_000_000_000u64);
    let ru_out = U256::from(2_600_000_000_000u64);          // 2600 USDC reserve (pool2)

    let amount_usdc_in = U256::from(1_000_000u64); // 1 USDC

    // Step 1: USDC → WETH in pool1
    let eth_out = get_amount_out_v2(amount_usdc_in, ru_in, re_in, fee_factor);
    // Step 2: WETH → USDC in pool2
    let usdc_out = get_amount_out_v2(eth_out, re_out, ru_out, fee_factor);

    assert!(usdc_out > amount_usdc_in, "should profit from the price discrepancy");
}
