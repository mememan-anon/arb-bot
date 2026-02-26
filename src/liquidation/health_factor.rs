/// AAVE v3 health-factor monitor — Phase 2 of the liquidation sub-system.
///
/// Adapted from overlord-rs/crates/vega-rs/src/{user_reserve_cache.rs, calc_utils.rs}.
///
/// Architecture (mirrors vega-rs original):
///   - Maintains UserReservesCache:
///       HashMap<ReserveAddress, HashMap<PositionType, Vec<UserAddress>>>
///   - Receives LiquidationUpdate events from the Phase 1 monitor (mpsc channel)
///   - On each Borrow/Supply/Repay:
///       1. Drop user from cache, re-fetch positions from chain, re-add
///       2. Check HF for ALL cached users in that reserve (not just the one who triggered)
///   - On LiquidationCall: drop the liquidated user (may no longer have debt)
///   - HF < 1 && collateral > threshold → emit UnderwaterUserAlert on broadcast channel
///
/// Key differences from vega-rs:
///   - No pre-loaded address file — cache is built purely from live AAVE events
///   - IPC transport replaced with WSS alloy provider (same URL as arb core)
///   - ZMQ/event bus replaced with tokio::sync::broadcast channel
///   - No ForkProvider / fork simulation; we query real chain state
///   - Chainlink price-update trigger replaced with AAVE pool event trigger

use alloy::{
    primitives::{address, Address, U256},
    providers::{Provider, ProviderBuilder, WsConnect},
    sol,
    sol_types::SolCall,
};
use futures::future::join_all;
use log::{info, warn};
use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::{BufRead, BufReader},
    str::FromStr,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::{broadcast, mpsc},
    task,
};

use super::types::{LiquidationUpdate, UnderwaterUserAlert, WhistleblowerEventType};

// ── Inline ABI bindings ───────────────────────────────────────────────────────
// Using inline sol! avoids duplicating the JSON files.
// Function selectors are derived from the Solidity signatures and must exactly
// match the on-chain AAVE v3 contract ABI.

sol! {
    /// Subset of the AAVE v3 Pool interface — only what Phase 2 needs.
    #[sol(rpc)]
    interface IPool {
        function getUserAccountData(address user) external view returns (
            uint256 totalCollateralBase,
            uint256 totalDebtBase,
            uint256 availableBorrowsBase,
            uint256 currentLiquidationThreshold,
            uint256 ltv,
            uint256 healthFactor
        );
    }
}

sol! {
    /// UserReserveData as returned by UiPoolDataProvider.getUserReservesData.
    /// Field order matches the on-chain struct definition exactly.
    struct UserReserveData {
        address underlyingAsset;
        uint256 scaledATokenBalance;
        bool    usageAsCollateralEnabledOnUser;
        uint256 scaledVariableDebt;
    }

    /// Subset of AAVE v3 UiPoolDataProvider interface.
    #[sol(rpc)]
    interface IUiPoolDataProvider {
        function getUserReservesData(
            address poolAddressesProvider,
            address user
        ) external view returns (UserReserveData[], uint8);
    }
}

sol! {
    /// Multicall3 — deployed at the same address on every EVM chain.
    /// We use aggregate3 (allowFailure=true) so one bad address never aborts the batch.
    #[sol(rpc)]
    interface IMulticall3 {
        struct Call3 {
            address target;
            bool allowFailure;
            bytes callData;
        }
        struct Result {
            bool success;
            bytes returnData;
        }
        function aggregate3(Call3[] calldata calls)
            external
            payable
            returns (Result[] memory returnData);
    }
}

// ── Constants (ported from vega-rs/calc_utils.rs) ────────────────────────────

/// Multicall3 canonical address — same on Base, Ethereum, every EVM chain.
const MULTICALL3: Address = address!("cA11bde05977b3631167028862bE2a173976CA11");

/// Addresses per Multicall3 batch in `initialize_cache` Phase A.
/// 500 getUserAccountData calls → one RPC round-trip.
const MC_BATCH_SIZE: usize = 500;

/// AAVE health factor uses ray units (1e18).  HF < 1e18 means liquidatable.
const HF_MIN_THRESHOLD: u128 = 1_000_000_000_000_000_000_u128;

