#!/usr/bin/env python3
"""
Fetch PancakeSwap V2 and V3 pools on Base via The Graph and append them to
the same bot pool caches used by pull_base_uniswap.py.

Output (appended, duplicates skipped):
  cache/base/.cached-pools.csv        ← V2 pairs
  cache/base/.cached-v3cl-pools.csv   ← V3 concentrated-liquidity

PancakeSwap V3 uses the same Uniswap V3 slot0 / swap ABI, so no Rust changes
needed — pools land in the UniswapV3CL path automatically.

Usage:
  python scripts/pull_pancakeswap.py                  # both V2 + V3
  python scripts/pull_pancakeswap.py --v2              # V2 only
  python scripts/pull_pancakeswap.py --v3              # V3 only
  python scripts/pull_pancakeswap.py --min-tvl 100000  # higher TVL cutoff
  python scripts/pull_pancakeswap.py --dry-run         # preview, no writes

The Graph subgraph IDs (Base mainnet):
  V2  — https://thegraph.com/explorer/subgraphs/2NjL7L4CmQaGJSacM43ofmH6ARf6gJoBeBaJtz9eWAQ9
  V3  — https://thegraph.com/explorer/subgraphs/BHWNsedAHtmTCzXxCCDfhPmm6iN9rxUhoRHdHKyujic3

If these IDs have changed or don't respond, find updated IDs at:
  https://thegraph.com/explorer → search "pancakeswap" → filter by "Base"
"""

from __future__ import annotations

import argparse
import os
import sys

# ── Reuse all helpers from pull_base_uniswap.py ──────────────────────────────
_HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, _HERE)

from pull_base_uniswap import (   # type: ignore[import]
    _graph_url, _repo_root, _load_chain_config, _main_tokens,
    _load_existing_addresses, _load_existing_cl_rows,
    fetch_v2_pools, fetch_cl_pools,
    write_v2_csv, write_cl_csv,
    V2_CSV_HEADER, V3CL_CSV_HEADER,
    DEFAULT_API_KEY,
)

# ─── PancakeSwap subgraph IDs (Base mainnet) ─────────────────────────────────

SUBGRAPH_PANCAKE_V2 = "2NjL7L4CmQaGJSacM43ofmH6ARf6gJoBeBaJtz9eWAQ9"
SUBGRAPH_PANCAKE_V3 = "BHWNsedAHtmTCzXxCCDfhPmm6iN9rxUhoRHdHKyujic3"

# PancakeSwap fee tiers (ppm): 100=0.01%, 500=0.05%, 2500=0.25%, 10000=1%
PANCAKE_V3_TICK_SPACINGS = {
    100:   1,
    500:   10,
    2500:  50,
    10000: 200,
}

DEFAULT_MIN_TVL_V2 = 50_000.0
DEFAULT_MIN_TVL_V3 = 50_000.0


# ─── Main ─────────────────────────────────────────────────────────────────────

def main() -> None:
    ap = argparse.ArgumentParser(description="Pull PancakeSwap V2/V3 pools on Base")
    ap.add_argument("--v2", action="store_true", help="V2 only")
    ap.add_argument("--v3", action="store_true", help="V3 only")
    ap.add_argument("--min-tvl",    type=float, default=None)
    ap.add_argument("--min-tvl-v2", type=float, default=None)
    ap.add_argument("--min-tvl-v3", type=float, default=None)
    ap.add_argument("--api-key",    default=DEFAULT_API_KEY)
    ap.add_argument("--chain",      default="base")
    ap.add_argument("--config",     default=None)
    ap.add_argument("--dry-run",    action="store_true")
    args = ap.parse_args()

    run_v2 = args.v2 or (not args.v2 and not args.v3)
    run_v3 = args.v3 or (not args.v2 and not args.v3)

    min_tvl_v2 = args.min_tvl_v2 or args.min_tvl or DEFAULT_MIN_TVL_V2
    min_tvl_v3 = args.min_tvl_v3 or args.min_tvl or DEFAULT_MIN_TVL_V3

    root   = _repo_root()
    cfg    = _load_chain_config(args.config, args.chain)
    tokens = _main_tokens(cfg)

    cache_dir  = (cfg.get("chain") or {}).get("cache_dir") or args.chain
    v2_csv     = os.path.join(root, "cache", cache_dir, ".cached-pools.csv")
    v3cl_csv   = os.path.join(root, "cache", cache_dir, ".cached-v3cl-pools.csv")

    total_new = 0

    # ── V2 ──────────────────────────────────────────────────────────────────
    if run_v2:
        url = _graph_url(args.api_key, SUBGRAPH_PANCAKE_V2)
        print(f"\n{'='*60}")
        print(f"  PancakeSwap V2  (min TVL: ${min_tvl_v2:,.0f})")
        print(f"  subgraph: {SUBGRAPH_PANCAKE_V2[:24]}…")
        print(f"{'='*60}")
        try:
            pools    = fetch_v2_pools(url, min_tvl_v2, tokens)
            existing = _load_existing_addresses(v2_csv)
            n = write_v2_csv(v2_csv, pools, existing, dry_run=args.dry_run)
            total_new += n
            if args.dry_run:
                print(f"  [dry-run] would add {n} V2 pools")
            else:
                print(f"  ✓ {n} new V2 pools written → {v2_csv}")
        except Exception as e:
            print(f"  [error] PancakeSwap V2 fetch failed: {e}", file=sys.stderr)
            print(f"  Check/update SUBGRAPH_PANCAKE_V2 at top of this script.", file=sys.stderr)

    # ── V3 ──────────────────────────────────────────────────────────────────
    if run_v3:
        url = _graph_url(args.api_key, SUBGRAPH_PANCAKE_V3)
        print(f"\n{'='*60}")
        print(f"  PancakeSwap V3  (min TVL: ${min_tvl_v3:,.0f})")
        print(f"  subgraph: {SUBGRAPH_PANCAKE_V3[:24]}…")
        print(f"{'='*60}")
        try:
            pools        = fetch_cl_pools(url, min_tvl_v3, tokens,
                                          protocol="UniswapV3CL",
                                          dex="PancakeSwapV3")
            existing_rows = _load_existing_cl_rows(v3cl_csv)
            # Override tick spacing using PancakeSwap's fee→tickSpacing map
            corrected: list = []
            for p in pools:
                ts = p.tick_spacing if p.tick_spacing != 0 else PANCAKE_V3_TICK_SPACINGS.get(p.fee_ppm, 60)
                corrected.append(p._replace(tick_spacing=ts))
            pools = corrected
            n = write_cl_csv(v3cl_csv, pools, existing_rows, dry_run=args.dry_run)
            total_new += n
            if args.dry_run:
                print(f"  [dry-run] would add {n} V3 CL pools")
            else:
                print(f"  ✓ {n} new/updated V3 CL pools written → {v3cl_csv}")
        except Exception as e:
            print(f"  [error] PancakeSwap V3 fetch failed: {e}", file=sys.stderr)
            print(f"  Check/update SUBGRAPH_PANCAKE_V3 at top of this script.", file=sys.stderr)

    print(f"\nDone. Total new/updated: {total_new}")


if __name__ == "__main__":
    main()
