//! Pool loader — alloy-native pool discovery and CSV caching.
//!
//! Replaces the old `cfmms::sync_pairs` call in `pools.rs`.
//! Reads cached CSVs first; optionally crawls factory logs via `eth_getLogs`
//! when the cache is absent/stale.
//!
//! Two CSV formats are supported:
//!  - V2 format:  `address,version,token0,token1,decimals0,decimals1,fee,...`
//!  - V3CL format: `protocol,address,...,tickSpacing,fee,token0_address,token1_address,decimals0,decimals1,dex`

use alloy::primitives::{Address, B256};
use alloy::providers::Provider;
use alloy::rpc::types::{Filter, Log};
use alloy::transports::Transport;
use anyhow::{Context, Result};
use log::{info, warn};
use std::path::Path;
use std::str::FromStr;

use crate::searcher_pipeline::PoolMeta;
use crate::swap_types::PoolProtocol;

// ── Event topic selectors ────────────────────────────────────────────────────

/// `PairCreated(address indexed,address indexed,address,uint256)`
const PAIR_CREATED_TOPIC: &str =
    "0x0d3648bd0f6ba80134a33ba9275ac585d9d315f0ad8355cddefde31afa28d0e9";

/// `PoolCreated(address indexed,address indexed,uint24 indexed,int24,address)`
const POOL_CREATED_TOPIC: &str =
    "0x783cca1c0412dd0d695e784568c96da2e9c22ff989357a2e8b1d9b2b4e6b7118";

// ── Pool record (intermediate) ───────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct RawPool {
    pub address: Address,
    pub token0: Address,
    pub token1: Address,
    pub decimals0: u8,
    pub decimals1: u8,
    pub fee: u32,
    pub tick_spacing: i32,
    pub protocol: PoolProtocol,
}

impl RawPool {
    pub fn is_v3(&self) -> bool {
        self.protocol.is_v3()
    }

    pub fn to_pool_meta(&self) -> PoolMeta {
        PoolMeta {
            address: self.address,
            token0: self.token0,
            token1: self.token1,
            decimals0: self.decimals0,
            decimals1: self.decimals1,
            is_v3: self.is_v3(),
            fee: self.fee,
            tick_spacing: self.tick_spacing,
            protocol: self.protocol,
        }
    }
}

// ── CSV loading ──────────────────────────────────────────────────────────────

/// Load V2 pools from the standard `.cached-pools.csv`.
///
/// Header: `address,version,token0,token1,decimals0,decimals1,fee[,block_number,timestamp,id]`
pub fn load_v2_pools_from_csv(path: &str) -> Result<Vec<RawPool>> {
    if !Path::new(path).exists() {
        return Ok(vec![]);
    }

    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(true)
        .flexible(true)
        .from_path(path)
        .with_context(|| format!("opening V2 CSV {path}"))?;

    let mut pools = Vec::new();
    for result in rdr.records() {
        let rec = result?;
        let address = addr_from_field(&rec, 0)?;
        let version: u8 = rec.get(1).unwrap_or("2").parse().unwrap_or(2);
        let token0 = addr_from_field(&rec, 2)?;
        let token1 = addr_from_field(&rec, 3)?;
        let decimals0: u8 = rec.get(4).unwrap_or("18").parse().unwrap_or(18);
        let decimals1: u8 = rec.get(5).unwrap_or("18").parse().unwrap_or(18);
        let fee: u32 = rec.get(6).unwrap_or("30").parse().unwrap_or(30);

        // version codes:
        //   2 = UniswapV2 / generic V2 (0.30% fee, fee_factor 9970)
        //   3 = PancakeSwapV2          (0.25% fee, fee_factor 9975)
        //   4 = Aerodrome/Solidly stable (x³y+xy³=k)  → tick_spacing=0 convention
        //   5 = Aerodrome/Solidly volatile (x·y=k)    → tick_spacing=1 convention
        //  10 = BalancerV2, 11 = CurveTwoCrypto, 12 = CurveTriCrypto, 13 = MaverickV2
        let protocol = match version {
            2 => PoolProtocol::UniswapV2,
            3 => PoolProtocol::PancakeSwapV2,
            4 => PoolProtocol::Aerodrome, // Solidly stable
            5 => PoolProtocol::Aerodrome, // Solidly volatile
            10 => PoolProtocol::BalancerV2,
            11 => PoolProtocol::CurveTwoCrypto,
            12 => PoolProtocol::CurveTriCrypto,
            13 => PoolProtocol::MaverickV2,
            _ => PoolProtocol::UniswapV2,
        };

        // Aerodrome stable=version 4 (tick_spacing=0), volatile=version 5 (tick_spacing=1).
        // The rate estimator uses `tick_spacing == 0` to identify stable pools.
        let tick_spacing: i32 = if version == 5 { 1 } else { 0 };

        pools.push(RawPool {
            address,
            token0,
            token1,
            decimals0,
            decimals1,
            fee,
            tick_spacing,
            protocol,
        });
    }

    info!("Loaded {} V2 pools from {}", pools.len(), path);
    Ok(pools)
}

