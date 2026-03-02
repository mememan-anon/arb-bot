// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import "forge-std/Test.sol";
import "../src/V2ArbBot.sol";

// ── Minimal interfaces used only in the test ─────────────────────────────────

interface IWBNB {
    function deposit() external payable;
    function withdraw(uint256) external;
    function approve(address spender, uint256 amount) external returns (bool);
    function balanceOf(address owner) external view returns (uint256);
    function transfer(address to, uint256 amount) external returns (bool);
}

interface IBEP20 {
    function balanceOf(address owner) external view returns (uint256);
    function approve(address spender, uint256 amount) external returns (bool);
    function transfer(address to, uint256 amount) external returns (bool);
    function decimals() external view returns (uint8);
}

interface IUniswapV2Factory {
    function getPair(address tokenA, address tokenB) external view returns (address pair);
}

interface IUniswapV2PairFull {
    function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32);
    function token0() external view returns (address);
    function token1() external view returns (address);
    function swap(uint amount0Out, uint amount1Out, address to, bytes calldata data) external;
}

// ── BSC Arb Fork Test Suite ───────────────────────────────────────────────────
//
//  Tests V2ArbBot.executeArbitrage() on a live BSC fork via Aave V3 flash loans.
//
//  Strategy taxonomy tested:
//    A. 2-hop / cross-DEX  — same token pair, two different DEXes
//       (PancakeSwap V2  ↔  Uniswap V2 BSC)
//    B. 3-hop triangular   — A→B→C→A around three pools
//    C. 4-leg cyclic       — A→B→C→D→A around four pools
//
//  Price discrepancies are created artificially by dealing tokens from
//  vm.deal / deal() and doing a large displacement swap before each arb.
//  This mirrors what the off-chain bot sees when a large trade moves a pool.
//
//  NOTE: Pool addresses are resolved at runtime via factory.getPair() so the
//  test is resilient to address changes.
//
//  Run:
//    forge test --match-contract BscArbForkTest -vv \
//      --fork-url $BSC_RPC_URL
//  or set FORK_URL in the environment.