/// Minimum collateral base (AAVE oracle units, 8 decimals) before routing
/// an alert to the executor.  Ported from vega-rs MIN_REPORTABLE_COLLATERAL.
///   1e10 ≈ $100 · 1e8/unit   |   1e12 ≈ $10 000
/// Using 1e10 to match vega-rs default.
const MIN_REPORTABLE_COLLATERAL: u128 = 10_000_000_000_u128; // 1e10 ≈ $100

// ── Cache types (ported from vega-rs/user_reserve_cache.rs) ──────────────────

#[derive(Debug, Eq, PartialEq, Hash, Clone)]
enum PositionType {
    Borrowed,
    Collateral,
}

type UserAddress = Address;
type ReserveAddress = Address;

/// Per-reserve, per-position-type user list.
///
/// Direct port of vega-rs `UserReservesCache`.  RwLock is removed — the
/// entire cache lives in the `run()` async fn on a single Tokio task.
struct UserReservesCache {
    inner: HashMap<ReserveAddress, HashMap<PositionType, Vec<UserAddress>>>,
}

impl UserReservesCache {
    fn new() -> Self {
        Self {
            inner: HashMap::new(),
        }
    }

    // ── Port of _drop_user_from_cache ────────────────────────────────────────
    fn drop_user(&mut self, user: UserAddress) {
        for by_position in self.inner.values_mut() {
            for users in by_position.values_mut() {
                users.retain(|u| u != &user);
            }
        }
    }

    // ── Port of _add_user_to_cache ───────────────────────────────────────────
    /// Query chain positions for `user`; skip if no debt; populate cache.
    /// Takes `provider` directly and creates contract instances internally,
    /// avoiding complex alloy generic instance types.
    async fn add_user<P>(
        &mut self,
        user: UserAddress,
        pool_address: Address,
        ui_address: Address,
        pool_addresses_provider: Address,
        provider: &P,
    ) -> Result<(), String>
    where
        P: Provider + Clone,
    {
        let pool = IPool::new(pool_address, provider.clone());
        let ui   = IUiPoolDataProvider::new(ui_address, provider.clone());

        // Step 0 — skip users with no debt (mirrors vega-rs has_debt check)
        let debt = pool
            .getUserAccountData(user)
            .call()
            .await
            .map(|d| d.totalDebtBase)
            .map_err(|e| format!("getUserAccountData({user}): {e}"))?;

        if debt == U256::ZERO {
            info!("[health_factor] {user} has no debt — skipping cache add");
            return Ok(());
        }

        // Step 1 — fetch per-reserve positions
        let positions = ui
            .getUserReservesData(pool_addresses_provider, user)
            .call()
            .await
            .map(|d| d._0)
            .map_err(|e| format!("getUserReservesData({user}): {e}"))?;

        // Step 2 — insert into cache (same logic as vega-rs)
        for pos in positions {
            let entry = self.inner.entry(pos.underlyingAsset).or_default();
            if pos.scaledVariableDebt > U256::ZERO {
                entry
                    .entry(PositionType::Borrowed)
                    .or_default()
                    .push(user);
            }
            if pos.usageAsCollateralEnabledOnUser && pos.scaledATokenBalance > U256::ZERO {
                entry
                    .entry(PositionType::Collateral)
                    .or_default()
                    .push(user);
            }
        }
        Ok(())
    }

    // ── Port of update_cache: drop + re-add ──────────────────────────────────
    async fn refresh_user<P>(
        &mut self,
        user: UserAddress,
        pool_address: Address,
        ui_address: Address,
        pool_addresses_provider: Address,
        provider: &P,
    ) where
        P: Provider + Clone,
    {
        self.drop_user(user);
        if let Err(e) = self
            .add_user(user, pool_address, ui_address, pool_addresses_provider, provider)
            .await
        {
            warn!("[health_factor] refresh_user {user}: {e}");
        }
    }

    /// All unique users that touch a given reserve (borrowed OR supplied as collateral).
    fn users_for_reserve(&self, reserve: ReserveAddress) -> Vec<UserAddress> {
        let Some(by_position) = self.inner.get(&reserve) else {
            return vec![];
        };
        let mut seen = HashSet::new();
        let mut out = vec![];
        for users in by_position.values() {
            for &u in users {
                if seen.insert(u) {
                    out.push(u);
                }
            }
        }
        out
    }

    /// Total number of (user, position) entries across all reserves.
    fn len(&self) -> usize {
        self.inner
            .values()
            .flat_map(|m| m.values())
            .map(|v| v.len())
            .sum()
    }

