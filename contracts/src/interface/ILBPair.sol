// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

/// @title ILBPair — Minimal interface for LFJ Liquidity Book V2.1/V2.2 pairs.
///
/// Full reference: https://docs.lfj.gg/versioned_docs/version-V2.1/APIs/interfaces/ILBPair
interface ILBPair {
    /// @notice Returns the current aggregate reserves of the pair.
    /// The reserves represent the total token balances across all active bins.
    function getReserves()
        external
        view
        returns (uint128 reserveX, uint128 reserveY);

    /// @notice Returns the active bin ID (current price bin).
    function getActiveId() external view returns (uint24 activeId);

    /// @notice Returns the address of token X (the first token in terms of sorting).
    function getTokenX() external pure returns (address tokenX);

    /// @notice Returns the address of token Y.
    function getTokenY() external pure returns (address tokenY);

    /// @notice Returns the bin step (price tick size) of the pair.
    function getBinStep() external pure returns (uint16 binStep);

    /// @notice Simulates a swap and returns how much would be received.
    /// @param amountIn  The amount of token being sent in.
    /// @param swapForY  True when swapping tokenX → tokenY, false for Y → X.
    /// @return amountInLeft  Remaining input not consumed (should be 0 on success).
    /// @return amountOut     Amount of the other token received.
    /// @return fee           Fee charged in input token units.
    function getSwapOut(uint128 amountIn, bool swapForY)
        external
        view
        returns (
            uint128 amountInLeft,
            uint128 amountOut,
            uint128 fee
        );

    /// @notice Executes a swap.
    /// @dev    The contract must have already received `amountIn` of the input token.
    ///         Caller must transfer tokens to the pair *before* calling swap().
    /// @param  swapForY  True → send tokenX, receive tokenY.  False → send tokenY, receive tokenX.
    /// @param  to        Recipient of the output tokens.
    /// @return amountsOut Packed uint256: high 128 bits = amountY out, low 128 bits = amountX out.
    function swap(bool swapForY, address to)
        external
        returns (bytes32 amountsOut);

    /// @notice Returns the static fee parameters.
    /// baseFactor is the multiplier applied to bin step to yield the base fee rate.
    /// effectiveBaseFee (bps) = baseFactor * binStep / 100.
    function getStaticFeeParameters()
        external
        view
        returns (
            uint16 baseFactor,
            uint16 filterPeriod,
            uint16 decayPeriod,
            uint16 reductionFactor,
            uint24 variableFeeControl,
            uint16 protocolShare,
            uint24 maxVolatilityAccumulator
        );
}
