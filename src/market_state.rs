/// MarketState worker — traces each new block, updates the BlockStateDB,
/// and sends PoolsTouched events to the searcher.
///
/// Pipeline position: NewBlock → [MarketState] → PoolsTouched
///
/// Uses `debug_traceBlockByNumber` in PreStateTracer diff mode to discover
/// which pool storage slots changed. Only tracked pools are considered.

use alloy::primitives::Address;
use alloy::providers::{Provider, ProviderBuilder};
use std::sync::{Arc, RwLock};
use tokio::sync::{broadcast, mpsc};

use crate::gas_station::GasStation;
use crate::pipeline_events::{PipelineEvent, PoolsTouched};
use crate::state_db::BlockStateDB;
use crate::tracing_revm::trace_block_diffs;

/// Configuration for the MarketState worker.
pub struct MarketStateConfig {
    /// HTTP RPC URL for debug_traceBlockByNumber.
    pub rpc_url: String,
    /// Last block that our state DB was synced to (from cache/startup).
    /// If 0 or unknown, no catch-up is performed.
    pub last_synced_block: u64,
}

/// MarketState worker — owns a reference to the shared BlockStateDB.
pub struct MarketStateWorker {
    pub config: MarketStateConfig,
    pub state_db: Arc<RwLock<BlockStateDB>>,
    pub gas_station: Arc<GasStation>,
}

impl MarketStateWorker {
    pub fn new(
        config: MarketStateConfig,
        state_db: Arc<RwLock<BlockStateDB>>,
        gas_station: Arc<GasStation>,
    ) -> Self {
        Self {
            config,
            state_db,
            gas_station,
        }
    }

    /// Run the market state worker.
    ///
    /// Listens for NewBlock events on the broadcast channel, traces each block,
    /// updates the state DB, and sends PoolsTouched downstream.
    pub async fn run(
        &self,
        mut block_rx: broadcast::Receiver<PipelineEvent>,
        pools_tx: mpsc::Sender<PoolsTouched>,
    ) {
        log::info!("[MarketState] Worker started");

        // ── Block catch-up: replay any blocks we missed since last_synced_block ──
        if self.config.last_synced_block > 0 {
            match ProviderBuilder::new()
                .on_http(self.config.rpc_url.parse().expect("valid rpc url"))
                .get_block_number()
                .await
            {
                Ok(chain_head) => {
                    let start_at = self.config.last_synced_block + 1;
                    if chain_head > self.config.last_synced_block {
                        log::info!(
                            "[MarketState] Catch-up: replaying blocks {} → {}",
                            start_at,
                            chain_head
                        );
                        // extract tracked_pools once (doesn't change after startup)
                        let tracked_for_catchup = self
                            .state_db
                            .read()
                            .map(|db| db.tracked_pools.clone())
                            .unwrap_or_default();
                        for bn in start_at..=chain_head {
                            match trace_block_diffs(&self.config.rpc_url, bn, &tracked_for_catchup).await {
                                Ok(diffs) => {
                                    if let Ok(mut db) = self.state_db.write() {
                                        db.set_block(bn);
                                        for addr in &diffs.touched_addresses {
                                            if db.tracked_pools.contains(addr) {
                                                if let Some(slots) = diffs.get_address_diffs(addr) {
                                                    for (slot, value) in slots {
                                                        db.update_slot(*addr, *slot, *value);
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                Err(e) => {
                                    log::warn!("[MarketState] Catch-up block {bn} failed: {e}");
                                }
                            }
                        }
                        log::info!("[MarketState] Catch-up complete — at block {}", chain_head);
                    } else {
                        log::info!("[MarketState] Already up to date at block {}", chain_head);
                    }
                }
                Err(e) => {
                    log::warn!("[MarketState] Could not get chain head for catch-up: {e}");
                }
            }
        }

        // ── Normal streaming from broadcast ────────────────────────────────
        loop {
            // Handle Lagged explicitly so the worker doesn't silently exit
            // when the broadcast buffer fills up during fast sync / catch-up.
            let event = match block_rx.recv().await {
                Ok(e) => e,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    log::warn!("[MarketState] Lagged: skipped {} blocks", n);
                    continue;
                }
                Err(_) => {
                    log::info!("[MarketState] Broadcast channel closed, shutting down");
                    break;
                }
            };
            match event {
                PipelineEvent::NewBlock {
                    block_number,
                    base_fee,
                    gas_used,
                    gas_limit,
                    ..
                } => {
                    let start = std::time::Instant::now();

                    // Update gas station with new block info
                    self.gas_station.update_from_block(base_fee, gas_used, gas_limit);

                    // Trace the block for storage diffs
                    // Extract tracked_pools (immutable after startup; clone is cheap)
                    let tracked_pools = self
                        .state_db
                        .read()
                        .map(|db| db.tracked_pools.clone())
                        .unwrap_or_default();
                    let trace_result =
                        trace_block_diffs(&self.config.rpc_url, block_number, &tracked_pools).await;

                    let block_diffs = match trace_result {
                        Ok(diffs) => diffs,
                        Err(e) => {
                            log::warn!(
                                "[MarketState] Block {} trace failed: {e}",
                                block_number
                            );
                            continue;
                        }
                    };

                    // Apply diffs to our state DB and collect touched pool addresses
                    let mut touched: Vec<Address> = Vec::new();
                    {
                        let mut db = match self.state_db.write() {
                            Ok(db) => db,
                            Err(_) => continue,
                        };

                        db.set_block(block_number);

                        for addr in &block_diffs.touched_addresses {
                            if db.tracked_pools.contains(addr) {
                                if let Some(slots) = block_diffs.get_address_diffs(addr) {
                                    for (slot, value) in slots {
                                        db.update_slot(*addr, *slot, *value);
                                    }
                                    touched.push(*addr);
                                }
                            }
                        }
                    }

                    log::info!(
                        "[MarketState] Block {} processed: {} pools updated in {:?}",
                        block_number,
                        touched.len(),
                        start.elapsed()
                    );

                    if !touched.is_empty() {
                        let event = PoolsTouched {
                            block_number,
                            touched_pools: touched,
                        };
                        if pools_tx.send(event).await.is_err() {
                            log::error!("[MarketState] Channel closed");
                            return;
                        }
                    }
                }
                _ => {} // ignore non-NewBlock events
            }
        }

        log::info!("[MarketState] Broadcast channel closed, shutting down");
    }
}