    /// Insert pre-fetched reserve positions for a user directly into the cache.
    /// Used by `initialize_cache` to bulk-seed from the borrowers file.
    fn insert_user_positions(&mut self, user: UserAddress, positions: Vec<UserReserveData>) {
        for pos in positions {
            let entry = self.inner.entry(pos.underlyingAsset).or_default();
            if pos.scaledVariableDebt > U256::ZERO {
                entry.entry(PositionType::Borrowed).or_default().push(user);
            }
            if pos.usageAsCollateralEnabledOnUser && pos.scaledATokenBalance > U256::ZERO {
                entry
                    .entry(PositionType::Collateral)
                    .or_default()
                    .push(user);
            }
        }
    }
}

// ── Startup cache seed (vega-rs initialize_cache port) ───────────────────────

/// Load a flat borrowers file (one address per line) and pre-populate the
/// `UserReservesCache` with their current on-chain positions.
///
/// Called once at Phase 2 startup before entering the live-event loop.
/// Direct port of `vega-rs::UserReservesCache::initialize_cache()`.
async fn initialize_cache<P>(
    cache: &mut UserReservesCache,
    borrowers_file: &str,
    pool_addr: Address,
    ui_addr: Address,
    pap_addr: Address,
    provider: &P,
) where
    P: Provider + Clone + Send + Sync + 'static,
{
    // ── Load addresses from file ─────────────────────────────────────────────
    let file = match File::open(borrowers_file) {
        Ok(f) => f,
        Err(e) => {
            warn!("[health_factor] initialize_cache: cannot open '{borrowers_file}': {e}");
            return;
        }
    };
    let raw_addresses: Vec<Address> = BufReader::new(file)
        .lines()
        .filter_map(|l| l.ok())
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .filter_map(|l| Address::from_str(&l).ok())
        .collect::<HashSet<_>>()   // deduplicate
        .into_iter()
        .collect();

    let total = raw_addresses.len();
    info!("[health_factor] initialize_cache: {total} unique addresses from {borrowers_file}");

    if total == 0 {
        warn!("[health_factor] initialize_cache: no valid addresses in file — starting empty");
        return;
    }

    // ── Phase A: Multicall3 — getUserAccountData in batches of MC_BATCH_SIZE ─────
    // 75,610 addresses / 500 per batch = ~152 RPC calls vs 75,610 individual calls.
    // aggregate3 with allowFailure=true so a bad address skips cleanly.
    let chunks_a: Vec<Vec<Address>> = raw_addresses
        .chunks(MC_BATCH_SIZE)
        .map(|c| c.to_vec())
        .collect();
    let num_batches = chunks_a.len();
    info!(
        "[health_factor] initialize_cache: phase A — {num_batches} multicall batches × {MC_BATCH_SIZE} (getUserAccountData)"
    );

    let mut tasks_a = vec![];
    for chunk in chunks_a {
        let provider = provider.clone();
        tasks_a.push(task::spawn(async move {
            let calls: Vec<IMulticall3::Call3> = chunk
                .iter()
                .map(|&user| IMulticall3::Call3 {
                    target: pool_addr,
                    allowFailure: true,
                    callData: IPool::getUserAccountDataCall { user }
                        .abi_encode()
                        .into(),
                })
                .collect();

            let mc = IMulticall3::new(MULTICALL3, provider);
            let mc_results = match mc.aggregate3(calls).call().await {
                Ok(r) => r,
                Err(e) => {
                    warn!("[health_factor] multicall batch failed: {e}");
                    return vec![];
                }
            };

            mc_results
                .iter()
                .zip(chunk.iter())
                .filter_map(|(result, &addr)| {
                    if !result.success {
                        return None;
                    }
                    let ret = IPool::getUserAccountDataCall::abi_decode_returns(
                        &result.returnData,
                    )
                    .ok()?;
                    if ret.totalDebtBase > U256::ZERO {
                        Some(addr)
                    } else {
                        None
                    }
                })
                .collect::<Vec<Address>>()
        }));
    }

    let debt_holders: Vec<Address> = join_all(tasks_a)
        .await
        .into_iter()
        .filter_map(|r| r.ok())
        .flatten()
        .collect();

    let debtor_count = debt_holders.len();
    info!(
        "[health_factor] initialize_cache: phase A done — {debtor_count}/{total} have active debt"
    );

    // ── Phase B: getUserReservesData for debt holders only ───────────────────────
    // Dynamic array return makes multicall decoding complex; parallel individual
    // calls on the much-smaller debt_holders set is fast enough.
    let bucket_size = 50_usize;
    let buckets_b: Vec<Vec<Address>> = debt_holders
        .chunks(bucket_size)
        .map(|c| c.to_vec())
        .collect();
    let num_buckets = buckets_b.len();
    info!(
        "[health_factor] initialize_cache: phase B — {num_buckets} buckets × {bucket_size} (getUserReservesData)"
    );

    let mut tasks_b = vec![];
    for bucket in buckets_b {
        let provider = provider.clone();
        tasks_b.push(task::spawn(async move {
            let ui = IUiPoolDataProvider::new(ui_addr, provider);
            let mut results: Vec<(Address, Vec<UserReserveData>)> = vec![];
            for addr in bucket {
                let positions = ui
                    .getUserReservesData(pap_addr, addr)
                    .call()
                    .await
                    .map(|d| d._0)
                    .unwrap_or_default();
                results.push((addr, positions));
            }
            results
        }));
    }

    let mut seeded = 0_usize;
    for task_result in join_all(tasks_b).await {
        let Ok(user_positions) = task_result else {
            continue;
        };
        for (user, positions) in user_positions {
            cache.insert_user_positions(user, positions);
            seeded += 1;
        }
    }

    info!(
        "[health_factor] initialize_cache done: {seeded}/{debtor_count} seeded  cache_entries={}",
        cache.len()
    );
}

