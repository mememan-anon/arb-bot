/// Block tracing — lightweight eth_getLogs + eth_getStorageAt approach.
///
/// Replaces the expensive `debug_traceBlockByNumber` with PreStateTracer which
/// hangs on Base mainnet blocks (>5 min per block due to EVM replay cost).
///
/// New approach:
///   1. `eth_getLogs` for the block — find which tracked pools had events
///   2. For each touched pool, `eth_getStorageAt` the 2-3 relevant slots
///   3. Return as BlockTraceDiffs (same output type, no changes to callers)
///
/// Supported pool types:
///   - UniswapV2 / SushiSwapV2 (Sync uint112 event → slot 8 reserves)
///   - UniswapV3 / SushiSwapV3 / Slipstream (Swap event → slot 0 + slot 4)
///   - Aerodrome V2 (Solidly Sync uint256 → slot 8, approximate)

use alloy::primitives::{Address, U256};
use futures::future::join_all;
use std::collections::{HashMap, HashSet};

/// Result of tracing a block: per-address storage diffs.
#[derive(Debug, Clone)]
pub struct BlockTraceDiffs {
    /// Block number that was traced.
    pub block_number: u64,
    /// Per-address storage slot changes: address → {slot → new_value}.
    pub diffs: HashMap<Address, HashMap<U256, U256>>,
    /// Set of all addresses that had any state change.
    pub touched_addresses: HashSet<Address>,
}

impl BlockTraceDiffs {
    pub fn new(block_number: u64) -> Self {
        Self {
            block_number,
            diffs: HashMap::new(),
            touched_addresses: HashSet::new(),
        }
    }

    /// Add a storage diff.
    pub fn add_diff(&mut self, address: Address, slot: U256, new_value: U256) {
        self.touched_addresses.insert(address);
        self.diffs
            .entry(address)
            .or_insert_with(HashMap::new)
            .insert(slot, new_value);
    }

    /// Get diffs for a specific address.
    pub fn get_address_diffs(&self, address: &Address) -> Option<&HashMap<U256, U256>> {
        self.diffs.get(address)
    }

    /// Filter to only include addresses in the tracked set.
    pub fn filter_tracked(&self, tracked: &HashSet<Address>) -> BlockTraceDiffs {
        let mut filtered = BlockTraceDiffs::new(self.block_number);
        for addr in &self.touched_addresses {
            if tracked.contains(addr) {
                if let Some(slots) = self.diffs.get(addr) {
                    for (slot, value) in slots {
                        filtered.add_diff(*addr, *slot, *value);
                    }
                }
            }
        }
        filtered
    }
}

