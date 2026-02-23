#!/usr/bin/env python3
"""
pull_lfj.py — Fetch LFJ (Liquidity Book) pool data for Avalanche (or any chain)
and write it to <cache_dir>/.cached-lfj-pools.csv.

Usage:
  python scripts/pull_lfj.py                         # defaults: avax, $10k min TVL
  python scripts/pull_lfj.py --min-tvl 50000          # raise TVL filter to $50k
  python scripts/pull_lfj.py --chain arbitrum          # different chain
  python scripts/pull_lfj.py --dry-run                 # print CSV, don't write
  python scripts/pull_lfj.py --config-path config/avax.toml  # read cache_dir from config
"""

import argparse
import csv
import json
import os
import sys
from pathlib import Path

import requests

# ── API endpoint ────────────────────────────────────────────────────────────
LFJ_API_BASE = "https://barn.lfj.gg/v1/pools"

# Supported pool versions (V2.0 is dead, skip it)
SUPPORTED_VERSIONS = {"v2.1", "v2.2"}

# CSV column header
CSV_HEADER = [
    "address",
    "version",
    "token_x",
    "token_y",
    "decimals_x",
    "decimals_y",
    "bin_step",
    "base_fee_bps",
    "liquidity_usd",
    "symbol_x",
    "symbol_y",
]

# Default output filename inside the cache directory
OUTPUT_FILENAME = ".cached-lfj-pools.csv"


def fetch_pools(chain: str) -> list[dict]:
    """Pull all LFJ pools for the given chain from the API."""
    url = f"{LFJ_API_BASE}/{chain}"
    print(f"[pull_lfj] Fetching pools from {url} ...")
    resp = requests.get(url, timeout=30)
    resp.raise_for_status()
    data = resp.json()

    # The API may return either a plain list or {"pools": [...]}
    if isinstance(data, list):
        return data
    if isinstance(data, dict):
        # Try common wrapper keys
        for key in ("pools", "data", "pairs"):
            if key in data and isinstance(data[key], list):
                return data[key]
    # Fallback: log the structure and abort
    print(f"[pull_lfj] Unexpected API response structure: {list(data.keys()) if isinstance(data, dict) else type(data)}", file=sys.stderr)
    sys.exit(1)


def pool_to_row(pool: dict) -> list:
    """Convert a raw API pool dict to a CSV row."""
    token_x = pool["tokenX"]
    token_y = pool["tokenY"]

    # base_fee_bps: lbBaseFeePct is in percent (e.g. 0.05 = 0.05%).
    # Convert to basis points: 0.05% → 5 bps, 0.007% → 1 bps (rounded).
    base_fee_bps = round(pool["lbBaseFeePct"] * 100)

    return [
        pool["pairAddress"].lower(),
        pool["version"],
        token_x["address"].lower(),
        token_y["address"].lower(),
        int(token_x["decimals"]),
        int(token_y["decimals"]),
        int(pool["lbBinStep"]),
        base_fee_bps,
        float(pool["liquidityUsd"]),
        token_x.get("symbol", ""),
        token_y.get("symbol", ""),
    ]


def read_cache_dir_from_toml(config_path: str) -> str | None:
    """
    Very simple TOML line-reader (no dependency on toml package) to extract
    [chain] cache_dir.  Returns None if not found.
    """
    try:
        with open(config_path) as f:
            in_chain = False
            for line in f:
                stripped = line.strip()
                if stripped == "[chain]":
                    in_chain = True
                    continue
                if stripped.startswith("[") and stripped != "[chain]":
                    in_chain = False
                if in_chain and stripped.startswith("cache_dir"):
                    _, _, val = stripped.partition("=")
                    return val.strip().strip('"').strip("'")
    except FileNotFoundError:
        pass
    return None


def main():
    parser = argparse.ArgumentParser(description="Pull LFJ pools to CSV cache.")
    parser.add_argument("--chain", default="avalanche", help="Chain name (default: avalanche)")
    parser.add_argument("--min-tvl", type=float, default=10_000.0, metavar="USD",
                        help="Minimum pool TVL in USD (default: 10000)")
    parser.add_argument("--cache-dir", default=None,
                        help="Cache directory, e.g. 'avax' (default: derived from --chain or config)")
    parser.add_argument("--config-path", default=None,
                        help="Path to TOML config to read cache_dir from (e.g. config/avax.toml)")
    parser.add_argument("--dry-run", action="store_true",
                        help="Print CSV rows to stdout instead of writing to file")
    parser.add_argument("--out", default=None,
                        help="Override output file path (default: cache/<dir>/.cached-lfj-pools.csv)")
    args = parser.parse_args()

    # Resolve cache_dir
    cache_dir = args.cache_dir
    if cache_dir is None and args.config_path:
        cache_dir = read_cache_dir_from_toml(args.config_path)
    if cache_dir is None:
        # Derive from chain name: "avalanche" → "avax", otherwise use chain name
        mapping = {"avalanche": "avax", "arbitrum": "arbitrum", "bsc": "bsc"}
        cache_dir = mapping.get(args.chain, args.chain)

    # Determine output path
    if args.out:
        out_path = Path(args.out)
    else:
        out_path = Path("cache") / cache_dir / OUTPUT_FILENAME

    # Fetch pools
    raw_pools = fetch_pools(args.chain)
    print(f"[pull_lfj] Total raw pools returned: {len(raw_pools)}")

    # Filter by version and TVL
    filtered = [
        p for p in raw_pools
        if p.get("version") in SUPPORTED_VERSIONS
        and float(p.get("liquidityUsd", 0)) >= args.min_tvl
    ]
    print(f"[pull_lfj] Pools after version+TVL filter (>=${args.min_tvl:.0f}): {len(filtered)}")

    # Sort descending by TVL for readability
    filtered.sort(key=lambda p: float(p.get("liquidityUsd", 0)), reverse=True)

    rows = [pool_to_row(p) for p in filtered]

    if args.dry_run:
        import io
        buf = io.StringIO()
        writer = csv.writer(buf)
        writer.writerow(CSV_HEADER)
        writer.writerows(rows)
        print(buf.getvalue())
        return

    # Write CSV
    out_path.parent.mkdir(parents=True, exist_ok=True)
    with open(out_path, "w", newline="") as f:
        writer = csv.writer(f)
        writer.writerow(CSV_HEADER)
        writer.writerows(rows)

    print(f"[pull_lfj] Wrote {len(rows)} pools → {out_path}")
    if rows:
        print(f"[pull_lfj] Top 5 by TVL:")
        for row in rows[:5]:
            addr, ver, tx, ty, dx, dy, bs, bfee, tvl, sx, sy = row
            print(f"  {sx}/{sy} ({ver}) bs={bs} fee={bfee}bps tvl=${tvl:,.0f}  {addr}")


if __name__ == "__main__":
    main()
