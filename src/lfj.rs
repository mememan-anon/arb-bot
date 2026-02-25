/// lfj.rs — LFJ Liquidity Book V2.1/V2.2 pool support.
///
/// # Design
/// - `LFJPoolInfo`  — static data loaded once from the CSV cache.
/// - `LFJState`     — per-block dynamic state: **active bin reserves only**.
///
/// # Simulation strategy
/// LFJ is a bin-based AMM.  Each bin is constant-sum (not constant-product), and
/// only the *active bin* accepts swaps — non-active bins hold only one token.
///
/// Using `getReserves()` (total reserves across ALL bins) with x*y=k math is
/// incorrect: it inflates available liquidity by 10-100× and produces fictitiously
/// large output amounts (false profit signals).
///
/// Correct approach: call `getActiveId()` then `getBin(activeId)` to obtain only
/// the active bin's reserves.  Simulate with x*y=k on those reserves.  This gives
/// an accurate result for swaps that fit within one bin, and a conservative
/// (under-estimate) result for larger swaps — never a false positive.
use anyhow::{anyhow, Result};
use ethers::contract::abigen;
use ethers::providers::{Provider, Ws};
use ethers::types::{H160, U256};
use futures::future::join_all;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

// ── On-chain ABI binding ─────────────────────────────────────────────────────
abigen!(
    LBPair,
    r#"[
        function getReserves()         view returns (uint128 reserveX, uint128 reserveY)
        function getActiveId()         view returns (uint24 activeId)
        function getBin(uint24 id)     view returns (uint128 binReserveX, uint128 binReserveY)
        function getTokenX()           pure returns (address)
        function getTokenY()           pure returns (address)
        function getBinStep()          pure returns (uint16)
    ]"#
);

// ── Data structures ──────────────────────────────────────────────────────────

/// Static pool metadata — read once from the CSV cache file.
#[derive(Debug, Clone)]
pub struct LFJPoolInfo {
    /// Pair contract address.
    pub address: H160,
    /// Token X address (first token by Liquidity Book convention).
    /// Maps to AnyPool.token0 / path zero_for_one semantics.
    pub token_x: H160,
    /// Token Y address.
    pub token_y: H160,
    pub decimals_x: u8,
    pub decimals_y: u8,
    /// Bin step — price granularity (e.g. 1 = 0.01%, 10 = 0.1%, 25 = 0.25%).
    pub bin_step: u16,
    /// Base fee in basis points (e.g. 5 = 0.05%).
    /// Derived from API `lbBaseFeePct * 100`.
    pub base_fee_bps: u32,
    /// Pool version: 21 = V2.1, 22 = V2.2.
    pub version: u8,
    pub symbol_x: String,
    pub symbol_y: String,
}

/// Per-block dynamic state — fetched each block via `getActiveId` + `getBin`.
///
/// In addition to the active bin, we fetch a few adjacent bins on each side so
/// that the simulation can traverse bins when a swap exhausts the active bin's
/// reserve.  Each entry in `bins` is `(bin_id, reserve_x, reserve_y)` sorted by
/// ascending bin_id.
///
/// `active_id` is required to compute the bin's fixed price for constant-sum simulation.
#[derive(Debug, Clone)]
pub struct LFJState {
    /// Reserve of token X in the currently active bin (raw u128, same decimals as tokenX).
    pub reserve_x: U256,
    /// Reserve of token Y in the currently active bin.
    pub reserve_y: U256,
    /// The active bin ID returned by `getActiveId()`.  Used to compute bin price:
    /// `price = (1 + bin_step/10000)^(active_id - 8388608)` in raw Y-per-X units.
    pub active_id: u32,
    /// Adjacent bins: `(bin_id, reserve_x, reserve_y)`, sorted ascending by bin_id.
    /// Includes the active bin itself.  Used for multi-bin traversal simulation.
    pub bins: Vec<(u32, U256, U256)>,
}

// ── CSV loader ───────────────────────────────────────────────────────────────

