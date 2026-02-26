/// AAVE v3 event monitor — Phase 1 of the liquidation sub-system.
///
/// Adapted from overlord-rs/crates/whistleblower-rs/src/main.rs.
///
/// Changes from the original:
///   • IPC transport (`/tmp/reth.ipc`) replaced with WSS — same URL already
///     used by the arb-bot core.
///   • ZMQ PUSH socket replaced with a `tokio::sync::mpsc` channel — the
///     downstream consumer (health_factor task, Phase 2) receives
///     `LiquidationUpdate` messages directly.
///   • The Ethereum mainnet AAVE pool address is replaced with the address
///     read from the bot's config (`Config.aave_v3.pool`).
///   • `#[tokio::main]` entry point replaced with a plain `async fn` that
///     the arb-bot `JoinSet` spawns as a peer task.
///   • Linux rsyslog / rolling-file logging removed; the existing fern
///     logger already covers the process.

use alloy::{
    primitives::{keccak256, Address, FixedBytes, U64},
    providers::{Provider, ProviderBuilder, WsConnect},
    rpc::types::{Filter, Log},
    sol,
};
use futures_util::{stream::select_all, StreamExt};
use log::{info, warn};
use std::{collections::{HashMap, HashSet}, str::FromStr, sync::Arc};
use tokio::{sync::mpsc, time::{sleep, Duration}};
use std::time::{SystemTime, UNIX_EPOCH};

use super::types::{
    LiquidationUpdate, WhistleblowerEventDetails, WhistleblowerEventType,
};

// ---------------------------------------------------------------------------
// AAVE v3 Pool ABI — sol! binding identical to whistleblower-rs.
// Path is relative to arb-bot/Cargo.toml (CARGO_MANIFEST_DIR).
// ---------------------------------------------------------------------------
sol!(
    #[allow(missing_docs)]
    #[sol(rpc)]
    #[allow(clippy::too_many_arguments)]
    AAVE_V3_POOL,
    "src/abi/aave_v3_pool.json",
);

// ---------------------------------------------------------------------------
// Error type (direct port of WhistleblowerError from whistleblower-rs)
// ---------------------------------------------------------------------------
#[allow(dead_code)] // variants will be used in Phase 2 error propagation
#[allow(clippy::enum_variant_names)]
#[derive(Debug)]
enum MonitorError {
    ProviderError(String),
    SubscriptionError(String),
    EventProcessingError(String),
}

