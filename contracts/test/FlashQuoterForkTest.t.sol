// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import "forge-std/Test.sol";
import "../src/FlashQuoter.sol";
import "../src/interface/IUniswapV2.sol";
import "../src/interface/IUniswapV3Pool.sol";

/// @notice V2 Factory — resolve pair addresses
interface IUniswapV2Factory {
    function getPair(address tokenA, address tokenB) external view returns (address pair);
}

/// @notice Full V2 Pair interface with token0/token1
interface IUniswapV2PairFull {
    function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32);
    function token0() external view returns (address);
    function token1() external view returns (address);
    function swap(uint amount0Out, uint amount1Out, address to, bytes calldata data) external;
}

/// @notice PancakeSwap V3 QuoterV2 — the on-chain oracle of truth for V3 quotes
interface IQuoterV2 {
    struct QuoteExactInputSingleParams {
        address tokenIn;
        address tokenOut;
        uint256 amountIn;
        uint24  fee;
        uint160 sqrtPriceLimitX96;
    }

    function quoteExactInputSingle(QuoteExactInputSingleParams memory params)
        external
        returns (
            uint256 amountOut,
            uint160 sqrtPriceX96After,
            uint32  initializedTicksCrossed,
            uint256 gasEstimate
        );
}

/// @notice PancakeSwap V3 Factory — to resolve pool addresses
interface IPancakeV3Factory {
    function getPool(address tokenA, address tokenB, uint24 fee)
        external view returns (address pool);
}

/// @notice Uniswap V3 Factory (BSC deployment)
interface IUniswapV3Factory {
    function getPool(address tokenA, address tokenB, uint24 fee)
        external view returns (address pool);
}

/// @notice Minimal ERC20
interface IERC20 {
    function decimals() external view returns (uint8);
    function balanceOf(address) external view returns (uint256);
}

