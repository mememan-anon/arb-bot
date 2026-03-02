/// Lightweight simulation-only database for REVM quoter calls.
///
/// Created per `quote_path` invocation: snapshots all cached data from
/// the shared `BlockStateDB` (read-lock), then runs REVM without holding
/// any lock. Cache misses are fetched directly via RPC.
///
/// This eliminates the write-lock contention that was blocking the entire
/// pipeline when V3 swaps triggered lazy storage fetches.

use alloy::primitives::{Address, B256, Bytes, U256, keccak256};
use revm::primitives::{AccountInfo, Bytecode, KECCAK_EMPTY};
use revm::Database;
use std::collections::HashMap;

use crate::state_db::BlockStateDB;

/// Per-simulation database — no locks, no shared state.
pub struct SimDb {
    accounts: HashMap<Address, SimAccount>,
    rpc_url: String,
    client: reqwest::Client,
    /// When true, cache misses return defaults instead of RPC calls.
    /// Makes simulation ~1000x faster for V3 paths (no tick-data RPC).
    /// Swaps that don't cross tick boundaries still compute correctly
    /// because slot0 (sqrtPriceX96) and liquidity are already prefetched.
    pub fast_mode: bool,
}

struct SimAccount {
    info: AccountInfo,
    storage: HashMap<U256, U256>,
}

impl SimDb {
    /// Snapshot all cached data from a `BlockStateDB` into a new `SimDb`.
    ///
    /// Takes a READ reference (caller must already hold the read lock).
    /// Account bytecodes are reference-counted (`Bytes`), so cloning is cheap.
    pub fn snapshot(db: &BlockStateDB) -> Self {
        let mut accounts = HashMap::with_capacity(db.accounts.len());
        for (addr, acct) in &db.accounts {
            let storage: HashMap<U256, U256> = acct.storage.iter()
                .map(|(k, v)| (*k, v.value))
                .collect();
            accounts.insert(*addr, SimAccount {
                info: acct.info.clone(),
                storage,
            });
        }
        Self {
            accounts,
            rpc_url: db.rpc_url.clone(),
            client: reqwest::Client::new(),
            fast_mode: true, // fast mode: cache misses return defaults (all pool state is prefetched)
        }
    }

    /// Write back any newly-fetched (RPC) storage/account data into the shared
    /// BlockStateDB so that future snapshots include it — no repeated RPC calls.
    ///
    /// Caller must hold a WRITE lock on state_db.
    pub fn write_back(&self, state_db: &mut BlockStateDB) {
        use crate::state_db::{BlockStateAccount, BlockStateSlot, InsertionType};
        for (addr, acct) in &self.accounts {
            // Ensure the account exists in state_db
            let entry = state_db.accounts.entry(*addr).or_insert_with(|| {
                BlockStateAccount {
                    info: acct.info.clone(),
                    storage: Default::default(),
                }
            });
            // Merge code if we fetched it from RPC
            if acct.info.code.is_some() && entry.info.code.is_none() {
                entry.info.code = acct.info.code.clone();
                entry.info.code_hash = acct.info.code_hash;
            }
            // Merge storage — only insert slots that don't already exist
            // (state_db's version is canonical; we only add cache misses)
            for (slot, val) in &acct.storage {
                entry.storage.entry(*slot).or_insert_with(|| {
                    BlockStateSlot {
                        value: *val,
                        insertion_type: InsertionType::OnChain,
                    }
                });
            }
        }
    }

    /// Blocking RPC: eth_getCode + eth_getBalance + nonce.
    fn fetch_account_rpc(&self, address: Address) -> AccountInfo {
        let addr_hex = format!("0x{}", hex::encode(address.as_slice()));
        let url = self.rpc_url.clone();
        let client = self.client.clone();

        tokio::task::block_in_place(|| {
            let rt = tokio::runtime::Handle::current();
            rt.block_on(async {
                // Batch: getCode, getBalance, getTransactionCount
                let body = serde_json::json!([
                    {"jsonrpc":"2.0","method":"eth_getCode","params":[&addr_hex,"latest"],"id":1},
                    {"jsonrpc":"2.0","method":"eth_getBalance","params":[&addr_hex,"latest"],"id":2},
                    {"jsonrpc":"2.0","method":"eth_getTransactionCount","params":[&addr_hex,"latest"],"id":3}
                ]);
                let resp = match client.post(&url).json(&body).send().await {
                    Ok(r) => r,
                    Err(_) => return AccountInfo::default(),
                };
                let results: Vec<serde_json::Value> = match resp.json().await {
                    Ok(r) => r,
                    Err(_) => return AccountInfo::default(),
                };

                let mut info = AccountInfo::default();

                // Parse code
                if let Some(s) = results.get(0)
                    .and_then(|v| v.get("result"))
                    .and_then(|v| v.as_str())
                {
                    let hex_str = s.trim_start_matches("0x");
                    if hex_str.len() > 2 {
                        if let Ok(code_bytes) = hex::decode(hex_str) {
                            info.code_hash = keccak256(&code_bytes);
                            info.code = Some(Bytecode::new_raw(Bytes::from(code_bytes)));
                        }
                    }
                }

                // Parse balance
                if let Some(s) = results.get(1)
                    .and_then(|v| v.get("result"))
                    .and_then(|v| v.as_str())
                {
                    let hex_str = s.trim_start_matches("0x");
                    let padded = format!("{:0>64}", hex_str);
                    if let Ok(bytes) = hex::decode(&padded) {
                        let mut arr = [0u8; 32];
                        arr.copy_from_slice(&bytes[..32]);
                        info.balance = U256::from_be_bytes(arr);
                    }
                }

                // Parse nonce
                if let Some(s) = results.get(2)
                    .and_then(|v| v.get("result"))
                    .and_then(|v| v.as_str())
                {
                    let hex_str = s.trim_start_matches("0x");
                    if let Ok(n) = u64::from_str_radix(hex_str, 16) {
                        info.nonce = n;
                    }
                }

                info
            })
        })
    }

