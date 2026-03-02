/// Alloy WS block stream for the arb-bot pipeline.
///
/// Replaces the legacy ethers `streams.rs`. Subscribes to new blocks via
/// alloy's WebSocket provider and pushes `PipelineEvent::NewBlock` into the
/// pipeline broadcast channel.
///
/// Usage:
/// ```ignore
/// tokio::spawn(block_stream::stream_new_blocks(ws_url, pipeline.block_tx.clone()));
/// ```

use alloy::providers::{Provider, ProviderBuilder};
use alloy::transports::ws::WsConnect;
use futures::StreamExt;
use log::{debug, error, warn};
use tokio::sync::broadcast;

use crate::pipeline_events::PipelineEvent;

/// Subscribe to new blocks and push `PipelineEvent::NewBlock` to the pipeline.
///
/// Connects to `ws_url`, subscribes to new block headers, and sends a
/// `PipelineEvent::NewBlock` for every block received. Exits cleanly when
/// the broadcast channel is closed or the WS connection drops.
///
/// In production, call this inside a reconnect loop (backoff + retry).
pub async fn stream_new_blocks(ws_url: String, block_tx: broadcast::Sender<PipelineEvent>) {
    log::info!("[BlockStream] Connecting to {ws_url}");

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

    log::info!("[BlockStream] Subscribed — streaming blocks");

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
