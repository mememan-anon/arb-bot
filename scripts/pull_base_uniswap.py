#!/usr/bin/env python3
"""
Fetch Uniswap V2, V3, and V4 pools on Base via The Graph subgraphs and write
them into the bot's pool caches.

Output files:
  cache/base/.cached-pools.csv        ← V2 volatile pairs
  cache/base/.cached-v3cl-pools.csv   ← V3 concentrated-liquidity
  cache/base/.cached-v4cl-pools.csv   ← V4 concentrated-liquidity (data only;
                                           execution needs V4 contract support)

Usage:
  python scripts/pull_base_uniswap.py                     # all three
  python scripts/pull_base_uniswap.py --v2                 # V2 only
  python scripts/pull_base_uniswap.py --v3                 # V3 only
  python scripts/pull_base_uniswap.py --v4                 # V4 only
  python scripts/pull_base_uniswap.py --min-tvl 100000     # higher TVL cutoff
  python scripts/pull_base_uniswap.py --dry-run            # preview, no writes

The Graph API key and subgraph IDs are hard-coded below and can also be
overridden via CLI flags.

"""

from __future__ import annotations

import argparse
import csv
import json
import os
import sys
import time
import urllib.error
import urllib.request
from typing import Any, Dict, List, NamedTuple, Optional, Set

try:
    import tomllib  # py311+
except ImportError:
    try:
        import tomli as tomllib  # type: ignore[no-redef]
    except ImportError:
        tomllib = None  # type: ignore[assignment]

# ─── The Graph endpoints ────────────────────────────────────────────────────

GRAPH_GATEWAY_URL = "https://gateway.thegraph.com/api/{api_key}/subgraphs/id/{subgraph_id}"
GRAPH_LEGACY_URL  = "https://gateway.thegraph.com/api/subgraphs/id/{subgraph_id}"

# V4 uses the same authenticated gateway endpoint as V2/V3
SUBGRAPH_V4_LEGACY = False

DEFAULT_API_KEY     = "b4bae262bb9edd76e5034f00732d20cd"
SUBGRAPH_V2         = "D31gzGUtVNhHNdnxeELUBdch5rzDRm5cddvae9GzhCLu"
SUBGRAPH_V3         = "HMuAwufqZ1YCRmzL2SfHTVkzZovC9VL2UAKhjvRqKiR1"
SUBGRAPH_V4         = "7SP2t3PQd7LX19riCfwX5znhFdULjwRofQZtRZMJ8iW8"

DEFAULT_MIN_TVL     = None        # global override (optional)
DEFAULT_MIN_TVL_V2  =  50_000.0
DEFAULT_MIN_TVL_V3  =  50_000.0
DEFAULT_MIN_TVL_V4  =  50_000.0
PAGE_SIZE           = 1000      # max per The Graph page
RETRY_DELAY_S       = 5         # seconds between retries
MAX_RETRIES         = 3

# ─── CSV schemas ────────────────────────────────────────────────────────────

# Matches blackhole / pools.rs V2 loader
V2_CSV_HEADER = [
    "address", "version", "token0", "token1",
    "decimals0", "decimals1", "fee",
    "block_number", "timestamp", "id",
]

# Matches pharaoh / uniswapv3cl.rs loader
V3CL_CSV_HEADER = [
    "protocol", "address",
    "token0_symbol", "token1_symbol",
    "tickSpacing", "fee",
    "token0_address", "token1_address",
    "decimals0", "decimals1",
    "dex",
]

# ─── Data classes ───────────────────────────────────────────────────────────

class TokenInfo(NamedTuple):
    address:  str
    symbol:   str
    decimals: int


class V2Pool(NamedTuple):
    address: str
    token0:  TokenInfo
    token1:  TokenInfo
    tvl_usd: float


class CLPool(NamedTuple):
    address:      str
    token0:       TokenInfo
    token1:       TokenInfo
    fee_ppm:      int
    tick_spacing: int
    tvl_usd:      float
    protocol:     str   # "UniswapV3CL" or "UniswapV4CL"
    dex:          str   # "UniswapV3"   or "UniswapV4"


# ─── Helpers ────────────────────────────────────────────────────────────────

