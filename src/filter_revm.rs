/// REVM roundtrip swap filter + GeckoTerminal API volume/token filter.
///
/// Two complementary filters:
///
/// 1. **REVM Roundtrip**: For each pool, simulate A→B→A. If >5% loss or
///    revert, the pool is blacklisted as toxic (fee-on-transfer, honeypot, etc.)
///
/// 2. **GeckoTerminal API**: Fetch the top-N pools by 24 h volume, add both
///    sides of each pair to an allowed-token set, and drop pools whose tokens
///    are not in that set (illiquid / irrelevant junk).
///    No API key required — the free tier allows 10 req/min.

use alloy::primitives::{address, Address, U160, U256};
use anyhow::anyhow;
use std::collections::{HashMap, HashSet};

// ── Chain-aware router registry ──────────────────────────────────────────────
//
// The REVM startup filter must route through contracts that exist on the active
// chain. Using Base addresses on BSC causes swap simulations to fail/revert.
//
// Keep this registry minimal and explicit. Unsupported protocol+chain pairs
// return None and are fail-open (the simulator/quoter still validates live).
use revm::primitives::{AccountInfo, ExecutionResult, TransactTo, KECCAK_EMPTY};
use alloy::sol_types::SolCall;

use crate::state_db::BlockStateDB;
use crate::calculation::v2;
use crate::swap_types::{PoolProtocol, SwapPath};

// ── REVM Roundtrip Filter ───────────────────────────────────────────────────

/// Result of a roundtrip validation.
#[derive(Debug, Clone, Copy)]
pub enum RoundtripResult {
    /// Pool is safe: roundtrip loss below threshold.
    Safe { loss_bps: u64 },
    /// Pool is toxic: too much loss or revert.
    Toxic { loss_bps: u64 },
    /// Could not evaluate (missing state).
    Unknown,
}

/// Validate a V2 pool by simulating a roundtrip swap: token0 → token1 → token0.
///
/// Returns the percentage loss in basis points. A healthy pool should have
/// ~60 bps loss (two 0.3% fees). Anything over `max_loss_bps` is toxic.
pub fn validate_v2_roundtrip(
    state_db: &BlockStateDB,
    pool: &Address,
    fee_factor: u64,
    test_amount: U256,
    max_loss_bps: u64,
) -> RoundtripResult {
    let (reserve0, reserve1) = match state_db.read_v2_reserves(pool) {
        Some(r) => r,
        None => return RoundtripResult::Unknown,
    };

    if reserve0.is_zero() || reserve1.is_zero() {
        return RoundtripResult::Toxic { loss_bps: 10_000 };
    }

    // Forward: token0 → token1
    let mid = v2::get_amount_out_v2(test_amount, reserve0, reserve1, fee_factor);
    if mid.is_zero() {
        return RoundtripResult::Toxic { loss_bps: 10_000 };
    }

    // Update reserves after forward swap
    let new_r0 = reserve0 + test_amount;
    let new_r1 = reserve1 - mid;

    // Reverse: token1 → token0
    let back = v2::get_amount_out_v2(mid, new_r1, new_r0, fee_factor);
    if back.is_zero() {
        return RoundtripResult::Toxic { loss_bps: 10_000 };
    }

    // Calculate loss
    if back >= test_amount {
        return RoundtripResult::Safe { loss_bps: 0 };
    }

    let loss = test_amount - back;
    let loss_bps = (loss * U256::from(10_000u64) / test_amount).to::<u64>();

    if loss_bps > max_loss_bps {
        RoundtripResult::Toxic { loss_bps }
    } else {
        RoundtripResult::Safe { loss_bps }
    }
}

/// Batch validate V2 pools and return the set of safe pools.
pub fn batch_validate_v2(
    state_db: &BlockStateDB,
    pools: &[(Address, u64)], // (pool_address, fee_factor)
    test_amount: U256,
    max_loss_bps: u64,
) -> HashSet<Address> {
    let mut safe = HashSet::new();
    for (pool, fee_factor) in pools {
        match validate_v2_roundtrip(state_db, pool, *fee_factor, test_amount, max_loss_bps) {
            RoundtripResult::Safe { .. } => {
                safe.insert(*pool);
            }
            RoundtripResult::Toxic { loss_bps } => {
                log::debug!("Pool {} is toxic: {loss_bps} bps loss", pool);
            }
            RoundtripResult::Unknown => {
                log::debug!("Pool {} state unknown, skipping", pool);
            }
        }
    }
    safe
}

// ── GeckoTerminal API Filter ─────────────────────────────────────────────────

/// A token address+symbol pair collected from a GeckoTerminal pool.
#[derive(Debug, Clone)]
pub struct GeckoToken {
    pub address: String,
    pub symbol:  String,
}

/// Map a chain name to its GeckoTerminal network slug.
fn chain_to_gecko(chain: &str) -> &'static str {
    match chain.to_lowercase().as_str() {
        "base"               => "base",
        "eth" | "ethereum"   => "eth",
        "arbitrum" | "arb"  => "arbitrum",
        "optimism" | "op"   => "optimism",
        "avalanche" | "avax" => "avax",
        "bsc"                => "bsc",
        "sonic"              => "sonic",
        _                    => "base",
    }
}

/// Extract the `0xADDR` portion from a GeckoTerminal relationship ID.
///
/// IDs are formatted as `"{network}_{0xADDR}"`, e.g.
/// `"base_0x4200000000000000000000000000000000000006"`.
#[inline]
fn gecko_id_to_addr(id: &str) -> Option<&str> {
    id.find("0x").map(|pos| &id[pos..])
}

