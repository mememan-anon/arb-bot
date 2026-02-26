// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

/// @notice Minimal Morpho Blue interface for flash loans (no fee).
/// Morpho Blue on Base: 0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb
///
/// Flash loan flow:
///   1. Morpho transfers `assets` of `token` to msg.sender
///   2. Morpho calls `onMorphoFlashLoan(assets, data)` on msg.sender
///   3. Morpho calls `safeTransferFrom(msg.sender, morpho, assets)` — pulls repayment
///      ⇒ the callback must `approve(morpho, assets)` before returning.
///   Fee: 0% — only the exact `assets` amount needs to be returned.
interface IMorpho {
    function flashLoan(address token, uint256 assets, bytes calldata data) external;
}

/// @notice Callback interface that Morpho invokes during flashLoan().
interface IMorphoFlashLoanCallback {
    function onMorphoFlashLoan(uint256 assets, bytes calldata data) external;
}
