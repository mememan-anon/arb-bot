/// REVM-backed local state database for zero-RPC swap simulation.
///
/// Ported from BaseBuster's state_db/blockstate_db.rs.
/// Stores all tracked pool storage slots in memory. Unknown slots
/// are lazily fetched from HTTP RPC. Supports `Database`, `DatabaseRef`,
/// and `DatabaseCommit` for REVM execution.

use alloy::primitives::{Address, B256, Bytes, U256, keccak256};
use revm::primitives::{AccountInfo, Bytecode, KECCAK_EMPTY};
use revm::{Database, DatabaseCommit};
use std::collections::HashMap;

// ── Storage layout constants ────────────────────────────────────────────────

/// V2 pair: reserves are packed in slot 8 (reserve0:uint112 | reserve1:uint112 | timestamp:uint32)
pub const V2_RESERVES_SLOT: U256 = U256::from_limbs([8, 0, 0, 0]);
/// V2 pair: token0 is in slot 6
pub const V2_TOKEN0_SLOT: U256 = U256::from_limbs([6, 0, 0, 0]);
/// V2 pair: token1 is in slot 7
pub const V2_TOKEN1_SLOT: U256 = U256::from_limbs([7, 0, 0, 0]);

/// V3 pool: slot0 is in storage slot 0 (sqrtPriceX96:uint160 | tick:int24 | ...)
pub const V3_SLOT0: U256 = U256::from_limbs([0, 0, 0, 0]);
/// V3 pool (PancakeSwapV3): slot0 overflows into slot 1 because they use
/// uint32 feeProtocol (vs UniswapV3's uint8), making the packed struct 34 bytes.
pub const V3_SLOT0_OVERFLOW: U256 = U256::from_limbs([1, 0, 0, 0]);
/// V3 pool (UniswapV3/SushiSwapV3): liquidity is in slot 4
pub const V3_LIQUIDITY_SLOT: U256 = U256::from_limbs([4, 0, 0, 0]);
/// V3 pool (PancakeSwapV3): liquidity is in slot 5 (shifted +1 due to slot0 overflow)
pub const V3_LIQUIDITY_SLOT_PANCAKE: U256 = U256::from_limbs([5, 0, 0, 0]);
/// V3 pool: tick spacing in slot 14 (some implementations)
pub const V3_TICK_SPACING_SLOT: U256 = U256::from_limbs([14, 0, 0, 0]);

// ── Slot derivation helpers ─────────────────────────────────────────────────

/// Compute the storage slot for `ticks[tick]` in a V3 pool.
/// UniswapV3: ticks mapping at position 5.  PancakeSwapV3: position 6.
pub fn v3_tick_slot(tick: i32) -> U256 {
    v3_tick_slot_base(tick, 5)
}

/// Compute the tick slot with an explicit mapping base position.
pub fn v3_tick_slot_base(tick: i32, base: u64) -> U256 {
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(&i256_bytes(tick));
    buf[32..64].copy_from_slice(&U256::from(base).to_be_bytes::<32>());
    U256::from_be_bytes(keccak256(buf).0)
}

/// Compute the storage slot for `tickBitmap[wordPos]` in a V3 pool.
/// UniswapV3: tickBitmap at position 6. PancakeSwapV3: position 7.
pub fn v3_tick_bitmap_slot(word_pos: i16) -> U256 {
    v3_tick_bitmap_slot_base(word_pos, 6)
}

/// Compute the bitmap slot with an explicit mapping base position.
pub fn v3_tick_bitmap_slot_base(word_pos: i16, base: u64) -> U256 {
    let mut buf = [0u8; 64];
    let extended = word_pos as i32;
    buf[..32].copy_from_slice(&i256_bytes(extended));
    buf[32..64].copy_from_slice(&U256::from(base).to_be_bytes::<32>());
    U256::from_be_bytes(keccak256(buf).0)
}

/// Helper: convert a signed integer to 32-byte big-endian (two's complement).
fn i256_bytes(v: i32) -> [u8; 32] {
    let mut bytes = [0u8; 32];
    if v >= 0 {
        let b = v.to_be_bytes();
        bytes[28..32].copy_from_slice(&b);
    } else {
        bytes.fill(0xff);
        let b = v.to_be_bytes();
        bytes[28..32].copy_from_slice(&b);
    }
    bytes
}

#[allow(dead_code)]
fn tick_i256_bytes(tick: i32) -> [u8; 32] {
    i256_bytes(tick)
}

// ── Account and slot tracking ───────────────────────────────────────────────

/// Tracks whether a slot was loaded from chain or injected by the bot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertionType {
    OnChain,
    Custom,
}

/// A single storage slot with its value and provenance.
#[derive(Debug, Clone)]
pub struct BlockStateSlot {
    pub value: U256,
    pub insertion_type: InsertionType,
}

/// Account state within our local database.
#[derive(Debug, Clone)]
pub struct BlockStateAccount {
    pub info: AccountInfo,
    pub storage: HashMap<U256, BlockStateSlot>,
}

// ── BlockStateDB ────────────────────────────────────────────────────────────

