/// REVM quoter — inject FlashQuoter bytecode and simulate full swap paths.
///
/// Ported from BaseBuster's quoter pattern. Deploys a quoter contract into
/// REVM at a fixed address (0x1000), then calls it with the swap path to get
/// the exact amount_out. This serves as a verification step after the fast
/// estimator identifies promising paths.
///
/// The quoter executes actual pool swap logic (transferring tokens between
/// pools) in the REVM sandbox, so it catches edge cases that the calculation
/// modules might miss (rounding, fee-on-transfer tokens, etc.).

use alloy::primitives::{Address, Bytes, U256};
use alloy::sol_types::SolCall;
use revm::primitives::{AccountInfo, Bytecode, ExecutionResult, TransactTo, KECCAK_EMPTY};
use std::sync::{Arc, RwLock};

use crate::gen_alloy::FlashQuoter;
use crate::sim_db::SimDb;
use crate::state_db::BlockStateDB;
use crate::swap_types::SwapPath;

/// Fixed address where the quoter contract is deployed in REVM.
pub const QUOTER_ADDRESS: Address = Address::new([
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00,
]);

/// Caller address for quoter simulations.
pub const CALLER_ADDRESS: Address = Address::new([
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xCA, 0x11,
]);

/// REVM-based quoter for verifying swap paths.
pub struct RevmQuoter {
    /// Quoter contract bytecode (compiled FlashQuoter.sol).
    pub quoter_bytecode: Option<Bytes>,
}

impl RevmQuoter {
    /// Create a new quoter. Call `set_bytecode` with the compiled FlashQuoter
    /// contract before using `quote_path`.
    pub fn new() -> Self {
        Self {
            quoter_bytecode: None,
        }
    }

    /// Create with pre-loaded bytecode.
    pub fn with_bytecode(bytecode: Bytes) -> Self {
        Self {
            quoter_bytecode: Some(bytecode),
        }
    }

    /// Set the quoter bytecode (from compiled FlashQuoter.sol).
    pub fn set_bytecode(&mut self, bytecode: Bytes) {
        self.quoter_bytecode = Some(bytecode);
    }

    /// Deploy the quoter contract into a BlockStateDB instance.
    ///
    /// Injects the quoter bytecode at QUOTER_ADDRESS and gives the caller
    /// a large ETH balance for gas.
    pub fn deploy_into(&self, state_db: &mut BlockStateDB) -> Result<(), String> {
        let bytecode = self
            .quoter_bytecode
            .as_ref()
            .ok_or("Quoter bytecode not set")?;

        // Deploy quoter at fixed address
        let quoter_info = AccountInfo {
            balance: U256::ZERO,
            nonce: 1,
            code_hash: alloy::primitives::keccak256(bytecode.as_ref()),
            code: Some(Bytecode::new_raw(bytecode.clone().into())),
        };
        state_db.inject_account(QUOTER_ADDRESS, quoter_info);

        // Give caller ETH for gas
        let caller_info = AccountInfo {
            balance: U256::from(1_000_000_000_000_000_000_000u128), // 1000 ETH
            nonce: 0,
            code_hash: KECCAK_EMPTY,
            code: None,
        };
        state_db.inject_account(CALLER_ADDRESS, caller_info);

        Ok(())
    }

    /// Quote a swap path through the REVM quoter.
    ///
    /// Creates a lightweight snapshot of state_db (read-lock, ~1ms), then
    /// runs REVM on the owned snapshot — no lock held during execution.
    /// Cache misses in the snapshot fall through to direct RPC.
    pub fn quote_path(
        &self,
        state_db: &Arc<RwLock<BlockStateDB>>,
        path: &SwapPath,
        amount_in: U256,
    ) -> Option<U256> {
        if self.quoter_bytecode.is_none() { return None; }
        let mut sim_db = {
            let db = state_db.read().ok()?;
            SimDb::snapshot(&*db)
        };
        self.quote_with_db(&mut sim_db, path, amount_in)
    }

