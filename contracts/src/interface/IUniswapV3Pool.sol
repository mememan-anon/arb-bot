// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

/// @notice Minimal interface for a Uniswap V3 pool (any V3 fork: Aerodrome CL, PancakeSwap V3, etc.).
interface IUniswapV3Pool {
    /// @notice Returns the current sqrt price and tick.
    function slot0()
        external
        view
        returns (
            uint160 sqrtPriceX96,
            int24  tick,
            uint16 observationIndex,
            uint16 observationCardinality,
            uint16 observationCardinalityNext,
            uint8  feeProtocol,
            bool   unlocked
        );

    function liquidity() external view returns (uint128);

    function fee() external view returns (uint24);

    function token0() external view returns (address);

    function token1() external view returns (address);

    /// @notice Swap token0 for token1, or token1 for token0.
    /// @param recipient         Address to receive the output tokens.
    /// @param zeroForOne        Direction of the swap; true for token0→token1, false for token1→token0.
    /// @param amountSpecified   Amount of input (positive = exact input).
    /// @param sqrtPriceLimitX96 Price limit — must be MIN_SQRT_RATIO+1 / MAX_SQRT_RATIO-1.
    /// @param data              Arbitrary calldata forwarded to the callback.
    function swap(
        address recipient,
        bool    zeroForOne,
        int256  amountSpecified,
        uint160 sqrtPriceLimitX96,
        bytes   calldata data
    ) external returns (int256 amount0, int256 amount1);
}

/// @notice Callback that Uniswap V3 pools call to collect the input tokens.
interface IUniswapV3SwapCallback {
    /// @dev Standard Uniswap V3 swap callback — all V3 forks retain this same ABI.
    function uniswapV3SwapCallback(
        int256 amount0Delta,
        int256 amount1Delta,
        bytes  calldata data
    ) external;
}
