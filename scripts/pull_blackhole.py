#!/usr/bin/env python3
"""
Fetch Blackhole DEX pools and write them to the bot's pool caches.

Blackhole uses two AMM architectures:
  AMM      — x*y=k V2 pairs from on-chain factory
               → cache/<chain>/.cached-pools.csv
  Algebra  — Concentrated liquidity (Algebra Integral, globalState() ABI)
               → cache/<chain>/.cached-algebra-pools.csv

Usage examples:
  python scripts/pull_blackhole.py                    # both V2 + Algebra
  python scripts/pull_blackhole.py --v2               # V2 only
  python scripts/pull_blackhole.py --algebra          # Algebra CL only
  python scripts/pull_blackhole.py --min-tvl 50000    # higher TVL threshold
  python scripts/pull_blackhole.py --algebra --refresh-json  # re-download JSON first
"""

from __future__ import annotations

import argparse
import csv
import json
import os
import sys
import urllib.error
import urllib.request
from typing import Any, Dict, Iterable, List, Set

try:
    import tomllib  # py311+
except Exception:
    tomllib = None  # type: ignore[assignment]

# Blackhole public endpoints
BH_CL_POOLS_URL = "https://resources.blackhole.xyz/cl-pools-list/cl-pools.json"

# On-chain selectors for V2 factory calls
SEL_ALL_PAIRS_LENGTH = "0x574f2ba3"
SEL_ALL_PAIRS        = "0x1e3dd18b"
SEL_TOKEN0           = "0x0dfe1681"
SEL_TOKEN1           = "0xd21220a7"
SEL_STABLE           = "0x22be3de1"
SEL_GET_FEE          = "0xcc56b2c5"  # getFee(address pair, bool stable)

V2_CSV_HEADER      = ["address", "version", "token0", "token1", "decimals0",
                      "decimals1", "fee", "block_number", "timestamp", "id"]
ALGEBRA_CSV_HEADER = ["id", "address", "version", "token0", "token1",
                      "fee", "block_number", "timestamp", "tick_spacing"]


# ── helpers ───────────────────────────────────────────────────────────────────

def _repo_root() -> str:
    return os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def _load_chain_config(chain: str, config_path: str | None) -> Dict[str, Any]:
    if not tomllib:
        raise RuntimeError("tomllib unavailable: use Python 3.11+ or pip install tomli")
    if config_path:
        path = config_path
    else:
        path = os.path.join(_repo_root(), "config", "chains", f"{chain}.toml")
        if not os.path.exists(path):
            alt = os.path.join(_repo_root(), "config", f"{chain}.toml")
            if os.path.exists(alt):
                path = alt
            else:
                raise RuntimeError(f"missing chain config: {path}")
    with open(path, "rb") as f:
        return tomllib.load(f)


def _main_tokens(cfg: Dict[str, Any]) -> Set[str]:
    out: Set[str] = set()
    for key in ("main_tokens", "start_tokens"):
        for token in cfg.get(key) or []:
            if not isinstance(token, dict):
                continue
            addr = str(token.get("address", "")).strip().lower()
            if addr:
                out.add(addr)
    if not out:
        raise RuntimeError("main_tokens / start_tokens missing in chain config")
    return out


# ── RPC helpers ───────────────────────────────────────────────────────────────