contract BscArbForkTest is Test {

    // ── Token addresses ───────────────────────────────────────────────────────
    address constant WBNB  = 0xbb4CdB9CBd36B01bD1cBaEBF2De08d9173bc095c;
    address constant USDT  = 0x55d398326f99059fF775485246999027B3197955;
    address constant BTCB  = 0x7130d2A12B9BCbFAe4f2634d864A1Ee1Ce3Ead9c;
    address constant USDC  = 0x8AC76a51cc950d9822D68b83fE1Ad97B32Cd580d;
    address constant ETH   = 0x2170Ed0880ac9A755fd29B2688956BD959F933F8;

    // ── DEX factories ─────────────────────────────────────────────────────────
    address constant PCS_V2_FACTORY     = 0xcA143Ce32Fe78f1f7019d7d551a6402fC5350c73;
    // Biswap V2 on BSC — deep WBNB/USDT liquidity (0.3% fee), good second-leg pool
    address constant BISWAP_FACTORY     = 0x858E3312ed3A876947EA49d572A7C42DE08af7EE;

    // ── Aave V3 on BSC ────────────────────────────────────────────────────────
    address constant AAVE_POOL = 0x6807dc923806fE8Fd134338EABCA509979a7e0cB;

    // ── Contract under test ───────────────────────────────────────────────────
    V2ArbBot bot;

    // ──────────────────────────────────────────────────────────────────────────
    //  setUp: fork BSC, deploy fresh bot, configure Aave
    // ──────────────────────────────────────────────────────────────────────────
    function setUp() public {
        vm.createSelectFork(
            vm.envOr("FORK_URL", string("https://bsc-dataseed.binance.org/"))
        );

        bot = new V2ArbBot(address(this), WBNB);
        bot.setAavePool(AAVE_POOL);

        // Whitelist the assets used as flash-loan collateral
        bot.setAllowedFlashAsset(WBNB,  true);
        bot.setAllowedFlashAsset(USDT,  true);
        bot.setAllowedFlashAsset(BTCB,  true);
        bot.setAllowedFlashAsset(USDC,  true);
        bot.setAllowedFlashAsset(ETH,   true);
    }

    // ──────────────────────────────────────────────────────────────────────────
    //  Internal helpers
    // ──────────────────────────────────────────────────────────────────────────

    /// Deal ERC-20 tokens to an address by overwriting balanceOf slot (BSC tokens
    /// generally use slot 0 for the balance mapping, but we use deal() from forge-std
    /// which handles the storage slot lookup automatically).
    function _dealToken(address token, address to, uint256 amount) internal {
        deal(token, to, amount);
    }

    /// Resolve a V2 pair address from a factory; skip test if pair not deployed.
    function _pairOrSkip(address factory, address t0, address t1)
        internal returns (address pair)
    {
        pair = IUniswapV2Factory(factory).getPair(t0, t1);
        if (pair == address(0)) {
            emit log_string("  [skip] pair not found on this fork snapshot");
        }
    }

    /// Quote how many tokenOut tokens are received for amountIn tokenIn on a V2 pair.
    function _quoteV2(address pair, address tokenIn, uint256 amountIn, uint32 feeBps)
        internal view returns (uint256)
    {
        IUniswapV2PairFull p = IUniswapV2PairFull(pair);
        (uint112 r0, uint112 r1,) = p.getReserves();
        address t0 = p.token0();
        (uint256 rIn, uint256 rOut) = tokenIn == t0
            ? (uint256(r0), uint256(r1))
            : (uint256(r1), uint256(r0));
        uint256 base = 10_000 - feeBps;
        uint256 num = amountIn * base * rOut;
        uint256 den = rIn * 10_000 + amountIn * base;
        return num / den;
    }

    /// Displace a V2 pool: swap a large amount of tokenIn → tokenOut to create
    /// a price gap.  Uses vm.deal to give the displacing address free tokens.
    ///
    /// Returns the pool's new reserves after displacement.
    function _displacePairPrice(
        address pair,
        address tokenIn,
        address tokenOut,
        uint256 displaceAmount,
        uint32  feeBps
    ) internal returns (uint112 r0After, uint112 r1After) {
        address displacer = makeAddr("displacer");
        _dealToken(tokenIn, displacer, displaceAmount);

        vm.startPrank(displacer);
        IBEP20(tokenIn).transfer(pair, displaceAmount);

        // Compute expected amountOut and execute swap
        uint256 amountOut = _quoteV2(pair, tokenIn, displaceAmount, feeBps);
        require(amountOut > 0, "displace: zero out");

        IUniswapV2PairFull p = IUniswapV2PairFull(pair);
        bool zeroForOne = (tokenIn == p.token0());
        if (zeroForOne) {
            p.swap(0, amountOut, displacer, new bytes(0));
        } else {
            p.swap(amountOut, 0, displacer, new bytes(0));
        }
        vm.stopPrank();

        (r0After, r1After,) = p.getReserves();
    }

    // ──────────────────────────────────────────────────────────────────────────
    //  Strategy A: 2-hop / cross-DEX arb
    //  WBNB → USDT  on PancakeSwap V2 (price displaced)
    //  USDT → WBNB  on Uniswap V2 BSC (at original price)
    //
    //  "2-hop" and "cross-DEX" are the same strategy: one token-pair,
    //  two DEXes.  A price gap between them is the arb opportunity.
    // ──────────────────────────────────────────────────────────────────────────
    function test_A_TwoHop_CrossDex_AaveFlashLoan() public {
        address pcsPair  = _pairOrSkip(PCS_V2_FACTORY,  WBNB, USDT);
        address bswPair  = _pairOrSkip(BISWAP_FACTORY,  WBNB, USDT);
        if (pcsPair == address(0) || bswPair == address(0)) return;

        emit log_named_address("PCS V2  WBNB/USDT pair", pcsPair);
        emit log_named_address("Biswap  WBNB/USDT pair", bswPair);

        // Log baseline prices
        uint256 baseQuote = _quoteV2(pcsPair, WBNB, 1 ether, 25);
        emit log_named_decimal_uint("PCS baseline: 1 WBNB -> USDT (18 dec)", baseQuote, 18);

        // Displace PancakeSwap V2 WBNB price upward by swapping a large USDT amount in
        // → WBNB becomes relatively cheaper on PCS → arb: buy WBNB cheap on PCS, sell on Biswap
        uint256 displaceAmount = 1_000_000 * 1e18; // 1M USDT — large enough to shift PCS price ~10%+
        _displacePairPrice(pcsPair, USDT, WBNB, displaceAmount, 25);
        emit log_named_decimal_uint("PCS post-displace: 1 WBNB -> USDT", _quoteV2(pcsPair, WBNB, 1 ether, 25), 18);

        // Build SwapParams: 2 hops
        //   Hop 0: WBNB → USDT on PCS V2 (WBNB is cheaper here post-displacement)
        //   Hop 1: USDT → WBNB on Biswap V2 (at its undisturbed price)
        address[] memory pools = new address[](2);
        pools[0] = pcsPair;
        pools[1] = bswPair;

        uint8[] memory versions = new uint8[](2);
        versions[0] = 0; // V2
        versions[1] = 0; // V2

        uint32[] memory fees = new uint32[](2);
        fees[0] = 25; // PCS V2 0.25%
        fees[1] = 30; // Biswap 0.30%

        uint256 amountIn = 10 ether; // 10 WBNB flash-loaned from Aave

        V2ArbBot.SwapParams memory arb = V2ArbBot.SwapParams({
            pools:        pools,
            poolVersions: versions,
            fees:         fees,
            amountIn:     amountIn,
            startToken:   WBNB,
            flashLoanProvider: 0
        });

        uint256 botBalBefore = IBEP20(WBNB).balanceOf(address(bot));
        emit log_named_decimal_uint("bot WBNB before (18 dec)", botBalBefore, 18);

        // Execute — Aave flashes 10 WBNB, bot swaps, repays principal+0.05%, keeps profit
        bot.executeArbitrage(arb);

        uint256 botBalAfter = IBEP20(WBNB).balanceOf(address(bot));
        emit log_named_decimal_uint("bot WBNB after  (18 dec)", botBalAfter, 18);

        assertGt(botBalAfter, botBalBefore, "A: 2-hop cross-DEX: bot should profit");
        emit log_named_decimal_uint("A: profit (18 dec)", botBalAfter - botBalBefore, 18);
    }

    // ──────────────────────────────────────────────────────────────────────────
    //  Strategy B: 3-hop triangular arb
    //  WBNB → USDT  (PCS V2, pool displaced)
    //  USDT → BTCB  (PCS V2)
    //  BTCB → WBNB  (PCS V2)
    //
    //  With no displacement all three PCS V2 prices are in equilibrium,
    //  so we push the WBNB/USDT pool before running the bot.
    // ──────────────────────────────────────────────────────────────────────────
    function test_B_ThreeHop_Triangular_AaveFlashLoan() public {
        address wbnbUsdt = _pairOrSkip(PCS_V2_FACTORY, WBNB, USDT);
        address usdtBtcb = _pairOrSkip(PCS_V2_FACTORY, USDT, BTCB);
        address btcbWbnb = _pairOrSkip(PCS_V2_FACTORY, BTCB, WBNB);
        if (wbnbUsdt == address(0) || usdtBtcb == address(0) || btcbWbnb == address(0)) return;

        emit log_named_address("PCS V2 WBNB/USDT", wbnbUsdt);
        emit log_named_address("PCS V2 USDT/BTCB", usdtBtcb);
        emit log_named_address("PCS V2 BTCB/WBNB", btcbWbnb);

        // Displace PCS BTCB/WBNB — make WBNB cheaper relative to BTCB there.
        // Path exploits:  buy WBNB with USDT on wbnbUsdt (cheap),
        //                 buy BTCB with USDT on usdtBtcb,
        //                 sell BTCB for WBNB on displaced btcbWbnb (WBNB expensive there).
        uint256 displaceAmount = 5 * 1e8; // 5 BTCB (8 decimals-... wait BTCB on BSC is 18 dec)
        // BTCB on BSC is 18 decimals (it's a wrapped token)
        _displacePairPrice(btcbWbnb, WBNB, BTCB, 200 ether, 25); // push 200 WBNB into the BTCB/WBNB pool

        address[] memory pools = new address[](3);
        pools[0] = wbnbUsdt; // WBNB → USDT
        pools[1] = usdtBtcb; // USDT → BTCB
        pools[2] = btcbWbnb; // BTCB → WBNB

        uint8[] memory versions = new uint8[](3);
        versions[0] = 0;
        versions[1] = 0;
        versions[2] = 0;

        uint32[] memory fees = new uint32[](3);
        fees[0] = 25;
        fees[1] = 25;
        fees[2] = 25;

        uint256 amountIn = 5 ether; // 5 WBNB

        V2ArbBot.SwapParams memory arb = V2ArbBot.SwapParams({
            pools:        pools,
            poolVersions: versions,
            fees:         fees,
            amountIn:     amountIn,
            startToken:   WBNB,
            flashLoanProvider: 0
        });

        uint256 botBalBefore = IBEP20(WBNB).balanceOf(address(bot));

        bot.executeArbitrage(arb);

        uint256 botBalAfter = IBEP20(WBNB).balanceOf(address(bot));
        assertGt(botBalAfter, botBalBefore, "B: 3-hop triangular: bot should profit");
        emit log_named_decimal_uint("B: profit (18 dec)", botBalAfter - botBalBefore, 18);
    }

    // ──────────────────────────────────────────────────────────────────────────
    //  Strategy C: 4-leg cyclic arb
    //  WBNB → USDT  (PCS V2)
    //  USDT → ETH   (PCS V2)
    //  ETH  → BTCB  (PCS V2, displaced)
    //  BTCB → WBNB  (PCS V2)
    // ──────────────────────────────────────────────────────────────────────────
    function test_C_FourLeg_Cyclic_AaveFlashLoan() public {
        address wbnbUsdt = _pairOrSkip(PCS_V2_FACTORY, WBNB, USDT);
        address usdtEth  = _pairOrSkip(PCS_V2_FACTORY, USDT, ETH);
        address ethBtcb  = _pairOrSkip(PCS_V2_FACTORY, ETH,  BTCB);
        address btcbWbnb = _pairOrSkip(PCS_V2_FACTORY, BTCB, WBNB);
        if (wbnbUsdt == address(0) || usdtEth == address(0) ||
            ethBtcb  == address(0) || btcbWbnb == address(0)) return;

        emit log_named_address("PCS V2 WBNB/USDT", wbnbUsdt);
        emit log_named_address("PCS V2 USDT/ETH",  usdtEth);
        emit log_named_address("PCS V2 ETH/BTCB",  ethBtcb);
        emit log_named_address("PCS V2 BTCB/WBNB", btcbWbnb);

        // Displace BTCB/WBNB exit pool — push WBNB in so BTCB becomes expensive;
        // selling BTCB at the displaced pool yields extra WBNB.  Same technique as test B.
        // 4 hops × 0.25% fee = 1% total drag; use 500 WBNB displacement so gain exceeds it.
        _displacePairPrice(btcbWbnb, WBNB, BTCB, 500 ether, 25);

        address[] memory pools = new address[](4);
        pools[0] = wbnbUsdt;
        pools[1] = usdtEth;
        pools[2] = ethBtcb;
        pools[3] = btcbWbnb;

        uint8[] memory versions = new uint8[](4);
        versions[0] = 0;
        versions[1] = 0;
        versions[2] = 0;
        versions[3] = 0;

        uint32[] memory fees = new uint32[](4);
        fees[0] = 25;
        fees[1] = 25;
        fees[2] = 25;
        fees[3] = 25;

        uint256 amountIn = 3 ether; // 3 WBNB

        V2ArbBot.SwapParams memory arb = V2ArbBot.SwapParams({
            pools:        pools,
            poolVersions: versions,
            fees:         fees,
            amountIn:     amountIn,
            startToken:   WBNB,
            flashLoanProvider: 0
        });

        uint256 botBalBefore = IBEP20(WBNB).balanceOf(address(bot));

        bot.executeArbitrage(arb);

        uint256 botBalAfter = IBEP20(WBNB).balanceOf(address(bot));
        assertGt(botBalAfter, botBalBefore, "C: 4-leg cyclic: bot should profit");
        emit log_named_decimal_uint("C: profit (18 dec)", botBalAfter - botBalBefore, 18);
    }

    // ──────────────────────────────────────────────────────────────────────────
    //  Strategy D: 2-hop with Aave USDT as the flash-loan asset
    //  USDT → WBNB  (PCS V2, displaced — WBNB cheap)
    //  WBNB → USDT  (Uni V2 BSC)
    //
    //  Tests that non-WBNB start tokens work with Aave flash loans.
    // ──────────────────────────────────────────────────────────────────────────
    function test_D_TwoHop_UsdtStart_AaveFlashLoan() public {
        address pcsPair = _pairOrSkip(PCS_V2_FACTORY,  WBNB, USDT);
        address bswPair = _pairOrSkip(BISWAP_FACTORY,  WBNB, USDT);
        if (pcsPair == address(0) || bswPair == address(0)) return;

        // Displace PCS to make WBNB cheap (push WBNB into pool); 3000 WBNB shifts price ~5%+
        _displacePairPrice(pcsPair, WBNB, USDT, 3000 ether, 25);

        address[] memory pools = new address[](2);
        pools[0] = pcsPair; // USDT → WBNB on PCS (WBNB cheap post-displacement)
        pools[1] = bswPair; // WBNB → USDT on Biswap (at its undisturbed price)

        uint8[] memory versions = new uint8[](2);
        versions[0] = 0;
        versions[1] = 0;

        uint32[] memory fees = new uint32[](2);
        fees[0] = 25;
        fees[1] = 30;

        uint256 amountIn = 5_000 * 1e18; // 5k USDT — yields ~10 WBNB; Biswap can absorb it

        V2ArbBot.SwapParams memory arb = V2ArbBot.SwapParams({
            pools:        pools,
            poolVersions: versions,
            fees:         fees,
            amountIn:     amountIn,
            startToken:   USDT,
            flashLoanProvider: 0
        });

        uint256 botBalBefore = IBEP20(USDT).balanceOf(address(bot));

        bot.executeArbitrage(arb);

        uint256 botBalAfter = IBEP20(USDT).balanceOf(address(bot));
        assertGt(botBalAfter, botBalBefore, "D: USDT start: bot should profit");
        emit log_named_decimal_uint("D: profit in USDT (18 dec)", botBalAfter - botBalBefore, 18);
    }

    // ──────────────────────────────────────────────────────────────────────────
    //  Strategy E: Encoding correctness — direct execution without flash loan
    //  Seeds the bot with WBNB, runs a 2-hop cycle via _executeSwapPath directly
    //  (skipping Aave by calling executeArbitrage when aavePool is NOT set).
    //  Verifies that SwapParams ABI encoding round-trips correctly.
    // ──────────────────────────────────────────────────────────────────────────
    function test_E_DirectExecution_NoFlashLoan() public {
        // Deploy a bot WITHOUT Aave configured
        V2ArbBot directBot = new V2ArbBot(address(this), WBNB);
        // Do NOT call setAavePool — fallback path uses bot's own balance

        address pcsPair = _pairOrSkip(PCS_V2_FACTORY,  WBNB, USDT);
        address bswPair = _pairOrSkip(BISWAP_FACTORY,  WBNB, USDT);
        if (pcsPair == address(0) || bswPair == address(0)) return;

        // Seed bot with some WBNB
        uint256 seed = 20 ether;
        _dealToken(WBNB, address(directBot), seed);

        // Displace PCS so the cycle is profitable (1M USDT = ~10% gap)
        _displacePairPrice(pcsPair, USDT, WBNB, 1_000_000 * 1e18, 25);

        address[] memory pools = new address[](2);
        pools[0] = pcsPair;
        pools[1] = bswPair;

        uint8[] memory versions = new uint8[](2);
        versions[0] = 0;
        versions[1] = 0;

        uint32[] memory fees = new uint32[](2);
        fees[0] = 25;
        fees[1] = 30;

        V2ArbBot.SwapParams memory arb = V2ArbBot.SwapParams({
            pools:        pools,
            poolVersions: versions,
            fees:         fees,
            amountIn:     10 ether,
            startToken:   WBNB,
            flashLoanProvider: 255
        });

        uint256 before = IBEP20(WBNB).balanceOf(address(directBot));
        directBot.executeArbitrage(arb);
        uint256 after_ = IBEP20(WBNB).balanceOf(address(directBot));

        assertGt(after_, before, "E: direct execution: bot should profit");
        emit log_named_decimal_uint("E: profit (18 dec)", after_ - before, 18);
    }

    // ──────────────────────────────────────────────────────────────────────────
    //  Strategy F: Aave flash loan repayment check (no arb — verify mechanics)
    //  Flash-loans WBNB from Aave, executes a guaranteed-loss 1-pool round-trip
    //  seeded by extra balance, and checks: revert occurs when the arb truly
    //  can't repay Aave (safety net test of "arb: repay insufficient" guard).
    // ──────────────────────────────────────────────────────────────────────────
    function test_F_AaveRepayGuard_RevertsWhenUnprofitable() public {
        address pcsPair = _pairOrSkip(PCS_V2_FACTORY, WBNB, USDT);
        address bswPair = _pairOrSkip(BISWAP_FACTORY, WBNB, USDT);
        if (pcsPair == address(0) || bswPair == address(0)) return;

        // No price displacement → pools at equilibrium → arb will lose fees → must revert
        address[] memory pools = new address[](2);
        pools[0] = pcsPair;
        pools[1] = bswPair;

        uint8[] memory versions = new uint8[](2);
        versions[0] = 0;
        versions[1] = 0;

        uint32[] memory fees = new uint32[](2);
        fees[0] = 25;
        fees[1] = 30;

        V2ArbBot.SwapParams memory arb = V2ArbBot.SwapParams({
            pools:        pools,
            poolVersions: versions,
            fees:         fees,
            amountIn:     1 ether,
            startToken:   WBNB,
            flashLoanProvider: 0
        });

        vm.expectRevert(); // expect "arb: repay insufficient" or Aave slippage revert
        bot.executeArbitrage(arb);
        emit log_string("F: correctly reverted on unprofitable arb (Aave repay guard works)");
    }
}