/// @title FlashQuoterForkTest — Accuracy verification on BSC mainnet fork
/// @notice Compares FlashQuoter outputs against:
///   - V2: manual constant-product math from live reserves
///   - V3: PancakeSwap V3 QuoterV2 (on-chain quoter that does full tick traversal)
///   - Multi-hop: chained on-chain quotes vs single FlashQuoter.getAmountOut() call
contract FlashQuoterForkTest is Test {
    // ── BSC Addresses ────────────────────────────────────────────────────────
    address constant WBNB = 0xbb4CdB9CBd36B01bD1cBaEBF2De08d9173bc095c;
    address constant USDT = 0x55d398326f99059fF775485246999027B3197955;
    address constant BTCB = 0x7130d2A12B9BCbFAe4f2634d864A1Ee1Ce3Ead9c;
    address constant USDC = 0x8AC76a51cc950d9822D68b83fE1Ad97B32Cd580d;
    address constant ETH  = 0x2170Ed0880ac9A755fd29B2688956BD959F933F8;
    address constant CAKE = 0x0E09FaBB73Bd3Ade0a17ECC321fD13a19e81cE82;

    // ── DEX Infrastructure ───────────────────────────────────────────────────
    address constant PCS_V2_FACTORY     = 0xcA143Ce32Fe78f1f7019d7d551a6402fC5350c73;
    address constant PCS_V3_FACTORY     = 0x0BFbCF9fa4f9C56B0F40a671Ad40E0805A091865;
    address constant PCS_V3_QUOTER_V2   = 0xB048Bbc1Ee6b733FFfCFb9e9CeF7375518e25997;
    address constant UNISWAP_V3_FACTORY = 0xdB1d10011AD0Ff90774D0C6Bb92e5C5c8b4461F7;

    // ── Test contract instances ──────────────────────────────────────────────
    FlashQuoter quoter;

    // ── Tolerance: 0.5% relative deviation allowed ──────────────────────────
    uint256 constant TOLERANCE_BPS = 50; // 0.5%

    function setUp() public {
        vm.createSelectFork(
            vm.envOr("FORK_URL", string("https://bsc-dataseed.binance.org/"))
        );
        quoter = new FlashQuoter();
    }

    // ═════════════════════════════════════════════════════════════════════════
    //  HELPER: check relative deviation
    // ═════════════════════════════════════════════════════════════════════════

    function _assertCloseEnough(
        uint256 actual,
        uint256 expected,
        string memory label
    ) internal {
        assertGt(actual, 0, string.concat(label, ": actual is zero"));
        assertGt(expected, 0, string.concat(label, ": expected is zero"));

        uint256 diff = actual > expected ? actual - expected : expected - actual;
        uint256 deviation = diff * 10_000 / expected;

        emit log_named_uint(string.concat(label, " actual"), actual);
        emit log_named_uint(string.concat(label, " expected"), expected);
        emit log_named_uint(string.concat(label, " deviation_bps"), deviation);

        assertLe(
            deviation,
            TOLERANCE_BPS,
            string.concat(label, ": deviation exceeds 0.5%")
        );
    }

    // ═════════════════════════════════════════════════════════════════════════
    //  HELPER: manual V2 quote from reserves
    // ═════════════════════════════════════════════════════════════════════════

    function _manualV2Quote(
        address pair,
        address tokenIn,
        uint256 amountIn,
        uint32  feeBps
    ) internal view returns (uint256) {
        IUniswapV2PairFull p = IUniswapV2PairFull(pair);
        (uint112 r0, uint112 r1, ) = p.getReserves();
        address t0 = p.token0();
        (uint256 rIn, uint256 rOut) = tokenIn == t0
            ? (uint256(r0), uint256(r1))
            : (uint256(r1), uint256(r0));
        uint256 base = 10_000 - uint256(feeBps);
        uint256 num = amountIn * base * rOut;
        uint256 den = rIn * 10_000 + amountIn * base;
        return num / den;
    }

    // ═════════════════════════════════════════════════════════════════════════
    //  HELPER: build single-hop FlashQuoter params
    // ═════════════════════════════════════════════════════════════════════════

    function _singleHopParams(
        address pool,
        uint8   version,
        uint32  fee,
        uint256 amountIn,
        address startToken
    ) internal pure returns (FlashQuoter.SwapParams memory p) {
        address[] memory pools = new address[](1);
        pools[0] = pool;
        uint8[] memory versions = new uint8[](1);
        versions[0] = version;
        uint32[] memory fees = new uint32[](1);
        fees[0] = fee;
        p = FlashQuoter.SwapParams({
            pools: pools,
            poolVersions: versions,
            fees: fees,
            amountIn: amountIn,
            startToken: startToken
        });
    }

    // ═════════════════════════════════════════════════════════════════════════
    //  TEST 1: V2 — PancakeSwap WBNB/USDT, small input
    // ═════════════════════════════════════════════════════════════════════════

    function test_V2_PCS_WBNB_USDT_Small() public {
        address pair = IUniswapV2Factory(PCS_V2_FACTORY).getPair(WBNB, USDT);
        require(pair != address(0), "pair not found");

        uint256 amountIn = 1 ether; // 1 WBNB
        uint32  feeBps = 25; // PCS V2 0.25%

        uint256 expected = _manualV2Quote(pair, WBNB, amountIn, feeBps);
        FlashQuoter.SwapParams memory params =
            _singleHopParams(pair, 0, feeBps, amountIn, WBNB);
        uint256 actual = quoter.getAmountOut(params);

        _assertCloseEnough(actual, expected, "V2_PCS_WBNB_USDT_1BNB");
        // Exact match expected for V2 (pure math, no state)
        assertEq(actual, expected, "V2 should be EXACT match");
    }

    // ═════════════════════════════════════════════════════════════════════════
    //  TEST 2: V2 — PancakeSwap WBNB/USDT, LARGE input (50 WBNB)
    // ═════════════════════════════════════════════════════════════════════════

    function test_V2_PCS_WBNB_USDT_Large() public {
        address pair = IUniswapV2Factory(PCS_V2_FACTORY).getPair(WBNB, USDT);
        require(pair != address(0), "pair not found");

        uint256 amountIn = 50 ether; // 50 WBNB — large, should stress numerator
        uint32  feeBps = 25;

        uint256 expected = _manualV2Quote(pair, WBNB, amountIn, feeBps);
        FlashQuoter.SwapParams memory params =
            _singleHopParams(pair, 0, feeBps, amountIn, WBNB);
        uint256 actual = quoter.getAmountOut(params);

        _assertCloseEnough(actual, expected, "V2_PCS_WBNB_USDT_50BNB");
        assertEq(actual, expected, "V2 should be EXACT match");
    }

    // ═════════════════════════════════════════════════════════════════════════
    //  TEST 3: V2 — Reverse direction: USDT → WBNB
    // ═════════════════════════════════════════════════════════════════════════

    function test_V2_PCS_USDT_to_WBNB() public {
        address pair = IUniswapV2Factory(PCS_V2_FACTORY).getPair(WBNB, USDT);
        require(pair != address(0), "pair not found");

        uint256 amountIn = 3000 * 1e18; // 3000 USDT → ~5 WBNB
        uint32  feeBps = 25;

        uint256 expected = _manualV2Quote(pair, USDT, amountIn, feeBps);
        FlashQuoter.SwapParams memory params =
            _singleHopParams(pair, 0, feeBps, amountIn, USDT);
        uint256 actual = quoter.getAmountOut(params);

        assertEq(actual, expected, "V2 reverse direction should be EXACT match");
        emit log_named_decimal_uint("USDT->WBNB output", actual, 18);
    }

    // ═════════════════════════════════════════════════════════════════════════
    //  TEST 4: V2 — Mixed decimals: BTCB (18 dec) → WBNB (18 dec)
    // ═════════════════════════════════════════════════════════════════════════

    function test_V2_PCS_BTCB_WBNB() public {
        address pair = IUniswapV2Factory(PCS_V2_FACTORY).getPair(BTCB, WBNB);
        if (pair == address(0)) return; // skip if not found

        uint256 amountIn = 1e17; // 0.1 BTCB
        uint32 feeBps = 25;

        uint256 expected = _manualV2Quote(pair, BTCB, amountIn, feeBps);
        FlashQuoter.SwapParams memory params =
            _singleHopParams(pair, 0, feeBps, amountIn, BTCB);
        uint256 actual = quoter.getAmountOut(params);

        assertEq(actual, expected, "V2 BTCB/WBNB should be EXACT match");
        emit log_named_decimal_uint("0.1 BTCB -> WBNB", actual, 18);
    }

    // ═════════════════════════════════════════════════════════════════════════
    //  TEST 5: V3 — PancakeSwap V3 WBNB/USDT, small input vs QuoterV2
    // ═════════════════════════════════════════════════════════════════════════

    function test_V3_PCS_WBNB_USDT_Small() public {
        // Resolve pool
        address pool = IPancakeV3Factory(PCS_V3_FACTORY).getPool(WBNB, USDT, 500);
        if (pool == address(0)) {
            pool = IPancakeV3Factory(PCS_V3_FACTORY).getPool(WBNB, USDT, 2500);
        }
        require(pool != address(0), "V3 pool not found");

        uint24 poolFee = IUniswapV3Pool(pool).fee();
        uint256 amountIn = 1 ether;

        // On-chain QuoterV2 (ground truth — full tick traversal)
        IQuoterV2.QuoteExactInputSingleParams memory q = IQuoterV2.QuoteExactInputSingleParams({
            tokenIn: WBNB,
            tokenOut: USDT,
            amountIn: amountIn,
            fee: poolFee,
            sqrtPriceLimitX96: 0
        });
        (uint256 expected, , uint32 ticksCrossed, ) = IQuoterV2(PCS_V3_QUOTER_V2).quoteExactInputSingle(q);

        emit log_named_uint("V3 QuoterV2 expected", expected);
        emit log_named_uint("V3 ticks crossed", ticksCrossed);

        // Our FlashQuoter
        FlashQuoter.SwapParams memory params =
            _singleHopParams(pool, 1, uint32(poolFee), amountIn, WBNB);
        uint256 actual = quoter.getAmountOut(params);

        emit log_named_uint("V3 FlashQuoter actual", actual);

        // For small swaps within one tick, should be very close
        _assertCloseEnough(actual, expected, "V3_PCS_WBNB_USDT_1BNB");
    }

    // ═════════════════════════════════════════════════════════════════════════
    //  TEST 6: V3 — Large input (10 WBNB) — likely crosses ticks
    // ═════════════════════════════════════════════════════════════════════════

    function test_V3_PCS_WBNB_USDT_Large() public {
        address pool = IPancakeV3Factory(PCS_V3_FACTORY).getPool(WBNB, USDT, 500);
        if (pool == address(0)) {
            pool = IPancakeV3Factory(PCS_V3_FACTORY).getPool(WBNB, USDT, 2500);
        }
        require(pool != address(0), "V3 pool not found");

        uint24 poolFee = IUniswapV3Pool(pool).fee();
        uint256 amountIn = 10 ether; // 10 WBNB — will cross ticks

        // On-chain QuoterV2 (ground truth)
        IQuoterV2.QuoteExactInputSingleParams memory q = IQuoterV2.QuoteExactInputSingleParams({
            tokenIn: WBNB,
            tokenOut: USDT,
            amountIn: amountIn,
            fee: poolFee,
            sqrtPriceLimitX96: 0
        });
        (uint256 expected, , uint32 ticksCrossed, ) = IQuoterV2(PCS_V3_QUOTER_V2).quoteExactInputSingle(q);

        emit log_named_uint("V3 large: QuoterV2 expected", expected);
        emit log_named_uint("V3 large: ticks crossed", ticksCrossed);

        // Our FlashQuoter (with tick data, try-catch swap should work)
        FlashQuoter.SwapParams memory params =
            _singleHopParams(pool, 1, uint32(poolFee), amountIn, WBNB);
        uint256 actual = quoter.getAmountOut(params);

        emit log_named_uint("V3 large: FlashQuoter actual", actual);

        // With tick data prefetched, should match even for multi-tick swaps
        // Using wider tolerance for cross-tick (1%)
        if (ticksCrossed > 0) {
            // Cross-tick: 1% tolerance
            uint256 diff = actual > expected ? actual - expected : expected - actual;
            uint256 deviation = diff * 10_000 / expected;
            emit log_named_uint("V3 large: deviation_bps", deviation);
            assertLe(deviation, 100, "V3 large: deviation exceeds 1%");
        } else {
            _assertCloseEnough(actual, expected, "V3_PCS_WBNB_USDT_10BNB");
        }
    }

    // ═════════════════════════════════════════════════════════════════════════
    //  TEST 7: V3 — Reverse direction: USDT → WBNB on PancakeSwap V3
    // ═════════════════════════════════════════════════════════════════════════

    function test_V3_PCS_USDT_to_WBNB() public {
        address pool = IPancakeV3Factory(PCS_V3_FACTORY).getPool(WBNB, USDT, 500);
        if (pool == address(0)) {
            pool = IPancakeV3Factory(PCS_V3_FACTORY).getPool(WBNB, USDT, 2500);
        }
        require(pool != address(0), "V3 pool not found");

        uint24 poolFee = IUniswapV3Pool(pool).fee();
        uint256 amountIn = 3000 * 1e18; // 3000 USDT → ~5 WBNB

        IQuoterV2.QuoteExactInputSingleParams memory q = IQuoterV2.QuoteExactInputSingleParams({
            tokenIn: USDT,
            tokenOut: WBNB,
            amountIn: amountIn,
            fee: poolFee,
            sqrtPriceLimitX96: 0
        });
        (uint256 expected, , , ) = IQuoterV2(PCS_V3_QUOTER_V2).quoteExactInputSingle(q);

        FlashQuoter.SwapParams memory params =
            _singleHopParams(pool, 1, uint32(poolFee), amountIn, USDT);
        uint256 actual = quoter.getAmountOut(params);

        _assertCloseEnough(actual, expected, "V3_PCS_USDT_WBNB_3000");
    }

    // ═════════════════════════════════════════════════════════════════════════
    //  TEST 8: V3 — BTCB/WBNB high-value token (overflow stress test)
    //  This is the exact scenario that exposed the _mulDiv overflow bug:
    //  BTCB is ~$60k, so sqrtPriceX96 is huge → _mulDiv gets 512-bit products
    // ═════════════════════════════════════════════════════════════════════════

    function test_V3_PCS_BTCB_WBNB_OverflowStress() public {
        // Try common fee tiers
        address pool;
        uint24[4] memory feeTiers = [uint24(500), uint24(2500), uint24(10000), uint24(100)];
        for (uint i = 0; i < feeTiers.length; i++) {
            pool = IPancakeV3Factory(PCS_V3_FACTORY).getPool(BTCB, WBNB, feeTiers[i]);
            if (pool != address(0)) break;
        }
        if (pool == address(0)) {
            emit log_string("skip: no BTCB/WBNB V3 pool found");
            return;
        }

        uint24 poolFee = IUniswapV3Pool(pool).fee();
        uint256 amountIn = 1e17; // 0.1 BTCB (~$6k of value)

        // On-chain QuoterV2 truth
        IQuoterV2.QuoteExactInputSingleParams memory q = IQuoterV2.QuoteExactInputSingleParams({
            tokenIn: BTCB,
            tokenOut: WBNB,
            amountIn: amountIn,
            fee: poolFee,
            sqrtPriceLimitX96: 0
        });
        (uint256 expected, , , ) = IQuoterV2(PCS_V3_QUOTER_V2).quoteExactInputSingle(q);

        FlashQuoter.SwapParams memory params =
            _singleHopParams(pool, 1, uint32(poolFee), amountIn, BTCB);
        uint256 actual = quoter.getAmountOut(params);

        emit log_named_decimal_uint("BTCB->WBNB expected", expected, 18);
        emit log_named_decimal_uint("BTCB->WBNB actual  ", actual, 18);

        // This was the bug: old _mulDiv would return garbage (32+ WBNB for 0.1 BTCB)
        // With FullMath fix, should be ~17 WBNB (at ~$600/BNB, ~$3500/BTC price ratio)
        _assertCloseEnough(actual, expected, "V3_PCS_BTCB_WBNB_overflow");
    }

    // ═════════════════════════════════════════════════════════════════════════
    //  TEST 9: V3 — ETH/WBNB another high-value token pair
    // ═════════════════════════════════════════════════════════════════════════

    function test_V3_PCS_ETH_WBNB() public {
        address pool;
        uint24[4] memory feeTiers = [uint24(500), uint24(2500), uint24(10000), uint24(100)];
        for (uint i = 0; i < feeTiers.length; i++) {
            pool = IPancakeV3Factory(PCS_V3_FACTORY).getPool(ETH, WBNB, feeTiers[i]);
            if (pool != address(0)) break;
        }
        if (pool == address(0)) {
            emit log_string("skip: no ETH/WBNB V3 pool found");
            return;
        }

        uint24 poolFee = IUniswapV3Pool(pool).fee();
        uint256 amountIn = 1 ether; // 1 ETH

        IQuoterV2.QuoteExactInputSingleParams memory q = IQuoterV2.QuoteExactInputSingleParams({
            tokenIn: ETH,
            tokenOut: WBNB,
            amountIn: amountIn,
            fee: poolFee,
            sqrtPriceLimitX96: 0
        });
        (uint256 expected, , , ) = IQuoterV2(PCS_V3_QUOTER_V2).quoteExactInputSingle(q);

        FlashQuoter.SwapParams memory params =
            _singleHopParams(pool, 1, uint32(poolFee), amountIn, ETH);
        uint256 actual = quoter.getAmountOut(params);

        emit log_named_decimal_uint("1 ETH -> WBNB expected", expected, 18);
        emit log_named_decimal_uint("1 ETH -> WBNB actual  ", actual, 18);

        _assertCloseEnough(actual, expected, "V3_PCS_ETH_WBNB");
    }

    // ═════════════════════════════════════════════════════════════════════════
    //  TEST 10: Multi-hop V2 path: WBNB → USDT → BTCB (2 hops, both V2)
    //  Verifies chained quotes match manual chaining
    // ═════════════════════════════════════════════════════════════════════════

    function test_MultiHop_V2_WBNB_USDT_BTCB() public {
        address pair1 = IUniswapV2Factory(PCS_V2_FACTORY).getPair(WBNB, USDT);
        address pair2 = IUniswapV2Factory(PCS_V2_FACTORY).getPair(USDT, BTCB);
        require(pair1 != address(0) && pair2 != address(0), "pairs not found");

        uint256 amountIn = 5 ether; // 5 WBNB
        uint32  fee = 25;

        // Manual chain: hop1 then hop2
        uint256 hop1Out = _manualV2Quote(pair1, WBNB, amountIn, fee);
        uint256 hop2Out = _manualV2Quote(pair2, USDT, hop1Out, fee);

        // FlashQuoter multi-hop
        address[] memory pools = new address[](2);
        pools[0] = pair1;
        pools[1] = pair2;

        uint8[] memory versions = new uint8[](2);
        versions[0] = 0;
        versions[1] = 0;

        uint32[] memory fees = new uint32[](2);
        fees[0] = fee;
        fees[1] = fee;

        FlashQuoter.SwapParams memory params = FlashQuoter.SwapParams({
            pools: pools,
            poolVersions: versions,
            fees: fees,
            amountIn: amountIn,
            startToken: WBNB
        });

        uint256 actual = quoter.getAmountOut(params);

        emit log_named_decimal_uint("V2 multi-hop expected", hop2Out, 18);
        emit log_named_decimal_uint("V2 multi-hop actual  ", actual, 18);

        assertEq(actual, hop2Out, "Multi-hop V2 should be EXACT match");
    }

    // ═════════════════════════════════════════════════════════════════════════
    //  TEST 11: Multi-hop mixed V2+V3: WBNB → USDT (V2) → BTCB (V3)
    //  Verifies mixed-version paths work correctly
    // ═════════════════════════════════════════════════════════════════════════

    function test_MultiHop_Mixed_V2_V3() public {
        address v2Pair = IUniswapV2Factory(PCS_V2_FACTORY).getPair(WBNB, USDT);
        require(v2Pair != address(0), "V2 pair not found");

        // Find a V3 USDT/BTCB pool
        address v3Pool;
        uint24[4] memory feeTiers = [uint24(500), uint24(2500), uint24(100), uint24(10000)];
        for (uint i = 0; i < feeTiers.length; i++) {
            v3Pool = IPancakeV3Factory(PCS_V3_FACTORY).getPool(USDT, BTCB, feeTiers[i]);
            if (v3Pool != address(0)) break;
        }
        if (v3Pool == address(0)) {
            emit log_string("skip: no USDT/BTCB V3 pool found");
            return;
        }

        uint24 v3Fee = IUniswapV3Pool(v3Pool).fee();
        uint256 amountIn = 5 ether; // 5 WBNB

        // Manual: hop1 = V2 WBNB → USDT
        uint256 hop1Out = _manualV2Quote(v2Pair, WBNB, amountIn, 25);

        // hop2 = V3 USDT → BTCB via QuoterV2
        IQuoterV2.QuoteExactInputSingleParams memory q = IQuoterV2.QuoteExactInputSingleParams({
            tokenIn: USDT,
            tokenOut: BTCB,
            amountIn: hop1Out,
            fee: v3Fee,
            sqrtPriceLimitX96: 0
        });
        (uint256 hop2Out, , , ) = IQuoterV2(PCS_V3_QUOTER_V2).quoteExactInputSingle(q);

        // FlashQuoter: V2 then V3
        address[] memory pools = new address[](2);
        pools[0] = v2Pair;
        pools[1] = v3Pool;

        uint8[] memory versions = new uint8[](2);
        versions[0] = 0;  // V2
        versions[1] = 1;  // V3

        uint32[] memory fees = new uint32[](2);
        fees[0] = 25;
        fees[1] = uint32(v3Fee);

        FlashQuoter.SwapParams memory params = FlashQuoter.SwapParams({
            pools: pools,
            poolVersions: versions,
            fees: fees,
            amountIn: amountIn,
            startToken: WBNB
        });

        uint256 actual = quoter.getAmountOut(params);

        emit log_named_decimal_uint("Mixed V2+V3 expected", hop2Out, 18);
        emit log_named_decimal_uint("Mixed V2+V3 actual  ", actual, 18);

        _assertCloseEnough(actual, hop2Out, "Mixed_V2_V3_WBNB_USDT_BTCB");
    }

    // ═════════════════════════════════════════════════════════════════════════
    //  TEST 12: quoteArbitrage per-hop amounts consistency
    //  Verify that quoteArbitrage amounts array matches getAmountOut
    // ═════════════════════════════════════════════════════════════════════════

    function test_QuoteArbitrage_PerHop_Consistency() public {
        address pair1 = IUniswapV2Factory(PCS_V2_FACTORY).getPair(WBNB, USDT);
        address pair2 = IUniswapV2Factory(PCS_V2_FACTORY).getPair(USDT, BTCB);
        address pair3 = IUniswapV2Factory(PCS_V2_FACTORY).getPair(BTCB, WBNB);
        require(
            pair1 != address(0) && pair2 != address(0) && pair3 != address(0),
            "pairs"
        );

        address[] memory pools = new address[](3);
        pools[0] = pair1;
        pools[1] = pair2;
        pools[2] = pair3;

        uint8[] memory versions = new uint8[](3);
        versions[0] = 0;
        versions[1] = 0;
        versions[2] = 0;

        uint32[] memory fees = new uint32[](3);
        fees[0] = 25;
        fees[1] = 25;
        fees[2] = 25;

        FlashQuoter.SwapParams memory params = FlashQuoter.SwapParams({
            pools: pools,
            poolVersions: versions,
            fees: fees,
            amountIn: 5 ether,
            startToken: WBNB
        });

        uint256 finalOut = quoter.getAmountOut(params);
        uint256[] memory amounts = quoter.quoteArbitrage(params);

        assertEq(amounts.length, 4, "should have 4 amounts (3 hops + input)");
        assertEq(amounts[0], 5 ether, "amounts[0] should be input");
        assertEq(
            amounts[3],
            finalOut,
            "quoteArbitrage final != getAmountOut"
        );

        // Each intermediate amount should be nonzero
        for (uint i = 1; i < amounts.length; i++) {
            assertGt(amounts[i], 0, "intermediate amount is zero");
        }

        emit log_named_decimal_uint("hop0: WBNB in ", amounts[0], 18);
        emit log_named_decimal_uint("hop1: USDT out", amounts[1], 18);
        emit log_named_decimal_uint("hop2: BTCB out", amounts[2], 18);
        emit log_named_decimal_uint("hop3: WBNB out", amounts[3], 18);
    }

    // ═════════════════════════════════════════════════════════════════════════
    //  TEST 13: Bogus detection — FlashQuoter MUST NOT return > 1.5x input
    //  for any single V2 hop (sanity check that pure math is correct)
    // ═════════════════════════════════════════════════════════════════════════

    function test_V2_NoBogusProfits() public {
        address pair = IUniswapV2Factory(PCS_V2_FACTORY).getPair(WBNB, USDT);
        require(pair != address(0), "pair not found");

        // Try various input sizes — none should ever return > 1.5x in value terms
        uint256[4] memory inputs = [uint256(1 ether), 10 ether, 50 ether, 100 ether];

        for (uint i = 0; i < inputs.length; i++) {
            FlashQuoter.SwapParams memory params =
                _singleHopParams(pair, 0, 25, inputs[i], WBNB);
            uint256 out = quoter.getAmountOut(params);

            // Quote the reverse: if we got `out` USDT, how much WBNB back?
            FlashQuoter.SwapParams memory rev =
                _singleHopParams(pair, 0, 25, out, USDT);
            uint256 roundTrip = quoter.getAmountOut(rev);

            // Round-trip should ALWAYS be less than input (fees eat ~0.5%)
            assertLt(
                roundTrip,
                inputs[i],
                "V2 round-trip profit impossible: bug detected"
            );

            uint256 loss = inputs[i] - roundTrip;
            uint256 lossBps = loss * 10_000 / inputs[i];
            emit log_named_uint(
                string.concat("V2 round-trip loss_bps for input=", vm.toString(inputs[i])),
                lossBps
            );
            // Expected loss: ~50 bps (2 × 0.25% fee)
            assertGt(lossBps, 30, "loss too small - suspicious");
            assertLt(lossBps, 200, "loss too large - something wrong");
        }
    }

    // ═════════════════════════════════════════════════════════════════════════
    //  TEST 14: V3 fee tier 10000 (1% fee, tickSpacing=200) — most common
    //  These pools have the most in our cache (2617 pools)
    // ═════════════════════════════════════════════════════════════════════════

    function test_V3_PCS_HighFee_10000() public {
        // CAKE/WBNB is commonly at 10000 fee tier
        address pool = IPancakeV3Factory(PCS_V3_FACTORY).getPool(CAKE, WBNB, 10000);
        if (pool == address(0)) {
            emit log_string("skip: no CAKE/WBNB 10000 pool");
            return;
        }

        uint128 liq = IUniswapV3Pool(pool).liquidity();
        if (liq == 0) {
            emit log_string("skip: zero liquidity");
            return;
        }

        uint256 amountIn = 100 * 1e18; // 100 CAKE

        IQuoterV2.QuoteExactInputSingleParams memory q = IQuoterV2.QuoteExactInputSingleParams({
            tokenIn: CAKE,
            tokenOut: WBNB,
            amountIn: amountIn,
            fee: 10000,
            sqrtPriceLimitX96: 0
        });
        (uint256 expected, , , ) = IQuoterV2(PCS_V3_QUOTER_V2).quoteExactInputSingle(q);

        FlashQuoter.SwapParams memory params =
            _singleHopParams(pool, 1, 10000, amountIn, CAKE);
        uint256 actual = quoter.getAmountOut(params);

        emit log_named_decimal_uint("CAKE->WBNB expected", expected, 18);
        emit log_named_decimal_uint("CAKE->WBNB actual  ", actual, 18);

        _assertCloseEnough(actual, expected, "V3_PCS_CAKE_WBNB_10000");
    }
}