def _post_json(url: str, payload: Dict[str, Any], timeout: int = 30) -> Dict[str, Any]:
    data = json.dumps(payload).encode("utf-8")
    req = urllib.request.Request(
        url=url, data=data, method="POST",
        headers={"Content-Type": "application/json", "User-Agent": "arb-bot-blackhole/1.0"},
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return json.loads(resp.read().decode("utf-8"))
    except urllib.error.HTTPError as e:
        body = e.read().decode("utf-8", errors="replace") if hasattr(e, "read") else ""
        raise RuntimeError(f"RPC HTTP {e.code}: {body}") from e
    except urllib.error.URLError as e:
        raise RuntimeError(f"RPC URL error: {e}") from e


def _eth_call(rpc_url: str, to: str, data: str) -> str:
    resp = _post_json(rpc_url, {
        "jsonrpc": "2.0", "id": 1, "method": "eth_call",
        "params": [{"to": to, "data": data}, "latest"],
    })
    if resp.get("error"):
        raise RuntimeError(f"eth_call error: {resp['error']}")
    return str(resp.get("result") or "0x")


def _hex_to_int(value: str) -> int:
    return int(value, 16) if value and value != "0x" else 0


def _decode_address(word_hex: str) -> str:
    h = word_hex.lower().replace("0x", "")
    if len(h) < 40:
        return "0x" + "0" * 40
    return "0x" + h[-40:]


def _encode_uint256(value: int) -> str:
    return f"{value:064x}"


# ── Algebra CL JSON ───────────────────────────────────────────────────────────

def _fetch_cl_json(cl_url: str) -> List[Dict[str, Any]]:
    req = urllib.request.Request(cl_url, headers={"User-Agent": "arb-bot-blackhole/1.0"})
    with urllib.request.urlopen(req, timeout=30) as resp:
        data = json.loads(resp.read().decode("utf-8"))
    pools = data.get("pools") if isinstance(data, dict) else None
    if not isinstance(pools, list):
        raise RuntimeError("unexpected CL pools payload format")
    return pools


def _write_algebra_csv(
    out_csv: str,
    main_tokens: Set[str],
    cl_pools: Iterable[Dict[str, Any]],
    min_tvl: float = 0.0,
) -> int:
    os.makedirs(os.path.dirname(out_csv) or ".", exist_ok=True)
    written = 0
    with open(out_csv, "w", newline="", encoding="utf-8") as f:
        w = csv.writer(f)
        w.writerow(ALGEBRA_CSV_HEADER)
        for pool in cl_pools:
            if not isinstance(pool, dict):
                continue
            tvl = float(pool.get("totalValueLockedUSD") or pool.get("tvl") or 0)
            if tvl < min_tvl:
                continue
            token0 = str(((pool.get("token0") or {}).get("id") or "")).strip().lower()
            token1 = str(((pool.get("token1") or {}).get("id") or "")).strip().lower()
            if not token0 or not token1:
                continue
            if token0 not in main_tokens and token1 not in main_tokens:
                continue
            address = str(pool.get("id") or "").strip().lower()
            if not address:
                continue
            fee          = int(str(pool.get("fee") or "0"))
            tick_spacing = int(str(pool.get("tickSpacing") or "0"))
            w.writerow([written, address, 3, token0, token1, fee, 0, 0, tick_spacing])
            written += 1
    return written


# ── V2 factory ────────────────────────────────────────────────────────────────

def _load_token_decimals(tokens_json: str | None) -> Dict[str, int]:
    if not tokens_json or not os.path.exists(tokens_json):
        return {}
    with open(tokens_json, "r", encoding="utf-8-sig") as f:
        data = json.load(f)
    out: Dict[str, int] = {}
    if isinstance(data, dict):
        for addr, meta in data.items():
            try:
                dec = int(meta.get("decimal") or meta.get("decimals") or 18)
                out[str(addr).lower()] = dec
            except Exception:
                continue
    return out


def _query_pair_fee(rpc_url: str, factory: str, pair: str, stable: bool, default_fee: int) -> int:
    """Call getFee(pair, stable) on the factory to get the live swap fee in bps.

    Blackhole V2 pairs can have per-pool fees set via the factory.  Using the
    factory value ensures our K-invariant simulation matches what the pair
    actually enforces on-chain.
    """
    # Encode: getFee(address pair, bool stable)
    # selector cc56b2c5 + address (padded 32 bytes) + bool (padded 32 bytes)
    addr_padded   = pair.lower().replace("0x", "").zfill(64)
    stable_padded = ("1" if stable else "0").zfill(64)
    data = SEL_GET_FEE + addr_padded + stable_padded
    try:
        result = _eth_call(rpc_url, factory, data)
        fee = _hex_to_int(result)
        if fee > 0:
            return fee
    except Exception:
        pass
    return default_fee


def _write_v2_csv(
    out_csv: str,
    rpc_url: str,
    v2_factory: str,
    main_tokens: Set[str],
    decimals_by_addr: Dict[str, int],
    fee_bps: int = 300,
) -> int:
    os.makedirs(os.path.dirname(out_csv) or ".", exist_ok=True)

    length = _hex_to_int(_eth_call(rpc_url, v2_factory, SEL_ALL_PAIRS_LENGTH))
    print(f"[v2]     factory has {length} pairs")

    written = 0
    with open(out_csv, "w", newline="", encoding="utf-8") as f:
        w = csv.writer(f)
        w.writerow(V2_CSV_HEADER)
        for idx in range(length):
            pair_data = SEL_ALL_PAIRS + _encode_uint256(idx)
            pair      = _decode_address(_eth_call(rpc_url, v2_factory, pair_data))
            if pair == "0x" + "0" * 40:
                continue
            token0 = _decode_address(_eth_call(rpc_url, pair, SEL_TOKEN0))
            token1 = _decode_address(_eth_call(rpc_url, pair, SEL_TOKEN1))
            stable = _hex_to_int(_eth_call(rpc_url, pair, SEL_STABLE)) != 0
            if stable:
                continue  # stable V2 handled separately
            if token0.lower() not in main_tokens and token1.lower() not in main_tokens:
                continue
            dec0 = decimals_by_addr.get(token0.lower(), 18)
            dec1 = decimals_by_addr.get(token1.lower(), 18)
            # Query live swap fee from factory — per-pool fees may differ from
            # the global default (e.g. Blackhole charges 150 bps on some pairs).
            live_fee = _query_pair_fee(rpc_url, v2_factory, pair, stable, fee_bps)
            w.writerow([pair.lower(), 2, token0.lower(), token1.lower(),
                        dec0, dec1, live_fee, 0, 0, written])
            written += 1
            if written % 10 == 0:
                print(f"  ... {written} V2 pairs written", end="\r", flush=True)

    print()
    return written


# ── CLI ───────────────────────────────────────────────────────────────────────

def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(
        description="Fetch Blackhole AMM (V2) and/or Algebra CL pools into the bot's cache CSVs"
    )
    p.add_argument("--chain",          default="avalanche")
    p.add_argument("--config-path",    default="")
    p.add_argument("--rpc-url",        default="https://api.avax.network/ext/bc/C/rpc")
    p.add_argument("--v2-factory",     default="0xfe926062fb99ca5653080d6c14fe945ad68c265c")
    p.add_argument("--cl-url",         default=BH_CL_POOLS_URL,
                   help="URL for Blackhole CL pools JSON")
    p.add_argument("--cl-json",        default="",
                   help="Local JSON file to use instead of fetching (skips network)")
    p.add_argument("--tokens-json",    default="",
                   help="Token metadata JSON for decimal lookup (optional)")
    p.add_argument("--min-tvl",        type=float, default=0.0,
                   help="Min TVL (USD) for Algebra CL pools (default: 0 = no filter)")
    p.add_argument("--fee-bps",        type=int, default=None,
                   help="Override V2 fee bps (default: read from config [dexes.v2].fee_bps)")
    p.add_argument("--v2",             action="store_true",
                   help="Fetch V2 factory pools only (skip Algebra CL)")
    p.add_argument("--algebra",        action="store_true",
                   help="Fetch Algebra CL pools only (skip V2)")
    p.add_argument("--refresh-json",   action="store_true",
                   help="Re-download blackhole_cl_pools.json even if a local file exists")
    p.add_argument("--v2-out",         default="",
                   help="Override V2 output CSV")
    p.add_argument("--algebra-out",    default="",
                   help="Override Algebra output CSV")
    p.add_argument("--dry-run",        action="store_true")
    return p.parse_args()


def main() -> int:
    args = parse_args()

    cfg         = _load_chain_config(args.chain, args.config_path.strip() or None)
    main_tokens = _main_tokens(cfg)
    cache_dir   = (cfg.get("chain") or {}).get("cache_dir") or args.chain

    do_v2      = args.v2      or (not args.v2 and not args.algebra)
    do_algebra = args.algebra or (not args.v2 and not args.algebra)

    v2_csv      = args.v2_out.strip()      or os.path.join("cache", cache_dir, ".cached-pools.csv")
    algebra_csv = args.algebra_out.strip() or os.path.join("cache", cache_dir, ".cached-algebra-pools.csv")

    json_cache_path = os.path.join(_repo_root(), "cache", cache_dir, "blackhole_cl_pools.json")

    print(f"[config] chain       : {args.chain}")
    print(f"[config] main_tokens : {sorted(main_tokens)}")
    print(f"[config] min_tvl     : ${args.min_tvl:,.0f}")

    # ── Algebra CL ────────────────────────────────────────────────────────────
    if do_algebra:
        print(f"\n[algebra] output csv : {algebra_csv}")

        # Use provided --cl-json file, otherwise always fetch live from URL.
        # --refresh-json is kept as a no-op alias so existing scripts don't break.
        local_json = args.cl_json.strip()
        if local_json and os.path.exists(local_json):
            print(f"[algebra] using local JSON override: {local_json}")
            with open(local_json, "r", encoding="utf-8-sig") as f:
                data = json.load(f)
            cl_pools = data.get("pools") if isinstance(data, dict) else []
        else:
            print(f"[algebra] fetching live: {args.cl_url}")
            cl_pools = _fetch_cl_json(args.cl_url)
            # Save snapshot for audit / manual inspection (never used as cache)
            os.makedirs(os.path.dirname(json_cache_path), exist_ok=True)
            with open(json_cache_path, "w", encoding="utf-8") as f:
                json.dump({"pools": cl_pools}, f, separators=(",", ":"))
            print(f"[algebra] snapshot saved → {json_cache_path}")

        print(f"[algebra] total CL pools in JSON: {len(cl_pools)}")

        if args.dry_run:
            # Just count without writing
            eligible = [
                p for p in cl_pools
                if isinstance(p, dict)
                and float(p.get("totalValueLockedUSD") or 0) >= args.min_tvl
                and (
                    str(((p.get("token0") or {}).get("id") or "")).strip().lower() in main_tokens
                    or str(((p.get("token1") or {}).get("id") or "")).strip().lower() in main_tokens
                )
            ]
            print(f"[algebra] [dry-run] would write {len(eligible)} pools")
        else:
            written = _write_algebra_csv(algebra_csv, main_tokens, cl_pools, args.min_tvl)
            print(f"[algebra] wrote {written} pools → {algebra_csv}")

    # ── V2 factory ────────────────────────────────────────────────────────────
    if do_v2:
        print(f"\n[v2]     output csv  : {v2_csv}")
        print(f"[v2]     factory     : {args.v2_factory}")
        print(f"[v2]     rpc         : {args.rpc_url}")

        config_fee = (cfg.get("dexes") or {}).get("v2", {}).get("fee_bps", None)
        fee_bps    = args.fee_bps if args.fee_bps is not None else (config_fee if config_fee is not None else 300)
        print(f"[v2]     fee_bps     : {fee_bps}")

        decimals_by_addr = _load_token_decimals(args.tokens_json.strip() or None)

        if args.dry_run:
            length_raw = _eth_call(args.rpc_url, args.v2_factory, SEL_ALL_PAIRS_LENGTH)
            length = _hex_to_int(length_raw)
            print(f"[v2]     [dry-run] factory has {length} pairs (no file written)")
        else:
            written = _write_v2_csv(v2_csv, args.rpc_url, args.v2_factory,
                                    main_tokens, decimals_by_addr, fee_bps=fee_bps)
            print(f"[v2]     wrote {written} pairs → {v2_csv}")

    if args.dry_run:
        print("\n[dry-run] No files written.")

    return 0


if __name__ == "__main__":
    sys.exit(main())
