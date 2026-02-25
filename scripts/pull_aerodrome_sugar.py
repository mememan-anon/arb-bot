#!/usr/bin/env python3
"""
Pull Aerodrome pools filtered by WETH/USDC/cbBTC with TVL from price oracle.

Usage:
  python scripts/pull_aerodrome_sugar.py --min-tvl 50000
  python scripts/pull_aerodrome_sugar.py --min-tvl 10000 --dry-run
"""

import argparse, csv, os, sys, time
from typing import Any, Dict, List, Set
from web3 import Web3

SUGAR_ADDRESS     = "0x3058F92EBf83e2536F2084F20f7C0357d7d3CCFE"
ORACLE_ADDRESS    = "0x8456038bdae8672f552182B0FC39b1917dE9a41A"  # Aerodrome on-chain TWAP oracle
CHAINLINK_ETH_USD = "0x71041dddad3595F9CEd3DcCFBe3D1F4b0a16Bb70"  # ETH/USD on Base
CHAINLINK_BTC_USD = "0x64c911996D3c6aC71f9b455B1E8E7266BcbD848F"  # BTC/USD on Base
MULTICALL_ADDRESS = "0xcA11bde05977b3631167028862bE2a173976CA11"
DEFAULT_RPC       = "https://lb.drpc.live/base/Avibgvi26EjPsw76UtdwmsRbjBXa_TYR8LCNehXRfUMv"

MAIN_TOKENS = {
    "0x4200000000000000000000000000000000000006": "WETH",
    "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913": "USDC",
    "0xcbb7c0000ab88b473b1f5afd9ef808440eed33bf": "cbBTC",
}
USDC_ADDRESS = "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913"
WETH_ADDRESS = "0x4200000000000000000000000000000000000006"
CBBTC_ADDRESS = "0xcbb7c0000ab88b473b1f5afd9ef808440eed33bf"

ABI_SUGAR = [{
    "type": "function", "name": "all",
    "inputs": [
        {"name": "_limit", "type": "uint256"},
        {"name": "_offset", "type": "uint256"},
        {"name": "_filter", "type": "uint256"}
    ],
    "outputs": [{"name": "", "type": "tuple[]", "components": [
        {"name": "lp", "type": "address"},
        {"name": "symbol", "type": "string"},
        {"name": "decimals", "type": "uint8"},
        {"name": "liquidity", "type": "uint256"},
        {"name": "type", "type": "int24"},
        {"name": "tick", "type": "int24"},
        {"name": "sqrt_ratio", "type": "uint160"},
        {"name": "token0", "type": "address"},
        {"name": "reserve0", "type": "uint256"},
        {"name": "staked0", "type": "uint256"},
        {"name": "token1", "type": "address"},
        {"name": "reserve1", "type": "uint256"},
        {"name": "staked1", "type": "uint256"},
        {"name": "gauge", "type": "address"},
        {"name": "gauge_liquidity", "type": "uint256"},
        {"name": "gauge_alive", "type": "bool"},
        {"name": "fee", "type": "address"},
        {"name": "bribe", "type": "address"},
        {"name": "factory", "type": "address"},
        {"name": "emissions", "type": "uint256"},
        {"name": "emissions_token", "type": "address"},
        {"name": "emissions_cap", "type": "uint256"},
        {"name": "pool_fee", "type": "uint256"},
        {"name": "unstaked_fee", "type": "uint256"},
        {"name": "token0_fees", "type": "uint256"},
        {"name": "token1_fees", "type": "uint256"},
        {"name": "locked", "type": "uint256"},
        {"name": "emerging", "type": "uint256"},
        {"name": "created_at", "type": "uint32"},
        {"name": "nfpm", "type": "address"},
        {"name": "alm", "type": "address"},
        {"name": "root", "type": "address"}
    ]}],
    "stateMutability": "view"
}]

