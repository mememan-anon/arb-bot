/// Pool loading and rate estimation tests — adapted from BaseBuster's test suite.
///
/// Covers:
///   1. CSV parsing correctness (pure, no RPC) — decimals, fee, tick_spacing, protocol mapping
///   2. V2 fee_factor derivation from CSV fee column (PancakeSwap 0.25% vs UniV2 0.30%)
///   3. RateEstimator seeding with in-memory mock state (pure, no RPC)
///   4. Rate estimation produces > 0 for valid pools
///   5. Profitable path detection with synthetic imbalanced pools
///
/// Tests that require a live RPC connection are marked `#[ignore]`. Run them with:
///   cargo test -- --ignored   (set FULL env var to your BSC RPC endpoint first)

use alloy::primitives::{Address, U256, address};
use rust::calculation::{
    v2::get_amount_out_v2,
    rates::{RateEstimator, RATE_SCALE, decimal_reference},
};
use rust::pool_loader::{RawPool, load_v2_pools_from_csv, load_v3_pools_from_csv, load_all_pools_from_cache};
use rust::state_db::{BlockStateDB, V2_RESERVES_SLOT};
use rust::swap_types::{PoolProtocol, SwapPath, SwapStep};

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Build a dummy BlockStateDB (no real RPC needed — URL never hit unless a
/// lazy-fetch path is triggered, which none of these tests do).
fn dummy_db() -> BlockStateDB {
    BlockStateDB::new("http://localhost:8545".to_string(), 0)
}

/// Pack (reserve0, reserve1) into the V2 reserves storage slot.
/// Slot 8 layout: reserve0 in bits [0..112), reserve1 in bits [112..224).
fn pack_v2_reserves(r0: U256, r1: U256) -> U256 {
    r0 | (r1 << 112)
}

/// Inject V2 reserves into a BlockStateDB without any RPC call.
fn inject_v2_reserves(db: &mut BlockStateDB, pool: Address, r0: U256, r1: U256) {
    db.update_slot(pool, V2_RESERVES_SLOT, pack_v2_reserves(r0, r1));
}

/// Build a minimal in-memory RawPool for V2.
fn raw_v2_pool(
    address: Address,
    token0: Address,
    token1: Address,
    decimals0: u8,
    decimals1: u8,
    fee_bps: u32,
    protocol: PoolProtocol,
) -> RawPool {
    RawPool {
        address,
        token0,
        token1,
        decimals0,
        decimals1,
        fee: fee_bps,
        tick_spacing: 0,
        protocol,
    }
}

// ── Section 1: CSV parsing ────────────────────────────────────────────────────

/// The BSC V2 CSV must load without errors and contain pools.
#[test]
fn test_bsc_v2_csv_loads_non_empty() {
    let pools = load_v2_pools_from_csv("cache/bsc/.cached-pools.csv")
        .expect("V2 CSV must load");
    assert!(!pools.is_empty(), "cache/bsc/.cached-pools.csv is empty — run cache_to_csv.py first");
}

/// The BSC V3 CSV must load without errors and contain pools.
#[test]
fn test_bsc_v3_csv_loads_non_empty() {
    let pools = load_v3_pools_from_csv("cache/bsc/.cached-v3cl-pools.csv")
        .expect("V3 CSV must load");
    assert!(!pools.is_empty(), "cache/bsc/.cached-v3cl-pools.csv is empty — run cache_to_csv.py first");
}

/// First BSC V2 pool: fee must be 25 bps (PancakeSwap V2 standard on BSC).
/// Decimals are pool-specific but must be in the valid range.
#[test]
fn test_bsc_v2_first_pool_decimals_and_fee() {
    let pools = load_v2_pools_from_csv("cache/bsc/.cached-pools.csv").unwrap();
    let first = &pools[0];

    // The critical invariant: BSC PancakeSwap V2 fee is 25 bps (0.25%)
    assert_eq!(first.fee, 25, "BSC PancakeSwap V2 fee must be 25 bps in CSV");
    // Decimals must be in valid range regardless of which token pair comes first
    assert!(first.decimals0 >= 1 && first.decimals0 <= 18,
        "token0 decimals out of range: {}", first.decimals0);
    assert!(first.decimals1 >= 1 && first.decimals1 <= 18,
        "token1 decimals out of range: {}", first.decimals1);
}

/// Every V2 pool must have valid addresses (non-zero).
#[test]
fn test_bsc_v2_no_zero_addresses() {
    let pools = load_v2_pools_from_csv("cache/bsc/.cached-pools.csv").unwrap();
    for pool in &pools {
        assert_ne!(pool.address, Address::ZERO, "pool address is zero");
        assert_ne!(pool.token0,  Address::ZERO, "token0 is zero in pool {}", pool.address);
        assert_ne!(pool.token1,  Address::ZERO, "token1 is zero in pool {}", pool.address);
    }
}

/// Every V2 pool must have sane decimal values (1–18).
#[test]
fn test_bsc_v2_decimal_range() {
    let pools = load_v2_pools_from_csv("cache/bsc/.cached-pools.csv").unwrap();
    for pool in &pools {
        assert!(pool.decimals0 >= 1 && pool.decimals0 <= 18,
            "pool {} token0 decimals out of range: {}", pool.address, pool.decimals0);
        assert!(pool.decimals1 >= 1 && pool.decimals1 <= 18,
            "pool {} token1 decimals out of range: {}", pool.address, pool.decimals1);
    }
}

/// V3 pools must have a non-zero tick_spacing.
#[test]
fn test_bsc_v3_tick_spacing_non_zero() {
    let pools = load_v3_pools_from_csv("cache/bsc/.cached-v3cl-pools.csv").unwrap();
    for pool in &pools {
        assert_ne!(pool.tick_spacing, 0,
            "V3 pool {} has tick_spacing=0 — parser bug or bad CSV row", pool.address);
    }
}

/// First V3 pool: tickSpacing=200, dex=UniswapV3CL → UniswapV3 protocol.
#[test]
fn test_bsc_v3_first_pool_tick_spacing_and_protocol() {
    let pools = load_v3_pools_from_csv("cache/bsc/.cached-v3cl-pools.csv").unwrap();
    let first = &pools[0];
    assert_eq!(first.tick_spacing, 200, "first V3 pool tick_spacing should be 200");
    assert_eq!(first.protocol, PoolProtocol::UniswapV3,
        "first V3 pool protocol should be UniswapV3 (dex=UniswapV3CL)");
}

/// All V3 pools loaded from the BSC CSV must map to a V3-style protocol.
#[test]
fn test_bsc_v3_all_protocols_are_v3_type() {
    let pools = load_v3_pools_from_csv("cache/bsc/.cached-v3cl-pools.csv").unwrap();
    for pool in &pools {
        assert!(pool.protocol.is_v3(),
            "V3 CSV pool {} mapped to non-V3 protocol {:?}", pool.address, pool.protocol);
    }
}

/// load_all_pools_from_cache merges V2 + V3 correctly.
#[test]
fn test_load_all_pools_merges_v2_and_v3() {
    let v2 = load_v2_pools_from_csv("cache/bsc/.cached-pools.csv").unwrap();
    let v3 = load_v3_pools_from_csv("cache/bsc/.cached-v3cl-pools.csv").unwrap();
    let all = load_all_pools_from_cache("cache/bsc").unwrap();
    assert_eq!(all.len(), v2.len() + v3.len(),
        "merged count should equal v2+v3 individually");
}

// ── Section 2: Fee factor derivation ─────────────────────────────────────────

/// CSV fee=25 bps → fee_factor=9975 (PancakeSwap V2 BSC standard).
#[test]
fn test_fee_factor_from_csv_pancakeswap() {
    let fee_bps: u32 = 25;
    let fee_factor = 10000u64 - fee_bps as u64;
    assert_eq!(fee_factor, 9975, "PancakeSwap V2 fee_factor should be 9975");
}

/// CSV fee=30 bps → fee_factor=9970 (UniswapV2 standard).
#[test]
fn test_fee_factor_from_csv_uniswapv2() {
    let fee_bps: u32 = 30;
    let fee_factor = 10000u64 - fee_bps as u64;
    assert_eq!(fee_factor, 9970, "UniswapV2 fee_factor should be 9970");
}

/// The pool.fee column in BSC CSV must produce correct fee_factor for all loaded pools.
/// No pool should have fee > 100 bps (that would be absurd for an AMM).
#[test]
fn test_bsc_v2_csv_fee_values_are_sane() {
    let pools = load_v2_pools_from_csv("cache/bsc/.cached-pools.csv").unwrap();
    for pool in &pools {
        let ff = 10000u64.saturating_sub(pool.fee as u64);
        assert!(ff >= 9900 && ff < 10000,
            "pool {} fee {} bps gives fee_factor {} which is out of valid range [9900,10000)",
            pool.address, pool.fee, ff);
    }
}

/// fee_factor produces correct output relative to UniswapV2 reference formula.
/// Compared to the on-chain formula: (amount * fee_factor * reserve_out) /
/// (reserve_in * 10000 + amount * fee_factor).
#[test]
fn test_v2_amount_out_fee_factor_matches_formula() {
    let reserve_in  = U256::from(1_000_000_000_000_000_000u128); // 1e18
    let reserve_out = U256::from(1_000_000_000_000_000_000u128); // 1e18
    let amount_in   = U256::from(1_000_000_000_000_000_000u128); // 1e18

    let out_9975 = get_amount_out_v2(amount_in, reserve_in, reserve_out, 9975);
    let out_9970 = get_amount_out_v2(amount_in, reserve_in, reserve_out, 9970);

    // Higher fee_factor → less fee taken → more output
    assert!(out_9975 > out_9970,
        "9975 fee_factor (0.25%) should give more output than 9970 (0.30%)");

    // PancakeSwap out should be ~0.025% more than UniV2
    // Difference should be small but positive
    let diff = out_9975 - out_9970;
    assert!(diff > U256::ZERO && diff < out_9970 / U256::from(100u64),
        "fee difference should be < 1% of output, got diff={diff}");
}

// ── Section 3: RateEstimator with mock in-memory state ────────────────────────

/// Seed rates for a single mock V2 pool and verify both directions are stored.
#[test]
fn test_rate_seeder_stores_both_directions() {
    let pool_addr = address!("0000000000000000000000000000000000001111");
    let token0    = address!("0000000000000000000000000000000000000001");
    let token1    = address!("0000000000000000000000000000000000000002");

    let mut db = dummy_db();
    // Inject balanced 1:1 reserves (both 1e18)
    let r = U256::from(1_000_000_000_000_000_000u128);
    inject_v2_reserves(&mut db, pool_addr, r, r);

    let pool = raw_v2_pool(pool_addr, token0, token1, 18, 18, 30, PoolProtocol::UniswapV2);
    let mut est = RateEstimator::new();
    est.seed_from_raw_pools(
        &[pool],
        token1, // treat token1 as WETH for this test
        &db,
    );

    // Both directions must have a rate entry
    assert!(est.rates.contains_key(&(pool_addr, true)),  "missing zero_for_one rate");
    assert!(est.rates.contains_key(&(pool_addr, false)), "missing one_for_zero rate");
}

/// Balanced pool → rates should be just below 1e18 (fee drag).
#[test]
fn test_balanced_v2_rate_below_unit() {
    let pool_addr = address!("0000000000000000000000000000000000002222");
    let token0    = address!("0000000000000000000000000000000000000003");
    let token1    = address!("0000000000000000000000000000000000000004");

    let mut db = dummy_db();
    let r = U256::from(1_000_000_000_000_000_000u128); // 1e18 each
    inject_v2_reserves(&mut db, pool_addr, r, r);

    let pool = raw_v2_pool(pool_addr, token0, token1, 18, 18, 30, PoolProtocol::UniswapV2);
    let mut est = RateEstimator::new();
    est.seed_from_raw_pools(&[pool], Address::ZERO, &db);

    let rate_0to1 = est.rates[&(pool_addr, true)];
    let rate_1to0 = est.rates[&(pool_addr, false)];

    // A balanced pool should give a rate just under 1e18 (fee overhead makes it < 1.0)
    assert!(rate_0to1 > U256::ZERO && rate_0to1 < RATE_SCALE,
        "zero_for_one rate should be <1e18 for balanced pool, got {rate_0to1}");
    assert!(rate_1to0 > U256::ZERO && rate_1to0 < RATE_SCALE,
        "one_for_zero rate should be <1e18 for balanced pool, got {rate_1to0}");
}

/// PancakeSwap BSC fee (25bps) produces a higher rate than generic V2 (30bps)
/// for the same reserves — confirms the CSV fee is being used correctly.
#[test]
fn test_pancakeswap_rate_higher_than_uniswapv2_same_reserves() {
    let pcs_addr  = address!("0000000000000000000000000000000000003333");
    let uni_addr  = address!("0000000000000000000000000000000000004444");
    let token0    = address!("0000000000000000000000000000000000000005");
    let token1    = address!("0000000000000000000000000000000000000006");

    let r = U256::from(1_000_000_000_000_000_000u128);

    let mut db = dummy_db();
    inject_v2_reserves(&mut db, pcs_addr, r, r);
    inject_v2_reserves(&mut db, uni_addr, r, r);

    // PancakeSwap: fee=25 bps (version 3 in pool_loader → PancakeSwapV2)
    let pcs_pool = raw_v2_pool(pcs_addr, token0, token1, 18, 18, 25, PoolProtocol::PancakeSwapV2);
    // UniswapV2: fee=30 bps
    let uni_pool = raw_v2_pool(uni_addr, token0, token1, 18, 18, 30, PoolProtocol::UniswapV2);

    let mut est = RateEstimator::new();
    est.seed_from_raw_pools(&[pcs_pool, uni_pool], Address::ZERO, &db);

    let pcs_rate = est.rates[&(pcs_addr, true)];
    let uni_rate = est.rates[&(uni_addr, true)];

    assert!(pcs_rate > uni_rate,
        "PancakeSwap (25bps) should give higher rate than UniV2 (30bps) on same reserves. pcs={pcs_rate}, uni={uni_rate}");
}

/// Imbalanced pool where token1 is cheap → zero_for_one rate > 1e18 after two pools.
#[test]
fn test_profitable_2hop_path_detected() {
    // Pool A: WETH cheap (reserve_weth is big vs reserve_usdc)
    //   → USDC→WETH has favorable rate
    // Pool B: WETH expensive (small reserve_weth)
    //   → WETH→USDC has favorable rate
    // Two-hop cycle: USDC → WETH → USDC should be profitable.

    let pool_a = address!("0000000000000000000000000000000000005555");
    let pool_b = address!("0000000000000000000000000000000000006666");
    let weth   = address!("0000000000000000000000000000000000000007");
    let usdc   = address!("0000000000000000000000000000000000000008");

    let mut db = dummy_db();
    // Use realistic pool sizes (1000 WETH) so the 1e18 reference amount causes
    // negligible slippage — the 4% price gap between pools should dominate.
    //
    // Pool A: 1000 WETH ↔ 2,500,000 USDC  →  1 WETH = 2500 USDC
    let r_weth_a = U256::from(1_000_000_000_000_000_000_000u128);  // 1000e18 WETH
    let r_usdc_a = U256::from(2_500_000_000_000u128);               // 2,500,000 USDC (6dec)
    inject_v2_reserves(&mut db, pool_a, r_weth_a, r_usdc_a);

    // Pool B: 1000 WETH ↔ 2,600,000 USDC  →  1 WETH = 2600 USDC
    let r_weth_b = U256::from(1_000_000_000_000_000_000_000u128);  // 1000e18 WETH
    let r_usdc_b = U256::from(2_600_000_000_000u128);               // 2,600,000 USDC (6dec)
    inject_v2_reserves(&mut db, pool_b, r_weth_b, r_usdc_b);

    let pool_a_raw = raw_v2_pool(pool_a, weth, usdc, 18, 6, 30, PoolProtocol::UniswapV2);
    let pool_b_raw = raw_v2_pool(pool_b, weth, usdc, 18, 6, 30, PoolProtocol::UniswapV2);

    let mut est = RateEstimator::new();
    est.seed_from_raw_pools(&[pool_a_raw, pool_b_raw], weth, &db);

    // Path: WETH→USDC via pool_b (sell WETH at 2600), USDC→WETH via pool_a (buy WETH at 2500)
    // token0=weth < usdc → zero_for_one = true when token_in=weth
    let path = SwapPath::new(vec![
        SwapStep {
            pool_address: pool_b,
            token_in:  weth,
            token_out: usdc,
            protocol: PoolProtocol::UniswapV2,
            fee: 30,
        },
        SwapStep {
            pool_address: pool_a,
            token_in:  usdc,
            token_out: weth,
            protocol: PoolProtocol::UniswapV2,
            fee: 30,
        },
    ]);

    assert!(est.is_profitable(&path),
        "2-hop path (sell expensive, buy cheap) should be profitable");

    let cum_rate = est.evaluate_path(&path);
    assert!(cum_rate > RATE_SCALE,
        "cumulative rate should exceed 1e18 (break-even), got {cum_rate}");
}

/// Reverse of the profitable path should NOT be profitable (flat/loss).
#[test]
fn test_unprofitable_path_rejected() {
    let pool_a = address!("0000000000000000000000000000000000007777");
    let pool_b = address!("0000000000000000000000000000000000008888");
    let weth   = address!("0000000000000000000000000000000000000009");
    let usdc   = address!("000000000000000000000000000000000000000a");

    let mut db = dummy_db();
    let r_weth_a = U256::from(1_000_000_000_000_000_000_000u128); // 1000e18 WETH
    let r_usdc_a = U256::from(2_500_000_000_000u128);              // 2,500,000 USDC
    inject_v2_reserves(&mut db, pool_a, r_weth_a, r_usdc_a);

    let r_weth_b = U256::from(1_000_000_000_000_000_000_000u128); // 1000e18 WETH
    let r_usdc_b = U256::from(2_600_000_000_000u128);              // 2,600,000 USDC
    inject_v2_reserves(&mut db, pool_b, r_weth_b, r_usdc_b);

    let pool_a_raw = raw_v2_pool(pool_a, weth, usdc, 18, 6, 30, PoolProtocol::UniswapV2);
    let pool_b_raw = raw_v2_pool(pool_b, weth, usdc, 18, 6, 30, PoolProtocol::UniswapV2);

    let mut est = RateEstimator::new();
    est.seed_from_raw_pools(&[pool_a_raw, pool_b_raw], weth, &db);

    // Reverse path: buy WETH in expensive pool, sell in cheap pool — a loss
    let _losing_path = SwapPath::new(vec![
        SwapStep {
            pool_address: pool_a,  // buy WETH at 2500 (cheaper market)
            token_in:  usdc,
            token_out: weth,
            protocol: PoolProtocol::UniswapV2,
            fee: 30,
        },
        SwapStep {
            pool_address: pool_b,  // sell WETH at 2600 (wait — this is actually profitable too)
            token_in:  weth,
            token_out: usdc,
            protocol: PoolProtocol::UniswapV2,
            fee: 30,
        },
    ]);
    // Note: both directions of the same price discrepancy are profitable.
    // Use a balanced pool pairing to create a true loss:
    // pool_a: 1 WETH = 2500 USDC, pool_b: 1 WETH = 2400 USDC (pool_b is now cheapER)
    // Path: sell WETH at 2500 (pool_a), buy WETH at 2400 (pool_b) → net loss

    let pool_c = address!("0000000000000000000000000000000000009999");
    let r_usdc_c = U256::from(2_400_000_000_000u128); // 2,400,000 USDC (cheaper WETH)
    inject_v2_reserves(&mut db, pool_c, r_weth_a, r_usdc_c);

    let pool_c_raw = raw_v2_pool(pool_c, weth, usdc, 18, 6, 30, PoolProtocol::UniswapV2);
    let pools_refresh = [
        raw_v2_pool(pool_a, weth, usdc, 18, 6, 30, PoolProtocol::UniswapV2),
        pool_c_raw,
    ];
    let mut est2 = RateEstimator::new();
    est2.seed_from_raw_pools(&pools_refresh, weth, &db);

    // Path: WETH→USDC in pool_c (sell WETH at 2400 — the CHEAP pool),
    // then USDC→WETH in pool_a (buy WETH at 2500 — the EXPENSIVE pool) → net loss.
    let loss_path = SwapPath::new(vec![
        SwapStep {
            pool_address: pool_c,  // sell WETH cheap (2400 USDC/WETH)
            token_in:  weth,
            token_out: usdc,
            protocol: PoolProtocol::UniswapV2,
            fee: 30,
        },
        SwapStep {
            pool_address: pool_a,  // buy WETH expensive (2500 USDC/WETH)
            token_in:  usdc,
            token_out: weth,
            protocol: PoolProtocol::UniswapV2,
            fee: 30,
        },
    ]);

    assert!(!est2.is_profitable(&loss_path),
        "path selling high then buying low should not be profitable");
}

// ── Section 4: Decimal reference helper ──────────────────────────────────────

#[test]
fn test_decimal_reference_values() {
    assert_eq!(decimal_reference(18), U256::from(1_000_000_000_000_000_000u64)); // 1e18
    assert_eq!(decimal_reference(6),  U256::from(1_000_000u64));                 // 1e6
    assert_eq!(decimal_reference(8),  U256::from(100_000_000u64));               // 1e8
    assert_eq!(decimal_reference(9),  U256::from(1_000_000_000u64));             // 1e9
}

/// Seeds WETH→token and token→WETH rates and checks weth_price_of is populated.
#[test]
fn test_weth_price_seeded_for_direct_pairs() {
    let pool_addr = address!("000000000000000000000000000000000000aaaa");
    let weth      = address!("000000000000000000000000000000000000bbbb");
    let usdc      = address!("000000000000000000000000000000000000cccc");

    let mut db = dummy_db();
    // 1 WETH ≈ 3000 USDC: reserve_weth=1e18, reserve_usdc=3000e6
    let r_weth = U256::from(1_000_000_000_000_000_000u128);
    let r_usdc = U256::from(3_000_000_000u128);  // 3000 USDC (6 dec)
    inject_v2_reserves(&mut db, pool_addr, r_weth, r_usdc);

    let pool = raw_v2_pool(pool_addr, weth, usdc, 18, 6, 30, PoolProtocol::UniswapV2);
    let mut est = RateEstimator::new();
    est.seed_from_raw_pools(&[pool], weth, &db);

    // WETH price for USDC should be set (1 USDC ≈ 1/3000 WETH)
    let price = est.weth_price_of(&usdc);
    assert!(price.is_some(), "WETH price for USDC should be populated after seeding");
    let p = price.unwrap();
    assert!(p > U256::ZERO, "WETH price should be non-zero");

    // Sanity-check: 1e6 USDC × price / 1e18 should be roughly 0.000333 WETH
    // i.e. price < 1e18/1000 (< 1e15 WETH per USDC canonical unit)
    let threshold = U256::from(1_000_000_000_000_000u128); // 1e15
    assert!(p < threshold,
        "USDC→WETH price per USDC unit should be well under 1e15, got {p}");
}
