/// Types shared across the liquidation sub-modules.
///
/// Adapted from overlord-rs/crates/overlord-shared/src/lib.rs.
/// ZMQ MessageBundle replaced with direct channel messages;
/// everything else is kept as close to the original as possible.

use alloy::primitives::{Address, U256};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Whistleblower (monitor) types — direct port of overlord-shared
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum WhistleblowerEventType {
    LiquidationCall,
    Borrow,
    Supply,
    Repay,
    /// Fired by the Chainlink price trigger (price_trigger.rs) when an
    /// AnswerUpdated log (confirmed path) or a forward(transmit()) pending
    /// tx (mempool path) is detected for a watched oracle feed.
    /// args[0] = the Aave reserve token address whose price changed.
    PriceUpdate,
}

/// Decoded arguments for an AAVE v3 event.
/// Each event type places its relevant addresses/amounts as strings in `args`
/// (same slot ordering as the original overlord-rs implementation).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WhistleblowerEventDetails {
    pub event: WhistleblowerEventType,
    /// Field layout per event type (mirrors overlord-rs):
    ///
    /// - LiquidationCall → [collateralAsset, debtAsset, user, debtToCover,
    ///                       liquidatedCollateralAmount, liquidator]
    /// - Borrow          → [reserve, onBehalfOf]
    /// - Supply          → [reserve, onBehalfOf]
    /// - Repay           → [reserve, user]
    pub args: Vec<String>,
}

/// Sent by the monitor task to downstream consumers (health_factor, executor).
/// Replaces the old ZMQ WhistleblowerUpdate + MessageBundle::WhistleblowerNotification.
#[derive(Serialize, Deserialize, Debug)]
pub struct LiquidationUpdate {
    /// Short trace ID — first 8 hex chars of the transaction hash.
    pub trace_id: String,
    pub block_number: u64,
    /// Unix epoch timestamp (ms) when monitor forwarded this update.
    /// Used to measure queue delay in Phase 2.
    #[serde(default)]
    pub enqueued_at_ms: u64,
    pub event_details: WhistleblowerEventDetails,
}

// ---------------------------------------------------------------------------
// Underwater user alert — sent by health_factor to executor (Phase 2+)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct UnderwaterUserAlert {
    pub user: Address,
    pub trace_id: String,
    pub health_factor: U256,
    pub total_collateral_base: U256,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CanonicalPriceEvent {
    pub source: String,
    pub asset: Address,
    pub price_e8: i128,
    pub confidence_bps: u32,
    pub publish_time: u64,
    pub received_at_ms: u64,
}
