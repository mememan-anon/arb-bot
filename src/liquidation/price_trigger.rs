/// Chainlink price-update trigger — injects PriceUpdate events into the
/// liquidation health-factor monitor so it reacts immediately when an
/// asset's oracle price changes, not only when Aave Borrow/Supply/Repay
/// events occur.
///
/// ## Two concurrent paths
///
/// **Path A — Confirmed-log (all chains):**
///   Subscribe to `AnswerUpdated(int256,uint256,uint256)` logs from each
///   Chainlink aggregator contract.  Fires as soon as the on-chain price
///   update is included in a block (~1 block latency on Base, ~12 s on
///   mainnet).
///
/// **Path B — Pending-mempool (chains with accessible mempool):**
///   Subscribe to full pending transactions.  Any transaction whose
///   calldata starts with the `forward(address,bytes)` selector (0x6fadcf72)
///   AND whose encoded inner `to` address is one of our watched aggregators
///   is treated as an imminent price update.  Fires *before* confirmation —
///   useful on Ethereum mainnet where the block window is 12 s.
///   On chains where the node does not support full pending-tx subscriptions
///   this path silently degrades to log-only mode.
///
/// ## Dedup / batching
///   Duplicate events (both paths firing for the same update) are handled
///   cheaply by health_factor.rs's batch-drain: it merges all queued events
///   into a single Multicall3 HF scan per cycle.
///
/// ## Config
///   Set `[[aave_v3.chainlink_feeds]]` entries in the chain TOML.  Each
///   entry provides the Aave reserve token address and its Chainlink
///   EACAggregatorProxy address.  The underlying OCR2 aggregator is resolved
///   at startup by calling `aggregator()` on the proxy; if that call fails
///   the proxy itself is used as the listen address.

use alloy::{
    primitives::Address,
    providers::{Provider, ProviderBuilder, WsConnect},
    rpc::types::{Filter, Log},
    sol,
    sol_types::SolCall,
};
use alloy::consensus::Transaction as _; // brings .input() into scope via deref
use futures_util::StreamExt;
use log::{debug, error, info, warn};
use std::{collections::HashMap, str::FromStr, time::{SystemTime, UNIX_EPOCH}};
use tokio::{
    sync::mpsc,
    time::{sleep, Duration},
};

use crate::config::ChainlinkFeed;
use super::types::{LiquidationUpdate, WhistleblowerEventDetails, WhistleblowerEventType};

// ── Chainlink constants ───────────────────────────────────────────────────────

/// ABI selector for `forward(address to, bytes calldata data)`.
/// keccak256("forward(address,bytes)")[..4] = 0x6fadcf72
const FORWARD_SELECTOR: [u8; 4] = [0x6f, 0xad, 0xcf, 0x72];

// ── ABI bindings ──────────────────────────────────────────────────────────────

sol! {
    /// Chainlink forwarder call — wraps transmit() in a forward() envelope.
    /// Selector 0x6fadcf72.
    function forward(address to, bytes calldata data) external;

    /// Chainlink EACAggregatorProxy — used to resolve the underlying OCR2
    /// aggregator address at startup.
    #[sol(rpc)]
    interface IEACAggregatorProxy {
        function aggregator() external view returns (address);
    }
}

// ── Aggregator resolution ─────────────────────────────────────────────────────

/// Call `aggregator()` on a Chainlink EACAggregatorProxy to get the underlying
/// OCR2 aggregator that actually emits `AnswerUpdated` events.
/// Falls back to `proxy` if the call fails or returns zero.
async fn resolve_aggregator<P: Provider>(proxy: Address, provider: &P) -> Address {
    match IEACAggregatorProxy::new(proxy, provider)
        .aggregator()
        .call()
        .await
    {
        Ok(ret) if ret != Address::ZERO => {
            info!("[price_trigger] proxy {proxy} → underlying aggregator {ret}");
            ret
        }
        Ok(_) => {
            info!(
                "[price_trigger] proxy {proxy} has no nested aggregator (zero) \
                — watching proxy directly"
            );
            proxy
        }
        Err(_) => {
            // Contract doesn't implement aggregator() — it's a direct aggregator
            // or custom adapter (PriceCapAdapter, etc.).  Watch it directly.
            info!("[price_trigger] proxy {proxy} is a direct feed — watching proxy directly");
            proxy
        }
    }
}

