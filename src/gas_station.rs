/// Gas station — next base fee prediction + profit-proportional priority fee.
///
/// Ported from BaseBuster's gas logic. Thread-safe via AtomicU64.
/// Chain-specific EIP-1559 parameters are supplied at construction time via
/// `GasParamsConfig`, so the same code works for Base, BSC, and Avalanche.
///
/// Base fee prediction: EIP-1559 formula with configurable per-chain params.
/// Priority fee: profit_share / gas_estimate, capped at a configurable gwei limit.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::config::GasParamsConfig;

/// Global thread-safe base fee tracker.
pub struct GasStation {
    /// Current base fee in wei.
    pub base_fee: AtomicU64,
    /// Last observed gas used.
    pub last_gas_used: AtomicU64,
    /// Last observed gas limit.
    pub last_gas_limit: AtomicU64,
    // —— Chain-specific EIP-1559 parameters (immutable after construction) ——
    /// gas_target = gas_limit / elasticity_multiplier.
    pub elasticity_multiplier: u64,
    /// How aggressively base fee adjusts each block.
    pub base_fee_change_denominator: u64,
    /// Chain-enforced minimum base fee floor (wei).
    pub min_base_fee_wei: u64,
    /// Hard cap on priority fee bid (wei).
    pub priority_fee_cap_wei: u64,
}

impl GasStation {
    pub fn new(params: &GasParamsConfig) -> Self {
        Self {
            base_fee: AtomicU64::new(params.min_base_fee_wei),
            last_gas_used: AtomicU64::new(0),
            last_gas_limit: AtomicU64::new(30_000_000),
            elasticity_multiplier: params.eip1559_elasticity_multiplier,
            base_fee_change_denominator: params.eip1559_base_fee_change_denominator,
            min_base_fee_wei: params.min_base_fee_wei,
            priority_fee_cap_wei: params.priority_fee_cap_gwei.saturating_mul(1_000_000_000),
        }
    }

    /// Update from a new block header.
    pub fn update_from_block(&self, base_fee: u64, gas_used: u64, gas_limit: u64) {
        self.base_fee.store(base_fee, Ordering::Relaxed);
        self.last_gas_used.store(gas_used, Ordering::Relaxed);
        self.last_gas_limit.store(gas_limit, Ordering::Relaxed);
    }

    /// Predict the next block's base fee using EIP-1559 with chain-specific parameters.
    pub fn predict_next_base_fee(&self) -> u64 {
        let parent_base_fee = self.base_fee.load(Ordering::Relaxed);
        let parent_gas_used = self.last_gas_used.load(Ordering::Relaxed);
        let parent_gas_limit = self.last_gas_limit.load(Ordering::Relaxed);

        calc_next_base_fee(
            parent_base_fee,
            parent_gas_used,
            parent_gas_limit,
            self.elasticity_multiplier,
            self.base_fee_change_denominator,
            self.min_base_fee_wei,
        )
    }

    /// Calculate priority fee as profit_share / gas_estimate.
    ///
    /// `profit_wei`: expected profit in wei.
    /// `gas_estimate`: estimated gas units for the transaction.
    /// `profit_share_bps`: how many bps of profit to spend on priority (e.g., 5000 = 50%).
    ///
    /// Returns priority fee in wei per gas unit.
    pub fn calc_priority_fee(
        &self,
        profit_wei: u64,
        gas_estimate: u64,
        profit_share_bps: u64,
    ) -> u64 {
        if gas_estimate == 0 {
            return 0;
        }
        let share = (profit_wei as u128 * profit_share_bps as u128) / 10_000u128;
        let priority = share / gas_estimate as u128;
        // Cap at configured gwei limit to avoid insane priority fees
        std::cmp::min(priority as u64, self.priority_fee_cap_wei)
    }

    /// Get current base fee.
    pub fn current_base_fee(&self) -> u64 {
        self.base_fee.load(Ordering::Relaxed)
    }

    /// Total fee per gas: predicted base fee + priority fee.
    pub fn total_fee_per_gas(&self, profit_wei: u64, gas_estimate: u64, profit_share_bps: u64) -> u64 {
        let base = self.predict_next_base_fee();
        let priority = self.calc_priority_fee(profit_wei, gas_estimate, profit_share_bps);
        base.saturating_add(priority)
    }

    /// Estimate total gas cost in wei for a given gas estimate.
    pub fn estimate_gas_cost_wei(&self, gas_estimate: u64) -> u64 {
        let base = self.predict_next_base_fee();
        base.saturating_mul(gas_estimate)
    }
}