/// Fetch top tokens by 24 h volume from the GeckoTerminal API.
///
/// Paginates `GET /api/v2/networks/{network}/pools?sort=h24_volume_usd_desc`.
/// Both sides of every pool are added to the result so the allowed-token set
/// covers any pair that appears in a high-volume pool — not just WETH pairs.
///
/// Requests are **staggered at 6 s apart** (≤ 10 req/min, within the free tier).
/// On a 429 the function stops early and returns whatever was collected.
///
/// The `_api_key` parameter is kept for API compatibility but is not used.
pub async fn fetch_top_tokens(
    _api_key: &str,
    chain: &str,
    limit: usize,
) -> Result<Vec<GeckoToken>, String> {
    use tokio::time::{sleep, Duration};

    let client = reqwest::Client::builder()
        .user_agent("arb-bot/1.0")
        .build()
        .map_err(|e| format!("HTTP client build failed: {e}"))?;

    let network = chain_to_gecko(chain);

    // GeckoTerminal returns 20 pools per page → up to 40 unique tokens per page.
    // Fetch enough pages to comfortably cover `limit` tokens; cap at 20 pages.
    let pages_needed: u64 = (((limit as u64) + 39) / 40).max(3).min(20);

    let mut tokens: Vec<GeckoToken> = Vec::with_capacity(limit + 1);
    let mut seen: HashSet<String> = HashSet::new();

    for page in 1..=pages_needed {
        if page > 1 {
            // Stagger: 6 s gap keeps us at ≤ 10 req/min (free-tier limit).
            sleep(Duration::from_millis(6_000)).await;
        }

        let url = format!(
            "https://api.geckoterminal.com/api/v2/networks/{network}/pools\
             ?sort=h24_volume_usd_desc&page={page}"
        );

        // Retry up to 3 times on 429 with exponential backoff.
        let mut resp_result = None;
        for attempt in 0u32..3 {
            let r = client
                .get(&url)
                .header("Accept", "application/json;version=20230302")
                .send()
                .await
                .map_err(|e| format!("GeckoTerminal request failed (page {page}): {e}"))?;

            if r.status().as_u16() == 429 {
                let backoff = 15_000u64 * 2u64.pow(attempt); // 15s, 30s, 60s
                log::warn!(
                    "GeckoTerminal 429 on page {page} (attempt {}); backing off {}s",
                    attempt + 1,
                    backoff / 1000
                );
                sleep(Duration::from_millis(backoff)).await;
                continue;
            }

            resp_result = Some(r);
            break;
        }

        let resp = match resp_result {
            Some(r) => r,
            None => {
                log::warn!(
                    "GeckoTerminal rate-limited on page {page} after 3 retries; stopping ({} tokens collected)",
                    tokens.len()
                );
                break;
            }
        };

        let status = resp.status();
        if !status.is_success() {
            log::warn!("GeckoTerminal HTTP {status} on page {page}; stopping early");
            break;
        }

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("GeckoTerminal JSON parse failed (page {page}): {e}"))?;

        let pools = match json.pointer("/data").and_then(|d| d.as_array()) {
            Some(p) => p,
            None => {
                log::warn!("GeckoTerminal unexpected response shape on page {page}");
                break;
            }
        };

        if pools.is_empty() {
            break; // no more data
        }

        for pool in pools {
            if tokens.len() >= limit {
                break;
            }

            // Addresses live in relationships, formatted as "{network}_{0xADDR}".
            let base_id = pool
                .pointer("/relationships/base_token/data/id")
                .and_then(|v| v.as_str())
                .and_then(gecko_id_to_addr)
                .unwrap_or("")
                .to_lowercase();

            let quote_id = pool
                .pointer("/relationships/quote_token/data/id")
                .and_then(|v| v.as_str())
                .and_then(gecko_id_to_addr)
                .unwrap_or("")
                .to_lowercase();

            // Derive symbols from the pool name: "BASE / QUOTE".
            let name = pool
                .pointer("/attributes/name")
                .and_then(|n| n.as_str())
                .unwrap_or("/");
            let parts: Vec<&str> = name.splitn(2, " / ").collect();
            let base_sym  = parts.first().copied().unwrap_or("?").to_string();
            let quote_sym = parts.get(1).copied().unwrap_or("?").to_string();

            for (addr, sym) in [(&base_id, base_sym), (&quote_id, quote_sym)] {
                if !addr.is_empty() && seen.insert(addr.clone()) {
                    tokens.push(GeckoToken { address: addr.clone(), symbol: sym });
                }
            }
        }

        log::debug!(
            "GeckoTerminal page {page}/{pages_needed}: {} unique tokens so far",
            tokens.len()
        );

        if tokens.len() >= limit {
            break;
        }
    }

    if tokens.is_empty() {
        return Err("GeckoTerminal returned no tokens".to_string());
    }

    log::info!(
        "GeckoTerminal: collected {} unique token addresses for chain '{chain}'",
        tokens.len()
    );
    Ok(tokens)
}

/// Convert GeckoTerminal token addresses to a `HashSet<Address>`.
pub fn gecko_to_address_set(tokens: &[GeckoToken]) -> HashSet<Address> {
    let mut set = HashSet::with_capacity(tokens.len());
    for token in tokens {
        let addr_str = token.address.trim_start_matches("0x");
        if let Ok(bytes) = hex::decode(format!("{:0>40}", addr_str)) {
            if bytes.len() == 20 {
                let mut arr = [0u8; 20];
                arr.copy_from_slice(&bytes);
                set.insert(Address::from(arr));
            }
        }
    }
    set
}

/// Filter: does a swap path only use tokens in the allowed set?
pub fn path_uses_allowed_tokens(path: &SwapPath, allowed: &HashSet<Address>) -> bool {
    path.steps.iter().all(|step| {
        allowed.contains(&step.token_in) && allowed.contains(&step.token_out)
    })
}

/// Combined filter: validates both token eligibility and pool health.
pub struct CombinedFilter {
    pub allowed_tokens: HashSet<Address>,
    pub safe_pools: HashSet<Address>,
    pub max_loss_bps: u64,
}

impl CombinedFilter {
    pub fn new(max_loss_bps: u64) -> Self {
        Self {
            allowed_tokens: HashSet::new(),
            safe_pools: HashSet::new(),
            max_loss_bps,
        }
    }

    /// Should this path be considered? Returns true if it passes all filters.
    pub fn accept_path(&self, path: &SwapPath) -> bool {
        // Check all pools are safe
        let all_safe = path
            .steps
            .iter()
            .all(|s| self.safe_pools.contains(&s.pool_address));

        // Check all tokens are in the allowed set (skip if empty = no filter)
        let tokens_ok = self.allowed_tokens.is_empty()
            || path_uses_allowed_tokens(path, &self.allowed_tokens);

        all_safe && tokens_ok
    }

    /// Update the allowed token set from DexScreener data.
    pub fn set_allowed_tokens(&mut self, tokens: HashSet<Address>) {
        self.allowed_tokens = tokens;
    }

    /// Add a safe pool.
    pub fn add_safe_pool(&mut self, pool: Address) {
        self.safe_pools.insert(pool);
    }

    /// Add multiple safe pools.
    pub fn add_safe_pools(&mut self, pools: HashSet<Address>) {
        self.safe_pools.extend(pools);
    }
}

/// Filter a path set with a pre-built `CombinedFilter`.
///
/// Returns only paths that satisfy both:
/// - all pools in `safe_pools`
/// - all tokens in `allowed_tokens` (if non-empty)
pub fn filter_paths(paths: Vec<SwapPath>, filter: &CombinedFilter) -> Vec<SwapPath> {
    paths
        .into_iter()
        .filter(|p| filter.accept_path(p))
        .collect()
}