    /// Quote using an existing (reusable) SimDb — avoids repeated snapshots.
    /// The SimDb accumulates lazy-fetched RPC data across calls.
    pub fn quote_with_db(
        &self,
        sim_db: &mut SimDb,
        path: &SwapPath,
        amount_in: U256,
    ) -> Option<U256> {
        if self.quoter_bytecode.is_none() { return None; }

        let params = path.to_quoter_params(amount_in);
        let call = FlashQuoter::getAmountOutCall { params };
        let calldata = call.abi_encode();

        let mut evm = revm::Evm::builder()
            .with_db(sim_db)
            .modify_tx_env(|tx| {
                tx.caller = CALLER_ADDRESS;
                tx.transact_to = TransactTo::Call(QUOTER_ADDRESS);
                tx.data = Bytes::from(calldata);
                tx.value = U256::ZERO;
                tx.gas_limit = 30_000_000;
            })
            .build();

        let result = match evm.transact() {
            Ok(r) => r,
            Err(e) => {
                log::debug!("[Quoter] transact() error: {:?}", e);
                drop(evm);
                return None;
            }
        };
        drop(evm);

        match &result.result {
            ExecutionResult::Success { output, gas_used, .. } => {
                static SUCCESS_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                let count = SUCCESS_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

                let decoded = FlashQuoter::getAmountOutCall::abi_decode_returns(output.data().as_ref(), false)
                    .ok()
                    .map(|r| r.amountOut);

                if count < 3 || count % 5000 == 0 {
                    let out_hex = hex::encode(output.data().as_ref());
                    log::debug!(
                        "[Quoter] Success #{}: gas_used={}, raw_output={}, decoded={:?}, versions={:?}",
                        count, gas_used,
                        if out_hex.len() <= 128 { &out_hex } else { &out_hex[..128] },
                        decoded,
                        path.steps.iter().map(|s| s.protocol.to_quoter_version()).collect::<Vec<_>>()
                    );
                }

                // Extra diagnostic: for the first 5 zero-output V3-containing paths, log
                // pool[0] sqrtPriceX96 / liquidity to verify state was loaded.
                if decoded == Some(U256::ZERO) {
                    static ZERO_V3_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                    let zc = ZERO_V3_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if zc < 5 {
                        let has_v3 = path.steps.iter().any(|s| s.protocol.to_quoter_version() == 1);
                        if has_v3 {
                            let pool0 = path.steps[0].pool_address;
                            // Read from the REVM execution's committed state to check what values
                            // the FlashQuoter saw. Use gas_used as proxy for execution depth:
                            // ~90-110K gas = math-only (V2 reserves or V3 analytical), ~200K+ = real V3 swap.
                            log::warn!(
                                "[Quoter] DIAG zero-V3 #{}: pool[0]={:?} gas_used={} hops={} versions={:?} fees={:?}",
                                zc, pool0, gas_used, path.steps.len(),
                                path.steps.iter().map(|s| s.protocol.to_quoter_version()).collect::<Vec<_>>(),
                                path.steps.iter().map(|s| s.fee).collect::<Vec<_>>()
                            );
                        }
                    }
                }

                decoded
            }
            ExecutionResult::Revert { output, gas_used } => {
                static REVERT_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                let count = REVERT_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if count < 5 || count % 500 == 0 {
                    let revert_hex = if output.len() <= 68 {
                        hex::encode(output.as_ref())
                    } else {
                        format!("{}... ({} bytes)", hex::encode(&output[..68]), output.len())
                    };
                    log::warn!(
                        "[Quoter] REVM revert #{}: gas_used={}, data={}",
                        count, gas_used, revert_hex
                    );
                }
                None
            }
            ExecutionResult::Halt { reason, gas_used } => {
                static HALT_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                let count = HALT_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if count < 5 || count % 500 == 0 {
                    log::warn!(
                        "[Quoter] REVM halt #{}: reason={:?}, gas_used={}",
                        count, reason, gas_used
                    );
                }
                None
            }
        }
    }

    /// Optimize input using a shared SimDb — one snapshot for all iterations.
    pub fn optimize_input(
        &self,
        state_db: &Arc<RwLock<BlockStateDB>>,
        path: &SwapPath,
        min_input: U256,
        max_input: U256,
        steps: usize,
    ) -> (U256, U256) {
        let mut sim_db = {
            let db = state_db.read().unwrap();
            SimDb::snapshot(&*db)
        };
        self.optimize_input_with_db(&mut sim_db, path, min_input, max_input, steps)
    }

    /// Optimize using an existing SimDb (no snapshot overhead).
    pub fn optimize_input_with_db(
        &self,
        sim_db: &mut SimDb,
        path: &SwapPath,
        min_input: U256,
        max_input: U256,
        steps: usize,
    ) -> (U256, U256) {
        let mut best_input = min_input;
        let mut best_profit = U256::ZERO;
        let mut best_output = U256::ZERO;

        // Phase 1: Exponential probing
        let mut probe = min_input;
        let mut bracket_lo = min_input;
        let mut bracket_hi = max_input;
        let mut probes_used: usize = 0;
        let max_probes = steps / 2;

        while probe <= max_input && probes_used < max_probes {
            let out = self.quote_with_db(sim_db, path, probe).unwrap_or(U256::ZERO);
            let profit = if out > probe { out - probe } else { U256::ZERO };
            probes_used += 1;

            if profit > best_profit {
                best_profit = profit;
                best_input = probe;
                best_output = out;
            } else if profit < best_profit && best_profit > U256::ZERO {
                bracket_hi = probe;
                bracket_lo = if probe > min_input * U256::from(2u64) {
                    probe / U256::from(4u64)
                } else { min_input };
                break;
            }

            if probe >= max_input { break; }
            probe = (probe * U256::from(2u64)).min(max_input);
        }

        if best_profit.is_zero() {
            return (min_input, U256::ZERO);
        }

        // Phase 2: Ternary search within bracket
        let mut lo = bracket_lo;
        let mut hi = bracket_hi.min(max_input);
        let refinement_steps = steps.saturating_sub(probes_used * 2);

        for _ in 0..refinement_steps {
            if hi <= lo + U256::from(1000u64) { break; }
            let mid1 = lo + (hi - lo) / U256::from(3u64);
            let mid2 = hi - (hi - lo) / U256::from(3u64);

            let out1 = self.quote_with_db(sim_db, path, mid1).unwrap_or(U256::ZERO);
            let profit1 = if out1 > mid1 { out1 - mid1 } else { U256::ZERO };
            let out2 = self.quote_with_db(sim_db, path, mid2).unwrap_or(U256::ZERO);
            let profit2 = if out2 > mid2 { out2 - mid2 } else { U256::ZERO };

            if profit1 > profit2 { hi = mid2; } else { lo = mid1; }

            if profit1 > best_profit { best_profit = profit1; best_input = mid1; best_output = out1; }
            if profit2 > best_profit { best_profit = profit2; best_input = mid2; best_output = out2; }
        }

        (best_input, best_output)
    }
}