/// Load V3 CL pools from `.cached-v3cl-pools.csv`.
///
/// Header: `protocol,address,token0_symbol,token1_symbol,tickSpacing,fee,token0_address,token1_address,decimals0,decimals1,dex`
pub fn load_v3_pools_from_csv(path: &str) -> Result<Vec<RawPool>> {
    if !Path::new(path).exists() {
        return Ok(vec![]);
    }

    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(true)
        .flexible(true)
        .from_path(path)
        .with_context(|| format!("opening V3 CSV {path}"))?;

    let mut pools = Vec::new();
    for result in rdr.records() {
        let rec = result?;
        let protocol_str = rec.get(0).unwrap_or("UniswapV3CL");
        let address = addr_from_field(&rec, 1)?;
        let tick_spacing: i32 = rec.get(4).unwrap_or("1").parse().unwrap_or(1);
        let fee: u32 = rec.get(5).unwrap_or("500").parse().unwrap_or(500);
        let token0 = addr_from_field(&rec, 6)?;
        let token1 = addr_from_field(&rec, 7)?;
        let decimals0: u8 = rec.get(8).unwrap_or("18").parse().unwrap_or(18);
        let decimals1: u8 = rec.get(9).unwrap_or("18").parse().unwrap_or(18);
        let dex_str = rec.get(10).unwrap_or("");

        let protocol = map_v3_protocol(protocol_str, dex_str);

        pools.push(RawPool {
            address,
            token0,
            token1,
            decimals0,
            decimals1,
            fee,
            tick_spacing,
            protocol,
        });
    }

    info!("Loaded {} V3CL pools from {}", pools.len(), path);
    Ok(pools)
}

fn map_v3_protocol(protocol: &str, dex: &str) -> PoolProtocol {
    let combined = format!("{protocol}{dex}").to_lowercase();
    if combined.contains("sushi") {
        return PoolProtocol::SushiSwapV3;
    }
    if combined.contains("pancake") {
        return PoolProtocol::PancakeSwapV3;
    }
    if combined.contains("baseswap") {
        return PoolProtocol::BaseSwapV3;
    }
    if combined.contains("alienbase") {
        return PoolProtocol::AlienBaseV3;
    }
    if combined.contains("aero") || combined.contains("slipstream") {
        return PoolProtocol::Slipstream;
    }
    // Algebra V1.x pools: Thena Fusion (BSC), Quickswap (Polygon), etc.
    // CSV dex column often contains "thena", "fusion", or "algebra".
    if combined.contains("algebra")
        || combined.contains("thena")
        || combined.contains("fusion")
    {
        return PoolProtocol::AlgebraV1;
    }
    if combined.contains("maverick") {
        return PoolProtocol::MaverickV2;
    }
    if combined.contains("balancer") {
        return PoolProtocol::BalancerV2;
    }
    if combined.contains("curve") && combined.contains("tri") {
        return PoolProtocol::CurveTriCrypto;
    }
    if combined.contains("curve") {
        return PoolProtocol::CurveTwoCrypto;
    }
    PoolProtocol::UniswapV3 // default
}

/// Load all pools from the standard cache dir structure.
///
/// Returns a combined list of all V2 + V3CL pools.
pub fn load_all_pools_from_cache(cache_dir: &str) -> Result<Vec<RawPool>> {
    let v2_path = format!("{cache_dir}/.cached-pools.csv");
    let v3_path = format!("{cache_dir}/.cached-v3cl-pools.csv");

    let mut pools = load_v2_pools_from_csv(&v2_path)?;
    pools.extend(load_v3_pools_from_csv(&v3_path)?);
    info!("Total pools loaded from cache: {}", pools.len());
    Ok(pools)
}

// ── On-chain discovery ───────────────────────────────────────────────────────

/// Descriptor for a factory to crawl.
pub struct FactorySpec {
    pub address: Address,
    pub protocol: PoolProtocol,
    pub from_block: u64,
    /// tick_spacing for V2 pools (always 0).
    pub default_tick_spacing: i32,
}

/// Crawl a single factory for PairCreated/PoolCreated events in block-range chunks.
/// Returns raw `(token0, token1, pool_addr, fee, tick_spacing)` tuples without
/// decimals — call `fetch_token_decimals` separately if needed.
pub async fn crawl_factory_logs<T, P>(
    provider: &P,
    spec: &FactorySpec,
    to_block: u64,
    chunk_size: u64,
) -> Result<Vec<RawPool>>
where
    T: Transport + Clone,
    P: Provider<T, alloy::network::Ethereum>,
{
    let topic: B256 = if spec.protocol.is_v3() {
        POOL_CREATED_TOPIC.parse().expect("valid topic hex")
    } else {
        PAIR_CREATED_TOPIC.parse().expect("valid topic hex")
    };

    let mut pools = Vec::new();
    let mut from = spec.from_block;

    while from <= to_block {
        let to = (from + chunk_size - 1).min(to_block);
        let filter = Filter::new()
            .address(spec.address)
            .event_signature(topic)
            .from_block(from)
            .to_block(to);

        match provider.get_logs(&filter).await {
            Ok(logs) => {
                for log in logs {
                    if let Some(pool) = decode_log(&log, spec) {
                        pools.push(pool);
                    }
                }
            }
            Err(e) => {
                warn!("Log fetch error for factory {:?} blocks {from}-{to}: {e}", spec.address);
            }
        }

        from = to + 1;
    }

    info!(
        "Crawled factory {:?}: {} pools ({}-{})",
        spec.address,
        pools.len(),
        spec.from_block,
        to_block
    );
    Ok(pools)
}