// ── REVM Swap Filter (construct_slot_map + filter_pools_by_swap) ────────────
//
// Two-phase startup filter ported from BaseBuster's filter.rs:
//
//   Phase 1 — `construct_slot_map`:
//     For each unique token in the pool set, detect the ERC20 balance storage
//     slot by comparing keccak256(holder, slot_idx) candidates against the
//     actual `balanceOf(holder)` returned by eth_call.  No local node needed.
//
//   Phase 2 — `filter_pools_by_swap`:
//     For each pool, inject unlimited token balances into the detected slots,
//     then run a full ERC20 approve + router swap (A→B and B→A) in REVM.
//     Pools that revert or return < 95% on the roundtrip are discarded.
//
// Together these filter out liquidity-free pools, honeypot tokens,
// fee-on-transfer tokens, and broken router integrations before the pipeline
// starts, without depending on any local node (all state fetched lazily from
// the HTTP RPC endpoint).

use crate::gen_alloy::{
    ERC20Token, IAerodromeRouter, ISolidlyRouter, ISlipstreamRouter, IV2Router, IV3Router, IV3RouterDeadline,
};
use crate::pool_loader::RawPool;

// ── Chain-aware router registry ──────────────────────────────────────────────
//
// The startup REVM filter must use routers deployed on the active chain.
// Using Base router addresses while running on BSC causes false reverts.

/// Dummy caller address for filter simulations.
const FILTER_ACCOUNT: Address = address!("0000000000000000000000000000000000000001");
/// Absurdly large token balance injected into every token for filter tests.
const TOKEN_LOTS: U256 = U256::from_limbs([u64::MAX, u64::MAX, 0, 0]);
/// Gas limit for each filter EVM call.
const FILTER_GAS: u64 = 800_000;
/// Test swap amount: 1e15 units (0.001 WETH-equivalent), matching BaseBuster.
const TEST_AMOUNT: U256 = U256::from_limbs([1_000_000_000_000_000u64, 0, 0, 0]);

/// Which router ABI variant to use for a given pool.
#[derive(Clone, Copy)]
enum RouterVariant {
    V2,
    V3Basic,
    V3Deadline,
    /// Aerodrome router: 4-field Route (from, to, stable, factory).
    Aerodrome { stable: bool },
    /// Original Solidly V2 router: 3-field Route (from, to, stable).
    /// Used by Thena on BSC and other pre-Aerodrome Solidly forks.
    Solidly { stable: bool },
    Slipstream,
}

/// Returns (router_address, variant) for the active chain.
/// Unknown protocol+chain pairs return None (fail-open: kept for simulator/quoter).
fn protocol_router_key(protocol: PoolProtocol) -> Option<&'static str> {
    match protocol {
        PoolProtocol::UniswapV2 => Some("uniswapv2"),
        PoolProtocol::SushiSwapV2 => Some("sushiswapv2"),
        PoolProtocol::PancakeSwapV2 => Some("pancakeswapv2"),
        PoolProtocol::BaseSwapV2 => Some("baseswapv2"),
        PoolProtocol::AlienBaseV2 => Some("alienbasev2"),
        PoolProtocol::Aerodrome => Some("aerodrome"),
        PoolProtocol::Slipstream => Some("slipstream"),
        PoolProtocol::UniswapV3 => Some("uniswapv3"),
        PoolProtocol::SushiSwapV3 => Some("sushiswapv3"),
        PoolProtocol::BaseSwapV3 => Some("baseswapv3"),
        PoolProtocol::PancakeSwapV3 => Some("pancakeswapv3"),
        PoolProtocol::AlienBaseV3 => Some("alienbasev3"),
        PoolProtocol::AlgebraV1
        | PoolProtocol::BalancerV2
        | PoolProtocol::CurveTwoCrypto
        | PoolProtocol::CurveTriCrypto
        | PoolProtocol::MaverickV2 => None,
    }
}

/// Router registry: protocol -> address, plus a flag indicating whether
/// Aerodrome-protocol pools should use the Solidly V2 (3-field Route) ABI
/// instead of the Aerodrome (4-field Route) ABI.
pub struct RouterRegistry {
    pub map: HashMap<PoolProtocol, Address>,
    /// When true, `PoolProtocol::Aerodrome` pools use `ISolidlyRouter` (3-field Route).
    /// Set automatically when the config contains a `solidly` key instead of `aerodrome`.
    pub solidly_mode: bool,
}

impl RouterRegistry {
    #[inline]
    pub fn len(&self) -> usize { self.map.len() }
    #[inline]
    pub fn is_empty(&self) -> bool { self.map.is_empty() }
    #[inline]
    pub fn contains_key(&self, k: &PoolProtocol) -> bool { self.map.contains_key(k) }
    #[inline]
    pub fn keys(&self) -> impl Iterator<Item = &PoolProtocol> { self.map.keys() }
    #[inline]
    pub fn get(&self, k: &PoolProtocol) -> Option<&Address> { self.map.get(k) }
}

/// Build a protocol->router map from TOML `[revm_filter.routers]`.
pub fn build_router_registry(raw_routers: &HashMap<String, String>) -> RouterRegistry {
    let mut map = HashMap::new();
    let mut solidly_mode = false;

    for (key, value) in raw_routers {
        let k = key.to_ascii_lowercase();
        let protocol = match k.as_str() {
            "uniswapv2" => Some(PoolProtocol::UniswapV2),
            "sushiswapv2" => Some(PoolProtocol::SushiSwapV2),
            "pancakeswapv2" => Some(PoolProtocol::PancakeSwapV2),
            "baseswapv2" => Some(PoolProtocol::BaseSwapV2),
            "alienbasev2" => Some(PoolProtocol::AlienBaseV2),
            "aerodrome" => Some(PoolProtocol::Aerodrome),
            // "solidly" = original Solidly V2 fork (Thena on BSC) — 3-field Route.
            // Maps to the same PoolProtocol::Aerodrome but selects ISolidlyRouter ABI.
            "solidly" => {
                solidly_mode = true;
                Some(PoolProtocol::Aerodrome)
            }
            "slipstream" => Some(PoolProtocol::Slipstream),
            "uniswapv3" => Some(PoolProtocol::UniswapV3),
            "sushiswapv3" => Some(PoolProtocol::SushiSwapV3),
            "baseswapv3" => Some(PoolProtocol::BaseSwapV3),
            "pancakeswapv3" => Some(PoolProtocol::PancakeSwapV3),
            "alienbasev3" => Some(PoolProtocol::AlienBaseV3),
            _ => None,
        };

        let Some(protocol) = protocol else { continue };
        let Ok(addr) = value.parse::<Address>() else { continue };
        map.insert(protocol, addr);
    }

    RouterRegistry { map, solidly_mode }
}

