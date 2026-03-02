/// Mempool listener — subscribes to BSC pending transactions and identifies
/// DEX swap transactions to trigger the searcher.
///
/// V1 implementation:
/// - Subscribes to full pending transactions via WS
/// - Filters by `to` address matching known DEX router addresses
/// - Decodes V2 swap calldata to extract token paths
/// - Maps token pairs to known pool addresses
/// - Sends PoolsTouched events (with block_number=0) to the searcher
///
/// The searcher treats mempool PoolsTouched identically to block-triggered events,
/// using `latest_known_block + 1` as the arb block number.

use alloy::consensus::Transaction as AlloyCTx;
use alloy::primitives::Address;
use alloy::providers::{Provider, ProviderBuilder};
use alloy::transports::ws::WsConnect;
use futures::StreamExt;
use log::{debug, error, info, warn};
use std::collections::{HashMap, HashSet};
use tokio::sync::mpsc;

use crate::pipeline_events::PoolsTouched;

/// Configuration for the mempool listener.
pub struct MempoolConfig {
    /// WS URL to subscribe to pending transactions.
    pub ws_url: String,
    /// Known DEX router addresses to filter pending transactions by.
    pub router_addresses: HashSet<Address>,
    /// Map (token0, token1) → vec of pool addresses for looking up affected pools.
    /// token0 < token1 (canonical ordering).
    pub token_pair_to_pool: HashMap<(Address, Address), Vec<Address>>,
}

// ── V2 swap function selectors ──────────────────────────────────────────────
const SWAP_EXACT_TOKENS_FOR_TOKENS: [u8; 4] = [0x38, 0xed, 0x17, 0x39];
const SWAP_TOKENS_FOR_EXACT_TOKENS: [u8; 4] = [0x88, 0x03, 0xdb, 0xee];
const SWAP_EXACT_ETH_FOR_TOKENS: [u8; 4] = [0x7f, 0xf3, 0x6a, 0xb5];
const SWAP_EXACT_TOKENS_FOR_ETH: [u8; 4] = [0x18, 0xcb, 0xaf, 0xe5];
// Fee-on-transfer variants
const SWAP_EXACT_TOKENS_FOR_TOKENS_FEE: [u8; 4] = [0x5c, 0x11, 0xd7, 0x95];
const SWAP_EXACT_TOKENS_FOR_ETH_FEE: [u8; 4] = [0x79, 0x1a, 0xc9, 0x47];

// ── V3 swap function selectors ──────────────────────────────────────────────
// exactInputSingle((address,address,uint24,address,uint256,uint256,uint160))
const EXACT_INPUT_SINGLE: [u8; 4] = [0x41, 0x4b, 0xf3, 0x89];
// exactInput((bytes,address,uint256,uint256,uint256))
const EXACT_INPUT: [u8; 4] = [0xc0, 0x4b, 0x8d, 0x59];
// exactOutputSingle((address,address,uint24,address,uint256,uint256,uint160))
const EXACT_OUTPUT_SINGLE: [u8; 4] = [0xdb, 0x3e, 0x21, 0x98];
// exactOutput((bytes,address,uint256,uint256,uint256))
const EXACT_OUTPUT: [u8; 4] = [0xf2, 0x8c, 0x04, 0x98];
// PancakeSwap V3 SmartRouter: exactInputSingle variant
// exactInputSingleV3((address,address,uint24,address,uint256,uint256,uint160))
const EXACT_INPUT_SINGLE_V3_ALT: [u8; 4] = [0x04, 0xe4, 0x5a, 0xaf];
// multicall(uint256,bytes[])
const MULTICALL_DEADLINE: [u8; 4] = [0x5a, 0xe4, 0x01, 0xdc];
// multicall(bytes[])
const MULTICALL_NO_DEADLINE: [u8; 4] = [0xac, 0x96, 0x50, 0xd8];