fn decode_log(log: &Log, spec: &FactorySpec) -> Option<RawPool> {
    let topics = &log.inner.data.topics();
    if topics.len() < 3 {
        return None;
    }

    let token0 = Address::from_slice(&topics[1].as_slice()[12..]);
    let token1 = Address::from_slice(&topics[2].as_slice()[12..]);

    if spec.protocol.is_v3() {
        // V3: topic[3] = fee (uint24), data[0..32] = tickSpacing, data[32..64] = pool addr
        let fee = if topics.len() >= 4 {
            let fee_bytes = topics[3].as_slice();
            u32::from_be_bytes([fee_bytes[29], fee_bytes[30], fee_bytes[31], 0]) >> 8
                | (fee_bytes[28] as u32) << 16
        } else {
            500u32
        };
        // data layout: [int24 tickSpacing (32 bytes), address pool (32 bytes)]
        let data = log.inner.data.data.as_ref();
        if data.len() < 64 {
            return None;
        }
        let tick_spacing = i32::from_be_bytes([data[28], data[29], data[30], data[31]]);
        let pool_addr = Address::from_slice(&data[44..64]);

        Some(RawPool {
            address: pool_addr,
            token0,
            token1,
            decimals0: 18,
            decimals1: 18,
            fee,
            tick_spacing,
            protocol: spec.protocol,
        })
    } else {
        // V2: data[0..32] = pair address, data[32..64] = index
        let data = log.inner.data.data.as_ref();
        if data.len() < 32 {
            return None;
        }
        let pair_addr = Address::from_slice(&data[12..32]);

        Some(RawPool {
            address: pair_addr,
            token0,
            token1,
            decimals0: 18,
            decimals1: 18,
            fee: 30, // default; updated later or overridden by factory config
            tick_spacing: 0,
            protocol: spec.protocol,
        })
    }
}

// ── CSV writing ──────────────────────────────────────────────────────────────

/// Append new V2 pools to the cached CSV (or create it if missing).
pub fn append_v2_pools_to_csv(path: &str, pools: &[RawPool]) -> Result<()> {
    let needs_header = !Path::new(path).exists();
    if let Some(parent) = Path::new(path).parent() {
        std::fs::create_dir_all(parent)?;
    }

    let file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)?;
    let mut wtr = csv::WriterBuilder::new()
        .has_headers(false)
        .from_writer(file);

    if needs_header {
        wtr.write_record(&["address", "version", "token0", "token1", "decimals0", "decimals1", "fee", "block_number", "timestamp", "id"])?;
    }

    for (i, p) in pools.iter().enumerate() {
        let version: u8 = match p.protocol {
            PoolProtocol::Aerodrome => 5,
            _ => 2,
        };
        wtr.write_record(&[
            format!("{:?}", p.address),
            version.to_string(),
            format!("{:?}", p.token0),
            format!("{:?}", p.token1),
            p.decimals0.to_string(),
            p.decimals1.to_string(),
            p.fee.to_string(),
            "0".to_string(),
            "0".to_string(),
            i.to_string(),
        ])?;
    }
    wtr.flush()?;
    Ok(())
}

// ── Helper: address field parsing ────────────────────────────────────────────

fn addr_from_field(rec: &csv::StringRecord, idx: usize) -> Result<Address> {
    let s = rec.get(idx).unwrap_or("0x0000000000000000000000000000000000000000");
    Address::from_str(s.trim())
        .with_context(|| format!("parsing address field {idx}: '{s}'"))
}

// ── Convenience: build Vec<PoolMeta> from raw pools ──────────────────────────

pub fn raw_to_pool_metas(pools: &[RawPool]) -> Vec<PoolMeta> {
    pools.iter().map(|p| p.to_pool_meta()).collect()
}

pub fn raw_to_addresses(pools: &[RawPool]) -> Vec<Address> {
    pools.iter().map(|p| p.address).collect()
}

/// Returns `(pool_address, is_v3)` pairs for `ignition::start_pipeline`.
pub fn raw_to_pools_vec(pools: &[RawPool]) -> Vec<(Address, bool)> {
    pools.iter().map(|p| (p.address, p.is_v3())).collect()
}