def _repo_root() -> str:
    return os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def _load_chain_config(config_path: Optional[str], chain: str = "base") -> Dict[str, Any]:
    if not tomllib:
        raise RuntimeError("tomllib unavailable — use Python 3.11+ or: pip install tomli")
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
            raise RuntimeError(f"Config not found for chain '{chain}'. Tried: {candidates}")
    with open(path, "rb") as f:
        return tomllib.load(f)


def _main_tokens(cfg: Dict[str, Any]) -> Set[str]:
    """Lowercase addresses of start/main tokens — used to filter pairs."""
    out: Set[str] = set()
    for key in ("start_tokens", "main_tokens"):
        for tok in cfg.get(key) or []:
            if isinstance(tok, dict):
                addr = str(tok.get("address", "")).strip().lower()
                if addr:
                    out.add(addr)
    if not out:
        raise RuntimeError("No start_tokens / main_tokens found in config")
    return out


def _graph_url(api_key: str, subgraph_id: str, legacy: bool = False) -> str:
    if legacy:
        return GRAPH_LEGACY_URL.format(subgraph_id=subgraph_id)
    return GRAPH_GATEWAY_URL.format(api_key=api_key, subgraph_id=subgraph_id)


def _graphql_post(url: str, query: str, retries: int = MAX_RETRIES) -> Dict[str, Any]:
    """POST a GraphQL query, return parsed JSON. Retries on transient errors."""
    payload = json.dumps({"query": query}).encode("utf-8")
    req = urllib.request.Request(
        url=url, data=payload, method="POST",
        headers={
            "Content-Type":  "application/json",
            "User-Agent":    "arb-bot-base/1.0",
        },
    )
    last_err: Exception = RuntimeError("never set")
    for attempt in range(retries):
        try:
            with urllib.request.urlopen(req, timeout=60) as resp:
                body = resp.read().decode("utf-8")
            parsed = json.loads(body)
            if "errors" in parsed:
                err_msgs = [e.get("message", "?") for e in parsed["errors"]]
                raise RuntimeError(f"GraphQL errors: {err_msgs}")
            return parsed
        except (urllib.error.HTTPError, urllib.error.URLError, RuntimeError) as e:
            last_err = e
            if attempt < retries - 1:
                wait = RETRY_DELAY_S * (2 ** attempt)
                print(f"  [warn] request failed ({e}), retrying in {wait}s …", flush=True)
                time.sleep(wait)
    raise RuntimeError(f"GraphQL request failed after {retries} attempts: {last_err}")


def _parse_token(raw: Any, key_prefix: str = "token") -> Optional[TokenInfo]:
    """Parse a token/currency GraphQL object. Handles 'token0'/'currency0' keys."""
    if not isinstance(raw, dict):
        return None
    addr = str(raw.get("id") or raw.get("address") or "").strip().lower()
    if not addr or addr == "0x" + "0" * 40:
        return None
    # Native ETH in V4 is represented as address(0)
    if addr == "0x0000000000000000000000000000000000000000":
        return TokenInfo(address=addr, symbol="ETH", decimals=18)
    symbol   = str(raw.get("symbol") or "").strip()
    decimals = int(raw.get("decimals") or 18)
    return TokenInfo(address=addr, symbol=symbol, decimals=decimals)


def _load_existing_addresses(csv_path: str, col: int = 0) -> Set[str]:
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


def _load_existing_cl_rows(csv_path: str) -> Dict[str, List[str]]:
    """Load V3CL/V4CL CSV as dict[lowercase_address → row]."""
    rows: Dict[str, List[str]] = {}
    if not os.path.exists(csv_path):
        return rows
    with open(csv_path, "r", newline="", encoding="utf-8") as f:
        reader = csv.reader(f)
        next(reader, None)
        for row in reader:
            if row and len(row) > 1:
                rows[row[1].strip().lower()] = row
    return rows


# ─── Subgraph fetchers ───────────────────────────────────────────────────────