// ── HF checker — Multicall3 batched ──────────────────────────────────────────

/// Check health factors for a slice of users via Multicall3.
///
/// Instead of N individual `getUserAccountData` RPC calls, this packs
/// MC_BATCH_SIZE (500) calls into a single `aggregate3` eth_call per batch.
/// 52,000 users → ~104 RPC round-trips instead of 52,000.
/// Batches run concurrently as Tokio tasks.
async fn check_hf_for_users<P>(
    users: Vec<UserAddress>,
    pool_address: Address,
    provider: P,
    trace_id: String,
    alert_tx: broadcast::Sender<UnderwaterUserAlert>,
) where
    P: Provider + Clone + Send + Sync + 'static,
{
    if users.is_empty() {
        return;
    }

    let total = users.len();
    let chunks: Vec<Vec<Address>> = users.chunks(MC_BATCH_SIZE).map(|c| c.to_vec()).collect();
    let num_batches = chunks.len();

    info!(
        "[health_factor] HF check — trace={trace_id} users={total} \
        batches={num_batches} batch_size={MC_BATCH_SIZE} (multicall3)"
    );

    let mut tasks = vec![];
    for chunk in chunks {
        let provider = provider.clone();
        let trace_id = trace_id.clone();
        let alert_tx = alert_tx.clone();
        tasks.push(task::spawn(async move {
            let calls: Vec<IMulticall3::Call3> = chunk
                .iter()
                .map(|&user| IMulticall3::Call3 {
                    target: pool_address,
                    allowFailure: true,
                    callData: IPool::getUserAccountDataCall { user }
                        .abi_encode()
                        .into(),
                })
                .collect();

            let mc = IMulticall3::new(MULTICALL3, provider);
            let mc_results = match mc.aggregate3(calls).call().await {
                Ok(r) => r,
                Err(e) => {
                    warn!("[health_factor] multicall3 batch failed (trace={trace_id}): {e}");
                    return;
                }
            };

            for (result, user) in mc_results.iter().zip(chunk.iter()) {
                if !result.success {
                    continue;
                }
                let ret = match IPool::getUserAccountDataCall::abi_decode_returns(
                    &result.returnData,
                ) {
                    Ok(r) => r,
                    Err(e) => {
                        warn!("[health_factor] decode getUserAccountData({user}): {e}");
                        continue;
                    }
                };
                let hf = ret.healthFactor;
                let collateral = ret.totalCollateralBase;
                // Gate: HF != 0 (uninitialized) AND < 1.0 AND enough collateral
                if hf != U256::ZERO
                    && hf < U256::from(HF_MIN_THRESHOLD)
                    && collateral > U256::from(MIN_REPORTABLE_COLLATERAL)
                {
                    info!(
                        "[health_factor] UNDERWATER user={user} \
                        hf={hf} collateral={collateral} trace={trace_id}"
                    );
                    let _ = alert_tx.send(UnderwaterUserAlert {
                        user: *user,
                        trace_id: trace_id.clone(),
                        health_factor: hf,
                        total_collateral_base: collateral,
                    });
                }
            }
        }));
    }

    join_all(tasks).await;
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Phase 2 entry point.  Spawned by `main.rs` as a `JoinSet` task.
///
/// Receives `LiquidationUpdate` events from the Phase 1 monitor, maintains
/// the `UserReservesCache`, and emits `UnderwaterUserAlert` events whenever
/// a user's health factor drops below 1.
///
/// Parameters mirror the per-chain fields added to `AaveV3Config`.
pub async fn run(
    wss_url: String,
    pool_address: String,
    pool_addresses_provider: String,
    ui_pool_data_provider: String,
    borrowers_file: Option<String>,
    mut liq_rx: mpsc::Receiver<LiquidationUpdate>,
    alert_tx: broadcast::Sender<UnderwaterUserAlert>,
) {
    // Parse config addresses up front so we fail loud at startup, not mid-run.
    let pool_addr = match Address::from_str(&pool_address) {
        Ok(a) => a,
        Err(e) => {
            warn!("[health_factor] invalid pool_address: {e}");
            return;
        }
    };
    let pap_addr = match Address::from_str(&pool_addresses_provider) {
        Ok(a) => a,
        Err(e) => {
            warn!("[health_factor] invalid pool_addresses_provider: {e}");
            return;
        }
    };
    let ui_addr = match Address::from_str(&ui_pool_data_provider) {
        Ok(a) => a,
        Err(e) => {
            warn!("[health_factor] invalid ui_pool_data_provider: {e}");
            return;
        }
    };

    info!("[health_factor] starting — pool={pool_addr} pap={pap_addr} ui={ui_addr}");

    let ws = WsConnect::new(wss_url.clone());
    let provider = match ProviderBuilder::new().connect_ws(ws).await {
        Ok(p) => p,
        Err(e) => {
            warn!("[health_factor] WSS connect failed: {e}");
            return;
        }
    };

    let mut cache = UserReservesCache::new();

    // ── Startup seed (vega-rs initialize_cache) ───────────────────────────────
    if let Some(ref path) = borrowers_file {
        initialize_cache(&mut cache, path, pool_addr, ui_addr, pap_addr, &provider).await;
    } else {
        info!("[health_factor] no borrowers_file — starting with empty cache (event-driven only)");
    }

    while let Some(first) = liq_rx.recv().await {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        // ── Drain the queue ───────────────────────────────────────────────────
        // Collect every message that is ALREADY waiting in the channel (non-
        // blocking try_recv) so we can merge them into one HF check instead of
        // running a separate 60-second batch for each stacked event.
        let mut batch = vec![first];
        while let Ok(msg) = liq_rx.try_recv() {
            batch.push(msg);
        }

        let batch_size = batch.len();

        let head_block = match provider.get_block_number().await {
            Ok(n) => n,
            Err(e) => {
                warn!("[health_factor] get_block_number failed: {e}");
                0
            }
        };

        let mut min_event_block = u64::MAX;
        let mut max_event_block = 0_u64;
        let mut max_queue_ms = 0_u64;
        for update in &batch {
            min_event_block = min_event_block.min(update.block_number);
            max_event_block = max_event_block.max(update.block_number);
            if update.enqueued_at_ms > 0 {
                max_queue_ms = max_queue_ms.max(now_ms.saturating_sub(update.enqueued_at_ms));
            }
        }

        if batch_size > 1 {
            let min_lag_blocks = head_block.saturating_sub(max_event_block);
            let max_lag_blocks = head_block.saturating_sub(min_event_block);
            // If min_event_block is 0 the range is misleading (offchain events).
            // Only print lag range when the earliest event has a real block number.
            let lag_display = if min_event_block > 0 {
                format!("{}..{}", min_lag_blocks, max_lag_blocks)
            } else if max_event_block > 0 {
                format!("{}..N/A", min_lag_blocks)
            } else {
                "N/A".to_string()
            };
            info!(
                "[health_factor] drained {batch_size} queued events — merging into one HF check \
                (lag_blocks={} max_queue_ms={})",
                lag_display,
                max_queue_ms,
            );
        }

        // ── Process each event in the batch ──────────────────────────────────
        // LiquidationCalls are acted on immediately; Borrow/Supply/Repay events
        // contribute their reserve's candidates to a merged set.
        let mut merged_candidates: std::collections::HashSet<Address> =
            std::collections::HashSet::new();
        let mut last_trace = String::new();

        for update in batch {
            let (user_str, reserve_str) = extract_user_and_reserve(&update);

            // ── LiquidationCall: drop user, no HF scan needed ────────────────
            if matches!(update.event_details.event, WhistleblowerEventType::LiquidationCall) {
                if let Some(u) = user_str {
                    if let Ok(user) = Address::from_str(&u) {
                        info!(
                            "[health_factor] LiquidationCall — dropping {user} from cache \
                            (trace={})",
                            update.trace_id
                        );
                        cache.drop_user(user);
                    }
                }
                continue;
            }

            // All remaining events need a valid reserve address
            let Some(reserve_str) = reserve_str else { continue; };
            let reserve = match Address::from_str(&reserve_str) {
                Ok(a) => a,
                Err(_) => continue,
            };

            // ── Borrow / Supply / Repay: re-sync the specific user ───────────
            // ── PriceUpdate: oracle changed — no user to re-sync, just scan ──
            if !matches!(update.event_details.event, WhistleblowerEventType::PriceUpdate) {
                if let Some(u) = user_str {
                    if let Ok(user) = Address::from_str(&u) {
                        cache
                            .refresh_user(user, pool_addr, ui_addr, pap_addr, &provider)
                            .await;
                    }
                }
            }

            // Merge all candidates for this reserve into the shared set.
            let reserve_candidates = cache.users_for_reserve(reserve);
            // lag_blocks is only meaningful for on-chain events (block_number > 0).
            // PriceUpdate events from the offchain trigger always have block_number=0;
            // displaying head_block - 0 ≈ 42M is misleading noise.
            let lag_str = if update.block_number > 0 {
                head_block.saturating_sub(update.block_number).to_string()
            } else {
                "N/A".to_string()
            };
            let queue_ms = if update.enqueued_at_ms > 0 {
                now_ms.saturating_sub(update.enqueued_at_ms)
            } else {
                0
            };
            info!(
                "[health_factor] trace={} block={} event={:?} reserve={reserve} \
                candidates={} (batch_size={batch_size} cache_total={} lag_blocks={} queue_ms={})",
                update.trace_id,
                update.block_number,
                update.event_details.event,
                reserve_candidates.len(),
                cache.len(),
                lag_str,
                queue_ms,
            );
            merged_candidates.extend(reserve_candidates);
            last_trace = update.trace_id;
        }

        // ── Single merged HF check across all batched events ─────────────────
        // Spawned as a background task so this event loop is never blocked.
        // The monitor can keep forwarding events while the RPC scan runs.
        if !merged_candidates.is_empty() {
            let candidates: Vec<Address> = merged_candidates.into_iter().collect();
            let provider_c = provider.clone();
            let alert_tx_c = alert_tx.clone();
            tokio::task::spawn(check_hf_for_users(
                candidates,
                pool_addr,
                provider_c,
                last_trace,
                alert_tx_c,
            ));
        }
    }

    warn!("[health_factor] liq_rx channel closed — exiting");
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Extract `(user_address, reserve_address)` from event args.
///
/// Arg slot layout mirrors the documentation in `types.rs`:
///   LiquidationCall → [collateralAsset, debtAsset, user, ...]
///   Borrow          → [reserve, onBehalfOf]
///   Supply          → [reserve, onBehalfOf]
///   Repay           → [reserve, user]
fn extract_user_and_reserve(update: &LiquidationUpdate) -> (Option<String>, Option<String>) {
    let args = &update.event_details.args;
    match update.event_details.event {
        WhistleblowerEventType::LiquidationCall => {
            (args.get(2).cloned(), args.get(0).cloned()) // user=2, collateralAsset=0
        }
        WhistleblowerEventType::Borrow | WhistleblowerEventType::Supply => {
            (args.get(1).cloned(), args.get(0).cloned()) // onBehalfOf=1, reserve=0
        }
        WhistleblowerEventType::Repay => {
            (args.get(1).cloned(), args.get(0).cloned()) // user=1, reserve=0
        }
        WhistleblowerEventType::PriceUpdate => {
            (None, args.get(0).cloned()) // no specific user; reserve=0
        }
    }
}
