/// Pipeline event types for the channel-based worker architecture.
///
/// Ported from BaseBuster's events.rs. These events flow through the pipeline:
///   NewBlock → MarketState → Searcher → Simulator → TxSender
///
/// Each worker receives events on its input channel and produces events on its output.

use alloy::primitives::{Address, U256};

use crate::swap_types::SwapPath;

/// A path identified as profitable by the searcher, pending REVM verification.
#[derive(Debug, Clone)]
pub struct ArbPath {
    pub path: SwapPath,
    pub expected_profit: U256,
    pub gas_estimate: u64,
    pub block_number: u64,
}

/// A path that passed REVM quoter verification, ready for submission.
#[derive(Debug, Clone)]
pub struct ValidPath {
    pub arb: ArbPath,
    pub amount_in: U256,
    pub amount_out: U256,
    pub gas_cost: U256,
    pub net_profit: U256,
}

/// Set of pool addresses whose state changed in the current block.
#[derive(Debug, Clone)]
pub struct PoolsTouched {
    pub block_number: u64,
    pub touched_pools: Vec<Address>,
}

/// Events that flow through the pipeline channels.
#[derive(Debug, Clone)]
pub enum PipelineEvent {
    /// New block header (broadcast to all workers).
    NewBlock {
        block_number: u64,
        base_fee: u64,
        gas_used: u64,
        gas_limit: u64,
        timestamp: u64,
    },
    /// State updater → Searcher: pools whose state changed.
    PoolsTouched(PoolsTouched),
    /// Searcher → Simulator: a path estimated to be profitable.
    ArbPath(ArbPath),
    /// Simulator → TxSender: a REVM-verified profitable path.
    ValidPath(ValidPath),
}
