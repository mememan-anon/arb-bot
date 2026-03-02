// ── Alloy + REVM pipeline (active codebase) ──────────────────────────────────
pub mod block_stream;
pub mod config;
pub mod utils;

// Pool discovery and path building (alloy-native replacements for cfmms/graph.rs)
pub mod pool_loader;
pub mod path_builder;

// Core pipeline types
pub mod gen_alloy;
pub mod swap_types;
pub mod pipeline_events;
pub mod state_db;
pub mod sim_db;

// AMM math (alloy / revm-native)
pub mod calculation;

// Pipeline workers
pub mod cache_v2;
pub mod gas_station;
pub mod tracing_revm;
pub mod market_state;
pub mod quoter_revm;
pub mod filter_revm;
pub mod searcher_pipeline;
pub mod simulator_pipeline;
pub mod tx_sender_pipeline;
pub mod mempool;
pub mod ignition;

/// Local reth Base node MDBX accessor — compiled only with `--features local-node`.
/// Requires: `cargo build --features local-node` after the reth deps are uncommented.
#[cfg(feature = "local-node")]
pub mod history_db;

#[cfg(test)]
mod tests {
    use alloy::primitives::Address;

    use crate::path_builder::build_arb_paths;
    use crate::pool_loader::RawPool;
    use crate::swap_types::PoolProtocol;

    fn make_pool(addr: u64, t0: u64, t1: u64) -> RawPool {
        RawPool {
            address: Address::from_slice(&{let mut b = [0u8;20]; b[16..].copy_from_slice(&addr.to_be_bytes()[4..]); b}),
            token0:  Address::from_slice(&{let mut b = [0u8;20]; b[16..].copy_from_slice(&t0.to_be_bytes()[4..]); b}),
            token1:  Address::from_slice(&{let mut b = [0u8;20]; b[16..].copy_from_slice(&t1.to_be_bytes()[4..]); b}),
            decimals0: 18,
            decimals1: 18,
            fee: 30,
            tick_spacing: 0,
            protocol: PoolProtocol::UniswapV2,
        }
    }

    fn addr(n: u64) -> Address {
        Address::from_slice(&{let mut b = [0u8;20]; b[16..].copy_from_slice(&n.to_be_bytes()[4..]); b})
    }

    #[test]
    fn test_triangular_path_three_v2_pools() {
        // T0-T1, T1-T2, T2-T0 — one perfect triangle
        let pools = vec![
            make_pool(101, 1, 2), // T0-T1
            make_pool(102, 2, 3), // T1-T2
            make_pool(103, 3, 1), // T2-T0
        ];
        let paths = build_arb_paths(&pools, &[addr(1)], 3, 10_000);
        assert!(!paths.is_empty(), "should find at least one 3-hop path");
        let p = &paths[0];
        assert_eq!(p.steps.len(), 3);
        assert_eq!(p.steps[0].token_in,  addr(1));
        assert_eq!(p.steps[2].token_out, addr(1), "path must close back to start token");
    }

    #[test]
    fn test_no_paths_with_disconnected_pools() {
        let pools = vec![
            make_pool(201, 1, 2),
            make_pool(202, 3, 4), // disconnected — no shared tokens
        ];
        let paths = build_arb_paths(&pools, &[addr(1)], 3, 10_000);
        assert!(paths.is_empty(), "disconnected pools should produce no paths");
    }

    #[test]
    fn test_path_deduplication() {
        // Two identical triangles — only one unique path hash should be emitted
        let pools = vec![
            make_pool(301, 1, 2),
            make_pool(302, 2, 3),
            make_pool(303, 3, 1),
        ];
        let paths = build_arb_paths(&pools, &[addr(1)], 3, 10_000);
        let hashes: std::collections::HashSet<u64> = paths.iter().map(|p| p.hash).collect();
        assert_eq!(paths.len(), hashes.len(), "no duplicate hashes");
    }

    #[test]
    fn test_pool_loader_missing_csv_returns_empty() {
        let result = crate::pool_loader::load_v2_pools_from_csv("/non/existent/path.csv");
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }

    #[test]
    fn test_raw_to_pool_metas_preserves_fields() {
        let raw = vec![make_pool(401, 10, 20)];
        let metas = crate::pool_loader::raw_to_pool_metas(&raw);
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].address, raw[0].address);
        assert_eq!(metas[0].token0, raw[0].token0);
        assert_eq!(metas[0].token1, raw[0].token1);
        assert!(!metas[0].is_v3, "V2 pool should not be marked as V3");
    }
}
