use anyhow::{anyhow, Result};
use ethers::contract::abigen;
use ethers::providers::{Provider, Ws};
use ethers::types::{H160, U256};
use futures::future::join_all;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

abigen!(
    AlgebraPool,
    r#"[
        function globalState() view returns (uint160 sqrtPriceX96, int24 tick, uint16 lastFee, uint8 pluginConfig, uint16 communityFee, bool unlocked)
        function liquidity() view returns (uint128)
        function token0() view returns (address)
        function token1() view returns (address)
        function fee() view returns (uint16)
    ]"#
);

abigen!(
    ERC20,
    r#"[function decimals() view returns (uint8)]"#
);

// ... (existing structs)

pub async fn fetch_full_algebra_pools(
    provider: Arc<Provider<Ws>>,
    pools_info: &[AlgebraPoolInfo],
) -> Vec<AlgebraPoolFull> {
    let mut futures = Vec::new();

    for info in pools_info {
        let provider = provider.clone();
        let pool_addr = info.address;
        let fee = info.fee;

        futures.push(async move {
            let pool_contract = AlgebraPool::new(pool_addr, provider.clone());

            // Parallel fetch within the pool
            // Use token0() and token1() matching solidity names usually
            let c0 = pool_contract.token_0();
            let token0_f = c0.call();
            let c1 = pool_contract.token_1();
            let token1_f = c1.call();
            let cl = pool_contract.liquidity();
            let liquidity_f = cl.call();
            let cs = pool_contract.global_state();
            let state_f = cs.call();

            // Run these concurrently
            let (token0_res, token1_res, liq_res, state_res) = 
                tokio::join!(token0_f, token1_f, liquidity_f, state_f);
            
            if token0_res.is_err() || token1_res.is_err() || liq_res.is_err() || state_res.is_err() {
                 return None;
            }
            
            let token0 = token0_res.unwrap();
            let token1 = token1_res.unwrap();
            let liquidity = U256::from(liq_res.unwrap());
            let state = state_res.unwrap();
            let sqrt_price = U256::from(state.0); // Assuming first element
            let tick = state.1;

            // Now get decimals.
            let t0_contract = ERC20::new(token0, provider.clone());
            let t1_contract = ERC20::new(token1, provider.clone());
            
            let d0_call = t0_contract.decimals();
            let d1_call = t1_contract.decimals();
            
            // Need to bind calls before joining future because call() borrows self usually
            let d0_fut = d0_call.call();
            let d1_fut = d1_call.call();
            
            let (d0_res, d1_res) = tokio::join!(d0_fut, d1_fut);
            
            if d0_res.is_err() || d1_res.is_err() {
                return None;
            }
            
            let decimals0 = d0_res.unwrap();
            let decimals1 = d1_res.unwrap();

            Some(AlgebraPoolFull {
                address: pool_addr,
                token0,
                token1,
                decimals0,
                decimals1,
                fee: fee,
                sqrt_price_x96: sqrt_price,
                liquidity,
                tick,
            })
        });
    }

    let results = join_all(futures).await;
    results.into_iter().flatten().collect()
}

#[derive(Debug, Clone)]
pub struct AlgebraPoolInfo {
    pub address: H160,
    pub token0_symbol: String,
    pub token1_symbol: String,
    pub fee: u32,
    pub tvl_usd: f64,
}

#[derive(Debug, Clone, Default)]
pub struct AlgebraState {
    pub sqrt_price_x96: U256,
    pub liquidity: U256,
    pub tick: i32,
    /// lastFee from globalState() — dynamic fee in ppm (parts-per-million).
    /// Zero means the fee did not change or could not be read; caller should
    /// keep the existing pool.fee in that case.
    pub fee: u32,
}

#[derive(Debug, Clone)]
pub struct AlgebraPoolFull {
    pub address: H160,
    pub token0: H160,
    pub token1: H160,
    pub decimals0: u8,
    pub decimals1: u8,
    pub fee: u32,
    pub sqrt_price_x96: U256,
    pub liquidity: U256,
    pub tick: i32,
}

pub fn load_algebra_pools_from_v3_csv(cache_dir: &str) -> Result<Vec<AlgebraPoolInfo>> {
    let path = format!("cache/{cache_dir}/.cached-algebra-pools.csv");
    let mut reader = csv::Reader::from_path(&path)
        .map_err(|e| anyhow!("failed to read {path}: {e}"))?;

    let mut pools = Vec::new();
    for row in reader.records() {
        let row = row.map_err(|e| anyhow!("invalid csv row: {e}"))?;
        let address = match H160::from_str(row.get(1).unwrap_or_default()) {
            Ok(addr) => addr,
            Err(_) => continue,
        };
        let fee = row
            .get(5)
            .unwrap_or("0")
            .parse::<u32>()
            .unwrap_or(0);
        pools.push(AlgebraPoolInfo {
            address,
            token0_symbol: String::new(),
            token1_symbol: String::new(),
            fee,
            tvl_usd: 0.0,
        });
    }

    Ok(pools)
}

pub async fn fetch_algebra_states(
    provider: Arc<Provider<Ws>>,
    pools: &[AlgebraPoolInfo],
) -> HashMap<H160, AlgebraState> {
    // Parallel fetching for 10-50x speedup
    let futures: Vec<_> = pools
        .iter()
        .map(|pool| {
            let contract = AlgebraPool::new(pool.address, provider.clone());
            let addr = pool.address;
            async move {
                let gs = contract.global_state();
                let state_f = gs.call();
                let cl = contract.liquidity();
                let liq_f = cl.call();
                
                let (state_res, liq_res) = tokio::join!(state_f, liq_f);

                if let (Ok(state), Ok(liq)) = (state_res, liq_res) {
                    Some((
                        addr,
                        AlgebraState {
                            sqrt_price_x96: U256::from(state.0),
                            liquidity: U256::from(liq),
                            tick: state.1,
                            // state.2 is lastFee (u16, ppm).  Non-zero means the
                            // dynamic fee plugin updated it; keep existing if 0.
                            fee: u32::from(state.2),
                        },
                    ))
                } else {
                    None
                }
            }
        })
        .collect();

    let results = join_all(futures).await;
    results.into_iter().flatten().collect()
}