/// Trace a block using eth_getLogs + eth_getStorageAt.
///
/// Replaces the expensive prestateTracer approach which hangs on Base mainnet.
/// For each block:
///   1. eth_getLogs — identify tracked pools that had events
///   2. Parallel eth_getStorageAt — fetch 2-3 key slots per touched pool
///   3. Classify pool type by event topic (V3 Swap vs V2 Sync)
///
/// `tracked_pools` — only these addresses are processed (caller's known pool set).
pub async fn trace_block_diffs(
    rpc_url: &str,
    block_number: u64,
    tracked_pools: &HashSet<Address>,
) -> Result<BlockTraceDiffs, String> {
    // V3 Swap(address,address,int256,int256,uint160,uint128,int24) — UniV3/Slipstream/SushiV3
    const V3_SWAP_TOPIC: &str =
        "0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67";
    // V2 Sync(uint112,uint112) — UniV2/SushiV2
    const V2_SYNC_U112_TOPIC: &str =
        "0x1c411e9a96e071241c2f21f7726b17ae89e3cab4c78be50e062b03a9fffbbad1";
    // Solidly Sync(uint256,uint256) — Aerodrome V2 / Velodrome V2
    const V2_SYNC_U256_TOPIC: &str =
        "0xcf2aa50876cdfbb541206f89af0ee78d44a2abf8d328e37fa4917f982149848a";

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| format!("Client build: {e}"))?;

    // Fetch STORAGE at "latest" so our state reflects the actual chain head,
    // not a 1-block-stale snapshot that causes phantom arbs.
    let storage_block_tag = "latest".to_string();

    // ── Step 1: eth_getLogs with topic filter ───────────────────────────────
    // Pin both fromBlock and toBlock to the exact block number.
    // Using "latest" as toBlock caused "invalid block range params" when the
    // HTTP RPC lagged behind the WS gossip endpoint (latest < block_number-1).
    let log_block_hex = format!("0x{:x}", block_number);

    let logs_arr = {
        let logs_body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_getLogs",
            "params": [{
                "fromBlock": &log_block_hex,
                "toBlock":   &log_block_hex,
                "topics": [[V3_SWAP_TOPIC, V2_SYNC_U112_TOPIC, V2_SYNC_U256_TOPIC]]
            }],
            "id": 1
        });

        // Retry with increasing delays — HTTP RPC may lag behind WS gossip.
        let retry_delays_ms: &[u64] = &[0, 250, 500];
        let mut last_err = String::new();
        let mut result_arr: Option<Vec<serde_json::Value>> = None;

        for (attempt, &delay_ms) in retry_delays_ms.iter().enumerate() {
            if delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            }

            let resp = match client.post(rpc_url).json(&logs_body).send().await {
                Ok(r) => r,
                Err(e) => {
                    last_err = format!("eth_getLogs send attempt {}: {e}", attempt);
                    continue;
                }
            };

            let json: serde_json::Value = match resp.json().await {
                Ok(j) => j,
                Err(e) => {
                    last_err = format!("eth_getLogs json attempt {}: {e}", attempt);
                    continue;
                }
            };

            match json.get("result").and_then(|r| r.as_array()).cloned() {
                Some(a) => {
                    result_arr = Some(a);
                    break;
                }
                None => {
                    last_err = json
                        .get("error")
                        .map(|e| e.to_string())
                        .unwrap_or_else(|| "no result".into());
                }
            }
        }

        match result_arr {
            Some(a) => a,
            None => return Err(format!("eth_getLogs error: {last_err}")),
        }
    };

    // ── Step 2: identify tracked pools and classify V2/V3 ──────────────────
    // true  = V3 (has Swap event)
    // false = V2 (only Sync event seen)
    let mut touched: HashMap<Address, bool> = HashMap::new();

    for log in logs_arr {
        let addr_str = log
            .get("address")
            .and_then(|a| a.as_str())
            .unwrap_or("");
        let Ok(addr) = parse_address(addr_str) else {
            continue;
        };
        if !tracked_pools.contains(&addr) {
            continue;
        }

        let topics = log
            .get("topics")
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default();
        let topic0 = topics
            .first()
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_lowercase();

        let is_v3 = topic0 == V3_SWAP_TOPIC;
        let is_v2 = topic0 == V2_SYNC_U112_TOPIC || topic0 == V2_SYNC_U256_TOPIC;

        let entry = touched.entry(addr).or_insert(false);
        if is_v3 {
            *entry = true;
        } else if is_v2 && !*entry {
            *entry = false;
        } else if !is_v3 && !is_v2 {
            // unknown event — mark as touched anyway (lazy fetch via REVM)
            touched.entry(addr).or_insert(false);
        }
    }

    if touched.is_empty() {
        return Ok(BlockTraceDiffs::new(block_number));
    }

    // ── Step 3: parallel eth_getStorageAt for key slots ────────────────────
    // Fetch at "latest" (not block N-1) so we track the actual chain head.
    let fetch_futs: Vec<_> = touched
        .iter()
        .map(|(addr, is_v3)| {
            let rpc = rpc_url.to_string();
            let a = *addr;
            let v3 = *is_v3;
            let stag = storage_block_tag.clone();
            async move {
                let mut pairs: Vec<(U256, U256)> = Vec::new();
                if v3 {
                    // V3: slot0 (sqrtPriceX96 + tick) and liquidity slot
                    if let Ok(v) = get_storage_at_raw(&rpc, a, 0u64, &stag).await {
                        pairs.push((U256::from(0u64), v));
                    }
                    if let Ok(v) = get_storage_at_raw(&rpc, a, 4u64, &stag).await {
                        pairs.push((U256::from(4u64), v));
                    }
                } else {
                    // V2: packed reserves in slot 8
                    if let Ok(v) = get_storage_at_raw(&rpc, a, 8u64, &stag).await {
                        pairs.push((U256::from(8u64), v));
                    }
                }
                (a, pairs)
            }
        })
        .collect();

    let results = join_all(fetch_futs).await;
    let mut diffs = BlockTraceDiffs::new(block_number);
    for (addr, pairs) in results {
        for (slot, value) in pairs {
            diffs.add_diff(addr, slot, value);
        }
    }

    Ok(diffs)
}

/// Fetch a single storage slot via JSON-RPC eth_getStorageAt.
async fn get_storage_at_raw(
    rpc_url: &str,
    addr: Address,
    slot: u64,
    block_tag: &str,
) -> Result<U256, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| e.to_string())?;

    let addr_hex = format!("0x{}", hex::encode(addr.as_slice()));
    let slot_hex = format!("0x{slot:x}");

    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "eth_getStorageAt",
        "params": [addr_hex, slot_hex, block_tag],
        "id": 1
    });

    let resp = client
        .post(rpc_url)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("eth_getStorageAt send: {e}"))?;

    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("eth_getStorageAt json: {e}"))?;

    let result_str = json
        .get("result")
        .and_then(|r| r.as_str())
        .ok_or_else(|| format!("eth_getStorageAt no result"))?;

    parse_u256(result_str)
}


/// Parse a hex address string like "0x1234..." into an Address.
fn parse_address(s: &str) -> Result<Address, String> {
    let s = s.trim_start_matches("0x");
    let bytes = hex::decode(format!("{:0>40}", s))
        .map_err(|e| format!("Invalid address hex: {e}"))?;
    if bytes.len() != 20 {
        return Err("Address must be 20 bytes".to_string());
    }
    let mut arr = [0u8; 20];
    arr.copy_from_slice(&bytes);
    Ok(Address::from(arr))
}

/// Parse a hex U256 string.
fn parse_u256(s: &str) -> Result<U256, String> {
    let s = s.trim_start_matches("0x");
    let bytes = hex::decode(format!("{:0>64}", s))
        .map_err(|e| format!("Invalid U256 hex: {e}"))?;
    if bytes.len() != 32 {
        return Err("U256 must be 32 bytes".to_string());
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    Ok(U256::from_be_bytes(arr))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_address() {
        let addr = parse_address("0x0000000000000000000000000000000000000001").unwrap();
        assert_eq!(addr, Address::from([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]));
    }

    #[test]
    fn test_parse_u256() {
        let value = parse_u256("0x1").unwrap();
        assert_eq!(value, U256::from(1u64));
    }

    #[test]
    fn test_block_trace_diffs() {
        let mut diffs = BlockTraceDiffs::new(100);
        let addr = Address::ZERO;
        diffs.add_diff(addr, U256::from(8u64), U256::from(42u64));
        assert!(diffs.touched_addresses.contains(&addr));
        assert_eq!(diffs.get_address_diffs(&addr).unwrap().len(), 1);
    }
}
