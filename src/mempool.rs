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

        // Decode V2 swap calldata to extract token path
        let calldata = tx.input().as_ref();
        if let Some(token_path) = decode_v2_swap_path(calldata) {
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