fn protocol_to_router(
    protocol: PoolProtocol,
    tick_spacing: i32,
    router_registry: &RouterRegistry,
) -> Option<(Address, RouterVariant)> {
    let variant = match protocol {
        PoolProtocol::UniswapV2
        | PoolProtocol::SushiSwapV2
        | PoolProtocol::PancakeSwapV2
        | PoolProtocol::BaseSwapV2
        | PoolProtocol::AlienBaseV2 => RouterVariant::V2,
        PoolProtocol::Aerodrome => {
            let stable = tick_spacing == 0;
            if router_registry.solidly_mode {
                RouterVariant::Solidly { stable }
            } else {
                RouterVariant::Aerodrome { stable }
            }
        }
        PoolProtocol::Slipstream => RouterVariant::Slipstream,
        PoolProtocol::UniswapV3
        | PoolProtocol::AlienBaseV3
        | PoolProtocol::PancakeSwapV3 => RouterVariant::V3Basic,
        PoolProtocol::SushiSwapV3
        | PoolProtocol::BaseSwapV3 => RouterVariant::V3Deadline,
        PoolProtocol::AlgebraV1 => return None,
        PoolProtocol::BalancerV2
        | PoolProtocol::CurveTwoCrypto
        | PoolProtocol::CurveTriCrypto
        | PoolProtocol::MaverickV2 => return None,
    };

    let router = *router_registry.get(&protocol)?;
    Some((router, variant))
}

/// Build router swap calldata for the given pool, direction, and amount.
fn build_swap_calldata(
    pool: &RawPool,
    variant: RouterVariant,
    recipient: Address,
    amount_in: U256,
    zero_for_one: bool,
) -> Vec<u8> {
    let (token_in, token_out) = if zero_for_one {
        (pool.token0, pool.token1)
    } else {
        (pool.token1, pool.token0)
    };

    match variant {
        RouterVariant::V2 => IV2Router::swapExactTokensForTokensCall {
            amountIn: amount_in,
            amountOutMin: U256::ZERO,
            path: vec![token_in, token_out],
            to: recipient,
            deadline: U256::MAX,
        }
        .abi_encode(),

        RouterVariant::V3Basic => IV3Router::exactInputSingleCall {
            params: IV3Router::ExactInputSingleParams {
                tokenIn: token_in,
                tokenOut: token_out,
                fee: pool.fee.try_into()
                    .unwrap_or_else(|_| alloy::primitives::Uint::from(3000u32)),
                recipient,
                amountIn: amount_in,
                amountOutMinimum: U256::ZERO,
                sqrtPriceLimitX96: U160::ZERO,
            },
        }
        .abi_encode(),

        RouterVariant::V3Deadline => IV3RouterDeadline::exactInputSingleCall {
            params: IV3RouterDeadline::ExactInputSingleParams {
                tokenIn: token_in,
                tokenOut: token_out,
                fee: pool.fee.try_into()
                    .unwrap_or_else(|_| alloy::primitives::Uint::from(3000u32)),
                recipient,
                deadline: U256::MAX,
                amountIn: amount_in,
                amountOutMinimum: U256::ZERO,
                sqrtPriceLimitX96: U160::ZERO,
            },
        }
        .abi_encode(),

        RouterVariant::Aerodrome { stable } => IAerodromeRouter::swapExactTokensForTokensCall {
            amountIn: amount_in,
            amountOutMin: U256::ZERO,
            routes: vec![IAerodromeRouter::Route {
                from: token_in,
                to: token_out,
                stable,
                factory: Address::ZERO,
            }],
            to: recipient,
            deadline: U256::MAX,
        }
        .abi_encode(),

        RouterVariant::Solidly { stable } => ISolidlyRouter::swapExactTokensForTokensCall {
            amountIn: amount_in,
            amountOutMin: U256::ZERO,
            routes: vec![ISolidlyRouter::Route {
                from: token_in,
                to: token_out,
                stable,
            }],
            to: recipient,
            deadline: U256::MAX,
        }
        .abi_encode(),

        RouterVariant::Slipstream => ISlipstreamRouter::exactInputSingleCall {
            params: ISlipstreamRouter::ExactInputSingleParams {
                tokenIn: token_in,
                tokenOut: token_out,
                tickSpacing: pool.tick_spacing.try_into()
                    .unwrap_or_else(|_| alloy::primitives::Signed::unchecked_from(100i128)),
                recipient,
                deadline: U256::MAX,
                amountIn: amount_in,
                amountOutMinimum: U256::ZERO,
                sqrtPriceLimitX96: U160::ZERO,
            },
        }
        .abi_encode(),
    }
}

/// Decode the output of a router swap call. V2/Aerodrome/Solidly return `uint256[]`;
/// V3/Slipstream return a plain `uint256`.
fn decode_swap_output(data: &[u8], variant: RouterVariant) -> U256 {
    match variant {
        RouterVariant::V2 | RouterVariant::Aerodrome { .. } | RouterVariant::Solidly { .. } => {
            // ABI: 0x20 offset (32) | length N (32) | [values...] (N*32)
            if data.len() < 96 {
                return U256::ZERO;
            }
            let n_bytes: [u8; 32] = match data[32..64].try_into() {
                Ok(b) => b,
                Err(_) => return U256::ZERO,
            };
            let n = U256::from_be_bytes(n_bytes).to::<usize>();
            let end = 64 + n * 32;
            if n == 0 || data.len() < end {
                return U256::ZERO;
            }
            let last: [u8; 32] = match data[end - 32..end].try_into() {
                Ok(b) => b,
                Err(_) => return U256::ZERO,
            };
            U256::from_be_bytes(last)
        }
        RouterVariant::V3Basic | RouterVariant::V3Deadline | RouterVariant::Slipstream => {
            if data.len() < 32 {
                return U256::ZERO;
            }
            U256::from_be_bytes(data[0..32].try_into().unwrap_or([0u8; 32]))
        }
    }
}