def fetch_v2_pools(
    url:      str,
    min_tvl:  float,
    main_tokens: Set[str],
) -> List[V2Pool]:
    """
    Pull all Uniswap V2 pairs from The Graph with reserveUSD >= min_tvl.
    Uses cursor-based pagination to avoid the 5,000-skip limit.
    """
    print(f"[v2]  fetching from subgraph …")
    pools:   List[V2Pool] = []
    last_id: str = ""
    page:    int = 0

    while True:
        where_clause = f'reserveUSD_gte: "{min_tvl}"'
        if last_id:
            where_clause += f', id_gt: "{last_id}"'

        query = f"""{{
  pairs(
    first: {PAGE_SIZE}
    orderBy: id
    orderDirection: asc
    where: {{ {where_clause} }}
  ) {{
    id
    token0 {{ id symbol decimals }}
    token1 {{ id symbol decimals }}
    reserveUSD
  }}
}}"""
        data  = _graphql_post(url, query)
        batch = (data.get("data") or {}).get("pairs") or []
        page += 1
        print(f"  page {page}: {len(batch)} pairs", end="\r", flush=True)

        for raw in batch:
            addr = str(raw.get("id") or "").strip().lower()
            if not addr:
                continue
            t0 = _parse_token(raw.get("token0"))
            t1 = _parse_token(raw.get("token1"))
            if t0 is None or t1 is None:
                continue
            if t0.address not in main_tokens and t1.address not in main_tokens:
                continue
            tvl = float(raw.get("reserveUSD") or 0)
            pools.append(V2Pool(address=addr, token0=t0, token1=t1, tvl_usd=tvl))
            last_id = addr

        if len(batch) < PAGE_SIZE:
            break

    print(f"  page {page}: done                          ")
    return pools


def fetch_cl_pools(
    url:         str,
    min_tvl:     float,
    main_tokens: Set[str],
    protocol:    str,
    dex:         str,
    v4:          bool = False,
) -> List[CLPool]:
    """
    Pull concentrated-liquidity pools from a V3 or V4 subgraph.

    V3 schema:  pools → token0/token1, feeTier, tickSpacing, totalValueLockedUSD
    V4 schema:  pools → currency0/currency1, fee, tickSpacing, totalValueLockedUSD
                (fee field name varies by subgraph deployment; we try both)
    """
    print(f"[{dex.lower()}]  fetching from subgraph …")
    pools:   List[CLPool] = []
    last_id: str = ""
    page:    int = 0

    # Both Uniswap V3 and V4 subgraphs on The Graph share the same field names:
    # token0/token1, feeTier, totalValueLockedUSD.
    # V4 additionally exposes tickSpacing; V3 does not (we derive from fee).
    token_fields = "token0 { id symbol decimals } token1 { id symbol decimals }"
    fee_field    = "feeTier"
    tvl_field    = "totalValueLockedUSD"

    while True:
        where_clause = f'{tvl_field}_gte: "{min_tvl}"'
        if last_id:
            where_clause += f', id_gt: "{last_id}"'

        # Request tickSpacing for V4 (it exists); skip for V3 (field absent in schema).
        tick_field_line = "tickSpacing" if v4 else ""
        extra_fields = f"\n    {tick_field_line}" if tick_field_line else ""
        query = f"""{{
  pools(
    first: {PAGE_SIZE}
    orderBy: id
    orderDirection: asc
    where: {{ {where_clause} }}
  ) {{
    id
    {token_fields}
    {fee_field}{extra_fields}
    {tvl_field}
  }}
}}"""
        data  = _graphql_post(url, query)
        batch = (data.get("data") or {}).get("pools") or []
        page += 1
        print(f"  page {page}: {len(batch)} pools", end="\r", flush=True)

        for raw in batch:
            addr = str(raw.get("id") or "").strip().lower()
            if not addr:
                continue

            t0 = _parse_token(raw.get("token0"))
            t1 = _parse_token(raw.get("token1"))
            if t0 is None or t1 is None:
                continue

            if t0.address not in main_tokens and t1.address not in main_tokens:
                continue

            try:
                fee_ppm = int(raw.get(fee_field) or 0)
            except (TypeError, ValueError):
                continue
            if fee_ppm <= 0:
                continue

            tick_spacing_raw = raw.get("tickSpacing")
            if tick_spacing_raw is not None:
                tick_spacing = int(tick_spacing_raw)
            else:
                # Derive from standard Uniswap V3 fee → tickSpacing mapping
                tick_spacing = {
                    100: 1, 500: 10, 3000: 60, 10000: 200,
                }.get(fee_ppm, 0)
            tvl = float(raw.get(tvl_field) or 0)

            pools.append(CLPool(
                address=addr, token0=t0, token1=t1,
                fee_ppm=fee_ppm, tick_spacing=tick_spacing, tvl_usd=tvl,
                protocol=protocol, dex=dex,
            ))
            last_id = addr

        if len(batch) < PAGE_SIZE:
            break

    print(f"  page {page}: done                          ")
    return pools


