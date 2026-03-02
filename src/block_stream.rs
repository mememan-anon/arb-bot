/// Alloy WS block stream for the arb-bot pipeline.
///
/// Subscribes to new blocks via alloy's WebSocket provider(s) and pushes
/// `PipelineEvent::NewBlock` into the pipeline broadcast channel.
///
/// **Gossip mode** (`stream_blocks_gossip`):
///   Connect to multiple WS endpoints simultaneously. The first endpoint to
///   report a given block number wins — duplicates are suppressed. Each
///   endpoint auto-reconnects with exponential backoff on failure.
///
/// **Single mode** (`stream_new_blocks`):
///   Simple single-endpoint subscription (kept for backward compat).

use alloy::providers::{Provider, ProviderBuilder};
use alloy::transports::ws::WsConnect;
use futures::StreamExt;
use log::{debug, error, info, warn};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex};

use crate::pipeline_events::PipelineEvent;

/// Maximum number of recent block numbers to remember for dedup.
/// 64 blocks ≈ 3 minutes on BSC (3 s blocks). Keeps memory bounded.
const DEDUP_WINDOW: usize = 64;

/// Rolling dedup set shared across all gossip streams.
struct BlockDedup {
    seen: HashSet<u64>,
    /// Ordered ring so we can evict the oldest entry when full.
    ring: Vec<u64>,
    head: usize,
}

impl BlockDedup {
    fn new() -> Self {
        Self {
            seen: HashSet::with_capacity(DEDUP_WINDOW),
            ring: vec![0u64; DEDUP_WINDOW],
            head: 0,
        }
    }

    /// Returns `true` if this block_number is new (first time seen).
    fn try_insert(&mut self, block_number: u64) -> bool {
        if !self.seen.insert(block_number) {
            return false; // already seen
        }
        // Evict the oldest entry from the ring
        let evicted = self.ring[self.head];
        if evicted != 0 {
            self.seen.remove(&evicted);
        }
        self.ring[self.head] = block_number;
        self.head = (self.head + 1) % DEDUP_WINDOW;
        true
    }
}

/// Subscribe to new blocks and push `PipelineEvent::NewBlock` to the pipeline.
///
/// Connects to `ws_url`, subscribes to new block headers, and sends a
/// `PipelineEvent::NewBlock` for every block received. Exits cleanly when
/// the broadcast channel is closed or the WS connection drops.
///
/// In production, call this inside a reconnect loop (backoff + retry).
pub async fn stream_new_blocks(ws_url: String, block_tx: broadcast::Sender<PipelineEvent>) {
    info!("[BlockStream] Connecting to {ws_url}");

    // Build an alloy WS provider
    let provider = match ProviderBuilder::new()
        .on_ws(WsConnect::new(ws_url.clone()))
        .await
    {
        Ok(p) => p,
        Err(e) => {
            error!("[BlockStream] WS connect failed for {ws_url}: {e}");
            return;
        }
    };

    // Subscribe to new block headers
    let subscription = match provider.subscribe_blocks().await {
        Ok(s) => s,
        Err(e) => {
            error!("[BlockStream] subscribe_blocks failed: {e}");
            return;
        }
    };

    info!("[BlockStream] Subscribed — streaming blocks");

    let mut stream = subscription.into_stream();

    while let Some(header) = stream.next().await {
        // Extract fields — use safe casts for wide types (u128 → u64)
        let block_number = header.number;
        let base_fee = header.base_fee_per_gas.unwrap_or(0) as u64;
        let gas_used = header.gas_used as u64;
        let gas_limit = header.gas_limit as u64;
        let timestamp = header.timestamp;

        let event = PipelineEvent::NewBlock {
            block_number,
            base_fee,
            gas_used,
            gas_limit,
            timestamp,
        };

        match block_tx.send(event) {
            Ok(receivers) => {
                debug!("[BlockStream] Block {block_number} sent to {receivers} receivers");
            }
            Err(_) => {
                debug!("[BlockStream] No receivers — channel closed, shutting down");
                return;
            }
        }
    }

    warn!("[BlockStream] Block stream ended unexpectedly");
}

// ── Gossip multiplexer ──────────────────────────────────────────────────────