/// Execute a single EVM call against `db`, committing state changes.
/// Returns the output bytes on `Success`, or `None` on revert/halt.
fn evm_call_commit(
    db: &mut BlockStateDB,
    from: Address,
    to: Address,
    calldata: Vec<u8>,
    value: U256,
) -> Option<Vec<u8>> {
    let mut evm = revm::Evm::builder()
        .with_db(db)
        .modify_tx_env(|tx| {
            tx.caller = from;
            tx.transact_to = TransactTo::Call(to);
            tx.data = alloy::primitives::Bytes::from(calldata);
            tx.value = value;
            tx.gas_limit = FILTER_GAS;
            tx.gas_price = alloy::primitives::U256::ZERO;
        })
        .build();
    let result = evm.transact_commit().ok()?;
    match result {
        ExecutionResult::Success { output, .. } => Some(output.into_data().to_vec()),
        _ => None,
    }
}

/// Test a single pool by running an A→B and B→A swap through the router in
/// REVM. Returns `true` if the roundtrip returns >= 95% of the input.
///
/// Injects huge token balances before the test so the simulation always has
/// enough input token. The caller must pass the pre-built `slot_map`.
fn test_pool_filter_reason(
    db: &mut BlockStateDB,
    pool: &RawPool,
    slot_map: &HashMap<Address, (u64, SlotLayout)>,
    router_registry: &RouterRegistry,
) -> Result<(), &'static str> {
    // Must know the balance slots for both tokens.
    let s0 = match slot_map.get(&pool.token0) {
        Some(&(slot_idx, layout)) => compute_slot(FILTER_ACCOUNT, slot_idx, layout),
        None => return Err("missing_slot_token0"),
    };
    let s1 = match slot_map.get(&pool.token1) {
        Some(&(slot_idx, layout)) => compute_slot(FILTER_ACCOUNT, slot_idx, layout),
        None => return Err("missing_slot_token1"),
    };

    let (router, variant) = if protocol_router_key(pool.protocol).is_some() {
        match protocol_to_router(pool.protocol, pool.tick_spacing, router_registry) {
            Some(r) => r,
            None => return Err("missing_router_config"),
        }
    } else {
        // Exotic protocols pass automatically — the REVM quoter handles them.
        return Ok(());
    };

    // Inject unlimited balance for FILTER_ACCOUNT into both tokens.
    db.inject_slot(pool.token0, s0, TOKEN_LOTS);
    db.inject_slot(pool.token1, s1, TOKEN_LOTS);

    let approve = ERC20Token::approveCall {
        spender: router,
        amount: TOKEN_LOTS,
    }
    .abi_encode();

    // Approve token0 → router
    if evm_call_commit(db, FILTER_ACCOUNT, pool.token0, approve.clone(), U256::ZERO).is_none() {
        return Err("approve_token0_failed");
    }
    // Approve token1 → router
    if evm_call_commit(db, FILTER_ACCOUNT, pool.token1, approve, U256::ZERO).is_none() {
        return Err("approve_token1_failed");
    }

    // Swap A → B
    let cd_ab = build_swap_calldata(pool, variant, FILTER_ACCOUNT, TEST_AMOUNT, true);
    let out_ab = match evm_call_commit(db, FILTER_ACCOUNT, router, cd_ab, U256::ZERO) {
        Some(o) => o,
        None => return Err("swap_ab_failed"),
    };
    let amt_b = decode_swap_output(&out_ab, variant);
    if amt_b.is_zero() {
        return Err("swap_ab_zero_output");
    }

    // Swap B → A
    let cd_ba = build_swap_calldata(pool, variant, FILTER_ACCOUNT, amt_b, false);
    let out_ba = match evm_call_commit(db, FILTER_ACCOUNT, router, cd_ba, U256::ZERO) {
        Some(o) => o,
        None => return Err("swap_ba_failed"),
    };
    let amt_a_back = decode_swap_output(&out_ba, variant);

    // Roundtrip check: at least 95% back.
    let lower = TEST_AMOUNT
        .saturating_mul(U256::from(95u64))
        / U256::from(100u64);
    if amt_a_back >= lower {
        Ok(())
    } else {
        Err("roundtrip_below_95pct")
    }
}

/// Storage layout variant for ERC20 balance mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotLayout {
    /// Solidity: keccak256(holder ++ slot_idx)  (OpenZeppelin, most tokens)
    Solidity,
    /// Vyper: keccak256(slot_idx ++ holder)  (Vyper tokens, some custom impls)
    Vyper,
}

/// Build a keccak256-based mapping slot: `keccak256(abi.encode(holder, slot_idx))`.
///
/// This is the standard ERC20 balance mapping slot for OpenZeppelin-style tokens.
fn mapping_slot(holder: Address, slot_idx: u64) -> U256 {
    let mut buf = [0u8; 64];
    // holder right-aligned in the first 32 bytes (address is 20 bytes)
    buf[12..32].copy_from_slice(holder.as_slice());
    // slot_idx right-aligned in the last 32 bytes
    let idx = U256::from(slot_idx).to_be_bytes::<32>();
    buf[32..64].copy_from_slice(&idx);
    U256::from_be_bytes(alloy::primitives::keccak256(buf).0)
}

/// Build a Vyper-style mapping slot: `keccak256(abi.encode(slot_idx, holder))`.
///
/// Vyper reverses the argument order compared to Solidity.
fn mapping_slot_vyper(holder: Address, slot_idx: u64) -> U256 {
    let mut buf = [0u8; 64];
    // slot_idx right-aligned in the first 32 bytes
    let idx = U256::from(slot_idx).to_be_bytes::<32>();
    buf[0..32].copy_from_slice(&idx);
    // holder right-aligned in the last 32 bytes (address is 20 bytes)
    buf[44..64].copy_from_slice(holder.as_slice());
    U256::from_be_bytes(alloy::primitives::keccak256(buf).0)
}

/// Compute the correct mapping slot for a given holder, slot index, and layout variant.
fn compute_slot(holder: Address, slot_idx: u64, layout: SlotLayout) -> U256 {
    match layout {
        SlotLayout::Solidity => mapping_slot(holder, slot_idx),
        SlotLayout::Vyper => mapping_slot_vyper(holder, slot_idx),
    }
}

/// Fetch `balanceOf(holder)` from `token` via eth_call.
async fn rpc_balance_of(
    token: Address,
    holder: Address,
    client: &reqwest::Client,
    rpc_url: &str,
) -> U256 {
    let calldata_hex = format!(
        "0x70a08231000000000000000000000000{}",
        hex::encode(holder.as_slice())
    );
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "eth_call",
        "params": [{"to": format!("{token:?}"), "data": calldata_hex}, "latest"],
        "id": 1
    });
    match client.post(rpc_url).json(&body).send().await {
        Ok(resp) => {
            if let Ok(json) = resp.json::<serde_json::Value>().await {
                if let Some(s) = json.get("result").and_then(|v| v.as_str()) {
                    let s = s.trim_start_matches("0x");
                    if s.len() >= 64 {
                        let padded = format!("{:0>64}", &s[s.len().saturating_sub(64)..]);
                        if let Ok(bytes) = hex::decode(&padded) {
                            if bytes.len() == 32 {
                                let mut arr = [0u8; 32];
                                arr.copy_from_slice(&bytes);
                                return U256::from_be_bytes(arr);
                            }
                        }
                    }
                }
            }
            U256::ZERO
        }
        Err(_) => U256::ZERO,
    }
}