# ─── Writers ─────────────────────────────────────────────────────────────────

def write_v2_csv(
    csv_path:  str,
    pools:     List[V2Pool],
    existing:  Set[str],
    dry_run:   bool = False,
) -> int:
    """Append new V2 pools. Returns count written."""
    new_pools = [p for p in pools if p.address not in existing]
    if not new_pools:
        return 0
    if dry_run:
        return len(new_pools)

    os.makedirs(os.path.dirname(os.path.abspath(csv_path)), exist_ok=True)
    file_exists = os.path.exists(csv_path)

    with open(csv_path, "a", newline="", encoding="utf-8") as f:
        w = csv.writer(f)
        if not file_exists:
            w.writerow(V2_CSV_HEADER)
        for i, p in enumerate(new_pools):
            w.writerow([
                p.address,
                2,                       # version
                p.token0.address,
                p.token1.address,
                p.token0.decimals,
                p.token1.decimals,
                30,                      # Uniswap V2 fee = 0.30% = 30 bps
                0,                       # block_number
                0,                       # timestamp
                len(existing) + i,       # id
            ])
            existing.add(p.address)

    return len(new_pools)


def write_cl_csv(
    csv_path:      str,
    pools:         List[CLPool],
    existing_rows: Dict[str, List[str]],
    dry_run:       bool = False,
) -> int:
    """
    Write CL pools (V3 or V4) to CSV.
    - Appends new pools.
    - Updates fee in-place for existing pools whose fee changed.
    Returns count written/updated.
    """
    existing_addrs = set(existing_rows.keys())
    new_pools = [p for p in pools if p.address not in existing_addrs]

    # Detect fee changes
    fee_updates: Dict[str, CLPool] = {}
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
            print(f"  [cl] {len(fee_updates)} fee updates would be applied")
        return len(new_pools)

    if not new_pools and not fee_updates:
        return 0

    def _make_row(p: CLPool) -> List:
        return [
            p.protocol,
            p.address,
            p.token0.symbol,
            p.token1.symbol,
            p.tick_spacing,
            p.fee_ppm,
            p.token0.address,
            p.token1.address,
            p.token0.decimals,
            p.token1.decimals,
            p.dex,
        ]

    os.makedirs(os.path.dirname(os.path.abspath(csv_path)), exist_ok=True)

    if fee_updates:
        updated = dict(existing_rows)
        for addr, p in fee_updates.items():
            row = list(updated[addr])
            row[5] = str(p.fee_ppm)
            updated[addr] = row
        with open(csv_path, "w", newline="", encoding="utf-8") as f:
            w = csv.writer(f)
            w.writerow(V3CL_CSV_HEADER)
            for row in updated.values():
                w.writerow(row)
            for p in new_pools:
                w.writerow(_make_row(p))
    else:
        file_exists = os.path.exists(csv_path)
        with open(csv_path, "a", newline="", encoding="utf-8") as f:
            w = csv.writer(f)
            if not file_exists:
                w.writerow(V3CL_CSV_HEADER)
            for p in new_pools:
                w.writerow(_make_row(p))

    return len(new_pools) + len(fee_updates)


# ─── Summary helpers ─────────────────────────────────────────────────────────

def _summarise_v2(pools: List[V2Pool]) -> None:
    tvls = sorted(p.tvl_usd for p in pools)
    if tvls:
        print(f"  count={len(pools)}  TVL ${tvls[0]:,.0f}–${tvls[-1]:,.0f}  "
              f"(median ${tvls[len(tvls)//2]:,.0f})")


def _summarise_cl(pools: List[CLPool]) -> None:
    from collections import Counter
    fees = Counter(p.fee_ppm for p in pools)
    tvls = sorted(p.tvl_usd for p in pools)
    if tvls:
        print(f"  count={len(pools)}  fees_ppm={dict(sorted(fees.items()))}")
        print(f"  TVL ${tvls[0]:,.0f}–${tvls[-1]:,.0f}  (median ${tvls[len(tvls)//2]:,.0f})")


# ─── CLI ─────────────────────────────────────────────────────────────────────

