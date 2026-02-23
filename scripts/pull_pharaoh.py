#!/usr/bin/env python3
"""
Fetch Pharaoh Exchange pools and write them to the bot''s pool caches.

Pharaoh uses two AMM architectures on Avalanche:
  AMM  (isCl=false) -- x*y=k volatile (version=2) or Solidly stable (version=4)
         -> cache/<chain>/.cached-pools.csv
  V3CL (isCl=true)  -- Uniswap V3 concentrated liquidity (slot0() ABI)
         -> cache/<chain>/.cached-v3cl-pools.csv

CSV schemas
-----------
Both formats are BACKWARD COMPATIBLE with the Rust CSV readers (columns read by
positional index) and carry full token metadata for Python tooling.

AMM CSV  (.cached-pools.csv) -- Rust reads col[0..6]:
  address, version, token0, token1, decimals0, decimals1, fee,
  token0_symbol, token1_symbol, dex

V3CL CSV (.cached-v3cl-pools.csv) -- Rust reads col[1], col[2], col[3], col[5]:
  protocol, address, token0_symbol, token1_symbol, tickSpacing, fee,
  token0_address, token1_address, decimals0, decimals1, dex

Usage examples
--------------
  python scripts/pull_pharaoh.py                     # both AMM and CL
  python scripts/pull_pharaoh.py --amm               # AMM pools only
  python scripts/pull_pharaoh.py --cl                # CL pools only
  python scripts/pull_pharaoh.py --min-tvl 5000
  python scripts/pull_pharaoh.py --include-stable    # volatile + stable AMM
  python scripts/pull_pharaoh.py --dry-run --print-pools
"""

from __future__ import annotations

import argparse
import csv
import json
import os
import sys
import urllib.request
from typing import Any, Dict, List, NamedTuple, Optional, Set

try:
    import tomllib          # py 3.11+
except ImportError:
    try:
        import tomli as tomllib   # pip install tomli
    except ImportError:
        tomllib = None  # type: ignore[assignment]

# -- Constants -----------------------------------------------------------------

PHARAOH_API_URL = "https://pharaoh-new-api-production.up.railway.app/all-pools"
DEX_NAME        = "pharaoh"

# AMM CSV: Rust reads col[0..6] by index, extra cols are ignored by Rust.
AMM_CSV_HEADER = [
    "address",       # col[0] - pool address
    "version",       # col[1] - 2=V2 volatile, 4=Solidly stable
    "token0",        # col[2] - token0 address
    "token1",        # col[3] - token1 address
    "decimals0",     # col[4]
    "decimals1",     # col[5]
    "fee",           # col[6] - fee in basis-points (e.g. 30 = 0.30 %)
    "token0_symbol", # col[7] - human-readable, Python use only
    "token1_symbol", # col[8]
    "dex",           # col[9]
]

# V3CL CSV: Rust reads col[1]=address, col[2-3]=symbols, col[5]=fee (ppm).
# Extra cols after col[5] are Python-only metadata.
V3CL_CSV_HEADER = [
    "protocol",        # col[0] - e.g. "UniswapV3CL"
    "address",         # col[1] - pool address  <- Rust
    "token0_symbol",   # col[2]                 <- Rust
    "token1_symbol",   # col[3]                 <- Rust
    "tickSpacing",     # col[4]
    "fee",             # col[5] - fee in ppm    <- Rust
    "token0_address",  # col[6] - Python use only
    "token1_address",  # col[7]
    "decimals0",       # col[8]
    "decimals1",       # col[9]
    "dex",             # col[10]
]


# -- Small data containers -----------------------------------------------------

class TokenInfo(NamedTuple):
    address:  str   # lowercase hex
    symbol:   str
    decimals: int


class AmmPool(NamedTuple):
    address:   str
    token0:    TokenInfo
    token1:    TokenInfo
    fee_bps:   int   # integer basis-points (30 = 0.30 %)
    is_stable: bool
    tvl_usd:   float


class V3CLPool(NamedTuple):
    address:      str
    token0:       TokenInfo
    token1:       TokenInfo
    fee_ppm:      int   # parts-per-million as returned by the API
    tick_spacing: int
    tvl_usd:      float


# -- Helpers -------------------------------------------------------------------

def _repo_root() -> str:
    return os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def _load_chain_config(chain: str, config_path: Optional[str]) -> Dict[str, Any]:
    if not tomllib:
        raise RuntimeError("tomllib unavailable - use Python 3.11+ or: pip install tomli")
    if config_path:
        path = config_path
    else:
        root = _repo_root()
        candidates = [
            os.path.join(root, "config", "chains", f"{chain}.toml"),
            os.path.join(root, "config", f"{chain}.toml"),
        ]
        path = next((p for p in candidates if os.path.exists(p)), None)
        if path is None:
            raise RuntimeError(
                f"Chain config not found for ''{chain}''. "
                f"Tried: {candidates}"
            )
    with open(path, "rb") as f:
        return tomllib.load(f)