/// Fetch `eth_getStorageAt(token, slot, "latest")`.
async fn rpc_get_storage_at(
    token: Address,
    slot: U256,
    client: &reqwest::Client,
    rpc_url: &str,
) -> U256 {
    let slot_hex = format!("0x{}", hex::encode(slot.to_be_bytes::<32>()));
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "eth_getStorageAt",
        "params": [format!("{token:?}"), slot_hex, "latest"],
        "id": 1
    });
    match client.post(rpc_url).json(&body).send().await {
        Ok(resp) => {
            if let Ok(json) = resp.json::<serde_json::Value>().await {
                if let Some(s) = json.get("result").and_then(|v| v.as_str()) {
                    let s = s.trim_start_matches("0x");
                    let padded = format!("{:0>64}", s);
                    if let Ok(bytes) = hex::decode(&padded) {
                        if bytes.len() == 32 {
                            let mut arr = [0u8; 32];
                            arr.copy_from_slice(&bytes);
                            return U256::from_be_bytes(arr);
                        }
                    }
                }
            }
            U256::ZERO
        }
        Err(_) => U256::ZERO,
    }
}

/// Try to find the balance storage slot for `token` using `holder` as the
/// reference address (a pool that holds the token).
///
/// Tests both Solidity (`keccak256(holder, slot_idx)`) and Vyper
/// (`keccak256(slot_idx, holder)`) layouts for `slot_idx` in 0..24,
/// verifying that the stored value matches `balanceOf(holder)`.
async fn find_balance_slot(
    token: Address,
    holder: Address,
    client: &reqwest::Client,
    rpc_url: &str,
) -> Option<(u64, SlotLayout)> {
    let actual_balance = rpc_balance_of(token, holder, client, rpc_url).await;
    if actual_balance.is_zero() {
        return None;
    }

    for slot_idx in 0..24u64 {
        // Try Solidity-style first (most common)
        let candidate_sol = mapping_slot(holder, slot_idx);
        let stored = rpc_get_storage_at(token, candidate_sol, client, rpc_url).await;
        if stored == actual_balance {
            return Some((slot_idx, SlotLayout::Solidity));
        }
        // Try Vyper-style (reversed argument order)
        let candidate_vyp = mapping_slot_vyper(holder, slot_idx);
        let stored = rpc_get_storage_at(token, candidate_vyp, client, rpc_url).await;
        if stored == actual_balance {
            return Some((slot_idx, SlotLayout::Vyper));
        }
    }
    None
}

/// Build a map of token address → ERC20 balance storage slot for all unique
/// tokens across `pools`.
///
/// Uses concurrent eth_call + eth_getStorageAt to detect slots without
/// requiring a local node.  Tokens for which the slot can't be determined
/// are excluded (those pools are skipped in the swap filter step).
pub async fn construct_slot_map(pools: &[RawPool], rpc_url: &str) -> HashMap<Address, (u64, SlotLayout)> {
    use tokio::task::JoinSet;
    use std::sync::Arc;

    // Collect unique tokens and pick a reference holder (the first pool holding each).
    let mut token_holders: HashMap<Address, Address> = HashMap::new();
    for pool in pools {
        token_holders.entry(pool.token0).or_insert(pool.address);
        token_holders.entry(pool.token1).or_insert(pool.address);
    }
    let total = token_holders.len();

    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(100)
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap_or_default();

    // Limit concurrent RPC tasks to avoid overwhelming the SSH-tunnelled node.
    // Each find_balance_slot makes 1 eth_call + up to 48 eth_getStorageAt calls.
    let semaphore = Arc::new(tokio::sync::Semaphore::new(100));

    let mut set: JoinSet<(Address, Option<(u64, SlotLayout)>)> = JoinSet::new();
    for (token, holder) in token_holders {
        let c = client.clone();
        let url = rpc_url.to_string();
        let sem = semaphore.clone();
        set.spawn(async move {
            let _permit = sem.acquire().await.unwrap();
            let slot = find_balance_slot(token, holder, &c, &url).await;
            (token, slot)
        });
    }

    let mut slot_map: HashMap<Address, (u64, SlotLayout)> = HashMap::new();
    let mut sol_count = 0usize;
    let mut vyp_count = 0usize;
    while let Some(Ok((token, maybe_slot))) = set.join_next().await {
        if let Some((idx, layout)) = maybe_slot {
            match layout {
                SlotLayout::Solidity => sol_count += 1,
                SlotLayout::Vyper => vyp_count += 1,
            }
            slot_map.insert(token, (idx, layout));
        }
    }

    log::info!(
        "[SlotMap] Detected balance slots for {}/{} unique tokens (solidity={}, vyper={})",
        slot_map.len(),
        total,
        sol_count,
        vyp_count,
    );
    slot_map
}

/// Run a full REVM swap filter over `pools`, returning only pools that pass a
/// roundtrip swap test (A→B then B→A ≥ 95% recovery).
///
/// Exotic protocols (Balancer, Curve, Maverick) are always passed through —
/// they are verified by the REVM quoter during live evaluation.
///
/// A single `BlockStateDB` is shared across all pool tests so that lazily
/// fetched contract state (router code, token code, pool storage) accumulates
/// in its cache, making each subsequent test cheaper.
/// Reasons that indicate the pool was untestable (not actually bad).
/// These pools are passed through — benefit of the doubt.
const UNTESTABLE_REASONS: &[&str] = &[
    "missing_slot_token0",
    "missing_slot_token1",
    "missing_router_config",
];