def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(
        description="Fetch Uniswap V2/V3/V4 pools on Base via The Graph",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=__doc__,
    )
    p.add_argument("--chain",        default="base",
                   help="Chain name, used to resolve config (default: base)")
    p.add_argument("--config-path",  default="",
                   help="Explicit path to chain TOML config")
    p.add_argument("--api-key",      default=DEFAULT_API_KEY,
                   help="The Graph API key")
    p.add_argument("--v2-subgraph",  default=SUBGRAPH_V2)
    p.add_argument("--v3-subgraph",  default=SUBGRAPH_V3)
    p.add_argument("--v4-subgraph",  default=SUBGRAPH_V4)
    p.add_argument("--min-tvl",      type=float, default=None,
                   help="Global minimum pool TVL in USD (overrides per-protocol defaults)")
    p.add_argument("--min-tvl-v2",   type=float, default=DEFAULT_MIN_TVL_V2,
                   help=f"V2 minimum TVL in USD (default: {DEFAULT_MIN_TVL_V2:,.0f})")
    p.add_argument("--min-tvl-v3",   type=float, default=DEFAULT_MIN_TVL_V3,
                   help=f"V3 minimum TVL in USD (default: {DEFAULT_MIN_TVL_V3:,.0f})")
    p.add_argument("--min-tvl-v4",   type=float, default=DEFAULT_MIN_TVL_V4,
                   help=f"V4 minimum TVL in USD (default: {DEFAULT_MIN_TVL_V4:,.0f})")
    p.add_argument("--v2-out",       default="",
                   help="Override V2 output CSV path")
    p.add_argument("--v3-out",       default="",
                   help="Override V3 output CSV path")
    p.add_argument("--v4-out",       default="",
                   help="Override V4 output CSV path")

    # Scope flags — default is all three
    p.add_argument("--v2",           action="store_true", help="Fetch V2 only")
    p.add_argument("--v3",           action="store_true", help="Fetch V3 only")
    p.add_argument("--v4",           action="store_true", help="Fetch V4 only")

    p.add_argument("--print-pools",  action="store_true")
    p.add_argument("--dry-run",      action="store_true",
                   help="Preview — print counts but write no files")
    return p.parse_args()


# ─── Main ─────────────────────────────────────────────────────────────────────