def _main_tokens(cfg: Dict[str, Any]) -> Set[str]:
    """Return a set of lowercase token addresses considered main for this chain."""
    out: Set[str] = set()
    for key in ("main_tokens", "start_tokens"):
        for token in cfg.get(key) or []:
            if not isinstance(token, dict):
                continue
            addr = str(token.get("address", "")).strip().lower()
            if addr:
                out.add(addr)
    if not out:
        raise RuntimeError(
            "No main_tokens or start_tokens found in chain config. "
            "At least one must be defined so we know which pairs to include."
        )
    return out


def _fetch_pools(api_url: str) -> List[Dict[str, Any]]:
    req = urllib.request.Request(api_url, headers={"User-Agent": "arb-bot/1.0"})
    with urllib.request.urlopen(req, timeout=30) as resp:
        data = json.loads(resp.read().decode("utf-8"))
    pools = data.get("pools") if isinstance(data, dict) else data
    if not isinstance(pools, list):
        raise RuntimeError(f"Unexpected API response shape: {type(data)}")
    return pools


def _parse_token(raw: Any) -> Optional[TokenInfo]:
    """Extract TokenInfo from an API token object. Returns None on malformed data."""
    if not isinstance(raw, dict):
        return None
    addr = (raw.get("address") or raw.get("id") or "").strip().lower()
    if not addr or addr == "0x" + "0" * 40:
        return None
    symbol   = str(raw.get("symbol") or "").strip()
    decimals = int(raw.get("decimals") or 18)
    return TokenInfo(address=addr, symbol=symbol, decimals=decimals)


def _load_existing_addresses(csv_path: str, col: int = 0) -> Set[str]:
    """Return the set of pool addresses already present in a CSV file."""
    if not os.path.exists(csv_path):
        return set()
    seen: Set[str] = set()
    with open(csv_path, "r", newline="", encoding="utf-8") as f:
        reader = csv.reader(f)
        header = next(reader, None)
        if header and "address" in header:
            col = header.index("address")
        for row in reader:
            if row and len(row) > col:
                seen.add(row[col].strip().lower())
    return seen


def _load_existing_v3cl_rows(csv_path: str) -> Dict[str, List[str]]:
    """Load V3CL CSV as a dict keyed by lowercase address (col[1]) -> full row."""
    rows: Dict[str, List[str]] = {}
    if not os.path.exists(csv_path):
        return rows
    with open(csv_path, "r", newline="", encoding="utf-8") as f:
        reader = csv.reader(f)
        next(reader, None)  # skip header
        for row in reader:
            if row and len(row) > 1:
                addr = row[1].strip().lower()
                rows[addr] = row
    return rows


# -- Filtering -----------------------------------------------------------------

def filter_amm_pools(
    raw_pools:   List[Dict[str, Any]],
    main_tokens:  Set[str],
    min_tvl:      float,
    stable:       bool,
) -> List[AmmPool]:
    """
    Filter raw API pools to AMM (non-CL) pools that:
      - match the requested stable/volatile type
      - have at least one token in main_tokens
      - have tvlUsd >= min_tvl
      - have a positive feeTier
    """
    out: List[AmmPool] = []
    for pool in raw_pools:
        if not isinstance(pool, dict):
            continue
        if pool.get("isCl"):
            continue  # skip concentrated-liquidity
        if bool(pool.get("isStable")) != stable:
            continue

        address = (pool.get("id") or pool.get("address") or "").strip().lower()
        if not address or address == "0x" + "0" * 40:
            continue

        t0 = _parse_token(pool.get("token0"))
        t1 = _parse_token(pool.get("token1"))
        if t0 is None or t1 is None:
            continue

        # At least one token must be a main token for this pair to be useful
        if t0.address not in main_tokens and t1.address not in main_tokens:
            continue

        tvl = float(pool.get("tvlUsd") or 0)
        if tvl < min_tvl:
            continue

        fee_tier = int(pool.get("feeTier") or 0)
        if fee_tier <= 0:
            continue
        fee_bps = fee_tier // 100   # ppm -> bps  (e.g. 3000 ppm -> 30 bps)
        if fee_bps <= 0:
            continue

        out.append(AmmPool(
            address=address,
            token0=t0,
            token1=t1,
            fee_bps=fee_bps,
            is_stable=stable,
            tvl_usd=tvl,
        ))
    return out


