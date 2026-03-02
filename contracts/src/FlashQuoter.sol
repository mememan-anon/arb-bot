// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import "./interface/IUniswapV2.sol";
import "./interface/IUniswapV3Pool.sol";
import "./interface/IAlgebraPool.sol";

/// @title FlashQuoter — REVM-only swap quoter for arb path simulation
/// @notice Deployed at a fixed address inside REVM (0x1000).
///         V2/Solidly: pure math (getReserves → constant product).
///         V3/Algebra: try-catch swap pattern (pool does tick math,
///                     callback reverts with output amount).
///
/// SwapParams ABI matches the Rust gen_alloy.rs definition (5 fields).
contract FlashQuoter {
    struct SwapParams {
        address[] pools;
        uint8[]   poolVersions; // 0=V2, 1=V3, 2=Algebra, 3=SolidlyVolatile
        uint32[]  fees;         // basis-points per hop (V2/Solidly use this; V3/Algebra ignore)
        uint256   amountIn;
        address   startToken;
    }

    // ── V3 price limits ─────────────────────────────────────────────────────
    uint160 private constant MIN_SQRT_RATIO = 4295128739;
    uint160 private constant MAX_SQRT_RATIO =
        1461446703485210103287273052203988822378723970342;

    // ── Public entry points ─────────────────────────────────────────────────

    /// @notice Returns the final amount_out for a multi-hop swap path.
    function getAmountOut(SwapParams calldata params)
        external
        returns (uint256 amountOut)
    {
        uint256 currentAmount = params.amountIn;
        address currentToken  = params.startToken;

        for (uint256 i = 0; i < params.pools.length; ) {
            address pool    = params.pools[i];
            uint8   version = params.poolVersions[i];

            // Determine tokens & direction
            address token0 = _token0(pool);
            address token1 = _token1(pool);
            bool zeroForOne = (currentToken == token0);

            if (version <= 0 || version == 3) {
                // V2 or Solidly-volatile: pure reserve math
                currentAmount = _quoteV2(pool, currentAmount, zeroForOne, params.fees[i]);
            } else if (version == 1) {
                // Uniswap-V3 style: try-catch swap, with analytical fallback
                currentAmount = _quoteV3(pool, currentAmount, zeroForOne, params.fees[i]);
            } else {
                // Algebra (version == 2): try-catch swap, with analytical fallback
                currentAmount = _quoteAlgebra(pool, currentAmount, zeroForOne, params.fees[i]);
            }

            // advance to next token
            currentToken = zeroForOne ? token1 : token0;

            unchecked { ++i; }
        }

        return currentAmount;
    }

    /// @notice Returns per-hop amounts (length = pools.length + 1).
    function quoteArbitrage(SwapParams calldata params)
        external
        returns (uint256[] memory amounts)
    {
        amounts = new uint256[](params.pools.length + 1);
        amounts[0] = params.amountIn;
        address currentToken = params.startToken;

        for (uint256 i = 0; i < params.pools.length; ) {
            address pool    = params.pools[i];
            uint8   version = params.poolVersions[i];

            address token0 = _token0(pool);
            address token1 = _token1(pool);
            bool zeroForOne = (currentToken == token0);

            if (version <= 0 || version == 3) {
                amounts[i + 1] = _quoteV2(pool, amounts[i], zeroForOne, params.fees[i]);
            } else if (version == 1) {
                amounts[i + 1] = _quoteV3(pool, amounts[i], zeroForOne, params.fees[i]);
            } else {
                amounts[i + 1] = _quoteAlgebra(pool, amounts[i], zeroForOne, params.fees[i]);
            }

            currentToken = zeroForOne ? token1 : token0;
            unchecked { ++i; }
        }
    }

    // ═════════════════════════════════════════════════════════════════════════
    //                          INTERNAL QUOTES
    // ═════════════════════════════════════════════════════════════════════════

    /// @dev V2 / Solidly-volatile constant-product quote (no token transfers).
    function _quoteV2(
        address pool,
        uint256 amountIn,
        bool    zeroForOne,
        uint32  feeBps
    ) private view returns (uint256) {
        (uint112 r0, uint112 r1, ) = IUniswapV2Pair(pool).getReserves();

        uint256 reserveIn  = uint256(zeroForOne ? r0 : r1);
        uint256 reserveOut = uint256(zeroForOne ? r1 : r0);

        // feeBps is in basis-points, e.g. 25 → 0.25 %
        // amountInWithFee = amountIn × (10 000 − feeBps)
        uint256 numerator   = amountIn * (10_000 - uint256(feeBps));
        uint256 denominator = reserveIn * 10_000 + numerator;

        return (numerator * reserveOut) / denominator;
    }

    /// @dev V3 quote via try-catch swap (pool does all tick math).
    ///      If the swap reverts with an error (e.g., missing tick data in fast mode),
    ///      falls back to analytical single-tick math using slot0 + liquidity.
    function _quoteV3(
        address pool,
        uint256 amountIn,
        bool    zeroForOne,
        uint32  fee
    ) private returns (uint256) {
        try IUniswapV3Pool(pool).swap(
            address(this),
            zeroForOne,
            int256(amountIn),
            zeroForOne ? MIN_SQRT_RATIO + 1 : MAX_SQRT_RATIO - 1,
            ""
        ) returns (int256, int256) {
            // Callback always reverts → should never reach here
            revert("V3_UNEXPECTED_SUCCESS");
        } catch (bytes memory reason) {
            uint256 result = _decodeAmount(reason);
            if (result > 0) return result;
        }
        // Fallback: analytical single-tick quoter
        return _quoteV3Math(pool, amountIn, zeroForOne, fee);
    }

    /// @dev Algebra (Thena/Camelot) quote via try-catch swap.
    ///      Same analytical fallback as V3.
    function _quoteAlgebra(
        address pool,
        uint256 amountIn,
        bool    zeroForOne,
        uint32  fee
    ) private returns (uint256) {
        try IAlgebraPool(pool).swap(
            address(this),
            zeroForOne,
            int256(amountIn),
            zeroForOne ? MIN_SQRT_RATIO + 1 : MAX_SQRT_RATIO - 1,
            ""
        ) returns (int256, int256) {
            revert("ALGEBRA_UNEXPECTED_SUCCESS");
        } catch (bytes memory reason) {
            uint256 result = _decodeAmount(reason);
            if (result > 0) return result;
        }
        // Fallback: analytical single-tick quoter (same math as V3)
        return _quoteV3Math(pool, amountIn, zeroForOne, fee);
    }

    /// @dev Analytical V3 quoter: computes swap output using only slot0() and
    ///      liquidity() — no tick data needed. Accurate for swaps that stay
    ///      within the current tick range (typical for small inputs).
    ///      Returns a conservative estimate (underestimates output for large swaps).
    function _quoteV3Math(
        address pool,
        uint256 amountIn,
        bool    zeroForOne,
        uint32  fee
    ) private view returns (uint256) {
        // Read pool state (works in fast_mode — data is prefetched)
        (uint160 sqrtPriceX96, , , , , , ) = IUniswapV3Pool(pool).slot0();
        uint128 liq = IUniswapV3Pool(pool).liquidity();

        if (liq == 0 || sqrtPriceX96 == 0 || amountIn == 0) return 0;

        uint256 sqrtP = uint256(sqrtPriceX96);
        uint256 L = uint256(liq);

        // Apply fee: amountInAfterFee = amountIn * (1e6 - fee) / 1e6
        uint256 feeNum = uint256(fee);
        if (feeNum >= 1_000_000) return 0;
        uint256 amountAfterFee = amountIn * (1_000_000 - feeNum) / 1_000_000;

        if (zeroForOne) {
            // Selling token0 → token1 (price decreases)
            // sqrtP_new = L * 2^96 * sqrtP / (L * 2^96 + amountAfterFee * sqrtP)
            // amountOut = L * (sqrtP - sqrtP_new) / 2^96
            uint256 numerator1 = L << 96; // L * 2^96
            uint256 product = _mulDiv(amountAfterFee, sqrtP, 1); // amountAfterFee * sqrtP
            if (product == 0 && amountAfterFee > 0 && sqrtP > 0) return 0; // overflow
            uint256 denominator = numerator1 + product;
            if (denominator < numerator1) return 0; // overflow check
            uint256 sqrtPNew = _mulDiv(numerator1, sqrtP, denominator);
            if (sqrtPNew >= sqrtP || sqrtPNew < MIN_SQRT_RATIO) return 0;
            return L * (sqrtP - sqrtPNew) >> 96;
        } else {
            // Selling token1 → token0 (price increases)
            // sqrtP_new = sqrtP + amountAfterFee * 2^96 / L
            // amountOut = L * 2^96 * (sqrtPNew - sqrtP) / (sqrtP * sqrtPNew)
            uint256 deltaSqrtP = (amountAfterFee << 96) / L;
            if (deltaSqrtP == 0) return 0;
            uint256 sqrtPNew = sqrtP + deltaSqrtP;
            if (sqrtPNew <= sqrtP || sqrtPNew > MAX_SQRT_RATIO) return 0;
            // amountOut = L * 2^96 / sqrtP - L * 2^96 / sqrtPNew
            //           = (L << 96) * (sqrtPNew - sqrtP) / (sqrtP * sqrtPNew)
            // Split to avoid overflow: (L << 96) / sqrtP * (sqrtPNew - sqrtP) / sqrtPNew
            uint256 step1 = (L << 96) / sqrtP;
            return step1 * (sqrtPNew - sqrtP) / sqrtPNew;
        }
    }

    /// @dev 256-bit safe multiply-divide: floor(a × b / d).
    ///      Ported from Uniswap V3 FullMath.sol — handles intermediate
    ///      512-bit products without truncation.
    function _mulDiv(uint256 a, uint256 b, uint256 d) private pure returns (uint256 result) {
        if (d == 0) return 0;
        assembly {
            // 512-bit multiply [prod1 prod0] = a * b
            let mm := mulmod(a, b, not(0))
            let prod0 := mul(a, b)
            let prod1 := sub(sub(mm, prod0), lt(mm, prod0))

            // No overflow: prod1 == 0 → simple division
            if iszero(prod1) {
                result := div(prod0, d)
                // done — leave assembly
            }

            if gt(prod1, 0) {
                // Overflow case — full 512-bit / 256-bit division
                // Require d > prod1 (otherwise result overflows 256 bits)
                if iszero(gt(d, prod1)) {
                    result := 0 // overflow → return 0 as safe default
                }
                if gt(d, prod1) {
                    ///////////////////////////////////////////////
                    // 512 by 256 division.
                    ///////////////////////////////////////////////

                    // Make division exact by subtracting the remainder from [prod1 prod0].
                    let remainder := mulmod(a, b, d)
                    prod1 := sub(prod1, gt(remainder, prod0))
                    prod0 := sub(prod0, remainder)

                    // Factor powers of two out of denominator and compute largest
                    // power of two divisor of denominator.
                    let twos := and(sub(0, d), d)
                    // Divide denominator by twos.
                    d := div(d, twos)
                    // Divide [prod1 prod0] by twos.
                    prod0 := div(prod0, twos)
                    // Flip twos such that it is 2^256 / twos. If twos is zero,
                    // then it becomes one.
                    twos := add(div(sub(0, twos), twos), 1)
                    prod0 := or(prod0, mul(prod1, twos))

                    // Invert denominator mod 2^256.
                    let inv := xor(mul(3, d), 2)
                    inv := mul(inv, sub(2, mul(d, inv))) // 4 bits
                    inv := mul(inv, sub(2, mul(d, inv))) // 8 bits
                    inv := mul(inv, sub(2, mul(d, inv))) // 16 bits
                    inv := mul(inv, sub(2, mul(d, inv))) // 32 bits
                    inv := mul(inv, sub(2, mul(d, inv))) // 64 bits
                    inv := mul(inv, sub(2, mul(d, inv))) // 128 bits
                    inv := mul(inv, sub(2, mul(d, inv))) // 256 bits

                    result := mul(prod0, inv)
                }
            }
        }
    }

    // ═════════════════════════════════════════════════════════════════════════
    //                       SWAP CALLBACKS (revert with output)
    // ═════════════════════════════════════════════════════════════════════════

    /// @dev UniswapV3 / PancakeSwapV3 / any V3-fork callback.
    ///      We don't pay — just revert with the output amount.
    function uniswapV3SwapCallback(
        int256 amount0Delta,
        int256 amount1Delta,
        bytes  calldata
    ) external {
        _revertWithOutput(amount0Delta, amount1Delta);
    }

    /// @dev PancakeSwapV3 uses a different callback selector.
    function pancakeV3SwapCallback(
        int256 amount0Delta,
        int256 amount1Delta,
        bytes  calldata
    ) external {
        _revertWithOutput(amount0Delta, amount1Delta);
    }

    /// @dev Algebra (Thena Fusion, Camelot) callback.
    function algebraSwapCallback(
        int256 amount0Delta,
        int256 amount1Delta,
        bytes  calldata
    ) external {
        _revertWithOutput(amount0Delta, amount1Delta);
    }

    // ═════════════════════════════════════════════════════════════════════════
    //                             HELPERS
    // ═════════════════════════════════════════════════════════════════════════

    /// @dev Revert with the unsigned output amount (the negative delta).
    function _revertWithOutput(int256 a0, int256 a1) private pure {
        // Negative delta = tokens the pool sends to us = output
        uint256 amountOut = a0 < 0 ? uint256(-a0) : uint256(-a1);
        assembly {
            let p := mload(0x40)
            mstore(p, amountOut)
            revert(p, 32)
        }
    }

    /// @dev Decode the revert reason as a uint256 output amount.
    ///      Our callback reverts with EXACTLY 32 bytes (a raw uint256).
    ///      ONLY accept the exact 32-byte format from our own callback.
    ///      Everything else (Error(string), Panic(uint256), custom errors,
    ///      or any other format) is treated as a failed swap → return 0.
    ///
    ///      This is intentionally strict: we never guess or reinterpret
    ///      error data as valid output amounts.
    function _decodeAmount(bytes memory reason) private pure returns (uint256) {
        // ONLY accept exactly 32 bytes — our callback's raw uint256 output.
        // Any other length means the pool itself reverted (error, panic, custom error, etc.)
        if (reason.length != 32) {
            return 0;
        }
        uint256 amount;
        assembly {
            amount := mload(add(reason, 32))
        }
        return amount;
    }

    function _token0(address pool) private view returns (address t) {
        (, bytes memory d) = pool.staticcall(abi.encodeWithSignature("token0()"));
        t = abi.decode(d, (address));
    }

    function _token1(address pool) private view returns (address t) {
        (, bytes memory d) = pool.staticcall(abi.encodeWithSignature("token1()"));
        t = abi.decode(d, (address));
    }

    receive() external payable {}
}