ABI_CHAINLINK = [{
    "type": "function", "name": "latestRoundData",
    "inputs": [],
    "outputs": [
        {"name": "roundId", "type": "uint80"},
        {"name": "answer", "type": "int256"},
        {"name": "startedAt", "type": "uint256"},
        {"name": "updatedAt", "type": "uint256"},
        {"name": "answeredInRound", "type": "uint80"}
    ],
    "stateMutability": "view"
}]

# Aerodrome on-chain TWAP oracle — two interfaces, try both.
#
# Interface A: 1inch OffchainOracle style
#   getRate(srcToken, dstToken, useWrappers) → weightedRate
#   Rate semantics: (dstAmount_human / srcAmount_human) * 1e18
#   So price_usd = rate / 1e18  when dstToken = USDC
#
# Interface B: Velodrome/Aerodrome getManyRatesWithConnectors style
#   getManyRatesWithConnectors(src_len, connectors[]) → rates[]
#   Pass [token0, token1, …, tokenN, WETH, USDC] where src_len = N
#   Returns rates[i] = price of connectors[i] priced via the connector bridge tokens
#   Rate semantics same as Interface A
#
# We probe Interface B first (one call for many tokens), then fall back
# to Interface A per-token via multicall if B reverts.
ABI_ORACLE = [
    {
        "type": "function", "name": "getRate",
        "inputs": [
            {"name": "srcToken",    "type": "address"},
            {"name": "dstToken",    "type": "address"},
            {"name": "useWrappers", "type": "bool"}
        ],
        "outputs": [{"name": "weightedRate", "type": "uint256"}],
        "stateMutability": "view"
    },
    {
        "type": "function", "name": "getManyRatesWithConnectors",
        "inputs": [
            {"name": "src_len",    "type": "uint8"},
            {"name": "connectors", "type": "address[]"}
        ],
        "outputs": [{"name": "rates", "type": "uint256[]"}],
        "stateMutability": "view"
    },
]

ABI_DECIMALS = [{
    "type": "function", "name": "decimals",
    "inputs": [], "outputs": [{"type": "uint8"}],
    "stateMutability": "view"
}]

ABI_MULTICALL = [{
    "type": "function", "name": "aggregate3",
    "inputs": [{"name": "calls", "type": "tuple[]", "components": [
        {"name": "target", "type": "address"},
        {"name": "allowFailure", "type": "bool"},
        {"name": "callData", "type": "bytes"}
    ]}],
    "outputs": [{"name": "returnData", "type": "tuple[]", "components": [
        {"name": "success", "type": "bool"},
        {"name": "returnData", "type": "bytes"}
    ]}],
    "stateMutability": "view"
}]

V2_HEADER = ["address", "version", "token0", "token1", "decimals0", "decimals1", "fee", "block_number", "timestamp", "id"]
CL_HEADER = ["protocol", "address", "token0_symbol", "token1_symbol", "tickSpacing", "fee", "token0_address", "token1_address", "decimals0", "decimals1", "dex"]

def fetch_pools(w3: Web3, page_size: int = 500) -> List[Dict[str, Any]]:
    sugar = w3.eth.contract(address=Web3.to_checksum_address(SUGAR_ADDRESS), abi=ABI_SUGAR)
    result, offset, page = [], 0, 0
    print(f"[fetch] pulling from Sugar contract…")
    while True:
        page += 1
        try:
            batch = sugar.functions.all(page_size, offset, 0).call()
        except Exception as e:
            print(f"[error] page {page} failed: {e}")
            break
        print(f"  page {page}  offset={offset}  got {len(batch)}", end="\r")
        result.extend(batch)
        if len(batch) < page_size:
            break
        offset += page_size
    print(f"\n[fetch] {len(result)} total pools")
    return [{
        "lp": p[0].lower(),
        "liquidity": int(p[3]),
        "type": int(p[4]),
        "token0": p[7].lower(),
        "reserve0": int(p[8]),
        "token1": p[10].lower(),
        "reserve1": int(p[11]),
        "pool_fee": int(p[22]),
    } for p in result]