pub fn filter_pools_by_swap(
    pools: Vec<RawPool>,
    slot_map: &HashMap<Address, (u64, SlotLayout)>,
    rpc_url: &str,
    router_registry: &RouterRegistry,
) -> Vec<RawPool> {
    // ── Quick pre-filter: skip pools missing slot mappings (saves ~30-50% work) ──
    let (testable_pools, untestable_pools): (Vec<_>, Vec<_>) = pools.into_iter().partition(|pool| {
        // Skip pools without slot mappings - they'll be passed through
        let has_slot0 = slot_map.contains_key(&pool.token0);
        let has_slot1 = slot_map.contains_key(&pool.token1);
        // Skip exotic protocols that auto-pass
        let needs_test = protocol_router_key(pool.protocol).is_some();
        (has_slot0 && has_slot1) || !needs_test
    });

    log::info!(
        "[SwapFilter] Pre-filter: {} testable, {} untestable (will pass through)",
        testable_pools.len(),
        untestable_pools.len()
    );

    // Use block 0 as a sentinel meaning "latest".
    // BlockStateDB encodes 0 as "latest" in all RPC calls.
    let mut db = BlockStateDB::new(rpc_url.to_string(), 0);

    // Give FILTER_ACCOUNT a large ETH balance for gas.
    db.inject_account(
        FILTER_ACCOUNT,
        AccountInfo {
            balance: U256::from(1_000_000_000_000_000_000_000u128), // 1000 ETH
            nonce: 0,
            code_hash: KECCAK_EMPTY,
            code: None,
        },
    );

    // ── Pre-fetch all needed bytecodes in parallel (pools + tokens + routers) ──
    // Without this, each REVM simulation lazily fetches bytecodes one-by-one,
    // making the filter take 30-60+ minutes. With batch prefetch: ~2-3 minutes.
    {
        use std::collections::HashSet;
        let mut addrs_to_fetch: HashSet<Address> = HashSet::new();
        for pool in &testable_pools {
            addrs_to_fetch.insert(pool.address);
            addrs_to_fetch.insert(pool.token0);
            addrs_to_fetch.insert(pool.token1);
        }
        // Also fetch router addresses used in tests
        for router_addr in router_registry.map.values() {
            addrs_to_fetch.insert(*router_addr);
        }
        let addrs: Vec<Address> = addrs_to_fetch.into_iter().collect();
        log::info!("[SwapFilter] Pre-fetching {} unique contract bytecodes...", addrs.len());
        for addr in &addrs {
            db.track_pool(*addr);
        }
        db.prefetch_pool_codes();
        // Also prefetch critical storage slots so REVM sims don't need lazy RPC.
        // Slot 8 = V2 packed reserves, Slot 0 = V3 slot0, Slot 4 = V3 liquidity.
        // Slot 6 = token0, Slot 7 = token1 (needed by router calls inside swap).
        db.prefetch_token_slots(); // slots 6 & 7
        db.prefetch_v3_slots();    // slots 0 & 4
        // V2 reserves: batch-fetch slot 8 for all pool addresses
        {
            let pool_addrs: Vec<Address> = testable_pools.iter().map(|p| p.address).collect();
            log::info!("[SwapFilter] Pre-fetching slot 8 (V2 reserves) for {} pools...", pool_addrs.len());
            let url = db.rpc_url.clone();
            let client = db.client.clone();
            let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(64));
            let results: Vec<(Address, U256)> = tokio::task::block_in_place(|| {
                let rt = tokio::runtime::Handle::current();
                rt.block_on(async {
                    use futures::stream::{FuturesUnordered, StreamExt};
                    let mut futs = FuturesUnordered::new();
                    for addr in pool_addrs {
                        let url = url.clone();
                        let client = client.clone();
                        let sem = sem.clone();
                        futs.push(async move {
                            let _permit = sem.acquire().await.unwrap();
                            let addr_hex = format!("0x{}", hex::encode(addr.as_slice()));
                            let slot_hex = "0x0000000000000000000000000000000000000000000000000000000000000008";
                            let body = serde_json::json!({
                                "jsonrpc": "2.0",
                                "method": "eth_getStorageAt",
                                "params": [&addr_hex, slot_hex, "latest"],
                                "id": 1
                            });
                            let value = match client.post(&url).json(&body).send().await {
                                Ok(resp) => {
                                    if let Ok(json) = resp.json::<serde_json::Value>().await {
                                        if let Some(s) = json.get("result").and_then(|v| v.as_str()) {
                                            let s = s.trim_start_matches("0x");
                                            let padded = format!("{:0>64}", s);
                                            if let Ok(bytes) = hex::decode(&padded) {
                                                let mut arr = [0u8; 32];
                                                arr.copy_from_slice(&bytes[..32]);
                                                U256::from_be_bytes(arr)
                                            } else { U256::ZERO }
                                        } else { U256::ZERO }
                                    } else { U256::ZERO }
                                }
                                Err(_) => U256::ZERO,
                            };
                            (addr, value)
                        });
                    }
                    let mut results = Vec::new();
                    while let Some(result) = futs.next().await {
                        results.push(result);
                    }
                    results
                })
            });
            let mut stored = 0usize;
            for (addr, value) in results {
                if !value.is_zero() {
                    db.update_slot(addr, crate::state_db::V2_RESERVES_SLOT, value);
                    stored += 1;
                }
            }
            log::info!("[SwapFilter] Pre-fetched slot 8: {} non-zero reserves stored", stored);
        }
        log::info!("[SwapFilter] All prefetch complete, testing pools...");
    }

    let before = testable_pools.len() + untestable_pools.len();
    let testable_count = testable_pools.len();
    let t0 = std::time::Instant::now();

    // ── Sequential pool testing (parallel with Mutex causes contention) ──
    log::info!(
        "[SwapFilter] Starting pool test: {} testable pools, {} untestable (pass-through)",
        testable_count, untestable_pools.len()
    );

    let mut filtered = Vec::with_capacity(before);
    let mut fail_reasons: HashMap<&'static str, usize> = HashMap::new();
    let mut untestable_count = 0usize;
    let mut toxic_count = 0usize;
    let mut tested = 0usize;

    for pool in testable_pools {
        tested += 1;
        if tested % 500 == 0 || tested == testable_count {
            log::info!(
                "[SwapFilter] Progress: {}/{} pools tested ({} passed, {} toxic) in {:.1}s",
                tested, testable_count, filtered.len(), toxic_count, t0.elapsed().as_secs_f32()
            );
        }
        match test_pool_filter_reason(&mut db, &pool, slot_map, router_registry) {
            Ok(()) => filtered.push(pool),
            Err(reason) => {
                *fail_reasons.entry(reason).or_insert(0) += 1;
                if UNTESTABLE_REASONS.contains(&reason) {
                    untestable_count += 1;
                    filtered.push(pool);
                } else {
                    toxic_count += 1;
                    log::debug!(
                        "[SwapFilter] DROPPED pool {} ({:?}) reason={}",
                        pool.address,
                        pool.protocol,
                        reason,
                    );
                }
            }
        }
    }

    // Add untestable pools (they pass through)
    filtered.extend(untestable_pools);

    log::info!(
        "[SwapFilter] {} → {} pools ({} passed swap test, {} untestable pass-through, {} toxic dropped) in {:.1}s",
        before,
        filtered.len(),
        filtered.len() - untestable_count,
        untestable_count,
        toxic_count,
        t0.elapsed().as_secs_f32(),
    );
    if !fail_reasons.is_empty() {
        log::info!("[SwapFilter] failure reasons: {:?}", fail_reasons);
    }
    filtered
}

