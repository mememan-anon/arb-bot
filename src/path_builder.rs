//! Path builder — arb cycle finder using a token graph (alloy-native + petgraph).
//!
//! Tokens are nodes in an undirected graph; pools are edges. A recursive DFS
//! emits every cycle of length `2..=max_hops` that closes back to the start
//! token as a `SwapPath`.
//!
//! BaseBuster's `graph.rs` shipped with `max_hops = 2`, which made 3-hop
//! triangles unreachable (the DFS bailed before ever checking closing edges at
//! depth 3). This implementation defaults `max_hops = 3` and correctly emits
//! cycles at every depth up to the limit.
//!
//! Supported cycle sizes:
//!   max_hops=2 → 2-hop cross-DEX same-pair arb only (BaseBuster behaviour)
//!   max_hops=3 → 2-hop + 3-hop triangles (default)
//!   max_hops=4 → 2- + 3- + 4-hop quadrangles

use alloy::primitives::Address;
use petgraph::graph::UnGraph;
use petgraph::prelude::*;
use std::collections::{HashMap, HashSet};

use crate::pool_loader::RawPool;
use crate::swap_types::{SwapPath, SwapStep};

// ── Public API ────────────────────────────────────────────────────────────────

/// Build all arb cycles up to `max_hops` deep for each start token.
///
/// - Tokens are nodes; pools are edges.
/// - Recursive DFS emits every cycle returning to `start_token` in 2..=max_hops swaps.
/// - `max_paths_per_token` caps emission per start token to prevent combinatorial explosion.
///
/// Typical call: `build_arb_paths(&pools, &start_tokens, 3, 200_000)`
pub fn build_arb_paths(
    pools: &[RawPool],
    start_tokens: &[Address],
    max_hops: usize,
    max_paths_per_token: usize,
) -> Vec<SwapPath> {
    // ── Build undirected token graph ─────────────────────────────────────────
    // Nodes = token Address, edge weight = index into `pools` slice.
    let mut graph: UnGraph<Address, usize> = UnGraph::new_undirected();
    let mut node_map: HashMap<Address, NodeIndex> = HashMap::new();

    for (i, pool) in pools.iter().enumerate() {
        let n0 = *node_map.entry(pool.token0).or_insert_with(|| graph.add_node(pool.token0));
        let n1 = *node_map.entry(pool.token1).or_insert_with(|| graph.add_node(pool.token1));
        graph.add_edge(n0, n1, i);
    }

    // ── DFS from each start token ────────────────────────────────────────────
    let mut all_paths: Vec<SwapPath> = Vec::new();
    let mut seen_hashes: HashSet<u64> = HashSet::new();

    for &start_token in start_tokens {
        let Some(&start_node) = node_map.get(&start_token) else { continue };

        let mut count = 0usize;
        let mut current_path: Vec<(NodeIndex, usize, NodeIndex)> = Vec::new(); // (from, pool_idx, to)
        let mut visited: HashSet<NodeIndex> = HashSet::new();
        visited.insert(start_node);

        find_cycles(
            &graph,
            pools,
            start_node,
            start_node,
            max_hops,
            &mut current_path,
            &mut visited,
            &mut all_paths,
            &mut seen_hashes,
            &mut count,
            max_paths_per_token,
        );
    }

    log::info!(
        "path_builder: {} arb paths (max_hops={}) from {} pools ({} start tokens)",
        all_paths.len(),
        max_hops,
        pools.len(),
        start_tokens.len()
    );
    all_paths
}

// ── DFS internals ─────────────────────────────────────────────────────────────

/// Recursive DFS that emits arb cycles closing back to `start_node`.
///
/// At each node we iterate incident edges. If an edge leads back to `start_node`
/// and the current depth is ≥ 2 we emit a cycle. Otherwise, if the neighbour
/// is unvisited and depth < max_hops we recurse.
fn find_cycles(
    graph: &UnGraph<Address, usize>,
    pools: &[RawPool],
    current_node: NodeIndex,
    start_node: NodeIndex,
    max_hops: usize,
    current_path: &mut Vec<(NodeIndex, usize, NodeIndex)>,
    visited: &mut HashSet<NodeIndex>,
    all_paths: &mut Vec<SwapPath>,
    seen_hashes: &mut HashSet<u64>,
    count: &mut usize,
    max_paths_per_token: usize,
) {
    if *count >= max_paths_per_token {
        return;
    }

    for edge in graph.edges(current_node) {
        // Determine the neighbour (petgraph undirected: one endpoint is current_node)
        let next_node = if edge.source() == current_node { edge.target() } else { edge.source() };
        let pool_idx  = *edge.weight();

        if next_node == start_node {
            // Close cycle — must have at least 2 hops total
            if current_path.len() >= 2 {
                let path = build_swap_path(graph, pools, current_path, current_node, pool_idx, start_node);
                if seen_hashes.insert(path.hash) {
                    all_paths.push(path);
                    *count += 1;
                    if *count >= max_paths_per_token {
                        return;
                    }
                }
            }
        } else if !visited.contains(&next_node) && current_path.len() < max_hops {
            current_path.push((current_node, pool_idx, next_node));
            visited.insert(next_node);

            find_cycles(
                graph, pools, next_node, start_node, max_hops,
                current_path, visited, all_paths, seen_hashes, count, max_paths_per_token,
            );

            current_path.pop();
            visited.remove(&next_node);
        }
    }
}

/// Materialise the accumulated DFS path + closing edge into a `SwapPath`.
fn build_swap_path(
    graph: &UnGraph<Address, usize>,
    pools: &[RawPool],
    current_path: &[(NodeIndex, usize, NodeIndex)],
    closing_from: NodeIndex,
    closing_pool_idx: usize,
    closing_to: NodeIndex,
) -> SwapPath {
    let mut steps: Vec<SwapStep> = Vec::with_capacity(current_path.len() + 1);

    for &(from, pidx, to) in current_path {
        let pool = &pools[pidx];
        steps.push(SwapStep {
            pool_address: pool.address,
            token_in:     graph[from],
            token_out:    graph[to],
            protocol:     pool.protocol,
            fee:          pool.fee,
        });
    }

    let cp = &pools[closing_pool_idx];
    steps.push(SwapStep {
        pool_address: cp.address,
        token_in:     graph[closing_from],
        token_out:    graph[closing_to],
        protocol:     cp.protocol,
        fee:          cp.fee,
    });

    SwapPath::new(steps)
}