/// Extract token addresses from V2 swap calldata.
///
/// Returns the token path (e.g. `[WBNB, USDT, BUSD]`) or `None` if the calldata
/// is not a recognized V2 swap function.
fn decode_v2_swap_path(calldata: &[u8]) -> Option<Vec<Address>> {
    if calldata.len() < 4 {
        return None;
    }
    let selector: [u8; 4] = calldata[..4].try_into().ok()?;

    // The `path` dynamic array's ABI-encoding offset (in 32-byte words from param start)
    // varies by function:
    //   swapExactTokensForTokens:   word 2 (amountIn, amountOutMin, [path], to, deadline)
    //   swapTokensForExactTokens:   word 2 (amountOut, amountInMax, [path], to, deadline)
    //   swapExactETHForTokens:      word 1 (amountOutMin, [path], to, deadline)  — no amountIn
    //   swapExactTokensForETH:      word 2 (amountIn, amountOutMin, [path], to, deadline)
    //   fee-on-transfer variants:   same as their non-fee counterparts
    let path_word_idx = match selector {
        SWAP_EXACT_TOKENS_FOR_TOKENS => 2,
        SWAP_TOKENS_FOR_EXACT_TOKENS => 2,
        SWAP_EXACT_ETH_FOR_TOKENS => 1,
        SWAP_EXACT_TOKENS_FOR_ETH => 2,
        SWAP_EXACT_TOKENS_FOR_TOKENS_FEE => 2,
        SWAP_EXACT_TOKENS_FOR_ETH_FEE => 2,
        _ => return None,
    };

    let offset_start = 4 + path_word_idx * 32;
    if calldata.len() < offset_start + 32 {
        return None;
    }

    // Read the ABI offset (uint256) to the dynamic array
    let offset_bytes: [u8; 32] = calldata[offset_start..offset_start + 32].try_into().ok()?;
    let offset = u64::from_be_bytes(offset_bytes[24..32].try_into().ok()?) as usize;

    let array_start = 4 + offset; // offset is relative to start of params (after selector)
    if calldata.len() < array_start + 32 {
        return None;
    }

    // Read array length
    let len_bytes: [u8; 32] = calldata[array_start..array_start + 32].try_into().ok()?;
    let path_len = u64::from_be_bytes(len_bytes[24..32].try_into().ok()?) as usize;

    if path_len < 2 || path_len > 10 {
        return None; // sanity
    }

    let data_start = array_start + 32;
    if calldata.len() < data_start + path_len * 32 {
        return None;
    }

    let mut addrs = Vec::with_capacity(path_len);
    for i in 0..path_len {
        let addr_start = data_start + i * 32 + 12; // address is right-aligned in 32-byte word
        if calldata.len() < addr_start + 20 {
            return None;
        }
        let addr = Address::from_slice(&calldata[addr_start..addr_start + 20]);
        addrs.push(addr);
    }

    Some(addrs)
}

/// Extract token pair from V3 `exactInputSingle` / `exactOutputSingle` calldata.
///
/// ABI: (address tokenIn, address tokenOut, uint24 fee, address recipient, ...)
/// The first two 32-byte words after the selector contain tokenIn and tokenOut.
fn decode_v3_single_swap(calldata: &[u8]) -> Option<Vec<Address>> {
    // selector(4) + tokenIn(32) + tokenOut(32) = 68 bytes minimum
    if calldata.len() < 68 {
        return None;
    }
    let token_in = Address::from_slice(&calldata[4 + 12..4 + 32]);
    let token_out = Address::from_slice(&calldata[4 + 32 + 12..4 + 64]);
    Some(vec![token_in, token_out])
}

