/// DashMap + FxHasher cache for computation results.
///
/// Ported from BaseBuster's cache pattern. Uses `DashMap` for lock-free
/// concurrent access and `FxHasher` (via rustc-hash) for fast hashing
/// on u64/address keys.
///
/// Supports per-pool invalidation: when a pool's state changes, we evict
/// all entries referencing that pool.

use alloy::primitives::{Address, U256};
use dashmap::DashMap;
use rustc_hash::FxBuildHasher;
use std::hash::{Hash, Hasher};

/// Cache key: (pool_address, amount_in, zero_for_one).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheKey {
    pub pool: Address,
    pub amount_in: U256,
    pub zero_for_one: bool,
}

impl Hash for CacheKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.pool.hash(state);
        self.amount_in.as_limbs().hash(state);
        self.zero_for_one.hash(state);
    }
}

/// Cached swap result.
#[derive(Debug, Clone, Copy)]
pub struct CacheValue {
    pub amount_out: U256,
    pub block_number: u64,
}

/// Concurrent cache for swap calculations.
///
/// Uses DashMap with FxBuildHasher for fast concurrent access.
/// Entries are tagged with block_number for staleness detection.
pub struct CalcCache {
    map: DashMap<CacheKey, CacheValue, FxBuildHasher>,
    /// Secondary index: pool → list of cache keys.
    /// Used for per-pool invalidation.
    pool_keys: DashMap<Address, Vec<CacheKey>, FxBuildHasher>,
}

impl CalcCache {
    /// Create a new empty cache.
    pub fn new() -> Self {
        Self {
            map: DashMap::with_hasher(FxBuildHasher),
            pool_keys: DashMap::with_hasher(FxBuildHasher),
        }
    }

    /// Create with pre-allocated capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            map: DashMap::with_capacity_and_hasher(capacity, FxBuildHasher),
            pool_keys: DashMap::with_hasher(FxBuildHasher),
        }
    }

    /// Get a cached result.
    pub fn get(&self, key: &CacheKey) -> Option<CacheValue> {
        self.map.get(key).map(|v| *v)
    }

    /// Get a cached result, but only if it's from the current block.
    pub fn get_fresh(&self, key: &CacheKey, current_block: u64) -> Option<U256> {
        self.map
            .get(key)
            .filter(|v| v.block_number == current_block)
            .map(|v| v.amount_out)
    }

    /// Insert a new cached result.
    pub fn insert(&self, key: CacheKey, value: CacheValue) {
        // Update secondary index
        self.pool_keys
            .entry(key.pool)
            .or_insert_with(Vec::new)
            .push(key);
        self.map.insert(key, value);
    }

    /// Convenience: cache a swap computation result.
    pub fn cache_amount_out(
        &self,
        pool: Address,
        amount_in: U256,
        zero_for_one: bool,
        amount_out: U256,
        block_number: u64,
    ) {
        let key = CacheKey {
            pool,
            amount_in,
            zero_for_one,
        };
        self.insert(key, CacheValue { amount_out, block_number });
    }

    /// Invalidate all cached entries for a specific pool.
    /// Called when a pool's state has changed (new block, state diff).
    pub fn invalidate_pool(&self, pool: &Address) {
        if let Some((_, keys)) = self.pool_keys.remove(pool) {
            for key in keys {
                self.map.remove(&key);
            }
        }
    }

    /// Invalidate multiple pools at once (batch operation for new blocks).
    pub fn invalidate_pools(&self, pools: &[Address]) {
        for pool in pools {
            self.invalidate_pool(pool);
        }
    }

    /// Clear the entire cache (e.g., on chain reorg).
    pub fn clear(&self) {
        self.map.clear();
        self.pool_keys.clear();
    }

    /// Number of entries in the cache.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Is the cache empty?
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Retain only entries matching a predicate.
    pub fn retain_fresh(&self, current_block: u64) {
        self.map.retain(|_, v| v.block_number == current_block);
        // Note: pool_keys secondary index may have stale entries.
        // This is acceptable — they'll be cleaned up on next invalidate_pool call.
    }
}

/// Path-level rate cache using DashMap + FxHasher.
/// Key: path hash (u64), Value: (cumulative_rate, block_number).
pub struct PathRateCache {
    map: DashMap<u64, (U256, u64), FxBuildHasher>,
}

impl PathRateCache {
    pub fn new() -> Self {
        Self {
            map: DashMap::with_hasher(FxBuildHasher),
        }
    }

    /// Get cached path rate, only if from current block.
    pub fn get_fresh(&self, path_hash: u64, current_block: u64) -> Option<U256> {
        self.map
            .get(&path_hash)
            .filter(|v| v.1 == current_block)
            .map(|v| v.0)
    }

    /// Cache a path rate.
    pub fn insert(&self, path_hash: u64, rate: U256, block_number: u64) {
        self.map.insert(path_hash, (rate, block_number));
    }

    /// Clear all entries.
    pub fn clear(&self) {
        self.map.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_insert_get() {
        let cache = CalcCache::new();
        let pool = Address::ZERO;
        let amount_in = U256::from(1000u64);
        let amount_out = U256::from(999u64);

        cache.cache_amount_out(pool, amount_in, true, amount_out, 100);

        let key = CacheKey { pool, amount_in, zero_for_one: true };
        assert_eq!(cache.get_fresh(&key, 100), Some(amount_out));
        assert_eq!(cache.get_fresh(&key, 101), None); // stale
    }

    #[test]
    fn test_pool_invalidation() {
        let cache = CalcCache::new();
        let pool = Address::ZERO;
        cache.cache_amount_out(pool, U256::from(100u64), true, U256::from(99u64), 1);
        cache.cache_amount_out(pool, U256::from(200u64), false, U256::from(198u64), 1);
        assert_eq!(cache.len(), 2);

        cache.invalidate_pool(&pool);
        assert_eq!(cache.len(), 0);
    }
}