def filter_v3cl_pools(
    raw_pools:   List[Dict[str, Any]],
    main_tokens:  Set[str],
    min_tvl:      float,
) -> List[V3CLPool]:
    """
    Filter raw API pools to V3 CL (isCl=true) pools that:
      - have at least one token in main_tokens
      - have tvlUsd >= min_tvl
      - have a positive feeTier
    """
    out: List[V3CLPool] = []
    for pool in raw_pools:
        if not isinstance(pool, dict):
            continue
        if not pool.get("isCl"):
            continue  # skip AMM

        address = (pool.get("id") or pool.get("address") or "").strip().lower()
        if not address or address == "0x" + "0" * 40:
            continue

        t0 = _parse_token(pool.get("token0"))
        t1 = _parse_token(pool.get("token1"))
        if t0 is None or t1 is None:
            continue

        if t0.address not in main_tokens and t1.address not in main_tokens:
            continue

        tvl = float(pool.get("tvlUsd") or 0)
        if tvl < min_tvl:
            continue

        fee_ppm = int(pool.get("feeTier") or 0)
        if fee_ppm <= 0:
            continue

        # tickSpacing may be null for newly created pools; default 0 and let
        # the Rust on-chain fetch handle the real value.
        tick_spacing = int(pool.get("tickSpacing") or 0)

        out.append(V3CLPool(
            address=address,
            token0=t0,
            token1=t1,
            fee_ppm=fee_ppm,
            tick_spacing=tick_spacing,
            tvl_usd=tvl,
        ))
    return out


# -- Writers -------------------------------------------------------------------

def write_amm_pools(
    csv_path: str,
    pools:    List[AmmPool],
    existing: Set[str],
    version:  int,         # 2 = volatile, 4 = stable
    dry_run:  bool = False,
) -> int:
    """Append new AMM pools to the CSV. Returns the number of rows written."""
    os.makedirs(os.path.dirname(os.path.abspath(csv_path)), exist_ok=True)

    file_exists = os.path.exists(csv_path)
    written = 0

    if dry_run:
        return sum(1 for p in pools if p.address not in existing)

    with open(csv_path, "a", newline="", encoding="utf-8") as f:
        w = csv.writer(f)
        if not file_exists:
            w.writerow(AMM_CSV_HEADER)
        for p in pools:
            if p.address in existing:
                continue
            w.writerow([
                p.address,
                version,
                p.token0.address,
                p.token1.address,
                p.token0.decimals,
                p.token1.decimals,
                p.fee_bps,
                p.token0.symbol,
                p.token1.symbol,
                DEX_NAME,
            ])
            existing.add(p.address)
            written += 1

    return written


def write_v3cl_pools(
    csv_path: str,
    pools:    List[V3CLPool],
    existing_rows: Dict[str, List[str]],
    dry_run:  bool = False,
) -> int:
    """
    Write V3CL pools to the CSV.
    - New pools are appended.
    - Existing pools whose fee has changed are updated in-place (full rewrite).
    Returns the number of rows written or updated.
    """
    os.makedirs(os.path.dirname(os.path.abspath(csv_path)), exist_ok=True)

    existing_addrs = set(existing_rows.keys())
    new_pools = [p for p in pools if p.address not in existing_addrs]

    # Detect fee changes for existing pools
    fee_updates: Dict[str, V3CLPool] = {}
    for p in pools:
        if p.address in existing_rows:
            try:
                cached_fee = int(existing_rows[p.address][5])
            except (IndexError, ValueError):
                cached_fee = -1
            if cached_fee != p.fee_ppm:
                fee_updates[p.address] = p

    if dry_run:
        if fee_updates:
            print(f"[v3cl]   fee updates     : {len(fee_updates)} (stale fees will be corrected)")
            for addr, p in fee_updates.items():
                try:
                    cached_fee = int(existing_rows[addr][5])
                except (IndexError, ValueError):
                    cached_fee = "?"
                print(f"  {p.token0.symbol}/{p.token1.symbol} ({addr[:10]}...)  "
                      f"cached={cached_fee} ppm  live={p.fee_ppm} ppm")
        return len(new_pools)

    if not new_pools and not fee_updates:
        return 0

    def _make_row(p: V3CLPool) -> List:
        return [
            "UniswapV3CL",
            p.address,
            p.token0.symbol,
            p.token1.symbol,
            p.tick_spacing,
            p.fee_ppm,
            p.token0.address,
            p.token1.address,
            p.token0.decimals,
            p.token1.decimals,
            DEX_NAME,
        ]

    if fee_updates:
        # Rewrite entire CSV so updated fees land in the right rows
        updated_rows = dict(existing_rows)  # shallow copy — values are lists
        for addr, p in fee_updates.items():
            row = list(updated_rows[addr])
            row[5] = str(p.fee_ppm)
            updated_rows[addr] = row

        with open(csv_path, "w", newline="", encoding="utf-8") as f:
            w = csv.writer(f)
            w.writerow(V3CL_CSV_HEADER)
            for row in updated_rows.values():
                w.writerow(row)
            for p in new_pools:
                w.writerow(_make_row(p))

        return len(new_pools) + len(fee_updates)
    else:
        # No fee updates — just append new pools
        file_exists = os.path.exists(csv_path)
        with open(csv_path, "a", newline="", encoding="utf-8") as f:
            w = csv.writer(f)
            if not file_exists:
                w.writerow(V3CL_CSV_HEADER)
            for p in new_pools:
                w.writerow(_make_row(p))
        return len(new_pools)