/// Extract token pairs from V3 `exactInput` / `exactOutput` calldata.
///
/// The `path` is a packed encoding of (address, uint24, address, uint24, ..., address).
/// Each address is 20 bytes, each fee is 3 bytes. So each hop is 23 bytes, and the
/// total path length = 20 + num_hops * 23.
///
/// For `exactInput`: ABI is (bytes path, address recipient, uint256 deadline,
///                           uint256 amountIn, uint256 amountOutMinimum)
/// The `path` bytes are at a dynamic offset in the first tuple element.
fn decode_v3_multi_swap(calldata: &[u8]) -> Option<Vec<Address>> {
    // The struct is ABI-encoded as a tuple:
    //   word 0: offset to path bytes (dynamic)
    //   word 1: recipient
    //   word 2: deadline (or amountIn depending on variant)
    //   ...
    // At the offset: length(32) + path_data
    if calldata.len() < 4 + 32 {
        return None;
    }

    // Read offset to the path bytes
    let offset_bytes: [u8; 32] = calldata[4..4 + 32].try_into().ok()?;
    let offset = u64::from_be_bytes(offset_bytes[24..32].try_into().ok()?) as usize;

    let path_start = 4 + offset;
    if calldata.len() < path_start + 32 {
        return None;
    }

    // Read path length
    let len_bytes: [u8; 32] = calldata[path_start..path_start + 32].try_into().ok()?;
    let path_len = u64::from_be_bytes(len_bytes[24..32].try_into().ok()?) as usize;

    let data_start = path_start + 32;
    if calldata.len() < data_start + path_len {
        return None;
    }
    let path_data = &calldata[data_start..data_start + path_len];

    decode_v3_path_bytes(path_data)
}

/// Decode packed V3 path bytes: (addr20, fee3, addr20, fee3, ..., addr20).
fn decode_v3_path_bytes(path: &[u8]) -> Option<Vec<Address>> {
    // Minimum path: 2 addresses + 1 fee = 20 + 3 + 20 = 43 bytes
    if path.len() < 43 {
        return None;
    }

    let mut addrs = Vec::new();
    let mut pos = 0;

    // First address
    if pos + 20 > path.len() {
        return None;
    }
    addrs.push(Address::from_slice(&path[pos..pos + 20]));
    pos += 20;

    // Each subsequent hop: 3 bytes fee + 20 bytes address
    while pos + 23 <= path.len() {
        pos += 3; // skip fee tier
        addrs.push(Address::from_slice(&path[pos..pos + 20]));
        pos += 20;
    }

    if addrs.len() >= 2 {
        Some(addrs)
    } else {
        None
    }
}

/// Try to extract token addresses from multicall calldata.
///
/// multicall wraps multiple sub-calls. We decode each sub-call and collect
/// all token addresses from V3 swap sub-calls.
fn decode_multicall_swaps(calldata: &[u8]) -> Option<Vec<Address>> {
    if calldata.len() < 4 {
        return None;
    }
    let selector: [u8; 4] = calldata[..4].try_into().ok()?;

    // Determine where the bytes[] array offset is
    let array_offset_word = match selector {
        MULTICALL_DEADLINE => 1,    // multicall(uint256 deadline, bytes[] data) → skip 1 word
        MULTICALL_NO_DEADLINE => 0, // multicall(bytes[] data) → first word is offset
        _ => return None,
    };

    let off_start = 4 + array_offset_word * 32;
    if calldata.len() < off_start + 32 {
        return None;
    }

    // Read the offset to the bytes[] array
    let offset_bytes: [u8; 32] = calldata[off_start..off_start + 32].try_into().ok()?;
    let offset = u64::from_be_bytes(offset_bytes[24..32].try_into().ok()?) as usize;

    let arr_start = 4 + offset;
    if calldata.len() < arr_start + 32 {
        return None;
    }

    // Read array length
    let len_bytes: [u8; 32] = calldata[arr_start..arr_start + 32].try_into().ok()?;
    let num_calls = u64::from_be_bytes(len_bytes[24..32].try_into().ok()?) as usize;

    if num_calls == 0 || num_calls > 20 {
        return None; // sanity
    }

    let mut all_tokens = Vec::new();

    // Read each sub-call's offset and decode it
    for i in 0..num_calls {
        let elem_off_pos = arr_start + 32 + i * 32;
        if calldata.len() < elem_off_pos + 32 {
            break;
        }
        let elem_off_bytes: [u8; 32] = calldata[elem_off_pos..elem_off_pos + 32]
            .try_into()
            .ok()?;
        let elem_offset =
            u64::from_be_bytes(elem_off_bytes[24..32].try_into().ok()?) as usize;

        let elem_start = arr_start + 32 + elem_offset;
        if calldata.len() < elem_start + 32 {
            break;
        }

        // Read sub-call data length
        let sub_len_bytes: [u8; 32] = calldata[elem_start..elem_start + 32]
            .try_into()
            .ok()?;
        let sub_len =
            u64::from_be_bytes(sub_len_bytes[24..32].try_into().ok()?) as usize;

        let sub_data_start = elem_start + 32;
        if calldata.len() < sub_data_start + sub_len || sub_len < 4 {
            continue;
        }
        let sub_call = &calldata[sub_data_start..sub_data_start + sub_len];

        // Try to decode the sub-call as a V3 swap
        let sub_sel: [u8; 4] = sub_call[..4].try_into().ok()?;
        let tokens = match sub_sel {
            EXACT_INPUT_SINGLE | EXACT_OUTPUT_SINGLE | EXACT_INPUT_SINGLE_V3_ALT => {
                decode_v3_single_swap(sub_call)
            }
            EXACT_INPUT | EXACT_OUTPUT => decode_v3_multi_swap(sub_call),
            // Also try V2 selectors inside multicall
            SWAP_EXACT_TOKENS_FOR_TOKENS
            | SWAP_TOKENS_FOR_EXACT_TOKENS
            | SWAP_EXACT_ETH_FOR_TOKENS
            | SWAP_EXACT_TOKENS_FOR_ETH
            | SWAP_EXACT_TOKENS_FOR_TOKENS_FEE
            | SWAP_EXACT_TOKENS_FOR_ETH_FEE => decode_v2_swap_path(sub_call),
            _ => None,
        };
        if let Some(toks) = tokens {
            all_tokens.extend(toks);
        }
    }

    if all_tokens.is_empty() {
        None
    } else {
        Some(all_tokens)
    }
}

