use anyhow::{anyhow, Result};
use ethers::contract::abigen;
use ethers::providers::{Middleware, Provider, Ws};
use ethers::types::{Bytes, H160, U256};
use futures::future::join_all;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use log::{info, warn};

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

/// Multicall3 deployed at the same address on every EVM chain.
const MULTICALL3_ADDR: &str = "0xcA11bde05977b3631167028862bE2a173976CA11";
/// aggregate3((address,bool,bytes)[]) selector = keccak256(...)[0..4]
const AGGREGATE3_SEL: [u8; 4] = [0x82, 0xad, 0x56, 0xcb];
/// Pools per Multicall3 batch (2 calls each: slot0 + liquidity).
/// 5000 pools → 10000 calls → ~1.6 MB response — safe on all major nodes.
const BATCH_SIZE: usize = 5000;

abigen!(
    UniswapV3CLERC20,
    r#"[function decimals() view returns (uint8)]"#
);

// ── CSV loader ────────────────────────────────────────────────────────────────

/// Load ALL static fields from `.cached-v3cl-pools.csv` directly into
/// `AlgebraPoolFull` structs.  Dynamic state (sqrtPriceX96, liquidity, tick)
/// is zeroed — call `fetch_full_uniswapv3cl_pools` to populate them with only
/// 2 on-chain calls per pool (slot0 + liquidity) instead of the previous 7.
///
/// CSV columns (0-based):
///   0=protocol  1=address  2=token0_symbol  3=token1_symbol
///   4=tickSpacing  5=fee  6=token0_address  7=token1_address
///   8=decimals0  9=decimals1  10=dex
pub fn load_uniswapv3cl_full_from_csv(cache_dir: &str) -> Result<Vec<AlgebraPoolFull>> {
    let path = format!("cache/{cache_dir}/.cached-v3cl-pools.csv");
    let mut reader = csv::Reader::from_path(&path)
        .map_err(|e| anyhow!("failed to read {path}: {e}"))?;

    let mut pools = Vec::new();
    for row in reader.records() {
        let row = row.map_err(|e| anyhow!("invalid csv row: {e}"))?;

        let address = match H160::from_str(row.get(1).unwrap_or_default()) {
            Ok(a) => a,
            Err(_) => continue,
        };
        let token0 = match H160::from_str(row.get(6).unwrap_or_default()) {
            Ok(a) => a,
            Err(_) => continue,
        };
        let token1 = match H160::from_str(row.get(7).unwrap_or_default()) {
            Ok(a) => a,
            Err(_) => continue,
        };
        let decimals0 = row.get(8).unwrap_or("18").parse::<u8>().unwrap_or(18);
        let decimals1 = row.get(9).unwrap_or("18").parse::<u8>().unwrap_or(18);
        let fee = row.get(5).unwrap_or("0").parse::<u32>().unwrap_or(0);
        let tick_spacing = row.get(4).unwrap_or("1").parse::<i32>().unwrap_or(1);

        pools.push(AlgebraPoolFull {
            address,
            token0,
            token1,
            decimals0,
            decimals1,
            fee,
            sqrt_price_x96: U256::zero(),
            liquidity: U256::zero(),
            tick: 0,
            tick_spacing,
        });
    }

    Ok(pools)
}

/// Legacy thin-info loader kept for any callers that need `AlgebraPoolInfo`.
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
        pools.push(AlgebraPoolInfo {
            address,
            token0_symbol: row.get(2).unwrap_or("").to_string(),
            token1_symbol: row.get(3).unwrap_or("").to_string(),
            fee: row.get(5).unwrap_or("0").parse::<u32>().unwrap_or(0),
            tvl_usd: 0.0,
            tick_spacing: row.get(4).unwrap_or("1").parse::<i32>().unwrap_or(1),
        });
    }
    Ok(pools)
}

// ── Full pool fetch ───────────────────────────────────────────────────────────

