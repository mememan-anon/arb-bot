// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

interface IFlashLoanSimpleReceiver {
    function executeOperation(
        address asset,
        uint256 amount,
        uint256 premium,
        address initiator,
        bytes calldata params
    ) external returns (bool);
}

interface IAaveV3Pool {
    function flashLoanSimple(
        address receiverAddress,
        address asset,
        uint256 amount,
        bytes calldata params,
        uint16 referralCode
    ) external;

    /// @notice Liquidates a position that has fallen below the liquidation threshold.
    /// @param collateralAsset  Asset the liquidator wants to claim as bonus collateral.
    /// @param debtAsset        Debt token the liquidator repays.
    /// @param user             Borrower whose position is being liquidated.
    /// @param debtToCover      Amount of debt to repay (use type(uint256).max to liquidate max).
    /// @param receiveAToken    If true, receive aTokens; if false, receive underlying.
    function liquidationCall(
        address collateralAsset,
        address debtAsset,
        address user,
        uint256 debtToCover,
        bool receiveAToken
    ) external;

    /// @notice Returns reserve data for a given underlying asset.
    function getReserveData(address asset)
        external
        view
        returns (
            uint256 configuration,
            uint128 liquidityIndex,
            uint128 currentLiquidityRate,
            uint128 variableBorrowIndex,
            uint128 currentVariableBorrowRate,
            uint128 currentStableBorrowRate,
            uint40  lastUpdateTimestamp,
            uint16  id,
            address aTokenAddress,
            address stableDebtTokenAddress,
            address variableDebtTokenAddress,
            address interestRateStrategyAddress,
            uint128 accruedToTreasury,
            uint128 unbacked,
            uint128 isolationModeTotalDebt
        );

    /// @notice Returns the list of active reserve addresses.
    function getReservesList() external view returns (address[] memory);
}