/// Try all known swap decoders on calldata. Returns token path if recognized.
fn decode_any_swap(calldata: &[u8]) -> Option<Vec<Address>> {
    if calldata.len() < 4 {
        return None;
    }
    let selector: [u8; 4] = calldata[..4].try_into().ok()?;

    match selector {
        // V2 swaps
        SWAP_EXACT_TOKENS_FOR_TOKENS
        | SWAP_TOKENS_FOR_EXACT_TOKENS
        | SWAP_EXACT_ETH_FOR_TOKENS
        | SWAP_EXACT_TOKENS_FOR_ETH
        | SWAP_EXACT_TOKENS_FOR_TOKENS_FEE
        | SWAP_EXACT_TOKENS_FOR_ETH_FEE => decode_v2_swap_path(calldata),
        // V3 single swaps
        EXACT_INPUT_SINGLE | EXACT_OUTPUT_SINGLE | EXACT_INPUT_SINGLE_V3_ALT => {
            decode_v3_single_swap(calldata)
        }
        // V3 multi-hop swaps
        EXACT_INPUT | EXACT_OUTPUT => decode_v3_multi_swap(calldata),
        // Multicall wrappers (V3 router / SmartRouter)
        MULTICALL_DEADLINE | MULTICALL_NO_DEADLINE => decode_multicall_swaps(calldata),
        _ => None,
    }
}