/// Populate dynamic state (sqrtPriceX96, liquidity, tick) for a list of pools
/// whose static fields are already filled in (from CSV).
///
/// Uses Multicall3 in batches of BATCH_SIZE pools (2 calls each: slot0 +
/// liquidity), reducing ~12,000 individual calls to ~24 multicall requests
/// for 5,989 pools.
pub async fn fetch_full_uniswapv3cl_pools(
    provider: Arc<Provider<Ws>>,
    pools: Vec<AlgebraPoolFull>,
) -> Vec<AlgebraPoolFull> {
    use ethers::abi::{self, ParamType, Token};

    // slot0()     selector: keccak256("slot0()")     = 0x3850c7bd
    // liquidity() selector: keccak256("liquidity()") = 0x1a686502
    let slot0_cd: Vec<u8> = vec![0x38, 0x50, 0xc7, 0xbd];
    let liq_cd:   Vec<u8> = vec![0x1a, 0x68, 0x65, 0x02];

    let mc_addr = H160::from_str(MULTICALL3_ADDR).expect("multicall3 addr");
    let total = pools.len();
    let mut result: Vec<AlgebraPoolFull> = Vec::with_capacity(total);

    for (batch_idx, chunk) in pools.chunks(BATCH_SIZE).enumerate() {
        // Each pool → 2 call tuples: (address, allowFailure=true, calldata)
        let call_tokens: Vec<Token> = chunk
            .iter()
            .flat_map(|p| [
                Token::Tuple(vec![
                    Token::Address(p.address),
                    Token::Bool(true),
                    Token::Bytes(slot0_cd.clone()),
                ]),
                Token::Tuple(vec![
                    Token::Address(p.address),
                    Token::Bool(true),
                    Token::Bytes(liq_cd.clone()),
                ]),
            ])
            .collect();

        let encoded = abi::encode(&[Token::Array(call_tokens)]);
        let mut calldata = Vec::with_capacity(4 + encoded.len());
        calldata.extend_from_slice(&AGGREGATE3_SEL);
        calldata.extend_from_slice(&encoded);

        let tx = ethers::types::TransactionRequest::new()
            .to(mc_addr)
            .data(calldata);

        let raw: Bytes = match provider.call(&tx.into(), None).await {
            Ok(b) => b,
            Err(e) => {
                warn!("multicall3 batch {} failed (pools refresh on first swap): {:?}", batch_idx, e);
                continue;
            }
        };

        // Decode return: (bool, bytes)[]
        let decoded = match abi::decode(
            &[ParamType::Array(Box::new(ParamType::Tuple(vec![
                ParamType::Bool,
                ParamType::Bytes,
            ])))],
            &raw,
        ) {
            Ok(d) => d,
            Err(e) => {
                warn!("multicall3 batch {} decode error: {:?}", batch_idx, e);
                continue;
            }
        };

        let responses: Vec<(bool, Vec<u8>)> =
            if let Some(Token::Array(arr)) = decoded.into_iter().next() {
                arr.into_iter()
                    .filter_map(|t| {
                        if let Token::Tuple(mut f) = t {
                            if f.len() >= 2 {
                                let ok = matches!(f[0], Token::Bool(true));
                                let data = if let Token::Bytes(b) = f.remove(1) { b } else { vec![] };
                                Some((ok, data))
                            } else { None }
                        } else { None }
                    })
                    .collect()
            } else {
                continue;
            };

        if responses.len() < chunk.len() * 2 {
            warn!("multicall3 batch {}: expected {} results, got {}",
                batch_idx, chunk.len() * 2, responses.len());
            continue;
        }

        for (i, mut p) in chunk.iter().cloned().enumerate() {
            let slot0_ok = responses[i * 2].0;
            let sd       = &responses[i * 2].1;
            let liq_ok   = responses[i * 2 + 1].0;
            let ld       = &responses[i * 2 + 1].1;

            if slot0_ok && liq_ok && sd.len() >= 64 && ld.len() >= 32 {
                p.sqrt_price_x96 = U256::from_big_endian(&sd[0..32]);
                // int24 ABI-encoded as 32 bytes sign-extended; last 4 bytes → i32
                p.tick = i32::from_be_bytes([sd[28], sd[29], sd[30], sd[31]]);
                p.liquidity = U256::from_big_endian(&ld[0..32]);
                result.push(p);
            }
            // pools that fail stay at zero state; refreshed on first Swap event
        }
    }

    info!(
        "Multicall3 startup: {}/{} V3CL pools loaded ({} batches of {})",
        result.len(), total,
        (total + BATCH_SIZE - 1) / BATCH_SIZE,
        BATCH_SIZE,
    );
    result
}

// ── Per-block state refresh ───────────────────────────────────────────────────

/// Refresh sqrtPriceX96, liquidity, and tick for all tracked UniswapV3-CL pools.
///
/// Called every block (same pattern as `fetch_algebra_states`).
pub async fn fetch_uniswapv3cl_states(
    provider: Arc<Provider<Ws>>,
    pools: &[AlgebraPoolFull],
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
                            fee: 0,
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