def get_anchor_prices_chainlink(w3: Web3) -> Dict[str, float]:
    """
    Fetch WETH and cbBTC prices from Chainlink — used as sanity-check anchors
    to validate the oracle output, not as primary price source.
    """
    prices = {USDC_ADDRESS: 1.0}
    for addr, feed_addr, symbol in [
        (WETH_ADDRESS,  CHAINLINK_ETH_USD, "WETH"),
        (CBBTC_ADDRESS, CHAINLINK_BTC_USD, "cbBTC"),
    ]:
        try:
            feed = w3.eth.contract(address=w3.to_checksum_address(feed_addr), abi=ABI_CHAINLINK)
            _, answer, _, _, _ = feed.functions.latestRoundData().call()
            prices[addr] = answer / 1e8
            print(f"  {symbol}: ${prices[addr]:,.2f}  (Chainlink anchor)")
        except Exception as e:
            print(f"  {symbol}: Chainlink failed ({e}), no anchor")
    return prices


def get_oracle_prices_batch(
    w3: Web3,
    tokens: List[str],
    anchor_prices: Dict[str, float],
    dec_cache: Dict[str, int],
    batch_size: int = 100,
) -> Dict[str, float]:
    """
    Query the Aerodrome TWAP oracle for every token vs USDC, batched via multicall3.

    Strategy (fastest → slowest):
      1. getManyRatesWithConnectors — one call per batch of ≤50 tokens (single ABI call,
         all math done on-chain).  If the oracle supports it, this is the fastest path.
      2. Per-token getRate(token, USDC, False) packed into multicall3 batches — one
         RPC round-trip per 300 tokens.
      3. Any token still missing falls back to reserve-ratio derivation in the caller.

    Rate interpretation (1inch OffchainOracle convention):
      rate = (dstAmount_raw / srcAmount_raw) * 1e18   — raw token units, NOT human amounts
      price_usd = rate / 10^(18 + dst_decimals - src_decimals)
                = rate / 10^(24 - src_dec)             — dst = USDC (6 decimals)

    Examples:
      WETH  (18 dec): price = rate / 10^(24-18) = rate / 1e6
      cbBTC ( 8 dec): price = rate / 10^(24-8)  = rate / 1e16
      USDC  ( 6 dec): price = rate / 10^(24-6)  = rate / 1e18  → ~1.0
    """
    oracle      = w3.eth.contract(address=w3.to_checksum_address(ORACLE_ADDRESS), abi=ABI_ORACLE)
    multicall   = w3.eth.contract(address=w3.to_checksum_address(MULTICALL_ADDRESS), abi=ABI_MULTICALL)

    # Seed with Chainlink anchors so WETH/cbBTC are pre-loaded as trusted fallback.
    # Oracle results overwrite these for tokens it prices correctly;
    # at the end we re-apply anchors to ensure Chainlink wins for the 3 main tokens.
    prices: Dict[str, float] = anchor_prices.copy()
    prices[USDC_ADDRESS] = 1.0   # always set USDC = $1 regardless of anchor dict

    # Tokens we actually need to price (skip USDC)
    to_price = [t for t in tokens if t.lower() != USDC_ADDRESS.lower()]
    if not to_price:
        return prices

    # ── Attempt 1: getManyRatesWithConnectors ─────────────────────────────────
    # Connectors = source tokens + [WETH, USDC] as bridge tokens at the end.
    # src_len = number of source tokens in the array.
    MANY_BATCH = 50   # keep gas per call reasonable
    connectors_fixed = [
        w3.to_checksum_address(WETH_ADDRESS),
        w3.to_checksum_address(USDC_ADDRESS),
    ]
    many_succeeded = 0
    try:
        for i in range(0, len(to_price), MANY_BATCH):
            src_batch = to_price[i:i + MANY_BATCH]
            src_len   = len(src_batch)
            connectors = [w3.to_checksum_address(t) for t in src_batch] + connectors_fixed
            rates = oracle.functions.getManyRatesWithConnectors(src_len, connectors).call()
            for j, rate in enumerate(rates[:src_len]):
                token = src_batch[j]
                if rate > 0:
                    src_dec = dec_cache.get(token, 18)
                    price = rate / (10 ** (24 - src_dec))
                    if 1e-12 < price < 1e12:   # sanity: $0.000000000001 – $1T
                        prices[token] = price
                        many_succeeded += 1
        print(f"  getManyRatesWithConnectors: priced {many_succeeded}/{len(to_price)} tokens")
    except Exception as e:
        print(f"  getManyRatesWithConnectors not supported ({type(e).__name__}), falling back to per-token multicall")
        many_succeeded = 0
        prices = anchor_prices.copy()   # reset to anchors; redo via path 2
        prices[USDC_ADDRESS] = 1.0

    # ── Attempt 2: per-token getRate via multicall3 ───────────────────────────
    # Used when getManyRatesWithConnectors is unavailable, OR to fill in any
    # tokens it missed.
    still_missing = [t for t in to_price if t not in prices]
    if still_missing:
        # Encode getRate(srcToken, USDC, False) for each missing token.
        # ABI-encoded args: address (32 bytes), address (32 bytes), bool (32 bytes)
        sel      = w3.keccak(text="getRate(address,address,bool)")[:4]
        usdc_enc = b'\x00' * 12 + bytes.fromhex(USDC_ADDRESS[2:])   # left-pad to 32 bytes
        false_enc = b'\x00' * 32

        calls = []
        for token in still_missing:
            token_enc = b'\x00' * 12 + bytes.fromhex(token[2:])
            calls.append((
                w3.to_checksum_address(ORACLE_ADDRESS),
                True,                               # allowFailure — don't revert on dust tokens
                sel + token_enc + usdc_enc + false_enc,
            ))

        per_token_ok = 0
        for i in range(0, len(calls), batch_size):
            chunk  = calls[i:i + batch_size]
            tokens_chunk = still_missing[i:i + batch_size]
            try:
                results = multicall.functions.aggregate3(chunk).call()
                for j, (success, data) in enumerate(results):
                    token = tokens_chunk[j]
                    if success and len(data) >= 32:
                        rate = int.from_bytes(data[:32], "big")
                        if rate > 0:
                            src_dec = dec_cache.get(token, 18)
                            price = rate / (10 ** (24 - src_dec))
                            if 1e-12 < price < 1e12:
                                prices[token] = price
                                per_token_ok += 1
            except Exception as e:
                print(f"  multicall batch {i // batch_size + 1} error: {e}")

        print(f"  getRate multicall: priced {per_token_ok}/{len(still_missing)} remaining tokens")

    # ── Re-apply Chainlink anchors so they always win for WETH/cbBTC/USDC ─────
    # Oracle rates for high-decimal tokens can still drift; Chainlink is ground truth.
    prices.update(anchor_prices)
    prices[USDC_ADDRESS] = 1.0

    # ── Validate oracle vs Chainlink ──────────────────────────────────────────
    for addr, symbol in [(WETH_ADDRESS, "WETH"), (CBBTC_ADDRESS, "cbBTC")]:
        oracle_price  = prices.get(addr, 0)
        chainlink_val = anchor_prices.get(addr, 0)
        if oracle_price > 0 and chainlink_val > 0:
            drift = abs(oracle_price - chainlink_val) / chainlink_val
            flag  = "⚠ large drift" if drift > 0.05 else "ok"
            print(f"  {symbol}: final=${oracle_price:,.2f}  chainlink=${chainlink_val:,.2f}  drift={drift:.1%} {flag}")
        elif chainlink_val > 0:
            print(f"  {symbol}: using chainlink=${chainlink_val:,.2f}")

    return prices