/// Subscribe to pending transactions and send detected DEX swap pool touches
/// to the searcher pipeline.
///
/// Connects via WebSocket, subscribes to full pending transactions, filters for
/// DEX router calls, decodes V2 swap paths, and maps token pairs to pool addresses.
///
/// The `pools_tx` channel is shared with `MarketStateWorker` — both feed `PoolsTouched`
/// events to the searcher.
pub async fn stream_pending_swaps(
    config: MempoolConfig,
    pools_tx: mpsc::Sender<PoolsTouched>,
) {
    info!("[Mempool] Connecting to {}", config.ws_url);
    info!(
        "[Mempool] Monitoring {} router address(es)",
        config.router_addresses.len()
    );
    info!(
        "[Mempool] Token-pair lookup has {} entries",
        config.token_pair_to_pool.len()
    );

    let provider = match ProviderBuilder::new()
        .on_ws(WsConnect::new(config.ws_url.clone()))
        .await
    {
        Ok(p) => p,
        Err(e) => {
            error!("[Mempool] WS connect failed: {e}");
            return;
        }
    };

    // Try full-object pending tx subscription first (best for decoding calldata).
    let subscription = match provider.subscribe_full_pending_transactions().await {
        Ok(s) => s,
        Err(e) => {
            warn!("[Mempool] Full pending tx subscription failed: {e}");
            warn!("[Mempool] Node may not support full mode — mempool disabled");
            // For BSC, full pending tx support depends on the node operator.
            // If not available, the mempool feature is disabled for this run.
            return;
        }
    };

    info!("[Mempool] Subscribed to full pending transactions");
    let mut stream = subscription.into_stream();

    let mut total_seen: u64 = 0;
    let mut dex_swaps: u64 = 0;
    let mut pools_forwarded: u64 = 0;
    let mut last_stats = std::time::Instant::now();

    while let Some(tx) = stream.next().await {
        total_seen += 1;

        // In alloy 0.6, Transaction wraps an inner TxEnvelope.
        // The Transaction trait provides to() and input() methods.
        let to_addr = match tx.to() {
            Some(addr) => addr,
            None => continue, // contract creation
        };

        if !config.router_addresses.contains(&to_addr) {
            continue;
        }

        dex_swaps += 1;

        // Decode swap calldata (V2, V3, or multicall) to extract token path
        let calldata = tx.input().as_ref();
        if let Some(token_path) = decode_any_swap(calldata) {
            // Map consecutive token pairs to pool addresses
            let mut affected_pools = Vec::new();
            for pair in token_path.windows(2) {
                let (t0, t1) = if pair[0] < pair[1] {
                    (pair[0], pair[1])
                } else {
                    (pair[1], pair[0])
                };
                if let Some(pools) = config.token_pair_to_pool.get(&(t0, t1)) {
                    affected_pools.extend_from_slice(pools);
                }
            }

            if !affected_pools.is_empty() {
                affected_pools.sort();
                affected_pools.dedup();
                pools_forwarded += affected_pools.len() as u64;

                debug!(
                    "[Mempool] Pending swap tx {:?}: {} tokens, {} pools affected",
                    tx.inner.tx_hash(),
                    token_path.len(),
                    affected_pools.len()
                );

                // block_number=0 signals mempool origin.
                // The searcher uses `latest_known_block + 1` when it sees block_number=0.
                let event = PoolsTouched {
                    block_number: 0,
                    touched_pools: affected_pools,
                };
                if pools_tx.send(event).await.is_err() {
                    info!("[Mempool] Pools channel closed, shutting down");
                    return;
                }
            }
        }

        // Periodic stats (every 30s)
        if last_stats.elapsed() > std::time::Duration::from_secs(30) {
            info!(
                "[Mempool] Stats: {} total pending txs, {} DEX swaps, {} pools forwarded",
                total_seen, dex_swaps, pools_forwarded
            );
            last_stats = std::time::Instant::now();
        }
    }

    warn!("[Mempool] Pending tx stream ended");
}

/// Build a token-pair → pool-address lookup table from pool metadata.
///
/// Returns `HashMap<(token0, token1), Vec<Address>>` where token0 < token1.
/// Multiple pools can exist for the same pair (e.g. V2 + V3, or different fee tiers).
pub fn build_token_pair_lookup(
    pool_metas: &[crate::searcher_pipeline::PoolMeta],
) -> HashMap<(Address, Address), Vec<Address>> {
    let mut map: HashMap<(Address, Address), Vec<Address>> = HashMap::new();
    for meta in pool_metas {
        let (t0, t1) = if meta.token0 < meta.token1 {
            (meta.token0, meta.token1)
        } else {
            (meta.token1, meta.token0)
        };
        map.entry((t0, t1)).or_default().push(meta.address);
    }
    // Dedup in case of duplicate pool entries
    for pools in map.values_mut() {
        pools.sort();
        pools.dedup();
    }
    map
}
