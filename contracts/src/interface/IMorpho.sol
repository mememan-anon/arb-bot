// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

/// @notice Morpho Blue complete interface.
/// Morpho Blue on Base: 0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb
///
/// Architecture:
///   - Singleton contract with isolated markets
///   - market = { loanToken, collateralToken, oracle, irm, lltv }
///   - marketId = keccak256(abi.encode(MarketParams))
///   - Health: borrow_assets > (collateral × oracle.price() / 1e36) × LLTV / 1e18
interface IMorpho {
    // ── Structs ─────────────────────────────────────────────────────────────

    struct MarketParams {
        address loanToken;
        address collateralToken;
        address oracle;
        address irm;
        uint256 lltv;
    }

    struct Market {
        uint128 totalSupplyAssets;
        uint128 totalSupplyShares;
        uint128 totalBorrowAssets;
        uint128 totalBorrowShares;
        uint128 lastUpdate;
        uint128 fee;
    }

    struct Position {
        uint256 supplyShares;
        uint128 borrowShares;
        uint128 collateral;
    }

    // ── Flash loan ────────────────────────────────────────────────────────────

    /// @notice Flash-borrow `assets` of `token` from Morpho Blue (0% fee).
    /// Morpho transfers the tokens, calls `onMorphoFlashLoan(assets, data)`,
    /// then pulls back exactly `assets` via `safeTransferFrom`.
    /// The callback must `approve(morpho, assets)` before returning.
    function flashLoan(address token, uint256 assets, bytes calldata data) external;

    // ── Liquidation ────────────────────────────────────────────────────────────

    /// @notice Liquidate a borrower's position in a specific Morpho Blue market.
    ///
    /// @param marketParams  Full market parameters (identifies the market).
    /// @param borrower      Address of the under-collateralised user.
    /// @param seizedAssets  Collateral amount to seize (>0: caller specifies;
    ///                      Morpho computes repaidShares including incentive).
    /// @param repaidShares  Borrow shares to repay (>0: caller specifies;
    ///                      Morpho computes seizedAssets including incentive).
    ///                      Exactly ONE of seizedAssets / repaidShares must be nonzero.
    /// @param data          Arbitrary data (empty = no callback).
    ///
    /// @return seizedAssets_  Actual collateral transferred to caller.
    /// @return repaidAssets_  Actual loan-token amount pulled from caller.
    function liquidate(
        MarketParams memory marketParams,
        address borrower,
        uint256 seizedAssets,
        uint256 repaidShares,
        bytes calldata data
    ) external returns (uint256 seizedAssets_, uint256 repaidAssets_);

    // ── State reads ────────────────────────────────────────────────────────────

    /// @notice Aggregated market state (totals, fee, timestamp).
    function market(bytes32 id) external view returns (Market memory);

    /// @notice User position in a market (supply shares, borrow shares, collateral).
    function position(bytes32 id, address user) external view returns (Position memory);

    /// @notice Resolve a marketId back to its full MarketParams.
    function idToMarketParams(bytes32 id) external view returns (MarketParams memory);
}

/// @notice Oracle interface used by all Morpho Blue markets.
///
/// price() ≡ collateralPrice/loanPrice × 10^(36 + loanDecimals − collateralDecimals)
///
/// Health check:
///   collateral_value_in_loan = collateral × price() / 1e36
///   is_liquidatable           = borrow_assets > collateral_value_in_loan × LLTV / 1e18
interface IMorphoOracle {
    function price() external view returns (uint256);
}

/// @notice Callback interface that Morpho Blue invokes during flashLoan().
/// The implementing contract MUST approve(morpho, assets) before returning.
interface IMorphoFlashLoanCallback {
    function onMorphoFlashLoan(uint256 assets, bytes calldata data) external;
}