# -- Summary helpers -----------------------------------------------------------

def _print_amm_summary(pools: List[AmmPool], label: str) -> None:
    from collections import Counter
    fees = Counter(p.fee_bps for p in pools)
    tvls = sorted(p.tvl_usd for p in pools)
    print(f"  [{label}] count={len(pools)}  fees(bps)={dict(sorted(fees.items()))}")
    if tvls:
        print(f"  [{label}] TVL ${tvls[0]:,.0f} -- ${tvls[-1]:,.0f}  "
              f"(median ${tvls[len(tvls)//2]:,.0f})")


def _print_v3cl_summary(pools: List[V3CLPool]) -> None:
    from collections import Counter
    fees = Counter(p.fee_ppm for p in pools)
    tvls = sorted(p.tvl_usd for p in pools)
    print(f"  [v3cl] count={len(pools)}  fees(ppm)={dict(sorted(fees.items()))}")
    if tvls:
        print(f"  [v3cl] TVL ${tvls[0]:,.0f} -- ${tvls[-1]:,.0f}  "
              f"(median ${tvls[len(tvls)//2]:,.0f})")


def _print_amm_pool(p: AmmPool, is_new: bool, version: int) -> None:
    tag   = "NEW " if is_new else "    "
    label = "VOL " if version == 2 else "STBL"
    print(f"  {tag}{label}  {p.address}"
          f"  {p.token0.symbol}/{p.token1.symbol}"
          f"  fee={p.fee_bps}bps"
          f"  tvl=${p.tvl_usd:,.0f}"
          f"  t0={p.token0.address}  t1={p.token1.address}")


def _print_v3cl_pool(p: V3CLPool, is_new: bool) -> None:
    tag = "NEW " if is_new else "    "
    print(f"  {tag}V3CL  {p.address}"
          f"  {p.token0.symbol}/{p.token1.symbol}"
          f"  fee={p.fee_ppm}ppm  tick={p.tick_spacing}"
          f"  tvl=${p.tvl_usd:,.0f}"
          f"  t0={p.token0.address}  t1={p.token1.address}")


# -- CLI -----------------------------------------------------------------------

def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(
        description="Fetch Pharaoh AMM and/or V3-CL pools into the bot cache CSVs",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )

    p.add_argument("--chain",          default="avalanche",
                   help="Chain identifier used to locate config (default: avalanche)")
    p.add_argument("--config-path",    default="",
                   help="Explicit path to the chain TOML config")
    p.add_argument("--api-url",        default=PHARAOH_API_URL,
                   help="Pharaoh API URL (default: %(default)s)")

    p.add_argument("--min-tvl",        type=float, default=10_000.0,
                   help="Minimum pool TVL in USD (default: %(default)s)")

    p.add_argument("--amm-out",        default="",
                   help="Override AMM output CSV path")
    p.add_argument("--cl-out",         default="",
                   help="Override V3CL output CSV path")

    # Scope flags (default: both)
    p.add_argument("--amm",            action="store_true",
                   help="Fetch AMM (non-CL) pools only")
    p.add_argument("--cl",             action="store_true",
                   help="Fetch V3CL pools only")
    p.add_argument("--include-stable", action="store_true",
                   help="Also include Solidly stable AMM pools (version=4)")

    p.add_argument("--print-pools",    action="store_true",
                   help="Print each pool to stdout")
    p.add_argument("--dry-run",        action="store_true",
                   help="Print what would be written but do not write any files")

    return p.parse_args()


# -- Main ----------------------------------------------------------------------