impl std::error::Error for MonitorError {}
impl std::fmt::Display for MonitorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MonitorError::ProviderError(e) => write!(f, "Provider error: {}", e),
            MonitorError::SubscriptionError(e) => write!(f, "Subscription error: {}", e),
            MonitorError::EventProcessingError(e) => {
                write!(f, "Event processing error: {}", e)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// EventProcessor trait + four processors — direct port from whistleblower-rs.
// ---------------------------------------------------------------------------
trait EventProcessor: Send + Sync {
    fn process(
        &self,
        log: &Log,
        block_number: U64,
    ) -> Result<WhistleblowerEventDetails, MonitorError>;
}

struct LiquidationCallProcessor;
impl EventProcessor for LiquidationCallProcessor {
    fn process(&self, log: &Log, block_number: U64) -> Result<WhistleblowerEventDetails, MonitorError> {
        let decoded = log.log_decode::<AAVE_V3_POOL::LiquidationCall>().map_err(|e| {
            MonitorError::EventProcessingError(format!("Failed to decode LiquidationCall: {}", e))
        })?;
        let AAVE_V3_POOL::LiquidationCall {
            collateralAsset,
            debtAsset,
            user,
            debtToCover,
            liquidatedCollateralAmount,
            liquidator,
            ..
        } = decoded.inner.data;
        info!(
            "LIQUIDATION CALL block={:?} tx={:?} user={} liquidator={}",
            block_number,
            log.transaction_hash,
            user,
            liquidator,
        );
        Ok(WhistleblowerEventDetails {
            event: WhistleblowerEventType::LiquidationCall,
            args: vec![
                collateralAsset.to_string(),
                debtAsset.to_string(),
                user.to_string(),
                debtToCover.to_string(),
                liquidatedCollateralAmount.to_string(),
                liquidator.to_string(),
            ],
        })
    }
}

struct BorrowProcessor;
impl EventProcessor for BorrowProcessor {
    fn process(&self, log: &Log, block_number: U64) -> Result<WhistleblowerEventDetails, MonitorError> {
        let decoded = log.log_decode::<AAVE_V3_POOL::Borrow>().map_err(|e| {
            MonitorError::EventProcessingError(format!("Failed to decode Borrow: {}", e))
        })?;
        let AAVE_V3_POOL::Borrow { reserve, onBehalfOf, .. } = decoded.inner.data;
        info!(
            "BORROW block={:?} reserve={} on_behalf_of={}",
            block_number, reserve, onBehalfOf,
        );
        Ok(WhistleblowerEventDetails {
            event: WhistleblowerEventType::Borrow,
            args: vec![reserve.to_string(), onBehalfOf.to_string()],
        })
    }
}

struct SupplyProcessor;
impl EventProcessor for SupplyProcessor {
    fn process(&self, log: &Log, block_number: U64) -> Result<WhistleblowerEventDetails, MonitorError> {
        let decoded = log.log_decode::<AAVE_V3_POOL::Supply>().map_err(|e| {
            MonitorError::EventProcessingError(format!("Failed to decode Supply: {}", e))
        })?;
        let AAVE_V3_POOL::Supply { reserve, onBehalfOf, .. } = decoded.inner.data;
        info!(
            "SUPPLY block={:?} reserve={} on_behalf_of={}",
            block_number, reserve, onBehalfOf,
        );
        Ok(WhistleblowerEventDetails {
            event: WhistleblowerEventType::Supply,
            args: vec![reserve.to_string(), onBehalfOf.to_string()],
        })
    }
}

struct RepayProcessor;
impl EventProcessor for RepayProcessor {
    fn process(&self, log: &Log, block_number: U64) -> Result<WhistleblowerEventDetails, MonitorError> {
        let decoded = log.log_decode::<AAVE_V3_POOL::Repay>().map_err(|e| {
            MonitorError::EventProcessingError(format!("Failed to decode Repay: {}", e))
        })?;
        let AAVE_V3_POOL::Repay { reserve, user, .. } = decoded.inner.data;
        info!(
            "REPAY block={:?} reserve={} user={}",
            block_number, reserve, user,
        );
        Ok(WhistleblowerEventDetails {
            event: WhistleblowerEventType::Repay,
            args: vec![reserve.to_string(), user.to_string()],
        })
    }
}

// ---------------------------------------------------------------------------
// send_update — replaces ZMQ send_whistleblower_update from whistleblower-rs.
// ---------------------------------------------------------------------------
fn send_update(
    log: &Log,
    event_details: &WhistleblowerEventDetails,
    tx: &mpsc::Sender<LiquidationUpdate>,
) {
    let trace_id = log
        .transaction_hash
        .as_ref()
        .map_or_else(|| "unknown".to_string(), |h| {
            let hex = hex::encode(h.as_slice());
            // Take first 8 chars like overlord-rs (skip the leading "0x" — raw bytes have none)
            hex[..8.min(hex.len())].to_string()
        });

    let update = LiquidationUpdate {
        trace_id,
        block_number: log.block_number.unwrap_or_default(),
        enqueued_at_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        event_details: event_details.clone(),
    };

    if let Err(e) = tx.try_send(update) {
        warn!("liquidation::monitor: failed to forward event: {}", e);
    } else {
        info!("liquidation::monitor: update forwarded (event={:?})", event_details.event);
    }
}

// ---------------------------------------------------------------------------
// Public entry point — spawned as a JoinSet task from main.rs.
//
// Parameters
//   wss_url      — same WSS URL used by the arb core
//   pool_address — AAVE v3 Pool address for this chain (from config)
//   tx           — channel to the downstream consumer (health_factor, Phase 2)
// ---------------------------------------------------------------------------
pub async fn run(
    wss_url: String,
    pool_address: String,
    tx: mpsc::Sender<LiquidationUpdate>,
) {
    // Build event-signature → processor map (direct port from whistleblower-rs)
    let liquidation_call_sig: FixedBytes<32> = keccak256(
        "LiquidationCall(address,address,address,uint256,uint256,address,bool)".as_bytes(),
    );
    let borrow_sig: FixedBytes<32> =
        keccak256("Borrow(address,address,address,uint256,uint8,uint256,uint16)".as_bytes());
    let supply_sig: FixedBytes<32> =
        keccak256("Supply(address,address,address,uint256,uint16)".as_bytes());
    let repay_sig: FixedBytes<32> =
        keccak256("Repay(address,address,address,uint256,bool)".as_bytes());

    let event_processors: HashMap<FixedBytes<32>, Arc<dyn EventProcessor>> = [
        (liquidation_call_sig, Arc::new(LiquidationCallProcessor) as Arc<dyn EventProcessor>),
        (borrow_sig,           Arc::new(BorrowProcessor)           as Arc<dyn EventProcessor>),
        (supply_sig,           Arc::new(SupplyProcessor)           as Arc<dyn EventProcessor>),
        (repay_sig,            Arc::new(RepayProcessor)            as Arc<dyn EventProcessor>),
    ]
    .into();

    let aave_pool_address: Address = match Address::from_str(&pool_address) {
        Ok(a) => a,
        Err(e) => {
            warn!("liquidation::monitor: invalid AAVE pool address '{}': {}", pool_address, e);
            return;
        }
    };

    info!(
        "liquidation::monitor: starting — pool={} wss={}",
        aave_pool_address, wss_url
    );

    // Outer reconnect loop — mirrors the `loop { … }` in whistleblower-rs main().
    loop {
        let ws = WsConnect::new(wss_url.clone());
        let provider = match ProviderBuilder::new().connect_ws(ws).await {
            Ok(p) => Arc::new(p),
            Err(e) => {
                warn!("liquidation::monitor: WS connect failed: {}. Retrying in 5s…", e);
                sleep(Duration::from_secs(5)).await;
                continue;
            }
        };

        // Build individual event subscriptions — same approach as whistleblower-rs.
        let make_sub = |sig: FixedBytes<32>| {
            let p = provider.clone();
            async move {
                p.subscribe_logs(&Filter::new().event_signature(sig)).await
            }
        };

        let (liq_sub, borrow_sub, supply_sub, repay_sub) = tokio::join!(
            make_sub(liquidation_call_sig),
            make_sub(borrow_sig),
            make_sub(supply_sig),
            make_sub(repay_sig),
        );

        let mut all_streams = match (liq_sub, borrow_sub, supply_sub, repay_sub) {
            (Ok(l), Ok(b), Ok(s), Ok(r)) => select_all(vec![
                l.into_stream(),
                b.into_stream(),
                s.into_stream(),
                r.into_stream(),
            ]),
            (l, b, s, r) => {
                if l.is_err() { warn!("liquidation::monitor: LiquidationCall subscribe failed: {:?}", l.err()); }
                if b.is_err() { warn!("liquidation::monitor: Borrow subscribe failed: {:?}", b.err()); }
                if s.is_err() { warn!("liquidation::monitor: Supply subscribe failed: {:?}", s.err()); }
                if r.is_err() { warn!("liquidation::monitor: Repay subscribe failed: {:?}", r.err()); }
                sleep(Duration::from_secs(5)).await;
                continue;
            }
        };

        info!("liquidation::monitor: listening for AAVE v3 events…");

        // Per-connection dedup set — clears on every new block to stay bounded.
        // dRPC sometimes delivers the same confirmed log 2-4x in the same millisecond;
        // tx_hash is the exact identity of a log entry.
        let mut seen_tx_hashes: HashSet<FixedBytes<32>> = HashSet::new();
        let mut last_cleared_block: u64 = 0;

        // Inner event loop — mirrors `while let Some(log) = all_event_streams.next().await`
        while let Some(log) = all_streams.next().await {
            let block_number = U64::from(log.block_number.unwrap_or_default());

            // Clear dedup set on block advance to prevent unbounded growth.
            let block_u64 = log.block_number.unwrap_or_default();
            if block_u64 > last_cleared_block {
                seen_tx_hashes.clear();
                last_cleared_block = block_u64;
            }

            // Deduplicate — skip if we already forwarded this exact transaction.
            if let Some(tx_hash) = log.transaction_hash {
                if !seen_tx_hashes.insert(tx_hash) {
                    // dRPC duplicate delivery — same log delivered again.
                    continue;
                }
            }

            // Filter: only process logs from the configured AAVE pool (same guard as whistleblower-rs)
            if log.address() != aave_pool_address {
                continue;
            }

            let Some(event_sig) = log.topics().first() else {
                warn!("liquidation::monitor: empty log topics: {:?}", log);
                continue;
            };

            match event_processors.get(event_sig) {
                Some(processor) => match processor.process(&log, block_number) {
                    Ok(details) => send_update(&log, &details, &tx),
                    Err(e) => warn!("liquidation::monitor: processing error: {}", e),
                },
                None => {
                    warn!("liquidation::monitor: unknown event signature {:?}", event_sig);
                }
            }
        }

        warn!("liquidation::monitor: stream closed. Reconnecting in 5s…");
        sleep(Duration::from_secs(5)).await;
    }
}
