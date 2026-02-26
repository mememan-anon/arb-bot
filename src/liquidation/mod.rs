/// Liquidation sub-system for arb-bot.
///
/// Ported from overlord-rs (https://github.com/hernan-erasmo/overlord-rs).
/// Each sub-module maps to one of the overlord-rs crates:
///
///   monitor      ← whistleblower-rs  (AAVE v3 event listener)
///   health_factor← vega-rs           (user health-factor cache)   [Phase 2]
///   executor     ← profito-rs        (liquidation tx builder)     [Phase 3]
///   types        ← overlord-shared   (shared structs)
///
/// Inter-crate ZMQ sockets are replaced by `tokio::sync::mpsc` channels.
/// IPC transport is replaced by the same WSS URL used by the arb core.

pub mod types;
pub mod monitor;
pub mod health_factor;
pub mod executor;
pub mod price_trigger;
pub mod offchain_price;

pub use types::{LiquidationUpdate, UnderwaterUserAlert};
pub use monitor::run as run_monitor;
pub use health_factor::run as run_health_factor;
pub use executor::run as run_executor;
pub use price_trigger::run as run_price_trigger;
pub use offchain_price::run as run_offchain_price;