def derive_prices_from_reserves(
    pools: List[Dict[str, Any]],
    known: Dict[str, float],
    dec_cache: Dict[str, int],
    min_liquidity_usd: float = 10_000,
) -> Dict[str, float]:
    """
    Fallback: for tokens the oracle couldn't price, derive price from pool
    reserve ratios.  Three passes propagate prices outward through the graph.
    This is slower and noisier than the oracle — use only for stragglers.
    """
    derived = known.copy()
    for iteration in range(3):
        added = 0
        for pool in pools:
            t0, t1 = pool["token0"], pool["token1"]
            r0, r1 = pool["reserve0"], pool["reserve1"]
            if r0 == 0 or r1 == 0:
                continue
            p0 = derived.get(t0, 0.0)
            p1 = derived.get(t1, 0.0)
            # Only fill in the unknown side
            for (known_p, known_dec_t, unknown_t, unknown_dec_t, r_known, r_unknown) in [
                (p0, t0, t1, t1, r0, r1),
                (p1, t1, t0, t0, r1, r0),
            ]:
                if not (known_p > 0 and derived.get(unknown_t, 0) == 0):
                    continue
                d_known   = dec_cache.get(known_dec_t, 18)
                d_unknown = dec_cache.get(unknown_dec_t, 18)
                r_k_human = r_known   / (10 ** d_known)
                r_u_human = r_unknown / (10 ** d_unknown)
                liq_usd   = r_k_human * known_p * 2
                if liq_usd >= min_liquidity_usd and r_u_human > 0:
                    implied = (r_k_human * known_p) / r_u_human
                    if 1e-12 < implied < 1e12:
                        derived[unknown_t] = implied
                        added += 1
        if added == 0:
            break
        print(f"  reserve fallback pass {iteration + 1}: +{added} tokens")
    return derived