/// Calculate next block base fee from parent block parameters.
///
/// Accepts chain-specific EIP-1559 parameters so the same function works
/// for Base/Optimism (elasticity=6, denominator=50) and BSC/Avalanche (elasticity=2, denominator=8).
pub fn calc_next_base_fee(
    parent_base_fee: u64,
    parent_gas_used: u64,
    parent_gas_limit: u64,
    elasticity_multiplier: u64,
    base_fee_change_denominator: u64,
    min_base_fee_wei: u64,
) -> u64 {
    if parent_gas_limit == 0 {
        return parent_base_fee.max(min_base_fee_wei);
    }

    let parent_gas_target = parent_gas_limit / elasticity_multiplier;
    if parent_gas_target == 0 {
        return parent_base_fee.max(min_base_fee_wei);
    }

    let next_fee = if parent_gas_used == parent_gas_target {
        parent_base_fee
    } else if parent_gas_used > parent_gas_target {
        // Gas used above target → base fee increases
        let gas_used_delta = parent_gas_used - parent_gas_target;
        let delta = (parent_base_fee as u128 * gas_used_delta as u128
            / parent_gas_target as u128
            / base_fee_change_denominator as u128) as u64;
        parent_base_fee.saturating_add(delta.max(1))
    } else {
        // Gas used below target → base fee decreases
        let gas_used_delta = parent_gas_target - parent_gas_used;
        let delta = (parent_base_fee as u128 * gas_used_delta as u128
            / parent_gas_target as u128
            / base_fee_change_denominator as u128) as u64;
        parent_base_fee.saturating_sub(delta)
    };

    next_fee.max(min_base_fee_wei)
}

/// Convenience: L1 data fee estimate for Optimism-style rollups.
///
/// On Base L2, transactions also pay an L1 data fee for calldata posted to L1.
/// This is a rough estimate based on calldata size.
pub fn estimate_l1_data_fee(calldata_bytes: usize, l1_base_fee_gwei: f64) -> u64 {
    // Simplified Ecotone formula:
    // l1_data_fee = l1_base_fee * (calldata_gas + overhead)
    // calldata_gas ≈ 16 * non_zero_bytes + 4 * zero_bytes
    // Rough average: 12 bytes per byte of calldata
    let calldata_gas = calldata_bytes as f64 * 12.0;
    let overhead = 2100.0; // fixed overhead
    let fee_wei = (calldata_gas + overhead) * l1_base_fee_gwei * 1e9;
    fee_wei as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GasParamsConfig;

    // Test helpers: Base/Optimism Canyon params
    const ELASTICITY: u64 = 6;
    const DENOMINATOR: u64 = 50;
    const MIN_FEE: u64 = 1_000_000;

    fn base_params() -> GasParamsConfig {
        GasParamsConfig {
            eip1559_elasticity_multiplier: ELASTICITY,
            eip1559_base_fee_change_denominator: DENOMINATOR,
            min_base_fee_wei: MIN_FEE,
            priority_fee_cap_gwei: 10,
        }
    }

    #[test]
    fn test_next_base_fee_at_target() {
        // When gas used == target, base fee stays the same
        let base = 1_000_000_000; // 1 gwei
        let limit = 30_000_000;
        let target = limit / ELASTICITY;
        let next = calc_next_base_fee(base, target, limit, ELASTICITY, DENOMINATOR, MIN_FEE);
        assert_eq!(next, base);
    }

    #[test]
    fn test_next_base_fee_above_target() {
        let base = 1_000_000_000u64;
        let limit = 30_000_000;
        let target = limit / ELASTICITY;
        let used = target + 1_000_000; // above target
        let next = calc_next_base_fee(base, used, limit, ELASTICITY, DENOMINATOR, MIN_FEE);
        assert!(next > base);
    }

    #[test]
    fn test_next_base_fee_below_target() {
        let base = 1_000_000_000u64;
        let limit = 30_000_000;
        let target = limit / ELASTICITY;
        let used = target - 1_000_000; // below target
        let next = calc_next_base_fee(base, used, limit, ELASTICITY, DENOMINATOR, MIN_FEE);
        assert!(next < base);
    }

    #[test]
    fn test_min_base_fee_floor() {
        let next = calc_next_base_fee(0, 0, 30_000_000, ELASTICITY, DENOMINATOR, MIN_FEE);
        assert!(next >= MIN_FEE);
    }

    #[test]
    fn test_priority_fee() {
        let gs = GasStation::new(&base_params());
        let priority = gs.calc_priority_fee(1_000_000_000, 200_000, 5000);
        // 50% of 1 gwei profit / 200k gas = 2500 wei per gas
        assert_eq!(priority, 2500);
    }
}