/// In-memory REVM database that mirrors on-chain state for tracked pools.
/// Unknown state is fetched lazily from the HTTP provider.
pub struct BlockStateDB {
    /// Account states keyed by address.
    pub accounts: HashMap<Address, BlockStateAccount>,
    /// HTTP RPC endpoint for fetching unknown state lazily.
    pub rpc_url: String,
    /// reqwest client for lazy fetches.
    pub client: reqwest::Client,
    /// Block number for state queries.
    pub block_number: u64,
    /// Set of tracked pool addresses (only these get state updates).
    pub tracked_pools: std::collections::HashSet<Address>,
    /// PancakeSwapV3 pools — use shifted tick/bitmap slot positions (6/7 instead of 5/6).
    pub pancake_v3_pools: std::collections::HashSet<Address>,
}

impl BlockStateDB {
    pub fn new(rpc_url: String, block_number: u64) -> Self {
        Self {
            accounts: HashMap::new(),
            rpc_url,
            client: reqwest::Client::new(),
            block_number,
            tracked_pools: std::collections::HashSet::new(),
            pancake_v3_pools: std::collections::HashSet::new(),
        }
    }

    /// Register a pool address for state tracking.
    pub fn track_pool(&mut self, pool: Address) {
        self.tracked_pools.insert(pool);
    }

    /// Pre-fetch contract bytecodes for all tracked pools.
    ///
    /// Runs at startup so that REVM simulation doesn't need lazy RPC
    /// fetches during hot-path execution. Uses concurrent HTTP requests
    /// with a semaphore to avoid overwhelming the RPC.
    pub fn prefetch_pool_codes(&mut self) {
        use futures::stream::{FuturesUnordered, StreamExt};
        use std::sync::Arc;

        // Collect addresses that need code fetched
        let needs_code: Vec<Address> = self
            .tracked_pools
            .iter()
            .filter(|addr| {
                match self.accounts.get(*addr) {
                    Some(acct) => acct.info.code.is_none(),
                    None => true,
                }
            })
            .copied()
            .collect();

        if needs_code.is_empty() {
            return;
        }

        log::info!(
            "[StateDB] Prefetching bytecodes for {} tracked pools...",
            needs_code.len()
        );

        let url = self.rpc_url.clone();
        let client = self.client.clone();
        let semaphore = Arc::new(tokio::sync::Semaphore::new(32)); // concurrency limit

        let results: Vec<(Address, Vec<u8>)> = tokio::task::block_in_place(|| {
            let rt = tokio::runtime::Handle::current();
            rt.block_on(async {
                let mut futs = FuturesUnordered::new();
                for addr in needs_code {
                    let url = url.clone();
                    let client = client.clone();
                    let sem = semaphore.clone();
                    futs.push(async move {
                        let _permit = sem.acquire().await.unwrap();
                        let addr_hex = format!("0x{}", hex::encode(addr.as_slice()));
                        let body = serde_json::json!({
                            "jsonrpc": "2.0",
                            "method": "eth_getCode",
                            "params": [&addr_hex, "latest"],
                            "id": 1
                        });
                        let code = match client.post(&url).json(&body).send().await {
                            Ok(resp) => {
                                if let Ok(json) = resp.json::<serde_json::Value>().await {
                                    if let Some(s) = json.get("result").and_then(|v| v.as_str()) {
                                        let s = s.trim_start_matches("0x");
                                        hex::decode(s).unwrap_or_default()
                                    } else { vec![] }
                                } else { vec![] }
                            }
                            Err(_) => vec![],
                        };
                        (addr, code)
                    });
                }
                let mut results = Vec::new();
                while let Some(result) = futs.next().await {
                    results.push(result);
                }
                results
            })
        });

        let mut loaded = 0usize;
        for (addr, code) in results {
            if code.is_empty() {
                continue;
            }
            let code_hash = keccak256(&code);
            let bytecode = Bytecode::new_raw(Bytes::from(code));
            let acct = self.accounts.entry(addr).or_insert_with(|| BlockStateAccount {
                info: AccountInfo::default(),
                storage: HashMap::new(),
            });
            acct.info.code_hash = code_hash;
            acct.info.code = Some(bytecode);
            loaded += 1;
        }

        log::info!(
            "[StateDB] Prefetched bytecodes: {}/{} pools now have code",
            loaded,
            self.tracked_pools.len()
        );
    }