    /// Blocking RPC: eth_getStorageAt.
    fn fetch_storage_rpc(&self, address: Address, index: U256) -> U256 {
        let addr_hex = format!("0x{}", hex::encode(address.as_slice()));
        let slot_hex = format!("0x{:0>64x}", index);
        let url = self.rpc_url.clone();
        let client = self.client.clone();

        tokio::task::block_in_place(|| {
            let rt = tokio::runtime::Handle::current();
            rt.block_on(async {
                let body = serde_json::json!({
                    "jsonrpc":"2.0",
                    "method":"eth_getStorageAt",
                    "params":[&addr_hex, &slot_hex, "latest"],
                    "id":1
                });
                match client.post(&url).json(&body).send().await {
                    Ok(resp) => {
                        if let Ok(json) = resp.json::<serde_json::Value>().await {
                            if let Some(s) = json.get("result").and_then(|v| v.as_str()) {
                                let s = s.trim_start_matches("0x");
                                let padded = format!("{:0>64}", s);
                                if let Ok(bytes) = hex::decode(&padded) {
                                    let mut arr = [0u8; 32];
                                    arr.copy_from_slice(&bytes[..32]);
                                    return U256::from_be_bytes(arr);
                                }
                            }
                        }
                        U256::ZERO
                    }
                    Err(_) => U256::ZERO,
                }
            })
        })
    }
}

impl Database for SimDb {
    type Error = String;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        if let Some(acct) = self.accounts.get(&address) {
            // Account exists in snapshot — check if code needs fetching
            if acct.info.code.is_none() && acct.info.code_hash == KECCAK_EMPTY {
                if self.fast_mode {
                    // Fast mode: return stub (no code) — REVM will treat as EOA
                    return Ok(Some(acct.info.clone()));
                }
                // Might be a codeless stub from update_slot; fetch real code
                let full_info = self.fetch_account_rpc(address);
                let entry = self.accounts.get_mut(&address).unwrap();
                entry.info.code = full_info.code;
                entry.info.code_hash = full_info.code_hash;
                entry.info.balance = full_info.balance;
                entry.info.nonce = full_info.nonce;
                Ok(Some(entry.info.clone()))
            } else {
                Ok(Some(acct.info.clone()))
            }
        } else {
            if self.fast_mode {
                // Fast mode: return empty account (no RPC)
                return Ok(Some(AccountInfo::default()));
            }
            // Not in snapshot — fetch from RPC and cache locally
            let info = self.fetch_account_rpc(address);
            self.accounts.insert(address, SimAccount {
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
        for acct in self.accounts.values() {
            if acct.info.code_hash == code_hash {
                if let Some(code) = &acct.info.code {
                    return Ok(code.clone());
                }
                break;
            }
        }
        Ok(Bytecode::default())
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        // Check local cache
        if let Some(acct) = self.accounts.get(&address) {
            if let Some(&val) = acct.storage.get(&index) {
                return Ok(val);
            }
        }
        if self.fast_mode {
            // Fast mode: return zero for uncached slots (no RPC).
            // V3 swaps within the current tick range still work because
            // slot0 and liquidity are prefetched. Missing tick data means
            // tick crossings return 0, correctly marking the path as non-viable.
            return Ok(U256::ZERO);
        }
        // Fetch from RPC and cache
        let value = self.fetch_storage_rpc(address, index);
        let entry = self.accounts.entry(address).or_insert_with(|| SimAccount {
            info: AccountInfo::default(),
            storage: HashMap::new(),
        });
        entry.storage.insert(index, value);
        Ok(value)
    }

    fn block_hash(&mut self, _number: u64) -> Result<B256, Self::Error> {
        Ok(B256::ZERO)
    }
}