def main() -> int:
    args = parse_args()

    # Config
    cfg         = _load_chain_config(args.chain, args.config_path.strip() or None)
    main_tokens = _main_tokens(cfg)
    cache_dir   = (cfg.get("chain") or {}).get("cache_dir") or args.chain

    do_amm = args.amm or (not args.amm and not args.cl)
    do_cl  = args.cl  or (not args.amm and not args.cl)

    amm_csv = args.amm_out.strip() or os.path.join("cache", cache_dir, ".cached-pools.csv")
    cl_csv  = args.cl_out.strip()  or os.path.join("cache", cache_dir, ".cached-v3cl-pools.csv")

    print(f"[config] chain        : {args.chain}")
    print(f"[config] main_tokens  : {len(main_tokens)}")
    print(f"[config] min_tvl      : ${args.min_tvl:,.0f}")
    if do_amm:
        print(f"[config] AMM csv      : {amm_csv}")
    if do_cl:
        print(f"[config] V3CL csv     : {cl_csv}")
    if args.dry_run:
        print("[config] DRY RUN -- no files will be written")

    # Fetch from API
    print(f"\n[fetch]  {args.api_url} ...")
    raw_pools = _fetch_pools(args.api_url)
    print(f"[fetch]  {len(raw_pools)} pools returned by API")

    total_written = 0

    # AMM: volatile (version=2)
    if do_amm:
        existing_amm = _load_existing_addresses(amm_csv, col=0)
        print(f"\n[amm]    existing rows   : {len(existing_amm)}")

        vol_pools = filter_amm_pools(raw_pools, main_tokens, args.min_tvl, stable=False)
        new_vol   = [p for p in vol_pools if p.address not in existing_amm]
        print(f"[amm]    volatile (v=2)  : {len(vol_pools)} total  ({len(new_vol)} new)")
        if vol_pools:
            _print_amm_summary(vol_pools, "volatile")

        # AMM: stable (version=4) -- optional
        stbl_pools: List[AmmPool] = []
        new_stbl:   List[AmmPool] = []
        if args.include_stable:
            stbl_pools = filter_amm_pools(raw_pools, main_tokens, args.min_tvl, stable=True)
            new_stbl   = [p for p in stbl_pools if p.address not in existing_amm]
            print(f"[amm]    stable  (v=4)  : {len(stbl_pools)} total  ({len(new_stbl)} new)")
            if stbl_pools:
                _print_amm_summary(stbl_pools, "stable")

        if args.print_pools or args.dry_run:
            for p in vol_pools:
                _print_amm_pool(p, is_new=(p.address not in existing_amm), version=2)
            for p in stbl_pools:
                _print_amm_pool(p, is_new=(p.address not in existing_amm), version=4)

        if not args.dry_run:
            w = write_amm_pools(amm_csv, vol_pools, existing_amm, version=2)
            total_written += w
            print(f"[amm]    wrote {w} volatile pools -> {amm_csv}")
            if args.include_stable and stbl_pools:
                w2 = write_amm_pools(amm_csv, stbl_pools, existing_amm, version=4)
                total_written += w2
                print(f"[amm]    wrote {w2} stable  pools -> {amm_csv}")
        else:
            print(f"[amm]    would write {len(new_vol)} volatile + {len(new_stbl)} stable pools")

    # V3CL: concentrated liquidity
    if do_cl:
        # Load existing rows as dict[address -> row] so we can detect fee changes
        existing_cl_rows = _load_existing_v3cl_rows(cl_csv)
        existing_cl = set(existing_cl_rows.keys())  # set for quick membership tests
        print(f"\n[v3cl]   existing rows   : {len(existing_cl)}")

        cl_pools = filter_v3cl_pools(raw_pools, main_tokens, args.min_tvl)
        new_cl   = [p for p in cl_pools if p.address not in existing_cl]
        print(f"[v3cl]   CL pools        : {len(cl_pools)} total  ({len(new_cl)} new)")
        if cl_pools:
            _print_v3cl_summary(cl_pools)

        if args.print_pools or args.dry_run:
            for p in cl_pools:
                _print_v3cl_pool(p, is_new=(p.address not in existing_cl))

        w = write_v3cl_pools(cl_csv, cl_pools, existing_cl_rows, dry_run=args.dry_run)
        if not args.dry_run:
            total_written += w
            print(f"[v3cl]   wrote/updated {w} CL pool rows -> {cl_csv}")
        else:
            print(f"[v3cl]   would write {len(new_cl)} new CL pools (fee updates shown above)")

    # Summary
    if args.dry_run:
        print("\n[done]  DRY RUN -- no files were written.")
    else:
        print(f"\n[done]  {total_written} new pool rows written in total.")

    return 0


if __name__ == "__main__":
    sys.exit(main())