    /// Deploy stub ERC20 bytecodes at all token addresses used by tracked pools.
    ///
    /// V3 pool swap() calls `token.transfer()` and `token.balanceOf()` internally.
    /// Without proper token bytecodes, these calls fail, causing the try-catch
    /// quoter to fall back to the less accurate analytical `_quoteV3Math`.
    ///
    /// Instead of loading REAL ERC20 bytecodes (which would fail on missing
    /// balance storage slots), we deploy a minimal 10-byte stub contract that
    /// returns uint256(1) for ANY function call:
    ///   - balanceOf(addr) → 1  (nonzero, so balance checks pass)
    ///   - transfer(to, amt) → true (boolean 1)
    ///   - transferFrom(f,t,a) → true
    ///   - approve(addr, amt) → true
    ///
    /// This lets V3 pool.swap() execute its full tick math, reach the swap
    /// callback (which reverts with the real output amount), and give us
    /// exact-match quotes without needing any ERC20 storage state.
    ///
    /// V2 quotes are unaffected — they only call pool.getReserves() (pool's
    /// own storage), never token contracts.
    pub fn deploy_token_stubs(&mut self, token_addresses: &[Address]) {
        // Minimal EVM bytecode: returns uint256(1) for any call.
        //   PUSH1 0x01  (60 01)   -- value = 1
        //   PUSH1 0x00  (60 00)   -- memory offset = 0
        //   MSTORE      (52)      -- mem[0:32] = 1
        //   PUSH1 0x20  (60 20)   -- return size = 32
        //   PUSH1 0x00  (60 00)   -- return offset = 0
        //   RETURN      (f3)      -- return 32 bytes
        let stub_bytecode: Vec<u8> = vec![0x60, 0x01, 0x60, 0x00, 0x52, 0x60, 0x20, 0x60, 0x00, 0xf3];
        let stub_hash = keccak256(&stub_bytecode);
        let stub = Bytecode::new_raw(Bytes::from(stub_bytecode));

        let mut deployed = 0usize;
        for &addr in token_addresses {
            // Skip addresses that already have code (e.g., pool contracts that
            // are also tokens, or tokens loaded from a prior step).
            if let Some(acct) = self.accounts.get(&addr) {
                if acct.info.code.is_some() {
                    continue;
                }
            }
            let acct = self.accounts.entry(addr).or_insert_with(|| BlockStateAccount {
                info: AccountInfo::default(),
                storage: HashMap::new(),
            });
            acct.info.code_hash = stub_hash;
            acct.info.code = Some(stub.clone());
            deployed += 1;
        }

        log::info!(
            "[StateDB] Deployed ERC20 stubs at {}/{} token addresses",
            deployed,
            token_addresses.len()
        );
    }