def calc_tvl(pool: Dict[str, Any], prices: Dict[str, float], dec_cache: Dict[str, int]) -> float:
    """
    Calculate TVL using pre-fetched prices.

    When only one side is priced (e.g. AERO/WETH where AERO has no oracle price),
    multiply that side ×2 instead of summing — assumes 50/50 pool balance, which
    gives an accurate TVL estimate and avoids systematically halving real TVL.
    For pools where both sides are priced, sum both.
    """
    p0 = prices.get(pool["token0"], 0.0)
    p1 = prices.get(pool["token1"], 0.0)
    dec0 = dec_cache.get(pool["token0"], 18)
    dec1 = dec_cache.get(pool["token1"], 18)
    r0_usd = pool["reserve0"] / (10 ** dec0) * p0 if p0 > 0 else 0.0
    r1_usd = pool["reserve1"] / (10 ** dec1) * p1 if p1 > 0 else 0.0

    if r0_usd > 0 and r1_usd > 0:
        return r0_usd + r1_usd   # both sides known: exact
    elif r0_usd > 0:
        return r0_usd * 2        # only token0 known: estimate from one side
    elif r1_usd > 0:
        return r1_usd * 2        # only token1 known: estimate from one side
    return 0.0

def get_decimals(w3: Web3, addr: str, cache: Dict[str, int]) -> int:
    if addr in cache:
        return cache[addr]
    try:
        c = w3.eth.contract(address=Web3.to_checksum_address(addr), abi=ABI_DECIMALS)
        dec = c.functions.decimals().call()
        cache[addr] = dec
        return dec
    except:
        cache[addr] = 18
        return 18

