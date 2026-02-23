// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

/// @title Algebra pool direct swap interface
/// @notice Used to call Algebra CL pools directly, bypassing the router
interface IAlgebraPool {
    /// @notice Swap token0 for token1, or token1 for token0
    /// @param recipient The address to receive the output tokens
    /// @param zeroToOne The direction of the swap: true for token0 -> token1, false for token1 -> token0
    /// @param amountSpecified The amount of the swap (positive = exact input, negative = exact output)
    /// @param limitSqrtPrice The price limit for the swap (use MIN or MAX sqrt ratio for unlimited)
    /// @param data Any data passed via callback to the caller
    /// @return amount0 The delta of the balance of token0
    /// @return amount1 The delta of the balance of token1
    function swap(
        address recipient,
        bool zeroToOne,
        int256 amountSpecified,
        uint160 limitSqrtPrice,
        bytes calldata data
    ) external returns (int256 amount0, int256 amount1);

    function token0() external view returns (address);
    function token1() external view returns (address);
    function globalState() external view returns (
        uint160 sqrtPriceX96,
        int24 tick,
        uint16 fee,
        uint16 communityFeeToken0,
        uint8 communityFeeToken1,
        bool unlocked
    );
}