/// Load LFJ pools from the CSV cache written by `scripts/pull_lfj.py`.
///
/// CSV format (1 header row):
/// ```text
/// address, version, token_x, token_y, decimals_x, decimals_y,
/// bin_step, base_fee_bps, liquidity_usd, symbol_x, symbol_y
/// ```
pub fn load_lfj_pools_from_csv(cache_dir: &str) -> Result<Vec<LFJPoolInfo>> {
    let path = format!("cache/{cache_dir}/.cached-lfj-pools.csv");
    let mut reader = csv::Reader::from_path(&path)
        .map_err(|e| anyhow!("failed to open {path}: {e}"))?;

    let mut pools = Vec::new();
    for record in reader.records() {
        let row = record.map_err(|e| anyhow!("malformed CSV row in {path}: {e}"))?;

        let address = match H160::from_str(row.get(0).unwrap_or_default().trim()) {
            Ok(a) => a,
            Err(_) => continue,
        };

        // version string e.g. "v2.2" → 22, "v2.1" → 21
        let version_str = row.get(1).unwrap_or_default().trim();
        let version: u8 = match version_str {
            "v2.2" => 22,
            "v2.1" => 21,
            _ => continue, // skip unknown versions
        };

        let token_x = match H160::from_str(row.get(2).unwrap_or_default().trim()) {
            Ok(a) => a,
            Err(_) => continue,
        };
        let token_y = match H160::from_str(row.get(3).unwrap_or_default().trim()) {
            Ok(a) => a,
            Err(_) => continue,
        };

        let decimals_x = row.get(4).unwrap_or("18").trim().parse::<u8>().unwrap_or(18);
        let decimals_y = row.get(5).unwrap_or("18").trim().parse::<u8>().unwrap_or(18);
        let bin_step = row.get(6).unwrap_or("10").trim().parse::<u16>().unwrap_or(10);
        let base_fee_bps = row.get(7).unwrap_or("5").trim().parse::<u32>().unwrap_or(5);
        let symbol_x = row.get(9).unwrap_or_default().trim().to_string();
        let symbol_y = row.get(10).unwrap_or_default().trim().to_string();

        pools.push(LFJPoolInfo {
            address,
            token_x,
            token_y,
            decimals_x,
            decimals_y,
            bin_step,
            base_fee_bps,
            version,
            symbol_x,
            symbol_y,
        });
    }

    Ok(pools)
}

// ── Per-block state fetcher ──────────────────────────────────────────────────

/// Number of bins to fetch on each side of the active bin.
const BINS_PER_SIDE: u32 = 5;

/// Fetch the **active bin reserves** and adjacent bins for all given LFJ pools in parallel.
///
/// Calls `ILBPair.getActiveId()` then `ILBPair.getBin(id)` for `active_id ± BINS_PER_SIDE`.
/// Pools that fail are silently skipped.
pub async fn fetch_lfj_states(
    provider: Arc<Provider<Ws>>,
    pools: &[LFJPoolInfo],
) -> HashMap<H160, LFJState> {
    let futures: Vec<_> = pools
        .iter()
        .map(|pool| {
            let contract = LBPair::new(pool.address, provider.clone());
            let addr = pool.address;
            async move {
                // Step 1: get the active bin ID.
                let active_id: u32 = match contract.get_active_id().call().await {
                    Ok(id) => id,
                    Err(_) => return None,
                };

                // Step 2: fetch the active bin + BINS_PER_SIDE on each side.
                let lo = active_id.saturating_sub(BINS_PER_SIDE);
                let hi = active_id.saturating_add(BINS_PER_SIDE);

                let bin_futures: Vec<_> = (lo..=hi)
                    .map(|id| {
                        let c = contract.clone();
                        async move {
                            match c.get_bin(id).call().await {
                                Ok((rx, ry)) => {
                                    let rx = U256::from(rx);
                                    let ry = U256::from(ry);
                                    if rx.is_zero() && ry.is_zero() {
                                        None
                                    } else {
                                        Some((id, rx, ry))
                                    }
                                }
                                Err(_) => None,
                            }
                        }
                    })
                    .collect();

                let bin_results = join_all(bin_futures).await;
                let mut bins: Vec<(u32, U256, U256)> = bin_results.into_iter().flatten().collect();
                bins.sort_by_key(|(id, _, _)| *id);

                // The active bin's reserves (for backward compat)
                let (reserve_x, reserve_y) = bins
                    .iter()
                    .find(|(id, _, _)| *id == active_id)
                    .map(|(_, rx, ry)| (*rx, *ry))
                    .unwrap_or((U256::zero(), U256::zero()));

                Some((
                    addr,
                    LFJState {
                        reserve_x,
                        reserve_y,
                        active_id,
                        bins,
                    },
                ))
            }
        })
        .collect();

    let results = join_all(futures).await;
    results.into_iter().flatten().collect()
}
