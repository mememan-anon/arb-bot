use anyhow::{anyhow, Result};
use ethers::contract::abigen;
use ethers::providers::{Middleware, Provider, Ws};
use ethers::types::{Bytes, H160, U256};
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
            tick_data: HashMap::new(),
        });
    }

    Ok(pools)
}

/// Fetch a small set of V3CL pools by address directly from chain.
/// Used to bootstrap explicit price-oracle pools that may not be in the CSV cache.
/// Both static fields (token0/1, fee, decimals) and live state (slot0, liquidity)
/// are populated in a single async pass.
pub async fn fetch_v3cl_oracle_pools(
    provider: Arc<Provider<Ws>>,
    addresses: Vec<H160>,
) -> Vec<AlgebraPoolFull> {
    use futures::future::join_all;

    let futs = addresses.into_iter().map(|addr| {
        let provider = provider.clone();
        async move {
            let pool = UniswapV3CLPool::new(addr, provider.clone());

            let ct0   = pool.token_0();
            let ct1   = pool.token_1();
            let cfee  = pool.fee();
            let cs0   = pool.slot_0();
            let cliq  = pool.liquidity();
            let (t0_res, t1_res, fee_res, slot0_res, liq_res) = tokio::join!(
                ct0.call(), ct1.call(), cfee.call(), cs0.call(), cliq.call(),
            );

            let token0 = match t0_res {
                Ok(a) => a,
                Err(e) => { warn!("price oracle pool {:?} token0() failed: {:?}", addr, e); return None; }
            };
            let token1 = match t1_res {
                Ok(a) => a,
                Err(e) => { warn!("price oracle pool {:?} token1() failed: {:?}", addr, e); return None; }
            };
            let fee = fee_res.unwrap_or(0) as u32;
            let (sqrt_price_x96, tick) = match slot0_res {
                Ok(s) => (s.0, s.1 as i32),
                Err(_) => (U256::zero(), 0i32),
            };
            let liquidity = U256::from(liq_res.unwrap_or(0u128));

            let erc0 = UniswapV3CLERC20::new(token0, provider.clone());
            let erc1 = UniswapV3CLERC20::new(token1, provider.clone());
            let cd0 = erc0.decimals();
            let cd1 = erc1.decimals();
            let (d0_res, d1_res) = tokio::join!(cd0.call(), cd1.call());
            let decimals0 = d0_res.unwrap_or(18);
            let decimals1 = d1_res.unwrap_or(18);

            info!(
                "Loaded price oracle pool {:?}: {:?}/{:?}  fee={}  dec=({},{})",
                addr, token0, token1, fee, decimals0, decimals1
            );

            Some(AlgebraPoolFull {
                address: addr,
                token0,
                token1,
                decimals0,
                decimals1,
                fee,
                sqrt_price_x96,
                liquidity,
                tick,
                tick_spacing: 1,
                tick_data: HashMap::new(),
            })
        }
    });

    join_all(futs).await.into_iter().flatten().collect()
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
            warn!("multicall3 batch {}: expected {} results, got {} — processing valid subset",
                batch_idx, chunk.len() * 2, responses.len());
        }

        for (i, mut p) in chunk.iter().cloned().enumerate() {
            let slot0_idx = i * 2;
            let liq_idx   = i * 2 + 1;
            if liq_idx >= responses.len() { break; }
            let slot0_ok = responses[slot0_idx].0;
            let sd       = &responses[slot0_idx].1;
            let liq_ok   = responses[liq_idx].0;
            let ld       = &responses[liq_idx].1;

            if slot0_ok && liq_ok && sd.len() >= 64 && ld.len() >= 32 {
                p.sqrt_price_x96 = U256::from_big_endian(&sd[0..32]);
                // int24 is ABI-encoded as the second 32-byte word (bytes 32-63),
                // sign-extended to int256.  Read last 4 bytes of that word → i32.
                p.tick = i32::from_be_bytes([sd[60], sd[61], sd[62], sd[63]]);
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

// ── Per-block / stale-guard state refresh ────────────────────────────────────

/// Refresh sqrtPriceX96, liquidity, and tick for a slice of UniswapV3-CL pools
/// using Multicall3 aggregate3 batching.
///
/// Called for 1–3 pools at a time (targeted per-path refresh before execution).
/// Uses the same BATCH_SIZE and slot0()+liquidity() pattern as
/// `fetch_full_uniswapv3cl_pools`.
pub async fn fetch_uniswapv3cl_states(
    provider: Arc<Provider<Ws>>,
    pools: &[AlgebraPoolFull],
) -> HashMap<H160, AlgebraState> {
    use ethers::abi::{self, ParamType, Token};

    if pools.is_empty() {
        return HashMap::new();
    }

    // slot0()     selector: keccak256("slot0()")     = 0x3850c7bd
    // liquidity() selector: keccak256("liquidity()") = 0x1a686502
    let slot0_cd: Vec<u8> = vec![0x38, 0x50, 0xc7, 0xbd];
    let liq_cd:   Vec<u8> = vec![0x1a, 0x68, 0x65, 0x02];

    let mc_addr = H160::from_str(MULTICALL3_ADDR).expect("multicall3 addr");
    let mut out: HashMap<H160, AlgebraState> = HashMap::with_capacity(pools.len());

    for (batch_idx, chunk) in pools.chunks(BATCH_SIZE).enumerate() {
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
                warn!("fetch_uniswapv3cl_states multicall batch {} failed: {:?}", batch_idx, e);
                continue;
            }
        };

        let decoded = match abi::decode(
            &[ParamType::Array(Box::new(ParamType::Tuple(vec![
                ParamType::Bool,
                ParamType::Bytes,
            ])))],
            &raw,
        ) {
            Ok(d) => d,
            Err(e) => {
                warn!("fetch_uniswapv3cl_states multicall decode error (batch {}): {:?}", batch_idx, e);
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
            warn!("fetch_uniswapv3cl_states batch {}: expected {} results, got {}",
                batch_idx, chunk.len() * 2, responses.len());
        }

        for (i, p) in chunk.iter().enumerate() {
            let slot0_idx = i * 2;
            let liq_idx   = i * 2 + 1;
            if liq_idx >= responses.len() { break; }

            let (slot0_ok, ref sd) = responses[slot0_idx];
            let (liq_ok, ref ld)   = responses[liq_idx];

            if slot0_ok && liq_ok && sd.len() >= 64 && ld.len() >= 32 {
                let sqrt_price = U256::from_big_endian(&sd[0..32]);
                // int24 tick is in the second 32-byte ABI word (bytes 32-63)
                let tick = i32::from_be_bytes([sd[60], sd[61], sd[62], sd[63]]);
                let liquidity = U256::from_big_endian(&ld[0..32]);
                out.insert(p.address, AlgebraState {
                    sqrt_price_x96: sqrt_price,
                    liquidity,
                    tick,
                    fee: 0, // fee unchanged; updated only on Swap events
                });
            }
        }
    }

    out
}

// ── Tick-data enrichment ──────────────────────────────────────────────────────

/// Selector for `ticks(int24)` — keccak256("ticks(int24)")[0..4].
/// Works for both Algebra (returns liquidityDelta at bytes 32-63) and
/// UniswapV3 (returns liquidityNet at bytes 32-63).
const TICKS_SEL: [u8; 4] = [0xf3, 0x0d, 0xba, 0x93];

/// Number of tick-spacing multiples to fetch on each side of the current tick.
/// 25 covers ~2.5 % for 10-spacing pools, ~15 % for 60-spacing.
/// This is intentionally wider than MAX_TICK_CROSSES (10) so that pools stay
/// "in range" longer between periodic re-enrichments and the scan-phase
/// simulation remains accurate without needing the confirm branch to fix it.
const TICKS_PER_SIDE: i32 = 25;

/// Maximum tick queries per Multicall3 batch.
const TICK_BATCH_SIZE: usize = 500;

/// Floor-division of `tick` to the nearest multiple of `spacing`.
fn floor_tick(tick: i32, spacing: i32) -> i32 {
    tick - tick.rem_euclid(spacing)
}

/// Populate `tick_data` for a slice of CL pools using Multicall3.
///
/// For each pool, queries `ticks(t)` at ±`TICKS_PER_SIDE` multiples of
/// `tick_spacing` around the current tick.  Only non-zero `liquidityNet`
/// values are stored, keeping the HashMap small.
///
/// Works for both Algebra and UniswapV3CL pools — both ABI-encode
/// `liquidityNet` / `liquidityDelta` (int128) at bytes 32..64 of the return.
pub async fn enrich_tick_data(
    provider: Arc<Provider<Ws>>,
    pools: &mut [AlgebraPoolFull],
) {
    use ethers::abi::{self, ParamType, Token};

    if pools.is_empty() {
        return;
    }

    // Build list of (pool_index, pool_address, tick_value) to query.
    let mut queries: Vec<(usize, H160, i32)> = Vec::new();

    for (idx, p) in pools.iter().enumerate() {
        if p.tick_spacing <= 0 || p.liquidity.is_zero() {
            continue;
        }
        let base = floor_tick(p.tick, p.tick_spacing);
        for offset in -TICKS_PER_SIDE..=TICKS_PER_SIDE {
            let t = base + offset * p.tick_spacing;
            if t >= -887272 && t <= 887272 {
                queries.push((idx, p.address, t));
            }
        }
    }

    if queries.is_empty() {
        return;
    }

    let mc_addr = H160::from_str(MULTICALL3_ADDR).expect("multicall3 addr");
    let total_queries = queries.len();
    let mut enriched = 0usize;

    for chunk in queries.chunks(TICK_BATCH_SIZE) {
        // Build call tokens
        let call_tokens: Vec<Token> = chunk
            .iter()
            .map(|(_idx, addr, tick_val)| {
                // ABI-encode int24 as int256 (sign-extended 32 bytes)
                let tick_bytes = {
                    let val = *tick_val as i32;
                    let mut buf = if val < 0 { [0xffu8; 32] } else { [0u8; 32] };
                    let be = val.to_be_bytes();
                    buf[28] = be[0];
                    buf[29] = be[1];
                    buf[30] = be[2];
                    buf[31] = be[3];
                    buf
                };
                let mut calldata = Vec::with_capacity(36);
                calldata.extend_from_slice(&TICKS_SEL);
                calldata.extend_from_slice(&tick_bytes);

                Token::Tuple(vec![
                    Token::Address(*addr),
                    Token::Bool(true), // allowFailure
                    Token::Bytes(calldata),
                ])
            })
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
                warn!("tick-data multicall batch failed: {:?}", e);
                continue;
            }
        };

        // Decode (bool, bytes)[]
        let decoded = match abi::decode(
            &[ParamType::Array(Box::new(ParamType::Tuple(vec![
                ParamType::Bool,
                ParamType::Bytes,
            ])))],
            &raw,
        ) {
            Ok(d) => d,
            Err(e) => {
                warn!("tick-data multicall decode error: {:?}", e);
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
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    })
                    .collect()
            } else {
                continue;
            };

        // Parse liquidityNet from bytes 32..64 of each response
        for (i, (pool_idx, _addr, tick_val)) in chunk.iter().enumerate() {
            if i >= responses.len() {
                break;
            }
            let (ok, ref data) = responses[i];
            if !ok || data.len() < 64 {
                continue;
            }
            // int128 is ABI-encoded as int256 (sign-extended) at bytes 32..64
            let mut liq_net_bytes = [0u8; 16];
            liq_net_bytes.copy_from_slice(&data[48..64]); // last 16 bytes of the 32-byte slot
            let liq_net = i128::from_be_bytes(liq_net_bytes);
            if liq_net != 0 {
                pools[*pool_idx].tick_data.insert(*tick_val, liq_net);
                enriched += 1;
            }
        }
    }

    info!(
        "Tick-data enrichment: {} non-zero entries across {} queries for {} pools",
        enriched,
        total_queries,
        pools.len(),
    );
}
