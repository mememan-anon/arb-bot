/// Maverick V2 bin-based concentrated-liquidity pool math via REVM.
///
/// Ported from BaseBuster's `calculation/maverick.rs`.
///
/// Maverick pools use a proprietary bin-based AMM that is not practical to
/// replicate in pure Rust. Instead, we call the Maverick V2 lens contract's
/// `calculateSwap` function inside the REVM sandbox to get an exact quote.
///
/// Lens contract on Base: `0xb40AfdB85a07f37aE217E7D6462e609900dD8D7A`
///
/// The lens is called with:
///   `calculateSwap(pool, amount, tokenAIn, exactOutput=false, tickLimit)`
/// and returns `(amountIn, amountOut, gasEstimate)`.
///
/// `tickLimit` of `i32::MAX` (positive direction) or `i32::MIN` (negative)
/// tells the lens to traverse all available bins.

use alloy::primitives::{address, Address, Bytes, U256};
use alloy::sol;
use alloy::sol_types::{SolCall, SolValue};
use revm::primitives::{ExecutionResult, TransactTo};
use std::sync::{Arc, RwLock};

use crate::state_db::BlockStateDB;

// ── ABI bindings ─────────────────────────────────────────────────────────────

sol! {
    #[sol(rpc)]
    contract MaverickLens {
        /// Returns the amount received for a given swap input (exactOutput=false).
        /// tickLimit: i32::MAX for A→B, i32::MIN for B→A to traverse all bins.
        function calculateSwap(
            address pool,
            uint128 amount,
            bool tokenAIn,
            bool exactOutput,
            int32 tickLimit
        ) external returns (uint256 amountIn, uint256 amountOut, uint256 gasEstimate);
    }
}

/// Maverick V2 lens contract address on Base mainnet.
const MAVERICK_LENS: Address = address!("b40AfdB85a07f37aE217E7D6462e609900dD8D7A");

/// Fixed caller address for Maverick REVM calls.
const MAVERICK_CALLER: Address = address!("0000000000000000000000000000000000000001");

// ── Public API ────────────────────────────────────────────────────────────────

/// Compute amount_out for a Maverick V2 pool using the lens contract via REVM.
///
/// - `state_db`: REVM database with current chain state
/// - `pool`: Maverick V2 pool address
/// - `amount_in`: raw token amount in
/// - `token_a_in`: true if swapping token A → B, false for B → A
///   (token A is the token with the lower address, i.e. token0)
///
/// Returns `None` if the REVM call reverts or DB lock fails.
pub fn get_amount_out_maverick(
    state_db: &Arc<RwLock<BlockStateDB>>,
    pool: Address,
    amount_in: U256,
    token_a_in: bool,
) -> Option<U256> {
    if amount_in.is_zero() {
        return Some(U256::ZERO);
    }

    // Clamp to u128 — Maverick lens takes uint128
    let amount_128 = if amount_in > U256::from(u128::MAX) {
        u128::MAX
    } else {
        amount_in.to::<u128>()
    };

    // tickLimit: traverse all bins in the appropriate direction
    let tick_limit: i32 = if token_a_in { i32::MAX } else { i32::MIN };

    let calldata = MaverickLens::calculateSwapCall {
        pool,
        amount: amount_128,
        tokenAIn: token_a_in,
        exactOutput: false,
        tickLimit: tick_limit,
    }
    .abi_encode();

    let mut db = state_db.write().ok()?;

    let mut evm = revm::Evm::builder()
        .with_db(&mut *db)
        .modify_tx_env(|tx| {
            tx.caller = MAVERICK_CALLER;
            tx.transact_to = TransactTo::Call(MAVERICK_LENS);
            tx.data = Bytes::from(calldata);
            tx.value = U256::ZERO;
        })
        .build();

    let result = evm.transact().ok()?;
    drop(evm);

    match result.result {
        ExecutionResult::Success { output, .. } => {
            // Returns (amountIn, amountOut, gasEstimate) — we want index 1
            let decoded = <(U256, U256, U256)>::abi_decode(output.data(), false).ok()?;
            Some(decoded.1)
        }
        _ => None,
    }
}

/// Rate (scaled 1e18) for estimator from a Maverick V2 pool.
///
/// Returns `RATE_SCALE` (1e18) as pass-through if the REVM call fails,
/// so the pool is not incorrectly filtered out at the pre-filter stage.
pub fn maverick_rate_1e18(
    state_db: &Arc<RwLock<BlockStateDB>>,
    pool: Address,
    amount_in: U256,
    token_a_in: bool,
    out_decimals: u8,
) -> U256 {
    let one = U256::from(1_000_000_000_000_000_000u64); // 1e18
    let out = match get_amount_out_maverick(state_db, pool, amount_in, token_a_in) {
        Some(v) => v,
        None => return one, // pass-through on failure
    };
    if out.is_zero() || amount_in.is_zero() {
        return one; // pass-through on zero
    }
    let dec = U256::from(10u64).pow(U256::from(out_decimals));
    if dec.is_zero() {
        return one;
    }
    if let Some(scaled) = out.checked_mul(one) {
        scaled / dec
    } else {
        one
    }
}