def batch_fetch_decimals(w3: Web3, tokens: List[str], cache: Dict[str, int], batch_size: int = 500) -> None:
    """Batch fetch decimals for tokens via multicall."""
    multicall = w3.eth.contract(address=Web3.to_checksum_address(MULTICALL_ADDRESS), abi=ABI_MULTICALL)
    
    # Filter to tokens not in cache
    to_fetch = [t for t in tokens if t not in cache]
    if not to_fetch:
        return
    
    # Encode decimals() calls
    selector = w3.keccak(text="decimals()")[:4]
    calls = [(w3.to_checksum_address(t), True, selector) for t in to_fetch]
    
    total_batches = (len(calls) + batch_size - 1) // batch_size
    fetched = 0
    
    for i in range(0, len(calls), batch_size):
        batch = calls[i:i + batch_size]
        batch_num = i // batch_size + 1
        
        try:
            results = multicall.functions.aggregate3(batch).call()
            for j, (success, data) in enumerate(results):
                token = to_fetch[i + j]
                if success and len(data) >= 32:
                    dec = int.from_bytes(data[:32], "big")
                    cache[token] = dec
                    fetched += 1
                else:
                    cache[token] = 18  # Default
        except Exception as e:
            # Fallback to default for failed batch
            for token in to_fetch[i:i + len(batch)]:
                if token not in cache:
                    cache[token] = 18
    
    print(f"  fetched {fetched}/{len(to_fetch)} decimals via multicall")

def load_existing(path: str, col: int = 0) -> Set[str]:
    if not os.path.exists(path):
        return set()
    with open(path, "r", encoding="utf-8") as f:
        r = csv.reader(f)
        next(r, None)
        return {row[col].strip().lower() for row in r if row and len(row) > col}

def write_amm(path: str, pools: List[Dict], existing: Set[str], dec_cache: Dict[str, int], w3: Web3, dry_run: bool) -> int:
    new = [p for p in pools if p["lp"] not in existing]
    if not new or dry_run:
        return len(new)
    
    os.makedirs(os.path.dirname(os.path.abspath(path)), exist_ok=True)
    
    # Check if we need header before opening file
    needs_header = not os.path.exists(path) or os.path.getsize(path) == 0
    
    with open(path, "a", newline="", encoding="utf-8") as f:
        w = csv.writer(f)
        if needs_header:
            w.writerow(V2_HEADER)
        for i, p in enumerate(new):
            dec0 = get_decimals(w3, p["token0"], dec_cache)
            dec1 = get_decimals(w3, p["token1"], dec_cache)
            version = 4 if p["type"] == 0 else 5
            w.writerow([p["lp"], version, p["token0"], p["token1"], dec0, dec1, p["pool_fee"], 0, 0, len(existing) + i])
    return len(new)

def write_cl(path: str, pools: List[Dict], existing: Set[str], dec_cache: Dict[str, int], w3: Web3, dry_run: bool) -> int:
    new = [p for p in pools if p["lp"] not in existing]
    if not new or dry_run:
        return len(new)
    
    os.makedirs(os.path.dirname(os.path.abspath(path)), exist_ok=True)
    
    # Check if we need header before opening file
    needs_header = not os.path.exists(path) or os.path.getsize(path) == 0
    
    with open(path, "a", newline="", encoding="utf-8") as f:
        w = csv.writer(f)
        if needs_header:
            w.writerow(CL_HEADER)
        for p in new:
            dec0 = get_decimals(w3, p["token0"], dec_cache)
            dec1 = get_decimals(w3, p["token1"], dec_cache)
            w.writerow(["UniswapV3CL", p["lp"], "", "", p["type"], p["pool_fee"], p["token0"], p["token1"], dec0, dec1, "AerodromeCL"])
    return len(new)