/// Connect to **all** WS endpoints in `urls` simultaneously and forward
/// deduplicated blocks into `block_tx`. Each endpoint auto-reconnects with
/// exponential backoff. The first endpoint to deliver a given block number wins;
/// duplicates from other endpoints are silently dropped.
///
/// Spawns one async task per endpoint, all sharing the same dedup state.
///
/// ```ignore
/// tokio::spawn(stream_blocks_gossip(all_ws_urls, pipeline.block_tx.clone()));
/// ```
pub async fn stream_blocks_gossip(
    urls: Vec<String>,
    block_tx: broadcast::Sender<PipelineEvent>,
) {
    if urls.is_empty() {
        warn!("[Gossip] No WS URLs configured — block stream disabled");
        return;
    }

    info!("[Gossip] Starting multi-endpoint block gossip with {} URL(s)", urls.len());

    let dedup = Arc::new(Mutex::new(BlockDedup::new()));
    let mut set = tokio::task::JoinSet::new();

    for (idx, url) in urls.into_iter().enumerate() {
        let tx = block_tx.clone();
        let dedup = Arc::clone(&dedup);
        set.spawn(gossip_endpoint(idx, url, tx, dedup));
    }

    // Keep running as long as at least one endpoint task is alive.
    while let Some(res) = set.join_next().await {
        if let Err(e) = res {
            error!("[Gossip] Endpoint task panicked: {e:?}");
        }
    }

    warn!("[Gossip] All WS endpoint tasks exited — block stream stopped");
}

/// Single endpoint loop with auto-reconnect and exponential backoff.
async fn gossip_endpoint(
    idx: usize,
    url: String,
    block_tx: broadcast::Sender<PipelineEvent>,
    dedup: Arc<Mutex<BlockDedup>>,
) {
    let tag = format!("ws-{idx}");
    let mut backoff_ms: u64 = 500; // start at 0.5 s
    const MAX_BACKOFF_MS: u64 = 30_000; // cap at 30 s

    loop {
        info!("[Gossip][{tag}] Connecting to {url}");
        let connected_at = std::time::Instant::now();

        let provider = match ProviderBuilder::new()
            .on_ws(WsConnect::new(url.clone()))
            .await
        {
            Ok(p) => p,
            Err(e) => {
                warn!("[Gossip][{tag}] Connect failed: {e} — retrying in {backoff_ms}ms");
                tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                backoff_ms = (backoff_ms * 2).min(MAX_BACKOFF_MS);
                continue;
            }
        };

        let subscription = match provider.subscribe_blocks().await {
            Ok(s) => s,
            Err(e) => {
                warn!("[Gossip][{tag}] subscribe_blocks failed: {e} — retrying in {backoff_ms}ms");
                tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                backoff_ms = (backoff_ms * 2).min(MAX_BACKOFF_MS);
                continue;
            }
        };

        info!("[Gossip][{tag}] Subscribed — streaming blocks");
        backoff_ms = 500; // reset on successful connect
        let mut stream = subscription.into_stream();

        while let Some(header) = stream.next().await {
            let block_number = header.number;
            let base_fee = header.base_fee_per_gas.unwrap_or(0) as u64;
            let gas_used = header.gas_used as u64;
            let gas_limit = header.gas_limit as u64;
            let timestamp = header.timestamp;
            let recv_at = std::time::Instant::now();

            // Dedup: only the first endpoint to see this block sends it downstream.
            let is_first = {
                let mut dd = dedup.lock().await;
                dd.try_insert(block_number)
            };

            if !is_first {
                let latency = recv_at.duration_since(connected_at);
                debug!(
                    "[Gossip][{tag}] Block {block_number} duplicate (arrived {:.1?} after connect)",
                    latency
                );
                continue;
            }

            let event = PipelineEvent::NewBlock {
                block_number,
                base_fee,
                gas_used,
                gas_limit,
                timestamp,
            };

            match block_tx.send(event) {
                Ok(receivers) => {
                    debug!(
                        "[Gossip][{tag}] Block {block_number} FIRST → sent to {receivers} receivers",
                    );
                }
                Err(_) => {
                    debug!("[Gossip][{tag}] Channel closed — shutting down");
                    return; // pipeline is dead, exit permanently
                }
            }
        }

        warn!("[Gossip][{tag}] Stream ended — reconnecting in {backoff_ms}ms");
        tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
        backoff_ms = (backoff_ms * 2).min(MAX_BACKOFF_MS);
    }
}
