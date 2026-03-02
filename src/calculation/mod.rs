//! DEX calculation modules — organized per AMM type.
//!
//! Design:
//!   Step 1 — Seed:    `rates::RateEstimator::seed_from_raw_pools()`
//!   Step 2 — Filter:  `rates::RateEstimator::is_profitable(path)`  — pre-REVM cull
//!   Step 3 — Simulate:`quoter_revm` / `simulator_pipeline`          — exact REVM quote
//!   Step 4 — Execute: `tx_sender_pipeline`                          — submit bundle
//!
//! Per-DEX modules:
//!   v2        — UniswapV2 constant-product (pure math, no state)
//!   v3        — UniswapV3 tick-bitmap traversal (reads local BlockStateDB)
//!   aerodrome — Aerodrome/Velodrome volatile + stable-swap
//!   balancer  — Balancer V2 weighted pool (LogExpMath)
//!   curve     — Curve tri-crypto / stable-swap (via REVM get_dy call)
//!   maverick  — Maverick V2 bin CL (via REVM lens contract call)
//!   rates     — Decimal-aware RateEstimator + WETH price chain

pub mod v2;
pub mod v3;
pub mod aerodrome;
pub mod balancer;
pub mod curve;
pub mod maverick;
pub mod rates;

// Top-level re-exports for convenience
pub use rates::{RateEstimator, RATE_SCALE, decimal_reference};