/// Full startup filter pipeline: optional GeckoTerminal token filter + slot-map detection +
/// REVM swap filter.
///
/// Call this from `main.rs` at startup before building arb paths.
/// Startup hard-fails only when no REVM router mappings are configured at all.
pub async fn filter_pools_full(
    mut pools: Vec<RawPool>,
    rpc_url: &str,
    router_registry: &RouterRegistry,
    _api_key: Option<&str>,   // unused — GeckoTerminal needs no key
    token_limit: usize,
    blacklisted_tokens: &HashSet<Address>,
) -> anyhow::Result<Vec<RawPool>> {
    // ── Stage 0: static token blacklist ───────────────────────────────────
    // Already applied in main.rs before this call, but belt-and-suspenders.
    if !blacklisted_tokens.is_empty() {
        pools.retain(|p| {
            !blacklisted_tokens.contains(&p.token0) && !blacklisted_tokens.contains(&p.token1)
        });
    }
    log::info!("[FilterFull] {} pools after blacklist", pools.len());

    // ── Stage 1: GeckoTerminal top-token filter (optional) ────────────────
    if token_limit > 0 {
        let chain = std::env::var("BOT_CHAIN")
            .unwrap_or_else(|_| "base".to_string());
        match fetch_top_tokens("", &chain, token_limit).await {
            Ok(tokens) => {
                let allowed = gecko_to_address_set(&tokens);
                if !allowed.is_empty() {
                    let before = pools.len();
                    pools.retain(|p| allowed.contains(&p.token0) && allowed.contains(&p.token1));
                    log::info!(
                        "[FilterFull] GeckoTerminal: {} -> {} pools ({} removed)",
                        before,
                        pools.len(),
                        before.saturating_sub(pools.len())
                    );
                }
            }
            Err(e) => {
                log::warn!("[FilterFull] GeckoTerminal filter skipped: {e}");
            }
        }
    } else {
        log::info!("[FilterFull] GeckoTerminal stage disabled");
    }

    // ── Stage 2: construct balance slot map ───────────────────────────────
    let slot_map = construct_slot_map(&pools, rpc_url).await;

    // Log slot coverage. Require at least 50% slot detection before
    // trusting the REVM filter results — below that, too many untestable
    // pools pass through as false-negatives.
    let token_count = pools
        .iter()
        .flat_map(|p| [p.token0, p.token1])
        .collect::<HashSet<_>>()
        .len();
    let coverage = if token_count == 0 {
        0.0
    } else {
        slot_map.len() as f64 / token_count as f64
    };
    log::info!(
        "[FilterFull] Slot-map coverage: {}/{} tokens ({:.1}%)",
        slot_map.len(),
        token_count,
        coverage * 100.0
    );
    if coverage < 0.50 {
        log::warn!(
            "[FilterFull] Slot-map coverage below 50% ({:.1}%). REVM swap filter may be unreliable — running anyway but many pools will be untestable.",
            coverage * 100.0
        );
    }

    // ── Stage 3: REVM swap roundtrip filter ───────────────────────────────
    if router_registry.is_empty() {
        return Err(anyhow!(
            "[FilterFull] No [revm_filter.routers] entries configured. Add at least one router mapping and restart."
        ));
    }

    let required_router_protocols: HashSet<PoolProtocol> = pools
        .iter()
        .map(|p| p.protocol)
        .filter(|p| protocol_router_key(*p).is_some())
        .collect();

    let configured_router_protocols: HashSet<PoolProtocol> = required_router_protocols
        .iter()
        .copied()
        .filter(|p| router_registry.contains_key(p))
        .collect();
    let missing_router_protocols: HashSet<PoolProtocol> = required_router_protocols
        .iter()
        .copied()
        .filter(|p| !router_registry.contains_key(p))
        .collect();

    log::info!(
        "[FilterFull] REVM router coverage: configured={:?} missing={:?}",
        configured_router_protocols,
        missing_router_protocols
    );

    if !required_router_protocols.is_empty() && configured_router_protocols.is_empty() {
        return Err(anyhow!(
            "[FilterFull] No router mappings match pool protocols {:?}. Add at least one matching [revm_filter.routers] entry and restart.",
            required_router_protocols
        ));
    }

    let before = pools.len();
    let filtered = filter_pools_by_swap(pools, &slot_map, rpc_url, router_registry);

    // No more fail-open: untestable pools already pass through in
    // filter_pools_by_swap, so the filtered set should never be empty
    // unless every single testable pool is toxic.
    if filtered.is_empty() {
        log::warn!(
            "[FilterFull] REVM swap filter returned 0 pools from {} input. This likely means all testable pools are toxic.",
            before
        );
    }

    pools = filtered;
    log::info!(
        "[FilterFull] REVM swap filter: {} → {} pools",
        before,
        pools.len()
    );

    Ok(pools)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::swap_types::{PoolProtocol, SwapStep};

    fn addr(n: u64) -> Address {
        let mut b = [0u8; 20];
        b[12..20].copy_from_slice(&n.to_be_bytes());
        Address::from(b)
    }

    fn mk_path(pool: Address, token_in: Address, token_out: Address) -> SwapPath {
        SwapPath::new(vec![SwapStep {
            pool_address: pool,
            token_in,
            token_out,
            protocol: PoolProtocol::UniswapV2,
            fee: 30,
        }])
    }

    #[test]
    fn combined_filter_respects_allowed_tokens_and_safe_pools() {
        let p1 = addr(1);
        let p2 = addr(2);
        let t1 = addr(11);
        let t2 = addr(12);
        let t3 = addr(13);

        let keep = mk_path(p1, t1, t2);
        let drop_pool = mk_path(p2, t1, t2);
        let drop_token = mk_path(p1, t1, t3);

        let mut filter = CombinedFilter::new(500);
        filter.add_safe_pool(p1);
        filter.set_allowed_tokens(HashSet::from([t1, t2]));

        let out = filter_paths(vec![keep.clone(), drop_pool, drop_token], &filter);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].hash, keep.hash);
    }
}