    /// Pre-fetch token0 (slot 6) and token1 (slot 7) for all tracked pools.
    ///
    /// These are immutable in V2 pools (storage) and V3 pools (immutable in code,
    /// but we fetch the storage slots anyway as a safety measure). Without this,
    /// every REVM quoter call lazy-loads these from RPC, causing massive delays.
    pub fn prefetch_token_slots(&mut self) {
        use futures::stream::{FuturesUnordered, StreamExt};
        use std::sync::Arc;

        // Collect (address, slot) pairs that need fetching
        let mut to_fetch: Vec<(Address, U256)> = Vec::new();
        for &addr in &self.tracked_pools {
            // Token0 = slot 6, Token1 = slot 7
            for slot in [V2_TOKEN0_SLOT, V2_TOKEN1_SLOT] {
                if let Some(acct) = self.accounts.get(&addr) {
                    if acct.storage.contains_key(&slot) {
                        continue; // already cached
                    }
                }
                to_fetch.push((addr, slot));
            }
        }

        if to_fetch.is_empty() {
            return;
        }

        log::info!(
            "[StateDB] Prefetching {} token0/token1 storage slots...",
            to_fetch.len()
        );

        let url = self.rpc_url.clone();
        let client = self.client.clone();
        let semaphore = Arc::new(tokio::sync::Semaphore::new(64));

        let results: Vec<(Address, U256, U256)> = tokio::task::block_in_place(|| {
            let rt = tokio::runtime::Handle::current();
            rt.block_on(async {
                let mut futs = FuturesUnordered::new();
                for (addr, slot) in to_fetch {
                    let url = url.clone();
                    let client = client.clone();
                    let sem = semaphore.clone();
                    futs.push(async move {
                        let _permit = sem.acquire().await.unwrap();
                        let addr_hex = format!("0x{}", hex::encode(addr.as_slice()));
                        let slot_hex = format!("0x{:0>64x}", slot);
                        let body = serde_json::json!({
                            "jsonrpc": "2.0",
                            "method": "eth_getStorageAt",
                            "params": [&addr_hex, &slot_hex, "latest"],
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
                        (addr, slot, value)
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
        for (addr, slot, value) in results {
            if !value.is_zero() {
                self.update_slot(addr, slot, value);
                stored += 1;
            }
        }

        log::info!("[StateDB] Prefetched token slots: {} non-zero values stored", stored);
    }

    /// Batch-prefetch V3-critical storage: slot0 (sqrtPriceX96, tick, ...) and liquidity
    /// for every tracked pool. These are read at the start of every V3 swap() call.
    ///
    /// Covers both UniswapV3 (slot0=0, liquidity=4) and PancakeSwapV3 (slot0=0+1,
    /// liquidity=5) layouts. PancakeV3 uses uint32 feeProtocol in its Slot0 struct
    /// which overflows into a second storage slot, shifting liquidity from slot 4 → 5.
    pub fn prefetch_v3_slots(&mut self) {
        use futures::stream::{FuturesUnordered, StreamExt};
        use std::sync::Arc;

        let mut to_fetch: Vec<(Address, U256)> = Vec::new();
        for &addr in &self.tracked_pools {
            for slot in [V3_SLOT0, V3_SLOT0_OVERFLOW, V3_LIQUIDITY_SLOT, V3_LIQUIDITY_SLOT_PANCAKE] {
                if let Some(acct) = self.accounts.get(&addr) {
                    if acct.storage.contains_key(&slot) {
                        continue;
                    }
                }
                to_fetch.push((addr, slot));
            }
        }

        if to_fetch.is_empty() {
            log::info!("[StateDB] V3 slots already cached, nothing to prefetch");
            return;
        }

        log::info!("[StateDB] Prefetching {} V3 slot0/liquidity storage slots...", to_fetch.len());

        let url = self.rpc_url.clone();
        let client = self.client.clone();
        let semaphore = Arc::new(tokio::sync::Semaphore::new(64));

        let results: Vec<(Address, U256, U256)> = tokio::task::block_in_place(|| {
            let rt = tokio::runtime::Handle::current();
            rt.block_on(async {
                let mut futs = FuturesUnordered::new();
                for (addr, slot) in to_fetch {
                    let url = url.clone();
                    let client = client.clone();
                    let sem = semaphore.clone();
                    futs.push(async move {
                        let _permit = sem.acquire().await.unwrap();
                        let addr_hex = format!("0x{}", hex::encode(addr.as_slice()));
                        let slot_hex = format!("0x{:0>64x}", slot);
                        let body = serde_json::json!({
                            "jsonrpc": "2.0",
                            "method": "eth_getStorageAt",
                            "params": [&addr_hex, &slot_hex, "latest"],
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
                        (addr, slot, value)
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
        for (addr, slot, value) in results {
            // slot0 can legitimately be non-zero even for V2 pools (it would just be
            // totalSupply or similar). We store it regardless — REVM will read whatever
            // the on-chain value is.
            self.update_slot(addr, slot, value);
            stored += 1;
        }

        // Diagnostic: count how many pools have non-zero liquidity (either slot 4 or 5)
        let mut nonzero_liq = 0usize;
        let mut nonzero_slot0 = 0usize;
        for &addr in &self.tracked_pools {
            if let Some(acct) = self.accounts.get(&addr) {
                let has_liq = acct.storage.get(&V3_LIQUIDITY_SLOT).map_or(false, |s| !s.value.is_zero())
                    || acct.storage.get(&V3_LIQUIDITY_SLOT_PANCAKE).map_or(false, |s| !s.value.is_zero());
                if has_liq { nonzero_liq += 1; }
                if let Some(s) = acct.storage.get(&V3_SLOT0) {
                    if !s.value.is_zero() { nonzero_slot0 += 1; }
                }
            }
        }
        log::info!(
            "[StateDB] Prefetched V3 slots: {}/{} stored (non-zero liquidity@4or5={}, non-zero slot0={})",
            stored, stored, nonzero_liq, nonzero_slot0
        );
    }

    /// Batch-prefetch V3 tick bitmap words and nearby tick data for all tracked V3 pools.
    ///
    /// Uses the current tick from slot0 and the tick_spacing from pool metadata to
    /// determine which bitmap words and ticks to fetch.
    ///
    /// This enables the V3 pool contracts to execute swap() correctly inside REVM
    /// without needing lazy RPC fallback for tick data (the try-catch quoter path).
    ///
    /// For each V3 pool:
    ///  1. Read current tick from prefetched slot0
    ///  2. Compute tick bitmap word positions near current tick
    ///  3. Fetch bitmap words ± TICK_BITMAP_RANGE around current word
    ///  4. Parse initialized ticks from bitmaps and fetch their tick info
    pub fn prefetch_v3_tick_data(&mut self, v3_pools: &[(Address, i32, bool)]) {
        use futures::stream::{FuturesUnordered, StreamExt};
        use std::sync::Arc;

        const TICK_BITMAP_RANGE: i16 = 4; // fetch ±4 bitmap words (covers ~2048 ticks per side)

        // Register PancakeSwapV3 pools so read functions use correct slot positions.
        for &(pool, _, is_pancake) in v3_pools {
            if is_pancake {
                self.pancake_v3_pools.insert(pool);
            }
        }

        let mut bm_to_fetch: Vec<(Address, i16, U256)> = Vec::new();

        for &(pool, tick_spacing, is_pancake) in v3_pools {
            if tick_spacing <= 0 { continue; }

            // Read current tick from already-prefetched slot0
            let current_tick = match self.read_v3_slot0(&pool) {
                Some((_, tick)) => tick,
                None => continue,
            };

            let bm_base: u64 = if is_pancake { 7 } else { 6 };

            // Compute the word position for the current compressed tick
            let compressed = current_tick / tick_spacing;
            let current_word = (compressed >> 8) as i16;

            // Fetch bitmap words in range [current_word - RANGE, current_word + RANGE]
            for offset in -TICK_BITMAP_RANGE..=TICK_BITMAP_RANGE {
                let word_pos = current_word.saturating_add(offset);
                let bm_slot = v3_tick_bitmap_slot_base(word_pos, bm_base);
                // Skip if already cached
                if let Some(acct) = self.accounts.get(&pool) {
                    if acct.storage.contains_key(&bm_slot) {
                        continue;
                    }
                }
                bm_to_fetch.push((pool, word_pos, bm_slot));
            }
        }

        if bm_to_fetch.is_empty() {
            log::info!("[StateDB] V3 tick bitmaps already cached, nothing to prefetch");
            return;
        }

        log::info!("[StateDB] Prefetching {} V3 tick bitmap slots...", bm_to_fetch.len());

        let url = self.rpc_url.clone();
        let client = self.client.clone();
        let semaphore = Arc::new(tokio::sync::Semaphore::new(64));

        // Phase 1: Fetch tick bitmap words
        let bm_results: Vec<(Address, i16, U256, U256)> = tokio::task::block_in_place(|| {
            let rt = tokio::runtime::Handle::current();
            rt.block_on(async {
                let mut futs = FuturesUnordered::new();
                for (addr, word_pos, bm_slot) in &bm_to_fetch {
                    let url = url.clone();
                    let client = client.clone();
                    let sem = semaphore.clone();
                    let addr = *addr;
                    let word_pos = *word_pos;
                    let bm_slot = *bm_slot;
                    futs.push(async move {
                        let _permit = sem.acquire().await.unwrap();
                        let addr_hex = format!("0x{}", hex::encode(addr.as_slice()));
                        let slot_hex = format!("0x{:0>64x}", bm_slot);
                        let body = serde_json::json!({
                            "jsonrpc": "2.0",
                            "method": "eth_getStorageAt",
                            "params": [&addr_hex, &slot_hex, "latest"],
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
                        (addr, word_pos, bm_slot, value)
                    });
                }
                let mut results = Vec::new();
                while let Some(result) = futs.next().await {
                    results.push(result);
                }
                results
            })
        });

        let mut bm_stored = 0usize;
        for &(addr, _, bm_slot, value) in &bm_results {
            self.update_slot(addr, bm_slot, value);
            bm_stored += 1;
        }
        log::info!("[StateDB] Stored {} tick bitmap slots", bm_stored);

        // Phase 2: Parse bitmaps to find initialized ticks, then fetch their tick info.
        // Create a map from pool -> tick_spacing for lookup.
        let pool_tick_spacing: HashMap<Address, i32> = v3_pools.iter().map(|&(a, ts, _)| (a, ts)).collect();

        let mut tick_slots_to_fetch: Vec<(Address, i32, U256)> = Vec::new();
        for &(pool, word_pos, _, bitmap_value) in &bm_results {
            if bitmap_value.is_zero() { continue; }
            let tick_spacing = match pool_tick_spacing.get(&pool) {
                Some(&ts) => ts,
                None => continue,
            };

            let tick_base: u64 = if self.pancake_v3_pools.contains(&pool) { 6 } else { 5 };

            // Each bit in the bitmap corresponds to a compressed tick:
            //   tick = (word_pos * 256 + bit_pos) * tick_spacing
            // Only fetch ticks where the bit is set (initialized ticks).
            let bitmap_u256 = bitmap_value;
            for bit_pos in 0..256u32 {
                if !bitmap_u256.bit(bit_pos as usize) { continue; }
                let compressed_tick = (word_pos as i32) * 256 + bit_pos as i32;
                let actual_tick = compressed_tick * tick_spacing;
                let tick_slot = v3_tick_slot_base(actual_tick, tick_base);
                // Skip if already cached
                if let Some(acct) = self.accounts.get(&pool) {
                    if acct.storage.contains_key(&tick_slot) {
                        continue;
                    }
                }
                tick_slots_to_fetch.push((pool, actual_tick, tick_slot));
            }
        }

        if tick_slots_to_fetch.is_empty() {
            log::info!("[StateDB] No initialized ticks to prefetch");
            return;
        }

        log::info!("[StateDB] Prefetching {} initialized tick slots...", tick_slots_to_fetch.len());

        let tick_results: Vec<(Address, U256, U256)> = tokio::task::block_in_place(|| {
            let rt = tokio::runtime::Handle::current();
            rt.block_on(async {
                let mut futs = FuturesUnordered::new();
                for (addr, _tick, tick_slot) in &tick_slots_to_fetch {
                    let url = url.clone();
                    let client = client.clone();
                    let sem = semaphore.clone();
                    let addr = *addr;
                    let tick_slot = *tick_slot;
                    futs.push(async move {
                        let _permit = sem.acquire().await.unwrap();
                        let addr_hex = format!("0x{}", hex::encode(addr.as_slice()));
                        let slot_hex = format!("0x{:0>64x}", tick_slot);
                        let body = serde_json::json!({
                            "jsonrpc": "2.0",
                            "method": "eth_getStorageAt",
                            "params": [&addr_hex, &slot_hex, "latest"],
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
                        (addr, tick_slot, value)
                    });
                }
                let mut results = Vec::new();
                while let Some(result) = futs.next().await {
                    results.push(result);
                }
                results
            })
        });

        let mut tick_stored = 0usize;
        for (addr, tick_slot, value) in tick_results {
            self.update_slot(addr, tick_slot, value);
            tick_stored += 1;
        }

        log::info!("[StateDB] Prefetched V3 tick data: {} bitmap slots, {} tick info slots", bm_stored, tick_stored);
    }

    /// Fetch ONLY the bytecode for an address via eth_getCode (1 RPC call).
    /// Works both within tokio context and from rayon worker threads.
    fn fetch_code_blocking(&self, address: Address) -> Vec<u8> {
        let url = self.rpc_url.clone();
        let client = self.client.clone();
        let block = self.block_number;
        let addr_hex = format!("0x{}", hex::encode(address.as_slice()));

        let fetch_async = async {
            let block_tag = if block == 0 {
                "latest".to_string()
            } else {
                format!("0x{:x}", block)
            };
            let body = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "eth_getCode",
                "params": [&addr_hex, &block_tag],
                "id": 1
            });
            match client.post(&url).json(&body).send().await {
                Ok(resp) => {
                    if let Ok(json) = resp.json::<serde_json::Value>().await {
                        if let Some(s) = json.get("result").and_then(|v| v.as_str()) {
                            let s = s.trim_start_matches("0x");
                            hex::decode(s).unwrap_or_default()
                        } else { vec![] }
                    } else { vec![] }
                }
                Err(_) => vec![],
            }
        };

        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            tokio::task::block_in_place(|| handle.block_on(fetch_async))
        } else {
            tokio::runtime::Runtime::new()
                .expect("failed to create runtime")
                .block_on(fetch_async)
        }
    }

    /// Update a specific storage slot for an account.
    pub fn update_slot(&mut self, address: Address, slot: U256, value: U256) {
        let account = self.accounts.entry(address).or_insert_with(|| BlockStateAccount {
            info: AccountInfo::default(),
            storage: HashMap::new(),
        });
        account.storage.insert(slot, BlockStateSlot {
            value,
            insertion_type: InsertionType::OnChain,
        });
    }

    /// Inject custom state (e.g., deploying quoter bytecode).
    pub fn inject_account(&mut self, address: Address, info: AccountInfo) {
        match self.accounts.entry(address) {
            std::collections::hash_map::Entry::Occupied(mut e) => {
                e.get_mut().info = info;
            }
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(BlockStateAccount {
                    info,
                    storage: HashMap::new(),
                });
            }
        }
    }

    /// Inject custom storage slot.
    pub fn inject_slot(&mut self, address: Address, slot: U256, value: U256) {
        if !self.accounts.contains_key(&address) {
            let info = self.fetch_account_blocking(address);
            self.accounts.insert(address, BlockStateAccount {
                info,
                storage: HashMap::new(),
            });
        }
        let account = self.accounts.get_mut(&address).expect("account inserted");
        account.storage.insert(slot, BlockStateSlot {
            value,
            insertion_type: InsertionType::Custom,
        });
    }

    /// Read V2 reserves from local state (no RPC).
    /// Returns (reserve0, reserve1) or None if slot not loaded.
    pub fn read_v2_reserves(&self, pool: &Address) -> Option<(U256, U256)> {
        let account = self.accounts.get(pool)?;
        let slot = account.storage.get(&V2_RESERVES_SLOT)?;
        let packed = slot.value;
        // reserve0 = lower 112 bits, reserve1 = next 112 bits
        let mask_112 = (U256::from(1u64) << 112) - U256::from(1u64);
        let reserve0 = packed & mask_112;
        let reserve1 = (packed >> 112) & mask_112;
        Some((reserve0, reserve1))
    }

    /// Read V3 slot0 from local state.
    /// Returns (sqrtPriceX96, tick) or None if not loaded.
    pub fn read_v3_slot0(&self, pool: &Address) -> Option<(U256, i32)> {
        let account = self.accounts.get(pool)?;
        let slot = account.storage.get(&V3_SLOT0)?;
        let packed = slot.value;
        // sqrtPriceX96 = lower 160 bits
        let mask_160 = (U256::from(1u64) << 160) - U256::from(1u64);
        let sqrt_price_x96 = packed & mask_160;
        // tick = next 24 bits (signed)
        let tick_u256: U256 = (packed >> 160) & U256::from(0xFFFFFFu64);
        let tick_raw = tick_u256.to::<u32>();
        let tick = if tick_raw & 0x800000 != 0 {
            (tick_raw | 0xFF000000) as i32  // sign-extend
        } else {
            tick_raw as i32
        };
        Some((sqrt_price_x96, tick))
    }

    /// Read V3 liquidity from local state.
    /// Checks both UniswapV3 layout (slot 4) and PancakeSwapV3 layout (slot 5).
    pub fn read_v3_liquidity(&self, pool: &Address) -> Option<u128> {
        let account = self.accounts.get(pool)?;
        // Mask to low 128 bits — liquidity is uint128 but some pool variants
        // pack additional data in the upper 128 bits of the same storage word.
        let mask = U256::from(u128::MAX);
        // Try UniswapV3 slot (4) first, then PancakeSwapV3 slot (5)
        for slot_key in [&V3_LIQUIDITY_SLOT, &V3_LIQUIDITY_SLOT_PANCAKE] {
            if let Some(slot) = account.storage.get(slot_key) {
                let val = (slot.value & mask).to::<u128>();
                if val > 0 {
                    return Some(val);
                }
            }
        }
        // Both slots are 0 (or missing) — return 0 from whichever exists
        if let Some(slot) = account.storage.get(&V3_LIQUIDITY_SLOT) {
            return Some((slot.value & mask).to::<u128>());
        }
        if let Some(slot) = account.storage.get(&V3_LIQUIDITY_SLOT_PANCAKE) {
            return Some((slot.value & mask).to::<u128>());
        }
        None
    }

    /// Read V3 tick data from local state.\n    /// Automatically uses the correct storage base (5 or 6) depending on pool type.
    pub fn read_v3_tick(&self, pool: &Address, tick: i32) -> Option<(u128, i128)> {
        let base: u64 = if self.pancake_v3_pools.contains(pool) { 6 } else { 5 };
        let tick_slot = v3_tick_slot_base(tick, base);
        let account = self.accounts.get(pool)?;
        let slot = account.storage.get(&tick_slot)?;
        let packed = slot.value;
        // liquidityGross = lower 128 bits
        let mask_128 = (U256::from(1u64) << 128) - U256::from(1u64);
        let lg_u256: U256 = packed & mask_128;
        let liquidity_gross = lg_u256.to::<u128>();
        // liquidityNet = next 128 bits (signed)
        let ln_u256: U256 = (packed >> 128) & mask_128;
        let liquidity_net_raw = ln_u256.to::<u128>();
        let liquidity_net = liquidity_net_raw as i128;
        Some((liquidity_gross, liquidity_net))
    }

    /// Read V3 tick bitmap word.\n    /// Automatically uses the correct storage base (6 or 7) depending on pool type.
    pub fn read_v3_tick_bitmap(&self, pool: &Address, word_pos: i16) -> Option<U256> {
        let base: u64 = if self.pancake_v3_pools.contains(pool) { 7 } else { 6 };
        let bm_slot = v3_tick_bitmap_slot_base(word_pos, base);
        let account = self.accounts.get(pool)?;
        let slot = account.storage.get(&bm_slot)?;
        Some(slot.value)
    }

    /// Lazy fetch a storage slot from RPC (blocking).
    /// Works both within tokio context and from rayon worker threads.
    fn fetch_storage_blocking(&self, address: Address, slot: U256) -> U256 {
        let url = self.rpc_url.clone();
        let client = self.client.clone();
        let block = self.block_number;

        let fetch_async = async {
            let slot_hex = format!("0x{:064x}", slot);
            let block_tag = if block == 0 {
                "latest".to_string()
            } else {
                format!("0x{:x}", block)
            };
            let body = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "eth_getStorageAt",
                "params": [format!("0x{}", hex::encode(address.as_slice())), slot_hex, block_tag],
                "id": 1
            });
            match client.post(&url).json(&body).send().await {
                Ok(resp) => {
                    if let Ok(json) = resp.json::<serde_json::Value>().await {
                        if let Some(result) = json.get("result").and_then(|v| v.as_str()) {
                            let result = result.trim_start_matches("0x");
                            if let Ok(bytes) = hex::decode(format!("{:0>64}", result)) {
                                let mut arr = [0u8; 32];
                                arr.copy_from_slice(&bytes);
                                return U256::from_be_bytes(arr);
                            }
                        }
                    }
                    U256::ZERO
                }
                Err(_) => U256::ZERO,
            }
        };

        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            tokio::task::block_in_place(|| handle.block_on(fetch_async))
        } else {
            tokio::runtime::Runtime::new()
                .expect("failed to create runtime")
                .block_on(fetch_async)
        }
    }

    /// Lazy fetch account info from RPC (blocking).
    /// Works both within tokio context and from rayon worker threads.
    fn fetch_account_blocking(&self, address: Address) -> AccountInfo {
        let url = self.rpc_url.clone();
        let client = self.client.clone();
        let block = self.block_number;
        let addr_hex = format!("0x{}", hex::encode(address.as_slice()));

        // Try to get current tokio runtime handle; if not available (e.g., rayon thread),
        // create a temporary runtime for this fetch.
        let fetch_async = async {
            // Fetch balance
            let block_tag = if block == 0 {
                "latest".to_string()
            } else {
                format!("0x{:x}", block)
            };

            let balance_body = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "eth_getBalance",
                "params": [&addr_hex, &block_tag],
                "id": 1
            });
            let balance = match client.post(&url).json(&balance_body).send().await {
                Ok(resp) => {
                    if let Ok(json) = resp.json::<serde_json::Value>().await {
                        if let Some(s) = json.get("result").and_then(|v| v.as_str()) {
                            let s = s.trim_start_matches("0x");
                            U256::from_str_radix(s, 16).unwrap_or(U256::ZERO)
                        } else { U256::ZERO }
                    } else { U256::ZERO }
                }
                Err(_) => U256::ZERO,
            };

            // Fetch nonce
            let nonce_body = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "eth_getTransactionCount",
                "params": [&addr_hex, &block_tag],
                "id": 2
            });
            let nonce = match client.post(&url).json(&nonce_body).send().await {
                Ok(resp) => {
                    if let Ok(json) = resp.json::<serde_json::Value>().await {
                        if let Some(s) = json.get("result").and_then(|v| v.as_str()) {
                            let s = s.trim_start_matches("0x");
                            u64::from_str_radix(s, 16).unwrap_or(0)
                        } else { 0 }
                    } else { 0 }
                }
                Err(_) => 0,
            };

            // Fetch code
            let code_body = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "eth_getCode",
                "params": [&addr_hex, &block_tag],
                "id": 3
            });
            let code = match client.post(&url).json(&code_body).send().await {
                Ok(resp) => {
                    if let Ok(json) = resp.json::<serde_json::Value>().await {
                        if let Some(s) = json.get("result").and_then(|v| v.as_str()) {
                            let s = s.trim_start_matches("0x");
                            hex::decode(s).unwrap_or_default()
                        } else { vec![] }
                    } else { vec![] }
                }
                Err(_) => vec![],
            };

            let code_hash = if code.is_empty() {
                revm::primitives::KECCAK_EMPTY
            } else {
                keccak256(&code)
            };

            AccountInfo {
                balance,
                nonce,
                code_hash,
                code: if code.is_empty() {
                    None
                } else {
                    Some(Bytecode::new_raw(Bytes::from(code)))
                },
            }
        };

        // Try existing runtime first (most common case: main tokio thread)
        // Fall back to creating a temporary runtime for rayon worker threads
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            // In tokio context - use block_in_place
            tokio::task::block_in_place(|| handle.block_on(fetch_async))
        } else {
            // Not in tokio context (e.g. rayon thread) - create temp runtime
            tokio::runtime::Runtime::new()
                .expect("failed to create runtime")
                .block_on(fetch_async)
        }
    }
}

// ── REVM Database implementation ────────────────────────────────────────────

impl Database for BlockStateDB {
    type Error = String;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        if let Some(account) = self.accounts.get(&address) {
            // If the account was inserted by update_slot() with default
            // AccountInfo (no bytecode), lazy-fetch ONLY the code (1 RPC call)
            // so pool contracts don't appear as EOAs during REVM execution.
            if account.info.code.is_none() && account.info.code_hash == KECCAK_EMPTY {
                let code = self.fetch_code_blocking(address);
                if !code.is_empty() {
                    let acct = self.accounts.get_mut(&address).unwrap();
                    acct.info.code_hash = keccak256(&code);
                    acct.info.code = Some(Bytecode::new_raw(Bytes::from(code)));
                    Ok(Some(acct.info.clone()))
                } else {
                    // Truly an EOA — return as-is
                    Ok(Some(account.info.clone()))
                }
            } else {
                Ok(Some(account.info.clone()))
            }
        } else {
            let info = self.fetch_account_blocking(address);
            self.accounts.insert(address, BlockStateAccount {
                info: info.clone(),
                storage: HashMap::new(),
            });
            Ok(Some(info))
        }
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        if code_hash == B256::ZERO || code_hash == KECCAK_EMPTY {
            return Ok(Bytecode::default());
        }

        for account in self.accounts.values() {
            if account.info.code_hash == code_hash {
                if let Some(code) = &account.info.code {
                    return Ok(code.clone());
                }
                break;
            }
        }

        Ok(Bytecode::default())
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        // Check local cache first
        if let Some(account) = self.accounts.get(&address) {
            if let Some(slot) = account.storage.get(&index) {
                return Ok(slot.value);
            }
        }
        // Lazy fetch from RPC
        let value = self.fetch_storage_blocking(address, index);
        self.update_slot(address, index, value);
        Ok(value)
    }

    fn block_hash(&mut self, _number: u64) -> Result<B256, Self::Error> {
        // Not critical for swap simulation
        Ok(B256::ZERO)
    }
}

// ── Batch state operations ──────────────────────────────────────────────────

// ── REVM DatabaseCommit implementation ──────────────────────────────────────

impl DatabaseCommit for BlockStateDB {
    /// Apply REVM execution result state changes back into the in-memory DB.
    ///
    /// Called automatically by `Evm::transact_commit()`. Persists modified
    /// account infos and changed storage slots so that subsequent EVM calls
    /// (e.g. approve() followed by swap()) see the updated approvals and
    /// balances.
    fn commit(&mut self, changes: revm::primitives::HashMap<Address, revm::primitives::Account>) {
        for (address, account) in changes {
            if account.is_empty() {
                continue;
            }
            let entry = self.accounts.entry(address).or_insert_with(|| BlockStateAccount {
                info: AccountInfo::default(),
                storage: HashMap::new(),
            });
            entry.info = account.info;
            for (slot, storage_slot) in account.storage {
                if storage_slot.is_changed() {
                    entry.storage.insert(
                        slot,
                        BlockStateSlot {
                            value: storage_slot.present_value(),
                            insertion_type: InsertionType::Custom,
                        },
                    );
                }
            }
        }
    }
}

impl BlockStateDB {
    /// Apply diff-mode trace results: update all changed slots for tracked pools.
    pub fn apply_state_diffs(&mut self, diffs: &HashMap<Address, HashMap<U256, U256>>) {
        for (address, slots) in diffs {
            if !self.tracked_pools.contains(address) {
                continue;
            }
            for (slot, value) in slots {
                self.update_slot(*address, *slot, *value);
            }
        }
    }

    /// Set the block number for subsequent lazy fetches.
    pub fn set_block(&mut self, block_number: u64) {
        self.block_number = block_number;
    }
}
