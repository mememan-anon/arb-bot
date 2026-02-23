//! Uniswap V3 concentrated-liquidity pool support.
//!
//! Any DEX that is a Uniswap V3 fork (Ramses, Pharaoh CL, Shadow, Lynex, etc.)
//! uses this module.  Architecture-level differences from Algebra Integral:
//!   * State is fetched via `slot0()` instead of `globalState()`
//!   * Pool fee is a separate `fee() view returns (uint24)` in parts-per-million
//!   * Swap callback name is `uniswapV3SwapCallback`, not `algebraSwapCallback`
//!
//! The tick math / simulation is identical to Algebra, so we reuse
//! `concentrated_liquidity::calculate_amount_out` and the
//! `AlgebraPoolFull` / `AlgebraPoolInfo` / `AlgebraState` types from
//! `crate::algebra`.

use anyhow::{anyhow, Result};
use ethers::contract::abigen;
use ethers::providers::{Provider, Ws};
use ethers::types::{H160, U256};
use futures::future::join_all;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

use crate::algebra::{AlgebraPoolFull, AlgebraPoolInfo, AlgebraState};

// ── ABIs ─────────────────────────────────────────────────────────────────────

abigen!(
    UniswapV3CLPool,
    r#"[
        function slot0() view returns (uint160 sqrtPriceX96, int24 tick, uint16 observationIndex, uint16 observationCardinality, uint16 observationCardinalityNext, uint8 feeProtocol, bool unlocked)
        function liquidity() view returns (uint128)
        function fee() view returns (uint24)
        function token0() view returns (address)
        function token1() view returns (address)
    ]"#
);

abigen!(
    UniswapV3CLERC20,
    r#"[function decimals() view returns (uint8)]"#
);

// ── CSV loader ────────────────────────────────────────────────────────────────

/// Load UniswapV3-CL pool infos from `.cached-v3cl-pools.csv`.
///
/// CSV format (same as `.cached-algebra-pools.csv` used for Algebra Integral):
///   protocol, address, token0_symbol, token1_symbol, tickSpacing, fee
pub fn load_uniswapv3cl_pools_from_csv(cache_dir: &str) -> Result<Vec<AlgebraPoolInfo>> {
    let path = format!("cache/{cache_dir}/.cached-v3cl-pools.csv");
    let mut reader = csv::Reader::from_path(&path)
        .map_err(|e| anyhow!("failed to read {path}: {e}"))?;

    let mut pools = Vec::new();
    for row in reader.records() {
        let row = row.map_err(|e| anyhow!("invalid csv row: {e}"))?;
        let address = match H160::from_str(row.get(1).unwrap_or_default()) {
            Ok(addr) => addr,
            Err(_) => continue,
        };
        let fee = row.get(5).unwrap_or("0").parse::<u32>().unwrap_or(0);
        pools.push(AlgebraPoolInfo {
            address,
            token0_symbol: row.get(2).unwrap_or("").to_string(),
            token1_symbol: row.get(3).unwrap_or("").to_string(),
            fee,
            tvl_usd: 0.0,
        });
    }

    Ok(pools)
}

// ── Full pool fetch ───────────────────────────────────────────────────────────

/// Fetch full on-chain state for a list of Uniswap V3 CL pools (any V3 fork).
///
/// Returns `AlgebraPoolFull` structs — both CL architectures share the same
/// tick-based fields (sqrtPriceX96, liquidity, tick, fee in ppm).
pub async fn fetch_full_uniswapv3cl_pools(
    provider: Arc<Provider<Ws>>,
    pools_info: &[AlgebraPoolInfo],
) -> Vec<AlgebraPoolFull> {
    let futures: Vec<_> = pools_info
        .iter()
        .map(|info| {
            let provider = provider.clone();
            let pool_addr = info.address;
            let info_fee = info.fee; // fallback if on-chain fee() reverts

            async move {
                let pool = UniswapV3CLPool::new(pool_addr, provider.clone());

                let c0 = pool.token_0();
                let c1 = pool.token_1();
                let cl = pool.liquidity();
                let cs = pool.slot_0();
                let cf = pool.fee();

                let (token0_res, token1_res, liq_res, slot0_res, fee_res) = tokio::join!(
                    c0.call(),
                    c1.call(),
                    cl.call(),
                    cs.call(),
                    cf.call(),
                );

                if token0_res.is_err()
                    || token1_res.is_err()
                    || liq_res.is_err()
                    || slot0_res.is_err()
                {
                    return None;
                }

                let token0 = token0_res.unwrap();
                let token1 = token1_res.unwrap();
                let liquidity = U256::from(liq_res.unwrap());
                let slot0 = slot0_res.unwrap();
                let sqrt_price = U256::from(slot0.0);
                let tick = slot0.1;
                // Use on-chain fee if available; fall back to CSV value.
                let fee = fee_res.map(|f| f as u32).unwrap_or(info_fee);

                // Fetch decimals in parallel
                let t0 = UniswapV3CLERC20::new(token0, provider.clone());
                let t1 = UniswapV3CLERC20::new(token1, provider.clone());
                let d0_call = t0.decimals();
                let d1_call = t1.decimals();
                let (d0_res, d1_res) =
                    tokio::join!(d0_call.call(), d1_call.call());

                if d0_res.is_err() || d1_res.is_err() {
                    return None;
                }

                Some(AlgebraPoolFull {
                    address: pool_addr,
                    token0,
                    token1,
                    decimals0: d0_res.unwrap(),
                    decimals1: d1_res.unwrap(),
                    fee,
                    sqrt_price_x96: sqrt_price,
                    liquidity,
                    tick,
                })
            }
        })
        .collect();

    join_all(futures).await.into_iter().flatten().collect()
}

// ── Per-block state refresh ───────────────────────────────────────────────────

/// Refresh sqrtPriceX96, liquidity, and tick for all tracked UniswapV3-CL pools.
///
/// Called every block (same pattern as `fetch_algebra_states`).
pub async fn fetch_uniswapv3cl_states(
    provider: Arc<Provider<Ws>>,
    pools: &[AlgebraPoolInfo],
) -> HashMap<H160, AlgebraState> {
    let futures: Vec<_> = pools
        .iter()
        .map(|info| {
            let pool = UniswapV3CLPool::new(info.address, provider.clone());
            let addr = info.address;
            async move {
                let cs = pool.slot_0();
                let cl = pool.liquidity();
                let (slot0_res, liq_res) = tokio::join!(cs.call(), cl.call());

                if let (Ok(slot0), Ok(liq)) = (slot0_res, liq_res) {
                    Some((
                        addr,
                        AlgebraState {
                            sqrt_price_x96: U256::from(slot0.0),
                            liquidity: U256::from(liq),
                            tick: slot0.1,
                            fee: 0, // UniswapV3 fees are immutable; 0 preserves CSV fee via guard
                        },
                    ))
                } else {
                    None
                }
            }
        })
        .collect();

    join_all(futures).await.into_iter().flatten().collect()
}