def main() -> int:
    args = parse_args()

    cfg         = _load_chain_config(args.config_path.strip() or None, args.chain)
    main_tokens = _main_tokens(cfg)
    cache_dir   = (cfg.get("chain") or {}).get("cache_dir") or args.chain

    # Per-protocol TVL thresholds — global --min-tvl overrides all
    tvl_v2 = args.min_tvl if args.min_tvl is not None else args.min_tvl_v2
    tvl_v3 = args.min_tvl if args.min_tvl is not None else args.min_tvl_v3
    tvl_v4 = args.min_tvl if args.min_tvl is not None else args.min_tvl_v4

    # Determine which variants to run
    explicit = args.v2 or args.v3 or args.v4
    do_v2 = args.v2 or not explicit
    do_v3 = args.v3 or not explicit
    do_v4 = args.v4 or not explicit

    # Output paths
    v2_csv = args.v2_out.strip() or os.path.join("cache", cache_dir, ".cached-pools.csv")
    v3_csv = args.v3_out.strip() or os.path.join("cache", cache_dir, ".cached-v3cl-pools.csv")
    v4_csv = args.v4_out.strip() or os.path.join("cache", cache_dir, ".cached-v4cl-pools.csv")

    # Config summary
    print(f"[config] chain        : {args.chain}")
    print(f"[config] main_tokens  : {sorted(main_tokens)}")
    print(f"[config] min_tvl      : V2=${tvl_v2:,.0f}  V3=${tvl_v3:,.0f}  V4=${tvl_v4:,.0f}")
    print(f"[config] fetch        : "
          f"{'V2 ' if do_v2 else ''}{'V3 ' if do_v3 else ''}{'V4' if do_v4 else ''}")
    if args.dry_run:
        print("[config] DRY RUN — no files will be written")
    print()

    total_written = 0
    errors: List[str] = []

    # ── Uniswap V2 ────────────────────────────────────────────────────────
    if do_v2:
        try:
            v2_url = _graph_url(args.api_key, args.v2_subgraph)
            pools  = fetch_v2_pools(v2_url, tvl_v2, main_tokens)
            print(f"[v2]  {len(pools)} pools pass filter (TVL ≥ ${tvl_v2:,.0f}, main-token pair)")
            _summarise_v2(pools)

            if args.print_pools:
                for p in pools:
                    print(f"  {p.address}  {p.token0.symbol}/{p.token1.symbol}  tvl=${p.tvl_usd:,.0f}")

            existing = _load_existing_addresses(v2_csv)
            print(f"[v2]  existing csv rows : {len(existing)}")
            written  = write_v2_csv(v2_csv, pools, existing, dry_run=args.dry_run)
            if args.dry_run:
                print(f"[v2]  would write {written} new rows → {v2_csv}")
            else:
                print(f"[v2]  wrote {written} new rows → {v2_csv}")
            total_written += written
        except Exception as exc:
            print(f"[v2]  ERROR: {exc}")
            print(f"[v2]  Skipping — check --v2-subgraph ID or API key")
            errors.append(f"V2: {exc}")
        print()

    # ── Uniswap V3 ────────────────────────────────────────────────────────
    if do_v3:
        try:
            v3_url = _graph_url(args.api_key, args.v3_subgraph)
            pools  = fetch_cl_pools(
                v3_url, tvl_v3, main_tokens,
                protocol="UniswapV3CL", dex="UniswapV3", v4=False,
            )
            print(f"[v3]  {len(pools)} pools pass filter (TVL ≥ ${tvl_v3:,.0f}, main-token pair)")
            _summarise_cl(pools)

            if args.print_pools:
                for p in pools:
                    print(f"  {p.address}  {p.token0.symbol}/{p.token1.symbol}"
                          f"  fee={p.fee_ppm}ppm  tick={p.tick_spacing}  tvl=${p.tvl_usd:,.0f}")

            existing = _load_existing_cl_rows(v3_csv)
            print(f"[v3]  existing csv rows : {len(existing)}")
            written  = write_cl_csv(v3_csv, pools, existing, dry_run=args.dry_run)
            if args.dry_run:
                print(f"[v3]  would write {written} new rows → {v3_csv}")
            else:
                print(f"[v3]  wrote/updated {written} rows → {v3_csv}")
            total_written += written
        except Exception as exc:
            print(f"[v3]  ERROR: {exc}")
            print(f"[v3]  Skipping — check --v3-subgraph ID or API key")
            errors.append(f"V3: {exc}")
        print()

    # ── Uniswap V4 ────────────────────────────────────────────────────────
    if do_v4:
        try:
            v4_url = _graph_url(args.api_key, args.v4_subgraph, legacy=SUBGRAPH_V4_LEGACY)
            pools  = fetch_cl_pools(
                v4_url, tvl_v4, main_tokens,
                protocol="UniswapV4CL", dex="UniswapV4", v4=True,
            )
            print(f"[v4]  {len(pools)} pools pass filter (TVL ≥ ${tvl_v4:,.0f}, main-token pair)")
            _summarise_cl(pools)

            if args.print_pools:
                for p in pools:
                    print(f"  {p.address}  {p.token0.symbol}/{p.token1.symbol}"
                          f"  fee={p.fee_ppm}ppm  tick={p.tick_spacing}  tvl=${p.tvl_usd:,.0f}")
            print("  [note] V4 pools stored for data; execution needs V4 contract support")

            existing = _load_existing_cl_rows(v4_csv)
            print(f"[v4]  existing csv rows : {len(existing)}")
            written  = write_cl_csv(v4_csv, pools, existing, dry_run=args.dry_run)
            if args.dry_run:
                print(f"[v4]  would write {written} new rows → {v4_csv}")
            else:
                print(f"[v4]  wrote/updated {written} rows → {v4_csv}")
            total_written += written
        except Exception as exc:
            print(f"[v4]  ERROR: {exc}")
            print(f"[v4]  Skipping — check --v4-subgraph ID or API key")
            errors.append(f"V4: {exc}")
        print()

    if args.dry_run:
        print(f"[done] DRY RUN — {total_written} rows would be written in total.")
    else:
        print(f"[done] {total_written} total new rows written.")

    if errors:
        print("\n[warn] The following phases encountered errors:")
        for err in errors:
            print(f"  • {err}")
        print("\n  The Graph decentralized network can have temporary indexer outages.")
        print("  To retry a specific phase: python scripts/pull_base_uniswap.py --v3")
        print("  To use a different subgraph: --v3-subgraph <ID>")
        return 1

    return 0


if __name__ == "__main__":
    sys.exit(main())