// ── Channel helper ────────────────────────────────────────────────────────────

fn inject_price_update(
    liq_tx: &mpsc::Sender<LiquidationUpdate>,
    asset: Address,
    block_number: u64,
    trace_id: String,
    source: &str,
) {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let update = LiquidationUpdate {
        trace_id: trace_id.clone(),
        block_number,
        enqueued_at_ms: now_ms,
        event_details: WhistleblowerEventDetails {
            event: WhistleblowerEventType::PriceUpdate,
            // args[0] = the Aave reserve token address (used by health_factor
            // to look up all users with that asset as collateral or debt).
            // Must use Display (to_string) to match Address::from_str() in health_factor.
            args: vec![asset.to_string()],
        },
    };
    match liq_tx.try_send(update) {
        Ok(_) => info!(
            "[price_trigger] [{source}] PriceUpdate queued — asset={asset} trace={trace_id}"
        ),
        Err(e) => warn!(
            "[price_trigger] [{source}] channel full — asset={asset} trace={trace_id}: {e}"
        ),
    }
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Spawn this as a peer task alongside `run_monitor` and `run_health_factor`.
/// It loops forever, reconnecting on WSS drops.
///
/// `enable_pending` — when `false`, Path B (mempool pending-tx subscription)
/// is skipped entirely; useful on chains like Base where the endpoint does not
/// support it, to avoid noisy warnings on every reconnect.
pub async fn run(
    wss_url: String,
    feeds: Vec<ChainlinkFeed>,
    liq_tx: mpsc::Sender<LiquidationUpdate>,
    enable_pending: bool,
) {
    if feeds.is_empty() {
        info!("[price_trigger] no chainlink_feeds configured — price trigger disabled");
        return;
    }

    if !enable_pending {
        info!("[price_trigger] mempool path DISABLED by config (chainlink_pending_txs=false)");
    }
    info!("[price_trigger] starting — {} configured feeds", feeds.len());

    loop {
        match run_inner(&wss_url, &feeds, &liq_tx, enable_pending).await {
            Ok(()) => warn!("[price_trigger] inner loop exited cleanly — restarting in 2 s"),
            Err(e) => error!("[price_trigger] inner loop error: {e} — restarting in 2 s"),
        }
        sleep(Duration::from_secs(2)).await;
    }
}

// ── Inner loop (reconnects on drop) ──────────────────────────────────────────

async fn run_inner(
    wss_url: &str,
    feeds: &[ChainlinkFeed],
    liq_tx: &mpsc::Sender<LiquidationUpdate>,
    enable_pending: bool,
) -> Result<(), String> {
    // ── Connect ───────────────────────────────────────────────────────────────
    let provider = ProviderBuilder::new()
        .connect_ws(WsConnect::new(wss_url))
        .await
        .map_err(|e| format!("WSS connect failed: {e}"))?;

    // ── Resolve aggregators ───────────────────────────────────────────────────
    // For each configured (asset, proxy) pair, call proxy.aggregator() to get
    // the underlying OCR2 contract that emits AnswerUpdated.  Build a reverse
    // map: aggregator_address → [aave_reserve_addresses].
    // A Vec is used because two different assets can share the same underlying
    // aggregator (e.g. a PriceCapAdapter wrapping the same OCR2 feed). Both
    // reserves must be scanned when that aggregator fires.
    let mut aggregator_to_asset: HashMap<Address, Vec<Address>> = HashMap::new();
    for feed in feeds {
        let proxy = Address::from_str(&feed.proxy)
            .map_err(|e| format!("invalid proxy address '{}': {e}", feed.proxy))?;
        let asset = Address::from_str(&feed.asset)
            .map_err(|e| format!("invalid asset address '{}': {e}", feed.asset))?;
        let aggregator = resolve_aggregator(proxy, &provider).await;
        aggregator_to_asset.entry(aggregator).or_default().push(asset);
    }

    let agg_addrs: Vec<Address> = aggregator_to_asset.keys().cloned().collect();

    // ── Path A: confirmed-log subscription ───────────────────────────────────
    let filter = Filter::new()
        .address(agg_addrs.clone())
        .event("AnswerUpdated(int256,uint256,uint256)");

    let mut log_stream = provider
        .subscribe_logs(&filter)
        .await
        .map_err(|e| format!("subscribe_logs failed: {e}"))?
        .into_stream();

    // ── Path B: pending-mempool subscription ─────────────────────────────────
    // Only attempted when `enable_pending` is true (set chainlink_pending_txs=false
    // in config to skip this entirely on chains that don't support it).
    let mut pending_stream = if !enable_pending {
        None
    } else { match provider.subscribe_full_pending_transactions().await {
        Ok(sub) => {
            info!("[price_trigger] mempool path ACTIVE — full pending-tx subscription ok");
            Some(sub.into_stream())
        }
        Err(e) => {
            warn!(
                "[price_trigger] full pending-tx subscription unavailable ({e}) \
                — running in confirmed-log mode only"
            );
            None
        }
    }};

    // ── Event loop ────────────────────────────────────────────────────────────
    loop {
        if let Some(ref mut pending) = pending_stream {
            tokio::select! {
                // Path A event
                log_opt = log_stream.next() => {
                    match log_opt {
                        Some(log) => handle_confirmed_log(&log, &aggregator_to_asset, liq_tx),
                        None => return Err("AnswerUpdated log stream closed".into()),
                    }
                }
                // Path B event
                tx_opt = pending.next() => {
                    match tx_opt {
                        Some(tx) => handle_pending_tx(tx, &aggregator_to_asset, liq_tx),
                        None => {
                            warn!(
                                "[price_trigger] mempool stream closed — \
                                switching to confirmed-log mode"
                            );
                            pending_stream = None;
                        }
                    }
                }
            }
        } else {
            // Log-only mode
            match log_stream.next().await {
                Some(log) => handle_confirmed_log(&log, &aggregator_to_asset, liq_tx),
                None => return Err("AnswerUpdated log stream closed".into()),
            }
        }
    }
}

// ── Path A handler ────────────────────────────────────────────────────────────

fn handle_confirmed_log(
    log: &Log,
    aggregator_to_asset: &HashMap<Address, Vec<Address>>,
    liq_tx: &mpsc::Sender<LiquidationUpdate>,
) {
    let aggregator = log.inner.address;
    let Some(assets) = aggregator_to_asset.get(&aggregator) else {
        debug!("[price_trigger] AnswerUpdated from non-watched address {aggregator}");
        return;
    };
    let block = log.block_number.unwrap_or(0);
    let trace = format!("CL{block:08x}");
    info!(
        "[price_trigger] AnswerUpdated confirmed — block={block} \
        aggregator={aggregator} assets={}",
        assets.len()
    );
    for &asset in assets {
        inject_price_update(liq_tx, asset, block, trace.clone(), "confirmed");
    }
}

// ── Path B handler ────────────────────────────────────────────────────────────

fn handle_pending_tx(
    tx: alloy::rpc::types::Transaction,
    aggregator_to_asset: &HashMap<Address, Vec<Address>>,
    liq_tx: &mpsc::Sender<LiquidationUpdate>,
) {
    // ── Gate 1: must start with forward(address,bytes) selector ─────────────
    // This is a fast O(4) check that drops the vast majority of pending txs
    // before any heap allocation.
    let input = tx.input();
    if input.len() < 4 || input[..4] != FORWARD_SELECTOR {
        return;
    }

    // ── Gate 2: decode forward calldata, check inner destination ────────────
    // The inner `to` field is the OCR2 aggregator being updated.
    let Ok(decoded) = forwardCall::abi_decode(input) else {
        return;
    };
    let inner_to = decoded.to;
    let Some(assets) = aggregator_to_asset.get(&inner_to) else {
        // `forward()` call aimed at some other contract — ignore
        return;
    };

    // ── Matched: Chainlink transmit for one of our watched aggregators ────────
    let tx_hash = tx.inner.tx_hash();
    let trace = format!("{tx_hash:?}")[2..10].to_string();
    info!(
        "[price_trigger] MEMPOOL forward(transmit) → aggregator={inner_to} \
        assets={} tx={tx_hash:?}",
        assets.len()
    );
    for &asset in assets {
        inject_price_update(liq_tx, asset, 0, trace.clone(), "mempool");
    }
}
