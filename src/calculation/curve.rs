/// Curve tricrypto / stable-swap pool math via REVM on-chain call.
///
/// Ported from BaseBuster's `calculation/curve.rs`.
///
/// Curve pools use complex piecewise AMM math (stableswap invariant or
/// tricrypto Newton-Raphson) that is impractical to replicate in pure Rust
/// without audited reference implementations. Instead, we call the pool's own
/// `get_dy(i, j, dx)` function inside the REVM sandbox — the same EVM state
/// used by the quoter — to get an exact on-chain-equivalent quote.
///
/// This is the same strategy as BaseBuster.

use alloy::primitives::{address, Address, Bytes, U256};
use alloy::sol;
use alloy::sol_types::SolCall;
use alloy::sol_types::SolValue;
use revm::primitives::{ExecutionResult, TransactTo};
use std::sync::{Arc, RwLock};

use crate::state_db::BlockStateDB;

// ── ABI bindings ─────────────────────────────────────────────────────────────

sol! {
    #[sol(rpc)]
    contract CurvePool {
        /// Returns the amount of token `j` received for `dx` of token `i`.
        function get_dy(uint256 i, uint256 j, uint256 dx) external view returns (uint256);
    }
}

/// Fixed caller address for Curve REVM calls.
const CURVE_CALLER: Address = address!("0000000000000000000000000000000000000001");

// ── Public API ────────────────────────────────────────────────────────────────

/// Compute amount_out for a Curve pool by calling `get_dy` inside REVM.
///
/// - `state_db`: REVM database with current chain state
/// - `pool`: Curve pool address
/// - `index_in`: token index for the input token (0, 1, 2, ...)
/// - `index_out`: token index for the output token
/// - `amount_in`: raw amount of input token
///
/// Returns `None` if the REVM call reverts or DB lock fails.
pub fn get_amount_out_curve(
    state_db: &Arc<RwLock<BlockStateDB>>,
    pool: Address,
    index_in: u64,
    index_out: u64,
    amount_in: U256,
) -> Option<U256> {
    if amount_in.is_zero() {
        return Some(U256::ZERO);
    }

    // Encode calldata for get_dy(i, j, dx)
    let calldata = CurvePool::get_dyCall {
        i: U256::from(index_in),
        j: U256::from(index_out),
        dx: amount_in,
    }
    .abi_encode();

    // Lock DB for REVM execution
    let mut db = state_db.write().ok()?;

    let mut evm = revm::Evm::builder()
        .with_db(&mut *db)
        .modify_tx_env(|tx| {
            tx.caller = CURVE_CALLER;
            tx.transact_to = TransactTo::Call(pool);
            tx.data = Bytes::from(calldata);
            tx.value = U256::ZERO;
        })
        .build();

    let result = evm.transact().ok()?;
    drop(evm); // release borrow before return

    match result.result {
        ExecutionResult::Success { output, .. } => {
            U256::abi_decode(output.data(), false).ok()
        }
        _ => None,
    }
}

/// Rate (scaled 1e18) for estimator from a Curve pool.
pub fn curve_rate_1e18(
    state_db: &Arc<RwLock<BlockStateDB>>,
    pool: Address,
    index_in: u64,
    index_out: u64,
    reference_amount: U256,
) -> U256 {
    let one = U256::from(1_000_000_000_000_000_000u64); // 1e18
    let out = get_amount_out_curve(state_db, pool, index_in, index_out, reference_amount)
        .unwrap_or(U256::ZERO);
    if reference_amount.is_zero() {
        return U256::ZERO;
    }
    (out * one) / reference_amount
}