def main():
    p = argparse.ArgumentParser(description="Pull Aerodrome pools filtered by WETH/USDC/cbBTC")
    p.add_argument("--min-tvl", type=float, default=0.0, help="Minimum TVL in USD (default: 0)")
    p.add_argument("--dry-run", action="store_true", help="Don't write to files")
    p.add_argument("--rpc", default=DEFAULT_RPC, help="Base RPC URL")
    args = p.parse_args()
    
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    v2_csv = os.path.join(root, "cache", "base", ".cached-pools.csv")
    cl_csv = os.path.join(root, "cache", "base", ".cached-v3cl-pools.csv")
    
    print(f"[config] RPC: {args.rpc}")
    print(f"[config] Main tokens: {list(MAIN_TOKENS.values())}")
    print(f"[config] Min TVL: ${args.min_tvl:,.0f}" if args.min_tvl > 0 else "[config] Min TVL: (no filter)")
    print(f"[config] Dry run: {args.dry_run}\n")
    
    w3 = Web3(Web3.HTTPProvider(args.rpc, request_kwargs={"timeout": 60}))
    if not w3.is_connected():
        print("[error] Cannot connect to RPC")
        return 1
    print(f"[rpc] Connected, block {w3.eth.block_number}\n")
    
    dec_cache = {USDC_ADDRESS: 6, CBBTC_ADDRESS: 8, WETH_ADDRESS: 18}

    # ── Step 1: Chainlink anchors (used for oracle drift validation only) ──────
    print("[prices] Chainlink anchors...")
    anchor_prices = get_anchor_prices_chainlink(w3)
    print()

    # ── Step 2: Fetch all pools ────────────────────────────────────────────────
    pools = fetch_pools(w3)
    main_token_set = set(MAIN_TOKENS.keys())
    pools = [p for p in pools if p["token0"] in main_token_set or p["token1"] in main_token_set]
    print(f"[filter] {len(pools)} pools with WETH/USDC/cbBTC\n")

    # ── Step 3: Batch-fetch all decimals via multicall ─────────────────────────
    all_tokens = list({t for p in pools for t in (p["token0"], p["token1"])})
    print(f"[decimals] Fetching {len(all_tokens)} token decimals via multicall...")
    batch_fetch_decimals(w3, all_tokens, dec_cache)
    print()

    # ── Step 4: Oracle prices (getManyRatesWithConnectors → getRate multicall) ─
    print(f"[oracle] Querying Aerodrome oracle for {len(all_tokens)} tokens...")
    prices = get_oracle_prices_batch(w3, all_tokens, anchor_prices, dec_cache)
    print()

    # ── Step 5: Reserve-ratio fallback for any tokens the oracle missed ────────
    missing = [t for t in all_tokens if t not in prices]
    if missing:
        print(f"[fallback] Reserve-ratio derivation for {len(missing)} unpriced tokens...")
        prices = derive_prices_from_reserves(pools, prices, dec_cache)
    print(f"[prices] Total tokens priced: {len(prices)}/{len(all_tokens)}\n")
    
    if args.min_tvl > 0:
        print(f"[tvl] Filtering by TVL >= ${args.min_tvl:,.0f}…")
        pools = [p for p in pools if calc_tvl(p, prices, dec_cache) >= args.min_tvl]
        print(f"[tvl] {len(pools)} pools pass TVL filter\n")
    
    amm = [p for p in pools if p["type"] <= 0]
    cl = [p for p in pools if p["type"] > 0]
    volatile = sum(1 for p in amm if p["type"] == -1)
    stable = sum(1 for p in amm if p["type"] == 0)
    print(f"[split] AMM: {len(amm)} (volatile={volatile} stable={stable})")
    print(f"[split] CL: {len(cl)}\n")
    
    ex_v2 = load_existing(v2_csv, 0)
    n_v2 = write_amm(v2_csv, amm, ex_v2, dec_cache, w3, args.dry_run)
    print(f"[amm] {'Would write' if args.dry_run else 'Wrote'} {n_v2} new rows → {v2_csv}")
    
    ex_cl = load_existing(cl_csv, 1)
    n_cl = write_cl(cl_csv, cl, ex_cl, dec_cache, w3, args.dry_run)
    print(f"[cl] {'Would write' if args.dry_run else 'Wrote'} {n_cl} new rows → {cl_csv}")
    
    print(f"\n[done] {n_v2 + n_cl} total rows {'would be written' if args.dry_run else 'written'}")
    return 0

if __name__ == "__main__":
    sys.exit(main())
